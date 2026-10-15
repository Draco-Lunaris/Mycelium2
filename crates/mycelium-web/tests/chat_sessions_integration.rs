//! Chat session history integration: the session-scoped chat endpoints
//! (draft-create + submit SSE streams, reattach, session list/meta,
//! delete) with auth, CSRF, and per-user ownership enforced (spec
//! §3.5/§6). Uses mock OpenAI-compatible LLMs — scripted, gated (a
//! watch channel holds the first request so a turn can be observed
//! mid-run), and request-capturing (multi-turn payload pins) — pointed
//! at via the ConfigStore "llm" key.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::response::IntoResponse;

use mycelium_auth::login::LoginService;
use mycelium_store::Store;
use mycelium_web::AppState;

/// Known admin password for the pre-seeded account (tests log in with it).
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";

/// Password for the second (non-admin) user minted by the ownership test.
const SECOND_PASSWORD: &str = "second user password known to the test suite!";

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
    state
        .config
        .set(
            "llm",
            &mycelium_web::LlmConfig {
                url: llm_url,
                model: "mock".into(),
                api_key: None,
            },
        )
        .await
        .unwrap();

    let https: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let http: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();

    let listener = tokio::net::TcpListener::bind(https).await.unwrap();
    let https_port = listener.local_addr().unwrap().port();
    drop(listener);

    let data_dir = dir.path().to_path_buf();
    let token = shutdown.clone();
    tokio::spawn(async move {
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
    (format!("https://127.0.0.1:{https_port}"), shutdown, dir)
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
    // The newest session is the ACTIVE one.
    assert!(
        html.contains(r#"class="chat-session-link active""#),
        "no active link: {html}"
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
    // An empty script exhausts on the first LLM request → the detached
    // runner persists the error row + finalizes `failed`.
    let llm_url = mock_llm(vec![]).await;
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

/// Task 7: the chat page's JS rides the session endpoints — the version
/// bump invalidates the served-asset caches, the body carries the
/// create/submit session URLs and the composer island's entry point,
/// and the REMOVED legacy stream route is gone from the asset entirely
/// (the Task-4 carry-forward: the pre-rewrite JS still POSTed a route
/// that no longer exists — the runtime submit path was broken until
/// this rewrite).
#[tokio::test]
async fn assets_version_bumped_and_chat_js_references_new_endpoints() {
    let llm_url = "http://127.0.0.1:1/v1".to_string(); // dead; no turn runs
    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();

    // The versioned URL the page emits: 200, served as JavaScript.
    let resp = client
        .get(format!("{base}/assets/chat.js?v=28"))
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
