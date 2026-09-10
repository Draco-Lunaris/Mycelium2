# Mycelium2 — Architecture Implementation Roadmap

> This roadmap breaks the design into implementation phases. Each phase produces a working milestone that can be built, tested, and reviewed independently.

## Phase 0: Foundation

**Goal**: a compiling Cargo workspace with crate skeletons and shared infrastructure.

**Deliverables**:
- Root `Cargo.toml` workspace definition.
- Crate skeletons:
  - `mycelium-core`
  - `mycelium-crypto`
  - `mycelium-store`
  - `mycelium-auth`
  - `mycelium-web`
  - `mycelium-mcp`
  - `mycelium-librarian`
  - `mycelium-server`
  - `mycelium-cli`
- Shared workspace dependencies and versions.
- `rust-toolchain.toml` (stable).
- `.gitignore` for Rust.
- `justfile` or `Makefile` with common commands (`check`, `test`, `fmt`, `clippy`).
- `AGENTS.md` updated with build commands.

**Exit criteria**: `cargo check --workspace` passes.

## Phase 1: Core OKF engine

**Goal**: parse, represent, and traverse OKF concepts without encryption or persistence.

> **Detailed plan below — see "Phase 1 detailed task breakdown".**

**Deliverables**:
- `mycelium-core`:
  - OKF concept struct with YAML frontmatter + markdown body.
  - Bundle/shelf directory model.
  - Markdown link parser for graph edges.
  - Graph scanner producing nodes + edges.
  - `book://` anchor parser and `read_passage` abstraction.
  - Search index trait and query model.
  - Unit tests for parsing and graph scanning.

**Exit criteria**: core crate tests pass; can parse a sample OKF bundle and emit a graph.

## Phase 2: Crypto layer

**Goal**: encrypt and decrypt individual files with per-user keys.

**Deliverables**:
- `mycelium-crypto`:
  - Argon2id password → KEK.
  - Master key generation and sealing (KEK wraps master key).
  - HKDF-SHA256 per-file DEK derivation.
  - `ring` AES-256-GCM/ChaCha20-Poly1305 file encryption/decryption.
  - Service key for global/shared data.
  - Recovery key generation.
  - Unit + property-based tests.

**Exit criteria**: round-trip encrypt/decrypt tests pass; key derivation is deterministic for same inputs; changing KEK does not change ciphertext.

## Phase 3: Storage layer

**Goal**: persist metadata in SQLite and encrypted files on disk.

**Deliverables**:
- `mycelium-store`:
  - SQLite schema for users, sessions, API keys, config, bookshelves, books, ingest jobs, skills shelf metadata. (Audit events go to structured logs per the DESIGN.md decision — no audit table.)
  - sqlx migrations (automatic on startup).
  - Encrypted file repository abstraction over `mycelium-crypto`.
  - Per-user encrypted inverted index in SQLite.
  - Global skills shelf storage (service-key encrypted).
  - Transaction and rollback helpers.
  - Integration tests with temp directories.

**Exit criteria**: store crate tests pass; migrations run cleanly; can write/read encrypted OKF files and index entries; global skills shelf read/write works with the service key.

## Phase 4: Authentication and authorization

**Goal**: users can log in, sessions work, RBAC enforced.

**Deliverables**:
- `mycelium-auth`:
  - Local account creation (admin-only), Argon2 password hashing.
  - Ed25519 JWT access tokens.
  - Server-side SQLite sessions.
  - TOTP enrollment/verification.
  - WebAuthn second-factor enrollment/verification.
  - OIDC provider configuration and callback handling (configured via admin portal).
  - API key generation, hashing, revocation.
  - RBAC middleware/extractor (`Admin`, `User`).
  - Unit + integration tests.

**Exit criteria**: can create admin, log in, obtain session, access protected route, reject unauthorized access.

## Phase 5: Web server and admin portal

**Goal**: HTTPS web UI with login, user pages, and admin panel.

