# REST API (`/api/v1`)

All endpoints require authentication: a session cookie (browser) or an
API key as `Authorization: Bearer myc2-...` (machine). Browser writes
additionally require the CSRF token (header `x-csrf-token` or form
field `csrf_token`); bearer requests are CSRF-exempt.

## Concepts

### `GET /api/v1/concepts/{*path}`
Fetch one concept's markdown (`text/markdown`). Paths are bundle-
relative with a leading slash, e.g. `/api/v1/concepts/notes/todo.md`.

### `PUT /api/v1/concepts/{*path}`
Create/replace a concept. Body: `{"markdown": "---\ntype: Note\n..."}`.
The frontmatter must parse and `type` is required; frontmatter whose
`status` / `superseded_by` / `valid_to` fails write-time validation is
rejected with `400` carrying the concept parse error text. `201` on success.

### `DELETE /api/v1/concepts/{*path}`
Delete a concept. `204` on success. Deleting a path that no concept
occupies answers `404` (the re-delete contract — the store fails
loudly and the route maps it).

## Revisions

Every concept change records an undo revision first (the write-ahead
log — one full markdown snapshot per change). Four routes read and
restore that log. Scopes are the concept-store ones: `user` (the
caller's private bundle, the default), `skills` (the global skills
shelf), `library` (the book catalogs).

