//! MCP integration test: boot the full server (web + /mcp), then
//! exercise the 2026-07-28 stateless MCP protocol end-to-end —
//! auth rejection, tools/list, tools/call (memory + skills), and
//! per-user isolation.
//!
//! Since the memory-lifecycle step 3, `memory_query`'s default is the
//! zero-LLM hits path; the `synthesize: true` path admits through the
//! process-global `llm_gate::LLM_GATE`. Every test in this binary that
//! runs a query — or holds/reconfigures gate slots — serializes on
//! [`TEST_LOCK`] and restores the gate's defaults through [`GateDefaults`]
//! (declared after the lock guard, so the restore happens while the lock
//! is still held); query fixtures also hold [`HealthDefaults`], resetting
//! the process-global `LLM_HEALTH` the query paths passively feed.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::response::IntoResponse;
use mycelium_auth::login::LoginService;
use mycelium_librarian::llm_gate::{
    DEFAULT_CAPACITY, DEFAULT_CHAT_QUEUE_DEPTH, DEFAULT_PER_USER_ACTIVE, LLM_GATE,
};
use mycelium_store::Store;
use mycelium_web::AppState;

/// Known admin password for the pre-seeded account (tests log in with it).
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";

/// Serializes the gate-sensitive tests in this binary (the synthesize
/// path's admission and the direct gate holds) — and, more generally,
/// every query static's read-write cycle: `memory_query` feeds the
/// process-global `LLM_HEALTH` and reads the lifecycle `query_top_k`.
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Wait bound for the gate holds this suite takes.
const GATE_BOUND: Duration = Duration::from_secs(10);

/// Restores the process-global gate's tunables on drop, so a failing
/// assertion cannot leak a reconfigured gate into the next test.
struct GateDefaults;

