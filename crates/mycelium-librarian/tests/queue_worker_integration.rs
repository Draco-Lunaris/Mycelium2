//! Mutation-queue drain worker integration tests: temp-dir store, mock
//! OpenAI-compatible LLM (copied from agent_integration.rs — the mock
//! helpers are file-local per test binary), stub master-key recovery.
//!
//! `agent::LLM_RUNS` is process-global with exactly 2 permits, so every
//! test in this file serializes on TEST_LOCK (T5.3 deliberately
//! starves all permits).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::response::IntoResponse;
use mycelium_core::concept::{Concept, Frontmatter};
use mycelium_core::search::SearchQuery;
use mycelium_crypto::keys::MasterKey;
use mycelium_crypto::store_keys::FileKeys;
use mycelium_librarian::agent;
use mycelium_librarian::fallback::derive_title;
use mycelium_librarian::llm::{LlmClient, LlmConfig};
use mycelium_librarian::queue_worker::{MasterKeyRecovery, QueueLimits, QueueWorker};
use mycelium_store::mutation_queue::{self, MutationPayload};
use mycelium_store::{ConceptStore, ConfigStore, FileRepo, QueueStatus, QueueTool, Store};

/// Serializes all tests in this binary around the shared LLM semaphore.
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------- mock LLM (copied from agent_integration.rs) ----------

/// A minimal OpenAI-compatible mock: serves scripted responses in
/// order (LIFO). Captures every request body for protocol assertions.
type Shared = Arc<tokio::sync::Mutex<MockState>>;
struct MockState {
    responses: Vec<serde_json::Value>,
    requests: Vec<serde_json::Value>,
}

/// Returns the mock's base URL — the drain worker builds its own
/// LlmClient from the ConfigStore key "llm".
async fn mock_llm(responses: Vec<serde_json::Value>) -> (String, Shared) {
    let state: Shared = Arc::new(tokio::sync::Mutex::new(MockState {
        responses,
        requests: Vec::new(),
    }));
    let handler_state = Arc::clone(&state);
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |body: String| {
            let state = handler_state;
            async move {
                let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let mut guard = state.lock().await;
                guard.requests.push(parsed);
                match guard.responses.pop() {
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
    (format!("http://{addr}/v1"), state)
}

fn text_response(text: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{ "message": { "role": "assistant", "content": text } }]
    })
}

fn tool_call_response(id: &str, name: &str, args: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args.to_string() }
                }]
            }
        }]
    })
}

