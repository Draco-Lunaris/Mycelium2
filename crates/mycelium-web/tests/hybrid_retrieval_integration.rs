//! Hybrid retrieval integration (memory-lifecycle spec §3.3, task 5):
//! `SearchMode` + per-scope RRF fusion across the composer's scopes,
//! wired to the VectorStore behind the embeddings client — the web REST
//! surface's pins. Mirrors embeddings_admin_integration.rs's TEST_LOCK +
//! Drop-guard discipline (the passive health hooks ride EVERY embed the
//! tests drive, so the process-global machines must be serialized and
//! restored the same way).
//!
//! The vector rows arrive DIRECTLY (the brief's contract): the test
//! embeds the fixture concept's `searchable_text` through a mock
//! embeddings client and `cs.vectors().upsert(...)`. No worker runs in
//! this binary — the queue path is Task 4's surface; this keeps the
//! fusion's own failure surface isolated.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::response::IntoResponse;

use mycelium_auth::login::LoginService;
use mycelium_librarian::embeddings::{EmbeddingsClient, EmbeddingsConfig};
use mycelium_librarian::llm_health::EMBEDDING_HEALTH;
use mycelium_store::Store;
use mycelium_web::AppState;

/// Serializes all tests in this binary around the shared embeddings
/// health machine (every embed request the tests drive feeds it).
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
/// server's request path on the SAME key (`admin_master()` from the
/// chat-resume file's pattern).
fn admin_master() -> mycelium_crypto::keys::MasterKey {
    mycelium_crypto::keys::MasterKey::from_bytes(&[7u8; 32]).unwrap()
}

/// The one-note user-scope fixture: a body about the executor's task
/// machinery that shares NO lexeme with the paraphrase question below
/// (the contrast pin's whole point — the vector source is the only
/// thing that can find it).
const FIXTURE_PATH: &str = "/notes/runtime.md";
const FIXTURE_MARKDOWN: &str = "---\ntype: note\ntitle: Rust Async Basics\n---\nthe executor pumps wakers and schedules green threads on the worker pool without blocking the host thread\n";

/// The paraphrase question: same subject, zero shared tokens with the
/// fixture (executor/wakers/schedules… vs concurrent/runtime/tasks).
const PARAPHRASE_Q: &str = "concurrent runtime parallel tasks";
/// A query whose mock vector is perpendicular to the doc vector (and
/// which lexically matches nothing either).
const ORTHOGONAL_Q: &str = "quantum entanglement probe";
/// A query that DOES share body tokens (the lexical-mode positive).
const LEXICAL_Q: &str = "green threads";

const MODEL: &str = "test-model";

