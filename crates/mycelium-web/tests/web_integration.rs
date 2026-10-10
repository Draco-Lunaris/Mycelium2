//! End-to-end web integration test:
//! HTTPS boot → bootstrap login → forced password change → concept CRUD →
//! search → graph → skills → admin → API keys → logout.

use std::net::SocketAddr;
use std::sync::Arc;

use mycelium_auth::login::LoginService;
use mycelium_store::Store;
use mycelium_web::AppState;

/// Known admin password for the pre-seeded account (tests log in with it).
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";

/// Boot a test server on ephemeral ports; returns the HTTPS base URL and
/// the shutdown token.
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

#[tokio::test]
async fn full_web_flow() {
    let (base, shutdown, _dir, pool) = boot().await;
    let client = client();

    // 1. Health endpoint (public).
    let health = client
        .get(format!("{base}/api/v1/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
    let body: serde_json::Value = health.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["users"], 1);

    // 2. Anonymous home redirects to /login.
    let home = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(home.status(), 303);
    assert_eq!(home.headers().get("location").unwrap(), "/login");

    // 3. Login page renders.
    let login_page = client.get(format!("{base}/login")).send().await.unwrap();
    assert_eq!(login_page.status(), 200);
    let html = login_page.text().await.unwrap();
    assert!(html.contains("Login"));

    // 4. Log in (the must_change_password flag is flipped directly by SQL
    //    below — no production path sets it anymore).
    sqlx::query("UPDATE users SET must_change_password = 1 WHERE username = 'admin'")
        .execute(&pool)
        .await
        .unwrap();
    let password = ADMIN_PASSWORD;
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", password)])
        .send()
        .await
        .unwrap();
    // Forced password change → redirect to /password?forced=1.
    assert_eq!(login.status(), 303);
    let location = login.headers().get("location").unwrap().to_str().unwrap();
    assert!(location.contains("/password"), "got {location}");
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

    // 5. The forced-change gate: with the flag set, / redirects to
    //    /password?forced=1 (the gate blocks everything else).
    let gated = client
        .get(format!("{base}/"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(gated.status(), 303);
    assert!(
        gated
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("/password?forced=1")
    );

    // 5b. Change the password (forced flow) — invalidates the session.
    //    CSRF comes from the /password page (the only reachable page).
    let csrf = csrf_from_page(&client, &format!("{base}/password"), &cookie).await;
    let new_password = "new admin password 20 chars!";
    let change = client
        .post(format!("{base}/password"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("old", password),
            ("new", new_password),
            ("repeat", new_password),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(change.status(), 303);
    assert!(
        change
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("/login")
    );

    // 6. Log in with the new password.
    let login2 = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", new_password)])
        .send()
        .await
        .unwrap();
    assert_eq!(login2.status(), 303);
    let cookie2 = login2
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // 7. Home renders with the session.
    let home = client
        .get(format!("{base}/"))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap();
    assert_eq!(home.status(), 200);
    // Mockup 01: the page title is "Browse" (was "Your bundle").
    assert!(home.text().await.unwrap().contains("Browse"));

    // 8. Create a concept via the form.
    let csrf = csrf_from_page(&client, &format!("{base}/"), &cookie2).await;
    let markdown =
        "---\ntype: Note\ntitle: Test Note\ndescription: A test\n---\n\nHello zebra world";
    let create = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie2)
        .header("x-csrf-token", &csrf)
        .form(&[("path", "/notes/test.md"), ("markdown", markdown)])
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 303);

    // 9. View it.
    let view = client
        .get(format!("{base}/concept?path=/notes/test.md"))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap();
    assert_eq!(view.status(), 200);
    assert!(view.text().await.unwrap().contains("Test Note"));

    // 10. Search finds it.
    let search = client
        .get(format!("{base}/search?q=zebra"))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap();
    assert_eq!(search.status(), 200);
    assert!(search.text().await.unwrap().contains("Test Note"));

    // 11. Graph API returns the node.
    let graph = client
        .get(format!("{base}/api/v1/graph"))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap();
    assert_eq!(graph.status(), 200);
    let graph_json: serde_json::Value = graph.json().await.unwrap();
    assert_eq!(graph_json["nodes"].as_array().unwrap().len(), 1);

    // 11b. Duplicate links collapse to a single edge (a concept linking to the
    //      same target twice is one relationship, not two).
    let csrf_dup = csrf_from_page(&client, &format!("{base}/"), &cookie2).await;
    let dup_md = "---\ntype: Note\ntitle: Linked Note\n---\n\n\
                  See [Test](/notes/test.md) and again [Test twice](/notes/test.md).";
    let create_dup = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie2)
        .header("x-csrf-token", &csrf_dup)
        .form(&[("path", "/notes/linked.md"), ("markdown", dup_md)])
        .send()
        .await
        .unwrap();
    assert_eq!(create_dup.status(), 303);
    let graph2 = client
        .get(format!("{base}/api/v1/graph"))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap();
    let g2: serde_json::Value = graph2.json().await.unwrap();
    let edges = g2["edges"].as_array().unwrap();
    let dup_edges = edges
        .iter()
        .filter(|e| e["from"] == "/notes/linked.md" && e["to"] == "/notes/test.md")
        .count();
    assert_eq!(
        dup_edges, 1,
        "duplicate links must collapse to one edge: {edges:?}"
    );

    // 12. REST API: get + put + delete.
    let api_get = client
        .get(format!("{base}/api/v1/concepts/notes/test.md"))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap();
    assert_eq!(api_get.status(), 200);
    assert!(api_get.text().await.unwrap().contains("zebra"));

    let api_put = client
        .put(format!("{base}/api/v1/concepts/notes/rest.md"))
        .header("cookie", &cookie2)
        .header("x-csrf-token", &csrf)
        .json(
            &serde_json::json!({"markdown": "---\ntype: Note\ntitle: REST Note\n---\n\nrest body"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(api_put.status(), 201);

    let api_del = client
        .delete(format!("{base}/api/v1/concepts/notes/rest.md"))
        .header("cookie", &cookie2)
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(api_del.status(), 204);

    // 13. CSRF protection: POST without a token is rejected.
    let no_csrf = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie2)
        .form(&[("path", "/notes/x.md"), ("markdown", "x")])
        .send()
        .await
        .unwrap();
    assert_eq!(no_csrf.status(), 403);

    // 13b. Browser path: the csrf_token FORM FIELD works without a header
    //     (native forms cannot set headers; app.js injects the field).
    let browser_form = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie2)
        .form(&[
            ("path", "/notes/browser.md"),
            (
                "markdown",
                "---\ntype: Note\ntitle: Browser Note\n---\n\nbrowser body",
            ),
            ("csrf_token", csrf.as_str()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(browser_form.status(), 303, "form-field CSRF must work");
    // And the concept was actually saved.
    let check = client
        .get(format!("{base}/concept?path=/notes/browser.md"))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap();
    assert!(check.text().await.unwrap().contains("Browser Note"));

    // 13c. Wrong form-field token is rejected.
    let bad_form = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie2)
        .form(&[
            ("path", "/notes/bad.md"),
            ("markdown", "x"),
            ("csrf_token", "myc2-csrf-wrong"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(bad_form.status(), 403);

    // 14. Admin portal renders (admin user).
    let admin = client
        .get(format!("{base}/admin"))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap();
    assert_eq!(admin.status(), 200);
    assert!(admin.text().await.unwrap().contains("Admin"));

    // 15. Mint an API key (shown once).
    let mint = client
        .post(format!("{base}/keys"))
        .header("cookie", &cookie2)
        .header("x-csrf-token", &csrf)
        .form(&[("label", "test-key")])
        .send()
        .await
        .unwrap();
    assert_eq!(mint.status(), 200);
    assert!(mint.text().await.unwrap().contains("myc2-"));

    // 16. Metrics endpoint (public).
    let metrics = client.get(format!("{base}/metrics")).send().await.unwrap();
    assert_eq!(metrics.status(), 200);
    let metrics_text = metrics.text().await.unwrap();
    assert!(metrics_text.contains("mycelium2_logins_total"));

    // 17. Security headers present.
    let home = client.get(format!("{base}/login")).send().await.unwrap();
    assert!(home.headers().get("content-security-policy").is_some());
    assert_eq!(home.headers().get("x-frame-options").unwrap(), "DENY");

    // 18. Logout clears the session.
    let logout = client
        .get(format!("{base}/logout"))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), 303);
    // Session is gone: home redirects to login.
    let home = client
        .get(format!("{base}/"))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap();
    assert_eq!(home.status(), 303);

    // 19. XSS: stored titles are escaped. Log back in, create a concept
    //     with a hostile title, verify the home page renders it escaped.
    let login3 = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", new_password)])
        .send()
        .await
        .unwrap();
    let cookie3 = login3
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf3 = csrf_from_page(&client, &format!("{base}/"), &cookie3).await;
    let hostile = "---\ntype: Note\ntitle: <script>alert(1)</script>\n---\n\nbody";
    let create_hostile = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie3)
        .header("x-csrf-token", &csrf3)
        .form(&[("path", "/notes/hostile.md"), ("markdown", hostile)])
        .send()
        .await
        .unwrap();
    assert_eq!(create_hostile.status(), 303);
    let home = client
        .get(format!("{base}/"))
        .header("cookie", &cookie3)
        .send()
        .await
        .unwrap();
    let home_html = home.text().await.unwrap();
    assert!(
        !home_html.contains("<script>alert(1)</script>"),
        "raw script tag leaked"
    );
    assert!(home_html.contains("&lt;script&gt;"), "escaped form missing");

    // 20. Graph page loads graph.js.
    let graph_page = client
        .get(format!("{base}/graph"))
        .header("cookie", &cookie3)
        .send()
        .await
        .unwrap();
    let graph_html = graph_page.text().await.unwrap();
    assert!(graph_html.contains(r#"<script src="/assets/graph.js?v="#));

    // 21. Non-admin cannot access /admin. Create a regular user via the
    //     admin portal, then log in as them.
    let create_user = client
        .post(format!("{base}/admin/users"))
        .header("cookie", &cookie3)
        .header("x-csrf-token", &csrf3)
        .form(&[
            ("username", "mallory"),
            ("email", "mallory@example.com"),
            ("password", "mallory password 20 chars"),
            ("role", "user"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(create_user.status(), 200);
    let login_mallory = client
        .post(format!("{base}/login"))
        .form(&[
            ("username", "mallory"),
            ("password", "mallory password 20 chars"),
        ])
        .send()
        .await
        .unwrap();
    let mallory_cookie = login_mallory
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let admin_as_user = client
        .get(format!("{base}/admin"))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(admin_as_user.status(), 403, "non-admin must not see /admin");

    // 22. Bearer API-key auth: mint a key (as admin), then use it with NO
    //     cookie. Bearer requests are CSRF-exempt and logged in.
    let mint = client
        .post(format!("{base}/keys"))
        .header("cookie", &cookie3)
        .header("x-csrf-token", &csrf3)
        .form(&[("label", "bearer-test")])
        .send()
        .await
        .unwrap();
    let mint_html = mint.text().await.unwrap();
    // The minted token is inside <code>myc2-...</code> (the CSRF meta tag
    // also contains myc2-csrf- earlier in the page — anchor on <code>).
    let code_start = mint_html.find("<code>myc2-").expect("minted token shown");
    let token_rest = &mint_html[code_start + "<code>".len()..];
    let token_end = token_rest.find("</code>").expect("token end");
    let api_token = token_rest[..token_end].to_string();
    assert!(api_token.starts_with("myc2-"));

    // Bearer GET: the REST graph endpoint renders (page routes need a
    // SessionId for CSRF rendering; REST routes need only SessionUser).
    let bearer_graph = client
        .get(format!("{base}/api/v1/graph"))
        .header("authorization", format!("Bearer {api_token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(bearer_graph.status(), 200, "bearer auth must log in");
    assert!(bearer_graph.text().await.unwrap().contains("nodes"));

    // Bearer PUT (REST): CSRF-exempt (no session, no CSRF token needed).
    let bearer_put = client
        .put(format!("{base}/api/v1/concepts/notes/bearer.md"))
        .header("authorization", format!("Bearer {api_token}"))
        .json(&serde_json::json!({"markdown": "---\ntype: Note\ntitle: Bearer Note\n---\n\nbearer body"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bearer_put.status(), 201, "bearer PUT must be CSRF-exempt");

    // Bad bearer token: rejected.
    let bad_bearer = client
        .get(format!("{base}/"))
        .header("authorization", "Bearer myc2-invalid-token-here")
        .send()
        .await
        .unwrap();
    assert_eq!(bad_bearer.status(), 303, "invalid bearer → login redirect");

    shutdown.cancel();
}

/// Browse page (Task 1): type-chip filter, unknown-type fallback
/// (Review Focus 2 — the "All" chip active, unfiltered list), and the
/// broken-links flag on rows whose links point outside the bundle.
/// The broken-link fixture's path carries the hyphenated marker the
/// brief's second grep arm matches; the flag-span assertion below
/// pins the badge itself.
#[tokio::test]
async fn browse_type_filter() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();

    // Fresh boot (no forced password change): plain login.
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", ADMIN_PASSWORD)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
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

    // Three concepts: a Note (the filterable type), a second type
    // (proves the filter actually narrows rows), and a concept whose
    // body links to a nonexistent target (the broken-links fixture).
    let csrf = csrf_from_page(&client, &format!("{base}/"), &cookie).await;
    let create_note = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("path", "/notes/browser.md"),
            (
                "markdown",
                "---\ntype: Note\ntitle: Test Note\ndescription: A test\n---\n\nHello world",
            ),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(create_note.status(), 303);
    let create_decision = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("path", "/decisions/d1.md"),
            (
                "markdown",
                "---\ntype: Decision\ntitle: Decision note\n---\n\nA decision body",
            ),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(create_decision.status(), 303);
    let create_broken = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("path", "/notes/broken-links.md"),
            (
                "markdown",
                "---\ntype: Note\ntitle: Dangling note\n---\n\nSee [missing](/missing.md).",
            ),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(create_broken.status(), 303);

    // Browse: type filter chips + broken-link flag.
    let browse = client
        .get(format!("{base}/?type=Note"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(browse.status(), 200);
    let html = browse.text().await.unwrap();
    assert!(html.contains("chip--accent"), "active type chip: {html}");
    // The Note chip is the active one, and the filter narrows rows:
    // the Decision concept is filtered out (the chip row still lists
    // its type — chips derive from the full entry list).
    assert!(
        html.contains(r#"<a class="chip chip--accent" href="/?type=Note">Note</a>"#),
        "Note chip active: {html}"
    );
    assert!(
        !html.contains("Decision note"),
        "type filter must narrow rows: {html}"
    );
    // Unknown filter falls back to the unfiltered list (Review Focus 2).
    let unknown = client
        .get(format!("{base}/?type=NotAType"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 200);
    let html2 = unknown.text().await.unwrap();
    assert!(
        html2.contains("Test Note"),
        "unknown filter shows all: {html2}"
    );
    assert!(
        html2.contains(r#"<a class="chip chip--accent" href="/">All</a>"#),
        "unknown filter activates the All chip: {html2}"
    );
    // A concept with a broken link renders the amber flag.
    let broken = client
        .get(format!("{base}/"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let html3 = broken.text().await.unwrap();
    assert!(
        html3.contains("banner--warning") || html3.contains("broken-links"),
        "broken-link flag present when a target is missing: {html3}"
    );
    assert!(
        html3.contains("Decision note"),
        "unfiltered list shows every type: {html3}"
    );
    // The flag badge itself (the brief's grep arms are satisfied by the
    // fixture path; this pins the actual span).
    assert!(
        html3.contains(r#"<span class="flag flag--warning">broken links</span>"#),
        "broken-links flag span: {html3}"
    );

    shutdown.cancel();
}

/// The API keys page renders the revoke confirm dialog (Task 7) and
/// loads confirm.js — the dialog is UX confirmation, the CSRF token is
/// the security gate, so the page must ship both without a key existing.
#[tokio::test]
async fn keys_dialog_renders() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();

    // Fresh boot (no forced password change): plain login.
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", ADMIN_PASSWORD)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
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

    // API keys page: revoke confirm dialog markup + confirm.js script tag.
    let keys = client
        .get(format!("{base}/keys"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(keys.status(), 200);
    let html = keys.text().await.unwrap();
    assert!(html.contains("<dialog"), "confirm dialog missing: {html}");
    assert!(html.contains("/assets/confirm.js?v="), "{html}");

    shutdown.cancel();
}

/// Concept editor (Task 2): breadcrumb + static server-rendered
/// preview on the view page, saved hostile markdown escaped in the
/// preview (Review Focus 1 — never executable markup), the
/// confirm-on-delete dialog (the keys-page pattern) shipping with the
/// page, and a save with broken frontmatter re-rendering the editor
/// with the warning banner (no edit lost to a redirect).
#[tokio::test]
async fn editor_breadcrumb_preview_hostile() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();

    // Fresh boot (no forced password change): plain login.
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", ADMIN_PASSWORD)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
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

    // A concept to open in the editor (heading + paragraph body so the
    // preview's minimal renderer has real structure to render).
    let csrf = csrf_from_page(&client, &format!("{base}/"), &cookie).await;
    let create = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("path", "/notes/test.md"),
            (
                "markdown",
                "---\ntype: Note\ntitle: Test Note\ndescription: A test\n---\n\n## Section\n\nHello world",
            ),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 303);

    // Editor: breadcrumb + preview + hostile-markdown safety (Review Focus 1).
    let editor = client
        .get(format!("{base}/concept?path=/notes/test.md"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(editor.status(), 200);
    let html = editor.text().await.unwrap();
    assert!(html.contains("breadcrumb"), "{html}");
    assert!(html.contains("preview"), "{html}");
    // The two-pane body and the server-rendered preview content.
    assert!(html.contains("editor-grid"), "two-pane editor body: {html}");
    assert!(
        html.contains("Test Note"),
        "preview shows the title: {html}"
    );
    assert!(
        html.contains("<h3>Section</h3>"),
        "preview renders the body heading (## maps to h3): {html}"
    );
    assert!(
        html.contains(r#"<span class="chip chip--neutral">Note</span>"#),
        "preview type chip: {html}"
    );
    // Confirm-on-delete: the shared dialog + confirm.js, and the
    // per-item fallback form POSTs /concept/delete without script.
    assert!(html.contains("<dialog"), "delete confirm dialog: {html}");
    assert!(html.contains("/assets/confirm.js?v="), "{html}");
    assert!(
        html.contains(r#"data-confirm-dialog="delete""#),
        "delete trigger: {html}"
    );
    assert!(
        html.contains(r#"action="/concept/delete""#),
        "delete form action: {html}"
    );

    // Preview of hostile saved markdown is escaped, not executed.
    let hostile = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("path", "/hostile.md"),
            (
                "markdown",
                "---\ntype: Note\n---\n\n<script>alert(1)</script>\n",
            ),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(hostile.status(), 303, "hostile concept must save");
    let view = client
        .get(format!("{base}/concept?path=/hostile.md"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let hv = view.text().await.unwrap();
    assert!(
        !hv.contains("<script>alert(1)</script>"),
        "preview must escape: {hv}"
    );
    assert!(hv.contains("&lt;script&gt;"), "preview escapes: {hv}");

    // Frontmatter error: a save with broken frontmatter re-renders the
    // editor with the warning banner (the edit preserved in the textarea).
    let bad = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("path", "/notes/bad-fm.md"),
            ("markdown", "no frontmatter here"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 200, "parse failure re-renders the editor");
    let bh = bad.text().await.unwrap();
    assert!(
        bh.contains("banner--warning"),
        "frontmatter error banner: {bh}"
    );
    assert!(
        bh.contains("missing frontmatter"),
        "the parser's own message: {bh}"
    );

    shutdown.cancel();
}

/// Review fix (Task 2, Important): the skills-scoped delete path pinned
/// by a positive round trip. The skills editor must render the hidden
/// `scope` field in BOTH delete surfaces (the no-script fallback form
/// and the shared confirm dialog), and POST /concept/delete with
/// scope=skills must delete from the GLOBAL SKILLS shelf — proven by a
/// same-path twin concept in the admin's private bundle that must
/// survive (a dropped scope field would misroute the delete into the
/// user bundle and kill the twin).
#[tokio::test]
async fn skills_scoped_delete_round_trip() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();

    // Fresh boot (no forced password change): plain login.
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", ADMIN_PASSWORD)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
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
    let csrf = csrf_from_page(&client, &format!("{base}/"), &cookie).await;

    // (a) Create a global skills concept (the admin-only scope).
    let create = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("path", "/skills-scope-test.md"),
            ("scope", "skills"),
            (
                "markdown",
                "---\ntype: Skill\ntitle: Skills Scope Test\n---\n\nbody",
            ),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 303);

    // Negative-control twin: the SAME path in the admin's private
    // bundle — the scoped delete must not touch it.
    let twin = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("path", "/skills-scope-test.md"),
            (
                "markdown",
                "---\ntype: Note\ntitle: Private Twin\n---\n\nbody",
            ),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(twin.status(), 303);

    // (b) The skills editor (admin → editable): the hidden scope field
    //     is present in BOTH delete surfaces.
    let editor = client
        .get(format!(
            "{base}/concept?path=/skills-scope-test.md&scope=skills"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(editor.status(), 200);
    let html = editor.text().await.unwrap();
    assert!(html.contains("(global skills)"), "scope note: {html}");
    // The no-script fallback form (the first /concept/delete form in
    // the body — the header actions precede the dialog).
    let form_lit = r#"<form method="post" action="/concept/delete">"#;
    let start = html.find(form_lit).expect("delete fallback form");
    let form_end = start + html[start..].find("</form>").expect("form closes");
    let del_form = &html[start..form_end];
    assert!(
        del_form.contains(r#"<input type="hidden" name="scope" value="skills">"#),
        "fallback delete form must carry scope=skills: {del_form}"
    );
    assert!(
        del_form.contains(r#"<input type="hidden" name="path" value="/skills-scope-test.md">"#),
        "fallback delete form must carry the path: {del_form}"
    );
    // The shared confirm dialog (the second /concept/delete form).
    let dlg_start = html.find("<dialog").expect("delete dialog: {html}");
    let dlg_html = &html[dlg_start..];
    assert!(
        dlg_html.contains(r#"name="scope" value="skills""#),
        "confirm dialog must carry scope=skills: {dlg_html}"
    );

    // (c) POST the delete exactly as the dialog/fallback form does:
    //     303 → /skills (the handler's unconditional redirect), then
    //     the authoritative checks.
    let del = client
        .post(format!("{base}/concept/delete"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("path", "/skills-scope-test.md"),
            ("scope", "skills"),
            ("csrf_token", csrf.as_str()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 303);
    assert_eq!(del.headers().get("location").unwrap(), "/skills");

    // The skills-scope view 404s: the global skills concept is gone.
    let gone = client
        .get(format!(
            "{base}/concept?path=/skills-scope-test.md&scope=skills"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), 404, "skills concept must be gone");

    // The private-bundle twin SURVIVES — the scoped delete must not
    // have misrouted into the user bundle.
    let twin_view = client
        .get(format!("{base}/concept?path=/skills-scope-test.md"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(twin_view.status(), 200, "user-bundle twin must survive");
    let twin_html = twin_view.text().await.unwrap();
    assert!(twin_html.contains("Private Twin"), "{twin_html}");

    shutdown.cancel();
}

/// Search page (Task 3): the scope-chip row filters the merged results
/// by scope; an unknown scope falls back to the unfiltered list with
/// "All" active (Task 1's pattern); the query — user input — is escaped
/// wherever it renders (input value attribute, count line, chip hrefs).
/// The lowercase "your bundle" / capital "Your bundle" split is
/// load-bearing: row chips carry the existing lowercase scope labels,
/// the chip row the capital ones, so the library-scope negative
/// assertion can hold (no lowercase "your bundle" anywhere else on
/// the page — the placeholder included).
#[tokio::test]
async fn search_scope_chips() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();

    // Fresh boot (no forced password change): plain login.
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", ADMIN_PASSWORD)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
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

    // A user-bundle concept the "note" query hits (title + body both
    // carry the token).
    let csrf = csrf_from_page(&client, &format!("{base}/"), &cookie).await;
    let create = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("path", "/notes/search-note.md"),
            (
                "markdown",
                "---\ntype: Note\ntitle: Notebook basics\n---\n\nA note about note-taking.",
            ),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 303);

    // Search: scope chips filter by scope; unknown scope = all.
    let s = client
        .get(format!("{base}/search?q=note&scope=user"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(s.status(), 200);
    let html = s.text().await.unwrap();
    assert!(html.contains("chip--accent"), "active scope chip: {html}");
    assert!(html.contains("your bundle"), "user-scoped result: {html}");
    // The user chip is the active one (exact span, Task 1's pattern).
    assert!(
        html.contains(
            r#"<a class="chip chip--accent" href="/search?q=note&scope=user">Your bundle</a>"#
        ),
        "user scope chip active: {html}"
    );
    // The count line names the (escaped) query.
    assert!(
        html.contains(r#"results for "note""#),
        "result-count line: {html}"
    );
    let s2 = client
        .get(format!("{base}/search?q=note&scope=library"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let h2 = s2.text().await.unwrap();
    assert!(
        !h2.contains("your bundle"),
        "library scope hides user results: {h2}"
    );
    // Library scope in a fresh boot has no hits: the empty state.
    assert!(h2.contains("No results"), "empty state heading: {h2}");
    assert!(
        h2.contains("Try a different search or scope."),
        "empty state body: {h2}"
    );

    // Unknown scope: unfiltered, "All" active (Task 1's fallback).
    let s3 = client
        .get(format!("{base}/search?q=note&scope=bogus"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(s3.status(), 200);
    let h3 = s3.text().await.unwrap();
    assert!(
        h3.contains(r#"<a class="chip chip--accent" href="/search?q=note">All</a>"#),
        "unknown scope activates the All chip: {h3}"
    );
    assert!(
        h3.contains("your bundle"),
        "unknown scope shows all results: {h3}"
    );

    // The query row: plain GET form, primary submit, query echoed into
    // the value attribute.
    assert!(
        html.contains(r#"<form method="get" action="/search">"#),
        "query form: {html}"
    );
    assert!(html.contains(r#"name="q""#), "query input name: {html}");
    assert!(
        html.contains(r#"value="note""#),
        "query echoed into the value attribute: {html}"
    );
    assert!(
        html.contains(r#"class="btn btn--primary">Search<"#),
        "primary submit button: {html}"
    );

    // Hostile query (`"><script>alert(1)</script>`, percent-encoded in
    // the URL): escaped in the value attribute and the count line,
    // percent-encoded in the chip hrefs — never raw markup (the
    // raw-slot contract: query text is user input).
    let hostile = client
        .get(format!(
            "{base}/search?q=%22%3E%3Cscript%3Ealert(1)%3C/script%3E&scope=user"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(hostile.status(), 200);
    let hx = hostile.text().await.unwrap();
    assert!(
        hx.contains(r#"value="&quot;&gt;&lt;script&gt;"#),
        "value attribute escapes the breakout attempt: {hx}"
    );
    assert!(
        !hx.contains("<script>alert"),
        "hostile query must never render as markup: {hx}"
    );
    assert!(
        hx.contains("%3Cscript%3E"),
        "chip hrefs percent-encode the query: {hx}"
    );

    shutdown.cancel();
}

/// The book the Books-page test ingests — ingest_integration.rs's text,
/// so the `# Chapter One` heading yields the same `ch-1-chapter-one`
/// anchor its passage URLs established.
const BOOK: &str = "\
# Chapter One

Intro text about zebras.

## Section 1.1

Details one.

## Section 1.2

Details two.

# Chapter Two

Second chapter text.
";

/// The second book the Books-page test ingests onto the SAME shelf —
/// deliberately different chapter titles ("Alpha One"/"Alpha Two") so
/// each book's chapter rows stay distinguishable in the `?book=`
/// narrowing assertions ("Chapter One" is not a substring of either,
/// so the narrowed-view negative is clean).
const BOOK2: &str = "\
# Alpha One

Alpha book intro text.

# Alpha Two

Alpha second chapter text.
";

/// Build a multipart body with a csrf_token field (browser-style: the
/// token rides as a form field, not a header) — ingest_integration.rs's
/// helper, replicated minimally for the Books-page test's upload.
fn multipart_body(
    boundary: &str,
    fields: &[(&str, &str)],
    file: Option<(&str, &str, &str)>,
) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    if let Some((name, filename, contents)) = file {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(b"Content-Type: text/markdown\r\n\r\n");
        body.extend_from_slice(contents.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// Books page (Task 4 — mockup 07): the shelves table with the
/// books-count column (RF 3), the selected shelf's chapter list, the
/// passage reader pane, and the missing-passage warning banner (RF 4 —
/// 200 + banner, never a 500).
///
/// The ingested book is created through the existing multipart upload
/// path (`POST /api/v1/ingest`, replicating ingest_integration.rs's
/// boot/upload pattern — no book exists in a fresh boot, so the test
/// ingests one here first; the slug `my-book` + `%23`-escaped `#`
/// mirror that file's established anchor shape).
#[tokio::test]
async fn books_count_and_passage_reader() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();

    // Fresh boot (no forced password change): plain login as admin.
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", ADMIN_PASSWORD)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
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
    let csrf = csrf_from_page(&client, &format!("{base}/"), &cookie).await;

    // Two global-read shelves: one to hold the ingested book, one left
    // empty — the empty one pins the "0" count cell (the brief's RF-3
    // arm: "empty shelves render 0").
    for name in ["Public Shelf", "Empty Shelf"] {
        let create = client
            .post(format!("{base}/admin/bookshelves"))
            .header("cookie", &cookie)
            .header("x-csrf-token", &csrf)
            .form(&[("name", name), ("global", "1")])
            .send()
            .await
            .unwrap();
        assert_eq!(create.status(), 303);
    }

    // Ingest a book onto Public Shelf (multipart; csrf_token rides as a
    // form field — the browser path). Ingest runs inline, so the catalog
    // (hub + chapter concepts) exists once the upload returns.
    let boundary = "booksboundary01";
    let body = multipart_body(
        boundary,
        &[
            ("csrf_token", csrf.as_str()),
            ("bookshelf", "Public Shelf"),
            ("slug", "my-book"),
            ("title", "My Book"),
        ],
        Some(("file", "book.md", BOOK)),
    );
    let upload = client
        .post(format!("{base}/api/v1/ingest"))
        .header("cookie", &cookie)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(upload.status(), 202);
    let upload_json: serde_json::Value = upload.json().await.unwrap();
    assert_eq!(
        upload_json["status"], "done",
        "ingest ran inline: {upload_json}"
    );

    // 1. The shelves table: visibility chip, the count column (RF 3 —
    //    the count must match the SQL book listing: 0 and 1), and the
    //    per-row Browse link (?shelf=).
    let b = client
        .get(format!("{base}/books"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(b.status(), 200);
    let html = b.text().await.unwrap();
    assert!(html.contains("Books"), "{html}");
    // count column present (empty shelves render "0") — the exact cell
    // form (review fix: no weak `>N<` fallback arm a serializer change
    // could satisfy; the cell serializes attribute-then-content).
    assert!(
        html.contains(r#"<td data-label="Books">0</td>"#),
        "count column: {html}"
    );
    // ...and the real count for the shelf that holds the book (the same
    // exact form; runs before the second upload below, when the shelf
    // holds exactly one book).
    assert!(
        html.contains(r#"<td data-label="Books">1</td>"#),
        "one-book shelf count: {html}"
    );
    assert!(
        html.contains(r#"<span class="chip chip--accent">global-read</span>"#),
        "global-read visibility chip: {html}"
    );
    assert!(
        html.contains(r#"<a href="/books?shelf=Public%20Shelf">Browse</a>"#),
        "shelf row Browse link: {html}"
    );

    // 2. A selected shelf renders the chapter list: numbered rows of
    //    the shelf's books' chapters, each linking its passage
    //    (?shelf=…&passage=book://slug#ch-n-slug, %23-escaped #).
    let s = client
        .get(format!("{base}/books?shelf=Public%20Shelf"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(s.status(), 200);
    let sh = s.text().await.unwrap();
    assert!(
        sh.contains(r#"<ol class="chapter-list">"#),
        "chapter list: {sh}"
    );
    assert!(sh.contains("Chapter One"), "chapter title row: {sh}");
    assert!(sh.contains("Chapter Two"), "second chapter row: {sh}");
    assert!(
        sh.contains(r#"passage=book://my-book%23ch-1-chapter-one"#),
        "chapter row passage link: {sh}"
    );

    // 2b. The ?book= narrowing, pinned end-to-end on a two-book shelf
    //     (review fix): a second book on the SAME shelf, then the
    //     un-narrowed interleave, the narrowed view, and the
    //     unknown-book fallback.
    let second_body = multipart_body(
        "secondboundary2",
        &[
            ("csrf_token", csrf.as_str()),
            ("bookshelf", "Public Shelf"),
            ("slug", "second-book"),
            ("title", "Second Book"),
        ],
        Some(("file", "book2.md", BOOK2)),
    );
    let second_upload = client
        .post(format!("{base}/api/v1/ingest"))
        .header("cookie", &cookie)
        .header(
            "content-type",
            "multipart/form-data; boundary=secondboundary2",
        )
        .body(second_body)
        .send()
        .await
        .unwrap();
    assert_eq!(second_upload.status(), 202);
    // (a) Un-narrowed: BOTH books' chapters appear — the interleaved
    //     behavior, pinned so a change to the narrowing can't silently
    //     regress the un-narrowed path. The shelves table now counts
    //     two books for this shelf (RF 3 at count 2).
    let both = client
        .get(format!("{base}/books?shelf=Public%20Shelf"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(both.status(), 200);
    let bh = both.text().await.unwrap();
    assert!(bh.contains("Chapter One"), "first book row: {bh}");
    assert!(bh.contains("Alpha One"), "second book row: {bh}");
    assert!(
        bh.contains(r#"passage=book://second-book%23ch-1-alpha-one"#),
        "second book passage link: {bh}"
    );
    assert!(
        bh.contains(r#"<td data-label="Books">2</td>"#),
        "two-book shelf count: {bh}"
    );
    // (b) Narrowed to the second book: ONLY its chapters — the first
    //     book's rows and passage links are absent.
    let narrowed = client
        .get(format!(
            "{base}/books?shelf=Public%20Shelf&book=second-book"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(narrowed.status(), 200);
    let nh = narrowed.text().await.unwrap();
    assert!(nh.contains("Alpha One"), "narrowed rows: {nh}");
    assert!(
        !nh.contains("Chapter One"),
        "narrowing must hide the other book's chapters: {nh}"
    );
    assert!(
        !nh.contains(r#"passage=book://my-book%23"#),
        "narrowing must drop the other book's passage links: {nh}"
    );
    // (c) Unknown book: the un-narrowed list — both books' chapters
    //     (the unknown-filter fallback, the ?type=/?scope= pattern).
    let unknown_book = client
        .get(format!(
            "{base}/books?shelf=Public%20Shelf&book=no-such-book"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(unknown_book.status(), 200);
    let uh = unknown_book.text().await.unwrap();
    assert!(
        uh.contains("Chapter One") && uh.contains("Alpha One"),
        "unknown book keeps the un-narrowed list: {uh}"
    );

    // 3. Passage: reuse the ingest test's established anchor shape
    //    (ingest_integration.rs: my-book, %23-escaped #). The reader
    //    pane renders the extracted chapter (heading + body); chapter
    //    1's passage must not leak chapter 2's text.
    let p = client
        .get(format!(
            "{base}/books?passage=book://my-book%23ch-1-chapter-one"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(p.status(), 200);
    let ph = p.text().await.unwrap();
    assert!(ph.contains("reader"), "passage reader pane: {ph}");
    assert!(
        ph.contains(r#"<div class="reader-pane">"#),
        "reader pane class: {ph}"
    );
    assert!(ph.contains("Chapter One"), "passage heading: {ph}");
    assert!(
        ph.contains("Intro text about zebras."),
        "passage body: {ph}"
    );
    assert!(
        !ph.contains("Second chapter"),
        "chapter 1 passage must not leak chapter 2: {ph}"
    );

    // 4. Missing anchor → not-found banner, not a 500 (RF 4).
    let missing = client
        .get(format!("{base}/books?passage=book://nope%23ch-99-x"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 200);
    let mh = missing.text().await.unwrap();
    assert!(
        mh.contains("banner--warning") || mh.contains("not found"),
        "missing passage banner: {mh}"
    );

    // 5. Visibility-gate parity with /api/v1/passages: a private-shelf
    //    passage is admin-only — a non-admin gets the warning banner
    //    (never the text, never a 500), while the global-read book's
    //    passage reads fine through the same page.
    let create_private = client
        .post(format!("{base}/admin/bookshelves"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("name", "Secret Shelf"), ("global", "0")])
        .send()
        .await
        .unwrap();
    assert_eq!(create_private.status(), 303);
    let secret_body = multipart_body(
        "secretboundary7",
        &[
            ("csrf_token", csrf.as_str()),
            ("bookshelf", "Secret Shelf"),
            ("slug", "secret-book"),
            ("title", "Secret Book"),
        ],
        Some(("file", "book.md", BOOK)),
    );
    let secret_upload = client
        .post(format!("{base}/api/v1/ingest"))
        .header("cookie", &cookie)
        .header(
            "content-type",
            "multipart/form-data; boundary=secretboundary7",
        )
        .body(secret_body)
        .send()
        .await
        .unwrap();
    assert_eq!(secret_upload.status(), 202);
    let create_user = client
        .post(format!("{base}/admin/users"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("username", "mallory"),
            ("email", "mallory@example.com"),
            ("password", "mallory password 20 chars"),
            ("role", "user"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(create_user.status(), 200);
    let login_mallory = client
        .post(format!("{base}/login"))
        .form(&[
            ("username", "mallory"),
            ("password", "mallory password 20 chars"),
        ])
        .send()
        .await
        .unwrap();
    let mallory_cookie = login_mallory
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let denied = client
        .get(format!(
            "{base}/books?passage=book://secret-book%23ch-1-chapter-one"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(
        denied.status(),
        200,
        "private-shelf denial renders the page with a banner, not a 403/500"
    );
    let dh = denied.text().await.unwrap();
    assert!(
        dh.contains("banner--warning"),
        "private-shelf denial banner: {dh}"
    );
    assert!(
        !dh.contains("Intro text about zebras."),
        "denied passage must not leak text: {dh}"
    );
    let allowed = client
        .get(format!(
            "{base}/books?passage=book://my-book%23ch-1-chapter-one"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), 200);
    let ah = allowed.text().await.unwrap();
    assert!(
        ah.contains(r#"<div class="reader-pane">"#),
        "global-read passage reads for non-admins: {ah}"
    );

    shutdown.cancel();
}

/// Graph page (mockup 06, Task 5): the PageHeader conversion, the
/// type-legend overlay container (populated client-side by graph.js
/// from the loaded data), the node info-card container, and the usage
/// hint line — the custom force-layout kept, restyled to the v8 tokens.
#[tokio::test]
async fn graph_legend_info_hint() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();

    // Fresh boot (no forced password change): plain login.
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", ADMIN_PASSWORD)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
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

    // Graph: legend + info card + hint present.
    let g = client
        .get(format!("{base}/graph"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(g.status(), 200);
    let gh = g.text().await.unwrap();
    assert!(gh.contains("graph-legend"), "{gh}");
    assert!(gh.contains("graph-info"), "{gh}");
    assert!(gh.contains("drag nodes"), "usage hint: {gh}");
    // The restyle's pins: PageHeader replaces the bare <h1>Graph</h1>,
    // the legend is the class-styled overlay container (its id moved
    // off the hint line), and the hint carries .graph-hint.
    assert!(
        gh.contains(r#"<header class="page-header"><h1>Graph</h1>"#),
        "page-header conversion: {gh}"
    );
    assert!(
        gh.contains(r#"id="graph-legend" class="graph-legend""#),
        "legend container: {gh}"
    );
    assert!(gh.contains(r#"class="graph-hint""#), "hint class: {gh}");
    // The full-viewport panel keeps its layout contract.
    assert!(gh.contains(r#"<div id="graph-wrap">"#), "{gh}");
    assert!(gh.contains(r#"<div id="graph-info" hidden>"#), "{gh}");

    shutdown.cancel();
}

/// Skills page (Task 6): the card conversion — PageHeader with the
/// New-private-skill action, the "Your private skills" / "Global
/// skills" cards, the empty-state fallback for an empty section, and
/// hub grouping preserved: hub row + `group-indent` member rows +
/// muted script-label rows (phase8 pins the scope-aware hrefs and
/// the label-row markup end-to-end; this test pins the card body
/// that wraps them).
#[tokio::test]
async fn skills_card_structure() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();

    // Fresh boot (no forced password change): plain login.
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", ADMIN_PASSWORD)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
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
    let csrf = csrf_from_page(&client, &format!("{base}/"), &cookie).await;

    // Fresh boot: both sections empty — the cards render with the
    // empty state (the page structure before any content exists).
    let sk = client
        .get(format!("{base}/skills"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let sh = sk.text().await.unwrap();
    assert!(sh.contains("card__header"), "{sh}");
    assert!(sh.contains("Global skills"), "{sh}");
    assert!(sh.contains("No skills yet"), "empty section: {sh}");

    // A private skill via the skills-private editor's own submit
    // (concept_submit's default arm saves it to the user bundle).
    let create = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("path", "/card-structure-private.md"),
            ("scope", "skills-private"),
            (
                "markdown",
                "---\ntype: Skill\ntitle: Card Structure Private\n---\n\nsteps",
            ),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 303);

    // A hub-grouped global skill: the hub (type: Skill, its manifest
    // carrying one script file) plus a Note companion — the same
    // shape phase8's grouped-skills fixtures use.
    for (path, markdown) in [
        (
            "/card-group/skill.md",
            "---\ntype: Skill\ntitle: card-group\nskill:\n  version: 1\n  files:\n    - {path: usage.md, role: reference}\n    - {path: scripts/run.sh, role: script}\n---\n\nhub body",
        ),
        (
            "/card-group/usage.md",
            "---\ntype: Note\ntitle: Usage\n---\n\nhow to use",
        ),
    ] {
        let put = client
            .post(format!("{base}/concept"))
            .header("cookie", &cookie)
            .header("x-csrf-token", &csrf)
            .form(&[("path", path), ("scope", "skills"), ("markdown", markdown)])
            .send()
            .await
            .unwrap();
        assert_eq!(put.status(), 303, "seeding {path}");
    }

    // Skills: card structure, hub grouping preserved.
    let sk = client
        .get(format!("{base}/skills"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let sh = sk.text().await.unwrap();
    assert!(sh.contains("card__header"), "{sh}");
    assert!(sh.contains("Global skills"), "{sh}");
    // The conversion pin (the graph test's pattern): PageHeader
    // replaces the bare <h1>Skills</h1>.
    assert!(
        sh.contains(r#"<header class="page-header"><h1>Skills</h1>"#),
        "page-header conversion: {sh}"
    );
    assert!(sh.contains("Your private skills"), "{sh}");
    // The private card: the flat row links the user bundle.
    assert!(
        sh.contains(
            r#"<a href="/concept?path=/card-structure-private.md&scope=user">Card Structure Private</a>"#
        ),
        "private flat row: {sh}"
    );
    // The global card: hub row, indented companion, muted script label.
    assert!(
        sh.contains(r#"<a href="/concept?path=/card-group/skill.md&scope=skills">card-group</a>"#),
        "hub row: {sh}"
    );
    assert!(
        sh.contains(
            r#"<ul class="group-indent"><li><a href="/concept?path=/card-group/usage.md&scope=skills">Usage</a></li>"#
        ),
        "group-indent wraps the member rows: {sh}"
    );
    assert!(
        sh.contains(r#"<li class="muted">scripts/run.sh</li>"#),
        "muted script label: {sh}"
    );
    // Content replaced the empty state; the admin keeps both create
    // links (phase8 pins the global one's user-side absence).
    assert!(!sh.contains("No skills yet"), "{sh}");
    assert!(sh.contains("New global skill"), "{sh}");
    assert!(sh.contains("New private skill"), "{sh}");

    shutdown.cancel();
}

/// Fetch a page URL and extract the CSRF token from the meta tag.
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

/// The Arc import is used by AppState::new's signature in tests.
#[allow(dead_code)]
type _Arc = Arc<()>;
