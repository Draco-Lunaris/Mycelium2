# Memory Lifecycle — Design

**Date:** 2026-10-31
**Status:** Approved design, pending implementation plan
**Scope:** mycelium-core (search trait, frontmatter), mycelium-store (encrypted index, revisions), mycelium-crypto (no new key types expected), mycelium-librarian (embedding client, dream), mycelium-web (revisions UI, admin config, scheduler, worker), mycelium-mcp (memory_query, search_knowledge)
**Implements:** GitHub issues #17 (memory lifecycle) and #18 (companion: retrieval-first reads, hybrid search, revisions, provenance) — one stream, ordered per #18's cross-issue ordering.

## 1. Goal

Memory accumulates records of discontinued ideas, services and concepts; nothing retires them, so stale material keeps surfacing in search and contaminating answers — the primary user has been hit **repeatedly**, with significant mistakes caused by out-of-date answers. The write path already tries to handle this (MUTATE step 2 "CHECK FOR CONTRADICTION", rule 8 "DEPRECATE, DON'T DELETE") but retrieval cannot support it.

Goals:

- **Real lifecycle state**: `status`/`superseded_by`/`valid_to` frontmatter actually enforced by every retrieval surface.
- **Honest scoring**: length-normalized (BM25) ranking so enriched-rotten concepts stop outranking focused ones.
- **Retrieval-first reads**: `memory_query` answers with raw ranked hits by default (calling model judges recency/status itself); LLM synthesis is opt-in.
- **Hybrid retrieval (opt-in backend)**: BM25 + encrypted embedding vectors, so paraphrased writes/queries reach the concept they contradict.
- **Revisions as the undo**: every mutation writes an encrypted revision; the web UI can diff and restore back through any number of revisions at once (restore-to-revision). No human-in-the-loop review list — the revision history IS the undo button.
- **Provenance**: `source` and `confirmed_at` stamped by the mutation path from the authenticated caller, never authored by the LLM.
- **Dream that maintains**: scheduled runs, a staleness signal keyed on `confirmed_at`, intra-concept consolidation, duplicate detection upgraded by embeddings, then a one-time corpus backfill.

Non-goals (rejected, with the reason):

