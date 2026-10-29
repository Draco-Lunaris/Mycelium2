//! Chat resume worker integration (spec §3.4/§6 cases 8–10): the boot
//! sweep flips interrupted runs resumable and the worker resumes each
//! ONCE — through the same runner + registry the submit endpoints use,
//! under the query-priority (`LLM_GATE.try_enter`) and key-recovery
//! gates. The worker is NOT spawned by `boot()` here — tests call
//! `run_pending` directly (one deterministic pass each).
//!
//! Every pass consumes capacity from the process-global
//! `llm_gate::LLM_GATE` (the resume worker's try class), and the
//! query-priority test holds its slots outright, so all tests here
//! serialize on TEST_LOCK and restore the gate's defaults through
//! [`GateDefaults`] — a held slot leaking into a sibling pass would
//! make that pass defer and the sibling fail. The backend-parking
//! tests (outage spec §3.3, the section at the end) additionally read
//! and drive the process-global `llm_health::LLM_HEALTH`, so every test
//! that touches it starts from `reset_for_test()` and restores Green
//! through [`HealthDefaults`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::response::IntoResponse;

use mycelium_auth::login::LoginService;
use mycelium_librarian::llm_gate::{
    DEFAULT_CAPACITY, DEFAULT_CHAT_QUEUE_DEPTH, DEFAULT_PER_USER_ACTIVE, LLM_GATE,
};
use mycelium_librarian::llm_health::{BackendState, LLM_HEALTH};
use mycelium_store::Store;
use mycelium_store::chat::{ChatRole, ChatTurnStatus};
use mycelium_web::AppState;
use mycelium_web::chat_runner::{ChatRunContext, ChatRunRegistry, spawn_chat_run};

/// Known admin password for the pre-seeded account (tests log in with it).
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";

/// Serializes all tests in this binary around the shared LLM gate.
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Wait bound for the gate holds this suite takes.
const GATE_BOUND: Duration = Duration::from_secs(10);

/// Restores the process-global gate's tunables on drop, so a failing
/// assertion cannot leak a reconfigured gate into the next test.
struct GateDefaults;

impl Drop for GateDefaults {
    fn drop(&mut self) {
        LLM_GATE.set_capacity(DEFAULT_CAPACITY);
        LLM_GATE.set_per_user_active(DEFAULT_PER_USER_ACTIVE);
        LLM_GATE.set_chat_queue_depth(DEFAULT_CHAT_QUEUE_DEPTH);
    }
}

/// Restores the process-global health registry to a fresh Green on
/// drop (LLM outage spec §3.1): the runner's parking rules read it —
/// the admission check acts on `Red`, the run-failure check on
/// `Disabled` — so a leaked non-Green state (a 502 mock's passive
/// failure note, a forced Red, a pinned Disabled) would make a sibling
/// test's turn park instead of reaching its own terminal outcome.
/// Declared after the TEST_LOCK guard so the restore happens while the
/// lock is still held.
struct HealthDefaults;

impl Drop for HealthDefaults {
    fn drop(&mut self) {
        LLM_HEALTH.reset_for_test();
    }
}

