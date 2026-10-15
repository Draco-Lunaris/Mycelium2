-- 0008_chat_sessions.sql
-- Chat session history (spec: docs/superpowers/specs/2026-10-14-chat-session-history-design.md §3.1).
-- SQLite holds metadata only: titles and message content are AEAD-sealed
-- under a purpose-bound DEK derived from the user's master key
-- (mycelium2/chat-dek/v1). No plaintext content in the database.

CREATE TABLE chat_sessions (
    id           TEXT PRIMARY KEY,
    user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title_sealed TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
CREATE INDEX idx_chat_sessions_user ON chat_sessions(user_id, updated_at DESC);

CREATE TABLE chat_messages (
    session_id     TEXT NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
    seq            INTEGER NOT NULL,
    role           TEXT NOT NULL CHECK (role IN ('user','tool','assistant','error')),
    content_sealed TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    PRIMARY KEY (session_id, seq)
);

CREATE TABLE chat_turns (
    id         TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
    user_seq   INTEGER NOT NULL,
    status     TEXT NOT NULL CHECK (status IN ('running','done','failed','interrupted','abandoned')),
    attempts   INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
-- The one-in-flight invariant (spec D3): at most one running turn per
-- session, enforced by the database, not by a JS flag.
CREATE UNIQUE INDEX uq_chat_turns_running ON chat_turns(session_id) WHERE status = 'running';
CREATE INDEX idx_chat_turns_resumable ON chat_turns(attempts) WHERE status = 'interrupted';
