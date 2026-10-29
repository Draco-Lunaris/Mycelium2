# LLM Outage Recovery & Admin Observability — Design

Date: 2026-10-28
Branch base: master @ 49c41db
Supersedes one prior decision: the "second interruption is final" rule (chat
spec D7) keeps its scope — crash-interrupted turns still get exactly one
resume — but the rule is removed as an accidental verdict on LLM-downtime:
backend-down is weather, not a verdict.

## 1. Goal

When the librarian's LLM backend (CCR, routing to athena) is down, hanging,
or flapping, mycelium2 must:

1. **Recover by itself.** Chat turns and queued writes that arrive (or
   survive a restart) during an outage wait and complete automatically when
   the backend returns — periodically retried, not just once at boot. No
   restart is ever required to unstick them.
2. **Say so.** The admin System page answers, at a glance: is the backend
   healthy (red/yellow/green), what is waiting on it, and is the system
   moving — a summary line plus per-item status.

Anti-goals: a permanent dead/failed verdict caused only by the backend
being down when we looked; an admin page that shows counts without a
"why"; letting a genuinely poisonous turn (content-caused failure) loop
forever; unbounded LLM cost while the backend is down (retry probes are
cheap; claims only happen when healthy).

## 2. Current state (evidence, master 49c41db)

The stall audit of 2026-10-28 (mycelium2 receipt db7e46fe) established:

- **Chat resume strands on downtime.** `chat_resume.rs` claims resumable
  turns (`attempts` 0→1, `chat.rs claim_resumable`) after a gate slot with
  no backend probe; a resumed turn whose run hits the unreachable backend
  finalizes `Interrupted` at `attempts = 1` (`chat_runner.rs:515-535`) —
  `claim_resumable` requires `attempts = 0`, so it can never be claimed
  again. One boot while the backend is down permanently strands every
  interrupted turn. This is the operator-reported failure (3 stuck messages,
  admin account; recovery attempt was a restart — which re-spends the
  single resume attempt against the still-broken backend).
- **Boot path deadens mutation-queue rows with no grace.**
  `queue_worker.rs recover_on_boot` `mark_dead`s staging rows whose
  master-key recovery fails ("payload check unavailable"), while the
  mid-pass path defers with deadline+grace (`queue_worker.rs:303-313`).
  Boot failures issue permanent verdicts; mid-pass failures don't.
- **Ingest boot recovery is one-shot.** `main.rs:112-122` runs the
  librarian boot sweep + one inline pass; `LibrarianWorker` has no periodic
  spawn loop — pending jobs wait until the next restart.
- **Fresh submits strand the same way.** Fresh submits with the backend
  down finalize `failed` + LLM_UNAVAILABLE (`chat_runner.rs:521-525`):
  never retried.
- **No backend health signal exists.** `LLM_LAST_SUCCESS` (llm.rs:259) is
  passive-only and stale-able; `llm_gate::stats()` reports queue shape, not
  reachability. The admin System card shows active/queued/capacity — it
  cannot distinguish "waiting its turn" from "backend is dead" from "will
  never run". There is no probe, no down-since, no per-item waiting list.
- Topology note: mycelium2 → CCR (OpenAI-compatible router) → athena model
  host. A backend failure surfaces either as a transport error
  (`LlmError::Request`) or as a CCR 5xx (`LlmError::Status` 502/503/504
  when athena is unreachable). Classification must treat both as
  backend-down.

## 3. Design

### 3.1 Backend health: `mycelium-librarian::llm_health`

A process-global health registry (static like `LLM_GATE`), fed by two
sources:

- **Active probe**: a background task probes `GET {base_url}/models`
  (OpenAI-compatible; the router implements it) every 30 s with a 5 s
  timeout, carrying the configured `Authorization` header. Any HTTP
  response counts as *reachable*, with two exceptions: 5xx and 401/403
  count as *unhealthy* (5xx = backend/router or its upstream is down;
  401/403 = misconfigured credentials — operator-actionable, not weather).
  Transport failure (connect refused, DNS, timeout) = unhealthy. A hanging
  backend trips the 5 s probe timeout → unhealthy, which is exactly the
  slot-holding failure mode.
- **Passive signals**: every `LlmClient` completion — chat runs, drain
  runs, ingest outlines, the admin test-connection endpoint — reports
  success (the existing `LLM_LAST_SUCCESS` choke point, which also counts
  as an ok observation) and failure (a new hook beside it) with the §3.2
  classification, so a health flip never waits up to 30 s for the next
  probe. An admin clicking "test connection" against a dead backend and
  watching the dot go Red is correct: it measured the backend.

