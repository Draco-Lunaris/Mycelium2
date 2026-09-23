# Mycelium2 — Design Plan

> Status: design decisions captured. Ready to move to architecture and implementation planning.

## 1. Goals

- **Ground-up Rust rewrite** of Mycelium, preserving the OKF knowledge-base model and book library.
- **All current Mycelium functionality** is recreated: OKF graph storage, search, shelf management, book ingest/catalog, MCP tools, web UI, and chat.
- **Per-user encrypted stores**: every user has their own encrypted OKF bundle.
- **User authentication** for the web interface, with self-contained local accounts and optional OIDC SSO.
- **Global read bookshelves**: an administrator can create bookshelves and mark them as globally readable by all users.
- **Skills store**: agentic skills (SKILL.md-style markdown files) are stored as OKF concepts — private per-user skills and admin-managed global skills — so Mycelium2 acts as a central memory and skills store.
  A packaged skill (`pdf-to-markdown`, PDF-book conversion) ships embedded
  in the server binary and is seeded into the global skills shelf on boot;
  refresh semantics mirror the default assets (version marker, admin edits
  preserved between bumps).
- **As standalone as possible**: container-friendly, local embedded services only (no cloud dependencies).

## 2. Non-goals (for this phase)

- Multi-node / distributed storage.
- Cloud object stores or managed databases.
- Mobile clients or third-party API integrations beyond MCP.

## 3. High-level architecture

