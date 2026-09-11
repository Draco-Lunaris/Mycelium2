# Mycelium2

A secure, self-hosted knowledge base and memory server: an OKF
(Open Knowledge Format) concept store with per-user encrypted bundles,
global bookshelves, an MCP (Model Context Protocol) server for AI
agents, and an integrated librarian that catalogs uploaded books.

**Quick start (Docker):**

```sh
docker compose up -d
# Initial admin password:
docker exec mycelium2 cat /opt/mycelium2/data/config/initial-admin-password
# Open https://<host>/ (self-signed cert on first run)
```

**Quick start (binary):**

```sh
mycelium2 --data-dir ./data --https-addr 127.0.0.1:8443 --http-addr 127.0.0.1:8080
cat data/config/initial-admin-password
```

## Documentation

- [Deployment](deployment.md) — Docker, compose, TLS, reverse proxies
- [Configuration](configuration.md) — env vars, runtime config, LLM backend
- [Architecture](architecture.md) — crates, encryption model, data flow
- [API](api.md) — REST `/api/v1` reference
- [MCP](mcp.md) — AI agent integration (tools, auth, examples)
- [Backup and restore](backup.md) — backups, recovery keys, migration

## Features

- **Per-user encrypted bundles** — every concept file and the search
  index are encrypted with per-user keys (Argon2id → KEK → master key →
  HKDF path-bound DEKs, AES-256-GCM).
- **Global bookshelves** — admin-managed shelves, global-read or
  admin-private, service-key encrypted; full book texts live once in
  shared encrypted stacks.
- **Librarian** — upload a markdown book; an LLM (Ollama default,
  any OpenAI-compatible endpoint) extracts chapters/sections with a
  total heuristic fallback; read passages via `book://` anchors.
- **MCP server** — stateless MCP 2026-07-28 over Streamable HTTP at
  `/mcp`; per-user API keys as bearer tokens; 7 memory/skill tools.
- **Skills store** — private per-user skills and admin-managed global
  skills, both stored as OKF concepts (`type: Skill`).
- **Web UI** — login, bundle browser, concept editor, cross-scope
  search, graph view, admin portal (users, OIDC, LLM, bookshelves,
  book upload, backups).

## Security properties

- Data-free Docker image; all state under one data directory.
- TLS by default (auto self-signed or admin-supplied certs); HTTP
  redirects to HTTPS.
- CSRF double-submit on all browser writes; session cookies are
  HttpOnly + Secure + SameSite=Strict.
- Admin-only user creation, bookshelf creation, book upload, and
  global-skills writes.
- Private-shelf books never appear in user browse/search/passage reads.
- Structured JSON logs; no secrets in logs.

## License

Dual-licensed under MIT OR Apache-2.0 — see [LICENSE](../LICENSE),
[LICENSE-MIT](../LICENSE-MIT), and [LICENSE-APACHE](../LICENSE-APACHE).