**Deliverables**:
- `mycelium-web`:
  - axum router with HTTPS (rustls) and HTTP redirect.
  - Auto self-signed cert generation.
  - Leptos SSR + hydrate frontend skeleton.
  - Login page and session-based auth.
  - User home: private bundle browser, concept editor, private skills manager, search, graph view.
  - Global skills browser (read-only for users).
  - Admin portal: users, OIDC config, LLM config, bookshelves, global skills management, book upload, maintenance, backup.
  - REST API under `/api/v1/`.
  - Static asset serving from disk.
  - CSP nonces, CSRF double-submit cookie.
  - Prometheus `/metrics` and detailed `/health`.

**Exit criteria**: server starts, serves HTTPS, login works, admin portal reachable.

## Phase 6: MCP 2026-07-28 server

**Goal**: stateless MCP server over Streamable HTTP with per-user API keys.

**Deliverables**:
- `mycelium-mcp`:
  - `rmcp` server setup with `ProtocolVersion::STANDARD_HEADERS`.
  - Bearer API key middleware resolving to user identity.
  - Tool implementations:
    - `mycelium2_memory_query`
    - `mycelium2_memory_add`
    - `mycelium2_memory_update`
    - `mycelium2_memory_status`
    - `mycelium2_memory_maintain`
    - `mycelium2_skill_get` (fetch a skill by name — private first, then global)
    - `mycelium2_skill_list` (list available skills — private + global)
  - `tools/list` with cache hints.
  - No server-to-client requests.
  - Integration tests with temp store.

**Exit criteria**: MCP server responds to `tools/list` and `tools/call` with valid API key; rejects unauthenticated requests; `skill_get`/`skill_list` return skills from the user's private bundle and the global skills shelf.

## Phase 7: Librarian and book ingest

**Goal**: admins can upload books; librarian catalogs them.

**Deliverables**:
- `mycelium-librarian`:
  - In-process async worker with one job per user at a time.
  - OpenAI-compatible LLM client (Ollama default, configurable).
  - Multipart book upload endpoint.
  - LLM-based chapter/section extraction.
  - Write catalog to admin bookshelf and full text to shared encrypted stacks.
  - Ingest job status tracking.

**Exit criteria**: can upload a book, see ingest job progress, read passages via `book://` anchors.

## Phase 8: Global bookshelves and search

**Goal**: global-read bookshelves and global skills work; search spans user bundle + global content.

**Deliverables**:
- Admin can mark bookshelf as global-read.
- Admin can manage the global skills shelf (create/edit skills; all users read).
- Users can browse/search global bookshelves and global skills.
- Users can create/edit private skills in their own bundle.
- Search defaults to user bundle + global bookshelves + global skills (web) or user bundle only (MCP).
- Graph view scoped to user private bundle.

**Exit criteria**: global books and skills visible to users; search returns global results; MCP query stays private by default; private skills editable only by their owner.

## Phase 9: Polish, docs, and CI

**Goal**: production-ready build, docs, and deployment pipeline.

**Deliverables**:
- `mycelium-server` binary wires all crates together.
- `mycelium-cli` admin commands.
- Dockerfile and `docker-compose.yml`.
- GitHub Actions workflow: test, clippy, build, publish GHCR image.
- Conventional commits + automated semver.
- Markdown docs in `docs/` and GitHub Pages.
- End-to-end integration tests.
- Security review pass.

**Exit criteria**: CI green; Docker image published; docs site live; end-to-end tests pass.

## Dependency order

```
Phase 0
  │
  ▼
Phase 1 (mycelium-core)
  │
  ▼
Phase 2 (mycelium-crypto)
  │
  ▼
Phase 3 (mycelium-store) ──▶ depends on core + crypto
  │
  ▼
Phase 4 (mycelium-auth) ──▶ depends on store
  │
  ▼
Phase 5 (mycelium-web) ───▶ depends on auth + store + core
  │
  ▼
Phase 6 (mycelium-mcp) ───▶ depends on auth + store + core
  │
  ▼
Phase 7 (mycelium-librarian) ──▶ depends on store + core
  │
  ▼
Phase 8 (global books + search)
  │
  ▼
Phase 9 (CI, docs, polish)
```