/// A mock whose responder sleeps `delay` before popping the scripted
/// response: a hung LLM for T11.4 (the age branch must route around
/// it without waiting) and a widened claim→done window for T11.11's
/// mid-run status polling.
async fn mock_llm_slow(
    responses: Vec<serde_json::Value>,
    delay: std::time::Duration,
) -> (String, Shared) {
    let state: Shared = Arc::new(tokio::sync::Mutex::new(MockState {
        responses,
        requests: Vec::new(),
    }));
    let handler_state = Arc::clone(&state);
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |body: String| {
            let state = handler_state;
            async move {
                tokio::time::sleep(delay).await;
                let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let mut guard = state.lock().await;
                guard.requests.push(parsed);
                match guard.responses.pop() {
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
    (format!("http://{addr}/v1"), state)
}

/// A mock whose scripted responses carry explicit HTTP status codes —
/// the retry/dead cases need real 500s (the plain mock always 200s).
/// Same LIFO pop + request capture as `mock_llm`.
type StatusShared = Arc<tokio::sync::Mutex<StatusMockState>>;
struct StatusMockState {
    script: Vec<(u16, serde_json::Value)>,
    requests: Vec<serde_json::Value>,
}

async fn mock_llm_status(script: Vec<(u16, serde_json::Value)>) -> (String, StatusShared) {
    let state: StatusShared = Arc::new(tokio::sync::Mutex::new(StatusMockState {
        script,
        requests: Vec::new(),
    }));
    let handler_state = Arc::clone(&state);
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |body: String| {
            let state = handler_state;
            async move {
                let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let mut guard = state.lock().await;
                guard.requests.push(parsed);
                match guard.script.pop() {
                    Some((code, resp)) if (200..300).contains(&code) => {
                        axum::Json(resp).into_response()
                    }
                    Some((code, body)) => (
                        axum::http::StatusCode::from_u16(code).expect("valid status"),
                        body.to_string(),
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
    (format!("http://{addr}/v1"), state)
}

// ---------- task-11 helpers ----------

/// Drive run_pending until a wake makes no progress (every due item
/// terminal, or re-armed past due): the spawn loop's steady state,
/// synchronous. Bounded so a pathological re-queue loop fails loudly
/// instead of hanging CI.
async fn drain_until_empty(worker: &QueueWorker) {
    for _ in 0..1000 {
        if worker.run_pending().await.unwrap() == 0 {
            return;
        }
    }
    panic!("drain made progress 1000 times without emptying");
}

/// An AgentScopes with no extra stores (drain runs carry none beyond
/// the trace sink; the query runs T11.12 spawns need none either).
fn no_scopes<'a>() -> agent::AgentScopes<'a> {
    agent::AgentScopes {
        skills: None,
        library: None,
        read_stack_text: None,
        visible_slugs: None,
        trace: None,
    }
}

// ---------- scaffolding ----------

async fn test_store() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path()).await.unwrap());
    (dir, store)
}

/// A user row to satisfy the mutation_queue FK on users(id).
async fn seed_user(store: &Store, name: &str) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
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
    id
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

/// Stub master-key recovery: returns the test's known master key for
/// any uid (the real MasterKeyCache::for_user lives at the binary seam).
fn recovery(master: MasterKey) -> Arc<MasterKeyRecovery> {
    let m2 = master.clone();
    Arc::new(move |_uid| {
        let m = m2.clone();
        Box::pin(async move { Ok(m) })
    })
}

/// An LLM config pointing at a port nothing listens on.
fn dead_port_config() -> LlmConfig {
    LlmConfig {
        url: "http://127.0.0.1:1/v1".into(),
        model: "m".into(),
        api_key: None,
    }
}

/// Build a worker over the store (fresh ConfigStore the test can seed).
fn worker(
    store: &Arc<Store>,
    recovery: Arc<MasterKeyRecovery>,
    limits: QueueLimits,
) -> (Arc<ConfigStore>, Arc<QueueWorker>) {
    let config = Arc::new(ConfigStore::new(store.pool().clone()));
    let service_key = Arc::new(test_service_key());
    let w = Arc::new(QueueWorker::new(
        Arc::clone(store),
        Arc::clone(&config),
        recovery,
        service_key,
        limits,
    ));
    (config, w)
}

/// A deterministic service key for tests that seed sealed config rows
/// (or read legacy ones without decrypting).
fn test_service_key() -> mycelium_crypto::keys::ServiceKey {
    use mycelium_crypto::keys::{ServiceKey, generate_master_key};
    let k = generate_master_key();
    ServiceKey::from_bytes(k.as_bytes()).expect("32 bytes")
}

/// Enqueue a full item the way the MCP tool will: row (capped), payload
/// blob, flip to pending. For `add` also writes the staging note
/// (through the internal bypass — /mutation-queue/ is reserved).
async fn enqueue_item(
    store: &Store,
    cs: &ConceptStore<'_>,
    master: &MasterKey,
    uid: uuid::Uuid,
    tool: QueueTool,
    args: serde_json::Value,
    content: &str,
) -> uuid::Uuid {
    let id = store.enqueue_capped(uid, tool, 50).await.unwrap();
    let payload = MutationPayload {
        tool,
        args_json: args.to_string(),
        content: content.to_string(),
    };
    mutation_queue::write_payload(&FileRepo::new(store.user_dir(uid)), master, id, &payload)
        .await
        .unwrap();
    store.activate(id).await.unwrap();
    if tool == QueueTool::Add {
        let note = Concept::new(
            Frontmatter {
                concept_type: "Note".into(),
                title: Some(derive_title(content)),
                tags: vec!["queue-staging".into()],
                timestamp: Some(chrono::Utc::now().to_rfc3339()),
                ..Default::default()
            },
            content.to_string(),
            mutation_queue::stage_note_path(id),
        );
        cs.put_batch_internal(&[note]).await.unwrap();
    }
    id
}

// ---------- T5.1: drain integrates a queued add via the agent ----------

#[tokio::test]
async fn drain_integrates_queued_add() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t51").await;
    let cs = ConceptStore::for_user(&store, uid, master.clone());

    // Script (LIFO): one write_concept call, then the final summary.
    let (url, _mock) = mock_llm(vec![
        text_response("recorded"),
        tool_call_response(
            "c1",
            "write_concept",
            serde_json::json!({
                "path": "/notes/drainme.md",
                "frontmatter": { "type": "Note", "title": "Drain Me Please" },
                "body": "drain me please"
            }),
        ),
    ])
    .await;

    let (config, worker) = worker(&store, recovery(master.clone()), QueueLimits::default());
    config
        .set(
            "llm",
            &LlmConfig {
                url,
                model: "mock".into(),
                api_key: None,
            },
        )
        .await
        .unwrap();

    let id = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Add,
        serde_json::json!({ "content": "drain me please" }),
        "drain me please",
    )
    .await;

    assert_eq!(worker.run_pending().await.unwrap(), 1);

    let row = store.queue_item(uid, id).await.unwrap().unwrap();
    assert_eq!(row.status, QueueStatus::Done);
    let paths: Vec<String> = serde_json::from_str(row.final_paths.as_deref().unwrap()).unwrap();
    assert!(paths.contains(&"/notes/drainme.md".to_string()));
    // Staging note gone from the bundle.
    let note_path = mutation_queue::stage_note_path(id);
    assert!(!cs.list().await.unwrap().iter().any(|e| e.path == note_path));
    // Terminal done: the payload blob is dropped (the integrated
    // concept holds the content; nothing re-reads the blob).
    let repo = FileRepo::new(store.user_dir(uid));
    assert!(
        mutation_queue::read_payload(&repo, &master, id)
            .await
            .unwrap()
            .is_none(),
        "payload blob must be dropped at done"
    );
    let stored = FileKeys::from_master_key(&master).stored_name(&mutation_queue::payload_path(id));
    assert!(
        !store.user_dir(uid).join(stored).exists(),
        "payload file must be physically gone"
    );
    // The concept the mock's write_concept call supplied.
    let written = cs.get("/notes/drainme.md").await.unwrap();
    assert_eq!(written.body.trim(), "drain me please");
    assert_eq!(worker.integrated_count.load(Ordering::Relaxed), 1);
    assert_eq!(worker.fallback_count.load(Ordering::Relaxed), 0);
}

// ---------- T5.2: dead LLM retries, then the fallback integrates ----------

#[tokio::test]
async fn dead_llm_retries_then_fallback() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t52").await;
    let cs = ConceptStore::for_user(&store, uid, master.clone());

    // Two attempts (zero backoff — instant retry), normal age deadline:
    // the Err-arm exhausts attempts on wake 2 and the fallback runs.
    let limits = QueueLimits {
        max_attempts: 2,
        backoff_secs: [0, 0, 0],
        age_deadline_secs: 600,
        ..QueueLimits::default()
    };
    let (config, worker) = worker(&store, recovery(master.clone()), limits);
    config.set("llm", &dead_port_config()).await.unwrap();

    let id = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Add,
        serde_json::json!({ "content": "fallback me" }),
        "fallback me",
    )
    .await;

    // Wake 1: transient LLM failure → attempt 1, retry due immediately.
    // Wake 2: attempt 2 → attempts exhausted → fallback integrates.
    // (The zero backoff may let one pass complete both wakes; the two
    // calls cover either distribution.)
    let _ = worker.run_pending().await.unwrap();
    let _ = worker.run_pending().await.unwrap();

    let row = store.queue_item(uid, id).await.unwrap().unwrap();
    assert_eq!(row.status, QueueStatus::Done);
    assert_eq!(row.attempts, 2, "exactly two attempts: {row:?}");
    assert!(row.detail.contains("fallback"), "detail: {}", row.detail);
    // Staging note gone; concept integrated by the direct-write fallback.
    let note_path = mutation_queue::stage_note_path(id);
    assert!(!cs.list().await.unwrap().iter().any(|e| e.path == note_path));
    let integrated = cs.get("/fallback-me.md").await.unwrap();
    assert_eq!(integrated.body.trim(), "fallback me");
    assert_eq!(worker.fallback_count.load(Ordering::Relaxed), 1);
    // Terminal done: the payload blob is dropped (the fallback-success
    // arm of the terminal cleanup). That the fallback could read it on
    // wake 2 also pins: the blob is NOT deleted before a retry.
    assert!(
        mutation_queue::read_payload(&FileRepo::new(store.user_dir(uid)), &master, id)
            .await
            .unwrap()
            .is_none(),
        "payload blob must be dropped at done"
    );
}

// ---------- T5.3: no capacity → the drain defers, never blocks ----------

#[tokio::test]
async fn drain_defers_when_no_permit() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t53").await;
    let cs = ConceptStore::for_user(&store, uid, master.clone());

    let limits = QueueLimits {
        age_deadline_secs: 0,
        ..QueueLimits::default()
    };
    let (config, worker) = worker(&store, recovery(master.clone()), limits);
    config.set("llm", &dead_port_config()).await.unwrap();

    let id = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Add,
        serde_json::json!({ "content": "defer me" }),
        "defer me",
    )
    .await;

    // Starve every LLM permit: the drain must skip this wake entirely —
    // query priority, never block.
    let permits = agent::LLM_RUNS.acquire_many(2).await.unwrap();
    assert_eq!(worker.run_pending().await.unwrap(), 0);
    let row = store.queue_item(uid, id).await.unwrap().unwrap();
    assert_eq!(row.status, QueueStatus::Pending);
    assert_eq!(row.attempts, 0, "no claim may burn an attempt: {row:?}");
    drop(permits);

    // Permits released: the next pass integrates (deadline 0 → the
    // deterministic fallback; no LLM needed).
    assert_eq!(worker.run_pending().await.unwrap(), 1);
    let row = store.queue_item(uid, id).await.unwrap().unwrap();
    assert_eq!(row.status, QueueStatus::Done);
}

