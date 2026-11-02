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
//!   6. admit through [`LLM_GATE.try_enter`] — try class, NEVER waits
//!      (the stream's global constraint): a busy gate defers the wake
//!      with the cursor untouched. A RED backend (the drain's Red-halt,
//!      2026-10-31: a doomed run would hold a max_parallel slot up to
//!      the LLM timeout) defers too; Disabled is step 4's no-op, NOT
//!      weather, and Yellow still runs.
//!   7. resolve the scope (stores, agent scopes, vectors — the
//!      `api_dream` construction, built fresh per attempt). An
//!      unrecoverable user master key is still an ATTEMPTED pass:
//!      warned, recorded `ran: false`, cursor advances.
//!   8. [`mycelium_librarian::dream::run_dream`] under the permit —
//!      dropped the moment the run ends; every completed attempt (ran,
//!      no-signal, or failure) writes the plain-JSON `dream_last_run`
//!      record (Ruling 7: summary bounded at the byte-safe cap) and
//!      advances the cursor. A PANICKING pass is caught (the queue
//!      worker's 2026-10-31 panic net) and takes the same attempted-
//!      failure shape — recorded `ran: false`, cursor advances, loop
//!      continues on the next tick.
//!
//! The backfill worker (Task 4) shares the per-scope pass code through
//! [`DreamScheduler::resolve_scope`] / [`DreamScheduler::finish_pass`]
//! but resolves keys BEFORE admission and records no last-run readout —
//! `dream_last_run` is the scheduler's own.
//!
//! /metrics exposes three counters (health.rs writes the lines): every
//! wake is a tick; every wake that ran a pass — ran, no-signal, or
//! resolve-failure — is a pass; every wake that ended without one
//! (gate busy, backend Red, disabled, unreadable/unusable config) is a
//! skip.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mycelium_auth::rbac::Role;
use mycelium_crypto::keys::ServiceKey;
use mycelium_librarian::agent::{AgentError, AgentScopes};
use mycelium_librarian::dream::DreamReport;
use mycelium_librarian::llm::{LlmClient, LlmConfig};
use mycelium_librarian::llm_gate::{GatePermit, LLM_GATE};
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

        // 6. One scope per wake, at the cursor; try class — a busy gate
        //    defers the wake with the cursor untouched (never waits).
        //    Backend weather first (the drain's Red-halt, 2026-10-31): a
        //    Red backend is a doomed run that would hold a max_parallel
        //    slot for up to the LLM timeout, recorded as a failure
        //    anyway — the wake defers instead. Disabled is deliberately
        //    NOT gated here (no llm row is step 4's no-op, not weather)
        //    and Yellow still runs (1-2 failures do not make every pass
        //    doomed).
        let target = &rotation[*cursor % rotation.len()];
        let scope_id = target.scope_id();
        if LLM_HEALTH.state() == BackendState::Red {
            tracing::warn!(
                scope = %scope_id,
                "dream scheduler: backend Red — the wake defers"
            );
            DREAM_PASSES_SKIPPED_TOTAL.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Some(permit) = LLM_GATE.try_enter() else {
            DREAM_PASSES_SKIPPED_TOTAL.fetch_add(1, Ordering::Relaxed);
            return;
        };

        // 7. Resolve the scope's stores/scopes/vectors. An unresolvable
        //    user key is still an ATTEMPTED pass (the resolve-failure
        //    ruling): warn, record `ran: false`, advance.
        let Some(scope) = self.resolve_scope(target).await else {
            drop(permit);
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
            return;
        };

        // 8. The pass — the permit rides along and is dropped the
        //    moment the run ends. Every completed attempt (Ok including
        //    the `ran: false` no-signal report, Err) is recorded and
        //    advances the cursor (Ruling 6).
        self.finish_pass(&client, scope, permit, true).await;
        DREAM_PASSES_TOTAL.fetch_add(1, Ordering::Relaxed);
        advance_cursor(cursor, rotation.len());
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
    /// master-key recovery (the skills branch is always resolvable):
    /// the backfill worker (Task 4) resolves its scopes through this fn
    /// too.
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
                    scope_id,
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
                    scope_id,
                    cs,
                    cons_cs,
                    scopes,
                    vectors,
                })
            }
        }
    }

    /// One already-admitted dream pass — the tick's per-scope body, and
    /// the backfill worker's (Task 4). `run_dream` runs under the
    /// caller's permit and under [`catch_pass_panic`]'s net; the permit
    /// is dropped the MOMENT the run ends (nothing between the run and
    /// the release). `write_readout`: the scheduler tick writes
    /// `dream_last_run` (Ruling 7); the backfill passes record nothing
    /// (Ruling 8). Every failure shape — a real `Err` and a caught
    /// panic alike — is the same ATTEMPTED outcome: recorded `ran:
    /// false` with the bounded detail; the loop continues.
    pub(crate) async fn finish_pass(
        &self,
        client: &LlmClient,
        scope: DreamScope<'_>,
        permit: GatePermit<'_>,
        write_readout: bool,
    ) {
        let report = catch_pass_panic(mycelium_librarian::dream::run_dream(
            client,
            &scope.cs,
            &scope.cons_cs,
            &scope.scopes,
            Some(&scope.vectors),
        ))
        .await;
        drop(permit);
        if !write_readout {
            return;
        }
        match report {
            Ok(report) => {
                // Ruling 9: the record serializes the report fields it
                // got — the agent's summary when it ran, the
                // deterministic skip reason otherwise (a `ran: false`
                // no-signal pass is still an honest record).
                let summary = report.summary.clone().or_else(|| report.reason.clone());
                self.record_last_run(&scope.scope_id, report.ran, summary)
                    .await;
            }
            Err(detail) => {
                tracing::warn!(scope = %scope.scope_id, error = %detail, "scheduled dream pass failed");
                self.record_last_run(&scope.scope_id, false, Some(detail))
                    .await;
            }
        }
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
/// scope, plus the scope id the record carries. The borrows are the
/// tick's own `&self` borrow of the scheduler's store.
pub(crate) struct DreamScope<'a> {
    pub scope_id: String,
    pub cs: ConceptStore<'a>,
    pub cons_cs: ConceptStore<'a>,
    pub scopes: AgentScopes<'a>,
    pub vectors: VectorStore<'a>,
}

/// The `dream_last_run` readout record (Ruling 7): `{at, scope, ran,
/// summary}` as plain (unsealed) ConfigStore JSON. Task 5's admin
/// readout renders it as-is.
#[derive(Debug, serde::Serialize)]
struct DreamLastRunRecord {
    /// The write time (RFC 3339).
    at: String,
    /// The scope the attempt ran for (`user:<uuid>` / `global:skills`).
    scope: String,
    /// Whether the agent actually ran (`run_dream`'s report) — false
    /// also marks skill-less no-signal passes, resolve-failures, and
    /// pass failures.
    ran: bool,
    /// The agent summary / the skip reason / the bounded failure
    /// detail, when anything readable came out.
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
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
