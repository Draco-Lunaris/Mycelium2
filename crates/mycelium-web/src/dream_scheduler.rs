//! The dream scheduler (memory-lifecycle spec §3.4, plan Task 3): a
//! long-lived worker that wakes on the stored dream interval and runs
//! ONE dream pass per wake, round-robin over the maintenance scopes.
//!
//! Per-wake flow (plan Rulings 5/6 + the controller rulings — the
//! order is the contract):
//!
//!   1. read the sealed `lifecycle` row — CLAMPED on read (Task 1's
//!      shared clamp; `get_sealed` deserializes unclamped, so the read
//!      side is the only defense a hand-edited row has). An absent row
//!      means defaults; an unreadable one defers the wake (warned,
//!      skipped).
//!   2. a PRESENT row's `dream_interval_s` re-tunes the wake interval
//!      live when it differs (a changed value applies on the next
//!      tick); an ABSENT row never re-tunes — a fresh install inherits
//!      the construction default forever.
//!   3. `dream_enabled == false` → skip: nothing admitted, nothing
//!      recorded, the cursor stays.
//!   4. resolve the [`LlmClient`] from the `llm` row BEFORE admission —
//!      an absent/undecryptable/unreadable row is a guaranteed-fail
//!      pass, so the wake defers without burning a try-class slot
//!      (warned, skipped).
//!   5. resolve the rotation: users (`SELECT id, role FROM users ORDER
//!      BY id`) then the `global:skills` shelf; a failed enumeration
//!      defers the wake.
//!   6. the shared per-scope pass ([`DreamScheduler::run_scope_pass`],
//!      Task 4's ONE source of truth for resolve+admit+dream): the
//!      backend-Red gate (the drain's Red-halt, 2026-10-31: a doomed
//!      run would hold a max_parallel slot up to the LLM timeout;
//!      Disabled is step 4's no-op, NOT weather, Yellow still runs) →
//!      scope resolution (the `api_dream` construction, built fresh
//!      per attempt — BEFORE admission, so an unrecoverable user
//!      master key never holds a queue slot) → [`LLM_GATE.try_enter`]
//!      admission (try class, NEVER waits — the stream's global
//!      constraint: a busy gate never does) →
//!      [`mycelium_librarian::dream::run_dream`] under the permit,
//!      dropped the moment the run ends.
//!   7. the CALLER's recording — the shared fn returns an outcome
//!      shape both callers account from: the tick counts the pass and
//!      writes the plain-JSON `dream_last_run` record for every
//!      Attempted outcome (ran, no-signal, failure — Ruling 7:
//!      summary bounded at the byte-safe cap) and advances the
//!      cursor. A PANICKING pass is caught (the queue worker's
//!      2026-10-31 panic net) and takes the same attempted-failure
//!      shape; an unrecoverable key is likewise an ATTEMPTED pass
//!      (recorded `ran: false`, cursor advances).
//!
//! The backfill worker (Task 4, Ruling 8) drives the SAME
//! [`DreamScheduler::run_scope_pass`] for every maintenance scope —
//! users `ORDER BY id` then the `global:skills` shelf — but records
//! nothing: no `dream_last_run` write, no cursor (the scheduler's
//! readout stays its own). On completion it leaves THE
//! [`DREAM_BACKFILL_AT_KEY`] marker (a plain RFC3339 row — the /health
//! `dream_backfill_complete` flag keys off its presence); the admin
//! click deletes the marker before spawning. Its shutdown-token select
//! arm kills a mid-walk silently — no marker — and the running-guard
//! is released on every exit (panic included) regardless.
//!
//! /metrics exposes three counters (health.rs writes the lines): every
//! wake is a tick; every wake that ran a pass — ran, no-signal, or
//! resolve-failure — is a pass; every wake that ended without one
//! (gate busy, backend Red, disabled, unreadable/unusable config) is a
//! skip.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use mycelium_auth::rbac::Role;
use mycelium_crypto::keys::ServiceKey;
use mycelium_librarian::agent::{AgentError, AgentScopes};
use mycelium_librarian::dream::DreamReport;
use mycelium_librarian::llm::{LlmClient, LlmConfig};
use mycelium_librarian::llm_gate::LLM_GATE;
use mycelium_librarian::llm_health::{BackendState, LLM_HEALTH, short_detail};
use mycelium_librarian::queue_worker::MasterKeyRecovery;
use mycelium_store::config::get_sealed;
use mycelium_store::revisions::{DEFAULT_DREAM_INTERVAL_S, LIFECYCLE_CONFIG_KEY, LifecycleConfig};
use mycelium_store::vectors::VectorStore;
use mycelium_store::{ConceptStore, ConfigStore, Store};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// The `dream_last_run` readout key (Ruling 7): a plain (UNsealed)
/// ConfigStore JSON key — the last attempted pass, surfaced as-is for
/// the admin readout (Task 5). Scheduler-writes-only; the backfill
/// passes record nothing, and no boot sweep prunes it.
pub const DREAM_LAST_RUN_KEY: &str = "dream_last_run";

