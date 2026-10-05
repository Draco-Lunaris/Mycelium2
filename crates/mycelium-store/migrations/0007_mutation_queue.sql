-- Phase 11: MCP mutation queue (issue #1).
--
-- Receipts-first writes: MCP write tools enqueue rows here and return
-- a receipt; the librarian drain worker (mycelium-librarian) integrates
-- each item in the background. Payload blobs (encrypted, user scope)
-- live in the FileRepo at canonical /mutation-queue/<id> — rows hold
-- metadata only, never content ("concept bodies never in SQLite").
--
-- `staging` is the internal enqueue phase: the row is inserted in
-- staging BEFORE the payload file exists, flipped to pending once the
-- file is durable. Callers never see staging (receipt views map it to
-- pending); the boot sweep completes or fails staging rows.
--
-- final_paths: JSON array string of the paths the integration wrote
-- (set on done; NULL otherwise). detail: human-readable provenance
-- (retry error, fallback note, dead reason).

CREATE TABLE mutation_queue (
    id            TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    tool          TEXT NOT NULL CHECK (tool IN ('add', 'update', 'maintain')),
    status        TEXT NOT NULL CHECK (status IN ('staging', 'pending', 'running', 'done', 'dead')),
    attempts      INTEGER NOT NULL DEFAULT 0,
    detail        TEXT NOT NULL DEFAULT '',
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    next_retry_at TEXT,
    final_paths   TEXT
);
CREATE INDEX idx_mutation_queue_due ON mutation_queue(status, next_retry_at, created_at);