// ---------- T5.4: co-queued updates apply in queue order ----------

#[tokio::test]
async fn coqueued_updates_apply_in_order() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t54").await;
    let cs = ConceptStore::for_user(&store, uid, master.clone());
    cs.put(&concept("/t.md", "Target", "start")).await.unwrap();

    let limits = QueueLimits {
        age_deadline_secs: 0,
        ..QueueLimits::default()
    };
    let (config, worker) = worker(&store, recovery(master.clone()), limits);
    config.set("llm", &dead_port_config()).await.unwrap();

    let _first = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Update,
        serde_json::json!({ "path": "/t.md" }),
        "first change",
    )
    .await;
    let _second = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Update,
        serde_json::json!({ "path": "/t.md" }),
        "second change",
    )
    .await;

    // One drain pass: both updates integrate via the dated-addendum
    // fallback, oldest row first (per-user queue order).
    assert_eq!(worker.run_pending().await.unwrap(), 2);

    let body = cs.get("/t.md").await.unwrap().body;
    assert_eq!(body.matches("<!-- mycelium2:update:").count(), 2);
    let first = body.find("first change").expect("first update applied");
    let second = body.find("second change").expect("second update applied");
    assert!(first < second, "queue order must be preserved: {body:?}");
    assert!(body.contains("start"));
}

// ---------- T5.5: fallback failure marks the row dead ----------

#[tokio::test]
async fn fallback_failure_marks_dead() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t55").await;
    let cs = ConceptStore::for_user(&store, uid, master.clone());

    // Empty bundle: the update fallback cannot resolve a target concept.
    let limits = QueueLimits {
        age_deadline_secs: 0,
        ..QueueLimits::default()
    };
    let (config, worker) = worker(&store, recovery(master.clone()), limits);
    config.set("llm", &dead_port_config()).await.unwrap();

    let id = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Update,
        serde_json::json!({ "content": "change nothing anywhere" }),
        "change nothing anywhere",
    )
    .await;

    worker.run_pending().await.unwrap();
    let row = store.queue_item(uid, id).await.unwrap().unwrap();
    assert_eq!(row.status, QueueStatus::Dead);
    assert!(
        row.detail.contains("fallback failed"),
        "detail: {}",
        row.detail
    );
    assert_eq!(worker.dead_count.load(Ordering::Relaxed), 1);
    // Terminal dead: the payload blob is dropped (the row — the
    // receipt — persists; the staged content does not).
    assert!(
        mutation_queue::read_payload(&FileRepo::new(store.user_dir(uid)), &master, id)
            .await
            .unwrap()
            .is_none(),
        "payload blob must be dropped at dead"
    );
}

// ---------- T12.I1: error detail persisted into the row is bounded ----------
//
// LLM status errors embed the upstream HTTP body; the drain persists
// error Display text into `detail` (re-served by receipts), so an
// over-cap body must be truncated at the queue's 512-byte detail cap.
// Also pins: the deterministic-error provenance keeps the
// `integrated via deterministic fallback (...)` literal byte-exact
// (the queue_totals LIKE discriminator depends on its prefix).
#[tokio::test]
async fn long_error_detail_is_bounded() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t12i1").await;
    let cs = ConceptStore::for_user(&store, uid, master.clone());

    // Bodies far past the 512-byte cap; the distinctive tails sit well
    // inside the LLM client's own 500-char body window but past the
    // queue's detail window. LIFO: the older item runs first (its 500
    // is the LAST script entry), the newer item's 400 second.
    let body_500 = format!("{}TAILQZX9{}", "B".repeat(485), "C".repeat(100));
    let body_400 = format!("{}NOLO_TAIL{}", "D".repeat(485), "E".repeat(100));
    let (url, _mock) = mock_llm_status(vec![
        (400, serde_json::json!(body_400)),
        (500, serde_json::json!(body_500)),
    ])
    .await;
    // Default limits: the 500 item re-arms with a 30 s backoff, so its
    // failed-attempt detail stays observable on the row.
    let (config, worker) = worker(&store, recovery(master.clone()), QueueLimits::default());
    config
        .set(
            "llm",
            &LlmConfig {
                url,
                model: "mock".into(),
                api_key: None,
            },
        )
        .await
        .unwrap();

    let id_retry = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Add,
        serde_json::json!({ "content": "bound my retry error detail" }),
        "bound my retry error detail",
    )
    .await;
    let id_det = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Add,
        serde_json::json!({ "content": "bound my deterministic error detail" }),
        "bound my deterministic error detail",
    )
    .await;

    assert_eq!(worker.run_pending().await.unwrap(), 2);

    // Site 1 — "attempt {n}: {e}" on the retry path: bounded, marker
    // present, past-cap tail absent. "attempt 1: " + capped error
    // (512) + marker ("…" is 3 bytes → 15).
    let row = store.queue_item(uid, id_retry).await.unwrap().unwrap();
    assert_eq!(row.attempts, 1);
    assert!(
        row.detail.starts_with("attempt 1: "),
        "failed-attempt context prefixes the detail: {}",
        row.detail
    );
    assert!(
        row.detail.contains("[truncated]"),
        "over-cap detail carries the truncation marker: {}",
        row.detail
    );
    assert!(
        !row.detail.contains("TAILQZX9"),
        "content past the cap must not persist: {}",
        row.detail
    );
    assert!(
        row.detail.len() <= "attempt 1: ".len() + 512 + "… [truncated]".len(),
        "detail: {}",
        row.detail
    );

    // Site 3 — "deterministic error: {e}" as the fallback provenance:
    // the pinned literal stays byte-exact around the bounded error,
    // so the queue_totals LIKE discriminator still matches.
    let row = store.queue_item(uid, id_det).await.unwrap().unwrap();
    assert_eq!(row.status, QueueStatus::Done);
    assert!(
        row.detail
            .starts_with("integrated via deterministic fallback (deterministic error: "),
        "pinned provenance literal intact: {}",
        row.detail
    );
    assert!(
        row.detail.contains("[truncated]"),
        "bounded provenance carries the marker: {}",
        row.detail
    );
    assert!(
        !row.detail.contains("NOLO_TAIL"),
        "content past the cap must not persist: {}",
        row.detail
    );
    let totals = store.queue_totals().await.unwrap();
    assert_eq!(totals.fallback, 1, "discriminator still matches");
    assert_eq!(totals.integrated, 0);
}

// ---------- T5.6: boot sweep reconciles interrupted state ----------

