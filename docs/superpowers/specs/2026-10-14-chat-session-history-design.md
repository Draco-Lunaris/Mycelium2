# Chat session history: durable per-user transcripts and detached, resumable turns

**Date:** 2026-10-15
**Status:** Approved design (decisions made interactively 2026-10-14/15); not yet implemented
**Baseline:** `bb05796`

## 1. Problem

The `/chat` page has no session history, and a chat turn does not survive the
browser leaving the page:

- The page renders an **empty log on every load** (`pages::chat_page` emits a bare
  `#chat-log` div). The server-side `ChatHistory` (state.rs — in-process
  `Mutex<HashMap<session_id, Vec<ConversationTurn>>>`, capped at 20 turns) has
  **no read API**, is keyed by *login* session (second device or re-login sees
  nothing), and is lost on restart and cleared on logout.
- `api_chat_stream` already detaches the agent into a background `tokio::spawn`
  that keeps running after the browser disconnects — but the assistant reply is
  recorded into `ChatHistory` **inside the SSE stream loop**. If the client is
  gone, the fully computed reply is dropped on the floor (`let _ = run_tx.send(...)`).
  Navigating away mid-answer destroys minutes of LLM work; the same run can hold
  one of the 2 global `LLM_RUNS` permits for up to 30 steps × 300 s.
- A restart mid-turn loses the conversation entirely (nothing durable exists).

Goal: sidebar session history per user, and a turn that — once submitted —
always completes server-side and lands in the transcript even if the browser
left, the tab closed, or the server restarted.

## 2. Decided

