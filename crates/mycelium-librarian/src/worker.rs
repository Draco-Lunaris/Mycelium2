//! In-process async librarian worker.
//!
//! One ingest job per user at a time (DESIGN). Jobs are tracked in the
//! `ingest_jobs` table; the worker polls for pending jobs, runs them
//! (LLM-assisted catalog with heuristic fallback), and updates status.
//! On boot, stale `running` jobs (previous process died) are failed and
//! `pending` jobs are requeued.

use std::collections::HashMap;
use std::sync::Arc;

use mycelium_crypto::keys::ServiceKey;
use mycelium_store::{ConfigStore, IngestStatus, Store};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::extract::{self, BookCatalog};
use crate::ingest;
use crate::llm::{LlmClient, LlmConfig};

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("store error: {0}")]
    Store(#[from] mycelium_store::BooksError),
    #[error("ingest error: {0}")]
    Ingest(#[from] ingest::IngestError),
    #[error("book not found: {0}")]
    BookNotFound(String),
    #[error("user already has an ingest job running")]
    UserBusy,
}

/// A submitted job (returned to the HTTP layer for status polling).
#[derive(Debug, Clone)]
pub struct SubmittedJob {
    pub job_id: Uuid,
    pub book_id: Uuid,
    pub slug: String,
}

/// The librarian worker.
pub struct LibrarianWorker {
    store: Arc<Store>,
    service_key: Arc<ServiceKey>,
    /// Per-user "one job at a time" slots: a user with a running job
    /// cannot submit another until it finishes.
    user_slots: Arc<Mutex<HashMap<Uuid, Uuid>>>,
    /// The staged book texts, keyed by job id (upload → run handoff).
    staged: Arc<Mutex<HashMap<Uuid, StagedBook>>>,
    /// Runtime LLM config source (admin-editable; read per job).
    config: Arc<ConfigStore>,
}

/// A staged book awaiting ingest.
#[derive(Debug, Clone)]
struct StagedBook {
    slug: String,
    title: String,
    text: String,
}

impl LibrarianWorker {
    pub fn new(store: Arc<Store>, service_key: Arc<ServiceKey>, config: Arc<ConfigStore>) -> Self {
        Self {
            store,
            service_key,
            user_slots: Arc::new(Mutex::new(HashMap::new())),
            staged: Arc::new(Mutex::new(HashMap::new())),
            config,
        }
    }

    /// The current LLM config (admin-managed; Ollama default).
    async fn llm_config(&self) -> LlmConfig {
        self.config
            .get::<LlmConfig>("llm")
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// Boot sweep: fail stale running jobs, requeue pending ones.
    /// Returns the requeued job ids.
    pub async fn recover_on_boot(&self) -> Result<Vec<Uuid>, WorkerError> {
        let stale = self.store.fail_stale_running_jobs().await?;
        for id in &stale {
            tracing::warn!(job_id = %id, "marked interrupted ingest job as failed");
        }
        let pending = self.store.pending_ingest_jobs().await?;
        Ok(pending)
    }

    /// Submit a book for ingest. Creates the book + job rows, stages
    /// the text, and returns the job handle. Fails when the user
    /// already has a job running.
    pub async fn submit(
        &self,
        bookshelf_id: Uuid,
        requested_by: Uuid,
        slug: &str,
        title: &str,
        text: &str,
    ) -> Result<SubmittedJob, WorkerError> {
        // One job per user at a time: check + reserve atomically under
        // the slot lock so two concurrent submits cannot both pass.
        {
            let mut slots = self.user_slots.lock().await;
            if slots.contains_key(&requested_by) {
                return Err(WorkerError::UserBusy);
            }
            slots.insert(requested_by, Uuid::nil());
        }
        let book_id = self
            .store
            .create_book(bookshelf_id, slug, title, &ingest::stack_path_for(slug))
            .await;
        if let Err(e) = book_id {
            self.user_slots.lock().await.remove(&requested_by);
            return Err(e.into());
        }
        let book_id = book_id.unwrap();
        let job_id = self
            .store
            .create_ingest_job(bookshelf_id, book_id, requested_by)
            .await;
        if let Err(e) = job_id {
            self.user_slots.lock().await.remove(&requested_by);
            return Err(e.into());
        }
        let job_id = job_id.unwrap();
        self.staged.lock().await.insert(
            job_id,
            StagedBook {
                slug: slug.to_string(),
                title: title.to_string(),
                text: text.to_string(),
            },
        );
        // Point the reserved slot at the real job id.
        self.user_slots.lock().await.insert(requested_by, job_id);
        Ok(SubmittedJob {
            job_id,
            book_id,
            slug: slug.to_string(),
        })
    }

    /// Run all pending jobs to completion (one pass). Returns the
    /// number of jobs processed. The web layer calls this after each
    /// upload and at boot; a long-running poll loop can also call it
    /// periodically.
    pub async fn run_pending(&self) -> Result<usize, WorkerError> {
        let pending = self.store.pending_ingest_jobs().await?;
        let mut processed = 0;
        for job_id in pending {
            self.run_job(job_id).await?;
            processed += 1;
        }
        Ok(processed)
    }

    /// Run one job: atomically claim it (pending → running), ingest,
    /// mark done/failed. Concurrent runners cannot double-run a job:
    /// the claim fails when another pass already took it. The web layer
    /// calls this for the just-submitted job; `run_pending` uses it for
    /// the whole queue.
    pub async fn run_job(&self, job_id: Uuid) -> Result<(), WorkerError> {
        let Some(job) = self.store.ingest_job(job_id).await? else {
            return Ok(());
        };
        // Atomically claim: only proceed when the job was still pending.
        if !self.store.claim_ingest_job(job_id).await? {
            return Ok(()); // another runner took it (or it is terminal)
        }
        // Take the staged text (jobs submitted before a restart have no
        // staged text — fail them; the upload must be retried).
        let staged = self.staged.lock().await.remove(&job_id);
        let Some(staged) = staged else {
            self.store
                .update_ingest_job(
                    job_id,
                    IngestStatus::Failed,
                    "staged text lost after restart — re-upload the book",
                )
                .await?;
            self.user_slots
                .lock()
                .await
                .remove(&job.requested_by_user_id);
            return Ok(());
        };
        let result = self
            .ingest_one(&staged.slug, &staged.title, &staged.text)
            .await;
        match result {
            Ok(count) => {
                self.store
                    .update_ingest_job(
                        job_id,
                        IngestStatus::Done,
                        &format!("{count} catalog concepts written"),
                    )
                    .await?;
            }
            Err(e) => {
                tracing::error!(job_id = %job_id, error = %e, "ingest job failed");
                self.store
                    .update_ingest_job(job_id, IngestStatus::Failed, &e.to_string())
                    .await?;
            }
        }
        self.user_slots
            .lock()
            .await
            .remove(&job.requested_by_user_id);
        Ok(())
    }

    /// Ingest one book: LLM-assisted catalog (heuristic fallback) +
    /// stack text + catalog concepts.
    async fn ingest_one(&self, slug: &str, title: &str, text: &str) -> Result<usize, WorkerError> {
        let outline = extract::parse_outline(text);
        let client = LlmClient::new(&self.llm_config().await);
        let catalog: BookCatalog =
            extract::build_catalog(Some(&client), slug, title, &outline).await;
        let written =
            ingest::ingest_book(&self.store, &self.service_key, slug, title, text, &catalog)
                .await?;
        Ok(written.len())
    }

    /// Job status for the HTTP layer.
    pub async fn job_status(&self, job_id: Uuid) -> Result<Option<JobStatus>, WorkerError> {
        let Some(job) = self.store.ingest_job(job_id).await? else {
            return Ok(None);
        };
        Ok(Some(JobStatus {
            id: job.id,
            status: job.status,
            detail: job.detail,
        }))
    }
}

/// Status shape returned to the HTTP layer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct JobStatus {
    pub id: Uuid,
    pub status: IngestStatus,
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use mycelium_store::Store;

    const BOOK: &str = "\
# Chapter One

Intro text.

## Section 1.1

Details one.

# Chapter Two

Second chapter text.
";

    async fn setup() -> (tempfile::TempDir, LibrarianWorker, Uuid) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let shelf = store.create_bookshelf("Shelf", true).await.unwrap();
        let config = ConfigStore::new(store.pool().clone());
        let worker = LibrarianWorker::new(
            Arc::new(store),
            Arc::new(ServiceKey::from_bytes(&[9u8; 32]).unwrap()),
            Arc::new(config),
        );
        (dir, worker, shelf)
    }

    #[tokio::test]
    async fn submit_run_and_read() {
        let (_dir, worker, shelf) = setup().await;
        let user = Uuid::new_v4();
        // Seed the user row (FK requirement).
        seed_user(&worker.store, user).await;

        let job = worker
            .submit(shelf, user, "my-book", "My Book", BOOK)
            .await
            .unwrap();
        assert_eq!(job.slug, "my-book");

        // Run to completion (LLM unreachable → heuristic catalog).
        let processed = worker.run_pending().await.unwrap();
        assert_eq!(processed, 1);

        let status = worker.job_status(job.job_id).await.unwrap().unwrap();
        assert_eq!(status.status, IngestStatus::Done);
        assert!(status.detail.contains("3 catalog concepts"));

        // The catalog is readable from the shared stacks.
        let text = ingest::read_stack_text(&worker.store, &worker.service_key, "my-book")
            .await
            .unwrap();
        assert!(text.contains("# Chapter One"));
    }

    #[tokio::test]
    async fn one_job_per_user_at_a_time() {
        let (_dir, worker, shelf) = setup().await;
        let user = Uuid::new_v4();
        seed_user(&worker.store, user).await;
        let _job1 = worker
            .submit(shelf, user, "book-a", "A", BOOK)
            .await
            .unwrap();
        // Second submit while the first is pending/running → rejected.
        let err = worker.submit(shelf, user, "book-b", "B", BOOK).await;
        assert!(err.is_err());
        // Run the first, then the second succeeds.
        worker.run_pending().await.unwrap();
        let job2 = worker.submit(shelf, user, "book-b", "B", BOOK).await;
        assert!(job2.is_ok());
    }

    #[tokio::test]
    async fn different_users_can_submit_concurrently() {
        let (_dir, worker, shelf) = setup().await;
        let u1 = Uuid::new_v4();
        let u2 = Uuid::new_v4();
        seed_user(&worker.store, u1).await;
        seed_user(&worker.store, u2).await;
        let _a = worker.submit(shelf, u1, "book-a", "A", BOOK).await.unwrap();
        let _b = worker.submit(shelf, u2, "book-b", "B", BOOK).await.unwrap();
        assert_eq!(worker.run_pending().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn failed_job_records_detail() {
        let (_dir, worker, shelf) = setup().await;
        let user = Uuid::new_v4();
        seed_user(&worker.store, user).await;
        let job = worker
            .submit(shelf, user, "bad-book", "Bad", "no headings here")
            .await
            .unwrap();
        worker.run_pending().await.unwrap();
        let status = worker.job_status(job.job_id).await.unwrap().unwrap();
        assert_eq!(status.status, IngestStatus::Failed);
        assert!(status.detail.contains("chapters"));
    }

    #[tokio::test]
    async fn boot_sweep_fails_stale_running() {
        let (_dir, worker, shelf) = setup().await;
        let user = Uuid::new_v4();
        seed_user(&worker.store, user).await;
        let job = worker
            .submit(shelf, user, "my-book", "My Book", BOOK)
            .await
            .unwrap();
        // Simulate a crash mid-run: mark running, clear staged text.
        worker
            .store
            .update_ingest_job(job.job_id, IngestStatus::Running, "")
            .await
            .unwrap();
        worker.staged.lock().await.clear();
        worker.user_slots.lock().await.clear();
        // Boot sweep: the stale running job is failed.
        worker.recover_on_boot().await.unwrap();
        let status = worker.job_status(job.job_id).await.unwrap().unwrap();
        assert_eq!(status.status, IngestStatus::Failed);
        assert!(status.detail.contains("interrupted"));
    }

    #[tokio::test]
    async fn concurrent_runners_cannot_double_run() {
        // Two run_pending passes over the same pending job: the second
        // must not clobber the first's result (atomic claim).
        let (_dir, worker, shelf) = setup().await;
        let user = Uuid::new_v4();
        seed_user(&worker.store, user).await;
        let job = worker
            .submit(shelf, user, "my-book", "My Book", BOOK)
            .await
            .unwrap();
        // First pass claims and completes the job.
        worker.run_pending().await.unwrap();
        // Second pass (e.g. a racing upload or boot sweep) sees no
        // pending jobs — and even a direct run_job on the finished job
        // must be a no-op, not a "staged text lost" failure.
        worker.run_pending().await.unwrap();
        worker.run_job(job.job_id).await.unwrap();
        let status = worker.job_status(job.job_id).await.unwrap().unwrap();
        assert_eq!(
            status.status,
            IngestStatus::Done,
            "terminal status must not be clobbered: {}",
            status.detail
        );
    }

    async fn seed_user(store: &Arc<Store>, user_id: Uuid) {
        // The FK on ingest_jobs.requested_by_user_id requires a users row.
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO users (id, username, email, role, auth_provider, sealed_master_key, must_change_password, created_at, updated_at)
             VALUES (?, ?, ?, 'user', 'local', '{}', 0, ?, ?)",
        )
        .bind(user_id.to_string())
        .bind(format!("user-{}", user_id.simple()))
        .bind(format!("user-{}@example.com", user_id.simple()))
        .bind(&now)
        .bind(&now)
        .execute(store.pool())
        .await
        .unwrap();
    }
}