## Risk areas

- **Leptos SSR + hydrate build complexity**: may require nightly Rust or specific cargo-leptos tooling.
- **Encrypted search index**: custom inverted index must be correct and performant; consider benchmarking early.
- **WebAuthn integration**: credential management and origin validation need careful testing.
- **MCP 2026-07-28 SDK maturity**: `rmcp` is in beta; may need workarounds or patches.
- **LLM extraction quality**: librarian output may need fallback/heuristic parsing.

## Recommended first three milestones

1. **Milestone 1**: workspace + core OKF parsing + graph scanner.
2. **Milestone 2**: crypto + storage layers with encrypted file round-trip.
3. **Milestone 3**: auth + a minimal axum HTTPS server with login.

These three milestones give us a secure, testable foundation before adding the web UI, MCP, and librarian.

---

# Phase 1 detailed task breakdown

> This section is the working plan for Phase 1. Tasks are ordered; each has concrete acceptance criteria. Verified dependency facts and gotchas are called out inline so they don't bite during implementation.

## 0. Verified facts this plan is built on (do not re-litigate)

These were checked against crates.io / GitHub on 2026-09-10:

- **rmcp 0.16.0 does NOT support MCP 2026-07-28.** Its newest protocol version is 2025-06-18 (and its `LATEST` is pinned to 2025-03-26). The 2026-07-28 stateless core, `STANDARD_HEADERS`, `legacy_session_mode`, and `stateless_protocol_metadata_required` only exist in **rmcp >= 3.2.0** (released 2026-08-31, MSRV Rust 1.88). The workspace must upgrade `rmcp` to 3.2.0 and the workspace MSRV to 1.88.
- **serde_yaml is deprecated** (unmaintained since 2024-03). Use **serde_yaml_ng 0.10** (drop-in API, MSRV 1.64, actively maintained) for frontmatter parsing.
- **MSRV chain**: rmcp 3.2.0 needs Rust 1.88; leptos 0.8.20 needs 1.88; jsonwebtoken 11 needs 1.88; totp-rs 6 needs 1.88; webauthn-rs 0.5.5 needs 1.88. Workspace `rust-version` must be **1.88** (currently 1.85 — will fail).
- **sqlx 0.9.0 needs Rust 1.94** — stay on **sqlx 0.8.6** (no MSRV requirement, 67M downloads, stable).
- **Leptos version choice**: 0.7.8 is what we pinned (MSRV 1.76) but 0.8.20 is the current stable line (MSRV 1.88, 319k downloads of 0.8.20). Since our MSRV moves to 1.88 anyway, use **leptos 0.8.20**.
- **OKF format facts** (from Mycelium memory):
  - Frontmatter: `type` is the only REQUIRED field; `title`, `description`, `resource`, `tags` recommended; `timestamp` also seen.
  - `index.md` and `log.md` are RESERVED, auto-maintained, and excluded from graph scans.
  - `info.md` is a reserved per-shelf metadata file (frontmatter `name`/`topic`/`description`, book shelves add `kind: book`).
  - Links: ONLY absolute leading-slash `.md` links count as graph edges (`[Foo](/path.md)`). Relative `./` links are silently ignored by the scanner (0 edges).
  - Scanner gotcha: the regex scans raw body text and does NOT skip code spans/backticks. A literal `](` + `/path.md` adjacency creates a false edge even inside backticks. We must decide: replicate this bug for compatibility, or fix it (skip code spans) and document the difference.
  - Book catalog: Book hub at `/<slug>/book.md`, chapters at `/<slug>/<anchor>.md`; chapter frontmatter carries `book: /<slug>/book.md` and `chapter_index: <n>`.
  - Anchors: `ch-<n>-<slug>` (chapter), `sec-<n>-<m>-<slug>` (section); `read_passage` cap is 128k chars.

## Task 1 — Workspace dependency corrections (blocking, do first)

**Why first**: `cargo check` will fail on MSRV once rmcp 3.2.0 is pulled in; everything else depends on this.

