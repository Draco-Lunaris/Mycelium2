# Mycelium2

A self-hosted memory, book, and skills server for AI agents, written in Rust and exposed over the Model Context Protocol (MCP).

Mycelium2 is built on two assumptions:

1. **The database is untrusted at rest.** A leaked database must not yield plaintext, a usable session, an API key, or a second-factor secret.
2. **The model is untrusted.** The LLM proposes changes to memory; code decides what is allowed, where it lands, and whether it worked.

It is a ground-up rewrite of [Mycelium](https://github.com/Draco-Lunaris/Mycelium) that keeps the Open Knowledge Format (OKF) memory model and adds per-user encrypted stores, user authentication, shared bookshelves, and a skills store.

## Status

In active development. The crypto, storage, authentication, MCP, and librarian layers are implemented and covered by 250+ tests. The web interface works and is being improved; expect it to change.

## What it does

- **Agent memory.** Agents store and query knowledge as markdown concepts with YAML frontmatter, cross-linked into a graph. Each user has a private, encrypted bundle.
- **Bookshelves.** An administrator uploads books; a librarian catalogs them into chapters and sections that agents can search and read by passage.
- **Skills store.** Agents fetch skills (SKILL.md-style markdown) by name: private skills per user and administrator-managed global skills.
- **MCP server.** Seven tools over Streamable HTTP at `/mcp`, authenticated per user.
- **Web interface.** Bundle browser, concept editor, search, graph view, chat, and an admin portal.

## Security model

### Encryption

Every concept file and every search index entry is encrypted. Nothing a user stores is written in plaintext.

```text
password ──Argon2id──▶ KEK ──unseals──▶ master key ──HKDF-SHA256──▶ per-file DEK ──AES-256-GCM──▶ file
```

- **Per-file keys.** Each file is encrypted under its own key, derived from the user's master key and bound to the file it protects.
- **Two-layer envelopes.** The outer layer is bound to the stored filename, so a file swapped under a known name fails to decrypt. The inner layer carries the true path, checked on read, so a file copied to another name fails too.
- **No path leakage.** Files are stored under flat, opaque HMAC filenames, so concept names and paths are not visible on disk.
- **Encrypted search.** Index terms are HMAC-SHA256 tokens under a per-scope key, and document payloads are AEAD envelopes. Search terms never reach the database in plaintext.
- **Cheap password changes.** Changing a password re-wraps the master key. File ciphertext is untouched.
- **Recovery key.** Generated at account creation and shown once. It unseals the master key if the password is lost.
- **Key types that can't be mixed.** `MasterKey`, `Kek`, `Dek`, `RecoveryKey`, and `ServiceKey` are distinct types, so using one in place of another is a compile error. Their debug output is redacted.

Shared data (bookshelves, book texts, global skills) uses the same envelope format under a service key.

### What a leaked database yields

| Stored item | At rest | Useful to an attacker with the database alone |
|---|---|---|
| Passwords | Argon2id hashes | No |
| Session IDs | SHA-256 hashes | No, the cookie value can't be recovered |
| API keys | SHA-256 hashes, shown once at creation | No |
| TOTP secrets | AES-256-GCM, sealed under the service key | No |
| OIDC client secret | Encrypted under the service key | No |
| User master keys | Sealed, never stored in plaintext | No |
| Concept files and search index | Encrypted as above | No |

### Authentication

- Local accounts with Argon2id hashing and a 20-character minimum password by default.
- Optional TOTP and WebAuthn second factors, and optional OIDC single sign-on.
- Ed25519-signed tokens and server-side sessions. A password change invalidates existing sessions.
- Login throttling with exponential backoff, per user and per IP.
- Two roles, `Admin` and `User`. No public registration: administrators create accounts.
- The first administrator is created on the `/setup` page with credentials the operator chooses. No generated password or bootstrap file is written to disk.
- Security settings are adjustable only within guardrails. Out-of-range values are clamped on save and on read.

### Web and transport

- HTTPS served directly by the binary, with a self-signed certificate generated on first run if none is supplied. HTTP redirects to HTTPS.
- Session cookies are `HttpOnly; Secure; SameSite=Strict`.
- CSRF protection on every browser write.
- Content-Security-Policy with a per-response nonce, plus `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: same-origin`, and HSTS.
- The container runs as a non-root user and ships with no data and no secrets.

### Trust boundaries

Be clear about what the encryption does and does not cover.

**The service key is the root secret.** It encrypts shared data and seals TOTP secrets. It also seals a copy of every user's master key, because the server has to serve API-key requests, where no password is present. Anyone holding the service key and the data directory can decrypt everything.

By default the service key is a `0600` file at `<data_dir>/config/service.key`, which means a full copy of the data directory, including a backup, contains both the ciphertext and the key. To keep the key out of the data directory and out of backups, supply it through the `MYCELIUM2_SERVICE_KEY` environment variable (64 hex characters) from wherever you manage secrets.

Mycelium2 does not protect against a compromised running server or a malicious operator, both of which hold the service key.

## AI controls

Agents reach Mycelium2 in two ways: as MCP clients calling its tools, and as the built-in librarian agent that reads and writes memory on a user's behalf. Both are constrained in code, not by prompt.

### MCP access

- **Per-user identity.** Each agent connects with an API key that belongs to one user. The key is resolved before a request reaches any tool handler; a missing or invalid key gets a `401`.
- **Scoped reads and writes.** Writes go only to the caller's own bundle. Queries read that bundle plus the shared material the user is allowed to see: global skills and books on shelves they can read.
- **Isolation by encryption.** Two users can't see each other's concepts because their data is under different keys, not because a query filter says so.
- **Immediate revocation.** Revoking a key blocks its next request.
- **Stateless.** Every request is authenticated on its own (MCP 2026-07-28). There is no session to hijack.
- **No internal detail in errors.** Expected failures return readable messages. Internal errors are logged and reduced to a generic message.
- **Skills are read-only over MCP.** Agents can list and fetch skills. No MCP tool writes to the global skills shelf, which only administrators can change.

### The librarian agent

The librarian is an LLM tool-call loop. Mycelium2 does not try to detect prompt injection in the content the agent reads. It limits what an injected instruction can do.

| Control | How it is enforced |
|---|---|
| Fixed tool set | Eight tools: five that read (`search_knowledge`, `read_concept`, `read_passage`, `list_directory`, `lint_knowledge`) and three that write (`write_concept`, `patch_concept`, `delete_concept`). No shell, no network, no filesystem. Unknown tool names are rejected. |
| Read-only queries | In query mode the write tools are refused in code, whatever the model asks for. |
| Write scope | Writes land only in the calling user's own bundle. Shared skills and the library are readable, never writable, by the agent. |
| Visibility | Library results and passages are filtered by the caller's bookshelf access before the model sees them. |
| Path safety | Concept paths are validated, reserved files (`index.md`, `log.md`, `info.md`) can't be written, and book references that try to escape the library are rejected. |
| Step cap | A run ends after 30 steps. |
| Bounded cost | Search terms are capped at 32 per query, because the model controls the term count and each term is an index lookup. Result sets and passage sizes are capped. Model requests time out. |
| One job at a time | One ingest job per user, claimed atomically. |

### Verify, don't trust

- **Maintenance is measured, not reported.** `memory_maintain` records graph health before the agent runs and measures it again afterward. The result shows the before and after counts from code, not the agent's account of what it did.
- **Every run is traced.** Each agent run records its input, every tool call with its arguments and the paths it touched, the final answer, the duration, and the outcome. Traces are stored encrypted in the caller's own scope.
- **Works without a model.** Every entry point has a deterministic fallback. If no LLM is reachable, memory tools and book ingest still work, and ingest never fails because of a model error.
- **Deterministic graph and search.** Edges, broken links, orphans, and ranking are sorted and stable, so the same inputs give the same outputs.

### Where your data goes

The LLM backend is whatever OpenAI-compatible endpoint the administrator configures. The default is a local Ollama instance. The model receives only what the agent reads for the calling user: that user's bundle, global skills, and the books that user is allowed to see. Nothing leaves the host unless you point it at a remote endpoint.

## Quick start

### Docker

```sh
docker compose up -d
```

Open `https://<host>/setup` and create the administrator account. The recovery key is shown once; save it.

### Binary

```sh
mycelium2 --data-dir ./data --https-addr 127.0.0.1:8443 --http-addr 127.0.0.1:8080
```

Then open `https://127.0.0.1:8443/setup`.

## Connect an agent

1. Sign in to the web interface, open **API Keys**, and mint a key. It is shown once.
2. Point your MCP client at the server:

```json
{
  "mcpServers": {
    "mycelium2": {
      "type": "http",
      "url": "https://mycelium.example.com/mcp",
      "headers": { "Authorization": "Bearer myc2-..." }
    }
  }
}
```

| Tool | What it does |
|---|---|
| `mycelium2_memory_query` | Answer a question from the caller's private bundle |
| `mycelium2_memory_add` | Store new knowledge as a concept |
| `mycelium2_memory_update` | Correct or extend an existing concept |
| `mycelium2_memory_status` | Bundle statistics and graph health |
| `mycelium2_memory_maintain` | Wire orphaned concepts into the graph and fix broken links |
| `mycelium2_skill_get` | Fetch a skill by name |
| `mycelium2_skill_list` | List available skills |

## Architecture

One Rust binary, nine crates, SQLite for metadata, and encrypted files on disk. No external services are required; the LLM backend is optional.

| Crate | Responsibility |
|---|---|
| `mycelium-core` | OKF parsing, links, graph, book anchors, search |
| `mycelium-crypto` | Key derivation, envelopes, key sealing |
| `mycelium-store` | SQLite metadata, encrypted file repository, encrypted search index |
| `mycelium-auth` | Accounts, sessions, TOTP, WebAuthn, OIDC, API keys, roles |
| `mycelium-web` | HTTPS server, REST API, web interface, admin portal |
| `mycelium-mcp` | MCP server and tools |
| `mycelium-librarian` | Librarian agent and book ingest |
| `mycelium-server` | The `mycelium2` binary |
| `mycelium-cli` | The `mycelium2-cli` admin tool |

## Documentation

- [Deployment](docs/deployment.md)
- [Configuration](docs/configuration.md)
- [Architecture](docs/architecture.md)
- [REST API](docs/api.md)
- [MCP integration](docs/mcp.md)
- [Backup and restore](docs/backup.md)
- [Design decisions](DESIGN.md)

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three run in CI on every change, along with a release build and a smoke test of the binary. Minimum supported Rust version: 1.88.

## Known limitations

- The web interface is still being improved.
- Single node only. There is no distributed or multi-node storage.
- The service key sits in the data directory unless you supply it through the environment (see [Trust boundaries](#trust-boundaries)).
- A backup taken from the admin portal is a best-effort snapshot of a live database. For a consistent one, stop the server and use `mycelium2-cli backup`.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
