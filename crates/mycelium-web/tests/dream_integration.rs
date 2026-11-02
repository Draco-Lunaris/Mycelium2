//! Dream's duplicate signal (memory-lifecycle spec §3.4's dream-cosine
//! bullet): the embedding cosine top-2 signal joins v1's jaccard title
//! signal when the scope's vector rows are trustworthy, and the jaccard
//! fallback stays byte-identical when it is not. Mirrors
//! hybrid_retrieval_integration.rs's shape — TEST_LOCK + Drop-guard
//! discipline around the process-global embeddings health machine, real
//! server boot via `mycelium_web::serve` — and drives the surface where
//! `api_dream` passes the VectorStore: the dream runs through
//! `POST /api/v1/dream` and the agent instruction is captured at the
//! mock LLM. Vector rows arrive DIRECTLY via `cs.vectors().upsert`
//! (the brief's contract): no worker in this binary's loop.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::response::IntoResponse;

use mycelium_auth::login::LoginService;
use mycelium_librarian::llm_health::EMBEDDING_HEALTH;
use mycelium_store::Store;
use mycelium_web::AppState;

/// Serializes all tests in this binary around the shared embeddings
/// health machine (the cosine branch gates on it).
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Restores the process-global embeddings health machine to a fresh
/// Green on drop, so a failing assertion's leftover state cannot
/// perturb a sibling test. Declared AFTER the `TEST_LOCK` guard so the
/// restore happens while the lock is held.
struct EmbDefaults;

impl Drop for EmbDefaults {
    fn drop(&mut self) {
        EMBEDDING_HEALTH.reset_for_test();
    }
}

/// Known admin password for the pre-seeded account (the shared boot
/// fixture's shape).
const ADMIN_PASSWORD: &str = "admin password known to the test suite!";

/// A fixed master key the fixture rows are sealed under — the tests
/// drive the store directly, and the injected cache entry keeps the
/// server's dream request path on the SAME key (same HMAC path tokens
/// on both sides of the vector rows).
fn admin_master() -> mycelium_crypto::keys::MasterKey {
    mycelium_crypto::keys::MasterKey::from_bytes(&[7u8; 32]).unwrap()
}

/// The model the fresh fixture rows carry (the "current model").
const MODEL: &str = "dream-test-model";

// ---------- harness (hybrid_retrieval_integration.rs / chat_sessions shape) ----------

/// A request-capturing scripted mock LLM: records every request body,
/// answers from the LIFO script.
async fn capturing_mock_llm(
    responses: Vec<serde_json::Value>,
) -> (String, Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>) {
    let requests: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let req_clone = Arc::clone(&requests);
    let state = Arc::new(tokio::sync::Mutex::new(responses));
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |body: String| {
            let state = state.clone();
            let requests = req_clone.clone();
            async move {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body) {
                    requests.lock().await.push(parsed);
                }
                match state.lock().await.pop() {
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
    (format!("http://{addr}/v1"), requests)
}

fn text_response(text: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{ "message": { "role": "assistant", "content": text } }]
    })
}

