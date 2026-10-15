# LLM Concurrency Layer — Design

**Date:** 2026-10-15
**Status:** Approved design, pending implementation plan
**Scope:** mycelium-librarian (gate), mycelium-web (chat, stateless chat, admin, health/metrics), mycelium-mcp (memory_query)

## 1. Goal

Make the LLM layer behave like a multi-user service:

- An **admin-settable parallel-call limit** (how many LLM runs execute at once), tuned to the backend's real capacity (CCR/Ollama).
- **Async queues for reads**: queries and chat turns wait in a fair FIFO when all slots are busy, with visible position, bounded waits, and explicit overflow behavior — never a silent hang.
- **Fairness**: no single user can monopolize the slots.
- **Observability**: queue depth, active runs, and admission stats on `/health`, `/metrics`, and the admin page.

Non-goals (rejected, see §2):

- Durable SQLite query queues (mutation-queue parity). A query answered minutes later is stale, the MCP tool protocol is request→response with no polling surface, and chat turns are already durable via the session-turn machinery.
- Token-level streaming, LLM provider-side rate limiting, role-based priority. All future candidates, none now.

## 2. Current state (the bottlenecks, with evidence)

| # | Bottleneck | Evidence |
|---|---|---|
| B1 | Concurrency hardcoded to 2 permits, process-global, not tunable | `crates/mycelium-librarian/src/agent.rs:26` `Semaphore::const_new(2)` |
| B2 | Queries and chat block invisibly in `acquire().await` — no bound, no position, no feedback; MCP tool calls and chat turns hang silently under load | `agent.rs:193` (run_query), `:216` (run_mutation), `:301` (run_chat_streaming) |
| B3 | No overflow policy or per-user fairness; many detached chat runners can pile onto the semaphore | `chat_runner.rs:213` spawns unbounded runners; only per-session one-in-flight exists (DB partial unique index) |
| B4 | No observability of LLM pressure: `/health` and `/metrics` carry write-queue counts only; the admin page has no LLM-run readout | Phase-11 health fields; `admin_save_llm` form has only url/model/api_key |

Preserved invariants (must survive this design):

- **Query priority**: the mutation-queue drain worker and the chat resume worker use `try_acquire` only — they never block and never steal capacity from interactive queries (`queue_worker.rs:287`, `chat_resume.rs:161`; pinned by `drain_defers_while_queries_wait` and the chat_resume query-priority test).
- **One in-flight turn per chat session** (migration 0008 partial unique index; double-submit → 409).
- **MCP `memory_query` always answers**: LLM-unreachable → deterministic keyword fallback (`mcp/src/tools.rs:194+`).

## 3. Design

### 3.1 The gate: `mycelium-librarian::llm_gate`

A process-global admission gate replacing `agent::LLM_RUNS`:

```rust
pub struct LlmGate {
    capacity: AtomicUsize,          // admin-set slot count
    active: AtomicUsize,            // runs currently admitted
    queue: Mutex<VecDeque<Waiter>>,  // FIFO of wait-bounded waiters
    per_user: Mutex<HashMap<Uuid, usize>>, // admitted per user
    stats: GateStats,               // counters + wait-duration ring
}
pub static LLM_GATE: LlmGate = /* init from defaults */;
```

Two admission classes:

- **Wait class** (chat turns, MCP queries, stateless chat, dream): enqueue, wait up to the surface's bound, then admit. The waiter receives position updates (see 3.4).
- **Try class** (mutation-queue drain, chat resume): `try_enter()` succeeds only when `queue.is_empty() && active < capacity`. This is the query-priority invariant in its cleanest form: background work never runs while anything interactive is queued, and never blocks.

Dispatch: when a slot frees (release), pop from the FIFO head; if the head's user is at the per-user cap, skip over it to the next waiter (head-of-line bypass for capped users — strict FIFO for everyone else). Slots apply to agent *runs* (one permit per `run_*` call, as today — an agent run is a multi-step LLM loop but holds one slot, unchanged semantics).

Per-user cap (`per_user_active`, default 2): counts **admitted** wait-class runs per user. A user's queued turns still wait (FIFO position preserved); the cap only limits how many of their runs hold slots simultaneously.

Hot resize: capacity changes take effect on the next dispatch decision — new admissions respect the new slot count; in-flight runs drain naturally (transient overshoot below the new lower cap; the gate never preempts a running agent). Shrinking below `active` simply holds admissions until runs release.

### 3.2 Configuration (extends the existing `llm` ConfigStore key)

Additive fields with serde defaults — existing stored rows parse unchanged:

| Field | Type | Default | Range | Meaning |
|---|---|---|---|---|
| `max_parallel` | u32 | 2 | 1–64 | concurrent admitted LLM runs |
| `query_wait_secs` | u32 | 30 | 1–600 | MCP query / stateless-chat wait bound |
| `chat_wait_secs` | u32 | 600 | 1–7200 | chat-turn wait bound |
| `per_user_active` | u32 | 2 | 1–16 | admitted wait-class runs per user |
| `chat_queue_depth` | u32 | 64 | 1–1024 | total chat waiters cap; submit → 429 beyond it |

Sync path: every consumer already loads the `llm` config per request to build `LlmClient`; the same load calls `LLM_GATE.set_capacity(max_parallel)`. `admin_save_llm` syncs immediately on save (the admin sees the effect without waiting for traffic). No restart, no new plumbing.

### 3.3 Surface behavior

**Chat sessions (the durable path).** Submitting a message (`POST /api/v1/chat/sessions/{id}/messages`) creates the turn as today, then the detached runner enqueues on the gate. While queued, the runner emits SSE queue events; on admission the normal step events begin. Overflows:

