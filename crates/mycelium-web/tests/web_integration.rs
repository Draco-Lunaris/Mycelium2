//! End-to-end web integration test:
//! HTTPS boot → bootstrap login → forced password change → concept CRUD →
//! search → graph → skills → admin → API keys → logout.

use std::net::SocketAddr;
use std::sync::Arc;

use mycelium_auth::login::LoginService;
use mycelium_store::Store;
use mycelium_web::AppState;

/// Boot a test server on ephemeral ports; returns the HTTPS base URL and
/// the shutdown token.
async fn boot() -> (
    String,
    tokio_util::sync::CancellationToken,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let service_key = mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();
    mycelium_auth::bootstrap_admin(&store)
        .await
        .unwrap()
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
    (format!("https://127.0.0.1:{https_port}"), shutdown, dir)
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
    let (base, shutdown, dir) = boot().await;
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

    // 4. Read the bootstrap password and log in.
    let password = std::fs::read_to_string(dir.path().join("config/initial-admin-password"))
        .unwrap()
        .trim()
        .to_string();
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", password.as_str())])
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
            ("old", password.as_str()),
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
    assert!(home.text().await.unwrap().contains("Your bundle"));

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
    assert!(graph_html.contains(r#"<script src="/assets/graph.js">"#));

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
