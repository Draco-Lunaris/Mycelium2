//! VectorWorker integration tests (memory-lifecycle spec §3.3): the
//! bounded write-hook queue drains through the admin-configured
//! embeddings mock into `search_vectors`; a dead backend pauses the
//! tick (Red) and a recovered one resumes — and a park-and-resume
//! worker re-queues what a mid-drain Red interrupted, so no boot
//! backlog dies to backend weather; the boot backlog re-enqueues
//! pre-worker writes and a model swap re-embeds the corpus; deleted
//! concepts are never resurrected; registry-dead `search_docs` rows
//! (the historical ghost residue) are swept — lexical index row plus
//! any vector row — instead of re-enqueueing every boot; and a drained
//! token resolving to no concept belts out with a DEBUG line carrying
//! the token. Direct-struct
//! tests (queue_worker_integration.rs shape): a real temp-dir store, a
//! stub master-key recovery, a scripted mock embeddings endpoint.
//!
//! Process-global state serializes on TEST_LOCK — the vector write
//! queue, the decoded scope cache, and the embeddings health machine.
//! Every test's queue is drained at entry, and every spawned worker
//! task is cancelled and JOINED before the test's lock guard releases
//! (the probe tests' shutdown discipline).

use std::ops::AsyncFnMut;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::response::IntoResponse;
use mycelium_core::concept::{Concept, Frontmatter};
use mycelium_crypto::keys::{MasterKey, ServiceKey, generate_master_key};
use mycelium_librarian::llm_health::{
    BackendState, EMB_LAST_SUCCESS, EMBEDDING_DIMS, EMBEDDING_HEALTH,
};
use mycelium_librarian::queue_worker::MasterKeyRecovery;
use mycelium_librarian::vector_worker::VectorWorker;
use mycelium_store::vectors::{
    VECTOR_PROCESSED_TOTAL, VECTOR_QUEUE_CAPACITY, VectorStore, drain_queue, enqueue_write,
    vector_queue_depth,
};
use mycelium_store::{ConceptStore, ConfigStore, Store};
use uuid::Uuid;

/// Serializes all tests in this binary around the shared queue/statics
/// and the embeddings health machine.
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Restores the process-global embeddings health machine and its
/// sentinels on drop, so a failed assertion's Red (or a leaked dims
/// stamp) cannot perturb a sibling test. Declared AFTER the TEST_LOCK
/// guard so the restore happens while the lock is held.
struct EmbDefaults;

impl Drop for EmbDefaults {
    fn drop(&mut self) {
        EMBEDDING_HEALTH.reset_for_test();
        EMB_LAST_SUCCESS.store(0, Ordering::Relaxed);
        EMBEDDING_DIMS.store(0, Ordering::Relaxed);
    }
}

/// The mock's scripted vector dimensions (8 — the backlog tests' era-1
/// learned dims; EMBEDDING_DIMS is stamped to it by every ok embed).
const DIMS: usize = 8;
/// Model name for the one-era tests.
const MODEL: &str = "vec-m";
/// Era models for the model-swap tests (era 1 → era 2 rewrite).
const MODEL1: &str = "vec-m1";
const MODEL2: &str = "vec-m2";
/// Test tick: far below the production `VECTOR_TICK_SECS` (5 s).
const TICK_MS: u64 = 100;

// ---------- scripted mock embeddings endpoint ----------

type Shared = Arc<tokio::sync::Mutex<MockState>>;

/// Scripted response spec.
#[derive(Debug, Clone)]
enum Resp {
    /// A canned status + body string (the failure eras: 503s etc.).
    Status(u16, String),
    /// A 200 shaped by the REQUEST itself: one deterministic
    /// `dims`-dimension vector per input element. The script cannot know
    /// a batch's size ahead of serving (a 2-item drain batch vs the
    /// backlog tests' 32+8 split), so the healthy entry takes the
    /// request's input length at serve time; values are constant float
    /// vectors — ranking is later tasks' concern, freshness is pinned
    /// here.
    Vecs(usize),
}

#[derive(Default)]
struct MockState {
    script: Vec<Resp>,
    requests: Vec<serde_json::Value>,
}

