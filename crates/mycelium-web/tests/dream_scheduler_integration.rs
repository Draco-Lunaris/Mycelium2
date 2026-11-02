//! DreamScheduler integration tests (memory-lifecycle spec §3.4, plan
//! Task 3): the scheduler's per-tick flow — lifecycle row read with
//! clamp-on-read, the live interval re-tune, the disabled short
//! circuit, the try-class gate admission, the users-then-skills
//! rotation with one pass per wake, and the `dream_last_run` readout —
//! pinned on direct `tick()` drives (a spawned loop's cadence is the
//! lifecycle row's ≥1 h, un-testable at sub-second periods; the
//! spawned smoke test covers the loop wiring itself).
//!
//! Direct-struct shape (vector_worker_integration.rs): a real temp-dir
//! store, a stub master-key recovery, a request-capturing mock LLM,
//! sealed config rows written the way the admin surfaces write them.
//!
//! Process-global state serializes on TEST_LOCK — the LLM gate and the
//! LLM health machine. Every spawned worker is cancelled and JOINED
//! before the test's lock guard releases.

use std::ops::AsyncFnMut;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::response::IntoResponse;
use mycelium_core::concept::{Concept, Frontmatter};
use mycelium_crypto::keys::{MasterKey, ServiceKey, generate_master_key};
use mycelium_librarian::llm_gate::{DEFAULT_CAPACITY, LLM_GATE};
use mycelium_librarian::llm_health::{BackendState, LLM_HEALTH, RED_FAILURE_THRESHOLD};
use mycelium_librarian::queue_worker::MasterKeyRecovery;
use mycelium_store::revisions::{DREAM_INTERVAL_S_RANGE, LIFECYCLE_CONFIG_KEY, LifecycleConfig};
use mycelium_store::{ConceptStore, ConfigStore, Store};
use mycelium_web::AppState;
use mycelium_web::LlmConfig;
use mycelium_web::dream_scheduler::{
    DREAM_BACKFILL_AT_KEY, DREAM_LAST_RUN_KEY, DREAM_PASSES_SKIPPED_TOTAL, DREAM_PASSES_TOTAL,
    DREAM_TICKS_TOTAL, DreamScheduler,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Serializes all tests in this binary around the LLM gate and the LLM
/// health machine.
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Restores the process-global LLM health machine on drop (the mocked
/// passes stamp successes into it), so a failing assertion's state
/// cannot perturb a sibling test. Declared AFTER the TEST_LOCK guard
/// so the restore happens while the lock is held.
struct HealthDefaults;

impl Drop for HealthDefaults {
    fn drop(&mut self) {
        LLM_HEALTH.reset_for_test();
    }
}

/// The lifecycle rows the tests store: the smallest in-range dream
/// interval (the clamp pin uses hand-edited values below/above it).
const TEST_INTERVAL: u32 = 3_600;

/// The gate wait bound for the wait-class fixture (chat_resume's
/// shape): long enough to never expire inside the test.
const GATE_BOUND: Duration = Duration::from_secs(10);

// ---------- request-capturing mock LLM (dream_integration.rs's shape) ----------

/// A request-capturing scripted mock LLM: records every request body,
/// answers from the LIFO script; an exhausted script answers 500 so an
/// unexpected extra call cannot masquerade as scripted success.
async fn capturing_mock_llm(
    responses: Vec<serde_json::Value>,
) -> (String, Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>) {
    capturing_mock_llm_with_delay(responses, Duration::ZERO).await
}

/// The same script server, with a per-request stall: the backfill
/// running-guard pins need a walk that cannot complete in an instant
/// (a request that arrives mid-run must land while the walk is live).
async fn capturing_mock_llm_with_delay(
    responses: Vec<serde_json::Value>,
    delay: Duration,
) -> (String, Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>) {
    let requests: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let req_clone = Arc::clone(&requests);
    let state = Arc::new(tokio::sync::Mutex::new(responses));
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |body: String| {
            let state = state.clone();
            let requests = req_clone.clone();
            async move {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body) {
                    requests.lock().await.push(parsed);
                }
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                match state.lock().await.pop() {
                    Some(resp) => axum::Json(resp).into_response(),
                    None => (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        "script exhausted",
                    )
                        .into_response(),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/v1"), requests)
}

fn text_response(text: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{ "message": { "role": "assistant", "content": text } }]
    })
}

// ---------- poll helper ----------

/// Poll until `cond` holds, bounded — never a fixed sleep as the
/// assertion.
async fn wait_until(what: &str, mut cond: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(cond().await) {
        assert!(Instant::now() < deadline, "never reached: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------- scaffolding ----------

struct Env {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    config: Arc<ConfigStore>,
    service_key: Arc<ServiceKey>,
    master: MasterKey,
    scheduler: Arc<DreamScheduler>,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).await.unwrap());
    let config = Arc::new(ConfigStore::new(store.pool().clone()));
    let service_key =
        Arc::new(mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap());
    let master = generate_master_key();
    let scheduler = Arc::new(DreamScheduler::new(
        Arc::clone(&store),
        Arc::clone(&config),
        Arc::clone(&service_key),
        stub_recovery(master.clone()),
    ));
    Env {
        _dir: dir,
        store,
        config,
        service_key,
        master,
        scheduler,
    }
}

/// Stub master-key recovery: returns the test's known master key for
/// any uid (the real MasterKeyCache::for_user lives at the binary seam).
fn stub_recovery(master: MasterKey) -> Arc<MasterKeyRecovery> {
    let m2 = master.clone();
    Arc::new(move |_uid| {
        let m = m2.clone();
        Box::pin(async move { Ok(m) })
    })
}

/// A user row to satisfy `SELECT id … FROM users` on the rotation
/// enumeration (below-auth-layer shape; the role string is a fixture
/// parameter — both roles are reachable in the users table).
async fn seed_user(store: &Store, id: Uuid, name: &str, role: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO users (id, username, email, role, auth_provider, password_hash, sealed_master_key, must_change_password, totp_secret, created_at, updated_at)
         VALUES (?, ?, ?, ?, 'local', NULL, 'placeholder', 0, NULL, ?, ?)",
    )
    .bind(id.to_string())
    .bind(name)
    .bind(format!("{name}@t"))
    .bind(role)
    .bind(&now)
    .bind(&now)
    .execute(store.pool())
    .await
    .unwrap();
}

/// Distinct deterministic user ids: users.id is TEXT, so `ORDER BY id`
/// is lexicographic on the hyphenated uuid — fixed ids fix the rotation
/// order the rotation pin asserts.
fn fixed_user(n: u16) -> Uuid {
    format!("00000000-0000-0000-0000-{n:012x}").parse().unwrap()
}

fn concept(path: &str, title: &str, body: &str) -> Concept {
    Concept::new(
        Frontmatter {
            concept_type: "Note".into(),
            title: Some(title.into()),
            ..Default::default()
        },
        body.into(),
        path.into(),
    )
}

/// A lone unlinked concept — the dream's orphan signal — into a store
/// the caller already built (a user scope under a chosen master key,
/// the service skills shelf, a web-boot user).
async fn put_orphan(cs: &ConceptStore<'_>, path: &str, title: &str, body: &str) {
    cs.put(&concept(path, title, body)).await.unwrap();
}

/// A lone unlinked concept in a user scope (the direct-drive default).
async fn orphan_in_user(env: &Env, uid: Uuid) {
    let cs = ConceptStore::for_user(env.store.as_ref(), uid, env.master.clone());
    put_orphan(
        &cs,
        "/orphan/lonely.md",
        "Lonely Dream Note",
        "nobody links here",
    )
    .await;
}

/// … in the service skills scope (the direct-drive default).
async fn orphan_in_skills(env: &Env) {
    let cs = ConceptStore::for_service(
        env.store.as_ref(),
        (*env.service_key).clone(),
        &env.store.skills_dir(),
        "skills",
    );
    put_orphan(
        &cs,
        "/skills/lonely.md",
        "Lonely Skill Note",
        "nobody links here",
    )
    .await;
}

/// Sealed-`lifecycle`-row writer (the step-4 admin surface's storage
/// shape). Tests hand-craft rows — including out-of-range values a
/// save-side clamp would never produce — to pin the READ-side clamp.
async fn write_lifecycle(
    config: &ConfigStore,
    service_key: &ServiceKey,
    interval_s: u32,
    enabled: bool,
) {
    let sealed = mycelium_store::config::seal_config(
        service_key,
        LIFECYCLE_CONFIG_KEY,
        &LifecycleConfig {
            dream_interval_s: interval_s,
            dream_enabled: enabled,
            ..Default::default()
        },
    );
    config.set(LIFECYCLE_CONFIG_KEY, &sealed).await.unwrap();
}

/// Sealed-`llm`-row writer (the admin LLM page's storage shape).
async fn write_llm(config: &ConfigStore, service_key: &ServiceKey, url: &str) {
    let sealed = mycelium_store::config::seal_config(
        service_key,
        "llm",
        &LlmConfig {
            url: url.to_string(),
            model: "mock".into(),
            api_key: None,
            ..Default::default()
        },
    );
    config.set("llm", &sealed).await.unwrap();
}

/// The last `dream_last_run` record (plain JSON — Ruling 7).
async fn last_run(config: &ConfigStore) -> serde_json::Value {
    config
        .get::<serde_json::Value>(DREAM_LAST_RUN_KEY)
        .await
        .unwrap()
        .expect("the attempted pass wrote the last-run record")
}

// ---------- metric / tick helpers ----------

struct Metrics {
    ticks: u64,
    passes: u64,
    skipped: u64,
}

fn metrics() -> Metrics {
    Metrics {
        ticks: DREAM_TICKS_TOTAL.load(Ordering::Relaxed),
        passes: DREAM_PASSES_TOTAL.load(Ordering::Relaxed),
        skipped: DREAM_PASSES_SKIPPED_TOTAL.load(Ordering::Relaxed),
    }
}

/// One awaited tick on a test-owned interval at `period`; returns the
/// interval's period AFTER the tick — the live-re-tune seam (absent
/// lifecycle rows must leave it untouched; present ones apply the
/// clamped stored value).
async fn tick_once(sched: &DreamScheduler, cursor: &mut usize, period: Duration) -> Duration {
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    sched.tick(cursor, &mut interval).await;
    interval.period()
}

/// A test interval builder (Delay — the loop's real behavior; the
/// default Burst would double-fire on the re-tuned periods).
fn test_interval(period: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

// ---------- pinned tests ----------

/// Bare install — no users, no lifecycle row, no llm row (the ruling's
/// llm-absent no-op): the first wake is a no-op pass — cursor stays,
/// nothing admitted, nothing recorded, skipped metric counts it, and
/// the absent lifecycle row never re-tunes the wake.
#[tokio::test]
async fn empty_install_first_tick_makes_no_llm_call() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let before = metrics();
    // A live mock is bound but never configured: any LLM call would be
    // recorded, so zero requests is proof by observation as well as by
    // construction.
    let (_llm_url, requests) = capturing_mock_llm(vec![]).await;
    let env = env().await;

    let mut cursor = 0usize;
    let period = tick_once(&env.scheduler, &mut cursor, Duration::from_millis(50)).await;

    assert_eq!(
        requests.lock().await.len(),
        0,
        "no backend call on an empty install"
    );
    assert_eq!(cursor, 0, "the no-op pass leaves the cursor alone");
    assert_eq!(
        period,
        Duration::from_millis(50),
        "an absent lifecycle row never re-tunes the wake"
    );
    let after = metrics();
    assert_eq!(after.ticks - before.ticks, 1, "exactly one wake");
    assert_eq!(after.passes - before.passes, 0, "nothing attempted");
    assert_eq!(
        after.skipped - before.skipped,
        1,
        "the no-op wake is a skip"
    );
    assert!(
        env.config
            .get::<serde_json::Value>(DREAM_LAST_RUN_KEY)
            .await
            .unwrap()
            .is_none(),
        "nothing attempted, nothing recorded"
    );
}

/// A mid-run disable finishes cleanly and skips thereafter — the
/// disabled short circuit: no admission, no pass, no cursor advance,
/// one skipped wake; re-enabling continues the rotation where it left
/// off (pinned by tick 3 hitting the NEXT entry).
#[tokio::test]
async fn disabled_mid_run_finishes_the_pass_then_skips() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let before = metrics();
    // The mock serves LIFO (later vec elements pop first), so a script
    // is written in REVERSE serve order: tick 1 pops "pass one", tick 3
    // pops "pass two".
    let (llm_url, requests) = capturing_mock_llm(vec![
        text_response("pass two summary text"),
        text_response("pass one summary text"),
    ])
    .await;
    let env = env().await;
    // An admin user exercises the unrestricted-visibility scope build.
    let user = fixed_user(0x0b01);
    seed_user(env.store.as_ref(), user, "dream-disabler", "admin").await;
    orphan_in_user(&env, user).await;
    orphan_in_skills(&env).await;
    write_llm(&env.config, &env.service_key, &llm_url).await;
    write_lifecycle(&env.config, &env.service_key, TEST_INTERVAL, true).await;

    let mut cursor = 0usize;
    let mut interval = test_interval(Duration::from_millis(50));

    // Tick 1 — enabled: the first rotation entry (the user) runs.
    env.scheduler.tick(&mut cursor, &mut interval).await;
    assert_eq!(
        requests.lock().await.len(),
        1,
        "the enabled tick made an LLM call"
    );
    let rec = last_run(&env.config).await;
    assert_eq!(rec["scope"], format!("user:{user}"));
    assert_eq!(rec["ran"], true);
    assert_eq!(
        rec["summary"], "pass one summary text",
        "the record serializes the report's summary"
    );
    let mid = metrics();
    assert_eq!(
        mid.passes - before.passes,
        1,
        "the enabled wake attempted a pass"
    );
    assert_eq!(mid.skipped - before.skipped, 0);
    assert_eq!(cursor, 1, "the ran pass advanced the cursor");

    // Tick 2 — disabled (stored with a DIFFERENT interval so the
    // retune-precedes-enabled-check order is observable): no admission,
    // no pass, no advance.
    write_lifecycle(&env.config, &env.service_key, 7_200, false).await;
    env.scheduler.tick(&mut cursor, &mut interval).await;
    assert_eq!(
        interval.period(),
        Duration::from_secs(7_200),
        "a present row re-tunes the interval even when disabled — \
         the retune precedes the enabled check"
    );
    assert_eq!(
        requests.lock().await.len(),
        1,
        "the disabled tick made no LLM call"
    );
    let mid2 = metrics();
    assert_eq!(
        mid2.passes - before.passes,
        1,
        "nothing attempted while disabled"
    );
    assert_eq!(
        mid2.skipped - before.skipped,
        1,
        "the disabled wake is a skip"
    );
    assert_eq!(cursor, 1, "the disabled tick advances nothing");

    // Tick 3 — re-enabled: the NEXT rotation entry runs — proving the
    // disabled tick never advanced (had it, this would re-hit the user).
    write_lifecycle(&env.config, &env.service_key, TEST_INTERVAL, true).await;
    env.scheduler.tick(&mut cursor, &mut interval).await;
    assert_eq!(
        requests.lock().await.len(),
        2,
        "the re-enabled tick runs the next pass"
    );
    let rec = last_run(&env.config).await;
    assert_eq!(rec["scope"], "global:skills");
    assert_eq!(rec["ran"], true);
    assert_eq!(rec["summary"], "pass two summary text");
}

