//! First-access setup-page integration tests: a fresh store has NO admin
//! and NO bootstrap files — /setup must be reachable, create the admin
//! with operator-chosen credentials, show the recovery key exactly once,
//! and become unreachable once any user exists.

use std::net::SocketAddr;

use mycelium_auth::login::LoginService;
use mycelium_store::Store;
use mycelium_web::AppState;

/// Boot a test server on ephemeral ports with a ZERO-user store (the true
/// first-access state). Returns (https base, shutdown token, temp dir,
/// sqlite pool) — the pool lets tests seed users or flip flags directly.
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
    // NOTE: no user creation here — /setup must be reachable from this
    // state (that is the whole feature).
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

#[tokio::test]
async fn setup_page_rendered_when_no_users() {
    let (base, _shutdown, dir, _pool) = boot().await;
    let client = client();

    let setup = client.get(format!("{base}/setup")).send().await.unwrap();
    assert_eq!(setup.status(), 200);
    let html = setup.text().await.unwrap();
    assert!(html.contains("action=\"/setup\""));
    assert!(html.contains("value=\"admin\""), "username prefilled");
    assert!(html.contains("name=\"password\""));
    assert!(html.contains("name=\"password_confirm\""));

    // No bootstrap ran: health reports zero users, no secret file exists.
    let health = client
        .get(format!("{base}/api/v1/health"))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = health.json().await.unwrap();
    assert_eq!(body["users"], 0);
    assert!(!dir.path().join("config/initial-admin-password").exists());
    assert!(
        !dir.path()
            .join("config/initial-admin-recovery-key")
            .exists()
    );
}

#[tokio::test]
async fn setup_creates_admin_and_shows_recovery_key_once() {
    let (base, _shutdown, dir, _pool) = boot().await;
    let client = client();
    let pw = "operator password over twenty chars";

    // app.js appends csrf_token="" to every form post — replicate it.
    let post = client
        .post(format!("{base}/setup"))
        .form(&[
            ("username", "admin"),
            ("password", pw),
            ("password_confirm", pw),
            ("csrf_token", ""),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 200);
    let html = post.text().await.unwrap();
    assert!(html.contains("shown ONCE"));
    let start = html.find("<pre>").expect("recovery key pre block");
    let end = html[start + 5..].find("</pre>").expect("pre close");
    let key = html[start + 5..start + 5 + end].trim().to_string();
    assert!(!key.is_empty());
    assert!(!key.contains('\n'), "single-line recovery key");
    // The key is only in the response body, never in a URL.
    assert!(!html.contains("location"));

    // The chosen credentials log in straight to home (no forced change).
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", "admin"), ("password", pw)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
    assert_eq!(login.headers().get("location").unwrap(), "/");
    let cookie = cookie_from(&login);
    let home = client
        .get(format!("{base}/"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(home.status(), 200);

    // Setup is gone once a user exists.
    let again = client.get(format!("{base}/setup")).send().await.unwrap();
    assert_eq!(again.status(), 303);
    assert_eq!(again.headers().get("location").unwrap(), "/login");

    // No secret files, ever.
    assert!(!dir.path().join("config/initial-admin-password").exists());
    assert!(
        !dir.path()
            .join("config/initial-admin-recovery-key")
            .exists()
    );
}

#[tokio::test]
async fn setup_post_without_csrf_field_succeeds() {
    let (base, _shutdown, _dir, _pool) = boot().await;
    let client = client();
    let pw = "native form post password ok!!";
    // No csrf_token field at all: the no-session path exemption must not
    // depend on body contents.
    let post = client
        .post(format!("{base}/setup"))
        .form(&[
            ("username", "admin"),
            ("password", pw),
            ("password_confirm", pw),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 200);
    assert!(post.text().await.unwrap().contains("Setup complete"));
}

#[tokio::test]
async fn setup_validation_errors_re_render_page() {
    let (base, _shutdown, _dir, _pool) = boot().await;
    let client = client();

    // Mismatched confirmation.
    let r = client
        .post(format!("{base}/setup"))
        .form(&[
            ("username", "admin"),
            ("password", "first password twenty chars!"),
            ("password_confirm", "other password twenty chars!"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("do not match"));

    // Too-short password.
    let r = client
        .post(format!("{base}/setup"))
        .form(&[
            ("username", "admin"),
            ("password", "short"),
            ("password_confirm", "short"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("at least"));

    // Blank username.
    let r = client
        .post(format!("{base}/setup"))
        .form(&[
            ("username", "  "),
            ("password", "blank username password 20!"),
            ("password_confirm", "blank username password 20!"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        r.text()
            .await
            .unwrap()
            .contains("Username must not be empty")
    );
}

#[tokio::test]
async fn setup_redirects_to_login_when_users_exist() {
    let (base, _shutdown, _dir, pool) = boot().await;
    let client = client();

    // Pre-create a user directly (setup is then permanently closed).
    let users = mycelium_auth::UserStore::new(pool);
    users
        .create_local(
            "admin",
            "admin@localhost.local",
            "pre-existing admin password!",
            mycelium_auth::Role::Admin,
        )
        .await
        .unwrap();

    // GET and POST both bounce to /login.
    let get = client.get(format!("{base}/setup")).send().await.unwrap();
    assert_eq!(get.status(), 303);
    assert_eq!(get.headers().get("location").unwrap(), "/login");

    let post = client
        .post(format!("{base}/setup"))
        .form(&[
            ("username", "other"),
            ("password", "second user password 20+"),
            ("password_confirm", "second user password 20+"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 303);
    assert_eq!(post.headers().get("location").unwrap(), "/login");
}
