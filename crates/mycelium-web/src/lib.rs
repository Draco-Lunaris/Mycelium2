//! Mycelium2 web server: HTTPS + Leptos SSR frontend + REST API.

pub mod api;
pub mod assets;
pub mod cert;
pub mod health;
pub mod middleware;
pub mod pages;
pub mod server;
pub mod state;

pub use server::serve;
pub use state::{AppState, MasterKeyCache};

/// Admin-managed LLM backend config (stored in ConfigStore under "llm").
/// Ollama default per DESIGN; any OpenAI-compatible endpoint works.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
