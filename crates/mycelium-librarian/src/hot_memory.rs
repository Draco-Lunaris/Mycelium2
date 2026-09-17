//! Hot memory (v1 parity): a small working set of recently written
//! concepts and recent Q&A pairs. Queries consult it BEFORE the deep
//! agent run — one cheap, tool-free LLM call over a tiny context.
//! Misses fall through to deep memory (the full agent loop). Short-term
//! memory in front of long-term.
//!
//! Staleness rules (v1):
//! - Hot concepts are stored as PATHS and read fresh at lookup — never
//!   stale. A path deleted behind our back drops out of the set.
//! - Hot Q&A pairs are purged on any write (the write may contradict
//!   them).
//! - Everything expires after the TTL (default 1h).
//!
//! State is process-global, keyed by scope id (`user:<uuid>` /
//! `global:<ns>`): survives the per-request server instances of the
//! stateless transports, and a write to one scope never purges another's
//! recent answers.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use mycelium_store::ConceptStore;

use crate::llm::LlmClient;

const MAX_CONCEPTS: usize = 10;
const MAX_QAS: usize = 10;
const TTL: std::time::Duration = std::time::Duration::from_secs(3600);
const MAX_EXCERPT_CHARS: usize = 1500;

#[derive(Debug, Clone)]
struct HotQa {
    question: String,
    answer: String,
    at: Instant,
}

#[derive(Debug, Default)]
struct HotScopeState {
    /// path → touched_at (insertion-ordered for LRU eviction).
    concepts: Vec<(String, Instant)>,
    qas: Vec<HotQa>,
}

static HOT: Mutex<Option<HashMap<String, HotScopeState>>> = Mutex::new(None);

fn state_for(scope_id: &str) -> HotScopeState {
    let mut guard = HOT.lock().unwrap();
    guard
        .get_or_insert_with(HashMap::new)
        .remove(scope_id)
        .unwrap_or_default()
}

fn put_state(scope_id: &str, state: HotScopeState) {
    let mut guard = HOT.lock().unwrap();
    guard
        .get_or_insert_with(HashMap::new)
        .insert(scope_id.to_string(), state);
}

/// Called after any concept write/patch (the agent's write tools and
/// the direct-write fallbacks). The concept moves to the front of the
/// hot set; Q&A pairs are purged (a write may contradict them).
pub fn record_hot_write(scope_id: &str, path: &str) {
    let mut state = state_for(scope_id);
    state.concepts.retain(|(p, _)| p != path);
    state.concepts.push((path.to_string(), Instant::now()));
    while state.concepts.len() > MAX_CONCEPTS {
        state.concepts.remove(0);
    }
    state.qas.clear();
    put_state(scope_id, state);
}

/// Called on deletes: the concept leaves the hot set; answers may be
/// stale.
pub fn record_hot_delete(scope_id: &str, path: &str) {
    let mut state = state_for(scope_id);
    state.concepts.retain(|(p, _)| p != path);
    state.qas.clear();
    put_state(scope_id, state);
}

/// Called after a deep query completes — its answer joins the hot set.
pub fn record_hot_query(scope_id: &str, question: &str, answer: &str) {
    let mut state = state_for(scope_id);
    state.qas.push(HotQa {
        question: question.to_string(),
        answer: answer.to_string(),
        at: Instant::now(),
    });
    while state.qas.len() > MAX_QAS {
        state.qas.remove(0);
    }
    put_state(scope_id, state);
}

/// Test hook: reset one scope's hot set (or all, when scope_id is None).
pub fn clear_hot_memory(scope_id: Option<&str>) {
    let mut guard = HOT.lock().unwrap();
    match scope_id {
        Some(id) => {
            if let Some(map) = guard.as_mut() {
                map.remove(id);
            }
        }
        None => *guard = None,
    }
}