/// The `dream_backfill_at` completion marker (Ruling 7): a plain
/// (UNsealed) ConfigStore RFC3339 string row — THE evidence a corpus
/// backfill completed (the /health `dream_backfill_complete` flag
/// keys off its presence). The admin click DELETES it (the walk is
/// re-clickable the moment it is launched) and the backfill worker
/// rewrites it on completion.
pub const DREAM_BACKFILL_AT_KEY: &str = "dream_backfill_at";

/// /metrics counters (health.rs writes the lines): every wake is a
/// tick; every wake that ran a pass — ran, no-signal, or resolve-
/// failure — is a pass; every wake that ended without one (gate busy,
/// disabled, unreadable/unusable config) is a skip.
pub static DREAM_TICKS_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static DREAM_PASSES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static DREAM_PASSES_SKIPPED_TOTAL: AtomicU64 = AtomicU64::new(0);

/// The dream scheduler (memory-lifecycle spec §3.4, plan Task 3). One
/// scope per wake in a round-robin over the maintenance scopes; the
/// wake cadence is the stored `dream_interval_s` (live-applied); the
/// admission is try class — a busy gate defers a wake, never waits.
pub struct DreamScheduler {
    pub store: Arc<Store>,
    pub config: Arc<ConfigStore>,
    /// The service key — decrypts the sealed lifecycle/llm rows and
    /// builds the service-scope stores.
    pub service_key: Arc<ServiceKey>,
    /// Master-key recovery for the user scopes (the shared
    /// MasterKeyCache seam the queue/vector workers use).
    pub recovery: Arc<MasterKeyRecovery>,
}

impl DreamScheduler {
    pub fn new(
        store: Arc<Store>,
        config: Arc<ConfigStore>,
        service_key: Arc<ServiceKey>,
        recovery: Arc<MasterKeyRecovery>,
    ) -> Self {
        Self {
            store,
            config,
            service_key,
            recovery,
        }
    }

    /// Spawn on the stored dream interval's cadence (the default: the
    /// row's value applies live on the first read, so a shorter stored
    /// value takes over immediately after this first one).
    pub fn spawn(self: &Arc<Self>, shutdown: CancellationToken) -> JoinHandle<()> {
        let tick = Duration::from_secs(u64::from(DEFAULT_DREAM_INTERVAL_S));
        self.spawn_with_tick(shutdown, tick)
    }

    /// Spawn on an explicit cadence (the tests' seam).
    pub fn spawn_with_tick(
        self: &Arc<Self>,
        shutdown: CancellationToken,
        tick: Duration,
    ) -> JoinHandle<()> {
        let me = Arc::clone(self);
        tokio::spawn(async move { me.run(shutdown, tick).await })
    }