```text
┌─────────────────────────────────────────────────────────────┐
│  HTTPS served directly by Rust binary (rustls)              │
│  HTTP port 80 redirects to HTTPS port 443                   │
└───────────────────────┬─────────────────────────────────────┘
                        │
┌───────────────────────▼─────────────────────────────────────┐
│  Mycelium2 Rust binary                                      │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐  │
│  │  Web API    │  │  MCP server │  │  Librarian agent    │  │
│  │  (axum)     │  │  (SSE)      │  │  (OpenAI-compatible)│  │
│  └──────┬──────┘  └──────┬──────┘  └──────────┬──────────┘  │
│         │                │                    │             │
│  ┌──────▼────────────────▼────────────────────▼──────────┐  │
│  │                 Core OKF engine                       │  │
│  │   graph scanner │ search index │ shelf manager      │  │
│  │   ingest        │ book library │ crypto layer       │  │
│  └───────────────────────┬────────────────────────────────┘  │
│                          │                                   │
│  ┌───────────────────────▼────────────────────────────────┐  │
│  │         Embedded storage (SQLite + filesystem)         │  │
│  │   per-user encrypted OKF files + search index          │  │
│  │   shared encrypted library stacks (service key)        │  │
│  └────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

## 4. Crate layout

A Cargo workspace with separate crates from day one:

| Crate | Responsibility |
|-------|----------------|
| `mycelium-core` | OKF parsing, graph scanner, shelf model, book library, search index abstractions. |
| `mycelium-crypto` | Per-user key derivation, file-level encryption/decryption, key sealing. |
| `mycelium-store` | Embedded storage backend: SQLite metadata, encrypted file I/O, transactions. |
| `mycelium-auth` | Local accounts (Argon2, JWT Ed25519, TOTP, WebAuthn, RBAC), OIDC SSO, sessions/API keys. |
| `mycelium-web` | axum HTTP API, Leptos SSR+hydrate frontend, static asset serving, admin portal. |
| `mycelium-mcp` | MCP server implementation (SSE transport, per-user API key identity). |
| `mycelium-librarian` | In-process async worker that calls the configured OpenAI-compatible LLM for book ingest/catalog. |
| `mycelium-server` | Binary entrypoint: composes all crates, runs migrations, starts HTTPS/MCP. |
| `mycelium-cli` | Admin CLI for bootstrap, maintenance, and one-off operations. |

## 5. Authentication

- **Local accounts**: username + password, Argon2id hashing, Ed25519 JWT `access_token`, optional TOTP, optional WebAuthn as second factor, RBAC (`Admin`, `User`).
- **OIDC SSO**: configurable via the admin portal. No default provider; admin supplies issuer, client ID, etc. Maps external identity to a local user record.
- **API keys / PATs**: opaque random tokens, hashed in SQLite, shown once at creation. Used for MCP and automation, scoped to a user, revocable.
- **Session model**: server-side sessions stored in SQLite, session cookie.
- **RBAC**:
  - `Admin`: create bookshelves, upload/catalog books, manage users, configure OIDC/LLM, run maintenance.
  - `User`: read/search global bookshelves, read/search/edit their private OKF bundle.
- **Password policy**: minimum length 20 characters.
- **Account security**: exponential backoff on failed logins; optional TOTP; optional WebAuthn second factor.
- **First admin setup**: first web access opens `/setup`, where the operator creates the admin account with their own credentials; the recovery key is shown once in the response. No generated passwords or bootstrap files are ever written.
- **User registration**: admin-only; no public registration.

Pattern alignment: reuse the LPM/LHFM `*-auth` crate pattern (JWT EdDSA/Ed25519, Argon2, RBAC, OIDC provider model).

## 6. Encryption model

- **Per-user file-level encryption**: each OKF markdown file and the per-user search index are encrypted individually with per-user data encryption keys (DEKs).
- **Key hierarchy**:
  - User password → Argon2id → key encryption key (KEK).
  - KEK encrypts the user's master key.
  - Master key + HKDF-SHA256 per file path/content hash → per-file DEK.
- **Encryption primitive**: `ring` (AES-256-GCM or ChaCha20-Poly1305).
- **Key sealing**: the user's master key is encrypted by the KEK. Changing the password only re-encrypts the KEK wrapper, not the data.
- **Recovery**: a user recovery key is generated at account creation and shown once. Admins cannot recover user data without it.
- **Service key**: global bookshelves and shared library stacks are encrypted with a service-wide key. The librarian has full access; users get read-only access.
- **No secrets in image**: service key, passwords, and tokens live in host config/volumes, never in the Docker image.

## 7. Storage

- **Metadata**: SQLite via `sqlx` (async).
- **Migrations**: automatic on startup using sqlx migrate or refinery.
- **Data directory**: `/opt/mycelium2/data` by default, configurable. Layout:
  - `config/` — runtime config (SQLite-stored settings plus env secrets).
  - `db/` — SQLite database and migrations state.
  - `users/` — per-user encrypted OKF bundles (concepts, notes, private skills) and search indexes.
  - `library/` — shared encrypted book stacks.
  - `skills/` — global skills shelf (service-key encrypted, admin-managed).
  - `assets/` — web UI static files served from disk.
- **Search index**: custom encrypted inverted index stored in SQLite per user.

## 8. Web interface

- **Frontend**: Leptos SSR + hydrate. Web assets served from disk.
- **CSP**: strict CSP with nonces for the inline Leptos hydration script.
- **CSRF**: double-submit cookie pattern.
- **Pages**:
  - Login / OIDC callback / account settings / 2FA enrollment.
  - Private OKF bundle browser and concept editor (plain textarea with preview).
  - Private skills manager (create/edit skills, same editor).
  - Global bookshelf browser and `read_passage` viewer.
  - Global skills browser (read-only for users; admin edit).
  - Graph view (D3.js) fetched from `/api/v1/graph` REST endpoint, scoped to the user's private bundle.
  - Search results (user private bundle + global bookshelves + global skills by default).
  - Admin portal: users, OIDC config, LLM config, bookshelves, global skills management, book upload, maintenance, backup.
- **Editor**: plain textarea with markdown/YAML preview.

## 9. Bookshelves and librarian

- **Bookshelves**: only admins can create bookshelves. A bookshelf is either:
  - **Global-read**: encrypted with the service key, readable by all users, full control by librarian/admins.
  - **Admin private**: encrypted with the service key, private to admins.
- **Book upload**: admins only. Multipart upload → in-process librarian worker → LLM extracts chapters/sections → catalog concepts written to the target bookshelf, full text written to shared encrypted library stacks.
- **Shared library stacks**: full book text stored once, encrypted with the service key. Catalog entries reference `book://<slug>#<anchor>`.
- **Librarian concurrency**: one ingest job per user at a time.
- **LLM backend**: Ollama (`http://localhost:11434`) default, admin-configurable to any OpenAI-compatible endpoint.

