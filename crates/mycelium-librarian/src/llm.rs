//! OpenAI-compatible LLM client (Ollama default per DESIGN).
//!
//! Talks to any endpoint implementing `POST {base_url}/chat/completions`
//! with the OpenAI request/response shape. Timeouts are enforced so a
//! hung backend can never wedge an ingest job.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
/// Redacted `Debug` so a future `debug!(cfg = ?cfg, ...)` site can't leak
/// the key. (Not derived: derive(Debug) would print api_key verbatim.)
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

/// Admin-managed LLM backend config (stored in ConfigStore under "llm").
/// Ollama default per DESIGN; any OpenAI-compatible endpoint works.
#[derive(Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub url: String,
    pub model: String,
    /// Optional bearer token for endpoints that require auth (an
    /// API-key gateway). Sealed at rest by the admin surface; sent as
    /// `Authorization: Bearer <api_key>` when present, no header when
    /// absent (keyless endpoints keep working).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Concurrent admitted LLM runs (the gate's slot count). 1–64.
    #[serde(default = "default_max_parallel")]
    pub max_parallel: u32,
    /// Admitted wait-class runs allowed per user. 1–16.
    #[serde(default = "default_per_user_active")]
    pub per_user_active: u32,
    /// Wait bound for MCP queries and stateless chat, in seconds. 1–600.
    #[serde(default = "default_query_wait_secs")]
    pub query_wait_secs: u32,
    /// Wait bound for chat turns, in seconds. 1–7200.
    #[serde(default = "default_chat_wait_secs")]
    pub chat_wait_secs: u32,
    /// Cap on total gate waiters; submissions beyond it are rejected by
    /// the surface. 1–1024.
    #[serde(default = "default_chat_queue_depth")]
    pub chat_queue_depth: u32,
}

debug_redacted!(
    LlmConfig {
        url,
        model,
        max_parallel,
        per_user_active,
        query_wait_secs,
        chat_wait_secs,
        chat_queue_depth,
    },
    key: api_key
);

/// Accepted range for [`LlmConfig::max_parallel`].
pub const MAX_PARALLEL_RANGE: (u32, u32) = (1, 64);
/// Accepted range for [`LlmConfig::per_user_active`].
pub const PER_USER_ACTIVE_RANGE: (u32, u32) = (1, 16);
/// Accepted range for [`LlmConfig::query_wait_secs`].
pub const QUERY_WAIT_SECS_RANGE: (u32, u32) = (1, 600);
/// Accepted range for [`LlmConfig::chat_wait_secs`].
pub const CHAT_WAIT_SECS_RANGE: (u32, u32) = (1, 7200);
/// Accepted range for [`LlmConfig::chat_queue_depth`].
pub const CHAT_QUEUE_DEPTH_RANGE: (u32, u32) = (1, 1024);

fn default_max_parallel() -> u32 {
    2
}
fn default_per_user_active() -> u32 {
    2
}
fn default_query_wait_secs() -> u32 {
    30
}
fn default_chat_wait_secs() -> u32 {
    600
}
fn default_chat_queue_depth() -> u32 {
    64
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:11434/v1".into(),
            model: "default".into(),
            api_key: None,
            max_parallel: default_max_parallel(),
            per_user_active: default_per_user_active(),
            query_wait_secs: default_query_wait_secs(),
            chat_wait_secs: default_chat_wait_secs(),
            chat_queue_depth: default_chat_queue_depth(),
        }
    }
}

