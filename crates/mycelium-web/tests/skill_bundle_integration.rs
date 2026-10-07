//! Task 8 integration tests: the skill bundle download endpoint
//! (GET /api/v1/skills/{slug}/bundle) over the real HTTPS server —
//! deterministic zip bytes for the seeded global skill, 404 for legacy
//! flat skills and unknown slugs, private-skill visibility (owner-only),
//! and hostile-hub containment (a traversal manifest never yields a
//! downloadable bundle).
//!
//! Harness copied from tests/phase8_integration.rs (boot/client/login
//! helpers; `create_local` does not set must_change_password, so plain
//! form logins work with no forced-change dance), extended in two ways:
//! the packaged skills are seeded here (production seeds from main.rs —
//! a test owns its store), and the store + service key are handed back
//! so tests can write the shelves directly.

use std::io::Read;
use std::net::SocketAddr;

use mycelium_auth::login::LoginService;
use mycelium_crypto::keys::ServiceKey;
use mycelium_store::{ConceptStore, FileRepo, Scope, Store};
use mycelium_web::AppState;

/// Known admin password for the pre-seeded account (tests log in with it).
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";

/// The seeded hub manifest's md5 for scripts/convert.py (pinned against
/// the embedded bytes by packaged_skills::asset_tests).
const CONVERT_PY_MD5: &str = "80ddf66fe575e11e0e1c8c64b2bb3b6f";

async fn boot() -> (
    String,
    tokio_util::sync::CancellationToken,
    tempfile::TempDir,
    Store,
    ServiceKey,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let service_key = mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();
    // Production boots seed the packaged skills from main.rs; the test
    // owns this store, so seed BEFORE the server starts serving.
    mycelium_web::packaged_skills::seed_packaged_skills(&store, &service_key, &store.skills_dir())
        .await
        .unwrap();
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
    let state = AppState::new(store.clone(), service_key.clone(), login, assets_dir);

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
        store,
        service_key,
    )
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

/// Log in over the form (create_local users carry must_change_password
/// = 0, so no forced-change redirect); returns the session cookie.
/// Bundle GETs need no CSRF token (the middleware exempts GET).
async fn login(client: &reqwest::Client, base: &str, username: &str, password: &str) -> String {
    let resp = client
        .post(format!("{base}/login"))
        .form(&[("username", username), ("password", password)])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 303, "login {username}");
    cookie_from(&resp)
}

/// md5 hex digest (the suite's idiom: md-5 + hex crates).
fn md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// GET /api/v1/skills/pdf-to-markdown/bundle twice: 200 with the
/// deterministic contract headers (application/zip, no-store,
/// attachment filename), byte-identical bodies, and the seeded
/// bundle's exact entry set in the assembler's fixed order with the
/// manifest md5 verified on a script payload.
#[tokio::test]
async fn bundle_download_serves_deterministic_zip() {
    let (base, shutdown, _dir, _store, _svc) = boot().await;
    let client = client();
    let cookie = login(&client, &base, "admin", ADMIN_PASSWORD).await;

    let url = format!("{base}/api/v1/skills/pdf-to-markdown/bundle");
    let resp1 = client
        .get(&url)
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp1.status(), 200);
    assert_eq!(
        resp1
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/zip"
    );
    assert_eq!(
        resp1
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap(),
        "no-store, no-cache, must-revalidate"
    );
    assert_eq!(
        resp1
            .headers()
            .get(reqwest::header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap(),
        "attachment; filename=\"pdf-to-markdown-skill.zip\""
    );
    let bytes1 = resp1.bytes().await.unwrap().to_vec();

    // GET twice → byte-identical (fixed order, mtime, perms, deflate).
    let resp2 = client
        .get(&url)
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    let bytes2 = resp2.bytes().await.unwrap().to_vec();
    assert_eq!(bytes1, bytes2, "two GETs must be byte-identical");

    // The archive: exact entry set, in the assembler's deterministic
    // order (SKILL.md, companions sorted, payloads sorted).
    let mut za = zip::ZipArchive::new(std::io::Cursor::new(bytes1)).unwrap();
    let names: Vec<String> = za.file_names().map(str::to_string).collect();
    assert_eq!(
        names,
        [
            "SKILL.md",
            "conventions.md",
            "references/docling-options.md",
            "LICENSE.txt",
            "scripts/convert.py",
            "scripts/docling_page_span.py",
            "scripts/html_cleanup.py",
            "scripts/inspect_pdf.py",
            "scripts/postprocess.py",
            "scripts/requirements.txt",
            "scripts/setup_venv.sh",
        ]
    );
    // Script payload bytes md5-verify against the hub manifest.
    let mut entry = za.by_name("scripts/convert.py").unwrap();
    let mut py = Vec::new();
    entry.read_to_end(&mut py).unwrap();
    assert_eq!(md5_hex(&py), CONVERT_PY_MD5);
    // Entry-level determinism, end-to-end (the same contract the unit
    // tests in skill_zip.rs pin: deflate, 0644 (+S_IFREG), 1980-01-01).
    assert_eq!(entry.compression(), zip::CompressionMethod::Deflated);
    assert_eq!(entry.unix_mode(), Some(0o100_644));
    assert_eq!(entry.last_modified(), Some(zip::DateTime::DEFAULT));

    shutdown.cancel();
}