Changes to root `Cargo.toml`:
- `rust-version = "1.88"` (workspace.package).
- `rmcp = { version = "3.2", features = ["server", "macros"] }` (verify exact feature names against rmcp 3.2.0 docs during implementation; `default` includes `macros` + `server`).
- Replace `serde_yaml` with `serde_yaml_ng = "0.10"` everywhere.
- `leptos = "0.8"`.
- Keep: sqlx 0.8.6, axum 0.8.9, ring 0.17, argon2 0.6 (verify API changes from 0.5 — argon2 0.6 changed some signatures), rand 0.10 (verify API — rand 0.9+ changed `thread_rng` to `rng()`), jsonwebtoken 11 (verify EdDSA API), totp-rs 6, webauthn-rs 0.5.5, openidconnect 4, reqwest 0.13, tower-http 0.7, axum-server 0.8.
- Update `crates/mycelium-core/Cargo.toml`: `serde_yaml` → `serde_yaml_ng`.
- Update `crates/mycelium-web/Cargo.toml`: same swap.
- Update `crates/mycelium-mcp/Cargo.toml` and `src/handler.rs` to the rmcp 3.2.0 macro API (`#[tool_router]` + `#[tool_handler]` two-block form, or `#[tool_router(server_handler)]` single-block form — both exist in 3.2.0).
- Update `rust-toolchain.toml` if it pins anything below 1.88 (it doesn't — stable channel is fine; local rustc is 1.98).

**Acceptance**: `cargo check --workspace` green with zero errors; `cargo tree -p mycelium-mcp | grep rmcp` shows 3.2.x.

**Gotchas**:
- argon2 0.5 → 0.6: `Argon2::new` / `hash_password_into` signatures changed; the `password-hash` crate version moved. Check compile errors carefully in mycelium-crypto and mycelium-auth.
- rand 0.8 → 0.10: `rand::thread_rng()` is removed in 0.9+; use `rand::rng()` and `RngCore::fill_bytes` from the new API. The placeholder code in `mycelium-crypto/src/keys.rs` uses `thread_rng` — must be rewritten.
- jsonwebtoken 9 → 11: `EncodingKey`/`DecodingKey` APIs and `Algorithm::EdDSA` usage may have shifted; verify in mycelium-auth.
- rmcp 3.2.0 `StreamableHttpServerConfig` has `allowed_hosts` defaulting to loopback-only — for production we must set it explicitly (Phase 6 concern, but note it now).

## Task 2 — OKF concept parsing (`mycelium-core/src/concept.rs`)

Implement for real:

- `Frontmatter` struct: `type` (required, string), `title`, `description`, `resource`, `tags: Vec<String>`, `timestamp` (optional). Use `#[serde(rename_all = "snake_case")]`-safe field names; keep `#[serde(rename = "type")]` for the `concept_type` field.
- **Lenient parsing rule**: unknown frontmatter fields must NOT error — use `#[serde(default)]` on optional fields and ignore unknowns (serde default behavior with `deny_unknown_fields` NOT set). Mycelium bundles in the wild carry extra fields.
- `parse_concept(path: &str, contents: &str) -> Result<Concept, ConceptError>`:
  - Split frontmatter from body on the `---\n ... \n---\n` delimiter (first occurrence only).
  - Frontmatter must parse as YAML mapping; `type` missing → error (the only hard requirement).
  - Body is everything after the closing `---`.
- `Concept::to_markdown()` — serialize back to frontmatter + body (needed for edits later; keep field order stable: type, title, description, resource, tags, timestamp).
- `ConceptError` variants: `MissingFrontmatter`, `MissingTypeField`, `YamlError`, `IoError`.
- Reserved filename constants: `INDEX_MD = "index.md"`, `LOG_MD = "log.md"`, `INFO_MD = "info.md"` in a `reserved` module.

**Tests** (in-module):
- Parse a full concept with all fields.
- Parse a concept with only `type` (all optionals default).
- Unknown extra frontmatter fields are ignored, not an error.
- Missing `type` → `MissingTypeField`.
- No frontmatter block → `MissingFrontmatter`.
- Round-trip: parse → to_markdown → parse yields identical struct.
- CRLF line endings handled (normalize or accept both).

**Gotchas**:
- serde_yaml_ng handles YAML 1.2; Mycelium frontmatter is simple enough that this is safe.
- Do NOT put `#[serde(deny_unknown_fields)]` — real bundles have extra keys.
- `timestamp` may be a string or date in the wild; parse as `Option<String>` for now (loose), tighten later if needed.

## Task 3 — Bundle walker (`mycelium-core/src/bundle.rs` — new module)

- `Bundle` struct: root path + list of concepts + reserved files encountered.
- `walk_bundle(root: &Path) -> Result<Bundle, BundleError>`:
  - Recursively walk `*.md` files.
  - EXCLUDE `index.md`, `log.md` from concepts (record their presence).
  - `info.md` at shelf root: parse as `ShelfInfo` (name/topic/description, optional `kind: book`) — not a concept.
  - Concept paths are bundle-relative with leading slash: `/tables/customers.md`.
  - Skip hidden files/dirs (`.traces/` etc. — anything starting with `.`).
- `ShelfInfo` struct + parser in the same module.
- Filename validation: warn (not error) on non-kebab-case names — the original doesn't enforce, we just surface them.

**Tests**:
- Walk a fixture bundle (Task 7) → correct concept count, reserved files excluded, `.traces/` skipped.
- `info.md` parsed as ShelfInfo with `kind: book` when present.
- Empty bundle → Ok with zero concepts.

**Gotchas**:
- Symlinks: do not follow (avoid loops) — use `walkdir` with `follow_links(false)` or manual recursion. We already have `walkdir` in the tree via transitive deps; add it explicitly to mycelium-core deps.
- Path normalization: always forward slashes, leading slash, no `./` segments — this is the canonical ID used by the graph and links.

## Task 4 — Link scanner (`mycelium-core/src/links.rs` — new module)

- `scan_links(body: &str) -> Vec<String>`:
  - Regex: markdown links whose target starts with `/` and ends with `.md`.
  - `\[([^\]]*)\]\((/[^\s)]*\.md)\)` — capture the target only.
  - Scans BODY ONLY (never frontmatter) — matches original behavior.
  - **Decision (confirmed by user 2026-09-10)**: **skip code spans** — Mycelium2 fixes the original's false-edge bug. Graph may differ from original Mycelium on bundles containing literal link examples; divergence documented in code and DESIGN.md.
- Also extract `book://` resource references from frontmatter `resource` fields (not body links) — these are catalog pointers, not graph edges.

**Tests**:
- Absolute link extracted: `[Foo](/foo.md)` → `/foo.md`.
- Relative link ignored: `[Foo](./foo.md)` → nothing.
- Non-md absolute ignored: `[Foo](/foo)` → nothing.
- Bare path not a link: `/foo.md` alone in text → nothing.
- Link inside inline code span: `` `[Foo](/foo.md)` `` → nothing (with decision b).
- Link inside fenced block → nothing (with decision b).
- Multiple links on one line all extracted.
- Link with fragment `[Foo](/foo.md#section)` → extract `/foo.md` (strip fragment before edge matching; original behavior: `.md` end check — verify whether `#frag` after `.md` counts; safest: accept and strip).

**Gotchas**:
- The `](\` adjacency bug: our regex must require the `](` to be a real markdown link, which the regex does. The original bug came from regex-matching `](/path.md)` even in backticks — decision (b) fixes this.
- Fragment handling: original scanner requires `.md` at END of target, so `/foo.md#frag` would NOT match. Verify against original regex during implementation; if original excludes fragments, exclude them too (compatibility) — flag for a quick check of `packages/core/src/okf/graph.ts` in the old repo.

