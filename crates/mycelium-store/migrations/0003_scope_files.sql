-- Phase 5: concept file registry per scope.
-- Maps canonical paths to their stored (opaque) files and caches listing
-- metadata (title/type are plaintext, consistent with concept_path in the
-- search tables — paths and titles are metadata; content stays encrypted).
CREATE TABLE scope_files (
    scope TEXT NOT NULL,
    path TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    concept_type TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL,
    PRIMARY KEY (scope, path)
);
CREATE INDEX idx_scope_files_scope ON scope_files(scope);