/// Mock embeddings endpoint (the config row's URL contract: server
/// ROOT, no `/v1` — the client appends the path). Responses serve LIFO
/// (the mock_llm semantics); a test ADDS entries for later pops by
/// pushing. An exhausted script answers 500 — itself backend-down, so
/// an unexpected extra attempt cannot masquerade as healthy.
async fn mock_embeddings(script: Vec<Resp>) -> (String, Shared) {
    let state: Shared = Arc::new(tokio::sync::Mutex::new(MockState {
        script,
        requests: Vec::new(),
    }));
    let handler_state = Arc::clone(&state);
    let app = axum::Router::new().route(
        "/v1/embeddings",
        axum::routing::post(move |body: String| {
            let state = handler_state;
            async move {
                let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let n = parsed["input"].as_array().map_or(0, Vec::len);
                let mut guard = state.lock().await;
                guard.requests.push(parsed);
                match guard.script.pop() {
                    Some(Resp::Vecs(dims)) => {
                        let data: Vec<serde_json::Value> = (0..n)
                            .map(|i| {
                                serde_json::json!({
                                    "index": i,
                                    "embedding": vec![0.25f64; dims],
                                })
                            })
                            .collect();
                        axum::Json(serde_json::json!({ "data": data })).into_response()
                    }
                    Some(Resp::Status(code, body)) => (
                        axum::http::StatusCode::from_u16(code).expect("valid status"),
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        body,
                    )
                        .into_response(),
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
    (format!("http://{addr}"), state)
}

fn status_resp(code: u16, body: &str) -> Resp {
    Resp::Status(code, body.to_string())
}

fn vec_resp(dims: usize) -> Resp {
    Resp::Vecs(dims)
}

// ---------- poll helper ----------

/// Poll until `cond` holds (re-evaled every step), bounded — never a
/// fixed sleep as the assertion.
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
    recovery: Arc<MasterKeyRecovery>,
    user: Uuid,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).await.unwrap());
    let config = Arc::new(ConfigStore::new(store.pool().clone()));
    let service_key =
        Arc::new(mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap());
    let master = generate_master_key();
    let recovery = stub_recovery(master.clone());
    let user = Uuid::new_v4();
    // The user row must EXIST: the worker's boot backlog enumerates
    // `SELECT id FROM users` below the auth layer.
    seed_user(store.as_ref(), user, "vec-user").await;
    Env {
        _dir: dir,
        store,
        config,
        service_key,
        master,
        recovery,
        user,
    }
}

impl Env {
    /// The test user's (ConceptStore, VectorStore) — one master key
    /// derives both stores' IndexKeys (same scope identity, so rows the
    /// worker upserts through the stub's recovered key are exactly the
    /// rows the test reads back through `master`).
    fn user_stores(&self) -> (ConceptStore<'_>, VectorStore<'_>) {
        (
            ConceptStore::for_user(self.store.as_ref(), self.user, self.master.clone()),
            VectorStore::for_user(self.store.pool(), self.user, &self.master),
        )
    }
}

/// A user row to satisfy `SELECT id FROM users` on the boot-backlog
/// enumeration (the queue-worker sweep's below-auth-layer shape).
async fn seed_user(store: &Store, id: Uuid, name: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO users (id, username, email, role, auth_provider, password_hash, sealed_master_key, must_change_password, totp_secret, created_at, updated_at)
         VALUES (?, ?, ?, 'user', 'local', NULL, 'placeholder', 0, NULL, ?, ?)",
    )
    .bind(id.to_string())
    .bind(name)
    .bind(format!("{name}@t"))
    .bind(&now)
    .bind(&now)
    .execute(store.pool())
    .await
    .unwrap();
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

/// Sealed-`embeddings`-row writer (the admin page's own storage shape).
async fn write_emb_row(
    config: &ConfigStore,
    service_key: &ServiceKey,
    base_url: &str,
    model: &str,
) {
    let sealed = mycelium_store::config::seal_config(
        service_key,
        "embeddings",
        &mycelium_librarian::EmbeddingsConfig {
            base_url: base_url.to_string(),
            model: model.to_string(),
            ..Default::default()
        },
    );
    config.set("embeddings", &sealed).await.unwrap();
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

/// A worker over `env`'s seams, spawned on a fresh token at `TICK_MS`.
fn spawn_worker(
    env: &Env,
    tick: Duration,
) -> (
    tokio_util::sync::CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let worker = Arc::new(VectorWorker::new(
        Arc::clone(&env.store),
        Arc::clone(&env.config),
        Arc::clone(&env.service_key),
        Arc::clone(&env.recovery),
    ));
    let shutdown = tokio_util::sync::CancellationToken::new();
    let handle = worker.spawn_with_tick(shutdown.clone(), tick);
    (shutdown, handle)
}

/// The machine's Red reached via the passive hooks implies
/// `EMBED_ATTEMPTS` dead batch attempts already ran inside a drain — a
/// test's Red-first poll can never fire on a pre-worker idle state (the
/// reset machine is fresh Green), and recovery assertions ordered after
/// it cannot race a tick that has not yet red.
async fn wait_red() {
    wait_until(
        "three dead batch attempts drive the machine Red",
        || async { EMBEDDING_HEALTH.state() == BackendState::Red },
    )
    .await;
}

/// Put 2 concepts → the put hooks enqueue exactly 2 → the worker drains
/// them through the mock into two fresh rows under the row's model; the
/// processed counter advances by exactly 2; the batch rode the model
/// name; and the drained upserts INVALIDATED the scope — a cache
/// snapshot built before the worker saw the rows cannot outlive the
/// worker's write (the controller's invalidation contract).
#[tokio::test]
async fn worker_drains_the_queue_into_vector_rows() {
    let _lock = TEST_LOCK.lock().await;
    let _emb = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    let (url, shared) = mock_embeddings(vec![vec_resp(DIMS)]).await;
    let env = env().await;
    write_emb_row(&env.config, &env.service_key, &url, MODEL).await;
    let (cs, vs) = env.user_stores();

    cs.put_batch(&[
        concept("/a.md", "A", "text a"),
        concept("/b.md", "B", "text b"),
    ])
    .await
    .unwrap();
    assert_eq!(
        vector_queue_depth(),
        2,
        "each put's hook enqueued exactly once"
    );
    // The invalidation pin's BEFORE side: the (put-invalidated) cache
    // rebuilt here is an empty snapshot; only the worker's OWN
    // invalidation can make the next read see the upserts.
    let before = vs.cache_scope().await.expect("cache built");
    assert!(before.rows.is_empty(), "no vector rows before the worker");

    let processed_before = VECTOR_PROCESSED_TOTAL.load(Ordering::Relaxed);
    let (shutdown, handle) = spawn_worker(&env, Duration::from_millis(TICK_MS));

    let (token_a, token_b) = (vs.path_token_of("/a.md"), vs.path_token_of("/b.md"));
    wait_until("both rows fresh under the scripted model", || async {
        vs.load_rows()
            .await
            .map(|rows| rows.len() == 2 && rows.iter().all(|r| !r.stale && r.model == MODEL))
            .unwrap_or(false)
    })
    .await;
    let tokens: Vec<String> = vs
        .load_rows()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.path_token)
        .collect();
    assert!(
        tokens.contains(&token_a) && tokens.contains(&token_b),
        "every drained token got its row: {tokens:?}"
    );
    assert_eq!(
        VECTOR_PROCESSED_TOTAL.load(Ordering::Relaxed) - processed_before,
        2,
        "the counter advances once per processed concept"
    );

    // The wire protocol: the batch carried the row's model name.
    let requests = shared.lock().await.requests.clone();
    assert_eq!(requests.len(), 1, "both concepts ride one cfg.batch batch");
    assert_eq!(requests[0]["model"], MODEL);
    assert_eq!(requests[0]["input"].as_array().expect("input").len(), 2);

    // The invalidation pin's AFTER side: this read must see the
    // upserts — they land through the cache's own keys only because the
    // drain dropped the pre-worker snapshot.
    let snap = vs.cache_scope().await.expect("rebuilt cache");
    assert_eq!(
        snap.rows.len(),
        2,
        "the drain's post-upsert invalidation rebuilt the cached scope"
    );

    shutdown.cancel();
    handle.await.unwrap();
}

/// A dead backend: tick 1's boot backlog re-enqueues the drained era's
/// two concepts (the test started with an emptied queue, so tick 1's
/// drain was a no-op), tick 2 drains them through three failing batch
/// attempts (the machine's Red threshold) — and the items PARK: they
/// re-enter the queue (park-and-resume; a mid-drain Red must not zero a
/// boot's backlog). Then a simulated probe recovery (fresh Green
/// machine), a healthy mock and a new write: a later tick drains the
/// parked pair and the new write into three fresh rows — no restart, no
/// second boot (spec §3.3: the red pause pauses, it does not drop).
#[tokio::test]
async fn worker_pauses_on_red_and_drains_after_recovery() {
    let _lock = TEST_LOCK.lock().await;
    let _emb = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    let (url, shared) = mock_embeddings(vec![status_resp(503, "down"); 3]).await;
    let env = env().await;
    write_emb_row(&env.config, &env.service_key, &url, MODEL).await;
    let (cs, vs) = env.user_stores();

    cs.put_batch(&[
        concept("/a.md", "A", "text a"),
        concept("/b.md", "B", "text b"),
    ])
    .await
    .unwrap();
    // The pre-worker era: the items exist on disk, the queue is empty.
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    assert_eq!(vector_queue_depth(), 0);

    let (shutdown, handle) = spawn_worker(&env, Duration::from_millis(TICK_MS));

    wait_red().await;
    wait_until(
        "the dead drain's items parked back into the queue",
        || async { vector_queue_depth() == 2 },
    )
    .await;
    let rows = vs.load_rows().await.unwrap();
    assert!(
        rows.is_empty(),
        "no rows were written from the red era: {rows:?}"
    );

    // Simulated recovery (the probe's ok feed) + a healthy era and a new
    // write: the next green tick's drain rides the parked pair and the
    // new item (three vec entries absorb whichever batch shapes the
    // drain's timing produces).
    shared.lock().await.script.push(vec_resp(DIMS));
    shared.lock().await.script.push(vec_resp(DIMS));
    shared.lock().await.script.push(vec_resp(DIMS));
    EMBEDDING_HEALTH.reset_for_test();
    cs.put(&concept("/c.md", "C", "text c")).await.unwrap();

    let want = [
        vs.path_token_of("/a.md"),
        vs.path_token_of("/b.md"),
        vs.path_token_of("/c.md"),
    ];
    wait_until(
        "the recovered era drained the parked pair and the new write",
        || async {
            vs.load_rows()
                .await
                .map(|rows| {
                    rows.len() == 3
                        && rows.iter().all(|r| !r.stale && r.model == MODEL)
                        && rows.iter().all(|r| want.contains(&r.path_token))
                })
                .unwrap_or(false)
        },
    )
    .await;
    assert_eq!(
        vector_queue_depth(),
        0,
        "the park drained empty after recovery"
    );

    shutdown.cancel();
    handle.await.unwrap();
}

/// The boot backlog concludes DESPITE a backend-cold window (the
/// production 2026-11-03 incident twice zeroed a whole backlog against a
/// boot-time cold backend: the backlog fired, the drain ran into the
/// dead backend and DROPPED everything, and the backlog resumed only at
/// the next boot). Here tick 1's backlog enqueues 40 concepts, tick 2's
/// drain burns the first batch's attempts against the dead mock and the
/// machine goes Red — the whole 40 PARK (they re-enter the queue) — and
/// the recovery's first green tick drains every one of them into rows.
/// No restart, no second boot, no lost era.
#[tokio::test]
async fn boot_backlog_survives_a_mid_drain_red() {
    let _lock = TEST_LOCK.lock().await;
    let _emb = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    let (url, shared) = mock_embeddings(vec![status_resp(503, "down"); 3]).await;
    let env = env().await;
    write_emb_row(&env.config, &env.service_key, &url, MODEL).await;
    let (cs, vs) = env.user_stores();

    let concepts: Vec<Concept> = (0..40)
        .map(|i| concept(&format!("/r-{i:02}.md"), &format!("R{i}"), "body"))
        .collect();
    cs.put_batch(&concepts).await.unwrap();
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    assert_eq!(
        vector_queue_depth(),
        0,
        "the pre-worker era's queue is empty at spawn"
    );

    let processed_before = VECTOR_PROCESSED_TOTAL.load(Ordering::Relaxed);
    let (shutdown, handle) = spawn_worker(&env, Duration::from_millis(TICK_MS));

    // The Red gate makes the order deterministic: the machine can only
    // reach Red through the drain's dead batch attempts, so recovery is
    // forced to run AFTER the red drain (a depth-only wait could race a
    // backlog enqueue against the drain that consumed it).
    wait_red().await;
    // Tick 1 backlogs (its drain is a no-op), tick 2's drain reds on the
    // first batch: the whole backlog parks back into the queue.
    wait_until(
        "the mid-drain red parked the whole boot backlog",
        || async { vector_queue_depth() == 40 },
    )
    .await;
    let rows = vs.load_rows().await.unwrap();
    assert!(rows.is_empty(), "the red era wrote no rows: {rows:?}");

    // Simulated recovery: the parked backlog completes in this process.
    shared.lock().await.script.push(vec_resp(DIMS));
    shared.lock().await.script.push(vec_resp(DIMS));
    shared.lock().await.script.push(vec_resp(DIMS));
    EMBEDDING_HEALTH.reset_for_test();
    wait_until("the recovered ticks completed the boot backlog", || async {
        vs.load_rows()
            .await
            .map(|rows| rows.len() == 40 && rows.iter().all(|r| !r.stale && r.model == MODEL))
            .unwrap_or(false)
    })
    .await;
    assert_eq!(
        vector_queue_depth(),
        0,
        "the park drained empty after the backlog completed"
    );
    assert_eq!(
        VECTOR_PROCESSED_TOTAL.load(Ordering::Relaxed) - processed_before,
        40,
        "every backloged concept upserted exactly once"
    );

    shutdown.cancel();
    handle.await.unwrap();
}

/// The boot backlog flag defers through Red ticks: the tick's Red pause
/// returns BEFORE the flag is consumed, so a backend that is already Red
/// at the first present-config tick no longer costs the boot's backlog
/// (the flag used to be consumed even by Red ticks — a red at boot meant
/// the pre-existing corpus never backfilled without a restart). The
/// recovery's first green tick fires the flag, enqueues the backlog and
/// the next tick embeds it.
#[tokio::test]
async fn boot_backlog_defers_through_red_ticks_and_fires_on_recovery() {
    let _lock = TEST_LOCK.lock().await;
    let _emb = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    let (url, shared) = mock_embeddings(vec![vec_resp(DIMS); 3]).await;
    let env = env().await;
    write_emb_row(&env.config, &env.service_key, &url, MODEL).await;
    let (cs, vs) = env.user_stores();

    let concepts: Vec<Concept> = (0..40)
        .map(|i| concept(&format!("/d-{i:02}.md"), &format!("D{i}"), "body"))
        .collect();
    cs.put_batch(&concepts).await.unwrap();
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    assert_eq!(
        vector_queue_depth(),
        0,
        "the pre-worker era's queue is empty at spawn"
    );

    // Force the machine Red BEFORE the worker spawns (the configured
    // machine counts consecutive failures; three go Red; a note under
    // Disabled is ignored, so the configured lift comes first).
    EMBEDDING_HEALTH.note_configured();
    for _ in 0..3 {
        EMBEDDING_HEALTH.note_failure();
    }
    assert_eq!(EMBEDDING_HEALTH.state(), BackendState::Red);

    let (shutdown, handle) = spawn_worker(&env, Duration::from_millis(TICK_MS));

    // Several red ticks: the flag stays held, nothing enqueues, the
    // mock never sees a request.
    tokio::time::sleep(Duration::from_millis(TICK_MS * 4)).await;
    assert_eq!(
        EMBEDDING_HEALTH.state(),
        BackendState::Red,
        "red ticks make no embed attempts, the machine stays Red"
    );
    assert_eq!(
        vector_queue_depth(),
        0,
        "red ticks enqueue no backlog (the flag is still held)"
    );

    // Simulated recovery: the first green tick fires the flag; the next
    // tick drains the backlog into rows. No restart.
    shared.lock().await.script.push(vec_resp(DIMS));
    shared.lock().await.script.push(vec_resp(DIMS));
    shared.lock().await.script.push(vec_resp(DIMS));
    EMBEDDING_HEALTH.reset_for_test();
    wait_until("the recovery's backlog fired and completed", || async {
        vs.load_rows()
            .await
            .map(|rows| rows.len() == 40 && rows.iter().all(|r| !r.stale && r.model == MODEL))
            .unwrap_or(false)
    })
    .await;
    assert_eq!(
        vector_queue_depth(),
        0,
        "the backlog drained empty after firing"
    );

    shutdown.cancel();
    handle.await.unwrap();
}

/// Boot backlog + model-swap steady rebuild: the pre-spawn era's 40
/// concepts (queue drained EMPTY — writes that happened before a worker
/// ever saw them) are re-enqueued by the FIRST tick's backlog at the
/// END of the tick and fully drained (queue cap 512 > 40 — the bound
/// exercised via the capacity math; the OVER-cap case is pinned by
/// vectors_store_integration's queue_is_bounded_and_deduped). Then a
/// model swap (a rewritten config row + a forced DIMS stamp) marks the
/// whole corpus stale and the bounded rebuild (256 rows/tick ≥ 40 here)
/// re-embeds under the new model.
#[tokio::test]
async fn boot_backlog_is_enqueued_bounded_and_resumed() {
    let _lock = TEST_LOCK.lock().await;
    let _emb = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    let (url, _shared) = mock_embeddings(vec![vec_resp(DIMS); 6]).await;
    let env = env().await;
    write_emb_row(&env.config, &env.service_key, &url, MODEL1).await;
    let (cs, vs) = env.user_stores();

    let concepts: Vec<Concept> = (0..40)
        .map(|i| concept(&format!("/n-{i:02}.md"), &format!("N{i}"), "body"))
        .collect();
    cs.put_batch(&concepts).await.unwrap();
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    assert_eq!(
        vector_queue_depth(),
        0,
        "the pre-worker era's queue is empty at spawn"
    );

    let (shutdown, handle) = spawn_worker(&env, Duration::from_millis(TICK_MS));

    wait_until("every backloged concept has a fresh row", || async {
        vs.load_rows()
            .await
            .map(|rows| rows.len() == 40 && rows.iter().all(|r| !r.stale && r.model == MODEL1))
            .unwrap_or(false)
    })
    .await;

    // Model swap: the row rewrite + (belt) a forced DIMS stamp — the
    // next tick's prologue marks the whole corpus stale and the bounded
    // rebuild re-embeds under the new model.
    write_emb_row(&env.config, &env.service_key, &url, MODEL2).await;
    EMBEDDING_DIMS.store(DIMS as u32, Ordering::Relaxed);
    wait_until(
        "the stale corpus was rebuilt under the new model",
        || async {
            vs.load_rows()
                .await
                .map(|rows| rows.len() == 40 && rows.iter().all(|r| !r.stale && r.model == MODEL2))
                .unwrap_or(false)
        },
    )
    .await;

    shutdown.cancel();
    handle.await.unwrap();
}

/// The boot backlog survives ticks that exit on the ABSENT embeddings
/// row (final-review Important 1 — the fresh-deployment adoption path):
/// the pre-spawn era's 40 concepts (queue drained EMPTY) stay
/// un-backfilled across several absent-row ticks — the backlog flag is
/// NOT consumed by them — and the row being SAVED post-boot (the
/// production seal helper, exactly `admin_save_embeddings`' storage
/// shape) makes the next tick enqueue the backlog and embed it. No
/// restart anywhere in the test.
#[tokio::test]
async fn boot_backlog_defers_to_a_post_boot_config_save() {
    let _lock = TEST_LOCK.lock().await;
    let _emb = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    let (url, _shared) = mock_embeddings(vec![vec_resp(DIMS); 6]).await;
    let env = env().await;
    // NO embeddings row yet — the fresh-deployment posture.
    let (cs, vs) = env.user_stores();

    let concepts: Vec<Concept> = (0..40)
        .map(|i| concept(&format!("/n-{i:02}.md"), &format!("N{i}"), "body"))
        .collect();
    cs.put_batch(&concepts).await.unwrap();
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    assert_eq!(
        vector_queue_depth(),
        0,
        "the pre-worker era's queue is empty at spawn"
    );

    let (shutdown, handle) = spawn_worker(&env, Duration::from_millis(TICK_MS));

    // Several ticks at the absent posture: the flag survives, nothing
    // enqueues, no rows appear (each absent tick ends at the config
    // check; TICK_MS×8 gives well past the first-tick interval).
    tokio::time::sleep(Duration::from_millis(TICK_MS * 8)).await;
    assert_eq!(
        vs.load_rows().await.unwrap().len(),
        0,
        "the absent row's ticks never created vector rows"
    );
    assert_eq!(vector_queue_depth(), 0, "the backlog flag is still held");
    assert_eq!(
        EMBEDDING_HEALTH.state(),
        BackendState::Disabled,
        "the absent row reads as Disabled"
    );

    // The operator saves the row post-boot (production seal shape)…
    write_emb_row(&env.config, &env.service_key, &url, MODEL).await;
    // …and the FIRST present-config tick backfills the whole corpus —
    // no restart: the backlog enqueues at that tick, embeds on the next.
    wait_until("the post-save tick backfilled the whole corpus", || async {
        vs.load_rows()
            .await
            .map(|rows| rows.len() == 40 && rows.iter().all(|r| !r.stale && r.model == MODEL))
            .unwrap_or(false)
    })
    .await;

    shutdown.cancel();
    handle.await.unwrap();
}

/// A deleted concept's stale row is never rebuilt (spec §3.3 Review
/// Focus 3): the delete hook removes the row directly, and the stale
/// enumeration keys on the search index's paths — a gone concept is in
/// neither, so no belt or backlog phase resurrects it.
#[tokio::test]
async fn a_deleted_concepts_stale_row_is_not_rebuilt() {
    let _lock = TEST_LOCK.lock().await;
    let _emb = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    let (url, _shared) = mock_embeddings(vec![vec_resp(DIMS); 4]).await;
    let env = env().await;
    write_emb_row(&env.config, &env.service_key, &url, MODEL1).await;
    let (cs, vs) = env.user_stores();
    cs.put(&concept("/a.md", "A", "text a")).await.unwrap();

    let (shutdown, handle) = spawn_worker(&env, Duration::from_millis(TICK_MS));

    // Wait for the deleted concept's era to be FULLY done (its fresh
    // row) — the delete then races no in-flight embed.
    wait_until("the row was embedded fresh under era one", || async {
        vs.load_rows()
            .await
            .map(|rows| rows.len() == 1 && rows[0].model == MODEL1 && !rows[0].stale)
            .unwrap_or(false)
    })
    .await;

    // Model swap → the bounded rebuild re-embeds the row under the new
    // model; the delete only rides after the rebuild is COMPLETE, so
    // nothing is ever in flight for the deleted path.
    write_emb_row(&env.config, &env.service_key, &url, MODEL2).await;
    EMBEDDING_DIMS.store(DIMS as u32, Ordering::Relaxed);
    wait_until(
        "the swap era rebuilt the row under the new model",
        || async {
            vs.load_rows()
                .await
                .map(|rows| rows.len() == 1 && rows[0].model == MODEL2 && !rows[0].stale)
                .unwrap_or(false)
        },
    )
    .await;
    cs.delete("/a.md").await.unwrap();
    assert!(
        vs.load_rows().await.unwrap().is_empty(),
        "the delete hook removed the row directly"
    );
    // Give later ticks a chance to resurrect the row (they must not —
    // the swap was already consumed, and the boot backlog is
    // first-tick-only).
    tokio::time::sleep(Duration::from_millis(TICK_MS * 3)).await;
    assert!(
        vs.load_rows().await.unwrap().is_empty(),
        "a deleted concept's stale row is never rebuilt"
    );
    assert_eq!(
        vector_queue_depth(),
        0,
        "no resurrection items queued either"
    );

    shutdown.cancel();
    handle.await.unwrap();
}

/// The belt for a drained item whose concept no longer exists: the
/// registry lookup misses the token, and the belt removes the row BY
/// TOKEN (the drained item carries only the one-way token). The
/// surviving concept drains normally beside it.
#[tokio::test]
async fn a_drained_item_for_a_deleted_concept_deletes_by_token() {
    let _lock = TEST_LOCK.lock().await;
    let _emb = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    let (url, _shared) = mock_embeddings(vec![vec_resp(DIMS)]).await;
    let env = env().await;
    write_emb_row(&env.config, &env.service_key, &url, MODEL).await;
    let (cs, vs) = env.user_stores();

    cs.put(&concept("/a.md", "A", "text a")).await.unwrap();
    cs.put(&concept("/b.md", "B", "text b")).await.unwrap();
    // Delete AFTER the put: the queued item stays (the delete hook
    // removes rows directly, it does not dequeue), the registry row is
    // already gone when the drain resolves it.
    cs.delete("/b.md").await.unwrap();
    assert_eq!(
        vector_queue_depth(),
        2,
        "the delete hook does not dequeue put-era items"
    );

    let (shutdown, handle) = spawn_worker(&env, Duration::from_millis(TICK_MS));

    let want_row = [vs.path_token_of("/a.md")];
    wait_until("the surviving concept's row is fresh", || async {
        vs.load_rows()
            .await
            .map(|rows| {
                rows.len() == 1
                    && want_row.contains(&rows[0].path_token)
                    && !rows[0].stale
                    && rows[0].model == MODEL
            })
            .unwrap_or(false)
    })
    .await;
    assert_eq!(
        vector_queue_depth(),
        0,
        "the belt consumed the deleted half's item too"
    );

    shutdown.cancel();
    handle.await.unwrap();
}

// ---------- dead-concept sweeps (the ghost-residue remediation) ----------

/// The boot backlog sweeps registry-dead `search_docs` rows instead of
/// re-enqueueing them forever: a concept whose FTS row outlived its
/// registry row (historical delete-path residue) is enumerated by EVERY
/// boot's `backlog_paths` and belted out silently — today. With the
/// sweep, that path is REMOVED at the backlog (lexical index row plus
/// any vector row) and never enqueued; the two live concepts still
/// embed, and exactly two embeds happen — the ghost never does.
#[tokio::test]
async fn backlog_sweeps_registry_dead_index_rows() {
    let _lock = TEST_LOCK.lock().await;
    let _emb = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    // One vec script entry serves the live pair's batch — a third embed
    // (the ghost ever embedding) would exhaust the script into 500s.
    let (url, _shared) = mock_embeddings(vec![vec_resp(DIMS)]).await;
    let env = env().await;
    write_emb_row(&env.config, &env.service_key, &url, MODEL).await;
    let (cs, vs) = env.user_stores();
    let scope = format!("user:{}", env.user);

    cs.put_batch(&[
        concept("/a.md", "A", "text a"),
        concept("/b.md", "B", "text b"),
    ])
    .await
    .unwrap();
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    assert_eq!(vector_queue_depth(), 0, "queue empty at spawn");

    // The ghost: a search_docs row whose concept the registry never
    // held. A zero payload is enough — the enumeration reads only the
    // path (the row is dead by construction; its AEAD payload is not
    // decryptable and never read).
    sqlx::query(
        "INSERT INTO search_docs (scope, concept_path, payload, length) VALUES (?, '/ghost.md', x'00', 4)",
    )
    .bind(&scope)
    .execute(env.store.pool())
    .await
    .unwrap();

    let processed_before = VECTOR_PROCESSED_TOTAL.load(Ordering::Relaxed);
    let (shutdown, handle) = spawn_worker(&env, Duration::from_millis(TICK_MS));

    wait_until("the ghost's search_docs row was swept", || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM search_docs WHERE scope = ? AND concept_path = '/ghost.md'",
        )
        .bind(&scope)
        .fetch_one(env.store.pool())
        .await
        .unwrap_or(1)
            == 0
    })
    .await;
    wait_until("the live concepts embedded fresh", || async {
        vs.load_rows()
            .await
            .map(|rows| rows.len() == 2 && rows.iter().all(|r| !r.stale && r.model == MODEL))
            .unwrap_or(false)
    })
    .await;
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    assert_eq!(
        vector_queue_depth(),
        0,
        "the ghost was never enqueued — the cycle is broken"
    );
    assert_eq!(
        VECTOR_PROCESSED_TOTAL.load(Ordering::Relaxed) - processed_before,
        2,
        "exactly the two live concepts embedded"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM search_tokens WHERE scope = ? AND concept_path = '/ghost.md'",
        )
        .bind(&scope)
        .fetch_one(env.store.pool())
        .await
        .unwrap_or(1),
        0,
        "the ghost's postings are gone too — nothing searchable remains"
    );

    shutdown.cancel();
    handle.await.unwrap();
}

