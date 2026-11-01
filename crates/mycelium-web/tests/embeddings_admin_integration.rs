//! Embeddings admin-surface integration: the `embeddings` config row
//! (POST /admin/embeddings), the test-connection flow
//! (POST /admin/embeddings/test), and the page rendering
//! (GET /admin/embeddings). Mirrors llm_test_connection_integration.rs.
//!
//! The embedding client's passive hooks feed the process-global
//! `EMBEDDING_HEALTH` machine from the test handler too, so every test
//! in this binary serializes on [`TEST_LOCK`] and restores the machine
//! through [`EmbDefaults`] (the chat-LLM file's pattern — integration
//! binaries cannot link the crate's `#[cfg(test)]` guard).
//!
//! The memory-lifecycle tests at the bottom add the probe task
//! ([`mycelium_web::server::spawn_embedding_probe`]) and the /health +
//! /metrics embedding fields; the same [`TEST_LOCK`] discipline covers
//! them, and every spawned tick-loop task is cancelled and JOINED
//! before the lock is released.

use std::net::SocketAddr;
use std::ops::AsyncFnMut;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use axum::response::IntoResponse;

use mycelium_auth::login::LoginService;
use mycelium_librarian::llm_health::{
    BackendState, EMB_LAST_SUCCESS, EMBEDDING_DIMS, EMBEDDING_HEALTH,
};
use mycelium_store::Store;
use mycelium_web::AppState;

/// Serializes all tests in this binary around the shared health
/// machine (the test-connection tests drive the passive embedding
/// hooks, which note into `EMBEDDING_HEALTH`).
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Restores the process-global embeddings health machine to a fresh
/// Green on drop, so a failed assertion's Red (or any other leftover)
/// cannot perturb a sibling test. Declared AFTER the `TEST_LOCK` guard
/// so the restore happens while the lock is held.
struct EmbDefaults;

impl Drop for EmbDefaults {
    fn drop(&mut self) {
        EMBEDDING_HEALTH.reset_for_test();
    }
}

/// Fresh "never observed" sentinels: zero `EMB_LAST_SUCCESS` +
/// `EMBEDDING_DIMS` before asserting an unknown-dims page state (they
/// are atomics, not part of `reset_for_test`'s machine reset).
fn reset_emb_sentinels() {
    EMB_LAST_SUCCESS.store(0, Ordering::Relaxed);
    EMBEDDING_DIMS.store(0, Ordering::Relaxed);
}

/// Known admin password for the pre-seeded account.
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";

const MALLORY_PASSWORD: &str = "mallory password 20 chars";

const STORED_KEY: &str = "stored-emb-key-123";

/// A one-input, 3-dimension embeddings response.
const EMBED_OK_BODY: &str = r#"{"data":[{"index":0,"embedding":[1.0,2.0,3.0]}]}"#;

/// A 503 body that must never surface in the handler's error flash
/// (bounded, non-leaking reasons — only the status rides the redirect).
const LEAK_STR: &str = "upstream-secret-body-must-not-leak-into-the-flash";