## Task 5 — Graph builder (`mycelium-core/src/graph.rs` — rewrite)

- `Graph` struct: `nodes: HashMap<String, Node>`, `edges: Vec<Edge>`, `broken_links: Vec<BrokenLink>`, `orphans: Vec<String>`, health metrics (`concept_count`, `edge_count`, `broken_link_count`, `orphan_count`).
- `build_graph(bundle: &Bundle) -> Graph`:
  - Node per concept (ID = canonical path).
  - Edge per scanned link where target exists in bundle.
  - BrokenLink per scanned link where target missing.
  - Orphan = node with 0 in + 0 out edges.
- `Graph::health()` → `GraphHealth { concept_count, edge_count, broken_links, orphans }` — this feeds `memory_status` later.
- Deterministic ordering: sort edges and orphans (stable output for tests and caching).

**Tests**:
- Fixture bundle graph: expected node/edge/broken/orphan counts.
- Orphan detection (concept with no links in or out).
- Broken link detection (link to nonexistent path).
- Health metrics correct.
- Determinism: build twice → identical output.

**Gotchas**:
- Self-links (concept linking to itself): count as an edge but NOT as an orphan-breaker (original counts them; verify).
- Duplicate links to the same target from one concept: original counts each occurrence as an edge? Decide: dedupe edges (cleaner graph) — flag for verification against original.

