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
  an outage.
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