    async fn run(self: Arc<Self>, shutdown: CancellationToken, tick: Duration) {
        // The rotation cursor lives for the worker's lifetime — one
        // continuous rotation across wakes (Ruling 6).
        let mut cursor = 0usize;
        // The interval's first tick completes immediately (boot posture:
        // the stored period's first fire is one FULL period later — and
        // a present row whose `dream_interval_s` differs re-tunes the
        // wake right on that first read). Delay over the default Burst:
        // a long stall defers the missed wake, never double-fires it.
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = interval.tick() => self.tick(&mut cursor, &mut interval).await,
            }
        }
    }

    /// One wake — the module doc's flow, step by step. `pub`: the
    /// integration tests drive this directly to pin the cursor/gate/
    /// disabled/re-tune semantics (a spawned loop's cadence is the
    /// lifecycle row's, ≥1 h — un-testable at sub-second periods). The
    /// `interval` rides along so a PRESENT lifecycle row can re-tune
    /// the running loop's wake period live.
    pub async fn tick(&self, cursor: &mut usize, interval: &mut tokio::time::Interval) {
        DREAM_TICKS_TOTAL.fetch_add(1, Ordering::Relaxed);

        // 1. The lifecycle row (Ruling 5: no statics — read per tick,
        //    and clamped on read; the sealed row itself is not clamped).
        let Some(read) = self.read_lifecycle().await else {
            DREAM_PASSES_SKIPPED_TOTAL.fetch_add(1, Ordering::Relaxed);
            return;
        };

        // 2. The live re-tune (Ruling 6): a stored interval that differs
        //    from the running one takes over from THIS wake on.
        if read.present {
            let want = Duration::from_secs(u64::from(read.cfg.dream_interval_s));
            if want != interval.period() {
                tracing::info!(
                    secs = read.cfg.dream_interval_s,
                    "dream scheduler: wake interval re-tuned to the stored value"
                );
                retune(interval, want);
            }
        }

        // 3. Disabled: advance nothing, admit nothing — the wake still
        //    counts as a skip (the skipped metric's gates+disabled).
        if !read.cfg.dream_enabled {
            DREAM_PASSES_SKIPPED_TOTAL.fetch_add(1, Ordering::Relaxed);
            return;
        }

        // 4. The LlmClient resolves BEFORE admission — a pass with no
        //    usable backend is guaranteed to fail, so the wake defers
        //    without burning a try-class slot. Warned (this is the one
        //    warn-per-wake spot).
        let Some(client) = self.resolve_llm_client().await else {
            DREAM_PASSES_SKIPPED_TOTAL.fetch_add(1, Ordering::Relaxed);
            return;
        };

        // 5. The rotation (users in `ORDER BY id` order, then the skills
        //    shelf) — a failed enumeration defers the whole wake.
        let Some(rotation) = self.rotation().await else {
            DREAM_PASSES_SKIPPED_TOTAL.fetch_add(1, Ordering::Relaxed);
            return;
        };

        // 6. One scope per wake, at the cursor — the SHARED per-scope
        //    pass (the ONE source of truth for resolve+admit+dream;
        //    the backfill worker drives the same fn). The tick owns
        //    the recording off the returned outcome: every Attempted
        //    outcome — ran, no-signal, pass failure, resolve-failure —
        //    is a pass recorded to `dream_last_run` (Ruling 7) and
        //    advanced (Ruling 6); BackendRed / GateBusy defer the wake
        //    with the cursor untouched.
        let target = &rotation[*cursor % rotation.len()];
        let scope_id = target.scope_id();
        match self.run_scope_pass(target, &client).await {
            ScopePassOutcome::Attempted(Ok(report)) => {
                // Ruling 9: the record serializes the report fields it
                // got — the agent's summary when it ran, the
                // deterministic skip reason otherwise (a `ran: false`
                // no-signal pass is still an honest record).
                let summary = report.summary.clone().or_else(|| report.reason.clone());
                self.record_last_run(&scope_id, report.ran, summary).await;
                DREAM_PASSES_TOTAL.fetch_add(1, Ordering::Relaxed);
                advance_cursor(cursor, rotation.len());
            }
            ScopePassOutcome::Attempted(Err(detail)) => {
                tracing::warn!(scope = %scope_id, error = %detail, "scheduled dream pass failed");
                self.record_last_run(&scope_id, false, Some(detail)).await;
                DREAM_PASSES_TOTAL.fetch_add(1, Ordering::Relaxed);
                advance_cursor(cursor, rotation.len());
            }
            ScopePassOutcome::Unresolvable => {
                // The resolve-failure ruling: an unrecoverable user
                // master key is still an ATTEMPTED pass (resolve_scope
                // already warned per scope).
                tracing::warn!(
                    scope = %scope_id,
                    "dream scheduler: scope unresolvable — counted as an attempted pass"
                );
                self.record_last_run(
                    &scope_id,
                    false,
                    Some("scope skipped: master key unrecoverable".to_string()),
                )
                .await;
                DREAM_PASSES_TOTAL.fetch_add(1, Ordering::Relaxed);
                advance_cursor(cursor, rotation.len());
            }
            ScopePassOutcome::BackendRed | ScopePassOutcome::GateBusy => {
                // Red warns inside run_scope_pass (the drain's
                // Red-halt — a doomed run would hold a max_parallel
                // slot for up to the LLM timeout); the gate-busy skip
                // keeps the tick's try-class silence. Either way: a
                // skip, the cursor untouched.
                DREAM_PASSES_SKIPPED_TOTAL.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// The sealed `lifecycle` row, always clamped. `Some` = the tick
    /// proceeds; `None` = the row cannot be trusted (unreadable DB /
    /// present-but-undecryptable) — deferred wake.
    async fn read_lifecycle(&self) -> Option<LifecycleRead> {
        match self.config.raw_get(LIFECYCLE_CONFIG_KEY).await {
            // Absent: the fresh-install posture — defaults, and (per
            // Ruling 6) no interval re-tune.
            Ok(None) => Some(LifecycleRead {
                cfg: LifecycleConfig::default().clamp(),
                present: false,
            }),
            Ok(Some(_)) => {
                match get_sealed::<LifecycleConfig>(
                    &self.config,
                    &self.service_key,
                    LIFECYCLE_CONFIG_KEY,
                )
                .await
                {
                    Some(cfg) => Some(LifecycleRead {
                        cfg: cfg.clamp(),
                        present: true,
                    }),
                    None => {
                        tracing::warn!(
                            "dream scheduler: the lifecycle row is present but undecryptable — the wake defers"
                        );
                        None
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "dream scheduler: lifecycle row read failed — the wake defers"
                );
                None
            }
        }
    }

    /// The `llm` row → client (clamped, gate-synced — the api_dream
    /// construction). `None` defers the wake.
    async fn resolve_llm_client(&self) -> Option<LlmClient> {
        match self.config.raw_get("llm").await {
            Ok(None) => {
                tracing::warn!(
                    "dream scheduler: no llm row — dreams defer until the admin configures one"
                );
                None
            }
            Ok(Some(_)) => {
                let cfg = get_sealed::<LlmConfig>(&self.config, &self.service_key, "llm")
                    .await
                    .map(|c| c.clamp());
                let Some(cfg) = cfg else {
                    tracing::warn!(
                        "dream scheduler: the llm row is present but undecryptable — dreams defer"
                    );
                    return None;
                };
                cfg.sync_gate();
                Some(LlmClient::new(&cfg))
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "dream scheduler: llm row read failed — dreams defer"
                );
                None
            }
        }
    }

    /// The rotation: users in `SELECT id, role FROM users ORDER BY id`
    /// order, then the `global:skills` shelf (Ruling 6, never empty).
    /// `None` = the enumeration failed — the wake defers.
    async fn rotation(&self) -> Option<Vec<RotationTarget>> {
        let rows: Vec<(String, String)> =
            match sqlx::query_as("SELECT id, role FROM users ORDER BY id")
                .fetch_all(self.store.pool())
                .await
            {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "dream scheduler: user enumeration failed — the wake defers"
                    );
                    return None;
                }
            };
        let mut out = Vec::with_capacity(rows.len() + 1);
        for (id, role) in rows {
            let Ok(uid) = Uuid::parse_str(&id) else {
                tracing::warn!(user = %id, "dream rotation: user row id is not a uuid — row skipped");
                continue;
            };
            let admin = match Role::parse(&role) {
                Some(Role::Admin) => true,
                Some(Role::User) => false,
                None => {
                    tracing::warn!(
                        user = %id,
                        role = %role,
                        "dream rotation: unknown role string — treated as non-admin"
                    );
                    false
                }
            };
            out.push(RotationTarget::User { id: uid, admin });
        }
        out.push(RotationTarget::Skills);
        Some(out)
    }

    /// The `api_dream` scope construction for one rotation entry — the
    /// same stores, agent scopes, and vectors the REST endpoint builds,
    /// per attempt. `pub(crate)` + fallible ONLY on the user branch's
    /// master-key recovery (the skills branch is always resolvable);
    /// called by [`DreamScheduler::run_scope_pass`], which both
    /// callers (the tick and the Task-4 backfill worker) drive.
    pub(crate) async fn resolve_scope<'a>(
        &'a self,
        target: &RotationTarget,
    ) -> Option<DreamScope<'a>> {
        match target {
            RotationTarget::User { id, admin } => {
                let Ok(master) = (self.recovery)(*id).await else {
                    tracing::warn!(
                        scope = %target.scope_id(),
                        "dream scheduler: user master key unrecoverable — scope skipped"
                    );
                    return None;
                };
                let scope_id = target.scope_id();
                let vectors = VectorStore::for_user(self.store.pool(), *id, &master);
                let cs = ConceptStore::for_user(self.store.as_ref(), *id, master.clone())
                    .with_provenance("dream");
                let cons_cs = ConceptStore::for_user(self.store.as_ref(), *id, master)
                    .with_provenance("dream")
                    .with_revision_op("consolidate");
                let visible = crate::api::visible_library_slugs(self.store.as_ref(), *admin).await;
                let scopes = crate::api::build_agent_scopes(
                    self.store.as_ref(),
                    &self.service_key,
                    visible,
                    &scope_id,
                );
                Some(DreamScope {
                    cs,
                    cons_cs,
                    scopes,
                    vectors,
                })
            }
            RotationTarget::Skills => {
                let scope_id = target.scope_id();
                let cs = ConceptStore::for_service(
                    self.store.as_ref(),
                    (*self.service_key).clone(),
                    &self.store.skills_dir(),
                    "skills",
                )
                .with_provenance("dream");
                let cons_cs = ConceptStore::for_service(
                    self.store.as_ref(),
                    (*self.service_key).clone(),
                    &self.store.skills_dir(),
                    "skills",
                )
                .with_provenance("dream")
                .with_revision_op("consolidate");
                let vectors =
                    VectorStore::for_service(self.store.pool(), &self.service_key, "skills");
                // The service scope reads only service-side concepts —
                // no per-user library visibility applies.
                let scopes = crate::api::build_agent_scopes(
                    self.store.as_ref(),
                    &self.service_key,
                    None,
                    &scope_id,
                );
                Some(DreamScope {
                    cs,
                    cons_cs,
                    scopes,
                    vectors,
                })
            }
        }
    }

    /// The per-scope dream pass — the ONE source of truth for
    /// resolve+admit+dream (Task 4's refactor ruling). Both callers —
    /// the tick and the backfill worker — drive this fn; the CALLER
    /// owns the recording off the returned [`ScopePassOutcome`] (the
    /// tick records `dream_last_run` + counts the pass + advances the
    /// rotation, Ruling 7; the backfill records nothing, Ruling 8).
    ///
    /// Order: the backend-Red gate (the drain's Red-halt, 2026-10-31 —
    /// a doomed run would hold a max_parallel slot for up to the LLM
    /// timeout; Disabled is deliberately NOT gated — no llm row is the
    /// no-backend case, not weather — and Yellow still runs) → scope
    /// resolution BEFORE admission (an unrecoverable user master key
    /// never holds a queue slot) → [`LLM_GATE.try_enter`] (try class,
    /// never waits) → the panic-netted [`mycelium_librarian::dream::
    /// run_dream`] under the permit, dropped the MOMENT the run ends
    /// (nothing between the run and the release). Every failure shape —
    /// a real `Err` and a caught panic alike — is the same ATTEMPTED
    /// outcome for the caller's accounting.
    pub(crate) async fn run_scope_pass(
        &self,
        target: &RotationTarget,
        client: &LlmClient,
    ) -> ScopePassOutcome {
        let scope_id = target.scope_id();
        if LLM_HEALTH.state() == BackendState::Red {
            tracing::warn!(scope = %scope_id, "dream pass: backend Red — the pass defers");
            return ScopePassOutcome::BackendRed;
        }
        // Resolve the scope's stores/scopes/vectors (the `api_dream`
        // construction, fresh per attempt) BEFORE admission — an
        // unloadable key never burns a try-class slot.
        let Some(scope) = self.resolve_scope(target).await else {
            return ScopePassOutcome::Unresolvable;
        };
        // Try class — a busy gate never waits; the caller decides the
        // skip accounting (the tick defers its wake, the backfill
        // skips the scope but still finishes its walk).
        let permit = match LLM_GATE.try_enter() {
            Some(permit) => permit,
            None => return ScopePassOutcome::GateBusy,
        };
        let report = catch_pass_panic(mycelium_librarian::dream::run_dream(
            client,
            &scope.cs,
            &scope.cons_cs,
            &scope.scopes,
            Some(&scope.vectors),
        ))
        .await;
        drop(permit);
        ScopePassOutcome::Attempted(report)
    }

    /// The last-run record write (plain JSON, summary bounded at the
    /// byte-safe cap). A failed write warns but does not fail the tick —
    /// the pass happened; the readout is observability, not state
    /// the flow depends on.
    async fn record_last_run(&self, scope: &str, ran: bool, summary: Option<String>) {
        let record = DreamLastRunRecord {
            at: chrono::Utc::now().to_rfc3339(),
            scope: scope.to_string(),
            ran,
            summary: summary.map(short_detail),
        };
        if let Err(e) = self.config.set(DREAM_LAST_RUN_KEY, &record).await {
            tracing::warn!(
                error = %e,
                "dream scheduler: dream_last_run write failed (the pass itself still happened)"
            );
        }
    }

    /// Launch the admin-triggered corpus backfill (Ruling 8): ONE
    /// detached worker over [`run_backfill`]. The handler claims the
    /// running-guard BEFORE calling this (the CAS is the only
    /// single-run synchronization) and passes the app's shutdown token
    /// — the walk selects on it like every boot-spawned worker and
    /// dies silently when the process shuts down.
    pub(crate) fn spawn_backfill(
        sched: Arc<DreamScheduler>,
        running: Arc<AtomicBool>,
        shutdown: CancellationToken,
    ) -> JoinHandle<()> {
        tokio::spawn(async move { run_backfill(sched, running, shutdown).await })
    }
}