/// The stale-rebuild site completes the same sweep: a concept whose
/// vector row went stale AND whose file is gone — while the FTS row
/// outlived the delete (the historical residue: today's delete path
/// removes registry → index → file → vector row in order) — is swept,
/// the lexical index row AND the stale vector row, rather than belted
/// token-only; the stale path then cannot re-enumerate next boot.
#[tokio::test]
async fn stale_rebuild_sweeps_registry_dead_stale_rows() {
    let _lock = TEST_LOCK.lock().await;
    let _emb = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    // Two vecs: /c's initial backlog-then-drain embed, then the live
    // pair's; a third (the ghost re-embedding) would exhaust the script.
    let (url, _shared) = mock_embeddings(vec![vec_resp(DIMS), vec_resp(DIMS)]).await;
    let env = env().await;
    write_emb_row(&env.config, &env.service_key, &url, MODEL).await;
    let (cs, vs) = env.user_stores();
    let scope = format!("user:{}", env.user);
    let token = vs.path_token_of("/c.md");

    cs.put(&concept("/c.md", "C", "text c")).await.unwrap();
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    let (shutdown, handle) = spawn_worker(&env, Duration::from_millis(TICK_MS));
    wait_until("c's row landed fresh", || async {
        vs.load_rows()
            .await
            .map(|rows| rows.len() == 1 && !rows[0].stale && rows[0].model == MODEL)
            .unwrap_or(false)
    })
    .await;
    // Tick 1's boot backlog enqueued c (no row yet) and is consumed;
    // the corpus is current.
    let processed_before = VECTOR_PROCESSED_TOTAL.load(Ordering::Relaxed);

    // The surgery: a FULL delete (file + registry + rows) — the
    // concept is truly gone — followed by the residue re-inserted by
    // hand: the FTS doc row, one posting row, and a stale vector row
    // (the shape the historical bug left; the garbage vec participates
    // in backlog_paths by metadata only and is never decoded — the
    // sweep deletes it before any load_rows read).
    cs.delete("/c.md").await.unwrap();
    sqlx::query(
        "INSERT INTO search_docs (scope, concept_path, payload, length) VALUES (?, '/c.md', x'00', 4)",
    )
    .bind(&scope)
    .execute(env.store.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO search_tokens (scope, token, concept_path, tf) VALUES (?, 'dead-token', '/c.md', 1)",
    )
    .bind(&scope)
    .execute(env.store.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO search_vectors (scope_id, path_token, model, dims, stale, vec, updated_at) \
         VALUES (?, ?, ?, ?, 1, x'00', ?)",
    )
    .bind(&scope)
    .bind(&token)
    .bind(MODEL)
    .bind(DIMS as i64)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(env.store.pool())
    .await
    .unwrap();
    assert!(
        cs.get("/c.md").await.is_err(),
        "surgery: the file is gone (cs.get must now miss)"
    );
    assert!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM search_docs WHERE scope = ? AND concept_path = '/c.md'",
        )
        .bind(&scope)
        .fetch_one(env.store.pool())
        .await
        .unwrap_or(0)
            == 1,
        "surgery: the FTS row outlives the concept (the ghost shape)"
    );

    // Two live concepts ride the NEXT tick's drain; the stale rebuild
    // meets the ghost in the same tick and must sweep it.
    cs.put_batch(&[
        concept("/a.md", "A", "text a"),
        concept("/b.md", "B", "text b"),
    ])
    .await
    .unwrap();

    wait_until("the ghost was swept out of every row", || async {
        let docs = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM search_docs WHERE scope = ? AND concept_path = '/c.md'",
        )
        .bind(&scope)
        .fetch_one(env.store.pool())
        .await
        .unwrap_or(1);
        let vecs = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM search_vectors WHERE scope_id = ? AND path_token = ?",
        )
        .bind(&scope)
        .bind(&token)
        .fetch_one(env.store.pool())
        .await
        .unwrap_or(1);
        docs == 0 && vecs == 0
    })
    .await;
    wait_until("the live concepts embedded fresh", || async {
        vs.load_rows()
            .await
            .map(|rows| rows.len() == 2 && rows.iter().all(|r| !r.stale && r.model == MODEL))
            .unwrap_or(false)
    })
    .await;
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    assert_eq!(
        vector_queue_depth(),
        0,
        "nothing of the ghost remains queued"
    );
    assert_eq!(
        VECTOR_PROCESSED_TOTAL.load(Ordering::Relaxed) - processed_before,
        2,
        "the stale ghost never re-embedded"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM search_tokens WHERE scope = ? AND concept_path = '/c.md'",
        )
        .bind(&scope)
        .fetch_one(env.store.pool())
        .await
        .unwrap_or(1),
        0,
        "the ghost's posting row is gone too"
    );

    shutdown.cancel();
    handle.await.unwrap();
}