/// Boot the real server with the `llm` row pointed at `llm_url`; the
/// pre-seeded admin exists and its master key resolves to
/// [`admin_master`]. Returns (base URL, shutdown, dir, state).
async fn boot(
    llm_url: &str,
) -> (
    String,
    tokio_util::sync::CancellationToken,
    tempfile::TempDir,
    AppState,
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
    // Point the librarian at the mock LLM (the plain-row shape the
    // chat-sessions boot fixture writes).
    state
        .config
        .set(
            "llm",
            &mycelium_web::LlmConfig {
                url: llm_url.to_string(),
                model: "mock".into(),
                api_key: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let https: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let http: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();

    let listener = tokio::net::TcpListener::bind(https).await.unwrap();
    let https_port = listener.local_addr().unwrap().port();
    drop(listener);

    let data_dir = dir.path().to_path_buf();
    let token = shutdown.clone();
    let serve_state = state.clone();
    tokio::spawn(async move {
        let _ = mycelium_web::serve(
            serve_state,
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
        state,
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

/// Log in as admin; returns (cookie, csrf).
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

/// A ConceptStore for the admin's user scope under the fixed master —
/// with the cache entry injected so the server's dream path derives the
/// same keys (the same HMAC path tokens) from the same bytes.
async fn concept_store(state: &AppState) -> mycelium_store::ConceptStore<'_> {
    let admin = state.users.get_by_username("admin").await.unwrap().id;
    let master = admin_master();
    state.master_keys.insert(admin, master.clone());
    mycelium_store::ConceptStore::for_user(&state.store, admin, master)
}

/// Seed a concept whose body links to every `links` path (a fully
/// cross-linked fixture keeps orphans/broken links out of the signals,
/// so the run's signal set is exactly the duplicate signal).
async fn seed(state: &AppState, path: &str, title: &str, links: &[&str]) {
    let mut md = format!("---\ntype: note\ntitle: {title}\n---\nnotes on {path}\n");
    for link in links {
        md.push_str(&format!("see also [{link}]({link})\n"));
    }
    let cs = concept_store(state).await;
    let concept = mycelium_core::Concept::parse(path, &md).unwrap();
    cs.put(&concept).await.unwrap();
}

/// Write one vector row for `path` DIRECTLY (no worker, no queue).
async fn upsert_vec(state: &AppState, path: &str, model: &str, v: &[f32]) {
    let cs = concept_store(state).await;
    cs.vectors()
        .upsert(path, model, v.len() as u32, v)
        .await
        .unwrap();
}

/// POST /api/v1/dream; asserts HTTP 200 and returns the report JSON.
async fn post_dream(
    cl: &reqwest::Client,
    base: &str,
    cookie: &str,
    csrf: &str,
) -> serde_json::Value {
    let resp = cl
        .post(format!("{base}/api/v1/dream"))
        .header("cookie", cookie)
        .header("x-csrf-token", csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the dream endpoint answers");
    resp.json().await.unwrap()
}

/// The dream instruction (the user message of the first agent call).
fn instruction_of(request: &serde_json::Value) -> String {
    request["messages"]
        .as_array()
        .expect("an agent request carries messages")
        .iter()
        .find(|m| m["role"].as_str() == Some("user"))
        .and_then(|m| m["content"].as_str())
        .expect("the dream instruction is the user message")
        .to_string()
}

/// One signal section of the instruction (from the header to the next
/// blank line). Section-scoped assertions are the real pin: the SYSTEM
/// prompt carries the whole bundle layout, so "the path appears
/// nowhere" would never be the claim.
fn section_of(instruction: &str, header: &str) -> Option<String> {
    let idx = instruction.find(header)?;
    let rest = &instruction[idx..];
    let end = rest.find("\n\n").unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Test 1 (brief): two concepts with NO shared title tokens and fresh
/// vector rows at cosine ≈ 0.99 (≥ the 0.92 default) → the pair rides
/// the dream's LIKELY DUPLICATES instruction. Jaccard cannot see this
/// pair; only the cosine branch can.
#[tokio::test]
async fn cosine_duplicate_candidates_find_close_pairs_without_shared_tokens() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    let (llm_url, requests) =
        capturing_mock_llm(vec![text_response("Maintenance complete.")]).await;
    let (base, _shutdown, _dir, state) = boot(&llm_url).await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    const A: &str = "/notes/alpha-executor.md";
    const B: &str = "/notes/beta-quantum.md";
    seed(&state, A, "Executor Pump Internals", &[B]).await;
    seed(&state, B, "Entanglement Probe Log", &[A]).await;
    // cos([1,0,0], [0.99,0.14,0]) ≈ 0.990 ≥ 0.92; with two rows the
    // top-2 condition is trivially mutual.
    upsert_vec(&state, A, MODEL, &[1.0, 0.0, 0.0]).await;
    upsert_vec(&state, B, MODEL, &[0.99, 0.14, 0.0]).await;

    let report = post_dream(&cl, &base, &cookie, &csrf).await;
    assert_eq!(report["ran"], true, "the cosine pair is a signal: {report}");
    assert_eq!(report["summary"], "Maintenance complete.");

    let reqs = requests.lock().await;
    assert_eq!(reqs.len(), 1, "one agent call for the one-signal pass");
    let instruction = instruction_of(reqs.first().unwrap());
    assert!(
        !instruction.contains("ORPHANED") && !instruction.contains("BROKEN LINKS"),
        "the cross-linked fixture keeps the dupes signal alone: {instruction}"
    );
    let section = section_of(&instruction, "LIKELY DUPLICATES")
        .expect("the cosine pair fires the duplicate signal");
    assert!(
        section.contains(&format!("- {A} ↔ {B}")),
        "the canonical cosine pair: {section}"
    );
}

/// Test 2 (brief): no vector rows in the scope at all → the branch
/// contributes nothing and dream behaves exactly as today: the
/// jaccard ≥ 0.8 pair fires, and a would-be cosine-0.95 pair (title
/// tokens disjoint) plus a below-bar jaccard pair (3 of 5 tokens,
/// 0.6) stay invisible — no threshold drift.
#[tokio::test]
async fn jaccard_fallback_survives_intact_when_vectors_absent() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    let (llm_url, requests) =
        capturing_mock_llm(vec![text_response("Maintenance complete.")]).await;
    let (base, _shutdown, _dir, state) = boot(&llm_url).await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    const DUP_A: &str = "/dupes/alpha.md";
    const DUP_B: &str = "/dupes/beta.md";
    const FAR_A: &str = "/dupes/signal.md";
    const FAR_B: &str = "/dupes/spectral.md";
    const MID_A: &str = "/dupes/near-alpha.md";
    const MID_B: &str = "/dupes/near-beta.md";
    // Today's signal: identical titles (jaccard 1.0).
    seed(&state, DUP_A, "Caching Layer Design", &[DUP_B]).await;
    seed(&state, DUP_B, "Caching Layer Design", &[DUP_A]).await;
    // A pair a 0.95 cosine WOULD pair if any rows existed — none do
    // (zero rows in the scope), so it must stay invisible.
    seed(&state, FAR_A, "Vector Signal Alpha", &[FAR_B]).await;
    seed(&state, FAR_B, "Spectral Probe Beta", &[FAR_A]).await;
    // 3 of 5 title tokens (jaccard 0.6): below the unchanged 0.8 bar.
    seed(&state, MID_A, "Alpha Beta Gamma Delta", &[MID_B]).await;
    seed(&state, MID_B, "Alpha Beta Gamma Epsilon", &[MID_A]).await;

    let report = post_dream(&cl, &base, &cookie, &csrf).await;
    assert_eq!(
        report["ran"], true,
        "the jaccard pair still fires: {report}"
    );

    let reqs = requests.lock().await;
    assert_eq!(reqs.len(), 1);
    let instruction = instruction_of(reqs.first().unwrap());
    let section = section_of(&instruction, "LIKELY DUPLICATES")
        .expect("the identical-titles pair is today's signal");
    assert!(
        section.contains(&format!("- {DUP_A} ↔ {DUP_B}")),
        "the jaccard pair survives intact: {section}"
    );
    for path in [FAR_A, FAR_B, MID_A, MID_B] {
        assert!(
            !section.contains(path),
            "without vectors the {path} pair is invisible (no drift): {section}"
        );
    }
}

/// Test 3 (brief): rows written under an old model and marked stale by
/// the corpus invalidation (mark_corpus_stale — the worker prologue's
/// exact statement on a model swap) count for NEITHER population nor
/// scoring: the cosine pair stays invisible while the jaccard pair,
/// alive on identical titles, proves the fallback.
#[tokio::test]
async fn stale_model_rows_are_ignored_by_the_dream_signal() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    let (llm_url, requests) =
        capturing_mock_llm(vec![text_response("Maintenance complete.")]).await;
    let (base, _shutdown, _dir, state) = boot(&llm_url).await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    const OLD_A: &str = "/stale/alpha-executor.md";
    const OLD_B: &str = "/stale/beta-quantum.md";
    const DUP_A: &str = "/stale/rebuild-one.md";
    const DUP_B: &str = "/stale/rebuild-two.md";
    seed(&state, OLD_A, "Executor Pump Internals", &[OLD_B]).await;
    seed(&state, OLD_B, "Entanglement Probe Log", &[OLD_A]).await;
    upsert_vec(&state, OLD_A, "old-dream-model", &[1.0, 0.0, 0.0]).await;
    upsert_vec(&state, OLD_B, "old-dream-model", &[0.99, 0.14, 0.0]).await;
    // The model swap's corpus invalidation: rows under a different
    // model get the stale marker (the ONLY carrier of "old model").
    mycelium_store::vectors::VectorStore::mark_corpus_stale(state.store.pool(), MODEL, 3)
        .await
        .unwrap();
    // The alive fallback pair.
    seed(&state, DUP_A, "Index Rebuild Notes", &[DUP_B]).await;
    seed(&state, DUP_B, "Index Rebuild Notes", &[DUP_A]).await;

    let report = post_dream(&cl, &base, &cookie, &csrf).await;
    assert_eq!(report["ran"], true, "the jaccard pair fires: {report}");

    let reqs = requests.lock().await;
    assert_eq!(reqs.len(), 1);
    let instruction = instruction_of(reqs.first().unwrap());
    let section = section_of(&instruction, "LIKELY DUPLICATES")
        .expect("the identical-titles pair proves the fallback");
    assert!(
        section.contains(&format!("- {DUP_A} ↔ {DUP_B}")),
        "jaccard fallback intact: {section}"
    );
    for path in [OLD_A, OLD_B] {
        assert!(
            !section.contains(path),
            "the stale-model cosine pair must not surface: {section}"
        );
    }
}

/// Population counts LIVE rows, not raw rows (review R1): a Red
/// backend plus ONE live fresh row and one ORPHAN fresh row (its
/// concept never existed) under the same model — the raw count of 2
/// would admit the cosine branch, whose only live concept then rides
/// no scan at all (embedded concepts skip jaccard, and the orphan
/// pairs with nothing), silently killing the title pair. The live
/// count of 1 keeps the branch absent and the jaccard pair surfaces.
#[tokio::test]
async fn population_counts_live_rows_not_orphan_rows() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    // Red the honest way: 3 consecutive failed observations.
    for _ in 0..3 {
        EMBEDDING_HEALTH.note_failure();
    }
    assert!(matches!(
        EMBEDDING_HEALTH.state(),
        mycelium_librarian::llm_health::BackendState::Red
    ));
    let (llm_url, requests) =
        capturing_mock_llm(vec![text_response("Maintenance complete.")]).await;
    let (base, _shutdown, _dir, state) = boot(&llm_url).await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    const DUP_A: &str = "/orphanpop/rebuild-one.md";
    const DUP_B: &str = "/orphanpop/rebuild-two.md";
    seed(&state, DUP_A, "Index Rebuild Notes", &[DUP_B]).await;
    seed(&state, DUP_B, "Index Rebuild Notes", &[DUP_A]).await;
    // One live fresh row + one orphan row, both under MODEL.
    upsert_vec(&state, DUP_A, MODEL, &[1.0, 0.0, 0.0]).await;
    upsert_vec(&state, "/gone/never-a-concept.md", MODEL, &[0.0, 1.0, 0.0]).await;

    let report = post_dream(&cl, &base, &cookie, &csrf).await;
    assert_eq!(
        report["ran"], true,
        "the orphan row must not admit the cosine branch and drop the pair: {report}"
    );
    let reqs = requests.lock().await;
    assert_eq!(reqs.len(), 1);
    let instruction = instruction_of(reqs.first().unwrap());
    let section = section_of(&instruction, "LIKELY DUPLICATES")
        .expect("the jaccard pair survives the Red + orphan-row shape");
    assert!(
        section.contains(&format!("- {DUP_A} ↔ {DUP_B}")),
        "live-count population keeps the branch absent: {section}"
    );
}

/// Top-2 pruning (the brief's candidate condition beyond the
/// threshold): the hub and the far concept sit at cosine ≈ 0.985 ≥
/// 0.92, but the far one is in no hub-side top-2 and the hub is in no
/// far-side top-2 — so the pair never enters the instruction, while
/// every drawn pair does.
#[tokio::test]
async fn top_two_pruning_keeps_a_pair_out_even_above_the_threshold() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    let (llm_url, requests) =
        capturing_mock_llm(vec![text_response("Maintenance complete.")]).await;
    let (base, _shutdown, _dir, state) = boot(&llm_url).await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    const HUB: &str = "/top2/alpha-hub.md";
    const N1: &str = "/top2/bravo-one.md";
    const N2: &str = "/top2/charlie-two.md";
    const FAR: &str = "/top2/delta-far.md";
    let all = [HUB, N1, N2, FAR];
    // Every concept links to every other: no orphans, no broken links.
    for (i, path) in all.iter().enumerate() {
        let links: Vec<&str> = all
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, p)| *p)
            .collect();
        seed(&state, path, all_titles(i), &links).await;
    }
    upsert_vec(&state, HUB, MODEL, &[1.0, 0.0, 0.0]).await;
    upsert_vec(&state, N1, MODEL, &[0.995, 0.0999, 0.0]).await;
    upsert_vec(&state, N2, MODEL, &[0.99, 0.1411, 0.0]).await;
    upsert_vec(&state, FAR, MODEL, &[0.985, 0.1737, 0.0]).await;

    let report = post_dream(&cl, &base, &cookie, &csrf).await;
    assert_eq!(report["ran"], true, "drawn pairs fire the signal: {report}");

    let reqs = requests.lock().await;
    assert_eq!(reqs.len(), 1);
    let instruction = instruction_of(reqs.first().unwrap());
    let section = section_of(&instruction, "LIKELY DUPLICATES").expect("pairs fire the signal");
    // The five drawn pairs (every pair except hub↔far is in at least
    // one endpoint's top-2).
    for (a, b) in [(HUB, N1), (HUB, N2), (N1, N2), (N1, FAR), (N2, FAR)] {
        assert!(
            section.contains(&format!("- {a} ↔ {b}")),
            "the drawn pair {a} ↔ {b}: {section}"
        );
    }
    assert!(
        !section.contains(&format!("{HUB} ↔ {FAR}")),
        "cos ≈ 0.985 but never drawn into a top-2: {section}"
    );
    assert_eq!(
        section.matches("↔").count(),
        5,
        "exactly the five drawn pairs: {section}"
    );
}