impl LlmConfig {
    /// A copy with every concurrency field forced into its documented
    /// range. Applied on save *and* on load: a row stored by an older
    /// (or hand-edited) build can never open an unbounded gate. Clamping
    /// is idempotent — `cfg.clamp().clamp() == cfg.clamp()`.
    #[must_use]
    pub fn clamp(&self) -> LlmConfig {
        LlmConfig {
            url: self.url.clone(),
            model: self.model.clone(),
            api_key: self.api_key.clone(),
            max_parallel: self
                .max_parallel
                .clamp(MAX_PARALLEL_RANGE.0, MAX_PARALLEL_RANGE.1),
            per_user_active: self
                .per_user_active
                .clamp(PER_USER_ACTIVE_RANGE.0, PER_USER_ACTIVE_RANGE.1),
            query_wait_secs: self
                .query_wait_secs
                .clamp(QUERY_WAIT_SECS_RANGE.0, QUERY_WAIT_SECS_RANGE.1),
            chat_wait_secs: self
                .chat_wait_secs
                .clamp(CHAT_WAIT_SECS_RANGE.0, CHAT_WAIT_SECS_RANGE.1),
            chat_queue_depth: self
                .chat_queue_depth
                .clamp(CHAT_QUEUE_DEPTH_RANGE.0, CHAT_QUEUE_DEPTH_RANGE.1),
        }
    }

    /// Push the clamped limits onto `gate`. `query_wait_secs` and
    /// `chat_wait_secs` are deliberately not applied: they are the
    /// calling surface's wait bounds, not gate state.
    pub fn apply_to_gate(&self, gate: &crate::llm_gate::LlmGate) {
        let cfg = self.clamp();
        // Order matters: `set_capacity` is the only setter that runs a
        // dispatch, so it goes LAST — a live reconfig that raises
        // capacity while lowering `per_user_active` must admit its
        // burst under the NEW per-user cap, not the stale one.
        gate.set_per_user_active(cfg.per_user_active as usize);
        gate.set_chat_queue_depth(cfg.chat_queue_depth as usize);
        gate.set_capacity(cfg.max_parallel as usize);
    }

    /// [`LlmConfig::apply_to_gate`] against the process-global gate, so
    /// a stored config takes effect without an admin re-save.
    pub fn sync_gate(&self) {
        self.apply_to_gate(&crate::llm_gate::LLM_GATE);
    }
}

/// Request body for `POST {base_url}/chat/completions`.
#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ToolSpec<'a>]>,
}

/// One message in the conversation. `content` is None for pure
/// tool-call assistant turns; `tool_calls` is None for user/assistant
/// text turns and for the tool RESULT messages (which carry
/// `tool_call_id` instead).
#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

/// A tool definition advertised to the model (OpenAI function shape).
#[derive(Serialize, Clone, Copy)]
pub struct ToolSpec<'a> {
    #[serde(rename = "type")]
    kind: &'a str, // always "function"
    function: FunctionSpec<'a>,
}

#[derive(Serialize, Clone, Copy)]
struct FunctionSpec<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

/// A tool invocation the model requested.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ToolCall {
    /// The model's call id (echoed back in the tool result message).
    #[serde(default)]
    pub id: String,
    /// Always "function" (OpenAI wire shape).
    #[serde(rename = "type", default = "default_call_type")]
    kind: String,
    pub function: FunctionCall,
}

fn default_call_type() -> String {
    "function".to_string()
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FunctionCall {
    pub name: String,
    /// Raw JSON arguments string from the model.
    pub arguments: String,
}

/// Response shape (only the fields we need).
#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("LLM request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("LLM response missing content")]
    EmptyResponse,
    #[error("LLM endpoint returned status {status}: {body}")]
    Status { status: u16, body: String },
}

impl LlmError {
    /// Backend-down classification (outage-recovery spec §3.2 / D1): this
    /// error means the request **never reached a working model**, so it
    /// says something about backend health, not about the run. A
    /// transport failure never arrived; 5xx is the backend or its
    /// upstream; 401/403 is misconfigured credentials (operator-
    /// actionable, treated as down, not as weather). Everything else —
    /// 4xx other than 401/403, an empty completion — is a real run
    /// outcome from a model that was reachable.
    #[must_use]
    pub fn is_backend_down(&self) -> bool {
        match self {
            LlmError::Request(_) => true,
            LlmError::Status { status, .. } => *status >= 500 || *status == 401 || *status == 403,
            LlmError::EmptyResponse => false,
        }
    }
}