/// Try to answer from the hot set. Returns the answer, or `None` when
/// hot memory is empty/expired or can't answer confidently (the model
/// must reply UNKNOWN in that case, which falls through to deep memory).
pub async fn hot_lookup(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    question: &str,
) -> Option<String> {
    let scope_id = store.scope_id();
    let state = state_for(&scope_id);
    let mut sections: Vec<String> = Vec::new();

    for (path, touched) in &state.concepts {
        if touched.elapsed() > TTL {
            continue;
        }
        // Fresh read — never stale. A path deleted behind our back
        // drops out of the set.
        match store.get(path).await {
            Ok(c) => {
                let title = c.frontmatter.title.clone().unwrap_or_default();
                let desc = c.frontmatter.description.clone().unwrap_or_default();
                let header = if title.is_empty() {
                    format!("CONCEPT {}", c.source_path)
                } else if desc.is_empty() {
                    format!("CONCEPT {} — {title}", c.source_path)
                } else {
                    format!("CONCEPT {} — {title} ({desc})", c.source_path)
                };
                let excerpt: String = c.body.chars().take(MAX_EXCERPT_CHARS).collect();
                sections.push(format!("{header}\n{excerpt}"));
            }
            Err(_) => {
                let mut s = state_for(&scope_id);
                s.concepts.retain(|(p, _)| p != path);
                put_state(&scope_id, s);
            }
        }
    }
    for qa in &state.qas {
        if qa.at.elapsed() > TTL {
            continue;
        }
        sections.push(format!(
            "PREVIOUS Q&A\nQ: {}\nA: {}",
            qa.question, qa.answer
        ));
    }

    if sections.is_empty() {
        return None;
    }

    let system = "You answer questions using ONLY the recent-memory excerpts provided. \
                  These are the most recently touched pieces of a larger knowledge base. \
                  If they fully and confidently answer the question, answer concisely (and \
                  cite concept paths when you used them). If they do NOT contain enough to \
                  answer confidently, reply with exactly: UNKNOWN";
    let prompt = format!(
        "RECENT MEMORY:\n\n{}\n\nQUESTION: {question}",
        sections.join("\n\n---\n\n")
    );
    let text = client.chat_system(Some(system), &prompt, 0.0).await.ok()?;
    let text = text.trim();
    if text.is_empty() || text.to_uppercase().starts_with("UNKNOWN") {
        return None;
    }
    Some(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_records_and_purges_qas() {
        clear_hot_memory(Some("user:test"));
        record_hot_write("user:test", "/a.md");
        record_hot_query("user:test", "q1", "a1");
        record_hot_write("user:test", "/b.md");
        let state = state_for("user:test");
        assert_eq!(state.concepts.len(), 2);
        assert!(state.qas.is_empty(), "a write must purge Q&As");
        clear_hot_memory(Some("user:test"));
    }

    #[test]
    fn delete_removes_concept() {
        clear_hot_memory(Some("user:test2"));
        record_hot_write("user:test2", "/a.md");
        record_hot_delete("user:test2", "/a.md");
        let state = state_for("user:test2");
        assert!(state.concepts.is_empty());
        clear_hot_memory(Some("user:test2"));
    }

    #[test]
    fn scopes_are_independent() {
        // Scoped clears only: clear_hot_memory(None) would wipe other
        // scopes mid-test and race with the parallel tests.
        clear_hot_memory(Some("user:scope-a"));
        clear_hot_memory(Some("user:scope-b"));
        record_hot_write("user:scope-a", "/x.md");
        record_hot_write("user:scope-b", "/y.md");
        let a = state_for("user:scope-a");
        assert_eq!(a.concepts.len(), 1);
        assert_eq!(a.concepts[0].0, "/x.md");
        assert_eq!(state_for("user:scope-b").concepts[0].0, "/y.md");
        clear_hot_memory(Some("user:scope-a"));
        clear_hot_memory(Some("user:scope-b"));
    }

    #[test]
    fn lru_cap_enforced() {
        clear_hot_memory(Some("user:cap"));
        for i in 0..15 {
            record_hot_write("user:cap", &format!("/{i}.md"));
        }
        let state = state_for("user:cap");
        assert_eq!(state.concepts.len(), MAX_CONCEPTS);
        assert_eq!(state.concepts[0].0, "/5.md", "oldest evicted first");
        clear_hot_memory(Some("user:cap"));
    }
}