#[tokio::test]
async fn boot_sweep_reconciliation() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t56").await;

    // (a) enqueue only — staging, no payload (enqueue died pre-durability).
    let a = store.enqueue(uid, QueueTool::Add).await.unwrap();
    // (b) enqueue + payload, NOT activated — still staging.
    let b = store.enqueue(uid, QueueTool::Update).await.unwrap();
    let payload = MutationPayload {
        tool: QueueTool::Update,
        args_json: serde_json::json!({ "path": "/t.md" }).to_string(),
        content: "b".into(),
    };
    mutation_queue::write_payload(&FileRepo::new(store.user_dir(uid)), &master, b, &payload)
        .await
        .unwrap();
    // (c) enqueue + payload + activate + claim — left running, as if the
    // process died mid-item.
    let c = store.enqueue(uid, QueueTool::Add).await.unwrap();
    let payload_c = MutationPayload {
        tool: QueueTool::Add,
        args_json: serde_json::json!({ "content": "c" }).to_string(),
        content: "c".into(),
    };
    mutation_queue::write_payload(&FileRepo::new(store.user_dir(uid)), &master, c, &payload_c)
        .await
        .unwrap();
    store.activate(c).await.unwrap();
    let claimed = store.claim_due_for_user(uid).await.unwrap().unwrap();
    assert_eq!(claimed.id, c);

    let (_config, worker) = worker(&store, recovery(master), QueueLimits::default());
    worker.recover_on_boot().await.unwrap();

    // (a) payload never became durable → dead, operator-visible.
    let row_a = store.queue_item(uid, a).await.unwrap().unwrap();
    assert_eq!(row_a.status, QueueStatus::Dead);
    assert!(
        row_a.detail.contains("payload missing"),
        "detail: {}",
        row_a.detail
    );
    // (b) payload durable → activated to pending.
    let row_b = store.queue_item(uid, b).await.unwrap().unwrap();
    assert_eq!(row_b.status, QueueStatus::Pending);
    // (c) was mid-flight → back to pending with the restart detail.
    let row_c = store.queue_item(uid, c).await.unwrap().unwrap();
    assert_eq!(row_c.status, QueueStatus::Pending);
    assert!(
        row_c.detail.contains("interrupted"),
        "detail: {}",
        row_c.detail
    );
}

// ---------- T5.7: the drain serves each due user with their own key ----------

#[tokio::test]
async fn drain_handles_all_due_users_separately() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let u1 = seed_user(&store, "t57a").await;
    let u2 = seed_user(&store, "t57b").await;
    let cs1 = ConceptStore::for_user(&store, u1, master.clone());
    let cs2 = ConceptStore::for_user(&store, u2, master.clone());

    // The recovery stub records every uid it is asked for.
    let seen: Arc<tokio::sync::Mutex<Vec<uuid::Uuid>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let m = master.clone();
    let seen2 = Arc::clone(&seen);
    let rec: Arc<MasterKeyRecovery> = Arc::new(move |uid| {
        let m = m.clone();
        let seen = Arc::clone(&seen2);
        Box::pin(async move {
            seen.lock().await.push(uid);
            Ok(m)
        })
    });

    let limits = QueueLimits {
        age_deadline_secs: 0,
        ..QueueLimits::default()
    };
    let (_config, worker) = worker(&store, rec, limits);

    let id1 = enqueue_item(
        &store,
        &cs1,
        &master,
        u1,
        QueueTool::Add,
        serde_json::json!({ "content": "user one fact" }),
        "user one fact",
    )
    .await;
    let id2 = enqueue_item(
        &store,
        &cs2,
        &master,
        u2,
        QueueTool::Add,
        serde_json::json!({ "content": "user two fact" }),
        "user two fact",
    )
    .await;

    assert_eq!(worker.run_pending().await.unwrap(), 2);

    // Both rows reached terminal success in one pass.
    assert_eq!(
        store.queue_item(u1, id1).await.unwrap().unwrap().status,
        QueueStatus::Done
    );
    assert_eq!(
        store.queue_item(u2, id2).await.unwrap().unwrap().status,
        QueueStatus::Done
    );
    // The drain asked for each user's own key — and no other user's.
    let recorded = seen.lock().await.clone();
    let mut uniq = recorded.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), 2, "recorded: {recorded:?}");
    assert!(recorded.contains(&u1));
    assert!(recorded.contains(&u2));
    // Each user's bundle holds only their own integrated concept.
    assert!(cs1.get("/user-one-fact.md").await.is_ok());
    assert!(cs2.get("/user-two-fact.md").await.is_ok());
    assert!(cs1.get("/user-two-fact.md").await.is_err());
    assert!(cs2.get("/user-one-fact.md").await.is_err());
}

// ---------- T9.1: the binary's recovery-closure seam ----------

// T9.1 — the binary's exact recovery-closure construction (from
// MasterKeyCache::for_user) unseals a persisted service seal.
#[tokio::test]
async fn recovery_closure_matches_binary_seam() {
    use mycelium_crypto::keys::service_seal_dek;
    use mycelium_mcp::MasterKeyCache;
    let dir = tempfile::tempdir().unwrap();
    let store = mycelium_store::Store::open(dir.path()).await.unwrap();
    let service_key = mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();
    let master = mycelium_crypto::generate_master_key();
    let user = uuid::Uuid::new_v4();
    let sealed = mycelium_crypto::aead::aead_seal(
        master.as_bytes(),
        b"mycelium2/seal/service/v1",
        &service_seal_dek(&service_key),
    )
    .unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, email, role, auth_provider, password_hash, sealed_master_key, master_key_service_sealed, created_at, updated_at) VALUES (?, 'u', 'u@x', 'user', 'local', '', '', ?, ?, ?)",
    )
    .bind(user.to_string())
    .bind(hex::encode(sealed))
    .bind(chrono::Utc::now().to_rfc3339())
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(store.pool())
    .await
    .unwrap();

    // The binary's closure (main.rs / AppState::new shape).
    let cache = std::sync::Arc::new(MasterKeyCache::default());
    let store2 = std::sync::Arc::new(store);
    let key2 = std::sync::Arc::new(service_key.clone());
    let recovery: std::sync::Arc<mycelium_librarian::queue_worker::MasterKeyRecovery> =
        std::sync::Arc::new({
            let cache = cache.clone();
            let store = store2.clone();
            let key = key2.clone();
            move |uid| {
                let cache = cache.clone();
                let store = store.clone();
                let key = key.clone();
                Box::pin(async move { cache.for_user(&store, &key, uid).await })
            }
        });

    let got = (recovery)(user).await.unwrap();
    assert_eq!(got.as_bytes(), master.as_bytes());
    // Unknown user → RowNotFound (the queue treats it as unavailable).
    let err = (recovery)(uuid::Uuid::new_v4()).await;
    assert!(matches!(err, Err(sqlx::Error::RowNotFound)));
}

