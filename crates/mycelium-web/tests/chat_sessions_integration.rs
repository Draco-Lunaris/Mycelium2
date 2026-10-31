//! Chat session history integration: the session-scoped chat endpoints
//! (draft-create + submit SSE streams, reattach, session list/meta,
//! delete) with auth, CSRF, and per-user ownership enforced (spec
//! §3.5/§6). Uses mock OpenAI-compatible LLMs — scripted, gated (a
//! watch channel holds the first request so a turn can be observed
//! mid-run), and request-capturing (multi-turn payload pins) — pointed
//! at via the ConfigStore "llm" key.
//!
//! Every chat turn now admits on the process-global
//! `llm_gate::LLM_GATE` (LLM concurrency spec §3.3 — the runner is the
//! chat surface's single admission point), so a turn here consumes gate
//! capacity: ALL tests in this binary serialize on [`TEST_LOCK`], and
//! the ones that hold slots or reconfigure the gate restore its
//! defaults through [`GateDefaults`] (declared after the lock guard, so
//! the restore happens while the lock is still held).

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
use mycelium_web::AppState;
use mycelium_web::chat_api::CHAT_QUEUE_FULL_MSG;
use mycelium_web::chat_runner::CHAT_CAPACITY_MSG;

/// Known admin password for the pre-seeded account (tests log in with it).
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";

/// Password for the second (non-admin) user minted by the ownership test.
const SECOND_PASSWORD: &str = "second user password known to the test suite!";

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

/// Restores the process-global health registry to a fresh Green on drop
/// (LLM outage spec §3.1): the runner parks a Red-backend turn at
/// admission, so a leaked Red would park a sibling test's turn instead
/// of running it. Declared after the TEST_LOCK guard so the restore
/// happens while the lock is still held.
struct HealthDefaults;

impl Drop for HealthDefaults {
    fn drop(&mut self) {
        LLM_HEALTH.reset_for_test();
    }
}