/// One embeddings mock keyed on the input text: the fixture doc gets
/// the doc vector, the paraphrase question gets a near-parallel one,
/// anything else gets a perpendicular one. With `fail`, every request
/// is answered with a bare 503 instead. Returns the base URL (server
/// root WITHOUT /v1 — the client POSTs {base_url}/v1/embeddings) and a
/// counter of the times the PARAPHRASE question itself was embedded
/// (the once-per-request pin; a per-scope embed would count 3).
async fn hybrid_mock(fail: bool) -> (String, Arc<AtomicU32>) {
    let paraphrase_embeds = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&paraphrase_embeds);
    let app = axum::Router::new().route(
        "/v1/embeddings",
        axum::routing::post(move |body: axum::Json<serde_json::Value>| {
            let counter = Arc::clone(&counter);
            async move {
                if fail {
                    return (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        axum::Json(serde_json::json!({
                            "error": {"message": "embeddings backend down"}
                        })),
                    )
                        .into_response();
                }
                let input = body.0["input"][0].as_str().unwrap_or("");
                let emb: Vec<f32> = if input.contains("wakers") {
                    // The fixture doc: a straight shot along axis 0.
                    vec![1.0, 0.0, 0.0]
                } else if input.contains("concurrent") {
                    // The paraphrase question: near-parallel (cos ≈ 0.898).
                    counter.fetch_add(1, Ordering::Relaxed);
                    vec![0.9, 0.44, 0.0]
                } else {
                    // Everything else: perpendicular — cosine 0 must
                    // never surface as a hit.
                    vec![0.0, 0.0, 1.0]
                };
                axum::Json(serde_json::json!({
                    "data": [{"index": 0, "embedding": emb}]
                }))
                .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, paraphrase_embeds)
}

async fn boot() -> (
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

/// Log in as `name`; returns (cookie, csrf).
async fn login_as(
    client: &reqwest::Client,
    base: &str,
    name: &str,
    password: &str,
) -> (String, String) {
    let login = client
        .post(format!("{base}/login"))
        .form(&[("username", name), ("password", password)])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
    let cookie = cookie_from(&login);
    let csrf = csrf_from_page(client, &format!("{base}/"), &cookie).await;
    (cookie, csrf)
}

/// The `embeddings` config row the surfaces read per request (sealed
/// under the service key, the admin save path's own shape).
async fn write_embeddings_row(state: &AppState, base_url: &str, model: &str) {
    let cfg = EmbeddingsConfig {
        base_url: base_url.to_string(),
        model: model.to_string(),
        api_key: None,
        timeout_secs: 5,
        batch: 8,
    };
    let sealed = mycelium_web::state::seal_config(&state.service_key, "embeddings", &cfg);
    state.config.set("embeddings", &sealed).await.unwrap();
}

/// Seed the fixture concept into the admin's user scope and populate
/// its vector row DIRECTLY: embed `searchable_text` through a mock
/// client (which returns the doc vector), then `upsert`. No worker, no
/// queue — the brief's direct-arrival contract.
async fn seed_concept_and_vector(state: &AppState, emb_url: &str) {
    let cs = concept_store(state).await;
    let concept = mycelium_core::Concept::parse(FIXTURE_PATH, FIXTURE_MARKDOWN).unwrap();
    cs.put(&concept).await.unwrap();
    let cfg = EmbeddingsConfig {
        base_url: emb_url.to_string(),
        model: MODEL.to_string(),
        api_key: None,
        timeout_secs: 5,
        batch: 8,
    };
    let client = EmbeddingsClient::new(&cfg);
    let text = mycelium_core::search::searchable_text(&concept);
    let vecs = client.embed_batch(&[text]).await.unwrap();
    assert_eq!(vecs.len(), 1, "one input, one vector");
    cs.vectors()
        .upsert(FIXTURE_PATH, MODEL, vecs[0].len() as u32, &vecs[0])
        .await
        .unwrap();
}

/// A ConceptStore for the admin's user scope under the fixed master —
/// with the cache entry injected so the server's request path derives
/// the same keys from the same bytes.
async fn concept_store(state: &AppState) -> mycelium_store::ConceptStore<'_> {
    let admin = state.users.get_by_username("admin").await.unwrap().id;
    let master = admin_master();
    state.master_keys.insert(admin, master.clone());
    mycelium_store::ConceptStore::for_user(&state.store, admin, master)
}

/// GET /api/v1/search with an optional `mode` param; returns (status,
/// body JSON).
async fn api_search(
    cl: &reqwest::Client,
    base: &str,
    cookie: &str,
    q: &str,
    mode: Option<&str>,
) -> (axum::http::StatusCode, serde_json::Value) {
    let query: Vec<(&str, &str)> = match mode {
        Some(m) => vec![("q", q), ("mode", m)],
        None => vec![("q", q)],
    };
    let resp = cl
        .get(format!("{base}/api/v1/search"))
        .header("cookie", cookie)
        .query(&query)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap())
}

async fn login_admin(cl: &reqwest::Client, base: &str) -> (String, String) {
    login_as(cl, base, "admin", ADMIN_PASSWORD).await
}

/// §6.5's paraphrase acceptance: the question shares NO tokens with the
/// fixture, so lexical search alone finds nothing — only the auto
/// composer's vector source does. (Also pins the mode param's error
/// shape and the HTML view honoring `mode=lexical`.)
#[tokio::test]
async fn hybrid_retrieval_finds_a_paraphrase_without_shared_tokens() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    let (emb_url, paraphrase_embeds) = hybrid_mock(false).await;
    let (base, _shutdown, _dir, state) = boot().await;
    let cl = client();
    let (cookie, _csrf) = login_admin(&cl, &base).await;
    seed_concept_and_vector(&state, &emb_url).await;
    write_embeddings_row(&state, &emb_url, MODEL).await;

    // CONTRAST pin — explicit Lexical mode: zero hits (no shared
    // tokens), and the paraphrase question was never embedded.
    let (status, body) = api_search(&cl, &base, &cookie, PARAPHRASE_Q, Some("lexical")).await;
    assert_eq!(status, 200);
    assert_eq!(
        body.as_array().expect("search returns an array").len(),
        0,
        "lexical-only must not see the paraphrase: {body}"
    );
    assert_eq!(
        paraphrase_embeds.load(Ordering::Relaxed),
        0,
        "the lexical path never embeds"
    );

    // The same question under Auto (the API's default): the vector
    // pool supplies the hit. The embed ran exactly ONCE per request —
    // before the scope loops, never per scope (include_global makes
    // this the skills/library loops' guard too).
    let (status, body) = api_search(&cl, &base, &cookie, PARAPHRASE_Q, None).await;
    assert_eq!(status, 200);
    let results = body.as_array().expect("search returns an array");
    assert_eq!(
        paraphrase_embeds.load(Ordering::Relaxed),
        1,
        "one qvec embed per REQUEST across all scopes"
    );
    assert_eq!(results.len(), 1, "the paraphrase hit: {body}");
    assert_eq!(results[0]["concept_path"], FIXTURE_PATH);
    assert_eq!(results[0]["scope"], "user");
    assert_eq!(results[0]["title"], "Rust Async Basics");
    // Enrichment from the concept itself (no lexical hit to borrow
    // from): the snippet is the body text.
    assert!(
        results[0]["snippet"]
            .as_str()
            .unwrap_or("")
            .starts_with("the executor pumps wakers"),
        "enriched snippet: {:?}",
        results[0]["snippet"]
    );
    // The fused score: w_vec/(k + rank) = 1/61 with the lexical pool
    // absent (rrf_params' defaults stamp the composer).
    let score = results[0]["score"].as_f64().unwrap_or(f64::NAN);
    assert!(
        (score - 1.0 / 61.0).abs() < 1e-6,
        "fused score {score} vs w_vec/(k+1)"
    );

    // An unknown mode is the client's bad input — HTTP 400, never a
    // silent fall-through.
    let (status, _body) = api_search(&cl, &base, &cookie, PARAPHRASE_Q, Some("hybrid")).await;
    assert_eq!(status, 400, "unknown mode must be rejected");

    // The HTML view honors the mode param the same way (mode accepted;
    // a lexical view of the lexical query renders).
    let page = cl
        .get(format!("{base}/search"))
        .header("cookie", &cookie)
        .query(&[("q", LEXICAL_Q)])
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200, "the search view renders");
    let html = page.text().await.unwrap();
    assert!(
        html.contains("Rust Async Basics"),
        "the lexical view lists the hit: {html}"
    );
    let page = cl
        .get(format!("{base}/search"))
        .header("cookie", &cookie)
        .query(&[("q", LEXICAL_Q), ("mode", "hybrid")])
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 400, "unknown mode on the view is rejected");
}

