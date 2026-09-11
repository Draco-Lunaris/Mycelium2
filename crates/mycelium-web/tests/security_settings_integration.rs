//! Security-settings integration test: the admin panel's guardrailed
//! security config (session TTL, login throttle, password policy,
//! passage cap) — saved via the form, clamped on save AND read, and
//! enforced at every call site.

use std::net::SocketAddr;

use mycelium_auth::login::LoginService;
use mycelium_store::Store;
use mycelium_web::AppState;

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

/// Log in as the bootstrap admin and complete the forced password
/// change; returns (cookie, csrf).
async fn login_admin(
    client: &reqwest::Client,
    base: &str,
    dir: &tempfile::TempDir,
) -> (String, String) {
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
    assert_eq!(login.status(), 303);
    let cookie = cookie_from(&login);
    let csrf = csrf_from_page(client, &format!("{base}/password"), &cookie).await;
    let new_password = "admin password twenty chars!";
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
    let login2 = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", new_password)])
        .send()
        .await
        .unwrap();
    assert_eq!(login2.status(), 303);
    let cookie2 = cookie_from(&login2);
    let csrf2 = csrf_from_page(client, &format!("{base}/"), &cookie2).await;
    (cookie2, csrf2)
}

#[tokio::test]
async fn security_settings_guardrails() {
    let (base, shutdown, dir) = boot().await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base, &dir).await;

    // 1. The admin page renders the security form with the defaults.
    let admin_html = client
        .get(format!("{base}/admin"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(admin_html.contains("Security settings"));
    assert!(admin_html.contains("value=\"720\""), "default session TTL");
    assert!(admin_html.contains("value=\"20\""), "default password min");

    // 2. Save valid settings: session TTL 30 min, password min 12.
    let save = client
        .post(format!("{base}/admin/security"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("session_ttl_minutes", "30"),
            ("login_max_failures", "3"),
            ("login_lockout_seconds", "30"),
            ("min_password_length", "12"),
            ("passage_max_chars", "131072"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(save.status(), 303, "valid settings save");

    // 3. The admin page reflects the saved values.
    let admin_html = client
        .get(format!("{base}/admin"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(admin_html.contains("value=\"30\""), "session TTL saved");

    // 4. Session TTL applies: a NEW login gets a 30-minute cookie
    //    (Max-Age=1800) and a session that expires in ~30 min.
    let login = client
        .post(format!("{base}/login"))
        .form(&[
            ("username", "admin"),
            ("password", "admin password twenty chars!"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
    let set_cookie = login
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        set_cookie.contains("Max-Age=1800"),
        "cookie Max-Age must follow the configured TTL: {set_cookie}"
    );
    let cookie2 = cookie_from(&login);

    // 5. Password policy applies at the configured minimum: a 9-char
    //    password (below the saved min of 12) is rejected with the
    //    configured minimum in the message. The session stays alive
    //    (the change failed), so the same cookie keeps working.
    let csrf2 = csrf_from_page(&client, &format!("{base}/password"), &cookie2).await;
    let really_short = client
        .post(format!("{base}/password"))
        .header("cookie", &cookie2)
        .header("x-csrf-token", &csrf2)
        .form(&[
            ("old", "admin password twenty chars!"),
            ("new", "tiny9char"), // 9 chars < 12 → must fail
            ("repeat", "tiny9char"),
        ])
        .send()
        .await
        .unwrap();
    let body = really_short.text().await.unwrap();
    assert!(
        body.contains("at least 12"),
        "password policy must use the configured min: {body}"
    );

    // 6. Guardrail clamping on save: out-of-range values are clamped,
    //    not rejected. Session TTL 5 (below min 15) → clamped to 15.
    let save = client
        .post(format!("{base}/admin/security"))
        .header("cookie", &cookie2)
        .header("x-csrf-token", &csrf2)
        .form(&[
            ("session_ttl_minutes", "5"),   // below min
            ("login_max_failures", "1"),    // below min
            ("login_lockout_seconds", "1"), // below min
            ("min_password_length", "4"),   // below min
            ("passage_max_chars", "100"),   // below min
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(save.status(), 303);
    let admin_html = client
        .get(format!("{base}/admin"))
        .header("cookie", &cookie2)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        admin_html.contains("value=\"15\""),
        "session TTL clamped to the 15-minute floor"
    );
    assert!(
        admin_html.contains("value=\"3\""),
        "login failures clamped to the 3 floor"
    );
    assert!(
        admin_html.contains("value=\"30\""),
        "lockout clamped to the 30s floor"
    );
    assert!(
        admin_html.contains("value=\"12\""),
        "password min clamped to the 12 floor"
    );
    assert!(
        admin_html.contains("value=\"16384\""),
        "passage cap clamped to the 16k floor"
    );

    shutdown.cancel();
}