impl Drop for GateDefaults {
    fn drop(&mut self) {
        LLM_GATE.set_capacity(DEFAULT_CAPACITY);
        LLM_GATE.set_per_user_active(DEFAULT_PER_USER_ACTIVE);
        LLM_GATE.set_chat_queue_depth(DEFAULT_CHAT_QUEUE_DEPTH);
    }
}

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

    // Forced-change gate: read the CSRF from /account (the /password
    // page is a 307 there since the Task-5 merge), change it.
    let page = client
        .get(format!("{base}/account"))
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
    // The minted token sits in the shown-once banner's <code> element
    // (Task 7 gave it the id the copy island targets).
    let code_start = html
        .find("<code id=\"minted-key\">")
        .expect("minted token shown");
    let token_rest = &html[code_start + "<code id=\"minted-key\">".len()..];
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
    // This flow runs memory_query (the zero-LLM hits default); it holds
    // the lock alongside the gate-sensitive tests.
    let _lock = TEST_LOCK.lock().await;
    let _defaults = GateDefaults;
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
        text.contains("accepted, queued receipt="),
        "memory_add must return a receipt: {text}"
    );
    assert!(
        text.contains("staging: /mutation-queue/"),
        "the add receipt names its staging note: {text}"
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
    // The staging note (title = derive_title(content), path
    // /mutation-queue/<id>.md) is searchable immediately: the query
    // hits on the content terms (the final concept is the drain's to
    // write, so the old path hint appears nowhere yet).
    assert!(
        text.contains("tokio::select"),
        "memory_query must find the staged content: {text}"
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
        text.starts_with("accepted, queued receipt="),
        "memory_update must return a receipt: {text}"
    );
    assert!(
        !text.contains("staging:"),
        "update receipts are receipts-only (no staging note): {text}"
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
    // The bundle holds one unlinked staging note → orphan → unhealthy →
    // enqueued (a healthy graph would return early, no receipt).
    assert_eq!(body["result"]["isError"], false);
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.starts_with("accepted, queued receipt="),
        "memory_maintain must return a receipt: {text}"
    );

    // (The broken-link marker-position regression moved with the repair
    // code to mycelium-librarian: fallback_integration.rs asserts the
    // marker placement in wire_and_flag_maintain. The MCP maintain
    // tool now only enqueues — nothing to inspect here.)

    // --- memory_add second-record: a different content under the same
    //     path hint gets its own receipt (path disambiguation is now
    //     the drain's direct_write_add fallback — covered in
    //     fallback_integration.rs), and the original staged content is
    //     still findable. ---
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
            text.starts_with("accepted, queued receipt="),
            "the second add returns its own receipt, got: {text}"
        );
        // The original staged content is still the findable match.
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
            "the original staged content must survive, got: {text}"
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
    // Seed a private skill with the same name via the REST API
    // (memory_add now enqueues — it no longer lands synchronously);
    // skill_get must return the private variant.
    let put = client
        .put(format!("{base}/api/v1/concepts/deploy-rust-service.md"))
        .header("authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "markdown": "---\ntype: Skill\ntitle: Deploy Rust Service\n---\n\nPrivate variant: use docker instead.\n"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 201, "private skill seed must succeed");
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
    // Find the revoke trigger's key id (the most recently minted key's
    // row — the active rows' revoke triggers carry data-key-id; the
    // confirm dialog carries no data-key-id — its hidden input is
    // name="id"). Marker
    // re-verified against the Task-8 keys restyle and kept: the keys
    // data_table renders rows in creation order (ASC), only ACTIVE rows
    // carry a data-key-id trigger (revoked rows render dimmed "Revoked"
    // text — no trigger), and neither the confirm dialog nor the
    // minted banner carries one — so the LAST occurrence is the newest
    // active key, which is token2's row.
    let revoke_marker = "data-key-id=\"";
    let mut revoke_id = None;
    let mut search_from = 0;
    while let Some(idx) = html[search_from..].find(revoke_marker) {
        let rest = &html[search_from + idx + revoke_marker.len()..];
        let end = rest.find('"').unwrap();
        revoke_id = Some(rest[..end].to_string());
        search_from += idx + revoke_marker.len();
    }
    let revoke_id = revoke_id.expect("revoke trigger present");
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

    // Bob's query must not find the admin's concept. Since the hits
    // default spans global scopes (spec §3.3 — the global skill above is
    // shared by design), an empty-hit error is no longer the probe: the
    // response may legitimately list the global skill, but it must NOT
    // carry the admin's concept's text or path (and must not carry the
    // admin's PRIVATE skill variant either).
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
    let bob_text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        !bob_text.contains("tokio::select") && !bob_text.contains("async-basics"),
        "bob's hit list must not carry the admin's concept: {bob_text}"
    );
    assert!(
        !bob_text.contains("Private variant"),
        "bob's hit list must not carry the admin's private skill: {bob_text}"
    );
    assert!(
        bob_text.contains("## Deploy Rust Service"),
        "the global skill stays visible across scopes (the hits default spans skills): {bob_text}"
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

// ---------------------------------------------------------------------------
// Skill bundles: skill_list returns one entry per logical skill (the hub
// of each nested bundle plus flat Skill roots); skill_get resolves a
// bare slug to its hub, falling back to the flat path.
// ---------------------------------------------------------------------------

/// Seed concepts into the global skills shelf via a second store handle
/// (the service scope is shared with the booted server; the seeding
/// pattern mcp_full_flow established, batched for one index regen).
async fn seed_global_skills(dir: &std::path::Path, concepts: &[(&str, &str)]) {
    let store = Store::open(dir).await.unwrap();
    let service_key = mycelium_crypto::load_or_create_service_key_with(dir, None).unwrap();
    let cs = mycelium_store::concept_store::ConceptStore::for_service(
        &store,
        service_key,
        &store.skills_dir(),
        "skills",
    );
    let parsed = concepts
        .iter()
        .map(|(path, md)| mycelium_core::concept::Concept::parse(path, md).unwrap())
        .collect::<Vec<_>>();
    cs.put_batch(&parsed).await.unwrap();
}

/// Seed a concept into the caller's private bundle via the REST API (the
/// bearer-authenticated PUT mcp_full_flow established).
async fn put_private_concept(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    markdown: &str,
) {
    let put = client
        .put(format!("{base}/api/v1/concepts/{path}"))
        .header("authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "markdown": markdown }))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 201, "private seed of {path} must succeed");
}

#[tokio::test]
async fn skill_list_returns_one_entry_per_nested_skill() {
    let (base, shutdown, dir, pool) = boot().await;
    let client = client();
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

    // Global shelf: a nested skill (hub + companion), a flat legacy
    // skill, a hub whose type is not Skill, and a flat non-Skill note.
    seed_global_skills(
        dir.path(),
        &[
            (
                "/nested/skill.md",
                "---\ntype: Skill\ntitle: Nested Skill\n---\n\nnested hub body\n",
            ),
            (
                "/nested/companion.md",
                "---\ntype: Skill\ntitle: Companion\n---\n\ncompanion body\n",
            ),
            (
                "/legacy-skill.md",
                "---\ntype: Skill\ntitle: Legacy Skill\n---\n\nlegacy flat body\n",
            ),
            (
                "/not-a-skill/skill.md",
                "---\ntype: Note\ntitle: Not A Skill\n---\n\nhub-shaped but wrong type\n",
            ),
            (
                "/plain-note.md",
                "---\ntype: Note\ntitle: Plain Note\n---\n\njust a note\n",
            ),
        ],
    )
    .await;

    let (status, body) = rpc(
        &client,
        &base,
        &token,
        1,
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
    // One entry per logical skill, path-sorted: the nested skill appears
    // as its hub only; the companion, the non-Skill hub, and the flat
    // non-Skill note never appear.
    assert_eq!(
        text, "- Legacy Skill (/legacy-skill.md)\n- Nested Skill (/nested/skill.md)",
        "skill_list must return one entry per logical skill"
    );

    shutdown.cancel();
}

#[tokio::test]
async fn skill_get_resolves_nested_hub_by_slug_and_falls_back_to_flat() {
    let (base, shutdown, dir, pool) = boot().await;
    let client = client();
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

    seed_global_skills(
        dir.path(),
        &[
            (
                "/nested/skill.md",
                "---\ntype: Skill\ntitle: Nested Skill\n---\n\nHUB: nested hub body\n",
            ),
            (
                "/nested/companion.md",
                "---\ntype: Skill\ntitle: Companion\n---\n\nCOMPANION: companion body\n",
            ),
            (
                "/legacy-skill.md",
                "---\ntype: Skill\ntitle: Legacy Skill\n---\n\nFLAT: legacy flat body\n",
            ),
        ],
    )
    .await;

    // Bare slug → the nested skill's hub (never a companion).
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        1,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_skill_get",
            "arguments": {"name": "nested"}
        }),
    )
    .await;
    assert_eq!(status, 200, "skill_get must succeed: {body}");
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("HUB: nested hub body"),
        "slug resolves the hub: {text}"
    );
    assert!(
        !text.contains("COMPANION:"),
        "the hub, not a companion: {text}"
    );

    // Flat fallback for legacy skills.
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        2,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_skill_get",
            "arguments": {"name": "legacy-skill"}
        }),
    )
    .await;
    assert_eq!(status, 200, "skill_get must succeed: {body}");
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("FLAT: legacy flat body"),
        "flat fallback for legacy skills: {text}"
    );

    // Exact bundle path (an arg ending in .md) fetches a companion
    // directly.
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        3,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_skill_get",
            "arguments": {"name": "/nested/companion.md"}
        }),
    )
    .await;
    assert_eq!(status, 200, "skill_get must succeed: {body}");
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("COMPANION: companion body"),
        "exact .md path fetches the companion directly: {text}"
    );

    shutdown.cancel();
}

