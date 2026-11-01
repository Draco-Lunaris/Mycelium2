//! OpenAI-compatible embeddings client (memory-lifecycle step 4's
//! hybrid-retrieval vector source).
//!
//! Talks to any endpoint implementing `POST {base_url}/v1/embeddings`
//! with the OpenAI request/response wire shape (`{"model", "input":
//! [texts]}` → `{"data": [{"index", "embedding": [f32]}]}`). A batch's
//! vectors are reordered by the response's `data[i].index` — the
//! authoritative mapping back to input positions; a gap, duplicate,
//! out-of-range index, non-finite value or empty vector is a
//! `Malformed` batch error, never a silent shift.
//!
//! The admin config row lives in ConfigStore under `"embeddings"`
//! (sealed typed JSON KV, cloned from the `llm` row's idiom end to end:
//! serde defaults, clamp on save AND on load, `Debug` redacts the key).
//! Note `base_url` is the server ROOT without `/v1` — the URL contract
//! is `{base_url}/v1/embeddings`.
//!
//! Passive health hooks ride every successful/failed
//! [`EmbeddingsClient::embed_batch`] call and feed ONLY the embeddings
//! machine — [`crate::llm_health::EMBEDDING_HEALTH`] with
//! [`crate::llm_health::EMB_LAST_SUCCESS`] and
//! [`crate::llm_health::EMBEDDING_DIMS`]. The chat-LLM machine
//! ([`crate::llm_health::LLM_HEALTH`]) is never touched from here.
//! `probe_once` (the background probe worker's one active probe)
//! deliberately does NOT go through the client: probe traffic must
//! never double-feed the passive hooks (`llm_health::probe_once`
//! precedent) — it reports to its caller, and the caller stamps the
//! result.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Redacted `Debug` so a future `debug!(cfg = ?cfg, ...)` site can't leak
/// the key. (Not derived: derive(Debug) would print api_key verbatim.)
/// A verbatim copy of the `llm.rs` macro — macros are textually scoped,
/// so each module carries its own copy.
macro_rules! debug_redacted {
    ($name:ident { $($field:ident),+ $(,)? }, key: $key:ident) => {
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name))
                    $(.field(stringify!($field), &self.$field))+
                    .field(stringify!($key), &self.$key.as_ref().map(|_| "***"))
                    .finish()
            }
        }
    };
}

use std::sync::atomic::Ordering;

/// Admin-managed embeddings backend config (stored in ConfigStore under
/// `"embeddings"`). Ollama default; any OpenAI-compatible endpoint
/// implementing `POST {base_url}/v1/embeddings` works. Note `base_url`
/// is the server ROOT — bare `http://host:11434`, no `/v1` suffix (the
/// client appends the path).
#[derive(Clone, Serialize, Deserialize)]
pub struct EmbeddingsConfig {
    pub base_url: String,
    pub model: String,
    /// Optional bearer token for endpoints that require auth (an
    /// API-key gateway). Sealed at rest by the admin surface; sent as
    /// `Authorization: Bearer <api_key>` when present, no header when
    /// absent (keyless endpoints keep working).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Per-request timeout, in seconds. 5–300.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u32,
    /// Batch size for the retrieval indexer (texts per POST). 8–128.
    #[serde(default = "default_batch")]
    pub batch: u32,
}

debug_redacted!(
    EmbeddingsConfig { base_url, model, timeout_secs, batch },
    key: api_key
);

/// Accepted range for [`EmbeddingsConfig::timeout_secs`].
pub const TIMEOUT_SECS_RANGE: (u32, u32) = (5, 300);
/// Accepted range for [`EmbeddingsConfig::batch`].
pub const BATCH_RANGE: (u32, u32) = (8, 128);
/// Per-input cap, in chars (24,000 ≈ 6,000 tokens): one oversized
/// concept must not dominate a batch's wire body. `embed_batch`
/// truncates every input to this char count before the POST.
pub const EMBED_INPUT_MAX_CHARS: usize = 24_000;
/// Sanity cap on a model's vector dimensions. A larger `dims` is a
/// config-level mistake (embedding a text column against the wrong
/// backend); [`probe_once`] rejects it by name rather than letting a
/// Task-3/-6 index build on useless vectors.
const MAX_EMBEDDING_DIMS: usize = 4096;