/// A RED backend defers the wake before admission (drain parity): no
/// admission, no LLM call, cursor stays, one skipped wake, nothing
/// recorded — and once the backend goes Green again, the SAME scope
/// runs (the cursor never moved).
#[tokio::test]
async fn red_backend_skips_the_tick() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let before = metrics();
    let (llm_url, requests) = capturing_mock_llm(vec![text_response("recovered summary")]).await;
    let env = env().await;
    let user = fixed_user(0x0d01);
    seed_user(env.store.as_ref(), user, "dream-red", "user").await;
    orphan_in_user(&env, user).await;
    write_llm(&env.config, &env.service_key, &llm_url).await;
    write_lifecycle(&env.config, &env.service_key, TEST_INTERVAL, true).await;

    // Drive the process-global health machine to Red: three consecutive
    // failures (RED_FAILURE_THRESHOLD). The fresh static is configured
    // (its Default), so the notes count.
    for _ in 0..RED_FAILURE_THRESHOLD {
        LLM_HEALTH.note_failure();
    }
    assert_eq!(LLM_HEALTH.state(), BackendState::Red);

    let mut cursor = 0usize;
    let mut interval = test_interval(Duration::from_millis(50));
    env.scheduler.tick(&mut cursor, &mut interval).await;

    assert_eq!(
        requests.lock().await.len(),
        0,
        "the Red tick made no LLM call — nothing admitted"
    );
    assert_eq!(cursor, 0, "the Red tick advances nothing");
    let now = metrics();
    assert_eq!(now.ticks - before.ticks, 1);
    assert_eq!(now.passes - before.passes, 0, "no pass attempted");
    assert_eq!(now.skipped - before.skipped, 1, "the Red wake is a skip");
    assert!(
        env.config
            .get::<serde_json::Value>(DREAM_LAST_RUN_KEY)
            .await
            .unwrap()
            .is_none(),
        "deferring is not an attempt — nothing recorded"
    );

    // Recovery — reset the machine (Green) and the SAME first scope
    // runs, proving the cursor never moved under Red.
    LLM_HEALTH.reset_for_test();
    env.scheduler.tick(&mut cursor, &mut interval).await;
    assert_eq!(requests.lock().await.len(), 1, "the recovered tick runs");
    let rec = last_run(&env.config).await;
    assert_eq!(rec["scope"], format!("user:{user}"));
    assert_eq!(rec["ran"], true);
    assert_eq!(cursor, 1);
}