/// A slug naming only a legacy flat skill (a root-level `/x.md`
/// concept, no `/<slug>/skill.md` hub) is not bundleable — 404, the
/// same answer as an unknown slug.
#[tokio::test]
async fn bundle_of_legacy_flat_skill_is_404() {
    let (base, shutdown, _dir, store, svc) = boot().await;
    // A real legacy flat skill in the global shelf.
    let cs = ConceptStore::for_service(&store, svc.clone(), &store.skills_dir(), "skills");
    cs.put(
        &mycelium_core::concept::Concept::parse(
            "/pdf-to-markdown-old.md",
            "---\ntype: Skill\ntitle: pdf-to-markdown-old\n---\n\nlegacy flat skill",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let client = client();
    let cookie = login(&client, &base, "admin", ADMIN_PASSWORD).await;
    let resp = client
        .get(format!("{base}/api/v1/skills/pdf-to-markdown-old/bundle"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "a legacy flat skill is not bundleable");
    // Unknown slugs are indistinguishable from flat skills.
    let unknown = client
        .get(format!("{base}/api/v1/skills/no-such-skill/bundle"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 404);
    shutdown.cancel();
}

/// Private nested skills are owner-only: alice's bundle (hub + companion
/// + script payload under her user scope) downloads for alice; bob gets
/// the same 404 as for an unknown slug.
#[tokio::test]
async fn private_skill_bundle_requires_same_user() {
    let (base, shutdown, _dir, store, _svc) = boot().await;
    let users = mycelium_auth::UserStore::new(store.pool().clone());
    let alice = users
        .create_local(
            "alice",
            "alice@localhost.local",
            "alice password twenty chars!",
            mycelium_auth::Role::User,
        )
        .await
        .unwrap();
    users
        .create_local(
            "bob",
            "bob@localhost.local",
            "bob password twenty chars!!!",
            mycelium_auth::Role::User,
        )
        .await
        .unwrap();

    // Alice's private nested skill, written through the exact scope
    // the endpoint reads back: ConceptStore::for_user's FileRepo base
    // (her user dir) + Scope::User(her master key).
    let script: &[u8] = b"print('private ok')\n";
    let hub_md = format!(
        "---\ntype: Skill\ntitle: my-private-skill\nskill:\n  version: 1\n  files:\n    - {{path: notes.md, role: reference}}\n    - {{path: scripts/run.py, role: script, md5: {}}}\n---\n\nPrivate skill body.",
        md5_hex(script)
    );
    let cs = ConceptStore::for_user(&store, alice.record.id, alice.master_key.clone());
    cs.put(&mycelium_core::concept::Concept::parse("/my-private-skill/skill.md", &hub_md).unwrap())
        .await
        .unwrap();
    cs.put(
        &mycelium_core::concept::Concept::parse(
            "/my-private-skill/notes.md",
            "---\ntype: Note\ntitle: Notes\n---\n\nHow to use.",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    FileRepo::new(store.user_dir(alice.record.id))
        .write(
            "/my-private-skill/scripts/run.py",
            script,
            &Scope::User(alice.master_key.clone()),
        )
        .await
        .unwrap();

    let client = client();
    // Bob: invisible — the same 404 as an unknown slug.
    let bob_cookie = login(&client, &base, "bob", "bob password twenty chars!!!").await;
    let bob_resp = client
        .get(format!("{base}/api/v1/skills/my-private-skill/bundle"))
        .header("cookie", &bob_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(
        bob_resp.status(),
        404,
        "another user's private skill is invisible"
    );

    // Alice: her bundle, with her payload bytes.
    let alice_cookie = login(&client, &base, "alice", "alice password twenty chars!").await;
    let resp = client
        .get(format!("{base}/api/v1/skills/my-private-skill/bundle"))
        .header("cookie", &alice_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let bytes = resp.bytes().await.unwrap().to_vec();
    let mut za = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let names: Vec<String> = za.file_names().map(str::to_string).collect();
    assert_eq!(names, ["SKILL.md", "notes.md", "scripts/run.py"]);
    let mut entry = za.by_name("scripts/run.py").unwrap();
    let mut got = Vec::new();
    entry.read_to_end(&mut got).unwrap();
    assert_eq!(got, script);

    shutdown.cancel();
}

/// Hostile-hub containment (the brief's traversal scenario, pinned to
/// the REAL behavior — controller ruling): Task 3's seed validates the
/// EMBEDDED packaged concepts and fails fast, so a hostile hub cannot
/// arrive through the seed; the only writable path is the store itself,
/// where Concept::parse accepts it (parse does not validate manifests)
/// and the write path checks only reserved basenames (both proven by
/// Task 7's companion_with_traversal_path_is_rejected). Assembly then
/// rejects it at SkillManifest::validated() → the endpoint contract
/// maps every non-NotBundle error to a logged generic 500: the hostile
/// hub NEVER yields a downloadable bundle, and the shelf's real bundle
/// keeps serving.
#[tokio::test]
async fn traversal_manifest_hub_never_yields_a_bundle() {
    let (base, shutdown, _dir, store, svc) = boot().await;
    let cs = ConceptStore::for_service(&store, svc.clone(), &store.skills_dir(), "skills");
    cs.put(
        &mycelium_core::concept::Concept::parse(
            "/evil/skill.md",
            "---\ntype: Skill\ntitle: evil\nskill:\n  version: 1\n  files:\n    - {path: ../escape, role: script}\n---\n\nHostile hub.",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let client = client();
    let cookie = login(&client, &base, "admin", ADMIN_PASSWORD).await;
    let resp = client
        .get(format!("{base}/api/v1/skills/evil/bundle"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    // Traversal is not NotBundle: logged server-side, generic 500 —
    // never a zip body, never the path in the response.
    assert_eq!(resp.status(), 500);
    assert_eq!(
        resp.text().await.unwrap(),
        "{\"error\":\"internal error\"}",
        "generic error only — no internal detail"
    );
    // The hostile hub does not poison the rest of the shelf.
    let ok = client
        .get(format!("{base}/api/v1/skills/pdf-to-markdown/bundle"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    shutdown.cancel();
}