- **Human review list for low-confidence retirements** (suggested in #17) — dropped by the user; revision history is the undo.
- **#17's write-path LLM synonym expansion** — superseded by embeddings (§5); the agent's existing miss-notice synonym retries on the read path remain.
- **A confidence threshold constant** — meaningless without the review list.
- **No change to the encryption model**: vectors and revisions reuse the existing per-user key derivation (§4 rationale); no new master/key ceremonies.
- **No per-section vectors, no body-chunking** — one vector per concept over the indexed text; re-visit only if recall proves poor.
- **Books are immutable** — the `global:library` scope gets vectors and revisions machinery for chapter concepts, but its catalog is never rewritten by dream.

Anti-goal / hard invariants (must survive):

- **Oracle equivalence**: the in-memory index (mycelium-core) and the encrypted store index must score identically. BM25 is implemented **once** in `mycelium-core::search` and called by both (this is the mechanism that keeps the oracle honest, not a convention).
- **Deterministic ranking everywhere** (stable sort, tie → path asc; RRF fuses **ranks**, never raw score scales).
- **No LLM in the default read path** — `memory_query` without `synthesize` must make zero LLM calls.
- **Fail-degrade-never-hang**: embedding backend down/lagging → lexical-only answers, no error to the caller, non-latching health (weather, not illness — the backend state never enters the `/health` degraded rule, same ruling as the LLM probe).
- **Revisions are append-only until pruned**: dream, backfill and migration never bypass a revision write; retention pruning is the only delete, and it is documented as permanent.

## 2. Current state (the gaps, with evidence)

Refs verified against master post-PR-#15; line numbers have drifted ~10 lines from the issue's `79ea8ac` refs — anchors below are current.

| # | Gap | Evidence |
|---|-----|----------|
| G1 | Scoring is raw summed TF, no doc-length normalization: large enriched concepts dominate | `crates/mycelium-store/src/search_index.rs:90-112` (SQL `SUM(tf)`), in-memory oracle same; `tokenize` has no stemming (`crates/mycelium-core/src/search.rs`) |
| G2 | No lifecycle enforcement: tags are ordinary tokens in `searchable_text` (title + description + tags + body); no code reads a `deprecated` tag back; `timestamp` set on create and never read | `search.rs` `searchable_text`; `crates/mycelium-mcp/src/tools.rs:395` `timestamp: Some(now_rfc3339())`; rule 8 at `crates/mycelium-librarian/src/agent.rs:1080` is prompt-only |
| G3 | `memory_query` runs the agent (up to `MAX_STEPS = 30`, `agent.rs:21`) and returns synthesized prose; the deterministic fallback (`Answer::Hits`) already exists but is only used when the LLM is down/gate-exhausted | `crates/mycelium-mcp/src/tools.rs:192+` |
| G4 | No revision history: `write_concept`/`patch_concept` overwrite in place; dream is authorized to delete duplicates after merging — an LLM drop is unrecoverable short of full backup | store layer (`FileRepo` payloads replaced atomically), `agent_traces` holds summaries only |
| G5 | Dream is manual (`POST /api/v1/dream`), signals are orphans/broken-links/title-duplicates(O(n²) jaccard)/oversized only; nothing asks "is this still true?" | `crates/mycelium-librarian/src/dream.rs:39-124` |
| G6 | No provenance: frontmatter has no `source`/`confirmed_at`; with multiple harnesses writing, bad writes can't be audited or bulk-retired | `Frontmatter` struct, `crates/mycelium-core/src/concept.rs:85-107` |

Working mechanics this design builds on: ConfigStore typed sealed JSON KV (`set("llm", …)` pattern + admin LLM portal section + `sync_gate`-style boot sync + raw-probe health worker — the concurrency/outage-recovery streams established all of it); purpose-bound DEK derivation (ChatKeys pattern); the mutation queue's per-user FIFO with staged payloads; `EncryptedIndex` scopes (`user:<uuid>`, `global:skills`, `global:library`) with HMAC path tokens; the boot-sweep + ticker worker pattern.

`Frontmatter` parsing is lenient (unknown fields ignored), and every user concept can carry the new fields without a format migration — but the *struct* must learn them for validation and the store index needs a plaintext status column (below).

## 3. Design

Five steps, each independently shippable, in #18's dependency order. Migrations: one migration `0010` carries all new SQL objects (§3.1) — land it in step 1.

### 3.0 Combined frontmatter schema (step 1)

One change, both issues' fields:

```yaml
status: active | superseded | retired   # absent ⇒ active (legacy and new concepts omit it
                                        # when active — this is the migration end-state, not an omission)
superseded_by: /path/to/concept.md      # absolute OKF path; the new/current concept
valid_to: 2027-01-01T00:00:00Z          # soft expiry; past-due ⇒ dream staleness signal (§3.4), retrieval ignores it
source: mcp:<api-key-label>             # stamped by mutation path; LLM-written values OVERWRITTEN
confirmed_at: 2026-10-31T14:00:00Z      # stamped by mutation path at integrate time; touched on every successful mutation
```

- `Frontmatter` gains `status: Option<LifecycleStatus>` (`active/superseded/retired` — a real enum, lenient `unknown ⇒` parse error concept, matching `SkillRole`'s leniency only for *skill* manifests; for status, unknown value ⇒ concept parse stays lenient but write-time validation rejects it), `superseded_by: Option<String>`, `valid_to: Option<String>`, `source: Option<String>`, `confirmed_at: Option<String>`. Unknown-value `status` on parse: keep the string, flag invalid at validation surfaces (write_concept/patch validation reject; never hard-parse-fail reads).
- `superseded_by` pointing at a missing path → flagged like a broken link (graph already computes broken links; the status link joins them — and is a dream signal).
- `searchable_text` EXCLUDES all five fields — status material never re-pollutes ranking. The `# History` section rule lands here too (§3.4): `searchable_text` stops at the first body line `# History`.
- Write-time validation (put/patch): `status` must parse to the enum when present; `superseded_by` must be an absolute `.md` path; `valid_to`/`confirmed_at` must be RFC3339. LLM-authored values for `source`/`confirmed_at` in a put/patch body are silently replaced (§3.1).
- Rule 8 (`agent.rs`) and the `delete_concept` tool description are rewritten to use these fields: supersede ⇒ `status: superseded` + `superseded_by`; retire ⇒ `status: retired`. "Deprecated tag" is deleted from the vocabulary.

**Migration of existing `deprecated` tags (boot sweep, idempotent):** at boot (and again after any restore/migration touch — predicate-driven, no marker needed), for each scope list concepts whose tags contain `deprecated`; rewrite frontmatter: remove the tag, set `status: retired`, `source: migration`, `confirmed_at: now`; write one revision (§3.2, `op: migration`); keep the index entry but flip the registry status column to `retired`. Retired concepts stop matching "deprecated" queries because the tag is gone and their text no longer contains it after rewrite.

### 3.1 Provenance + shared status filter (step 1)

**Where `source`/`confirmed_at` come from.** Enqueue time is when identity exists:

- The MCP enqueue path (`enqueue_mutation`, mycelium-mcp tools.rs — serving `memory_add`/`memory_update`, whose `QueueTool` is `add`/`update`/`maintain`) already reads the per-request identity (`http::request::Parts`) — capture a human-readable label at enqueue: `mcp:<api-key-label>` (ApiKeyRecord carries `label` for every minted key). Persisted with the queue row (one new `source` queue column; `user_id` already there).
- Web UI writes carry the session identity (`web:<username>`); dream passes carry `dream`; resume/drain re-integration carries `migration`/`resume` as appropriate.
- At integrate time the mutation path stamps `confirmed_at = now` and `source = <captured label>` into the concept frontmatter — after whatever the user-supplied body wanted to say on those two fields. `confirmed_at` updates on every successful create/patch/restore touching the concept. `timestamp` keeps its current create-time role (display provenance).

**Shared status filter.** Implemented at the trait level so both indexes — and the vector side in §3.3 — honor it:

- `SearchQuery` (`mycelium-core::search`) gains `status_filter: StatusFilter` (`ActiveOnly` default | `All`). Constructing a `SearchQuery` without saying otherwise means active-only — including the agent's `search_knowledge` tool and every other existing caller, which is the point.
- Store index: migration `0010` adds plaintext `lifecycle_status TEXT NULL` to the **`search_docs` index table** (verified: search never joins `scope_files` — the doc-load path reads `search_docs.payload` only — so the index carries its own status column; the registry stays untouched). `put`/`add` maintain it (it is non-sensitive ranking metadata; paths already carry HMAC tokens). The status filter becomes a SQL predicate — no extra decrypt round-trips.
- In-memory index: filters pre-scoring on the parsed frontmatter.
- `valid_to` does NOT participate in the filter (advisory only — staleness signal). Retrieval treats a concept with any absent/unknown status as active (lenient, matches "absent ⇒ active").
- Opt-in for history queries: MCP `memory_query` gains `include_inactive: bool = false` on the hits path, as does the librarian agent's internal `search_knowledge` tool (there is no MCP tool of that name; its schema + the rule-8 prompt rewrite cite it). Direct path reads (`read_concept`, graph, web viewer/writer) see everything always — lifecycle hides concepts from *retrieval*, not from existence.

### 3.2 Revisions (step 2) — the undo button

New table in migration `0010`:

```sql
CREATE TABLE concept_revisions (
  id INTEGER PRIMARY KEY,
  scope_id TEXT NOT NULL,          -- user:<uuid> | global:skills | global:library
  path_token TEXT NOT NULL,         -- HMAC path token, same scheme as search_tokens
  revision_no INTEGER NOT NULL,     -- monotonic per (scope_id, path_token); MAX+1 at insert
  op TEXT NOT NULL,                 -- create|update|delete|restore|merge|consolidate|migration
  source TEXT NOT NULL,             -- provenance captured at enqueue (§3.1)
  summary TEXT NOT NULL,            -- short deterministic op description (bounded like queue receipts' short_detail)
  payload BLOB NOT NULL,            -- AEAD full concept markdown snapshot (incl. frontmatter)
  created_at TEXT NOT NULL,
  UNIQUE (scope_id, path_token, revision_no)
);
```

- **Every mutation that changes or removes a concept writes a revision** — `write_concept` create (`op: create`), patch/replace (`update`), `delete_concept` (`delete`, payload = the last snapshot), dream merge/delete (`merge`/`delete`, source `dream`), restore (`restore`), consolidation rewrite (`consolidate`), migration sweep (`migration`). Revisions precede the effect (write-ahead): the concept change commits only after its revision row does.
- **Crypto**: AEAD under the existing per-user `IndexKeys` meta DEK — the same sensitivity class as the `EncryptedIndex` doc payloads that already live in SQLite under it; AES-GCM AAD binds the purpose explicitly (`rev:<scope_id>:<path_token>:<revision_no>`) so a revision blob can never be replayed as a doc payload or vice versa. No new key type; `mycelium-crypto` gains a small `revision_aead` helper only if AAD wiring warrants it (builder discretion, reviewed).
- **Retention**: after insert, prune rows beyond `revision_retention` (default 200, range 25–2000) oldest-revision-first per concept. Pruning is permanent and documented. Retention counts *all* ops including the migration sweep.
- **Restore semantics (settled call):** restore-to-revision N rewrites the concept to that snapshot verbatim (frontmatter + body, minus provenance — `source`/`confirmed_at` re-stamp per §3.1) and commits as a **new revision** (`op: restore`, `source: web:<username>`) — restoring revision 1 of a concept rewritten 40 times undoes all 40. Restored `status` is whatever the snapshot said (restore is literal); an explicit "Set status: active" action revives retired/superseded concepts.
- **Restore undeletes (settled call):** a deleted concept keeps its revision chain; restoring its `delete` revision (or any earlier one) re-creates it at the original path. Chains continue across delete/re-create eras at a path; because every restore is itself a revision, even a cross-era rollback is fully reversible. Restoring over a live concept from a stale-era revision requires explicit UI confirmation.
- **Web UI** — per-concept on the existing viewer/writer page: revisions list (revision_no desc, op, source, relative date, summary); diff view (any two revisions or HEAD↔N → unified diff of frontmatter+body through the same XSS escape discipline); restore button per row. **Plus a scope-level Revisions browser page** (owner for user scopes; admin for service scopes) listing every path with revisions, live **or deleted** (latest title + last op per path) — the entry point for deleted concepts' history, where the viewer page no longer exists for them.
- REST (session-scoped, like the other `/api/v1/concepts` routes): `GET /api/v1/revisions?scope=<scope>` (per-path index, dead paths included, meta only — never payload text), plus per-path `GET …/revisions`, `GET …/revisions/{n}` (snapshot for diffing), `POST …/revisions/{n}/restore` — all work identically whether the concept is live or deleted.
- **Write-ahead hook placement (settled call):** revision hooks live at the ConceptStore mutation boundary (put/patch/delete), so every writer is covered — MCP queue integrate, agent tools, web writer, ingest — not just the MCP enqueue path.
- MCP gets **no** revision tools (v1): undo is a human UI action by design.
- The mutation-queue receipt surface is unchanged; nothing about step 1–5 changes enqueue/drain shapes beyond the two provenance columns.

### 3.3 Retrieval-first reads + BM25 + hybrid vectors (steps 3 and 4)

**BM25 (step 3).** One scoring function, both indexes:

- `mycelium-core::search` gains `bm25::Params { k1: 1.2, b: 0.75 }` and `pub fn bm25_score(term_postings…)` — the single implementation, called by the in-memory index (df = posting list length, doc length tracked per document at put/remove) and by the store index (df = scoped rows per token, doc length = new `length INTEGER` column on the doc-metadata table computed at `put`; avg length = `SUM(length)/N` per scope, N = scoped doc count, one extra aggregate in the existing per-term query pass; the existing batch doc-load fetches lengths for scored paths).
- Score remains "sum over query terms" (no AND requirement, as today); stable sort desc, tie path asc. The oracle-equivalence tests switch to BM25 with identical inputs. Legacy rows: one boot re-put sweep backfills `length` (and re-applies the §3.0 `# History` split to indexed text) — idempotent boot-sweep machinery, same shape as the vector backlog sweep below.
- Effect per acceptance: on equal term matches, length normalization stops long enriched concepts outranking focused ones.

**`memory_query` default = hits (step 3).**

- `memory_query` with no options: **zero LLM calls**; returns top-k (default 8, admin-tunable `query_top_k` 1–64) hits, each `{scope, path, title, description, score, timestamp, confirmed_at, status, superseded_by}`. Status filter = ActiveOnly; `include_inactive: true` reaches retired/superseded material on the same path. This is today's deterministic fallback promoted to the default — no new machinery.
- **Scope set (user decision, 2026-10-31):** default hits span **user + skills + library**, applying the same visibility rules as the synthesis path (private-shelf content post-filtered out for non-admins). This supersedes the recorded user-only fallback posture (`tools.rs` "MCP query stays private by default"): the fallback has become the *default* answer, and a single-scope default would be strictly less informed than the synthesis it replaces — the posture flips back to match the agent scopes and the recorded comment is updated in code.
- Optional `synthesize: true`: the current agent path exactly as today (wait-class gate, deterministic fallback on gate timeout/unreachable — behavior already shipped by the outage stream). When the future query cache/hot Q&A lands (roadmap), it caches synthesis only — noted in api.md, nothing built now.
- The agent's `search_knowledge` gains `include_inactive` (§3.1) and returns the same hit fields; the agent's system prompt MUTATE section cites them so the *calling* model can judge staleness itself — this is what retires the weaker-model-filtering-stronger-model complaint in #18 §1.
- `Answer::Hits` becomes the *default* enum arm for the no-synthesize path; synthesis is the explicit arm.

**Hybrid retrieval (step 4) — the embeddings backend, very specific requirements:**

Config row `embeddings` (sealed typed JSON KV, cloned from the `llm` row end to end — admin portal section, clamp-on-save AND clamp-on-load, `sync_embedding_from_store` at boot before any sweep/worker spawn, raw-probe health worker on a 30s tick feeding `EMBEDDING_HEALTH`):

| Field | Semantics |
|---|---|
| `base_url` | OpenAI-compatible server root, e.g. `http://ollama.lan:11434` or a vLLM server |
| `model` | embedding model name, e.g. `nomic-embed-text` |
| `api_key` | optional Bearer; absent ⇒ no auth header |
| `timeout_secs` | per-request, default 30, clamp 5–300 |
| `batch` | max inputs per HTTP call, default 32, clamp 8–128 |

Endpoint contract (what "OpenAI-compatible" must mean for the builder, and what gets probed/validated):

- `POST {base_url}/v1/embeddings` with JSON `{"model": model, "input": ["text", …]}` (array in, **one vector per input element, response `data[i].index` order preserved** — the client must reorder by `index` and error on gaps).
- Response JSON: `{"data": [{"index": n, "embedding": [floats]}], "model": …}`. Floats parsed as `f32`.
- Fixed dimensionality per model: learned on first successful call, persisted per scope as vector-meta; **dimension change or model change ⇒ scope vectors marked stale ⇒ background re-embed rebuild** (§3.3 worker); lexical retrieval unaffected during rebuild.
- Hard limits: **save vs. validation split (settled call, `llm`-row precedent)** — saving a config validates shape/clamps only and never touches the backend; the 30s probe validates the endpoint contract, learns the dimension, and reports failures (including a >4096-dimension model) as config-level Red surfaced in the admin portal. Dimensionality is per-scope vector-meta, never config; query-time safety skips vector rows whose dims mismatch the active model.
- Failure classification (feeds health, same state machine as LLM_HEALTH — Green/Yellow/Red/Disabled, static extracted so both share it): DNS/connect error, timeout, non-2xx, malformed body; undecryptable config row = config problem, feeds NO observation (outage-recovery precedent).
- **Input policy (settled call):** embedding input = `searchable_text` truncated to a fixed 24,000 chars (~6k tokens; deterministic, no admin knob — enriched concepts and chapter bodies exceed embedding-model contexts, and an uncapped 400 from a healthy backend would misreport weather). Per-batch failures retry 3× then drop the item from the queue with a logged reason — never infinite retry, never a recall error to a caller; the next write re-enqueues. A `data[i].index` gap or missing item counts as a batch error under the same policy. Lexical retrieval is unaffected in every failure case.

Storage + worker (migration `0010`):

```sql
CREATE TABLE search_vectors (
  scope_id TEXT NOT NULL, path_token TEXT NOT NULL,   -- same HMAC path token scheme as search_tokens
  model TEXT NOT NULL, dims SMALLINT NOT NULL, stale INTEGER NOT NULL DEFAULT 0,
  vec BLOB NOT NULL,                                  -- AEAD f32 LE bytes under IndexKeys meta DEK; AAD `vec:<scope>:<token>`
  updated_at TEXT NOT NULL, PRIMARY KEY (scope_id, path_token)
);
```

- **Write hooks**: after each successful put/patch/delete, enqueue `(scope, path_token)` into an in-process bounded dedup work queue (cap 512, overflow drops — the next write re-enqueues; lexical unaffected). A web worker drains it: loads concepts for a scope in batches of `batch`, embeds `searchable_text(concept)`, writes `search_vectors` rows, removes markers. Service scopes (`global:skills`, `global:library` chapters) use the service key/master path like ingest does. **Embedding calls touch no LLM gate** — a separate small model, separate capacity; queue depth + processed counters go to `/metrics`.
- **Boot sweep**: build the initial vector backlog (every indexed concept lacking a fresh row) — bounded, resumable across boots (stale markers), same worker. Rebuild-after-model-change reuses this sweep via the stale flag.
- **Query fusion (settled call):** `SearchQuery` gains `mode: Auto` (default) `| Lexical` `| VectorOnly`-as-diagnostic. Auto with healthy embeddings: candidates are **per-scope pools** — lexical top-50 *and* vector top-50 *per scope* (BM25 scores are only comparable within a scope; fusing raw scores across scopes would bias toward large scopes), then **Reciprocal Rank Fusion** merges the per-scope rank lists: `score = w_lex/(rrf_k + rank_lex) + w_vec/(rrf_k + rank_vec)`, `rrf_k` default 60, weights default 1.0/1.0 (admin-tunable on the lifecycle config row §3.5). Status filter cuts before pooling; visibility filtering applies after fusion; rank-based fusion keeps raw score scales out, so the result stays deterministic (stable sort, tie → path asc). Degradation (backend Red/timeout/missing vector for a concept): that candidate source just drops out — lexical-only answers, never an error.
- **In-process vector cache**: decrypted scope vectors cached in `AppState` (per-scope `Arc<Vec<(token, dims, Vec<f32>)>>`, invalidation-on-write, sized by concept count × dims; a note documents the memory envelope: 10k concepts × 1024 dims ≈ 40 MB).
- `duplicate_candidates` (dream) upgrades to cosine top-2 above `dedup_cosine` (0.92) when the vector index is healthy and populated, with the jaccard title comparison as fallback — this addresses #18 §Minor's O(n²) scaling note in the same stroke.

### 3.4 Dream with a schedule and a staleness signal (step 5)

**Scheduler.** `crates/mycelium-web/src/dream_scheduler.rs` — a ticker worker (boot-sweep + ticker pattern, spawned in `main.rs` after `sync_*`):

- Every `dream_interval` (default 24 h, clamp 1 h–30 d) process the **next user scope, round-robin deterministic cursor** over users with dream signals; one scope per tick.
- Each pass admits through the LLM gate **try class** (background maintenance never waits ahead of an interactive waiter; full queue ⇒ skip this tick, signals persist). Dream-eligible service scopes (`global:skills`) go through the same round-robin after user scopes.
- `dream_enabled` admin toggle (default true). `POST /api/v1/dream` remains the manual trigger.

**Staleness signal** (deterministic, computed before any LLM call, appended to the existing signal list):

- Active concept whose `confirmed_at` (fallback: `timestamp`) is older than `stale_days` (default 90, clamp 7–3650) **and** whose title/tag tokens overlap any concept `confirmed_at` within the last `recent_window` days (default 14, clamp 1–365) → signal pairs them: old concept + recent concepts, agent decides supersede / retire / keep, applying status fields per §3.0.
- Active concept with `valid_to` in the past → "expired: retire or move valid_to forward".
- `superseded_by` pointing at a missing concept → repair-supersede signal.

**Intra-concept consolidation signal.** Revision count for a concept with consecutive `update` ops exceeding `consolidation_patches` (default 8, clamp 2–100; resets on `create`/`restore`/`consolidate`) → signal instructing the agent to rewrite the body as a current-state summary, moving superseded facts under a body line `# History` — which `searchable_text` skips (§3.0), so history text stops ranking. Applied as a normal patch (`op: consolidate`, revision written).

**Corpus backfill.** Admin button on the dream admin section: enqueue a one-time detached backfill task iterating **every** scope (users then service scopes) through the scheduler's signal path, bounded LLM work via the same try-class gate, ConfigStore marker `dream_backfill_at` records completion (re-clickable, marker reset). Signals are idempotent, so a crash mid-backfill just resurfaces remaining signals on later ticks.

### 3.5 Configuration, observability, docs

Two new sealed config rows, cloned from the `llm` row pattern (clamp save + load, blank keeps stored value, sync at boot):

| Row | Fields |
|---|---|
| `embeddings` | base_url, model, api_key, timeout_secs 30, batch 32 |
| `lifecycle` | query_top_k 8, rrf_k 60, rrf_weight_lex 1.0, rrf_weight_vec 1.0, revision_retention 200, stale_days 90, recent_window 14, dream_interval_s 86400, dream_enabled true, consolidation_patches 8, dedup_cosine 0.92 |

Surfaces:

- `/health`: embedding state (`+embedding_state`, seconds down/checked-ago) + backfill completion flag. **Not** in the degraded rule (backend weather — same ruling as the LLM probe).
- `/metrics`: `mycelium2_embedding_up`, embedding queue depth / processed / failed, dream ticks/passes/skipped, revision prune counts.
- `/admin`: embeddings section (form + probe dot), dream section (interval/enabled/backfill button + last-run readouts), system page embedding indicator.
- Docs: `api.md` (memory_query hits default + synthesize + include_inactive; revisions REST; search_vectors lifecycle), `configuration.md` (both config rows), `deployment.md` (embeddings operator section sizing + degradation, dream schedule, revisions storage/retention).

### 3.2a Implementation amendments (stream ledger, dated)

### 2026-10-31 — step 2 (revisions)

The following amendments bind; they are rulings recorded during step 2 planning.

1. **`concept_revisions` gains `path TEXT NOT NULL`** (after `path_token`).
   Reason: path tokens are one-way HMAC; `delete` erases the registry row
   (`scope_files`) and the index rows, so a dead path's display name would be
   unrecoverable — §3.2's browser page REQUIRES listing deleted paths. The
   plaintext path matches this codebase's established SQL posture
   (`scope_files` title/type, `search_tokens`/`search_docs` `concept_path` —
   audit metadata in SQL; content stays encrypted). Migration 0010 is amended
   in place (branch unmerged, migration never deployed).
2. **Axum route naming**: `{*path}` wildcards cannot be mid-path, so §3.2's
   "per-path `GET …/revisions`" routes concrete out as query-param routes:
   `GET /api/v1/revisions?scope=<s>` (scope-level per-path index, dead paths
   included, meta only), `GET /api/v1/revisions/list?scope=<s>&path=<p>`
   (per-path revision index, meta only), `GET
   /api/v1/revisions/snapshot?scope=<s>&path=<p>&n=<no>` (one revision's
   markdown snapshot for diffing),
   `POST /api/v1/revisions/restore?scope=<s>&path=<p>&n=<no>` (restore).
3. **Revision payload convention (explicit)**: `create` rows carry the
   created content; every other op row carries the PRE-mutation snapshot
   (the file's raw markdown as it stood immediately before the change, no
   re-serialization); `restore` rows that UNDELETE carry an empty payload (no
   prior state exists). Restore-to-N therefore restores `payload(N)` literally
   (frontmatter + body as it stood, then provenance re-stamps per §3.1), as
   §3.2 specifies. Restoring the `delete` row reproduces pre-delete content.
4. **Prune mechanics**: keep the newest `revision_retention` rows per
   concept (monotonic contiguous numbering ⇒ `revision_no <= MAX -
   retention`); pruned rows are irrecoverable. Retention is read through a
   process-global set at boot from the sealed `lifecycle` config row
   (clamp 25–2000 on save AND load; default 200 when the row is absent);
   step 2 wires the boot read ahead of every seed/sweep write, step 5's
   admin section updates it live.
5. **Revision-op capture**: ops derive mechanically at the ConceptStore
   boundary (`scope_files` existence ⇒ create vs update; delete path ⇒
   delete); restore uses a dedicated `ConceptStore::restore(path, n)`; the
   deprecated-tag boot sweep tags its writes `migration` via an explicit
   builder (`with_revision_op("migration")`). `merge`/`consolidate` are
   reserved tags for step 5's dream call sites. The `/mutation-queue/`
   staging-note bypass writes NO revision (queue-internal scaffolding, not
   user concepts).

### 2026-11-01 — step 3 (BM25 + memory_query hits default)

Rulings recorded during step 3 planning; they bind the step-3 plan the way
the step-2 block binds step 2.

1. **Non-negative BM25 idf**: `idf = ln(1 + (N − df + 0.5)/(df + 0.5))`
   (Lucene's form). Plain Robertson idf turns negative when df > N/2, and
   step-3 scoring is sum-over-terms — a term present in half the corpus
   would *subtract* from every multi-term hit. Deterministic, no ranking
   inversion.
2. **Query terms are deduped before scoring on BOTH indexes.** The store
   index already deduped (distinct sort+dedup before the term pass); the
   in-memory oracle did not (a duplicated term double-counted raw TF).
   Under BM25 a repeated query term must not double the term weight; both
   sides now agree on identical inputs.
3. **`search_docs.length` ships as migration `0011_search_length.sql`.**
   The spec's "one migration 0010 for the whole stream" predates a
   discovery from steps 1–2: 0010 is merged (shipped history) and cannot be
   amended; `search_vectors` already exists (created empty by 0010, written
   by step 4). The BM25 length column is a NEW migration.
4. **Backfill = index-only reindex sweep** (the concrete shape of §3.3's
   "boot re-put sweep"): candidates are `search_docs` rows with `length IS
   NULL`, per scope; the sweep reads each concept and re-adds it to the
   index directly (`ConceptStore::reindex`) — no concept write, no registry
   touch, **no revision row**, provenance/`confirmed_at` untouched. Because
   it never writes a concept, the deprecated-tag sweep's `global:library`
   exclusion does NOT apply: library index rows are swept too. The reindex
   re-applies §3.0's `# History` token split to legacy rows (their indexed
   text predates step 1's code).
5. **`length NULL` scoring guards**: a scored doc row with missing length
   counts as length 0 (still returned, transiently over-boosted — the state
   exists only while a scope's sweep is failed or pending, never mid-boot:
   the sweep is awaited inline before the listener opens); `n_docs` and
   `avgdl` come from `COUNT(length)`/`SUM(length)` (NULL rows excluded from
   both — superseded by 11 below, which restores `COUNT(*)`; read 11 for
   the live rule); `avgdl == 0` leaves only the tf-saturation term (no NaN).
6. **Hit shape**: both surfaces (MCP `memory_query` hits and the agent's
   `search_knowledge`) return the spec's nine fields `{scope, path, title,
   description, score, timestamp, confirmed_at, status, superseded_by}`
   PLUS `snippet` (additive — the spec's list omits it but the model and
   harness callers use the preview; keep it). `QueryOutput::Hits(Vec<
   QueryHit>)` replaces `Vec<SearchResult>`; `concept_path` renames to
   `path` — an output-shape change, recorded here; api.md's memory_query
   schema lands in step 5's docs sweep.
7. **Caller `limit` ceiling**: absent ⇒ `query_top_k()`; an explicit
   `limit` is honored but hard-capped at 64 (shared ceiling with the
   admin-tunable range 1–64 — the same query-amplification surface); a
   literal `limit: 0` stays zero hits (not silently 1).
8. **`include_inactive` on the MCP tool affects the hits path only**; the
   synthesis path's filtering is the agent's own `search_knowledge`
   argument (model-controlled, shipped in step 1).
9. **The in-memory oracle's re-add REPLACES prior postings** for the same
   path. Previously a re-add *accumulated* (the old content's postings
   stayed live, inflating df and resurrecting dropped terms) — a
   test-reference bug that BM25's df term makes load-bearing. The store
   index already replaces (DELETE-then-INSERT); the two sides now agree.
10. **The agent's `search_knowledge` keeps its 10-hit cap** — a model-facing
    step budget, not the admin-tunable `query_top_k`; §3.3 does not tie
    them.
11. **Corpus-stat universe correction (step-3 Task-2 review ruling)**:
    amendment 5's `COUNT(length)` for `n_docs` is revoked — a NULL-length
    row still owns postings, so `df` (scoped rows per token) can exceed
    `COUNT(length)`, making idf negative and turning the NULL-length
    "over-boost" into a demotion (worst mid-boot when 0011 has run but a
    scope's sweep has not: every term's idf goes negative and higher tf
    ranks lower). Amended: `n_docs = COUNT(*) FROM search_docs WHERE
    scope = ?` (EVERY scoped row — exactly the oracle's `docs.len()`,
    restoring oracle equivalence in the NULL case), `avgdl =
    COALESCE(SUM(length), 0) / n_docs` (NULL rows count in `N` but add
    nothing to the sum), `avgdl == 0` guard unchanged; AND the store's
    `bm25_score` call site clamps `n.max(df)` as a hard invariant guard
    (callee contract: `df` must not exceed `n_docs`; covers the transient
    concurrent-add window). NULL-length rows still score as length 0 —
    over-boosted, present, never an error — and the
    `null_length_scoring_guard` test gains ordering + non-negativity pins.
12. **MCP wire delivery of hits (Task-3 review ruling — supersedes the
    plan's "handler.rs:58–62 serializes whatever the arms carry" premise,
    which was wrong: the handler formats a fixed four-field text
    template)**: the hits arm's wire result carries BOTH (a) the text
    template extended per hit with `scope` and the lifecycle fields
    (`status`/`confirmed_at`/`superseded_by` — compact lines after the
    snippet) and (b) `structuredContent` = `{"hits": [...]}` — the
    schema-facing delivery of §3.2a-6's ten-field shape; the MCP
    integration wire tests read `structuredContent`, not the live Rust
    value. AND (c) empty render: an empty hits list keeps the existing
    isError "no concepts matched the question" content — a literal
    `limit: 0` result is indistinguishable from a miss on the wire
    (documented; §3.2a-7's zero-honoring lives at the function boundary;
    the isolation probes key on isError-on-none).

### 2026-11-01 — step 4 (embeddings + hybrid search)

Rulings recorded during step-4 planning; they bind the step-4 plan the way
the earlier blocks bind their plans.

13. **Vector access is reached through `ConceptStore`, not around it**:
    `ConceptStore` gains a `vectors: VectorStore<'a>` field built alongside
    its existing repo/index/revisions siblings (both constructors), with a
    `pub fn vectors(&self) -> &VectorStore<'a>` accessor — every consumer
    (agent / MCP hits path / web search) already holds a `ConceptStore` per
    scope, so no new state plumbing is added to surfaces. The shared fusion
    helper is `cross_scope_hits` (librarian agent.rs — the one function all
    three consumers already call), extended with a trailing
    `embeddings: Option<&EmbeddingsClient>` parameter;
    per-surface callers build the one-shot client from the decrypted
    `embeddings` config row (absent row ⇒ `None` ⇒ lexical-only).
14. **RRF is per-scope fusion, then a cross-scope merge of fused scores**:
    each scope's lexical top-50 and vector top-50 are fused with RRF *within
    that scope* (lexical pools are corpus-relative; BM25 ranks only compare
    within one scope — §3.3), and the per-scope fused lists are then merged
    across scopes by fused score desc, path-asc ties, followed by the
    library-visibility post-filter and the caller's cap. Fused scores are
    rank-based, so they ARE comparable across scopes; raw BM25 never was.
    Cost if wrong: cross-scope ordering slightly off in multi-scope
    deployments — observable only with a populated multi-scope corpus.
15. **The embeddings health REUSES `LlmHealth` by instantiation** — no
    rename, no trait, no second state machine: `LlmHealth` is cheap to
    construct, so `pub static EMBEDDING_HEALTH: LazyLock<LlmHealth>` is a
    second process-global of the same type; the four `#[doc(hidden)]
    *_at` fns are un-hidden as the documented per-instance seam (the
    production `LLM_HEALTH` reads the chat-LLM inputs inside `state()`/
    `snapshot()`; every other instance feeds its own inputs via the `_at`
    fns). The embeddings instance always passes `busy: false` to the
    staleness rule (that rule exists for queue waiters; embeddings has no
    wait queue) — its `wired` free fns are `embedding_state()` /
    `embedding_snapshot()`, reading `EMB_LAST_SUCCESS` (0 ⇒ None).
16. **Deletes remove their vector row DIRECTLY at the `ConceptStore::delete`
    tail** (`remove_row` + cache eviction; NO enqueue — a deleted concept
    has nothing to embed, and an enqueue would re-create the classic
    delete-then-rebuild race). put/patch/restore enqueue once per call
    (dedup-on-push, cap 512, back-drop) AND evict the decoded cache;
    `reindex` (index-only, content unchanged) does neither. Stale-scope
    rebuild therefore resurrects no row a delete already removed.
17. **The admin-form rrf tuning fields ride step 5's dream admin section**
    (they live on the same `lifecycle` config row and the same admin page
    region); step 4 ships the `lifecycle` row EXTENSION structurally —
    serde(default) fields (rrf_k, rrf_weight_lex, rrf_weight_vec,
    dedup_cosine) with clamps and process-global statics — with no form
    UI yet; the form fields are added in step 5 with the other lifecycle
    controls. Config-level embedding settings DO get their admin section
    in step 4 (`/admin/embeddings` — new row, not the lifecycle row).
18. **Boot posture for the embeddings health mirrors the LLM seed**: main.rs
    calls `EMBEDDING_HEALTH.set_disabled()` BEFORE anything can observe it
    (a just-constructed `LlmHealth` reads Green — never trust the trivial
    default), then `sync_embedding_from_store(&state)` lifts the posture
    exactly like `sync_llm_gate_from_store` (absent row ⇒ stays disabled;
    present ⇒ `note_configured()`; undecryptable = warn + still
    configured — the observation belongs to the 30s probe worker). Boot
    order pin: sync_embedding_from_store < vector-worker spawn <
    embedding-probe spawn < serve.
19. **§6.2's end-to-end supersede acceptance is demonstrated by step 4's
    dream cosine-dupes work (Task 6), not the composer (Task 5)** — review
    ruling. "Hybrid recall guarantees the supersede flow" means recall of
    the still-ACTIVE original through a rephrased duplicate (no shared
    tokens) so the dedup/supersede machinery can find it; the composer
    test covers exactly that recall half. The pools stay ACTIVE-ONLY in
    BOTH sources — a superseded row never ranks (spec §6.1 corollary,
    "status filter cuts before pooling", Task 3's cache pins); the
    implementer report's "status-blind vec pool" phrasing was wrong and is
    retracted. Related: `SearchMode::VectorOnly` deliberately bypasses the
    health machine — the embed's own result is the truth for a diagnostic
    mode; a failing backend yields an empty pool, which is the truthful
    reading.

## 4. Crypto model (unchanged invariants)

- No new key hierarchy. Existing `MasterKey`/KEK/DEK layering untouched.
- Vectors and revisions are AES-GCM **under the existing per-user `IndexKeys` meta DEK** (service scopes: service-key-scoped IndexKeys), with explicit AAD strings (`rev:<scope>:<token>:<no>`, `vec:<scope>:<token>`) preventing cross-purpose replay — the same sensitivity class as the AEAD doc payloads already stored in SQLite under those DEKs. Key-purpose separation across *classes* (chat/doc/index) is preserved; within the index class this is intentional reuse, documented.
- All per-user data (vectors, revisions, config) remains encrypted at rest; nothing plaintext-sensitive is added (status/`confirmed_at`/`source` labels in SQL are non-sensitive ranking/audit metadata, consistent with existing plaintext registry columns).

## 5. Delivered order (PR plan sketch)

1. **Step 1** — schema + status filter + provenance + deprecated-tag migration + rule-8 rewrite (migration 0010 SQL objects all land here; unused tables in steps 1–4 are created empty).
2. **Step 2** — revisions store layer + write-ahead hooks + REST + web UI (list/diff/restore-to-N).
3. **Step 3** — BM25 shared scorer + `memory_query` hits default + `include_inactive`.
4. **Step 4** — embeddings config/probe/client/worker + `search_vectors` + RRF fusion + dream cosine dupes.
5. **Step 5** — dream scheduler + staleness/consolidation signals + backfill button; docs sweep for everything.

Each step: verify (fmt/clippy/test) → judge subagent → code-review subagent → re-verify, per house convention.

## 6. Acceptance criteria (combined #17 + #18)

1. `status: superseded|retired` concepts absent from default `search_knowledge`, default `memory_query`, and librarian agent answers; `include_inactive: true` surfaces them.
2. A write that supersedes an existing concept in *different wording* marks the old concept `superseded` with `superseded_by` → new path (guaranteed by hybrid recall; pinned by a mock-embedding integration test).
3. On equal term matches, a long enriched concept no longer outranks a short focused one (BM25; pinned by both-index oracle test).
4. `memory_query` without `synthesize` makes **zero LLM calls** and returns hits with status and timestamp/confirmed_at.
5. A paraphrased query with no shared tokens retrieves the relevant concept (hybrid).
6. Every LLM mutation — including dream merges/deletes and the migration sweep — has a revision; restore-to-revision N reverts all changes after N in one action, and restoring a deleted concept's revision re-creates it (the scope-level revisions browser lists deleted concepts).
7. Every MCP-written concept records `source` from the authenticated caller; LLM-authored `source`/`confirmed_at` values are overwritten.
8. Existing `deprecated`-tagged concepts become `status: retired` at boot and vanish from default retrieval.
9. Dream runs on schedule through the try class; consolidation and backfill exercisable; embeddings never entered the LLM gate.
10. With no embedding backend configured, everything above still passes on the lexical path (degradation is not a broken state).