State machine: `Disabled` (no stored `llm` config row — no probe runs,
fresh submits fail fast with the actionable configure message instead of
parking; the admin dot reads grey, "no backend configured"), `Green` (last
observation ok), `Yellow` (1–2 consecutive failures, or ok with the last
*model-call* success older than 10 min while runs are attempting), `Red`
(≥3 consecutive failures). Two clocks: `last_observation` (any ok — probe
or model call) and `last_model_success` (the existing `LLM_LAST_SUCCESS`,
real completions only — the Yellow staleness rule and the existing
`/metrics` gauge read it; the admin line's "last response" reads
observation). Tracks `down_since` (unix ts of the transition into
Yellow/Red; cleared on Green). The probe issues a raw `reqwest` request
(not `LlmClient`), so probe results never double-count through the passive
hooks. The probe task starts in `main.rs` (and is defused by the shutdown
token), re-reading the stored `llm` row each tick — row absent ⇒
`Disabled`.

### 3.2 Recovery classification (the core rule)

One classifier, used by chat and (for logging) the queue: an agent run that
ended with `AgentError::Llm(LlmError::Request(_))` or `LlmError::Status`
with `status >= 500` or `status` 401/403 **never reached a working model**
→ backend-down class. Everything else (EmptyResponse, 4xx other than
401/403, step cap, tool-arg errors, store errors) is a real run outcome
and keeps its current terminal handling.

### 3.3 Chat turns: the `waiting_backend` status

Migration `0009_chat_waiting.sql`: widen the `chat_turns` status CHECK
with `'waiting_backend'` (SQLite table rebuild per the CHECK rules; the
column additions ride the rebuild), add `park_state TEXT NOT NULL DEFAULT
'interrupted'` (claim provenance, §3.3 crash rule), `last_error TEXT NULL`
(unsealed failure metadata for the admin table, §3.6), and a partial index
on `status = 'waiting_backend'` (mirror of the resumable index). Existing
rows ride the rebuild unchanged.

State machine (new edges in bold):

- running → **waiting_backend**: two entry points. (a) Admission check:
  the runner reads `llm_health::state()` before entering the gate; on
  `Red` it finalizes `waiting_backend` immediately — never queue at the
  gate during a known outage. (b) Run-failure classification per §3.2:
  any run (fresh or resumed) whose agent error is the backend-down class.
  (History-rebuild failure stays `failed` — it is a store failure, not a
  backend failure.)
- **waiting_backend → running**: claimed by the resume worker on a pass
  where `llm_health` is not `Red`, via the existing once-guard shape but
  requiring `status = 'waiting_backend'` (new `claim_waiting`; **does not
  touch `attempts`** — waiting turns retry unboundedly; the backend-down
  failure returns them to `waiting_backend` at zero cost while Red).
- interrupted (crash-swept, `attempts = 0`) → running: unchanged
  `claim_resumable`, one shot, `attempts` → 1. A resumed turn that then
  hits backend-down goes to `waiting_backend` (attempts stays 1 — it is
  out of the crash-resume budget, into the unlimited weather pool).
- Terminal edges unchanged otherwise: a claimed turn that reaches the
  model and fails hard → `failed`; the Drop guard / shutdown-gate arms keep
  their current targets, except the Drop guard's resumed arm follows (b):
  backend-down class → `waiting_backend` is impossible in Drop (no
  classification available — Drop stays `interrupted`/`failed` exactly as
  today).

Crash interaction (provenance rule): both claim paths stamp where the turn
came from inside their conditional UPDATE — `claim_resumable` sets
`park_state = 'interrupted'`, `claim_waiting` sets `park_state =
'waiting_backend'` — and the boot sweep re-parks every `running` turn to
its `park_state` (`CASE`). A waiting-claim killed mid-flight lands back in
`waiting_backend` — still weather, still retriable; a crash-resume killed
mid-flight lands back `interrupted` at `attempts = 1` — final per D7, the
policy its own crash history earned, never a downtime verdict. A turn
cannot be stranded by downtime anymore; content poisoning is impossible
because any failure the model actually produced is terminal (`failed`),
and while `Red` nothing is claimed at all.

Parking rows: entering `waiting_backend` appends NO transcript row —
re-parking on every retry tick during a multi-hour outage would otherwise
append hundreds of duplicate error rows and break the exactly-one-error-row
invariant (D4), which stays a property of terminal paths only. The failure
record is the turn's status plus `last_error` — unsealed metadata, bounded
512 (the `short_detail` convention), overwritten on each park.

Supersede and pruning follow the new state: `create_turn` abandons a
session's `waiting_backend` turn like any unanswered ask (the user's new
message replaces the old one); `create_session` pruning skips sessions
holding a `waiting_backend` turn (still "owes an answer");
`session_meta.turn_resumable` includes `waiting_backend`; the /chat status
bubble renders a waiting note: "waiting for the librarian backend — it will
resume automatically." Fix from the audit: supersede's `interrupted`
abandonment keeps filtering `status = 'interrupted'` (abandoning a
final attempts=1 interrupted turn is correct; the meta-definition
mismatch is fixed by making resumable/expiry consistent per the above).