/// RAII release of the backfill running-guard: every exit path —
/// completion, unusable config, enumeration failure, shutdown, panic
/// unwind — hands the admin button back (deliberate: a dead worker
/// must never wedge it; the next click restarts the walk). The marker
/// is NOT part of the release — only a completed walk writes one.
struct BackfillGuard(Arc<AtomicBool>);

impl Drop for BackfillGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// The admin-triggered corpus backfill walk (Ruling 8): every
/// maintenance scope ONCE — the users `ORDER BY id` first, the
/// `global:skills` shelf strictly last (the one rotation builder) —
/// through the SAME per-scope pass the tick drives. `dream_enabled`
/// gates nothing here; `dream_last_run` is never written (the
/// scheduler's readout stays its own). A gate-busy scope is skipped
/// (warned; its signals persist for the scheduler's rotation or the
/// next walk) and the walk still completes; a Red backend defers
/// scope-by-scope through the same shared gate.
async fn run_backfill(
    sched: Arc<DreamScheduler>,
    running: Arc<AtomicBool>,
    shutdown: CancellationToken,
) {
    // Deliberate: the guard release rides the RAII guard, so a panic
    // in this walk's own body still hands the button back (the pass
    // net is inside run_scope_pass; this is the outer net).
    let _guard = BackfillGuard(running);

    // The client resolves ONCE, before the walk (the tick's step-4
    // posture): an absent/undecryptable/unreadable `llm` row is a
    // guaranteed-fail walk — warn and leave NO marker (nothing ran).
    let Some(client) = sched.resolve_llm_client().await else {
        tracing::warn!("dream backfill: no usable llm row — the walk is abandoned, no marker left");
        return;
    };

    // The walk's targets: every user scope, then the skills shelf (the
    // ONE rotation builder — never a third literal). An enumeration
    // failure leaves no marker either: nothing walked.
    let Some(rotation) = sched.rotation().await else {
        tracing::warn!(
            "dream backfill: user enumeration failed — the walk is abandoned, no marker left"
        );
        return;
    };
    let total = rotation.len();

    // One pass per scope; the shutdown select arm kills a mid-walk
    // silently — no marker (Ruling 8); the guard releases via the
    // RAII drop regardless.
    let mut attempted = 0usize;
    let mut skipped = 0usize;
    for target in &rotation {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            outcome = sched.run_scope_pass(target, &client) => match outcome {
                ScopePassOutcome::Attempted(Ok(_)) => attempted += 1,
                ScopePassOutcome::Attempted(Err(detail)) => {
                    attempted += 1;
                    tracing::warn!(
                        scope = %target.scope_id(),
                        error = %detail,
                        "dream backfill pass failed"
                    );
                }
                // The unloadable-key skip (Review Focus 4): warned
                // inside resolve_scope; the walk reaches the remaining
                // scopes and finishes.
                ScopePassOutcome::Unresolvable => skipped += 1,
                ScopePassOutcome::BackendRed => skipped += 1,
                ScopePassOutcome::GateBusy => {
                    skipped += 1;
                    tracing::warn!(
                        scope = %target.scope_id(),
                        "dream backfill: gate busy — scope skipped, its signals persist"
                    );
                }
            },
        }
    }

    // Completion: THE marker (Ruling 7 — a plain RFC3339 row; /health
    // keys `dream_backfill_complete` off its presence). A failed write
    // warns but never wedges the worker (the walk happened; the
    // marker is observability).
    if let Err(e) = sched
        .config
        .set(DREAM_BACKFILL_AT_KEY, &chrono::Utc::now().to_rfc3339())
        .await
    {
        tracing::warn!(error = %e, "dream backfill: marker write failed");
    }
    tracing::info!(
        attempted,
        skipped,
        total,
        "dream backfill complete — the corpus marker is set"
    );
}

