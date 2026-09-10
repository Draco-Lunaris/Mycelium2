//! OpenAI-compatible LLM client.

#[derive(Debug, Clone)]
pub struct LlmClient {
    pub base_url: String,
    pub model: String,
}

impl LlmClient {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
        }
    }

    pub async fn chat(&self, _prompt: &str) -> Result<String, reqwest::Error> {
        // TODO: call OpenAI-compatible chat completions endpoint
        Ok(String::new())
    }
}