fn all_titles(i: usize) -> &'static str {
    [
        "Scheduler Desk Notes",
        "Beacon Trail Mapping",
        "Coriolis Force Charts",
        "Tidal Gate Registers",
    ][i]
}

/// One assistant tool-call turn in the OpenAI wire shape the agent's
/// parser expects (chat_sessions_integration.rs's helper).
fn tool_call_response(id: &str, name: &str, args: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args.to_string() }
                }]
            }
        }]
    })
}

/// The full op chain of one concept's revision rows (ascending), and
/// its provenance labels with them — the deterministic shape of the
/// dream's two-store write history.
async fn op_chain(state: &AppState, path: &str) -> Vec<(String,)> {
    let admin = state.users.get_by_username("admin").await.unwrap().id;
    sqlx::query_as(
        "SELECT op FROM concept_revisions WHERE scope_id = ? AND path = ? ORDER BY revision_no",
    )
    .bind(format!("user:{admin}"))
    .bind(path)
    .fetch_all(state.store.pool())
    .await
    .unwrap()
}

/// Seed `path` with an initial put plus `updates` identical rewrites —
/// each re-put is a mechanically derived `update` revision (the
/// trailing-update chain the consolidation signal counts).
async fn seed_with_updates(state: &AppState, path: &str, md: &str, updates: usize) {
    let cs = concept_store(state).await;
    let concept = mycelium_core::Concept::parse(path, md).unwrap();
    cs.put(&concept).await.unwrap();
    for _ in 0..updates {
        cs.put(&concept).await.unwrap();
    }
}

