//! Dream/lifecycle admin-surface integration (memory-lifecycle step 5,
//! plan Task 5): GET /admin/dream (the scheduler's readouts + the whole
//! `lifecycle` row form), POST /admin/dream (`admin_save_lifecycle` —
//! blank-keeps-stored per field, save-side clamp, checkbox POST
//! contract, live static re-stamp), the carried page-read clamp
//! obligation (Task 1: `get_sealed` deserializes unclamped — the GET
//! must render clamped values), and the System page's embeddings card.
//!
//! Mirrors embeddings_admin_integration.rs (real serve surface, login,
//! CSRF double-submit via the `csrf_token` form field) and
//! dream_scheduler_integration.rs (sealed-`lifecycle`-row writer with
//! the service seal, TEST_LOCK + after-lock Drop-guard resets).
//!
//! Process-global state serialized on TEST_LOCK: every save handler
//! re-stamps the NINE lifecycle statics live (`sync_lifecycle_from_store`),
//! so every test here holds the lock and installs [`LifecycleDefaults`]
//! (declared AFTER the lock guard so the reset drops first), restoring
//! every static-bearing field to its default. The System-page test also
//! drives the process-global embeddings health machine — [`EmbDefaults`]
//! restores it.

use std::net::SocketAddr;

use mycelium_auth::login::LoginService;
use mycelium_crypto::keys::ServiceKey;
use mycelium_librarian::llm_health::{EMBEDDING_HEALTH, RED_FAILURE_THRESHOLD};
use mycelium_store::ConfigStore;
use mycelium_store::Store;
use mycelium_store::revisions::{
    DEFAULT_CONSOLIDATION_PATCHES, DEFAULT_DEDUP_COSINE, DEFAULT_QUERY_TOP_K,
    DEFAULT_RECENT_WINDOW, DEFAULT_REVISION_RETENTION, DEFAULT_RRF_K, DEFAULT_RRF_WEIGHT,
    DEFAULT_STALE_DAYS, LIFECYCLE_CONFIG_KEY, LifecycleConfig,
};
use mycelium_web::AppState;
use reqwest::Response;

/// Serializes all tests in this binary around the lifecycle statics and
/// the embeddings health machine (every save re-stamps the statics; the
/// System-page test feeds the health machine).
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Restores every static-bearing lifecycle field to its default on drop
/// (the clamping setters — the only reset seam the statics have). This
/// suite's saves re-stamp them live (`sync_lifecycle_from_store`), so
/// the reset keeps a failing assertion's row values from becoming the
/// next test's baseline. Declared AFTER the TEST_LOCK guard so the
/// restore happens while the lock is held.
struct LifecycleDefaults;

impl Drop for LifecycleDefaults {
    fn drop(&mut self) {
        use mycelium_store::revisions as rev;
        rev::set_revision_retention(DEFAULT_REVISION_RETENTION);
        rev::set_query_top_k(DEFAULT_QUERY_TOP_K);
        rev::set_rrf_k(DEFAULT_RRF_K);
        rev::set_rrf_weight_lex(DEFAULT_RRF_WEIGHT);
        rev::set_rrf_weight_vec(DEFAULT_RRF_WEIGHT);
        rev::set_dedup_cosine(DEFAULT_DEDUP_COSINE);
        rev::set_stale_days(DEFAULT_STALE_DAYS);
        rev::set_recent_window(DEFAULT_RECENT_WINDOW);
        rev::set_consolidation_patches(DEFAULT_CONSOLIDATION_PATCHES);
    }
}

/// Restores the process-global embeddings health machine on drop (the
/// System-page test drives it Disabled → Red). Declared AFTER the
/// TEST_LOCK guard so the restore happens while the lock is held.
struct EmbDefaults;

impl Drop for EmbDefaults {
    fn drop(&mut self) {
        EMBEDDING_HEALTH.reset_for_test();
    }
}

/// Known admin password for the pre-seeded account.
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";
const MALLORY_PASSWORD: &str = "mallory password 20 chars";

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