## 10. MCP server

- **Protocol version**: MCP 2026-07-28 (stateless protocol core).
- **Transport**: Streamable HTTP (`/mcp`), not legacy SSE. Stateless requests land on any instance; no session handshake required.
- **SDK**: `rmcp` Rust SDK with `server` feature, configured for `ProtocolVersion::STANDARD_HEADERS` (2026-07-28).
- **Authentication**: per-user API key as bearer token, resolved before the request reaches the MCP handler.
- **Header-based routing**: the gateway and server honor `Mcp-Method` and `Mcp-Name` headers for routing and authorization.
- **Tools**:
  - `mycelium2_memory_query`
  - `mycelium2_memory_add`
  - `mycelium2_memory_update`
  - `mycelium2_memory_status`
  - `mycelium2_memory_maintain`
  - `mycelium2_skill_get` (fetch a skill by name — private first, then global)
  - `mycelium2_skill_list` (list available skills — private + global)
- **Default query scope**: user's private OKF bundle only.
- **Default write scope**: user's private OKF bundle only.
- **Global read**: queries can optionally include global bookshelves via an explicit scope parameter.
- **Skills via MCP**: `skill_get`/`skill_list` make Mycelium2 a central skills store for agents; skills are returned as full SKILL.md-style markdown so any agent runtime can consume them.
- **State**: no protocol-level session state. If a tool needs continuity, it mints an explicit handle and the model passes it back as an argument.
- **List caching**: `tools/list` responses include cache hints (`ttlMs`/`cacheScope`) so clients can cache the tool catalog.
- **Server-to-client requests**: not used. Sampling, roots, and logging are deprecated in 2026-07-28; elicitation is handled via Multi Round-Trip Requests (MRTR) if needed in the future.

## 11. Data model

- **OKF concept**: markdown file with YAML frontmatter (`type`, `title`, `description`, `resource`, `tags`).
- **User private bundle**: per-user encrypted OKF store for notes, concepts, and private skills.
- **Bookshelves**:
  - Admin-created.
  - Global-read or admin-private.
  - Book-only; enforced server-side.
- **Book library**:
  - `Book`/`Chapter` catalog concepts on bookshelves.
  - `book://<slug>#<anchor>` resources (`ch-<n>-<slug>`, `sec-<n>-<m>-<slug>`).
  - `read_passage` fetches decrypted text with the 128k-char cap preserved.
- **Skills**:
  - Agentic skills are OKF concepts with `type: Skill` (SKILL.md-style markdown: name, description, instructions, optional bundled resources).
  - **Private skills**: stored in the user's encrypted bundle (e.g., `/skills/<skill-name>.md`); only the owning user (and the librarian acting for them) can read them.
  - **Global skills**: admin-managed, stored in a global skills shelf encrypted with the service key; readable by all users, writable only by admins.
  - Skills are searchable like any other concept; the MCP `memory_query` tool can return skills, enabling Mycelium2 to act as a central skills store for agents.
- **User model**:
  - `id`, `username`, `email`, `role` (`Admin`/`User`), `auth_provider` (`local`/`oidc`), `created_at`.
  - Recovery key, TOTP secret, WebAuthn credentials.

## 12. Toolchain and dependencies