Permission matrix (mirroring the concept viewer's posture):

| Scope | Index | List/Snapshot | Restore |
|---|---|---|---|
| `user` | own bundle | own bundle | own bundle |
| `skills` | admin only | any user | admin only |
| `library` | admin only | global-read shelf (or admin) | never — always `403` |

`403`s answer before the store runs, so a forbidden restore cannot
probe whether a snapshot exists. Unknown or missing `scope` → `400`
(`user` when omitted); `n=0` or non-numeric → `400` (revisions are
1-based); a missing revision → `404`.

### `GET /api/v1/revisions?scope=<scope>`
The scope's per-path index — every path with revisions, live or
deleted:

```json
[{"path": "/notes/todo.md", "live": true, "latest_no": 7,
  "last_op": "update", "last_op_at": "2026-10-31T12:00:00+00:00"}]
```

Metadata only — payload text is never in the index. A deleted path
stays listed (`"live": false`) so its chain remains reachable.

### `GET /api/v1/revisions/list?scope=<scope>&path=</notes/todo.md>`
One path's chain, newest first:

```json
[{"revision_no": 7, "op": "update", "source": "web:alice",
  "summary": "concept updated",
  "created_at": "2026-10-31T12:00:00+00:00"}]
```

`op` is one of `create`, `update`, `delete`, `restore`, `merge`,
`consolidate`, `migration`; `source` is the actor's provenance stamp.
A path with no revisions (never had one, or out of the caller's scope)
answers `200 []` — indistinguishable between callers.

### `GET /api/v1/revisions/snapshot?scope=<scope>&path=...&n=<n>`
One revision's raw markdown (`text/markdown`), exactly as recorded.
The payload convention: a `create` row carries the created content;
every later row carries the pre-mutation snapshot (the concept as it
stood before that operation) — so restoring to revision `n` puts back
the content that row recorded. `404` when the revision (or the whole
chain) is absent or belongs to another scope.

### `POST /api/v1/revisions/restore?scope=<scope>&path=...&n=<n>`
Apply revision `n` through the normal stamped write path (the
ConceptStore restore): the restored content commits as a NEW revision
(`op: restore`, summary `restored to revision <n>`, source the
caller's provenance), so the restore itself is reversible. Restoring a
deleted chain's revision re-creates the concept at its original path.
The restored `status` is whatever the snapshot says — restore is
literal.

- `Content-Type: application/json` POST answers `200
  {"restored_to": n}` (the CSRF token rides the `x-csrf-token`
  header).
- A form POST (`application/x-www-form-urlencoded`, the `csrf_token`
  field) answers `303` redirect to `/concept?path=…&scope=…`
  — only that content type takes the redirect path; a form sent as
  `multipart/form-data` (also CSRF-checked, field or header) answers
  the `200` JSON instead.

A snapshot that no longer parses (frontmatter a current build rejects)
answers `400` with the concept parse error text — the same posture as
the PUT-concept route's lifecycle rejects. The library scope refuses
restores outright.

## Graph and search

### `GET /api/v1/graph`
The caller's private-bundle graph: `{"nodes": [...], "edges": [...]}`.
(Global scopes are never included — graph view is private by design.)

### `GET /api/v1/search?q=...&global=1`
Search the caller's bundle; `global=1` adds the global skills shelf
and library catalogs. Results are scope-tagged:

```json
[{"concept_path": "/my-book/book.md", "title": "My Book",
  "snippet": "...", "score": 3.0, "scope": "library"}]
```

`scope` is `"user"`, `"skills"`, or `"library"`. Library hits from
admin-private bookshelves are filtered out for non-admin callers.

## Books and passages

### `POST /api/v1/ingest` *(admin)*
Multipart book upload: fields `bookshelf` (name), `slug`, `title`,
`file` (markdown, ≤ the configured limit — default 32 MiB, admin-adjustable), plus the CSRF token. Runs the ingest
inline and returns `202`:

```json
{"job_id": "...", "book_id": "...", "slug": "my-book",
 "status": "done", "detail": "3 catalog concepts written"}
```

Errors: `400` unknown bookshelf/invalid input, `409` duplicate slug or
user already has a job running.

### `GET /api/v1/ingest` *(admin)*
List the 50 most recent ingest jobs.

### `GET /api/v1/ingest/{id}` *(admin)*
One job's status: `{"id": "...", "status": "done", "detail": "..."}`.

### `GET /api/v1/passages?resource=book://<slug>%23<anchor>`
Read a passage from the shared library stacks. Anchors:
`ch-<n>-<slug>` (chapter) or `sec-<n>-<m>-<slug>` (section); an empty
anchor returns the whole text (truncated at 128k chars). The book's
shelf must be global-read (or the caller is an admin).

## Skills

### `GET /api/v1/skills/{slug}/bundle`
Download a nested skill as a Claude-Code-shaped zip
(`application/zip`): `SKILL.md` (the hub concept's markdown), the
skill's companion concepts, and its manifest-declared scripts at
their manifest paths. Same visibility as the skills page — any user
may bundle a global skill; a private skill only from its own bundle.
Script payloads are md5-verified against the hub manifest during
assembly; a mismatch refuses the download rather than serving bad
bytes. The archive is deterministic — fixed entry order (`SKILL.md`,
companions sorted, scripts sorted), fixed mtime 1980-01-01, fixed
unix perms 0644, deflate — so two GETs of the same skill return
byte-identical bodies. `200` responses carry:

- `Content-Disposition: attachment; filename="<slug>-skill.zip"`
  (filename characters restricted to `[A-Za-z0-9._-]`; a slug that
  sanitizes to nothing falls back to `skill.zip`)
- `Cache-Control: no-store, no-cache, must-revalidate`

`404` when the slug names no nested skill — unknown slugs and legacy
flat skills answer identically (no oracle). Every other error
(traversal manifest, md5 mismatch, missing payload, key or zip
failure) is logged server-side and answered with a generic
`500 {"error": "internal error"}`.

Install into a Claude Code skills directory:

```sh
curl -fsSL -H "Authorization: Bearer myc2-..." \
  https://mycelium.example.com/api/v1/skills/pdf-to-markdown/bundle \
  -o pdf-to-markdown-skill.zip
unzip pdf-to-markdown-skill.zip -d ~/.claude/skills/pdf-to-markdown/
```

## Chat

Two surfaces drive the librarian agent: the session-scoped SSE API
(durable, multi-turn, one in-flight turn per session) and a stateless
one-shot endpoint. Both admit through the process-global LLM gate —
see [Configuration](configuration.md) for its tunables.

Message bodies are form-encoded (`message=<text>`). The two submit
endpoints and the stateless endpoint additionally require a browser
session cookie (they are the chat UI's endpoints): a bearer-only
client gets `401`.

### `GET /api/v1/chat/sessions`
The caller's sessions, newest-first by `updated_at`:

```json
{"sessions": [{"id": "...", "title": "...", "updated_at": "..."}]}
```

A title that will not decrypt renders as `(unreadable)` rather than
failing the list.

### `POST /api/v1/chat/sessions`
Draft create: creates the session (title derived from the message)
plus its first turn, then returns that turn's SSE stream. The
stream's first event is `session`, carrying the new id. A blank
message is `400 {"error": "empty message"}`.

### `POST /api/v1/chat/sessions/{id}/messages`
Submit into an existing session; returns the turn's SSE stream. Only
one turn may run per session, so a second submit while one is running
is `409 {"error": "a turn is already running in this session"}`.
Unknown and foreign ids both answer `404 {"error": "not found"}` (no
oracle).

### `GET /api/v1/chat/sessions/{id}/stream?after=<seq>`
Reattach to a session. `after` is the client's last-seen `seq`:
persisted rows after it replay, then the live tail follows until the
turn ends. An absent `after` is live-only (nothing replays). With no
turn running the stream replays and closes — poll semantics. User
rows never replay; a persisted assistant reply replays as
`assistant`, not `done`.

### `GET /api/v1/chat/sessions/{id}`
Session meta (the UI's liveness poll):

```json
{"id": "...", "title": "...", "turn_status": "running",
 "turn_resumable": false, "max_seq": 7}
```

`turn_status` is the latest non-abandoned turn's status (`running`,
`done`, `failed`, `interrupted`) or `null`; `turn_resumable` is true
only for an `interrupted` turn the resume worker may still claim;
`max_seq` is the transcript's highest `seq` (the `?after=` base).

### `DELETE /api/v1/chat/sessions/{id}`
Delete a session and its transcript: `{"ok": true}`. `409` while a
turn is running, `404` unknown/foreign.
`POST /api/v1/chat/sessions/{id}/delete` is the native-form variant —
same rules, `303` to `/chat` on success.

### Stream format

`text/event-stream` with `Cache-Control: no-store`. Each event is one
`data: <json>` line followed by a blank line. Every event carries
`type`. All but `queued` and `admitted` also carry `seq` — the
transcript row the event was persisted as, which clients use to drop
snapshot/live duplicates and as the `after` cursor when they reattach
(the draft-create `session` event has no row of its own and carries
`0`).

| `type` | Fields | Meaning |
|---|---|---|
| `session` | `session_id`, `seq` | draft-create only: the new session's id (first event) |
| `queued` | `position` | waiting for an LLM slot, at this 0-based queue position |
| `admitted` | *(none)* | a slot was granted; the run begins |
| `tool` | `name`, `detail`, `seq` | the agent invoked a tool |
| `done` | `reply`, `seq` | the live run finished with this reply |
| `assistant` | `reply`, `seq` | replay only: a persisted assistant reply |
| `error` | `error`, `seq` | the run failed with this message |

`queued` and `admitted` carry no `seq`: they are presentation, never
transcript rows, and never replay. `queued` is emitted on enqueue and
on every position change; `admitted` only for a turn that actually
queued — a turn admitted straight away emits neither. The stream ends
after `done` or `error` (or when the run's channel closes).

### Capacity

Both submit endpoints check the gate's waiter count once, before they
write anything. At the configured depth (`chat_queue_depth`) the
submit is refused with
`429 {"error": "LLM at capacity — queue full"}` and `Retry-After: 5`
— no session row and no turn row are created, so a full queue never
strands rows nobody will run.

An accepted turn waits up to `chat_wait_secs` for a slot. If that
bound elapses the turn finalizes `failed` and its error row (and
`error` event) reads
`LLM at capacity — turn not started (queue full)`. A turn still
queued when the server shuts down is finalized `interrupted` with no
error row instead, and is resumed once on the next boot.

### Failure rows

A terminal chat error row carries the backend's actual error text —
status and body for an HTTP failure, the transport reason for a
connection error — bounded to 512 bytes (cut on a character boundary,
suffixed `… [truncated]`) and escaped by the transcript path
like every other row. The
`the librarian is unavailable (no LLM backend reachable) — configure one in the admin portal`
wording is reserved for a backend that is not configured at all
(`backend_state: "disabled"`); a backend that is reachable but refused
the request — a `400 prompt is too long`, say — reports that refusal
instead of sending the user to the admin portal to fix something that
is already configured. A backend that is merely down produces no error
row at all: the turn parks as `waiting_backend` and retries when the
backend recovers.

### `POST /api/v1/chat`
The stateless one-shot: one message in, one reply out, no cross-call
memory. Returns `{"reply": "..."}`. Waits up to `query_wait_secs` for
an LLM slot; on timeout
`429 {"error": "LLM at capacity — try again shortly"}` with
`Retry-After: 5`. `503` carries the same failure text as the chat
failure rows above: the backend's actual error text when a backend is
configured (transport, 5xx, or the refusal itself — a `400 prompt is
too long`, say), and the
`the librarian is unavailable (no LLM backend reachable) — configure one in the admin portal`
wording only when no backend is configured at all.

## Dream

### `POST /api/v1/dream`
Run a maintenance pass over the caller's private bundle (orphans,
broken links, likely duplicates, oversized concepts). Deterministic
signals decide whether the agent runs at all. Returns
`{"ran": true, "summary": "...", "files_changed": [...]}`, or
`{"ran": false, "reason": "..."}` when nothing needed doing. It is
wait-class on the LLM gate with the chat bound (`chat_wait_secs`); no
slot within it answers
`429 {"error": "LLM at capacity — try again shortly"}` with
`Retry-After: 5`.

## Health

### `GET /api/v1/health`
Detailed JSON health (authenticated). `GET /health` (public coarse
status plus the mutation-queue fields `queue_depth`,
`oldest_pending_age_seconds`, `queue_dead_count`,
`llm_last_success_seconds`, and the LLM-gate fields `llm_active`,
`llm_queued`, `llm_capacity`; it answers 503 `degraded` when the
database is unreachable, any queued item is dead, or a pending item
has waited past twice the integration deadline — the gate fields are
observability only and never make it degraded) and `GET /metrics`
(Prometheus text) live outside `/api/v1` and are public by design —
for container orchestration and scrape endpoints.

**Backend-health fields** (outage recovery spec §3.6, on `/health`):

- `backend_state` — the reported LLM-backend state: `"green"` (last
  observation ok), `"yellow"` (1–2 consecutive probe failures, or
  reachable-but-stale while runs are attempting), `"red"` (≥3
  consecutive failures), or `"disabled"` (no stored `llm` config row —
  nothing to probe).
- `backend_down_seconds` — seconds since the outage opened (the first
  failure); absent while green or disabled (a disabled machine's
  frozen clock is "nothing to probe", not an outage).
- `backend_checked_seconds_ago` — seconds since the last health
  observation (probe or model-call note); absent if never observed.
  Unlike `backend_down_seconds` this is NOT gated on disabled: when
  health was last actually checked is a fact, not an outage claim.
- `chat_waiting_count` — chat turns owed an answer: parked
  `waiting_backend` during an outage plus crash-interrupted turns not
  yet claimed (all users).

**Backend-health metric lines** (`/metrics`):

- `mycelium2_backend_up` (gauge) — four-state: `0` = disabled,
  `1` = red, `2` = yellow, `3` = green.
- `mycelium2_backend_down_seconds` (gauge) — seconds the current
  outage has been open; `0` when not down (green or disabled).
- `mycelium2_chat_waiting_turns` (gauge) — the same turns as
  `chat_waiting_count`.

The `/health` **degraded posture is deliberately unaffected by backend
state**: the LLM backend being red is weather the queue and chat
surfaces absorb (parked chat turns waiting for the backend; queued
writes retry with backoff and integrate through the deterministic
fallback), not illness of this service — 503 `degraded` still means MY
database/queue is ill.
`llm_last_success_seconds` / `mycelium2_llm_last_success_timestamp`
keep their model-call-only meaning: the health probe never feeds them,
so a reachable-but-completion-less backend never looks "recently
successful" there.

## Error shape

Errors are `{"error": "..."}` with an appropriate status code.
Internal details (SQL, paths, crypto errors) are never included.