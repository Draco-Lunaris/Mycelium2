//! Dreaming (v1 parity): an autonomous consolidation pass over a
//! user's memory — what a brain does during sleep. Deterministic
//! signals decide whether there is anything to dream about (orphans,
//! broken links, likely duplicates, oversized concepts); the agent then
//! consolidates. No signals → no run, no tokens.
//!
//! The web layer exposes it as an admin/user-triggered maintenance
//! action (v1 ran it on a cron-like schedule; the trigger wiring is
//! the caller's choice).

use mycelium_store::ConceptStore;

use crate::agent::{self, AgentScopes};
use crate::llm::LlmClient;

/// What a dream pass found and did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DreamReport {
    pub ran: bool,
    /// Why the dream was skipped (when ran=false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_changed: Option<Vec<String>>,
}

/// Duplicate candidate: two concepts with similar titles/descriptions.
#[derive(Debug)]
struct DuplicateCandidate {
    a: String,
    b: String,
}

const OVERSIZED_CHARS: usize = 20_000;
const DUPLICATE_SIMILARITY: f64 = 0.8;

/// Run one dream pass over the caller's bundle. Deterministic signals
/// first; the agent runs only when there is something to do.
pub async fn run_dream(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    scopes: &AgentScopes<'_>,
) -> Result<DreamReport, agent::AgentError> {
    // Gather the deterministic signals (no LLM).
    let entries = store.list().await?;
    let mut concepts = Vec::with_capacity(entries.len());
    for entry in &entries {
        if let Ok(c) = store.get(&entry.path).await {
            concepts.push(c);
        }
    }
    let bundle = mycelium_core::bundle::Bundle {
        root: std::path::PathBuf::new(),
        concepts: concepts.clone(),
        shelf_info: None,
        reserved_files_seen: Vec::new(),
        naming_warnings: Vec::new(),
    };
    let graph = mycelium_core::graph::build_graph(&bundle);
    let _health = graph.health();
    let dupes = duplicate_candidates(&concepts);
    let fat = oversized_concepts(&concepts);

    let mut signals: Vec<String> = Vec::new();
    if !graph.orphans.is_empty() {
        signals.push(format!(
            "ORPHANED CONCEPTS (nothing links to them). Read each and wire it into genuinely \
             related concepts; if it relates to nothing, leave it alone:\n{}",
            graph
                .orphans
                .iter()
                .map(|o| format!("- {o}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !graph.broken_links.is_empty() {
        signals.push(format!(
            "BROKEN LINKS (target missing). Fix the path if the target moved, remove the link \
             if it is gone:\n{}",
            graph
                .broken_links
                .iter()
                .map(|b| format!("- {} → {}", b.from, b.to))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !dupes.is_empty() {
        signals.push(format!(
            "LIKELY DUPLICATES (title similarity). Read each pair; if they cover the same thing, \
             merge the content into the better-placed concept, update anything that linked to \
             the removed one, and delete the duplicate (deletion IS authorized for true \
             duplicates after merging). If they are genuinely distinct, cross-link them instead:\n{}",
            dupes
                .iter()
                .map(|d| format!("- {} ↔ {}", d.a, d.b))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !fat.is_empty() {
        signals.push(format!(
            "OVERSIZED CONCEPTS (grown too large through repeated enrichment). For each: if the \
             body contains genuinely separable topics, extract each into its OWN concept \
             (proper type/title, back-linked per the rules), then rewrite the ORIGINAL file as \
             a hub — a short summary linking to every extracted concept. NEVER delete or \
             rename the original path; other concepts link to it. If the content is one \
             indivisible topic, leave it alone:\n{}",
            fat.iter()
                .map(|f| format!("- {} ({} chars)", f.0, f.1))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    if signals.is_empty() {
        return Ok(DreamReport {
            ran: false,
            reason: Some(
                "no signals: no orphans, broken links, duplicates, or oversized concepts".into(),
            ),
            summary: None,
            files_changed: None,
        });
    }

    // Hand the signals to the agent as one mutation instruction.
    let instruction = format!(
        "This is a scheduled maintenance pass over the knowledge base (a 'dream'). Address each \
         signal below. Work through them in order; when done, summarize exactly what changed.\n\n{}",
        signals.join("\n\n")
    );
    let result = agent::run_mutation(client, store, scopes, &instruction).await?;
    Ok(DreamReport {
        ran: true,
        reason: None,
        summary: Some(result.summary),
        files_changed: Some(result.files_changed),
    })
}

/// Title-similarity duplicate candidates (v1's deterministic signal:
/// token-set Jaccard over titles, threshold 0.8).
fn duplicate_candidates(concepts: &[mycelium_core::Concept]) -> Vec<DuplicateCandidate> {
    let mut out = Vec::new();
    for i in 0..concepts.len() {
        for j in (i + 1)..concepts.len() {
            let a = concepts[i].frontmatter.title.clone().unwrap_or_default();
            let b = concepts[j].frontmatter.title.clone().unwrap_or_default();
            if a.is_empty() || b.is_empty() {
                continue;
            }
            if jaccard(&a, &b) >= DUPLICATE_SIMILARITY {
                out.push(DuplicateCandidate {
                    a: concepts[i].source_path.clone(),
                    b: concepts[j].source_path.clone(),
                });
            }
        }
    }
    out
}

fn jaccard(a: &str, b: &str) -> f64 {
    let ta: std::collections::HashSet<String> = mycelium_core::search::tokenize(&a.to_lowercase())
        .into_iter()
        .collect();
    let tb: std::collections::HashSet<String> = mycelium_core::search::tokenize(&b.to_lowercase())
        .into_iter()
        .collect();
    if ta.is_empty() && tb.is_empty() {
        return 1.0;
    }
    let inter = ta.intersection(&tb).count();
    let union = ta.union(&tb).count();
    if union == 0 {
        return 0.0;
    }
    inter as f64 / union as f64
}

/// Concepts whose body exceeds OVERSIZED_CHARS (v1's fat-concept signal).
fn oversized_concepts(concepts: &[mycelium_core::Concept]) -> Vec<(String, usize)> {
    concepts
        .iter()
        .filter(|c| c.body.len() > OVERSIZED_CHARS)
        .map(|c| (c.source_path.clone(), c.body.len()))
        .collect()
}