// ---------- the belt's debug-log observability ----------

/// A fmt-log-capture writer: every formatted line is appended to a
/// shared buffer. The worker task runs on the test's current-thread
/// runtime, so a per-thread default subscriber captures its events.
struct CaptureWriter(Arc<std::sync::Mutex<Vec<String>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(buf).to_string());
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for CaptureWriter {
    type Writer = CaptureWriter;
    fn make_writer(&self) -> Self::Writer {
        CaptureWriter(Arc::clone(&self.0))
    }
}

/// A drained queue token no registry concept resolves to goes to the
/// belt — historically a silent continue (the 2026-11-03 stream's
/// "deserves at least a debug log" gap). The belt's success path must
/// emit a DEBUG line carrying the dead token, so any belt out is
/// diagnosable from the logs.
#[tokio::test]
async fn belt_of_a_registry_dead_token_logs_at_debug() {
    let _lock = TEST_LOCK.lock().await;
    let _emb = EmbDefaults;
    drain_queue(VECTOR_QUEUE_CAPACITY);

    let mock = mock_embeddings(vec![vec_resp(DIMS)]).await;
    let env = env().await;
    write_emb_row(&env.config, &env.service_key, &mock.0, MODEL).await;

    let (cs, vs) = env.user_stores();
    cs.put(&concept("/a.md", "Live", "live body"))
        .await
        .unwrap();
    let scope = vs.scope_id().to_string();

    let captured = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(CaptureWriter(Arc::clone(&captured)))
            .finish(),
    );

    let (shutdown, handle) = spawn_worker(&env, Duration::from_millis(TICK_MS));
    let processed_before = VECTOR_PROCESSED_TOTAL.load(Ordering::Relaxed);
    wait_until("the live concept embedded", || async {
        VECTOR_PROCESSED_TOTAL.load(Ordering::Relaxed) - processed_before == 1
    })
    .await;

    // A token whose concept never existed: the registry maps nothing,
    // no vector row exists, the belt removes nothing — but its run
    // must be visible.
    let dead_token = vs.path_token_of("/never-lived.md");
    enqueue_write(&scope, &dead_token);
    let captured_for_wait = Arc::clone(&captured);
    let dead_for_wait = dead_token.clone();
    wait_until("the belt's debug line captured", || async {
        captured_for_wait
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("vector belt:") && l.contains(&dead_for_wait))
    })
    .await;

    let belt_lines: Vec<String> = captured
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains("vector belt:"))
        .cloned()
        .collect();
    assert!(
        belt_lines
            .iter()
            .all(|l| l.contains(&dead_token) && l.contains(&scope)),
        "each belt line must carry the dead token and scope; captured: {:?}",
        captured.lock().unwrap(),
    );

    shutdown.cancel();
    handle.await.unwrap();
}