#[tokio::test]
async fn private_shelf_gets_the_same_grouping() {
    let (base, shutdown, _dir, pool) = boot().await;
    let client = client();
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

    // Private bundle: a nested skill (hub + companion) plus a flat
    // legacy skill.
    put_private_concept(
        &client,
        &base,
        &token,
        "nested/skill.md",
        "---\ntype: Skill\ntitle: Private Nested\n---\n\nPRIVATE HUB: private hub body\n",
    )
    .await;
    put_private_concept(
        &client,
        &base,
        &token,
        "nested/companion.md",
        "---\ntype: Skill\ntitle: Private Companion\n---\n\nprivate companion body\n",
    )
    .await;
    put_private_concept(
        &client,
        &base,
        &token,
        "legacy-skill.md",
        "---\ntype: Skill\ntitle: Private Legacy\n---\n\nprivate legacy body\n",
    )
    .await;

    // skill_list groups the private scope identically: hub only for the
    // nested skill, flat listed, path-sorted.
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        1,
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
    assert_eq!(
        text, "- Private Legacy (/legacy-skill.md)\n- Private Nested (/nested/skill.md)",
        "private skills group identically: hub only, flat listed"
    );

    // And skill_get resolves the private hub by slug.
    let (status, body) = rpc(
        &client,
        &base,
        &token,
        2,
        "tools/call",
        serde_json::json!({
            "_meta": request_meta(),
            "name": "mycelium2_skill_get",
            "arguments": {"name": "nested"}
        }),
    )
    .await;
    assert_eq!(status, 200, "skill_get must succeed: {body}");
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("PRIVATE HUB: private hub body"),
        "slug resolves the private hub: {text}"
    );

    shutdown.cancel();
}

// ---------------------------------------------------------------------------
// Task 7: memory_query's bounded gate wait.
//
// These cases call `tools::memory_query` directly: the tool takes
// `&McpState` + `&McpUser`, so no HTTP server is needed — and with no
// server the only gate traffic is the test's own. They need a REACHABLE
// mock LLM: `boot()`'s default config points at an unreachable Ollama,
// so a "falls back" assertion would pass vacuously against it.
// ---------------------------------------------------------------------------

/// A minimal OpenAI-compatible mock (the file-local mock helper the
/// librarian test suite established — copied per test binary by
/// convention): serves scripted responses LIFO, captures every request.
type MockShared = Arc<tokio::sync::Mutex<MockLlmState>>;
struct MockLlmState {
    responses: Vec<serde_json::Value>,
    requests: Vec<serde_json::Value>,
}

async fn mock_llm(responses: Vec<serde_json::Value>) -> (String, MockShared) {
    let state: MockShared = Arc::new(tokio::sync::Mutex::new(MockLlmState {
        responses,
        requests: Vec::new(),
    }));
    let handler_state = Arc::clone(&state);
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |body: String| {
            let state = handler_state;
            async move {
                let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let mut guard = state.lock().await;
                guard.requests.push(parsed);
                match guard.responses.pop() {
                    Some(resp) => axum::Json(resp).into_response(),
                    None => (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        "script exhausted",
                    )
                        .into_response(),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/v1"), state)
}

fn text_response(text: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{ "message": { "role": "assistant", "content": text } }]
    })
}

/// How many requests the mock LLM has served.
async fn llm_requests(mock: &MockShared) -> usize {
    mock.lock().await.requests.len()
}

/// A direct-invocation harness for `tools::memory_query`.
struct QueryHarness {
    _dir: tempfile::TempDir,
    state: mycelium_mcp::McpState,
    user: mycelium_mcp::auth::McpUser,
    /// The harness user's master key, held so tests can seed further
    /// private concepts through the same encrypted scope the tool reads.
    master: mycelium_crypto::keys::MasterKey,
}

/// A user-scoped ConceptStore over the harness's bundle (for seeding
/// fixtures with the same keys the tool's own handle derives).
async fn harness_user_cs(h: &QueryHarness) -> mycelium_store::concept_store::ConceptStore<'_> {
    mycelium_store::concept_store::ConceptStore::for_user(
        &h.state.store,
        h.user.user_id,
        h.master.clone(),
    )
}

