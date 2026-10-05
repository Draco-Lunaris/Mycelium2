//! Mutation-queue MCP integration tests (plan task 6): the three write
//! tools are enqueue-only — receipts, queue rows, encrypted payload
//! blobs, staging notes; validation precedes the queue. The drain-side
//! integration behavior (worker runs, retries, fallbacks) is task 11's
//! tests; nothing here spawns the drain worker, so enqueued rows stay
//! pending by design.

use std::net::SocketAddr;

use mycelium_auth::login::LoginService;
use mycelium_store::Store;
use mycelium_web::AppState;

/// Known admin password for the pre-seeded account (tests log in with it).
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";

/// Boot a test server on an ephemeral HTTPS port; returns the base URL,
/// the shutdown token, and the temp data dir.
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
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Log in via the web login form, complete a forced password change
/// (the flag is flipped by SQL in the test), and return (session cookie,
/// csrf token).
async fn login_and_settle(
    client: &reqwest::Client,
    base: &str,
    pool: &sqlx::SqlitePool,
    username: &str,
    login_password: &str,
    new_password: &str,
) -> (String, String) {
    // Flip must_change_password directly: no production path sets it
    // anymore, but the gate still applies to admin-reset accounts.
    sqlx::query("UPDATE users SET must_change_password = 1 WHERE username = ?")
        .bind(username)
        .execute(pool)
        .await
        .unwrap();
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", username), ("password", login_password)])
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

    // Forced-change gate: read the CSRF from /password, change it.
    let page = client
        .get(format!("{base}/password"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let html = page.text().await.unwrap();
    let marker = r#"name="csrf-token" content=""#;
    let idx = html.find(marker).expect("csrf meta present");
    let rest = &html[idx + marker.len()..];
    let end = rest.find('"').unwrap();
    let csrf = rest[..end].to_string();

    let change = client
        .post(format!("{base}/password"))
        .header("cookie", &cookie)
        .form(&[
            ("csrf_token", csrf.as_str()),
            ("old", login_password),
            ("new", new_password),
            ("repeat", new_password),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(change.status(), 303, "password change must succeed");

    // Re-login with the new password.
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", username), ("password", new_password)])
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
    let page = client
        .get(format!("{base}/"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let html = page.text().await.unwrap();
    let marker = r#"name="csrf-token" content=""#;
    let idx = html.find(marker).expect("csrf meta present");
    let rest = &html[idx + marker.len()..];
    let end = rest.find('"').unwrap();
    let csrf = rest[..end].to_string();
    (cookie, csrf)
}

/// The admin's settle: same password for both params. The forced-change
/// gate is satisfied by an unchanged-password change (policy checks the
/// new password's length, not that it differs from the old one).
async fn login_and_settle_admin(
    client: &reqwest::Client,
    base: &str,
    pool: &sqlx::SqlitePool,
) -> (String, String) {
    login_and_settle(client, base, pool, "admin", ADMIN_PASSWORD, ADMIN_PASSWORD).await
}

/// Mint an API key via the web keys page; returns the raw token.
async fn mint_key(client: &reqwest::Client, base: &str, cookie: &str, csrf: &str) -> String {
    let mint = client
        .post(format!("{base}/keys"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .form(&[("label", "mcp-test")])
        .send()
        .await
        .unwrap();
    assert_eq!(mint.status(), 200);
    let html = mint.text().await.unwrap();
    let code_start = html.find("<code>myc2-").expect("minted token shown");
    let token_rest = &html[code_start + "<code>".len()..];
    let token_end = token_rest.find("</code>").unwrap();
    token_rest[..token_end].to_string()
}

/// POST a JSON-RPC message to /mcp with the 2026-07-28 per-request
/// protocol signals (header + _meta + SEP-2243 standard headers).
async fn rpc(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    id: i64,
    method: &str,
    params: serde_json::Value,
) -> (reqwest::StatusCode, serde_json::Value) {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    });
    let mut req = client
        .post(format!("{base}/mcp"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", method)
        .json(&body);
    // SEP-2243: tools/call carries Mcp-Name (the tool name).
    if method == "tools/call"
        && let Some(name) = params.get("name").and_then(|n| n.as_str())
    {
        req = req.header("Mcp-Name", name);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status();
    let body = resp.json().await.unwrap();
    (status, body)
}

/// Build the per-request `_meta` required by 2026-07-28.
fn request_meta() -> serde_json::Value {
    serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {
            "name": "mycelium2-test",
            "version": "1.0.0"
        },
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

// ---- task-6 helpers ----

/// Point the LLM config at a dead port (nothing listens; any HTTP
/// attempt fails fast in connect). The enqueue path never contacts the
/// LLM — this makes an accidental contact fail loudly instead of
/// succeeding against a developer's local Ollama.
async fn dead_llm(pool: &sqlx::SqlitePool) {
    let cfg = serde_json::json!({ "url": "http://127.0.0.1:1/v1", "model": "m" }).to_string();
    sqlx::query("INSERT INTO config (key, value, updated_at) VALUES ('llm', ?, ?)")
        .bind(&cfg)
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(pool)
        .await
        .unwrap();
}

/// The admin user's id.
async fn admin_uuid(pool: &sqlx::SqlitePool) -> uuid::Uuid {
    let row: (String,) = sqlx::query_as("SELECT id FROM users WHERE username = 'admin'")
        .fetch_one(pool)
        .await
        .unwrap();
    uuid::Uuid::parse_str(&row.0).unwrap()
}

/// Unseal the admin's master key from the service-key seal (mirrors
/// McpState::master_key_for's recovery path) so the test can decrypt
/// payload blobs and staging notes.
async fn admin_master_key(
    data_dir: &std::path::Path,
    pool: &sqlx::SqlitePool,
) -> mycelium_crypto::keys::MasterKey {
    let service_key = mycelium_crypto::load_or_create_service_key_with(data_dir, None).unwrap();
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, service_key.as_bytes());
    let mut material = [0u8; 32];
    let _ = hk.expand(b"mycelium2/service-seal-dek/v1", &mut material);
    let dek = mycelium_crypto::keys::Dek::from_bytes(&material).expect("32 bytes");
    let row: (String,) =
        sqlx::query_as("SELECT master_key_service_sealed FROM users WHERE username = 'admin'")
            .fetch_one(pool)
            .await
            .unwrap();
    let sealed = hex::decode(&row.0).unwrap();
    let master =
        mycelium_crypto::aead::aead_open(&sealed, b"mycelium2/seal/service/v1", &dek).unwrap();
    mycelium_crypto::keys::MasterKey::from_bytes(&master).unwrap()
}

/// Count mutation-queue rows.
async fn queue_rows(pool: &sqlx::SqlitePool) -> i64 {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mutation_queue")
        .fetch_one(pool)
        .await
        .unwrap();
    n
}

/// Parse the receipt id out of a receipt line (`receipt=<uuid>`).
fn receipt_id(text: &str) -> uuid::Uuid {
    uuid::Uuid::parse_str(
        text.split("receipt=")
            .nth(1)
            .expect("receipt= in text")
            .split([' ', '\''])
            .next()
            .expect("id terminated"),
    )
    .expect("valid uuid")
}

// T6.1 — memory_add returns a receipt with LLM dead; row + payload +
// staging note all landed, and the tool never waited on the LLM.
#[tokio::test]
async fn memory_add_returns_receipt_no_llm_contact() {
    let (base, shutdown, dir, pool) = boot().await;
    // Point the LLM config at a dead port (127.0.0.1:1 — nothing
    // listens; any HTTP attempt fails fast in connect).
    dead_llm(&pool).await;
    let client = client();
    let (cookie, csrf) = login_and_settle_admin(&client, &base, &pool).await;
    let token = mint_key(&client, &base, &cookie, &csrf).await;

    let (status, body) = rpc(
        &client,
        &base,
        &token,
        1,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_add",
            "arguments": {"content": "queued receipt test content", "path": "tq/one"}
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["isError"], false);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("accepted, queued receipt="), "{text}");
    assert!(
        text.contains(" the librarian will integrate this in the background"),
        "{text}"
    );
    assert!(text.contains("memory_status(receipt_id="), "{text}");
    assert!(text.contains("staging: /mutation-queue/"), "{text}");

    // The receipt names a real row, now pending (staging→pending flip
    // completed) — the enqueue finished its full write path.
    let id = receipt_id(text);
    let (row,): (String,) = sqlx::query_as("SELECT status FROM mutation_queue WHERE id = ?")
        .bind(id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row, "pending");

    // The encrypted payload blob decrypts with the admin's master key
    // and round-trips the tool + args + content.
    let store = Store::open(dir.path()).await.unwrap();
    let uid = admin_uuid(&pool).await;
    let master = admin_master_key(dir.path(), &pool).await;
    let repo = mycelium_store::FileRepo::new(store.user_dir(uid));
    let payload = mycelium_store::mutation_queue::read_payload(&repo, &master, id)
        .await
        .unwrap()
        .expect("payload blob present");
    assert_eq!(payload.content, "queued receipt test content");
    assert_eq!(payload.tool, mycelium_store::QueueTool::Add);
    let args: serde_json::Value = serde_json::from_str(&payload.args_json).unwrap();
    assert_eq!(args["path"], "tq/one");

    // The staging note: one registry row under /mutation-queue/ and a
    // decryptable concept carrying the content + the queue-staging tag.
    let cs = mycelium_store::ConceptStore::for_user(&store, uid, master);
    let (staged,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM scope_files WHERE scope = ? AND path LIKE '/mutation-queue/%'",
    )
    .bind(format!("user:{uid}"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(staged, 1, "exactly one staging note");
    let note = cs
        .get(&mycelium_store::mutation_queue::stage_note_path(id))
        .await
        .expect("staging note readable");
    assert_eq!(note.body.trim(), "queued receipt test content");
    assert!(
        note.frontmatter.tags.iter().any(|t| t == "queue-staging"),
        "staging note tagged queue-staging: {:?}",
        note.frontmatter.tags
    );

    shutdown.cancel();
}

// T6.2 — memory_update and memory_maintain return the receipt WITHOUT a
// staging path line (D4: receipts-only), and nothing appears in the
// user's concept list beyond the seeded concept.
#[tokio::test]
async fn update_and_maintain_are_receipts_only() {
    let (base, shutdown, _dir, pool) = boot().await;
    dead_llm(&pool).await;
    let client = client();
    let (cookie, csrf) = login_and_settle_admin(&client, &base, &pool).await;
    let token = mint_key(&client, &base, &cookie, &csrf).await;

    // Seed one orphan concept via the REST API (bearer). An unhealthy
    // graph is REQUIRED for maintain to enqueue — a healthy graph
    // returns early, never reaching the queue.
    let uid = admin_uuid(&pool).await;
    let put = client
        .put(format!("{base}/api/v1/concepts/seed/orphan.md"))
        .header("authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "markdown": "---\ntype: Note\ntitle: Seed Orphan\n---\n\nseeded body for the maintain enqueue\n"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 201, "seed PUT must succeed");

    // memory_update → receipt, no staging line, row tool='update'.
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        1,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_update",
            "arguments": {"instruction": "correct the seeded note", "path": "/seed/orphan.md"}
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["isError"], false);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("accepted, queued receipt="), "{text}");
    assert!(
        !text.contains("staging:"),
        "update receipts are receipts-only: {text}"
    );
    let (tool,): (String,) = sqlx::query_as("SELECT tool FROM mutation_queue WHERE id = ?")
        .bind(receipt_id(text).to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tool, "update");

    // memory_maintain → receipt (the seeded orphan makes the graph
    // unhealthy), no staging line, row tool='maintain'.
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        2,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_maintain",
            "arguments": {}
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["isError"], false);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("accepted, queued receipt="), "{text}");
    assert!(
        !text.contains("staging:"),
        "maintain receipts are receipts-only: {text}"
    );
    let (tool,): (String,) = sqlx::query_as("SELECT tool FROM mutation_queue WHERE id = ?")
        .bind(receipt_id(text).to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tool, "maintain");

    // No staging note was written: the concept list still counts only
    // the seeded concept, and no /mutation-queue/ registry rows exist.
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        3,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_status",
            "arguments": {}
        }),
    )
    .await;
    assert_eq!(status, 200);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("concepts: 1"),
        "only the seeded concept, no staging note: {text}"
    );
    let (staged,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM scope_files WHERE scope = ? AND path LIKE '/mutation-queue/%'",
    )
    .bind(format!("user:{uid}"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(staged, 0, "no staging notes for update/maintain");
    shutdown.cancel();
}

// T6.3 — depth cap: enqueue 50 adds, the 51st tool call is an error
// result whose text says "queue is at capacity — retry later"; the row
// count stays 50.
#[tokio::test]
async fn depth_cap_rejects_51st() {
    let (base, shutdown, _dir, pool) = boot().await;
    // No LLM config write — the dead default is fine: the enqueue path
    // never contacts the LLM.
    let client = client();
    let (cookie, csrf) = login_and_settle_admin(&client, &base, &pool).await;
    let token = mint_key(&client, &base, &cookie, &csrf).await;

    let mut next_id = 1i64;
    for i in 0..50 {
        let (status, body) = rpc(
            &client,
            &base,
            &token,
            next_id,
            "tools/call",
            serde_json::json!({
                "_meta": request_meta(), "name": "mycelium2_memory_add",
                "arguments": {
                    "content": format!("depth cap probe content {i}"),
                    "path": format!("cap/{i}")
                }
            }),
        )
        .await;
        assert_eq!(status, 200, "add {i} must succeed: {body}");
        let text = body["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.starts_with("accepted, queued receipt="),
            "add {i}: {text}"
        );
        next_id += 1;
    }
    assert_eq!(queue_rows(&pool).await, 50);

    // The 51st consumes no slot: capacity error, row count stays 50.
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        next_id,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_add",
            "arguments": {"content": "one mutation too many", "path": "cap/overflow"}
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["isError"], true);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("queue is at capacity — retry later"),
        "{text}"
    );
    assert_eq!(queue_rows(&pool).await, 50);

    shutdown.cancel();
}

// T6.4 — empty/whitespace content is rejected BEFORE a queue slot is
// consumed, then a valid enqueue still succeeds.
#[tokio::test]
async fn empty_content_rejected_before_enqueue() {
    let (base, shutdown, _dir, pool) = boot().await;
    // LLM config irrelevant — validation precedes any LLM or queue contact.
    let client = client();
    let (cookie, csrf) = login_and_settle_admin(&client, &base, &pool).await;
    let token = mint_key(&client, &base, &cookie, &csrf).await;

    // Empty content → caller-visible rejection, no queue slot consumed.
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        1,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_add",
            "arguments": {"content": ""}
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["isError"], true);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("not found: content must not be empty"),
        "{text}"
    );
    assert_eq!(queue_rows(&pool).await, 0);

    // Whitespace-only content → same rejection.
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        2,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_add",
            "arguments": {"content": "  \n\t "}
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["isError"], true);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("not found: content must not be empty"),
        "{text}"
    );
    assert_eq!(queue_rows(&pool).await, 0);

    // Empty update instruction → same shape.
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        3,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_update",
            "arguments": {"instruction": ""}
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["isError"], true);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("not found: instruction must not be empty"),
        "{text}"
    );
    assert_eq!(queue_rows(&pool).await, 0);

    // The path still works after rejections.
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        4,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_add",
            "arguments": {"content": "real content after rejections", "path": "tq/valid"}
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["isError"], false);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("accepted, queued receipt="), "{text}");
    assert_eq!(queue_rows(&pool).await, 1);

    shutdown.cancel();
}

