//! Standing mechanical guards (spec §9.4) — run forever by
//! `cargo test --workspace`:
//!
//! - `csp_sweep_no_inline_style_or_script` — no page route ships an
//!   inline `style=` attribute or a `<script>` tag without `src` (the
//!   CSP's `style-src`/`script-src 'self'` disallows both; the sweep
//!   pins that).
//! - `fields_pair_labels_and_inputs` — on the form-heavy pages, every
//!   `label for="X"` pairs with a matching `id="X"` (component-level
//!   pairing is pinned by the mycelium-ui render tests; this pins the
//!   rendered pages).
//! - `contrast_tokens_meet_wcag` — the design text tokens each meet
//!   WCAG AA (≥ 4.5) on the background; `muted` is asserted as an
//!   EXCLUDED token (below the threshold by design — decorative-only,
//!   spec §5).

use std::net::SocketAddr;

use mycelium_auth::login::LoginService;
use mycelium_store::Store;
use mycelium_web::AppState;

/// Known admin password for the pre-seeded account (tests log in with it).
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";

/// Boot a test server on ephemeral ports; returns the HTTPS base URL and
/// the shutdown token. (Copied from web_integration.rs — each crate test
/// file is self-contained by convention.)
async fn boot() -> (
    String,
    tokio_util::sync::CancellationToken,
    tempfile::TempDir,
    sqlx::SqlitePool,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let pool = store.pool().clone();
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

    // Ephemeral ports.
    let https: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let http: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();

    // Bind manually to learn the port, then serve on it.
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
    // Give the listener a moment.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    (
        format!("https://127.0.0.1:{https_port}"),
        shutdown,
        dir,
        pool,
    )
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true) // self-signed test cert
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Log in as the pre-seeded admin and return the session cookie.
async fn admin_cookie(client: &reqwest::Client, base: &str) -> String {
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", ADMIN_PASSWORD)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303, "login must redirect on success");
    let cookie = login
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert!(cookie.starts_with("myc2_session="));
    cookie
}

/// No `<script` tag may lack a src attribute (CSP `script-src 'self'`).
/// Regex-free scan: every `<script` occurrence must be immediately
/// followed (modulo whitespace) by `src`.
fn assert_no_inline_scripts(html: &str, route: &str) {
    let mut start = 0usize;
    while let Some(rel) = html[start..].find("<script") {
        let tag_open = start + rel;
        let after = &html[tag_open + "<script".len()..];
        assert!(
            after.trim_start().starts_with("src"),
            "{route}: <script> without src attribute: {html}"
        );
        start = tag_open + "<script".len();
    }
}

/// Sweep every page route under the CSP: zero `style=` attributes and
/// no `<script>` without `src` (spec §9.4). These are guards, not
/// feature tests — if one fails, fix the page, not the guard.
#[tokio::test]
async fn csp_sweep_no_inline_style_or_script() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();
    let cookie = admin_cookie(&client, &base).await;

    // Session pages.
    for route in [
        "/",
        "/search",
        "/graph",
        "/skills",
        "/books",
        "/chat",
        "/keys",
        "/password",
        "/admin",
    ] {
        let resp = client
            .get(format!("{base}{route}"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{route} must render 200 for a session");
        let html = resp.text().await.unwrap();
        assert!(
            !html.contains("style="),
            "{route}: inline style= attribute (CSP violation): {html}"
        );
        assert_no_inline_scripts(&html, route);
    }

    // Public auth page: the login form renders anonymously.
    let login = client.get(format!("{base}/login")).send().await.unwrap();
    assert_eq!(login.status(), 200, "/login must render 200 anonymously");
    let html = login.text().await.unwrap();
    assert!(
        !html.contains("style="),
        "/login: inline style= attribute (CSP violation): {html}"
    );
    assert_no_inline_scripts(&html, "/login");

    // /setup is one-shot: with the admin already created it redirects to
    // /login (303) — nothing to sweep, but pin the redirect so a change
    // to the setup lifecycle is noticed here.
    let setup = client.get(format!("{base}/setup")).send().await.unwrap();
    assert_eq!(
        setup.status(),
        303,
        "/setup must redirect once an admin exists"
    );
    assert_eq!(setup.headers().get("location").unwrap(), "/login");

    shutdown.cancel();
}

/// Form-heavy pages: every `label for="X"` must pair with an `id="X"`
/// element (spec §9.4). Contains-based check per pair — full DOM
/// parsing is out of scope for a standing guard.
#[tokio::test]
async fn fields_pair_labels_and_inputs() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();
    let cookie = admin_cookie(&client, &base).await;

    for route in ["/keys", "/password"] {
        let resp = client
            .get(format!("{base}{route}"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{route} must render 200 for a session");
        let html = resp.text().await.unwrap();
        for id in label_fors(&html) {
            assert!(
                html.contains(&format!(r#"id="{id}""#)),
                "{route}: label for=\"{id}\" has no matching id=\"{id}\" input: {html}"
            );
        }
    }

    shutdown.cancel();
}

/// Extract every `for="..."` value from `<label>` tags.
fn label_fors(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    while let Some(rel) = html[start..].find("<label") {
        let tag_open = start + rel;
        let end = html[tag_open..].find('>').expect("label tag must close");
        let tag = &html[tag_open..tag_open + end + 1];
        const MARKER: &str = r#"for=""#;
        if let Some(m) = tag.find(MARKER) {
            let value_start = m + MARKER.len();
            let value_end = tag[value_start..].find('"').expect("for= value must close");
            out.push(tag[value_start..value_start + value_end].to_string());
        }
        start = tag_open + "<label".len();
    }
    out
}

/// WCAG relative luminance + contrast ratio (spec §9.4 contrast guard).
fn luminance(hex: &str) -> f64 {
    let h = hex.trim_start_matches('#');
    let vals: Vec<u8> = (0..3)
        .map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap())
        .collect();
    let chan = |v: u8| {
        let c = f64::from(v) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * chan(vals[0]) + 0.7152 * chan(vals[1]) + 0.0722 * chan(vals[2])
}

fn contrast(a: &str, b: &str) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

#[test]
fn contrast_tokens_meet_wcag() {
    let bg = "#101312";
    for (name, token, min) in [
        ("heading", "#E4E9E5", 4.5),
        ("label", "#C9D2CC", 4.5),
        ("body", "#9AA69F", 4.5),
    ] {
        let ratio = contrast(token, bg);
        assert!(
            ratio >= min,
            "{name} token {token} on {bg}: {ratio:.2} < {min}"
        );
    }
    // Muted is decorative-only (spec §5 resolution): below 4.5 by
    // design, excluded from the text list.
    assert!(contrast("#3A453F", bg) < 4.5);
}