/// Unix timestamp of the last successful LLM completion (0 = none
/// yet). Set inside the response-parsing paths of `chat_system` and
/// `chat_with_tools` so every consumer (inline query/chat, the drain,
/// hot-memory lookups) reports through one choke point; /metrics and
/// /health read it as the LLM-alive signal.
pub static LLM_LAST_SUCCESS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn record_llm_success() {
    let unix = chrono::Utc::now().timestamp().max(0) as u64;
    LLM_LAST_SUCCESS.store(unix, std::sync::atomic::Ordering::Relaxed);
    // Spec §3.1: this choke point doubles as the health registry's ok
    // note — a real completion is the strongest healthy observation
    // there is, so the machine flips Green without waiting for a probe.
    crate::llm_health::LLM_HEALTH.note_ok();
}

/// Passive failure hook (outage-recovery spec §3.1): called at every
/// `LlmClient` completion with the run's outcome. Only the backend-down
/// classification ([`LlmError::is_backend_down`]) feeds the health
/// registry — a 400 from a reachable model is not backend health. Ok
/// results note nothing: `record_llm_success` already noted them at the
/// `LLM_LAST_SUCCESS` choke point.
fn record_outcome<T>(result: &Result<T, LlmError>) {
    if let Err(e) = result
        && e.is_backend_down()
    {
        crate::llm_health::LLM_HEALTH.note_failure();
    }
}

/// A client for one OpenAI-compatible endpoint.
#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    /// Bearer token attached to every request when configured.
    api_key: Option<String>,
}

debug_redacted!(LlmClient { http, base_url, model }, key: api_key);

impl LlmClient {
    pub fn new(config: &LlmConfig) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
            base_url: config.url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            api_key: config.api_key.clone(),
        }
    }

    /// The chat-completions request with auth (Bearer when a key is
    /// configured, bare otherwise) — the single POST site both send
    /// paths go through.
    async fn post_chat(
        &self,
        url: &str,
        body: &ChatRequest<'_>,
    ) -> Result<reqwest::Response, LlmError> {
        let mut request = self.http.post(url);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        Ok(request.json(body).send().await?)
    }

    /// Send a single user-message prompt; returns the assistant text.
    pub async fn chat(&self, prompt: &str) -> Result<String, LlmError> {
        let result = self.chat_system_impl(None, prompt, 0.2).await;
        record_outcome(&result);
        result
    }

    /// One tool-free generation with an optional system prompt and
    /// explicit temperature (hot-memory lookups use system + temp 0).
    pub async fn chat_system(
        &self,
        system: Option<&str>,
        prompt: &str,
        temperature: f32,
    ) -> Result<String, LlmError> {
        let result = self.chat_system_impl(system, prompt, temperature).await;
        record_outcome(&result);
        result
    }

    async fn chat_system_impl(
        &self,
        system: Option<&str>,
        prompt: &str,
        temperature: f32,
    ) -> Result<String, LlmError> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut messages = Vec::new();
        if let Some(sys) = system {
            messages.push(ChatMessage {
                role: "system",
                content: Some(sys),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        messages.push(ChatMessage {
            role: "user",
            content: Some(prompt),
            tool_calls: None,
            tool_call_id: None,
        });
        let body = ChatRequest {
            model: &self.model,
            messages,
            temperature,
            tools: None,
        };
        let response = self.post_chat(&url, &body).await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(LlmError::Status {
                status: status.as_u16(),
                body: text.chars().take(500).collect(),
            });
        }
        let parsed: ChatResponse = response.json().await?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or(LlmError::EmptyResponse)?;
        record_llm_success();
        Ok(content)
    }

    /// One agentic step: send the conversation (with tool specs) and
    /// get back either the assistant's final text or its requested
    /// tool calls. The caller executes the tools, appends the results
    /// as `role: "tool"` messages, and loops until a text answer or
    /// the step cap.
    pub async fn chat_with_tools(
        &self,
        system: &str,
        conversation: &[ConversationTurn],
        tools: &[ToolSpec<'_>],
        temperature: f32,
    ) -> Result<StepOutput, LlmError> {
        let result = self
            .chat_with_tools_impl(system, conversation, tools, temperature)
            .await;
        record_outcome(&result);
        result
    }

    async fn chat_with_tools_impl(
        &self,
        system: &str,
        conversation: &[ConversationTurn],
        tools: &[ToolSpec<'_>],
        temperature: f32,
    ) -> Result<StepOutput, LlmError> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut messages: Vec<ChatMessage> = Vec::with_capacity(conversation.len() + 1);
        messages.push(ChatMessage {
            role: "system",
            content: Some(system),
            tool_calls: None,
            tool_call_id: None,
        });
        for turn in conversation {
            messages.push(match turn {
                ConversationTurn::User(text) => ChatMessage {
                    role: "user",
                    content: Some(text),
                    tool_calls: None,
                    tool_call_id: None,
                },
                ConversationTurn::Assistant(text) => ChatMessage {
                    role: "assistant",
                    content: Some(text),
                    tool_calls: None,
                    tool_call_id: None,
                },
                ConversationTurn::AssistantToolCalls(calls) => ChatMessage {
                    role: "assistant",
                    content: None,
                    tool_calls: Some(calls.as_slice().to_vec()),
                    tool_call_id: None,
                },
                ConversationTurn::ToolResult { call_id, result } => ChatMessage {
                    role: "tool",
                    content: Some(result),
                    tool_calls: None,
                    tool_call_id: Some(call_id),
                },
            });
        }
        let body = ChatRequest {
            model: &self.model,
            messages,
            temperature,
            tools: Some(tools),
        };
        let response = self.post_chat(&url, &body).await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(LlmError::Status {
                status: status.as_u16(),
                body: text.chars().take(500).collect(),
            });
        }
        let parsed: ChatResponse = response.json().await?;
        let Some(choice) = parsed.choices.into_iter().next() else {
            return Err(LlmError::EmptyResponse);
        };
        let tool_calls = choice.message.tool_calls.unwrap_or_default();
        // Both Ok returns below are successful completions — record once
        // at the shared choke point (the Err paths already returned).
        record_llm_success();
        if !tool_calls.is_empty() {
            Ok(StepOutput::ToolCalls(tool_calls))
        } else {
            Ok(StepOutput::Text(choice.message.content.unwrap_or_default()))
        }
    }
}