/// What one per-scope pass produced — the shape BOTH callers (the
/// tick, the Task-4 backfill worker) account from off the shared
/// [`DreamScheduler::run_scope_pass`]. Attempted outcomes are the
/// caller's recording decision; the last two are honest skips.
/// `pub(crate)`: the fn returning it is.
pub(crate) enum ScopePassOutcome {
    /// An admitted pass ran to completion: a real report (ran or the
    /// `ran: false` no-signal posture) on Ok; a pass failure on Err —
    /// a real error or a caught panic's detail alike.
    Attempted(Result<DreamReport, String>),
    /// The backend is Red — the drain's Red-halt (a doomed run would
    /// hold a max_parallel slot for up to the LLM timeout). Warned in
    /// the shared fn; Disabled is NOT weather and Yellow still runs.
    BackendRed,
    /// The gate was busy — try class never waits.
    GateBusy,
    /// The user scope's master key could not be recovered (warned in
    /// `resolve_scope`). For the tick this is still an ATTEMPTED pass
    /// (the resolve-failure ruling); for the backfill it is the
    /// unloadable-key skip (Review Focus 4).
    Unresolvable,
}

/// What a wake knows about the lifecycle row: `cfg` is ALWAYS clamped
/// (`get_sealed` deserializes unclamped — the read side is Task 1's
/// shared clamp, the only defense a hand-edited row has); `present`
/// distinguishes an absent row (fresh install: defaults, no interval
/// re-tune) from a real one (a stored interval re-tunes the wake).
struct LifecycleRead {
    cfg: LifecycleConfig,
    present: bool,
}

