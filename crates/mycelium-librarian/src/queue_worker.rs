//! The mutation-queue drain worker: integrates queued writes in the
//! background with bounded LLM capacity, retry/backoff, and the
//! deterministic fallback as the integration guarantee.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use mycelium_crypto::keys::{MasterKey, ServiceKey};
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
use crate::llm_gate;
use crate::llm_health::{BackendState, LLM_HEALTH, short_detail};

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
    /// Global concurrent LLM-run cap (the real cap is
    /// `llm_gate::LLM_GATE`'s capacity, admin-configurable; this field
    /// documents the default the drain was sized against).
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

// `short_detail` (the byte-safe error-detail bound) moved to
// `crate::llm_health` so one copy serves every bounded-detail site.

/// A printable message from a caught panic payload: the two common
/// shapes (`&'static str` from `panic!("literal")`, `String` from a
/// formatted panic) plus a non-string fallback. Never itself panics —
/// the drain's panic isolation must not wedge on its own reporting
/// path (`unwrap_or_else`, never `unwrap`).
fn panic_message(payload: Box<dyn Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".into())
}

// ---- worker ----

pub struct QueueWorker {
    pub store: Arc<Store>,
    pub config: Arc<ConfigStore>,
    pub recovery: Arc<MasterKeyRecovery>,
    /// The service key, needed to decrypt the sealed LLM config row.
    pub service_key: Arc<ServiceKey>,
    pub notify: Arc<tokio::sync::Notify>,
    pub limits: QueueLimits,
    /// Fallback-integration counter. In-process test-visibility aid
    /// only — /metrics does NOT read this atomic; it reads the durable
    /// SQL COUNTs in `Store::queue_totals`, which survive restarts
    /// (this resets with the process).
    pub fallback_count: AtomicU64,
    pub dead_count: AtomicU64,
    pub integrated_count: AtomicU64,
    /// In-process test-visibility aid only (same class as
    /// `fallback_count`): when `Some` and the flag reads true,
    /// `run_item` panics once (the flag self-clears on the swap) so
    /// the drain's catch_unwind leg is exercisable end-to-end from the
    /// integration tests. Production wiring leaves it `None`.
    pub test_panic_on_run: Option<Arc<AtomicBool>>,
    /// Backend-health source consulted before claiming (outage-recovery
    /// spec §1: "claims only happen when healthy"). Defaults to the
    /// process-global [`LLM_HEALTH`]; injectable so integration tests
    /// pin a fixed state — the global is process-wide, this binary runs
    /// tests in parallel, and every mock/dead-port LLM call passively
    /// feeds it (see the fixture note in `tests/queue_worker_integration.rs`).
    pub health_source: Arc<dyn Fn() -> BackendState + Send + Sync>,
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

/// The drain's LLM backend config (admin-managed key "llm"): sealed
/// rows decrypt under the service key, legacy plaintext rows read as
/// before; config errors degrade to the default (Ollama localhost) —
/// the drain's own retry/fallback handles an unreachable backend.
pub async fn llm_config(config: &ConfigStore, service_key: &ServiceKey) -> LlmConfig {
    mycelium_store::config::get_sealed(config, service_key, "llm")
        .await
        .unwrap_or_default()
}

impl QueueWorker {
    pub fn new(
        store: Arc<Store>,
        config: Arc<ConfigStore>,
        recovery: Arc<MasterKeyRecovery>,
        service_key: Arc<ServiceKey>,
        limits: QueueLimits,
    ) -> Self {
        Self {
            store,
            config,
            recovery,
            service_key,
            notify: Arc::new(tokio::sync::Notify::new()),
            limits,
            fallback_count: AtomicU64::new(0),
            dead_count: AtomicU64::new(0),
            integrated_count: AtomicU64::new(0),
            test_panic_on_run: None,
            health_source: Arc::new(|| LLM_HEALTH.state()),
        }
    }

    /// Replace the backend-health source (test seam — stamp before
    /// wrapping the worker in an `Arc`).
    pub fn set_health_source(&mut self, f: Arc<dyn Fn() -> BackendState + Send + Sync>) {
        self.health_source = f;
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
        // Reconcile staging rows first (outage-recovery spec §3.4): the
        // staging scan is global, not due-scan-driven, so rows deferred
        // by an earlier key loss get re-checked on every pass even when
        // no user has a due pending item. Idempotent.
        self.recover_staging().await?;
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
    /// go back to pending; staging rows get the same reconciliation
    /// the periodic drain applies (spec §3.4).
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
        self.recover_staging().await
    }

