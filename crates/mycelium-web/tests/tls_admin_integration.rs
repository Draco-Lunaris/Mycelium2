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

/// Boot the full server (real TLS via `serve`) over a fresh tempdir
/// data directory; returns (base URL, shutdown token, data dir).
async fn boot() -> (
    String,
    tokio_util::sync::CancellationToken,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let (base, shutdown) = boot_with_dir(&dir).await;
    (base, shutdown, dir)
}

/// The boot body over a CALLER-SUPPLIED data dir — the boot-precedence
/// test pre-seeds `config/tls/` before the server starts, which `boot()`
/// (owning its own tempdir) cannot express. Returns (base URL, shutdown
/// token); the caller keeps the tempdir alive.
async fn boot_with_dir(dir: &tempfile::TempDir) -> (String, tokio_util::sync::CancellationToken) {
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
    (format!("https://127.0.0.1:{https_port}"), shutdown)
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

/// A leaf signed by a separate CA: (chain PEM leaf-first, leaf key PEM).
/// Stands in for a real CA-issued chain — same PEM shape, same ordering.
fn gen_chain() -> (Vec<u8>, Vec<u8>) {
    let ca_kp = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(vec!["ca.example".to_string()]).unwrap();
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "ca.example");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_kp).unwrap();

    let leaf_kp = rcgen::KeyPair::generate().unwrap();
    let mut leaf_params = rcgen::CertificateParams::new(vec!["chain.example".to_string()]).unwrap();
    leaf_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "chain.example");
    let leaf = leaf_params
        .signed_by(&leaf_kp, &ca, &ca_kp)
        .expect("CA-signed leaf");
    // Leaf first, then the intermediate — the order a CA hands over.
    (
        format!("{}{}", leaf.pem(), ca.pem()).into_bytes(),
        leaf_kp.serialize_pem().into_bytes(),
    )
}

/// A self-signed cert that expired long ago.
fn gen_expired_pair() -> (Vec<u8>, Vec<u8>) {
    let kp = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["expired.example".to_string()]).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "expired.example");
    params.not_before = time::OffsetDateTime::UNIX_EPOCH;
    params.not_after = time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(1);
    let cert = params.self_signed(&kp).unwrap();
    (cert.pem().into_bytes(), kp.serialize_pem().into_bytes())
}

/// SHA-256 hex of the first CERTIFICATE block's DER (the leaf fingerprint
/// the handler flashes) — computed independently of the handler.
fn leaf_fingerprint(chain_pem: &[u8]) -> String {
    let der = x509_parser::pem::Pem::iter_from_buffer(chain_pem)
        .next()
        .unwrap()
        .unwrap()
        .contents;
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(der))
}

