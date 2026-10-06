# Mutation queue: MCP writes return receipts; the librarian integrates in the background

**Date:** 2026-10-04
**Issue:** [Draco-Lunaris/Mycelium2#1](https://github.com/Draco-Lunaris/Mycelium2/issues/1)
**Status:** Approved design (decisions made interactively 2026-10-04); not yet implemented
**Baseline:** `a3d3748`

## 1. Problem

All three MCP write tools (`memory_add`, `memory_update`, `memory_maintain`) run the librarian
agent loop inline inside the MCP request. One run is up to 30 LLM steps (`agent.rs` `MAX_STEPS`),
each with a 300 s transport timeout and no overall deadline. Under a write burst, calling sessions
time out while `/health` (a bare `SELECT 1`) keeps reporting healthy. Query runs (`memory_query`)
share the same unbounded LLM capacity, so reads also queue behind writes. This is the same failure
the original Mycelium exhibited (`/mcp-servers/mycelium-write-latency.md` in the operator
knowledge base documents the v1 incident and its workaround), carried into the rewrite.

Secondary correctness issue: agent mutations are unlocked read-modify-writes — `patch_concept` does
`store.get` → merge → `store.put` with no lock or version check — so concurrent mutations for one
user can lose patches. Also, the current MCP fallback can double-write: when an in-flight agent run
partially writes concepts and then errors (including `StepCapExceeded`), the MCP layer *additionally*
runs the deterministic direct-write fallback on the same input.

## 2. Decided

| # | Decision |
|---|----------|
| D1 | All three write tools route through the queue; no LLM call on any write-tool request path. |
| D2 | Pending payload = encrypted FileRepo blob (user scope) + SQLite metadata row (hybrid). |
| D3 | One librarian run per queued item (no cross-item coalescing into one run). |
| D4 | `add` items are searchable as staging notes; `update`/`maintain` are receipts-only; the brief double-show window after placement is accepted. |
| D5 | Reserved-capacity semaphore: queries always admitted; drain runs take only spare capacity. |
| D6 | Retries (3 attempts, backoff) plus a per-item age deadline before the deterministic fallback; items never silently dropped. |
| D7 | Receipt lookup via `memory_status`'s optional `receipt_id` arg; no new wire tool. |
| D8 | `memory_query` stays inline (real answers); chat, dream, book ingest unchanged; book ingest is out of scope (separate issue). |

**Query priority (user-mandated invariant, extends D5):** a query run is never delayed by the
drain. Concretely: a single global semaphore of `CAPACITY` permits over all librarian LLM runs;
the drain `try_acquire`s **non-blockingly** and, being a single task, ever holds at most 1 permit —
so at least `CAPACITY − 1` permits are always free for query/chat runs. Writes queuing instead of
running inline is part of this invariant — inline writer runs are what burn query capacity today.

**Boundary:** a drain run already in flight is completed, not cancelled mid-run (bounded by its
12-step cap). A query never waits for *queue capacity* — only for genuinely held permits. The
semaphore caps concurrent LLM runs; it does not time-slice runs.

## 3. Architecture

Four components:

### 3.1 `mutation_queue` (new module in `mycelium-store`)

Mirrors the `ingest_jobs` pattern (books.rs). New migration adds:

```sql
mutation_queue (
  id            TEXT PK,
  user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  tool          TEXT NOT NULL CHECK (tool IN ('add','update','maintain')),
  status        TEXT NOT NULL CHECK (status IN ('staging','pending','running','done','dead')),
  attempts      INTEGER NOT NULL DEFAULT 0,
  detail        TEXT NOT NULL DEFAULT '',
  created_at    TEXT NOT NULL,
  updated_at    TEXT NOT NULL,
  next_retry_at TEXT
);
CREATE INDEX idx_mutation_queue_due ON mutation_queue(status, next_retry_at, created_at);
```

`staging` is an internal enqueue phase (never exposed to callers; receipt views map it to
`pending`). Operations: `enqueue` (INSERT in `staging`, flip to `pending` once the payload file
exists), `claim_due_for_user` (conditional `UPDATE … SET status='running', attempts = attempts + 1,
updated_at = ?` — `WHERE id = ? AND status = 'pending'` is the atomicity, same as
`claim_ingest_job`), `mark_done(paths)`, `mark_failed(next_retry_at)`, `mark_dead(detail)`,
`receipt(user_id, id)`, `queue_health(user_id)` (depth = staging+pending+running, oldest pending
age, per-tool counts), `boot_sweep()`, all row-driven (see §3.2). All timestamps RFC 3339 UTC
strings (convention everywhere else in the schema).

The `attempt` increment happens inside the claim so a claim that fails the conditional update
never double-counts.

### 3.2 Encrypted payload (raw FileRepo blob)

One blob per item at canonical path `/mutation-queue/<id>`, `Scope::User(master_key)` — never
registered as a concept, same raw-payload precedent as library stack texts
(`ConceptStore::put`/registry/index are bypassed for this path). Two-layer envelope via
`FileRepo::write` (inner `PathMeta` seal + per-file HKDF DEK with info `mycelium2/file-dek/v1`),
sealed under the user's master key: identical trust model to every other user concept, and the
sessionless drain worker unseals through the existing `master_key_for` path (service-key seal →
MasterKeyCache). Payload contents: the tool's args (as the tool received them) plus the raw
content. No plaintext of content or args in SQLite or on disk.

Enqueue order: **INSERT row with status `staging` → FileRepo.write (fsynced) → UPDATE row to
`pending` → return receipt.** The row precedes the file, so every crash window is row-driven and
cheap to detect (no file-side orphan scan — FileRepo lists HMAC names, not canonical paths, so a
file-without-row check would otherwise require unsealing every user file at boot):
- Crash before the file exists → boot sweep sees `staging` rows with no payload file → **fail fast
  with detail "payload missing"** (retryable state is undecidable; the caller still has the raw
  content it tried to store).
- Crash after the file, before the flip → boot sweep sees `staging` rows **with** a payload file →
  completes the flip to `pending` (idempotent, the file proves the item was durable).
- Receipt is returned only after the final flip, so callers never see or depend on `staging`.

### 3.3 Staging note (visible until integrated) — `add` items only

Staging notes exist only for queued `add` items: an `add` carries new content that should be
readable before integration. `update`/`maintain` carry *instructions* about existing content —
staging them as notes would misrepresent the patch as content, so they are receipts-only; nothing
about a queued update is visible to queries until the librarian applies it (receipt lookup excepted).

For `add` items, while staging/pending/running, the item is a real concept in the user bundle at
`mutation-queue/<id>.md` (type `Note`, title = `derive_title(content)`, body = raw content, tags
`["queue-staging"]`), so `memory_query` (search index + hot memory) can hit it immediately.
Staging writes/patching bypass the normal guard below (internal-only).

**`/mutation-queue/` prefix is reserved:** `ConceptStore` rejects `put`/`patch`/`delete` calls
whose canonical path lives under `mutation-queue/` when they come from non-queue callers. Internal
queue methods use a separate entry point. (Reservation precedent: reserved filenames
`index.md`/`log.md`/`info.md`.)

On integration — agent run success *or* deterministic fallback — the staging note is deleted.
Window between placement and staging delete: a query may briefly return both the final concept and
its staging copy (D4: accepted). The staging note registers `record_hot_write` for QA-cache
coherence (same as today's inline fallback path) and its delete registers `record_hot_delete`.

### 3.4 Drain worker (new module in `mycelium-librarian`)

One task, spawned in `main.rs` (no poll loop exists today; the librarian worker precedent is
inline invocation). Wake conditions: `tokio::Notify` fired on every enqueue, plus a 60 s periodic
sweep (retry timers, age deadlines). Per wake:

1. For each user with a due item (ordered by oldest `created_at`), **claim one item per user,
   then process that user's item to terminal state (done or dead/fallback) before touching any
   other user's items** — per-user serialization closes the `patch_concept` race. With a single
   drain task and capacity 1 for drain runs this is naturally serial; the per-user discipline is
   what makes it an invariant rather than an accident.
2. Acquire a semaphore permit **non-blockingly** (`try_acquire` on the shared global semaphore;
   a single drain task ever holds at most 1 permit, so it cannot starve query/chat runs — the
   query-priority invariant): if none free, wake again later (retry timer and periodic sweep are
   the backstops; queries always admit first).
3. `master_key_for(user)` → `ConceptStore::for_user` → `agent::run_mutation` with a step-cap
   parameter (12 for background runs; inline callers keep 30).
4. Success: delete staging note, `mark_done(run.files_changed)` (final paths), refresh seed (same
   as inline paths do today), metrics.
5. `AgentError` → classify: **transient** (LLM request error, empty response, HTTP 5xx, step-cap)
   → `mark_failed(next_retry_at = now + backoff(attempts))` while attempts < 3; **deterministic**
   (HTTP 4xx — e.g. wrong model name — and other non-retryable agent errors) → fallback directly,
   no pointless retries against a permanent failure; attempts exhausted or age deadline passed →
   fallback; fallback success → delete staging note, `mark_done` with detail noting fallback
   provenance; fallback failure (e.g. DB error) → `mark_dead(detail)` — never silently dropped,
   visible via receipt lookup, and `/health` degrades on any dead item.

**Fallback ownership moves to the librarian.** The four deterministic fallbacks currently in
`mycelium-mcp::tools.rs` (path derive/title + concept write; dated addendum append; related-link
wiring + broken-link flagging) relocate to a `fallback` module in `mycelium-librarian`. MCP keeps
none: enqueue-only tools cannot fail into an inline fallback. This also fixes the double-write
bug: an errored agent run never triggers a complementary MCP fallback anymore; only the drain
decides between retry → fallback → dead.

**Bootstrap refactor:** `master_key_for` (duplicated today in `McpState` and `AppState`) moves to
`MasterKeyCache` as a method taking `(store, service_key)`; both callers delegate to it. `LlmClient`
reuses the same semaphore (acquire in `LlmClient::chat`-family entry points? — no: acquire around
*agent-loop step calls* only, in `agent.rs`, so deterministic steps and passage retrieval don't
hold permits; chat-streaming holds one).

### 3.5 Where things live

| Component | Crate/module |
|---|---|
| Queue table + SQL | `mycelium-store/src/mutation_queue.rs` (+ migration) |
| Payload blobs | existing `FileRepo`, user scope, canonical `/mutation-queue/<id>` |
| Staging-note guard | `mycelium-store/src/concept_store.rs` (reserved-prefix check) |
| Drain worker | `mycelium-librarian/src/queue_worker.rs` (new) |
| Deterministic fallbacks | `mycelium-librarian/src/fallback.rs` (moved from mcp tools.rs) |
| `MasterKeyCache::master_key_for` | `mycelium-mcp/src/lib.rs` (dedup, web delegates) |
| Semaphore | `mycelium-librarian/src/agent.rs` (around step calls; shared handle) |
| Tool wiring | `mycelium-mcp/src/tools.rs` (enqueue only), `handler.rs` (descriptions, `memory_status` args) |
| Worker spawn + boot sweep | `mycelium-server/src/main.rs` |
| Health/metrics | `mycelium-web/src/health.rs` via a queue-health provider from store |

## 4. Surfaces

### 4.1 MCP tools

- `memory_add`, `memory_update`, `memory_maintain` each: validate args, enqueue (row `staging` →
  payload file → flip `pending`; staging note written when the tool is `add`), return
  `"accepted, queued receipt=<uuid> — the librarian will integrate this in the background; check
  with memory_status(receipt_id='<uuid>')"` plus (for `add`) the staging path. Tool descriptions
  gain the deferred-integration sentence verbatim, so calling agents learn the contract without a
  docs lookup.
- Receipt format: the literal token `receipt=<uuid>` inside the returned text (parseable, grep-able).
- `memory_status`: optional `receipt_id` arg (additive, back-compatible). Without it: unchanged
  graph-health output. With it: `{ state: pending|running|done|dead, tool, attempts, created_at,
  updated_at, final_paths, detail }`; receipt ids are user-scoped (lookup fails silently to
  state=unknown for another user's id — never leaks existence).

### 4.2 `/health`

Adds queue fields: `queue_depth` (staging + pending + running), `oldest_pending_age_seconds`,
`queue_dead_count`, `llm_last_success_seconds` (null if none yet). New degraded rules (503):
`oldest_pending_age_seconds > 2 × deadline` or `queue_dead_count > 0`.

### 4.3 `/metrics`

```
mycelium2_mutation_queue_depth           (gauge)
mycelium2_mutation_queue_oldest_pending  (gauge, seconds)
mycelium2_mutation_queued_total{tool}     (counter)
mycelium2_mutation_integrated_total      (counter)
mycelium2_mutation_fallback_total        (counter)
mycelium2_mutation_dead_total            (counter)
mycelium2_llm_last_success_timestamp     (gauge, unix ts; absent until first success)
```

Hand-rolled exposition consistent with the existing `/metrics` (no new dependency). Depth/oldest
recomputed per scrape from one SQL query (cheap at this scale).

### 4.4 SSE keep-alive — explicitly not re-enabled

`with_json_response(true)` (router.rs:40) means tool responses are plain JSON; keep-alive pings
are inert in that mode. Getting mid-flight pings would require `with_json_response(false)` — a
client-visible protocol change out of scope here. Since no write request is long anymore, the
motivation is gone. Revisit only if query-path clients develop idle-kill issues.

### 4.5 Keying

No new key path, no new ConfigStore entry. Payloads encrypt under the user master key via the
existing `FileKeys::from_master_key` / `dek_for_name` path. The `-staging` note is an ordinary
concept (user master key, as all concepts). `mutation_queue` rows hold metadata only
(id/tool/status/timestamps/detail — never content), consistent with "concept bodies never in
SQLite".

## 5. Tunables

One constants unit (`queue_worker::limits`), documented in one place. Admin-configurability is
explicitly deferred (future work; ConfigStore pattern would be reused then).

| Value | Setting | Rationale |
|---|---|---|
| Drain LLM runs concurrently | 1 (single drain task) | Per-user serialization needs one at a time; FIFO ordering keeps add→update semantics. |
| Global LLM run capacity `CAPACITY` | 2 | One shared LLM backend; drain holds ≤ 1 (query-priority invariant leaves a spare permit for queries). |
| Step cap, background mutation run | 12 (parameter on `run_mutation`; inline paths keep 30) | v1 parity; bounds per-item cost; fallback guarantees integration past the cap. |
| LLM step timeout | unchanged (300 s request / 10 s connect) | Existing. |
| Retries before fallback | 3 attempts; backoff 30 s → 60 s → 120 s via `next_retry_at` | Absorbs transient blips. |
| Per-item age deadline | 10 min from enqueue, enforced by the drain before each run | "Never stays pending indefinitely." |
| Queue depth cap | 50 per user | One runaway caller can't consume disk/memory unboundedly. |
| Queue full behavior | Tool returns an error ("queue is at capacity — retry later"); **no inline degradation** | Inline degradation would reintroduce exactly the blocking failure under burst; also violates query priority. |
| Periodic drain sweep | 60 s | Retry/backoff and deadline timers. |
| Boot sweep | same pattern as `recover_on_boot` | `running` → `pending` (detail: interrupted); `staging` rows without payload fail fast, `staging` rows with payload flip to pending. |

## 6. Error handling summary

- LLM transient error / step cap → retry with backoff (≤ 3) → then fallback.
- Age deadline → fallback regardless of remaining retries.
- Deterministic fallback fails (unexpected; DB error only) → `dead` + detail; receipt shows it;
  `/health` degrades; never silently dropped and never auto-deleted.
- Row without payload → fail fast with clear detail (same as ingest's "staged text lost").
- Payload blob without `staging/pending/running` row (impossible by enqueue order, but checked) →
  boot-swept. Orphan staging note whose row is terminal →
  deleted by drain (staging and row are reconciled by id).
- `master_key_for`/unseal failure → item stays `pending` (retry timer re-arms); after age deadline
  the fallback cannot run either (needs the same keys) → `dead` with detail (key path broken is an
  operator-visible condition, matching today's seal-failure logging posture).
- Queue full → enqueue rejected with an explicit error; caller retries later.
- In-flight drain run when SIGTERM arrives: the drain task's permit is held; on shutdown token,
  the worker finishes or aborts the current claim cleanly and leaves the item `running` with
  boot-sweep as recovery (same posture as the axum 10 s graceful window today — no new
  guarantee, but no *new* loss window either).

## 7. Testing

The issue's three required tests, plus what the decisions imply. All under temp data dirs,
mock LLM (existing agent_integration mock-request pattern), no network.

1. **Burst receipt + isolation:** N concurrent `memory_add` calls with the LLM pointed at a dead
   port → all return `receipt=<uuid>` without any LLM contact (mock asserts zero HTTP attempts);
   `/health` shows depth N.
2. **Durability across restart:** enqueue → drop/reopen Store/data dir → boot sweep → drain
   (mock LLM) integrates → staging note gone, concept placed per `files_changed`, receipt `done`.
3. **Dead LLM does not block reads or writes:** with LLM dead, `memory_add` returns a receipt and
   `memory_query` still answers (its existing deterministic keyword path) — neither can observe a
   blocked/failed query because of queued writes.
4. **Age deadline → fallback:** mock LLM that never responds within deadline → fallback integrates
   → receipt `done` with fallback-provenance detail; `mutation_fallback_total` increments.
5. **Transient retry:** mock LLM fails twice with connection errors then succeeds → integrated,
   no fallback; attempts = 2; backoff honored (item not re-run before `next_retry_at`).
6. **Exhausted retries → dead:** persistent failures → 3 attempts → fallback also fails (mock) →
   `dead`, receipt exposes it, `/health` degraded.
7. **Per-user serialization:** two co-queued `memory_update` items targeting the same concept →
   applied in queue order, second patch sees first's result (patch race closed).
8. **Staging note visibility:** staging note appears in memory_query results immediately post-add;
   gone after integration (single-query double-show window acknowledged, not asserted).
9. **Reserved prefix:** external `ConceptStore::put("/mutation-queue/…")` rejected; internal path
   works.
10. **Enqueue crash windows:** `staging` row without payload file → fail fast with clear detail;
     `staging` row *with* payload → boot sweep completes the flip to pending and integrates.
11. **`memory_status(receipt_id)`** transitions: pending → running → done with `final_paths`;
     another user's receipt id → not found. Staged `add` items look up while pending.
12. **Query priority invariant:** the drain `try_acquire`s its permit on the shared semaphore;
     test asserts a drain run never begins while a query run waits.
13. **`/health` + `/metrics`** expose all new fields, degraded rules fire.
14. **Receipts-only tools:** queued `update`/`maintain` items are NOT visible as staging notes
     (receipt lookup only).

## 8. Out of scope

- Book-upload ingest inline execution (`api.rs` upload handler; same blocking pattern, separate issue).
- `memory_query` contract change (stays inline).
- Chat, dream, WebAuthn/skills surfaces.
- Admin-configurable queue tunables (constants first).
- `rmcp` keep-alive/SSE response mode changes.
- LLM reachability preflight probe (last-success timestamp only).

## 9. Success criteria

1. A burst of `memory_add` calls returns receipts in ≤ 100 ms each with zero LLM contact.
2. Restart never loses a queued item (payload or staging note survives; boot sweep requeues and
   completes interrupted flips).
3. With a dead or hung LLM, `memory_add`/`memory_query`/`memory_update` never block:
   writes error-or-receipt within the enqueue path, queries answer via their fallback path.
4. Every queued item reaches `done` or `dead` (never stuck `pending` beyond deadline + sweep
   interval); `/health` reports the truth in both directions.
5. Query-priority invariant holds under test 12.
6. Full verify suite passes: `cargo check --workspace && cargo fmt --all --check && cargo clippy
   --workspace --all-targets -- -D warnings && cargo test --workspace`.