/// One dream-eligible scope in the rotation, in `ORDER BY id` user
/// order then the service skills shelf (Ruling 6). `admin` rides the
/// entry because the library visibility walk needs the ROLE, and the
/// scheduler sits below the session layer (no SessionUser to consult).
/// `pub(crate)`: the backfill worker (Task 4) resolves its targets
/// through the same fns.
#[derive(Debug, Clone)]
pub(crate) enum RotationTarget {
    User { id: Uuid, admin: bool },
    Skills,
}

impl RotationTarget {
    /// The scope id the readouts carry (the vector worker's scope-id
    /// convention).
    fn scope_id(&self) -> String {
        match self {
            RotationTarget::User { id, .. } => format!("user:{id}"),
            RotationTarget::Skills => "global:skills".to_string(),
        }
    }
}

/// One resolved dream target — everything [`run_dream`] needs for the
/// scope. The borrows are the tick's own `&self` borrow of the
/// scheduler's store; the scope id for records/warns stays with the
/// caller (a cheap `RotationTarget::scope_id` recompute), the struct
/// itself never carried it.
pub(crate) struct DreamScope<'a> {
    pub cs: ConceptStore<'a>,
    pub cons_cs: ConceptStore<'a>,
    pub scopes: AgentScopes<'a>,
    pub vectors: VectorStore<'a>,
}