## Task 6 — Book library abstractions (`mycelium-core/src/library.rs` — rewrite)

- `BookRef` parsing: `resource: book://<slug>#<anchor>` → `BookRef { slug, anchor }`.
- Anchor parsing: `ch-<n>-<slug>` → `Anchor::Chapter { index, slug }`; `sec-<n>-<m>-<slug>` → `Anchor::Section { chapter, section, slug }`. Invalid → `Anchor::Other(String)` (tolerant).
- `read_passage` trait: `trait PassageReader { fn read_passage(&self, book_ref: &BookRef) -> Result<Passage, PassageError>; }` — the core crate defines the trait + cap logic; the store crate implements it against encrypted stacks in Phase 3.
- Cap: `READ_PASSAGE_MAX_CHARS = 128 * 1024` (chars, not bytes — preserve original semantics).
- Chapter anchor extracts to the NEXT chapter heading; section anchor to the next same-or-higher-level heading. Implement heading-boundary extraction as a pure function over text: `extract_passage(full_text: &str, anchor: &Anchor) -> Result<Passage, PassageError>` — testable without any I/O.

**Tests**:
- Parse `book://my-book#ch-3-intro` → slug `my-book`, Chapter { index: 3 }.
- Parse `sec-2-4-details` → Section { chapter: 2, section: 4 }.
- `extract_passage` on a synthetic book: chapter anchor returns text up to next `# ` heading; section anchor up to next same-or-higher heading.
- Cap enforcement: passage longer than 128k chars is truncated (or error — original truncates? verify; plan: truncate and note).
- No-anchor behavior: discovery mode lists sections grouped under chapters (original `read_passage` no-anchor behavior).

**Gotchas**:
- The anchor slug suffix (`-intro` in `ch-3-intro`) is informational; matching is by index. Keep it in the struct but don't require it to match headings.
- Heading levels: `#` = chapter (h1), `##` = section (h2) in the stacks' markdown. Verify against original `read_passage` semantics during implementation.

## Task 7 — Search abstractions (`mycelium-core/src/search.rs` — refine)

- Keep `SearchIndex` trait, `SearchQuery`, `SearchResult` as designed.
- Add `InMemoryIndex` reference implementation (tokenize title + description + tags + body; simple tf scoring; no persistence). This gives Phase 1 a working search and a test oracle for the Phase 3 encrypted index.
- Tokenizer: lowercase, split on non-alphanumeric, min length 2. No stemming (keep it simple; document).

**Tests**:
- Index 3 concepts, query by title token → correct hit, ranked.
- Query with `include_global` flag respected (flag exists even if unused until Phase 8).
- Empty query → empty results (not an error).

## Task 8 — Fixture bundle + integration test (`mycelium-core/tests/`)