/// Log in as a freshly created non-admin; returns (cookie, csrf).
async fn login_plain_user(
    client: &reqwest::Client,
    base: &str,
    admin_cookie: &str,
) -> (String, String) {
    let csrf = csrf_from_page(client, &format!("{base}/admin/users"), admin_cookie).await;
    let created = client
        .post(format!("{base}/admin/users"))
        .header("cookie", admin_cookie)
        .form(&[
            ("csrf_token", csrf.as_str()),
            ("username", "plain"),
            ("email", "plain@example.invalid"),
            ("password", "a plain user password for tests!"),
            ("role", "user"),
        ])
        .send()
        .await
        .unwrap();
    // Success renders the users page (200); any bounce is a 303.
    assert_eq!(created.status(), 200, "user creation must succeed");
    let login = client
        .post(format!("{base}/login"))
        .form(&[
            ("username", "plain"),
            ("password", "a plain user password for tests!"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
    let cookie = cookie_from(&login);
    let csrf = csrf_from_page(client, &format!("{base}/"), &cookie).await;
    (cookie, csrf)
}

#[tokio::test]
async fn upload_keeps_full_chain_and_flashes_leaf_fingerprint() {
    // Review M-5(1): a leaf+intermediate upload must survive on disk as
    // the WHOLE chain (a server that drops the intermediate breaks every
    // client that does not already trust it), while the fingerprint the
    // admin is shown is the LEAF's (what a CA publishes).
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let (chain, key) = gen_chain();
    let flash = upload(&cl, &base, &cookie, &csrf, chain.clone(), key, "chained").await;
    assert!(flash.contains("ok="), "{flash}");
    let on_disk = std::fs::read_to_string(dir.path().join("config/tls/uploaded-cert.pem")).unwrap();
    assert_eq!(
        normalize_pem(&on_disk),
        normalize_pem(&String::from_utf8_lossy(&chain)),
        "the whole chain must be stored verbatim"
    );
    assert_eq!(
        on_disk.matches("-----BEGIN CERTIFICATE-----").count(),
        2,
        "expected two CERTIFICATE blocks on disk"
    );
    // The flash names the LEAF's fingerprint, not the chain's or the
    // intermediate's.
    let leaf_fp = leaf_fingerprint(&chain);
    assert!(
        flash.contains(&leaf_fp[..16]),
        "flash must carry the leaf fingerprint {}…: {flash}",
        &leaf_fp[..16]
    );
    let meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("config/tls/uploaded.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["fingerprint_sha256"], leaf_fp);
    // And the download serves the whole chain too.
    let body = download_cert(&cl, &base, &cookie).await;
    assert_eq!(body.matches("-----BEGIN CERTIFICATE-----").count(), 2);
}

#[tokio::test]
async fn upload_expired_cert_succeeds_with_notafter_warning() {
    // Review M-5(2) / Decision 2: expiry is warn-only, and the warning
    // must actually reach the admin's flash with the notAfter value.
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let (cert, key) = gen_expired_pair();
    let flash = upload(&cl, &base, &cookie, &csrf, cert.clone(), key, "expired").await;
    // Success (ok=), not an error.
    assert!(flash.contains("ok="), "expired must still install: {flash}");
    assert!(
        flash.contains("WARNING"),
        "warning must be flashed: {flash}"
    );
    assert!(flash.contains("expired on"), "{flash}");
    // The warning carries the leaf's notAfter date (1970-01-02 for the
    // fixture), so the admin can see WHICH date the warning means.
    let not_after = x509_parser::pem::Pem::iter_from_buffer(&cert)
        .next()
        .unwrap()
        .unwrap()
        .contents;
    let (_, parsed) = x509_parser::parse_x509_certificate(&not_after).unwrap();
    let not_after = parsed.validity().not_after.to_datetime();
    let date = format!(
        "{:04}-{:02}-{:02}",
        not_after.year(),
        not_after.month() as u8,
        not_after.day()
    );
    assert!(
        flash.contains(&date),
        "flash must contain notAfter date {date}: {flash}"
    );
    // Installed for real: the files exist and the download serves them.
    assert!(dir.path().join("config/tls/uploaded-cert.pem").exists());
    let body = download_cert(&cl, &base, &cookie).await;
    assert_eq!(
        normalize_pem(&body),
        normalize_pem(&String::from_utf8_lossy(&cert))
    );
}

#[tokio::test]
async fn upload_without_csrf_header_is_rejected_before_buffering() {
    // Review I-1: the CSRF middleware buffers a multipart body to find a
    // form field, and it runs OUTSIDE the route's body limit and outside
    // RequireAdmin — so before the path-scoped cap, ANY logged-in
    // non-admin could make the server buffer up to the book ceiling
    // (33 MiB default, 256 MiB max) per concurrent request. A
    // non-admin's over-capacity upload is now refused up front: 413
    // (declared length over the small cap) rather than 403 after the
    // whole body was read, and 400 for a chunked body that trips the
    // streaming cap.
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (admin_cookie, _admin_csrf) = login_admin(&cl, &base).await;
    let (cookie, _csrf) = login_plain_user(&cl, &base, &admin_cookie).await;

    // 12 MiB declared: over the small multipart cap (8 MiB), under the
    // book ceiling (33 MiB) — exactly the window the bug lived in.
    let body = vec![b'A'; 12 * 1024 * 1024];
    let resp = cl
        .post(format!("{base}/admin/tls/upload"))
        .header("cookie", &cookie)
        .header(
            reqwest::header::CONTENT_TYPE,
            "multipart/form-data; boundary=x",
        )
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        413,
        "declared over-capacity length must be rejected before buffering"
    );
    assert!(!dir.path().join("config/tls/uploaded-cert.pem").exists());

    // The book-upload path is untouched: a 12 MiB body there is UNDER
    // its 33 MiB ceiling, so it is still buffered and parsed (reaching
    // the 403 missing-CSRF branch), proving the small cap did not leak
    // onto the ingest path.
    let resp = cl
        .post(format!("{base}/api/v1/ingest"))
        .header("cookie", &cookie)
        .header(
            reqwest::header::CONTENT_TYPE,
            "multipart/form-data; boundary=x",
        )
        .body(vec![b'A'; 12 * 1024 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        403,
        "book ingest keeps buffering up to its own ceiling"
    );
}

// ---------- CSR generation + pending lifecycle + reset (Task 5) ----------

/// POST /admin/tls/csr (urlencoded form); returns the response.
async fn post_csr(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
    cn: &str,
    sans: &str,
) -> reqwest::Response {
    client
        .post(format!("{base}/admin/tls/csr"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .form(&[("cn", cn), ("sans", sans)])
        .send()
        .await
        .unwrap()
}

/// POST a TLS admin action that answers with a flash redirect; returns
/// the decoded flash query.
async fn post_flash(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
    path: &str,
) -> String {
    let resp = client
        .post(format!("{base}{path}"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 303, "POST {path} must flash-redirect");
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
async fn csr_flow_generate_download_upload_match_discard() {
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    // Generate → PEM download response + pending files on disk.
    let resp = post_csr(
        &cl,
        &base,
        &cookie,
        &csrf,
        "mycelium2.example",
        "mycelium2.example",
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/x-pem-file")
    );
    assert_eq!(
        resp.headers()
            .get("content-disposition")
            .and_then(|v| v.to_str().ok()),
        Some("attachment; filename=\"csr.pem\"")
    );
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    let csr_pem = resp.text().await.unwrap();
    assert!(csr_pem.contains("BEGIN CERTIFICATE REQUEST"), "{csr_pem}");
    assert!(!csr_pem.contains("PRIVATE KEY"), "key never in body");
    let pending_key_path = dir.path().join("config/tls/pending-csr-key.pem");
    let pending_meta_path = dir.path().join("config/tls/pending.json");
    assert!(pending_key_path.exists());
    assert!(pending_meta_path.exists());
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&pending_key_path)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&pending_meta_path).unwrap()).unwrap();
    assert_eq!(meta["cn"], "mycelium2.example");
    assert_eq!(meta["sans"][0], "mycelium2.example");
    assert!(meta["created_at"].as_str().unwrap().contains('T'));
    // Re-download (GET) rebuilds from the pending key — same key, so the
    // CSR carries it (rcgen's serialize_request is not byte-stable, so
    // structure, not bytes, is the contract).
    let resp = cl
        .get(format!("{base}/admin/tls/csr"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let csr2 = resp.text().await.unwrap();
    assert!(csr2.contains("BEGIN CERTIFICATE REQUEST"));
    let spki_of = |pem: &str| -> Vec<u8> {
        use x509_parser::prelude::*;
        let der = x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes())
            .next()
            .unwrap()
            .unwrap()
            .contents;
        let (_, csr) = X509CertificationRequest::from_der(&der).unwrap();
        csr.certification_request_info.subject_pki.raw.to_vec()
    };
    assert_eq!(
        spki_of(&csr2),
        spki_of(&csr_pem),
        "re-download must reuse the pending key"
    );
    // Fulfill: issue a cert FROM the pending key locally (the CA's role),
    // upload it — the pending flow must accept and clear.
    let key_pem = std::fs::read_to_string(&pending_key_path).unwrap();
    let key = rcgen::KeyPair::from_pem(&key_pem).unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["mycelium2.example".to_string()]).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "mycelium2.example");
    let cert = params.self_signed(&key).unwrap();
    let form = reqwest::multipart::Form::new()
        .part(
            "cert_pem",
            reqwest::multipart::Part::bytes(cert.pem().into_bytes()).file_name("cert.pem"),
        )
        .part(
            "key_pem",
            reqwest::multipart::Part::bytes(key_pem.as_bytes().to_vec()).file_name("key.pem"),
        )
        .text("label", "from-csr".to_string());
    let resp = cl
        .post(format!("{base}/admin/tls/upload"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
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
    assert!(percent_decode(&loc).contains("ok="), "{loc}");
    // Pending cleared (both files).
    assert!(!pending_key_path.exists());
    assert!(!pending_meta_path.exists());
    // An upload with a DIFFERENT key while a CSR is pending is refused
    // and the pending pair survives (the admin still needs it to retry).
    post_csr(&cl, &base, &cookie, &csrf, "x.example", "").await;
    assert!(pending_key_path.exists());
    let (cert2, key2) = gen_pair("x.example");
    let flash = upload(&cl, &base, &cookie, &csrf, cert2, key2, "wrong-key").await;
    assert!(flash.contains("error="), "{flash}");
    assert!(
        flash.contains("pending CSR key"),
        "pending-key mismatch must name the pending key: {flash}"
    );
    assert!(pending_key_path.exists(), "rejection keeps the pending key");
    // Discard path.
    let flash = post_flash(&cl, &base, &cookie, &csrf, "/admin/tls/pending/discard").await;
    assert!(flash.contains("ok="), "{flash}");
    assert!(!pending_key_path.exists());
    assert!(!pending_meta_path.exists());
    // GET with nothing pending → flash-not-found redirect, not a 200.
    let resp = cl
        .get(format!("{base}/admin/tls/csr"))
        .header("cookie", &cookie)
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
    assert!(percent_decode(&loc).contains("error="), "{loc}");
}

#[tokio::test]
async fn csr_generate_replaces_prior_pending_last_one_wins() {
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let key_path = dir.path().join("config/tls/pending-csr-key.pem");
    post_csr(&cl, &base, &cookie, &csrf, "first.example", "").await;
    let first_key = std::fs::read_to_string(&key_path).unwrap();
    let resp = post_csr(&cl, &base, &cookie, &csrf, "second.example", "").await;
    assert_eq!(resp.status(), 200);
    let second_key = std::fs::read_to_string(&key_path).unwrap();
    assert_ne!(first_key, second_key, "a new CSR replaces the pending key");
    let meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("config/tls/pending.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["cn"], "second.example");
    // Empty sans field → the localhost default in the persisted meta.
    assert_eq!(meta["sans"][0], "localhost");
    assert_eq!(meta["sans"].as_array().unwrap().len(), 1);
    // A blank CN is refused with an error flash and leaves the pending
    // pair untouched.
    let flash_resp = post_csr(&cl, &base, &cookie, &csrf, "   ", "").await;
    assert_eq!(flash_resp.status(), 303);
    let loc = flash_resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(percent_decode(&loc).contains("error="), "{loc}");
    assert_eq!(std::fs::read_to_string(&key_path).unwrap(), second_key);
}

#[tokio::test]
async fn csr_endpoints_admin_only() {
    let (base, _shutdown, _dir) = boot().await;
    let cl = client();
    // Unauthenticated GET/POST → the login gate redirect.
    let resp = cl
        .get(format!("{base}/admin/tls/csr"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 303);
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        "/login"
    );
    let resp = cl
        .post(format!("{base}/admin/tls/csr"))
        .form(&[("cn", "anon.example"), ("sans", "")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 303);
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        "/login"
    );
    let resp = cl
        .post(format!("{base}/admin/tls/pending/discard"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 303);
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        "/login"
    );
    let resp = cl
        .post(format!("{base}/admin/tls/reset"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 303);
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        "/login"
    );
}

#[tokio::test]
async fn reset_returns_to_generated_pair() {
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let generated = std::fs::read_to_string(dir.path().join("config/tls/cert.pem")).unwrap();
    let generated_key = std::fs::read_to_string(dir.path().join("config/tls/key.pem")).unwrap();
    let (cert, key) = gen_pair("reset.example");
    // Upload then reset.
    let flash = upload(&cl, &base, &cookie, &csrf, cert, key, "tmp").await;
    assert!(flash.contains("ok="), "{flash}");
    assert!(dir.path().join("config/tls/uploaded-cert.pem").exists());
    let flash = post_flash(&cl, &base, &cookie, &csrf, "/admin/tls/reset").await;
    assert!(flash.contains("ok="), "{flash}");
    assert!(!dir.path().join("config/tls/uploaded-cert.pem").exists());
    assert!(!dir.path().join("config/tls/uploaded-key.pem").exists());
    assert!(!dir.path().join("config/tls/uploaded.json").exists());
    // The generated pair is NEVER deleted by reset.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("config/tls/cert.pem")).unwrap(),
        generated
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("config/tls/key.pem")).unwrap(),
        generated_key
    );
    // Download serves the generated pair again.
    let body = download_cert(&cl, &base, &cookie).await;
    assert_eq!(normalize_pem(&body), normalize_pem(&generated));
    // And the LIVE listener still serves after the reload. This client
    // accepts any cert, so the handshake itself is not the reload proof
    // — the ok flash means `reload_from_pem_file` returned Ok, and
    // Task 4's upload test pins that the same call swaps the served
    // cert. What this request does prove: the reset reload did not
    // break the listener.
    let live = reqwest::Client::builder()
        .use_rustls_tls()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = live
        .get(format!("{base}/admin/tls/cert"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("live TLS handshake after reset");
    assert_eq!(resp.status(), 200);
    // Reset with nothing uploaded is idempotent and still ok.
    let flash = post_flash(&cl, &base, &cookie, &csrf, "/admin/tls/reset").await;
    assert!(flash.contains("ok="), "{flash}");
}

#[tokio::test]
async fn reset_never_touches_the_pending_csr() {
    // reset is the uploaded-pair rollback action: pending is its own
    // lifecycle (discard), and reset must not cost the admin a CSR key.
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    post_csr(&cl, &base, &cookie, &csrf, "keep.example", "").await;
    let key = std::fs::read_to_string(dir.path().join("config/tls/pending-csr-key.pem")).unwrap();
    let flash = post_flash(&cl, &base, &cookie, &csrf, "/admin/tls/reset").await;
    assert!(flash.contains("ok="), "{flash}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("config/tls/pending-csr-key.pem")).unwrap(),
        key
    );
    assert!(dir.path().join("config/tls/pending.json").exists());
}

// ---------- TLS card on the admin System page + boot precedence (Task 6) ----------

#[tokio::test]
async fn admin_system_page_shows_tls_card() {
    let (base, _shutdown, _dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let _ = csrf;
    let page = cl
        .get(format!("{base}/admin/system"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains("TLS certificate"), "card missing: {html}");
    assert!(html.contains("SHA-256"), "fingerprint label missing");
    assert!(html.contains("generated"), "source chip missing");
    assert!(html.contains("/admin/tls/cert"), "download link missing");
    assert!(html.contains("Generate CSR"), "CSR form missing");
    assert!(html.contains("/admin/tls/upload"), "upload form missing");
    // The card's forms carry the app-wide csrf_token field (the name
    // the CSRF middleware reads from native form bodies).
    assert!(
        html.contains("name=\"csrf_token\""),
        "TLS forms must carry the csrf_token field"
    );
}

#[tokio::test]
async fn boot_precedence_uploaded_beats_generated_integration() {
    // Pre-seed an uploaded pair in the tempdir BEFORE boot: the served
    // cert must be the uploaded one, while the generated fallback pair
    // is still created and kept current (Task 2's carried M-1 pin,
    // proven over the wire rather than at the cert.rs unit level).
    let dir = tempfile::tempdir().unwrap();
    let tls_dir = dir.path().join("config/tls");
    std::fs::create_dir_all(&tls_dir).unwrap();
    let kp = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["preseed.example".to_string()]).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "preseed.example");
    let cert = params.self_signed(&kp).unwrap();
    std::fs::write(tls_dir.join("uploaded-cert.pem"), cert.pem()).unwrap();
    std::fs::write(tls_dir.join("uploaded-key.pem"), kp.serialize_pem()).unwrap();
    std::fs::write(
        tls_dir.join("uploaded.json"),
        serde_json::json!({
            "label": "preseed", "uploaded_at": "now", "fingerprint_sha256": "00", "source": "uploaded"
        })
        .to_string(),
    )
    .unwrap();
    let (base, _shutdown) = boot_with_dir(&dir).await;
    let cl = client();
    let (cookie, _csrf) = login_admin(&cl, &base).await;
    // The download serves the UPLOADED cert (uploaded beats generated).
    let body = download_cert(&cl, &base, &cookie).await;
    let uploaded = std::fs::read_to_string(tls_dir.join("uploaded-cert.pem")).unwrap();
    assert_eq!(
        normalize_pem(&body),
        normalize_pem(&uploaded),
        "boot precedence: uploaded pair must serve, not generated"
    );
    // The generated fallback pair also exists (kept current at boot).
    assert!(tls_dir.join("cert.pem").exists());
    // And the card names the uploaded source with its label.
    let page = cl
        .get(format!("{base}/admin/system"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let html = page.text().await.unwrap();
    assert!(html.contains("uploaded"), "source must read uploaded");
    assert!(html.contains("preseed"), "the sidecar label must render");
}

#[tokio::test]
async fn tls_card_escapes_uploaded_cert_subject() {
    // XSS pin: an uploaded cert's subject/issuer/SAN strings are
    // attacker-influenced and must never reach the page unescaped.
    let (base, _shutdown, _dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let payload = "<script>alert(1)</script>";
    let kp = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec![format!("x{payload}.example")]).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, payload);
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, payload);
    let cert = params.self_signed(&kp).unwrap();
    let flash = upload(
        &cl,
        &base,
        &cookie,
        &csrf,
        cert.pem().into_bytes(),
        kp.serialize_pem().into_bytes(),
        payload,
    )
    .await;
    // The upload itself may succeed or warn; either way the card must
    // render the value escaped.
    assert!(flash.contains("ok=") || flash.contains("error="), "{flash}");
    let page = cl
        .get(format!("{base}/admin/system"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let html = page.text().await.unwrap();
    assert!(
        !html.contains("<script>alert(1)</script>"),
        "raw script payload reached the page"
    );
    // The escaped form is present (the payload really did render, just
    // escaped) — proves the escape path ran rather than the value being
    // dropped.
    assert!(
        html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "escaped payload must be what renders"
    );
}

/// M5: `AppState.tls == None` (a process that never served TLS) — the
/// reset handler must flash the restart-required variant instead of
/// claiming a live reload it cannot perform. Driven at the handler
/// level because that is what this test is pinning: the None branch's
/// flash and the on-disk rollback, directly. The route wiring and
/// auth/CSRF gates around this handler are already pinned over HTTP by
/// the `tls: Some` tests (reset_returns_to_generated_pair,
/// csr_endpoints_admin_only), so an HTTP round-trip here would add
/// nothing the None branch needs.
#[tokio::test]
async fn tls_reset_without_live_listener_flashes_restart_required() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let service_key = mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();
    let login = mycelium_auth::login::LoginService::new(store.pool().clone());
    let assets_dir = dir.path().join("assets");
    mycelium_web::assets::scaffold_defaults(&assets_dir).unwrap();
    let state = AppState::new(
        store,
        service_key,
        login,
        assets_dir,
        dir.path().to_path_buf(),
        None, // no live TLS handle
    );
    // A generated pair must exist for reset to fall back to, plus an
    // uploaded pair to remove.
    let tls_dir = dir.path().join("config/tls");
    std::fs::create_dir_all(&tls_dir).unwrap();
    let (cert, key) = gen_pair("reset-none.example");
    std::fs::write(tls_dir.join("cert.pem"), &cert).unwrap();
    std::fs::write(tls_dir.join("key.pem"), &key).unwrap();
    std::fs::write(tls_dir.join("uploaded-cert.pem"), &cert).unwrap();
    std::fs::write(tls_dir.join("uploaded-key.pem"), &key).unwrap();
    std::fs::write(tls_dir.join("uploaded.json"), "{}").unwrap();

    let resp = mycelium_web::api::tls_reset(
        axum::extract::State(state),
        mycelium_auth::rbac::RequireAdmin,
    )
    .await;
    assert_eq!(resp.status(), 303);
    let loc = resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let flash = percent_decode(&loc);
    assert!(
        flash.contains("takes effect after restart"),
        "None-path must flash the restart-required variant: {flash}"
    );
    // The uploaded pair is gone from disk (the rollback is real), the
    // generated pair survives.
    assert!(!tls_dir.join("uploaded-cert.pem").exists());
    assert!(tls_dir.join("cert.pem").exists());
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

// ---------- Regenerate self-signed certificate (SANs from the UI) ----------

/// POST /admin/tls/regenerate with a SAN list; returns the decoded
/// flash query. `sans: None` posts an empty (absent) field.
async fn post_regenerate(
    client: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
    sans: Option<&str>,
) -> String {
    let resp = client
        .post(format!("{base}/admin/tls/regenerate"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .form(&[("sans", sans.unwrap_or(""))])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 303, "regenerate must flash-redirect");
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
async fn regenerate_replaces_cert_key_and_sans_live() {
    // The happy path end to end: new SANs on disk + sidecar, a fresh
    // key, the live listener hot-swapped to the new pair, and the
    // flash naming the new fingerprint.
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let cert_before = std::fs::read_to_string(dir.path().join("config/tls/cert.pem")).unwrap();
    let key_before = std::fs::read_to_string(dir.path().join("config/tls/key.pem")).unwrap();
    let flash = post_regenerate(
        &cl,
        &base,
        &cookie,
        &csrf,
        Some("regen.example, alt.example,regen.example"),
    )
    .await;
    assert!(flash.contains("ok="), "{flash}");
    assert!(flash.contains("fingerprint"), "{flash}");
    // The flash names the new SANs (design: "new SANs + fingerprint").
    assert!(
        flash.contains("regen.example") && flash.contains("alt.example"),
        "flash must name the new SANs: {flash}"
    );
    // Sidecar: trimmed, deduped, order-stable.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("config/tls/sans.txt")).unwrap(),
        "regen.example\nalt.example\n"
    );
    // Fresh cert + key, carrying the new SANs.
    let cert_after = std::fs::read_to_string(dir.path().join("config/tls/cert.pem")).unwrap();
    let key_after = std::fs::read_to_string(dir.path().join("config/tls/key.pem")).unwrap();
    assert_ne!(normalize_pem(&cert_after), normalize_pem(&cert_before));
    assert_ne!(key_after, key_before, "must mint a fresh key");
    // The minted cert actually carries the requested SANs.
    let (_, p) = x509_parser::pem::parse_x509_pem(cert_after.as_bytes()).unwrap();
    let cert = p.parse_x509().unwrap();
    let san_text = format!(
        "{:?}",
        cert.extensions()
            .iter()
            .find(|e| e.oid == x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME)
            .expect("SAN extension")
    );
    assert!(san_text.contains("regen.example"), "{san_text}");
    assert!(san_text.contains("alt.example"), "{san_text}");
    // Key perms stay 0600 across regeneration.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(dir.path().join("config/tls/key.pem"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // The download endpoint serves the NEW generated pair (the live
    // reload happened; nothing uploaded is in the way).
    let body = download_cert(&cl, &base, &cookie).await;
    assert_eq!(normalize_pem(&body), normalize_pem(&cert_after));
    assert_ne!(normalize_pem(&body), normalize_pem(&cert_before));
    // And the listener still handshakes after the reload.
    let live = reqwest::Client::builder()
        .use_rustls_tls()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = live
        .get(format!("{base}/admin/tls/cert"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("live TLS handshake after regenerate");
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn regenerate_survives_restart_via_sidecar() {
    // Sidecar-wins semantics over the wire: after a UI regeneration,
    // boot's ensure_managed_pair(tls_dir, boot_sans) must NOT replace
    // the UI's pair even though the requested set (localhost, from
    // the test harness boot) differs. Proven by calling the same
    // function serve wires in, over the post-regenerate disk state.
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let flash = post_regenerate(&cl, &base, &cookie, &csrf, Some("ui.example")).await;
    assert!(flash.contains("ok="), "{flash}");
    let cert_after_ui = std::fs::read_to_string(dir.path().join("config/tls/cert.pem")).unwrap();
    // The boot path: the env/flag set (localhost in this harness) is
    // only an initializer — the sidecar keeps the UI's pair.
    mycelium_web::cert::ensure_managed_pair(
        &dir.path().join("config/tls"),
        &["localhost".to_string()],
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("config/tls/cert.pem")).unwrap(),
        cert_after_ui,
        "boot must not regenerate a UI-chosen pair"
    );
}

#[tokio::test]
async fn regenerate_empty_sans_is_localhost_not_an_error() {
    // Empty field → sans_from_env semantics: localhost only. NOT the
    // empty-list refusal (the handler applies the default first).
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let flash = post_regenerate(&cl, &base, &cookie, &csrf, None).await;
    assert!(flash.contains("ok="), "{flash}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("config/tls/sans.txt")).unwrap(),
        "localhost\n"
    );
    // The minted cert carries localhost.
    let cert = std::fs::read_to_string(dir.path().join("config/tls/cert.pem")).unwrap();
    let (_, p) = x509_parser::pem::parse_x509_pem(cert.as_bytes()).unwrap();
    let parsed = p.parse_x509().unwrap();
    let san_text = format!(
        "{:?}",
        parsed
            .extensions()
            .iter()
            .find(|e| e.oid == x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME)
            .expect("SAN extension")
    );
    assert!(san_text.contains("localhost"), "{san_text}");
}

#[tokio::test]
async fn regenerate_refuses_over_limit_san_list() {
    // Security review F1: an over-limit SAN list (more than 64 names)
    // is refused with a clear flash and nothing on disk changes — no
    // giant permanent cert minted from a 2 MiB form body.
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let cert_before = std::fs::read_to_string(dir.path().join("config/tls/cert.pem")).unwrap();
    let sidecar_before = std::fs::read_to_string(dir.path().join("config/tls/sans.txt")).unwrap();
    let many: Vec<String> = (0..100).map(|i| format!("n{i}.example")).collect();
    let flash = post_regenerate(&cl, &base, &cookie, &csrf, Some(&many.join(","))).await;
    assert!(flash.contains("error="), "{flash}");
    assert!(flash.contains("too many"), "{flash}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("config/tls/cert.pem")).unwrap(),
        cert_before,
        "refused list must not touch the cert"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("config/tls/sans.txt")).unwrap(),
        sidecar_before,
        "refused list must not touch the sidecar"
    );
}

#[tokio::test]
async fn regenerate_keeps_uploaded_cert_active() {
    // Uploaded beats generated: a regeneration under an active upload
    // mints the new fallback pair but the LIVE listener must keep
    // serving the uploaded cert — and the flash must say so.
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;
    let (cert, key) = gen_pair("still-up.example");
    let flash = upload(&cl, &base, &cookie, &csrf, cert.clone(), key, "up").await;
    assert!(flash.contains("ok="), "{flash}");
    let flash = post_regenerate(&cl, &base, &cookie, &csrf, Some("fallback.example")).await;
    assert!(flash.contains("ok="), "{flash}");
    // The generated pair WAS regenerated (fresh fallback)...
    assert_eq!(
        std::fs::read_to_string(dir.path().join("config/tls/sans.txt")).unwrap(),
        "fallback.example\n"
    );
    // ...but the download still serves the UPLOADED cert.
    let body = download_cert(&cl, &base, &cookie).await;
    assert_eq!(
        normalize_pem(&body),
        normalize_pem(&String::from_utf8_lossy(&cert)),
        "uploaded pair must stay active under regeneration"
    );
}

#[tokio::test]
async fn regenerate_admin_and_csrf_gated() {
    // Unauthenticated → the login gate. Logged-in non-admin → 403
    // (RequireAdmin). Missing CSRF → 403 (middleware, outside
    // RequireAdmin). None of these may touch the disk.
    let (base, _shutdown, dir) = boot().await;
    let cl = client();
    let (admin_cookie, _admin_csrf) = login_admin(&cl, &base).await;
    let cert_before = std::fs::read_to_string(dir.path().join("config/tls/cert.pem")).unwrap();

    // Unauthenticated: the session gate redirects to /login.
    let resp = cl
        .post(format!("{base}/admin/tls/regenerate"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 303);
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        "/login"
    );

    // Logged-in non-admin: 403 from RequireAdmin.
    let (plain_cookie, _plain_csrf) = login_plain_user(&cl, &base, &admin_cookie).await;
    let resp = cl
        .post(format!("{base}/admin/tls/regenerate"))
        .header("cookie", &plain_cookie)
        .form(&[("sans", "nope.example")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // Admin but missing CSRF token → 403 before the handler runs.
    let resp = cl
        .post(format!("{base}/admin/tls/regenerate"))
        .header("cookie", &admin_cookie)
        .form(&[("sans", "nope.example")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // Nothing regenerated in any refusal path.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("config/tls/cert.pem")).unwrap(),
        cert_before,
        "refused requests must not regenerate"
    );
}

#[tokio::test]
async fn regenerate_form_is_on_the_tls_card() {
    let (base, _shutdown, _dir) = boot().await;
    let cl = client();
    let (cookie, _csrf) = login_admin(&cl, &base).await;
    let page = cl
        .get(format!("{base}/admin/system"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(
        html.contains("/admin/tls/regenerate"),
        "regenerate form action missing: {html}"
    );
    assert!(
        html.contains("Regenerate self-signed certificate"),
        "regenerate form heading missing"
    );
}

#[tokio::test]
async fn tls_regenerate_without_live_listener_flashes_restart_required() {
    // The None branch (a process that never served TLS): disk state is
    // real but not live — flash the restart-required variant instead
    // of claiming a hot-swap that cannot happen. Driven at the
    // handler level for the same reasons as the reset M5 test above.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let service_key = mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();
    let login = mycelium_auth::login::LoginService::new(store.pool().clone());
    let assets_dir = dir.path().join("assets");
    mycelium_web::assets::scaffold_defaults(&assets_dir).unwrap();
    let state = AppState::new(
        store,
        service_key,
        login,
        assets_dir,
        dir.path().to_path_buf(),
        None, // no live TLS handle
    );
    let resp = mycelium_web::api::tls_regenerate(
        axum::extract::State(state),
        mycelium_auth::rbac::RequireAdmin,
        axum::Form(mycelium_web::api::RegenerateForm {
            sans: "none.example".to_string(),
        }),
    )
    .await;
    assert_eq!(resp.status(), 303);
    let loc = resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let flash = percent_decode(&loc);
    assert!(
        flash.contains("takes effect after restart"),
        "None-path must flash the restart-required variant: {flash}"
    );
    // The pair on disk was regenerated with the requested SANs.
    let tls_dir = dir.path().join("config/tls");
    assert_eq!(
        std::fs::read_to_string(tls_dir.join("sans.txt")).unwrap(),
        "none.example\n"
    );
    let cert = std::fs::read_to_string(tls_dir.join("cert.pem")).unwrap();
    let (_, p) = x509_parser::pem::parse_x509_pem(cert.as_bytes()).unwrap();
    let parsed = p.parse_x509().unwrap();
    let san_text = format!(
        "{:?}",
        parsed
            .extensions()
            .iter()
            .find(|e| e.oid == x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME)
            .expect("SAN extension")
    );
    assert!(san_text.contains("none.example"), "{san_text}");
}