Fresh submits during a known outage: **accepted** (decision in §6), per the
admission-check edge (a) — the turn is created, immediately parked as
`waiting_backend`, and the SSE stream closes on the terminal (the client's
existing meta-poll + transcript replay shows it waiting, then finishing).

### 3.4 Mutation queue: no permanent verdict from boot

`recover_on_boot` staging path: master-key recovery failure becomes
`defer_retry` (the 30 s defer, mid-pass shape) instead of `mark_dead`.
Rows whose payload is genuinely absent/corrupt keep the dead verdict
(permanent condition). The mid-pass deadline+grace dead
(`queue_worker.rs:303-313`) is retained unchanged: an item that reaches
deadline + 300 s grace still without a recoverable key is documented as
requiring re-submission; the key comes back on its own via the service
seal, which needs no login, so the boot defer covers realistic outages.
The periodic drain (existing 60 s tick + Notify) is already the
"periodically retry, not just first boot" engine for pending rows — the
fix is only to stop boot from deadening what the ticks would recover.

### 3.5 Ingest: periodic pending sweep

`LibrarianWorker` gains a `spawn(shutdown)` loop (queue-worker shape:
Notify + 60 s ticker) running the same claim-and-run path the boot block
uses. `main.rs` keeps its boot pass (immediacy) but failure there is no
longer terminal for pending jobs. Ingest already tolerates LLM failure via
the heuristic outline fallback, so a periodic sweep is all outages need;
`recover_on_boot`'s fail-stale-running + requeue-pending stays.

### 3.6 Observability

