//! LLM api_key integration: the admin portal's sealed save (the config
//! column holds a hex envelope, never plaintext), no-render (GET /admin
//! never contains the key), blank-keep (a save with a blank api_key
//! preserves the stored key), and the end-to-end Bearer header (POST
//! /api/v1/chat sends the configured key to the LLM backend).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::response::IntoResponse;

use mycelium_auth::login::LoginService;
use mycelium_store::Store;
use mycelium_store::config::ConfigStore;
use mycelium_web::AppState;

/// Known admin password for the pre-seeded account.
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";

const TEST_KEY: &str = "test-llm-secret-123";

/// A capturing mock LLM: records the request's Authorization header,
/// then answers with a fixed assistant text.
async fn auth_mock() -> (String, Arc<tokio::sync::Mutex<Option<String>>>) {
    let captured: Arc<tokio::sync::Mutex<Option<String>>> = Arc::new(tokio::sync::Mutex::new(None));
    let handler_state = Arc::clone(&captured);
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |headers: axum::http::HeaderMap| {
            let captured = handler_state;
            async move {
                let auth = headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.to_string());
                *captured.lock().await = auth;
                axum::Json(serde_json::json!({
                    "choices": [{ "message": { "role": "assistant", "content": "ok" } }]
                }))
                .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://127.0.0.1:{}/v1", addr.port()), captured)
}

async fn boot() -> (
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

/// POST /admin/llm with the given url + optional api_key (admin cookie
/// + CSRF header, the page's own form flow).
///
/// Passing `None` omits the form field entirely; passing `Some("")`
/// sends a blank value. Both must keep the stored key.
async fn admin_save_llm(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
    url: &str,
    api_key: Option<&str>,
) -> reqwest::Response {
    let mut form = vec![("url", url.to_string()), ("model", "mycelium".to_string())];
    if let Some(k) = api_key {
        form.push(("api_key", k.to_string()));
    }
    client
        .post(format!("{base}/admin/llm"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .form(&form)
        .send()
        .await
        .unwrap()
}

/// Decrypt the stored "llm" config from the tempdir's on-disk service
/// key (load_or_create is idempotent: same key file, same key).
async fn stored_llm_config(dir: &std::path::Path) -> Option<mycelium_web::LlmConfig> {
    let store = Store::open(dir).await.unwrap();
    let service_key = mycelium_crypto::load_or_create_service_key_with(dir, None).unwrap();
    let config = ConfigStore::new(store.pool().clone());
    mycelium_store::config::get_sealed(&config, &service_key, "llm").await
}

#[tokio::test]
async fn admin_save_seals_key_never_renders_it_and_blank_keeps_it() {
    let (llm_url, _captured) = auth_mock().await;
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    // Absent at start: the status line says "not set".
    let page = cl
        .get(format!("{base}/admin"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert!(page.text().await.unwrap().contains("API key: not set"));

    // Save the config with a key.
    let resp = admin_save_llm(&cl, &base, &cookie, &csrf, &llm_url, Some(TEST_KEY)).await;
    assert_eq!(resp.status(), 303, "admin save redirects");

    // The stored row is the hex envelope: no plaintext key or url.
    let store = Store::open(dir.path()).await.unwrap();
    let (row,): (String,) = sqlx::query_as("SELECT value FROM config WHERE key = 'llm'")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert!(
        !row.contains(TEST_KEY),
        "api key leaked plaintext into the config column"
    );
    assert!(!row.contains("127.0.0.1"), "url leaked at rest: {row}");

    // GET /admin never contains the key; the status line reads "stored".
    let page = cl
        .get(format!("{base}/admin"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let html = page.text().await.unwrap();
    assert!(
        !html.contains(TEST_KEY),
        "the api key rendered back to the browser"
    );
    assert!(
        html.contains("API key: stored"),
        "status line missing: {html}"
    );

    // The stored config decrypts back with the key kept.
    let cfg = stored_llm_config(dir.path()).await.expect("config stored");
    assert_eq!(cfg.api_key.as_deref(), Some(TEST_KEY));
    assert_eq!(cfg.url, llm_url);

    // A blank save keeps the stored key.
    let resp = admin_save_llm(&cl, &base, &cookie, &csrf, &llm_url, Some("")).await;
    assert_eq!(resp.status(), 303);
    let cfg = stored_llm_config(dir.path()).await.expect("still stored");
    assert_eq!(
        cfg.api_key.as_deref(),
        Some(TEST_KEY),
        "blank api_key must keep the stored key"
    );
    // And a save with the field entirely absent (no key in the form at
    // all — e.g. a client that does not know about it) keeps it too.
    let resp = admin_save_llm(&cl, &base, &cookie, &csrf, &llm_url, None).await;
    assert_eq!(resp.status(), 303);
    let cfg = stored_llm_config(dir.path()).await.expect("still stored");
    assert_eq!(cfg.api_key.as_deref(), Some(TEST_KEY));

    // A whitespace-only value also counts as blank (trimmed) and keeps
    // the stored key.
    let resp = admin_save_llm(&cl, &base, &cookie, &csrf, &llm_url, Some(" \t\n")).await;
    assert_eq!(resp.status(), 303);
    let cfg = stored_llm_config(dir.path()).await.expect("still stored");
    assert_eq!(
        cfg.api_key.as_deref(),
        Some(TEST_KEY),
        "whitespace-only api_key must keep the stored key"
    );
}

#[tokio::test]
async fn chat_sends_bearer_header_end_to_end() {
    let (llm_url, captured) = auth_mock().await;
    let (base, _shutdown, _dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    // Save the key through the admin form — the same path a real admin
    // uses.
    let resp = admin_save_llm(&cl, &base, &cookie, &csrf, &llm_url, Some(TEST_KEY)).await;
    assert_eq!(resp.status(), 303);
    let expected_auth: Option<&str> = Some(&format!("Bearer {TEST_KEY}"));

    // The agent's LLM request carries the configured Bearer header to
    // the backend.
    let chat = cl
        .post(format!("{base}/api/v1/chat"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("message", "hello")])
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), 200, "chat endpoint");
    let body: serde_json::Value = chat.json().await.unwrap();
    assert_eq!(body["reply"], "ok");
    assert_eq!(
        captured.lock().await.as_deref(),
        expected_auth,
        "chat must send Authorization: Bearer <configured key>"
    );
}