/// Build a harness whose sealed "llm" config points at `llm_url` with
/// the given gate limits, and whose private bundle holds one concept (so
/// the deterministic keyword fallback has something to rank).
async fn query_harness(llm_url: &str, max_parallel: u32, query_wait_secs: u32) -> QueryHarness {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let service_key = mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();
    let config = mycelium_store::ConfigStore::new(store.pool().clone());
    let llm = mycelium_librarian::llm::LlmConfig {
        url: llm_url.to_string(),
        model: "mock".into(),
        max_parallel,
        query_wait_secs,
        ..Default::default()
    };
    // Sealed shape (what the admin surface writes); the tool reads it
    // back through config::get_sealed.
    config
        .set(
            "llm",
            &mycelium_store::config::seal_config(&service_key, "llm", &llm),
        )
        .await
        .unwrap();

    let user_id = uuid::Uuid::new_v4();
    let master = mycelium_crypto::generate_master_key();
    let cs = mycelium_store::concept_store::ConceptStore::for_user(&store, user_id, master.clone());
    cs.put(
        &mycelium_core::concept::Concept::parse(
            "/notes/zebras.md",
            "---\ntype: Note\ntitle: Zebra Facts\n---\n\nZebras have distinctive stripes.\n",
        )
        .unwrap(),
    )
    .await
    .unwrap();

    // The master key is primed in the cache, so no user row (and no
    // service seal) is needed for the tool's key lookup.
    let master_keys = Arc::new(mycelium_mcp::MasterKeyCache::default());
    master_keys.insert(user_id, master.clone());
    let state = mycelium_mcp::McpState {
        store: Arc::new(store),
        service_key: Arc::new(service_key),
        master_keys,
        config: Arc::new(config),
        notify: Arc::new(tokio::sync::Notify::new()),
        enqueue_depth_cap: 50,
    };
    let user = mycelium_mcp::auth::McpUser {
        user_id,
        username: "gate-test".into(),
        role: mycelium_auth::rbac::Role::User,
        source: mycelium_mcp::auth::api_key_source("gate-key"),
    };
    QueryHarness {
        _dir: dir,
        state,
        user,
        master,
    }
}

fn query_args(question: &str) -> mycelium_mcp::tools::MemoryQueryArgs {
    mycelium_mcp::tools::MemoryQueryArgs {
        question: question.to_string(),
        limit: None,
        synthesize: None,
        include_inactive: None,
    }
}

/// Query args for the agent path (`synthesize: true`). The three gate
/// tests keep pinning gate/cache behavior on THE SYNTHESIS FLOW after
/// the hits-default switch — it is the only gate-entrant memory_query
/// path (spec §3.3), so a gate test without it would prove nothing.
fn synth_args(question: &str) -> mycelium_mcp::tools::MemoryQueryArgs {
    mycelium_mcp::tools::MemoryQueryArgs {
        question: question.to_string(),
        limit: None,
        synthesize: Some(true),
        include_inactive: None,
    }
}