fn default_timeout_secs() -> u32 {
    30
}
fn default_batch() -> u32 {
    32
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:11434".into(),
            model: "default".into(),
            api_key: None,
            timeout_secs: default_timeout_secs(),
            batch: default_batch(),
        }
    }
}

impl EmbeddingsConfig {
    /// A copy with the tunables forced into their documented ranges.
    /// Applied on save *and* on load: a row stored by an older (or
    /// hand-edited) build can never carry an unworkable value. Clamping
    /// is idempotent — `cfg.clamp().clamp()` is field-equal to
    /// `cfg.clamp()`.
    #[must_use]
    pub fn clamp(&self) -> EmbeddingsConfig {
        EmbeddingsConfig {
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            api_key: self.api_key.clone(),
            timeout_secs: self
                .timeout_secs
                .clamp(TIMEOUT_SECS_RANGE.0, TIMEOUT_SECS_RANGE.1),
            batch: self.batch.clamp(BATCH_RANGE.0, BATCH_RANGE.1),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("embedding request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("embeddings endpoint returned status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("embedding response malformed: {0}")]
    Malformed(String),
}

impl EmbeddingError {
    /// Backend-down classification (outage-recovery spec §3.2 / D1,
    /// mirroring [`crate::llm::LlmError::is_backend_down`]): this error
    /// means the request **never reached a working embeddings model**,
    /// so it says something about backend health, not about the run. A
    /// transport failure never arrived; 5xx is the backend or its
    /// upstream; 401/403 is misconfigured credentials (operator-
    /// actionable, treated as down, not as weather). Everything else —
    /// 4xx other than 401/403, a body that fails to parse or validate —
    /// is a real run outcome from a backend that was reachable.
    #[must_use]
    pub fn is_backend_down(&self) -> bool {
        match self {
            EmbeddingError::Request(_) => true,
            EmbeddingError::Status { status, .. } => {
                *status >= 500 || *status == 401 || *status == 403
            }
            EmbeddingError::Malformed(_) => false,
        }
    }
}

/// Passive success hook (the embeddings instance of llm.rs's
/// `record_llm_success`): stamps `EMB_LAST_SUCCESS` (unix secs), the
/// served dimension count (`EMBEDDING_DIMS` — the first vector's
/// length; skipped when a batch somehow carries none, so a stale dims
/// readout is never clobbered to "unknown"), and notes ok on
/// [`crate::llm_health::EMBEDDING_HEALTH`]. Called at the single
/// success choke inside `embed_batch`.
fn record_emb_success(vectors: &[Vec<f32>]) {
    let unix = chrono::Utc::now().timestamp().max(0) as u64;
    crate::llm_health::EMB_LAST_SUCCESS.store(unix, Ordering::Relaxed);
    if let Some(dims) = vectors.first().and_then(|v| u32::try_from(v.len()).ok()) {
        crate::llm_health::EMBEDDING_DIMS.store(dims, Ordering::Relaxed);
    }
    // A real embedding is the strongest healthy observation there is,
    // so the machine flips Green without waiting for a probe.
    crate::llm_health::EMBEDDING_HEALTH.note_ok();
}

/// Passive failure hook (the embeddings mirror of llm.rs's
/// `record_outcome`): only the backend-down classification
/// ([`EmbeddingError::is_backend_down`]) feeds the health machine — a
/// 400 from a reachable backend is not backend health. Ok results note
/// nothing: `record_emb_success` already noted them at the choke point.
fn record_outcome<T>(result: &Result<T, EmbeddingError>) {
    if let Err(e) = result
        && e.is_backend_down()
    {
        crate::llm_health::EMBEDDING_HEALTH.note_failure();
    }
}

/// Request body for `POST {base_url}/v1/embeddings`.
#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

/// Response shape (only the fields we need). `data[i].index` is the
/// authoritative mapping back to input positions.
#[derive(Deserialize)]
struct EmbedResponse {
    #[serde(default)]
    data: Vec<EmbedData>,
}

#[derive(Deserialize)]
struct EmbedData {
    index: usize,
    embedding: Vec<f32>,
}

/// Slot the response's `data` entries by their `index` and return one
/// vector per input position, in the INPUT order. Any duplicate index,
/// out-of-range index, missing slot, empty vector or non-finite value
/// is `Malformed` — the reorder can never silently shift positions.
fn ordered_vectors(data: Vec<EmbedData>, expected: usize) -> Result<Vec<Vec<f32>>, EmbeddingError> {
    let mut slots: Vec<Option<Vec<f32>>> = (0..expected).map(|_| None).collect();
    for entry in data {
        let idx = entry.index;
        if idx >= expected {
            return Err(EmbeddingError::Malformed(format!(
                "embedding index {idx} is out of range for {expected} input(s)"
            )));
        }
        if slots[idx].is_some() {
            return Err(EmbeddingError::Malformed(format!(
                "duplicate embedding index {idx} in the response"
            )));
        }
        slots[idx] = Some(entry.embedding);
    }
    slots
        .into_iter()
        .enumerate()
        .map(|(i, slot)| {
            let vector = slot.ok_or_else(|| {
                EmbeddingError::Malformed(format!(
                    "the embeddings response is missing index {i} (incomplete batch)"
                ))
            })?;
            if vector.is_empty() {
                return Err(EmbeddingError::Malformed(format!(
                    "the embeddings response carries an empty vector at index {i}"
                )));
            }
            if vector.iter().any(|v| !v.is_finite()) {
                return Err(EmbeddingError::Malformed(format!(
                    "the embeddings response carries a non-finite value at index {i}"
                )));
            }
            Ok(vector)
        })
        .collect()
}

/// A client for one OpenAI-compatible embeddings endpoint.
#[derive(Clone)]
pub struct EmbeddingsClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    /// Bearer token attached to every request when configured.
    api_key: Option<String>,
}

debug_redacted!(EmbeddingsClient { http, base_url, model }, key: api_key);

impl EmbeddingsClient {
    pub fn new(config: &EmbeddingsConfig) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(u64::from(config.timeout_secs)))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
            base_url: config.base_url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            api_key: config.api_key.clone(),
        }
    }

    /// The embed request with auth (Bearer when a key is configured,
    /// bare otherwise) — the single POST site every caller goes through.
    async fn post_embeddings(
        &self,
        url: &str,
        body: &EmbedRequest<'_>,
    ) -> Result<reqwest::Response, EmbeddingError> {
        let mut request = self.http.post(url);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        Ok(request.json(body).send().await?)
    }

    /// Embed a batch of texts; one vector per input element, in the
    /// input order. Every input is truncated to
    /// [`EMBED_INPUT_MAX_CHARS`] chars first; an empty batch is an
    /// early `Ok(vec![])` (nothing is POSTed and nothing is observed —
    /// the backend saw no traffic to report on).
    pub async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let result = self.embed_batch_impl(texts).await;
        record_outcome(&result);
        result
    }

    async fn embed_batch_impl(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let input: Vec<String> = texts
            .iter()
            .map(|t| t.chars().take(EMBED_INPUT_MAX_CHARS).collect())
            .collect();
        let url = format!("{}/v1/embeddings", self.base_url);
        let body = EmbedRequest {
            model: &self.model,
            input: &input,
        };
        let response = self.post_embeddings(&url, &body).await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(EmbeddingError::Status {
                status: status.as_u16(),
                body: text.chars().take(500).collect(),
            });
        }
        let bytes = response.bytes().await?;
        // Decode via serde_json (not reqwest's `json()`): every parse
        // failure must land in `Malformed` — a body that fails to parse
        // is a reachable backend answering badly, never backend-down.
        let parsed: EmbedResponse = serde_json::from_slice(&bytes)
            .map_err(|e| EmbeddingError::Malformed(format!("invalid embeddings JSON body: {e}")))?;
        let vectors = ordered_vectors(parsed.data, texts.len())?;
        record_emb_success(&vectors);
        Ok(vectors)
    }
}