// ---------- task 11: spec §7's 14 e2e cases ----------
//
// §7 case map — the cases earlier tasks already pinned are
// cross-referenced by name (asserted unchanged, not duplicated):
// - §7.9  reserved prefix → `external_put_rejected_internal_allowed`
//   (crates/mycelium-store/tests/mutation_queue_integration.rs,
//   Task 3 — the reservation mod lives in the store crate where Task 3
//   landed it; the /mutation-queue/ path never reaches non-queue code
//   by construction).
// - §7.10 enqueue crash windows → `boot_sweep_reconciliation` (this
//   file, Task 5): the staging row without a payload deads with
//   "payload missing — enqueue interrupted before durability;
//   re-submit"; staging-with-payload activates; running returns to
//   pending.
// - §7.14 receipts-only tools → `update_and_maintain_are_receipts_only`
//   (crates/mycelium-mcp/tests/mutation_queue_mcp.rs, Task 6's T6.2).

// T11.1 [spec §7.1 + §9.1 — burst receipt + isolation, zero LLM
// contact]: 20 concurrent full enqueues (the store/payload APIs a
// burst of memory_add calls drives) all land inside the §9.1 latency
// budget, and the drain integrates every one via the deterministic
// fallback with the LLM pointed at a dead port — zero LLM contact.
#[tokio::test]
async fn burst_of_enqueues_never_touches_the_llm_and_all_integrate() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t111").await;
    let cs = ConceptStore::for_user(&store, uid, master.clone());

    // Age deadline 0: the drain's age branch routes every item to the
    // fallback before any LLM contact — the dead port backstops the
    // "never touches the LLM" claim (a contacted LLM would error into
    // the 30 s backoff path and the all-done assertions would fail
    // loudly).
    let limits = QueueLimits {
        age_deadline_secs: 0,
        ..QueueLimits::default()
    };
    let (config, worker) = worker(&store, recovery(master.clone()), limits);
    config.set("llm", &dead_port_config()).await.unwrap();

    // The burst: 20 full enqueues (row + payload + staging note +
    // activate), all in flight at once.
    let t0 = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for i in 0..20 {
        let store = Arc::clone(&store);
        let master_for_cs = master.clone();
        let master_for_call = master.clone();
        set.spawn(async move {
            let cs = ConceptStore::for_user(&store, uid, master_for_cs);
            enqueue_item(
                &store,
                &cs,
                &master_for_call,
                uid,
                QueueTool::Add,
                serde_json::json!({ "content": format!("burst item {i}") }),
                &format!("burst item {i}"),
            )
            .await
        });
    }
    let mut ids = Vec::with_capacity(20);
    while let Some(joined) = set.join_next().await {
        ids.push(joined.expect("enqueue task panicked"));
    }
    let enqueue_elapsed = t0.elapsed();

    // Every receipt distinct; the enqueue phase beat the §9.1 budget
    // (2 s total ≈ 100 ms per receipt).
    let unique: std::collections::HashSet<uuid::Uuid> = ids.iter().copied().collect();
    assert_eq!(unique.len(), 20, "receipts must be distinct");
    assert!(
        enqueue_elapsed < Duration::from_secs(2),
        "20-enqueue burst took {enqueue_elapsed:?} (§9.1 budget 2 s)"
    );

    // Drain until users-with-due is empty: every item reaches done.
    drain_until_empty(&worker).await;
    for &id in &ids {
        let row = store.queue_item(uid, id).await.unwrap().unwrap();
        assert_eq!(row.status, QueueStatus::Done, "item {id}");
        let paths: Vec<String> = serde_json::from_str(row.final_paths.as_deref().unwrap()).unwrap();
        assert_eq!(paths.len(), 1, "final_paths on {id}: {paths:?}");
    }
    // Every staging note is gone.
    let entries = cs.list().await.unwrap();
    assert!(
        !entries
            .iter()
            .any(|e| e.path.starts_with("/mutation-queue/")),
        "staging notes must be reconciled: {entries:?}"
    );

    // Zero LLM contact: all 20 integrations carry fallback provenance.
    let totals = store.queue_totals().await.unwrap();
    assert_eq!(totals.fallback, 20);
    assert_eq!(totals.integrated, 0);
    assert_eq!(totals.queued_per_tool[0], (QueueTool::Add, 20));
    assert_eq!(worker.fallback_count.load(Ordering::Relaxed), 20);
}

// T11.2 [spec §7.2 + §9.2 — durability across restart]: rows, payload
// blobs, and staging notes survive dropping the store and worker; the
// boot sweep completes the interrupted staging→pending flip, and a
// fresh worker over the reopened store integrates all three items —
// the payloads decrypt with the same master key across the restart.
#[tokio::test]
async fn restarted_relay_requeues_and_integrates() {
    let _g = TEST_LOCK.lock().await;
    let (dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t112").await;
    let cs = ConceptStore::for_user(&store, uid, master.clone());
    cs.put(&concept(
        "/target.md",
        "Restart Target",
        "restart anchor body",
    ))
    .await
    .unwrap();

    let limits = QueueLimits {
        age_deadline_secs: 0,
        ..QueueLimits::default()
    };
    let (config, worker_a) = worker(&store, recovery(master.clone()), limits.clone());
    config.set("llm", &dead_port_config()).await.unwrap();

    // (a) a fully enqueued add, (c) a fully enqueued update …
    let a = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Add,
        serde_json::json!({ "content": "restart add one" }),
        "restart add one",
    )
    .await;
    let c = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Update,
        serde_json::json!({ "path": "/target.md" }),
        "restart update lands",
    )
    .await;
    // … and (b) the interrupted flip: row + payload, never activated —
    // exactly the enqueue-crash window the boot sweep must complete.
    let b = store.enqueue_capped(uid, QueueTool::Add, 50).await.unwrap();
    let payload_b = MutationPayload {
        tool: QueueTool::Add,
        args_json: serde_json::json!({ "content": "restart add two" }).to_string(),
        content: "restart add two".into(),
    };
    mutation_queue::write_payload(&FileRepo::new(store.user_dir(uid)), &master, b, &payload_b)
        .await
        .unwrap();

    // DROP the worker, its config, the concept store, the store
    // handle — then reopen the same data dir.
    drop(worker_a);
    drop(config);
    drop(cs);
    drop(store);
    let store2 = Arc::new(Store::open(dir.path()).await.unwrap());
    let cs2 = ConceptStore::for_user(&store2, uid, master.clone());
    let (config2, worker2) = worker(&store2, recovery(master.clone()), limits);
    config2.set("llm", &dead_port_config()).await.unwrap();

    // The boot sweep: (b)'s interrupted flip completes to pending; the
    // already-activated rows stay pending.
    worker2.recover_on_boot().await.unwrap();
    for id in [a, b, c] {
        let row = store2.queue_item(uid, id).await.unwrap().unwrap();
        assert_eq!(row.status, QueueStatus::Pending, "row {id} requeued");
    }

    // Durability: (b)'s payload blob decrypts with the same master key.
    let repo2 = FileRepo::new(store2.user_dir(uid));
    let read_back = mutation_queue::read_payload(&repo2, &master, b)
        .await
        .unwrap()
        .expect("payload survived the restart");
    assert_eq!(read_back.content, "restart add two");

    // A fresh drain integrates all three.
    drain_until_empty(&worker2).await;
    for id in [a, b, c] {
        let row = store2.queue_item(uid, id).await.unwrap().unwrap();
        assert_eq!(row.status, QueueStatus::Done, "row {id}");
        let paths: Vec<String> = serde_json::from_str(row.final_paths.as_deref().unwrap()).unwrap();
        assert!(!paths.is_empty(), "final_paths on {id}");
    }
    // The two adds landed as concepts; the update appended its
    // addendum to the seeded target.
    assert_eq!(
        cs2.get("/restart-add-one.md").await.unwrap().body.trim(),
        "restart add one"
    );
    assert_eq!(
        cs2.get("/restart-add-two.md").await.unwrap().body.trim(),
        "restart add two"
    );
    let target = cs2.get("/target.md").await.unwrap();
    assert!(target.body.contains("restart anchor body"));
    assert!(target.body.contains("restart update lands"));
    assert!(target.body.contains("<!-- mycelium2:update:"));
    // Staging notes gone after integration.
    let entries = cs2.list().await.unwrap();
    assert!(
        !entries
            .iter()
            .any(|e| e.path.starts_with("/mutation-queue/"))
    );
}