/// An embeddings mock: every POST to /v1/embeddings is answered with
/// `status` + fixed JSON body; the Authorization header of the last
/// request is captured.
async fn embeddings_mock(
    status: u16,
    body: &'static str,
) -> (String, Arc<tokio::sync::Mutex<Option<String>>>) {
    let captured: Arc<tokio::sync::Mutex<Option<String>>> = Arc::new(tokio::sync::Mutex::new(None));
    let handler_state = Arc::clone(&captured);
    let app = axum::Router::new().route(
        "/v1/embeddings",
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
    // The embeddings row's shape: server root WITHOUT /v1 — the client
    // POSTs {base_url}/v1/embeddings.
    (format!("http://127.0.0.1:{}", addr.port()), captured)
}

async fn boot() -> (
    String,
    tokio_util::sync::CancellationToken,
    tempfile::TempDir,
    AppState,
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
    users
        .create_local(
            "mallory",
            "mallory@localhost.local",
            MALLORY_PASSWORD,
            mycelium_auth::Role::User,
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
    let serve_state = state.clone();
    tokio::spawn(async move {
        let _ = mycelium_web::serve(
            serve_state,
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
    (
        format!("https://127.0.0.1:{https_port}"),
        shutdown,
        dir,
        state,
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

/// Log in as user `name`; returns (cookie, csrf).
async fn login_as(
    client: &reqwest::Client,
    base: &str,
    name: &str,
    password: &str,
) -> (String, String) {
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", name), ("password", password)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
    let cookie = cookie_from(&login);
    let csrf = csrf_from_page(client, &format!("{base}/"), &cookie).await;
    (cookie, csrf)
}

async fn login_admin(client: &reqwest::Client, base: &str) -> (String, String) {
    login_as(client, base, "admin", ADMIN_PASSWORD).await
}

/// The Location of a 303 redirect, percent-decoded for assertions.
async fn location_of(resp: reqwest::Response) -> String {
    assert_eq!(resp.status(), 303, "admin POSTs must redirect back");
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

/// The saved `embeddings` row as the admin surfaces would read it.
async fn saved_config(state: &AppState) -> mycelium_librarian::embeddings::EmbeddingsConfig {
    mycelium_web::state::decrypt_config::<mycelium_librarian::embeddings::EmbeddingsConfig>(
        &state.config,
        &state.service_key,
        "embeddings",
    )
    .await
    .expect("the embeddings row was sealed and stored")
}

/// POST /admin/embeddings with a base_url/model/api_key form (the
/// page's own flow; timeout_secs/batch omitted = keep the stored
/// values, which on a fresh store are the defaults).
async fn admin_save_embeddings(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
) -> reqwest::Response {
    let mut form = vec![
        ("base_url", base_url.to_string()),
        ("model", model.to_string()),
    ];
    if let Some(k) = api_key {
        form.push(("api_key", k.to_string()));
    }
    client
        .post(format!("{base}/admin/embeddings"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .form(&form)
        .send()
        .await
        .unwrap()
}

/// POST /admin/embeddings/test with base_url/model/api_key.
async fn admin_test_embeddings(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
    base_url: &str,
    api_key: Option<&str>,
) -> reqwest::Response {
    let mut form = vec![
        ("base_url", base_url.to_string()),
        ("model", "mymodel".to_string()),
    ];
    if let Some(k) = api_key {
        form.push(("api_key", k.to_string()));
    }
    client
        .post(format!("{base}/admin/embeddings/test"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .form(&form)
        .send()
        .await
        .unwrap()
}

/// The page's form, with the tunables as submitted strings. Used by the
/// clamp test (out-of-range values) via POST /admin/embeddings.
async fn save_with_tunables(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
    form: &[(&str, &str)],
) -> reqwest::Response {
    client
        .post(format!("{base}/admin/embeddings"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .form(form)
        .send()
        .await
        .unwrap()
}

/// The save handler mirrors the `llm` row: the row at rest is always in
/// range (clamped BEFORE sealing), keyless first saves hold no key, and
/// the page renders the stored values back into the boxes.
#[tokio::test]
async fn save_then_read_back_roundtrips_the_clamped_config() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    let (base, _shutdown, _dir, state) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    // Out-of-range low timeout + out-of-range high batch: both clamp.
    let save = save_with_tunables(
        &cl,
        &base,
        &cookie,
        &csrf,
        &[
            ("base_url", "http://10.0.0.9:9999"),
            ("model", "nomic-embed-text"),
            ("timeout_secs", "4"),
            ("batch", "1000"),
        ],
    )
    .await;
    assert_eq!(save.status(), 303);
    let loc = location_of(save).await;
    assert!(loc.contains("ok="), "a save must flash ok: {loc}");
    let cfg = saved_config(&state).await;
    assert_eq!(cfg.base_url, "http://10.0.0.9:9999");
    assert_eq!(cfg.model, "nomic-embed-text");
    assert_eq!(cfg.timeout_secs, 5, "below-range clamps up on save");
    assert_eq!(cfg.batch, 128, "above-range clamps down on save");
    assert_eq!(
        cfg.api_key, None,
        "no submitted key + no stored row = no key"
    );

    // The page renders the stored values back into the boxes.
    let page = cl
        .get(format!("{base}/admin/embeddings"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains(r#"value="http://10.0.0.9:9999""#), "{html}");
    assert!(html.contains(r#"value="nomic-embed-text""#), "{html}");
    assert!(html.contains(r#"value="5""#), "{html}");
    assert!(html.contains(r#"value="128""#), "{html}");
}

/// Blank api_key (and one omitted entirely) keeps the currently stored
/// key — the key is never rendered back, so keep-on-blank is the only
/// way an edit re-saves its key.
#[tokio::test]
async fn blank_api_key_keeps_the_stored_key() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    // A stored key is REQUIRED: reset first (a sibling's success
    // stamped the machine's sentinels, but the row state is local).
    EMBEDDING_HEALTH.reset_for_test();
    reset_emb_sentinels();
    let (base, _shutdown, _dir, state) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    admin_save_embeddings(
        &cl,
        &base,
        &cookie,
        &csrf,
        "http://10.0.0.9:1",
        "first-model",
        Some(STORED_KEY),
    )
    .await;
    let cfg = saved_config(&state).await;
    assert_eq!(cfg.api_key.as_deref(), Some(STORED_KEY));

    // Re-save with the key field blank: the stored key survives.
    admin_save_embeddings(
        &cl,
        &base,
        &cookie,
        &csrf,
        "http://10.0.0.9:2",
        "second-model",
        Some(""),
    )
    .await;
    let cfg = saved_config(&state).await;
    assert_eq!(
        cfg.api_key.as_deref(),
        Some(STORED_KEY),
        "a blank api_key keeps the stored key"
    );
    assert_eq!(cfg.base_url, "http://10.0.0.9:2", "the edit took");
    assert_eq!(cfg.model, "second-model", "the edit took");

    // Re-save with the key field omitted entirely: same keep semantics.
    admin_save_embeddings(
        &cl,
        &base,
        &cookie,
        &csrf,
        "http://10.0.0.9:3",
        "third-model",
        None,
    )
    .await;
    let cfg = saved_config(&state).await;
    assert_eq!(
        cfg.api_key.as_deref(),
        Some(STORED_KEY),
        "an omitted api_key keeps the stored key"
    );
}

/// After a save, Test connection probes the CURRENT STORED row — even
/// when the submitted form points elsewhere (the post-save flow: save
/// first, then test; the page's hint says so).
#[tokio::test]
async fn test_connection_uses_the_stored_row_once_saved() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    reset_emb_sentinels();
    let (emb_url, captured) = embeddings_mock(200, EMBED_OK_BODY).await;
    let (base, _shutdown, _dir, _state) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    let save = admin_save_embeddings(
        &cl,
        &base,
        &cookie,
        &csrf,
        &emb_url,
        "mymodel",
        Some(STORED_KEY),
    )
    .await;
    assert_eq!(save.status(), 303);

    // The form points at a dead port with a junk key: the STORED row
    // must win (both values ignored).
    let resp = admin_test_embeddings(
        &cl,
        &base,
        &cookie,
        &csrf,
        "http://127.0.0.1:1",
        Some("ignored"),
    )
    .await;
    let loc = location_of(resp).await;
    assert!(
        loc.contains("ok=connected"),
        "the saved row was probed, not the form: {loc}"
    );
    assert!(
        !loc.contains("1:1"),
        "the junk form URL never reaches the probe: {loc}"
    );
    let want = format!("Bearer {STORED_KEY}");
    assert_eq!(
        captured.lock().await.as_deref(),
        Some(want.as_str()),
        "the probe must use the STORED key (the form's junk key is ignored)"
    );
}

/// First run (no stored row): the submitted form values drive the
/// probe, and the ok verdict carries the served dimensions.
#[tokio::test]
async fn test_connection_without_a_row_uses_the_submitted_form() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    reset_emb_sentinels();
    let (emb_url, captured) = embeddings_mock(200, EMBED_OK_BODY).await;
    let (base, _shutdown, _dir, _state) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    let resp =
        admin_test_embeddings(&cl, &base, &cookie, &csrf, &emb_url, Some("first-run-key")).await;
    let loc = location_of(resp).await;
    assert!(loc.contains("ok=connected"), "first-run probe ok: {loc}");
    assert!(
        loc.contains('3'),
        "the ok verdict carries the served dimensions: {loc}"
    );
    assert_eq!(
        captured.lock().await.as_deref(),
        Some("Bearer first-run-key"),
        "the first-run probe must carry the submitted key"
    );
}

/// No stored row and a bare form (no URL/model): the ruled error
/// redirect — the probe never goes out with an empty endpoint.
#[tokio::test]
async fn test_connection_without_a_row_or_a_valid_form_redirects_an_error() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    let (base, _shutdown, _dir, _state) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    let resp = admin_test_embeddings(&cl, &base, &cookie, &csrf, "", None).await;
    let loc = location_of(resp).await;
    assert!(
        loc.contains("error="),
        "nothing to test must flash an error: {loc}"
    );
}

/// A backend failure reports a BOUNDED, non-leaking error: the status
/// code surfaces, the upstream body does not.
#[tokio::test]
async fn test_connection_failure_reports_a_bounded_backend_error() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    reset_emb_sentinels();
    let body: &'static str = Box::leak(LEAK_STR.to_string().into_boxed_str());
    let (emb_url, _captured) = embeddings_mock(503, body).await;
    let (base, _shutdown, _dir, _state) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    let resp = admin_test_embeddings(&cl, &base, &cookie, &csrf, &emb_url, None).await;
    let loc = location_of(resp).await;
    assert!(
        loc.contains("error="),
        "a failing backend must flash an error: {loc}"
    );
    assert!(loc.contains("503"), "the status must surface: {loc}");
    assert!(
        !loc.contains("upstream-secret"),
        "the upstream body must not leak: {loc}"
    );
    assert!(
        loc.len() < 300,
        "the flash reason is bounded, got {} chars: {loc}",
        loc.len()
    );
    // A passive failure note fired for the 503 (the hooks ride every
    // embed_batch call); a following page render may show it — nothing
    // to assert on the flash beyond the verdict.
}

/// The page: the probe dot + state word + dims readout ("unknown"
/// before any success) + checked/down spans, and the form with both
/// submit buttons.
#[tokio::test]
async fn page_renders_probe_dot_dims_and_the_form() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    // Fresh machine + unknown sentinels: the page's default posture.
    EMBEDDING_HEALTH.reset_for_test();
    reset_emb_sentinels();
    let (base, _shutdown, _dir, _state) = boot().await;
    let cl = client();
    let (cookie, _csrf) = login_admin(&cl, &base).await;

    let page = cl
        .get(format!("{base}/admin/embeddings"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains(r#"action="/admin/embeddings""#), "{html}");
    assert!(html.contains(r#"name="base_url""#), "{html}");
    assert!(html.contains(r#"name="model""#), "{html}");
    assert!(html.contains(r#"name="timeout_secs""#), "{html}");
    assert!(html.contains(r#"name="batch""#), "{html}");
    assert!(
        html.contains(r#"type="password""#),
        "the api_key field: {html}"
    );
    assert!(html.contains(r#"autocomplete="off""#), "{html}");
    assert!(
        html.contains(r#"formaction="/admin/embeddings/test""#),
        "the page's second button tests the connection: {html}"
    );
    assert!(
        html.contains(r#"health-dot health-dot--green"#),
        "the probe dot renders the state class: {html}"
    );
    assert!(
        html.contains("Embeddings backend green"),
        "the state word beside the dot: {html}"
    );
    assert!(
        html.contains("unknown (no successful probe yet)"),
        "dims readout for a zero DIMS: {html}"
    );
    assert!(html.contains("never checked"), "no observation yet: {html}");
    assert!(
        !html.contains("down for"),
        "no outage window on a fresh machine: {html}"
    );
    // The key is never rendered back.
    assert!(
        !html.contains(STORED_KEY),
        "no stored key inside the page html: {html}"
    );
}

/// Three backend-down test-connection probes feed the passive failure
/// hooks exactly once each; the machine's Red then renders on the admin
/// page (dot class, state word, "down for" window). This drives the
/// hooks through the real surface, not a direct `note_failure` call.
#[tokio::test]
async fn page_reflects_a_failed_probe_state() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    reset_emb_sentinels();
    let (emb_url, _captured) = embeddings_mock(503, "backend down").await;
    let (base, _shutdown, _dir, _state) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    // Three failed probes: Yellow (1) → Yellow (2) → Red (3).
    for _ in 0..3 {
        let resp = admin_test_embeddings(&cl, &base, &cookie, &csrf, &emb_url, None).await;
        let loc = location_of(resp).await;
        assert!(loc.contains("error="), "each failing probe flashes: {loc}");
    }

    let page = cl
        .get(format!("{base}/admin/embeddings"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(
        html.contains(r#"health-dot health-dot--red"#),
        "three failures are Red on the page: {html}"
    );
    assert!(html.contains("Embeddings backend red"), "{html}");
    assert!(
        html.contains("down for"),
        "a live outage shows its window: {html}"
    );
}

/// The admin surfaces sit behind RequireAdmin: an unauthenticated hit
/// redirects to /login (the gate runs before CSRF), a logged-in
/// non-admin is 403.
#[tokio::test]
async fn embeddings_admin_surfaces_require_admin() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    let (base, _shutdown, _dir, _state) = boot().await;
    let cl = client();

    for path in ["/admin/embeddings", "/admin/embeddings/test"] {
        let resp = cl
            .post(format!("{base}{path}"))
            .form(&[("base_url", ""), ("model", "")])
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            303,
            "unauthenticated POST {path} must redirect to login"
        );
    }
    let get = cl
        .get(format!("{base}/admin/embeddings"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        get.status(),
        303,
        "unauthenticated GET must redirect to login"
    );

    // Logged-in non-admin: every admin surface is 403.
    let (cookie, csrf) = login_as(&cl, &base, "mallory", MALLORY_PASSWORD).await;
    let get = cl
        .get(format!("{base}/admin/embeddings"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(get.status(), 403, "a non-admin GET must be refused");
    let post = cl
        .post(format!("{base}/admin/embeddings/test"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("base_url", "http://127.0.0.1:1"), ("model", "m")])
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 403, "a non-admin probe must be refused");
}

// ---- probe task + observability (memory-lifecycle spec §3.3) ----
//
// `spawn_embedding_probe` mirrors `spawn_backend_probe` above: the
// machine is process-global and the task feeds it from a spawned tick
// loop, so each test here holds TEST_LOCK for its whole body and joins
// its task before releasing the lock (token cancel + await unwrap);
// EmbDefaults re-zeroes the machine on drop (the ok probes stamp
// EMB_LAST_SUCCESS + EMBEDDING_DIMS, which reset_for_test does not).

/// A live-swappable probe mock: POST /v1/embeddings answers with the
/// CURRENT atomic status and a per-request rebuilt body of `dims`
/// coordinates, so a successful RE-stamp is observable as a dims change
/// (a restamp-always pin cannot ride a fixed body, where a stamp-once
/// implementation would look identical).
async fn embeddings_mock_live(status: Arc<AtomicU16>, dims: Arc<AtomicU16>) -> String {
    let app = axum::Router::new().route(
        "/v1/embeddings",
        axum::routing::post(move || {
            let status = Arc::clone(&status);
            let dims = Arc::clone(&dims);
            async move {
                let n = dims.load(Ordering::SeqCst) as usize;
                let vec = vec![1.5f64; n]
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let body = format!(r#"{{"data":[{{"index":0,"embedding":[{vec}]}}]}}"#);
                (
                    axum::http::StatusCode::from_u16(status.load(Ordering::SeqCst)).unwrap(),
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
    // The embeddings row's shape: server root WITHOUT /v1 — the client
    // POSTs {base_url}/v1/embeddings.
    format!("http://127.0.0.1:{}", addr.port())
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

/// Sealed-`embeddings`-row writer (the admin page's own storage shape).
async fn write_embeddings_row(state: &AppState, base_url: String) {
    let sealed = mycelium_web::state::seal_config(
        &state.service_key,
        "embeddings",
        &mycelium_librarian::embeddings::EmbeddingsConfig {
            base_url,
            model: "mymodel".into(),
            ..Default::default()
        },
    );
    state.config.set("embeddings", &sealed).await.unwrap();
}

/// Poll until `cond` holds, with a deadline (never a bare sleep); the
/// failure reports the machine snapshot.
async fn poll_until(what: &str, mut cond: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(cond().await) {
        assert!(
            Instant::now() < deadline,
            "never reached: {what} (snapshot {:?})",
            EMBEDDING_HEALTH.snapshot()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Poll until the machine itself reaches `want`.
async fn poll_state_until(want: BackendState, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while EMBEDDING_HEALTH.state() != want {
        assert!(
            Instant::now() < deadline,
            "never saw {what}: {:?}",
            EMBEDDING_HEALTH.snapshot()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn fetch_health(cl: &reqwest::Client, base: &str) -> serde_json::Value {
    cl.get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()
}

/// The probe task mirrors the LLM probe loop: row absent → Disabled;
/// row present + reachable → Green (dims stamped); 503 → Red at the
/// 3-failure threshold; recovery → Green with the dims RE-stamped
/// (restamp-always on every ok probe); row deleted → Disabled.
#[tokio::test]
async fn embedding_probe_task_reaches_green_then_red_then_disabled() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    reset_emb_sentinels();
    // Pin Disabled first (the production boot seed): the reset state is
    // trivially Green, so without the pin the first Green assertion
    // could not tell a fed machine from an un-started task. Reaching
    // Green from here requires note_configured AND note_ok — exactly
    // what the first tick does against a reachable row.
    EMBEDDING_HEALTH.reset_for_test();
    EMBEDDING_HEALTH.set_disabled();

    let status = Arc::new(AtomicU16::new(200));
    let dims = Arc::new(AtomicU16::new(3));
    let url = embeddings_mock_live(Arc::clone(&status), Arc::clone(&dims)).await;
    let (state, _dir) = state_only().await;
    write_embeddings_row(&state, url).await;

    let shutdown = tokio_util::sync::CancellationToken::new();
    let probe = mycelium_web::server::spawn_embedding_probe(
        state.clone(),
        shutdown.clone(),
        Duration::from_millis(100),
    )
    .await;

    // Reachable → Green AND the dims stamped: the FIRST Green alone is
    // the note_configured lift (a present row lifts the Disabled pin to
    // a fresh Green BEFORE the probe's outcome applies), so the DIMS
    // condition must ride in the poll — the ok probe stamps the body's
    // 3 coordinates with note_ok. (The probe does NOT stamp
    // EMB_LAST_SUCCESS — that sentinel is the model-call hook's "last
    // successful completion"; the raw probe feeds only machine + dims.)
    poll_until("Green with the ok probe's dims stamped", || async {
        EMBEDDING_HEALTH.state() == BackendState::Green
            && EMBEDDING_DIMS.load(Ordering::Relaxed) == 3
    })
    .await;

    // Backend 503 → Yellow after 1 failure, Red at the 3-probe
    // threshold. A 503 answers instantly, so the 10 s bound is generous
    // headroom over 3×100 ms ticks.
    status.store(503, Ordering::SeqCst);
    poll_state_until(BackendState::Red, "Red after 3 failed probes").await;

    // Backend recovers → Green AND a re-stamp: the mock now answers 5
    // dims, so a stamp-once implementation would still read 3 here.
    status.store(200, Ordering::SeqCst);
    dims.store(5, Ordering::SeqCst);
    poll_state_until(BackendState::Green, "Green after the backend recovers").await;
    poll_until("the ok dims were re-stamped", || async {
        EMBEDDING_DIMS.load(Ordering::Relaxed) == 5
    })
    .await;

    // Row deleted → Disabled within two ticks.
    state.config.delete("embeddings").await.unwrap();
    poll_state_until(BackendState::Disabled, "Disabled after the row is deleted").await;

    // Join before releasing the lock (select! may pick a due tick over
    // the cancellation, so a late note_* could otherwise land after the
    // test releases TEST_LOCK and perturb a sibling's window); the
    // unwrap keeps a panicking probe a visible failure.
    shutdown.cancel();
    probe.await.unwrap();
}

/// /health carries the embedding fields (state word + optional outage
/// windows, deliberately outside the degraded rule) and /metrics
/// carries the embedding gauge and the vector counters — read through
/// the live server's real surfaces. The machine is pinned Disabled (the
/// boot seed's posture for an absent row), then driven Red through the
/// admin test-connection probes — the real passive-hook surface, never
/// a direct note call.
#[tokio::test]
async fn health_and_metrics_carry_the_embedding_fields() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    reset_emb_sentinels();
    EMBEDDING_HEALTH.reset_for_test();
    EMBEDDING_HEALTH.set_disabled();

    let (emb_url, _captured) = embeddings_mock(503, "still down").await;
    let (base, shutdown, _dir, state) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    // No row anywhere (the seed saw an absent embeddings row): Disabled
    // posture, no outage windows, and the degraded rule stays "ok" —
    // the embedding fields are observability, not part of the gate.
    let json = fetch_health(&cl, &base).await;
    assert_eq!(
        json["status"], "ok",
        "the degraded rule stays intact: {json}"
    );
    assert_eq!(json["embedding_state"], "disabled", "{json}");
    assert!(json["embedding_down_seconds"].is_null(), "{json}");
    assert!(json["embedding_checked_seconds_ago"].is_null(), "{json}");

    // A configured row lifts the posture: the machine stays Disabled
    // for the passive failure hooks (they ignore notes while pinned),
    // so the row is written and the REAL boot seed runs — present row →
    // configured postured machine.
    write_embeddings_row(&state, emb_url.clone()).await;
    mycelium_web::server::sync_embedding_from_store(&state).await;

    // Three failed probes through the real admin surface → Red.
    for _ in 0..3 {
        let resp = admin_test_embeddings(&cl, &base, &cookie, &csrf, &emb_url, None).await;
        let loc = location_of(resp).await;
        assert!(loc.contains("error="), "each failing probe flashes: {loc}");
    }
    poll_state_until(BackendState::Red, "Red through the passive hooks").await;

    let json = fetch_health(&cl, &base).await;
    assert_eq!(
        json["status"], "ok",
        "a red backend is weather, not this service's illness: {json}"
    );
    assert_eq!(json["embedding_state"], "red", "{json}");
    let down = json["embedding_down_seconds"]
        .as_i64()
        .expect("outage window");
    assert!(
        (0..=10).contains(&down),
        "the outage window is live: {down}"
    );
    let checked = json["embedding_checked_seconds_ago"]
        .as_i64()
        .expect("checked");
    assert!((0..=10).contains(&checked), "just checked: {checked}");

    let metrics = cl.get(format!("{base}/metrics")).send().await.unwrap();
    assert_eq!(metrics.status(), 200);
    let text = metrics.text().await.unwrap();
    for line in [
        "# TYPE mycelium2_embedding_up gauge",
        "# TYPE mycelium2_vector_queue_depth gauge",
        "# TYPE mycelium2_vector_processed_total counter",
    ] {
        assert!(text.contains(line), "{line} present: {text}");
    }
    assert!(
        text.contains("mycelium2_embedding_up 1"),
        "red maps to 1: {text}"
    );

    shutdown.cancel();
}
