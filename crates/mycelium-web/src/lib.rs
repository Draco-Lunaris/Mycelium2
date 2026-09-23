//! Mycelium2 web server: HTTPS + Leptos SSR frontend + REST API.

pub mod api;
pub mod assets;
pub mod cert;
pub mod health;
pub mod middleware;
pub mod packaged_skills;
pub mod pages;
pub mod server;
pub mod state;

pub use server::serve;
pub use state::{AppState, MasterKeyCache};

/// Admin-managed LLM backend config (stored in ConfigStore under "llm").
/// Ollama default per DESIGN; any OpenAI-compatible endpoint works.
/// The canonical definition lives in mycelium-librarian (the consumer);
/// re-exported here for existing callers.
pub use mycelium_librarian::llm::LlmConfig;
