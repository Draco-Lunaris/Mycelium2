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

/// Minimal percent-decoder for the flash query values (the handler
/// encodes with `pages::urlencoding_encode`, which escapes everything
/// outside the unreserved set plus `/`).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// A self-signed cert/key pair for `san` (rcgen as a stand-in for a
/// CA-issued pair); returns (cert PEM bytes, key PEM bytes).
fn gen_pair(san: &str) -> (Vec<u8>, Vec<u8>) {
    let kp = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec![san.to_string()]).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, san);
    let cert = params.self_signed(&kp).unwrap();
    (cert.pem().into_bytes(), kp.serialize_pem().into_bytes())
}

/// POST /admin/tls/upload with the given cert/key/label; returns the
/// decoded flash query (the part after `?` in the redirect Location).
async fn upload(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
    cert: Vec<u8>,
    key: Vec<u8>,
    label: &str,
) -> String {
    let form = reqwest::multipart::Form::new()
        .part(
            "cert_pem",
            reqwest::multipart::Part::bytes(cert).file_name("cert.pem"),
        )
        .part(
            "key_pem",
            reqwest::multipart::Part::bytes(key).file_name("key.pem"),
        )
        .text("label", label.to_string());
    let resp = client
        .post(format!("{base}/admin/tls/upload"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 303);
    let loc = resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(loc.starts_with("/admin/system?"), "{loc}");
    percent_decode(&loc)
}

#[tokio::test]
async fn upload_swaps_live_cert_and_flashes_fingerprint() {
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let generated = std::fs::read_to_string(dir.path().join("config/tls/cert.pem")).unwrap();
    let (cert, key) = gen_pair("uploaded.example");
    let flash = upload(
        &cl,
        &base,
        &cookie,
        &csrf,
        cert.clone(),
        key.clone(),
        "test-upload",
    )
    .await;
    assert!(flash.contains("ok="), "{flash}");
    assert!(flash.contains("fingerprint"), "{flash}");
    // Files on disk: the pair + the sidecar.
    let up_cert = dir.path().join("config/tls/uploaded-cert.pem");
    let up_key = dir.path().join("config/tls/uploaded-key.pem");
    assert!(up_cert.exists());
    assert!(up_key.exists());
    assert_eq!(
        normalize_pem(&std::fs::read_to_string(&up_cert).unwrap()),
        normalize_pem(&String::from_utf8_lossy(&cert))
    );
    // Key perms 0600; the key is never served.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&up_key).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    // The download endpoint now serves the UPLOADED cert, not the
    // generated one (Task 2's carried M-1: uploaded is the served pair).
    let body = download_cert(&cl, &base, &cookie).await;
    assert_eq!(
        normalize_pem(&body),
        normalize_pem(&String::from_utf8_lossy(&cert))
    );
    assert_ne!(normalize_pem(&body), normalize_pem(&generated));
    // Sidecar.
    let meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("config/tls/uploaded.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["label"], "test-upload");
    assert_eq!(meta["source"], "uploaded");
    assert!(meta["uploaded_at"].as_str().unwrap().contains('T'));
    let first_fp = meta["fingerprint_sha256"].as_str().unwrap().to_string();
    assert_eq!(first_fp.len(), 64, "{first_fp}");

    // Double-upload race consistency (Review Focus line 5): a SECOND
    // successful upload of a DIFFERENT pair leaves files + sidecar
    // consistently naming the second pair — no torn first/second mix.
    let (cert2, key2) = gen_pair("second.example");
    let flash2 = upload(
        &cl,
        &base,
        &cookie,
        &csrf,
        cert2.clone(),
        key2.clone(),
        "second-upload",
    )
    .await;
    assert!(flash2.contains("ok="), "{flash2}");
    assert_eq!(
        normalize_pem(&std::fs::read_to_string(&up_cert).unwrap()),
        normalize_pem(&String::from_utf8_lossy(&cert2))
    );
    assert_eq!(
        normalize_pem(&std::fs::read_to_string(&up_key).unwrap()),
        normalize_pem(&String::from_utf8_lossy(&key2))
    );
    let meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("config/tls/uploaded.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["label"], "second-upload");
    let second_fp = meta["fingerprint_sha256"].as_str().unwrap().to_string();
    assert_ne!(first_fp, second_fp, "sidecar must name the second pair");
    // And the served cert is the second pair too.
    let body = download_cert(&cl, &base, &cookie).await;
    assert_eq!(
        normalize_pem(&body),
        normalize_pem(&String::from_utf8_lossy(&cert2))
    );
}

#[tokio::test]
async fn upload_mismatch_rejected_no_state_change() {
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let (cert, _key) = gen_pair("a.example");
    let (_c2, key2) = gen_pair("b.example");
    let flash = upload(&cl, &base, &cookie, &csrf, cert, key2, "bad").await;
    assert!(flash.contains("error="), "{flash}");
    assert!(flash.contains("does not match"), "{flash}");
    // No state change: no uploaded files, no sidecar.
    assert!(!dir.path().join("config/tls/uploaded-cert.pem").exists());
    assert!(!dir.path().join("config/tls/uploaded-key.pem").exists());
    assert!(!dir.path().join("config/tls/uploaded.json").exists());
    // Active cert unchanged: download still serves the generated cert.
    let body = download_cert(&cl, &base, &cookie).await;
    let generated = std::fs::read_to_string(dir.path().join("config/tls/cert.pem")).unwrap();
    assert_eq!(normalize_pem(&body), normalize_pem(&generated));
}

#[tokio::test]
async fn upload_requires_both_fields_and_admin() {
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    // Missing key field → clear flash, nothing written.
    let (cert, _key) = gen_pair("solo.example");
    let resp = cl
        .post(format!("{base}/admin/tls/upload"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .multipart(reqwest::multipart::Form::new().part(
            "cert_pem",
            reqwest::multipart::Part::bytes(cert).file_name("cert.pem"),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 303);
    let loc = resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(percent_decode(&loc).contains("required"), "{loc}");
    assert!(!dir.path().join("config/tls/uploaded-cert.pem").exists());
    // Unauthenticated → login redirect (the session gate, not the handler).
    let (cert, key) = gen_pair("anon.example");
    let resp = cl
        .post(format!("{base}/admin/tls/upload"))
        .multipart(
            reqwest::multipart::Form::new()
                .part(
                    "cert_pem",
                    reqwest::multipart::Part::bytes(cert).file_name("cert.pem"),
                )
                .part(
                    "key_pem",
                    reqwest::multipart::Part::bytes(key).file_name("key.pem"),
                ),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 303);
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        "/login"
    );
    assert!(!dir.path().join("config/tls/uploaded-cert.pem").exists());
}

#[tokio::test]
async fn upload_rejects_oversized_field() {
    // 2 MiB per field: a cert chain plus its key is a few KiB, so an
    // over-cap upload is refused with a clear flash and nothing written.
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let (_cert, key) = gen_pair("big.example");
    // 3 MiB of PEM-ish filler: over the 2 MiB cap, under the route's
    // 5 MiB body ceiling, so the handler's own check is what rejects it.
    let oversized = vec![b'A'; 3 * 1024 * 1024];
    let flash = upload(&cl, &base, &cookie, &csrf, oversized, key, "big").await;
    assert!(flash.contains("error="), "{flash}");
    assert!(flash.contains("too large"), "{flash}");
    assert!(!dir.path().join("config/tls/uploaded-cert.pem").exists());
    assert!(!dir.path().join("config/tls/uploaded-key.pem").exists());
    // The good pair still uploads afterwards.
    let (cert2, key2) = gen_pair("after-big.example");
    let flash2 = upload(&cl, &base, &cookie, &csrf, cert2, key2, "retry").await;
    assert!(flash2.contains("ok="), "{flash2}");
}

#[tokio::test]
async fn upload_reload_failure_rolls_back_all_files() {
    // A pair that PASSES validation but cannot LOAD: a valid leaf plus a
    // trailing CERTIFICATE block whose base64 is garbage. x509-parser
    // skips the unparseable block (the leaf validates against the key),
    // but rustls's strict PEM decode rejects the whole file — so the
    // live reload fails after the writes. The handler must then remove
    // all three files: a pair the live listener cannot load must not
    // survive as the "active" cert.
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let generated = std::fs::read_to_string(dir.path().join("config/tls/cert.pem")).unwrap();
    let (cert, key) = gen_pair("rollback.example");
    let chain = format!(
        "{}-----BEGIN CERTIFICATE-----\n!!!!not base64!!!!\n-----END CERTIFICATE-----\n",
        String::from_utf8(cert.clone()).unwrap()
    );
    let flash = upload(
        &cl,
        &base,
        &cookie,
        &csrf,
        chain.into_bytes(),
        key,
        "rollback",
    )
    .await;
    assert!(flash.contains("error="), "{flash}");
    assert!(
        flash.contains("could not load"),
        "reload failure must flash its own message: {flash}"
    );
    // No partial state: none of the three files survive.
    assert!(!dir.path().join("config/tls/uploaded-cert.pem").exists());
    assert!(!dir.path().join("config/tls/uploaded-key.pem").exists());
    assert!(!dir.path().join("config/tls/uploaded.json").exists());
    // The live listener still serves the generated cert.
    let body = download_cert(&cl, &base, &cookie).await;
    assert_eq!(normalize_pem(&body), normalize_pem(&generated));
    // And a subsequent GOOD upload still works (the rollback left the
    // handler and the listener healthy).
    let (cert2, key2) = gen_pair("after-rollback.example");
    let flash2 = upload(&cl, &base, &cookie, &csrf, cert2.clone(), key2, "retry").await;
    assert!(flash2.contains("ok="), "{flash2}");
    let body = download_cert(&cl, &base, &cookie).await;
    assert_eq!(
        normalize_pem(&body),
        normalize_pem(&String::from_utf8_lossy(&cert2))
    );
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
