# Configuration

## Environment variables / CLI flags

| Env var | Flag | Default | Meaning |
|---|---|---|---|
| `MYCELIUM2_DATA_DIR` | `--data-dir` | `/opt/mycelium2/data` | State directory (SQLite, encrypted files, config, certs, assets) |
| `MYCELIUM2_HTTPS_ADDR` | `--https-addr` | `0.0.0.0:443` | HTTPS listen address |
| `MYCELIUM2_HTTP_ADDR` | `--http-addr` | `0.0.0.0:80` | HTTP → HTTPS redirect listener |
| `MYCELIUM2_TLS_CERT` | `--tls-cert` | *(auto self-signed)* | TLS certificate PEM path |
| `MYCELIUM2_TLS_KEY` | `--tls-key` | *(auto self-signed)* | TLS private key PEM path |
| `MYCELIUM2_WEBAUTHN_ORIGIN` | — | `https://localhost` | WebAuthn relying-party origin — the full HTTPS origin (e.g. `https://mycelium.example.com`) passkey ceremonies bind to. Domain deployments must set it to their HTTPS origin or the browser refuses passkey ceremonies; invalid values fail boot |
| `RUST_LOG` | — | `info` | Log filter (e.g. `mycelium_web=debug,info`) |

## Service key

The service key encrypts all global-scope data (bookshelves, library
stacks, global skills). It is created automatically on first boot and
stored `0600` in `<data_dir>/config/service-key`. Back it up with the
data directory — without it, global data is unrecoverable.

## Runtime config (admin portal)

Stored in SQLite, edited via the admin portal — no environment
variables for runtime settings:

