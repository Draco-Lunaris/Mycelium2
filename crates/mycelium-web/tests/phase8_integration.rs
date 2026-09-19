//! Phase 8 integration test: global bookshelves, browsing, cross-scope
//! search, and skills authorization.
//!
//! Boots the full HTTPS server, then verifies: the /books browse page
//! respects shelf visibility (global-read for users, all for admins);
//! library concept reads are shelf-visibility gated; search spans user
//! bundle + global skills + library but never leaks admin-private
//! books to users; global skills are admin-write, user-read; private
//! skills are owner-only.

use std::net::SocketAddr;

use mycelium_auth::login::LoginService;
use mycelium_store::Store;
use mycelium_web::AppState;

/// Known admin password for the pre-seeded account (tests log in with it).
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";

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
const BOOK: &str = "\
# Chapter One

Zebra intro text.

# Chapter Two

Second chapter text.
";

#[tokio::test]
async fn global_bookshelves_browse_search_and_skills() {
    let (base, shutdown, _dir) = boot().await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // 1. Create a global-read shelf and a private shelf.
    for (name, global) in [("Public Shelf", "1"), ("Secret Shelf", "0")] {
        let create = client
            .post(format!("{base}/admin/bookshelves"))
            .header("cookie", &cookie)
            .header("x-csrf-token", &csrf)
            .form(&[("name", name), ("global", global)])
            .send()
            .await
            .unwrap();
        assert_eq!(create.status(), 303);
    }

    // 2. Upload a book to each shelf (multipart, form-field CSRF).
    let upload = |shelf: &'static str, slug: &'static str, title: &'static str, csrf: &str| {
        let boundary = "bnd8";
        let mut body = Vec::new();
        for (name, value) in [
            ("csrf_token", csrf),
            ("bookshelf", shelf),
            ("slug", slug),
            ("title", title),
        ] {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(value.as_bytes());
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            "Content-Disposition: form-data; name=\"file\"; filename=\"book.md\"\r\n".as_bytes(),
        );
        body.extend_from_slice(b"Content-Type: text/markdown\r\n\r\n");
        body.extend_from_slice(BOOK.as_bytes());
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        body
    };
    for (shelf, slug, title) in [
        ("Public Shelf", "public-book", "Public Book"),
        ("Secret Shelf", "secret-book", "Secret Book"),
    ] {
        let body = upload(shelf, slug, title, &csrf);
        let resp = client
            .post(format!("{base}/api/v1/ingest"))
            .header("cookie", &cookie)
            .header("content-type", "multipart/form-data; boundary=bnd8")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202, "{slug} upload");
        let json: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(json["status"], "done", "{slug} ingest");
    }

    // 3. Seed a global skill (admin, via the concept form scope=skills).
    let skill_md = "---\ntype: Skill\ntitle: Deploy Service\n---\n\n1. build\n2. ship\n";
    let create_skill = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("path", "/deploy-service.md"),
            ("markdown", skill_md),
            ("scope", "skills"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(create_skill.status(), 303, "admin creates global skill");

    // 4. Create a regular user and log in as them.
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
    let mallory_cookie = cookie_from(&login_mallory);
    let mallory_csrf = csrf_from_page(&client, &format!("{base}/"), &mallory_cookie).await;

    // 5. Browse page: mallory sees ONLY the global-read shelf.
    let browse = client
        .get(format!("{base}/books"))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(browse.status(), 200);
    let browse_html = browse.text().await.unwrap();
    assert!(browse_html.contains("Public Shelf"));
    assert!(browse_html.contains("public-book"));
    assert!(
        !browse_html.contains("Secret Shelf"),
        "private shelf must not appear for users"
    );
    assert!(
        !browse_html.contains("secret-book"),
        "private-shelf book must not appear for users"
    );
    // Admin sees both.
    let admin_browse = client
        .get(format!("{base}/books"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let admin_html = admin_browse.text().await.unwrap();
    assert!(admin_html.contains("Public Shelf"));
    assert!(admin_html.contains("Secret Shelf"));

    // 6. Library concept read: public book hub OK for mallory; secret
    //    book hub FORBIDDEN.
    let public_hub = client
        .get(format!(
            "{base}/concept?path=/public-book/book.md&scope=library"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(public_hub.status(), 200);
    assert!(public_hub.text().await.unwrap().contains("Public Book"));
    let secret_hub = client
        .get(format!(
            "{base}/concept?path=/secret-book/book.md&scope=library"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(
        secret_hub.status(),
        403,
        "private-shelf catalog must be admin-only"
    );
    // Admin can read the secret hub.
    let admin_secret = client
        .get(format!(
            "{base}/concept?path=/secret-book/book.md&scope=library"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(admin_secret.status(), 200);

    // 7. Search spans user + skills + library, but never leaks the
    //    private book. Catalog concepts (hub + chapter titles) are
    //    indexed; the raw stack text is not (passages are read via
    //    book:// anchors, not search).
    let search = client
        .get(format!("{base}/api/v1/search?q=chapter&global=1"))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(search.status(), 200);
    let results: serde_json::Value = search.json().await.unwrap();
    let arr = results.as_array().unwrap();
    assert!(
        arr.iter().any(|r| r["scope"] == "library"
            && r["concept_path"]
                .as_str()
                .unwrap()
                .starts_with("/public-book/")),
        "public book must be searchable: {results}"
    );
    assert!(
        !arr.iter().any(|r| r["concept_path"]
            .as_str()
            .unwrap()
            .starts_with("/secret-book/")),
        "private book must never leak into user search: {results}"
    );
    // The global skill is searchable too.
    let skill_search = client
        .get(format!("{base}/api/v1/search?q=deploy&global=1"))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    let skill_results: serde_json::Value = skill_search.json().await.unwrap();
    assert!(
        skill_results
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["scope"] == "skills" && r["concept_path"] == "/deploy-service.md"),
        "global skill must be searchable: {skill_results}"
    );
    // Admin search DOES see the secret book.
    let admin_search = client
        .get(format!("{base}/api/v1/search?q=chapter&global=1"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let admin_results: serde_json::Value = admin_search.json().await.unwrap();
    assert!(
        admin_results
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["concept_path"]
                .as_str()
                .unwrap()
                .starts_with("/secret-book/")),
        "admin search must see private-shelf books: {admin_results}"
    );

    // 8. Skills page: mallory sees the global skill (read-only), and
    //    the admin edit link is absent for users.
    let skills_page = client
        .get(format!("{base}/skills"))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    let skills_html = skills_page.text().await.unwrap();
    assert!(skills_html.contains("Deploy Service"));
    assert!(
        !skills_html.contains("New global skill"),
        "users must not see the admin create-global-skill link"
    );
    // Admin sees the edit link.
    let admin_skills = client
        .get(format!("{base}/skills"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert!(
        admin_skills
            .text()
            .await
            .unwrap()
            .contains("New global skill")
    );

    // 9. Global skill write authz: mallory cannot create/edit/delete.
    let mallory_skill = client
        .post(format!("{base}/concept"))
        .header("cookie", &mallory_cookie)
        .header("x-csrf-token", &mallory_csrf)
        .form(&[
            ("path", "/evil.md"),
            ("markdown", "---\ntype: Skill\ntitle: Evil\n---\n\nx"),
            ("scope", "skills"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(
        mallory_skill.status(),
        403,
        "user cannot write global skills"
    );
    let mallory_edit = client
        .post(format!("{base}/concept"))
        .header("cookie", &mallory_cookie)
        .header("x-csrf-token", &mallory_csrf)
        .form(&[
            ("path", "/deploy-service.md"),
            ("markdown", "---\ntype: Skill\ntitle: Hacked\n---\n\nx"),
            ("scope", "skills"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(mallory_edit.status(), 403, "user cannot edit global skills");
    let mallory_delete = client
        .post(format!("{base}/concept/delete"))
        .header("cookie", &mallory_cookie)
        .header("x-csrf-token", &mallory_csrf)
        .form(&[("path", "/deploy-service.md"), ("scope", "skills")])
        .send()
        .await
        .unwrap();
    assert_eq!(
        mallory_delete.status(),
        403,
        "user cannot delete global skills"
    );
    // Library writes are forbidden for everyone (ingest-only).
    let mallory_lib = client
        .post(format!("{base}/concept"))
        .header("cookie", &mallory_cookie)
        .header("x-csrf-token", &mallory_csrf)
        .form(&[
            ("path", "/public-book/book.md"),
            ("markdown", "---\ntype: Book\ntitle: Hacked\n---\n\nx"),
            ("scope", "library"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(mallory_lib.status(), 403, "library is read-only");
    let admin_lib = client
        .post(format!("{base}/concept"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("path", "/public-book/book.md"),
            ("markdown", "---\ntype: Book\ntitle: Hacked\n---\n\nx"),
            ("scope", "library"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(
        admin_lib.status(),
        403,
        "library is read-only even for admins"
    );

    // 10. Private skills: mallory can create one in her own bundle.
    let private_skill = client
        .post(format!("{base}/concept"))
        .header("cookie", &mallory_cookie)
        .header("x-csrf-token", &mallory_csrf)
        .form(&[
            ("path", "/my-private-skill.md"),
            (
                "markdown",
                "---\ntype: Skill\ntitle: My Private Skill\n---\n\nsteps",
            ),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(private_skill.status(), 303);
    // It appears in her skills page (private section)...
    let skills_html = client
        .get(format!("{base}/skills"))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(skills_html.contains("My Private Skill"));
    // ...but NOT in the admin's view of the global skills shelf.
    let admin_skills_html = client
        .get(format!("{base}/skills"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!admin_skills_html.contains("My Private Skill"));

    // 11. The skills page does not list book catalogs (namespace split).
    assert!(
        !skills_html.contains("Public Book"),
        "book catalogs must not appear on the skills page"
    );

    // 12. Reflected injection: a hostile `scope` query value must be
    //     HTML-escaped in the new-concept form (regression for the
    //     unescaped hidden-input attribute).
    let hostile_scope = client
        .get(format!(
            "{base}/concept?new=1&scope=%22%3E%3Cscript%3Ealert(1)%3C%2Fscript%3E"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(hostile_scope.status(), 200);
    let hostile_html = hostile_scope.text().await.unwrap();
    assert!(
        !hostile_html.contains("\"><script>alert(1)</script>"),
        "raw scope value must not reach the page unescaped"
    );
    assert!(
        hostile_html.contains("&quot;&gt;&lt;script&gt;"),
        "scope value must be HTML-escaped"
    );

    shutdown.cancel();
}