/// The `dream_last_run` readout record (Ruling 7): `{at, scope, ran,
/// summary}` as plain (unsealed) ConfigStore JSON. Task 5's admin
/// readout renders it as-is — public with `Deserialize` so the GET
/// page reads the SAME shape the scheduler writes (a missing summary
/// deserializes as `None`; `skip_serializing_if` is serialize-side
/// only).
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct DreamLastRunRecord {
    /// The write time (RFC 3339).
    pub at: String,
    /// The scope the attempt ran for (`user:<uuid>` / `global:skills`).
    pub scope: String,
    /// Whether the agent actually ran (`run_dream`'s report) — false
    /// also marks skill-less no-signal passes, resolve-failures, and
    /// pass failures.
    pub ran: bool,
    /// The agent summary / the skip reason / the bounded failure
    /// detail, when anything readable came out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Re-arm the wake interval at `period` (the live re-tune): a rebuilt
/// interval would complete its first tick immediately and default to
/// Burst missed-tick behavior — Delay is restored, and `reset()` puts
/// the next wake one full NEW period away (tokio's reset semantics),
/// so a re-tuned loop never self-inflicts a stampede.
fn retune(interval: &mut tokio::time::Interval, period: Duration) {
    *interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.reset();
}

/// Run one dream pass under the queue-worker's panic net (the
/// 2026-10-31 precedent: a panicking `run_item` wedged the drain — an
/// escaping panic must never take the long-lived worker task down): a
/// panicking pass future is caught and flattened into the SAME
/// `Result` shape the attempted-failure path consumes, where a panic
/// is exactly as attempted as an `Err`. Never swallows real errors —
/// an `Err` from the pass keeps flowing with its own string.
/// `AssertUnwindSafe` is justified like the drain's: nothing shared is
/// poisoned by an unwind here (no locks held across the await; sqlx
/// state is unaffected).
async fn catch_pass_panic(
    pass: impl std::future::Future<Output = Result<DreamReport, AgentError>>,
) -> Result<DreamReport, String> {
    match futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(pass)).await {
        Ok(Ok(report)) => Ok(report),
        Ok(Err(e)) => Err(e.to_string()),
        Err(payload) => Err(panic_message(payload)),
    }
}