- **Language**: Rust (latest stable).
- **Async runtime**: `tokio`.
- **HTTP framework**: `axum`.
- **TLS**: `rustls` / `tokio_rustls`; HTTPS served directly by the Rust binary.
- **Auth**: `argon2`, `jsonwebtoken` (Ed25519), `totp-rs`, `webauthn-rs`, `openidconnect`.
- **Crypto**: `ring` + `hkdf` + `sha2`.
- **Storage**: `sqlx` + SQLite.
- **Search**: custom encrypted inverted index in SQLite.
- **Frontend**: `leptos` SSR + hydrate, `d3` for graph.
- **MCP**: `rmcp` Rust SDK with `server` feature, targeting MCP 2026-07-28 stateless streamable HTTP.
- **LLM**: OpenAI-compatible HTTP client (e.g., `reqwest` + custom types).
- **Observability**: structured JSON logs via `tracing`, Prometheus `/metrics` endpoint, detailed `/health` status.

## 13. Deployment

- **Primary artifact**: data-free Docker image published to GHCR.
- **Tags**: `latest` on every `main` push; semver tags on releases.
- **CI/CD**: GitHub Actions; conventional commits + automated semver.
- **Default ports**: HTTPS on `0.0.0.0:443`, HTTP redirect on `0.0.0.0:80`.
- **Certificates**: auto-generated self-signed cert on first run if none provided.
- **Reverse proxy**: not a primary path; haproxy can call upstream over HTTPS if desired.
- **Data directory**: `/opt/mycelium2/data` default, configurable via env.
- **Backup**: admin portal button triggers a full data directory backup (tar archive).

## 14. Security principles

- **Fail-closed**: no valid identity → no data access.
- **Least privilege**: RBAC, scoped API keys, no admin elevation without explicit role.
- **Encryption at rest**: every user file encrypted; keys derived/sealed, never stored plaintext.
- **No secrets in image**: keys, passwords, tokens live in host config/volumes.
- **Verify maintenance**: `memory_maintain` must be transactional and verifiable; never trust its self-report.
- **LLM isolation**: the configured LLM backend receives only decrypted content for the active user's request.
- **Audit**: structured logs for auth, admin, and data-modification events; configurable retention (default 90 days).
- **Rate limiting**: in-memory per IP/user exponential backoff.

### Trust boundary: the database is untrusted at rest

Anyone with read access to the data directory (host shell, stolen backup,
`docker exec`, a careless agent) is assumed to read the SQLite database and
`config/` freely. Everything security-relevant must survive that:

- **Sessions**: session ids (the cookie values) are stored SHA-256 hashed;
  a leaked database yields no usable session cookie. Legacy plaintext rows
  were invalidated by migration 0006.
- **TOTP secrets**: AES-256-GCM sealed under the service key (HKDF domain
  `totp-secret:v1`, `enc1:` prefix). Encrypted rows fail closed if the
  service key is unavailable. Legacy plaintext rows still verify (older
  deployments); new enrollment always encrypts.
- **API keys**: SHA-256 hashed (established Phase 4).
- **Passwords**: Argon2id hashes (established Phase 4).
- **User files**: two-layer AES-256-GCM envelopes under per-user master
  keys, sealed at rest under password + recovery key + service key.
- **Initial admin**: created only through the first-access `/setup` page;
  operator-chosen credentials; the recovery key is shown once in the
  response and stored nowhere. No bootstrap password/recovery files exist.
- **Remaining file-borne secret**: `config/service.key` (or
  `MYCELIUM2_SERVICE_KEY`). It unseals global data, TOTP secrets, and
  service-key master-key seals — protect it like the backups (which
  contain it).
- **Direct DB modification**: never used as an operational pattern; all
  account and data changes go through the authenticated web/MCP surface.
  Any manual DB edit on a live deployment is a trust-boundary violation,
  not a supported path. (On throwaway test hosts it may be acceptable —
  stop the server, edit offline, document it.)

## 15. Testing

- Unit tests in each crate.
- Integration tests that spin up the full server against a temporary data directory.
- Property-based tests for crypto where practical.

## 16. Documentation

- Markdown docs in `docs/` directory.
- Published to GitHub Pages.
- Rustdoc for crate APIs.

## 17. Migration / compatibility