// T11.3 [spec §7.3 + §9.3 — dead LLM doesn't block read or write]:
// with the LLM dead, the enqueue path completes, the drain absorbs the
// failure within its retry budget and integrates via the fallback in
// bounded wall-clock (no LLM timeout burn), and search (the
// memory_query deterministic path) finds the integrated concept.
#[tokio::test]
async fn dead_llm_receipts_and_reads() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t113").await;
    let cs = ConceptStore::for_user(&store, uid, master.clone());

    // One attempt against the dead port, zero backoff: the first
    // failure exhausts the budget and the fallback integrates.
    let limits = QueueLimits {
        max_attempts: 1,
        backoff_secs: [0, 0, 0],
        ..QueueLimits::default()
    };
    let (config, worker) = worker(&store, recovery(master.clone()), limits);
    config.set("llm", &dead_port_config()).await.unwrap();

    let id = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Add,
        serde_json::json!({ "content": "zebras graze at dawn quietly", "path": "tq/deadllm" }),
        "zebras graze at dawn quietly",
    )
    .await;

    let t0 = Instant::now();
    drain_until_empty(&worker).await;
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "dead LLM must not burn timeouts: {elapsed:?}"
    );

    let row = store.queue_item(uid, id).await.unwrap().unwrap();
    assert_eq!(row.status, QueueStatus::Done);
    assert!(
        row.detail.contains("integrated via deterministic fallback"),
        "detail: {}",
        row.detail
    );
    assert_eq!(row.attempts, 1, "single-attempt budget: {row:?}");

    // The read path (memory_query's deterministic search equivalent)
    // finds the integrated concept — the queued write never blocked
    // reads.
    let hits = cs
        .search(&SearchQuery::new(vec!["graze".into()]))
        .await
        .unwrap();
    assert!(!hits.is_empty(), "search must find the integrated concept");
    assert_eq!(hits[0].concept_path, "/tq/deadllm.md");
}

// T11.4 [spec §7.4 — age deadline → fallback]: a hung LLM (the mock
// sleeps 2 s before replying) cannot hold an item past its age
// deadline — the drain routes to the fallback without contacting the
// LLM at all (zero requests hit the mock), and the receipt carries
// the age-deadline provenance byte-exact.
#[tokio::test]
async fn age_deadline_routes_to_fallback() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t114").await;
    let cs = ConceptStore::for_user(&store, uid, master.clone());

    // A mock that BLOCKS 2 s before replying — success-scripted, so a
    // broken age branch would integrate via the agent run with the
    // wrong provenance and fail the detail assert below.
    let (url, mock) = mock_llm_slow(
        vec![
            text_response("recorded"),
            tool_call_response(
                "c1",
                "write_concept",
                serde_json::json!({
                    "path": "/notes/never-written.md",
                    "frontmatter": { "type": "Note", "title": "Never Written" },
                    "body": "never written"
                }),
            ),
        ],
        Duration::from_secs(2),
    )
    .await;
    let limits = QueueLimits {
        age_deadline_secs: 1,
        ..QueueLimits::default()
    };
    let (config, worker) = worker(&store, recovery(master.clone()), limits);
    config
        .set(
            "llm",
            &LlmConfig {
                url,
                model: "mock".into(),
                api_key: None,
            },
        )
        .await
        .unwrap();

    let id = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Add,
        serde_json::json!({ "content": "age out to fallback" }),
        "age out to fallback",
    )
    .await;

    // Age past the 1 s deadline, then drain.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let t0 = Instant::now();
    assert_eq!(worker.run_pending().await.unwrap(), 1);
    let elapsed = t0.elapsed();

    let row = store.queue_item(uid, id).await.unwrap().unwrap();
    assert_eq!(row.status, QueueStatus::Done);
    assert_eq!(
        row.detail, "integrated via deterministic fallback (age deadline passed)",
        "detail must name the age provenance"
    );
    // Zero LLM contact: the age branch skips the agent run entirely.
    {
        let guard = mock.lock().await;
        assert!(
            guard.requests.is_empty(),
            "the drain must not contact a hung LLM"
        );
    }
    assert!(
        elapsed < Duration::from_millis(1500),
        "no 2 s mock delay may be paid: {elapsed:?}"
    );
    // fallback_total == 1; the staging note is gone.
    assert_eq!(store.queue_totals().await.unwrap().fallback, 1);
    assert!(
        !cs.list()
            .await
            .unwrap()
            .iter()
            .any(|e| e.path == mutation_queue::stage_note_path(id)),
        "staging note deleted"
    );
}

// T11.5 [spec §7.5 — transient retry via failing-twice mock]: two
// scripted 500s burn two attempts (zero backoff), the third attempt
// succeeds through the agent — done with agent provenance, attempts
// 3, no fallback.
#[tokio::test]
async fn transient_errors_retry_then_integrate() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t115").await;
    let cs = ConceptStore::for_user(&store, uid, master.clone());

    // LIFO script: pop order is failure, failure, then the success
    // run (tool call + final text) — all entries (status, body) so
    // both the 500s and the 200s go through the status-aware mock.
    let (url, mock) = mock_llm_status(vec![
        (200, text_response("recorded")),
        (
            200,
            tool_call_response(
                "c1",
                "write_concept",
                serde_json::json!({
                    "path": "/notes/retried.md",
                    "frontmatter": { "type": "Note", "title": "Retried" },
                    "body": "retried body"
                }),
            ),
        ),
        (500, serde_json::json!("first scripted failure")),
        (500, serde_json::json!("second scripted failure")),
    ])
    .await;
    let limits = QueueLimits {
        backoff_secs: [0, 0, 0],
        ..QueueLimits::default()
    };
    let (config, worker) = worker(&store, recovery(master.clone()), limits);
    config
        .set(
            "llm",
            &LlmConfig {
                url,
                model: "mock".into(),
                api_key: None,
            },
        )
        .await
        .unwrap();

    let id = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Add,
        serde_json::json!({ "content": "retry me until integrated" }),
        "retry me until integrated",
    )
    .await;

    drain_until_empty(&worker).await;

    let row = store.queue_item(uid, id).await.unwrap().unwrap();
    assert_eq!(row.status, QueueStatus::Done);
    assert_eq!(row.attempts, 3, "two failures then success: {row:?}");
    assert_eq!(
        row.detail, "recorded",
        "agent-run summary, not fallback: {row:?}"
    );
    let paths: Vec<String> = serde_json::from_str(row.final_paths.as_deref().unwrap()).unwrap();
    assert_eq!(paths, vec!["/notes/retried.md".to_string()]);
    assert_eq!(
        cs.get("/notes/retried.md").await.unwrap().body.trim(),
        "retried body"
    );
    // Agent-integrated, no fallback — and the mock saw exactly the
    // four scripted requests (two failures + tool call + text).
    let totals = store.queue_totals().await.unwrap();
    assert_eq!(totals.integrated, 1);
    assert_eq!(totals.fallback, 0);
    assert_eq!(mock.lock().await.requests.len(), 4);
    assert_eq!(worker.integrated_count.load(Ordering::Relaxed), 1);
}

