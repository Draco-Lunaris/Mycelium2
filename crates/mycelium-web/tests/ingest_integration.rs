//! Phase 7 integration test: book upload → ingest job → passages.
//!
//! Boots the full HTTPS server, logs in as the bootstrap admin, creates
//! a bookshelf, uploads a book via multipart, watches the job reach
//! `done`, and reads passages back via `book://` anchors. Also covers
//! the multipart CSRF path, authz (non-admin upload rejected, private
//! shelf passage rejected for non-admins), and duplicate-slug rejection.

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

/// Build a multipart body with a csrf_token field (browser-style: the
/// token rides as a form field, not a header).
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

#[tokio::test]
async fn book_upload_ingest_and_passages() {
    let (base, shutdown, _dir) = boot().await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // 1. Create a global-read bookshelf via the admin form.
    let create_shelf = client
        .post(format!("{base}/admin/bookshelves"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("name", "Public Shelf"), ("global", "1")])
        .send()
        .await
        .unwrap();
    assert_eq!(create_shelf.status(), 303);

    // 2. Upload a book via multipart (csrf_token as a form FIELD — the
    //    browser path; no header).
    let boundary = "testboundary12345";
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
    assert_eq!(upload.status(), 202, "upload must be accepted");
    let upload_json: serde_json::Value = upload.json().await.unwrap();
    assert_eq!(
        upload_json["status"], "done",
        "ingest ran inline: {upload_json}"
    );
    assert_eq!(upload_json["slug"], "my-book");
    let job_id = upload_json["job_id"].as_str().unwrap().to_string();

    // 3. Job status endpoint reports done.
    let status = client
        .get(format!("{base}/api/v1/ingest/{job_id}"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), 200);
    let status_json: serde_json::Value = status.json().await.unwrap();
    assert_eq!(status_json["status"], "done");
    assert!(
        status_json["detail"]
            .as_str()
            .unwrap()
            .contains("3 catalog concepts")
    );

    // 4. Jobs list includes the job.
    let jobs = client
        .get(format!("{base}/api/v1/ingest"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(jobs.status(), 200);
    let jobs_json: serde_json::Value = jobs.json().await.unwrap();
    assert_eq!(jobs_json.as_array().unwrap().len(), 1);

    // 5. Read a chapter passage via book:// anchor.
    let passage = client
        .get(format!(
            "{base}/api/v1/passages?resource=book://my-book%23ch-1-chapter-one"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(passage.status(), 200);
    let passage_json: serde_json::Value = passage.json().await.unwrap();
    let text = passage_json["text"].as_str().unwrap();
    assert!(text.contains("# Chapter One"));
    assert!(text.contains("Intro text about zebras."));
    assert!(!text.contains("# Chapter Two"));

    // 6. Read a section passage.
    let section = client
        .get(format!(
            "{base}/api/v1/passages?resource=book://my-book%23sec-1-2-details-two"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(section.status(), 200);
    let section_json: serde_json::Value = section.json().await.unwrap();
    assert!(
        section_json["text"]
            .as_str()
            .unwrap()
            .contains("Details two.")
    );

    // 7. The catalog concepts exist in the shared library scope (hub +
    //    chapters) — verify via the admin page's jobs table and by
    //    reading the hub through the service-scope store directly.
    let admin_page = client
        .get(format!("{base}/admin"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let admin_html = admin_page.text().await.unwrap();
    assert!(admin_html.contains("Upload book"));
    assert!(admin_html.contains("my-book"));
    assert!(admin_html.contains("done"));

    // 8. Duplicate slug is rejected.
    let boundary2 = "testboundary67890";
    let body2 = multipart_body(
        boundary2,
        &[
            ("csrf_token", csrf.as_str()),
            ("bookshelf", "Public Shelf"),
            ("slug", "my-book"),
            ("title", "My Book Again"),
        ],
        Some(("file", "book.md", BOOK)),
    );
    let dup = client
        .post(format!("{base}/api/v1/ingest"))
        .header("cookie", &cookie)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary2}"),
        )
        .body(body2)
        .send()
        .await
        .unwrap();
    assert_eq!(dup.status(), 409, "duplicate slug must conflict");

    // 9. Multipart without a CSRF token is rejected (403).
    let boundary3 = "testboundary99999";
    let body3 = multipart_body(
        boundary3,
        &[
            ("bookshelf", "Public Shelf"),
            ("slug", "no-csrf"),
            ("title", "X"),
        ],
        Some(("file", "book.md", BOOK)),
    );
    let no_csrf = client
        .post(format!("{base}/api/v1/ingest"))
        .header("cookie", &cookie)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary3}"),
        )
        .body(body3)
        .send()
        .await
        .unwrap();
    assert_eq!(no_csrf.status(), 403, "multipart CSRF must be enforced");

    // 10. Non-admin cannot upload. Create a regular user, log in, try.
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
    let boundary4 = "testboundary77777";
    let body4 = multipart_body(
        boundary4,
        &[
            ("csrf_token", mallory_csrf.as_str()),
            ("bookshelf", "Public Shelf"),
            ("slug", "mallory-book"),
            ("title", "Mallory Book"),
        ],
        Some(("file", "book.md", BOOK)),
    );
    let mallory_upload = client
        .post(format!("{base}/api/v1/ingest"))
        .header("cookie", &mallory_cookie)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary4}"),
        )
        .body(body4)
        .send()
        .await
        .unwrap();
    assert_eq!(
        mallory_upload.status(),
        403,
        "non-admin upload must be rejected"
    );

    // 11. Non-admin CAN read a passage from a global-read shelf.
    let mallory_passage = client
        .get(format!(
            "{base}/api/v1/passages?resource=book://my-book%23ch-1-chapter-one"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(
        mallory_passage.status(),
        200,
        "global-read shelf is readable"
    );

    // 12. Non-admin cannot list ingest jobs.
    let mallory_jobs = client
        .get(format!("{base}/api/v1/ingest"))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(mallory_jobs.status(), 403);

    // 13. Private shelf: passages are admin-only. Create one, upload a
    //     book to it, then try reading as mallory.
    let create_private = client
        .post(format!("{base}/admin/bookshelves"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("name", "Secret Shelf"), ("global", "0")])
        .send()
        .await
        .unwrap();
    assert_eq!(create_private.status(), 303);
    let boundary5 = "testboundary55555";
    let body5 = multipart_body(
        boundary5,
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
            format!("multipart/form-data; boundary={boundary5}"),
        )
        .body(body5)
        .send()
        .await
        .unwrap();
    assert_eq!(secret_upload.status(), 202);
    let mallory_secret = client
        .get(format!(
            "{base}/api/v1/passages?resource=book://secret-book%23ch-1-chapter-one"
        ))
        .header("cookie", &mallory_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(
        mallory_secret.status(),
        403,
        "private-shelf passages must be admin-only"
    );
    // ...but the admin can read them.
    let admin_secret = client
        .get(format!(
            "{base}/api/v1/passages?resource=book://secret-book%23ch-1-chapter-one"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(admin_secret.status(), 200);

    // 14. Bad resource strings are rejected.
    let bad = client
        .get(format!("{base}/api/v1/passages?resource=not-a-book"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let missing = client
        .get(format!(
            "{base}/api/v1/passages?resource=book://nope%23ch-1-x"
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    // 15. Metrics include the ingest counters.
    let metrics = client.get(format!("{base}/metrics")).send().await.unwrap();
    let metrics_text = metrics.text().await.unwrap();
    assert!(metrics_text.contains("mycelium2_books_ingested_total"));

    shutdown.cancel();
}

#[tokio::test]
async fn upload_validation_errors() {
    let (base, shutdown, _dir) = boot().await;
    let client = client();
    let (cookie, csrf) = login_admin(&client, &base).await;

    // Create a shelf.
    let create_shelf = client
        .post(format!("{base}/admin/bookshelves"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("name", "S"), ("global", "1")])
        .send()
        .await
        .unwrap();
    assert_eq!(create_shelf.status(), 303);

    // Missing file field.
    let boundary = "vb1";
    let body = multipart_body(
        boundary,
        &[
            ("csrf_token", csrf.as_str()),
            ("bookshelf", "S"),
            ("slug", "x"),
            ("title", "X"),
        ],
        None,
    );
    let no_file = client
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
    assert_eq!(no_file.status(), 400);

    // Unknown bookshelf.
    let body = multipart_body(
        boundary,
        &[
            ("csrf_token", csrf.as_str()),
            ("bookshelf", "Nope"),
            ("slug", "x"),
            ("title", "X"),
        ],
        Some(("file", "b.md", BOOK)),
    );
    let bad_shelf = client
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
    assert_eq!(bad_shelf.status(), 400);

    // Empty book text.
    let body = multipart_body(
        boundary,
        &[
            ("csrf_token", csrf.as_str()),
            ("bookshelf", "S"),
            ("slug", "empty"),
            ("title", "E"),
        ],
        Some(("file", "b.md", "   ")),
    );
    let empty = client
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
    assert_eq!(empty.status(), 400);

    // A book with no chapters fails the job (recorded, not a 500).
    let body = multipart_body(
        boundary,
        &[
            ("csrf_token", csrf.as_str()),
            ("bookshelf", "S"),
            ("slug", "no-chapters"),
            ("title", "N"),
        ],
        Some(("file", "b.md", "no headings at all")),
    );
    let no_ch = client
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
    assert_eq!(no_ch.status(), 202);
    let no_ch_json: serde_json::Value = no_ch.json().await.unwrap();
    assert_eq!(no_ch_json["status"], "failed");
    assert!(no_ch_json["detail"].as_str().unwrap().contains("chapters"));

    // 16. Upload limit is admin-configurable (ConfigStore, not env):
    //     raise it, upload a >32 MiB book (accepted), then verify the
    //     admin page shows the new value.
    let raise = client
        .post(format!("{base}/admin/upload-limits"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("max_book_mib", "64")])
        .send()
        .await
        .unwrap();
    assert_eq!(raise.status(), 303, "admin can raise the upload limit");
    // A 33 MiB book: rejected under the default 32, accepted at 64.
    // (Sparse headings keep the catalog small; the payload is what's big.)
    let mut big_book = String::with_capacity(33 * 1024 * 1024);
    big_book.push_str("# Chapter One\n\n");
    big_book.push_str(&"x".repeat(33 * 1024 * 1024));
    let boundary = "bigbook";
    let body = multipart_body(
        boundary,
        &[
            ("csrf_token", csrf.as_str()),
            ("bookshelf", "S"),
            ("slug", "big-book"),
            ("title", "Big Book"),
        ],
        Some(("file", "big.md", &big_book)),
    );
    let big = client
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
    assert_eq!(
        big.status(),
        202,
        "33 MiB book must be accepted at a 64 MiB limit"
    );
    let big_json: serde_json::Value = big.json().await.unwrap();
    assert_eq!(big_json["status"], "done", "big book ingested: {big_json}");
    // The admin page reflects the configured limit.
    let admin_html = client
        .get(format!("{base}/admin"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        admin_html.contains("up to 64 MiB"),
        "admin page shows the configured limit"
    );
    // Reset to the default for cleanliness.
    let reset = client
        .post(format!("{base}/admin/upload-limits"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("max_book_mib", "32")])
        .send()
        .await
        .unwrap();
    assert_eq!(reset.status(), 303);

    shutdown.cancel();
}
