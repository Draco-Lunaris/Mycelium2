//! TLS admin integration: GET /admin/tls/cert serves the ACTIVE cert
//! PEM (uploaded pair when one is in effect, else the managed
//! generated pair) to admins only, never the key. The harness boots a
//! real HTTPS server over a tempdir exactly like
//! tests/llm_test_connection_integration.rs; Task 4 grows this file
//! with the upload flow, whose test reuses `served_cert_pem` to assert
//! the UPLOADED cert is served after an upload.

use std::net::SocketAddr;

use mycelium_auth::login::LoginService;
use mycelium_store::Store;
use mycelium_web::AppState;

/// Known admin password for the pre-seeded account.
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";

/// Boot the full server (real TLS via `serve`) over a tempdir data
/// directory; returns (base URL, shutdown token, data dir).
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

/// GET /admin/tls/cert as the admin; returns the response body.
async fn download_cert(client: &reqwest::Client, base: &str, cookie: &str) -> String {
    let resp = client
        .get(format!("{base}/admin/tls/cert"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.text().await.unwrap()
}

/// Compare PEMs modulo trailing whitespace/line-ending noise.
fn normalize_pem(pem: &str) -> String {
    pem.lines().map(|l| l.trim()).collect::<Vec<_>>().join("\n")
}

#[tokio::test]
async fn tls_cert_download_admin_only_and_serves_generated_pem() {
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    // Unauthenticated → login redirect.
    let resp = cl
        .get(format!("{base}/admin/tls/cert"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 303);
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let _ = csrf;
    // Admin → 200 with the generated cert PEM.
    let resp = cl
        .get(format!("{base}/admin/tls/cert"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/x-pem-file")
    );
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.starts_with("-----BEGIN CERTIFICATE-----"),
        "PEM body: {body}"
    );
    assert!(!body.contains("PRIVATE KEY"), "key must never be served");
    // Task 2 guarantees the HTTPS listener serves exactly the managed
    // generated pair; compare the download against that file.
    let served_file = std::fs::read_to_string(dir.path().join("config/tls/cert.pem")).unwrap();
    assert_eq!(normalize_pem(&body), normalize_pem(&served_file));
    // The download helper agrees with the inline assertions above
    // (Task 4's upload test asserts the UPLOADED cert through it).
    assert_eq!(
        normalize_pem(&download_cert(&cl, &base, &cookie).await),
        normalize_pem(&served_file)
    );
}