/// A busy gate defers the tick: the try class never waits — no slot is
/// taken, the cursor stays, the wake counts as skipped; once the gate
/// frees, the SAME first scope runs (the cursor never moved).
#[tokio::test]
async fn gate_busy_tick_does_not_advance_the_cursor() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let before = metrics();
    let (llm_url, requests) = capturing_mock_llm(vec![text_response("one dream summary")]).await;
    let env = env().await;
    let user = fixed_user(0x0c01);
    seed_user(env.store.as_ref(), user, "dream-gate", "user").await;
    orphan_in_user(&env, user).await;
    write_llm(&env.config, &env.service_key, &llm_url).await;
    write_lifecycle(&env.config, &env.service_key, TEST_INTERVAL, true).await;

    // Fill the gate by try-entering until the gate refuses (the brief's
    // hygiene: hold permits until None — never resize the capacity).
    let mut holds = Vec::new();
    while let Some(permit) = LLM_GATE.try_enter() {
        holds.push(permit);
    }
    assert_eq!(
        holds.len(),
        DEFAULT_CAPACITY,
        "the try class admitted up to capacity"
    );
    // A REAL wait-class waiter: queued behind the held slots.
    let waiter = tokio::spawn(async move {
        LLM_GATE
            .enter(None, GATE_BOUND, |_| {})
            .await
            .expect("admitted once a slot frees");
    });
    wait_until("the waiter queued", async || LLM_GATE.stats().queued == 1).await;

    let mut cursor = 0usize;
    let mut interval = test_interval(Duration::from_millis(50));

    // Tick while the gate is busy: try class — no wait, no pass.
    env.scheduler.tick(&mut cursor, &mut interval).await;
    assert_eq!(
        requests.lock().await.len(),
        0,
        "a busy gate makes no LLM call"
    );
    assert_eq!(cursor, 0, "a deferred tick leaves the cursor alone");
    let mid = metrics();
    assert_eq!(mid.passes - before.passes, 0);
    assert_eq!(mid.skipped - before.skipped, 1, "the gated wake is a skip");

    // Release: the waiter is admitted and completes; the gate drains.
    drop(holds);
    waiter.await.unwrap();
    wait_until("the gate drained", async || {
        let s = LLM_GATE.stats();
        s.active == 0 && s.queued == 0
    })
    .await;

    // The freed gate admits the tick: the SAME first scope runs.
    env.scheduler.tick(&mut cursor, &mut interval).await;
    assert_eq!(
        requests.lock().await.len(),
        1,
        "the freed tick runs its pass"
    );
    let rec = last_run(&env.config).await;
    assert_eq!(
        rec["scope"],
        format!("user:{user}"),
        "the deferred tick did not advance"
    );
    assert_eq!(rec["ran"], true);
    assert_eq!(cursor, 1);
    let after = metrics();
    assert_eq!(after.passes - before.passes, 1);
}