- **Global queue depth cap** (`chat_queue_depth`, default 64 total waiters): submit → **429 `LLM at capacity`** *before* any turn row is created (no zombie turns; the client can retry).
- **Wait bound exceeded** (`chat_wait_secs`): the runner finalizes the turn as `failed` with a clear capacity message (persisted — reconnecting clients see it).
- **Per-user submission throttle**: a user already holding `per_user_active` admitted runs gets their new submissions queued behind (FIFO), not rejected — the cap binds at dispatch, not at submit. Combined with the depth cap this bounds any one user's footprint.

**SSE additions**: `queued` (with position, emitted on enqueue and on position change) and `admitted`. `chat.js` renders a small "queued (position N)" status line. The turn's DB status stays `running` from creation as today (the queue events are presentation, not lifecycle).

**MCP `memory_query`.** Bounded wait (`query_wait_secs`); on timeout the existing deterministic keyword fallback answers (the tool degrades, never hangs, never errors on capacity). No protocol change.

**Stateless `POST /api/v1/chat`.** Bounded wait → on timeout **429** with `Retry-After: 5` (today's 503 stays for LLM-unreachable/unconfigured).

**Dream (`run_dream`).** Wait class with the chat bound (admin-triggered, rare; never blocks other users longer than one slot-release cycle).

**Mutation queue drain & chat resume.** Try class (§3.1). The drain's existing 3-attempt/backoff/dead logic is untouched — only its capacity check moves from `LLM_RUNS.try_acquire()` to `LLM_GATE.try_enter()`.

**Agent entry points.** `run_query`, `run_chat_streaming`, `run_mutation` lose their internal `LLM_RUNS.acquire()`; admission moves to the callers (the surfaces know their class and bound). `run_mutation_capped` (the drain's entry) acquires nothing, as today.

### 3.4 Waiter positions

Each FIFO waiter holds its index; on any admission/release the gate wakes affected waiters so they can emit their new position. Positions are advisory (FIFO with per-user bypass means a waiter's position can move down when a capped head is skipped) — events carry the current observed position, not a promise.

### 3.5 Error handling

- Gate timeout is a distinct error kind (`GateTimeout`) so each surface maps it to its contract: chat → failed turn with capacity message; MCP → fallback; stateless → 429.
- Gate operations are infallible aside from timeouts (no I/O; the queue is in-memory). A panic in a run releases its slot via the existing `RunGuard`/permit-drop pattern (RAII guard on admission).
- Shutdown: the server's shutdown token already finalizes runners; queued waiters are released with `GateTimeout`-equivalent at drop (the process is exiting — no persistence work needed beyond what exists).

### 3.6 Observability

- `/health`: `llm_active`, `llm_queued`, `llm_capacity` (mirrors the write-queue fields; degraded rules unchanged — the gate never degrades health on its own).
- `/metrics`: gate counters — `llm_gate_active`, `llm_gate_queued`, `llm_gate_capacity`, `llm_gate_admissions_total`, `llm_gate_timeouts_total`, `llm_gate_wait_seconds` (cumulative + count for averaging).
- `/admin/system`: an LLM card readout (active/queued/capacity, recent timeouts) next to the existing queue table.
- `/admin/llm`: the four new form fields with ranges, validated server-side, saved through the existing config path.

### 3.7 Testing

- **Unit (llm_gate.rs)**: FIFO order; try-class blocked while queue non-empty (the query-priority invariant, stronger form); per-user bypass at cap; head position updates; hot resize (grow admits queued immediately; shrink holds admissions); wait-bound timeout; RAII release on drop; stats counters.
- **Integration**: chat 429 at depth cap (no turn row created); SSE queue events observed over the wire; chat turn failed with capacity message after a shrunk wait bound; MCP query falls back after timeout (mock slow LLM); stateless chat 429; drain defers while queue non-empty (extends the T4 pin `drain_defers_while_queries_wait` from permits to queue state); health/metrics fields present.
- **Existing pins updated**: tests holding `LLM_RUNS.acquire_many(2)` etc. re-pin against the gate's test surface (`LLM_GATE.set_capacity_for_test`, `acquire_test_hold`). The chat_resume query-priority test moves to `try_enter` semantics.

## 4. Phasing

1. **Gate + config + wiring**: `llm_gate` module, `llm` config fields, agent entry points stop self-acquiring, surfaces enqueue (chat/SSE events can land crude first), drain/resume on `try_enter`. All invariants pinned.
2. **Chat UX**: 429 depth cap, `queued`/`admitted` SSE events, chat.js status line, wait-bound turn finalization.
3. **MCP + stateless**: bounded waits, fallback on timeout, 429.
4. **Admin/health/metrics**: form fields, system card readout, health/metrics lines.

Each phase lands green (fmt/clippy/full tests) and is independently shippable. Phases 1 and 4 could merge as one PR; 2 and 3 each stand alone.

## 5. Decisions (settled)

- **In-process gate, not durable SQLite queues** (user-approved): reads need fairness and bounded waits, not receipts; chat turns are already durable; MCP's request→response contract has no polling surface.
- **Fail fast + degrade** (user-approved): chat 429s at the depth cap, failed-turn on wait timeout; MCP degrades to keyword fallback; stateless 429s. Never a silent hang.
- **Admission moves out of the agent entry points** into the surfaces — the agent layer stays a pure LLM/tool loop; the gate is a librarian-module peer of `agent.rs`.
- **Slots are per-run, not per-token-call** — unchanged from today's semaphore semantics; agent multi-step loops hold one slot.
- **Dream is wait-class** (admin-triggered, bounded by `chat_wait_secs`).
- `LLM_RUNS` is deleted, not deprecated alongside the gate — one concurrency mechanism, no double-gating.