fn cookie_from(response: &Response) -> String {
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

async fn csrf_from_page(cl: &reqwest::Client, url: &str, cookie: &str) -> String {
    let page = cl.get(url).header("cookie", cookie).send().await.unwrap();
    let html = page.text().await.unwrap();
    let marker = r#"name="csrf-token" content=""#;
    let idx = html.find(marker).expect("csrf meta present");
    let rest = &html[idx + marker.len()..];
    let end = rest.find('"').expect("closing quote");
    rest[..end].to_string()
}

/// Log in as user `name`; returns (cookie, csrf).
async fn login_as(
    cl: &reqwest::Client,
    base: &str,
    name: &str,
    password: &str,
) -> (String, String) {
    let login = cl
        .post(format!("{base}/login"))
        .form(&[("username", name), ("password", password)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
    let cookie = cookie_from(&login);
    let csrf = csrf_from_page(cl, &format!("{base}/"), &cookie).await;
    (cookie, csrf)
}

async fn login_admin(cl: &reqwest::Client, base: &str) -> (String, String) {
    login_as(cl, base, "admin", ADMIN_PASSWORD).await
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

/// The Location of a 303 redirect, percent-decoded for assertions.
async fn location_of(resp: Response) -> String {
    assert_eq!(resp.status(), 303, "admin POSTs must redirect back");
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("location header")
        .to_string();
    percent_decode(&loc)
}

/// POST /admin/dream — browser-path double submit: the csrf token rides
/// as the `csrf_token` FORM FIELD (the page's own hidden field), never
/// a header. `fields` carries the rest of the form verbatim, so a test
/// controls the exact field set (the checkbox POST contract pins).
async fn save_lifecycle(
    cl: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
    fields: &[(&str, &str)],
) -> Response {
    let mut form: Vec<(String, String)> = vec![("csrf_token".to_string(), csrf.to_string())];
    for (k, v) in fields {
        form.push(((*k).to_string(), (*v).to_string()));
    }
    let refs: Vec<(&str, &str)> = form.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    cl.post(format!("{base}/admin/dream"))
        .header("cookie", cookie)
        .form(&refs)
        .send()
        .await
        .unwrap()
}

/// Sealed-`lifecycle`-row writer (dream_scheduler_integration.rs's
/// shape, generalized: the tests hand-craft rows — including
/// out-of-range values a save-side clamp would never produce — to pin
/// the READ-side clamp and the checkbox contract).
async fn write_lifecycle(config: &ConfigStore, service_key: &ServiceKey, row: LifecycleConfig) {
    let sealed = mycelium_store::config::seal_config(service_key, LIFECYCLE_CONFIG_KEY, &row);
    config.set(LIFECYCLE_CONFIG_KEY, &sealed).await.unwrap();
}

/// The stored `lifecycle` row, exactly as sealed (`get_sealed` does NOT
/// clamp — so these assertions see the truly stored values).
async fn saved_lifecycle(state: &AppState) -> LifecycleConfig {
    mycelium_store::config::get_sealed::<LifecycleConfig>(
        &state.config,
        &state.service_key,
        LIFECYCLE_CONFIG_KEY,
    )
    .await
    .expect("the lifecycle row was sealed and stored")
}

/// The distinctive stored row the blank-keeps tests compare against:
/// every numeric field in range, all different from each other and from
/// the defaults, so a blank-keeps violation cannot hide.
fn stored_row() -> LifecycleConfig {
    LifecycleConfig {
        query_top_k: 48,
        revision_retention: 333,
        rrf_k: 777,
        rrf_weight_lex: 2.5,
        rrf_weight_vec: 3.5,
        dedup_cosine: 0.8,
        stale_days: 120,
        recent_window: 30,
        consolidation_patches: 44,
        dream_interval_s: 7200,
        dream_enabled: true,
    }
}

// ---------- pinned tests ----------

/// The admin gating posture (embeddings_admin_integration.rs's
/// `embeddings_admin_surfaces_require_admin`): unauthenticated hits
/// redirect to /login (the gate runs before CSRF), a logged-in
/// non-admin is 403 — on the GET page, the save POST, and the
/// backfill POST that renders beside it.
#[tokio::test]
async fn dream_page_requires_admin() {
    let _lock = TEST_LOCK.lock().await;
    let _defaults = LifecycleDefaults;
    let (base, _shutdown, _dir, _state) = boot().await;
    let cl = client();

    // Unauthenticated: every /admin/dream surface redirects to login.
    let get = cl.get(format!("{base}/admin/dream")).send().await.unwrap();
    assert_eq!(
        get.status(),
        303,
        "unauthenticated GET /admin/dream must redirect to login"
    );
    let post = cl
        .post(format!("{base}/admin/dream"))
        .form(&[("query_top_k", "8")])
        .send()
        .await
        .unwrap();
    assert_eq!(
        post.status(),
        303,
        "unauthenticated POST /admin/dream must redirect to login"
    );
    let post = cl
        .post(format!("{base}/admin/dream/backfill"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        post.status(),
        303,
        "unauthenticated POST /admin/dream/backfill must redirect to login"
    );

    // Logged-in non-admin: every admin surface is 403.
    let (cookie, csrf) = login_as(&cl, &base, "mallory", MALLORY_PASSWORD).await;
    let get = cl
        .get(format!("{base}/admin/dream"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(get.status(), 403, "a non-admin GET must be refused");
    let post = cl
        .post(format!("{base}/admin/dream"))
        .header("cookie", &cookie)
        .form(&[("csrf_token", csrf.as_str()), ("query_top_k", "8")])
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 403, "a non-admin save must be refused");
}

/// The mixed save (the brief's both-directions pin): numerical blanks
/// keep the stored values, over-range values clamp (both directions),
/// and the checkbox POST contract — marker PRESENT with the checkbox
/// OFF ⇒ `dream_enabled` false, marker ABSENT ⇒ `dream_enabled`
/// UNCHANGED — exactly one POST encoding, pinned.
#[tokio::test]
async fn save_field_blank_keeps_stored_and_clamps() {
    let _lock = TEST_LOCK.lock().await;
    let _defaults = LifecycleDefaults;
    let (base, _shutdown, _dir, state) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    write_lifecycle(&state.config, &state.service_key, stored_row()).await;

    // Direction 1: the page's own encoding — the hidden marker rides,
    // the checkbox does not (unchecked ⇒ false). Two over-range numeric
    // edits (one above, one below the range), everything else blank.
    let resp = save_lifecycle(
        &cl,
        &base,
        &cookie,
        &csrf,
        &[
            ("dream_enabled_present", "1"),
            ("query_top_k", "9999"),
            ("stale_days", "1"),
        ],
    )
    .await;
    assert_eq!(
        location_of(resp).await,
        "/admin/dream?ok=Lifecycle config saved",
        "an accepted save flashes ok on the Dream page"
    );
    let cfg = saved_lifecycle(&state).await;
    assert_eq!(cfg.query_top_k, 64, "above-range clamps down on save");
    assert_eq!(cfg.stale_days, 7, "below-range clamps up on save");
    assert!(!cfg.dream_enabled, "marker present + checkbox off ⇒ false");
    // The blanks kept the stored values.
    assert_eq!(cfg.revision_retention, 333);
    assert_eq!(cfg.rrf_k, 777);
    assert_eq!(cfg.rrf_weight_lex, 2.5);
    assert_eq!(cfg.rrf_weight_vec, 3.5);
    assert_eq!(cfg.dedup_cosine, 0.8);
    assert_eq!(cfg.recent_window, 30);
    assert_eq!(cfg.consolidation_patches, 44);
    assert_eq!(cfg.dream_interval_s, 7200);

    // Direction 2: a synthetic client that never rendered the form —
    // NO marker field: `dream_enabled` is UNCHANGED even though no
    // checkbox arrived, and the in-range numeric edits still land.
    write_lifecycle(&state.config, &state.service_key, stored_row()).await;
    let resp = save_lifecycle(
        &cl,
        &base,
        &cookie,
        &csrf,
        &[("query_top_k", "10"), ("revision_retention", "999")],
    )
    .await;
    assert_eq!(
        location_of(resp).await,
        "/admin/dream?ok=Lifecycle config saved"
    );
    let cfg = saved_lifecycle(&state).await;
    assert!(
        cfg.dream_enabled,
        "marker ABSENT ⇒ dream_enabled unchanged (stored true survives)"
    );
    assert!(
        cfg.query_top_k == 10 && cfg.revision_retention == 999,
        "the in-range edits landed"
    );
    assert_eq!(cfg.stale_days, 120, "a blank field keeps the stored value");
}

/// The save's live contract (plan Ruling 5 / §3.2a-step-2-4): after the
/// POST, every static-bearing field reads the new value WITHOUT a
/// restart — the save re-runs `sync_lifecycle_from_store` on the
/// persisted row. `dream_enabled`/`dream_interval_s` carry no statics
/// (the scheduler reads the sealed row per tick), so they pin nothing
/// here — their live path is the scheduler's row read.
#[tokio::test]
async fn save_restamps_statics_live() {
    let _lock = TEST_LOCK.lock().await;
    let _defaults = LifecycleDefaults;
    use mycelium_store::revisions as rev;
    let (base, _shutdown, _dir, state) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    // Baseline: the fresh statics (no row anywhere — a prior test's
    // guard reset them) read the defaults, so the restamp IS observable.
    assert_eq!(rev::query_top_k(), DEFAULT_QUERY_TOP_K);
    assert_eq!(rev::revision_retention(), DEFAULT_REVISION_RETENTION);
    assert_eq!(rev::rrf_params().rrf_k, DEFAULT_RRF_K as f32);
    assert_eq!(rev::stale_days(), DEFAULT_STALE_DAYS);
    assert_eq!(rev::recent_window(), DEFAULT_RECENT_WINDOW);
    assert_eq!(rev::consolidation_patches(), DEFAULT_CONSOLIDATION_PATCHES);
    assert_eq!(rev::dedup_cosine(), DEFAULT_DEDUP_COSINE);

    let resp = save_lifecycle(
        &cl,
        &base,
        &cookie,
        &csrf,
        &[
            ("dream_enabled_present", "1"),
            ("dream_enabled", "on"),
            ("dream_interval_s", "10800"),
            ("query_top_k", "50"),
            ("revision_retention", "1500"),
            ("rrf_k", "999"),
            ("rrf_weight_lex", "7.5"),
            ("rrf_weight_vec", "0.25"),
            ("dedup_cosine", "0.99"),
            ("stale_days", "3000"),
            ("recent_window", "200"),
            ("consolidation_patches", "90"),
        ],
    )
    .await;
    assert_eq!(
        location_of(resp).await,
        "/admin/dream?ok=Lifecycle config saved"
    );

    // The statics read the saved values the instant the POST returned —
    // no restart, no boot sync.
    assert_eq!(rev::query_top_k(), 50);
    assert_eq!(rev::revision_retention(), 1500);
    assert_eq!(rev::stale_days(), 3000);
    assert_eq!(rev::recent_window(), 200);
    assert_eq!(rev::consolidation_patches(), 90);
    assert_eq!(rev::dedup_cosine(), 0.99);
    let params = rev::rrf_params();
    assert_eq!(params.rrf_k, 999.0);
    assert_eq!(params.w_lex, 7.5, "the composer's lexical weight is live");
    assert_eq!(params.w_vec, 0.25, "the composer's vector weight is live");
    // The sealed row stores exactly these values too.
    let cfg = saved_lifecycle(&state).await;
    assert_eq!(cfg.query_top_k, 50);
    assert_eq!(cfg.rrf_weight_lex, 7.5);
    assert!(cfg.dream_enabled);
    assert_eq!(cfg.dream_interval_s, 10800);
}

/// The carried obligation from Task 1 (its page half): `get_sealed`
/// deserializes the row UNclamped, so the GET page's read path applies
/// `clamp()` — an out-of-range (hand-edited) row renders CLAMPED
/// numbers in the boxes, never values a save would not accept.
#[tokio::test]
async fn page_read_renders_clamped_values() {
    let _lock = TEST_LOCK.lock().await;
    let _defaults = LifecycleDefaults;
    let (base, _shutdown, _dir, state) = boot().await;
    let cl = client();
    let (cookie, _csrf) = login_admin(&cl, &base).await;
    // A hand-edited row: every numeric field out of range (below or
    // above), which a save-side clamp would never produce.
    write_lifecycle(
        &state.config,
        &state.service_key,
        LifecycleConfig {
            query_top_k: 9999,
            revision_retention: 5,
            rrf_k: 0,
            rrf_weight_lex: 999.0,
            rrf_weight_vec: 999.0,
            dedup_cosine: 0.1,
            stale_days: 1,
            recent_window: 0,
            consolidation_patches: 1,
            dream_interval_s: 5,
            dream_enabled: true,
        },
    )
    .await;

    let page = cl
        .get(format!("{base}/admin/dream"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    // Full input-tag assertions: each field renders its clamped value
    // AND its clamp range as min/max (the unclamped value would read
    // 9999/5/0/999/0.1/1/0/1/5).
    assert!(
        html.contains(r#"name="query_top_k" type="number" min="1" max="64" value="64""#),
        "query_top_k clamps down on read: {html}"
    );
    assert!(
        html.contains(r#"name="revision_retention" type="number" min="25" max="2000" value="25""#),
        "revision_retention clamps up on read: {html}"
    );
    assert!(
        html.contains(r#"name="rrf_k" type="number" min="1" max="1024" value="1""#),
        "rrf_k clamps up on read: {html}"
    );
    assert!(
        html.contains(
            r#"name="rrf_weight_lex" type="number" step="any" min="0" max="10" value="10""#
        ),
        "rrf_weight_lex clamps down on read: {html}"
    );
    assert!(
        html.contains(
            r#"name="rrf_weight_vec" type="number" step="any" min="0" max="10" value="10""#
        ),
        "rrf_weight_vec clamps down on read: {html}"
    );
    assert!(
        html.contains(
            r#"name="dedup_cosine" type="number" step="any" min="0.5" max="1" value="0.5""#
        ),
        "dedup_cosine clamps up on read: {html}"
    );
    assert!(
        html.contains(r#"name="stale_days" type="number" min="7" max="3650" value="7""#),
        "stale_days clamps up on read: {html}"
    );
    assert!(
        html.contains(r#"name="recent_window" type="number" min="1" max="365" value="1""#),
        "recent_window clamps up on read: {html}"
    );
    assert!(
        html.contains(r#"name="consolidation_patches" type="number" min="2" max="100" value="2""#),
        "consolidation_patches clamps up on read: {html}"
    );
    assert!(
        html.contains(
            r#"name="dream_interval_s" type="number" min="3600" max="2592000" value="3600""#
        ),
        "dream_interval_s clamps up on read: {html}"
    );
}

/// The Dream page's readouts (plan Ruling 7 + the Task-4 flash targets):
/// 1. no record, no marker, not running → "no scheduled run yet" +
///    "never run";
/// 2. a `dream_last_run` record renders (at, scope, ran, summary — the
///    summary escapes its HTML, the XSS discipline);
/// 3. the `dream_backfill_at` marker's timestamp renders when present
///    (even while a walk runs — the marker is the last COMPLETION),
///    "in flight" takes over only when the marker is gone AND the
///    running-guard is claimed, "never run" when neither; the backfill
///    button posts to /admin/dream/backfill with the CSRF form field.
#[tokio::test]
async fn dream_page_renders_readouts_and_backfill_button() {
    let _lock = TEST_LOCK.lock().await;
    let _defaults = LifecycleDefaults;
    use mycelium_web::dream_scheduler::{DREAM_BACKFILL_AT_KEY, DREAM_LAST_RUN_KEY};
    let (base, _shutdown, _dir, state) = boot().await;
    let cl = client();
    let (cookie, _csrf) = login_admin(&cl, &base).await;

    // Leg 1 — the fresh-install readout.
    let page = cl
        .get(format!("{base}/admin/dream"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(
        html.contains("none yet"),
        "no record ⇒ the no-run readout: {html}"
    );
    assert!(html.contains("never run"), "no marker ⇒ never run: {html}");
    assert!(!html.contains("in flight"), "nothing running: {html}");
    assert!(
        html.contains(r#"action="/admin/dream/backfill""#),
        "the backfill button posts to the Task-4 route: {html}"
    );
    assert!(
        html.contains(r#"input type="hidden" name="csrf_token""#),
        "the button's form carries the CSRF form field: {html}"
    );
    assert!(
        html.contains(r#"action="/admin/dream""#),
        "the lifecycle form posts back to the page: {html}"
    );

    // Leg 2 — the scheduler's record renders (typed through the SAME
    // struct the scheduler writes). The summary carries markup to pin
    // the escape.
    state
        .config
        .set(
            DREAM_LAST_RUN_KEY,
            &mycelium_web::dream_scheduler::DreamLastRunRecord {
                at: "2026-11-01T12:00:00+00:00".to_string(),
                scope: "user:00000000-0000-0000-0000-000000000001".to_string(),
                ran: true,
                summary: Some("dream summary <script>alert(1)</script>".to_string()),
            },
        )
        .await
        .unwrap();
    let page = cl
        .get(format!("{base}/admin/dream"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let html = page.text().await.unwrap();
    assert!(html.contains("2026-11-01T12:00:00+00:00"), "{html}");
    assert!(
        html.contains("user:00000000-0000-0000-0000-000000000001"),
        "{html}"
    );
    assert!(html.contains("ran"), "{html}");
    assert!(
        html.contains("dream summary &lt;script&gt;alert(1)&lt;/script&gt;"),
        "the summary renders escaped: {html}"
    );
    assert!(
        !html.contains("<script>alert(1)"),
        "no executable markup survives: {html}"
    );

    // Leg 3a — the marker (the last COMPLETION) renders its timestamp.
    state
        .config
        .set(
            DREAM_BACKFILL_AT_KEY,
            &"2026-11-01T13:30:00+00:00".to_string(),
        )
        .await
        .unwrap();
    state
        .dream_backfill_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let page = cl
        .get(format!("{base}/admin/dream"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let html = page.text().await.unwrap();
    assert!(
        html.contains("2026-11-01T13:30:00+00:00"),
        "a present marker renders even while a walk is running: {html}"
    );

    // Leg 3b — marker gone + guard claimed ⇒ "in flight".
    state.config.delete(DREAM_BACKFILL_AT_KEY).await.unwrap();
    let page = cl
        .get(format!("{base}/admin/dream"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let html = page.text().await.unwrap();
    assert!(html.contains("in flight"), "marker gone + running: {html}");
    assert!(
        !html.contains("never run"),
        "in flight is not never run: {html}"
    );

    // Leg 3c — marker gone + guard released ⇒ "never run" again.
    state
        .dream_backfill_running
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let page = cl
        .get(format!("{base}/admin/dream"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let html = page.text().await.unwrap();
    assert!(
        html.contains("never run"),
        "no marker + not running ⇒ never run: {html}"
    );

    // The checkbox contract is the page's ONE encoding: the hidden
    // marker is always rendered beside the checkbox.
    assert!(
        html.contains(r#"name="dream_enabled_present""#),
        "the hidden marker field: {html}"
    );
    assert!(
        html.contains(r#"name="dream_enabled" type="checkbox""#),
        "the checkbox: {html}"
    );
}

/// The System page's embeddings indicator (facts-D §3): the mapped
/// `embedding_state` renders as the health-dot card (dot class + state
/// word + last-checked line, the backend-block shape copy) beside the
/// untouched LLM card. Driven Disabled (the boot posture) then Red
/// (three failures) so the mapping is observable, never Green-only.
#[tokio::test]
async fn system_page_shows_embedding_state_card() {
    let _lock = TEST_LOCK.lock().await;
    let _defaults = LifecycleDefaults;
    let _emb = EmbDefaults;
    let (base, _shutdown, _dir, _state) = boot().await;
    let cl = client();
    let (cookie, _csrf) = login_admin(&cl, &base).await;

    // Disabled (the production boot seed's posture for an absent row).
    EMBEDDING_HEALTH.reset_for_test();
    EMBEDDING_HEALTH.set_disabled();
    let page = cl
        .get(format!("{base}/admin/system"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(
        html.contains("health-dot health-dot--disabled"),
        "the embeddings card renders the Disabled dot: {html}"
    );
    assert!(
        html.contains("Embeddings backend disabled"),
        "the state word beside the dot (WCAG — color alone): {html}"
    );
    assert!(html.contains("never checked"), "no observation yet: {html}");
    assert!(
        html.contains("LLM runs"),
        "the llm card renders beside it: {html}"
    );

    // Lift the Disabled pin (notes are ignored while pinned —
    // llm_health.rs's `set_disabled` contract), then drive the machine
    // Red: the dot flips, and the "last checked" line replaces "never
    // checked" (the observation carried).
    EMBEDDING_HEALTH.note_configured();
    for _ in 0..RED_FAILURE_THRESHOLD {
        EMBEDDING_HEALTH.note_failure();
    }
    let page = cl
        .get(format!("{base}/admin/system"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let html = page.text().await.unwrap();
    assert!(
        html.contains("health-dot health-dot--red"),
        "the embeddings card renders the Red dot: {html}"
    );
    assert!(html.contains("Embeddings backend red"), "{html}");
    assert!(
        html.contains("last checked"),
        "the observation carries: {html}"
    );
    assert!(
        !html.contains("Embeddings backend disabled"),
        "no stale Disabled state: {html}"
    );
}