// T11.6 [spec §7.6 — exhausted retries → dead → operator recovery]:
// three scripted 500s exhaust the retry budget; the fallback ALSO
// fails (an update with no matching concept), so the row lands dead
// with the failure detail — and operator recovery is a NEW enqueue
// (the dead row stays for visibility), which integrates cleanly once
// the backend answers again.
#[tokio::test]
async fn persistent_failure_goes_dead_then_recovers() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t116").await;
    let cs = ConceptStore::for_user(&store, uid, master.clone());

    let (url_a, _mock_a) = mock_llm_status(vec![
        (500, serde_json::json!("failure one")),
        (500, serde_json::json!("failure two")),
        (500, serde_json::json!("failure three")),
    ])
    .await;
    let limits = QueueLimits {
        backoff_secs: [0, 0, 0],
        ..QueueLimits::default()
    };
    let (config, worker) = worker(&store, recovery(master.clone()), limits);
    config
        .set(
            "llm",
            &LlmConfig {
                url: url_a,
                model: "mock".into(),
                api_key: None,
            },
        )
        .await
        .unwrap();

    // An update whose fallback cannot resolve a target (empty bundle):
    // when the retries exhaust, the fallback fails and the row deads.
    let dead_id = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Update,
        serde_json::json!({ "content": "change nothing anywhere" }),
        "change nothing anywhere",
    )
    .await;
    drain_until_empty(&worker).await;

    let dead_row = store.queue_item(uid, dead_id).await.unwrap().unwrap();
    assert_eq!(dead_row.status, QueueStatus::Dead);
    assert_eq!(
        dead_row.attempts, 3,
        "drain stops at the budget: {dead_row:?}"
    );
    assert!(
        dead_row.detail.starts_with("fallback failed"),
        "dead detail carries the failure reason: {}",
        dead_row.detail
    );
    assert_eq!(worker.dead_count.load(Ordering::Relaxed), 1);
    assert_eq!(store.queue_totals().await.unwrap().dead, 1);
    // Terminal dead: the payload blob is dropped; the row persists.
    assert!(
        mutation_queue::read_payload(&FileRepo::new(store.user_dir(uid)), &master, dead_id)
            .await
            .unwrap()
            .is_none(),
        "payload blob must be dropped at dead"
    );

    // Operator recovery: the backend answers again (fresh mock) and a
    // NEW enqueue integrates — the dead row is never resurrected.
    let (url_b, _mock_b) = mock_llm(vec![
        text_response("recorded"),
        tool_call_response(
            "c2",
            "write_concept",
            serde_json::json!({
                "path": "/notes/recovered.md",
                "frontmatter": { "type": "Note", "title": "Recovered" },
                "body": "recovered body"
            }),
        ),
    ])
    .await;
    config
        .set(
            "llm",
            &LlmConfig {
                url: url_b,
                model: "mock".into(),
                api_key: None,
            },
        )
        .await
        .unwrap();

    let recovered_id = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Add,
        serde_json::json!({ "content": "recovered after operator resubmit" }),
        "recovered after operator resubmit",
    )
    .await;
    drain_until_empty(&worker).await;

    let recovered = store.queue_item(uid, recovered_id).await.unwrap().unwrap();
    assert_eq!(recovered.status, QueueStatus::Done);
    let paths: Vec<String> =
        serde_json::from_str(recovered.final_paths.as_deref().unwrap()).unwrap();
    assert_eq!(paths, vec!["/notes/recovered.md".to_string()]);
    assert_eq!(
        cs.get("/notes/recovered.md").await.unwrap().body.trim(),
        "recovered body"
    );

    // The dead row stays dead — operator visibility, no resurrection.
    let dead_after = store.queue_item(uid, dead_id).await.unwrap().unwrap();
    assert_eq!(dead_after.status, QueueStatus::Dead);
    assert_eq!(dead_after.attempts, 3);
    let totals = store.queue_totals().await.unwrap();
    assert_eq!(totals.integrated, 1);
    assert_eq!(totals.dead, 1);
}

// T11.7 [spec §7.7 + §9.5 — per-user serialization ordering]: two
// co-queued items for one user (an add, then an update targeting the
// add's concept) integrate strictly in enqueue order — the update's
// addendum lands on the concept the add created, and the rows' own
// timestamps witness the order.
#[tokio::test]
async fn same_user_fifo_add_then_update() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t117").await;
    let cs = ConceptStore::for_user(&store, uid, master.clone());
    cs.put(&concept("/a.md", "Zebras", "zebras are striped"))
        .await
        .unwrap();

    let limits = QueueLimits {
        age_deadline_secs: 0,
        ..QueueLimits::default()
    };
    let (config, worker) = worker(&store, recovery(master.clone()), limits);
    config.set("llm", &dead_port_config()).await.unwrap();

    let add_id = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Add,
        serde_json::json!({ "content": "zebras roam the savanna in herds", "path": "zebras/facts" }),
        "zebras roam the savanna in herds",
    )
    .await;
    let update_id = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Update,
        serde_json::json!({ "path": "/zebras/facts.md" }),
        "zebras also migrate seasonally",
    )
    .await;

    // Row order as enqueued (the FIFO claim order's input).
    let add_row = store.queue_item(uid, add_id).await.unwrap().unwrap();
    let update_row = store.queue_item(uid, update_id).await.unwrap().unwrap();
    assert!(add_row.created_at < update_row.created_at, "enqueue order");

    drain_until_empty(&worker).await;

    // BOTH applied, IN ORDER, to the same concept: the add's content
    // first, then the update's dated addendum after it.
    let facts = cs.get("/zebras/facts.md").await.unwrap();
    let add_at = facts
        .body
        .find("zebras roam the savanna in herds")
        .expect("the add's content integrated");
    let marker_at = facts
        .body
        .find("<!-- mycelium2:update:")
        .expect("the update appended its addendum");
    let upd_at = facts
        .body
        .find("zebras also migrate seasonally")
        .expect("update content present");
    assert!(add_at < marker_at, "add content precedes the marker");
    assert!(marker_at < upd_at, "marker precedes the update content");
    // The seeded concept is untouched by the targeted update.
    assert_eq!(
        cs.get("/a.md").await.unwrap().body.trim(),
        "zebras are striped"
    );

    // The rows finished in queue order too.
    let add_done = store.queue_item(uid, add_id).await.unwrap().unwrap();
    let update_done = store.queue_item(uid, update_id).await.unwrap().unwrap();
    assert_eq!(add_done.status, QueueStatus::Done);
    assert_eq!(update_done.status, QueueStatus::Done);
    assert!(
        add_done.updated_at < update_done.updated_at,
        "add row reached done first: {add_done:?} vs {update_done:?}"
    );
}