/// A stored row whose dream_interval_s differs from the running period
/// re-tunes the wake live (Ruling 6): the first tick applies it, and a
/// changed row applies on the next tick.
#[tokio::test]
async fn interval_rebuild_applies_live() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let before = metrics();
    let env = env().await; // no llm row: the ticks end at the LLM-resolution no-op
    write_lifecycle(&env.config, &env.service_key, 3_600, true).await;

    let mut cursor = 0usize;
    let mut interval = test_interval(Duration::from_millis(50));

    // Tick 1: the stored 3600 s replaces the test period, live.
    env.scheduler.tick(&mut cursor, &mut interval).await;
    assert_eq!(
        interval.period(),
        Duration::from_secs(3_600),
        "the stored interval applied live"
    );
    assert_eq!(cursor, 0, "the no-backend tick attempted nothing");

    // Tick 2: a newly stored value applies on the NEXT tick.
    write_lifecycle(&env.config, &env.service_key, 7_200, true).await;
    env.scheduler.tick(&mut cursor, &mut interval).await;
    assert_eq!(
        interval.period(),
        Duration::from_secs(7_200),
        "the changed value applies on the next tick"
    );

    let after = metrics();
    assert_eq!(after.ticks - before.ticks, 2);
    assert_eq!(after.passes - before.passes, 0);
    assert_eq!(
        after.skipped - before.skipped,
        2,
        "both no-backend wakes are skips"
    );
}

/// The carried obligation from Task 1: `get_sealed` deserializes the
/// row unclamped, so the per-tick read MUST apply `clamp()` — pinned
/// through the interval re-tune seam, both directions.
#[tokio::test]
async fn out_of_range_dream_interval_s_clamps_on_read() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let env = env().await;
    // A hand-edited row below the range floor — a save-side clamp would
    // never produce it; the read-side clamp is the only defense.
    write_lifecycle(&env.config, &env.service_key, 5, true).await;

    let mut cursor = 0usize;
    let mut interval = test_interval(Duration::from_millis(50));
    env.scheduler.tick(&mut cursor, &mut interval).await;
    assert_eq!(
        interval.period(),
        Duration::from_secs(u64::from(DREAM_INTERVAL_S_RANGE.0)),
        "5 clamps UP to the range floor on read"
    );

    write_lifecycle(&env.config, &env.service_key, 5_000_000, true).await;
    env.scheduler.tick(&mut cursor, &mut interval).await;
    assert_eq!(
        interval.period(),
        Duration::from_secs(u64::from(DREAM_INTERVAL_S_RANGE.1)),
        "5M clamps DOWN to the range ceiling on read"
    );
}

/// One continuous rotation (users in `ORDER BY id` order, then the
/// skills shelf), one scope per wake, wrapping after the tail; an
/// empty scope is still an ATTEMPTED pass (an honest no-signal record,
/// no LLM call) — the cursor advances on it like any other attempt.
#[tokio::test]
async fn scheduler_rotates_all_scopes_then_wraps() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let before = metrics();
    // The mock serves LIFO (later vec elements pop first): tick 1 pops
    // "user a pass", tick 2 "user b pass", tick 4 "user a pass, wrapped".
    let (llm_url, requests) = capturing_mock_llm(vec![
        text_response("user a pass, wrapped"),
        text_response("user b pass"),
        text_response("user a pass"),
    ])
    .await;
    let env = env().await;
    let a = fixed_user(1);
    let b = fixed_user(2); // "…0001" < "…0002" — ORDER BY id fixes the order
    seed_user(env.store.as_ref(), a, "dream-rot-a", "user").await;
    seed_user(env.store.as_ref(), b, "dream-rot-b", "user").await;
    orphan_in_user(&env, a).await;
    orphan_in_user(&env, b).await;
    // The skills shelf stays EMPTY: its pass is attempted-without-signals
    // (an honest record, zero LLM calls) — rotation coverage.
    write_llm(&env.config, &env.service_key, &llm_url).await;
    write_lifecycle(&env.config, &env.service_key, TEST_INTERVAL, true).await;

    let mut cursor = 0usize;
    let mut interval = test_interval(Duration::from_millis(50));

    let mut seen = Vec::new();
    for _ in 0..4 {
        env.scheduler.tick(&mut cursor, &mut interval).await;
        let rec = last_run(&env.config).await;
        seen.push(rec["scope"].as_str().unwrap().to_string());
    }
    assert_eq!(
        seen,
        [
            format!("user:{a}"),
            format!("user:{b}"),
            "global:skills".to_string(),
            format!("user:{a}"),
        ],
        "one continuous rotation users-then-skills, wrapping after the tail"
    );
    assert_eq!(
        requests.lock().await.len(),
        3,
        "the two user scopes + the wrap re-hit ask the LLM; the empty skills shelf signals nothing"
    );
    assert_eq!(
        cursor, 1,
        "four wakes on a three-entry rotation leave the cursor one in"
    );
    let after = metrics();
    assert_eq!(after.ticks - before.ticks, 4);
    assert_eq!(
        after.passes - before.passes,
        4,
        "every wake attempted a pass"
    );
    assert_eq!(after.skipped - before.skipped, 0, "nothing was skipped");
    let rec = last_run(&env.config).await;
    assert_eq!(rec["ran"], true);
    assert_eq!(rec["summary"], "user a pass, wrapped");
}

/// The run-loop wiring: a spawned worker ticks at the test cadence on
/// a bare install (every wake the LLM no-op), attempts nothing, and
/// exits cleanly on shutdown. The cadence pin lives here because the
/// production wake period (≥1 h) cannot be observed in a test.
#[tokio::test]
async fn spawned_worker_ticks_and_shuts_down() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let before = metrics();
    let env = env().await;
    let shutdown = CancellationToken::new();
    let handle = env
        .scheduler
        .clone()
        .spawn_with_tick(shutdown.clone(), Duration::from_millis(20));
    tokio::time::sleep(Duration::from_millis(150)).await;
    let after = metrics();
    assert!(
        after.ticks - before.ticks >= 2,
        "the loop ticks at the test cadence"
    );
    assert_eq!(
        after.passes - before.passes,
        0,
        "a bare install attempts nothing"
    );
    assert_eq!(
        after.skipped - before.skipped,
        after.ticks - before.ticks,
        "every no-op wake skips"
    );
    shutdown.cancel();
    handle
        .await
        .expect("the scheduler exits cleanly on shutdown");
}

