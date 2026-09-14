-- Phase 11: performance indexes + pragmas for large libraries.
--
-- The library catalog scope grows to hundreds of concepts per book and
-- thousands across books. The hot paths:
--   * search: search_tokens by (scope, token) [indexed], then
--     search_docs by (scope, concept_path) [PK] — fine.
--   * remove/re-add: search_tokens by concept_path — was a FULL SCAN
--     (the PK covers scope+token+path, but a delete by path alone
--     cannot use it). This migration adds the missing side indexes.
--   * list_prefix: scope_files by (scope, path LIKE 'x/%') — the
--     PK (scope, path) already serves the prefix range; no change.
--   * ingest: one search_tokens row per term per concept — thousands
--     of INSERTs per book; covered by the batched-write code path.

-- Delete postings by concept path (remove/re-add path of add_async).
CREATE INDEX idx_search_tokens_path ON search_tokens(scope, concept_path);

-- Doc lookups by scope alone (maintenance scans).
CREATE INDEX idx_search_docs_scope ON search_docs(scope);

-- Ingest job history per book (admin jobs table).
CREATE INDEX idx_ingest_jobs_user ON ingest_jobs(requested_by_user_id);
-- Agent run traces (v1 .traces/ parity): one row per run. The JSON body
-- is the full AgentTrace record (steps, notation, outcome). Stored in
-- the DB (not the file repo) so listing/pruning is indexed, not a
-- directory scan; bodies are small (input/answer truncated).
CREATE TABLE agent_traces (
    id TEXT PRIMARY KEY,
    scope TEXT NOT NULL,          -- 'user:<uuid>' (traces are per-user)
    kind TEXT NOT NULL,            -- 'query' | 'mutation' | 'chat'
    input TEXT NOT NULL,
    answer TEXT NOT NULL DEFAULT '',
    notation TEXT NOT NULL DEFAULT '',
    outcome TEXT NOT NULL,
    duration_ms INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    body TEXT NOT NULL             -- full JSON (steps etc.)
);
CREATE INDEX idx_agent_traces_scope ON agent_traces(scope, created_at DESC);