/// Poll a gate/stats condition with a deadline (never a bare sleep —
/// the poll target is observable gate state).
async fn poll_gate_until<F: FnMut() -> bool>(mut want: F, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if want() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "never saw {what}: {:?}",
            LLM_GATE.stats()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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

/// A gated scripted mock LLM: every request is held until the watch
/// gate flips true, then answered from the same LIFO script. The gate
/// stays open, so later requests of the same run (e.g. the second call
/// after a tool result) pass immediately.
async fn gated_mock_llm(
    responses: Vec<serde_json::Value>,
) -> (String, tokio::sync::watch::Sender<bool>) {
    let state = Arc::new(tokio::sync::Mutex::new(responses));
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let gate = Arc::new(gate_rx);
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |body: String| {
            let state = state.clone();
            let gate = gate.clone();
            async move {
                let _ = body;
                let mut g = gate.as_ref().clone();
                while !*g.borrow_and_update() {
                    if g.changed().await.is_err() {
                        break;
                    }
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
    (format!("http://{addr}/v1"), gate_tx)
}

/// A request-capturing scripted mock LLM: records every request body,
/// answers from the LIFO script (the multi-turn-history pin).
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

async fn boot(
    llm_url: String,
) -> (
    String,
    tokio_util::sync::CancellationToken,
    tempfile::TempDir,
) {
    boot_with(mycelium_web::LlmConfig {
        url: llm_url,
        model: "mock".into(),
        api_key: None,
        ..Default::default()
    })
    .await
}

/// `boot` with the full stored "llm" row, so a test can set the gate
/// tunables the runner syncs from (`max_parallel`, `chat_wait_secs`, …)
/// the way the admin page does — a config write, not a code change.
async fn boot_with(
    llm: mycelium_web::LlmConfig,
) -> (
    String,
    tokio_util::sync::CancellationToken,
    tempfile::TempDir,
) {
    let (base, shutdown, dir, _serve) = boot_with_handle(llm).await;
    // The handle is dropped (the task detaches, as it always has);
    // only the shutdown test needs to observe `serve` returning.
    (base, shutdown, dir)
}

/// [`boot_with`] that also hands back the `serve` task's join handle,
/// so a test can assert the serve future itself COMPLETES after the
/// shutdown token fires (the LLM-gate close path, spec §3.5).
async fn boot_with_handle(
    llm: mycelium_web::LlmConfig,
) -> (
    String,
    tokio_util::sync::CancellationToken,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
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
    // Point the librarian at the mock LLM.
    state.config.set("llm", &llm).await.unwrap();

    let https: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let http: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();

    let listener = tokio::net::TcpListener::bind(https).await.unwrap();
    let https_port = listener.local_addr().unwrap().port();
    drop(listener);

    let data_dir = dir.path().to_path_buf();
    let token = shutdown.clone();
    let serve = tokio::spawn(async move {
        let _ = mycelium_web::serve(
            state,
            &data_dir,
            format!("127.0.0.1:{https_port}").parse().unwrap(),
            http,
            None,
            None,
            vec!["localhost".to_string()],
            token,
        )
        .await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    (
        format!("https://127.0.0.1:{https_port}"),
        shutdown,
        dir,
        serve,
    )
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
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

/// Log in as the pre-seeded admin; returns (cookie, csrf).
async fn login_admin(client: &reqwest::Client, base: &str) -> (String, String) {
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", ADMIN_PASSWORD)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
    let cookie = cookie_from(&login);
    let csrf = csrf_from_page(client, &format!("{base}/"), &cookie).await;
    (cookie, csrf)
}

/// Parse an SSE body into its JSON events (one `data:` object per event).
fn sse_events(body: &str) -> Vec<serde_json::Value> {
    body.split("\n\n")
        .filter(|chunk| !chunk.trim().is_empty())
        .map(|chunk| {
            let data = chunk.strip_prefix("data: ").expect("sse data line");
            serde_json::from_str(data).expect("valid sse json")
        })
        .collect()
}

/// Incremental SSE reader over a still-open response body: parses
/// complete frames as the chunks arrive, so a test can observe an event
/// MID-stream. The queue events (`queued`/`admitted`) are live
/// presentation — reading the whole body would prove only that they
/// were somewhere in it, not that they reached the client before the
/// run did.
struct SseReader {
    resp: reqwest::Response,
    buf: String,
    events: Vec<serde_json::Value>,
}

impl SseReader {
    fn new(resp: reqwest::Response) -> Self {
        Self {
            resp,
            buf: String::new(),
            events: Vec::new(),
        }
    }

    /// Index of the first parsed event of this type, if any.
    fn index_of(&self, kind: &str) -> Option<usize> {
        self.events.iter().position(|e| e["type"] == kind)
    }

    fn kinds(&self) -> Vec<&str> {
        self.events
            .iter()
            .map(|e| e["type"].as_str().unwrap_or("?"))
            .collect()
    }

    /// Read frames until an event of `kind` has been parsed; returns
    /// its index. Bounded by a deadline (never a fixed sleep): the
    /// stream ending first, or the deadline, fails the test.
    async fn read_until(&mut self, kind: &str) -> usize {
        if let Some(i) = self.index_of(kind) {
            return i;
        }
        let read = async {
            loop {
                let chunk = self
                    .resp
                    .chunk()
                    .await
                    .expect("sse chunk")
                    .unwrap_or_else(|| {
                        panic!("stream ended before a `{kind}` event: {:?}", self.kinds())
                    });
                self.buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(end) = self.buf.find("\n\n") {
                    let frame: String = self.buf.drain(..end + 2).collect();
                    let frame = frame.trim();
                    if frame.is_empty() {
                        continue;
                    }
                    let data = frame.strip_prefix("data: ").expect("sse data line");
                    self.events
                        .push(serde_json::from_str(data).expect("valid sse json"));
                }
                if let Some(i) = self.index_of(kind) {
                    return i;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), read)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for a `{kind}` sse event"))
    }
}

/// POST /api/v1/chat/sessions (the draft-create submit).
async fn submit_draft(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
    message: &str,
) -> reqwest::Response {
    client
        .post(format!("{base}/api/v1/chat/sessions"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .form(&[("message", message)])
        .send()
        .await
        .unwrap()
}

/// Submit a draft and read its full SSE body; returns (session_id,
/// events). The session id comes from the first (session) event.
async fn submit_draft_collect(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
    message: &str,
) -> (String, Vec<serde_json::Value>) {
    let resp = submit_draft(client, base, cookie, csrf, message).await;
    assert_eq!(resp.status(), 200, "draft submit");
    let body = resp.text().await.unwrap();
    let events = sse_events(&body);
    assert!(!events.is_empty(), "no sse events: {body}");
    assert_eq!(
        events[0]["type"], "session",
        "first event must be the session event: {events:?}"
    );
    let session_id = events[0]["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    (session_id, events)
}

/// GET the session-meta JSON.
async fn get_meta(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    session_id: &str,
) -> serde_json::Value {
    let resp = client
        .get(format!("{base}/api/v1/chat/sessions/{session_id}"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "session meta");
    resp.json().await.unwrap()
}

/// Poll the session-meta endpoint until `turn_status` equals `want`
/// (poll observable state with a deadline — never a bare sleep).
async fn poll_meta_until(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    session_id: &str,
    want: &str,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let meta = get_meta(client, base, cookie, session_id).await;
        if meta["turn_status"].as_str() == Some(want) {
            return meta;
        }
        assert!(
            Instant::now() < deadline,
            "turn never reached {want}: {meta}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// POST /api/v1/chat/sessions/{id}/messages (the existing-session submit).
async fn submit_into_session(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
    session_id: &str,
    message: &str,
) -> reqwest::Response {
    client
        .post(format!("{base}/api/v1/chat/sessions/{session_id}/messages"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .form(&[("message", message)])
        .send()
        .await
        .unwrap()
}

/// `(chat_sessions, chat_turns)` row counts read straight from the
/// server's SQLite file (a second pool; WAL readers are concurrent).
/// The depth-cap contract is "no row written", so it is asserted
/// against the tables themselves, not an endpoint's view of them.
async fn chat_row_counts(store: &Store) -> (i64, i64) {
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_sessions")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_turns")
        .fetch_one(store.pool())
        .await
        .unwrap();
    (sessions, turns)
}

/// The user's single session id from the list endpoint — the draft
/// create persists the session row before the stream yields anything,
/// so this is the no-partial-read way to learn the id mid-run.
async fn first_session_id(client: &reqwest::Client, base: &str, cookie: &str) -> String {
    let resp = client
        .get(format!("{base}/api/v1/chat/sessions"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "sessions list");
    let list: serde_json::Value = resp.json().await.unwrap();
    let sessions = list["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 1, "exactly one session: {list}");
    sessions[0]["id"].as_str().expect("id").to_string()
}

#[tokio::test]
async fn submit_tool_done_flow_persists() {
    let _g = TEST_LOCK.lock().await;
    // Script: (1) the final answer, (2) a search tool call — the agent
    // runs the tool, then answers. Draft-create: first event carries
    // the new session id, then tool progress, then done.
    let llm_url = mock_llm(vec![
        text_response("Here is the answer you asked for."),
        tool_call_response(
            "call-1",
            "search_knowledge",
            serde_json::json!({ "query": "test" }),
        ),
    ])
    .await;
    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    let (session_id, events) =
        submit_draft_collect(&client, &base, &cookie, &csrf, "hello librarian").await;
    assert_eq!(events[0]["seq"], 0, "session event seq is 0");
    assert_eq!(events[1]["type"], "tool");
    assert_eq!(events[1]["name"], "search_knowledge");
    assert_eq!(events[1]["seq"], 2);
    assert_eq!(events[2]["type"], "done");
    assert_eq!(events[2]["reply"], "Here is the answer you asked for.");
    assert_eq!(events[2]["seq"], 3);

    // Reattach after the run: no live entry → the tool + assistant
    // rows replay and the stream closes (poll semantics). The user row
    // never replays, and the replay vocabulary says "assistant", not
    // "done" (done means a live turn finished).
    let replay = client
        .get(format!(
            "{base}/api/v1/chat/sessions/{session_id}/stream?after=0"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), 200);
    assert_eq!(
        replay.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let rbody = replay.text().await.unwrap();
    let revents = sse_events(&rbody);
    assert_eq!(
        revents
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["tool", "assistant"],
        "replay = tool + assistant rows, no user row, no done"
    );
    assert_eq!(revents[0]["seq"], 2);
    assert_eq!(revents[1]["reply"], "Here is the answer you asked for.");
    assert_eq!(revents[1]["seq"], 3);
    assert!(!rbody.contains("hello librarian"), "user rows never replay");

    // Meta: the turn is done and the transcript holds all three rows.
    let meta = poll_meta_until(&client, &base, &cookie, &session_id, "done").await;
    assert_eq!(meta["id"], session_id);
    assert_eq!(meta["turn_status"], "done");
    assert_eq!(meta["turn_resumable"], false);
    assert_eq!(meta["max_seq"], 3, "user, tool, assistant rows");

    // Absent `after` = live-only: a stream GET with no cursor on a
    // finished session replays nothing and closes immediately (the
    // client that wants the transcript asks with ?after=).
    let live_only = client
        .get(format!("{base}/api/v1/chat/sessions/{session_id}/stream"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(live_only.status(), 200);
    let lbody = live_only.text().await.unwrap();
    assert!(
        !lbody.contains("data:"),
        "live-only reattach must replay nothing: {lbody}"
    );

    shutdown.cancel();
}

#[tokio::test]
async fn double_submit_conflict() {
    let _g = TEST_LOCK.lock().await;
    // Review Focus 1: a gated mock holds the runner at the LLM while
    // the second submit arrives — the unique running-turn index must
    // 409 it, never spawn a second run.
    let (llm_url, gate) = gated_mock_llm(vec![
        text_response("First turn reply."),
        tool_call_response(
            "call-1",
            "search_knowledge",
            serde_json::json!({ "query": "held" }),
        ),
    ])
    .await;
    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // Submit 1 (draft create): 200, body left unread (held open).
    let resp1 = submit_draft(&client, &base, &cookie, &csrf, "held question").await;
    assert_eq!(resp1.status(), 200);

    let session_id = first_session_id(&client, &base, &cookie).await;

    // Submit 2 to the same session while the turn runs → 409.
    let second = client
        .post(format!("{base}/api/v1/chat/sessions/{session_id}/messages"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("message", "second while running")])
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 409, "double submit must 409");
    let body: serde_json::Value = second.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({"error": "a turn is already running in this session"})
    );

    // Release; the held stream completes with tool + done.
    gate.send(true).unwrap();
    let events = sse_events(&resp1.text().await.unwrap());
    assert_eq!(events[0]["type"], "session");
    assert_eq!(events[1]["type"], "tool");
    assert_eq!(events[2]["type"], "done");
    assert_eq!(events[2]["reply"], "First turn reply.");

    // The store shows one finished turn — the 409'd submit never ran.
    let meta = poll_meta_until(&client, &base, &cookie, &session_id, "done").await;
    assert_eq!(
        meta["max_seq"], 3,
        "user, tool, assistant — exactly one run"
    );

    shutdown.cancel();
}

#[tokio::test]
async fn navigate_away_durability() {
    let _g = TEST_LOCK.lock().await;
    // Today's-bug regression pin (spec §6.6): the client drops the
    // response mid-run (never reads the body); the detached runner
    // must still persist the reply and finalize the turn.
    let (llm_url, gate) =
        gated_mock_llm(vec![text_response("Reply written with nobody watching.")]).await;
    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    let resp = submit_draft(&client, &base, &cookie, &csrf, "durable question").await;
    assert_eq!(resp.status(), 200);
    // Navigate away: drop the response without reading the body — the
    // connection dies while the turn is mid-run.
    drop(resp);

    // Mid-run, the turn is running with only the user row — the gate
    // holds the LLM, so this state is deterministic, not a race.
    let session_id = first_session_id(&client, &base, &cookie).await;
    let meta = get_meta(&client, &base, &cookie, &session_id).await;
    assert_eq!(meta["turn_status"], "running");
    assert_eq!(meta["max_seq"], 1, "only the user row so far");

    // Release; poll the store (observable state, deadline-bounded).
    gate.send(true).unwrap();
    let meta = poll_meta_until(&client, &base, &cookie, &session_id, "done").await;
    assert_eq!(meta["max_seq"], 2, "user + assistant rows persisted");

    // The reply survived with no listener attached.
    let replay = client
        .get(format!(
            "{base}/api/v1/chat/sessions/{session_id}/stream?after=0"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let revents = sse_events(&replay.text().await.unwrap());
    assert_eq!(revents.len(), 1);
    assert_eq!(revents[0]["type"], "assistant");
    assert_eq!(revents[0]["reply"], "Reply written with nobody watching.");
    assert_eq!(revents[0]["seq"], 2);

    shutdown.cancel();
}

#[tokio::test]
async fn reattach_mid_run_tails_live() {
    let _g = TEST_LOCK.lock().await;
    // Reattach while the turn runs: the replay is empty (no agent rows
    // yet; user rows never replay), the stream holds open, and the
    // live tool + done events arrive on the REATTACH stream after the
    // gate release.
    let (llm_url, gate) = gated_mock_llm(vec![
        text_response("Tailed reply."),
        tool_call_response(
            "call-1",
            "search_knowledge",
            serde_json::json!({ "query": "tail" }),
        ),
    ])
    .await;
    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    let resp1 = submit_draft(&client, &base, &cookie, &csrf, "tail me").await;
    assert_eq!(resp1.status(), 200);
    let session_id = first_session_id(&client, &base, &cookie).await;

    // Reattach BEFORE release: headers arrive, the stream holds open
    // (nothing to replay, nothing broadcast yet).
    let resp2 = client
        .get(format!(
            "{base}/api/v1/chat/sessions/{session_id}/stream?after=0"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);

    // Reader task: a closed stream would finish the read immediately —
    // it must still be open while the turn is gated.
    let reader = tokio::spawn(async move { resp2.text().await.unwrap() });
    let hold = Instant::now() + Duration::from_millis(300);
    while Instant::now() < hold {
        assert!(
            !reader.is_finished(),
            "reattach stream closed before the turn finished"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Release: tool + done must arrive on the reattach stream, in
    // order, each carrying its persisted seq (the dedup shape).
    gate.send(true).unwrap();
    let events = sse_events(&reader.await.unwrap());
    assert_eq!(
        events
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["tool", "done"],
        "the reattach stream tails the live events"
    );
    assert_eq!(events[0]["seq"], 2);
    assert_eq!(events[1]["seq"], 3);
    assert_eq!(events[1]["reply"], "Tailed reply.");

    // The unread submit stream is irrelevant now; the transcript is
    // the truth.
    drop(resp1);
    let meta = poll_meta_until(&client, &base, &cookie, &session_id, "done").await;
    assert_eq!(meta["max_seq"], 3);

    shutdown.cancel();
}

#[tokio::test]
async fn csrf_auth_and_ownership() {
    let _g = TEST_LOCK.lock().await;
    let llm_url = mock_llm(vec![text_response("owners reply")]).await;
    let (base, shutdown, dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // No session → the login gate redirects (303) to /login.
    let no_auth = client
        .post(format!("{base}/api/v1/chat/sessions"))
        .form(&[("message", "hi")])
        .send()
        .await
        .unwrap();
    assert_eq!(
        no_auth.status(),
        303,
        "unauthenticated must hit the login gate"
    );

    // Session but no CSRF token → 403.
    let no_csrf = client
        .post(format!("{base}/api/v1/chat/sessions"))
        .header("cookie", &cookie)
        .form(&[("message", "hi")])
        .send()
        .await
        .unwrap();
    assert_eq!(no_csrf.status(), 403, "missing CSRF must 403");

    // Empty message → 400.
    let empty = client
        .post(format!("{base}/api/v1/chat/sessions"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("message", "  ")])
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), 400, "empty message must 400");
    let body: serde_json::Value = empty.json().await.unwrap();
    assert_eq!(body, serde_json::json!({"error": "empty message"}));

    // The admin's session (the message completes; the session id comes
    // from the first SSE event).
    let (admin_session, _events) =
        submit_draft_collect(&client, &base, &cookie, &csrf, "admin private question").await;

    // A second user (sessions are private per user — D10, not even
    // admins see foreign sessions; this one is a plain user).
    let store = Store::open(dir.path()).await.unwrap();
    let users = mycelium_auth::UserStore::new(store.pool().clone());
    users
        .create_local(
            "other",
            "other@t.local",
            SECOND_PASSWORD,
            mycelium_auth::Role::User,
        )
        .await
        .unwrap();
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "other"), ("password", SECOND_PASSWORD)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
    let cookie2 = cookie_from(&login);
    let csrf2 = csrf_from_page(&client, &format!("{base}/"), &cookie2).await;

    // Every {id} route 404s for the foreign user — indistinguishable
    // from an unknown id (no oracle).
    let submit = client
        .post(format!(
            "{base}/api/v1/chat/sessions/{admin_session}/messages"
        ))
        .header("cookie", &cookie2)
        .header("x-csrf-token", &csrf2)
        .form(&[("message", "intruder")])
        .send()
        .await
        .unwrap();
    assert_eq!(submit.status(), 404, "foreign submit must 404");

    let meta = client
        .get(format!("{base}/api/v1/chat/sessions/{admin_session}"))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap();
    assert_eq!(meta.status(), 404, "foreign meta must 404");
    let body: serde_json::Value = meta.json().await.unwrap();
    assert_eq!(body, serde_json::json!({"error": "not found"}));

    let stream = client
        .get(format!(
            "{base}/api/v1/chat/sessions/{admin_session}/stream?after=0"
        ))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 404, "foreign stream must 404");

    let delete = client
        .delete(format!("{base}/api/v1/chat/sessions/{admin_session}"))
        .header("cookie", &cookie2)
        .header("x-csrf-token", &csrf2)
        .send()
        .await
        .unwrap();
    assert_eq!(delete.status(), 404, "foreign delete must 404");

    let delete_form = client
        .post(format!(
            "{base}/api/v1/chat/sessions/{admin_session}/delete"
        ))
        .header("cookie", &cookie2)
        .header("x-csrf-token", &csrf2)
        .send()
        .await
        .unwrap();
    assert_eq!(delete_form.status(), 404, "foreign form delete must 404");

    shutdown.cancel();
}

#[tokio::test]
async fn draft_flow_first_session() {
    let _g = TEST_LOCK.lock().await;
    // Review Focus 5: the first-ever session — the create POST's first
    // event is the session id, and the sidebar list shows exactly one
    // entry with the derived title.
    let llm_url = mock_llm(vec![text_response("Rust is a systems language.")]).await;
    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // Empty list before the first session.
    let list = client
        .get(format!("{base}/api/v1/chat/sessions"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200);
    let list: serde_json::Value = list.json().await.unwrap();
    assert_eq!(list, serde_json::json!({"sessions": []}), "no sessions yet");

    // Draft-create: the first event is the session event with the id.
    let (session_id, events) =
        submit_draft_collect(&client, &base, &cookie, &csrf, "what is rust?").await;
    assert!(
        uuid::Uuid::parse_str(&session_id).is_ok(),
        "session_id must be a UUID"
    );
    let last = events.last().expect("terminal event");
    assert_eq!(last["type"], "done");
    assert_eq!(last["reply"], "Rust is a systems language.");

    // Exactly one session, titled from the first message
    // (derive_title("what is rust?") = "what is rust?").
    let list = client
        .get(format!("{base}/api/v1/chat/sessions"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let list: serde_json::Value = list.json().await.unwrap();
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "exactly one session");
    assert_eq!(sessions[0]["id"], session_id);
    assert_eq!(sessions[0]["title"], "what is rust?");
    assert!(sessions[0]["updated_at"].as_str().is_some());

    shutdown.cancel();
}

#[tokio::test]
async fn multi_turn_context_rebuilt_from_rows() {
    let _g = TEST_LOCK.lock().await;
    // The second turn's LLM request must carry the first exchange —
    // history rebuilt from persisted rows (D11), not memory.
    let (llm_url, requests) = capturing_mock_llm(vec![
        text_response("Second reply."),
        text_response("First reply."),
    ])
    .await;
    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // Turn 1 (draft create).
    let (session_id, _events) =
        submit_draft_collect(&client, &base, &cookie, &csrf, "first question").await;
    poll_meta_until(&client, &base, &cookie, &session_id, "done").await;

    // Turn 2 (same session, new message). The stream has no session
    // event (only the draft flow emits one).
    let resp = client
        .post(format!("{base}/api/v1/chat/sessions/{session_id}/messages"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("message", "second question")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let events = sse_events(&resp.text().await.unwrap());
    // Submit replays only THIS turn's events: the first turn's rows
    // (seq 1-2) are already rendered by the client — a "First reply."
    // re-emission would double the bubble. The submit cursor is the
    // new turn's user row.
    assert!(
        !events.iter().any(|e| e["reply"] == "First reply."),
        "prior turns must not replay on a later submit: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| e["type"] == "assistant" && e["seq"] == 2),
        "no stale assistant row from turn 1: {events:?}"
    );
    let last = events.last().expect("terminal event");
    // Live tail or replayed row — the dual vocabulary is by design;
    // either way the reply is this turn's.
    assert!(
        last["type"] == "done" || last["type"] == "assistant",
        "terminal event: {events:?}"
    );
    assert_eq!(last["reply"], "Second reply.");
    assert!(
        !events.iter().any(|e| e["type"] == "session"),
        "no session event on the {{id}} submit"
    );

    // The second LLM request: [system, user("first question"),
    // assistant("First reply."), user("second question")] — rebuilt
    // from session rows.
    let captured = requests.lock().await;
    assert_eq!(captured.len(), 2, "one LLM call per turn");
    let second = &captured[1];
    let messages = second["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        vec!["system", "user", "assistant", "user"],
        "multi-turn history must be present: {roles:?}"
    );
    assert!(
        messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("first question"),
        "prior user turn present"
    );
    assert!(
        messages[2]["content"]
            .as_str()
            .unwrap()
            .contains("First reply."),
        "prior assistant turn present"
    );
    assert!(
        messages[3]["content"]
            .as_str()
            .unwrap()
            .contains("second question"),
        "new user turn present"
    );

    shutdown.cancel();
}

#[tokio::test]
async fn delete_own_sessions_and_refuse_while_running() {
    let _g = TEST_LOCK.lock().await;
    // D12: delete refuses while a turn runs (409, both variants); the
    // owner's delete succeeds — the POST variant redirects (303
    // /chat, the native sidebar form) and the DELETE variant answers
    // {"ok":true}; the session (and its rows) are gone afterwards.
    let (llm_url, gate) = gated_mock_llm(vec![
        text_response("second session reply"),
        text_response("first session reply"),
    ])
    .await;
    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // Session A with the turn held running.
    let resp1 = submit_draft(&client, &base, &cookie, &csrf, "to be deleted").await;
    assert_eq!(resp1.status(), 200);
    let session_a = first_session_id(&client, &base, &cookie).await;

    // Delete while running → 409 from both variants (the POST variant
    // renders the same JSON — the no-JS path shows it).
    let del = client
        .delete(format!("{base}/api/v1/chat/sessions/{session_a}"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 409, "delete while running must 409");
    let body: serde_json::Value = del.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({"error": "a turn is already running in this session"})
    );
    let del_form = client
        .post(format!("{base}/api/v1/chat/sessions/{session_a}/delete"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(del_form.status(), 409, "form delete while running must 409");

    // Let the turn finish, then delete via the native-form variant.
    gate.send(true).unwrap();
    poll_meta_until(&client, &base, &cookie, &session_a, "done").await;
    let gone = client
        .post(format!("{base}/api/v1/chat/sessions/{session_a}/delete"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), 303, "form delete redirects");
    assert_eq!(gone.headers().get("location").unwrap(), "/chat");

    // Gone: meta 404s, the list is empty (messages + turns cascaded).
    let meta = client
        .get(format!("{base}/api/v1/chat/sessions/{session_a}"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(meta.status(), 404);
    let list = client
        .get(format!("{base}/api/v1/chat/sessions"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let list: serde_json::Value = list.json().await.unwrap();
    assert_eq!(list, serde_json::json!({"sessions": []}));

    // The DELETE variant on a second session: {"ok": true}.
    let (session_b, _events) =
        submit_draft_collect(&client, &base, &cookie, &csrf, "json delete").await;
    poll_meta_until(&client, &base, &cookie, &session_b, "done").await;
    let del = client
        .delete(format!("{base}/api/v1/chat/sessions/{session_b}"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 200);
    let body: serde_json::Value = del.json().await.unwrap();
    assert_eq!(body, serde_json::json!({"ok": true}));

    shutdown.cancel();
}

/// The /chat page's two-pane SSR (spec §3.6/§6.15): the sessions sidebar
/// (newest-first links, the active one highlighted, the native delete
/// form) and the selected session's transcript rendered with the same
/// bubble markup chat.js builds — user/librarian bubbles, the tool row
/// as a note, and NO pending bubble on a done turn. The `?s=` matrix:
/// `new` → draft (no row, nothing selected), foreign/unknown/invalid
/// id → 303 to /chat (the page redirects where the API 404s — both
/// "not yours" with no oracle), absent + sessions → newest. A
/// `<script>` first message renders its derived title (and the user
/// bubble) HTML-escaped — never executable markup.
#[tokio::test]
async fn chat_page_sidebar_and_transcript_ssr() {
    let _g = TEST_LOCK.lock().await;
    // LIFO script: the hostily-titled session (created first) pops the
    // LAST response ("First reply."); the tool session (created second)
    // pops the tool call, then — the agent's second request — the
    // markdown reply.
    let llm_url = mock_llm(vec![
        text_response("Second reply with markdown."),
        tool_call_response(
            "call-1",
            "search_knowledge",
            serde_json::json!({ "query": "rust" }),
        ),
        text_response("First reply."),
    ])
    .await;
    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // Session A (created first, so the sidebar's OLDEST entry): its
    // first message is the hostile title of spec §6.15.
    let (hostile_id, _) = submit_draft_collect(
        &client,
        &base,
        &cookie,
        &csrf,
        "<script>alert(1)</script> hostile probe",
    )
    .await;
    poll_meta_until(&client, &base, &cookie, &hostile_id, "done").await;

    // Session B (created second → newest): a tool-call turn.
    let (session_id, _b) =
        submit_draft_collect(&client, &base, &cookie, &csrf, "rust question").await;
    poll_meta_until(&client, &base, &cookie, &session_id, "done").await;

    // GET /chat (no ?s) → the NEWEST session's transcript rendered.
    let page = client
        .get(format!("{base}/chat"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();

    // Sidebar: "+ New chat" and both session links, newest FIRST.
    assert!(
        html.contains(r##"<a class="chat-new" href="/chat?s=new">+ New chat</a>"##),
        "{html}"
    );
    let s2_pos = html
        .find(&format!(r##"href="/chat?s={session_id}""##))
        .unwrap_or_else(|| panic!("newest session link missing: {html}"));
    let hostile_pos = html
        .find(&format!(r##"href="/chat?s={hostile_id}""##))
        .unwrap_or_else(|| panic!("older session link missing: {html}"));
    assert!(
        s2_pos < hostile_pos,
        "sidebar links must be newest-first: {html}"
    );
    // The newest session is the ACTIVE one (the row container carries
    // the active highlight now that the row is icon+link).
    assert!(
        html.contains(r#"class="chat-session-row active""#),
        "no active row: {html}"
    );

    // The native delete form per session (works without JS).
    let form_action = format!(r#"action="/api/v1/chat/sessions/{session_id}/delete""#);
    assert!(html.contains(&form_action), "delete form action: {html}");
    assert!(
        html.contains(r#"name="csrf_token""#),
        "delete form must carry the CSRF hidden field: {html}"
    );
    // The delete trigger rides the keys-page confirm-dialog pattern
    // (final-review Finding 1): without JS the form POSTs directly;
    // with JS confirm.js opens the shared dialog, copying this row's
    // data-key-id AND its per-row form action onto the dialog form.
    assert!(
        html.contains(r##"data-confirm-dialog="confirm-delete-chat""##),
        "delete trigger must name the shared dialog: {html}"
    );
    assert!(
        html.contains(&format!(r##"data-key-id="{session_id}""##)),
        "delete trigger must carry its session id: {html}"
    );
    assert!(
        !html.contains(r#"data-confirm=""#) && html.contains(r#"data-confirm-dialog"#),
        "bare unwired data-confirm must not survive (Finding 1): {html}"
    );
    // The trigger is a compact icon button, not a text button: the
    // visible label is an aria-label (screen readers + the tooltip),
    // never rendered text, and the button carries the trash icon class
    // (the layout-fix contract: a small icon at the start of each
    // session row, not a large text button beside the title).
    assert!(
        html.contains(r##"data-confirm-dialog="confirm-delete-chat""##)
            && html.contains(r#"class="chat-delete-btn""#),
        "delete trigger must be the icon button: {html}"
    );
    assert!(
        html.contains(r#"aria-label="Delete chat""#),
        "icon delete button must carry the accessible label: {html}"
    );
    // The row trigger is ICON-ONLY: its markup is the SVG alone. The
    // shared dialog's own confirm button is legitimately labeled
    // "Delete", so the negative pin is scoped to the row form: no
    // text may sit between the icon button's tags.
    assert!(
        html.contains(r#"</svg></button></form></div>"#),
        "row delete button must contain only the icon (no text): {html}"
    );
    // The sessions sidebar is collapsible (layout fix #5): the toggle
    // button exists and the pane carries the collapse hook class; the
    // JS (chat.js) flips the class + persists per browser.
    assert!(
        html.contains(r##"<button type="button" id="chat-sidebar-toggle""##),
        "sidebar collapse toggle missing: {html}"
    );
    assert!(
        html.contains(r#"class="chat-sessions""#),
        "sessions pane carries the collapse hook: {html}"
    );
    // The shared dialog exists in the markup and confirm.js loads with
    // the page (the guard chat.js alone doesn't provide).
    assert!(
        html.contains(r##"<dialog id="confirm-delete-chat""##),
        "delete dialog missing: {html}"
    );
    assert!(
        html.contains("/assets/confirm.js?v="),
        "confirm.js must load on /chat: {html}"
    );
    assert!(
        html.contains("/assets/chat.js?v="),
        "chat.js must still load on /chat: {html}"
    );

    // Transcript (the done tool turn): user bubble, tool note carrying
    // the sealed JSON's parts escaped, the reply as a markdown <p>.
    assert!(
        html.contains(r#"<div class="chat-msg user">rust question</div>"#),
        "user bubble: {html}"
    );
    assert!(
        html.contains(r#"class="chat-msg tool-note">· search_knowledge ("#),
        "tool note: {html}"
    );
    assert!(
        html.contains(r#"{&quot;query&quot;:&quot;rust&quot;})</div>"#),
        "tool detail escaped: {html}"
    );
    assert!(
        html.contains("<div class=\"chat-msg librarian\"><p>Second reply with markdown.</p></div>"),
        "assistant reply as a rendered <p>: {html}"
    );
    assert!(
        !html.contains("The librarian is thinking…"),
        "a done turn must NOT render the pending bubble: {html}"
    );
    assert!(
        !html.contains("chat-status-note"),
        "a done turn must NOT render a status note: {html}"
    );

    // The page's data-* state for chat.js (Task 7's contract).
    assert!(
        html.contains(&format!(r#"data-session="{session_id}""#)),
        "{html}"
    );
    assert!(html.contains(r#"data-max-seq="3""#), "max seq: {html}");
    assert!(html.contains(r#"data-turn-status="done""#), "{html}");
    assert!(
        html.contains(&format!(r#"data-csrf="{csrf}""#)),
        "data-csrf must carry the session token: {html}"
    );

    // GET /chat?s=new → the draft: empty log, nothing highlighted,
    // no session row created (the sidebar still lists the two).
    let draft = client
        .get(format!("{base}/chat?s=new"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(draft.status(), 200);
    let dh = draft.text().await.unwrap();
    assert!(dh.contains(r#"+ New chat</a>"#), "{dh}");
    assert!(dh.contains(r#"data-session="new""#), "{dh}");
    assert!(dh.contains(r#"data-max-seq="0""#), "{dh}");
    assert!(dh.contains(r#"data-turn-status="none""#), "{dh}");
    assert!(!dh.contains("chat-msg user"), "empty log: {dh}");
    assert!(
        !dh.contains("The librarian is thinking…"),
        "draft must not fake a running turn: {dh}"
    );
    assert!(
        !dh.contains("chat-session-link active"),
        "draft sidebar un-highlighted: {dh}"
    );
    assert!(
        dh.contains(&format!(r#"href="/chat?s={hostile_id}""#)),
        "sidebar still lists sessions on the draft view: {dh}"
    );

    // GET /chat?s=<hostile id>: the derived title and the user bubble
    // ESCAPED (spec §6.15) — the marker text never renders raw.
    let hpage = client
        .get(format!("{base}/chat?s={hostile_id}"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(hpage.status(), 200);
    let hh = hpage.text().await.unwrap();
    assert!(
        hh.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "escaped title missing: {hh}"
    );
    assert!(
        !hh.contains("<script>alert(1)</script>"),
        "raw script tag leaked: {hh}"
    );
    assert!(
        hh.contains("<div class=\"chat-msg librarian\"><p>First reply.</p></div>"),
        "the session's own reply (markdown <p>): {hh}"
    );
    assert!(!hh.contains(r#"data-session="new""#), "{hh}");

    // GET /chat?s=<foreign uuid> → 303 to /chat (page redirects where
    // the API 404s — same "not yours", no oracle difference).
    let foreign = client
        .get(format!("{base}/chat?s={}", uuid::Uuid::new_v4()))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), 303, "foreign id must redirect");
    assert_eq!(foreign.headers().get("location").unwrap(), "/chat");

    // A non-UUID ?s value parses badly → the same 303.
    let invalid = client
        .get(format!("{base}/chat?s=not-a-uuid"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 303, "invalid id must redirect");
    assert_eq!(invalid.headers().get("location").unwrap(), "/chat");

    shutdown.cancel();
}

/// The page's status bubbles (spec §3.6): a RUNNING turn SSRs the
/// pending "The librarian is thinking…" bubble; an interrupted+resumable
/// turn (the navigate-away + boot-sweep shape) SSRs the resume note —
/// and no longer the pending bubble. Rows land via the transcript, so
/// the later re-render collects the tool-note + reply once the runner
/// finishes.
#[tokio::test]
async fn chat_page_running_and_interrupted_notes() {
    let _g = TEST_LOCK.lock().await;
    // The mock's response is never released — the turn stays held
    // running (then interrupted after the sweep); the reader dropping
    // the submit response is what lets the test end cleanly.
    // The gate is never released: the turn stays held running (then
    // interrupted after the sweep) for the whole test — the response's
    // unread body is simply dropped at the end.
    let (llm_url, _gate) = gated_mock_llm(vec![text_response("held reply.")]).await;
    let (base, shutdown, dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    let resp = submit_draft(&client, &base, &cookie, &csrf, "run into the hold").await;
    assert_eq!(resp.status(), 200);
    let session_id = first_session_id(&client, &base, &cookie).await;

    // Mid-run: the pending bubble IS server-rendered.
    let page = client
        .get(format!("{base}/chat?s={session_id}"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(
        html.contains(
            r#"<div class="chat-msg librarian pending">The librarian is thinking…</div>"#
        ),
        "running turn must SSR the pending bubble: {html}"
    );
    assert!(html.contains(r#"data-turn-status="running""#), "{html}");
    assert!(
        html.contains(r#"data-max-seq="1""#),
        "user row only: {html}"
    );
    assert!(
        !html.contains("chat-status-note"),
        "a running turn is not interrupted: {html}"
    );

    // Navigate away + simulate the restart (what main.rs calls at boot):
    // the held turn goes interrupted with attempts 0.
    drop(resp);
    let store = Store::open(dir.path()).await.unwrap();
    let chat = mycelium_store::chat::ChatStore::new(store.pool().clone());
    let swept = chat.boot_sweep().await.unwrap();
    assert_eq!(swept, 1, "the one held turn was swept");

    let page = client
        .get(format!("{base}/chat?s={session_id}"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(
        html.contains(
            r#"<div class="chat-msg chat-status-note">Interrupted by a server restart — it will resume automatically.</div>"#,
        ),
        "resumable note missing: {html}"
    );
    assert!(
        !html.contains("The librarian is thinking…"),
        "an interrupted turn is not running: {html}"
    );
    assert!(html.contains(r#"data-turn-status="interrupted""#), "{html}");
    assert!(html.contains(r#"data-max-seq="1""#), "{html}");

    shutdown.cancel();
}

/// A failed turn renders its error row as an error bubble — the
/// transcript already carries the message (no separate status note for
/// `failed`), and the page's data-turn-status says so.
#[tokio::test]
async fn chat_page_failed_error_bubble() {
    let _g = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    LLM_HEALTH.reset_for_test();
    // The mock answers 200 with an EMPTY choices array: a reachable
    // model that produced nothing is a real run outcome (EmptyResponse
    // is NOT the backend-down class), so the detached runner persists
    // the error row + finalizes `failed`. (A 500/exhausted script would
    // now be weather and park the turn instead — see
    // `fresh_submit_parks_on_red_backend` for that path.)
    let llm_url = mock_llm(vec![serde_json::json!({"choices": []})]).await;
    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    let (session_id, _events) =
        submit_draft_collect(&client, &base, &cookie, &csrf, "doomed question").await;
    poll_meta_until(&client, &base, &cookie, &session_id, "failed").await;

    let page = client
        .get(format!("{base}/chat?s={session_id}"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains(r#"data-turn-status="failed""#), "{html}");
    assert!(!html.contains("chat-status-note"), "{html}");
    assert!(
        !html.contains("The librarian is thinking…"),
        "a failed turn is not running: {html}"
    );
    // The error bubble exists and carries the row's message (non-empty).
    let marker = r#"class="chat-msg error">"#;
    let idx = html
        .find(marker)
        .unwrap_or_else(|| panic!("error bubble missing: {html}"));
    let rest = &html[idx + marker.len()..];
    let end = rest
        .find("</div>")
        .unwrap_or_else(|| panic!("unterminated error bubble: {html}"));
    assert!(
        !rest[..end].trim().is_empty(),
        "the error row's message must render: {html}"
    );

    shutdown.cancel();
}

/// The outage waiting bubble (outage spec §3.3 copy, pinned verbatim;
/// page contract like the running/interrupted notes): a turn parked as
/// `waiting_backend` SSRs the waiting note reusing the `chat-status-note`
/// class (Task 6 ruling — no new CSS), the page carries
/// `data-turn-status="waiting_backend"`, and the GET `{id}` meta reports
/// `waiting_backend` + `turn_resumable: true` — the server-side twin of
/// chat.js's poll branch, which stops polling ONLY for
/// `interrupted` + non-resumable (assets.rs), so waiting turns keep the
/// 5 s meta live with NO client change. The note is a constant with no
/// user content: the session is created with a hostile first message, so
/// the render proves the pinned string survives alongside the escaped
/// title (§6.15 escaping stays the pages-level contract).
#[tokio::test]
async fn chat_page_waiting_backend_note_and_poll_contract() {
    let _g = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    // Green here on purpose: the runner parks on Red at ADMISSION, and
    // this test drives the mid-run park instead — the store's
    // `park_waiting` against a held-running turn, exactly what the
    // runner does when the backend goes Red mid-flight.
    LLM_HEALTH.reset_for_test();
    // The mock's gate is never released: the turn stays held running for
    // the whole test; the response's unread body is dropped at the end.
    let (llm_url, _gate) = gated_mock_llm(vec![text_response("held reply.")]).await;
    let (base, shutdown, dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // Hostile first message → the derived session title carries markup.
    let resp = submit_draft(
        &client,
        &base,
        &cookie,
        &csrf,
        "<img src=x onerror=alert(2)> parked ask",
    )
    .await;
    assert_eq!(resp.status(), 200);
    let session_id = first_session_id(&client, &base, &cookie).await;
    poll_meta_until(&client, &base, &cookie, &session_id, "running").await;

    // Park the running turn through the store (the mid-run outage park).
    let store = Store::open(dir.path()).await.unwrap();
    let chat = mycelium_store::chat::ChatStore::new(store.pool().clone());
    let (turn_id, user_id): (String, String) = sqlx::query_as(
        "SELECT t.id, s.user_id FROM chat_turns t \
         JOIN chat_sessions s ON s.id = t.session_id WHERE s.id = ?",
    )
    .bind(&session_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    chat.park_waiting(
        uuid::Uuid::parse_str(&user_id).unwrap(),
        uuid::Uuid::parse_str(&turn_id).unwrap(),
        Some("backend unhealthy at admission"),
    )
    .await
    .unwrap();

    let page = client
        .get(format!("{base}/chat?s={session_id}"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    // The pinned copy, verbatim (spec §3.3).
    assert!(
        html.contains(
            r#"<div class="chat-msg chat-status-note">Waiting for the librarian backend — it will resume automatically.</div>"#,
        ),
        "waiting note missing: {html}"
    );
    assert!(
        html.contains(r#"data-turn-status="waiting_backend""#),
        "{html}"
    );
    assert!(html.contains(r#"data-max-seq="1""#), "{html}");
    assert!(
        !html.contains("The librarian is thinking…"),
        "a waiting turn is not running: {html}"
    );
    assert!(
        !html.contains("Interrupted by a server restart"),
        "a waiting turn is not an interrupted turn: {html}"
    );
    // The note itself carries no user content (the exact-div assertion
    // above pins the constant); the hostile title renders escaped and
    // the raw markup never appears anywhere in the page.
    assert!(
        html.contains("&lt;img src=x onerror=alert(2)&gt;"),
        "hostile title must render escaped: {html}"
    );
    assert!(
        !html.contains("<img src=x onerror=alert(2)>"),
        "raw hostile markup leaked: {html}"
    );

    // The meta endpoint — the poll's 5 s input: waiting AND resumable
    // (chat.js keeps polling on this shape; only interrupted+spent
    // stops it), with the transcript cursor at the trigger row.
    let meta = get_meta(&client, &base, &cookie, &session_id).await;
    assert_eq!(meta["id"], session_id);
    assert_eq!(meta["turn_status"], "waiting_backend");
    assert_eq!(
        meta["turn_resumable"], true,
        "waiting turns are resumable: {meta}"
    );
    assert_eq!(meta["max_seq"], 1, "only the trigger row: {meta}");

    shutdown.cancel();
}

/// Fresh submit during a known outage (outage spec §3.3 admission edge
/// (a), decision §6: the submit is ACCEPTED): with the health machine
/// Red, the runner parks the turn as `waiting_backend` at admission —
/// never queueing it through an outage. The SSE stream closes on the
/// park (the registry entry is dropped; the stream carries the session
/// event and nothing else — no error event, no run events), the meta
/// endpoint reports `waiting_backend` + resumable, the turn records
/// `last_error`, NO transcript row beyond the trigger message exists,
/// and the gate was never entered (`admissions_total` unchanged — the
/// admission check sits ahead of it).
#[tokio::test]
async fn fresh_submit_parks_on_red_backend() {
    let _g = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    // Force Red through the machine's own transition rule (3
    // consecutive failures), per the brief.
    let fresh = LLM_HEALTH.reset_for_test();
    assert_eq!(fresh.state, BackendState::Green);
    LLM_HEALTH.note_failure();
    LLM_HEALTH.note_failure();
    LLM_HEALTH.note_failure();
    assert_eq!(LLM_HEALTH.snapshot().state, BackendState::Red);
    // A REACHABLE mock with an empty script: a request would 500, so
    // "no request recorded" proves the run never started.
    let (llm_url, requests) = capturing_mock_llm(vec![]).await;
    let (base, shutdown, dir) = boot_with(mycelium_web::LlmConfig {
        url: llm_url,
        model: "mock".into(),
        api_key: None,
        ..Default::default()
    })
    .await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;
    // A second pool over the same file: the row + last_error
    // observables.
    let store = Store::open(dir.path()).await.unwrap();

    let admissions_before = LLM_GATE.stats().admissions_total;
    let resp = submit_draft(&client, &base, &cookie, &csrf, "asked during an outage").await;
    assert_eq!(
        resp.status(),
        200,
        "a fresh submit is accepted during an outage"
    );
    let body = resp.text().await.unwrap();
    let events = sse_events(&body);
    assert_eq!(
        events
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["session"],
        "the stream carries the session event only — the park emits no event: {events:?}"
    );
    let session_id = events[0]["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    // The turn parked: the meta endpoint (the client's poll) shows it
    // waiting + resumable, and the stream closed (poll semantics).
    let meta = poll_meta_until(&client, &base, &cookie, &session_id, "waiting_backend").await;
    assert_eq!(
        meta["turn_resumable"], true,
        "waiting turns are resumable: {meta}"
    );
    assert_eq!(meta["max_seq"], 1, "only the trigger message row: {meta}");

    let error_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_messages WHERE role = ?")
        .bind("error")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(error_rows, 0, "a park writes no error row");
    let (last_error, attempts): (Option<String>, i32) = sqlx::query_as(
        "SELECT t.last_error, t.attempts FROM chat_turns t \
         JOIN chat_sessions s ON s.id = t.session_id WHERE s.id = ?",
    )
    .bind(&session_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert!(last_error.is_some(), "the park records last_error");
    assert_eq!(attempts, 0, "parking never touches attempts");
    assert_eq!(
        LLM_GATE.stats().admissions_total,
        admissions_before,
        "the admission check parked BEFORE the gate — it was never entered"
    );
    assert!(
        requests.lock().await.is_empty(),
        "a turn parked at admission never calls the LLM"
    );

    shutdown.cancel();
}

/// Task 7: the chat page's JS rides the session endpoints — the version
/// bump invalidates the served-asset caches, the body carries the
/// create/submit session URLs and the composer island's entry point,
/// and the REMOVED legacy stream route is gone from the asset entirely
/// (the Task-4 carry-forward: the pre-rewrite JS still POSTed a route
/// that no longer exists — the runtime submit path was broken until
/// this rewrite).
#[tokio::test]
async fn assets_version_bumped_and_chat_js_references_new_endpoints() {
    let _g = TEST_LOCK.lock().await;
    let llm_url = "http://127.0.0.1:1/v1".to_string(); // dead; no turn runs
    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();

    // The versioned URL the page emits: 200, served as JavaScript.
    // (The version is pinned twice on purpose: the served asset must
    // be the NEW default (v=29 — the bumped const scaffolds to disk),
    // and the PAGE must reference that version, so a stale ?v= in the
    // shell or a missing version bump both fail here.)
    let (cookie, _csrf) = login_admin(&client, &base).await;
    let page = client
        .get(format!("{base}/chat"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let page_html = page.text().await.unwrap();
    assert!(
        page_html.contains("/assets/chat.js?v=32"),
        "the /chat page must reference the bumped chat.js version: {page_html}"
    );
    let resp = client
        .get(format!("{base}/assets/chat.js?v=32"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "chat.js must serve");
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/javascript",
        "chat.js content type"
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("/api/v1/chat/sessions"),
        "chat.js must reference the session endpoints: {body}"
    );
    assert!(
        body.contains("myceliumChatSend"),
        "chat.js must keep the composer island's entry point: {body}"
    );
    assert!(
        !body.contains("/api/v1/chat/stream"),
        "the removed legacy stream route must not appear anywhere in chat.js: {body}"
    );
    // The queue events are handled BEFORE the seq-dedup gate (they
    // carry no seq) and the note is built with textContent, never
    // innerHTML (LLM concurrency spec §3.3).
    for needle in [
        "\"queued\"",
        "\"admitted\"",
        "setQueueNote",
        "clearQueueNote",
        "chat-queue-note",
    ] {
        assert!(
            body.contains(needle),
            "chat.js must carry the queue-status handling ({needle}): {body}"
        );
    }
    let queued_at = body.find("ev.type === \"queued\"").expect("queued branch");
    let dedup_at = body.find("seen[ev.seq]").expect("the seq dedup gate");
    assert!(
        queued_at < dedup_at,
        "the queued branch must precede the seq-dedup gate in chat.js"
    );
    assert!(
        body.find("ev.type === \"admitted\"")
            .expect("admitted branch")
            < dedup_at,
        "the admitted branch must precede the seq-dedup gate in chat.js"
    );

    shutdown.cancel();
}

/// Task 7: the CONTRACT the JS's liveness poll implements (the meta
/// endpoint's shape), pinned server-side — the JS's browser behavior is
/// out of scope for the suite (the version + reference pins above and
/// the Task-6 SSR contracts are the seam). A turn interrupted by a
/// restart must reattach via the poll: the page renders the resume
/// note (which the poll clears when the turn moves), and the meta
/// endpoint reports `turn_status: "interrupted"` with
/// `turn_resumable: true` — the resume worker owns the claim (then the
/// poll's `running` flip reopens the stream with the cursor); the
/// client only watches.
#[tokio::test]
async fn interrupted_turn_js_reattaches_via_poll() {
    let _g = TEST_LOCK.lock().await;
    // The gate is never released: the turn stays held running — then
    // interrupted after the sweep — for the whole test; the response's
    // unread body is dropped at the end.
    let (llm_url, _gate) = gated_mock_llm(vec![text_response("held reply.")]).await;
    let (base, shutdown, dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    let resp = submit_draft(&client, &base, &cookie, &csrf, "poll me after the restart").await;
    assert_eq!(resp.status(), 200);
    let session_id = first_session_id(&client, &base, &cookie).await;

    // Navigate away (drop the unsent stream) + simulate the restart
    // (what main.rs calls at boot): running → interrupted, attempts 0.
    drop(resp);
    let store = Store::open(dir.path()).await.unwrap();
    let chat = mycelium_store::chat::ChatStore::new(store.pool().clone());
    let swept = chat.boot_sweep().await.unwrap();
    assert_eq!(swept, 1, "the one held turn was swept");

    // The page: the resume note rendered, the interrupted status, and
    // the transcript cursor (the user row) — the load-time state the
    // JS's poll entry reads.
    let page = client
        .get(format!("{base}/chat?s={session_id}"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(
        html.contains(
            r#"<div class="chat-msg chat-status-note">Interrupted by a server restart — it will resume automatically.</div>"#,
        ),
        "resumable note missing: {html}"
    );
    assert!(html.contains(r#"data-turn-status="interrupted""#), "{html}");
    assert!(html.contains(r#"data-max-seq="1""#), "{html}");

    // The meta endpoint — the poll's 5 s input: interrupted AND
    // resumable (the worker has not claimed it here — the test's boot
    // never spawns the worker), with the transcript cursor.
    let meta = get_meta(&client, &base, &cookie, &session_id).await;
    assert_eq!(meta["id"], session_id);
    assert_eq!(meta["turn_status"], "interrupted");
    assert_eq!(meta["turn_resumable"], true);
    assert_eq!(meta["max_seq"], 1, "only the user row persisted");

    shutdown.cancel();
}

/// Spec D9/§6.16 standing guard: `POST /api/v1/chat` (the pre-session
/// one-shot endpoint) carries NO memory — the second call's LLM request
/// contains ONLY [system, user("<second message>")], never the first
/// exchange. Callers manage their own context; multi-turn history is the
/// session endpoints' job (`multi_turn_context_rebuilt_from_rows` pins
/// THAT side). This pins the stateless half with the same
/// request-capturing mock — the REAL second payload, not an inference.
#[tokio::test]
async fn api_chat_is_stateless_single_turn() {
    let _g = TEST_LOCK.lock().await;
    let (llm_url, requests) = capturing_mock_llm(vec![
        text_response("Second one-shot reply."),
        text_response("First one-shot reply."),
    ])
    .await;
    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // Two consecutive POSTs to the one-shot endpoint.
    for msg in ["first message", "second message"] {
        let resp = client
            .post(format!("{base}/api/v1/chat"))
            .header("cookie", &cookie)
            .header("x-csrf-token", &csrf)
            .form(&[("message", msg)])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "one-shot chat ({msg})");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body["reply"].as_str().unwrap().ends_with("reply."),
            "one-shot reply shape: {body}"
        );
    }

    // The captured payloads are the truth: request 2 must be
    // [system, user("second message")] — two messages only.
    let captured = requests.lock().await;
    assert_eq!(captured.len(), 2, "one LLM call per one-shot POST");
    let second = &captured[1];
    let messages = second["messages"].as_array().unwrap();
    assert_eq!(
        messages.len(),
        2,
        "the second request carries exactly system + this message: {messages:?}"
    );
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "second message");
    let all = serde_json::to_string(&captured[1]).unwrap();
    assert!(
        !all.contains("first message"),
        "no prior context may leak into the second one-shot request: {all}"
    );
    assert!(
        !all.contains("First one-shot reply."),
        "no prior assistant reply may leak into the second one-shot request: {all}"
    );

    shutdown.cancel();
}

#[tokio::test]
async fn stateless_chat_503_carries_the_backend_real_error() {
    // The one-shot 503's message routes through the same
    // `llm_failure_detail` helper as the runner's failure rows: a
    // reachable backend's refusal is the backend's own text — the
    // configure-me wording is reserved for a backend that is not
    // configured at all. (A 400 is not backend-down, so health stays
    // Green here and feeds nothing.) The 503 status itself is unchanged.
    let _g = TEST_LOCK.lock().await;
    let _h = HealthDefaults;
    LLM_HEALTH.reset_for_test();

    // A reachable mock backend that refuses every completion with 400.
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|| async {
            (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "error": { "message": "prompt too long" }
                })),
            )
                .into_response()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let (base, shutdown, _dir) = boot(format!("http://{addr}/v1")).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    let resp = client
        .post(format!("{base}/api/v1/chat"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("message", "too long for the mock")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503, "the status contract is unchanged");
    let body: serde_json::Value = resp.json().await.unwrap();
    let err = body["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("prompt too long"),
        "the refusal text reaches the caller: {body}"
    );
    assert!(
        !err.contains("configure one"),
        "a reachable backend must never claim the system is unconfigured: {body}"
    );

    shutdown.cancel();
}

#[tokio::test]
async fn chat_capacity_timeout_fails_turn() {
    // LLM concurrency spec §3.3: a turn that never gets a slot within
    // `chat_wait_secs` finalizes `failed` with the capacity message —
    // and the LLM is never called (the run never started).
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    // A capturing mock with an empty script: a request would 500, so
    // "no request recorded" is the no-run observable.
    let (llm_url, requests) = capturing_mock_llm(vec![]).await;
    let (base, shutdown, _dir) = boot_with(mycelium_web::LlmConfig {
        url: llm_url,
        model: "mock".into(),
        api_key: None,
        // One slot, a one-second wait bound: the runner syncs both from
        // this row (no new plumbing — the admin page's own path).
        max_parallel: 1,
        chat_wait_secs: 1,
        ..Default::default()
    })
    .await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // Hold the single slot for the whole submit (anonymous hold — the
    // per-user cap must not be what rejects this turn).
    LLM_GATE.set_capacity(1);
    let hold = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("the only slot is free before the submit");

    let resp = submit_draft(&client, &base, &cookie, &csrf, "asked at capacity").await;
    assert_eq!(resp.status(), 200, "the submit itself still streams");
    let body = resp.text().await.unwrap();
    let events = sse_events(&body);
    let error = events
        .iter()
        .find(|e| e["type"] == "error")
        .unwrap_or_else(|| panic!("a capacity error event: {events:?}"));
    assert_eq!(
        error["error"], CHAT_CAPACITY_MSG,
        "the pinned capacity message: {events:?}"
    );
    let session_id = events[0]["session_id"].as_str().expect("session_id");

    // Persisted: a reconnecting client sees the failed turn + the row.
    let meta = poll_meta_until(&client, &base, &cookie, session_id, "failed").await;
    assert_eq!(meta["turn_status"], "failed");
    assert!(
        requests.lock().await.is_empty(),
        "a turn that never got a slot never calls the LLM"
    );
    drop(hold);
    shutdown.cancel();
}

#[tokio::test]
async fn shutdown_releases_queued_turn_as_interrupted() {
    // LLM concurrency spec §3.5 (controller ruling R31): when the
    // shutdown token fires, `serve` closes the gate — every queued
    // waiter is released at once rather than sitting out its wait
    // bound inside the graceful-shutdown window. A turn released that
    // way never ran, so it finalizes `interrupted` with NO error row:
    // byte for byte the state a crash leaves behind after the boot
    // sweep (`interrupted`, `attempts = 0`), which is exactly what the
    // resume path claims. It is NOT `failed`, and it carries no
    // capacity message — nothing was at capacity.
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    // A reachable mock with an empty script: a request would 500, so
    // "no request recorded" proves the run never started.
    let (llm_url, requests) = capturing_mock_llm(vec![]).await;
    let (base, shutdown, dir, serve) = boot_with_handle(mycelium_web::LlmConfig {
        url: llm_url,
        model: "mock".into(),
        api_key: None,
        // One slot and a wait bound far longer than this test: a
        // finalized turn here can only come from the close path, never
        // from the bound elapsing, and a `serve` that waited the
        // waiter out would blow the 15 s assertion below.
        max_parallel: 1,
        chat_wait_secs: 600,
        ..Default::default()
    })
    .await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;
    // A second pool over the same file: the turn row stays observable
    // after the server is gone (no endpoint to ask).
    let store = Store::open(dir.path()).await.unwrap();

    // Hold the single slot anonymously (the per-user cap must not be
    // what queues this turn).
    LLM_GATE.set_capacity(1);
    let hold = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("the only slot is free before the submit");

    // Submit: the runner queues on the gate. Its SSE stream stays open
    // for the life of the turn, so drive it in a task.
    let (c2, b2, ck2, cs2) = (client.clone(), base.clone(), cookie.clone(), csrf.clone());
    let submit = tokio::spawn(async move {
        let resp = submit_draft(&c2, &b2, &ck2, &cs2, "queued when the server stops").await;
        assert_eq!(resp.status(), 200, "the queued submit still streams");
        // The stream ends when the runner drops its broadcast sender;
        // a body cut short by the shutdown is fine here — the
        // capacity-message assertion holds either way.
        resp.text().await.unwrap_or_default()
    });
    poll_gate_until(
        || LLM_GATE.stats().queued == 1,
        "the submitted turn queued on the gate",
    )
    .await;

    // Shut down. `serve` must come back promptly: the queued waiter is
    // released by `close()`, not waited out for its 600 s bound.
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(15), serve)
        .await
        .expect("serve returned after the shutdown token fired")
        .expect("the serve task did not panic");

    // The turn is the resumable state, not a failure.
    let deadline = Instant::now() + Duration::from_secs(10);
    let turn = loop {
        let row: (String, i64) = sqlx::query_as("SELECT status, attempts FROM chat_turns LIMIT 1")
            .fetch_one(store.pool())
            .await
            .unwrap();
        if row.0 != "running" {
            break row;
        }
        assert!(
            Instant::now() < deadline,
            "the queued turn never left `running`: {row:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(
        turn,
        ("interrupted".to_string(), 0),
        "a fresh turn released by the shutdown is interrupted with attempts = 0"
    );

    // No error row at all — so certainly not the capacity message
    // (`role` is the one plaintext column; content is sealed).
    let error_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_messages WHERE role = ?")
        .bind("error")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(
        error_rows, 0,
        "the shutdown path writes no error row (no capacity message)"
    );
    let body = submit.await.unwrap();
    assert!(
        !body.contains(CHAT_CAPACITY_MSG),
        "the stream never carried the capacity message: {body}"
    );
    assert!(
        requests.lock().await.is_empty(),
        "a turn that never got a slot never calls the LLM"
    );

    // And the resume path picks it up: `resumable_turns` is the exact
    // predicate `claim_resumable` enforces (`interrupted`,
    // `attempts = 0`).
    let chat = mycelium_store::chat::ChatStore::new(store.pool().clone());
    let resumable = chat.resumable_turns().await.unwrap();
    assert_eq!(
        resumable.len(),
        1,
        "the released turn is claimable by the resume worker: {resumable:?}"
    );

    drop(hold);
}

#[tokio::test]
async fn chat_admits_after_slot_frees() {
    // The wait class, end to end: with every slot held the turn queues
    // (observable: the gate's own `queued` count — Task 5 turns the
    // same positions into SSE events), and it runs to `done` as soon as
    // a slot frees.
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    let llm_url = mock_llm(vec![text_response("answered after the wait")]).await;
    let (base, shutdown, _dir) = boot_with(mycelium_web::LlmConfig {
        url: llm_url,
        model: "mock".into(),
        api_key: None,
        max_parallel: 1,
        // Generous bound: this test pins admission-after-release, not
        // the timeout path.
        chat_wait_secs: 60,
        ..Default::default()
    })
    .await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    LLM_GATE.set_capacity(1);
    let hold = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("the only slot is free before the submit");

    // The submit streams until the turn finishes — drive it in a task
    // so the hold can be released mid-wait.
    let (c2, b2, ck2, cs2) = (client.clone(), base.clone(), cookie.clone(), csrf.clone());
    let submit = tokio::spawn(async move {
        let resp = submit_draft(&c2, &b2, &ck2, &cs2, "asked while queued").await;
        assert_eq!(resp.status(), 200, "queued submit still streams");
        resp.text().await.unwrap()
    });

    poll_gate_until(
        || LLM_GATE.stats().queued >= 1,
        "the turn queued on the gate",
    )
    .await;
    // Still queued = nothing ran: the turn row is `running` (queue
    // state is presentation, not lifecycle — spec §3.3).
    let session_id = first_session_id(&client, &base, &cookie).await;
    let meta = get_meta(&client, &base, &cookie, &session_id).await;
    assert_eq!(meta["turn_status"], "running", "queued turns stay running");

    drop(hold);

    let body = submit.await.unwrap();
    let events = sse_events(&body);
    let done = events
        .iter()
        .find(|e| e["type"] == "done")
        .unwrap_or_else(|| panic!("the admitted turn finishes: {events:?}"));
    assert_eq!(done["reply"], "answered after the wait");
    let meta = poll_meta_until(&client, &base, &cookie, &session_id, "done").await;
    assert_eq!(meta["turn_status"], "done");
    shutdown.cancel();
}

#[tokio::test]
async fn chat_sse_emits_queued_then_admitted() {
    // LLM concurrency spec §3.3/§3.4: a turn that waits for a slot
    // streams `queued` with its position LIVE — before any step event —
    // and `admitted` once a slot frees. Both are broadcast-only
    // presentation: no `seq` on the wire, no transcript row, and so
    // nothing for the reattach replay to carry.
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    // LIFO script: the tool call is served first, then the answer — so
    // the run has a step event the queue events must precede.
    let llm_url = mock_llm(vec![
        text_response("answered after queueing"),
        tool_call_response(
            "call-1",
            "search_knowledge",
            serde_json::json!({ "query": "queue" }),
        ),
    ])
    .await;
    let (base, shutdown, _dir) = boot_with(mycelium_web::LlmConfig {
        url: llm_url,
        model: "mock".into(),
        api_key: None,
        max_parallel: 1,
        // Generous bound: this pins the queue events, not the timeout.
        chat_wait_secs: 60,
        ..Default::default()
    })
    .await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // One slot, held anonymously (the per-user cap must not be what
    // queues this turn): the submit's turn has to wait.
    LLM_GATE.set_capacity(1);
    let hold = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("the only slot is free before the submit");

    let resp = submit_draft(
        &client,
        &base,
        &cookie,
        &csrf,
        "asked while the gate is full",
    )
    .await;
    assert_eq!(resp.status(), 200, "the queued submit still streams");
    let mut sse = SseReader::new(resp);

    // `queued` arrives while the slot is still held — the run has not
    // started, so no step event can have streamed yet.
    let queued_at = sse.read_until("queued").await;
    assert_eq!(
        sse.events[queued_at]["position"], 0,
        "the only waiter sits at the head of the queue: {:?}",
        sse.events
    );
    assert!(
        sse.events[queued_at].get("seq").is_none(),
        "a queue event carries no seq: {:?}",
        sse.events
    );
    assert!(
        sse.index_of("tool").is_none()
            && sse.index_of("done").is_none()
            && sse.index_of("admitted").is_none(),
        "nothing of the run streams before admission: {:?}",
        sse.kinds()
    );
    let session_id = sse.events[0]["session_id"]
        .as_str()
        .expect("the first event is the draft session event")
        .to_string();

    // Release the slot: the turn is admitted and runs to done.
    drop(hold);

    let admitted_at = sse.read_until("admitted").await;
    assert!(
        admitted_at > queued_at,
        "admitted follows queued: {:?}",
        sse.kinds()
    );
    assert!(
        sse.events[admitted_at].get("seq").is_none(),
        "a queue event carries no seq: {:?}",
        sse.events
    );
    let tool_at = sse.read_until("tool").await;
    let done_at = sse.read_until("done").await;
    assert!(
        admitted_at < tool_at && tool_at < done_at,
        "the queue events precede every step event: {:?}",
        sse.kinds()
    );
    assert_eq!(sse.events[done_at]["reply"], "answered after queueing");
    // The whole observed order, pinned.
    assert_eq!(
        sse.kinds(),
        vec!["session", "queued", "admitted", "tool", "done"],
        "the submit stream's event order"
    );

    // The transcript is untouched by the queue events: the reattach
    // replay (?after=0) carries the persisted rows only.
    let meta = poll_meta_until(&client, &base, &cookie, &session_id, "done").await;
    assert_eq!(meta["turn_status"], "done");
    let replay = client
        .get(format!(
            "{base}/api/v1/chat/sessions/{session_id}/stream?after=0"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), 200);
    let revents = sse_events(&replay.text().await.unwrap());
    let rkinds: Vec<&str> = revents
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert!(
        !rkinds.contains(&"queued") && !rkinds.contains(&"admitted"),
        "queue events are never replayed: {rkinds:?}"
    );
    assert_eq!(
        rkinds,
        vec!["tool", "assistant"],
        "the replay vocabulary is the persisted rows"
    );

    shutdown.cancel();
}

#[tokio::test]
async fn chat_submit_429_at_depth_cap() {
    // LLM concurrency spec §3.3: with the gate's waiter queue at its
    // configured depth, a NEW draft submit is refused 429 +
    // `Retry-After: 5` BEFORE any row is written — no empty draft
    // session, no zombie turn (the client can retry). The turn already
    // queued is untouched and runs normally once a slot frees.
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    let llm_url = mock_llm(vec![text_response("answered once the queue drained")]).await;
    let (base, shutdown, dir) = boot_with(mycelium_web::LlmConfig {
        url: llm_url,
        model: "mock".into(),
        api_key: None,
        // One slot and room for exactly one waiter; a generous wait
        // bound, so nothing here depends on the timeout path.
        max_parallel: 1,
        chat_queue_depth: 1,
        chat_wait_secs: 60,
        ..Default::default()
    })
    .await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;
    // A second pool over the same file: the row-count observable.
    let store = Store::open(dir.path()).await.unwrap();

    // Hold the single slot anonymously (the per-user cap must not be
    // what queues the first turn).
    LLM_GATE.set_capacity(1);
    let hold = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("the only slot is free before the submit");

    // Submit A: its runner takes the one queue place. The stream stays
    // open until the turn finishes, so drive it in a task.
    let (c2, b2, ck2, cs2) = (client.clone(), base.clone(), cookie.clone(), csrf.clone());
    let submit_a = tokio::spawn(async move {
        let resp = submit_draft(&c2, &b2, &ck2, &cs2, "first in the queue").await;
        assert_eq!(resp.status(), 200, "the queued submit still streams");
        resp.text().await.unwrap()
    });
    // The runner is async: wait for the REAL queue state, never assume it.
    poll_gate_until(|| LLM_GATE.stats().queued == 1, "turn A queued on the gate").await;
    let before = chat_row_counts(&store).await;
    assert_eq!(
        before,
        (1, 1),
        "A's session and turn rows exist: {before:?}"
    );

    // Submit B: the queue is full (depth 1) → refused, nothing written.
    let refused = submit_draft(&client, &base, &cookie, &csrf, "second in the queue").await;
    assert_eq!(
        refused.status(),
        429,
        "a submit beyond the depth cap is refused"
    );
    assert_eq!(
        refused
            .headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap()),
        Some("5"),
        "the refusal carries Retry-After: 5"
    );
    let body: serde_json::Value = refused.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({ "error": CHAT_QUEUE_FULL_MSG }),
        "the pinned 429 body"
    );
    assert_eq!(
        chat_row_counts(&store).await,
        before,
        "the refusal wrote neither a session nor a turn row"
    );
    assert_eq!(
        LLM_GATE.stats().queued,
        1,
        "the refused submit never enqueued: {:?}",
        LLM_GATE.stats()
    );

    // Release the slot: A is admitted and finishes normally.
    drop(hold);
    let events = sse_events(&submit_a.await.unwrap());
    let done = events
        .iter()
        .find(|e| e["type"] == "done")
        .unwrap_or_else(|| panic!("the queued turn still finishes: {events:?}"));
    assert_eq!(done["reply"], "answered once the queue drained");
    let session_id = first_session_id(&client, &base, &cookie).await;
    let meta = poll_meta_until(&client, &base, &cookie, &session_id, "done").await;
    assert_eq!(meta["turn_status"], "done");
    assert_eq!(
        chat_row_counts(&store).await,
        (1, 1),
        "still exactly one session and one turn"
    );
    shutdown.cancel();
}

#[tokio::test]
async fn chat_submit_session_429_at_depth_cap() {
    // The same cap on the existing-session endpoint (spec §3.3 applies
    // to the submit surface, not just the draft flow): the target
    // session has NO running turn, so a 409 cannot be what refuses this
    // — it is the depth cap, and it lands before the turn row.
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    // LIFO script: the warm-up turn is served first, then turn A's.
    let llm_url = mock_llm(vec![
        text_response("answered once the queue drained"),
        text_response("warm-up reply"),
    ])
    .await;
    let (base, shutdown, dir) = boot_with(mycelium_web::LlmConfig {
        url: llm_url,
        model: "mock".into(),
        api_key: None,
        max_parallel: 1,
        chat_queue_depth: 1,
        chat_wait_secs: 60,
        ..Default::default()
    })
    .await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;
    let store = Store::open(dir.path()).await.unwrap();

    // A finished session to submit into later (no slot held yet, so it
    // runs straight through).
    let (session_id, events) =
        submit_draft_collect(&client, &base, &cookie, &csrf, "warm up").await;
    assert_eq!(
        events.last().map(|e| e["type"].clone()),
        Some(serde_json::json!("done")),
        "the warm-up turn finishes: {events:?}"
    );
    let warm_meta = poll_meta_until(&client, &base, &cookie, &session_id, "done").await;
    assert_eq!(warm_meta["max_seq"], 2, "user + assistant rows");

    // Fill the gate: one slot held, one waiter queued (a second,
    // separate session's turn).
    LLM_GATE.set_capacity(1);
    let hold = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("the only slot is free before the submit");
    let (c2, b2, ck2, cs2) = (client.clone(), base.clone(), cookie.clone(), csrf.clone());
    let submit_a = tokio::spawn(async move {
        let resp = submit_draft(&c2, &b2, &ck2, &cs2, "first in the queue").await;
        assert_eq!(resp.status(), 200, "the queued submit still streams");
        resp.text().await.unwrap()
    });
    poll_gate_until(|| LLM_GATE.stats().queued == 1, "turn A queued on the gate").await;
    let before = chat_row_counts(&store).await;
    assert_eq!(before, (2, 2), "two sessions, two turns: {before:?}");

    // Submit into the IDLE session while the queue is full → 429.
    let refused = submit_into_session(
        &client,
        &base,
        &cookie,
        &csrf,
        &session_id,
        "a second turn in a quiet session",
    )
    .await;
    assert_eq!(
        refused.status(),
        429,
        "the existing-session submit is capped too"
    );
    assert_eq!(
        refused
            .headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap()),
        Some("5"),
        "the refusal carries Retry-After: 5"
    );
    let body: serde_json::Value = refused.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({ "error": CHAT_QUEUE_FULL_MSG }),
        "the pinned 429 body"
    );
    assert_eq!(
        chat_row_counts(&store).await,
        before,
        "the refusal wrote no turn row"
    );
    let meta = get_meta(&client, &base, &cookie, &session_id).await;
    assert_eq!(
        meta["turn_status"], "done",
        "the refused submit left the session's last turn alone: {meta}"
    );
    assert_eq!(meta["max_seq"], 2, "no new user row: {meta}");
    assert_eq!(
        LLM_GATE.stats().queued,
        1,
        "the refused submit never enqueued: {:?}",
        LLM_GATE.stats()
    );

    // Release: the queued turn finishes normally.
    drop(hold);
    let events = sse_events(&submit_a.await.unwrap());
    let done = events
        .iter()
        .find(|e| e["type"] == "done")
        .unwrap_or_else(|| panic!("the queued turn still finishes: {events:?}"));
    assert_eq!(done["reply"], "answered once the queue drained");
    shutdown.cancel();
}

#[tokio::test]
async fn stateless_chat_429_when_saturated() {
    // LLM concurrency spec §3.3: the stateless one-shot
    // `POST /api/v1/chat` is wait class, bounded by `query_wait_secs`.
    // With every slot held it waits the bound and then 429s with
    // `Retry-After: 5` — distinguishable from the 503 it returns when
    // the LLM is unreachable.
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    // A REACHABLE mock with an empty script: a request would 500, so
    // "no request recorded" proves the run never started — and the
    // 429 cannot be the unreachable-LLM 503 in disguise.
    let (llm_url, requests) = capturing_mock_llm(vec![]).await;
    let (base, shutdown, _dir) = boot_with(mycelium_web::LlmConfig {
        url: llm_url,
        model: "mock".into(),
        api_key: None,
        // Two slots (the default), a one-second wait bound: the handler
        // syncs the gate from this row, as the admin page's save does.
        max_parallel: 2,
        query_wait_secs: 1,
        ..Default::default()
    })
    .await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    let timeouts_before = LLM_GATE.stats().timeouts_total;
    // Saturate: anonymous holds, so the per-user cap is not what
    // rejects this request — only the global capacity.
    let hold_a = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("slot 1 of 2 is free before the request");
    let hold_b = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("slot 2 of 2 is free before the request");

    let resp = client
        .post(format!("{base}/api/v1/chat"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("message", "asked while the gate is full")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429, "capacity, not 503 unreachable");
    assert_eq!(
        resp.headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap()),
        Some("5"),
        "the refusal carries Retry-After: 5"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({ "error": mycelium_web::api::LLM_CAPACITY_MSG }),
        "the pinned 429 body"
    );
    assert!(
        requests.lock().await.is_empty(),
        "a request that never got a slot never calls the LLM"
    );
    assert_eq!(
        LLM_GATE.stats().timeouts_total,
        timeouts_before + 1,
        "exactly one gate timeout — the 429 came from the gate: {:?}",
        LLM_GATE.stats()
    );
    drop(hold_a);
    drop(hold_b);
    shutdown.cancel();
}

#[tokio::test]
async fn dream_429_when_saturated() {
    // LLM concurrency spec §3.3/§5: dream is wait class with the chat
    // bound. Saturated → the endpoint-wide JSON error shape with the
    // capacity message and a 429, never a hang.
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    let (llm_url, requests) = capturing_mock_llm(vec![]).await;
    let (base, shutdown, _dir) = boot_with(mycelium_web::LlmConfig {
        url: llm_url,
        model: "mock".into(),
        api_key: None,
        max_parallel: 2,
        chat_wait_secs: 1,
        ..Default::default()
    })
    .await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // No session → the login gate redirects, before any admission.
    let no_auth = client
        .post(format!("{base}/api/v1/dream"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        no_auth.status(),
        303,
        "unauthenticated must hit the login gate"
    );

    let timeouts_before = LLM_GATE.stats().timeouts_total;
    let hold_a = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("slot 1 of 2 is free before the request");
    let hold_b = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("slot 2 of 2 is free before the request");

    let resp = client
        .post(format!("{base}/api/v1/dream"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429, "capacity refusal, not a 500 or a hang");
    assert_eq!(
        resp.headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap()),
        Some("5"),
        "the refusal carries Retry-After: 5"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({ "error": mycelium_web::api::LLM_CAPACITY_MSG }),
        "dream's error-envelope shape with the capacity message"
    );
    assert!(
        requests.lock().await.is_empty(),
        "a dream that never got a slot never calls the LLM"
    );
    assert_eq!(
        LLM_GATE.stats().timeouts_total,
        timeouts_before + 1,
        "exactly one gate timeout — the 429 came from the gate: {:?}",
        LLM_GATE.stats()
    );
    drop(hold_a);
    drop(hold_b);
    shutdown.cancel();
}

/// The value of a Prometheus sample line (the first line that starts
/// with `<name> `), so a `# TYPE` line is never read as a sample.
fn metric_sample(text: &str, name: &str) -> String {
    let prefix = format!("{name} ");
    let line = text
        .lines()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("/metrics has no sample line for {name}:\n{text}"));
    line[prefix.len()..].trim().to_string()
}

/// /health's `llm_*` fields and /metrics' gate lines report LIVE gate
/// state (spec §3.6): with one slot held, both surfaces see an active
/// run, and both go back to zero once the permit drops. The counters
/// are asserted RELATIVELY (before/after) — LLM_GATE is process-global,
/// so no absolute total is pinned. Runs under this binary's gate lock
/// with a defaults guard (it holds a slot).
#[tokio::test]
async fn health_and_metrics_reflect_live_gate_state() {
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    let llm = mock_llm(vec![]).await; // never called: no chat turn here
    let (base, shutdown, _dir) = boot(llm).await;
    let client = client();

    let before = LLM_GATE.stats();
    let permit = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("a slot is free with the whole gate to ourselves");
    assert!(
        LLM_GATE.stats().active >= 1,
        "the held permit is active: {:?}",
        LLM_GATE.stats()
    );

    let held: serde_json::Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        held["llm_active"].as_i64().unwrap() >= 1,
        "/health shows the held slot: {held}"
    );
    assert_eq!(
        held["llm_queued"].as_i64().unwrap(),
        0,
        "nothing is queued behind us: {held}"
    );
    assert_eq!(
        held["llm_capacity"].as_i64().unwrap(),
        DEFAULT_CAPACITY as i64,
        "the configured slot count, not a literal: {held}"
    );
    assert_eq!(
        held["status"], "ok",
        "a held gate slot never degrades health: {held}"
    );

    let text = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metric_sample(&text, "mycelium2_llm_gate_active")
            .parse::<u64>()
            .unwrap()
            >= 1,
        "the active gauge shows the held slot:\n{text}"
    );
    assert_eq!(
        metric_sample(&text, "mycelium2_llm_gate_capacity"),
        DEFAULT_CAPACITY.to_string(),
        "the capacity gauge mirrors the gate"
    );
    assert!(
        metric_sample(&text, "mycelium2_llm_gate_admissions_total")
            .parse::<u64>()
            .unwrap()
            > before.admissions_total,
        "our admission moved the counter (relative, not absolute):\n{text}"
    );
    assert!(
        metric_sample(&text, "mycelium2_llm_gate_wait_count")
            .parse::<u64>()
            .unwrap()
            > before.wait_count,
        "the wait-class admission recorded a wait sample:\n{text}"
    );
    assert!(
        metric_sample(&text, "mycelium2_llm_gate_wait_seconds_total")
            .parse::<f64>()
            .unwrap()
            // 1e-6 slack: the line is rendered at six decimals.
            >= before.wait_seconds_total - 1e-6,
        "the cumulative wait never goes backwards:\n{text}"
    );
    assert!(
        metric_sample(&text, "mycelium2_llm_gate_timeouts_total")
            .parse::<u64>()
            .unwrap()
            >= before.timeouts_total,
        "the timeout counter never goes backwards:\n{text}"
    );

    drop(permit);
    poll_gate_until(|| LLM_GATE.stats().active == 0, "the gate draining").await;
    let drained: serde_json::Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        drained["llm_active"].as_i64().unwrap(),
        0,
        "the released slot is reported free: {drained}"
    );

    shutdown.cancel();
}

// ---------------------------------------------------------------------
// Admin LLM concurrency fields + live gate sync (LLM concurrency spec
// §3.2/§3.6). The admin form carries the five tunables; saving clamps
// them and pushes them onto the process-global gate immediately, and
// boot syncs the stored row before the listener accepts traffic.
// ---------------------------------------------------------------------

/// POST /admin/llm with an arbitrary field set (the page's own flow:
/// admin cookie + CSRF header). Fields are passed verbatim so a test
/// can omit the concurrency inputs entirely (an old cached form).
async fn post_admin_llm(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
    fields: &[(&str, &str)],
) -> reqwest::Response {
    client
        .post(format!("{base}/admin/llm"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .form(fields)
        .send()
        .await
        .unwrap()
}

/// Decrypt the stored "llm" row from the server's tempdir (the on-disk
/// service key is idempotent: same file, same key).
async fn stored_llm(dir: &std::path::Path) -> mycelium_web::LlmConfig {
    let store = Store::open(dir).await.unwrap();
    let service_key = mycelium_crypto::load_or_create_service_key_with(dir, None).unwrap();
    let config = mycelium_store::config::ConfigStore::new(store.pool().clone());
    mycelium_store::config::get_sealed(&config, &service_key, "llm")
        .await
        .expect("a stored llm row")
}

/// The `<input ...>` tag carrying `name="<field>"`, for attribute
/// assertions that do not pin the whole rendered line.
fn input_tag<'a>(html: &'a str, field: &str) -> &'a str {
    let marker = format!(r#"name="{field}""#);
    let idx = html
        .find(&marker)
        .unwrap_or_else(|| panic!("no input named {field} in the admin LLM form"));
    let start = html[..idx].rfind('<').expect("an opening tag");
    let end = idx + html[idx..].find('>').expect("a closing bracket") + 1;
    &html[start..end]
}

#[tokio::test]
async fn admin_llm_form_has_concurrency_fields() {
    // Spec §3.6: the five tunables render with their CURRENT stored
    // value — clamped, so a stale out-of-range row can never put 999
    // back in the box — plus the range as min/max attributes.
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    let llm_url = mock_llm(vec![]).await;
    let (base, shutdown, _dir) = boot_with(mycelium_web::LlmConfig {
        url: llm_url,
        model: "mock".into(),
        api_key: None,
        // Deliberately out of range: the form must render the clamp.
        max_parallel: 999,
        per_user_active: 5,
        query_wait_secs: 45,
        chat_wait_secs: 900,
        chat_queue_depth: 128,
    })
    .await;
    let client = client();
    let (cookie, _csrf) = login_admin(&client, &base).await;

    let html = client
        .get(format!("{base}/admin/llm"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    for (field, value, min, max) in [
        ("max_parallel", "64", "1", "64"),
        ("per_user_active", "5", "1", "16"),
        ("query_wait_secs", "45", "1", "600"),
        ("chat_wait_secs", "900", "1", "7200"),
        ("chat_queue_depth", "128", "1", "1024"),
    ] {
        let tag = input_tag(&html, field);
        assert!(
            tag.contains(r#"type="number""#),
            "{field} is a number input: {tag}"
        );
        assert!(
            tag.contains(&format!(r#"value="{value}""#)),
            "{field} renders the clamped stored value {value}: {tag}"
        );
        assert!(
            tag.contains(&format!(r#"min="{min}""#)) && tag.contains(&format!(r#"max="{max}""#)),
            "{field} carries its clamp range {min}..={max}: {tag}"
        );
    }
    // The range is visible to a human too, not just to the browser.
    assert!(
        html.contains("1&ndash;64") || html.contains("1–64") || html.contains("1-64"),
        "the max_parallel range hint is visible in the form"
    );
    shutdown.cancel();
}

#[tokio::test]
async fn admin_save_syncs_gate_immediately() {
    // Spec §3.2: "admin_save_llm syncs immediately on save — the admin
    // sees the effect without waiting for traffic. No restart." The
    // queued waiter is admitted by the save itself: nothing releases a
    // slot (the hold is still held when the assertion runs).
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    let llm_url = mock_llm(vec![]).await;
    let (base, shutdown, _dir) = boot_with(mycelium_web::LlmConfig {
        url: llm_url.clone(),
        model: "mock".into(),
        api_key: None,
        max_parallel: 1,
        ..Default::default()
    })
    .await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    LLM_GATE.set_capacity(1);
    let hold = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("the only slot is free before the save");
    // One waiter queued behind it.
    let waiter =
        tokio::spawn(async move { LLM_GATE.enter(None, GATE_BOUND, |_| {}).await.is_ok() });
    poll_gate_until(|| LLM_GATE.stats().queued == 1, "one waiter queued").await;

    let resp = post_admin_llm(
        &client,
        &base,
        &cookie,
        &csrf,
        &[
            ("url", llm_url.as_str()),
            ("model", "mock"),
            ("max_parallel", "2"),
        ],
    )
    .await;
    assert_eq!(resp.status(), 303, "admin save redirects");
    assert_eq!(
        LLM_GATE.capacity(),
        2,
        "the save synced the gate with no traffic: {:?}",
        LLM_GATE.stats()
    );

    // The raise admitted the waiter — while the first slot is STILL
    // held (nothing was released).
    let admitted = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("the queued waiter is admitted by the capacity raise")
        .unwrap();
    assert!(admitted, "the waiter got a slot, not a timeout");
    drop(hold);
    shutdown.cancel();
}

#[tokio::test]
async fn admin_save_clamps_out_of_range() {
    // Spec §3.2: server-side validation is clamping, never a 500 and
    // never an unbounded gate.
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    let llm_url = mock_llm(vec![]).await;
    let (base, shutdown, dir) = boot_with(mycelium_web::LlmConfig {
        url: llm_url.clone(),
        model: "mock".into(),
        api_key: None,
        ..Default::default()
    })
    .await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    let resp = post_admin_llm(
        &client,
        &base,
        &cookie,
        &csrf,
        &[
            ("url", llm_url.as_str()),
            ("model", "mock"),
            ("max_parallel", "999"),
            ("per_user_active", "99"),
            ("chat_queue_depth", "99999"),
        ],
    )
    .await;
    assert_eq!(resp.status(), 303);
    let stored = stored_llm(dir.path()).await;
    assert_eq!(stored.max_parallel, 64, "max_parallel clamped at rest");
    assert_eq!(
        stored.per_user_active, 16,
        "per_user_active clamped at rest"
    );
    assert_eq!(
        stored.chat_queue_depth, 1024,
        "chat_queue_depth clamped at rest"
    );
    assert_eq!(LLM_GATE.capacity(), 64, "the gate took the clamped value");

    let resp = post_admin_llm(
        &client,
        &base,
        &cookie,
        &csrf,
        &[
            ("url", llm_url.as_str()),
            ("model", "mock"),
            ("max_parallel", "0"),
        ],
    )
    .await;
    assert_eq!(resp.status(), 303);
    let stored = stored_llm(dir.path()).await;
    assert_eq!(stored.max_parallel, 1, "zero clamps up to the floor");
    assert_eq!(LLM_GATE.capacity(), 1, "the gate took the clamped floor");
    shutdown.cancel();
}

#[tokio::test]
async fn admin_save_without_concurrency_fields_keeps_stored_values() {
    // An old cached form (or a non-numeric value) must save url/model
    // without resetting the tunables — and must not 500.
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    let llm_url = mock_llm(vec![]).await;
    let (base, shutdown, dir) = boot_with(mycelium_web::LlmConfig {
        url: llm_url.clone(),
        model: "mock".into(),
        api_key: None,
        max_parallel: 5,
        per_user_active: 3,
        query_wait_secs: 45,
        chat_wait_secs: 900,
        chat_queue_depth: 128,
    })
    .await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    let resp = post_admin_llm(
        &client,
        &base,
        &cookie,
        &csrf,
        &[("url", "http://elsewhere.invalid/v1"), ("model", "other")],
    )
    .await;
    assert_eq!(resp.status(), 303, "a form without the inputs still saves");
    let stored = stored_llm(dir.path()).await;
    assert_eq!(stored.url, "http://elsewhere.invalid/v1");
    assert_eq!(stored.model, "other");
    assert_eq!(stored.max_parallel, 5);
    assert_eq!(stored.per_user_active, 3);
    assert_eq!(stored.query_wait_secs, 45);
    assert_eq!(stored.chat_wait_secs, 900);
    assert_eq!(stored.chat_queue_depth, 128);

    // A non-numeric value is the same case: keep the stored value, no 500.
    let resp = post_admin_llm(
        &client,
        &base,
        &cookie,
        &csrf,
        &[
            ("url", "http://elsewhere.invalid/v1"),
            ("model", "other"),
            ("max_parallel", "not a number"),
        ],
    )
    .await;
    assert_eq!(resp.status(), 303, "a junk value must not 500");
    assert_eq!(stored_llm(dir.path()).await.max_parallel, 5);
    shutdown.cancel();
}

#[tokio::test]
async fn boot_syncs_gate_from_stored_config() {
    // Spec §3.2 + R29(c): a stored `max_parallel` takes effect at boot,
    // before the listener accepts — the try-class workers (drain, chat
    // resume) would otherwise see the default capacity until some
    // request happened to sync.
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    LLM_GATE.set_capacity(DEFAULT_CAPACITY);
    assert_ne!(DEFAULT_CAPACITY, 5, "the test value must differ");
    let llm_url = mock_llm(vec![]).await;
    let (_base, shutdown, _dir) = boot_with(mycelium_web::LlmConfig {
        url: llm_url,
        model: "mock".into(),
        api_key: None,
        max_parallel: 5,
        ..Default::default()
    })
    .await;
    // No request has been made: boot alone must have applied the row.
    poll_gate_until(
        || LLM_GATE.capacity() == 5,
        "boot syncing the stored capacity",
    )
    .await;
    shutdown.cancel();
}

#[tokio::test]
async fn sync_llm_gate_from_store_applies_the_stored_limits() {
    // The shared boot-sync entry point (`main` calls it before the
    // queue drain pass and before either worker is spawned; `serve`
    // calls it again, idempotently). Asserted WITHOUT a server: the
    // function alone must make a stored row live, clamped, so the
    // try-class boot work cannot run against the defaults.
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    LLM_GATE.set_capacity(DEFAULT_CAPACITY);

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let service_key = mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();
    let login = LoginService::new(store.pool().clone());
    let state = AppState::new(
        store,
        service_key,
        login,
        dir.path().join("assets"),
        dir.path().to_path_buf(),
        None,
    );
    // Stored out of range on purpose: the sync clamps as it applies.
    state
        .config
        .set(
            "llm",
            &mycelium_web::LlmConfig {
                url: "http://unused.invalid/v1".into(),
                model: "mock".into(),
                api_key: None,
                max_parallel: 999,
                per_user_active: 7,
                chat_queue_depth: 11,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    mycelium_web::server::sync_llm_gate_from_store(&state).await;
    assert_eq!(LLM_GATE.capacity(), 64, "clamped capacity applied");
    assert_eq!(LLM_GATE.per_user_active(), 7, "per-user cap applied");
    assert_eq!(
        LLM_GATE.chat_queue_depth_limit(),
        11,
        "queue-depth cap applied"
    );

    // Idempotent: calling it again (as `serve` does after `main`) is a
    // no-op on the same row.
    mycelium_web::server::sync_llm_gate_from_store(&state).await;
    assert_eq!(LLM_GATE.capacity(), 64);
}

// ---------------------------------------------------------------------
// /admin/system LLM readout card (LLM concurrency spec §3.6: "an LLM
// card readout (active/queued/capacity, recent timeouts) next to the
// existing queue table"). The card renders from the request's ONE
// `LLM_GATE.stats()` read, so slots held here must show up in the
// markup as live values — never the all-zero shape a hardcoded card
// would print.
// ---------------------------------------------------------------------

/// The card's rendered stat row for `label`, as the page composes it.
fn llm_stat_row(label: &str, value: &str) -> String {
    format!("<strong>{label}</strong> <span>{value}</span>")
}

#[tokio::test]
async fn admin_system_shows_llm_readout() {
    let _g = TEST_LOCK.lock().await;
    let _gate = GateDefaults;
    let llm_url = mock_llm(vec![]).await;
    let (base, shutdown, _dir) = boot_with(mycelium_web::LlmConfig {
        url: llm_url,
        model: "mock".into(),
        api_key: None,
        max_parallel: 2,
        ..Default::default()
    })
    .await;
    let client = client();
    let (cookie, _csrf) = login_admin(&client, &base).await;

    LLM_GATE.set_capacity(2);
    // Both slots held: active == capacity == 2.
    let _hold_a = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("the first slot is free");
    let _hold_b = LLM_GATE
        .enter(None, GATE_BOUND, |_| {})
        .await
        .expect("the second slot is free");

    // One wait-class admission that cannot be served: its bound elapses
    // against the full gate, so the cumulative timeout counter moves
    // (proving the card's timeouts row is read, not printed as a 0).
    let timeouts_before = LLM_GATE.stats().timeouts_total;
    assert!(
        LLM_GATE
            .enter(None, Duration::from_millis(50), |_| {})
            .await
            .is_err(),
        "a full gate cannot admit within a 50ms bound"
    );
    let want_timeouts = timeouts_before + 1;
    poll_gate_until(
        || LLM_GATE.stats().timeouts_total == want_timeouts,
        "the timeout counter advancing",
    )
    .await;

    // …and one waiter still queued when the page renders.
    let waiter =
        tokio::spawn(async move { LLM_GATE.enter(None, GATE_BOUND, |_| {}).await.is_ok() });
    poll_gate_until(|| LLM_GATE.stats().queued == 1, "one waiter queued").await;

    let html = client
        .get(format!("{base}/admin/system"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        html.contains("LLM runs"),
        "the System page must carry the LLM runs card: {html}"
    );
    for row in [
        llm_stat_row("Active", "2"),
        llm_stat_row("Queued", "1"),
        llm_stat_row("Capacity", "2"),
        llm_stat_row(
            "Capacity timeouts (since start)",
            &want_timeouts.to_string(),
        ),
    ] {
        assert!(
            html.contains(&row),
            "the LLM card must render the live gate value {row}: {html}"
        );
    }

    waiter.abort();
    shutdown.cancel();
}

/// Provenance (lifecycle spec §3.1): a chat-mode agent's concept write
/// is SYSTEM provenance — the session chat surface's store stamps
/// `source = "chat"` (never the model's or the author's value) and
/// `confirmed_at` on every put.
#[tokio::test]
async fn chat_agent_write_carries_provenance_label() {
    let _g = TEST_LOCK.lock().await;
    // Script: (1) the final answer, (2) a write_concept tool call — the
    // agent writes a concept through the chat surface's labeled store.
    let llm_url = mock_llm(vec![
        text_response("Written."),
        tool_call_response(
            "call-1",
            "write_concept",
            serde_json::json!({
                "path": "/chat-written.md",
                "body": "recorded by the chat agent zebra",
                "frontmatter": { "type": "Note", "title": "Chat Written" }
            }),
        ),
    ])
    .await;
    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();
    let (cookie, _csrf) = login_admin(&client, &base).await;

    let (_session_id, events) =
        submit_draft_collect(&client, &base, &cookie, &_csrf, "record this").await;
    assert_eq!(events[2]["type"], "done", "tool run, then the answer");
    assert_eq!(
        events[1]["name"], "write_concept",
        "the mocked turn exercised the write tool"
    );

    // Read the concept back through the user-scope REST API.
    // (The login cookie is for the same admin bundle the runner wrote.)
    let resp = client
        .get(format!("{base}/api/v1/concepts/chat-written.md"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the agent's write landed");
    let md = resp.text().await.unwrap();
    let concept = mycelium_core::concept::Concept::parse("/chat-written.md", &md)
        .expect("stored concept parses");
    assert_eq!(
        concept.frontmatter.source.as_deref(),
        Some("chat"),
        "the chat surface's label overwrites anything else"
    );
    assert!(
        concept
            .frontmatter
            .confirmed_at
            .as_ref()
            .map(|t| chrono::DateTime::parse_from_rfc3339(t).is_ok())
            .unwrap_or(false),
        "confirmed_at is stamped at the put boundary"
    );
    shutdown.cancel();
}

/// Provenance (lifecycle spec §3.1): the dream pass carries the
/// `dream` label on every concept its agent writes.
#[tokio::test]
async fn dream_agent_write_carries_provenance_label() {
    let _g = TEST_LOCK.lock().await;
    // Script: (1) the summarizing answer, (2) a write_concept call.
    // LIFO pop: the tool call answers the first request.
    let llm_url = mock_llm(vec![
        text_response("Dream pass complete."),
        tool_call_response(
            "call-1",
            "write_concept",
            serde_json::json!({
                "path": "/dream-written.md",
                "body": "written by the dream pass",
                "frontmatter": { "type": "Note", "title": "Dream Written" }
            }),
        ),
    ])
    .await;
    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // Seed the signal: a lone unlinked concept is a graph orphan, which
    // makes run_dream run the agent instead of reporting "no signals".
    let seed = client
        .put(format!("{base}/api/v1/concepts/orphan.md"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .json(&serde_json::json!(
            {"markdown": "---\ntype: Note\ntitle: Lonely\n---\n\nnothing links here"}
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(seed.status(), 201, "the orphan seeds the dream signal");

    let resp = client
        .post(format!("{base}/api/v1/dream"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the orphan gives the dream a signal");
    let report: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(report["ran"], true, "the dream ran the agent");

    // The concept the dream agent wrote carries the dream label.
    let got = client
        .get(format!("{base}/api/v1/concepts/dream-written.md"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200, "the dream's write landed");
    let concept =
        mycelium_core::concept::Concept::parse("/dream-written.md", &got.text().await.unwrap())
            .expect("stored concept parses");
    assert_eq!(
        concept.frontmatter.source.as_deref(),
        Some("dream"),
        "the surface's label (spec §3.1) overwrites anything else"
    );
    assert!(
        concept
            .frontmatter
            .confirmed_at
            .as_ref()
            .map(|t| chrono::DateTime::parse_from_rfc3339(t).is_ok())
            .unwrap_or(false),
        "confirmed_at is stamped at the put boundary"
    );
    shutdown.cancel();
}