// ---------- admin-triggered corpus backfill (Task 4 — server-boot) ----------
//
// The pinned backfill tests drive the REAL admin surface: a real
// AppState (the production MasterKeyCache recovery seam the walk
// resolves scopes through) behind `mycelium_web::serve`, an admin
// login + CSRF flow, and POST /admin/dream/backfill. The direct-struct
// pins above cover the scheduler's own flow; these pin Ruling 8's
// detached worker, Ruling 7's marker semantics, Review Focus 4, and
// the /health + /metrics wiring. The same TEST_LOCK discipline covers
// the process-global gate/health the worker touches.

const ADMIN_PASSWORD: &str = "backfill admin password for the suite!";
const REAL_PASSWORD: &str = "backfill user password for the suite!";

/// The orphan paths the walk pins ride on: the agent's mutation
/// instruction carries every signal's path, so a captured request body
/// identifies the scope it belonged to.
const A_PATH: &str = "/orphan/backfill-a.md";
const B_PATH: &str = "/orphan/backfill-b.md";
const REAL_PATH: &str = "/orphan/backfill-real.md";
const BROKEN_PATH: &str = "/orphan/backfill-broken-secret.md";
const OWNER_PATH: &str = "/orphan/backfill-gate.md";
const SLOW_PATH: &str = "/orphan/backfill-slow.md";
const SKILLS_PATH: &str = "/skills/backfill-skills-orphan.md";

/// A REAL app behind `mycelium_web::serve`: the production AppState
/// (no stub recovery), the admin account, and the /health + /metrics
/// readouts the pins assert on.
struct WebEnv {
    _dir: tempfile::TempDir,
    base: String,
    shutdown: CancellationToken,
    state: AppState,
    client: reqwest::Client,
}

