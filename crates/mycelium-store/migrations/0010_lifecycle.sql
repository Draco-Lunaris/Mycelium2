-- 0010_lifecycle.sql
-- Memory lifecycle (spec: docs/superpowers/specs/2026-10-31-memory-lifecycle-design.md §3.1–§3.3).
--
-- Step 1 of the stream lands the retrieval-filter columns; the revision and
-- vector tables are created here EMPTY and stay unpopulated until the later
-- steps of this stream add their writers (one migration for the whole stream,
-- so no later step needs to touch the schema).
--
--   * search_docs.lifecycle_status: the frontmatter `status` value as-written
--     (NULL when absent). Non-sensitive ranking metadata — docs live in the
--     same table as AEAD payloads addressed by plaintext path — so the
--     ActiveOnly status filter is a SQL predicate rather than a decrypt of
--     every candidate payload.
--   * mutation_queue.source: provenance label captured at enqueue
--     (`mcp:<key-label>`, `web:<username>`, `dream`, …), stamped into the
--     concept frontmatter at integrate time (spec §3.1). Existing rows keep
--     '' (unknown provenance, never rewritten).
--   * concept_revisions / search_vectors: the undo-log (spec §3.2) and the
--     hybrid vector index (spec §3.3), columns exactly as those sections
--     specify them. Step 2 (amendments §3.2a) adds `path TEXT NOT NULL`
--     after `path_token`: path tokens are one-way HMAC and `delete` erases
--     the registry row, so §3.2's revisions browser page cannot name a
--     deleted path otherwise — the same plaintext-metadata posture as
--     scope_files title/type.

ALTER TABLE search_docs ADD COLUMN lifecycle_status TEXT;

ALTER TABLE mutation_queue ADD COLUMN source TEXT NOT NULL DEFAULT '';

-- Spec §3.2 (+ §3.2a-1 `path`): write-ahead undo log for concept mutations.
-- One row per concept-changing op, newest revision_no per (scope, path token).
CREATE TABLE concept_revisions (
    id          INTEGER PRIMARY KEY,
    scope_id    TEXT NOT NULL,               -- user:<uuid> | global:skills | global:library
    path_token  TEXT NOT NULL,               -- HMAC path token, same scheme as search_tokens
    path        TEXT NOT NULL,               -- plaintext concept path (dead paths must stay nameable — §3.2a-1)
    revision_no INTEGER NOT NULL,            -- monotonic per (scope_id, path_token); MAX+1 at insert
    op          TEXT NOT NULL,               -- create|update|delete|restore|merge|consolidate|migration
    source      TEXT NOT NULL,               -- provenance captured at enqueue (spec §3.1)
    summary     TEXT NOT NULL,               -- short deterministic op description
    payload     BLOB NOT NULL,               -- AEAD full concept markdown snapshot (incl. frontmatter)
    created_at  TEXT NOT NULL,
    UNIQUE (scope_id, path_token, revision_no)
);

-- Spec §3.3: per-scope embedding rows for hybrid retrieval.
CREATE TABLE search_vectors (
    scope_id   TEXT NOT NULL,
    path_token TEXT NOT NULL,                -- same HMAC path token scheme as search_tokens
    model      TEXT NOT NULL,
    dims       SMALLINT NOT NULL,
    stale      INTEGER NOT NULL DEFAULT 0,
    vec        BLOB NOT NULL,                -- AEAD f32 LE bytes under IndexKeys meta DEK; AAD `vec:<scope>:<token>`
    updated_at TEXT NOT NULL,
    PRIMARY KEY (scope_id, path_token)
);