/// Poll a gate condition with a deadline (never a bare sleep — the poll
/// target is observable gate state).
async fn poll_gate_until<F: FnMut() -> bool>(mut want: F, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if want() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "never saw {what}: {:?}",
            LLM_GATE.stats()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Spec §3.3: with every slot taken, the query waits its bound and then
/// the deterministic keyword fallback answers. The mock LLM proves the
/// fallback came from the gate and not from an unreachable backend: it
/// is reachable and sees ZERO requests. (`synthesize: true` is what
/// still enters the gate — the hits default skips it entirely.)
#[tokio::test]
async fn memory_query_falls_back_when_gate_saturated() {
    let _lock = TEST_LOCK.lock().await;
    let _defaults = GateDefaults;
    let _health = HealthDefaults;
    let (llm_url, mock) = mock_llm(vec![text_response("the agent would have answered")]).await;
    let h = query_harness(&llm_url, DEFAULT_CAPACITY as u32, 1).await;

    let timeouts_before = LLM_GATE.stats().timeouts_total;
    let hold1 = LLM_GATE.enter(None, GATE_BOUND, |_| {}).await.unwrap();
    let hold2 = LLM_GATE.enter(None, GATE_BOUND, |_| {}).await.unwrap();
    assert_eq!(LLM_GATE.stats().active, DEFAULT_CAPACITY);

    let out = mycelium_mcp::tools::memory_query(
        &h.state,
        &h.user,
        &synth_args("zebra stripes under a saturated gate"),
    )
    .await
    .expect("a saturated gate never errors the tool");

    match out {
        mycelium_mcp::tools::QueryOutput::Hits(hits) => {
            assert!(
                !hits.is_empty(),
                "the keyword fallback must still rank the bundle"
            );
        }
        mycelium_mcp::tools::QueryOutput::Answer(a) => {
            panic!(
                "a saturated gate must degrade to the keyword fallback, got: {}",
                a.answer
            )
        }
    }
    assert_eq!(
        llm_requests(&mock).await,
        0,
        "the reachable mock LLM must never be called: the gate, not the backend, caused the fallback"
    );
    assert_eq!(
        LLM_GATE.stats().timeouts_total,
        timeouts_before + 1,
        "exactly one wait-class timeout"
    );

    drop(hold1);
    drop(hold2);
}

/// The wait is a wait, not a refusal: a query queued behind a full gate
/// is admitted the moment a slot frees, and the agent's answer comes
/// back rather than the fallback.
#[tokio::test]
async fn memory_query_admits_when_slot_frees() {
    let _lock = TEST_LOCK.lock().await;
    let _defaults = GateDefaults;
    let _health = HealthDefaults;
    let _health = HealthDefaults;
    let (llm_url, mock) = mock_llm(vec![text_response(
        "Zebras are striped.\n\nSources:\n- /notes/zebras.md",
    )])
    .await;
    // One slot, a generous wait bound: the query must queue, not time out.
    let h = query_harness(&llm_url, 1, 60).await;
    LLM_GATE.set_capacity(1);
    let hold = LLM_GATE.enter(None, GATE_BOUND, |_| {}).await.unwrap();

    let state = h.state.clone();
    let user = h.user.clone();
    let task = tokio::spawn(async move {
        mycelium_mcp::tools::memory_query(
            &state,
            &user,
            &synth_args("what do you know about zebras while a slot is busy"),
        )
        .await
    });
    poll_gate_until(|| LLM_GATE.stats().queued == 1, "the query queued").await;
    assert_eq!(
        llm_requests(&mock).await,
        0,
        "a queued query has not reached the LLM yet"
    );

    drop(hold);
    let out = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("the admitted query must finish")
        .expect("task joined")
        .expect("query must succeed");
    match out {
        mycelium_mcp::tools::QueryOutput::Answer(a) => {
            assert!(
                a.answer.contains("Zebras are striped"),
                "the agent's answer must come back: {}",
                a.answer
            );
        }
        mycelium_mcp::tools::QueryOutput::Hits(hits) => {
            panic!("a freed slot must admit the agent, not fall back: {hits:?}")
        }
    }
    assert_eq!(llm_requests(&mock).await, 1, "exactly one agent LLM call");
}

/// A cached answer costs no LLM time, so it must never wait on the gate:
/// the same question answers promptly even with every slot taken, with
/// no new LLM call and no new timeout.
#[tokio::test]
async fn memory_query_cache_hit_never_waits() {
    let _lock = TEST_LOCK.lock().await;
    let _defaults = GateDefaults;
    let question = "which animal has distinctive stripes in this bundle";
    let (llm_url, mock) = mock_llm(vec![text_response(
        "Zebras do.\n\nSources:\n- /notes/zebras.md",
    )])
    .await;
    // A long bound: a gate wait would be unmistakable against it.
    let h = query_harness(&llm_url, DEFAULT_CAPACITY as u32, 60).await;

    // Prime the exact cache with one successful agent answer.
    let primed = mycelium_mcp::tools::memory_query(&h.state, &h.user, &synth_args(question))
        .await
        .expect("priming query must succeed");
    let primed = match primed {
        mycelium_mcp::tools::QueryOutput::Answer(a) => a.answer,
        mycelium_mcp::tools::QueryOutput::Hits(hits) => {
            panic!("priming query must reach the agent: {hits:?}")
        }
    };
    assert_eq!(llm_requests(&mock).await, 1);

    // Saturate the gate, then ask the identical question.
    let timeouts_before = LLM_GATE.stats().timeouts_total;
    let hold1 = LLM_GATE.enter(None, GATE_BOUND, |_| {}).await.unwrap();
    let hold2 = LLM_GATE.enter(None, GATE_BOUND, |_| {}).await.unwrap();
    assert_eq!(LLM_GATE.stats().active, DEFAULT_CAPACITY);

    let started = Instant::now();
    let out = mycelium_mcp::tools::memory_query(&h.state, &h.user, &synth_args(question))
        .await
        .expect("a cache hit must answer");
    let elapsed = started.elapsed();
    match out {
        mycelium_mcp::tools::QueryOutput::Answer(a) => assert_eq!(a.answer, primed),
        mycelium_mcp::tools::QueryOutput::Hits(hits) => {
            panic!("the cached answer must be returned, not the fallback: {hits:?}")
        }
    }
    assert!(
        elapsed < Duration::from_secs(5),
        "a cache hit must not wait on the gate (took {elapsed:?} of a 60s bound)"
    );
    assert_eq!(
        llm_requests(&mock).await,
        1,
        "a cache hit makes no new LLM call"
    );
    assert_eq!(
        LLM_GATE.stats().timeouts_total,
        timeouts_before,
        "a cache hit never queues, so it never times out"
    );

    drop(hold1);
    drop(hold2);
}

// ---------------------------------------------------------------------------
// Task 3 (memory-lifecycle): memory_query's DEFAULT is the zero-LLM
// ranked-hits path (spec §3.3), with `synthesize` as the opt-in for the
// agent loop and `include_inactive` as the opt-in for history queries.
// Direct-invocation tests (the Task-7 gate harness shape).
// ---------------------------------------------------------------------------

/// Fixed-Green health hygiene (the 2026-10-31 note): `memory_query` is
/// a passive FEEDER of the process-global `LLM_HEALTH` — its LlmClient
/// observes backend failures even along this tool's paths — so dead-LLM
/// fixture runs must not leak Red/Yellow state into the next test in
/// this binary. `QueueWorker::set_health_source` is the queue's READ
/// pin; this side has no reader-facing seam, so the reset guard is the
/// feed-side hygiene: on drop the static re-inits to its fresh default
/// (Green, configured) — held AFTER the lock guard, like GateDefaults.
struct HealthDefaults;

impl Drop for HealthDefaults {
    fn drop(&mut self) {
        let _ = mycelium_librarian::llm_health::LLM_HEALTH.reset_for_test();
    }
}

/// The ten §3.2a-6 hit keys, pinned as key PRESENCE (the store-side
/// values are exercised by the lifecycle tests); `query_top_k` tests
/// size the set.
const HIT_KEYS: [&str; 10] = [
    "scope",
    "path",
    "title",
    "description",
    "snippet",
    "score",
    "timestamp",
    "confirmed_at",
    "status",
    "superseded_by",
];

/// Unwrap the hits variant, phrased for the default-path tests: a
/// `memory_query` that returns the agent's Answer instead of hits
/// violates the zero-LLM default (or proves the fallthrough broke).
fn hits_or_panic(
    out: mycelium_mcp::tools::QueryOutput,
    what: &str,
) -> Vec<mycelium_librarian::agent::QueryHit> {
    match out {
        mycelium_mcp::tools::QueryOutput::Hits(hits) => hits,
        other => panic!("{what} must return enriched hits, got: {other:?}"),
    }
}

/// Spec §3.3 + §3.2a-6: DEFAULT memory_query is the zero-LLM ranked
/// hits — no gate slot, no LLM contact, even with the llm row pointing
/// at an unreachable backend (the deterministic-fallback fixture's
/// shape). Every hit carries ALL TEN keys; the user scope tags the
/// seeded private bundle.
#[tokio::test]
async fn memory_query_default_returns_hits_no_llm() {
    let _lock = TEST_LOCK.lock().await;
    let _defaults = GateDefaults;
    let _health = HealthDefaults;
    let h = query_harness("http://127.0.0.1:1/v1", DEFAULT_CAPACITY as u32, 1).await;

    let out = mycelium_mcp::tools::memory_query(&h.state, &h.user, &query_args("zebra stripes"))
        .await
        .expect("the hits default must answer without an LLM");
    let hits = hits_or_panic(out, "the default memory_query");
    assert!(!hits.is_empty(), "the seeded bundle must rank");
    let first = serde_json::to_value(hits.first().unwrap()).unwrap();
    for key in HIT_KEYS {
        assert!(first.get(key).is_some(), "hit must carry {key}: {first}");
    }
    assert_eq!(
        first["scope"], "user",
        "the seeded bundle is the user scope"
    );
}

/// Spec §3.3: the hits path spans the SAME scopes as the synthesis path
/// — the caller's bundle, global skills, and the library catalog — with
/// the same bookshelf visibility: a private-shelf chapter is ABSENT for
/// the non-admin caller and PRESENT for an admin (agent_integration's
/// library_with_books arrangement, rebuilt over real bookshelf rows so
/// `agent_scopes` derives the visible slugs).
#[tokio::test]
async fn memory_query_hits_span_scopes_with_visibility() {
    let _lock = TEST_LOCK.lock().await;
    let _defaults = GateDefaults;
    let _health = HealthDefaults;
    // The llm row is present but unreachable — irrelevant to the
    // default path, which by construction makes zero LLM calls.
    let h = query_harness("http://127.0.0.1:1/v1", DEFAULT_CAPACITY as u32, 1).await;

    // A global skill whose body matches the query term.
    seed_global_skills(
        h._dir.path(),
        &[(
            "/deploy-rust-service.md",
            "---\ntype: Skill\ntitle: Deploy Rust Service\n---\n\nZebras of deployment: the runbook body.\n",
        )],
    )
    .await;

    // The library catalog: real bookshelf rows (agent_scopes derives
    // visible_slugs from them) + chapter concepts keyed by book slug.
    let store = Store::open(h._dir.path()).await.unwrap();
    let public_shelf = store.create_bookshelf("Public Shelf", true).await.unwrap();
    store
        .create_book(
            public_shelf,
            "public-book",
            "A Public Book",
            "/library/public-book/book.md",
        )
        .await
        .unwrap();
    let secret_shelf = store.create_bookshelf("Secret Shelf", false).await.unwrap();
    store
        .create_book(
            secret_shelf,
            "secret-book",
            "A Secret Book",
            "/library/secret-book/book.md",
        )
        .await
        .unwrap();
    let service_key =
        mycelium_crypto::load_or_create_service_key_with(h._dir.path(), None).unwrap();
    let library = mycelium_store::concept_store::ConceptStore::for_service(
        &store,
        service_key,
        &store.library_dir(),
        "library",
    );
    // Both chapters carry the query's terms ("zebras graze"): the
    // non-admin's ABSENCE below is then the visibility gate, not a
    // no-match.
    library
        .put(
            &mycelium_core::concept::Concept::parse(
                "/public-book/ch-1-zebras.md",
                "---\ntype: Chapter\nbook: public-book\nchapter_index: 1\ntitle: Zebra Chapter\n---\n\nZebras graze on the plain.\n",
            )
            .unwrap(),
        )
        .await
        .unwrap();
    library
        .put(
            &mycelium_core::concept::Concept::parse(
                "/secret-book/ch-1-zebras.md",
                "---\ntype: Chapter\nbook: secret-book\nchapter_index: 1\ntitle: Secret Zebra Chapter\n---\n\nThe private shelf's zebras graze hidden from plain readers.\n",
            )
            .unwrap(),
        )
        .await
        .unwrap();

    // Non-admin caller: all three scopes span the results, but the
    // secret-book chapter is filtered out by shelf visibility.
    let out = mycelium_mcp::tools::memory_query(&h.state, &h.user, &query_args("zebras graze"))
        .await
        .expect("the hits path must answer the non-admin caller");
    let user_hits = hits_or_panic(out, "the non-admin hits path");
    let scopes_seen: std::collections::HashSet<&str> =
        user_hits.iter().map(|hit| hit.scope.as_str()).collect();
    for scope in ["user", "skills", "library"] {
        assert!(
            scopes_seen.contains(scope),
            "hits must span the {scope} scope: {scopes_seen:?}"
        );
    }
    assert!(
        !user_hits
            .iter()
            .any(|hit| hit.path.starts_with("/secret-book/")),
        "a private-shelf chapter must be absent for a non-admin: {user_hits:?}"
    );
    assert!(
        user_hits
            .iter()
            .any(|hit| hit.path == "/public-book/ch-1-zebras.md"),
        "the global-read chapter must be present for a non-admin"
    );

    // Same state, admin caller: unrestricted visibility, the secret
    // chapter surfaces.
    let admin = mycelium_mcp::auth::McpUser {
        role: mycelium_auth::rbac::Role::Admin,
        ..h.user.clone()
    };
    let out = mycelium_mcp::tools::memory_query(&h.state, &admin, &query_args("zebras graze"))
        .await
        .expect("the hits path must answer the admin caller");
    let admin_hits = hits_or_panic(out, "the admin hits path");
    assert!(
        admin_hits
            .iter()
            .any(|hit| hit.path == "/secret-book/ch-1-zebras.md"),
        "a private-shelf chapter must be present for an admin: {admin_hits:?}"
    );
}

/// Spec §3.2a-8: `include_inactive` widens the hits path over retired
/// (and superseded) concepts — default searches hide them.
#[tokio::test]
async fn memory_query_include_inactive_surfaces_retired() {
    let _lock = TEST_LOCK.lock().await;
    let _defaults = GateDefaults;
    let _health = HealthDefaults;
    let h = query_harness("http://127.0.0.1:1/v1", DEFAULT_CAPACITY as u32, 1).await;
    harness_user_cs(&h)
        .await
        .put(
            &mycelium_core::concept::Concept::parse(
                "/notes/retired-zebra.md",
                "---\ntype: Note\ntitle: Retired Zebra Doctrine\nstatus: retired\n---\n\nThe retired zebra doctrine predates the current one.\n",
            )
            .unwrap(),
        )
        .await
        .unwrap();

    // Default: the retired concept is invisible.
    let out =
        mycelium_mcp::tools::memory_query(&h.state, &h.user, &query_args("retired zebra doctrine"))
            .await
            .expect("the default hits path must answer");
    let hits = hits_or_panic(out, "the default hits path");
    assert!(
        !hits.iter().any(|hit| hit.path == "/notes/retired-zebra.md"),
        "a retired concept must not surface by default: {hits:?}"
    );

    // include_inactive: it surfaces, and the status rides the hit.
    let out = mycelium_mcp::tools::memory_query(
        &h.state,
        &h.user,
        &mycelium_mcp::tools::MemoryQueryArgs {
            question: "retired zebra doctrine".to_string(),
            limit: None,
            synthesize: None,
            include_inactive: Some(true),
        },
    )
    .await
    .expect("the include_inactive hits path must answer");
    let hits = hits_or_panic(out, "the include_inactive hits path");
    let hit = hits
        .iter()
        .find(|hit| hit.path == "/notes/retired-zebra.md")
        .expect("the retired concept must surface with include_inactive");
    assert_eq!(
        hit.status.as_deref(),
        Some("retired"),
        "the hit carries the concept's status"
    );
}

/// Spec §3.3: `synthesize: true` keeps the exact agent path — cache →
/// gate → run — and on an unreachable backend the failed agent run
/// degrades into the SAME enriched hits (one shared hits constructor;
/// the old keyword fallback body is gone).
#[tokio::test]
async fn memory_query_synthesize_true_keeps_agent_path() {
    let _lock = TEST_LOCK.lock().await;
    let _defaults = GateDefaults;
    let _health = HealthDefaults;
    let h = query_harness("http://127.0.0.1:1/v1", DEFAULT_CAPACITY as u32, 1).await;

    let out = mycelium_mcp::tools::memory_query(
        &h.state,
        &h.user,
        &synth_args("zebra stripes under synthesis"),
    )
    .await
    .expect("a failed agent run must degrade to hits, not error");
    let hits = hits_or_panic(out, "the failed synthesis");
    assert!(!hits.is_empty(), "the degraded hits must rank the bundle");
    let first = serde_json::to_value(hits.first().unwrap()).unwrap();
    for key in HIT_KEYS {
        assert!(first.get(key).is_some(), "hit must carry {key}: {first}");
    }
}

/// Spec §3.2a-7 + §3.3: the caller-absent default comes from the sealed
/// lifecycle row's `query_top_k` (admin-tunable, 1–64), the hard
/// ceiling is 64 irrespective of the row, and a literal 0 stays zero.
/// ~70 matching concepts prove all three cuts.
#[tokio::test]
async fn memory_query_top_k_default_and_ceiling() {
    let _lock = TEST_LOCK.lock().await;
    let _defaults = GateDefaults;
    let _health = HealthDefaults;
    let h = query_harness("http://127.0.0.1:1/v1", DEFAULT_CAPACITY as u32, 1).await;

    // A sealed lifecycle row with query_top_k = 2 (the mutation-queue
    // tests' sealed-row pattern), applied through the boot-time sync.
    let lifecycle = mycelium_store::revisions::LifecycleConfig {
        revision_retention: mycelium_store::revisions::DEFAULT_REVISION_RETENTION,
        query_top_k: 2,
    };
    h.state
        .config
        .set(
            "lifecycle",
            &mycelium_store::config::seal_config(&h.state.service_key, "lifecycle", &lifecycle),
        )
        .await
        .unwrap();
    mycelium_store::revisions::sync_lifecycle_from_store(&h.state.config, &h.state.service_key)
        .await
        .unwrap();
    assert_eq!(
        mycelium_store::revisions::query_top_k(),
        2,
        "the row must drive the process-global through the sync"
    );

    // ~70 matching concepts in the caller's bundle.
    let cs = harness_user_cs(&h).await;
    for n in 1..=70 {
        let md =
            format!("---\ntype: Note\ntitle: Zebra Herd {n}\n---\n\nZebras of herd {n} graze.\n");
        cs.put(
            &mycelium_core::concept::Concept::parse(&format!("/notes/herd-{n}.md"), &md).unwrap(),
        )
        .await
        .unwrap();
    }

    // Caller absent: the row's default (2) governs the cut.
    let out = mycelium_mcp::tools::memory_query(&h.state, &h.user, &query_args("zebras graze"))
        .await
        .expect("the hits default must answer");
    let hits = hits_or_panic(out, "the absent-limit query");
    assert_eq!(
        hits.len(),
        2,
        "absent limit follows the sealed query_top_k: {hits:?}"
    );

    // Explicit 64: more candidates than the cap, so the ceiling binds.
    let out = mycelium_mcp::tools::memory_query(
        &h.state,
        &h.user,
        &mycelium_mcp::tools::MemoryQueryArgs {
            question: "zebras graze".to_string(),
            limit: Some(64),
            synthesize: None,
            include_inactive: None,
        },
    )
    .await
    .expect("the capped query must answer");
    let hits = hits_or_panic(out, "the 64-cap query");
    assert_eq!(hits.len(), 64, "the hard ceiling is 64: {hits:?}");

    // Literal zero stays zero (§3.2a-7) — the caller asked for nothing.
    let out = mycelium_mcp::tools::memory_query(
        &h.state,
        &h.user,
        &mycelium_mcp::tools::MemoryQueryArgs {
            question: "zebras graze".to_string(),
            limit: Some(0),
            synthesize: None,
            include_inactive: None,
        },
    )
    .await
    .expect("the zero query must answer");
    let hits = hits_or_panic(out, "the zero query");
    assert!(hits.is_empty(), "a literal zero returns no hits: {hits:?}");

    // Leave the process-global at its default for the other tests.
    mycelium_store::revisions::set_query_top_k(mycelium_store::revisions::DEFAULT_QUERY_TOP_K);
}

/// Spec §3.2a-8 schema posture: the new opt-ins are described booleans
/// defaulting to false, `limit` advertises the hits default, and none
/// of the optional args enters `required` (only the question does).
/// Raw `schema_for!` types an Option<bool> as `["boolean","null"]` —
/// the pin is "a boolean arg (nullability aside)".
#[test]
fn include_inactive_schema_defaults_false() {
    let schema = schemars::schema_for!(mycelium_mcp::tools::MemoryQueryArgs);
    let json = serde_json::to_value(&schema).unwrap();

    let is_boolean = |prop: &serde_json::Value| match &prop["type"] {
        serde_json::Value::String(t) => t == "boolean",
        serde_json::Value::Array(t) => t.iter().any(|t| t == "boolean"),
        _ => false,
    };

    let syn = &json["properties"]["synthesize"];
    assert!(is_boolean(syn), "synthesize must be a boolean arg: {syn}");
    assert_eq!(
        syn["description"],
        "Full agent synthesis (LLM tool-call loop: search, read, synthesize). Default false: top-k ranked hits with zero LLM calls."
    );

    let inc = &json["properties"]["include_inactive"];
    assert!(
        is_boolean(inc),
        "include_inactive must be a boolean arg: {inc}"
    );
    assert_eq!(
        inc["description"],
        "Include retired/superseded concepts (history queries). Default false."
    );

    let limit = &json["properties"]["limit"];
    assert_eq!(
        limit["description"], "Maximum hits; default = admin-tunable top_k (8), hard ceiling 64",
        "limit description must advertise the hits default: {limit}"
    );

    let required = json["required"].as_array().unwrap();
    assert_eq!(required.len(), 1, "only the question is required");
    assert_eq!(required[0], "question");
    assert!(
        !required
            .iter()
            .any(|r| r == "synthesize" || r == "include_inactive" || r == "limit"),
        "no opt-in arg is required: {required:?}"
    );
}
