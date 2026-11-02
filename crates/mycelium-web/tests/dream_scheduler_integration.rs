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
use mycelium_librarian::llm_health::LLM_HEALTH;
use mycelium_librarian::queue_worker::MasterKeyRecovery;
use mycelium_store::revisions::{DREAM_INTERVAL_S_RANGE, LIFECYCLE_CONFIG_KEY, LifecycleConfig};
use mycelium_store::{ConceptStore, ConfigStore, Store};
use mycelium_web::LlmConfig;
use mycelium_web::dream_scheduler::{
    DREAM_LAST_RUN_KEY, DREAM_PASSES_SKIPPED_TOTAL, DREAM_PASSES_TOTAL, DREAM_TICKS_TOTAL,
    DreamScheduler,
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

/// A lone unlinked concept — the dream's orphan signal — in a user scope.
async fn orphan_in_user(env: &Env, uid: Uuid) {
    let cs = ConceptStore::for_user(env.store.as_ref(), uid, env.master.clone());
    cs.put(&concept(
        "/orphan/lonely.md",
        "Lonely Dream Note",
        "nobody links here",
    ))
    .await
    .unwrap();
}

/// … in the service skills scope.
async fn orphan_in_skills(env: &Env) {
    let cs = ConceptStore::for_service(
        env.store.as_ref(),
        (*env.service_key).clone(),
        &env.store.skills_dir(),
        "skills",
    );
    cs.put(&concept(
        "/skills/lonely.md",
        "Lonely Skill Note",
        "nobody links here",
    ))
    .await
    .unwrap();
}

/// Sealed-`lifecycle`-row writer (the step-4 admin surface's storage
/// shape). Tests hand-craft rows — including out-of-range values a
/// save-side clamp would never produce — to pin the READ-side clamp.
async fn write_lifecycle(env: &Env, interval_s: u32, enabled: bool) {
    let sealed = mycelium_store::config::seal_config(
        &env.service_key,
        LIFECYCLE_CONFIG_KEY,
        &LifecycleConfig {
            dream_interval_s: interval_s,
            dream_enabled: enabled,
            ..Default::default()
        },
    );
    env.config.set(LIFECYCLE_CONFIG_KEY, &sealed).await.unwrap();
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
    write_lifecycle(&env, TEST_INTERVAL, true).await;

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

    // Tick 2 — disabled: no admission, no pass, no advance.
    write_lifecycle(&env, TEST_INTERVAL, false).await;
    env.scheduler.tick(&mut cursor, &mut interval).await;
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
    write_lifecycle(&env, TEST_INTERVAL, true).await;
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
    write_lifecycle(&env, TEST_INTERVAL, true).await;

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
    write_lifecycle(&env, 3_600, true).await;

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
    write_lifecycle(&env, 7_200, true).await;
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
    write_lifecycle(&env, 5, true).await;

    let mut cursor = 0usize;
    let mut interval = test_interval(Duration::from_millis(50));
    env.scheduler.tick(&mut cursor, &mut interval).await;
    assert_eq!(
        interval.period(),
        Duration::from_secs(u64::from(DREAM_INTERVAL_S_RANGE.0)),
        "5 clamps UP to the range floor on read"
    );

    write_lifecycle(&env, 5_000_000, true).await;
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
    write_lifecycle(&env, TEST_INTERVAL, true).await;

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
