//! LLM test-connection integration: POST /admin/llm/test probes the
//! backend with one minimal completion using the submitted form values
//! (blank api_key = the stored key) and redirects back with a verdict
//! flash. Success → `?ok=connected…`; backend errors → `?error=…`
//! carrying the backend's status and body so a mis-pasted key or wrong
//! URL is visible in seconds.
//!
//! The file also hosts the background backend-probe loop tests (outage
//! recovery spec §3.1: `spawn_backend_probe` keeps the process-global
//! `LLM_HEALTH` registry fresh). `LLM_HEALTH` is process-global, so
//! every test in this binary serializes on [`TEST_LOCK`], and every
//! test restores the registry through [`HealthDefaults`] (chat_sessions
//! _integration.rs's pattern — integration binaries cannot link the
//! crate's `#[cfg(test)]` guard).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::response::IntoResponse;

use mycelium_auth::login::LoginService;
use mycelium_librarian::llm_health::{BackendState, LLM_HEALTH};
use mycelium_store::Store;
use mycelium_web::AppState;

/// Serializes all tests in this binary around the shared health
/// registry (the test-connection tests drive the passive `LlmClient`
/// hooks, which note into `LLM_HEALTH` from `chat_system`).
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Restores the process-global health registry to a fresh Green on drop,
/// so a probe loop still running (or a leaked Red from a failed
/// assertion) cannot perturb a sibling test. Declared AFTER the
/// `TEST_LOCK` guard so the restore happens while the lock is held.
struct HealthDefaults;

impl Drop for HealthDefaults {
    fn drop(&mut self) {
        LLM_HEALTH.reset_for_test();
    }
}