// T11.11 [spec §7.11 — receipt transitions incl. running +
// final_paths]: the row is pending before any drain, flips through
// running while the drain is mid-item (observed by polling the store;
// the slow mock widens the claim→done window), and lands done with
// nonempty final_paths and detail. The MCP-surface receipt view of
// these states is T8.1's `receipt_transitions_and_scoping`
// (crates/mycelium-mcp/tests/mutation_queue_mcp.rs).
#[tokio::test]
async fn receipt_state_flips_pending_running_done() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t1111").await;
    let cs = ConceptStore::for_user(&store, uid, master.clone());

    // 400 ms mock delay: the running window is wide enough to poll.
    let (url, _mock) = mock_llm_slow(
        vec![
            text_response("recorded"),
            tool_call_response(
                "c1",
                "write_concept",
                serde_json::json!({
                    "path": "/notes/midrun.md",
                    "frontmatter": { "type": "Note", "title": "Mid Run" },
                    "body": "mid run body"
                }),
            ),
        ],
        Duration::from_millis(400),
    )
    .await;
    let (config, worker) = worker(&store, recovery(master.clone()), QueueLimits::default());
    config
        .set(
            "llm",
            &LlmConfig {
                url,
                model: "mock".into(),
                api_key: None,
            },
        )
        .await
        .unwrap();

    let id = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Add,
        serde_json::json!({ "content": "watch me run" }),
        "watch me run",
    )
    .await;

    // BEFORE any drain: pending.
    let row = store.queue_item(uid, id).await.unwrap().unwrap();
    assert_eq!(row.status, QueueStatus::Pending);

    // Drain in the background; poll the row through its transitions.
    let drained = tokio::spawn({
        let worker = Arc::clone(&worker);
        async move { worker.run_pending().await }
    });
    let mut saw_running = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let row = store.queue_item(uid, id).await.unwrap().unwrap();
        match row.status {
            QueueStatus::Running => saw_running = true,
            QueueStatus::Done => break,
            QueueStatus::Pending => {}
            other => panic!("unexpected mid-drain status {other:?}"),
        }
        assert!(
            Instant::now() < deadline,
            "drain never reached done (saw_running={saw_running})"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        saw_running,
        "the running state must be observable mid-drain"
    );
    assert_eq!(drained.await.unwrap().unwrap(), 1);

    let done = store.queue_item(uid, id).await.unwrap().unwrap();
    assert_eq!(done.status, QueueStatus::Done);
    let paths: Vec<String> = serde_json::from_str(done.final_paths.as_deref().unwrap()).unwrap();
    assert_eq!(paths, vec!["/notes/midrun.md".to_string()]);
    assert!(!done.detail.is_empty(), "agent summary as detail: {done:?}");
}

// T11.12 [spec §7.12 + §9.5 — query priority invariant]: with both
// LLM permits held by in-flight query runs (and a third query queued
// behind them), the drain's wake defers immediately — run_pending
// returns Ok(0) without claiming (no attempt burned) and without
// blocking; once the queries finish, the drain claims and integrates.
#[tokio::test]
async fn drain_defers_while_queries_wait() {
    let _g = TEST_LOCK.lock().await;
    let (_dir, store) = test_store().await;
    let master = mycelium_crypto::generate_master_key();
    let uid = seed_user(&store, "t1112").await;
    let cs = ConceptStore::for_user(&store, uid, master.clone());

    // Three slow query runs: two hold the permits (inside the LLM
    // call), the third waits on acquire — the exact state the drain
    // must never preempt.
    let (url, _mock) = mock_llm_slow(
        vec![
            text_response("answer three"),
            text_response("answer two"),
            text_response("answer one"),
        ],
        Duration::from_millis(400),
    )
    .await;
    let client = LlmClient::new(&LlmConfig {
        url,
        model: "mock".into(),
        api_key: None,
    });
    let mut handles = Vec::new();
    for q in ["first question", "second question", "third question"] {
        let client = client.clone();
        let store = Arc::clone(&store);
        let master = master.clone();
        handles.push(tokio::spawn(async move {
            let cs = ConceptStore::for_user(&store, uid, master);
            let scopes = no_scopes();
            agent::run_query(&client, &cs, &scopes, q).await
        }));
    }
    // Synchronize on the actual permit state, not on a fixed sleep:
    // the drain must defer EXACTLY while both permits are held, and
    // available_permits()==0 is the observable form of that state. A
    // sleep races the mock's 400ms hold — on a slow runner the queries
    // can finish and free the permits before the drain runs, which
    // once failed CI (drain legitimately claimed, Ok(1) != 0).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while agent::LLM_RUNS.available_permits() > 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(
            tokio::time::Instant::now() < deadline,
            "query runs never acquired both permits"
        );
    }

    let limits = QueueLimits {
        age_deadline_secs: 0,
        ..QueueLimits::default()
    };
    let (config, worker) = worker(&store, recovery(master.clone()), limits);
    config.set("llm", &dead_port_config()).await.unwrap();
    let id = enqueue_item(
        &store,
        &cs,
        &master,
        uid,
        QueueTool::Add,
        serde_json::json!({ "content": "deferred behind queries" }),
        "deferred behind queries",
    )
    .await;

    // The starved wake: no permit → no claim, and it returns promptly
    // (timeout proves the drain never blocks on the semaphore).
    let n = tokio::time::timeout(Duration::from_secs(2), worker.run_pending())
        .await
        .expect("run_pending must never block on the semaphore")
        .unwrap();
    assert_eq!(n, 0, "no claim without a permit");
    let row = store.queue_item(uid, id).await.unwrap().unwrap();
    assert_eq!(row.status, QueueStatus::Pending);
    assert_eq!(
        row.attempts, 0,
        "a starved wake may not burn an attempt: {row:?}"
    );

    // The queries all complete — the waiting one got the freed permit,
    // not the drain.
    for h in handles {
        let result = h.await.unwrap().unwrap();
        assert!(!result.answer.is_empty());
    }

    // Permits free: the drain claims and integrates (deadline 0 → the
    // fallback; the queries consumed the mock's script).
    assert_eq!(worker.run_pending().await.unwrap(), 1);
    let done = store.queue_item(uid, id).await.unwrap().unwrap();
    assert_eq!(done.status, QueueStatus::Done);
}