/// The two-phase dream (Ruling 3) on its only pinned surface —
/// POST /api/v1/dream, whose handler builds BOTH stores: a concept
/// whose trailing-update chain exceeds `consolidation_patches()` fires
/// the consolidation signal, the dream runs TWO passes under the one
/// gate permit (a main-signal pass through `store`, then a
/// consolidation-only pass through the `consolidate`-tagged store), the
/// scripted agent patches the body via patch_concept's new `body`
/// argument, and the revision log ends up mixed: the consolidation
/// concept's tail row is `consolidate` (count reset to 0 — the op row
/// breaks the trailing-update chain) while the main pass's own write in
/// the same run stays mechanically tagged (`create`, never
/// `consolidate`).
#[tokio::test]
async fn consolidation_signal_writes_op_consolidate_revision() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();

    const ORPHAN: &str = "/consolidated-run/orphan.md";
    const TARGET: &str = "/consolidated-run/disorganized.md";
    const MAIN_WRITE: &str = "/consolidated-run/main-write.md";
    let summary = "Current state of the layout.\n\n# History\n\nolder notes";

    // LIFO script. The main pass runs first: one write_concept (its own
    // store, mechanically tagged), then its closing text. The
    // consolidation pass follows: one patch_concept with the new `body`
    // argument (the consolidate-tagged store), then its closing text.
    let (llm_url, requests) = capturing_mock_llm(vec![
        text_response("Consolidation pass complete."),
        tool_call_response(
            "call-cons",
            "patch_concept",
            serde_json::json!({ "path": TARGET, "body": summary }),
        ),
        text_response("Main pass complete."),
        tool_call_response(
            "call-main",
            "write_concept",
            serde_json::json!({
                "path": MAIN_WRITE,
                "frontmatter": { "type": "Note", "title": "Main Written" },
                "body": "written by the main pass",
            }),
        ),
    ])
    .await;
    let (base, _shutdown, _dir, state) = boot(&llm_url).await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    // The main signal (a lone unlinked concept) sends the main pass;
    // the 9-update chain on TARGET (> consolidation_patches() = 8 by
    // default) sends the consolidation pass. Every put stamps a fresh
    // confirmed_at, so nothing else is stale/expired here — the run's
    // signals are exactly the two families.
    seed(&state, ORPHAN, "Lonely Main Note", &[]).await;
    seed_with_updates(
        &state,
        TARGET,
        "---\ntype: note\ntitle: Disorganized Storage Layout\n---\n\nworking notes\n",
        9,
    )
    .await;

    let report = post_dream(&cl, &base, &cookie, &csrf).await;
    assert_eq!(report["ran"], true, "two signals, one dream: {report}");
    assert_eq!(
        report["summary"], "Main pass complete.\n\nConsolidation pass complete.",
        "both passes' summaries aggregate into the one report"
    );

    let reqs = requests.lock().await;
    assert_eq!(reqs.len(), 4, "two passes, two steps each: {reqs:?}");

    // The MAIN instruction carries the graph signal only — the
    // consolidation family never leaks into the untagged pass.
    let main_instruction = instruction_of(&reqs[0]);
    assert!(
        main_instruction.contains("ORPHANED"),
        "the orphan seeds the main pass: {main_instruction}"
    );
    assert!(
        !main_instruction.contains("DISORGANIZED") && !main_instruction.contains("consolidation"),
        "no consolidation work in the main pass: {main_instruction}"
    );

    // The CONSOLIDATION instruction carries the disorganized concept
    // and steers the agent at patch_concept's `body` argument.
    let cons_instruction = instruction_of(&reqs[2]);
    let cons_section = section_of(&cons_instruction, "DISORGANIZED CONCEPTS")
        .expect("the over-threshold chain fires the consolidation signal");
    assert!(
        cons_section.contains(TARGET) && cons_section.contains("9 consecutive patch revisions"),
        "the over-threshold chain is named: {cons_section}"
    );
    assert!(
        cons_instruction.contains("`body`"),
        "the consolidation pass names the body argument: {cons_instruction}"
    );
    assert!(
        !cons_instruction.contains("ORPHANED"),
        "main-signal work never reappears in the consolidation pass: {cons_instruction}"
    );

    // The revision log carries the MIXED tags: the main pass's own
    // write is a mechanically derived `create`, while the seeded chain
    // (create + 9 updates) ends in exactly one `consolidate` row.
    let main_ops = op_chain(&state, MAIN_WRITE).await;
    assert_eq!(
        main_ops.iter().map(|(op,)| op.as_str()).collect::<Vec<_>>(),
        vec!["create"],
        "the main store's write is never consolidate-tagged: {main_ops:?}"
    );
    let target_ops = op_chain(&state, TARGET).await;
    let target_ops: Vec<&str> = target_ops.iter().map(|(op,)| op.as_str()).collect();
    let mut expected = vec!["create"];
    expected.extend(std::iter::repeat_n("update", 9));
    expected.push("consolidate");
    assert_eq!(
        target_ops, expected,
        "create + 9 updates + exactly one consolidate on top: {target_ops:?}"
    );

    // The consolidate row RESETS the trailing-update count (Ruling 2).
    let admin = state.users.get_by_username("admin").await.unwrap().id;
    let master = admin_master();
    let revisions =
        mycelium_store::revisions::RevisionStore::for_user(state.store.pool(), admin, &master);
    let count_after = revisions
        .consecutive_update_count(&revisions.path_token_of(TARGET))
        .await
        .unwrap();
    assert_eq!(count_after, 0, "the consolidate row breaks the chain");

    // The scripted `body` patch landed as a whole-body replace with the
    // frontmatter untouched.
    let got = cl
        .get(format!("{base}/api/v1/concepts{TARGET}"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200, "the consolidation patch landed");
    let stored = mycelium_core::Concept::parse(TARGET, &got.text().await.unwrap()).unwrap();
    assert_eq!(stored.body, summary, "the body argument replaced the body");
    assert_eq!(
        stored.frontmatter.title.as_deref(),
        Some("Disorganized Storage Layout"),
        "frontmatter survives the consolidate rewrite"
    );
}

