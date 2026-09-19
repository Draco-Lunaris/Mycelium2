# Configuration

## Environment variables / CLI flags

| Env var | Flag | Default | Meaning |
|---|---|---|---|
| `MYCELIUM2_DATA_DIR` | `--data-dir` | `/opt/mycelium2/data` | State directory (SQLite, encrypted files, config, certs, assets) |
| `MYCELIUM2_HTTPS_ADDR` | `--https-addr` | `0.0.0.0:443` | HTTPS listen address |
| `MYCELIUM2_HTTP_ADDR` | `--http-addr` | `0.0.0.0:80` | HTTP → HTTPS redirect listener |
| `MYCELIUM2_TLS_CERT` | `--tls-cert` | *(auto self-signed)* | TLS certificate PEM path |
| `MYCELIUM2_TLS_KEY` | `--tls-key` | *(auto self-signed)* | TLS private key PEM path |
| `RUST_LOG` | — | `info` | Log filter (e.g. `mycelium_web=debug,info`) |

## Service key

The service key encrypts all global-scope data (bookshelves, library
stacks, global skills). It is created automatically on first boot and
stored `0600` in `<data_dir>/config/service-key`. Back it up with the
data directory — without it, global data is unrecoverable.

## Runtime config (admin portal)

Stored in SQLite, edited via the admin portal — no environment
variables for runtime settings:

- **LLM backend** — OpenAI-compatible base URL + model. Default
  `http://localhost:11434/v1` (Ollama). Used by the librarian for
  chapter/section extraction; unreachable backends fall back to a
  heuristic catalog (ingest never fails on LLM errors).
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