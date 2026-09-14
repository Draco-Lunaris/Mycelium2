//! Seed memory (v1 parity): a compact overview of the GLOBAL stores —
//! concept types in use, per-segment concept descriptions, the library's
//! books — injected into the MCP `initialize` instructions and the
//! memory_query tool description. Without it the client model has no
//! signal that memory might hold an answer, so it never thinks to look.
//!
//! Unlike the on-disk index.md (navigation: titles + links), the seed
//! lists concept DESCRIPTIONS per segment — semantic hooks beat
//! filenames for igniting the "memory might know this" instinct.
//!
//! The seed is process-global ( RwLock) and refreshed after mutations
//! and book ingests; `initialize` and `tools/list` are per-request but
//! must stay cheap, so they read the cached string.

use std::sync::RwLock;

use mycelium_crypto::keys::ServiceKey;
use mycelium_store::{ConceptStore, Store};

static SEED: RwLock<Option<String>> = RwLock::new(None);

const MAX_SEED_CHARS: usize = 3000;
const MAX_DESCRIPTIONS_PER_SEGMENT: usize = 10;

/// Build the seed overview from the global skills shelf + library
/// catalogs. Deterministic, no LLM. Best-effort: unreadable segments are
/// skipped, an empty store degrades to a minimal seed.
pub async fn build_seed(store: &Store, service_key: &ServiceKey) -> String {
    let skills =
        ConceptStore::for_service(store, service_key.clone(), &store.skills_dir(), "skills");
    let library =
        ConceptStore::for_service(store, service_key.clone(), &store.library_dir(), "library");

    let mut sections: Vec<String> = Vec::new();

    // Skills shelf segment.
    if let Ok(entries) = skills.list().await
        && !entries.is_empty()
    {
        let shown: Vec<String> = entries
            .iter()
            .take(MAX_DESCRIPTIONS_PER_SEGMENT)
            .map(|e| format!("    * **{}** [{}]", e.title, e.concept_type))
            .collect();
        let more = entries.len().saturating_sub(shown.len());
        sections.push(format!(
            "* skills/ — {} concept(s):\n{}{}",
            entries.len(),
            shown.join("\n"),
            if more > 0 {
                format!("\n    * …and {more} more")
            } else {
                String::new()
            }
        ));
    }

    // Library segment: books by shelf visibility (the seed is global —
    // private-shelf books are listed by title only for admins; the
    // per-request visibility filter enforces the real gate).
    if let Ok(entries) = library.list().await {
        let books: Vec<&mycelium_store::ConceptEntry> = entries
            .iter()
            .filter(|e| e.path.ends_with("/book.md"))
            .collect();
        if !books.is_empty() {
            let lines: Vec<String> = books
                .iter()
                .map(|b| format!("    * **{}** ({})", b.title, b.path))
                .collect();
            sections.push(format!(
                "* library/ — {} book(s):\n{}",
                books.len(),
                lines.join("\n")
            ));
        }
    }

    let mut seed = if sections.is_empty() {
        "The knowledge base is currently empty.".to_string()
    } else {
        format!("Memory segments:\n{}", sections.join("\n"))
    };
    if seed.len() > MAX_SEED_CHARS {
        seed.truncate(MAX_SEED_CHARS);
        seed.push_str("\n… (truncated — use memory_query to explore further)");
    }
    seed
}

/// Refresh the cached seed (called at boot, after mutations, and after
/// book ingests). Never fails the caller — a refresh error keeps the
/// old seed.
pub async fn refresh_seed(store: &Store, service_key: &ServiceKey) {
    let seed = build_seed(store, service_key).await;
    if let Ok(mut guard) = SEED.write() {
        *guard = Some(seed);
    }
}

/// The cached seed (empty string before the first refresh).
pub fn current_seed() -> String {
    SEED.read().ok().and_then(|g| g.clone()).unwrap_or_default()
}

/// The initialize `instructions` block — seed plus the
/// instinct-igniting rules (v1's seedInstructions).
pub fn seed_instructions() -> String {
    let seed = current_seed();
    format!(
        "This server is your persistent memory — an OKF knowledge base of markdown concepts that survives across sessions.\n\n\
         MEMORY OVERVIEW (as of the last refresh):\n\n{seed}\n\n\
         How to use your memory:\n\
         - BEFORE answering anything related to the topics above, call mycelium2_memory_query — the answer may already be stored. Prefer stored knowledge over guessing.\n\
         - When you learn a lasting fact, decision, preference, or piece of documentation, persist it with mycelium2_memory_add. If it isn't stored, it will be forgotten.\n\
         - When existing knowledge turns out to be wrong or outdated, fix it with mycelium2_memory_update.\n\
         - mycelium2_memory_status reports the size and health of the memory.\n\
         - Books: the library catalogs books as `Book`/`Chapter` concepts; a chapter's full text is fetched on demand via the agent's read_passage tool. Ask memory_query about book content and the librarian agent will search the catalogs and read the passages."
    )
}