    /// Staging-row reconciliation — the pass `recover_on_boot` runs
    /// once and `run_pending` re-runs on every wake (outage-recovery
    /// spec §3.4: boot must never issue a permanent verdict for a
    /// recoverable condition). The existence check is filename-level
    /// (HMAC of the canonical payload path), which needs the row's
    /// user's master key via the recovery callback:
    ///
    /// - key unrecoverable → **skip the row this pass** (stays
    ///   `staging`, untouched — no dead verdict, no retry timer). The
    ///   key comes back on its own via the service seal, and the next
    ///   pass re-checks; log-and-continue so one bad row can't stall
    ///   the pass.
    /// - payload blob present → activate (the interrupted flip
    ///   completes).
    /// - payload genuinely absent → dead with the re-submit detail
    ///   (permanent condition keeps its verdict).
    ///
    /// Idempotent: safe to call every tick and after every boot.
    pub async fn recover_staging(&self) -> Result<(), QueueWorkerError> {
        for item in self.store.staging_rows().await? {
            let master = match ((self.recovery)(item.user_id)).await {
                Ok(m) => m,
                Err(e) => {
                    // No key → the blob's stored name cannot be
                    // derived, so the row can be neither completed nor
                    // fairly failed. Defer: it stays `staging` and the
                    // next pass re-checks (a restart during a key
                    // outage must not deaden what a later tick would
                    // recover).
                    tracing::warn!(
                        error = %e,
                        receipt = %item.id,
                        user = %item.user_id,
                        "staging row deferred: master-key recovery failed"
                    );
                    continue;
                }
            };
            let name = FileKeys::from_master_key(&master)
                .stored_name(&mutation_queue::payload_path(item.id));
            if self.store.user_dir(item.user_id).join(name).exists() {
                self.store.activate(item.id).await?;
            } else {
                // Dead flips are never log-silent: the row's persisted
                // detail is an operator's only other clue, and reading
                // it once took raw SQL.
                let detail = "payload missing — enqueue interrupted before durability; re-submit";
                tracing::warn!(
                    receipt = %item.id,
                    user = %item.user_id,
                    detail = %detail,
                    "queue row marked dead: staging payload never became durable"
                );
                self.store
                    .mark_dead(item.id, detail)
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

    /// One user's next due item: backend health first (Red → claim
    /// nothing, spec §1), then capacity (the gate's try class — query
    /// priority, never block), then claim, recover the key, read the
    /// payload, and run — or take the fallback. Per-item LLM/agent
    /// errors are classified here (retry/fallback/dead), never
    /// surfaced above the item.
    async fn process_next_for_user(&self, uid: Uuid) -> Result<bool, QueueWorkerError> {
        // Outage-recovery spec §1 anti-goal ("claims only happen when
        // healthy"): while Red, the probe/passive hooks observed a
        // failing backend — running would burn each row's attempts
        // toward a fallback that hits the same backend and deads it.
        // Yellow proceeds (spec §7: gate on Red only); the source is
        // injectable so tests stay deterministic (the global is
        // process-wide and this binary runs tests in parallel).
        if (self.health_source)() == BackendState::Red {
            return Ok(false);
        }
        // Capacity first: don't claim anything we can't run now
        // (claiming then reverting would burn the retry's attempts
        // accounting). try_enter only — the query-priority invariant:
        // no slot, or anything queued ahead of us, → skip this wake,
        // never block.
        let Some(permit) = llm_gate::LLM_GATE.try_enter() else {
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
                let detail = "payload missing — enqueue interrupted before durability; re-submit the mutation";
                tracing::warn!(
                    receipt = %item.id,
                    user = %item.user_id,
                    detail = %detail,
                    "queue row marked dead: payload missing at claim"
                );
                self.store
                    .mark_dead(item.id, detail)
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
                let detail = format!("payload unreadable: {}", short_detail(&e));
                tracing::error!(
                    receipt = %item.id,
                    user = %item.user_id,
                    detail = %detail,
                    "queue row marked dead: payload unreadable/corrupt"
                );
                self.store
                    .mark_dead(item.id, &detail)
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

        // Belt-and-braces against the 2026-10-31 wedge class: if any
        // panic ever escapes a run's future again, the single drain
        // task survives, this item takes the deterministic-error path
        // (fallback), and later items still drain. `AssertUnwindSafe`
        // is justified: nothing shared is poisoned by an unwind in
        // these futures (no locks held across the await; sqlx state is
        // unaffected).
        let result = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
            self.run_item(&item, &master, &payload),
        ))
        .await
        .unwrap_or_else(|payload| {
            let msg = panic_message(payload);
            tracing::error!(
                receipt = %item.id,
                user = %item.user_id,
                panic = %msg,
                "drain agent run panicked — routing to the deterministic fallback"
            );
            // A panic never reached a real model verdict: deterministic-error shape
            // (non-transient) → fallback, per the classification the drain already owns.
            Err(AgentError::BadToolArgs {
                name: "(panic)".into(),
                reason: format!("agent run panicked: {msg}"),
            })
        });

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
        // Test-injection point for the catch_unwind leg (see the
        // `test_panic_on_run` field doc): fires once, before any work,
        // so an integration test exercises the wrapper's full route —
        // catch, log, deterministic fallback — on the first item only.
        if let Some(flag) = &self.test_panic_on_run
            && flag.swap(false, Ordering::Relaxed)
        {
            panic!("test-injected panic");
        }
        let uid = item.user_id;
        let client = LlmClient::new(&llm_config(&self.config, &self.service_key).await);
        let cs = self.user_cs(uid, master).with_provenance(&item.source);
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
            let detail = "fallback unavailable: master-key recovery failed";
            tracing::warn!(
                receipt = %item.id,
                user = %item.user_id,
                detail = %detail,
                provenance = %provenance,
                "queue row marked dead: fallback unavailable without the master key"
            );
            self.store
                .mark_dead(item.id, detail)
                .await
                .map_err(QueueWorkerError::Queue)?;
            self.dead_count.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        let cs = self
            .user_cs(item.user_id, master)
            .with_provenance(&item.source);
        let repo = FileRepo::new(self.store.user_dir(item.user_id));
        // The payload may be missing (enqueue died pre-durability, or
        // corrupted) — dead, never a panic.
        let Some(payload) = mutation_queue::read_payload(&repo, master, item.id).await? else {
            let detail =
                "payload missing — enqueue interrupted before durability; re-submit the mutation";
            tracing::warn!(
                receipt = %item.id,
                user = %item.user_id,
                detail = %detail,
                provenance = %provenance,
                "queue row marked dead: payload missing at fallback"
            );
            self.store
                .mark_dead(item.id, detail)
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
                let detail = format!("fallback failed: {}", short_detail(&e));
                tracing::error!(
                    receipt = %item.id,
                    user = %item.user_id,
                    detail = %detail,
                    provenance = %provenance,
                    "queue row marked dead: deterministic fallback failed"
                );
                self.store
                    .mark_dead(item.id, &detail)
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
            .mark_done(item.id, &res.files_changed, &short_detail(&res.summary))
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

    /// Is the error worth a retry (spec D6)? Backend-down errors —
    /// transport, 5xx, and 401/403 auth failures (the
    /// [`crate::llm::LlmError::is_backend_down`] classifier: the
    /// request never reached a working model) — plus the step cap and
    /// empty completions are transient; store errors, bad tool args and
    /// content 4xx verdicts are deterministic. 401/403 must retry
    /// rather than fall back: the deterministic fallback never contacts
    /// the LLM, so a one-pass fallback would succeed with the librarian
    /// step silently skipped.
    fn is_transient(e: &AgentError) -> bool {
        match e {
            AgentError::StepCapExceeded => true,
            // Transient today, but a *model outcome* (a reachable model
            // answered nothing), not backend-down — hence its own arm
            // rather than the classifier below.
            AgentError::Llm(crate::llm::LlmError::EmptyResponse) => true,
            AgentError::Llm(e) if e.is_backend_down() => true,
            // BadToolArgs, Store errors and content 4xx are deterministic.
            _ => false,
        }
    }
}

// The `short_detail` tests moved with the function to
// `crate::llm_health::tests`.

#[cfg(test)]
mod tests {
    use super::{QueueWorker, panic_message};
    use crate::agent::AgentError;
    use crate::llm::LlmError;
    use std::any::Any;

    // Task 3: auth-class backend failures (revoked/misconfigured
    // credentials) are backend-down, not content verdicts — they must
    // retry through the backoff instead of one-passing to done via the
    // deterministic fallback (the librarian step silently skipped).
    #[test]
    fn transient_retries_backend_auth_failures() {
        let auth = |s| {
            AgentError::Llm(LlmError::Status {
                status: s,
                body: "x".into(),
            })
        };
        assert!(QueueWorker::is_transient(&auth(401)));
        assert!(QueueWorker::is_transient(&auth(403)));
        assert!(
            !QueueWorker::is_transient(&auth(400)),
            "a content 400 is a real verdict"
        );
    }

    fn caught(f: impl FnOnce() + std::panic::UnwindSafe) -> Box<dyn Any + Send> {
        std::panic::catch_unwind(f).expect_err("closure must panic")
    }

    #[test]
    fn panic_message_reads_static_str_payload() {
        let payload = caught(|| panic!("static str panic"));
        assert_eq!(panic_message(payload), "static str panic");
    }

    #[test]
    fn panic_message_reads_string_payload() {
        let detail = String::from("formatted");
        let payload = caught(move || panic!("a {detail} panic"));
        assert_eq!(panic_message(payload), "a formatted panic");
    }

    #[test]
    fn panic_message_falls_back_for_non_string_payload() {
        // A payload that is neither &str nor String must never be
        // unwrap'd — the reporting path may not itself panic.
        let payload = caught(|| std::panic::panic_any(42u8));
        assert_eq!(panic_message(payload), "non-string panic payload");
    }
}