/// One active probe of the embeddings backend: a one-element
/// `["probe"]` input POSTed to `{base_url}/v1/embeddings` with a
/// [`crate::llm_health::PROBE_TIMEOUT_SECS`] timeout and the configured
/// bearer header. Healthy = a well-formed single-vector response; its
/// dimension count is returned for the caller to stamp
/// ([`crate::llm_health::EMBEDDING_DIMS`]) or reject.
///
/// Deliberately raw `reqwest` (not [`EmbeddingsClient`]) so probe
/// traffic never double-counts through the passive hooks, and **this
/// function itself never calls `note_*`** — it reports to its caller,
/// and the caller feeds the result exactly once
/// (`llm_health::probe_once` precedent).
///
/// # Errors
///
/// A transport failure or a ≥500/401/403 status comes back in that
/// class; a body that fails to parse or validate, or a dimension count
/// above the 4096 sanity cap, is `Malformed`.
#[must_use = "the probe's verdict is the caller's health evidence — \
dropping a passed probe would leave the machine un-stamped"]
pub async fn probe_once(cfg: &EmbeddingsConfig) -> Result<usize, EmbeddingError> {
    let url = format!("{}/v1/embeddings", cfg.base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(crate::llm_health::PROBE_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(10))
        .build()?;
    let input = ["probe".to_string()];
    let body = EmbedRequest {
        model: &cfg.model,
        input: &input,
    };
    let mut request = client.post(&url);
    if let Some(key) = &cfg.api_key {
        request = request.bearer_auth(key);
    }
    let response = request.json(&body).send().await?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(EmbeddingError::Status {
            status: status.as_u16(),
            body: text.chars().take(500).collect(),
        });
    }
    let bytes = response.bytes().await?;
    // Same decode discipline as `embed_batch`: parse failures are
    // `Malformed`, never backend-down.
    let parsed: EmbedResponse = serde_json::from_slice(&bytes)
        .map_err(|e| EmbeddingError::Malformed(format!("invalid embeddings JSON body: {e}")))?;
    let vectors = ordered_vectors(parsed.data, 1)?;
    let dims = vectors.into_iter().next().map_or(0, |v| v.len());
    if dims > MAX_EMBEDDING_DIMS {
        return Err(EmbeddingError::Malformed(format!(
            "model dims {dims} exceeds the {MAX_EMBEDDING_DIMS} cap — config-level problem"
        )));
    }
    Ok(dims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use crate::llm_health::{EMB_LAST_SUCCESS, EMBEDDING_DIMS};

    use axum::response::IntoResponse;

    type AuthCapture = Arc<tokio::sync::Mutex<Option<String>>>;
    type BodyCapture = Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>;

    /// Restores every embeddings process-global a driven call can touch —
    /// the in-crate twin of the integration binaries' `EmbDefaults`.
    /// The passive hooks stamp `EMB_LAST_SUCCESS`/`EMBEDDING_DIMS` and
    /// note into `EMBEDDING_HEALTH`; the crate's other tests (Task 1's
    /// state-machine pins, including its initial-value zeros) must not
    /// observe leftovers. Dropped while `HEALTH_TEST_LOCK` is still
    /// held (declared after the guard).
    struct EmbGlobals;

    impl Drop for EmbGlobals {
        fn drop(&mut self) {
            reset_emb_globals();
        }
    }

    /// Fresh embeddings everything: Green machine, 0 = never observed
    /// on both sentinels.
    fn reset_emb_globals() {
        crate::llm_health::EMBEDDING_HEALTH.reset_for_test();
        EMB_LAST_SUCCESS.store(0, Ordering::Relaxed);
        EMBEDDING_DIMS.store(0, Ordering::Relaxed);
    }

    /// An axum mock of the embeddings endpoint: every POST to
    /// /v1/embeddings answers `status` + `body`; the request's
    /// Authorization header and its parsed JSON body are captured.
    async fn embeddings_mock(status: u16, body: String) -> (String, AuthCapture, BodyCapture) {
        let captured: AuthCapture = Arc::new(tokio::sync::Mutex::new(None));
        let bodies: BodyCapture = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let handler_state = Arc::clone(&captured);
        let bodies_state = Arc::clone(&bodies);
        let app = axum::Router::new().route(
            "/v1/embeddings",
            axum::routing::post(
                move |headers: axum::http::HeaderMap,
                      axum::Json(payload): axum::Json<serde_json::Value>| {
                    let captured = Arc::clone(&handler_state);
                    let bodies = Arc::clone(&bodies_state);
                    async move {
                        let auth = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok().map(str::to_string));
                        *captured.lock().await = auth;
                        bodies.lock().await.push(payload);
                        (
                            axum::http::StatusCode::from_u16(status).unwrap(),
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            body,
                        )
                            .into_response()
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // The CONFIG row's shape: server root without /v1 — the client
        // appends /v1/embeddings.
        (format!("http://{addr}"), captured, bodies)
    }

    fn cfg_for(url: String, api_key: Option<&str>) -> EmbeddingsConfig {
        EmbeddingsConfig {
            base_url: url,
            model: "m".into(),
            api_key: api_key.map(str::to_string),
            ..Default::default()
        }
    }

    /// One one-dimension-vector response with the given (index, value)
    /// entries, pre-shuffled by the caller when order matters.
    fn embeddings_json(entries: &[(usize, f64)]) -> String {
        let data: Vec<serde_json::Value> = entries
            .iter()
            .map(|&(i, v)| serde_json::json!({ "index": i, "embedding": [v] }))
            .collect();
        serde_json::json!({ "data": data }).to_string()
    }

    /// A single-input response carrying one `dims`-dimension vector.
    fn dims_json(dims: usize) -> String {
        serde_json::json!({ "data": [{ "index": 0, "embedding": vec![0.25f64; dims] }] })
            .to_string()
    }

    /// Batch order rides the response's `index` — a pre-shuffled
    /// `[2, 0, 1]` data array must land each vector on its own input
    /// position (input order, not response order).
    #[tokio::test]
    async fn embed_batch_preserves_input_order_by_index() {
        let _guard = crate::llm_health::health_test_guard().await;
        let _globals = EmbGlobals;
        let body = embeddings_json(&[(2, 12.5), (0, 10.5), (1, 11.5)]);
        let (url, _auth, _bodies) = embeddings_mock(200, body).await;
        let client = EmbeddingsClient::new(&cfg_for(url, None));
        let out = client
            .embed_batch(&["a".into(), "b".into(), "c".into()])
            .await
            .expect("a complete response parses");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], vec![10.5], "input[0] gets the index-0 vector");
        assert_eq!(out[1], vec![11.5], "input[1] gets the index-1 vector");
        assert_eq!(out[2], vec![12.5], "input[2] gets the index-2 vector");
    }

    /// A missing middle index is a batch-level Malformed error — the
    /// reorder can never silently shift positions (the spec's gap rule).
    #[tokio::test]
    async fn embed_batch_rejects_an_index_gap() {
        let _guard = crate::llm_health::health_test_guard().await;
        let _globals = EmbGlobals;
        let body = embeddings_json(&[(0, 1.0), (2, 3.0)]);
        let (url, _auth, _bodies) = embeddings_mock(200, body).await;
        let client = EmbeddingsClient::new(&cfg_for(url, None));
        let err = client
            .embed_batch(&["a".into(), "b".into(), "c".into()])
            .await
            .unwrap_err();
        assert!(matches!(err, EmbeddingError::Malformed(_)), "{err}");
        assert!(
            format!("{err}").contains("missing index 1"),
            "the gap names the absent position: {err}"
        );
    }

    /// An out-of-range index (the backend answering a batch it did not
    /// receive) is malformed — never a panic.
    #[tokio::test]
    async fn embed_batch_rejects_an_out_of_range_index() {
        let _guard = crate::llm_health::health_test_guard().await;
        let _globals = EmbGlobals;
        let body = embeddings_json(&[(0, 1.0), (7, 70.0)]);
        let (url, _auth, _bodies) = embeddings_mock(200, body).await;
        let client = EmbeddingsClient::new(&cfg_for(url, None));
        let err = client
            .embed_batch(&["a".into(), "b".into()])
            .await
            .unwrap_err();
        assert!(matches!(err, EmbeddingError::Malformed(_)), "{err}");
        assert!(format!("{err}").contains("out of range"), "{err}");
    }

    /// A duplicate index is malformed (two vectors for one input slot).
    #[tokio::test]
    async fn embed_batch_rejects_a_duplicate_index() {
        let _guard = crate::llm_health::health_test_guard().await;
        let _globals = EmbGlobals;
        let body = embeddings_json(&[(0, 1.0), (0, 2.0)]);
        let (url, _auth, _bodies) = embeddings_mock(200, body).await;
        let client = EmbeddingsClient::new(&cfg_for(url, None));
        let err = client.embed_batch(&["a".into()]).await.unwrap_err();
        assert!(
            matches!(&err, EmbeddingError::Malformed(m) if m.contains("duplicate")),
            "{err}"
        );
    }

    /// A zero-dimension "vector" is malformed: it can never be ranked
    /// or stored, and stamping DIMS 0 would claim "unknown".
    #[tokio::test]
    async fn embed_batch_rejects_an_empty_vector() {
        let _guard = crate::llm_health::health_test_guard().await;
        let _globals = EmbGlobals;
        let body = serde_json::json!({ "data": [{ "index": 0, "embedding": [] }] }).to_string();
        let (url, _auth, _bodies) = embeddings_mock(200, body).await;
        let client = EmbeddingsClient::new(&cfg_for(url, None));
        let err = client.embed_batch(&["a".into()]).await.unwrap_err();
        assert!(matches!(err, EmbeddingError::Malformed(_)), "{err}");
    }

    /// A non-finite embedding value (or any unparsable payload) is
    /// malformed, and never a backend-down classification.
    #[tokio::test]
    async fn embed_batch_rejects_a_non_finite_value() {
        let _guard = crate::llm_health::health_test_guard().await;
        let _globals = EmbGlobals;
        // 1e400: float overflow — parsed as inf (or rejected by the
        // parser; both roads are the Malformed class).
        let body = r#"{"data":[{"index":0,"embedding":[1e400]}]}"#.to_string();
        let (url, _auth, _bodies) = embeddings_mock(200, body).await;
        let client = EmbeddingsClient::new(&cfg_for(url, None));
        let err = client.embed_batch(&["a".into()]).await.unwrap_err();
        assert!(matches!(err, EmbeddingError::Malformed(_)), "{err}");
        assert!(!err.is_backend_down());
    }

    /// The Bearer header rides the request only when a key is
    /// configured (auth-echo mock, llm.rs:851 style).
    #[tokio::test]
    async fn embed_batch_sends_bearer_only_when_key_configured() {
        let _guard = crate::llm_health::health_test_guard().await;
        let _globals = EmbGlobals;
        let body = embeddings_json(&[(0, 1.0)]);
        let (url, auth, _bodies) = embeddings_mock(200, body.clone()).await;
        let keyed = EmbeddingsClient::new(&cfg_for(url.clone(), Some("k")));
        keyed.embed_batch(&["t".into()]).await.unwrap();
        assert_eq!(auth.lock().await.as_deref(), Some("Bearer k"));

        let (url, auth, _bodies) = embeddings_mock(200, body).await;
        let keyless = EmbeddingsClient::new(&cfg_for(url, None));
        keyless.embed_batch(&["t".into()]).await.unwrap();
        assert_eq!(
            auth.lock().await.as_deref(),
            None,
            "no Authorization header for a keyless endpoint"
        );
    }

    /// The per-input cap: a 30k-char input is truncated to
    /// `EMBED_INPUT_MAX_CHARS` before the POST.
    #[tokio::test]
    async fn embed_batch_truncates_input_at_the_char_cap() {
        let _guard = crate::llm_health::health_test_guard().await;
        let _globals = EmbGlobals;
        let (url, _auth, bodies) = embeddings_mock(200, dims_json(2)).await;
        let client = EmbeddingsClient::new(&cfg_for(url, None));
        let long = "a".repeat(30_000);
        let out = client.embed_batch(&[long]).await.unwrap();
        assert_eq!(out.len(), 1, "the truncated input still comes back");
        let sent = bodies.lock().await;
        let input = sent[0]["input"].as_array().expect("array input");
        assert_eq!(input.len(), 1);
        assert_eq!(
            input[0].as_str().unwrap().chars().count(),
            EMBED_INPUT_MAX_CHARS,
            "the wire body carries exactly the capped text"
        );
    }

    /// D1 classifier pin (mirrors llm.rs's backend_down_class_pins):
    /// transport failures + 5xx/401/403 are backend-down; 4xx real
    /// outcomes and malformed bodies are not.
    #[tokio::test]
    async fn backend_down_class_pins() {
        let reqwest_err = reqwest::Client::new()
            .get("http://127.0.0.1:1/v1/embeddings")
            .send()
            .await
            .unwrap_err();
        assert!(
            EmbeddingError::from(reqwest_err).is_backend_down(),
            "a transport failure is backend-down"
        );
        let status = |code: u16| EmbeddingError::Status {
            status: code,
            body: String::new(),
        };
        for code in [500u16, 502, 503, 504, 401, 403] {
            assert!(status(code).is_backend_down(), "{code} is backend-down");
        }
        for code in [404u16, 400, 429] {
            assert!(
                !status(code).is_backend_down(),
                "{code} is a real run outcome, not backend health"
            );
        }
        assert!(
            !EmbeddingError::Malformed("x".into()).is_backend_down(),
            "a malformed body is not backend weather"
        );
    }

    /// The raw probe: `Ok(dims)` on a normal model, `Malformed` past the
    /// 4096 cap — and probe_once itself never feeds the machine (the
    /// caller stamps the result; llm_health.rs:336 precedent).
    #[tokio::test]
    async fn probe_once_returns_dims_and_rejects_over_4096() {
        let _guard = crate::llm_health::health_test_guard().await;
        let _globals = EmbGlobals;

        let (url, _auth, _bodies) = embeddings_mock(200, dims_json(768)).await;
        let dims = probe_once(&cfg_for(url.clone(), None))
            .await
            .expect("a normal probe returns dims");
        assert_eq!(dims, 768);

        // A trailing slash on the row's root must not double-slash into
        // //v1/embeddings (the config's own URL contract).
        let dims = probe_once(&cfg_for(format!("{url}/"), None))
            .await
            .expect("a trailing slash is trimmed");
        assert_eq!(dims, 768);

        let (url, _auth, _bodies) = embeddings_mock(200, dims_json(8192)).await;
        let err = probe_once(&cfg_for(url, None)).await.unwrap_err();
        assert!(matches!(err, EmbeddingError::Malformed(_)), "{err}");
        assert!(
            format!("{err}").contains("4096"),
            "the cap names itself in the error: {err}"
        );

        // The probe fed nothing — stamping is the caller's job.
        let s = crate::llm_health::embedding_snapshot();
        assert_eq!(s.consecutive_failures, 0, "probe_once must not call note_*");
        assert_eq!(s.checked_at, None, "probe_once must not stamp clocks");
        assert_eq!(
            EMB_LAST_SUCCESS.load(Ordering::Relaxed),
            0,
            "probe_once must not stamp the sentinel"
        );
    }

    /// Instance isolation end-to-end: one successful embed stamps the
    /// embeddings machine (Green + fresh + sentinels) and leaves the
    /// chat machine's registry and `LLM_LAST_SUCCESS` untouched —
    /// closing the `record_outcome`/`record_emb_success` wiring.
    #[tokio::test]
    async fn a_healthy_embed_success_stamps_only_the_embeddings_machine() {
        let _guard = crate::llm_health::health_test_guard().await;
        let _globals = EmbGlobals;
        reset_emb_globals();
        let llm_before = crate::llm_health::LLM_HEALTH.snapshot();
        let llm_stamp_before = crate::llm::LLM_LAST_SUCCESS.load(Ordering::Relaxed);

        let (url, _auth, _bodies) = embeddings_mock(200, dims_json(1024)).await;
        let client = EmbeddingsClient::new(&cfg_for(url, None));
        client.embed_batch(&["t".into()]).await.unwrap();

        let emb = crate::llm_health::embedding_snapshot();
        assert_eq!(emb.state, crate::llm_health::BackendState::Green);
        assert_eq!(emb.consecutive_failures, 0);
        assert!(emb.checked_at.is_some(), "the success was observed");
        assert!(emb.last_observation.is_some(), "ok stamps the observation");
        let stamp = EMB_LAST_SUCCESS.load(Ordering::Relaxed);
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        assert!(
            stamp >= now.saturating_sub(2) && stamp <= now + 2,
            "the sentinel was stamped with a unix-seconds tick (got {stamp}, now {now})"
        );
        assert_eq!(
            EMBEDDING_DIMS.load(Ordering::Relaxed),
            1024,
            "the served dimension count is stamped"
        );

        // The chat machine's registry + real-completion clock never move.
        let llm_after = crate::llm_health::LLM_HEALTH.snapshot();
        assert_eq!(llm_after, llm_before, "LLM_HEALTH untouched");
        assert_eq!(
            crate::llm::LLM_LAST_SUCCESS.load(Ordering::Relaxed),
            llm_stamp_before,
            "LLM_LAST_SUCCESS untouched by an embedding success"
        );
    }

    // ---- config row shapes (the `llm` row's pins, mirrored) ----

    #[test]
    fn defaults_parse_when_absent() {
        // A row written before `timeout_secs`/`batch` existed: every
        // new field must fall back to its documented default.
        let cfg: EmbeddingsConfig =
            serde_json::from_str(r#"{"base_url": "http://b", "model": "nomic-embed-text"}"#)
                .unwrap();
        assert_eq!(cfg.timeout_secs, 30);
        assert_eq!(cfg.batch, 32);
        assert_eq!(cfg.api_key, None);

        let d = EmbeddingsConfig::default();
        assert_eq!(d.base_url, "http://localhost:11434");
        assert_eq!(d.model, "default");
        assert_eq!(d.api_key, None);
        assert_eq!(d.timeout_secs, 30);
        assert_eq!(d.batch, 32);
    }

    #[test]
    fn clamp_high_and_low() {
        let high = EmbeddingsConfig {
            timeout_secs: 10_000,
            batch: 999,
            ..Default::default()
        }
        .clamp();
        assert_eq!(high.timeout_secs, 300);
        assert_eq!(high.batch, 128);

        let low = EmbeddingsConfig {
            timeout_secs: 0,
            batch: 0,
            ..Default::default()
        }
        .clamp();
        assert_eq!(low.timeout_secs, 5);
        assert_eq!(low.batch, 8);

        // base_url/model/api_key pass through untouched.
        let cfg = EmbeddingsConfig {
            base_url: "http://b".into(),
            model: "m".into(),
            api_key: Some("k".into()),
            timeout_secs: 0,
            batch: 0,
        }
        .clamp();
        assert_eq!(cfg.base_url, "http://b");
        assert_eq!(cfg.model, "m");
        assert_eq!(cfg.api_key.as_deref(), Some("k"));
    }

    #[test]
    fn clamp_is_idempotent() {
        for cfg in [
            EmbeddingsConfig::default(),
            EmbeddingsConfig {
                timeout_secs: 10_000,
                batch: 5,
                ..Default::default()
            },
        ] {
            let once = cfg.clamp();
            let twice = once.clamp();
            assert_eq!(twice.timeout_secs, once.timeout_secs);
            assert_eq!(twice.batch, once.batch);
        }
    }

    #[test]
    fn roundtrip_serialize() {
        // Sealing is serialize + AEAD, so a serde roundtrip is what the
        // stored row goes through: every field must survive it.
        let cfg = EmbeddingsConfig {
            base_url: "http://b".into(),
            model: "m".into(),
            api_key: Some("k".into()),
            timeout_secs: 45,
            batch: 16,
        };
        let back: EmbeddingsConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.base_url, cfg.base_url);
        assert_eq!(back.model, cfg.model);
        assert_eq!(back.api_key, cfg.api_key);
        assert_eq!(back.timeout_secs, 45);
        assert_eq!(back.batch, 16);

        // skip_serializing_if keeps a keyless row's JSON free of the
        // api_key key (byte-shape parity with the `llm` row).
        let json = serde_json::to_string(&EmbeddingsConfig::default()).unwrap();
        assert!(!json.contains("api_key"), "{json}");
    }

    #[test]
    fn debug_impl_redacts_api_key() {
        // A future debug!-formatting site must never leak the key.
        let cfg = EmbeddingsConfig {
            base_url: "http://b".into(),
            model: "m".into(),
            api_key: Some("super-secret".into()),
            ..Default::default()
        };
        let rendered = format!("{cfg:?}");
        assert!(
            rendered.contains("***"),
            "key placeholder missing: {rendered}"
        );
        assert!(!rendered.contains("super-secret"), "key leaked: {rendered}");
        let client = EmbeddingsClient::new(&cfg);
        let rendered = format!("{client:?}");
        assert!(rendered.contains("***"));
        assert!(!rendered.contains("super-secret"));
    }
}