/// One turn of the agent conversation (the caller builds the history).
#[derive(Debug, Clone)]
pub enum ConversationTurn {
    User(String),
    Assistant(String),
    /// The assistant's tool-call request — MUST precede the matching
    /// ToolResult turns (OpenAI protocol: each role:"tool" message
    /// responds to a preceding assistant message with tool_calls).
    AssistantToolCalls(Vec<ToolCall>),
    ToolResult {
        call_id: String,
        result: String,
    },
}

/// The model's output for one step.
pub enum StepOutput {
    /// The model wants tools executed (loop continues).
    ToolCalls(Vec<ToolCall>),
    /// The model produced a final text answer (loop ends).
    Text(String),
}

/// Build a tool spec from name + description + JSON-schema parameters.
pub fn tool_spec<'a>(
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
) -> ToolSpec<'a> {
    ToolSpec {
        kind: "function",
        function: FunctionSpec {
            name,
            description,
            parameters,
        },
    }
}

/// Strip a single enclosing markdown code fence from an LLM response
/// (models habitually wrap JSON in ```json ... ```).
pub fn strip_code_fence(text: &str) -> String {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed.to_string();
    };
    // Skip an optional language tag on the opening fence line.
    let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or(rest);
    let Some(stripped) = rest.strip_suffix("```") else {
        return rest.trim().to_string();
    };
    stripped.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // The header tests need axum + IntoResponse; axum is a dev-dependency
    // (re-exported here via the workspace dep in tests), so these imports
    // are test-scope only.
    use axum::response::IntoResponse;

    #[test]
    fn strips_fences() {
        assert_eq!(strip_code_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fence("```\nplain\n```"), "plain");
        assert_eq!(strip_code_fence("no fence"), "no fence");
        assert_eq!(strip_code_fence("  trimmed  "), "trimmed");
    }

    /// D1 (§3.2) classifier pin: what counts as "never reached a working
    /// model" (backend-down class) versus a real run outcome.
    #[tokio::test]
    async fn backend_down_class_pins() {
        // Request-shaped: transport never reached the backend.
        let reqwest_err = reqwest::Client::new()
            .get("http://127.0.0.1:1/v1/models")
            .send()
            .await
            .unwrap_err();
        assert!(
            LlmError::from(reqwest_err).is_backend_down(),
            "a transport failure is backend-down"
        );

        let status = |code: u16| LlmError::Status {
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
            !LlmError::EmptyResponse.is_backend_down(),
            "an empty completion is a model outcome, not backend health"
        );

        use crate::agent::AgentError;
        assert!(
            !AgentError::Store(mycelium_store::ConceptStoreError::NotFound("/x.md".into()))
                .is_backend_down()
        );
        assert!(!AgentError::StepCapExceeded.is_backend_down());
        assert!(
            !AgentError::BadToolArgs {
                name: "t".into(),
                reason: "r".into()
            }
            .is_backend_down()
        );
        // The Llm variant delegates to the classifier above.
        assert!(
            AgentError::from(LlmError::Status {
                status: 503,
                body: String::new()
            })
            .is_backend_down()
        );
        assert!(!AgentError::from(LlmError::EmptyResponse).is_backend_down());
    }

    #[tokio::test]
    async fn unreachable_endpoint_errors() {
        let _health = crate::llm_health::health_test_guard().await;
        let client = LlmClient::new(&LlmConfig {
            url: "http://127.0.0.1:1/v1".into(),
            model: "m".into(),
            api_key: None,
            ..Default::default()
        });
        assert!(client.chat("hi").await.is_err());
    }

    /// Passive health hooks (spec §3.1): every completion feeds the
    /// registry — a transport failure notes one backend-down failure
    /// exactly once (no double-count through `chat`'s delegation), and
    /// a success flips the machine back to Green.
    #[tokio::test]
    async fn passive_hooks_feed_health() {
        let _health = crate::llm_health::health_test_guard().await;
        crate::llm_health::LLM_HEALTH.reset_for_test();

        let dead = LlmClient::new(&LlmConfig {
            url: "http://127.0.0.1:1/v1".into(),
            model: "m".into(),
            api_key: None,
            ..Default::default()
        });
        assert!(dead.chat("hi").await.is_err());
        let s = crate::llm_health::LLM_HEALTH.snapshot();
        assert_eq!(
            s.consecutive_failures, 1,
            "one failed completion notes exactly one backend-down failure"
        );
        assert_eq!(s.state, crate::llm_health::BackendState::Yellow);

        let (url, _captured) = auth_echo_mock().await;
        let live = LlmClient::new(&LlmConfig {
            url,
            model: "m".into(),
            api_key: None,
            ..Default::default()
        });
        assert_eq!(live.chat("hi").await.unwrap(), "ok");
        let s = crate::llm_health::LLM_HEALTH.snapshot();
        assert_eq!(
            s.state,
            crate::llm_health::BackendState::Green,
            "a successful completion flips health back to Green"
        );
        assert_eq!(s.consecutive_failures, 0);
        assert!(
            s.last_observation.is_some(),
            "the success noted an observation"
        );
    }

    /// An axum mock that echoes back the request's Authorization header
    /// (`Some("Bearer ...")`, or None when the header is absent) so
    /// tests can assert exactly what the client put on the wire.
    async fn auth_echo_mock() -> (String, Arc<tokio::sync::Mutex<Option<String>>>) {
        type Captured = Arc<tokio::sync::Mutex<Option<String>>>;
        let state: Captured = Arc::new(tokio::sync::Mutex::new(None));
        let handler_state = Arc::clone(&state);
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let state = handler_state;
                async move {
                    let auth = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok().map(|s| s.to_string()));
                    *state.lock().await = auth;
                    axum::Json(serde_json::json!({
                        "choices": [{ "message": { "role": "assistant", "content": "ok" } }]
                    }))
                    .into_response()
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

    #[tokio::test]
    async fn bearer_header_sent_when_key_configured() {
        // Feeds the global health registry (success notes) — serialize
        // against the tests that assert on it.
        let _health = crate::llm_health::health_test_guard().await;
        let (url, captured) = auth_echo_mock().await;
        let client = LlmClient::new(&LlmConfig {
            url: url.clone(),
            model: "m".into(),
            api_key: Some("test-secret".into()),
            ..Default::default()
        });
        assert_eq!(client.chat("hi").await.unwrap(), "ok");
        let seen = captured.lock().await.clone();
        let expected = Some("Bearer test-secret".to_string());
        assert_eq!(seen, expected, "plain chat must send the Bearer header");
        // One generation with tools goes through the same POST site.
        let no_params = serde_json::json!({});
        let spec = tool_spec("noop", "nothing", &no_params);
        let step = client
            .chat_with_tools("sys", &[], &[spec], 0.2)
            .await
            .unwrap();
        assert!(matches!(step, StepOutput::Text(_)));
        let seen = captured.lock().await.clone();
        assert_eq!(
            seen, expected,
            "Authorization: Bearer must ride both send paths"
        );
    }

    #[tokio::test]
    async fn no_header_without_key() {
        let _health = crate::llm_health::health_test_guard().await;
        let (url, captured) = auth_echo_mock().await;
        let client = LlmClient::new(&LlmConfig {
            url,
            model: "m".into(),
            api_key: None,
            ..Default::default()
        });
        assert_eq!(client.chat("hi").await.unwrap(), "ok");
        let seen = captured.lock().await.clone();
        assert_eq!(
            seen, None,
            "no Authorization header may be sent for a keyless endpoint"
        );
    }

    #[test]
    fn legacy_config_json_parses_without_api_key() {
        // What v0.2.0-beta.1's admin surface wrote: no api_key field.
        let cfg: LlmConfig =
            serde_json::from_str(r#"{"url": "http://b/v1", "model": "mycelium"}"#).unwrap();
        assert_eq!(cfg.url, "http://b/v1");
        assert_eq!(cfg.model, "mycelium");
        assert_eq!(cfg.api_key, None);
    }

    #[test]
    fn debug_impl_redacts_api_key() {
        // A future debug!-formatting site must never leak the key.
        let cfg = LlmConfig {
            url: "http://b/v1".into(),
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
        let client = LlmClient::new(&cfg);
        let rendered = format!("{client:?}");
        assert!(rendered.contains("***"));
        assert!(!rendered.contains("super-secret"));
    }

    #[test]
    fn defaults_parse_when_absent() {
        // A sealed row written before the concurrency fields existed:
        // every new field must fall back to its documented default.
        let cfg: LlmConfig =
            serde_json::from_str(r#"{"url": "http://b/v1", "model": "m"}"#).unwrap();
        assert_eq!(cfg.max_parallel, 2);
        assert_eq!(cfg.per_user_active, 2);
        assert_eq!(cfg.query_wait_secs, 30);
        assert_eq!(cfg.chat_wait_secs, 600);
        assert_eq!(cfg.chat_queue_depth, 64);

        // Default::default() agrees with the serde defaults.
        let d = LlmConfig::default();
        assert_eq!(d.max_parallel, cfg.max_parallel);
        assert_eq!(d.per_user_active, cfg.per_user_active);
        assert_eq!(d.query_wait_secs, cfg.query_wait_secs);
        assert_eq!(d.chat_wait_secs, cfg.chat_wait_secs);
        assert_eq!(d.chat_queue_depth, cfg.chat_queue_depth);
    }

    #[test]
    fn clamp_high_and_low() {
        let high = LlmConfig {
            max_parallel: 999,
            per_user_active: 999,
            query_wait_secs: 10_000,
            chat_wait_secs: 100_000,
            chat_queue_depth: 99_999,
            ..LlmConfig::default()
        }
        .clamp();
        assert_eq!(high.max_parallel, 64);
        assert_eq!(high.per_user_active, 16);
        assert_eq!(high.query_wait_secs, 600);
        assert_eq!(high.chat_wait_secs, 7200);
        assert_eq!(high.chat_queue_depth, 1024);

        let low = LlmConfig {
            max_parallel: 0,
            per_user_active: 0,
            query_wait_secs: 0,
            chat_wait_secs: 0,
            chat_queue_depth: 0,
            ..LlmConfig::default()
        }
        .clamp();
        assert_eq!(low.max_parallel, 1);
        assert_eq!(low.per_user_active, 1);
        assert_eq!(low.query_wait_secs, 1);
        assert_eq!(low.chat_wait_secs, 1);
        assert_eq!(low.chat_queue_depth, 1);

        // url/model/api_key pass through untouched.
        let cfg = LlmConfig {
            url: "http://b/v1".into(),
            model: "m".into(),
            api_key: Some("k".into()),
            max_parallel: 0,
            ..LlmConfig::default()
        }
        .clamp();
        assert_eq!(cfg.url, "http://b/v1");
        assert_eq!(cfg.model, "m");
        assert_eq!(cfg.api_key.as_deref(), Some("k"));
    }

    #[test]
    fn clamp_is_idempotent() {
        for cfg in [
            LlmConfig::default(),
            LlmConfig {
                max_parallel: 999,
                per_user_active: 0,
                query_wait_secs: 0,
                chat_wait_secs: 100_000,
                chat_queue_depth: 7,
                ..LlmConfig::default()
            },
        ] {
            let once = cfg.clamp();
            let twice = once.clamp();
            assert_eq!(twice.max_parallel, once.max_parallel);
            assert_eq!(twice.per_user_active, once.per_user_active);
            assert_eq!(twice.query_wait_secs, once.query_wait_secs);
            assert_eq!(twice.chat_wait_secs, once.chat_wait_secs);
            assert_eq!(twice.chat_queue_depth, once.chat_queue_depth);
        }
    }

    #[test]
    fn roundtrip_serialize() {
        // Sealing is serialize + AEAD, so a serde roundtrip is what the
        // stored row goes through: every field must survive it.
        let cfg = LlmConfig {
            url: "http://b/v1".into(),
            model: "m".into(),
            api_key: Some("k".into()),
            max_parallel: 8,
            per_user_active: 3,
            query_wait_secs: 45,
            chat_wait_secs: 900,
            chat_queue_depth: 128,
        };
        let back: LlmConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.url, cfg.url);
        assert_eq!(back.model, cfg.model);
        assert_eq!(back.api_key, cfg.api_key);
        assert_eq!(back.max_parallel, 8);
        assert_eq!(back.per_user_active, 3);
        assert_eq!(back.query_wait_secs, 45);
        assert_eq!(back.chat_wait_secs, 900);
        assert_eq!(back.chat_queue_depth, 128);
    }

    #[test]
    fn apply_to_gate_sets_clamped_limits() {
        // A fresh gate (never the LLM_GATE static — tests must not
        // mutate process-global state).
        let gate = crate::llm_gate::LlmGate::new(2);
        LlmConfig {
            max_parallel: 999,
            per_user_active: 3,
            chat_queue_depth: 10,
            ..LlmConfig::default()
        }
        .apply_to_gate(&gate);
        assert_eq!(gate.capacity(), 64, "capacity takes the clamped value");
        assert_eq!(gate.per_user_active(), 3);
        assert_eq!(gate.chat_queue_depth_limit(), 10);

        // Out-of-range lows clamp up rather than opening a zero gate.
        LlmConfig {
            max_parallel: 0,
            per_user_active: 0,
            chat_queue_depth: 0,
            ..LlmConfig::default()
        }
        .apply_to_gate(&gate);
        assert_eq!(gate.capacity(), 1);
        assert_eq!(gate.per_user_active(), 1);
        assert_eq!(gate.chat_queue_depth_limit(), 1);
    }

    #[test]
    fn debug_impl_shows_concurrency_fields() {
        let rendered = format!(
            "{:?}",
            LlmConfig {
                max_parallel: 7,
                api_key: Some("super-secret".into()),
                ..LlmConfig::default()
            }
        );
        assert!(rendered.contains("max_parallel: 7"), "{rendered}");
        assert!(rendered.contains("chat_queue_depth"), "{rendered}");
        assert!(!rendered.contains("super-secret"), "key leaked: {rendered}");
    }
}