/// A bounded panic detail from the unwind payload (the queue worker's
/// `panic_message` twin): `&str`/`String` payloads read through,
/// anything else gets the fixed fallback — the reporting path may not
/// itself panic.
fn panic_message(payload: Box<dyn Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".into())
}

/// Advance the rotation cursor one entry, wrapping. `wrapping_add`
/// keeps a centuries-long process honest; `len.max(1)` keeps the
/// modulo on a never-empty rotation (the skills shelf is always
/// pushed) even if a future refactor drops that guarantee.
fn advance_cursor(cursor: &mut usize, len: usize) {
    *cursor = cursor.wrapping_add(1) % len.max(1);
}

#[cfg(test)]
mod tests {
    use super::catch_pass_panic;

    /// The catch on the pass boundary (the queue worker's 2026-10-31
    /// panic net, without its e2e fixture: the malformed-legacy-payload
    /// panic the drain wedged on no longer exists post-BadToolArgs
    /// guard): a panicking pass future converts to the SAME Err shape
    /// the attempted-failure path consumes — the panic detail becomes
    /// the failed attempt's summary and a real error keeps its own
    /// string; neither kills the caller.
    #[tokio::test]
    async fn panicking_pass_is_caught_as_failed_attempt() {
        // A panic mid-execution of the future (the agent bug shape):
        // the detail flows through as the failed-attempt summary.
        let outcome = catch_pass_panic(async {
            let why = "boom: malformed tool args";
            panic!("a {why} panic")
        })
        .await;
        assert_eq!(
            outcome.expect_err("a panic must convert to a failed attempt"),
            "a boom: malformed tool args panic"
        );

        // A non-string payload never breaks the reporting path — the
        // fixed fallback detail stands in. (panic_any diverges, so its
        // tail-as-only-expression coerces to the pass type.)
        let outcome = catch_pass_panic(async { std::panic::panic_any(42u8) }).await;
        assert_eq!(
            outcome.expect_err("the fallback must hold for foreign payloads"),
            "non-string panic payload"
        );

        // Real errors are NOT swallowed: an Err from the pass keeps
        // flowing (with its own string) — the catch is panics-only.
        let outcome =
            catch_pass_panic(async { Err(mycelium_librarian::agent::AgentError::StepCapExceeded) })
                .await;
        assert_eq!(
            outcome.expect_err("a real error passes through the catch"),
            "agent step cap exceeded"
        );

        // And a calm report passes through untouched.
        let outcome = catch_pass_panic(async {
            Ok(mycelium_librarian::dream::DreamReport {
                ran: true,
                reason: None,
                summary: Some("done".into()),
                files_changed: None,
            })
        })
        .await;
        assert!(
            outcome.expect("a pass result passes through the catch").ran,
            "a pass result passes through the catch untouched"
        );
    }
}
