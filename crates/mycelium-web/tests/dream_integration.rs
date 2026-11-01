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