/// A mock that refuses every completion with HTTP 502 — the
/// backend-down class (outage spec §3.2): the agent's first request
/// fails with `LlmError::Status { status: 502, .. }`.
async fn status_502_mock() -> String {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|_body: String| async move {
            (axum::http::StatusCode::BAD_GATEWAY, "upstream unavailable").into_response()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/v1")
}

/// Build the detached runner's context directly — the store-driven
/// counterpart of the endpoint's `submit_turn` / the resume worker's
/// spawn — with the caller's own live-run registry, so the parking
/// tests can watch `register`/`remove` without an HTTP server.
fn run_ctx(state: &AppState, runs: Arc<ChatRunRegistry>, llm_url: String) -> ChatRunContext {
    ChatRunContext {
        store: (*state.store).clone(),
        service_key: (*state.service_key).clone(),
        chat: (*state.chat_sessions).clone(),
        runs,
        llm: mycelium_web::LlmConfig {
            url: llm_url,
            model: "mock".into(),
            api_key: None,
            ..Default::default()
        },
        scopes_visible: None,
    }
}

/// The error-role rows of a decrypted transcript (D4's invariant:
/// exactly one on a terminal path, exactly none on a park).
fn error_rows(
    rows: &[mycelium_store::chat::ChatMessageRow],
) -> Vec<&mycelium_store::chat::ChatMessageRow> {
    rows.iter()
        .filter(|r| matches!(r.role, ChatRole::Error))
        .collect()
}

/// A scripted mock LLM: pops responses LIFO.
async fn mock_llm(responses: Vec<serde_json::Value>) -> String {
    let state = Arc::new(tokio::sync::Mutex::new(responses));
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |body: String| {
            let state = state.clone();
            async move {
                let _ = body;
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
    format!("http://{addr}/v1")
}

/// An ungated mock that refuses every request at the TCP level is just
/// a dead URL — `http://127.0.0.1:1/v1` (port 1: connection refused
/// instantly, no listener to time out against).
const DEAD_LLM_URL: &str = "http://127.0.0.1:1/v1";

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

/// A mock that answers EVERY completion with a tool call (the step-cap
/// mock: the agent walks to MAX_STEPS and errors StepCapExceeded).
async fn endless_tool_mock() -> String {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |_body: String| async move {
            axum::Json(tool_call_response(
                "call-loop",
                "search_knowledge",
                serde_json::json!({ "query": "loop" }),
            ))
            .into_response()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/v1")
}

async fn boot(llm_url: String) -> (AppState, tempfile::TempDir) {
    boot_with(mycelium_web::LlmConfig {
        url: llm_url,
        model: "mock".into(),
        api_key: None,
        ..Default::default()
    })
    .await
}

/// `boot` with the full stored "llm" row, so a test can set the gate
/// tunables the runner syncs from (`max_parallel`, `per_user_active`,
/// `chat_wait_secs`) the way the admin page does.
async fn boot_with(llm: mycelium_web::LlmConfig) -> (AppState, tempfile::TempDir) {
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
    let login = LoginService::new(store.pool().clone());
    let state = AppState::new(
        store,
        service_key,
        login,
        assets_dir,
        dir.path().to_path_buf(),
        None,
    );
    // Point the librarian at the mock LLM (the config row is what the
    // resume pass decrypts, same as the submit endpoints).
    state.config.set("llm", &llm).await.unwrap();

    // Login populates the master-key cache + the service seal — the
    // production state where `master_key_for` succeeds. The tests
    // drive the store directly, so the rows must decrypt under the
    // key the cache holds: insert the fixed key this file seals with.
    let admin = admin_id(&state).await;
    state.master_keys.insert(admin, admin_master());
    let https: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let http: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let _ = (https, http);
    (state, dir)
}

/// The pre-seeded admin's id.
async fn admin_id(state: &AppState) -> uuid::Uuid {
    state.users.get_by_username("admin").await.unwrap().id
}

/// A fixed master key the admin's chat rows were sealed under —
/// the tests drive the store directly (no HTTP round trip), so the
/// rows just have to decrypt under the key the cache holds.
fn admin_master() -> mycelium_crypto::keys::MasterKey {
    mycelium_crypto::keys::MasterKey::from_bytes(&[7u8; 32]).unwrap()
}

// ---- the store-driving helpers (no HTTP: the resume path is
// worker-in, store-out; the endpoint seams are pinned elsewhere) ----

/// Create a session + submit a message directly through the store
/// (the turn starts `running`, like the endpoint's submit leaves it).
async fn submit_message(state: &AppState, user_id: uuid::Uuid, message: &str) -> uuid::Uuid {
    let master = state
        .master_key_for(user_id)
        .await
        .expect("master key for submit");
    let session_id = state
        .chat_sessions
        .create_session(user_id, &master, message)
        .await
        .unwrap();
    state
        .chat_sessions
        .create_turn(user_id, &master, session_id, message)
        .await
        .unwrap();
    session_id
}

/// Read the session's latest turn row.
async fn latest_turn(
    state: &AppState,
    user_id: uuid::Uuid,
    session_id: uuid::Uuid,
) -> mycelium_store::chat::ChatTurnRow {
    state
        .chat_sessions
        .latest_turn(user_id, session_id)
        .await
        .unwrap()
        .expect("turn row exists")
}

/// Every transcript row of the session (decrypted) under the
/// caller's key — `transcript`'s key-free form for users whose
/// `master_key_for` must NOT be touched (the key-unavailable case).
async fn transcript_under(
    state: &AppState,
    user_id: uuid::Uuid,
    master: &mycelium_crypto::keys::MasterKey,
    session_id: uuid::Uuid,
) -> Vec<mycelium_store::chat::ChatMessageRow> {
    state
        .chat_sessions
        .messages_after(user_id, master, session_id, 0)
        .await
        .unwrap()
}
async fn transcript(
    state: &AppState,
    user_id: uuid::Uuid,
    session_id: uuid::Uuid,
) -> Vec<mycelium_store::chat::ChatMessageRow> {
    let master = state.master_key_for(user_id).await.unwrap();
    state
        .chat_sessions
        .messages_after(user_id, &master, session_id, 0)
        .await
        .unwrap()
}

/// Poll a store condition with a deadline — never a bare sleep (the
/// queue_worker_integration.rs ci-flake rule: the poll target is
/// observable state — the turn row / the transcript — not a timer).
async fn poll_until<F, Fut>(mut want: F, what: &str)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if want().await {
            return;
        }
        assert!(Instant::now() < deadline, "never saw {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Whether this error is SQLite's transient write-lock contention
/// (`SQLITE_BUSY`) — sqlx applies a 5 s busy timeout, so it surfaces
/// only when the parallel test runners' combined I/O load (step-cap
/// mock traffic, WAL checkpointing) exceeds it. A retryable event,
/// never a logic failure: the harness treats it like the ci-flake
/// precedent (poll the observable with a deadline).
fn is_busy(e: &mycelium_store::chat::ChatError) -> bool {
    matches!(e, mycelium_store::chat::ChatError::Db(sqlx::Error::Database(d))
        if d.code().as_deref() == Some("5"))
}

/// Run a store write with SQLITE_BUSY retries until the deadline —
/// the harness counterpart of SQLite's own busy handler (its 5 s
/// budget is not always enough under the full parallel suite's I/O).
async fn with_busy_retry<T, F, Fut>(what: &str, f: F) -> T
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, mycelium_store::chat::ChatError>>,
{
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match f().await {
            Ok(v) => return v,
            Err(e) if is_busy(&e) => {
                assert!(
                    Instant::now() < deadline,
                    "sqlite stayed busy for 15s during {what}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("{what} failed: {e}"),
        }
    }
}

#[tokio::test]
async fn restart_marks_interrupted_and_resumes_once() {
    let _g = TEST_LOCK.lock().await;
    // Mock: [answer] — the resume run consumes exactly one completion.
    let llm_url = mock_llm(vec![text_response("resumed reply")]).await;
    let (state, _dir) = boot(llm_url).await;
    let user = admin_id(&state).await;

    // Submit: session + running turn (endpoint shape, store-driven).
    let session = submit_message(&state, user, "hello after restart").await;
    assert_eq!(
        latest_turn(&state, user, session).await.status,
        ChatTurnStatus::Running
    );

    // Simulate the crash (what main.rs calls at boot): running →
    // interrupted, attempts still 0 — resumable.
    state.chat_sessions.boot_sweep().await.unwrap();
    let turn = latest_turn(&state, user, session).await;
    assert_eq!(turn.status, ChatTurnStatus::Interrupted);
    assert_eq!(turn.attempts, 0);

    // The worker pass (mock primed with the answer) claims + runs it.
    mycelium_web::chat_resume::ChatResumeWorker::new(Arc::new(state.clone()))
        .run_pending()
        .await
        .unwrap();
    poll_until(
        || async { latest_turn(&state, user, session).await.status == ChatTurnStatus::Done },
        "resumed turn done",
    )
    .await;
    let turn = latest_turn(&state, user, session).await;
    assert_eq!(turn.attempts, 1, "resume-once: exactly one attempt");
    let rows = transcript(&state, user, session).await;
    let assistant: Vec<_> = rows
        .iter()
        .filter(|r| matches!(r.role, ChatRole::Assistant))
        .collect();
    assert_eq!(assistant.len(), 1, "the reply landed in the transcript");
    assert_eq!(assistant[0].content, "resumed reply");

    // A second sweep + pass cycle changes nothing (attempts = 1 →
    // claim_resumable false — no second claim, no re-answer). The
    // observable: resumable_turns() is EMPTY — the store's own
    // claim-guard invariant, checked directly (never a sleep).
    state.chat_sessions.boot_sweep().await.unwrap();
    mycelium_web::chat_resume::ChatResumeWorker::new(Arc::new(state.clone()))
        .run_pending()
        .await
        .unwrap();
    assert!(
        state
            .chat_sessions
            .resumable_turns()
            .await
            .unwrap()
            .is_empty(),
        "a swept-and-resumed turn is never listed again"
    );
    assert_eq!(
        latest_turn(&state, user, session).await.status,
        ChatTurnStatus::Done
    );
    assert_eq!(latest_turn(&state, user, session).await.attempts, 1);
    let rows2 = transcript(&state, user, session).await;
    assert_eq!(
        rows2
            .iter()
            .filter(|r| matches!(r.role, ChatRole::Assistant))
            .count(),
        1,
        "no second run — the assistant reply is exactly one row"
    );
}

#[tokio::test]
async fn resume_llm_dead_parks_as_waiting() {
    let _g = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    LLM_HEALTH.reset_for_test();
    // Dead URL (connection refused instantly) on a RESUMED turn: a
    // backend-down run is weather, not a verdict (outage spec §3.3) —
    // the turn parks `waiting_backend` instead of the pre-outage
    // interrupted-final, with `attempts` left at 1 (out of the
    // crash-resume budget, into the unlimited weather pool) and NO
    // error row (parking rows write nothing). This assertion replaced
    // the pre-outage `interrupted + attempts = 1 + LLM-down row` pin
    // when the parking rule landed.
    let (state, _dir) = boot(DEAD_LLM_URL.to_string()).await;
    let user = admin_id(&state).await;
    let session = submit_message(&state, user, "asked while down").await;
    state.chat_sessions.boot_sweep().await.unwrap();
    assert_eq!(latest_turn(&state, user, session).await.attempts, 0);

    mycelium_web::chat_resume::ChatResumeWorker::new(Arc::new(state.clone()))
        .run_pending()
        .await
        .unwrap();
    poll_until(
        || async {
            latest_turn(&state, user, session).await.status == ChatTurnStatus::WaitingBackend
        },
        "waiting_backend after a dead-LLM resume",
    )
    .await;
    let parked = latest_turn(&state, user, session).await;
    assert_eq!(parked.attempts, 1, "the spent crash-resume attempt is kept");
    let rows = transcript(&state, user, session).await;
    assert_eq!(
        error_rows(&rows).len(),
        0,
        "a park writes no error row (spec §3.3 parking rows)"
    );

    // The crash-resume claim path never lists it again (waiting turns
    // are the resume worker's `claim_waiting` supply — Task 5 — not the
    // once-guard's), and a second pass is a transcript no-op. The
    // observable: the store's own listing + the transcript, checked
    // directly (never a sleep).
    let before = transcript(&state, user, session).await.len();
    mycelium_web::chat_resume::ChatResumeWorker::new(Arc::new(state.clone()))
        .run_pending()
        .await
        .unwrap();
    assert!(
        state
            .chat_sessions
            .resumable_turns()
            .await
            .unwrap()
            .is_empty(),
        "a waiting turn is never in the crash-resumable listing"
    );
    assert_eq!(
        transcript(&state, user, session).await.len(),
        before,
        "second pass no-op"
    );
    assert_eq!(latest_turn(&state, user, session).await.attempts, 1);
    assert!(turn_last_error(&state, parked.id).await.is_some());
}

#[tokio::test]
async fn resume_step_cap_marks_failed() {
    let _g = TEST_LOCK.lock().await;
    // Spec §6.10 split: a resumed turn that RUNS but hits the step cap
    // is `failed` (it genuinely ran; no retry), with an error row —
    // every completion is a tool call, so the agent walks MAX_STEPS.
    let llm_url = endless_tool_mock().await;
    let (state, _dir) = boot(llm_url).await;
    let user = admin_id(&state).await;
    let session = submit_message(&state, user, "run into the cap").await;
    state.chat_sessions.boot_sweep().await.unwrap();

    mycelium_web::chat_resume::ChatResumeWorker::new(Arc::new(state.clone()))
        .run_pending()
        .await
        .unwrap();
    poll_until(
        || async { latest_turn(&state, user, session).await.status == ChatTurnStatus::Failed },
        "step-capped resume failed",
    )
    .await;
    let turn = latest_turn(&state, user, session).await;
    assert_eq!(turn.attempts, 1, "the resume attempt is accounted");
    let rows = transcript(&state, user, session).await;
    assert!(
        rows.iter().any(|r| matches!(r.role, ChatRole::Tool)),
        "the run genuinely ran — tool rows present"
    );
    let errors: Vec<_> = rows
        .iter()
        .filter(|r| matches!(r.role, ChatRole::Error))
        .collect();
    assert_eq!(errors.len(), 1, "exactly one error row");
    assert!(
        errors[0].content.contains("step cap"),
        "the step-cap error surfaced: {}",
        errors[0].content
    );
}

#[tokio::test]
async fn resume_claim_vs_submit_race() {
    let _g = TEST_LOCK.lock().await;
    // Review Focus 4: the accepted race is closed by the store — once
    // the resume worker claims the turn (running), the user's submit
    // on the same session hits the running-turn unique index →
    // Conflict (the endpoint turns that into 409). Never two runs.
    let (state, _dir) = boot(DEAD_LLM_URL.to_string()).await;
    let user = admin_id(&state).await;
    let master = state.master_key_for(user).await.unwrap();
    let session = with_busy_retry("raced create_session", || async {
        state
            .chat_sessions
            .create_session(user, &master, "raced session")
            .await
    })
    .await;
    let turn = with_busy_retry("raced create_turn", || async {
        state
            .chat_sessions
            .create_turn(user, &master, session, "the interrupted question")
            .await
    })
    .await;

    with_busy_retry("raced boot sweep", || async {
        state.chat_sessions.boot_sweep().await
    })
    .await;

    // The worker's win: the atomic claim flips interrupted+0 → running+1.
    assert!(
        state
            .chat_sessions
            .claim_resumable(user, session, turn.id)
            .await
            .unwrap(),
        "first claim wins"
    );
    // The user's submit racing in: → Conflict (running unique index).
    let res = state
        .chat_sessions
        .create_turn(user, &master, session, "racing submit")
        .await;
    assert!(
        matches!(res, Err(mycelium_store::chat::ChatError::Conflict)),
        "submit after claim must Conflict, got {res:?}"
    );

    // The reverse order, in its own session: a submit on an UNCLAIMED
    // interrupted turn supersedes it (→ abandoned, D8) and runs — the
    // worker's later claim finds nothing resumable (the row is
    // `abandoned`), so the never-two-runs invariant holds either way.
    let session2 = with_busy_retry("superseded create_session", || async {
        state
            .chat_sessions
            .create_session(user, &master, "superseded session")
            .await
    })
    .await;
    let turn2 = with_busy_retry("superseded create_turn", || async {
        state
            .chat_sessions
            .create_turn(user, &master, session2, "superseded question")
            .await
    })
    .await;

    with_busy_retry("superseded boot sweep", || async {
        state.chat_sessions.boot_sweep().await
    })
    .await;
    let res2 = with_busy_retry("superseding create_turn", || async {
        state
            .chat_sessions
            .create_turn(user, &master, session2, "superseding question")
            .await
    })
    .await;
    assert_eq!(res2.user_seq, turn2.user_seq + 1, "the new message landed");
    assert!(
        !state
            .chat_sessions
            .claim_resumable(user, session2, turn2.id)
            .await
            .unwrap(),
        "an abandoned turn is never claimable"
    );
}

#[tokio::test]
async fn resume_defers_to_interactive_llm() {
    // Query-priority invariant: every LLM_GATE slot held → the pass
    // never CLAIMS (claim-after-admission ordering — nothing is held
    // while waiting); the turn stays interrupted with attempts 0.
    // Release, and the next pass claims + completes it.
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    let llm_url = mock_llm(vec![text_response("ran after release")]).await;
    let (state, _dir) = boot(llm_url).await;
    let user = admin_id(&state).await;
    let session = submit_message(&state, user, "deferred question").await;
    state.chat_sessions.boot_sweep().await.unwrap();

    // Hold every gate slot — the wait class is what an interactive
    // chat turn uses, and the bound guards against a sibling that
    // leaked a slot (TEST_LOCK should already rule that out).
    let p1 = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("slot 1 free within the bound");
    let p2 = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("slot 2 free within the bound");
    let stats = LLM_GATE.stats();
    assert_eq!(
        stats.active, stats.capacity,
        "every slot held — the deferred pass must find none: {stats:?}"
    );
    mycelium_web::chat_resume::ChatResumeWorker::new(Arc::new(state.clone()))
        .run_pending()
        .await
        .unwrap();
    let turn = latest_turn(&state, user, session).await;
    assert_eq!(turn.status, ChatTurnStatus::Interrupted, "untouched");
    assert_eq!(turn.attempts, 0, "claim never happened");
    drop(p1);
    drop(p2);

    mycelium_web::chat_resume::ChatResumeWorker::new(Arc::new(state.clone()))
        .run_pending()
        .await
        .unwrap();
    poll_until(
        || async { latest_turn(&state, user, session).await.status == ChatTurnStatus::Done },
        "resume completes after permits free",
    )
    .await;
    assert_eq!(latest_turn(&state, user, session).await.attempts, 1);
    poll_until(
        || async {
            transcript(&state, user, session)
                .await
                .iter()
                .any(|r| matches!(r.role, ChatRole::Assistant))
        },
        "reply persisted",
    )
    .await;
}

#[tokio::test]
async fn resumed_turn_waits_on_the_gate() {
    // R17 / LLM concurrency spec §3.3: admission lives in the runner,
    // the single path for fresh AND resumed turns — so a RESUMED turn
    // queues on the gate too, and reaches the LLM only once a slot
    // frees. The resume pass's own `try_enter` is a capacity CHECK it
    // drops past the spawn, so the binding limit here is the user's
    // own: `per_user_active = 1` plus a held per-user slot means the
    // runner's `enter(Some(user), …)` must queue — no timing race.
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    let llm_url = mock_llm(vec![text_response("resumed after capacity")]).await;
    let (state, _dir) = boot_with(mycelium_web::LlmConfig {
        url: llm_url,
        model: "mock".into(),
        api_key: None,
        max_parallel: 2,
        per_user_active: 1,
        chat_wait_secs: 60,
        ..Default::default()
    })
    .await;
    let user = admin_id(&state).await;
    let session = submit_message(&state, user, "interrupted at capacity").await;
    state.chat_sessions.boot_sweep().await.unwrap();

    // The user's one admitted run (held by this test), so the resumed
    // runner finds itself at its own cap. A free slot remains, which is
    // what lets the pass's try_enter + claim proceed.
    LLM_GATE.set_per_user_active(1);
    let hold = LLM_GATE
        .enter(Some(user), GATE_BOUND, |_| {})
        .await
        .expect("the user's slot is free before the pass");

    mycelium_web::chat_resume::ChatResumeWorker::new(Arc::new(state.clone()))
        .run_pending()
        .await
        .unwrap();
    // The claim happened (the pass had capacity), but the run is
    // waiting: the gate's queue is the observable.
    poll_until(
        || async { LLM_GATE.stats().queued >= 1 },
        "the resumed runner queued on the gate",
    )
    .await;
    let turn = latest_turn(&state, user, session).await;
    assert_eq!(turn.status, ChatTurnStatus::Running, "claimed and queued");
    assert_eq!(turn.attempts, 1, "the resume attempt is accounted");
    // Nothing ran: no agent row exists while the turn is queued.
    assert_eq!(
        transcript(&state, user, session)
            .await
            .iter()
            .filter(|r| !matches!(r.role, ChatRole::User))
            .count(),
        0,
        "a queued resumed turn has not reached the LLM"
    );

    // Free the user's slot: the queued runner is dispatched and the
    // resumed turn completes normally.
    drop(hold);
    poll_until(
        || async { latest_turn(&state, user, session).await.status == ChatTurnStatus::Done },
        "resumed turn done after the slot frees",
    )
    .await;
    let rows = transcript(&state, user, session).await;
    let assistant: Vec<_> = rows
        .iter()
        .filter(|r| matches!(r.role, ChatRole::Assistant))
        .collect();
    assert_eq!(assistant.len(), 1, "exactly one reply");
    assert_eq!(assistant[0].content, "resumed after capacity");
}

#[tokio::test]
async fn resume_key_unavailable_skips_user() {
    let _g = TEST_LOCK.lock().await;
    // A user with an unrecoverable key (garbage service seal): the
    // pass skips their turns (zero LLM cost) and stays Ok — the turns
    // are untouched and retried next tick.
    let llm_url = mock_llm(vec![]).await;
    let (state, _dir) = boot(llm_url).await;
    // The broken user: the row must satisfy FKs, its service seal must
    // not unseal — garbage hex is rejected before any AEAD runs.
    let broken = {
        let id = uuid::Uuid::new_v4();
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO users (id, username, email, role, auth_provider, password_hash, \
             sealed_master_key, master_key_service_sealed, must_change_password, created_at, updated_at) \
             VALUES (?, ?, ?, 'user', 'local', NULL, 'placeholder', 'zzzz-not-hex', 0, ?, ?)",
        )
        .bind(id.to_string())
        .bind("keyless")
        .bind("keyless@t.local")
        .bind(&now)
        .bind(&now)
        .execute(state.store.pool())
        .await
        .unwrap();
        id
    };
    // The resumable turn is attached to the broken user via the store
    // (needs a master key only for the sealed title/message bodies —
    // any key derives a consistent chat DEK).
    let keymaker = mycelium_crypto::generate_master_key();
    let session = state
        .chat_sessions
        .create_session(broken, &keymaker, "asked before the key broke")
        .await
        .unwrap();
    let turn = state
        .chat_sessions
        .create_turn(broken, &keymaker, session, "the question")
        .await
        .unwrap();
    state.chat_sessions.boot_sweep().await.unwrap();

    // master_key_for errors for this user (bad hex → decode error).
    assert!(state.master_key_for(broken).await.is_err());

    // The pass: Ok, and the user's turns are untouched.
    mycelium_web::chat_resume::ChatResumeWorker::new(Arc::new(state.clone()))
        .run_pending()
        .await
        .unwrap();
    let t = latest_turn(&state, broken, session).await;
    assert_eq!(t.id, turn.id, "same turn");
    assert_eq!(t.status, ChatTurnStatus::Interrupted, "still interrupted");
    assert_eq!(t.attempts, 0, "never claimed");
    // Zero LLM cost — the observable: an LLM request would have had
    // to claim first (the pass skips the user before any claim), and
    // a run would surface tool/error rows in the transcript; none
    // exist. Rows decrypt under the keymaker key (the one the rows
    // were sealed under — master_key_for stays untouched for this
    // user, that is the case being pinned).
    assert_eq!(
        transcript_under(&state, broken, &keymaker, session)
            .await
            .iter()
            .filter(|r| !matches!(r.role, ChatRole::User))
            .count(),
        0,
        "no agent rows — nothing ran"
    );
}

// ---------------------------------------------------------------------
// Backend-down parking (LLM outage spec §3.3): the runner treats the
// backend being down as weather, not a verdict — both fresh submits
// and resumed turns park as `waiting_backend` with NO transcript row,
// NO SSE event, and the live-run entry closed. Every test here drives
// the process-global health machine, so each takes TEST_LOCK and
// restores Green through HealthDefaults.
// ---------------------------------------------------------------------

/// The turn row's `last_error` (unsealed failure metadata, spec §3.6)
/// read straight from the store.
async fn turn_last_error(state: &AppState, turn_id: uuid::Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT last_error FROM chat_turns WHERE id = ?")
        .bind(turn_id.to_string())
        .fetch_one(state.store.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn resumed_run_backend_down_parks() {
    let _g = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    LLM_HEALTH.reset_for_test();
    // Resumed turn (attempts = 1, the once-claim already spent) hitting
    // a 502: backend-down class → park, NOT the interrupted-final the
    // pre-outage code wrote. The park never touches attempts.
    let llm_url = status_502_mock().await;
    let (state, _dir) = boot(llm_url.clone()).await;
    let user = admin_id(&state).await;
    let session = submit_message(&state, user, "resumed into an outage").await;
    state.chat_sessions.boot_sweep().await.unwrap();
    let first = latest_turn(&state, user, session).await;
    assert!(
        state
            .chat_sessions
            .claim_resumable(user, session, first.id)
            .await
            .unwrap(),
        "the crash-resume claim wins (attempts 0 → 1)"
    );
    let turn = latest_turn(&state, user, session).await;
    assert_eq!(turn.status, ChatTurnStatus::Running);
    assert_eq!(turn.attempts, 1);

    let runs = Arc::new(ChatRunRegistry::default());
    let ctx = run_ctx(&state, Arc::clone(&runs), llm_url);
    let turn_id = spawn_chat_run(
        ctx,
        user,
        admin_master(),
        session,
        turn,
        /* resumed */ true,
    )
    .await;

    poll_until(
        || async {
            latest_turn(&state, user, session).await.status == ChatTurnStatus::WaitingBackend
        },
        "the backend-down resume parked",
    )
    .await;
    let parked = latest_turn(&state, user, session).await;
    assert_eq!(parked.id, turn_id);
    assert_eq!(parked.attempts, 1, "parking never touches attempts");
    assert!(
        turn_last_error(&state, turn_id).await.is_some(),
        "the park records last_error"
    );
    // Weather leaves no trace in the transcript: the trigger user row
    // is the ONLY row (spec §3.3 parking rows).
    let rows = transcript(&state, user, session).await;
    assert_eq!(error_rows(&rows).len(), 0, "no error row on a park");
    assert_eq!(
        rows.iter()
            .filter(|r| !matches!(r.role, ChatRole::User))
            .count(),
        0,
        "no transcript rows at all beyond the trigger"
    );
    poll_until(
        || async { !runs.contains(turn_id) },
        "the registry entry closed on the park",
    )
    .await;
}

#[tokio::test]
async fn fresh_run_backend_down_parks() {
    let _g = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    LLM_HEALTH.reset_for_test();
    // A FRESH run whose agent error is the backend-down class parks too
    // — no `failed`, no error row. The turn is health Green (not
    // Disabled), so the admission check does not fire and the run
    // reaches the gate + the mock before the 502.
    let llm_url = status_502_mock().await;
    let (state, _dir) = boot(llm_url.clone()).await;
    let user = admin_id(&state).await;
    let session = submit_message(&state, user, "fresh into an outage").await;
    let turn = latest_turn(&state, user, session).await;

    let runs = Arc::new(ChatRunRegistry::default());
    let ctx = run_ctx(&state, Arc::clone(&runs), llm_url);
    let turn_id = spawn_chat_run(
        ctx,
        user,
        admin_master(),
        session,
        turn,
        /* resumed */ false,
    )
    .await;

    poll_until(
        || async {
            latest_turn(&state, user, session).await.status == ChatTurnStatus::WaitingBackend
        },
        "the backend-down fresh run parked",
    )
    .await;
    let parked = latest_turn(&state, user, session).await;
    assert_eq!(parked.id, turn_id);
    assert_eq!(
        parked.attempts, 0,
        "a parked fresh turn keeps its resume budget"
    );
    assert!(turn_last_error(&state, turn_id).await.is_some());
    let rows = transcript(&state, user, session).await;
    assert_eq!(
        error_rows(&rows).len(),
        0,
        "no failed row — it is not a verdict"
    );
    poll_until(
        || async { !runs.contains(turn_id) },
        "the registry entry closed on the park",
    )
    .await;
}

#[tokio::test]
async fn fresh_run_hard_error_still_fails() {
    let _g = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    LLM_HEALTH.reset_for_test();
    // Invariant pin (D4 + §3.2): a run that genuinely ran and hit the
    // step cap is a real outcome, NOT weather — `failed` with exactly
    // one error row, unchanged by the parking rules.
    let llm_url = endless_tool_mock().await;
    let (state, _dir) = boot(llm_url.clone()).await;
    let user = admin_id(&state).await;
    let session = submit_message(&state, user, "run into the cap fresh").await;
    let turn = latest_turn(&state, user, session).await;

    let runs = Arc::new(ChatRunRegistry::default());
    let ctx = run_ctx(&state, Arc::clone(&runs), llm_url);
    let turn_id = spawn_chat_run(
        ctx,
        user,
        admin_master(),
        session,
        turn,
        /* resumed */ false,
    )
    .await;

    poll_until(
        || async { latest_turn(&state, user, session).await.status == ChatTurnStatus::Failed },
        "the step-capped fresh run failed",
    )
    .await;
    let rows = transcript(&state, user, session).await;
    let errors = error_rows(&rows);
    assert_eq!(errors.len(), 1, "exactly one error row on a hard failure");
    assert!(
        errors[0].content.contains("step cap"),
        "the step-cap error surfaced: {}",
        errors[0].content
    );
    assert!(!runs.contains(turn_id));
}

#[tokio::test]
async fn parking_writes_no_transcript_row() {
    let _g = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    LLM_HEALTH.reset_for_test();
    for _ in 0..3 {
        LLM_HEALTH.note_failure();
    }
    assert_eq!(
        LLM_HEALTH.snapshot().state,
        BackendState::Red,
        "Red at both parks"
    );
    // The multi-retry guard (spec §3.3 parking rows): park → claim →
    // park again — a multi-hour outage re-parks on every retry tick,
    // so parking must append NOTHING. Through two full park cycles the
    // transcript holds only the trigger message.
    let (state, _dir) = boot(DEAD_LLM_URL.to_string()).await;
    let user = admin_id(&state).await;
    let session = submit_message(&state, user, "asked during a long outage").await;
    let turn = latest_turn(&state, user, session).await;

    // Park #1 (admission check: Red → park before the gate).
    let runs = Arc::new(ChatRunRegistry::default());
    let ctx = run_ctx(&state, Arc::clone(&runs), DEAD_LLM_URL.to_string());
    let turn_id = spawn_chat_run(ctx, user, admin_master(), session, turn, false).await;
    poll_until(
        || async {
            latest_turn(&state, user, session).await.status == ChatTurnStatus::WaitingBackend
        },
        "the first park",
    )
    .await;
    let after_first = transcript(&state, user, session).await;
    assert_eq!(error_rows(&after_first).len(), 0, "park #1 wrote no row");
    assert_eq!(after_first.len(), 1, "only the trigger message");

    // The weather claim (Task 3's `claim_waiting` — does not touch
    // attempts), then a second runner pass in the still-Red machine.
    assert!(
        state
            .chat_sessions
            .claim_waiting(user, session, turn_id)
            .await
            .unwrap(),
        "a waiting turn is claimable again"
    );
    let r2 = latest_turn(&state, user, session).await;
    assert_eq!(r2.status, ChatTurnStatus::Running);
    let ctx2 = run_ctx(&state, Arc::clone(&runs), DEAD_LLM_URL.to_string());
    spawn_chat_run(ctx2, user, admin_master(), session, r2, false).await;
    poll_until(
        || async {
            latest_turn(&state, user, session).await.status == ChatTurnStatus::WaitingBackend
        },
        "the second park",
    )
    .await;
    let after_second = transcript(&state, user, session).await;
    assert_eq!(
        error_rows(&after_second).len(),
        0,
        "park #2 wrote no row either"
    );
    assert_eq!(
        after_second.len(),
        1,
        "still only the trigger message after two parks"
    );
    let parked = latest_turn(&state, user, session).await;
    assert_eq!(
        parked.attempts, 0,
        "parking is not a claim — attempts untouched"
    );
    assert!(turn_last_error(&state, turn_id).await.is_some());
}

#[tokio::test]
async fn disabled_llm_fails_fast() {
    let _g = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    LLM_HEALTH.reset_for_test();
    LLM_HEALTH.set_disabled();
    // "Not configured" is not weather: it will not fix itself, and
    // parking it would promise a resume that never comes. Disabled
    // keeps today's exact terminal outcomes on the SAME dead-LLM input
    // the parking tests use — fresh → `failed` + the LLM_UNAVAILABLE
    // row; resumed → `interrupted` (attempts 1 = final) + row.
    let (state, _dir) = boot(DEAD_LLM_URL.to_string()).await;
    let user = admin_id(&state).await;

    // Fresh submit: failed with the configure message.
    let session = submit_message(&state, user, "asked with no backend").await;
    let turn = latest_turn(&state, user, session).await;
    let runs = Arc::new(ChatRunRegistry::default());
    let ctx = run_ctx(&state, Arc::clone(&runs), DEAD_LLM_URL.to_string());
    spawn_chat_run(ctx, user, admin_master(), session, turn, false).await;
    poll_until(
        || async { latest_turn(&state, user, session).await.status == ChatTurnStatus::Failed },
        "the fresh disabled-backend turn failed",
    )
    .await;
    let rows = transcript(&state, user, session).await;
    let errors = error_rows(&rows);
    assert_eq!(errors.len(), 1, "the fresh path keeps its one error row");
    assert!(
        errors[0].content.contains("unavailable"),
        "the user-facing configure message: {}",
        errors[0].content
    );
    assert_eq!(
        latest_turn(&state, user, session).await.status,
        ChatTurnStatus::Failed,
        "Disabled never parks"
    );

    // Resumed turn: back to `interrupted` (the once-budget spent),
    // with its row — again no parking.
    let session2 = submit_message(&state, user, "asked before the shutdown").await;
    state.chat_sessions.boot_sweep().await.unwrap();
    let first = latest_turn(&state, user, session2).await;
    assert!(
        state
            .chat_sessions
            .claim_resumable(user, session2, first.id)
            .await
            .unwrap()
    );
    let turn2 = latest_turn(&state, user, session2).await;
    let ctx2 = run_ctx(&state, Arc::clone(&runs), DEAD_LLM_URL.to_string());
    spawn_chat_run(
        ctx2,
        user,
        admin_master(),
        session2,
        turn2,
        /* resumed */ true,
    )
    .await;
    poll_until(
        || async {
            let t = latest_turn(&state, user, session2).await;
            t.status == ChatTurnStatus::Interrupted && t.attempts == 1
        },
        "the resumed disabled-backend turn is interrupted-final",
    )
    .await;
    let rows2 = transcript(&state, user, session2).await;
    assert_eq!(
        error_rows(&rows2).len(),
        1,
        "the resumed path keeps its row too"
    );
}