/// VectorOnly is a TRUTHFUL diagnostic: the concept appears when its
/// vector matches; a query with no matching vector returns EMPTY — no
/// lexical fallback, ever. Scores carry the plain cosine (the vector
/// truth, not an RRF weighting).
#[tokio::test]
async fn vector_only_mode_is_a_truthful_diagnostic() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    let (emb_url, _paraphrase_embeds) = hybrid_mock(false).await;
    let (base, _shutdown, _dir, state) = boot().await;
    let cl = client();
    let (cookie, _csrf) = login_admin(&cl, &base).await;
    seed_concept_and_vector(&state, &emb_url).await;
    write_embeddings_row(&state, &emb_url, MODEL).await;

    let (status, body) = api_search(&cl, &base, &cookie, PARAPHRASE_Q, Some("vector_only")).await;
    assert_eq!(status, 200);
    let results = body.as_array().expect("search returns an array");
    assert_eq!(results.len(), 1, "the vector source's list: {body}");
    assert_eq!(results[0]["concept_path"], FIXTURE_PATH);
    assert_eq!(results[0]["scope"], "user");
    // The score IS the cosine (cos([0.9,0.44,0],[1,0,0]) ≈ 0.898), not
    // an RRF-fused rank weight.
    let score = results[0]["score"].as_f64().unwrap_or(f64::NAN);
    assert!(
        (score - 0.9 / (0.9f64 * 0.9 + 0.44 * 0.44).sqrt()).abs() < 1e-3,
        "vector-only score is the raw cosine: {score}"
    );

    // No matching vector → EMPTY even though the lexical index WOULD
    // have answered the same question — the truthful no-fallback pin
    // (LEXICAL_Q lexically matches the fixture; its mock vector is
    // perpendicular, so only a fallback could resurrect it).
    let (status, body) = api_search(&cl, &base, &cookie, LEXICAL_Q, Some("vector_only")).await;
    assert_eq!(status, 200);
    assert_eq!(
        body.as_array().expect("search returns an array").len(),
        0,
        "no lexical fallback in vector_only mode: {body}"
    );
    // ...and a query that matches neither source stays empty too.
    let (status, body) = api_search(&cl, &base, &cookie, ORTHOGONAL_Q, Some("vector_only")).await;
    assert_eq!(status, 200);
    assert_eq!(
        body.as_array().expect("search returns an array").len(),
        0,
        "a perpendicular query stays empty: {body}"
    );
}

