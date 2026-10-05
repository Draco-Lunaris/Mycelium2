//! Mutation-queue drain worker integration tests: temp-dir store, mock
//! OpenAI-compatible LLM (copied from agent_integration.rs — the mock
//! helpers are file-local per test binary), stub master-key recovery.
//!
//! `agent::LLM_RUNS` is process-global with exactly 2 permits, so every
//! test in this file serializes on TEST_LOCK (T5.3 deliberately
//! starves all permits).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::response::IntoResponse;
use mycelium_core::concept::{Concept, Frontmatter};
use mycelium_crypto::keys::{MasterKey, ServiceKey};
use mycelium_librarian::agent;
use mycelium_librarian::fallback::derive_title;
use mycelium_librarian::llm::LlmConfig;
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
    }
}

/// Build a worker over the store (fresh ConfigStore the test can seed).
fn worker(
    store: &Arc<Store>,
    recovery: Arc<MasterKeyRecovery>,
    limits: QueueLimits,
) -> (Arc<ConfigStore>, Arc<QueueWorker>) {
    let config = Arc::new(ConfigStore::new(store.pool().clone()));
    let service_key = Arc::new(ServiceKey::from_bytes(&[9u8; 32]).unwrap());
    let w = Arc::new(QueueWorker::new(
        Arc::clone(store),
        service_key,
        Arc::clone(&config),
        recovery,
        limits,
    ));
    (config, w)
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