- **LLM backend** — OpenAI-compatible base URL + model, plus an
  optional API key (sent as a Bearer auth header when set, omitted
  otherwise — keyless endpoints like plain Ollama keep working). The
  key is sealed at rest with the service key and never rendered back
  (the form shows only whether one is stored); a blank save keeps the
  stored key. Default `http://localhost:11434/v1` (Ollama). Used by
  the librarian for chapter/section extraction and the /chat agent;
  unreachable backends fall back to a heuristic catalog (ingest never
  fails on LLM errors). A background probe checks the backend every
  30 s (`GET {url}/models`, 5 s timeout) to keep the health indicator,
  `/health`, and `/metrics` current — no new config fields for it; a
  deleted config row means `Disabled` (nothing to probe, grey dot), not
  an outage. `/admin/llm` has a **Test connection** button that makes one
  minimal completion with the submitted form values (a blank API key
  means the stored one) and persists nothing; its flash is deliberately
  bounded — a failure names the class (`HTTP 401`, "the backend did not
  answer", "the backend returned no content") and never carries the
  upstream body, because flash text rides a URL; the whole error goes to
  the server log.
- **Embeddings backend** — base URL + model (+ optional API key) for the
  vector half of retrieval, sealed at rest like the LLM row. Absent means
  vector retrieval is off, not broken. Fields, ranges and the
  live-apply rule: [Embeddings backend](#embeddings-backend-admin--embeddings).
- **Memory lifecycle** — retrieval fusion weights, the retrieval cap,
  revision retention, the dream signal thresholds, and the dream
  schedule (`dream_enabled` / `dream_interval_s`), all in one sealed
  `lifecycle` row edited on `/admin/dream`. Table:
  [Memory lifecycle and dreaming](#memory-lifecycle-and-dreaming-admin--dream).
- **Upload limits** — maximum book upload size (MiB, 1–255; default
  32, the original Mycelium's default). Takes effect immediately.
- **Security settings** — tunable within guardrails (out-of-range
  values are clamped, on save and on read):
  - Session TTL: 15–10080 minutes (default 720 = 12h; applies to new
    logins)
  - Login failures before lockout: 3–10 (default 3)
  - Lockout backoff cap: 30–3600 seconds (default 30)
  - Minimum password length: 12–128 (default 20)
  - Passage cap: 16384–1048576 chars (default 131072 = 128k)
- **OIDC SSO** — issuer URL, client ID/secret (encrypted at rest with
  the service key), redirect URI. Disabled until configured.

## LLM concurrency (Admin → LLM)

How many LLM runs may execute at once, and how long each surface will
wait for a slot. These are **not** environment variables: they live in
the sealed `llm` config row and are edited on `/admin/llm`, beside the
backend URL and model.

One process-global admission gate owns the slots. Chat turns, MCP
queries, the stateless chat endpoint and dream are *wait class* — they
join a FIFO queue and wait up to their surface's bound. The
mutation-queue drain and the chat resume worker are *try class*: they
take a slot only when nothing is queued and one is free, so
background work never runs ahead of an interactive waiter and never
blocks.

| Field | Default | Range | Meaning | Used by |
|---|---|---|---|---|
| `max_parallel` | 2 | 1–64 | Concurrent admitted LLM runs (the gate's slot count) | every surface |
| `per_user_active` | 2 | 1–16 | Admitted wait-class runs one user may hold at once; further turns keep their queue place rather than being rejected | every wait-class surface |
| `query_wait_secs` | 30 | 1–600 | Wait bound before giving up on a slot | MCP `memory_query`, stateless `POST /api/v1/chat` |
| `chat_wait_secs` | 600 | 1–7200 | Wait bound before giving up on a slot | chat-session turns, `POST /api/v1/dream` |
| `chat_queue_depth` | 64 | 1–1024 | Cap on total queued waiters; a chat submit beyond it is refused `429` before any row is written | chat-session submits |

Behavior:

- **Clamped, never rejected.** Out-of-range values are clamped into
  the range on save *and* on load, so a hand-edited or older row can
  never open an unbounded gate.
- **Blank keeps the stored value.** A field left empty — or carrying
  something that is not a number — keeps what is stored, so an old
  cached form still saves the URL/model without resetting the gate.
- **Applies immediately.** A successful save pushes the new limits
  onto the running gate: raising `max_parallel` admits queued waiters
  on the spot, lowering it only holds future admissions (a running
  agent is never preempted). No restart.
- **Synced at boot**, before the boot sweeps and the background
  workers start, so the stored row governs the first runs of the
  process rather than the built-in defaults.

What each bound does when it elapses: a chat turn finalizes `failed`
with a capacity message; MCP `memory_query` falls back to the
deterministic keyword search (it never errors on capacity); the
stateless chat and dream endpoints answer `429` with `Retry-After: 5`.
Exact bodies are in [API](api.md). Gate state is visible on `/health`,
`/metrics` and the admin System page (see
[Deployment](deployment.md)).

## Embeddings backend (Admin → Embeddings)

The vector half of retrieval: an OpenAI-compatible `/embeddings`
endpoint whose model turns concept text into the vectors the hybrid
composer fuses with the lexical hits (see
[API → search](api.md)). Not an environment
variable: the whole row seals under the service key as the `embeddings`
config row and is edited on `/admin/embeddings`.

**No row means vector retrieval is simply off** — the health dot reads
`Disabled`, the composer answers lexically, the vector worker idles, and
nothing anywhere errors. Saving a row turns it on and backfills the
corpus without a restart.

| Field | Default | Range | Meaning |
|---|---|---|---|
| `base_url` | — (required) | non-empty after trim | Endpoint base URL, e.g. `http://localhost:11434` (no `/v1` suffix — the client appends the path). An empty or whitespace-only value is refused with a flash and nothing is written: such a row would fail every embed at request time and read to the operator as an outage |
| `model` | — (required) | non-empty after trim | Model name sent in each request. Refused when empty for the same reason |
| `api_key` | none | — | Optional bearer token (`Authorization: Bearer …`), sealed at rest, never rendered back; a blank save keeps the stored key |
| `timeout_secs` | 30 | 5–300 | Per-request timeout |
| `batch` | 32 | 8–128 | Texts per `/embeddings` POST (the indexer's batch size) |

- **Dims are learned, not typed.** A successful probe stamps the model's
  dimensionality; a response wider than 4096 dims is rejected by name as
  a config mistake. Changing `model` marks every row written under the
  old model (or a different width) stale on the next worker tick, so the
  corpus re-embeds itself.
- **Clamped, never rejected** (same rule as every other row): out of
  range clamps on save *and* on load. **Blank keeps the stored value**
  for the two number fields.
- **Live without a restart.** The 30 s probe, the search composer and
  the vector worker all read the stored row per use; `/admin/embeddings`
  has a **Test connection** button that probes once with the submitted
  form (or the stored row once one exists — save first, then test) and
  flashes the verdict. A failure flash carries the HTTP status or "the
  backend did not answer" and never the upstream body; the full error
  goes to the server log.
- **Never on the LLM gate.** Embedding traffic is not an LLM run: it
  neither occupies nor waits behind the chat/query slots.
- **Tuning for the deployed model (bge-m3).** The production row serves
  `bge-m3` (Ollama) at `batch: 8`, `timeout_secs: 300` — a large model
  behind a latency-jittered host: 32-deep default batches overrun the
  request window and a cold backend's first embeds strain the 30 s
  default timeout. Failed embeddings are weather, not a restart: a
  batch retries three times, re-parks its items into the queue, and a
  later tick resumes them (the boot backlog defers through backend-Red
  ticks the same way) — a slow-starting backend needs no restart and
  nothing is lost.

## Memory lifecycle and dreaming (Admin → Dream)

One sealed `lifecycle` row holds every retrieval- and maintenance-side
tunable; `/admin/dream` edits all of it and also shows the last
scheduled run and the corpus-backfill state. A save clamps on the way
in, so the row at rest is always in range.

| Field | Default | Range | Meaning | Live how |
|---|---|---|---|---|
| `revision_retention` | 200 | 25–2000 | Revision rows kept per concept; older ones prune on the next write | static, re-stamped on save |
| `query_top_k` | 8 | 1–64 | Hits a zero-LLM `memory_query` returns when the caller sends no `limit` | static |
| `rrf_k` | 60 | 1–1024 | Fusion rank-softening constant: `weight / (k + rank)` | static |
| `rrf_weight_lex` | 1.0 | 0.0–10.0 | The lexical pool's fusion weight | static |
| `rrf_weight_vec` | 1.0 | 0.0–10.0 | The vector pool's fusion weight | static |
| `dedup_cosine` | 0.92 | 0.5–1.0 | Cosine at which the dream nominates a duplicate pair | static |
| `stale_days` | 90 | 7–3650 | An active concept unchanged longer than this is a staleness candidate | static |
| `recent_window` | 14 | 1–365 | A newer concept inside this window counts as "recently confirmed" against a stale candidate | static |
| `consolidation_patches` | 8 | 2–100 | Consecutive `update` revisions that mark a concept as disorganized (fires only when the count strictly exceeds it) | static |
| `dream_interval_s` | 86400 | 3600–2592000 | The dream scheduler's wake cadence | read per tick |
| `dream_enabled` | true | — | Whether the scheduler runs at all | read per tick |

Behavior, mirroring the LLM row:

- **Blank or non-numeric keeps the stored value** (an old cached form
  still saves the fields it carries); out of range clamps.
- **Nine static-bearing fields re-stamp on save** — the search composer,
  the retrieval caps, the retention pruning and the dream signals read
  process-global values, so "saved" means live, not "after a restart".
  The two dream-pair fields carry no static: the scheduler re-reads the
  sealed row each wake, which is why changing the cadence takes effect
  on the next tick.
- **`dream_enabled` is a checkbox with a marker**: the form always
  posts a hidden `dream_enabled_present` field, so a real submission
  with the box unticked means "off" while a POST that carries neither
  field (an old form, a synthetic request) leaves the stored flag
  untouched.
- **Boot order matters and is enforced**: retention syncs from the row
  before the packaged-skill seed and every boot sweep, because those
  write revisions that the retention rule then governs.

**Consolidation threshold vs revision retention.** The consolidation
signal counts consecutive `update` revisions in a concept's retained
history, and the two fields' ranges can contradict each other:
`consolidation_patches` goes as high as 100 while
`revision_retention` bottoms out at 25, and a concept whose threshold
sits above its retained history can never reach the count — the signal
silently never fires for it. The defaults are far from that corner
(8 patches against 200 retained revisions), and the counting itself is
correct in every combination: it reads what the log actually holds. An
operator who raises the threshold toward its ceiling should raise
retention with it; Mycelium2 does not refuse the pair, because both
values are individually legitimate.

## Data directory layout

```
<data_dir>/
├── config/          # service key, initial admin secrets, JWT keys
├── db/              # SQLite (WAL) — metadata, sessions, index tokens
├── users/<uuid>/    # per-user encrypted concept files
├── library/         # shared encrypted stacks (book texts, catalogs)
├── skills/           # global skills shelf (service-key encrypted)
└── assets/           # served static assets (CSS/JS)
```

## Security-relevant defaults

- Sessions: server-side, cookie `HttpOnly; Secure; SameSite=Strict`;
  TTL admin-configurable (default 12h, floor 15min).
- Login throttling with exponential backoff (threshold and cap
  admin-configurable within guardrails); TOTP/WebAuthn optional
  second factors.
- Forced password change for any admin-reset account (the first admin
  is created via `/setup` with an operator-chosen password, so no
  forced change applies).
- Password policy: minimum 20 characters.
- Uploads: books capped at the admin-configured limit (default 32 MiB); multipart CSRF enforced.
- MCP: per-user API keys (SHA-256 hashed at rest) as bearer tokens;
  the MCP endpoint enforces its own auth and is exempt from the
  session/CSRF gates.