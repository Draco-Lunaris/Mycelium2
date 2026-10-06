//! The mutation-queue drain worker: integrates queued writes in the
//! background with bounded LLM capacity, retry/backoff, and the
//! deterministic fallback as the integration guarantee.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mycelium_crypto::keys::MasterKey;
use mycelium_crypto::store_keys::FileKeys;
use mycelium_store::config::ConfigStore;
use mycelium_store::file_repo::{FileRepo, Scope};
use mycelium_store::models::{MutationQueueItem, QueueStatus, QueueTool};
use mycelium_store::mutation_queue::{self, MutationPayload};
use mycelium_store::{ConceptStore, ConceptStoreError, Store};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::{self, AgentError, AgentScopes, MutationResult, TraceSink};
use crate::fallback;
use crate::llm::{LlmClient, LlmConfig};

/// Recovery of a user's master key via the service-key seal.
/// Implemented with MasterKeyCache::for_user at the binary seam
/// (mycelium-mcp is not a librarian dependency — no import cycle).
pub type MasterKeyRecovery = dyn Fn(
        Uuid,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<MasterKey, sqlx::Error>> + Send>,
    > + Send
    + Sync;

// ---- limits ----

/// Queue tunables (spec §5). Admin-configurability deferred; edit the
/// defaults here to retune.
#[derive(Debug, Clone)]
pub struct QueueLimits {
    /// Global concurrent LLM-run cap (agent.rs LLM_RUNS is built to
    /// match; this field documents the number in one place).
    pub capacity: usize,
    /// Max agent steps for background mutation runs (inline runs
    /// keep agent::MAX_STEPS).
    pub drain_step_cap: u32,
    /// Retry attempts before the fallback (excluding the fallback run).
    pub max_attempts: u32,
    /// Per-item integration deadline from enqueue (seconds).
    pub age_deadline_secs: i64,
    /// Max staging+pending+running items per user (enqueue guard).
    pub depth_cap: u32,
    /// Backoff schedule between attempts, seconds.
    pub backoff_secs: [i64; 3],
    /// Periodic drain sweep (seconds).
    pub sweep_secs: u64,
}

impl Default for QueueLimits {
    fn default() -> Self {
        Self {
            capacity: 2,
            drain_step_cap: 12,
            max_attempts: 3,
            age_deadline_secs: 600,
            depth_cap: 50,
            backoff_secs: [30, 60, 120],
            sweep_secs: 60,
        }
    }
}

// ---- error-detail bounding (I1) ----

/// Cap on error text persisted into `mutation_queue.detail` (and
/// re-served through receipts): LLM status errors embed upstream HTTP
/// bodies, so the drain bounds what it persists.
const DETAIL_CAP: usize = 512;

/// Bound an error's Display text for the `detail` column: byte-safe
/// truncation at the cap, cut on a char boundary, with the marker
/// appended only when something was actually cut. Short errors pass
/// through unchanged.
fn short_detail(e: impl std::fmt::Display) -> String {
    let s = e.to_string();
    if s.len() <= DETAIL_CAP {
        return s;
    }
    let mut end = DETAIL_CAP;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated]", &s[..end])
}

// ---- worker ----

pub struct QueueWorker {
    pub store: Arc<Store>,
    pub config: Arc<ConfigStore>,
    pub recovery: Arc<MasterKeyRecovery>,
    pub notify: Arc<tokio::sync::Notify>,
    pub limits: QueueLimits,
    /// Fallback-integration counter. In-process test-visibility aid
    /// only — /metrics does NOT read this atomic; it reads the durable
    /// SQL COUNTs in `Store::queue_totals`, which survive restarts
    /// (this resets with the process).
    pub fallback_count: AtomicU64,
    pub dead_count: AtomicU64,
    pub integrated_count: AtomicU64,
}