/// The consolidation-only dream (the controller's adjudication, Task 3's
/// scheduler rides it): a scope with NO main-signal family but an
/// over-threshold trailing-update chain runs exactly ONE pass — the
/// consolidation pass — never an empty main pass under the main
/// preamble. The two-pass test above saw 4 requests because both
/// families fired; here the sequence is only the consolidation pass's
/// own (instruction + tool call, then the closing text — one
/// run_mutation with a tool call is exactly two LLM requests): the
/// instruction opens with the consolidation preamble (not the main
/// "This is a scheduled maintenance pass" one), and the revision tail
/// records op `consolidate` with the count reset — the two-pass test's
/// assertions minus any main-pass request.
#[tokio::test]
async fn consolidation_only_dream_runs_single_consolidation_pass() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();

    const TARGET: &str = "/consolidation-only/disorganized.md";
    let summary = "Current state only.\n\n# History\n\nolder notes";

    // Exactly ONE concept, self-linked (the orphan rule counts incoming
    // edges, and a self-link is one) with a 9-update trailing chain
    // over the 8 default — the run's ONLY signal family. No other
    // concept exists, so orphans/broken links/duplicates/oversized/
    // stale/expired/broken supersedes are all empty (every put stamps a
    // fresh confirmed_at).
    const MD: &str = "---\ntype: note\ntitle: Disorganized Storage Layout\n---\n\nworking notes, see [self](/consolidation-only/disorganized.md)\n";
    let (llm_url, requests) = capturing_mock_llm(vec![
        text_response("Consolidation pass complete."),
        tool_call_response(
            "call-cons",
            "patch_concept",
            serde_json::json!({ "path": TARGET, "body": summary }),
        ),
    ])
    .await;
    let (base, _shutdown, _dir, state) = boot(&llm_url).await;
    let cl = client();
    let (cookie, csrf) = login_admin(&cl, &base).await;

    seed_with_updates(&state, TARGET, MD, 9).await;

    let report = post_dream(&cl, &base, &cookie, &csrf).await;
    assert_eq!(
        report["ran"], true,
        "the consolidation signal runs the dream: {report}"
    );
    assert_eq!(
        report["summary"], "Consolidation pass complete.",
        "one pass: the report aggregates only the consolidation summary"
    );

    let reqs = requests.lock().await;
    assert_eq!(
        reqs.len(),
        2,
        "exactly the consolidation pass's two steps, no empty main pass: {reqs:?}"
    );

    // Its instruction IS the consolidation preamble — and nothing rides
    // the main one (the prefixes are the two distinct preambles; the
    // consolidation one embeds the main wording AFTER "pass of").
    let instruction = instruction_of(&reqs[0]);
    assert!(
        instruction.starts_with("This is the consolidation pass of a scheduled maintenance pass"),
        "the cons-only instruction opens with the consolidation preamble: {instruction}"
    );
    assert!(
        !instruction.starts_with("This is a scheduled maintenance pass"),
        "no empty main pass rides the main preamble: {instruction}"
    );
    let cons_section = section_of(&instruction, "DISORGANIZED CONCEPTS")
        .expect("the over-threshold chain fires the consolidation signal");
    assert!(
        cons_section.contains(TARGET) && cons_section.contains("9 consecutive patch revisions"),
        "the over-threshold chain is named: {cons_section}"
    );
    assert!(
        instruction.contains("`body`"),
        "the consolidation pass names the body argument: {instruction}"
    );

    // The one patch records exactly one `consolidate` row atop the
    // seeded chain (create + 9 updates), resetting the trailing count.
    let target_ops = op_chain(&state, TARGET).await;
    let target_ops: Vec<&str> = target_ops.iter().map(|(op,)| op.as_str()).collect();
    let mut expected = vec!["create"];
    expected.extend(std::iter::repeat_n("update", 9));
    expected.push("consolidate");
    assert_eq!(
        target_ops, expected,
        "create + 9 updates + exactly one consolidate: {target_ops:?}"
    );
    let admin = state.users.get_by_username("admin").await.unwrap().id;
    let master = admin_master();
    let revisions =
        mycelium_store::revisions::RevisionStore::for_user(state.store.pool(), admin, &master);
    let count_after = revisions
        .consecutive_update_count(&revisions.path_token_of(TARGET))
        .await
        .unwrap();
    assert_eq!(count_after, 0, "the consolidate row breaks the chain");
}
