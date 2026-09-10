-- Mycelium2 initial schema.
-- Audit events go to structured logs (DESIGN.md decision) — no audit table.

-- Users. Password hashes are Argon2 PHC strings; the sealed master key
-- record is the mycelium-crypto SealedMasterKey JSON.
CREATE TABLE users (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE COLLATE NOCASE,
    email TEXT NOT NULL UNIQUE COLLATE NOCASE,
    role TEXT NOT NULL CHECK (role IN ('admin', 'user')),
    auth_provider TEXT NOT NULL CHECK (auth_provider IN ('local', 'oidc')),
    password_hash TEXT,
    sealed_master_key TEXT NOT NULL,
    must_change_password INTEGER NOT NULL DEFAULT 0,
    totp_secret TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Server-side sessions (httpOnly cookie -> session id).
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    csrf_token TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
CREATE INDEX idx_sessions_user ON sessions(user_id);
CREATE INDEX idx_sessions_expires ON sessions(expires_at);

-- Per-user API keys (opaque random tokens, stored hashed, shown once).
CREATE TABLE api_keys (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    key_hash TEXT NOT NULL UNIQUE,
    label TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    last_used_at TEXT,
    revoked_at TEXT
);
CREATE INDEX idx_api_keys_user ON api_keys(user_id);
CREATE INDEX idx_api_keys_hash ON api_keys(key_hash);

-- Runtime configuration KV (admin-managed: OIDC, LLM, feature flags).
-- Values are JSON; sensitive values are stored encrypted by the caller.
CREATE TABLE config (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Bookshelves (admin-created; global-read or admin-private).
CREATE TABLE bookshelves (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT '',
    is_global_read INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);

-- Books (catalog hubs) on bookshelves; full text lives in the shared
-- encrypted library stacks (file repo, service-key scope).
CREATE TABLE books (
    id TEXT PRIMARY KEY,
    bookshelf_id TEXT NOT NULL REFERENCES bookshelves(id) ON DELETE CASCADE,
    slug TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX idx_books_shelf ON books(bookshelf_id);

-- Librarian ingest jobs (one per user at a time).
CREATE TABLE ingest_jobs (
    id TEXT PRIMARY KEY,
    bookshelf_id TEXT NOT NULL REFERENCES bookshelves(id),
    requested_by_user_id TEXT NOT NULL REFERENCES users(id),
    status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'done', 'failed')),
    detail TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX idx_ingest_jobs_status ON ingest_jobs(status);

-- Encrypted per-scope search index.
-- scope: 'user:<uuid>' or 'global'. Tokens are HMAC-SHA256 hex of the
-- search term (keyed per scope) — plaintext terms never touch the DB.
-- doc payloads are AEAD envelopes (title + snippet), keyed per scope.
CREATE TABLE search_tokens (
    scope TEXT NOT NULL,
    token TEXT NOT NULL,
    concept_path TEXT NOT NULL,
    tf INTEGER NOT NULL,
    PRIMARY KEY (scope, token, concept_path)
);
CREATE INDEX idx_search_tokens_scope_token ON search_tokens(scope, token);

CREATE TABLE search_docs (
    scope TEXT NOT NULL,
    concept_path TEXT NOT NULL,
    payload BLOB NOT NULL,
    PRIMARY KEY (scope, concept_path)
);