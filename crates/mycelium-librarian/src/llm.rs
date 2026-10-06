//! OpenAI-compatible LLM client (Ollama default per DESIGN).
//!
//! Talks to any endpoint implementing `POST {base_url}/chat/completions`
//! with the OpenAI request/response shape. Timeouts are enforced so a
//! hung backend can never wedge an ingest job.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Admin-managed LLM backend config (stored in ConfigStore under "llm").
/// Ollama default per DESIGN; any OpenAI-compatible endpoint works.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub url: String,
    pub model: String,
    /// Optional bearer token for endpoints that require auth (an
    /// API-key gateway). Sealed at rest by the admin surface; sent as
    /// `Authorization: Bearer <api_key>` when present, no header when
    /// absent (keyless endpoints keep working).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:11434/v1".into(),
            model: "default".into(),
            api_key: None,
        }
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

/// Unix timestamp of the last successful LLM completion (0 = none
/// yet). Set inside the response-parsing paths of `chat_system` and
/// `chat_with_tools` so every consumer (inline query/chat, the drain,
/// hot-memory lookups) reports through one choke point; /metrics and
/// /health read it as the LLM-alive signal.
pub static LLM_LAST_SUCCESS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn record_llm_success() {
    let unix = chrono::Utc::now().timestamp().max(0) as u64;
    LLM_LAST_SUCCESS.store(unix, std::sync::atomic::Ordering::Relaxed);
}

/// A client for one OpenAI-compatible endpoint.
#[derive(Debug, Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    /// Bearer token attached to every request when configured.
    api_key: Option<String>,
}

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
    async fn post_chat(&self, url: &str, body: &ChatRequest<'_>) -> Result<reqwest::Response, LlmError> {
        let mut request = self.http.post(url);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        Ok(request.json(body).send().await?)
    }

    /// Send a single user-message prompt; returns the assistant text.
    pub async fn chat(&self, prompt: &str) -> Result<String, LlmError> {
        self.chat_system(None, prompt, 0.2).await
    }

    /// One tool-free generation with an optional system prompt and
    /// explicit temperature (hot-memory lookups use system + temp 0).
    pub async fn chat_system(
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

    #[tokio::test]
    async fn unreachable_endpoint_errors() {
        let client = LlmClient::new(&LlmConfig {
            url: "http://127.0.0.1:1/v1".into(),
            model: "m".into(),
            api_key: None,
        });
        assert!(client.chat("hi").await.is_err());
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
        let (url, captured) = auth_echo_mock().await;
        let client = LlmClient::new(&LlmConfig {
            url: url.clone(),
            model: "m".into(),
            api_key: Some("test-secret".into()),
        });
        assert_eq!(client.chat("hi").await.unwrap(), "ok");
        let seen = captured.lock().await.clone();
        let expected = Some("Bearer test-secret".to_string());
        assert_eq!(
            seen, expected, "plain chat must send the Bearer header"
        );
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
        let (url, captured) = auth_echo_mock().await;
        let client = LlmClient::new(&LlmConfig {
            url,
            model: "m".into(),
            api_key: None,
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
}