/// Red/broken embeddings degrade to lexical-only without an error:
/// a 503 mock, Auto mode, HTTP 200, the lexical hit intact, the vector
/// source contributing nothing — the response indistinguishable from
/// explicit Lexical.
#[tokio::test]
async fn red_embeddings_degrade_to_lexical_only_without_an_error() {
    let _lock = TEST_LOCK.lock().await;
    let _health = EmbDefaults;
    EMBEDDING_HEALTH.reset_for_test();
    let (emb_url, paraphrase_embeds) = hybrid_mock(true).await;
    let (base, _shutdown, _dir, state) = boot().await;
    let cl = client();
    let (cookie, _csrf) = login_admin(&cl, &base).await;

    // The concept AND a vector row exist (a hand-upserted doc vector —
    // the dead mock cannot serve one): if the composer ignored the
    // failed embed it would find the paraphrase anyway, so the empty
    // paraphrase results below pin "no vector contribution" for real.
    let cs = concept_store(&state).await;
    let concept = mycelium_core::Concept::parse(FIXTURE_PATH, FIXTURE_MARKDOWN).unwrap();
    cs.put(&concept).await.unwrap();
    cs.vectors()
        .upsert(FIXTURE_PATH, MODEL, 3, &[1.0, 0.0, 0.0])
        .await
        .unwrap();
    write_embeddings_row(&state, &emb_url, MODEL).await;

    // Drive the health machine RED the honest way — three consecutive
    // failed embed attempts through real Auto searches (1-2 = Yellow,
    // >=3 = Red). Every one of them must answer HTTP 200 with the
    // lexical truth — never a request error, in ANY weather.
    for _ in 0..3 {
        let (status, auto_paraphrase) = api_search(&cl, &base, &cookie, PARAPHRASE_Q, None).await;
        assert_eq!(status, 200, "degradation is never an error");
        assert_eq!(
            auto_paraphrase
                .as_array()
                .expect("search returns an array")
                .len(),
            0,
            "a failed qvec contributes no vector hits: {auto_paraphrase}"
        );
    }
    // The three Auto searches attempted the embed while the machine was
    // still Yellow (Yellow proceeds — the Auto gate is Green|Yellow),
    // and the failures climbed it all the way to Red.
    assert!(
        matches!(
            EMBEDDING_HEALTH.state(),
            mycelium_librarian::llm_health::BackendState::Red
        ),
        "three consecutive failures must park the state machine on Red"
    );
    // The never-serving mock never counted a paraphrase embed: the
    // empty results above REALLY are "no vector contribution".
    assert_eq!(paraphrase_embeds.load(Ordering::Relaxed), 0);

    // Red: Auto stays lexical (HTTP 200 with the lexical hit) and is
    // byte-identical to explicit Lexical — indistinguishable, as
    // §3.3 requires. The vector row exists, yet Auto stops attempting
    // (no more embed traffic on a Red backend).
    let (auto_status, auto_lexical) = api_search(&cl, &base, &cookie, LEXICAL_Q, None).await;
    assert_eq!(auto_status, 200, "degradation is never an error");
    let results = auto_lexical.as_array().expect("search returns an array");
    assert_eq!(results.len(), 1, "the lexical hit survives: {auto_lexical}");
    assert_eq!(results[0]["concept_path"], FIXTURE_PATH);
    assert_eq!(results[0]["scope"], "user");
    let fourth = api_search(&cl, &base, &cookie, PARAPHRASE_Q, None).await;
    assert_eq!(fourth.0, 200);
    assert_eq!(
        fourth.1.as_array().unwrap().len(),
        0,
        "Red stays lexical: the paraphrase stays empty: {fourth:?}"
    );
    assert_eq!(
        paraphrase_embeds.load(Ordering::Relaxed),
        0,
        "Red: no embed"
    );

    // Byte-identity with explicit Lexical: same store, same queries.
    let (lex_status, lexical_green) =
        api_search(&cl, &base, &cookie, LEXICAL_Q, Some("lexical")).await;
    assert_eq!(lex_status, 200);
    assert_eq!(lexical_green, auto_lexical, "auto-degrade == lexical");
    let (lex_status, lexical_paraphrase) =
        api_search(&cl, &base, &cookie, PARAPHRASE_Q, Some("lexical")).await;
    assert_eq!(lex_status, 200);
    let (fourth_lexical_status, fourth_lexical) =
        api_search(&cl, &base, &cookie, PARAPHRASE_Q, Some("lexical")).await;
    assert_eq!(fourth_lexical_status, 200);
    assert_eq!(lexical_paraphrase, fourth_lexical);
    assert_eq!(lexical_paraphrase, fourth.1);
}
