# Architecture

Mycelium2 is a Rust workspace of nine crates over an axum HTTPS
server. Everything is self-contained: SQLite metadata, encrypted files
on disk, no external services (the LLM backend is optional).

```
┌──────────────────────────────────────────── mycelium2 (bin) ───┐
│                                                               │
│  mycelium-web ── axum HTTPS + SSR pages + REST /api/v1        │
│      │              merges the MCP router at /mcp             │
│      │                                                        │
│      ├── mycelium-auth ── login, sessions, TOTP/WebAuthn,     │
│      │                     OIDC, API keys, RBAC               │
│      │                                                        │
│      ├── mycelium-librarian ── book ingest worker (LLM +      │
│      │                           heuristic fallback)          │
│      │                                                        │
│      ├── mycelium-mcp ── stateless MCP 2026-07-28 server       │
│      │                    (rmcp, Streamable HTTP)              │
│      │                                                        │
│      └── mycelium-store ── SQLite + encrypted file repo +      │
│                 │            encrypted search index           │
│                 │                                             │
│                 ├── mycelium-crypto ── Argon2id, HKDF,         │
│                 │                 AES-256-GCM, key newtypes    │
│                 │                                             │
│                 └── mycelium-core ── OKF parsing, links,       │
│                                    graph, book anchors, search│
└───────────────────────────────────────────────────────────────┘
```

## Encryption model

- **Per-user data**: password → Argon2id → KEK → unseals the master
  key → HKDF-SHA256 derives path-bound DEKs → AES-256-GCM per file.
  The search index uses the same pattern (HMAC tokens + AEAD doc
  payloads) so plaintext never touches the database.
- **Global data** (bookshelves, library stacks, global skills): the
  service key (0600 file, env override) with the same envelope
  format.
- **Key newtypes** (`MasterKey`/`Kek`/`Dek`/`ServiceKey`/...) make
  key-mixing a compile error.

## Scopes and namespaces

| Scope id | Key | Contents |
|---|---|---|
| `user:<uuid>` | user master key | private bundle: concepts, notes, private skills |
| `global:skills` | service key | global skills shelf (admin-write, all-read) |
| `global:library` | service key | book catalog concepts (hub + chapters) |

Book **stack texts** (the full 32 MiB-capable book bodies) are raw
encrypted FileRepo payloads — deliberately outside the registry and
search index; they are read via `book://<slug>#<anchor>` passage
extraction, never listed or searched.

## OKF format

Concepts are markdown files with YAML frontmatter (`type` required).
Graph edges are absolute leading-slash `.md` links; links inside code
spans/fenced blocks are skipped (a deliberate divergence from the
original Mycelium — it fixes false-edge bugs). `index.md`, `log.md`,
and `info.md` are reserved. Skills are concepts with `type: Skill`.

## Book ingest flow

1. Admin uploads a markdown book (multipart, ≤32 MiB, CSRF-checked).
2. The librarian worker stages the text, creates book + job rows
   (one job per user at a time; atomic claim prevents double-runs).
3. The LLM (optional) enriches a catalog; the heuristic outline is the
   total fallback.
4. Full text → `/library/<slug>.md` (raw payload); catalog →
   `/<slug>/book.md` hub + `/<slug>/ch-<n>-<slug>.md` chapters in
   `global:library`.
5. Passages are read via `GET /api/v1/passages?resource=book://<slug>#<anchor>`
   with shelf-visibility enforcement (global-read or admin).

## MCP server

Stateless MCP 2026-07-28 over Streamable HTTP at `/mcp`. Per-request
identity comes from the bearer API key (rmcp injects the request parts
into each call). Tools: `mycelium2_memory_query/add/update/status/
maintain`, `mycelium2_skill_get/list`. MCP queries stay private to the
caller's bundle by default (web search spans user + global scopes).

## Determinism

Graph edges, broken links, orphans, and search ranking are all
deterministic (sorted/stable) — the same inputs always produce the
same outputs, which the test suite relies on.