- OKF format remains compatible; existing markdown files can be imported.
- `book://` anchor scheme is preserved.
- Existing global shelf content can be migrated into a global-read bookshelf by an admin.
- User migration path: admin creates account, user sets password, import bundle, re-encrypt under user key.

## 18. Success criteria

- An admin can log in, configure OIDC/LLM, create a global bookshelf, and upload/catalog a book.
- A user can log in, read/search global bookshelves, and read/search/edit their private OKF bundle.
- An admin can mark a bookshelf as global-read, and all users can see/search those books.
- MCP tools operate per-user against encrypted stores.
- The system deploys as a single data-free Docker image with no cloud dependencies.

## 19. Decisions log

| Decision | Choice |
|----------|--------|
| Frontend | Leptos SSR + hydrate |
| Search index | Custom encrypted inverted index in SQLite |
| MCP transport | Streamable HTTP (`/mcp`), MCP 2026-07-28 stateless |
| OIDC default | Disabled by default; configured via admin portal |
| Encryption crate | `ring` |
| Storage metadata | `sqlx` (async) |
| Web assets | Served from disk |
| LLM backend | Ollama default, OpenAI-compatible configurable |
| Bookshelves | Admin-created only; global-read or admin-private |
| User private data | Per-user private OKF bundle |
| First admin setup | First-access `/setup` page; operator-chosen credentials; recovery key shown once; no bootstrap files |
| User registration | Admin-only |
| MCP identity | Per-user API key as bearer token |
| Recovery | User recovery key; admin cannot recover |
| Password change | Separate KEK and DEKs; only KEK changes |
| Global bookshelf encryption | Service key; librarian full, users read-only |
| Book text sharing | Shared encrypted stacks with service key |
| Deployment artifact | Docker image (data-free) |
| Configuration | SQLite-stored runtime config + env secrets |
| Migrations | Automatic on startup |
| MCP tool names | `mycelium2_memory_*` |
| MCP protocol version | 2026-07-28 stateless |
| Graph library | D3.js |
| Editor | Plain textarea with preview |
| Librarian | In-process async worker |
| Librarian concurrency | One job per user at a time |
| RBAC roles | Admin, User |
| API versioning | `/api/v1/...` |
| Sessions | Server-side sessions in SQLite |
| TLS termination | Rust binary serves HTTPS directly |
| Certificates | Auto self-signed on first run |
| HTTP redirect | Redirect HTTP to HTTPS on port 80 |
| Listen address | `0.0.0.0` |
| HTTPS port | `443` |
| Data directory | `/opt/mycelium2/data` default |
| Backup | Admin portal button; full data directory backup |
| Logging | Structured JSON logs |
| Metrics | Prometheus `/metrics` + logs |
| Health | Detailed `/health` status |
| Testing | Unit + integration tests with temp data dir |
| CI/CD | GitHub Actions |
| Registry | GHCR |
| Tags | `latest` + semver |
| Versioning | Conventional commits + automated semver |
| Docs | Markdown docs on GitHub Pages |
| Feature flags | Single binary, no feature flags |
| Password policy | Minimum length 20 |
| Lockout | Exponential backoff |
| TOTP | Optional; admin can require |
| WebAuthn | Second factor only |
| Rate limiting | In-memory per IP/user |
| Audit logs | Structured logs only; configurable retention (default 90 days) |
| CSRF | Double-submit cookie |
| CSP | Strict CSP with nonces for inline scripts |
| Graph data API | REST `/api/v1/graph` endpoint |
| Graph scope | User private bundle only |
| Search scope default | User private bundle + global bookshelves + global skills |
| MCP query scope | User private bundle only |
| MCP write scope | User private bundle only |
| MCP protocol version | 2026-07-28 stateless |
| Librarian writes | Admin bookshelves + shared stacks |
| User upload | Users cannot upload books |
| Book upload permissions | Admins only |
| Skills model | OKF concepts with `type: Skill`; private in user bundle, global in service-key shelf |
| Skills MCP tools | `mycelium2_skill_get`, `mycelium2_skill_list` |
| Global skills management | Admin-only writes; all users read |
