//! Chat streaming integration test: the SSE endpoint streams the
//! agent's tool progress and the final reply, with auth + CSRF
//! enforced. Uses a mock OpenAI-compatible LLM (scripted tool-call
//! then answer) pointed at via the ConfigStore "llm" key.

use std::net::SocketAddr;

use axum::response::IntoResponse;

use mycelium_auth::login::LoginService;
use mycelium_store::Store;
use mycelium_web::AppState;

/// Known admin password for the pre-seeded account (tests log in with it).
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";

/// A scripted mock LLM: pops responses LIFO, captures requests.
async fn mock_llm(responses: Vec<serde_json::Value>) -> String {
    let state = std::sync::Arc::new(tokio::sync::Mutex::new(responses));
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
    let state = AppState::new(store, service_key, login, assets_dir);
    // Point the librarian at the mock LLM.
    state
        .config
        .set(
            "llm",
            &mycelium_web::LlmConfig {
                url: llm_url,
                model: "mock".into(),
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
            token,
        )
        .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
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
#[tokio::test]
async fn chat_stream_emits_tool_progress_then_reply() {
    // Script: (1) final answer, (2) a search tool call.
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

    // Stream a chat message.
    let resp = client
        .post(format!("{base}/api/v1/chat/stream"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .header("accept", "text/event-stream")
        .form(&[("message", "hello librarian")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "stream endpoint");
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let body = resp.text().await.unwrap();

    // The SSE stream contains the tool event then the done event.
    assert!(
        body.contains("\"type\":\"tool\""),
        "tool progress event missing: {body}"
    );
    assert!(
        body.contains("search_knowledge"),
        "tool name missing: {body}"
    );
    assert!(
        body.contains("\"type\":\"done\""),
        "done event missing: {body}"
    );
    assert!(
        body.contains("Here is the answer you asked for."),
        "final reply missing: {body}"
    );
    // Event ordering: tool before done.
    let tool_idx = body.find("\"type\":\"tool\"").unwrap();
    let done_idx = body.find("\"type\":\"done\"").unwrap();
    assert!(tool_idx < done_idx, "tool event must precede done");

    shutdown.cancel();
}

#[tokio::test]
async fn chat_stream_requires_csrf_and_auth() {
    let llm_url = mock_llm(vec![]).await;
    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // No session → the login gate redirects (303) to /login.
    let no_auth = client
        .post(format!("{base}/api/v1/chat/stream"))
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
        .post(format!("{base}/api/v1/chat/stream"))
        .header("cookie", &cookie)
        .form(&[("message", "hi")])
        .send()
        .await
        .unwrap();
    assert_eq!(no_csrf.status(), 403, "missing CSRF must 403");

    // Empty message → 400.
    let empty = client
        .post(format!("{base}/api/v1/chat/stream"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("message", "  ")])
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), 400, "empty message must 400");

    shutdown.cancel();
}
#[tokio::test]
async fn chat_history_persists_across_messages() {
    // Two sequential messages: the second request must include the
    // first exchange in its conversation (multi-turn memory). The
    // mock captures request bodies; assert the history is present.
    use std::sync::Arc;
    type Shared = Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>;
    let requests: Shared = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let req_clone = Arc::clone(&requests);
    let state = Arc::new(tokio::sync::Mutex::new(vec![
        text_response("Second reply."),
        text_response("First reply."),
    ]));
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |body: String| {
            let state = Arc::clone(&state);
            let requests = Arc::clone(&req_clone);
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
    let llm_url = format!("http://{addr}/v1");

    let (base, shutdown, _dir) = boot(llm_url).await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // First message.
    let resp = client
        .post(format!("{base}/api/v1/chat/stream"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("message", "first question")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await.unwrap();

    // Second message — same session.
    let resp = client
        .post(format!("{base}/api/v1/chat/stream"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("message", "second question")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await.unwrap();

    // The second LLM request must contain the prior exchange:
    // [system, user("first question"), assistant("First reply."),
    //  user("second question")]
    let captured = requests.lock().await;
    assert_eq!(captured.len(), 2, "two LLM calls");
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