// ---- task 8: memory_status(receipt_id) — the receipt lookup ----

/// Known password for the second (non-admin) user in the scoping probe.
const BOB_PASSWORD: &str = "bob password known to the test suite!";

// T8.1 — receipt_id lifecycle: add → "pending" (staging mapped to
// pending, never "staging"); done → final_paths JSON; unknown and
// unparseable ids → "unknown"; another user's id → "unknown" (no leak).
#[tokio::test]
async fn receipt_transitions_and_scoping() {
    let (base, shutdown, dir, pool) = boot().await;
    dead_llm(&pool).await;
    let client = client();
    let (cookie, csrf) = login_and_settle_admin(&client, &base, &pool).await;
    let token = mint_key(&client, &base, &cookie, &csrf).await;

    // Enqueue one add; harvest the receipt id from the receipt line.
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        1,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_add",
            "arguments": {"content": "receipt lifecycle content"}
        }),
    )
    .await;
    assert_eq!(status, 200, "add must succeed: {body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let id = receipt_id(text);

    // Lookup with the id: pending, tool add, attempts 0, final_paths null.
    let (_s, b2) = rpc(
        &client,
        &base,
        &token,
        2,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_status",
            "arguments": {"receipt_id": id.to_string()}
        }),
    )
    .await;
    assert_eq!(b2["result"]["isError"], false, "{b2}");
    let t2 = b2["result"]["content"][0]["text"].as_str().unwrap();
    let receipt: serde_json::Value = serde_json::from_str(t2).unwrap();
    assert_eq!(receipt["state"], "pending", "{t2}");
    assert_eq!(receipt["tool"], "add", "{t2}");
    assert_eq!(receipt["attempts"], 0, "{t2}");
    assert_eq!(receipt["final_paths"], serde_json::Value::Null, "{t2}");

    // A staging row (SQL flip — the enqueue path never leaves one
    // observable, it flips to pending before returning) still renders
    // "pending": receipt views never say staging (spec §4.1).
    sqlx::query("UPDATE mutation_queue SET status = 'staging' WHERE id = ?")
        .bind(id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let (_s, b3) = rpc(
        &client,
        &base,
        &token,
        3,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_status",
            "arguments": {"receipt_id": id.to_string()}
        }),
    )
    .await;
    let t3 = b3["result"]["content"][0]["text"].as_str().unwrap();
    let staged: serde_json::Value = serde_json::from_str(t3).unwrap();
    assert_eq!(staged["state"], "pending", "{t3}");

    // A done row renders done with final_paths as a JSON array (the raw
    // column is written only by mark_done) and the provenance detail.
    let store = Store::open(dir.path()).await.unwrap();
    let paths = vec!["/receipt/lifecycle.md".to_string()];
    store.mark_done(id, &paths, "integrated").await.unwrap();
    let (_s, b4) = rpc(
        &client,
        &base,
        &token,
        4,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_status",
            "arguments": {"receipt_id": id.to_string()}
        }),
    )
    .await;
    let t4 = b4["result"]["content"][0]["text"].as_str().unwrap();
    let done: serde_json::Value = serde_json::from_str(t4).unwrap();
    assert_eq!(done["state"], "done", "{t4}");
    assert_eq!(
        done["final_paths"],
        serde_json::json!(["/receipt/lifecycle.md"]),
        "{t4}"
    );
    assert_eq!(done["detail"], "integrated", "{t4}");

    // A random uuid → state unknown, success (never an error — probing
    // is cheap and erroring would leak validation behavior).
    let (_s, b5) = rpc(
        &client,
        &base,
        &token,
        5,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_status",
            "arguments": {"receipt_id": uuid::Uuid::new_v4().to_string()}
        }),
    )
    .await;
    assert_eq!(b5["result"]["isError"], false, "{b5}");
    let t5 = b5["result"]["content"][0]["text"].as_str().unwrap();
    let unknown: serde_json::Value = serde_json::from_str(t5).unwrap();
    assert_eq!(unknown["state"], "unknown", "{t5}");

    // An unparseable id → same unknown shape.
    let (_s, b6) = rpc(
        &client,
        &base,
        &token,
        6,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_status",
            "arguments": {"receipt_id": "not-a-uuid"}
        }),
    )
    .await;
    assert_eq!(b6["result"]["isError"], false, "{b6}");
    let t6 = b6["result"]["content"][0]["text"].as_str().unwrap();
    let bad: serde_json::Value = serde_json::from_str(t6).unwrap();
    assert_eq!(bad["state"], "unknown", "{t6}");

    // Another user's id → unknown: user scoping is enforced in the SQL
    // (`WHERE id = ? AND user_id = ?`), so a foreign receipt is
    // indistinguishable from an absent one — no existence leak.
    let users = mycelium_auth::UserStore::new(pool.clone());
    users
        .create_local(
            "bob",
            "bob@example.com",
            BOB_PASSWORD,
            mycelium_auth::Role::User,
        )
        .await
        .unwrap();
    let (bob_cookie, bob_csrf) =
        login_and_settle(&client, &base, &pool, "bob", BOB_PASSWORD, BOB_PASSWORD).await;
    let bob_token = mint_key(&client, &base, &bob_cookie, &bob_csrf).await;
    let (_s, b7) = rpc(
        &client,
        &base,
        &bob_token,
        1,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_status",
            "arguments": {"receipt_id": id.to_string()}
        }),
    )
    .await;
    assert_eq!(b7["result"]["isError"], false, "{b7}");
    let t7 = b7["result"]["content"][0]["text"].as_str().unwrap();
    let foreign: serde_json::Value = serde_json::from_str(t7).unwrap();
    assert_eq!(foreign["state"], "unknown", "{t7}");

    shutdown.cancel();
}

// T8.2 — no-arg call is byte-identical to today's output (back-compat:
// graph-health text, four lines) — with an empty arguments object and
// with the arguments key absent entirely.
#[tokio::test]
async fn status_without_receipt_id_unchanged() {
    let (base, shutdown, _dir, pool) = boot().await;
    let client = client();
    let (cookie, csrf) = login_and_settle_admin(&client, &base, &pool).await;
    let token = mint_key(&client, &base, &cookie, &csrf).await;

    // Fresh admin bundle, zero enqueues: nothing staggers the concept
    // count, so the graph-health text starts at zero.
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        1,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_status",
            "arguments": {}
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["isError"], false);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "concepts: 0\nedges: 0\nbroken links: 0\norphans: 0");

    // No arguments key at all: the optional field keeps the call valid
    // (rmcp treats absent arguments as an empty object).
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        2,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(), "name": "mycelium2_memory_status"
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["isError"], false);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "concepts: 0\nedges: 0\nbroken links: 0\norphans: 0");

    shutdown.cancel();
}