- `tests/fixtures/sample-bundle/`:
  - `/info.md` (shelf info, `kind: book` absent)
  - `/index.md`, `/log.md` (reserved — must be excluded)
  - `/decisions/auth-model.md` (links to `/apis/read-passage.md` and `/tables/users.md`)
  - `/apis/read-passage.md` (links back to `/decisions/auth-model.md`)
  - `/tables/users.md` (no links — orphan)
  - `/books/my-book/book.md` + `/books/my-book/ch-1-intro.md` (book catalog concepts with `book://` resources)
  - `/skills/deploy-rust-service.md` (`type: Skill` concept — SKILL.md-style body with name/description/instructions; links to `/decisions/auth-model.md`)
  - `/broken/concept-with-broken-link.md` (links to `/nonexistent.md`)
  - `.traces/hidden.md` (must be skipped)
- `tests/bundle_integration.rs`:
  - Walk → parse → graph → search end-to-end over the fixture.
  - Assert exact node/edge/broken/orphan counts.
  - Assert the Skill concept parses with `type: Skill` and is searchable.
  - Assert `read_passage`-style extraction works over a fixture stack file.

**Gotchas**:
- Fixtures live in the repo — they double as documentation of the OKF format for future agents. Keep them realistic (copy structure from the real Mycelium bundle conventions).
- Use `include_str!` or relative paths via `CARGO_MANIFEST_DIR` — tests must pass regardless of cwd.
- Skills are just concepts with `type: Skill` — no special parsing in Phase 1. The skill-specific storage/MCP behavior arrives in Phases 3/6/8; Phase 1 only guarantees skills parse, link, and search like any concept.

## Task 9 — Cleanup and verification

- Remove placeholder `TODO` stubs that Phase 1 replaces (concept.rs, graph.rs, library.rs, search.rs, shelf.rs get real implementations; `ShelfKind` stays as-is).
- `cargo fmt --all`.
- `cargo clippy --workspace --all-targets -- -D warnings` (fix everything; the existing unused-import warnings in crypto/auth/mcp stubs get cleaned too).
- `cargo test --workspace` green.
- Update `AGENTS.md` with verified commands (`cargo check/test/clippy/fmt`) and the Phase 1 completion state.
- Update `DESIGN.md` decisions log: link scanner code-span decision (b), fragment handling decision, self-link/duplicate-edge decisions.

## Open questions to resolve during implementation (quick checks, not blockers)

1. **Fragment links** (`/foo.md#frag`): does the original scanner match them? Check `packages/core/src/okf/graph.ts` in the Draco-Lunaris/Mycelium repo; mirror its behavior.
2. **Self-links and duplicate edges**: does the original dedupe? Mirror.
3. **Passage cap behavior**: truncate or error at 128k? Original truncates (READ_PASSAGE_MAX_CHARS) — mirror.
4. **rmcp 3.2.0 exact feature names** for `server` + macros: confirm from its Cargo.toml/docs when wiring (the `default` feature set includes both).

**Resolved decisions:**
- Link scanner skips code spans/fenced blocks (user-confirmed 2026-09-10) — fixes the original's false-edge bug class.

## Definition of done — Phase 1

- [ ] Workspace deps corrected (rmcp 3.2, serde_yaml_ng, MSRV 1.88, leptos 0.8.20) and `cargo check --workspace` green.
- [ ] Concept parsing with lenient frontmatter, round-trip serialization, reserved-file rules.
- [ ] Bundle walker with exclusions and path canonicalization.
- [ ] Link scanner with code-span skipping (documented divergence from original).
- [ ] Graph builder with health metrics and deterministic output.
- [ ] Book anchor parsing + pure `extract_passage` with 128k cap.
- [ ] Search trait + in-memory reference index.
- [ ] Skill concepts (`type: Skill`) parse, link, and search like any concept (fixture coverage).
- [ ] Fixture bundle + integration tests covering all of the above.
- [ ] `cargo fmt` clean, `cargo clippy -D warnings` clean, `cargo test --workspace` green.
- [ ] AGENTS.md and DESIGN.md updated with decisions and verified commands.
