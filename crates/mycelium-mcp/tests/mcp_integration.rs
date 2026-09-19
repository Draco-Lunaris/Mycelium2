//! MCP integration test: boot the full server (web + /mcp), then
//! exercise the 2026-07-28 stateless MCP protocol end-to-end —
//! auth rejection, tools/list, tools/call (memory + skills), and
//! per-user isolation.

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

#[tokio::test]
async fn mcp_full_flow() {
    let (base, shutdown, dir, pool) = boot().await;
    let client = client();

    // --- Auth rejection (no/invalid bearer) ---
    let no_auth = client
        .post(format!("{base}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list",
            "params": {"_meta": request_meta()}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(no_auth.status(), 401, "unauthenticated must be rejected");
    assert!(
        no_auth.headers().get("www-authenticate").is_some(),
        "401 must carry a WWW-Authenticate challenge"
    );

    let bad_auth = client
        .post(format!("{base}/mcp"))
        .header("authorization", "Bearer myc2-invalid-token-here")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list",
            "params": {"_meta": request_meta()}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_auth.status(), 401, "invalid key must be rejected");

    // --- Login, settle password, mint an API key ---
    let (cookie, csrf) = login_and_settle(
        &client,
        &base,
        &pool,
        "admin",
        ADMIN_PASSWORD,
        "a-very-long-test-password-123!",
    )
    .await;
    let token = mint_key(&client, &base, &cookie, &csrf).await;
    assert!(token.starts_with("myc2-"));

    // --- tools/list ---
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        1,
        "tools/list",
        serde_json::json!({"_meta": request_meta()}),
    )
    .await;
    assert_eq!(status, 200, "tools/list must succeed: {body}");
    let tools = body["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "mycelium2_memory_query",
        "mycelium2_memory_add",
        "mycelium2_memory_update",
        "mycelium2_memory_status",
        "mycelium2_memory_maintain",
        "mycelium2_skill_get",
        "mycelium2_skill_list",
    ] {
        assert!(
            names.contains(&expected),
            "tools/list must include {expected}: got {names:?}"
        );
    }
    // SEP-2549 cache hints on 2026-07-28.
    assert_eq!(body["result"]["ttlMs"], 0);
    assert_eq!(body["result"]["cacheScope"], "public");

    // --- tools/call: memory_add ---
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        2,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_memory_add",
            "arguments": {
                "content": "Rust async basics: tokio::select! races futures and cancels the losers.",
                "path": "rust/async-basics"
            }
        }),
    )
    .await;
    assert_eq!(status, 200, "memory_add must succeed: {body}");
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("recorded at /rust/async-basics.md"),
        "memory_add must report the path: {text}"
    );
    assert_eq!(body["result"]["isError"], false);

    // --- tools/call: memory_query finds it ---
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        3,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_memory_query",
            "arguments": {"question": "rust async tokio select"}
        }),
    )
    .await;
    assert_eq!(status, 200, "memory_query must succeed: {body}");
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("async-basics"),
        "memory_query must find the added concept: {text}"
    );

    // --- tools/call: memory_status ---
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        4,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_memory_status",
            "arguments": {}
        }),
    )
    .await;
    assert_eq!(status, 200, "memory_status must succeed: {body}");
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("concepts: 1"),
        "status must count concepts: {text}"
    );

    // --- tools/call: memory_update ---
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        5,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_memory_update",
            "arguments": {
                "instruction": "Correction: select! also supports biased mode.",
                "path": "/rust/async-basics.md"
            }
        }),
    )
    .await;
    assert_eq!(status, 200, "memory_update must succeed: {body}");
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("updated /rust/async-basics.md"),
        "memory_update must report the path: {text}"
    );

    // --- tools/call: memory_maintain ---
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        6,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_memory_maintain",
            "arguments": {}
        }),
    )
    .await;
    assert_eq!(status, 200, "memory_maintain must succeed: {body}");
    assert_eq!(body["result"]["isError"], false);

    // --- memory_maintain regression: broken-link flagging must keep the
    //     markdown link well-formed (marker AFTER the closing paren, so
    //     the scanner still sees the target on the next health check). ---
    {
        // Add a concept with a broken link.
        let (status, _body) = rpc(
            &client,
            &base,
            &token,
            30,
            "tools/call",
            serde_json::json!({
                "_meta": request_meta(),
                "name": "mycelium2_memory_add",
                "arguments": {
                    "content": "See [the missing doc](/nonexistent/target.md) for details.",
                    "path": "broken-links/holder"
                }
            }),
        )
        .await;
        assert_eq!(status, 200);
        let (status, _body) = rpc(
            &client,
            &base,
            &token,
            31,
            "tools/call",
            serde_json::json!({
                "_meta": request_meta(),
                "name": "mycelium2_memory_maintain",
                "arguments": {}
            }),
        )
        .await;
        assert_eq!(status, 200);
        // Fetch the concept via the REST API and inspect the body.
        let fetched = client
            .get(format!("{base}/api/v1/concepts/broken-links/holder.md"))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(fetched.status(), 200);
        let markdown = fetched.text().await.unwrap();
        assert!(
            markdown.contains("](/nonexistent/target.md) <!-- mycelium2:broken-link -->"),
            "broken-link marker must follow the closing paren, got: {markdown:?}"
        );
        assert!(
            !markdown.contains("](/nonexistent/target.md <!--"),
            "marker must NOT be inside the link destination"
        );
    }

    // --- memory_add slug-collision regression: same path, different
    //     content → disambiguated suffix, NOT a silent overwrite. ---
    {
        let (status, body) = rpc(
            &client,
            &base,
            &token,
            32,
            "tools/call",
            serde_json::json!({
                "_meta": request_meta(),
                "name": "mycelium2_memory_add",
                "arguments": {
                    "content": "A completely different note about async rust.",
                    "path": "rust/async-basics"
                }
            }),
        )
        .await;
        assert_eq!(status, 200);
        let text = body["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        assert!(
            text.contains("recorded at /rust/async-basics-2.md"),
            "collision must disambiguate with a suffix, got: {text}"
        );
        // The original must still exist.
        let (status, body) = rpc(
            &client,
            &base,
            &token,
            33,
            "tools/call",
            serde_json::json!({
                "_meta": request_meta(),
                "name": "mycelium2_memory_query",
                "arguments": {"question": "tokio select races futures cancels losers"}
            }),
        )
        .await;
        assert_eq!(status, 200);
        let text = body["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        assert!(
            text.contains("races futures"),
            "the original concept must survive the collision, got: {text}"
        );
    }

    // --- skills: seed a global skill via the service scope ---
    {
        let store = Store::open(dir.path()).await.unwrap();
        let service_key =
            mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();
        let cs = mycelium_store::concept_store::ConceptStore::for_service(
            &store,
            service_key,
            &store.skills_dir(),
            "skills",
        );
        let skill = mycelium_core::concept::Concept::parse(
            "/deploy-rust-service.md",
            "---\ntype: Skill\ntitle: Deploy Rust Service\n---\n\n1. cargo build --release\n2. copy binary\n",
        )
        .unwrap();
        cs.put(&skill).await.unwrap();
    }

    // --- tools/call: skill_list (private + global) ---
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        7,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_skill_list",
            "arguments": {}
        }),
    )
    .await;
    assert_eq!(status, 200, "skill_list must succeed: {body}");
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("Deploy Rust Service"),
        "skill_list must include the global skill: {text}"
    );

    // --- tools/call: skill_get (global fallback) ---
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        8,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_skill_get",
            "arguments": {"name": "deploy-rust-service"}
        }),
    )
    .await;
    assert_eq!(status, 200, "skill_get must succeed: {body}");
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("cargo build --release"),
        "skill_get must return the skill body: {text}"
    );

    // --- skill_get: private-first ---
    // Add a private skill with the same name; skill_get must return it.
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        9,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_memory_add",
            "arguments": {
                "content": "Private variant: use docker instead.",
                "path": "deploy-rust-service",
                "concept_type": "Skill"
            }
        }),
    )
    .await;
    assert_eq!(status, 200);
    let _ = body;
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        10,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_skill_get",
            "arguments": {"name": "deploy-rust-service"}
        }),
    )
    .await;
    assert_eq!(status, 200);
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("Private variant"),
        "skill_get must prefer the private skill: {text}"
    );

    // --- Unknown tool → caller-visible error ---
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        11,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_nonexistent",
            "arguments": {}
        }),
    )
    .await;
    // rmcp's tool router reports unknown tools as invalid_params (-32602,
    // HTTP 400) — a caller-visible protocol rejection.
    assert_eq!(status, 400, "unknown tool must be rejected: {body}");
    assert_eq!(body["error"]["code"], -32602);

    // --- Missing per-request _meta → 400 (stateless metadata required) ---
    let resp = client
        .post(format!("{base}/mcp"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 12, "method": "tools/list", "params": {}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        400,
        "missing _meta must be rejected (stateless metadata required)"
    );

    // --- Revoked key → 401 ---
    // (mint a second key, revoke it via the web UI, verify rejection)
    let token2 = mint_key(&client, &base, &cookie, &csrf).await;
    let page = client
        .get(format!("{base}/keys"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let html = page.text().await.unwrap();
    // Find the revoke form's key id (the most recently minted key's row).
    let revoke_marker = "name=\"id\" value=\"";
    let mut revoke_id = None;
    let mut search_from = 0;
    while let Some(idx) = html[search_from..].find(revoke_marker) {
        let rest = &html[search_from + idx + revoke_marker.len()..];
        let end = rest.find('"').unwrap();
        revoke_id = Some(rest[..end].to_string());
        search_from += idx + revoke_marker.len();
    }
    let revoke_id = revoke_id.expect("revoke form present");
    let revoke = client
        .post(format!("{base}/keys/revoke"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[("id", revoke_id.as_str())])
        .send()
        .await
        .unwrap();
    assert!(revoked_ok(revoke).await);
    let revoked_auth = client
        .post(format!("{base}/mcp"))
        .header("authorization", format!("Bearer {token2}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 13, "method": "tools/list",
            "params": {"_meta": request_meta()}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(revoked_auth.status(), 401, "revoked key must be rejected");

    // --- Per-user isolation: a second user's MCP tools must not see the
    //     admin's concepts, and vice versa. ---
    // Create a second user via the admin portal.
    let created = client
        .post(format!("{base}/admin/users"))
        .header("cookie", &cookie)
        .header("x-csrf-token", &csrf)
        .form(&[
            ("username", "bob"),
            ("email", "bob@example.com"),
            ("password", "another-very-long-password-456!"),
            ("role", "user"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 200, "user creation must succeed");

    // Bob logs in (no forced change — that's bootstrap-only), mints a key.
    let bob_login = client
        .post(format!("{base}/login"))
        .form(&[
            ("username", "bob"),
            ("password", "another-very-long-password-456!"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(bob_login.status(), 303);
    let bob_cookie = bob_login
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    // Bob's own CSRF token (bound to his session).
    let bob_page = client
        .get(format!("{base}/keys"))
        .header("cookie", &bob_cookie)
        .send()
        .await
        .unwrap();
    let bob_html = bob_page.text().await.unwrap();
    let marker = r#"name="csrf-token" content=""#;
    let idx = bob_html.find(marker).expect("bob csrf meta present");
    let rest = &bob_html[idx + marker.len()..];
    let end = rest.find('"').unwrap();
    let bob_csrf = rest[..end].to_string();
    let bob_token = mint_key(&client, &base, &bob_cookie, &bob_csrf).await;

    // Bob's status: zero concepts (admin's /rust/async-basics.md invisible).
    let (status, body) = rpc(
        &client,
        &base,
        &bob_token,
        14,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_memory_status",
            "arguments": {}
        }),
    )
    .await;
    assert_eq!(status, 200);
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("concepts: 0"),
        "bob must see zero concepts (isolation): {text}"
    );

    // Bob's query must not find the admin's concept.
    let (status, body) = rpc(
        &client,
        &base,
        &bob_token,
        15,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_memory_query",
            "arguments": {"question": "rust async tokio select"}
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        body["result"]["isError"], true,
        "bob's query must not return admin's concept"
    );

    // Bob adds his own concept; admin's query must not see it.
    let (status, _body) = rpc(
        &client,
        &base,
        &bob_token,
        16,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_memory_add",
            "arguments": {
                "content": "Bob's secret note about qilin scales.",
                "path": "personal/qilin"
            }
        }),
    )
    .await;
    assert_eq!(status, 200);
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        17,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_memory_query",
            "arguments": {"question": "qilin scales secret"}
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        body["result"]["isError"], true,
        "admin's query must not return bob's concept"
    );

    shutdown.cancel();
}

async fn revoked_ok(resp: reqwest::Response) -> bool {
    resp.status().is_success() || resp.status().as_u16() == 303
}