| # | Decision |
|---|----------|
| D1 | The durable, encrypted transcript is the source of truth. Three new tables (`chat_sessions`, `chat_messages`, `chat_turns`); SQLite holds metadata only, content is AEAD-sealed per user (the `search_docs` pattern). |
| D2 | Sidebar of sessions on `/chat` (`?s=<id>`; `?s=new` draft creates **no row until the first message**). Title derived from the first user message (`derive_title`), sealed at rest. |
| D3 | One in-flight turn per session, **enforced by a partial unique index** — a second submit gets 409. Busy is per-session; other sessions stay usable. |
| D4 | Detached runner persists **every** agent event (tool progress included) before broadcasting it; every event carries its persisted `seq`; clients dedup by `seq`. A Drop guard finalizes the turn and removes the registry entry, so a `running` row can never wedge. |
| D5 | Reattach = subscribe → replay rows after a cursor → live tail. Submit and reattach use the same replay+tail stream. |
| D6 | Restart: boot sweep marks `running` turns `interrupted`; a resume worker **auto-resumes once** (`attempts` 0 → 1) when the user's master key is recoverable, using `try_acquire` only (never steals an `LLM_RUNS` permit from interactive use). A second interruption is final (user-confirmed auto-resume). |
| D7 | Resume failure semantics: transient LLM failure → turn returns to `interrupted` (final, shown as interrupted, no loop); hard error (e.g. step cap) → `failed` + error row. |
| D8 | Submitting a new message in a session supersedes any resumable `interrupted` turn → `abandoned`. |
| D9 | Legacy `POST /api/v1/chat/stream` is **removed**; `ChatHistory` is **deleted**. The non-streaming `POST /api/v1/chat` stays as a **stateless** single-turn endpoint for programmatic callers. |
| D10 | Sessions are private per user — never visible cross-user, not even to admins. Foreign/unknown ids → 404 with no oracle. |
| D11 | Caps: 100 sessions per user (oldest non-running pruned on create); agent context = last 20 user/assistant pairs (matches today's `ChatHistory` cap). Tool rows are never fed back to the LLM. |
| D12 | Delete refuses while a turn is running (409). A native-form POST variant (`.../delete` → 303) keeps the sidebar delete button working without JS. |

## 3. Architecture

### 3.1 Store: `mycelium-store::chat` + migration `0008_chat_sessions.sql`

Mirrors `mutation_queue.rs` (SQL module in mycelium-store). All timestamps
RFC 3339 UTC strings; ids TEXT UUIDs (house conventions).

```sql
chat_sessions (
  id           TEXT PK,
  user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  title_sealed TEXT NOT NULL,            -- hex AEAD envelope
  created_at   TEXT NOT NULL,
  updated_at   TEXT NOT NULL
);
CREATE INDEX idx_chat_sessions_user ON chat_sessions(user_id, updated_at DESC);

chat_messages (
  session_id     TEXT NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
  seq            INTEGER NOT NULL,
  role           TEXT NOT NULL CHECK (role IN ('user','tool','assistant','error')),
  content_sealed TEXT NOT NULL,          -- hex AEAD envelope
  created_at     TEXT NOT NULL,
  PRIMARY KEY (session_id, seq)
);

chat_turns (
  id         TEXT PK,
  session_id TEXT NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
  user_seq   INTEGER NOT NULL,           -- seq of the triggering user message
  status     TEXT NOT NULL CHECK (status IN ('running','done','failed','interrupted','abandoned')),
  attempts   INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE UNIQUE INDEX uq_chat_turns_running ON chat_turns(session_id) WHERE status = 'running';
CREATE INDEX idx_chat_turns_resumable ON chat_turns(attempts) WHERE status = 'interrupted';
```

Operations (all ownership-scoped — every query filters `user_id`): `create_session`
(prune first: if the user has ≥ 100 sessions, delete oldest by `updated_at`
skipping sessions with a `running` **or resumable `interrupted`** turn — never
delete a session whose turn still owes an answer), `list_sessions` (newest-first),
`session_meta` (id, title, `turn_status`, `turn_resumable`, `max_seq` — the JS
liveness poll input), `delete_session` (refuse 409 if a turn is running),
`append_message` (assign `seq = MAX(seq)+1` **and bump the session's
`updated_at`** inside the insert transaction; SQLite write serialization +
one-writer-per-turn makes this safe), `messages_after(cursor)`
(transcript read / replay), `create_turn` (single transaction: abandon resumable
`interrupted` turns of this session, insert the user message (bumping
`updated_at`), insert the `running` turn — the unique index is the
double-submit guard), `claim_resumable` (conditional `UPDATE … SET
status='running', attempts = attempts + 1 WHERE status='interrupted' AND
attempts = 0` — the atomic once-guard, same shape as `claim_due_for_user`),
`finalize_turn(status)` (conditional on `status='running'`),
`boot_sweep` (`running` → `interrupted`).

### 3.2 Crypto: `ChatKeys`

Purpose-bound DEK from the user's master key via the existing HKDF derivation
helper, info string `mycelium2/chat-dek/v1` — same pattern as `IndexKeys`
(`search_index.rs`). `title_sealed`/`content_sealed` are hex AEAD envelopes under
that DEK (the `config_dek`/`search_docs` envelope format). No plaintext content
or titles in SQLite, ever. Tool rows store sealed JSON `{"name":…,"detail":…}`;
user/assistant/error rows store sealed plain text.

### 3.3 Runner, registry, and the one stream shape

- **`ChatRunRegistry`** (mycelium-web, in `AppState`):
  `Mutex<HashMap<turn_id, broadcast::Sender<ChatSseEvent>>>`. The submit endpoint
  creates the entry **before** spawning the runner; the runner removes it on
  finalization (and the Drop guard removes it if the task panics).
- **Runner** (spawned task, web side): rebuilds LLM history from the session's
  persisted `user`/`assistant` rows with `seq ≤ turn.user_seq` (last 20 pairs),
  then drives the existing `mycelium_librarian::agent::run_chat_streaming`
  (unchanged, MAX_STEPS 30, `LLM_RUNS.acquire()` as today — interactive use).
  For every `AgentEvent`: **persist the row first, then broadcast** the event
  carrying the row's `seq`. `Done` → assistant row + turn `done`; step-cap or
  hard error → error row + turn `failed`; LLM-unreachable on a fresh submit →
  error row + turn `failed` (visible bubble; the session stays sendable) and a
  `tracing::warn!` (fixes the known api.rs:3143 logging gap).
- **Stream shape** (identical for submit and reattach): subscribe to the turn's
  broadcast channel → replay persisted `tool`/`assistant`/`error` rows with
  `seq > cursor` (user rows are never replayed — they only appear at submit
  time when the submitting client is the originator) → forward live events →
  close after the terminal event. Event vocabulary (superset of today's):
  `{"type":"session","session_id":…}` (draft create, first event),
  `{"type":"tool","name":…,"detail":…,"seq":…}`,
  `{"type":"done","reply":…,"seq":…}`,
  `{"type":"error","error":…,"seq":…}`. When the registry has no live entry for
  the turn (already finished or never spawned), the endpoint replays rows after
  the cursor and **closes immediately** — a poll-shaped reopen is well-defined.
  No-dedup burden on the server: because
  every event is persisted before broadcast and carries its `seq`, a client that
  sees a row twice (snapshot + broadcast overlap) drops the duplicate by `seq`.
  A lagged broadcast receiver (or an unexpected stream close with the turn still
  running) is recovered by the client reopening the stream with
  `?after=<last-seen-seq>`; the cursor makes the ordering races harmless.

### 3.4 Boot sweep + resume worker

`main.rs`, after the existing sweeps: `chat_boot_sweep()` (running → interrupted),
then spawn the `ChatResumeWorker` (mycelium-web::chat; constructed in
`AppState::new` like the queue worker; wake loop = 60 s tick only — resumable
turns only ever appear at boot). Each pass: list users with `interrupted`,
`attempts = 0` turns; per user, in-session order: recover the master key
(`master_key_for` — cache or service-key seal recovery; on failure leave the
turn `interrupted` and retry next tick — no LLM cost); build scopes via the
same `build_agent_scopes` + `visible_library_slugs` the chat endpoints use;
`try_acquire` an `LLM_RUNS` permit (skip if none free — interactive use wins);
`claim_resumable` (atomic); run the **same runner** registered in the same
registry — so a user who opens the session mid-resume watches it live. The
runner's terminal handling is **path-dependent** (D7): a fresh-submit turn
finalizes `failed` on any error; a resumed turn finalizes `interrupted`
(attempts already 1 → final) when the LLM is unreachable — transient, no
side-effect doubt — and `failed` + error row on a hard error (e.g. step cap,
which means the run genuinely ran and could not converge). On success the reply
simply lands in the transcript.

Known accepted race: if the resume worker claims a turn in the same instant the
user submits a new message in that session, the submit's `create_turn` hits the
partial unique index → 409 with the resumed turn visible as running. Rare
(60 s tick vs. typing); honest; the user retries.

### 3.5 API surface (all under the existing session-auth + CSRF middleware)

| Endpoint | Behavior |
|---|---|
| `GET /chat` / `GET /chat?s=<id>` / `GET /chat?s=new` | SSR page (§3.6). |
| `GET /api/v1/chat/sessions` | List own sessions (id, title, updated_at). |
| `GET /api/v1/chat/sessions/{id}` | Session meta: id, title, `turn_status`, `turn_resumable`, `max_seq` (JS liveness poll; also the no-live-turn poll response). |
| `POST /api/v1/chat/sessions` | Draft flow: body `message` → create session + first turn → the submit SSE stream; first event is `session` with the new id (JS fixes the URL). |
| `POST /api/v1/chat/sessions/{id}/messages` | Submit → the same SSE stream. |
| `GET /api/v1/chat/sessions/{id}/stream?after=<seq>` | Reattach SSE (`after` optional; absent = live-only). |
| `DELETE /api/v1/chat/sessions/{id}` and `POST /api/v1/chat/sessions/{id}/delete` | Delete; POST variant → 303 to `/chat` (native sidebar form). 409 if a turn is running. |

Ownership check first on every `{id}` route: not the owner's id → 404
(indistinguishable from unknown). Bearer API-key callers pass the existing
middleware (CSRF-exempt) but sessions are a browser feature, not an API target.
`POST /api/v1/chat` (stateless one-shot, D9) keeps its current 503-on-unreachable
behavior and tests. Legacy `/api/v1/chat/stream` route is deleted.

### 3.6 UI

`/chat` becomes two-pane, all SSR-first:

- **Sidebar** (no-JS readable): session links `/chat?s=<id>` newest-first,
  active highlighted, HTML-escaped titles, per-session delete form (native POST),
  "New chat" → `/chat?s=new`.
- **Transcript**: SSR renders the selected session's rows in seq order — the
  same bubble markup `chat.js` builds, so JS appends seamlessly. A `running`
  last turn renders the pending "The librarian is thinking…" bubble;
  `interrupted`/`failed` render a small status note; `error` rows render as
  error bubbles. The page embeds `data-max-seq` and `data-running-turn` attrs.
- **Composer**: the existing `ChatComposer` island is untouched. `chat.js` is
  rewritten in place (still an external scaffolded asset — CSP: no inline
  scripts; `ASSETS_VERSION` bump): session state from `?s=`, per-session busy
  flag, the XSS-safe markdown renderer kept as-is, reattach on load when
  `data-running-turn` is set, stream-drop reopen with `?after=` cursor
  (≤ 3 attempts, 2 s backoff), draft flow via the create endpoint +
  `history.replaceState`, and `?s=new` renders an empty draft with the sidebar
  un-highlighted. **Liveness polling:** a page whose session shows an
  `interrupted` (or silently-ended) turn polls the session-meta endpoint
  (`GET …/{id}`, 5 s interval) until `turn_status` is no longer
  `interrupted`/`running` — this is how an already-open page notices the resume
  worker's claim and output (then it reopens the stream with `?after=`).
  A `running` status with no live SSE stream also reopens via meta polling
  (multi-tab: one tab streams, others poll).

No-JS: the page is read-only for chat (the composer is an island and is inert
without hydration — same as today); the sidebar and delete work without JS.

## 4. Tunables

| Value | Setting | Rationale |
|---|---|---|
| Sessions per user | 100 (prune oldest non-running on create) | Bounded storage; one runaway caller can't hoard rows. |
| Agent context | last 20 user/assistant pairs | Matches today's `MAX_CHAT_HISTORY`. |
| Chat step cap / LLM timeouts | unchanged (30 / 300 s / 10 s) | Existing. |
| Resume tick | 60 s | Turns become resumable only at boot; tick absorbs key-recovery latency. |
| Resume permit | `try_acquire` only | Query-priority invariant (Phase 11). |
| Registry channel capacity | 256 events/turn | Lag is recovered by cursor reattach, not buffering. |
| JS reopen attempts | 3, 2 s apart | Covers proxy idle-kills; transcript is the fallback truth. |
| Title | `derive_title(first message)`, ≤ 80 chars | Existing helper; sealed at rest. |

## 5. Error handling summary

- 400 empty message · 403 CSRF · 401/gate as today · 404 foreign/unknown id ·
  409 turn already running (double-submit, delete-while-running, resume race).
- LLM failure mid-turn → SSE `error` event + `failed` turn + error row +
  `tracing::warn!` (no generic stream death, no silent drop).
- Submit endpoints never preflight the LLM (the run is detached); there is no
  503 on the session endpoints — failures arrive as events.
- Runner panic → Drop guard finalizes (`failed` + error row) and removes the
  registry entry; a `running` row can never wedge the session.
- Process death mid-turn → boot sweep `interrupted` → resume once (D6/D7);
  second death is final and visible.
- Key unrecoverable at resume → stays `interrupted`, retried each tick, zero LLM
  cost, visible in the transcript.
- Anything unexpected → generic 500 logged server-side (house convention).

## 6. Testing

House patterns: temp data dirs, scripted mock OpenAI-compatible LLM, and the
request-capturing mock for LLM-payload pins. All under
`mycelium-store/tests`, `mycelium-librarian` (unchanged agent suite), and a
rewritten `mycelium-web/tests/chat_stream_integration.rs`.

1. **Store roundtrip:** create/append/list; sealed columns decrypt only with the
   owner's key; raw DB contains no plaintext (scan for a marker string);
   seq monotonic per session; cascade delete removes messages + turns.
2. **Ownership:** foreign session id → 404 on messages/stream/delete; list
   returns only own sessions.
3. **One-running invariant:** concurrent double-submit → exactly one stream +
   one 409 (unique index, not a JS flag).
4. **Submit → tool → done:** SSE order pinned; user/tool/assistant rows and
   `done` turn persisted; transcript query matches.
5. **Multi-turn context (replaces `chat_history_persists_across_messages`):**
   request-capturing mock pins the second turn's LLM payload =
   `[system, user1, assistant1, user2]` rebuilt **from session rows**.
6. **Navigate-away durability:** gated mock (holds the LLM response); drop the
   SSE client mid-run; release → assistant row + `done` turn persist with no
   listener attached (the today's-bug regression pin).
7. **Reattach:** fresh client with `?after=` replays missed tool rows then
   tails; duplicate `seq` events are dropped client-side; submit and reattach
   streams are byte-identical in shape.
8. **Restart + auto-resume once:** running turn → drop/reopen Store → boot
   sweep marks `interrupted` → resume worker (mock LLM) completes → assistant
   row, `attempts = 1`; a user watching the session sees it live (registry).
9. **Resume-once final:** interrupt the resume (second restart) → `interrupted`
   with `attempts = 1` → never claimed again; transcript shows the note.
10. **Resume failure split:** LLM dead at resume → back to `interrupted`
    (final, no loop); step-cap at resume → `failed` + error row.
11. **Supersede:** new message in a session with a resumable `interrupted` turn →
    old turn `abandoned`, new turn runs; LLM context excludes the abandoned
    turn's partial tool rows.
12. **Draft flow:** `POST /api/v1/chat/sessions` → first event `session` with id;
    title derived and sealed; second message routes to the `{id}` endpoint.
13. **Delete:** own → cascade gone + 303 (form variant); running → 409;
    foreign → 404.
14. **Cap:** 101st session prunes the oldest non-running session — and never
    one with a resumable `interrupted` turn.
15. **Auth/CSRF/guards:** no session → login gate; missing CSRF → 403; empty →
    400; `/chat` CSP standing sweep still passes; title `<script>` renders
    escaped; logout no longer clears anything (durable).
16. **Stateless `/api/v1/chat`:** request-capturing mock pins that a second call
    carries no prior context; bearer-key test unchanged.
17. **Meta endpoint:** own session → `{turn_status, turn_resumable, max_seq}`;
    foreign/unknown id → 404; a finished session's stream reopen (no live
    entry, `?after=max_seq`) replays nothing and closes — poll semantics
    pinned.

## 7. Out of scope

- Token-level streaming (events stay step-granularity, Phase 10 vocabulary).
- Session rename/pin/search; multi-tab push-sync of user rows (refresh shows truth).
- `/health` and `/metrics` changes (chat unavailability is already covered by
  the LLM fields; a stuck turn is structurally impossible per §5).
- Admin/cross-user visibility (explicitly never), session export tooling.
- `POST /api/v1/chat` beyond statelessness (its contract is otherwise frozen).

## 8. Success criteria

1. Send a message, navigate away or close the tab, return later — even after a
   server restart — and the full transcript (including the completed reply) is
   there. No computed reply is ever dropped.
2. A turn keeps running server-side after the browser disconnects; watching or
   not watching changes only whether you see it live.
3. Sidebar of sessions, per user, private; multiple sessions usable
   concurrently with one in flight each (DB-enforced).
4. Restart mid-turn → auto-resume completes the answer once the key is
   recoverable; a second interruption surfaces visibly and never loops.
5. Nothing readable in SQLite without the user's master key (metadata only).
6. Full verify suite green: `cargo check --workspace && cargo fmt --all --check
   && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.