/// Poll a health-state condition with a deadline (never a bare sleep —
/// the poll target is observable registry state).
async fn poll_health_until(state: BackendState, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if LLM_HEALTH.state() == state {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "never saw {what}: {:?}",
            LLM_HEALTH.snapshot()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Known admin password for the pre-seeded account.
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";

const TEST_KEY: &str = "test-llm-secret-123";

/// A configurable mock LLM: every POST to /v1/chat/completions is
/// answered with `status` + fixed JSON body; the Authorization header
/// of the last request is captured.
async fn status_mock(
    status: u16,
    body: &'static str,
) -> (String, Arc<tokio::sync::Mutex<Option<String>>>) {
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
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    body,
                )
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
    let state = AppState::new(
        store,
        service_key,
        login,
        assets_dir,
        dir.path().to_path_buf(),
        None,
    );

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

/// POST /admin/llm/test with the given form values (admin cookie +
/// CSRF header, the page's own form flow).
async fn admin_test_llm(
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
        .post(format!("{base}/admin/llm/test"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .form(&form)
        .send()
        .await
        .unwrap()
}

/// The Location of a 303 redirect, percent-decoded for assertions.
async fn location_of(resp: reqwest::Response) -> String {
    assert_eq!(resp.status(), 303, "test-connection must redirect back");
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("location header")
        .to_string();
    percent_decode(&loc)
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

#[tokio::test]
async fn test_connection_success_reports_connected() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let (llm_url, captured) = status_mock(
        200,
        r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
    )
    .await;
    let (base, _shutdown, _dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    let resp = admin_test_llm(&cl, &base, &cookie, &csrf, &llm_url, Some(TEST_KEY)).await;
    let loc = location_of(resp).await;
    assert!(
        loc.contains("ok=connected"),
        "success verdict must flash ok=connected: {loc}"
    );
    assert_eq!(
        captured.lock().await.as_deref(),
        Some("Bearer test-llm-secret-123"),
        "the probe must send the configured key"
    );
}

#[tokio::test]
async fn test_connection_failure_carries_backend_status_and_body() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let (llm_url, _captured) =
        status_mock(401, r#"{"error":{"message":"API key is missing."}}"#).await;
    let (base, _shutdown, _dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    let resp = admin_test_llm(&cl, &base, &cookie, &csrf, &llm_url, Some("wrong-key")).await;
    let loc = location_of(resp).await;
    assert!(
        loc.contains("error="),
        "failure verdict must flash an error: {loc}"
    );
    assert!(
        loc.contains("401"),
        "the backend status must surface: {loc}"
    );
    assert!(
        loc.contains("API key is missing"),
        "the backend body must surface: {loc}"
    );
}

#[tokio::test]
async fn test_connection_blank_key_probes_the_stored_config() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    // Seed a stored config with a key, then test with a blank key: the
    // probe must reach the backend with the STORED key, not nothing.
    let (llm_url, captured) = status_mock(
        200,
        r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
    )
    .await;
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    // Save a config with a key first (the page's own save flow).
    let mut form = vec![("url", llm_url.clone()), ("model", "mycelium".to_string())];
    form.push(("api_key", TEST_KEY.to_string()));
    let save = cl
        .post(format!("{base}/admin/llm"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&form)
        .send()
        .await
        .unwrap();
    assert_eq!(save.status(), 303);
    let _ = dir; // keep tempdir alive for the test's duration

    // Blank key field: the probe uses the stored key.
    let resp = admin_test_llm(&cl, &base, &cookie, &csrf, &llm_url, Some("")).await;
    let loc = location_of(resp).await;
    assert!(
        loc.contains("ok=connected"),
        "blank-key test must probe the stored config: {loc}"
    );
    assert_eq!(
        captured.lock().await.as_deref(),
        Some("Bearer test-llm-secret-123"),
        "the probe must use the STORED key when the form field is blank"
    );
}

#[tokio::test]
async fn test_connection_unreachable_url_reports_connect_failure() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    // Port 1 on loopback: connection refused territory.
    let (base, _shutdown, _dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    let resp = admin_test_llm(&cl, &base, &cookie, &csrf, "http://127.0.0.1:1/v1", None).await;
    let loc = location_of(resp).await;
    assert!(
        loc.contains("error="),
        "unreachable backend must flash an error: {loc}"
    );
}

#[tokio::test]
async fn test_connection_requires_admin() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let (base, _shutdown, _dir) = boot().await;
    let cl = client();

    // Create a non-admin user and log in.
    // (No admin create endpoint for users in this suite; use the store
    // directly through a second boot's tempdir — simplest: expect the
    // unauthenticated case: no session → 303 login redirect.)
    let resp = cl
        .post(format!("{base}/admin/llm/test"))
        .form(&[("url", ""), ("model", "")])
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        303,
        "unauthenticated POST must redirect to login"
    );
}

// ---- background backend probe task (outage recovery spec §3.1) ----
//
// `LLM_HEALTH` is process-global and the probe loop feeds it from a
// spawned task, so every test here holds `TEST_LOCK` for its whole body
// and stops its probe (token cancel) before releasing the lock. The
// probe feeds no other process-global, so nothing else needs guarding.

/// A raw `/models` mock (the probe's own endpoint — NOT the chat-
/// completions mock the passive hooks ride, so a probe tick never notes
/// through `LlmClient`). The health verdict is hot-swappable through the
/// shared cell so one listener can change from reachable to 503 without
/// a port change.
async fn models_mock_live(status: Arc<std::sync::atomic::AtomicU16>) -> String {
    let app = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(move || {
            let status = Arc::clone(&status);
            async move {
                (
                    axum::http::StatusCode::from_u16(
                        status.load(std::sync::atomic::Ordering::SeqCst),
                    )
                    .unwrap(),
                    "probe body",
                )
                    .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{}/v1", addr.port())
}

/// A bare `AppState` over a fresh store (no HTTP listener): the probe
/// task needs the config store + service key, nothing else.
async fn state_only() -> (AppState, tempfile::TempDir) {
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
    let login = LoginService::new(store.pool().clone());
    let state = AppState::new(
        store,
        service_key,
        login,
        dir.path().join("assets"),
        dir.path().to_path_buf(),
        None,
    );
    (state, dir)
}

/// Sealed-`llm`-row writer (the admin page's own storage shape).
async fn write_llm_row(state: &AppState, url: String) {
    let sealed = mycelium_web::state::seal_config(
        &state.service_key,
        "llm",
        &mycelium_web::LlmConfig {
            url,
            model: "probe".into(),
            ..Default::default()
        },
    );
    state.config.set("llm", &sealed).await.unwrap();
}

/// The probe's first tick fires immediately: a reachable mock → Green
/// within one short interval; flipping the mock to 503 → three interval
/// ticks (1/failure) take it through Yellow to Red; deleting the config
/// row → Disabled within two ticks.
#[tokio::test]
async fn probe_task_reaches_green_then_red_then_disabled() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    // Pin Disabled first (the production boot seed): the reset state is
    // trivially Green, so without the pin the first Green assertion
    // could not tell a fed machine from an un-started task. Reaching
    // Green from here requires note_configured (lift) AND note_ok (the
    // probe's ok feed) — exactly what the task's first tick must do.
    LLM_HEALTH.reset_for_test();
    LLM_HEALTH.set_disabled();

    let status = Arc::new(std::sync::atomic::AtomicU16::new(200));
    let url = models_mock_live(Arc::clone(&status)).await;
    let (state, _dir) = state_only().await;
    write_llm_row(&state, url).await;

    let shutdown = tokio_util::sync::CancellationToken::new();
    let probe = mycelium_web::server::spawn_backend_probe(
        state.clone(),
        shutdown.clone(),
        Duration::from_millis(100),
    )
    .await;

    // Reachable → Green (first tick fires at once).
    poll_health_until(BackendState::Green, "Green against a reachable mock").await;

    // Backend returns 503: unhealthy → Yellow after 1 failure, Red at
    // the 3-failure threshold. A 503 answers instantly, so the poll's
    // 10 s bound is generous headroom over 3×100 ms intervals.
    status.store(503, std::sync::atomic::Ordering::SeqCst);
    poll_health_until(BackendState::Red, "Red after 3 failed probes").await;

    // Backend recovers → the very next ok note clears everything.
    status.store(200, std::sync::atomic::Ordering::SeqCst);
    poll_health_until(BackendState::Green, "Green after the backend recovers").await;

    // Config row deleted → Disabled within two ticks.
    state.config.delete("llm").await.unwrap();
    poll_health_until(BackendState::Disabled, "Disabled after the row is deleted").await;

    // Stop the task and WAIT for it: `select!` may pick a due tick over
    // the cancellation, so a late `note_*` could otherwise land after
    // this test releases TEST_LOCK and perturb a sibling's window. The
    // unwrap keeps a panicking probe visible as a test failure.
    shutdown.cancel();
    probe.await.unwrap();
}

/// The probe re-reads the CURRENT config row every tick, so an admin's
/// config swap applies within one interval: Red against a dropped
/// listener, then the row repointed at a live mock heals it to Green
/// without touching the running task.
#[tokio::test]
async fn probe_task_honours_a_config_row_swap_within_a_tick() {
    let _lock = TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    LLM_HEALTH.reset_for_test();

    // A port that refuses connections (bound, then dropped).
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);
    let (state, _dir) = state_only().await;
    write_llm_row(&state, format!("http://{dead_addr}/v1")).await;

    let shutdown = tokio_util::sync::CancellationToken::new();
    let probe = mycelium_web::server::spawn_backend_probe(
        state.clone(),
        shutdown.clone(),
        Duration::from_millis(100),
    )
    .await;

    poll_health_until(BackendState::Red, "Red against a refused port").await;

    // Repoint the row at a live mock (the config swap). The probe reads
    // the row each tick, so it must heal within the next interval — no
    // task restart, no reboot.
    let status = Arc::new(std::sync::atomic::AtomicU16::new(200));
    let live = models_mock_live(status).await;
    write_llm_row(&state, live).await;
    poll_health_until(
        BackendState::Green,
        "Green after the row points at a live mock",
    )
    .await;

    // Join the probe before releasing the lock (see the other test);
    // unwrap keeps a probe panic a test failure.
    shutdown.cancel();
    probe.await.unwrap();
}