**`SystemStats` / `/health` / `/metrics`** (observability only — the
degraded rule is NOT widened, matching the gate-fields precedent):
- `backend_state`: "green" | "yellow" | "red"; `backend_down_since_seconds`
  (None when green); `backend_checked_seconds_ago`;
  `backend_last_success_seconds` (existing field, now also fed by the
  probe's successes).
- `chat_waiting_count` (count of `waiting_backend` + resumable
  `interrupted` turns, all users), `queue_depth` (existing).
- `/health` JSON gains the `backend_*` + `chat_waiting_count` fields;
  `/metrics` gains `mycelium2_backend_up` gauge (0/1/2 for the three
  states), `mycelium2_backend_down_seconds`,
  `mycelium2_chat_waiting_turns`. The existing last-success gauge keeps
  its model-call semantics unchanged (the probe never feeds it); the
  health module's own observation clock feeds the new fields.

**Admin System page** (`/admin/system`) gains, above the existing tiles:

1. **Backend health indicator** — a red/yellow/green dot (CSS-styled span,
   `aria-label` + text state, not color alone) + one line:
   green: "Backend healthy — last response 12 s ago";
   yellow: "Backend unstable — N failures, down since 14:02";
   red: "Backend down since 14:02 (47 m) — retries are paused, items will
   resume automatically."
2. **Summary line** — "3 chat turns and 2 queued writes waiting — they will
   resume automatically when the backend returns." Zero-count parts drop.
3. **Waiting-items table** — every non-terminal item, newest outage first
   (oldest waiting first within class): kind (chat turn / queued write /
   ingest job), user (username), what it is (chat: session id + turn age;
   write: tool + receipt id + age + attempts; ingest: slug + age), state,
   last error detail (chat rows from `chat_turns.last_error`; queue rows
   from the row's `detail`, bounded 512 per `short_detail`). Chat
   transcript content is never shown or unsealed (metadata only).
   Sources: new admin-scoped `ChatStore::admin_waiting_turns()` (join
   users for the username, read `last_error`), existing queue scans,
   `pending_ingest_jobs`.
4. The page self-refreshes every 30 s (`<meta http-equiv="refresh">` on
   this page only — no new JS).

`/admin` librarian status tab keeps its job rows (no change) and the System
page's ingest rows link to it.

### 3.7 Configuration

No new admin fields in v1: probe interval 30 s, probe timeout 5 s, red
threshold 3, waiting-claim tick = the existing 60 s resume tick. (Builder
call; the existing `llm` row stays the single LLM config surface.)

### 3.8 Testing

- Classifier unit tests: Request / 5xx / 401 / 403 / 404 / EmptyResponse /
  StepCap / Store — classed per §3.2.
- Probe tests: mock server 200/502/401/refused/slow; state machine
  transitions (3 failures → Red, 1 success → Green, flapping → Yellow),
  `down_since` set/cleared.
- Chat integration (extend `chat_resume_integration.rs`): backend-down
  during a resumed run → `waiting_backend` (not interrupted-attempts-1);
  health Red → resume pass claims nothing; health returns → turn completes
  on a later tick with no restart; fresh submit during Red → parked
  waiting, completes after Green; supersede abandons a waiting turn;
  prune never deletes a waiting session; Drop-guard arms unchanged.
- Queue integration: boot staging sweep with unrecoverable key → deferred
  (pending), activates after the key is available; payload-absent still
  dead.
- Ingest: pending job survives a failed boot pass and runs on a later
  worker tick.
- /health + /metrics field pins; admin System page renders indicator +
  table (no-JS), XSS-escaped.

## 4. Phasing

1. **Recovery engine**: classifier + `llm_health` + probe task, migration
   0009, chat state machine + resume gating + fresh-submit parking +
   waiting bubble, queue boot defer, ingest worker loop. (The user-visible
   pain; the UI can ship after.)
2. **Observability**: health fields on /health + /metrics, admin indicator,
   summary line, waiting table, meta-refresh; docs updates (api.md,
   configuration.md, deployment.md) + AGENTS.md state line.

## 5. Decisions

- **D1** Backend-down class = transport error ∪ HTTP 5xx ∪ 401/403; 404
  means the router is up (path unknown) and is NOT backend-down.
- **D2** New `waiting_backend` status (not overloaded `interrupted`):
  weather-state and crash-state stay distinguishable; `attempts` keeps its
  crash-resume meaning only.
- **D3** `waiting_backend` retries are unbounded (claims only while
  health ≠ Red, zero-cost while Red); poisoning is prevented because any
  failure produced by an actually-reached model is terminal.
- **D4** Boot paths never issue permanent verdicts for recoverable
  conditions; permanent conditions (missing/corrupt payload, rejected
  fallback) keep their dead/failed verdicts.
- **D5** /health degraded semantics unchanged (backend red ≠ server ill);
  all new fields are observability.

## 6. Builder calls (user should veto on review if wrong)

- Fresh chat messages during a known outage are **accepted** and parked
  waiting (not refused 503): one recovery engine for everything; the
  capacity 429 stays for queue-full, which is a different condition.
- Probe endpoint `GET {base_url}/models`; any HTTP status except 5xx and
  401/403 is reachable.
- Probe 30 s / timeout 5 s / Red after 3 consecutive failures; admin page
  self-refresh 30 s.
- Waiting table shows metadata only — chat content is never decrypted for
  an admin listing.
- Yellow also covers "reachable but work piling up" (last success > 10 min
  while runs attempt).
- No stored `llm` row is `Disabled`, not an outage: fresh submits fail
  fast with the actionable configure message, nothing parks, the dot is
  grey.
- Parked turns carry no transcript row and no SSE error event; the stream
  just closes and the waiting state is SSR + meta-poll vocabulary only
  (no new `ChatSseKind`).
- Passive health hooks sit at `LlmClient`'s completion choke point
  (chat/drain/ingest/test-connection); the probe is a separate raw
  `reqwest` path and never self-feeds.
- The crash-stranded waiting-claim the builder check found is closed by
  the `park_state` provenance rule (§3.3), not by widening D7.

## 7. Implementation amendments (2026-10-29, at ship)

Recorded while implementing this spec on the `worktree-llm-outage-
recovery` branch; the implementation is the authority for these three
details, everything else stands as designed.

- **/health field name**: the outage-age field ships as
  `backend_down_seconds` (§3.6 said `backend_down_since_seconds`).
  It is an age in seconds (like `oldest_pending_age_seconds`), not a
  timestamp, and the `/metrics` gauge already shipped under the
  matching name `mycelium2_backend_down_seconds`.
- **§3.4 queue staging — skip-in-place supersedes defer-to-pending**:
  a staging row whose master key is unrecoverable stays `staging` and
  is re-checked on every drain pass (and every boot sweep), activating
  when the key becomes recoverable. The spec asked for a defer back to
  `pending`; skipping in place keeps the row out of the pending/due
  scans it would only churn through, with the same operator-visible
  outcome (the item waits for the key, never dies from key loss).
- **Park-on-Red only (clarification of §3.3a)**: the admission park
  fires exclusively on `Red`. `Disabled` keeps the fail-fast terminal
  path — "not configured" is not weather and would promise a resume
  that never comes — and `Yellow` proceeds. The spec text ("on `Red`")
  implies this; the code and chat_runner comments pin it.

Probe wiring as shipped: `mycelium_web::server::spawn_backend_probe`
(queue-worker-shaped ticker, first tick immediate = the boot health
seed; `main.rs` seeds `LLM_HEALTH.set_disabled()` before spawning so a
request never sees the static's trivial Green). An `llm` row that is
present but undecryptable feeds NO health observation (config problem,
not backend weather) and logs a warning per tick.