#[derive(Debug, thiserror::Error)]
pub enum QueueWorkerError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("queue error: {0}")]
    Queue(#[from] mycelium_store::QueueError),
    #[error("store error: {0}")]
    Store(#[from] mycelium_store::ConceptStoreError),
    #[error("file repo error: {0}")]
    File(#[from] mycelium_store::file_repo::FileRepoError),
}

/// The drain's LLM backend config (admin-managed key "llm"); config
/// errors degrade to the default (Ollama localhost) — the drain's own
/// retry/fallback handles an unreachable backend.
pub async fn llm_config(config: &ConfigStore) -> LlmConfig {
    config
        .get::<LlmConfig>("llm")
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

impl QueueWorker {
    pub fn new(
        store: Arc<Store>,
        config: Arc<ConfigStore>,
        recovery: Arc<MasterKeyRecovery>,
        limits: QueueLimits,
    ) -> Self {
        Self {
            store,
            config,
            recovery,
            notify: Arc::new(tokio::sync::Notify::new()),
            limits,
            fallback_count: AtomicU64::new(0),
            dead_count: AtomicU64::new(0),
            integrated_count: AtomicU64::new(0),
        }
    }

    /// The enqueue-signal handle (the MCP enqueue path notifies this).
    pub fn notify(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.notify)
    }

    /// The wake loop: drain on enqueue signal, on the periodic sweep,
    /// or stop on shutdown. The only spawn signature (Task 9's main.rs
    /// calls `worker.spawn(shutdown.clone())` on the Arc'd worker).
    pub fn spawn(self: Arc<Self>, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(self.limits.sweep_secs));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = self.notify.notified() => {}
                    _ = ticker.tick() => {}
                    _ = shutdown.cancelled() => {
                        // Bounded shutdown: one last drain of due items
                        // happens via the boot sweep on next start (spec
                        // §6: no new guarantee, no new loss window).
                        tracing::info!("mutation queue drain stopped");
                        return;
                    }
                }
                if let Err(e) = self.run_pending().await {
                    tracing::error!(error = %e, "mutation queue drain pass failed");
                }
            }
        })
    }

    /// One drain pass: users with due items, oldest first; per user,
    /// claim + process one item, then re-check (the drain also
    /// reconciles that user's orphaned staging notes here). Returns
    /// the count of processed items.
    pub async fn run_pending(&self) -> Result<usize, QueueWorkerError> {
        let mut processed = 0usize;
        loop {
            let users = self.store.queue_users_with_due().await?;
            if users.is_empty() {
                break;
            }
            let mut progress = 0usize;
            for uid in users {
                // Orphan staging-note reconciliation for this user
                // (rows terminal but the note still present).
                self.reconcile_notes(uid).await;
                if self.process_next_for_user(uid).await? {
                    processed += 1;
                    progress += 1;
                }
            }
            if progress == 0 {
                break;
            }
        }
        Ok(processed)
    }

    /// Boot sweep: running rows (a dead process left them mid-flight)
    /// go back to pending; staging rows (the enqueue phase died) are
    /// completed or failed by payload existence.
    pub async fn recover_on_boot(&self) -> Result<(), QueueWorkerError> {
        // Running → pending. Direct SQL: queue-specific state repair
        // the store module didn't need to expose.
        for item in self.store.running_rows().await? {
            let now = chrono::Utc::now().to_rfc3339();
            sqlx::query(
                "UPDATE mutation_queue
                 SET status = 'pending', detail = ?, updated_at = ?, next_retry_at = NULL
                 WHERE id = ? AND status = 'running'",
            )
            .bind("interrupted by restart")
            .bind(&now)
            .bind(item.id.to_string())
            .execute(self.store.pool())
            .await?;
        }
        // Staging rows: payload durable → activate; missing → dead
        // (operator-visible). The existence check is filename-level
        // (HMAC of the canonical payload path), which needs the row's
        // user's master key via the recovery callback; a user whose
        // key can't be recovered fails fast instead of blocking boot.
        for item in self.store.staging_rows().await? {
            let master = match ((self.recovery)(item.user_id)).await {
                Ok(m) => m,
                Err(_) => {
                    // No key → the blob's stored name cannot be derived,
                    // so terminal cleanup can only skip it (it stays
                    // sealed, same as the row's content).
                    self.store
                        .mark_dead(
                            item.id,
                            "payload check unavailable: master-key recovery failed",
                        )
                        .await
                        .map_err(QueueWorkerError::Queue)?;
                    continue;
                }
            };
            let name = FileKeys::from_master_key(&master)
                .stored_name(&mutation_queue::payload_path(item.id));
            if self.store.user_dir(item.user_id).join(name).exists() {
                self.store.activate(item.id).await?;
            } else {
                self.store
                    .mark_dead(
                        item.id,
                        "payload missing — enqueue interrupted before durability; re-submit",
                    )
                    .await
                    .map_err(QueueWorkerError::Queue)?;
                // Terminal dead: drop the blob (no-op here — the
                // existence check just proved it absent; kept for
                // uniformity with the other dead flips).
                self.delete_payload(item.user_id, &master, item.id).await;
            }
        }
        Ok(())
    }

    /// One user's next due item: capacity first (try_acquire — query
    /// priority, never block), then claim, recover the key, read the
    /// payload, and run — or take the fallback. Per-item LLM/agent
    /// errors are classified here (retry/fallback/dead), never
    /// surfaced above the item.
    async fn process_next_for_user(&self, uid: Uuid) -> Result<bool, QueueWorkerError> {
        // Capacity first: don't claim anything we can't run now
        // (claiming then reverting would burn the retry's attempts
        // accounting). try_acquire only — the query-priority
        // invariant: no permit → skip this wake, never block.
        let Ok(permit) = agent::LLM_RUNS.try_acquire() else {
            return Ok(false);
        };
        let Some(item) = self.store.claim_due_for_user(uid).await? else {
            drop(permit);
            return Ok(false);
        };

        // Key recovery (the sealed blob unseals here; failures defer —
        // NOT an attempt, per defer_retry's contract). But never defer
        // forever: past deadline + 300s of grace, run_fallback(None)
        // deads the item (spec §6 — the fallback can't run without
        // the key either).
        let master = match ((self.recovery)(uid)).await {
            Ok(m) => m,
            Err(e) => {
                drop(permit);
                let age = self.age_past(&item);
                let why = format!("master-key recovery failed: {e}");
                if age >= self.limits.age_deadline_secs + 300 {
                    return self.run_fallback(&item, None, &why).await.map(|()| true);
                }
                return self.defer_retry(&item, &why).await.map(|()| true);
            }
        };

        // Payload read (fail fast if the enqueue died pre-durability).
        let repo = FileRepo::new(self.store.user_dir(uid));
        let payload: MutationPayload = match mutation_queue::read_payload(&repo, &master, item.id)
            .await
        {
            Ok(Some(p)) => p,
            Ok(None) => {
                drop(permit);
                self.store
                        .mark_dead(
                            item.id,
                            "payload missing — enqueue interrupted before durability; re-submit the mutation",
                        )
                        .await
                        .map_err(QueueWorkerError::Queue)?;
                // Terminal dead: drop the blob (a no-op when absent; a
                // present-but-unparseable one gets cleaned up too).
                self.delete_payload(uid, &master, item.id).await;
                self.reconcile_notes(uid).await;
                return Ok(true);
            }
            Err(e) => {
                drop(permit);
                // Unseal/corrupt errors: dead (content unreadable;
                // a retry can't fix a key/path mismatch).
                self.store
                    .mark_dead(
                        item.id,
                        &format!("payload unreadable: {}", short_detail(&e)),
                    )
                    .await
                    .map_err(QueueWorkerError::Queue)?;
                // Terminal dead: the blob is unreadable garbage — drop it.
                self.delete_payload(uid, &master, item.id).await;
                self.reconcile_notes(uid).await;
                return Ok(true);
            }
        };

        // Age deadline: past it, skip the LLM entirely.
        if self.age_past(&item) >= self.limits.age_deadline_secs {
            drop(permit);
            return self
                .run_fallback(&item, Some(&master), "age deadline passed")
                .await
                .map(|()| true);
        }

        let result = self.run_item(&item, &master, &payload).await;

        // Attempt accounting (`attempts` was already incremented in
        // the claim, so `item.attempts` is 1-based: first failure has
        // attempts == 1).
        match result {
            Ok(res) => {
                drop(permit);
                let cs = self.user_cs(uid, &master);
                self.finish_success(&item, &master, &cs, res).await?;
                Ok(true)
            }
            Err(e) => {
                drop(permit);
                if Self::is_transient(&e)
                    && item.attempts < self.limits.max_attempts
                    && self.age_past(&item) < self.limits.age_deadline_secs
                {
                    // attempts == 1 → backoff[0], attempts == 2 →
                    // backoff[1]: index = attempts − 1, clamped.
                    let idx = usize::try_from(item.attempts)
                        .unwrap_or(3)
                        .saturating_sub(1)
                        .min(self.limits.backoff_secs.len() - 1);
                    let backoff = self.limits.backoff_secs[idx];
                    let next =
                        (chrono::Utc::now() + chrono::Duration::seconds(backoff)).to_rfc3339();
                    self.store
                        .mark_failed(
                            item.id,
                            next,
                            &format!("attempt {}: {}", item.attempts, short_detail(&e)),
                        )
                        .await
                        .map_err(QueueWorkerError::Queue)?;
                    Ok(true)
                } else {
                    // Deterministic error, attempts exhausted, or
                    // in-flight deadline pass → fallback (the
                    // provenance names which).
                    let why = if !Self::is_transient(&e) {
                        format!("deterministic error: {}", short_detail(&e))
                    } else {
                        format!(
                            "attempts exhausted ({} of {})",
                            item.attempts, self.limits.max_attempts
                        )
                    };
                    self.run_fallback(&item, Some(&master), &why)
                        .await
                        .map(|()| true)
                }
            }
        }
    }

    /// Rebuild the instruction EXACTLY as the inline tool would have
    /// (the wrapper directives are content-shaping, not state), then
    /// run the agent with the drain's step cap. The drain's single
    /// permit is already held by the caller.
    async fn run_item(
        &self,
        item: &MutationQueueItem,
        master: &MasterKey,
        payload: &MutationPayload,
    ) -> Result<MutationResult, AgentError> {
        let uid = item.user_id;
        let client = LlmClient::new(&llm_config(&self.config).await);
        let cs = self.user_cs(uid, master);
        let scopes = AgentScopes {
            skills: None,
            library: None,
            read_stack_text: None,
            visible_slugs: None,
            trace: Some(TraceSink {
                pool: self.store.pool().clone(),
                scope_id: format!("user:{uid}"),
            }),
        };
        let instruction = match payload.tool {
            QueueTool::Add => {
                let args: serde_json::Value =
                    serde_json::from_str(&payload.args_json).unwrap_or_default();
                fallback::add_instruction(
                    &payload.content,
                    args.get("path").and_then(|v| v.as_str()),
                    args.get("concept_type").and_then(|v| v.as_str()),
                )
            }
            QueueTool::Update => {
                let args: serde_json::Value =
                    serde_json::from_str(&payload.args_json).unwrap_or_default();
                fallback::update_instruction(
                    &payload.content,
                    args.get("path").and_then(|v| v.as_str()),
                )
            }
            // The maintain instruction needs live graph state, derived
            // from the store at run time exactly like the inline tool
            // (a healthy graph is NOT an early return here — the agent
            // answers healthy-nothing-to-do and the row converges to
            // done).
            QueueTool::Maintain => {
                fallback::maintain_instruction(&cs)
                    .await
                    .map_err(AgentError::Store)?
                    .0
            }
        };
        agent::run_mutation_capped(
            &client,
            &cs,
            &scopes,
            &instruction,
            self.limits.drain_step_cap,
        )
        .await
    }

    /// The deterministic integration guarantee: run the fallback for
    /// one item and mark the row from its outcome. `master: None`
    /// (key recovery failed past the deadline + grace) deads the item
    /// per spec §6 — the fallback cannot run without the key either.
    async fn run_fallback(
        &self,
        item: &MutationQueueItem,
        master: Option<&MasterKey>,
        provenance: &str,
    ) -> Result<(), QueueWorkerError> {
        let Some(master) = master else {
            // No key → the payload blob's stored name cannot be derived,
            // so terminal cleanup can only skip it (it stays sealed and
            // unrecoverable, same as the row's content).
            self.store
                .mark_dead(item.id, "fallback unavailable: master-key recovery failed")
                .await
                .map_err(QueueWorkerError::Queue)?;
            self.dead_count.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        let cs = self.user_cs(item.user_id, master);
        let repo = FileRepo::new(self.store.user_dir(item.user_id));
        // The payload may be missing (enqueue died pre-durability, or
        // corrupted) — dead, never a panic.
        let Some(payload) = mutation_queue::read_payload(&repo, master, item.id).await? else {
            self.store
                .mark_dead(
                    item.id,
                    "payload missing — enqueue interrupted before durability; re-submit the mutation",
                )
                .await
                .map_err(QueueWorkerError::Queue)?;
            // Terminal dead: drop the blob (no-op when absent; cleans a
            // present-but-unparseable one).
            self.delete_payload(item.user_id, master, item.id).await;
            self.dead_count.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        // Fallbacks operate on the args + content; args come out of
        // args_json (a JSON object string) per tool.
        let args_json: serde_json::Value =
            serde_json::from_str(&payload.args_json).unwrap_or(serde_json::json!({}));
        let arg = |key: &str| args_json.get(key).and_then(serde_json::Value::as_str);
        let outcome: Result<Vec<String>, ConceptStoreError> = match payload.tool {
            QueueTool::Add => fallback::direct_write_add(
                &cs,
                &payload.content,
                arg("path"),
                arg("shelf"),
                arg("concept_type"),
            )
            .await
            .map(|p| vec![p]),
            QueueTool::Update => {
                fallback::dated_addendum_update(&cs, &payload.content, arg("path"))
                    .await
                    .map(|p| vec![p])
            }
            // Maintenance edits multiple concepts; final_paths is the
            // exact before/after path diff, no guesswork.
            QueueTool::Maintain => {
                let before: std::collections::BTreeSet<String> =
                    cs.list().await?.into_iter().map(|e| e.path).collect();
                let _summary = fallback::wire_and_flag_maintain(&cs).await?;
                let after: std::collections::BTreeSet<String> =
                    cs.list().await?.into_iter().map(|e| e.path).collect();
                Ok(after.symmetric_difference(&before).cloned().collect())
            }
        };
        match outcome {
            Ok(paths) => {
                self.store
                    .mark_done(
                        item.id,
                        &paths,
                        &format!("integrated via deterministic fallback ({provenance})"),
                    )
                    .await
                    .map_err(QueueWorkerError::Queue)?;
                // Terminal done: drop the payload blob (the integrated
                // concept holds the content now).
                self.delete_payload(item.user_id, master, item.id).await;
                self.delete_staging_note(&cs, item.id).await?;
                self.fallback_count.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                self.store
                    .mark_dead(item.id, &format!("fallback failed: {}", short_detail(&e)))
                    .await
                    .map_err(QueueWorkerError::Queue)?;
                // Terminal dead: drop the payload blob, then the note
                // cleanup (best-effort even on dead).
                self.delete_payload(item.user_id, master, item.id).await;
                self.delete_staging_note(&cs, item.id).await?;
                self.dead_count.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
    }

    /// Terminal success from an agent run: mark done with the run's
    /// files + summary, drop the payload blob and the staging note.
    async fn finish_success(
        &self,
        item: &MutationQueueItem,
        master: &MasterKey,
        cs: &ConceptStore<'_>,
        res: MutationResult,
    ) -> Result<(), QueueWorkerError> {
        self.store
            .mark_done(item.id, &res.files_changed, &res.summary)
            .await
            .map_err(QueueWorkerError::Queue)?;
        // Terminal done: drop the payload blob (the integrated concept
        // holds the content now).
        self.delete_payload(item.user_id, master, item.id).await;
        self.delete_staging_note(cs, item.id).await?;
        self.integrated_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Delete an item's staged payload blob (the user-scope FileRepo
    /// file at /mutation-queue/<id>): terminal cleanup. After done the
    /// integrated concept holds the content; after dead the recovery
    /// is a fresh enqueue — nothing ever re-reads the blob past the
    /// terminal flip, so it is dropped there (only there: the retry
    /// path re-reads it between attempts). Best-effort — a failure
    /// logs and never fails the row transition; a missing blob is
    /// already the desired state (delete is a no-op), and a
    /// present-but-corrupt one is cleaned up too.
    async fn delete_payload(&self, uid: Uuid, master: &MasterKey, id: Uuid) {
        let repo = FileRepo::new(self.store.user_dir(uid));
        if let Err(e) = repo
            .delete(
                &mutation_queue::payload_path(id),
                &Scope::User(master.clone()),
            )
            .await
        {
            tracing::warn!(error = %e, receipt = %id, "terminal payload-blob delete failed");
        }
    }

    /// Delete an item's staging note (present only for `add` items);
    /// the internal bypass — /mutation-queue/ is reserved for exactly
    /// this path.
    async fn delete_staging_note(
        &self,
        cs: &ConceptStore<'_>,
        id: Uuid,
    ) -> Result<(), ConceptStoreError> {
        let path = mutation_queue::stage_note_path(id);
        match cs.get(&path).await {
            Ok(_) => {
                cs.delete_internal(&path).await?;
                crate::hot_memory::record_hot_delete(&cs.scope_id(), &path);
            }
            Err(ConceptStoreError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Best-effort: delete any /mutation-queue/<id>.md staging note
    /// whose row is terminal (done/dead) — the row and the note are
    /// reconciled by id (spec §6). Swallow+log errors; idempotent, the
    /// next wake retries.
    async fn reconcile_notes(&self, uid: Uuid) {
        let Ok(master) = ((self.recovery)(uid)).await else {
            return;
        };
        let cs = self.user_cs(uid, &master);
        let Ok(entries) = cs.list().await else {
            return;
        };
        for entry in entries {
            if !entry.path.starts_with("/mutation-queue/") {
                continue;
            }
            let Some(id) = entry
                .path
                .trim_start_matches("/mutation-queue/")
                .trim_end_matches(".md")
                .parse::<Uuid>()
                .ok()
            else {
                continue;
            };
            if let Ok(Some(item)) = self.store.queue_item(uid, id).await
                && matches!(item.status, QueueStatus::Done | QueueStatus::Dead)
                && let Err(e) = self.delete_staging_note(&cs, id).await
            {
                tracing::warn!(error = %e, path = %entry.path, "staging-note reconcile failed");
            }
        }
    }

    /// Re-arm the retry timer without spending an attempt (the claim
    /// already counted this wake): a small fixed defer for stalls the
    /// item itself didn't cause (key recovery).
    async fn defer_retry(
        &self,
        item: &MutationQueueItem,
        why: &str,
    ) -> Result<(), QueueWorkerError> {
        let next = (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339();
        self.store
            .mark_failed(item.id, next, why)
            .await
            .map_err(QueueWorkerError::Queue)
    }

    /// Seconds since the item was enqueued; unparseable created_at
    /// treats as fully aged (fail toward the fallback, never away).
    fn age_past(&self, item: &MutationQueueItem) -> i64 {
        chrono::DateTime::parse_from_rfc3339(&item.created_at)
            .map(|t| chrono::Utc::now().signed_duration_since(t).num_seconds())
            .unwrap_or(i64::MAX)
    }

    /// A user-scoped concept store over the worker's store handle.
    fn user_cs(&self, uid: Uuid, master: &MasterKey) -> ConceptStore<'_> {
        ConceptStore::for_user(self.store.as_ref(), uid, master.clone())
    }

    /// Is the error worth a retry (spec D6)? Transport failures and
    /// the step cap are transient; store errors and bad tool args are
    /// deterministic.
    fn is_transient(e: &AgentError) -> bool {
        match e {
            AgentError::StepCapExceeded => true,
            AgentError::Llm(crate::llm::LlmError::Request(_)) => true,
            AgentError::Llm(crate::llm::LlmError::EmptyResponse) => true,
            AgentError::Llm(crate::llm::LlmError::Status { status, .. }) => {
                (500..600).contains(status)
            }
            // BadToolArgs and Store errors are deterministic.
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::short_detail;

    #[test]
    fn short_detail_caps_byte_safe() {
        // Under the cap: unchanged, no marker.
        assert_eq!(short_detail("boom"), "boom");
        assert_eq!(short_detail("x".repeat(512)), "x".repeat(512));
        // Over the cap (ASCII): cut at the cap + marker.
        let out = short_detail("x".repeat(600));
        assert_eq!(out.len(), 512 + "… [truncated]".len());
        assert!(out.ends_with("[truncated]"));
        // Over the cap (multibyte): the cut backs off to a char
        // boundary — no panic, never past the cap + marker. "€" is
        // 3 bytes, so byte 512 lands mid-char and the back-off loop
        // must move the cut to 510.
        let out = short_detail("€".repeat(200)); // 600 bytes
        assert!(out.ends_with("[truncated]"));
        assert_eq!(out.len(), 510 + "… [truncated]".len());
        assert!(out.starts_with(&"€".repeat(100)));
    }
}
