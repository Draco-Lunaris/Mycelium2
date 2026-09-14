//! Book and ingest-job database helpers (catalog rows; full text lives in
//! the shared encrypted library stacks via the file repo).

use chrono::Utc;
use uuid::Uuid;

use crate::Store;
use crate::models::{IngestJob, IngestStatus};

#[derive(Debug, thiserror::Error)]
pub enum BooksError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("duplicate book slug: {0}")]
    DuplicateSlug(String),
    #[error("book not found: {0}")]
    NotFound(String),
}

impl Store {
    /// Delete a book's catalog row by slug (re-ingest support). The
    /// caller also removes the stack text + catalog concepts.
    pub async fn delete_book_row(&self, slug: &str) -> Result<(), BooksError> {
        let res = sqlx::query("DELETE FROM books WHERE slug = ?")
            .bind(slug)
            .execute(self.pool())
            .await?;
        if res.rows_affected() == 0 {
            return Err(BooksError::NotFound(slug.to_string()));
        }
        Ok(())
    }

    /// Insert a book catalog row. Fails on a duplicate slug.
    pub async fn create_book(
        &self,
        bookshelf_id: Uuid,
        slug: &str,
        title: &str,
        stack_path: &str,
    ) -> Result<Uuid, BooksError> {
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        let res = sqlx::query(
            "INSERT INTO books (id, bookshelf_id, slug, title, stack_path, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(bookshelf_id.to_string())
        .bind(slug)
        .bind(title)
        .bind(stack_path)
        .bind(&now)
        .execute(self.pool())
        .await;
        match res {
            Ok(_) => Ok(id),
            Err(sqlx::Error::Database(e)) if e.message().contains("UNIQUE") => {
                Err(BooksError::DuplicateSlug(slug.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Look up a book by slug.
    pub async fn book_by_slug(&self, slug: &str) -> Result<Option<BookRow>, BooksError> {
        let row: Option<(String, String, String, String, String)> = sqlx::query_as(
            "SELECT id, bookshelf_id, slug, title, stack_path FROM books WHERE slug = ?",
        )
        .bind(slug)
        .fetch_optional(self.pool())
        .await?;
        Ok(
            row.map(|(id, bookshelf_id, slug, title, stack_path)| BookRow {
                id: Uuid::parse_str(&id).unwrap_or_default(),
                bookshelf_id: Uuid::parse_str(&bookshelf_id).unwrap_or_default(),
                slug,
                title,
                stack_path,
            }),
        )
    }

    /// List books on a bookshelf (slug + title), ordered by slug.
    pub async fn books_on_shelf(
        &self,
        bookshelf_id: Uuid,
    ) -> Result<Vec<(String, String)>, BooksError> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT slug, title FROM books WHERE bookshelf_id = ? ORDER BY slug")
                .bind(bookshelf_id.to_string())
                .fetch_all(self.pool())
                .await?;
        Ok(rows)
    }

    /// Create an ingest job (status pending).
    pub async fn create_ingest_job(
        &self,
        bookshelf_id: Uuid,
        book_id: Uuid,
        requested_by: Uuid,
    ) -> Result<Uuid, BooksError> {
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO ingest_jobs (id, bookshelf_id, book_id, requested_by_user_id, status, detail, created_at, updated_at)
             VALUES (?, ?, ?, ?, 'pending', '', ?, ?)",
        )
        .bind(id.to_string())
        .bind(bookshelf_id.to_string())
        .bind(book_id.to_string())
        .bind(requested_by.to_string())
        .bind(&now)
        .bind(&now)
        .execute(self.pool())
        .await?;
        Ok(id)
    }

    /// Update a job's status (+ human-readable detail).
    pub async fn update_ingest_job(
        &self,
        job_id: Uuid,
        status: IngestStatus,
        detail: &str,
    ) -> Result<(), BooksError> {
        let now = Utc::now().to_rfc3339();
        sqlx::query("UPDATE ingest_jobs SET status = ?, detail = ?, updated_at = ? WHERE id = ?")
            .bind(status.as_str())
            .bind(detail)
            .bind(&now)
            .bind(job_id.to_string())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Atomically claim a pending job: transitions `pending → running`
    /// only if the job is still pending. Returns false when another
    /// runner already claimed it (or it is done/failed), so concurrent
    /// `run_pending` passes cannot double-run or clobber a job.
    pub async fn claim_ingest_job(&self, job_id: Uuid) -> Result<bool, BooksError> {
        let now = Utc::now().to_rfc3339();
        let res = sqlx::query(
            "UPDATE ingest_jobs SET status = 'running', detail = '', updated_at = ?
             WHERE id = ? AND status = 'pending'",
        )
        .bind(&now)
        .bind(job_id.to_string())
        .execute(self.pool())
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Fetch a job row.
    pub async fn ingest_job(&self, job_id: Uuid) -> Result<Option<IngestJob>, BooksError> {
        let row: Option<(String, String, String, String, String, String, String)> = sqlx::query_as(
            "SELECT id, bookshelf_id, book_id, requested_by_user_id, status, detail, created_at
             FROM ingest_jobs WHERE id = ?",
        )
        .bind(job_id.to_string())
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(
            |(id, shelf, book, user, status, detail, created_at)| IngestJob {
                id: Uuid::parse_str(&id).unwrap_or_default(),
                bookshelf_id: Uuid::parse_str(&shelf).unwrap_or_default(),
                book_id: Uuid::parse_str(&book).unwrap_or_default(),
                requested_by_user_id: Uuid::parse_str(&user).unwrap_or_default(),
                status: IngestStatus::parse(&status).unwrap_or(IngestStatus::Failed),
                detail,
                created_at: chrono::DateTime::parse_from_rfc3339(&created_at)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_default(),
                updated_at: Utc::now(),
            },
        ))
    }

    /// List recent ingest jobs (newest first), capped.
    pub async fn list_ingest_jobs(&self, limit: u32) -> Result<Vec<IngestJob>, BooksError> {
        let rows: Vec<(String, String, String, String, String, String, String)> = sqlx::query_as(
            "SELECT id, bookshelf_id, book_id, requested_by_user_id, status, detail, created_at
             FROM ingest_jobs ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, shelf, book, user, status, detail, created_at)| IngestJob {
                    id: Uuid::parse_str(&id).unwrap_or_default(),
                    bookshelf_id: Uuid::parse_str(&shelf).unwrap_or_default(),
                    book_id: Uuid::parse_str(&book).unwrap_or_default(),
                    requested_by_user_id: Uuid::parse_str(&user).unwrap_or_default(),
                    status: IngestStatus::parse(&status).unwrap_or(IngestStatus::Failed),
                    detail,
                    created_at: chrono::DateTime::parse_from_rfc3339(&created_at)
                        .map(|d| d.with_timezone(&Utc))
                        .unwrap_or_default(),
                    updated_at: Utc::now(),
                },
            )
            .collect())
    }

    /// Mark stale `running` jobs as failed (boot sweep: a previous
    /// process died mid-job) and return the ids.
    pub async fn fail_stale_running_jobs(&self) -> Result<Vec<Uuid>, BooksError> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT id FROM ingest_jobs WHERE status = 'running'")
                .fetch_all(self.pool())
                .await?;
        let ids = rows
            .into_iter()
            .map(|(id,)| Uuid::parse_str(&id).unwrap_or_default())
            .collect::<Vec<_>>();
        for id in &ids {
            let _ = self
                .update_ingest_job(*id, IngestStatus::Failed, "interrupted by restart")
                .await;
        }
        Ok(ids)
    }

    /// Pending jobs (boot requeue).
    pub async fn pending_ingest_jobs(&self) -> Result<Vec<Uuid>, BooksError> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT id FROM ingest_jobs WHERE status = 'pending'")
                .fetch_all(self.pool())
                .await?;
        Ok(rows
            .into_iter()
            .map(|(id,)| Uuid::parse_str(&id).unwrap_or_default())
            .collect())
    }
}

/// A books catalog row.
#[derive(Debug, Clone)]
pub struct BookRow {
    pub id: Uuid,
    pub bookshelf_id: Uuid,
    pub slug: String,
    pub title: String,
    /// Canonical path of the full text in the shared library stacks
    /// (e.g. `/library/<slug>.md`).
    pub stack_path: String,
}
