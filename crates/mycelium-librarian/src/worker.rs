//! In-process async librarian worker.
//!
//! One ingest job per user at a time (DESIGN). Jobs are tracked in the
//! `ingest_jobs` table; the worker polls for pending jobs, runs them
//! (LLM-assisted catalog with heuristic fallback), and updates status.
//! On boot, stale `running` jobs (previous process died) are failed and
//! `pending` jobs are requeued. [`LibrarianWorker::spawn`] runs the same
//! pending sweep on a wake signal and a periodic ticker, so a job left
//! behind by a failed boot pass (or an LLM outage) is picked up on a
//! later tick instead of waiting for the next restart (outage-recovery
//! spec §3.5).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use mycelium_crypto::keys::ServiceKey;
use mycelium_store::{ConfigStore, IngestStatus, Store};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
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

/// Periodic pending-sweep interval for the spawn loop (spec §3.5; the
/// same cadence the mutation-queue drain uses).
const SWEEP_SECS: u64 = 60;

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
    /// Sweep-signal handle ([`LibrarianWorker::spawn`] wakes on it, the
    /// queue worker's shape).
    notify: Arc<tokio::sync::Notify>,
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
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// The sweep-signal handle (callers that enqueue work can wake the
    /// spawn loop immediately instead of waiting for the next tick).
    pub fn notify(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.notify)
    }

    /// The wake loop: sweep pending ingest jobs on the signal, on the
    /// periodic tick, or stop on shutdown (queue-worker shape, spec
    /// §3.5). The boot block's inline pass stays for immediacy; this
    /// loop is what makes a failed boot pass — or an LLM outage —
    /// non-terminal for pending jobs, since ingest completes on the
    /// heuristic outline fallback and a later tick re-runs anything
    /// still pending. The only spawn signature (main.rs calls
    /// `librarian.clone().spawn(shutdown.clone())` on the Arc'd
    /// worker).
    pub fn spawn(self: Arc<Self>, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(SWEEP_SECS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = self.notify.notified() => {}
                    _ = ticker.tick() => {}
                    _ = shutdown.cancelled() => {
                        // A job left pending at shutdown is picked up
                        // by the next boot's sweep (or this loop's
                        // first tick if the process comes back).
                        tracing::info!("ingest sweep stopped");
                        return;
                    }
                }
                if let Err(e) = self.run_pending().await {
                    tracing::error!(error = %e, "ingest sweep pass failed");
                }
            }
        })
    }

    /// The current LLM config (admin-managed; Ollama default). Sealed
    /// rows decrypt under the service key; legacy plaintext rows read
    /// as before.
    async fn llm_config(&self) -> LlmConfig {
        mycelium_store::config::get_sealed(&self.config, &self.service_key, "llm")
            .await
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
            extract::build_catalog(Some(&client), slug, title, &outline, text).await;
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
        // run_pending drives a real LlmClient (unreachable default URL
        // -> a backend-down note on the global health registry) —
        // serialize against the health-state tests.
        let _health = crate::llm_health::health_test_guard().await;
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
        let _health = crate::llm_health::health_test_guard().await;
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
        let _health = crate::llm_health::health_test_guard().await;
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
        let _health = crate::llm_health::health_test_guard().await;
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
        let _health = crate::llm_health::health_test_guard().await;
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

    #[tokio::test]
    async fn pending_job_runs_on_later_tick() {
        // Outage-recovery spec §3.5: a pending row the boot pass never
        // ran must not be stranded — a later sweep pass picks it up and
        // the job completes. run_pending drives a real LlmClient
        // (unreachable default URL → heuristic fallback) — guarded.
        let _health = crate::llm_health::health_test_guard().await;
        let (_dir, worker, shelf) = setup().await;
        let user = Uuid::new_v4();
        seed_user(&worker.store, user).await;
        // A pending row straight in the DB (not via submit — this is
        // the row that survived a boot whose inline pass never ran),
        // with its staged text supplied to the worker.
        let book_id = worker
            .store
            .create_book(
                shelf,
                "my-book",
                "My Book",
                &ingest::stack_path_for("my-book"),
            )
            .await
            .unwrap();
        let job_id = worker
            .store
            .create_ingest_job(shelf, book_id, user)
            .await
            .unwrap();
        worker.staged.lock().await.insert(
            job_id,
            StagedBook {
                slug: "my-book".to_string(),
                title: "My Book".to_string(),
                text: BOOK.to_string(),
            },
        );

        // Boot sweep: the pending row is reported as requeued, never
        // terminalized — this pass does not run jobs.
        let requeued = worker.recover_on_boot().await.unwrap();
        assert!(
            requeued.contains(&job_id),
            "recover_on_boot must report the surviving pending row"
        );
        let status = worker.job_status(job_id).await.unwrap().unwrap();
        assert_eq!(
            status.status,
            IngestStatus::Pending,
            "the boot sweep must not terminalize a pending row"
        );

        // A later pass (what the spawn loop's tick now provides): the
        // surviving row runs and completes. The LLM is unreachable
        // here, so this asserts the heuristic-fallback path: an LLM
        // error alone must NOT terminalize the job as failed.
        assert_eq!(worker.run_pending().await.unwrap(), 1);
        let status = worker.job_status(job_id).await.unwrap().unwrap();
        assert_eq!(
            status.status,
            IngestStatus::Done,
            "an LLM error must not terminalize the job: {}",
            status.detail
        );
        assert!(status.detail.contains("3 catalog concepts"));
    }

    #[tokio::test]
    async fn spawn_wakes_on_notify() {
        // The queue-worker wake loop, end to end: spawn → notify →
        // job processed → cancel → clean stop (JoinHandle resolves).
        // The pass drives a real LlmClient — guarded.
        let _health = crate::llm_health::health_test_guard().await;
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        let (_dir, worker, shelf) = setup().await;
        let worker = Arc::new(worker);
        let user = Uuid::new_v4();
        seed_user(&worker.store, user).await;

        let shutdown = CancellationToken::new();
        let handle = worker.clone().spawn(shutdown.clone());

        // Let the loop's immediate first tick run (finds no jobs), so
        // the processing below can only come from the notify wake.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let job = worker
            .submit(shelf, user, "my-book", "My Book", BOOK)
            .await
            .unwrap();
        worker.notify().notify_one();

        // The wake runs the job to Done (unreachable LLM → heuristic
        // fallback completes it).
        let mut terminal = None;
        for _ in 0..100 {
            let status = worker.job_status(job.job_id).await.unwrap().unwrap();
            if !matches!(status.status, IngestStatus::Pending | IngestStatus::Running) {
                terminal = Some(status);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let status = terminal.expect("job did not reach a terminal status within 2s");
        assert_eq!(
            status.status,
            IngestStatus::Done,
            "the notify pass must run the job to completion: {}",
            status.detail
        );

        // Shutdown: the loop exits cleanly and the JoinHandle resolves.
        shutdown.cancel();
        let joined = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("spawn loop must stop on shutdown");
        joined.expect("spawn loop task panicked");
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
