# AGENTS.md — Mycelium2 (Rust Rewrite)

## Project identity

- Ground-up Rust rewrite of [Mycelium](https://github.com/Draco-Lunaris/Mycelium) (TypeScript/Node.js monorepo). Goal: a more secure implementation of the same OKF knowledge-base / memory system, plus per-user encrypted stores, user auth, global-read bookshelves, and a skills store.
- Design: `DESIGN.md` (goals, architecture, decisions log). Roadmap: `ROADMAP.md` (phases + Phase 1 detailed task breakdown).
- OKF = Open Knowledge Format: directory tree of markdown concepts with YAML frontmatter, cross-linked via bundle-relative absolute `.md` links; `book://<slug>#<anchor>` resources for book passages.

## Build and verify (all must pass before declaring done)

```
cargo check --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Run a single crate's tests: `cargo test -p mycelium-core`. Single test: `cargo test -p mycelium-core extract_section`.

## Workspace layout

Cargo workspace, 9 crates under `crates/`:

- `mycelium-core` — OKF engine: concept parsing, bundle walking, link scanning, graph building, book anchors/passage extraction, search trait + in-memory index. **Phase 1 complete.**
- `mycelium-crypto` — per-user key derivation, file encryption. **Phase 2 complete.** Key newtypes (`MasterKey`/`Kek`/`Dek`/`RecoveryKey`/`ServiceKey` — never mix them), Argon2id KEK, HKDF path-bound DEKs, AES-256-GCM envelopes (magic `MYC2` v1), `SealedMasterKey` (versioned, password + recovery seals), service key (env override or 0600 file, race-safe creation, permissive-perms rejected on load), `FileKeys`/`IndexKeys` (HMAC filenames/tokens + meta DEKs).
- `mycelium-store` — SQLite metadata + encrypted file repo. **Phase 3 complete.** `Store::open` (WAL, FKs, auto-migrations, data-dir layout), `FileRepo` (flat HMAC filenames, two-layer envelopes, atomic writes, user/service scopes), `EncryptedIndex` (HMAC tokens, AEAD doc payloads, per-user scope ids `user:<uuid>`, oracle-equivalent ranking), `ConfigStore` (typed JSON KV).
- `mycelium-auth` — local accounts, OIDC, TOTP, WebAuthn, RBAC. **Phase 4 complete.** `LoginService` (throttle + TOTP + session in one entry point; password change invalidates sessions), `UserStore` (Argon2id, sealed master keys), `SessionManager` (CSRF tokens, constant-time verify), `ApiKeyManager` (SHA-256 hashed), `JwtKeys` (Ed25519, typed Role), TOTP/WebAuthn (counter persistence, atomic challenge consume), OIDC (state+nonce verified), `bootstrap_admin` (files-first atomicity).
- `mycelium-web` — axum HTTPS + Leptos SSR frontend. **Phase 5 complete.** `AppState` (MasterKeyCache + service-key seal recovery), auto self-signed TLS (rcgen), session/bearer auth middleware, CSRF double-submit (header OR form field — native forms work), forced-password-change gate, CSP + security headers, XSS-escaped pages, `ConceptStore` facade (file → registry → index ordering), REST `/api/v1` (graph/search/concepts/health), admin portal (users, OIDC w/ encrypted secret, LLM, bookshelves), Prometheus `/metrics`, HTTP:80 redirect, graceful shutdown. Merges the MCP router at `/mcp` (shared store/service key/master-key cache; `/mcp` exempt from session/CSRF gates — MCP enforces its own bearer auth).
- `mycelium-mcp` — MCP 2026-07-28 stateless server via rmcp. **Phase 6 complete.** `McpState` (store + service key + shared `MasterKeyCache`), `require_bearer` middleware (401 + WWW-Authenticate), `MyceliumMcpServer` (`#[tool_router]` 7 tools + `#[tool_handler]`, `supported_protocol_versions() = [V_2026_07_28]` only), `StreamableHttpService` (stateless, per-request metadata required, JSON responses, allowed_hosts disabled w/ documented rationale), tools read per-request identity via rmcp's injected `http::request::Parts` extensions.
- `mycelium-librarian` — in-process book ingest worker (Phase 7 stub).
- `mycelium-server` — main binary (`mycelium2`).
- `mycelium-cli` — admin CLI (`mycelium2-cli`).

## Critical dependency facts (verified 2026-09-10)

- **MSRV 1.88** (rmcp 3.2, leptos 0.8.20, jsonwebtoken 11, totp-rs 6, webauthn-rs 0.5.5 all require it).
- **rmcp >= 3.2.0** is required for MCP 2026-07-28 (stateless). rmcp 0.16 maxes at 2025-06-18 — do not downgrade. Features: `server`, `macros`, `transport-streamable-http-server` (the last is REQUIRED for `StreamableHttpService`; `server` alone does not include it).
- rmcp 3.2 API facts: `STANDARD_HEADERS == V_2026_07_28`; stateless mode injects `http::request::Parts` (with axum extensions) into JSON-RPC request extensions → tool handlers read per-request state via `Extension<http::request::Parts>`; `#[tool_handler]` auto-generates `list_tools` with SEP-2549 cache hints (ttlMs=0, cacheScope=public) for ≥2026-07-28; `Parameters<T>` is a tuple struct (`args.0.field`); `ServerInfo = InitializeResult` (use `ServerInfo::new(caps).with_server_info(Implementation::new(...))`); rmcp does NOT re-export the `http` crate (add `http = "1.5"`); schemars 1.x (`schemars::JsonSchema` derive on tool arg structs).
- **serde_yaml is deprecated** — use `serde_yaml_ng` (drop-in).
- **sqlx stays 0.8.x** (0.9 needs Rust 1.94).
- rand 0.10: `rand::rng()`, `Rng` trait (not `thread_rng`/`RngCore`).
- reqwest 0.13: TLS feature is `rustls` (not `rustls-tls`).

## OKF format rules (mycelium-core)

- Frontmatter: `type` is the only required field; title/description/resource/tags/timestamp optional; unknown fields ignored (lenient).
- Reserved files, never concepts: `index.md`, `log.md` (auto-maintained), `info.md` (shelf metadata; `kind: book` marks book shelves).
- Graph edges: ONLY absolute leading-slash `.md` links (`[Foo](/foo.md)`). Relative `./` links, fragments (`/foo.md#x`), and non-md targets are ignored.
- **Divergence from original (user-confirmed)**: links inside code spans/fenced blocks are SKIPPED (original counted them — the memory_maintain regression bug class). Multi-backtick spans follow the CommonMark run-length rule.
- Book anchors: `ch-<n>-<slug>` / `sec-<n>-<m>-<slug>`, 1-based (0 rejected). Chapter extracts to next `# ` heading; section to next `## `/`# `. Cap: 128k chars (truncated).
- Skills are concepts with `type: Skill` — no special parsing.

## Conventions

- Deterministic output everywhere (sorted edges/broken links/orphans; stable search ranking).
- Errors: `thiserror` enums; stubs fail loudly (`Err(NotImplemented)`) rather than returning fake data.
- Tests: unit tests in-module + `tests/bundle_integration.rs` over `tests/fixtures/sample-bundle/` (fixtures double as OKF format documentation).
- Dev loop: verify (fmt/clippy/test) → judge subagent (PASS/FAIL vs spec) → code-reviewer subagent (fix Critical/Important findings) → re-verify.

## Current state (2026-09-10)

- Phase 1 (core OKF engine) complete: 53 tests green, clippy clean, judge PASS, review findings fixed.
- Phase 2 (crypto layer) complete: 46 crypto tests green (96 total), judge PASS (after service-key race fix), review findings fixed (key newtypes, seal versioning, perms check on load).
- Phase 3 (storage layer) complete: 24 store tests green (125 total), judge PASS (after per-user scope-id fix), review findings fixed (Uuid-typed user_dir, async file I/O, race-resilient search).
- Phase 4 (auth) complete: 40 auth tests green (165 total), judge PASS first run, review findings fixed (WebAuthn counter+expiry+atomic consume, OIDC state verify, LoginService with throttle/session-invalidation, JWT typed roles + 0600-at-create, bootstrap files-first atomicity).
- Phase 5 (web server + admin portal) complete: 172 tests green, judge PASS (after admin-RBAC/forced-change-gate/XSS/CSRF-form-field fixes, real-browser verified), review findings fixed (OIDC secret encrypted at rest, seal-failure logging, ConceptStore registry-before-index ordering, bearer-auth middleware for Phase 6, LoginService Mutex removed, dead modules deleted).
- Phase 6 (MCP 2026-07-28 server) complete: 173 tests green, judge PASS (46/46 empirical probes incl. two-user isolation, revoked-key rejection, protocol-posture checks), review findings fixed (broken-link flagging kept well-formed markdown, search-term cap 32 vs query-amplification DoS, slug-collision disambiguation instead of silent overwrite, consistent error surfacing w/o internal-detail leaks, shared MasterKeyCache, shutdown token wired, double bearer-verify skipped, dead code removed).
- Next: Phase 7 (librarian + book ingest) per ROADMAP.md.