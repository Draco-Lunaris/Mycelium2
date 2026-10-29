-- 0009_chat_waiting.sql
-- LLM outage recovery (spec: docs/superpowers/specs/2026-10-28-llm-outage-recovery-design.md §3.3).
-- `chat_turns` gains a sixth status, 'waiting_backend' — turns parked while
-- the LLM backend is down, retried unboundedly by the resume worker — plus
-- provenance and error-metadata columns:
--   park_state: which claim path parked the turn ('interrupted' for crash
--     resumes, 'waiting_backend' for weather claims); the boot sweep
--     re-parks running turns to their park_state so a kill mid-flight
--     lands back where it came from.
--   last_error: unsealed, bounded (512-char short_detail convention)
--     failure metadata for the admin table; overwritten on each park.
-- SQLite cannot widen a CHECK in place, so the table is rebuilt and
-- existing rows ride over unchanged: park_state defaults to 'interrupted'
-- (pre-migration rows carry no weather history) and last_error is NULL.

CREATE TABLE chat_turns_new (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
    user_seq    INTEGER NOT NULL,
    status      TEXT NOT NULL CHECK (status IN ('running','done','failed','interrupted','abandoned','waiting_backend')),
    attempts    INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    park_state  TEXT NOT NULL DEFAULT 'interrupted',
    last_error  TEXT
);
INSERT INTO chat_turns_new (id, session_id, user_seq, status, attempts,
                            created_at, updated_at, park_state, last_error)
SELECT id, session_id, user_seq, status, attempts,
       created_at, updated_at, 'interrupted', NULL
FROM chat_turns;

-- Drop the old table's indexes before the swap: they belong to
-- chat_turns and free their names for the recreated versions. (Migrations
-- run at boot before traffic, so no turn can be mid-flight across this
-- swap; the one-running invariant briefly lives only on the new table.)
DROP INDEX uq_chat_turns_running;
DROP INDEX idx_chat_turns_resumable;
DROP TABLE chat_turns;
ALTER TABLE chat_turns_new RENAME TO chat_turns;

-- Recreated exactly as 0008 had them, on the rebuilt table.
CREATE UNIQUE INDEX uq_chat_turns_running ON chat_turns(session_id) WHERE status = 'running';
CREATE INDEX idx_chat_turns_resumable ON chat_turns(attempts) WHERE status = 'interrupted';
-- Mirror of the resumable index for the weather pool (spec §3.3).
CREATE INDEX idx_chat_turns_waiting ON chat_turns(created_at) WHERE status = 'waiting_backend';
