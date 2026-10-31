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
    let state = AppState::new(
        store,
        service_key,
        login,
        assets_dir,
        dir.path().to_path_buf(),
        None,
    );

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
            vec!["localhost".to_string()],
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
    // Forced password change → redirect to /account?forced=1 (Task 5:
    // the /password merge — the password card's home is /account now).
    assert_eq!(login.status(), 303);
    let location = login.headers().get("location").unwrap().to_str().unwrap();
    assert_eq!(location, "/account?forced=1", "got {location}");
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
    //    /account?forced=1 (the gate blocks everything else).
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
            .contains("/account?forced=1")
    );

    // 5b. Change the password (forced flow) — invalidates the session.
    //    The /password GET is a 307 to /account now (Task 5's merge —
    //    the password card lives there); CSRF comes from the /account
    //    page (the only reachable page), and ?forced=1 renders the
    //    must-change warning banner above the password card.
    let pw_page = client
        .get(format!("{base}/password"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(pw_page.status(), 307, "/password GET must be a 307");
    assert_eq!(pw_page.headers().get("location").unwrap(), "/account");
    let account_page = client
        .get(format!("{base}/account?forced=1"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(account_page.status(), 200);
    let acct_html = account_page.text().await.unwrap();
    assert!(
        acct_html.contains("You must change your password before continuing"),
        "forced banner on /account?forced=1: {acct_html}"
    );
    assert!(
        acct_html.contains("card__header"),
        "account page renders cards: {acct_html}"
    );
    let csrf = csrf_from_page(&client, &format!("{base}/account"), &cookie).await;
    let new_password = "new admin password 20 chars!";

    // 5b-i. Wrong old password → the /account composition with the
    // failure in the banner slot (200, not a redirect), and — Task 7 —
    // the re-render must carry a LIVE csrf token in both the password
    // form's hidden field and the head's csrf meta. An empty token
    // would strand the user: the retry they just clicked would 403.
    let wrong = client
        .post(format!("{base}/password"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("old", "definitely not the old password"),
            ("new", new_password),
            ("repeat", new_password),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 200, "wrong password re-renders /account");
    let wrong_html = wrong.text().await.unwrap();
    assert!(
        wrong_html.contains("invalid credentials"),
        "the failure rides the banner slot: {wrong_html}"
    );
    assert!(
        !wrong_html.contains("You must change your password before continuing"),
        "the forced banner is replaced by the failure banner (single slot): {wrong_html}"
    );
    let retry_csrf = {
        let marker = r#"name="csrf-token" content=""#;
        let idx = wrong_html
            .find(marker)
            .expect("csrf meta on the error re-render");
        let rest = &wrong_html[idx + marker.len()..];
        rest[..rest.find('"').expect("closing quote")].to_string()
    };
    assert!(
        !retry_csrf.is_empty(),
        "the error re-render carries a live csrf token (Task 7): {wrong_html}"
    );
    let hidden = {
        let marker = r#"action="/password">
  <input type="hidden" name="csrf_token" value=""#;
        let idx = wrong_html
            .find(marker)
            .expect("password form hidden csrf on the error re-render");
        let rest = &wrong_html[idx + marker.len()..];
        rest[..rest.find('"').expect("closing quote")].to_string()
    };
    assert_eq!(
        hidden, retry_csrf,
        "the form's hidden field and the head meta carry the same live token"
    );

    // 5b-ii. The retry the user actually makes: the token from the
    // error page's own form field (no re-GET), plus the RIGHT old
    // password → the change succeeds (303 → /login).
    let change = client
        .post(format!("{base}/password"))
        .header("cookie", &cookie)
        .form(&[
            ("csrf_token", retry_csrf.as_str()),
            ("old", password),
            ("new", new_password),
            ("repeat", new_password),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(
        change.status(),
        303,
        "retry with the error page's token succeeds: {change:?}"
    );
    assert!(
        change
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("/login")
    );

    // (The pre-Task-7 success leg — fresh csrf via the x-csrf-token
    // header — is folded into 5b-ii above: the retry IS the successful
    // change, so the sequence pins both the fix and the change itself.)

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

    // 14. Admin portal: PR 3's split — /admin is now a TRUE 307 to
    //     /admin/users (bookmarked URLs keep working; RequireAdmin
    //     stays on the redirect so the non-admin gate below still
    //     sees 403), and the users page renders.
    let admin = client
        .get(format!("{base}/admin"))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap();
    assert_eq!(admin.status(), 307, "/admin must be a 307, not axum's 303");
    assert_eq!(
        admin.headers().get("location").unwrap(),
        "/admin/users",
        "307 targets the users page"
    );
    let users_page = client
        .get(format!("{base}/admin/users"))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap();
    assert_eq!(users_page.status(), 200);
    assert!(
        users_page.text().await.unwrap().contains("Username"),
        "users page table"
    );

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
    // The minted token is inside <code id="minted-key">myc2-...</code>
    // (the CSRF meta tag also contains myc2-csrf- earlier in the page —
    // anchor on the minted <code> element; its id is the Task-7 copy
    // island's target).
    let code_start = mint_html
        .find("<code id=\"minted-key\">")
        .expect("minted token shown");
    let token_rest = &mint_html[code_start + "<code id=\"minted-key\">".len()..];
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
        html.contains(
            r#"<a class="chip chip--accent" href="/?type=Note" aria-current="true">Note</a>"#
        ),
        "Note chip active: {html}"
    );
    // Filter semantics: `aria-current="true"` on the active chip only
    // (the tab bar's navigation semantics use `aria-current="page"`);
    // inactive chips carry no aria-current at all.
    assert!(
        html.contains(r#"<a class="chip" href="/">All</a>"#),
        "inactive All chip has no aria-current: {html}"
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
        html2.contains(r#"<a class="chip chip--accent" href="/" aria-current="true">All</a>"#),
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
    // Task 8 (mockup 01): the header actions area carries the small
    // search field — a plain GET form aimed at /search with the `q`
    // input, beside the type chips. Scoped to the actions slot so the
    // assertion is about the header, not any other form on the page.
    let actions_area = html3
        .split(r#"<div class="actions">"#)
        .nth(1)
        .and_then(|s| s.split("</header>").next())
        .unwrap_or("");
    assert!(
        actions_area.contains(r#"action="/search""#) && actions_area.contains(r#"name="q""#),
        "browse header actions must carry the small search form: {actions_area}"
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
            r#"<a class="chip chip--accent" href="/search?q=note&scope=user" aria-current="true">Your bundle</a>"#
        ),
        "user scope chip active: {html}"
    );
    // Inactive scope chips carry no aria-current (filter semantics).
    assert!(
        html.contains(r#"<a class="chip" href="/search?q=note">All</a>"#),
        "inactive All scope chip has no aria-current: {html}"
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
        h3.contains(
            r#"<a class="chip chip--accent" href="/search?q=note" aria-current="true">All</a>"#
        ),
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
    // The inline ingest drives the LlmClient (no `llm` row at boot → the
    // default endpoint), whose passive hooks (llm.rs record_outcome) note
    // the process-global LLM_HEALTH machine at the real clock — hold
    // HEALTH_TEST_LOCK across the upload so those notes cannot land
    // inside a health-pin test's force→assert window (llm_health.rs's
    // guard rule; see the section comment by HEALTH_TEST_LOCK below).
    let upload_json: serde_json::Value = {
        let _health_guard = HEALTH_TEST_LOCK.lock().await;
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
        upload.json().await.unwrap()
    };
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
    // (Guard: inline-ingest passive health note — see the first upload.)
    let second_upload = {
        let _health_guard = HEALTH_TEST_LOCK.lock().await;
        client
            .post(format!("{base}/api/v1/ingest"))
            .header("cookie", &cookie)
            .header(
                "content-type",
                "multipart/form-data; boundary=secondboundary2",
            )
            .body(second_body)
            .send()
            .await
            .unwrap()
    };
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
    // (Guard: inline-ingest passive health note — see the first upload.)
    let secret_upload = {
        let _health_guard = HEALTH_TEST_LOCK.lock().await;
        client
            .post(format!("{base}/api/v1/ingest"))
            .header("cookie", &cookie)
            .header(
                "content-type",
                "multipart/form-data; boundary=secretboundary7",
            )
            .body(secret_body)
            .send()
            .await
            .unwrap()
    };
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

/// Task 7: the first hydration islands go live. Chat + keys load the
/// hydrate bundle as a module script plus the islands.js traversal
/// script (the standing-guards sweep's /chat and /keys legs then pin
/// the type-first attribute order — the linked carry), and the keys
/// minted banner carries the copy-button island with the key value
/// still selectable text beside it (the SSR markup IS the no-JS
/// degradation).
#[tokio::test]
async fn chat_and_keys_load_hydrate_bundle_and_copy_island() {
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

    // Chat + keys load the hydrate bundle as a module script; guards
    // scan accepts the type-first attribute order (linked carry).
    let chat = client
        .get(format!("{base}/chat"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), 200);
    let ch = chat.text().await.unwrap();
    assert!(
        ch.contains(r#"<script type="module" src="/assets/mycelium_ui.js"#),
        "{ch}"
    );
    assert!(ch.contains(r#"src="/assets/islands.js?v="#), "{ch}");
    assert!(ch.contains("leptos-island"), "composer island root: {ch}");

    let keys = client
        .get(format!("{base}/keys"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(keys.status(), 200);
    let kh = keys.text().await.unwrap();
    assert!(
        kh.contains(r#"<script type="module" src="/assets/mycelium_ui.js"#),
        "{kh}"
    );

    // Keys: the minted banner gains the copy-button island; the key
    // value stays selectable text beside it (minting re-renders the
    // page with the shown-once banner).
    let csrf = csrf_from_page(&client, &format!("{base}/keys"), &cookie).await;
    let mint = client
        .post(format!("{base}/keys"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("label", "hydrate")])
        .send()
        .await
        .unwrap();
    assert_eq!(mint.status(), 200);
    let mh = mint.text().await.unwrap();
    assert!(mh.contains("copy-button"), "copy island: {mh}");
    assert!(
        mh.contains(r#"id="minted-key""#),
        "selectable key value: {mh}"
    );
    assert!(
        mh.contains(r#"data-copy-target="minted-key""#),
        "island points at the value element: {mh}"
    );

    shutdown.cancel();
}

/// Task 8: the keys page restyle — the minted banner is a shown-once
/// card (the `shown-once` class marks the value row) carrying the copy
/// island beside the still-selectable key value, and a revoked key's
/// row renders dimmed: `row--revoked` rides on the row's pre-composed
/// cell content (data_table owns the `<tr>`, so the cells carry the row
/// state) with "Revoked" text in the action cell instead of a live
/// Revoke button.
#[tokio::test]
async fn keys_shown_once_banner_and_revoked_row() {
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
    let csrf = csrf_from_page(&client, &format!("{base}/keys"), &cookie).await;

    // Mint a key: the response re-renders with the shown-once banner
    // card — the copy island beside the selectable key value.
    let mint = client
        .post(format!("{base}/keys"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("label", "e2e-key")])
        .send()
        .await
        .unwrap();
    assert_eq!(mint.status(), 200);
    let mh = mint.text().await.unwrap();
    assert!(mh.contains("shown-once"), "shown-once card: {mh}");
    assert!(
        mh.contains("copy-button"),
        "copy island on minted key: {mh}"
    );
    assert!(
        mh.contains(r#"id="minted-key""#),
        "selectable key value: {mh}"
    );

    // Mint a second key, then revoke the FIRST: the table renders rows
    // in creation order, so the first data-key-id occurrence is the
    // first key's row trigger.
    let mint2 = client
        .post(format!("{base}/keys"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("label", "e2e-key-2")])
        .send()
        .await
        .unwrap();
    assert_eq!(mint2.status(), 200);
    let m2h = mint2.text().await.unwrap();
    let marker = "data-key-id=\"";
    let idx = m2h.find(marker).expect("first key's revoke trigger");
    let rest = &m2h[idx + marker.len()..];
    let end = rest.find('"').expect("id must close");
    let first_id = rest[..end].to_string();
    let revoke = client
        .post(format!("{base}/keys/revoke"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("id", first_id.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(revoke.status(), 303, "revoke redirects back to /keys");

    // The keys page now dims the revoked row: row--revoked on the
    // row's cell content, "Revoked" text in the action cell (no live
    // button on a revoked row), and only the ACTIVE key still carries
    // a data-key-id trigger.
    let keys = client
        .get(format!("{base}/keys"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(keys.status(), 200);
    let kh = keys.text().await.unwrap();
    assert!(kh.contains("row--revoked"), "dimmed revoked row: {kh}");
    assert!(
        kh.contains(">Revoked</span>"),
        "action cell shows Revoked text: {kh}"
    );
    assert_eq!(
        kh.matches(marker).count(),
        1,
        "only the active key carries a revoke trigger: {kh}"
    );

    shutdown.cancel();
}

/// Admin split (Task 4, PR 3): the five admin pages render under the
/// TabBar, `/admin` is a TRUE 307 to `/admin/users`, every admin GET
/// stays behind the RequireAdmin gate (403 for a logged-in non-admin —
/// the exact status the old `/admin` gave non-admins, pinned by
/// full_web_flow step 21 for `/admin` itself), hostile usernames
/// escape in the users table, and the create-user error flow lands on
/// the users page with the error banner visible.
#[tokio::test]
async fn admin_split_pages() {
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

    // 1. All five pages render 200 for the admin, each carrying the
    //    TabBar's five labels (spec §6) and its own markers.
    let pages: &[(&str, &[&str])] = &[
        ("/admin/users", &["Username", "Create user"]),
        // The en-dash range hint is the current form label's exact text.
        ("/admin/sign-in", &["Session TTL", "15–10080", "OIDC"]),
        ("/admin/llm", &["LLM backend"]),
        (
            "/admin/library",
            &["Bookshelves", "Upload", "Global skills"],
        ),
        ("/admin/system", &["Download backup"]),
    ];
    let mut sign_in_html = String::new();
    let mut system_html = String::new();
    for (route, markers) in pages {
        let resp = client
            .get(format!("{base}{route}"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{route} must render for the admin");
        let html = resp.text().await.unwrap();
        for label in ["Users", "Sign-in", "LLM backend", "Library", "System"] {
            assert!(
                html.contains(label),
                "{route}: TabBar label {label} missing: {html}"
            );
        }
        for marker in *markers {
            assert!(
                html.contains(marker),
                "{route}: marker {marker} missing: {html}"
            );
        }
        if *route == "/admin/system" {
            system_html = html;
        } else if *route == "/admin/sign-in" {
            sign_in_html = html;
        }
    }
    // Mockup 11: the five security fields' ranges render as SMALL LINES
    // UNDER the fields in `.field__hint` elements (the CSS exists), not
    // baked into the labels. Each hint text is the old label's range
    // text, character-for-character (en dashes and all).
    let hints: &[(&str, &str)] = &[
        ("Session TTL", "(minutes, 15–10080; 720 = 12h)"),
        ("Login failures before lockout", "(3–10)"),
        ("Lockout backoff cap", "(seconds, 30–3600)"),
        ("Minimum password length", "(12–128)"),
        ("Passage cap", "(chars, 16384–1048576; 131072 = 128k)"),
    ];
    let mut hint_zone = sign_in_html
        .find(r#"action="/admin/security""#)
        .unwrap_or(sign_in_html.len());
    // Narrow to the security form: from the form tag to its closing tag.
    if let Some(form_end) = sign_in_html[hint_zone..].find("</form>") {
        hint_zone += form_end;
    }
    let form = &sign_in_html[..hint_zone];
    for (label, hint) in hints {
        assert!(
            form.contains(label),
            "sign-in: plain field label {label} missing: {form}"
        );
        assert!(
            !form.contains(&format!(">{label} {hint}<")),
            "sign-in: {label} range must live in a hint line, not the label: {form}"
        );
        assert!(
            form.contains(&format!(r#"<p class="field__hint">{hint}</p>"#)),
            "sign-in: hint {hint} must render in a field__hint element: {form}"
        );
    }
    // The system page surfaces /health's fields as StatTiles (the
    // StatTile class), not prose.
    assert!(
        system_html.contains(r#"<div class="stat">"#),
        "StatTile markup on /admin/system: {system_html}"
    );

    // Sidebar (spec §6): the Admin group's ACTIVE item is the current
    // page — pinned on both sides. The markup is the sidebar's
    // `nav-item` (the TabBar's `tab` links carry their own active state),
    // so the assertion targets the nav-item class and would fail if the
    // page passed a stale `/admin` active_path.
    let users_page_html = client
        .get(format!("{base}/admin/users"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        users_page_html.contains(
            r#"<a href="/admin/users" aria-current="page" class="nav-item nav-item--active">"#
        ),
        "sidebar Users item must be active on /admin/users: {users_page_html}"
    );
    assert!(
        users_page_html.contains(r#"<a href="/admin/system" class="nav-item">"#),
        "sidebar System item must NOT be active on /admin/users: {users_page_html}"
    );

    // 2. /admin → 307 with Location: /admin/users (a TRUE 307 — axum's
    //    Redirect::to is a 303; the spec's §6 route map pins 307).
    let redirect = client
        .get(format!("{base}/admin"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(redirect.status(), 307, "/admin must be a 307, not a 303");
    assert_eq!(redirect.headers().get("location").unwrap(), "/admin/users");

    // 3. Non-admin gate: every admin GET (the redirect included) is
    //    rejected with RequireAdmin's 403 — the same status the old
    //    /admin gave non-admins (rbac.rs: a logged-in non-admin gets
    //    FORBIDDEN from the extractor).
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
    assert_eq!(create_user.status(), 200, "user creation must succeed");
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
    for route in [
        "/admin",
        "/admin/users",
        "/admin/sign-in",
        "/admin/llm",
        "/admin/library",
        "/admin/system",
    ] {
        let resp = client
            .get(format!("{base}{route}"))
            .header("cookie", &mallory_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            403,
            "{route} must reject a non-admin with the old gate's status"
        );
    }

    // 4. Hostile username escapes in the users table: seed a user with
    //    <script> in the name through the real create form (no
    //    production path validates usernames — the table must escape).
    let hostile = client
        .post(format!("{base}/admin/users"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("username", "<script>alert(1)</script>"),
            ("email", "hostile@example.com"),
            ("password", "hostile password 20 chars"),
            ("role", "user"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(hostile.status(), 200, "hostile-name creation must succeed");
    let users_page = client
        .get(format!("{base}/admin/users"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let users_html = users_page.text().await.unwrap();
    assert!(
        !users_html.contains("<script>alert(1)</script>"),
        "raw script leaked into the users table: {users_html}"
    );
    assert!(
        users_html.contains("&lt;script&gt;"),
        "escaped hostile username missing: {users_html}"
    );

    // 5. Create-user error flow: mismatched passwords redirect to
    //    /admin/users?error=… (followed manually — the client pins
    //    redirects), and the error renders as the page's banner.
    let mismatch = client
        .post(format!("{base}/admin/users"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("username", "mismatch-user"),
            ("email", "mismatch@example.com"),
            ("password", "mismatch password 20 chars"),
            ("password_confirm", "different password entirely!!"),
            ("role", "user"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(mismatch.status(), 303, "mismatch must redirect, not 400");
    let location = mismatch
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        location.starts_with("/admin/users?error="),
        "error redirect must land on the users page: {location}"
    );
    let follow = client
        .get(format!("{base}{location}"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(follow.status(), 200);
    let follow_html = follow.text().await.unwrap();
    assert!(
        follow_html.contains("passwords do not match"),
        "the error must render on the users page: {follow_html}"
    );

    // 6. Session-only provisioning: POST /admin/users is a browser-flow
    //    admin action. A bearer API key (an MCP/REST data credential)
    //    passes CSRF (bearer-exempt) and RequireAdmin, but the
    //    SessionId extractor 401s the session-less request before the
    //    handler runs — pinning that user creation is not a bearer
    //    capability.
    let mint = client
        .post(format!("{base}/keys"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("label", "admin-split-bearer")])
        .send()
        .await
        .unwrap();
    assert_eq!(mint.status(), 200);
    let mint_html = mint.text().await.unwrap();
    let code_start = mint_html
        .find("<code id=\"minted-key\">")
        .expect("minted token shown");
    let token_rest = &mint_html[code_start + "<code id=\"minted-key\">".len()..];
    let token_end = token_rest.find("</code>").expect("token end");
    let api_token = token_rest[..token_end].to_string();
    assert!(api_token.starts_with("myc2-"));
    let bearer_create = client
        .post(format!("{base}/admin/users"))
        .header("authorization", format!("Bearer {api_token}"))
        .form(&[
            ("username", "bearer-user"),
            ("email", "bearer@example.com"),
            ("password", "bearer password 20 chars"),
            ("role", "user"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(
        bearer_create.status(),
        401,
        "bearer API keys must not provision users — the session extractor rejects session-less POSTs"
    );

    shutdown.cancel();
}

/// Clamp round-trip pin (plan §"open questions" item 4): a POST
/// /admin/security with values outside the guardrail ranges —
/// bypassing the form's min/max — is CLAMPED by the handler, never
/// rejected (admin_save_security builds a SecurityConfig and calls
/// `.clamped()` before storing; state.rs `clamped()` clamps every
/// field into security_bounds). Pinned on both sides of the round
/// trip: the STORED config row holds the clamped values (save-side
/// clamp — the raw ConfigStore read proves the handler, not the
/// read-side clamp, did the work), and GET /admin/sign-in renders
/// them back. Own boot, so the mutated security config never leaks
/// to other tests.
#[tokio::test]
async fn security_settings_clamp_round_trip() {
    let (base, shutdown, _dir, pool) = boot().await;
    let client = client();
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
    let csrf = csrf_from_page(&client, &format!("{base}/admin/sign-in"), &cookie).await;

    // Below-min AND above-max values, sent directly (no form min/max).
    let save = client
        .post(format!("{base}/admin/security"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("session_ttl_minutes", "1"),      // below min 15
            ("login_max_failures", "1000"),    // above max 10
            ("login_lockout_seconds", "1"),    // below min 30
            ("min_password_length", "999999"), // above max 128
            ("passage_max_chars", "1"),        // below min 16384
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(
        save.status(),
        303,
        "out-of-range values must clamp, not reject"
    );
    assert_eq!(
        save.headers().get("location").unwrap(),
        "/admin/sign-in?ok=Security%20settings%20saved"
    );

    // The stored row itself holds the clamped values — the handler's
    // `.clamped()` ran before the write (security_bounds: TTL 15..=10080,
    // failures 3..=10, lockout 30..=3600, password 12..=128,
    // passage 16384..=1048576).
    let config = mycelium_store::ConfigStore::new(pool.clone());
    let stored: serde_json::Value = config
        .get::<serde_json::Value>("security")
        .await
        .unwrap()
        .expect("security config stored");
    assert_eq!(stored["session_ttl_minutes"], 15, "TTL clamped up to 15");
    assert_eq!(
        stored["login_max_failures"], 10,
        "failures clamped down to 10"
    );
    assert_eq!(
        stored["login_lockout_seconds"], 30,
        "lockout clamped up to 30"
    );
    assert_eq!(
        stored["min_password_length"], 128,
        "password length clamped down to 128"
    );
    assert_eq!(
        stored["passage_max_chars"], 16384,
        "passage cap clamped up to 16384"
    );

    // The page's own read-back renders the clamped values.
    let page = client
        .get(format!("{base}/admin/sign-in"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    for value in ["15", "10", "30", "128", "16384"] {
        assert!(
            html.contains(&format!(r#"value="{value}""#)),
            "clamped value {value} must render back: {html}"
        );
    }

    shutdown.cancel();
}

/// Test-side HTML entity decoder for the five entities
/// `pages::html_escape` produces. `&amp;` MUST be decoded last, or a
/// source `&lt;` would double-decode into `<` — the same ordering rule
/// the real parsers follow.
fn unescape_for_test(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

/// Library page (Task 6, PR 3): the shelves table's Books count column
/// and the upload form's shelf select, pinned against a HOSTILE shelf
/// name (`<b>Shelf</b> & Co` — `POST /admin/bookshelves` validates
/// nothing, so the name reaches the page verbatim).
///
/// Three positions are pinned, and they are pinned SEPARATELY because
/// the raw name legitimately appears in two different rendered shapes:
///
/// 1. the table CELL — `TrustedHtml::from_markup(html_escape(name))`
///    interpolated into `<td data-label="Name">…</td>` via `inner_html`
///    (Task 3's raw-slot contract: the caller escapes), so the exact
///    escaped text must sit between the cell's tags;
/// 2. the select OPTION — `<option value="{escaped}">{escaped}</option>`
///    (the same `html_escape` for both positions; it escapes `"` too,
///    which is what makes it attribute-safe);
/// 3. the raw markup must be ABSENT — no `<b>Shelf` anywhere, i.e. the
///    name never becomes an element.
///
/// The PAIRING pin is empirical, not textual: the test extracts the
/// option's `value` attribute from the rendered page, decodes it the way
/// a browser would, and POSTs it as the upload form's `bookshelf` field
/// — the ingest resolves the shelf by name, so a value that did not
/// round-trip would answer "unknown bookshelf". The count column is
/// pinned on both arms: `0` while the hostile shelf is empty, `1` once
/// the paired upload lands (and the `0` cell is then gone — the column
/// tracks the store, it is not a constant).
#[tokio::test]
async fn admin_library_escapes_and_pairs_shelf_names() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();
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
    let csrf = csrf_from_page(&client, &format!("{base}/admin/library"), &cookie).await;

    // The hostile name: tags, an ampersand, and a quote-free space —
    // `html_escape` turns it into `&lt;b&gt;Shelf&lt;/b&gt; &amp; Co`.
    let hostile = "<b>Shelf</b> & Co";
    let escaped = "&lt;b&gt;Shelf&lt;/b&gt; &amp; Co";
    let create = client
        .post(format!("{base}/admin/bookshelves"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("name", hostile), ("global", "1")])
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 303, "shelf creation redirects to library");

    let page = client
        .get(format!("{base}/admin/library"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();

    // 1. The table cell: the escaped name, exactly, between the Name
    //    cell's tags (the data_table's narrow-reflow attribute and the
    //    cell content are one contiguous string).
    assert!(
        html.contains(&format!(r#"<td data-label="Name">{escaped}</td>"#)),
        "hostile shelf name must render escaped in the table cell: {html}"
    );
    // 2. The select option: value attribute AND label text both in the
    //    escaped-for-attribute form (html_escape escapes `"` as well, so
    //    the attribute cannot be broken out of).
    assert!(
        html.contains(&format!(r#"<option value="{escaped}">{escaped}</option>"#)),
        "upload form's shelf option must carry the escaped name in both positions: {html}"
    );
    // 3. No raw markup anywhere: the name never becomes a `<b>` element
    //    (in a cell, an option, or anywhere else on the page).
    assert!(
        !html.contains("<b>Shelf"),
        "raw hostile markup leaked into /admin/library: {html}"
    );
    assert!(
        !html.contains("&lt;b&gt;Shelf&lt;/b&gt; & Co<"),
        "half-escaped form (ampersand left raw) would double-decode: {html}"
    );
    // 4. The count column for the fresh, empty hostile shelf. One shelf
    //    on a fresh boot → exactly one Books cell, holding "0".
    assert_eq!(
        html.matches(r#"<td data-label="Books">"#).count(),
        1,
        "one row per shelf: {html}"
    );
    assert!(
        html.contains(r#"<td data-label="Books">0</td>"#),
        "empty shelf count: {html}"
    );

    // 5. The pairing pin, empirically: decode the option's value the way
    //    a browser would and post it back as the upload form's
    //    `bookshelf` field. Scoped to the upload form's select — the
    //    create form above it has its own `global` select whose options
    //    are `0`/`1`.
    let select_at = html
        .find(r#"<select name="bookshelf""#)
        .expect("upload form's shelf select");
    let after_select = &html[select_at..];
    let option_marker = r#"<option value=""#;
    let at = after_select
        .find(option_marker)
        .expect("shelf option present in the upload form");
    let rest = &after_select[at + option_marker.len()..];
    let raw_value = &rest[..rest.find('"').expect("value attribute closes")];
    assert_eq!(
        raw_value, escaped,
        "the option's value attribute is the escaped-for-attribute form"
    );
    let posted = unescape_for_test(raw_value);
    assert_eq!(
        posted, hostile,
        "the decoded value is exactly the stored shelf name"
    );

    // The upload form posts that decoded value (browser semantics: the
    // option's value attribute is what the form submits, entities
    // already decoded). Ingest resolves the shelf BY NAME, so this is
    // the pairing proof — a non-round-tripping value would answer
    // "unknown bookshelf" (400).
    let boundary = "librarypairing01";
    let body = multipart_body(
        boundary,
        &[
            ("csrf_token", csrf.as_str()),
            ("bookshelf", &posted),
            ("slug", "hostile-shelf-book"),
            ("title", "Hostile Shelf Book"),
        ],
        Some(("file", "book.md", BOOK)),
    );
    // The inline ingest drives the LlmClient (no `llm` row at boot → the
    // default endpoint), whose passive hooks (llm.rs record_outcome) note
    // the process-global LLM_HEALTH machine at the real clock — hold
    // HEALTH_TEST_LOCK across the upload so those notes cannot land
    // inside a health-pin test's force→assert window (llm_health.rs's
    // guard rule; see the section comment by HEALTH_TEST_LOCK below).
    let upload_json: serde_json::Value = {
        let _health_guard = HEALTH_TEST_LOCK.lock().await;
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
        assert_eq!(upload.status(), 202, "the posted shelf name must resolve");
        upload.json().await.unwrap()
    };
    assert_eq!(
        upload_json["status"], "done",
        "ingest ran inline: {upload_json}"
    );

    // 6. The count column follows the store: the same shelf now holds
    //    one book, and the "0" cell is gone (still exactly one Books
    //    cell — the row count did not change). The escaping pins still
    //    hold with a book on the shelf.
    let after = client
        .get(format!("{base}/admin/library"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let after_html = after.text().await.unwrap();
    assert_eq!(
        after_html.matches(r#"<td data-label="Books">"#).count(),
        1,
        "still one row: {after_html}"
    );
    assert!(
        after_html.contains(r#"<td data-label="Books">1</td>"#),
        "count column tracks the seeded book: {after_html}"
    );
    assert!(
        !after_html.contains(r#"<td data-label="Books">0</td>"#),
        "the stale 0 cell must be gone: {after_html}"
    );
    assert!(
        after_html.contains(&format!(r#"<td data-label="Name">{escaped}</td>"#)),
        "cell escaping holds after ingest: {after_html}"
    );
    assert!(
        !after_html.contains("<b>Shelf"),
        "no raw markup after ingest: {after_html}"
    );

    shutdown.cancel();
}

/// Task 6 (PR 4, carried Phase-9 follow-up): `POST /admin/bookshelves`
/// no longer swallows the store result. A duplicate shelf name is
/// rejected by `bookshelves.name TEXT NOT NULL UNIQUE`; the old
/// `let _ =` discarded that error and answered with the SUCCESS banner
/// while creating nothing — a false success.
///
/// Legs:
/// 1. the first create succeeds and redirects with `?ok=`;
/// 2. the duplicate redirects to the library page with the GENERIC
///    error literal (`?error=bookshelf%20creation%20failed`) — and the
///    store's Display text must NOT ride the URL (the Phase-9
///    convention: no internal detail in a URL that lands in history and
///    referrers);
/// 3. GET /admin/library renders the danger banner with the generic
///    message and NOT the success banner;
/// 4. the table holds exactly ONE row with that name (the duplicate
///    never landed), pinned both in the rendered page and in the DB.
#[tokio::test]
async fn duplicate_bookshelf_name_reports_error_not_false_success() {
    let (base, shutdown, _dir, pool) = boot().await;
    let client = client();
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
    let csrf = csrf_from_page(&client, &format!("{base}/admin/library"), &cookie).await;

    let post_create = || {
        let client = &client;
        let base = &base;
        let cookie = &cookie;
        let csrf = &csrf;
        async move {
            client
                .post(format!("{base}/admin/bookshelves"))
                .header("cookie", cookie)
                .header("x-csrf-token", csrf)
                .form(&[("name", "Duplicate Shelf"), ("global", "0")])
                .send()
                .await
                .unwrap()
        }
    };

    // 1. First create: success, `?ok=` flash.
    let first = post_create().await;
    assert_eq!(first.status(), 303);
    assert_eq!(
        first.headers().get("location").unwrap(),
        "/admin/library?ok=Bookshelf%20created",
        "the success arm is unchanged"
    );

    // 2. The SAME name again: the UNIQUE constraint rejects the insert.
    //    The redirect must carry the generic literal, never the store's
    //    error Display.
    let second = post_create().await;
    assert_eq!(second.status(), 303);
    let location = second
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        location, "/admin/library?error=bookshelf%20creation%20failed",
        "duplicate create must bounce the generic error literal"
    );
    let lower = location.to_lowercase();
    for leak in ["sqlite", "constraint", "unique", "0001_init", "database"] {
        assert!(
            !lower.contains(leak),
            "internal error detail must not ride the URL: {location}"
        );
    }

    // 3. Following the redirect, the library page renders the danger
    //    banner with the generic message and no success banner (the
    //    flash lives in the query string, so the redirect TARGET is
    //    what carries it).
    let page = client
        .get(format!("{base}{location}"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(
        html.contains("banner--danger"),
        "the error flash renders the danger banner: {html}"
    );
    assert!(
        html.contains("<p>bookshelf creation failed</p>"),
        "the generic message renders in the banner: {html}"
    );
    assert!(
        !html.contains("banner--success"),
        "the false-success banner must be gone: {html}"
    );
    assert!(
        !html.contains("Bookshelf created"),
        "no ok flash after a failed create: {html}"
    );

    // 4. Exactly one row with that name — in the rendered table and in
    //    the store.
    assert_eq!(
        html.matches(r#"<td data-label="Name">Duplicate Shelf</td>"#)
            .count(),
        1,
        "the duplicate must not create a second row: {html}"
    );
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM bookshelves WHERE name = 'Duplicate Shelf'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 1, "the store holds exactly one such shelf");

    shutdown.cancel();
}

/// Task 2 (PR 4) + Task 6: the Librarian status page — GET /status
/// renders for the admin with the four queue StatTiles, the state chips,
/// the ingest-jobs table (moved here from the PR-3 System stopgap, spec
/// §8's PR-4 ruling), and the receipts panel over real queue rows
/// (Task 6 retired the issue-#1 placeholder): the honest empty state
/// before any row exists, then every seeded row — terminal `done`/`dead`
/// included — newest-first with its tool, status, user, attempts, age and
/// escaped detail. The sidebar's Librarian group carries the Status item
/// (admin-only, spec §6).
#[tokio::test]
async fn librarian_status_page() {
    let (base, shutdown, _dir, pool) = boot().await;
    let client = client();
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

    // Seed one completed ingest job so the jobs table renders its
    // headers (data_table renders the empty state — no headers — on
    // zero rows) and a row whose cells the assertions can pin. Direct
    // SQL on the test pool, satisfying the FKs (shelf → book → job →
    // the seeded admin user).
    let now = chrono::Utc::now().to_rfc3339();
    let user_id: String = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let shelf_id = uuid::Uuid::new_v4().to_string();
    let book_id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO bookshelves (id, name, created_at) VALUES (?, 'Status Shelf', ?)")
        .bind(&shelf_id)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO books (id, bookshelf_id, slug, title, created_at)
         VALUES (?, ?, 'status-book', 'Status Book', ?)",
    )
    .bind(&book_id)
    .bind(&shelf_id)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO ingest_jobs (id, bookshelf_id, book_id, requested_by_user_id, status, detail, created_at, updated_at)
         VALUES (?, ?, ?, ?, 'done', '', ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&shelf_id)
    .bind(&book_id)
    .bind(&user_id)
    .bind(&now)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();

    // 1. Admin GET → 200 with every surface (spec §8's /status slice).
    let status = client
        .get(format!("{base}/status"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), 200, "/status must render for the admin");
    let html = status.text().await.unwrap();
    // The four queue StatTiles (the brief's exact labels) inside the
    // PR-3 .stat-row (reused, not duplicated).
    for label in ["Queue depth", "Oldest pending", "Dead", "LLM last success"] {
        assert!(
            html.contains(&format!(r#"<span class="stat__label">{label}</span>"#)),
            "{label} tile missing: {html}"
        );
    }
    assert!(
        html.contains(r#"<div class="stat-row">"#),
        "StatTile row missing: {html}"
    );
    // State chips: fresh boot → db ok, empty queue → the "ok" chip
    // (the /health status rule mirrored); no LLM success ever recorded
    // in this test process → the "LLM unreachable" chip.
    assert!(
        html.contains(r#"<span class="chip chip--neutral">ok</span>"#),
        "ok state chip missing: {html}"
    );
    assert!(
        html.contains("LLM unreachable"),
        "LLM unreachable chip missing: {html}"
    );
    // The jobs table moved here from /admin/system: headers + the
    // seeded row's Book cell.
    assert!(
        html.contains("<th>Book</th>") && html.contains("<th>Status</th>"),
        "jobs table headers missing: {html}"
    );
    assert!(
        html.contains(r#"<td data-label="Book">status-book</td>"#),
        "seeded job row missing: {html}"
    );
    // The receipts panel with an empty queue (Task 6): no placeholder,
    // no fabricated row — the card says there is nothing to show.
    assert!(
        !html.contains("queue-work") && !html.contains("Receipt IDs and state names are pending"),
        "the issue-#1 receipt placeholder must be retired: {html}"
    );
    assert!(
        html.contains(r#"<p class="muted">no queue items</p>"#),
        "an empty receipts panel must say so: {html}"
    );

    // 2. The sidebar's Librarian group carries Status for the admin,
    //    active on this page (spec §6: the item is admin-only).
    assert!(
        html.contains(
            r#"<a href="/status" aria-current="page" class="nav-item nav-item--active">"#
        ),
        "Status nav item must be present and active for the admin: {html}"
    );

    // 3. Task 6: two TERMINAL queue rows for the admin — the states the
    //    System page's waiting table deliberately hides and the receipts
    //    panel exists to show. Direct SQL on the test pool (the panel
    //    renders metadata only, so no payload blob is involved); the
    //    dead row is the newer enqueue, and its detail is live markup
    //    that must arrive escaped.
    let done_id = uuid::Uuid::new_v4().to_string();
    let dead_id = uuid::Uuid::new_v4().to_string();
    for (id, tool, status, attempts, detail, age_secs) in [
        (done_id.as_str(), "add", "done", 1, "integrated", 3720),
        (
            dead_id.as_str(),
            "update",
            "dead",
            3,
            "<script>alert(1)</script>",
            120,
        ),
    ] {
        let aged = (chrono::Utc::now() - chrono::Duration::seconds(age_secs)).to_rfc3339();
        sqlx::query(
            "INSERT INTO mutation_queue (id, user_id, tool, status, attempts, detail,
                                         created_at, updated_at, next_retry_at, final_paths)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL)",
        )
        .bind(id)
        .bind(&user_id)
        .bind(tool)
        .bind(status)
        .bind(attempts)
        .bind(detail)
        .bind(&aged)
        .bind(&aged)
        .execute(&pool)
        .await
        .unwrap();
    }
    let page = client
        .get(format!("{base}/status"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    // Headers + both rows, full receipt ids and every column.
    for header in [
        "Receipt", "Tool", "Status", "User", "Attempts", "Age", "Detail",
    ] {
        assert!(
            html.contains(&format!("<th>{header}</th>")),
            "receipts column {header} missing: {html}"
        );
    }
    assert!(
        html.contains(r#"<td data-label="Tool">add</td>"#)
            && html.contains(r#"<td data-label="Status">done</td>"#)
            && html.contains(r#"<td data-label="Attempts">1</td>"#)
            && html.contains(r#"<td data-label="Age">1h 2m</td>"#)
            && html.contains("<td data-label=\"Detail\">integrated</td>"),
        "the done receipt row missing its cells: {html}"
    );
    assert!(
        html.contains(r#"<td data-label="Tool">update</td>"#)
            && html.contains(r#"<td data-label="Status">dead</td>"#)
            && html.contains(r#"<td data-label="Attempts">3</td>"#)
            && html.contains(r#"<td data-label="Age">2m</td>"#),
        "the dead receipt row missing its cells: {html}"
    );
    // Both owners read "admin" (the join), and the full ids render — the
    // receipt is the thing an operator pastes into `memory_status`.
    assert_eq!(
        html.matches(r#"<td data-label="User">admin</td>"#).count(),
        2,
        "both rows must carry the joined username: {html}"
    );
    assert!(
        html.contains(&done_id) && html.contains(&dead_id),
        "full receipt ids missing ({done_id} / {dead_id}): {html}"
    );
    // Newest enqueue first (the dead row is the later one).
    assert!(
        html.find(&dead_id) < html.find(&done_id),
        "receipts must render newest-enqueued-first: {html}"
    );
    // The detail path is escaped — the payload never reaches the page as
    // live markup.
    assert!(
        html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "the dead row's detail must render escaped: {html}"
    );
    assert!(
        !html.contains("<script>alert(1)</script>"),
        "unescaped detail markup leaked into the page: {html}"
    );
    // The empty-state line is gone once there is something to show.
    assert!(
        !html.contains(r#"<p class="muted">no queue items</p>"#),
        "the empty line must not stand beside real rows: {html}"
    );

    shutdown.cancel();
}

/// Task 2 (PR 4), Review Focus 3: a non-admin's GET /status is rejected
/// with the admin gate's exact 403 (RequireAdmin — the same status the
/// /admin pages' loop pins), and the sidebar's Librarian group shows no
/// Status item for a normal user (spec §6: Status is admin-only; the
/// 403 body has no sidebar, so the nav check runs on a real page).
#[tokio::test]
async fn status_non_admin_403() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();
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
    // Create a normal user through the real admin form.
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
    assert_eq!(create_user.status(), 200, "user creation must succeed");
    let login_mallory = client
        .post(format!("{base}/login"))
        .form(&[
            ("username", "mallory"),
            ("password", "mallory password 20 chars"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(login_mallory.status(), 303);
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

    // The gate: the same 403 the admin pages' loop pins.
    let status = client
        .get(format!("{base}/status"))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(
        status.status(),
        403,
        "non-admin GET /status must hit the RequireAdmin 403"
    );

    // The nav gate: a normal user's shelled page carries no /status
    // nav item.
    let browse = client
        .get(format!("{base}/"))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(browse.status(), 200);
    let html = browse.text().await.unwrap();
    assert!(
        !html.contains(r#"href="/status""#),
        "Status nav item must not render for a normal user: {html}"
    );

    shutdown.cancel();
}

/// Task 3 (PR 4): the Account security page + the TOTP enroll →
/// verify → disable round-trip.
///
/// TOTP semantics pinned from mycelium-auth (totp.rs): `set_secret`
/// carries NO pending/confirmed state — its `Option<&ServiceKey>`
/// parameter only selects the at-rest encoding (`Some` = sealed
/// `enc1:` envelope, `None` = legacy plaintext) — so enroll makes the
/// second factor LIVE the moment it persists (login requires a code
/// from then on; login.rs's `totp_enrolled_requires_code` pins it)
/// and the verify POST is a UX confirmation, not a state gate. The
/// wrong-code leg (stale_totp_code_keeps_enrollment_pending) pins
/// that reality exactly: the DB row stays SET, password-only login
/// is rejected, and the page re-renders the verify form with the
/// try-again banner — never the resting "enrolled" row.
///
/// QR deviation: no QR generator exists in the repo and adding a
/// dependency is not this task's decision — the enrollment card
/// renders the otpauth:// URL as selectable text beside the secret;
/// a QR image lands when a QR dependency does.
#[tokio::test]
async fn account_totp_flow() {
    let (base, shutdown, _dir, pool) = boot().await;
    let client = client();

    // Fresh boot: no forced password change; plain admin login.
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

    // Leg 1: GET /account → 200 with the three cards; the TOTP row
    // and the WebAuthn row both read not-enrolled; the WebAuthn row
    // shows the credential count + the disabled Register placeholder
    // (Task 4 wires the real action); the password card posts the
    // EXISTING /password handler; and the nav's Account group carries
    // Account security (leg 6 — the group is not admin-gated).
    let page = client
        .get(format!("{base}/account"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200, "GET /account for a session");
    let html = page.text().await.unwrap();
    // Task-5 review pin: the forced-change banner is ?forced=1-driven,
    // never session-state-driven — a plain GET on a normal account must
    // NOT show it.
    assert!(
        !html.contains("must change your password before continuing"),
        "no forced banner on a plain GET /account: {html}"
    );
    assert!(html.contains("Change password"), "password card: {html}");
    assert!(html.contains("Two-factor"), "2FA card: {html}");
    assert!(html.contains("Recovery key"), "recovery card: {html}");
    assert!(
        html.contains(r#"<form method="post" action="/password">"#),
        "password card posts the existing /password handler: {html}"
    );
    assert_eq!(
        html.matches("not enrolled").count(),
        2,
        "TOTP + WebAuthn rows both show not-enrolled chips: {html}"
    );
    assert!(
        html.contains(r#"<form method="post" action="/account/totp/enroll">"#),
        "TOTP Enroll action: {html}"
    );
    assert!(
        html.contains("0 credentials"),
        "WebAuthn row shows the credential count: {html}"
    );
    assert!(
        html.contains(r#"<button class="btn" disabled data-webauthn-register>Register</button>"#),
        "WebAuthn row's disabled Register button (webauthn.js enables it when script runs): {html}"
    );
    assert!(
        html.contains(r#"href="/account""#),
        "nav Account group carries Account security (not admin-gated): {html}"
    );

    let csrf = csrf_from_page(&client, &format!("{base}/account"), &cookie).await;

    // Leg 2: POST /account/totp/enroll → the enrollment card: the
    // secret text, the otpauth URL (rendered as text — the QR
    // deviation), and the verify form.
    let enroll = client
        .post(format!("{base}/account/totp/enroll"))
        .header("cookie", &cookie)
        .form(&[("csrf_token", csrf.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(enroll.status(), 200);
    let eh = enroll.text().await.unwrap();
    assert!(eh.contains("otpauth://"), "otpauth URL renders: {eh}");
    assert!(
        eh.contains(r#"<form method="post" action="/account/totp/verify">"#),
        "verify form on the enrollment card: {eh}"
    );
    let marker = r#"id="totp-secret">"#;
    let idx = eh.find(marker).expect("secret renders for manual entry");
    let rest = &eh[idx + marker.len()..];
    let secret = rest[..rest.find('<').unwrap()].to_string();
    assert!(!secret.is_empty(), "non-empty secret: {eh}");

    // Leg 3 — stale_totp_code_keeps_enrollment_pending: a wrong code
    // does NOT complete the flow. Reality: enrollment went live at
    // enroll time — the row stays SET, password-only login is now
    // rejected, and the response re-renders the verify form with the
    // spec §7 try-again banner. It must NOT render the resting
    // enrolled row (the Disable action) as if verified.
    let wrong = client
        .post(format!("{base}/account/totp/verify"))
        .header("cookie", &cookie)
        .form(&[("csrf_token", csrf.as_str()), ("code", "000000")])
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 200);
    let wh = wrong.text().await.unwrap();
    assert!(
        wh.contains("code expired/invalid"),
        "the spec §7 try-again banner: {wh}"
    );
    assert!(
        wh.contains(r#"<form method="post" action="/account/totp/verify">"#),
        "verify form re-renders: {wh}"
    );
    assert!(
        !wh.contains(r#"action="/account/totp/disable""#),
        "the resting enrolled row must not render before a successful verify: {wh}"
    );
    let (row,): (Option<String>,) =
        sqlx::query_as("SELECT totp_secret FROM users WHERE username = 'admin'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        row.is_some(),
        "set_secret made the enrollment live (no pending state exists)"
    );
    // The behavior-level reality pin: password-only login is rejected.
    let totp_login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", ADMIN_PASSWORD)])
        .send()
        .await
        .unwrap();
    assert_eq!(
        totp_login.status(),
        200,
        "not a 303: password-only login is rejected while enrolled"
    );
    assert!(
        totp_login
            .text()
            .await
            .unwrap()
            .contains("Invalid credentials")
    );

    // Leg 4: the RIGHT code (minted with mycelium-auth's exact
    // verification parameters) completes the flow → the resting
    // enrolled row (Disable action) + the success banner.
    let code = totp_code(&secret);
    let verify = client
        .post(format!("{base}/account/totp/verify"))
        .header("cookie", &cookie)
        .form(&[("csrf_token", csrf.as_str()), ("code", code.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(verify.status(), 200);
    let vh = verify.text().await.unwrap();
    assert!(
        vh.contains("Authenticator app verified"),
        "success banner: {vh}"
    );
    assert!(
        vh.contains(r#"<form method="post" action="/account/totp/disable">"#),
        "Disable action on the enrolled row: {vh}"
    );
    assert!(
        !vh.contains(r#"<form method="post" action="/account/totp/verify">"#),
        "verify form gone after success: {vh}"
    );

    // Leg 5: POST disable → the not-enrolled row again; the row is
    // gone at the store level too.
    let disable = client
        .post(format!("{base}/account/totp/disable"))
        .header("cookie", &cookie)
        .form(&[("csrf_token", csrf.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(disable.status(), 200);
    let dh = disable.text().await.unwrap();
    assert!(dh.contains("not enrolled"), "not-enrolled chip back: {dh}");
    assert!(
        dh.contains(r#"<form method="post" action="/account/totp/enroll">"#),
        "Enroll action back: {dh}"
    );
    let (row,): (Option<String>,) =
        sqlx::query_as("SELECT totp_secret FROM users WHERE username = 'admin'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(row.is_none(), "clear_secret removed the enrollment");

    shutdown.cancel();
}

/// Task 4 (PR 4): the recovery-key regenerate flow + the WebAuthn
/// register/remove wiring on /account.
///
/// Recovery-flow reality, pinned by reading login.rs in full before
/// writing this: NO recovery-login path exists — `unseal_with_recovery`
/// has zero production callers and login takes username + password (+
/// TOTP) only, so a recovery key can never log a user in. The
/// old-dead/new-live legs therefore pin the SEAL level: the record
/// persisted by rotation unseals with the new key and refuses the old
/// one — the exact property any future recovery-login path would rely
/// on. On top of the brief's floor (row-change +
/// session-preservation) this is the strongest pin expressible today.
///
/// Review Focus 1 (wrong password): the failed check must leave the
/// stored sealed row byte-identical — no rotation, no persist. Review
/// Focus 5: rotation touches seals only, never the sessions table, so
/// a second session keeps working.
///
/// WebAuthn: webauthn-rs-core 0.5 ships no soft-token test helper
/// (fake.rs only generates credential-ID distributions), so no real
/// browser ceremony can run in-process; the legs pin the server
/// surfaces — the register POST failing closed without a challenge,
/// the challenge GET's creation-options JSON (session-gated; anonymous
/// requests hit the uniform login-gate 303 like every other protected
/// page, not a 401), and the per-credential Remove rows + removal via
/// a seeded credential.
///
/// Fix round — concurrent regenerate: the race is closed by
/// compare-and-swap (update_sealed conditions the UPDATE on the
/// PRE-READ sealed JSON, so the loser writes zero rows and the
/// handler renders a retry banner, never a shown-once card for an
/// already-dead key). The racing interleaving itself is not
/// deterministically expressible in an in-process e2e test — the CAS
/// semantics are pinned by users.rs's stale-pre-read unit leg, and
/// the handler's zero-rows arm rides code review; this flow pins the
/// sequential face of the same mechanism (each POST re-reads the
/// record, so its own CAS matches).
#[tokio::test]
async fn recovery_and_webauthn_flows() {
    let (base, shutdown, _dir, pool) = boot().await;
    let client = client();

    // Seed a second user with a KNOWN recovery key: boot() discards the
    // admin's create-time key (shown once, by design), but old-dead/
    // new-live needs key material for the previous seal.
    const ROT_PW: &str = "rotator password known to the suite";
    let rot_master = mycelium_crypto::generate_master_key();
    let old_recovery = mycelium_crypto::generate_recovery_key();
    let rot_sealed = mycelium_crypto::seal_master_key(&rot_master, ROT_PW, &old_recovery).unwrap();
    let rot_json = serde_json::to_string(&rot_sealed).unwrap();
    let rot_hash = mycelium_auth::hash_password(ROT_PW).unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO users (id, username, email, role, auth_provider, password_hash, sealed_master_key, created_at, updated_at)
         VALUES (?, 'rotator', 'rotator@localhost.local', 'user', 'local', ?, ?, ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&rot_hash)
    .bind(&rot_json)
    .bind(&now)
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();

    // Two independent sessions (Review Focus 5's setup).
    let login1 = client
        .post(format!("{base}/login"))
        .form(&[("username", "rotator"), ("password", ROT_PW)])
        .send()
        .await
        .unwrap();
    assert_eq!(login1.status(), 303);
    let cookie1 = login1
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let login2 = client
        .post(format!("{base}/login"))
        .form(&[("username", "rotator"), ("password", ROT_PW)])
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
    let csrf1 = csrf_from_page(&client, &format!("{base}/account"), &cookie1).await;

    // Leg 1 — wrong_password_leaves_seal_untouched (Review Focus 1):
    // the password check runs BEFORE any rotation or persist, so a
    // failed check leaves the stored row byte-identical.
    let (pre_sealed,): (String,) =
        sqlx::query_as("SELECT sealed_master_key FROM users WHERE username = 'rotator'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let wrong = client
        .post(format!("{base}/account/recovery/regenerate"))
        .header("cookie", &cookie1)
        .form(&[
            ("csrf_token", csrf1.as_str()),
            ("current_password", "wrong password entirely!!"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 200, "the failed check re-renders the page");
    let wh = wrong.text().await.unwrap();
    assert!(
        wh.contains("Current password is incorrect"),
        "the wrong-password banner: {wh}"
    );
    assert!(
        !wh.contains(r#"id="new-recovery-key""#),
        "no key may render on a failed password check: {wh}"
    );
    // The resting regenerate form renders with paired label/input (the
    // standing label-pairing guard also sweeps /account).
    assert!(
        wh.contains(r#"<form method="post" action="/account/recovery/regenerate">"#),
        "regenerate form: {wh}"
    );
    assert!(wh.contains(r#"for="regen-password""#), "label pairs: {wh}");
    assert!(wh.contains(r#"id="regen-password""#), "input pairs: {wh}");
    // The Register button stays the script-less disabled shape and the
    // hydrate bundle does NOT load on the resting page.
    assert!(
        wh.contains(r#"<button class="btn" disabled data-webauthn-register>Register</button>"#),
        "Register renders disabled for script-less browsers: {wh}"
    );
    assert!(!wh.contains("islands.js"), "resting page stays light: {wh}");
    let (post_sealed,): (String,) =
        sqlx::query_as("SELECT sealed_master_key FROM users WHERE username = 'rotator'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        pre_sealed, post_sealed,
        "Review Focus 1: the sealed row must be byte-identical after the failed check"
    );

    // Leg 2 — rotation_shows_once_and_stores: the right password rotates
    // the recovery seal, persists, and renders the shown-once card.
    let ok = client
        .post(format!("{base}/account/recovery/regenerate"))
        .header("cookie", &cookie1)
        .form(&[("csrf_token", csrf1.as_str()), ("current_password", ROT_PW)])
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let oh = ok.text().await.unwrap();
    assert!(
        oh.contains(r#"<code id="new-recovery-key">"#),
        "the shown-once key renders: {oh}"
    );
    assert!(
        oh.contains(r#"class="shown-once""#),
        "the keys-page shown-once treatment: {oh}"
    );
    assert!(
        oh.contains(r#"data-copy-target="new-recovery-key""#),
        "copy-button island: {oh}"
    );
    assert!(
        oh.contains("islands.js"),
        "the rotated page loads the hydrate bundle for the copy island: {oh}"
    );
    // Extract the shown-once key (hex + prefix — no escapable characters).
    let marker = r#"<code id="new-recovery-key">"#;
    let idx = oh.find(marker).expect("key marker");
    let rest = &oh[idx + marker.len()..];
    let new_key = rest[..rest.find('<').unwrap()].to_string();
    assert!(
        new_key.starts_with("myc2-recovery-"),
        "parsed key looks like a recovery key: {new_key}"
    );
    // The stored row CHANGED.
    let (rot_stored,): (String,) =
        sqlx::query_as("SELECT sealed_master_key FROM users WHERE username = 'rotator'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_ne!(rot_stored, pre_sealed, "rotation must persist a new record");
    // Seal-level old-dead / new-live (the strongest pin expressible
    // while no recovery-login path exists): the new key unseals the
    // SAME master; the old key is dead.
    let rot_after: mycelium_crypto::SealedMasterKey = serde_json::from_str(&rot_stored).unwrap();
    let by_new = mycelium_crypto::unseal_with_recovery(&rot_after, &new_key).unwrap();
    assert_eq!(
        by_new.as_bytes(),
        rot_master.as_bytes(),
        "the new key must unseal the SAME master key"
    );
    assert!(
        mycelium_crypto::unseal_with_recovery(&rot_after, &old_recovery).is_err(),
        "the OLD recovery key must be dead after rotation"
    );
    // The password seal carried over: password login still works.
    let relogin = client
        .post(format!("{base}/login"))
        .form(&[("username", "rotator"), ("password", ROT_PW)])
        .send()
        .await
        .unwrap();
    assert_eq!(relogin.status(), 303, "password login survives rotation");

    // Leg 3 — rotation_preserves_other_sessions (Review Focus 5): the
    // second session still works — rotation never touches sessions.
    let still = client
        .get(format!("{base}/account"))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap();
    assert_eq!(
        still.status(),
        200,
        "rotation must not invalidate other sessions"
    );

    // Leg 4 — the pre-seeded admin rotates too (its create-time key is
    // unknown by design, so this leg pins row-change + the new key
    // unsealing the same master the password does).
    let admin_login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", ADMIN_PASSWORD)])
        .send()
        .await
        .unwrap();
    assert_eq!(admin_login.status(), 303);
    let cookie_a = admin_login
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf_a = csrf_from_page(&client, &format!("{base}/account"), &cookie_a).await;
    let (admin_pre,): (String,) =
        sqlx::query_as("SELECT sealed_master_key FROM users WHERE username = 'admin'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let admin_ok = client
        .post(format!("{base}/account/recovery/regenerate"))
        .header("cookie", &cookie_a)
        .form(&[
            ("csrf_token", csrf_a.as_str()),
            ("current_password", ADMIN_PASSWORD),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(admin_ok.status(), 200);
    let ah = admin_ok.text().await.unwrap();
    let idx = ah.find(marker).expect("admin key marker");
    let rest = &ah[idx + marker.len()..];
    let admin_key = rest[..rest.find('<').unwrap()].to_string();
    let (admin_stored,): (String,) =
        sqlx::query_as("SELECT sealed_master_key FROM users WHERE username = 'admin'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_ne!(admin_stored, admin_pre, "admin rotation persisted");
    let admin_after: mycelium_crypto::SealedMasterKey =
        serde_json::from_str(&admin_stored).unwrap();
    let by_rec = mycelium_crypto::unseal_with_recovery(&admin_after, &admin_key).unwrap();
    let by_pw = mycelium_crypto::unseal_with_password(&admin_after, ADMIN_PASSWORD).unwrap();
    assert_eq!(
        by_rec.as_bytes(),
        by_pw.as_bytes(),
        "the new key unseals the same master the password does"
    );
    // The WebAuthn resting row on this render (0 credentials).
    assert!(ah.contains("0 credentials"), "count renders: {ah}");

    // Leg 5a — malformed register body: the JSON extractor's rejection
    // maps to the endpoint-wide JSON error (fix-round hardening —
    // axum's default plain-text rejection would echo the serde error
    // string).
    let bad = client
        .post(format!("{base}/account/webauthn/register"))
        .header("cookie", &cookie_a)
        .header("x-csrf-token", csrf_a.as_str())
        .header("content-type", "application/json")
        .body("this is not json")
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let bj: serde_json::Value = bad.json().await.unwrap();
    assert_eq!(bj["error"], "invalid body", "JSON error shape: {bj}");

    // Leg 5b — a well-formed body with no live challenge fails closed
    // (this leg runs BEFORE any challenge GET so ChallengeNotFound is
    // the true failure reason).
    let reg = client
        .post(format!("{base}/account/webauthn/register"))
        .header("cookie", &cookie_a)
        .header("x-csrf-token", csrf_a.as_str())
        .header("content-type", "application/json")
        .body(r#"{"id":"x","rawId":"eA","response":{"attestationObject":"eA","clientDataJSON":"eA"},"type":"public-key"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(reg.status(), 400, "no challenge to consume → fail closed");
    let rj: serde_json::Value = reg.json().await.unwrap();
    assert!(rj["error"].is_string(), "error shape: {rj}");

    // Leg 6 — the challenge GET: session-gated creation-options JSON.
    let ch = client
        .get(format!("{base}/account/webauthn/challenge"))
        .header("cookie", &cookie_a)
        .send()
        .await
        .unwrap();
    assert_eq!(ch.status(), 200);
    let cj: serde_json::Value = ch.json().await.unwrap();
    let challenge = cj["publicKey"]["challenge"].as_str().unwrap_or("");
    assert!(!challenge.is_empty(), "challenge field: {cj}");
    assert_eq!(
        cj["publicKey"]["rp"]["id"], "localhost",
        "the default RP id derives from the default origin: {cj}"
    );
    assert_eq!(cj["publicKey"]["user"]["name"], "admin", "{cj}");
    // Anonymous: the uniform login gate (303 → /login) — the codebase's
    // real behavior for every protected page, not a 401.
    let anon = client
        .get(format!("{base}/account/webauthn/challenge"))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 303);
    assert_eq!(anon.headers().get("location").unwrap(), "/login");

    // Leg 7 — per-credential Remove rows + removal. Seed a credential
    // (list_credentials reads id/created_at only — the key blob is
    // never parsed on the render path).
    let (admin_id,): (String,) = sqlx::query_as("SELECT id FROM users WHERE username = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO webauthn_credentials (id, user_id, public_key, counter, transports, created_at)
         VALUES ('seeded-cred-1', ?, ?, 0, '', ?)",
    )
    .bind(&admin_id)
    .bind(b"seed" as &[u8])
    .bind(&now)
    .execute(&pool)
    .await
    .unwrap();
    let page = client
        .get(format!("{base}/account"))
        .header("cookie", &cookie_a)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let ph = page.text().await.unwrap();
    assert!(ph.contains("1 credential"), "count after seed: {ph}");
    assert!(
        ph.contains(r#"<span class="chip chip--accent">enrolled</span>"#),
        "enrolled chip: {ph}"
    );
    assert!(
        ph.contains("seeded-cred-1"),
        "the credential id renders: {ph}"
    );
    assert!(
        ph.contains(r#"<form method="post" action="/account/webauthn/remove">"#),
        "per-credential Remove form: {ph}"
    );
    let remove = client
        .post(format!("{base}/account/webauthn/remove"))
        .header("cookie", &cookie_a)
        .form(&[
            ("csrf_token", csrf_a.as_str()),
            ("credential_id", "seeded-cred-1"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(remove.status(), 200);
    let rh = remove.text().await.unwrap();
    assert!(rh.contains("0 credentials"), "count back to zero: {rh}");
    assert!(!rh.contains("seeded-cred-1"), "the row is gone: {rh}");
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM webauthn_credentials WHERE user_id = ?")
            .bind(&admin_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "remove_credential deleted the row");
    // Removing an unknown id: the page re-renders (the user-scoped
    // delete matched nothing — a no-op, not an error page).
    let remove2 = client
        .post(format!("{base}/account/webauthn/remove"))
        .header("cookie", &cookie_a)
        .form(&[
            ("csrf_token", csrf_a.as_str()),
            ("credential_id", "does-not-exist"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(remove2.status(), 200);

    shutdown.cancel();
}

/// Mint the current TOTP code for a secret using mycelium-auth's exact
/// verification parameters (SHA1 / 6 digits / skew 1 / 30s step —
/// totp.rs's make_totp). Mirrors how login.rs's tests mint codes.
fn totp_code(secret: &str) -> String {
    use totp_rs::{Algorithm, Builder, Secret};
    let s = Secret::try_from_base32(secret).unwrap();
    let totp = Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(6)
        .with_skew(1)
        .with_step_duration(30)
        .with_secret(s)
        .with_account_name("verify")
        .with_issuer(Some("Mycelium2"))
        .build()
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    totp.generate(now).to_string()
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

/// The seven gate metric names with their Prometheus types (spec §3.6).
/// The flag marks the integral samples — `wait_seconds_total` is the
/// one float.
const GATE_METRICS: [(&str, &str, bool); 7] = [
    ("mycelium2_llm_gate_active", "gauge", true),
    ("mycelium2_llm_gate_queued", "gauge", true),
    ("mycelium2_llm_gate_capacity", "gauge", true),
    ("mycelium2_llm_gate_admissions_total", "counter", true),
    ("mycelium2_llm_gate_timeouts_total", "counter", true),
    ("mycelium2_llm_gate_wait_seconds_total", "counter", false),
    ("mycelium2_llm_gate_wait_count", "counter", true),
];

/// The value of a Prometheus sample line (the first line that starts
/// with `<name> `), so a `# TYPE` line is never mistaken for a sample.
fn metric_sample(text: &str, name: &str) -> String {
    let prefix = format!("{name} ");
    let line = text
        .lines()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("/metrics has no sample line for {name}:\n{text}"));
    line[prefix.len()..].trim().to_string()
}

/// The PUBLIC /health JSON carries the three LLM-gate fields beside the
/// write-queue ones (spec §3.6; `/api/v1/health` deliberately stays
/// unchanged). Presence and types only: the gate is process-global, so
/// nothing absolute is pinned here and no slot is held — the live-state
/// assertion lives in chat_sessions_integration.rs, under that binary's
/// gate lock.
#[tokio::test]
async fn health_reports_llm_gate_fields() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();

    let health = client.get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(health.status(), 200, "a healthy boot is not degraded");
    let body: serde_json::Value = health.json().await.unwrap();
    for field in ["llm_active", "llm_queued", "llm_capacity"] {
        let v = body
            .get(field)
            .unwrap_or_else(|| panic!("/health is missing {field}: {body}"));
        assert!(
            v.is_i64() || v.is_u64(),
            "{field} must be an integer, got {v}"
        );
        assert!(
            v.as_i64().unwrap() >= 0,
            "{field} must be non-negative, got {v}"
        );
    }
    assert!(
        body["llm_capacity"].as_i64().unwrap() >= 1,
        "the gate always has at least one slot: {body}"
    );
    // The gate never degrades health on its own (spec §3.6).
    assert_eq!(body["status"], "ok", "gate fields do not change status");

    // The detail endpoint keeps its old shape.
    let detail: serde_json::Value = client
        .get(format!("{base}/api/v1/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for field in ["llm_active", "llm_queued", "llm_capacity"] {
        assert!(
            detail.get(field).is_none(),
            "/api/v1/health must stay unchanged, found {field}: {detail}"
        );
    }

    shutdown.cancel();
}

/// /metrics carries the gate gauges and counters in the same
/// `# TYPE`-then-sample shape the mutation-queue metrics use.
#[tokio::test]
async fn metrics_reports_llm_gate_lines() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();

    let metrics = client.get(format!("{base}/metrics")).send().await.unwrap();
    assert_eq!(metrics.status(), 200);
    let text = metrics.text().await.unwrap();

    for (name, kind, integral) in GATE_METRICS {
        assert!(
            text.contains(&format!("# TYPE {name} {kind}\n")),
            "/metrics lacks the `# TYPE {name} {kind}` line:\n{text}"
        );
        let value = metric_sample(&text, name);
        if integral {
            assert!(
                value.parse::<u64>().is_ok(),
                "{name} sample {value:?} is not a non-negative integer"
            );
        } else {
            let f: f64 = value
                .parse()
                .unwrap_or_else(|e| panic!("{name} sample {value:?} is not a number: {e}"));
            assert!(f >= 0.0, "{name} must be non-negative, got {f}");
        }
    }
    assert!(
        metric_sample(&text, "mycelium2_llm_gate_capacity")
            .parse::<u64>()
            .unwrap()
            >= 1,
        "the gate always has at least one slot"
    );

    shutdown.cancel();
}

// ---------------------------------------------------------------------
// Backend health + chat waiting surfaces (LLM outage recovery spec
// §3.5). The two pins below FORCE the process-global `LLM_HEALTH`
// machine, so they serialize on `HEALTH_TEST_LOCK` and restore a fresh
// Green through `HealthDefaults` — declared after the lock guard so the
// restore runs while the lock is still held (reverse drop order). This
// mirrors the `TEST_LOCK` + `HealthDefaults` pattern
// chat_sessions_integration.rs / chat_resume_integration.rs use for the
// same global; `llm_health::health_test_guard()` itself is
// `#[cfg(test)] pub(crate)`, so it exists only inside the librarian's
// own unit tests and cannot link into an integration binary.
//
// FEEDERS on this lock: the inline-ingest uploads in
// books_count_and_passage_reader and
// admin_library_escapes_and_pairs_shelf_names post /api/v1/ingest,
// which runs the catalog through the LlmClient against the default
// endpoint (no `llm` row at boot) — the llm.rs passive hooks note this
// global at the REAL clock on every upload (refused = failure note;
// on an Ollama-running host, success = ok note + LLM_LAST_SUCCESS).
// Those four uploads each hold the lock across request+response. Any
// new LLM-driving surface added to this binary must take the lock the
// same way (llm_health.rs's guard rule).
//
// READERS that do NOT take the lock: /health and /metrics on every
// request snapshot the global server-side via system_stats, and the
// /status + /admin/system page tests read it the same way — harmless
// TODAY only because nothing those pages render consumes the backend_*
// fields yet (the /status chips key off LLM_LAST_SUCCESS and the gate).
// If Task 10's admin page starts rendering backend health, its tests
// must join this lock.
// ---------------------------------------------------------------------

use mycelium_librarian::llm_health::{BackendState, LLM_HEALTH};

/// Serializes every test whose leg forces the process-global
/// backend-health state OR drives a passive health note (the inline
/// ingest uploads) — mirrors the chat integration binaries' TEST_LOCK
/// role for this global. See the section comment above.
static HEALTH_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Restores the process-global health registry to a fresh Green on
/// drop, so a failed assertion cannot leak a forced Red/Disabled into
/// a sibling test sharing this process.
struct HealthDefaults;

impl Drop for HealthDefaults {
    fn drop(&mut self) {
        LLM_HEALTH.reset_for_test();
    }
}

/// Force the global machine to Red with the outage opened `ago`
/// seconds back: three failures stamped at `now - ago` (Red threshold
/// 3; the first failure sets `down_since`, so the outage age pins to
/// ~`ago`). Callers hold `HEALTH_TEST_LOCK`.
fn force_red(ago: u64) {
    LLM_HEALTH.reset_for_test();
    let since = chrono::Utc::now().timestamp().max(ago as i64) as u64 - ago;
    for _ in 0..3 {
        LLM_HEALTH.note_failure_at(since);
    }
    assert_eq!(
        LLM_HEALTH.state(),
        BackendState::Red,
        "Red before the request"
    );
}

/// Seed ONE `waiting_backend` turn row directly against the test pool
/// (the same direct-SQL style the /status test uses, satisfying the FK
/// chain session → seeded admin user). The row is inert here: `boot()`
/// serves but starts no resume worker (`main.rs` owns that spawn), so
/// nothing claims the row while the request runs.
async fn seed_waiting_turn(pool: &sqlx::SqlitePool) {
    let now = chrono::Utc::now().to_rfc3339();
    let user_id: String = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
        .fetch_one(pool)
        .await
        .unwrap();
    let session_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO chat_sessions (id, user_id, title_sealed, created_at, updated_at)
         VALUES (?, ?, 'sealed-by-nothing', ?, ?)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO chat_turns (id, session_id, user_seq, status, attempts,
                                 created_at, updated_at, park_state, last_error)
         VALUES (?, ?, 1, 'waiting_backend', 0, ?, ?, 'waiting_backend', NULL)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&session_id)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
}

/// The PUBLIC /health JSON carries the four outage-recovery fields
/// (spec §3.5) beside the gate and queue ones: under a forced Red with
/// a seeded waiting turn, `backend_state` is "red", `backend_down_seconds`
/// is the outage age, `backend_checked_seconds_ago` is present, and
/// `chat_waiting_count` counts the parked turn. Disabled reports
/// `backend_state` "disabled" with a NULL `backend_down_seconds` —
/// nothing-to-probe is not an outage. The degraded rule is queue/db
/// only: an EXPLICIT non-regression pin below proves a Red backend
/// never flips /health to degraded.
#[tokio::test]
async fn health_reports_backend_and_waiting_fields() {
    let _g = HEALTH_TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let (base, shutdown, _dir, pool) = boot().await;
    let client = client();

    seed_waiting_turn(&pool).await;
    force_red(3600);

    let health = client.get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(
        health.status(),
        200,
        "non-regression: a Red backend must NOT degrade /health"
    );
    let body: serde_json::Value = health.json().await.unwrap();
    assert_eq!(
        body["status"], "ok",
        "non-regression: the degraded rule never consults backend health: {body}"
    );
    assert_eq!(body["backend_state"], "red", "{body}");
    let down = body["backend_down_seconds"]
        .as_i64()
        .unwrap_or_else(|| panic!("backend_down_seconds must be a number: {body}"));
    assert!(
        (3600..=3605).contains(&down),
        "backend_down_seconds should be ~3600 (outage forced 3600 s back), got {down}: {body}"
    );
    let checked = body["backend_checked_seconds_ago"]
        .as_i64()
        .unwrap_or_else(|| panic!("backend_checked_seconds_ago must be a number: {body}"));
    assert!(
        (3600..=3605).contains(&checked),
        "backend_checked_seconds_ago should be ~3600 (notes stamped at now-3600), got {checked}: {body}"
    );
    assert_eq!(
        body["chat_waiting_count"].as_i64(),
        Some(1),
        "the seeded waiting turn must be counted: {body}"
    );

    // Disabled: nothing to probe is NOT an outage — the state name and
    // a NULL outage age, still "ok".
    LLM_HEALTH.set_disabled();
    let body: serde_json::Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["status"], "ok", "Disabled never degrades: {body}");
    assert_eq!(body["backend_state"], "disabled", "{body}");
    assert!(
        body["backend_down_seconds"].is_null(),
        "a stale down_since under Disabled must read as not-down: {body}"
    );
    assert_eq!(
        body["chat_waiting_count"].as_i64(),
        Some(1),
        "the waiting count is not health-gated: {body}"
    );

    shutdown.cancel();
}

/// /metrics carries the three outage-recovery lines in the same
/// `# TYPE`-then-sample shape as the rest: `mycelium2_backend_up`
/// (0=disabled, 1=red, 2=yellow, 3=green), `mycelium2_backend_down_seconds`
/// (0 when not down), and `mycelium2_chat_waiting_turns`. Values pinned
/// across green → red → disabled, plus the model-call-only rule: with
/// no LLM completion in this process (the probe never feeds
/// `LLM_LAST_SUCCESS`), `mycelium2_llm_last_success_timestamp` stays
/// absent.
#[tokio::test]
async fn metrics_reports_backend_health_lines() {
    let _g = HEALTH_TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let (base, shutdown, _dir, pool) = boot().await;
    let client = client();

    for (name, kind) in [
        ("mycelium2_backend_up", "gauge"),
        ("mycelium2_backend_down_seconds", "gauge"),
        ("mycelium2_chat_waiting_turns", "gauge"),
    ] {
        assert!(
            client
                .get(format!("{base}/metrics"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
                .contains(&format!("# TYPE {name} {kind}\n")),
            "/metrics lacks the `# TYPE {name} {kind}` line"
        );
    }

    // Green (idle process: no runs queued, so the staleness rule
    // cannot fire) → up 3, outage age 0.
    LLM_HEALTH.reset_for_test();
    let text = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        metric_sample(&text, "mycelium2_backend_up"),
        "3",
        "green maps to 3:\n{text}"
    );
    assert_eq!(
        metric_sample(&text, "mycelium2_backend_down_seconds"),
        "0",
        "not down reads 0:\n{text}"
    );
    assert_eq!(
        metric_sample(&text, "mycelium2_chat_waiting_turns"),
        "0",
        "no waiting turns seeded yet:\n{text}"
    );

    // Red + one seeded waiting turn → up 1, age ~3600, count 1.
    seed_waiting_turn(&pool).await;
    force_red(3600);
    let text = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        metric_sample(&text, "mycelium2_backend_up"),
        "1",
        "red maps to 1:\n{text}"
    );
    let down: u64 = metric_sample(&text, "mycelium2_backend_down_seconds")
        .parse()
        .unwrap();
    assert!(
        (3600..=3605).contains(&down),
        "backend_down_seconds should be ~3600, got {down}:\n{text}"
    );
    assert_eq!(
        metric_sample(&text, "mycelium2_chat_waiting_turns"),
        "1",
        "the seeded waiting turn must be exported:\n{text}"
    );

    // Disabled → up 0 and 0 again (not down), even with a stale
    // down_since still stored.
    LLM_HEALTH.set_disabled();
    let text = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        metric_sample(&text, "mycelium2_backend_up"),
        "0",
        "disabled maps to 0:\n{text}"
    );
    assert_eq!(
        metric_sample(&text, "mycelium2_backend_down_seconds"),
        "0",
        "disabled is not down:\n{text}"
    );

    // The probe never feeds LLM_LAST_SUCCESS, and no LLM completion
    // ran in this process → the model-call-only gauge stays absent.
    assert!(
        !text.contains("mycelium2_llm_last_success_timestamp"),
        "llm_last_success_timestamp is model-call-only and no call ran here:\n{text}"
    );

    shutdown.cancel();
}

// ---------------------------------------------------------------------
// Task 10: the admin System page renders the backend indicator, the
// waiting summary, and the per-item waiting table. The indicator test
// asserts backend_* content, so — per the section rule above — it holds
// HEALTH_TEST_LOCK with HealthDefaults declared after the guard. The
// summary/meta test asserts no backend content, so it may read the page
// unlocked even though admin_system_view snapshots the global
// server-side.
// ---------------------------------------------------------------------

/// Direct-SQL user row (no UserStore validation) — needed for the
/// `<script>` username probe, which `create_local` would reject.
async fn seed_user_named(pool: &sqlx::SqlitePool, name: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO users (id, username, email, role, auth_provider, password_hash,
                            sealed_master_key, must_change_password, totp_secret, created_at, updated_at)
         VALUES (?, ?, ?, 'user', 'local', NULL, 'placeholder-sealed-master-key', 0, NULL, ?, ?)",
    )
    .bind(&id)
    .bind(name)
    .bind(format!("{name}@t.local"))
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// A `waiting_backend` turn for `user_id`, aged `age_secs`, with an
/// optional `last_error`; seeds a transcript row whose sealed payload
/// carries `TRANSCRIPT-SECRET` so the test can pin that transcript
/// content NEVER reaches the admin page. Returns the session id.
async fn seed_turn_for(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    age_secs: i64,
    last_error: Option<&str>,
) -> String {
    let now = chrono::Utc::now();
    let aged = (now - chrono::Duration::seconds(age_secs)).to_rfc3339();
    let session_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO chat_sessions (id, user_id, title_sealed, created_at, updated_at)
         VALUES (?, ?, 'sealed-by-nothing', ?, ?)",
    )
    .bind(&session_id)
    .bind(user_id)
    .bind(&aged)
    .bind(&aged)
    .execute(pool)
    .await
    .unwrap();
    let turn_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO chat_turns (id, session_id, user_seq, status, attempts,
                                 created_at, updated_at, park_state, last_error)
         VALUES (?, ?, 1, 'waiting_backend', 0, ?, ?, 'waiting_backend', ?)",
    )
    .bind(&turn_id)
    .bind(&session_id)
    .bind(&aged)
    .bind(&aged)
    .bind(last_error)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO chat_messages (session_id, seq, role, content_sealed, created_at)
         VALUES (?, 1, 'user', 'TRANSCRIPT-SECRET', ?)",
    )
    .bind(&session_id)
    .bind(&aged)
    .execute(pool)
    .await
    .unwrap();
    session_id
}

/// A pending mutation-queue row (metadata only — no payload) for
/// `user_id`, aged `age_secs`.
async fn seed_pending_queue_row(pool: &sqlx::SqlitePool, user_id: &str, age_secs: i64) {
    let aged = (chrono::Utc::now() - chrono::Duration::seconds(age_secs)).to_rfc3339();
    sqlx::query(
        "INSERT INTO mutation_queue (id, user_id, tool, status, attempts, detail,
                                     created_at, updated_at, next_retry_at, final_paths)
         VALUES (?, ?, 'add', 'pending', 0, '', ?, ?, NULL, NULL)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(user_id)
    .bind(&aged)
    .bind(&aged)
    .execute(pool)
    .await
    .unwrap();
}

/// A pending ingest job on a fresh shelf + book (FK chain like the
/// /status test's seed).
async fn seed_pending_ingest(pool: &sqlx::SqlitePool, user_id: &str, slug: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    let shelf_id = uuid::Uuid::new_v4().to_string();
    let book_id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO bookshelves (id, name, created_at) VALUES (?, ?, ?)")
        .bind(&shelf_id)
        .bind(format!("shelf-{slug}"))
        .bind(&now)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO books (id, bookshelf_id, slug, title, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&book_id)
    .bind(&shelf_id)
    .bind(slug)
    .bind(format!("Book {slug}"))
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO ingest_jobs (id, bookshelf_id, book_id, requested_by_user_id, status, detail, created_at, updated_at)
         VALUES (?, ?, ?, ?, 'pending', '', ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&shelf_id)
    .bind(&book_id)
    .bind(user_id)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
}

/// Extract the waiting-summary sentence (the muted paragraph ending
/// in the resume promise) from a rendered /admin/system page.
fn summary_sentence(html: &str) -> &str {
    let tail = " waiting — they will resume automatically when the backend returns.";
    let idx = html.find(tail).expect("summary sentence present");
    let open = r#"<p class="muted">"#;
    let start = html[..idx].rfind(open).expect("summary paragraph opens") + open.len();
    &html[start..idx]
}

/// The /admin/system page under a forced Red with waiting work: the
/// indicator carries the state's dot class, an `aria-label`, and a
/// visible state word (not color alone); the summary line reads
/// exactly as pinned when both kinds are non-zero; the waiting table
/// shows metadata (username, kind, waited time, last_error) for the
/// seeded turn, queue row, and pending ingest job — HTML-escaped, with
/// NO transcript content; and the page self-refreshes every 30 s,
/// which `/admin/llm` must not do. Also pins Disabled (dot class +
/// no outage age) on the same forced clock.
#[tokio::test]
async fn admin_system_backend_indicator_and_waiting_table() {
    let _g = HEALTH_TEST_LOCK.lock().await;
    let _health = HealthDefaults;
    let (base, shutdown, _dir, pool) = boot().await;
    let client = client();
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", ADMIN_PASSWORD)])
        .send()
        .await
        .unwrap();
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

    // Hostile owner for one chat turn, the queue rows, and the ingest
    // job; the admin owns the other two turns.
    let hostile = seed_user_named(&pool, "<script>alert(1)</script>").await;
    let admin_id: String = sqlx::query_scalar("SELECT id FROM users WHERE username = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    seed_turn_for(&pool, &admin_id, 7200, Some("LLM refused the connection")).await;
    seed_turn_for(&pool, &admin_id, 60, None).await;
    seed_turn_for(&pool, &hostile, 60, None).await;
    seed_pending_queue_row(&pool, &hostile, 60).await;
    seed_pending_queue_row(&pool, &hostile, 30).await;
    seed_pending_ingest(&pool, &hostile, "waiting-book").await;

    force_red(3600);
    let html = client
        .get(format!("{base}/admin/system"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // 1. Indicator: dot class per state + aria-label + visible word.
    assert!(
        html.contains(r#"health-dot health-dot--red"#),
        "red health dot missing: {html}"
    );
    assert!(
        html.contains(r#"aria-label="Backend health: red""#),
        "indicator aria-label missing: {html}"
    );
    assert!(
        html.contains("Backend red"),
        "visible state word missing: {html}"
    );
    // Outage age ~3600 s: "1h 0m", or "1h 1m" if a second boundary
    // slipped between forcing and the request (the same ~5 s slack
    // the /health pins tolerate).
    assert!(
        html.contains("down for 1h 0m") || html.contains("down for 1h 1m"),
        "outage age (~3600 s) missing: {html}"
    );
    assert!(
        html.contains("last checked"),
        "last-check age missing: {html}"
    );

    // 2. Summary line — exact, both kinds non-zero (3 turns + 2 writes).
    assert!(
        html.contains(
            "3 chat turns and 2 queued writes waiting — they will resume automatically when the backend returns."
        ),
        "exact mixed summary missing: {html}"
    );

    // 3. Waiting table — metadata cells only, escaped, no transcript.
    assert!(
        html.contains(r#"<td data-label="User">&lt;script&gt;alert(1)&lt;/script&gt;</td>"#),
        "hostile username must render escaped: {html}"
    );
    assert!(
        !html.contains("<script>alert(1)</script>"),
        "raw script probe leaked: {html}"
    );
    assert!(
        html.contains(r#"<td data-label="Kind">chat turn</td>"#),
        "chat-turn kind cell missing: {html}"
    );
    assert!(
        html.contains(r#"<td data-label="Kind">queued write</td>"#),
        "queued-write kind cell missing: {html}"
    );
    assert!(
        html.contains(r#"<td data-label="Kind">ingest job</td>"#),
        "ingest-job kind cell missing: {html}"
    );
    assert!(
        html.contains("waiting-book"),
        "ingest slug cell missing: {html}"
    );
    assert!(
        html.contains(r#"<td data-label="State">waiting_backend</td>"#),
        "turn state cell missing: {html}"
    );
    assert!(
        html.contains(r#"<td data-label="Waited">2h 0m</td>"#),
        "7200 s turn should read 2h 0m: {html}"
    );
    assert!(
        html.contains("LLM refused the connection"),
        "last_error text missing: {html}"
    );
    assert!(
        !html.contains("TRANSCRIPT-SECRET"),
        "transcript content must never reach the admin page: {html}"
    );

    // 4. 30 s meta refresh on /admin/system ONLY.
    assert!(
        html.contains(r#"<meta http-equiv="refresh" content="30">"#),
        "30 s refresh meta missing on /admin/system: {html}"
    );
    let llm_html = client
        .get(format!("{base}/admin/llm"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        llm_html.contains("LLM backend"),
        "sanity: /admin/llm renders"
    );
    assert!(
        !llm_html.contains(r#"<meta http-equiv="refresh""#),
        "refresh meta must NOT appear on /admin/llm: {llm_html}"
    );

    // 5. Disabled: the dot + word follow the state, and a frozen
    //    down_since reads as NOT an outage (no "down for" clause).
    LLM_HEALTH.set_disabled();
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
        html.contains(r#"health-dot health-dot--disabled"#),
        "disabled dot missing: {html}"
    );
    assert!(
        html.contains("Backend disabled"),
        "disabled state word missing: {html}"
    );
    assert!(
        !html.contains("down for"),
        "Disabled is not an outage: {html}"
    );

    shutdown.cancel();
}

/// The summary line drops a part when its kind's count is zero:
/// chat-only, then mixed (exact when both), then queue-only, then
/// nothing at all (no sentence). Asserts no backend content, so it
/// runs unlocked (the page's server-side snapshot read is harmless —
/// the summary and table are health-independent).
#[tokio::test]
async fn admin_system_summary_drops_zero_parts() {
    let (base, shutdown, _dir, pool) = boot().await;
    let client = client();
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", ADMIN_PASSWORD)])
        .send()
        .await
        .unwrap();
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
    let admin_id: String = sqlx::query_scalar("SELECT id FROM users WHERE username = 'admin'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let get_system = || {
        let client = client.clone();
        let base = base.clone();
        let cookie = cookie.clone();
        async move {
            client
                .get(format!("{base}/admin/system"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        }
    };

    // Chat-only: the queued-writes part is dropped. (The LLM card's
    // helper text mentions "chat turns" too, so the part-drop checks
    // scope to the summary sentence itself, never the whole page.)
    seed_turn_for(&pool, &admin_id, 60, None).await;
    let html = get_system().await;
    assert_eq!(
        summary_sentence(&html),
        "1 chat turn",
        "chat-only summary wrong: {html}"
    );

    // Mixed: exact two-part sentence (singular chat part).
    seed_pending_queue_row(&pool, &admin_id, 60).await;
    seed_pending_queue_row(&pool, &admin_id, 30).await;
    let html = get_system().await;
    assert_eq!(
        summary_sentence(&html),
        "1 chat turn and 2 queued writes",
        "mixed summary wrong: {html}"
    );

    // Queue-only: the chat part drops once the turn is terminal.
    sqlx::query("UPDATE chat_turns SET status = 'done'")
        .execute(&pool)
        .await
        .unwrap();
    let html = get_system().await;
    assert_eq!(
        summary_sentence(&html),
        "2 queued writes",
        "queue-only summary wrong: {html}"
    );

    // Nothing waiting: no sentence at all.
    sqlx::query("UPDATE mutation_queue SET status = 'done'")
        .execute(&pool)
        .await
        .unwrap();
    let html = get_system().await;
    assert!(
        !html.contains("they will resume automatically when the backend returns"),
        "no waiting items means no summary sentence: {html}"
    );

    shutdown.cancel();
}

/// Regression: a browser that still holds a live session cookie (bookmark,
/// back button, restart) must be able to submit /login again. The login
/// page has no session token to render, so app.js injects an empty
/// `csrf_token`; the CSRF gate used to exempt /login only when NO session
/// was present, answering 403 "CSRF token mismatch" otherwise.
#[tokio::test]
async fn login_post_with_live_session_cookie_succeeds() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();

    let first = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", ADMIN_PASSWORD)])
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 303);
    let cookie = first
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // Second submit from the same browser: live cookie + app.js's empty field.
    let again = client
        .post(format!("{base}/login"))
        .header("cookie", &cookie)
        .form(&[
            ("username", "admin"),
            ("password", ADMIN_PASSWORD),
            ("csrf_token", ""),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(
        again.status(),
        303,
        "login must not be CSRF-gated by an existing session"
    );
    assert_eq!(again.headers().get("location").unwrap(), "/");

    shutdown.cancel();
}

/// REST PUT /api/v1/concepts (api.rs `api_put_concept`): a write-time
/// lifecycle reject (`Frontmatter::validated` surfacing as a
/// `ConceptStoreError::Parse`) is the client's bad input, not a server
/// failure — 400 with the concept error text (the same message the
/// form path's warning banner carries), while a put that fails for a
/// NON-lifecycle reason keeps the generic all-store-errors 500 posture.
#[tokio::test]
async fn rest_put_lifecycle_reject_is_400_and_non_lifecycle_stays_500() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();

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

    // Bad `status` value: 400, and the body carries the rejected value.
    let bad_status = client
        .put(format!("{base}/api/v1/concepts/notes/lifecycle-bad.md"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .json(&serde_json::json!({
            "markdown": "---\ntype: Note\ntitle: Bad Status\nstatus: archive\n---\n\nrest body"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        bad_status.status(),
        400,
        "a lifecycle reject is a client input error, not internal"
    );
    let body: serde_json::Value = bad_status.json().await.unwrap();
    let err_text = body["error"].as_str().expect("JSON error body");
    assert!(
        err_text.contains("archive"),
        "the 400 body must mention the bad status value: {err_text}"
    );

    // Over-matching guard: a put that fails for a NON-lifecycle reason —
    // a reserved filename, the only input-dependent non-lifecycle put
    // reject — keeps the generic 500 posture.
    let reserved = client
        .put(format!("{base}/api/v1/concepts/index.md"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .json(&serde_json::json!({
            "markdown": "---\ntype: Note\ntitle: Reserved\n---\n\nrest body"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        reserved.status(),
        500,
        "non-lifecycle store errors stay 500"
    );

    shutdown.cancel();
}

// ---------- Revisions REST (Task 3: index, list, snapshot, restore) ----------
//
// The four routes are query-param shaped (spec §3.2a-2). These tests pin
// the happy paths (JSON + form restore variants), the dead-path behavior,
// the 403 matrix, the 400/404 matrix, and the DELETE-missing-concept 404
// rider. Helpers below mirror the inline login/user-creation blocks the
// file already repeats.

/// Login as the pre-seeded admin; returns (session cookie, csrf token).
async fn login_admin(client: &reqwest::Client, base: &str) -> (String, String) {
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
    let csrf = csrf_from_page(client, &format!("{base}/"), &cookie).await;
    (cookie, csrf)
}

/// Create a regular user through the real admin form, log them in;
/// returns (session cookie, csrf token).
async fn create_and_login_user(
    client: &reqwest::Client,
    base: &str,
    admin_cookie: &str,
    admin_csrf: &str,
    name: &str,
) -> (String, String) {
    let create = client
        .post(format!("{base}/admin/users"))
        .header("cookie", admin_cookie)
        .header("x-csrf-token", admin_csrf)
        .form(&[
            ("username", name),
            ("email", &format!("{name}@example.com")),
            ("password", "a long enough password here 22"),
            ("role", "user"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 200, "test user creation must succeed");
    let login = client
        .post(format!("{base}/login"))
        .form(&[
            ("username", name),
            ("password", "a long enough password here 22"),
        ])
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
    let csrf = csrf_from_page(client, &format!("{base}/"), &cookie).await;
    (cookie, csrf)
}

/// PUT a concept into the logged-in user's private bundle via the REST
/// route; asserts the 201.
async fn rest_put(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
    path: &str,
    markdown: &str,
) {
    let r = client
        .put(format!("{base}/api/v1/concepts{}", path))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .json(&serde_json::json!({ "markdown": markdown }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "rest put {path} must succeed");
}

/// GET the user-scope concept markdown via the REST route (asserts 200,
/// returns the raw text/markdown body).
async fn rest_get_markdown(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    path: &str,
) -> String {
    let r = client
        .get(format!("{base}/api/v1/concepts{}", path))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "rest get {path} must succeed");
    r.text().await.unwrap()
}

/// Restore happy paths (spec §3.2a-2): both the JSON and the native-form
/// variants restore the recorded content, the index/list shapes and DESC
/// order hold, and the form variant is CSRF-gated. The payload
/// convention (§3.2a-3) is pinned too: `create` rows carry the created
/// content; every later row carries the PRE-mutation snapshot (rev 2 =
/// the v1 state, rev 3 = the v2 state), so restoring to revision N puts
/// the concept back at the content that row recorded.
#[tokio::test]
async fn revisions_rest_round_trip_json_and_form() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // Three recorded versions of one concept.
    rest_put(
        &client,
        &base,
        &cookie,
        &csrf,
        "/rv.md",
        "---\ntype: Note\ntitle: Rv One\n---\n\nfirst body",
    )
    .await;
    rest_put(
        &client,
        &base,
        &cookie,
        &csrf,
        "/rv.md",
        "---\ntype: Note\ntitle: Rv Two\n---\n\nsecond body",
    )
    .await;
    rest_put(
        &client,
        &base,
        &cookie,
        &csrf,
        "/rv.md",
        "---\ntype: Note\ntitle: Rv Three\n---\n\nthird body",
    )
    .await;

    // Index: one path with the latest revision at the head of its chain.
    let idx = client
        .get(format!("{base}/api/v1/revisions?scope=user"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(idx.status(), 200, "revisions index must answer");
    let idx_json: serde_json::Value = idx.json().await.unwrap();
    let entries = idx_json.as_array().expect("index is a JSON array");
    let entry = entries
        .iter()
        .find(|e| e["path"] == "/rv.md")
        .expect("the concept's path appears in the index");
    assert_eq!(
        entry["live"], true,
        "a live concept's index entry is live: {entry}"
    );
    assert_eq!(entry["latest_no"], 3, "three puts: {entry}");
    assert_eq!(entry["last_op"], "update", "index entry: {entry}");
    assert!(
        entry["last_op_at"].as_str().is_some_and(|t| !t.is_empty()),
        "last_op_at present: {entry}"
    );
    // No payload text anywhere in the index (metadata only).
    let idx_text = serde_json::to_string(&idx_json).unwrap();
    assert!(
        !idx_text.contains("first body") && !idx_text.contains("second body"),
        "the index must not carry payload text: {idx_text}"
    );

    // List: DESC order, one row per revision, provenance threaded.
    let list = client
        .get(format!(
            "{base}/api/v1/revisions/list?scope=user&path=/rv.md"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200, "revisions list must answer");
    let list_json: serde_json::Value = list.json().await.unwrap();
    let rows = list_json.as_array().expect("list is a JSON array");
    assert_eq!(rows.len(), 3, "three revisions listed: {list_json}");
    assert_eq!(rows[0]["revision_no"], 3, "newest first: {list_json}");
    assert_eq!(
        rows[0]["op"], "update",
        "newest is the third put: {list_json}"
    );
    assert_eq!(rows[1]["revision_no"], 2);
    assert_eq!(rows[1]["op"], "update");
    assert_eq!(rows[2]["revision_no"], 1);
    assert_eq!(rows[2]["op"], "create");
    for row in rows {
        assert_eq!(
            row["source"], "web:admin",
            "the REST put's provenance stamp lands in the row: {row}"
        );
        assert!(
            row["created_at"].as_str().is_some_and(|t| !t.is_empty()),
            "created_at present: {row}"
        );
        assert!(
            row["summary"].as_str().is_some_and(|t| !t.is_empty()),
            "summary present: {row}"
        );
    }
    // No payload text in the list either.
    let list_text = serde_json::to_string(&list_json).unwrap();
    assert!(
        !list_text.contains("first body") && !list_text.contains("second body"),
        "the list must not carry payload text: {list_text}"
    );

    // Snapshot n=1: the created content (create rows carry it).
    let snap = client
        .get(format!(
            "{base}/api/v1/revisions/snapshot?scope=user&path=/rv.md&n=1"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(snap.status(), 200, "snapshot n=1 must answer");
    assert!(
        snap.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/markdown")),
        "snapshot is raw markdown"
    );
    let snap_one = snap.text().await.unwrap();
    assert!(
        snap_one.contains("Rv One") && snap_one.contains("first body"),
        "snapshot n=1 is the created v1: {snap_one}"
    );
    assert!(!snap_one.contains("Rv Two"), "n=1 must not show v2");

    // Snapshot n=2: the update row's PRE-mutation snapshot — the
    // recorded v1 state, byte-for-byte the same bytes the create row
    // recorded (both are the concept as it stood after put one).
    let snap2 = client
        .get(format!(
            "{base}/api/v1/revisions/snapshot?scope=user&path=/rv.md&n=2"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let snap_two = snap2.text().await.unwrap();
    assert_eq!(
        snap_one, snap_two,
        "rev 2 records the pre-update state of put two"
    );

    // Snapshot n=3: the third put's pre-state — the v2 content, the
    // distinct second recorded state.
    let snap3 = client
        .get(format!(
            "{base}/api/v1/revisions/snapshot?scope=user&path=/rv.md&n=3"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let snap_three = snap3.text().await.unwrap();
    assert!(
        snap_three.contains("Rv Two") && snap_three.contains("second body"),
        "snapshot n=3 is the pre-put state: {snap_three}"
    );
    assert!(!snap_three.contains("Rv Three"), "n=3 is the v2 state");

    // JSON restore: application/json POST answers {"restored_to": n}
    // and the live concept is exactly the row's recorded content (rev
    // 3's snapshot: the v2 state).
    let restore = client
        .post(format!(
            "{base}/api/v1/revisions/restore?scope=user&path=/rv.md&n=3"
        ))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(restore.status(), 200, "JSON restore must answer 200");
    let restored: serde_json::Value = restore.json().await.unwrap();
    assert_eq!(restored["restored_to"], 3, "restore body: {restored}");
    let back = rest_get_markdown(&client, &base, &cookie, "/rv.md").await;
    assert!(
        back.contains("Rv Two") && back.contains("second body") && !back.contains("Rv Three"),
        "the live concept is rev 3's recorded content: {back}"
    );

    // The restore itself is a recorded revision (reversible undo chain).
    let list = client
        .get(format!(
            "{base}/api/v1/revisions/list?scope=user&path=/rv.md"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let list_json: serde_json::Value = list.json().await.unwrap();
    let rows = list_json.as_array().unwrap();
    assert_eq!(rows.len(), 4, "restore appended a revision: {list_json}");
    assert_eq!(rows[0]["revision_no"], 4);
    assert_eq!(rows[0]["op"], "restore");
    assert_eq!(rows[0]["source"], "web:admin");
    assert!(
        rows[0]["summary"]
            .as_str()
            .is_some_and(|s| s.contains("restored to revision 3")),
        "restore summary: {list_json}"
    );
    assert_eq!(rows[1]["revision_no"], 3);
    assert_eq!(rows[1]["op"], "update");
    assert_eq!(rows[2]["revision_no"], 2);
    assert_eq!(rows[2]["op"], "update");
    assert_eq!(rows[3]["revision_no"], 1);
    assert_eq!(rows[3]["op"], "create");

    // Updated index: the restore at the head, still live.
    let idx = client
        .get(format!("{base}/api/v1/revisions?scope=user"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let idx_json: serde_json::Value = idx.json().await.unwrap();
    let entry = idx_json
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "/rv.md")
        .unwrap();
    assert_eq!(entry["latest_no"], 4, "index entry: {entry}");
    assert_eq!(entry["last_op"], "restore", "index entry: {entry}");

    // Form restore (the browser path): 303 redirect to the concept
    // viewer; the csrf_token rides as a form field, params in the query.
    // Restoring to rev 2 puts back ITS recorded content (the v1 state).
    let restore2 = client
        .post(format!(
            "{base}/api/v1/revisions/restore?scope=user&path=/rv.md&n=2"
        ))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("csrf_token={csrf}"))
        .send()
        .await
        .unwrap();
    assert_eq!(restore2.status(), 303, "form restore redirects");
    assert_eq!(
        restore2.headers().get("location").unwrap(),
        "/concept?path=/rv.md&scope=user",
        "form restore redirects to the concept viewer"
    );
    let back = rest_get_markdown(&client, &base, &cookie, "/rv.md").await;
    assert!(
        back.contains("Rv One") && back.contains("first body") && !back.contains("Rv Two"),
        "the live concept is rev 2's recorded state: {back}"
    );

    // CSRF gate on the form variant: no token in the form body → the
    // middleware's 403, before the handler runs — and the concept is
    // left untouched.
    let csrfless = client
        .post(format!(
            "{base}/api/v1/revisions/restore?scope=user&path=/rv.md&n=2"
        ))
        .header("cookie", &cookie)
        .header("content-type", "application/x-www-form-urlencoded")
        .body("n=2")
        .send()
        .await
        .unwrap();
    assert_eq!(csrfless.status(), 403, "form restore carries no CSRF token");
    let back = rest_get_markdown(&client, &base, &cookie, "/rv.md").await;
    assert!(
        back.contains("Rv One"),
        "a rejected restore left the concept untouched: {back}"
    );

    shutdown.cancel();
}

/// Dead paths stay in the index with live=false and the delete op; the
/// restore undeletes; error matrix: unknown/missing n, unknown scope,
/// missing revision, missing path, missing path param.
#[tokio::test]
async fn revisions_index_dead_path_and_error_matrix() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    rest_put(
        &client,
        &base,
        &cookie,
        &csrf,
        "/doomed.md",
        "---\ntype: Note\ntitle: Doomed\n---\n\ndoomed body",
    )
    .await;

    let del = client
        .delete(format!("{base}/api/v1/concepts/doomed.md"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 204);

    // The dead path stays listed (the plaintext path column carries the
    // display name long after the registry row is gone) with the delete
    // op at its head.
    let idx = client
        .get(format!("{base}/api/v1/revisions?scope=user"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let idx_json: serde_json::Value = idx.json().await.unwrap();
    let entry = idx_json
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "/doomed.md")
        .expect("a deleted path stays in the index");
    assert_eq!(entry["live"], false, "deleted is not live: {entry}");
    assert_eq!(entry["latest_no"], 2, "create + delete: {entry}");
    assert_eq!(entry["last_op"], "delete", "index entry: {entry}");

    // The delete row's payload is the pre-delete state: the content
    // chain survives the era (metadata says delete, the snapshot is the
    // concept as it stood).
    let snap = client
        .get(format!(
            "{base}/api/v1/revisions/snapshot?scope=user&path=/doomed.md&n=2"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(snap.status(), 200, "the deletion-era snapshot answers");
    let snap_text = snap.text().await.unwrap();
    assert!(
        snap_text.contains("Doomed") && snap_text.contains("doomed body"),
        "the delete row records the pre-delete content: {snap_text}"
    );

    // The chain survives beyond the era: restoring a deleted chain's
    // revision re-creates the concept at its original path.
    let restore = client
        .post(format!(
            "{base}/api/v1/revisions/restore?scope=user&path=/doomed.md&n=1"
        ))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(restore.status(), 200, "undeleting restore: {restore:?}");
    let restored: serde_json::Value = restore.json().await.unwrap();
    assert_eq!(restored["restored_to"], 1);
    let back = rest_get_markdown(&client, &base, &cookie, "/doomed.md").await;
    assert!(
        back.contains("Doomed") && back.contains("doomed body"),
        "the undeleted concept is v1 again: {back}"
    );

    let idx = client
        .get(format!("{base}/api/v1/revisions?scope=user"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let idx_json: serde_json::Value = idx.json().await.unwrap();
    let entry = idx_json
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "/doomed.md")
        .unwrap();
    assert_eq!(entry["live"], true, "restored to live again: {entry}");
    assert_eq!(entry["latest_no"], 3, "index entry: {entry}");
    assert_eq!(entry["last_op"], "restore", "index entry: {entry}");

    // Error matrix.
    // A never-existed path: snapshot 404, list a 200 empty chain, restore 404.
    let snap = client
        .get(format!(
            "{base}/api/v1/revisions/snapshot?scope=user&path=/nothing.md&n=1"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(snap.status(), 404, "no snapshots for a path with no chain");
    let list = client
        .get(format!(
            "{base}/api/v1/revisions/list?scope=user&path=/nothing.md"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200);
    let list_json: serde_json::Value = list.json().await.unwrap();
    assert!(list_json.as_array().unwrap().is_empty());
    let restore = client
        .post(format!(
            "{base}/api/v1/revisions/restore?scope=user&path=/nothing.md&n=1"
        ))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(restore.status(), 404, "nothing to restore");

    // A known path with an out-of-range revision: 404.
    let snap = client
        .get(format!(
            "{base}/api/v1/revisions/snapshot?scope=user&path=/doomed.md&n=99"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(snap.status(), 404, "revision 99 does not exist");

    // n=0 → 400 (1-based); non-numeric n → 400; missing n → 400.
    for bad_n in ["0", "abc", ""] {
        let snap = client
            .get(format!(
                "{base}/api/v1/revisions/snapshot?scope=user&path=/doomed.md&n={bad_n}"
            ))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(snap.status(), 400, "n={bad_n:?} must be 400");
        let json: serde_json::Value = snap.json().await.unwrap();
        assert!(
            json["error"].as_str().is_some(),
            "400 body is the JSON error shape: {json}"
        );
    }
    let snap = client
        .get(format!(
            "{base}/api/v1/revisions/snapshot?scope=user&path=/doomed.md"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(snap.status(), 400, "snapshot requires n");

    // Unknown scope → 400 on every route that reads it.
    for url in [
        "/api/v1/revisions?scope=admin",
        "/api/v1/revisions/list?scope=admin&path=/doomed.md",
        "/api/v1/revisions/snapshot?scope=admin&path=/doomed.md&n=1",
        "/api/v1/revisions/restore?scope=admin&path=/doomed.md&n=1",
    ] {
        let req = if url.contains("restore") {
            client
                .post(format!("{base}{url}"))
                .header("cookie", &cookie)
                .header("x-csrf-token", &csrf)
                .json(&serde_json::json!({}))
                .send()
                .await
                .unwrap()
        } else {
            client
                .get(format!("{base}{url}"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
        };
        assert_eq!(req.status(), 400, "unknown scope: {url}");
    }

    // A list without a path param → 400.
    let no_path = client
        .get(format!("{base}/api/v1/revisions/list?scope=user"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(no_path.status(), 400, "list requires a path");

    shutdown.cancel();
}

/// The 403 matrix (spec §3.2a-2): non-admin index + restore forbidden on
/// the skills scope (any-user read stays allowed), library restore
/// forbidden ALWAYS — admin or not, existing path or not (the perm check
/// precedes the store) — and another user's private-bundle path follows
/// the api_get_concept posture (not found in the caller's own scope).
#[tokio::test]
async fn revisions_permission_matrix() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();
    let (admin_cookie, admin_csrf) = login_admin(&client, &base).await;
    let (mallory_cookie, mallory_csrf) =
        create_and_login_user(&client, &base, &admin_cookie, &admin_csrf, "mallory").await;

    // A skills-scope concept with a two-revision chain (form puts —
    // writes to the skills shelf are admin-only via the form).
    let put1 = client
        .post(format!("{base}/concept"))
        .header("cookie", &admin_cookie)
        .header("x-csrf-token", &admin_csrf)
        .form(&[
            ("path", "/sk-rev.md"),
            ("scope", "skills"),
            (
                "markdown",
                "---\ntype: Skill\ntitle: Sk V1\n---\n\nskill first body",
            ),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(put1.status(), 303);
    let put2 = client
        .post(format!("{base}/concept"))
        .header("cookie", &admin_cookie)
        .header("x-csrf-token", &admin_csrf)
        .form(&[
            ("path", "/sk-rev.md"),
            ("scope", "skills"),
            (
                "markdown",
                "---\ntype: Skill\ntitle: Sk V2\n---\n\nskill second body",
            ),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(put2.status(), 303);

    // Skills INDEX: admin only — a non-admin gets 403.
    let idx = client
        .get(format!("{base}/api/v1/revisions?scope=skills"))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(idx.status(), 403, "non-admin skills index is forbidden");
    let idx_json: serde_json::Value = idx.json().await.unwrap();
    assert!(
        idx_json["error"].as_str().is_some(),
        "403 body is the JSON error shape: {idx_json}"
    );
    // ...while the admin (and any user through list/snapshot) may read.
    let admin_idx = client
        .get(format!("{base}/api/v1/revisions?scope=skills"))
        .header("cookie", &admin_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(admin_idx.status(), 200, "admin skills index answers");
    let admin_idx_json: serde_json::Value = admin_idx.json().await.unwrap();
    let entry = admin_idx_json
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "/sk-rev.md")
        .expect("skills revisions sit in the skills index: {admin_idx_json}");
    assert_eq!(entry["latest_no"], 2, "two form puts: {entry}");
    assert_eq!(entry["live"], true);
    assert_eq!(entry["last_op"], "update");

    // Skills LIST/SNAPSHOT: any user may read.
    let list = client
        .get(format!(
            "{base}/api/v1/revisions/list?scope=skills&path=/sk-rev.md"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200, "skills revisions are readable by all");
    let list_json: serde_json::Value = list.json().await.unwrap();
    let rows = list_json.as_array().unwrap();
    assert_eq!(rows.len(), 2, "both revisions visible: {list_json}");
    let snap = client
        .get(format!(
            "{base}/api/v1/revisions/snapshot?scope=skills&path=/sk-rev.md&n=1"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(snap.status(), 200);
    let snap_text = snap.text().await.unwrap();
    assert!(
        snap_text.contains("Sk V1") && snap_text.contains("skill first body"),
        "mallory reads the recorded v1: {snap_text}"
    );

    // Skills RESTORE: admin only — before the store, even for a path
    // that does not exist (no 404 oracle for the permission gate).
    let restore = client
        .post(format!(
            "{base}/api/v1/revisions/restore?scope=skills&path=/sk-rev.md&n=1"
        ))
        .header("cookie", &mallory_cookie)
        .header("x-csrf-token", &mallory_csrf)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        restore.status(),
        403,
        "non-admin skills restore is forbidden"
    );
    let restore = client
        .post(format!(
            "{base}/api/v1/revisions/restore?scope=skills&path=/never-a-skill.md&n=1"
        ))
        .header("cookie", &mallory_cookie)
        .header("x-csrf-token", &mallory_csrf)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        restore.status(),
        403,
        "the perm gate precedes the 404 store miss"
    );

    // Library: restore is ALWAYS forbidden — admin or not, path or not.
    let restore = client
        .post(format!(
            "{base}/api/v1/revisions/restore?scope=library&path=/anywhere.md&n=1"
        ))
        .header("cookie", &admin_cookie)
        .header("x-csrf-token", &admin_csrf)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(restore.status(), 403, "library restore: admin 403 too");
    let restore = client
        .post(format!(
            "{base}/api/v1/revisions/restore?scope=library&path=/anywhere.md&n=1"
        ))
        .header("cookie", &mallory_cookie)
        .header("x-csrf-token", &mallory_csrf)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(restore.status(), 403, "library restore: non-admin 403");

    // Library LIST/SNAPSHOT: shelf-visibility gated. A path whose book
    // slug is not on a global-read shelf (here: no book row at all)
    // follows concept_view's library-invisibility posture → 403.
    let list = client
        .get(format!(
            "{base}/api/v1/revisions/list?scope=library&path=/invisible/book.md"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 403, "invisible library path: 403");
    let snap = client
        .get(format!(
            "{base}/api/v1/revisions/snapshot?scope=library&path=/invisible/book.md&n=1"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(snap.status(), 403, "invisible library path: 403");

    // Another user's private-bundle path: api_get_concept posture — the
    // route reads the CALLER's own scope, so a foreign path is "not
    // found" (404), indistinguishable from a never-existed path.
    rest_put(
        &client,
        &base,
        &admin_cookie,
        &admin_csrf,
        "/a-secret.md",
        "---\ntype: Note\ntitle: Secret\n---\n\nsecret body",
    )
    .await;
    let snap = client
        .get(format!(
            "{base}/api/v1/revisions/snapshot?scope=user&path=/a-secret.md&n=1"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(
        snap.status(),
        404,
        "mallory's own bundle has no such path: 404"
    );
    let list = client
        .get(format!(
            "{base}/api/v1/revisions/list?scope=user&path=/a-secret.md"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200);
    let list_json: serde_json::Value = list.json().await.unwrap();
    assert!(
        list_json.as_array().unwrap().is_empty(),
        "mallory's chain for that path is empty: {list_json}"
    );

    shutdown.cancel();
}

/// Library positives: a real book ingested onto a global-read shelf —
/// a non-admin lists and snapshots its catalog revisions (shelf
/// visibility), and the library index itself stays admin-only.
#[tokio::test]
async fn revisions_library_visible_book_reads() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();
    let (admin_cookie, admin_csrf) = login_admin(&client, &base).await;

    for name in ["Public Shelf"] {
        let create = client
            .post(format!("{base}/admin/bookshelves"))
            .header("cookie", &admin_cookie)
            .header("x-csrf-token", &admin_csrf)
            .form(&[("name", name), ("global", "1")])
            .send()
            .await
            .unwrap();
        assert_eq!(create.status(), 303);
    }

    let boundary = "revisionsboundary1";
    let body = multipart_body(
        boundary,
        &[
            ("csrf_token", admin_csrf.as_str()),
            ("bookshelf", "Public Shelf"),
            ("slug", "my-book"),
            ("title", "My Book"),
        ],
        Some(("file", "book.md", BOOK)),
    );
    // Guard: inline-ingest passive health notes (see books_count_…).
    let upload_json: serde_json::Value = {
        let _health_guard = HEALTH_TEST_LOCK.lock().await;
        let upload = client
            .post(format!("{base}/api/v1/ingest"))
            .header("cookie", &admin_cookie)
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(upload.status(), 202);
        upload.json().await.unwrap()
    };
    assert_eq!(
        upload_json["status"], "done",
        "ingest ran inline: {upload_json}"
    );

    let (mallory_cookie, _mallory_csrf) =
        create_and_login_user(&client, &base, &admin_cookie, &admin_csrf, "mallory").await;

    // Library index: admin only.
    let idx = client
        .get(format!("{base}/api/v1/revisions?scope=library"))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(idx.status(), 403, "non-admin library index is forbidden");
    let admin_idx = client
        .get(format!("{base}/api/v1/revisions?scope=library"))
        .header("cookie", &admin_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(admin_idx.status(), 200, "admin library index answers");
    let admin_idx_json: serde_json::Value = admin_idx.json().await.unwrap();
    let entries = admin_idx_json.as_array().unwrap();
    let entry = entries
        .iter()
        .find(|e| e["path"] == "/my-book/book.md")
        .expect("the catalog hub has revisions: {admin_idx_json}");
    assert_eq!(entry["live"], true);

    // The hub chain (global-read shelf → visible to a non-admin).
    let list = client
        .get(format!(
            "{base}/api/v1/revisions/list?scope=library&path=/my-book/book.md"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200, "visible book: mallory reads revisions");
    let list_json: serde_json::Value = list.json().await.unwrap();
    let rows = list_json.as_array().unwrap();
    assert_eq!(rows.len(), 1, "the hub was put once: {list_json}");
    assert_eq!(rows[0]["revision_no"], 1);
    assert_eq!(rows[0]["op"], "create");

    // The recorded catalog snapshot carries the chapter titles.
    let snap = client
        .get(format!(
            "{base}/api/v1/revisions/snapshot?scope=library&path=/my-book/book.md&n=1"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(snap.status(), 200);
    let snap_text = snap.text().await.unwrap();
    assert!(
        snap_text.contains("Chapter One") && snap_text.contains("Chapter Two"),
        "the hub's snapshot is the recorded catalog: {snap_text}"
    );

    // A chapter file's snapshot too.
    let snap = client
        .get(format!(
            "{base}/api/v1/revisions/snapshot?scope=library&path=/my-book/ch-1-chapter-one.md&n=1"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(snap.status(), 200);
    let snap_text = snap.text().await.unwrap();
    assert!(
        snap_text.contains("Intro text about zebras"),
        "the chapter's snapshot is the recorded chapter text: {snap_text}"
    );

    shutdown.cancel();
}

/// Rider (from Task 2's review): since the store fails loudly on a
/// missing concept's delete, the REST DELETE maps the store's NotFound
/// to 404 — a never-existed path and the re-delete of an already-deleted
/// path both answer 404 instead of the legacy 204-swallow posture.
#[tokio::test]
async fn rest_delete_missing_concept_returns_404() {
    let (base, shutdown, _dir, _pool) = boot().await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // Never-existed path: 404.
    let del = client
        .delete(format!("{base}/api/v1/concepts/never-was.md"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(
        del.status(),
        404,
        "deleting a concept that never existed is 404"
    );
    let json: serde_json::Value = del.json().await.unwrap();
    assert_eq!(json["error"], "not found", "error shape: {json}");

    // The re-delete: 204 once, then 404.
    rest_put(
        &client,
        &base,
        &cookie,
        &csrf,
        "/del-target.md",
        "---\ntype: Note\ntitle: Del Target\n---\n\nbody",
    )
    .await;
    let del = client
        .delete(format!("{base}/api/v1/concepts/del-target.md"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 204, "first delete succeeds");
    let del = client
        .delete(format!("{base}/api/v1/concepts/del-target.md"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 404, "the re-delete answers 404");

    shutdown.cancel();
}
