-- Phase 7: book ingest.
--
-- Rebuild ingest_jobs with a book_id link and proper cascades (the
-- Phase 4 review flagged the missing ON DELETE CASCADE on
-- requested_by_user_id — user deletion would have failed once jobs
-- existed). SQLite cannot ALTER most constraints, so we recreate the
-- table; it was unused before Phase 7 (zero rows in practice).

DROP INDEX IF EXISTS idx_ingest_jobs_status;
DROP TABLE IF EXISTS ingest_jobs;

CREATE TABLE ingest_jobs (
    id TEXT PRIMARY KEY,
    bookshelf_id TEXT NOT NULL REFERENCES bookshelves(id) ON DELETE CASCADE,
    book_id TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    requested_by_user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'done', 'failed')),
    detail TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX idx_ingest_jobs_status ON ingest_jobs(status);
CREATE INDEX idx_ingest_jobs_book ON ingest_jobs(book_id);

-- Full book text lives in the shared encrypted library stacks; the
-- books row is the catalog pointer. Track the library stack path so
-- read_passage can find the text without a scan.
ALTER TABLE books ADD COLUMN stack_path TEXT NOT NULL DEFAULT '';