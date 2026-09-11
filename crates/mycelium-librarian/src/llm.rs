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
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:11434/v1".into(),
            model: "default".into(),
        }
    }
}

/// Request body for `POST {base_url}/chat/completions`.
#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
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
    content: String,
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

/// A client for one OpenAI-compatible endpoint.
#[derive(Debug, Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
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
        }
    }

    /// Send a single user-message prompt; returns the assistant text.
    pub async fn chat(&self, prompt: &str) -> Result<String, LlmError> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = ChatRequest {
            model: &self.model,
            messages: vec![ChatMessage {
                role: "user",
                content: prompt,
            }],
            temperature: 0.2,
        };
        let response = self.http.post(&url).json(&body).send().await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(LlmError::Status {
                status: status.as_u16(),
                body: text.chars().take(500).collect(),
            });
        }
        let parsed: ChatResponse = response.json().await?;
        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or(LlmError::EmptyResponse)
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
        });
        assert!(client.chat("hi").await.is_err());
    }
}