async fn boot_web() -> WebEnv {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let service_key = mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();
    let users = mycelium_auth::UserStore::new(store.pool().clone());
    users
        .create_local(
            "admin",
            "admin@localhost.local",
            ADMIN_PASSWORD,
            mycelium_auth::Role::Admin,
        )
        .await
        .unwrap();
    let assets_dir = dir.path().join("assets");
    mycelium_web::assets::scaffold_defaults(&assets_dir).unwrap();
    let login = mycelium_auth::login::LoginService::new(store.pool().clone());
    let state = AppState::new(
        store,
        service_key,
        login,
        assets_dir,
        dir.path().to_path_buf(),
        None,
    );
    // The app token installs ahead of `serve`: the handler-spawned
    // backfill worker selects on it like every spawned-at-boot worker
    // (main.rs performs the same install at boot).
    let shutdown = CancellationToken::new();
    let _ = state.shutdown.set(shutdown.clone());

    let https: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let http: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = tokio::net::TcpListener::bind(https).await.unwrap();
    let https_port = listener.local_addr().unwrap().port();
    drop(listener);
    let data_dir = dir.path().to_path_buf();
    let serve_state = state.clone();
    let serve_token = shutdown.clone();
    tokio::spawn(async move {
        let _ = mycelium_web::serve(
            serve_state,
            &data_dir,
            format!("127.0.0.1:{https_port}").parse().unwrap(),
            http,
            None,
            None,
            vec!["localhost".to_string()],
            serve_token,
        )
        .await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    WebEnv {
        _dir: dir,
        base: format!("https://127.0.0.1:{https_port}"),
        shutdown,
        state,
        client,
    }
}

/// A real, loadable user: create_local + the production service seal —
/// `persist_service_seal` is exactly what the user-creation and login
/// handlers run, and without it the cache's cold recovery cannot
/// unseal a freshly created user.
async fn real_user(env: &WebEnv, name: &str) -> (Uuid, mycelium_crypto::keys::MasterKey) {
    let created = env
        .state
        .users
        .create_local(
            name,
            &format!("{name}@localhost.local"),
            REAL_PASSWORD,
            mycelium_auth::Role::User,
        )
        .await
        .unwrap();
    env.state
        .persist_service_seal(created.record.id, &created.master_key)
        .await
        .unwrap();
    (created.record.id, created.master_key)
}

/// An orphan on the service skills shelf (the walk's final scope).
async fn put_skills_orphan(state: &AppState, path: &str, title: &str, body: &str) {
    let cs = ConceptStore::for_service(
        state.store.as_ref(),
        (*state.service_key).clone(),
        &state.store.skills_dir(),
        "skills",
    );
    put_orphan(&cs, path, title, body).await;
}

fn cookie_from(response: &reqwest::Response) -> String {
    response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

async fn csrf_from_page(client: &reqwest::Client, url: &str, cookie: &str) -> String {
    let page = client
        .get(url)
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    let html = page.text().await.unwrap();
    let marker = r#"name="csrf-token" content=""#;
    let idx = html.find(marker).expect("csrf meta present");
    let rest = &html[idx + marker.len()..];
    let end = rest.find('"').expect("closing quote");
    rest[..end].to_string()
}

/// Log in as `name`; returns (cookie, csrf).
async fn login_as(
    client: &reqwest::Client,
    base: &str,
    name: &str,
    password: &str,
) -> (String, String) {
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", name), ("password", password)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303, "the login POST redirects");
    let cookie = cookie_from(&login);
    let csrf = csrf_from_page(client, &format!("{base}/"), &cookie).await;
    (cookie, csrf)
}

async fn login_admin(client: &reqwest::Client, base: &str) -> (String, String) {
    login_as(client, base, "admin", ADMIN_PASSWORD).await
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"),
                16,
            ) {
                out.push(b);
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

/// The Location of a 303 redirect, percent-decoded for assertions.
async fn location_of(resp: reqwest::Response) -> String {
    assert_eq!(resp.status(), 303, "admin POSTs must redirect back");
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("location header")
        .to_string();
    percent_decode(&loc)
}

/// POST /admin/dream/backfill — the page's own flow (session cookie +
/// CSRF header; the handler takes no form body).
async fn post_backfill(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
) -> reqwest::Response {
    client
        .post(format!("{base}/admin/dream/backfill"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .send()
        .await
        .unwrap()
}

/// The Ruling-7 marker: a plain RFC3339 string row.
async fn backfill_marker(config: &ConfigStore) -> Option<String> {
    config.get::<String>(DREAM_BACKFILL_AT_KEY).await.unwrap()
}

/// /health's `dream_backfill_complete` flag (Task 4's field).
async fn backfill_flag(client: &reqwest::Client, base: &str) -> bool {
    let health: serde_json::Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    health["dream_backfill_complete"]
        .as_bool()
        .expect("dream_backfill_complete present")
}

/// The `mycelium2_dream_backfill_active` gauge value from /metrics.
async fn backfill_gauge(client: &reqwest::Client, base: &str) -> u64 {
    let text = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let line = text
        .lines()
        .find(|l| l.starts_with("mycelium2_dream_backfill_active"))
        .expect("the backfill gauge line is present");
    line.split(' ')
        .next_back()
        .unwrap()
        .trim()
        .parse()
        .expect("the gauge line's value")
}

/// The shared running-guard (the same flag /metrics' gauge reads) —
/// for the shutdown pin, where a cancelled server may not serve new
/// requests.
fn backfill_guard(env: &WebEnv) -> bool {
    env.state
        .dream_backfill_running
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// The whole-walk happy path (Rulings 7+8): the click accepts with an
/// ok flash targeting /admin/dream, the walk addresses every
/// maintenance scope — the users `ORDER BY id` first, the
/// `global:skills` shelf strictly last — and only completion writes
/// THE marker (a plain RFC3339 row), which flips /health's
/// `dream_backfill_complete` from false to true. `dream_last_run` is
/// NOT touched: that record is the scheduler's own (Ruling 8).
#[tokio::test]
async fn backfill_walks_every_scope_and_writes_marker() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    // The mock serves LIFO; the walk makes exactly one request per
    // signal-carrying scope (a text answer ends the pass) — three
    // scripts for two orphaned users + the orphaned skills shelf.
    let (llm_url, requests) = capturing_mock_llm(vec![
        text_response("skills summary text"),
        text_response("user b summary text"),
        text_response("user a summary text"),
    ])
    .await;
    let env = boot_web().await;
    let (a_id, a_master) = real_user(&env, "backfill-a").await;
    let (b_id, b_master) = real_user(&env, "backfill-b").await;
    put_orphan(
        &ConceptStore::for_user(env.state.store.as_ref(), a_id, a_master),
        A_PATH,
        "Backfill Alpha Note",
        "nobody links to alpha",
    )
    .await;
    put_orphan(
        &ConceptStore::for_user(env.state.store.as_ref(), b_id, b_master),
        B_PATH,
        "Backfill Bravo Note",
        "nobody links to bravo",
    )
    .await;
    put_skills_orphan(
        &env.state,
        SKILLS_PATH,
        "Backfill Skills Orphan",
        "nobody links to the skills orphan",
    )
    .await;
    write_llm(&env.state.config, &env.state.service_key, &llm_url).await;

    // Pre-state: no marker, the /health flag false (Ruling 7's flip).
    assert!(backfill_marker(&env.state.config).await.is_none());
    assert!(!backfill_flag(&env.client, &env.base).await);

    let (cookie, csrf) = login_admin(&env.client, &env.base).await;
    let resp = post_backfill(&env.client, &env.base, &cookie, &csrf).await;
    assert_eq!(resp.status(), 303, "the accepted click redirects");
    assert_eq!(
        location_of(resp).await,
        "/admin/dream?ok=Backfill started",
        "the ok flash targets the Dream admin page"
    );

    wait_until("the backfill marker written", async || {
        backfill_marker(&env.state.config).await.is_some()
    })
    .await;
    assert!(
        backfill_flag(&env.client, &env.base).await,
        "/health flips true once the walk completes"
    );
    let marker = backfill_marker(&env.state.config).await.unwrap();
    chrono::DateTime::parse_from_rfc3339(&marker)
        .expect("the marker records an RFC3339 timestamp (Ruling 7)");

    assert!(
        env.state
            .config
            .get::<serde_json::Value>(DREAM_LAST_RUN_KEY)
            .await
            .unwrap()
            .is_none(),
        "the backfill never writes dream_last_run — that readout is the scheduler's own"
    );

    // Order: the two user scopes first (either user order), the skills
    // shelf strictly last — each scope's orphan path rides its
    // request's mutation instruction.
    let reqs = requests.lock().await;
    assert_eq!(
        reqs.len(),
        3,
        "one request per signal-carrying scope: two users + skills (the admin scope is unresolvable — no service seal)"
    );
    let bodies: Vec<String> = reqs
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect();
    for body in &bodies[..2] {
        assert!(
            body.contains(A_PATH) || body.contains(B_PATH),
            "the first two requests are the two user scopes: {body}"
        );
        assert!(
            !body.contains(SKILLS_PATH),
            "the skills shelf runs strictly last: {body}"
        );
    }
    assert!(
        bodies[0].contains(A_PATH) != bodies[1].contains(A_PATH),
        "the two user requests are the two distinct user scopes"
    );
    assert!(
        bodies[2].contains(SKILLS_PATH),
        "the last request is the skills shelf: {}",
        bodies[2]
    );

    // Teardown: stop the server before the lock releases (the file's
    // spawned-worker discipline; the walk itself is already finished —
    // the marker + the released gauge pinned that above).
    env.shutdown.cancel();
}

/// Review Focus 4: a user whose master key cannot be unsealed is
/// warned + skipped — no stall (the walk reaches the remaining scopes
/// and writes THE marker), no plaintext (its orphan was seeded under a
/// key the walk never learns, and its path appears in no captured
/// request), no record for it (`dream_last_run` stays absent — the
/// backfill writes no readout at all).
#[tokio::test]
async fn backfill_skips_unloadable_keys_and_finishes() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let (llm_url, requests) = capturing_mock_llm(vec![
        text_response("skills summary text"),
        text_response("real summary text"),
    ])
    .await;
    let env = boot_web().await;
    // A REAL user: production keys, one orphan.
    let (real_id, real_master) = real_user(&env, "backfill-real").await;
    put_orphan(
        &ConceptStore::for_user(env.state.store.as_ref(), real_id, real_master),
        REAL_PATH,
        "Backfill Real Note",
        "nobody links to real",
    )
    .await;
    // A broken-key user: a raw row WITHOUT a service seal — the
    // production recovery cannot unseal it (the exact unloadable-key
    // posture). Its corpus is seeded under a test-owned master the
    // walk never learns.
    let broken = fixed_user(0x4b01);
    seed_user(env.state.store.as_ref(), broken, "backfill-broken", "user").await;
    let broken_master = generate_master_key();
    put_orphan(
        &ConceptStore::for_user(env.state.store.as_ref(), broken, broken_master),
        BROKEN_PATH,
        "Broken Secret Note",
        "nobody links to the broken orphan",
    )
    .await;
    put_skills_orphan(
        &env.state,
        SKILLS_PATH,
        "Backfill Skills Orphan",
        "nobody links to the skills orphan",
    )
    .await;
    write_llm(&env.state.config, &env.state.service_key, &llm_url).await;

    let (cookie, csrf) = login_admin(&env.client, &env.base).await;
    let resp = post_backfill(&env.client, &env.base, &cookie, &csrf).await;
    assert_eq!(resp.status(), 303);
    assert_eq!(location_of(resp).await, "/admin/dream?ok=Backfill started");

    wait_until("the backfill marker written", async || {
        backfill_marker(&env.state.config).await.is_some()
    })
    .await;

    let reqs = requests.lock().await;
    assert_eq!(
        reqs.len(),
        2,
        "the real user + the skills shelf; the broken-key user never reached admission"
    );
    for r in reqs.iter() {
        let body = serde_json::to_string(r).unwrap();
        assert!(
            !body.contains(BROKEN_PATH),
            "no plaintext read of the broken scope: {body}"
        );
    }
    assert!(backfill_flag(&env.client, &env.base).await);
    assert!(
        env.state
            .config
            .get::<serde_json::Value>(DREAM_LAST_RUN_KEY)
            .await
            .unwrap()
            .is_none(),
        "no record for the skipped scope either — the walk writes no readout"
    );

    env.shutdown.cancel();
}

/// Ruling 8's single-run guard: the guard is claimed by the FIRST
/// click (set synchronously before its redirect), a second click while
/// the walk runs is rejected with the running flash and touches
/// nothing; after the walk completes the guard releases (the gauge
/// reads 0), and a re-click re-runs the walk — the marker CLEARED at
/// click time (that is Ruling 7's re-clickable semantics), then
/// rewritten at completion.
#[tokio::test]
async fn backfill_double_click_rejected_then_reclickable() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    // A slow pass: one orphaned user, the only signal-carrying scope —
    // the walk's lifetime is the mock's delay, so the second POST
    // arrives mid-run deterministically.
    let (llm_url, requests) = capturing_mock_llm_with_delay(
        vec![
            text_response("second run summary"),
            text_response("first run summary"),
        ],
        Duration::from_millis(400),
    )
    .await;
    let env = boot_web().await;
    let (uid, master) = real_user(&env, "backfill-slow").await;
    put_orphan(
        &ConceptStore::for_user(env.state.store.as_ref(), uid, master),
        SLOW_PATH,
        "Backfill Slow Note",
        "nobody links to slow",
    )
    .await;
    write_llm(&env.state.config, &env.state.service_key, &llm_url).await;
    let (cookie, csrf) = login_admin(&env.client, &env.base).await;

    let r1 = post_backfill(&env.client, &env.base, &cookie, &csrf).await;
    assert_eq!(r1.status(), 303);
    assert_eq!(location_of(r1).await, "/admin/dream?ok=Backfill started");
    wait_until("the gauge reads running", async || {
        backfill_gauge(&env.client, &env.base).await == 1
    })
    .await;

    // The second click is rejected while the walk runs.
    let r2 = post_backfill(&env.client, &env.base, &cookie, &csrf).await;
    assert_eq!(r2.status(), 303);
    assert_eq!(
        location_of(r2).await,
        "/admin/dream?error=Backfill already running"
    );

    // The first run completes: the marker appears, the guard releases.
    wait_until("the first marker", async || {
        backfill_marker(&env.state.config).await.is_some()
    })
    .await;
    let first_marker = backfill_marker(&env.state.config).await.unwrap();
    wait_until("the gauge back to 0", async || {
        backfill_gauge(&env.client, &env.base).await == 0
    })
    .await;

    // Re-clickable: the third click accepts, the marker dies at click
    // time, then is rewritten at completion.
    let r3 = post_backfill(&env.client, &env.base, &cookie, &csrf).await;
    assert_eq!(r3.status(), 303);
    assert_eq!(location_of(r3).await, "/admin/dream?ok=Backfill started");
    assert!(
        backfill_marker(&env.state.config).await.is_none(),
        "the marker dies at click time"
    );
    wait_until("the rewritten marker", async || {
        backfill_marker(&env.state.config).await.is_some()
    })
    .await;
    let second_marker = backfill_marker(&env.state.config).await.unwrap();
    assert_ne!(second_marker, first_marker, "a fresh completion rewrote it");
    wait_until("the gauge back to 0 again", async || {
        backfill_gauge(&env.client, &env.base).await == 0
    })
    .await;
    assert_eq!(requests.lock().await.len(), 2, "two runs, one pass each");

    env.shutdown.cancel();
}

/// Ruling 8's admission posture: a busy gate skips every scope (try
/// class never waits), the skipped scopes are untouched (the signals
/// persist for the scheduler / the next click), the run still
/// completes and writes THE marker — and a scheduler tick later, on
/// the freed gate, surfaces the persisted signal exactly as the
/// scheduler's readout promises.
#[tokio::test]
async fn backfill_gate_busy_scope_skipped_signals_persist() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    // ONE script: only the post-walk tick asks.
    let (llm_url, requests) =
        capturing_mock_llm(vec![text_response("surfaced summary text")]).await;
    let env = boot_web().await;
    // A FIXED-id loadable user: 00000000-… sorts before every real v4
    // id, so the post-walk tick's first rotation entry is THIS scope.
    let owner = fixed_user(0x0d01);
    seed_user(env.state.store.as_ref(), owner, "backfill-gate", "user").await;
    let owner_master = generate_master_key();
    env.state
        .persist_service_seal(owner, &owner_master)
        .await
        .unwrap();
    put_orphan(
        &ConceptStore::for_user(env.state.store.as_ref(), owner, owner_master.clone()),
        OWNER_PATH,
        "Backfill Gate Orphan",
        "nobody links to gate",
    )
    .await;
    put_skills_orphan(
        &env.state,
        SKILLS_PATH,
        "Backfill Skills Orphan",
        "nobody links to the skills orphan",
    )
    .await;
    write_llm(&env.state.config, &env.state.service_key, &llm_url).await;

    // Fill the gate (try-permit holds until None — the file's hygiene
    // never resizes the capacity), then click.
    let mut holds = Vec::new();
    while let Some(permit) = LLM_GATE.try_enter() {
        holds.push(permit);
    }
    assert_eq!(
        holds.len(),
        DEFAULT_CAPACITY,
        "the try class admitted up to capacity"
    );

    let (cookie, csrf) = login_admin(&env.client, &env.base).await;
    let resp = post_backfill(&env.client, &env.base, &cookie, &csrf).await;
    assert_eq!(resp.status(), 303);
    assert_eq!(location_of(resp).await, "/admin/dream?ok=Backfill started");

    // The walk skips every scope and still completes (the marker).
    wait_until("the marker despite the busy gate", async || {
        backfill_marker(&env.state.config).await.is_some()
    })
    .await;
    wait_until("the guard released", async || {
        backfill_gauge(&env.client, &env.base).await == 0
    })
    .await;
    assert_eq!(
        requests.lock().await.len(),
        0,
        "a busy gate never admits a backfill pass"
    );

    // The skipped scopes are untouched — the signals persist.
    let cs = ConceptStore::for_user(env.state.store.as_ref(), owner, owner_master);
    assert!(
        cs.get(OWNER_PATH).await.is_ok(),
        "the skipped scope's orphan is still there"
    );
    assert!(
        env.state
            .config
            .get::<serde_json::Value>(DREAM_LAST_RUN_KEY)
            .await
            .unwrap()
            .is_none(),
        "the walk wrote no scheduler readout"
    );

    // Later, on the freed gate, the scheduler tick surfaces it.
    drop(holds);
    wait_until("the gate drained", async || {
        let s = LLM_GATE.stats();
        s.active == 0 && s.queued == 0
    })
    .await;
    write_lifecycle(
        &env.state.config,
        &env.state.service_key,
        TEST_INTERVAL,
        true,
    )
    .await;
    let mut cursor = 0usize;
    let mut interval = test_interval(Duration::from_millis(50));
    env.state
        .dream_scheduler
        .tick(&mut cursor, &mut interval)
        .await;
    assert_eq!(
        requests.lock().await.len(),
        1,
        "the freed tick surfaces the first scope's persisted signal"
    );
    let rec = last_run(&env.state.config).await;
    assert_eq!(rec["scope"], format!("user:{owner}"));
    assert_eq!(rec["ran"], true);

    env.shutdown.cancel();
}

/// Ruling 8's shutdown clause: when the app token fires mid-walk the
/// worker dies silently — no marker, and the guard RELEASED (a dead
/// worker must never wedge the button; the next click restarts it
/// after the process comes back). The guard is read off the shared
/// flag (the same state the /metrics gauge reads) because a cancelled
/// server may not serve new requests.
#[tokio::test]
async fn backfill_shutdown_dies_silently_without_marker() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let (llm_url, _requests) = capturing_mock_llm_with_delay(
        vec![text_response("never surfaces")],
        Duration::from_millis(800),
    )
    .await;
    let env = boot_web().await;
    let (uid, master) = real_user(&env, "backfill-shutdown").await;
    put_orphan(
        &ConceptStore::for_user(env.state.store.as_ref(), uid, master),
        SLOW_PATH,
        "Backfill Shutdown Orphan",
        "nobody links to shutdown",
    )
    .await;
    write_llm(&env.state.config, &env.state.service_key, &llm_url).await;
    let (cookie, csrf) = login_admin(&env.client, &env.base).await;

    let resp = post_backfill(&env.client, &env.base, &cookie, &csrf).await;
    assert_eq!(resp.status(), 303);
    assert_eq!(location_of(resp).await, "/admin/dream?ok=Backfill started");
    assert!(
        backfill_guard(&env),
        "the guard is claimed synchronously before the redirect"
    );

    env.shutdown.cancel();
    wait_until("the guard released on shutdown", async || {
        !backfill_guard(&env)
    })
    .await;
    assert!(
        backfill_marker(&env.state.config).await.is_none(),
        "a shutdown walk writes NO marker"
    );
}

/// Ruling 8's explicit-intent clause: a backfill is the admin's own
/// action, so `dream_enabled` gates NOTHING about it — with a lifecycle
/// row that would stop every scheduler wake cold, the walk still runs
/// the scopes and writes THE marker.
#[tokio::test]
async fn dream_enabled_false_does_not_gate_backfill() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let (llm_url, requests) =
        capturing_mock_llm(vec![text_response("disabled-row summary text")]).await;
    let env = boot_web().await;
    // A lifecycle row that DISABLES dreams (the scheduler's own short
    // circuit). Admin intent is NOT the scheduler's: the row does not
    // consult it.
    write_lifecycle(
        &env.state.config,
        &env.state.service_key,
        TEST_INTERVAL,
        false,
    )
    .await;
    let (uid, master) = real_user(&env, "backfill-disabled-row").await;
    put_orphan(
        &ConceptStore::for_user(env.state.store.as_ref(), uid, master),
        REAL_PATH,
        "Disabled Row Note",
        "nobody links to the disabled row",
    )
    .await;
    write_llm(&env.state.config, &env.state.service_key, &llm_url).await;
    let (cookie, csrf) = login_admin(&env.client, &env.base).await;

    let resp = post_backfill(&env.client, &env.base, &cookie, &csrf).await;
    assert_eq!(resp.status(), 303);
    assert_eq!(location_of(resp).await, "/admin/dream?ok=Backfill started");

    // The walk RUNS anyway: the marker appears, the real scope makes
    // its request, /health flips.
    wait_until("the marker written despite the disabled row", async || {
        backfill_marker(&env.state.config).await.is_some()
    })
    .await;
    assert!(
        backfill_flag(&env.client, &env.base).await,
        "the walk completed behind the disabled lifecycle row"
    );
    assert_eq!(
        requests.lock().await.len(),
        1,
        "the orphaned user scope ran (the admin scope is unresolvable, the skills shelf has no signals)"
    );
    assert!(
        env.state
            .config
            .get::<serde_json::Value>(DREAM_LAST_RUN_KEY)
            .await
            .unwrap()
            .is_none(),
        "the backfill still writes no scheduler readout"
    );

    env.shutdown.cancel();
}

/// The no-backend posture (the tick's step-4 shape): a walk without a
/// usable llm row abandons BEFORE the rotation — warned, guard
/// released by the RAII drop, NO marker — and a re-click after the
/// admin configures a backend restarts the walk cleanly (the guard
/// never wedged the button).
#[tokio::test]
async fn backfill_without_usable_llm_row_abandons_silently() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    // Deliberately NO llm row and NO mock server yet: the first walk
    // must abandon at resolve_llm_client, having touched nothing.
    let env = boot_web().await;
    let (uid, master) = real_user(&env, "backfill-nollm").await;
    put_orphan(
        &ConceptStore::for_user(env.state.store.as_ref(), uid, master),
        REAL_PATH,
        "No LLM Note",
        "nobody links to the no-llm orphan",
    )
    .await;
    let (cookie, csrf) = login_admin(&env.client, &env.base).await;

    let resp = post_backfill(&env.client, &env.base, &cookie, &csrf).await;
    assert_eq!(resp.status(), 303);
    assert_eq!(location_of(resp).await, "/admin/dream?ok=Backfill started");

    // The walk abandoned: the guard is back (RAII), nothing completed.
    wait_until("the guard released after the abandon", async || {
        backfill_gauge(&env.client, &env.base).await == 0
    })
    .await;
    assert!(
        backfill_marker(&env.state.config).await.is_none(),
        "no usable llm row — NO marker"
    );
    assert!(
        !backfill_flag(&env.client, &env.base).await,
        "/health stays false: nothing ran"
    );

    // Re-click after the backend exists: the SAME click flow works
    // (a wedged guard would return the already-running flash), the
    // walk runs the real scope, the marker appears.
    let (llm_url, requests) = capturing_mock_llm(vec![text_response("late summary text")]).await;
    write_llm(&env.state.config, &env.state.service_key, &llm_url).await;
    let r2 = post_backfill(&env.client, &env.base, &cookie, &csrf).await;
    assert_eq!(r2.status(), 303);
    assert_eq!(location_of(r2).await, "/admin/dream?ok=Backfill started");
    wait_until("the marker written by the restarted walk", async || {
        backfill_marker(&env.state.config).await.is_some()
    })
    .await;
    assert_eq!(
        requests.lock().await.len(),
        1,
        "the restarted walk ran its pass (and nothing ran in the first, aborted one)"
    );
    assert!(backfill_flag(&env.client, &env.base).await);

    env.shutdown.cancel();
}
