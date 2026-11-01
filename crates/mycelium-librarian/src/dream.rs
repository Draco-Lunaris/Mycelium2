//! Dreaming (v1 parity): an autonomous consolidation pass over a
//! user's memory — what a brain does during sleep. Deterministic
//! signals decide whether there is anything to dream about (orphans,
//! broken links, likely duplicates, oversized concepts); the agent then
//! consolidates. No signals → no run, no tokens.
//!
//! The web layer exposes it as an admin/user-triggered maintenance
//! action (v1 ran it on a cron-like schedule; the trigger wiring is
//! the caller's choice).

use std::collections::{BTreeSet, HashMap, HashSet};

use mycelium_store::ConceptStore;
use mycelium_store::revisions::dedup_cosine;
use mycelium_store::vectors::VectorStore;

use crate::agent::{self, AgentScopes};
use crate::llm::LlmClient;
use crate::llm_health::{BackendState, embedding_state};

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
/// Nearest neighbors drawn per concept by the cosine duplicate signal
/// (spec §3.4): a pair is a candidate only if it appears in at least
/// one endpoint's top-K list — unbounded all-pairs would nominate
/// every above-threshold pair in a large corpus.
const NEIGHBORS_PER_CONCEPT: usize = 2;

/// Run one dream pass over the caller's bundle. Deterministic signals
/// first; the agent runs only when there is something to do.
///
/// `vectors` is the caller scope's vector-row store: `Some` enables
/// the cosine duplicate signal whenever the embeddings backend is
/// healthy or the scope holds fresh current-model rows (the jaccard
/// title signal stays the fallback — and with `None` the branch is
/// absent entirely, dream behaving byte-identically to v1).
pub async fn run_dream(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    scopes: &AgentScopes<'_>,
    vectors: Option<&VectorStore<'_>>,
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
    let dupes = duplicate_candidates(&concepts, vectors).await;
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
            "LIKELY DUPLICATES (title or embedding similarity). Read each pair; if they cover the \
             same thing, \
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

/// Duplicate candidates: v1's title signal (token-set Jaccard over
/// titles, threshold 0.8) upgraded with the embedding-cosine signal
/// (spec §3.4's dream bullet). Concepts holding a fresh vector row
/// belong to the cosine branch; every other concept rides the jaccard
/// path, which stays byte-identical to v1 — and when the cosine
/// branch is absent the two-branch split is invisible: every concept
/// is "every other concept".
async fn duplicate_candidates(
    concepts: &[mycelium_core::Concept],
    vectors: Option<&VectorStore<'_>>,
) -> Vec<DuplicateCandidate> {
    let (mut pairs, embedded) = cosine_candidates(concepts, vectors).await;
    let scan: Vec<&mycelium_core::Concept> = if embedded.is_empty() {
        concepts.iter().collect()
    } else {
        concepts
            .iter()
            .filter(|c| !embedded.contains(c.source_path.as_str()))
            .collect()
    };
    pairs.extend(jaccard_pairs(&scan));
    pairs
        .into_iter()
        .map(|(a, b)| DuplicateCandidate { a, b })
        .collect()
}

/// The embedding-cosine duplicate signal over the scope's vector rows:
/// a pair is a candidate iff cosine ≥ [`dedup_cosine`] AND it sits in
/// at least one endpoint's top-[`NEIGHBORS_PER_CONCEPT`] nearest list
/// (union-of-drawings — one side suffices). Returns the candidate
/// pairs plus the paths holding a fresh row (the concepts the branch
/// owns); both empty when the branch is ABSENT: no store, an
/// unhealthy backend over an unpopulated scope, or unreadable rows
/// (a degraded embedding layer degrades dream to v1 — it never fails
/// the dream).
///
/// Population: ≥2 LIVE fresh rows under one current model — rows of
/// deleted concepts are orphaned and count for nothing. Stale rows —
/// written under a different model, the corpus invalidation's marker —
/// count for neither population nor scoring, and rows only score
/// against rows of the same model and dimensionality.
async fn cosine_candidates(
    concepts: &[mycelium_core::Concept],
    vectors: Option<&VectorStore<'_>>,
) -> (BTreeSet<(String, String)>, HashSet<String>) {
    let Some(vectors) = vectors else {
        return (BTreeSet::new(), HashSet::new());
    };
    let rows = match vectors.load_rows().await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                scope = %vectors.scope_id(),
                error = %e,
                "dream: vector rows unreadable — duplicate signal is jaccard-only"
            );
            return (BTreeSet::new(), HashSet::new());
        }
    };
    let fresh: Vec<_> = rows.into_iter().filter(|r| !r.stale).collect();
    // Row token → live concept path (rows are keyed by the same
    // one-way HMAC token; a row whose concept is gone falls away).
    let by_token: HashMap<String, &str> = concepts
        .iter()
        .map(|c| {
            (
                vectors.path_token_of(&c.source_path),
                c.source_path.as_str(),
            )
        })
        .collect();
    let live: Vec<(&str, &str, u32, &Vec<f32>)> = fresh
        .iter()
        .filter_map(|r| {
            by_token
                .get(&r.path_token)
                .map(|path| (*path, r.model.as_str(), r.dims, &r.vec_f32))
        })
        .collect();
    let embedded: HashSet<String> = live.iter().map(|(path, ..)| (*path).to_string()).collect();
    // Population counts LIVE fresh rows per model: an orphan row (its
    // concept deleted) must not admit the branch under an unhealthy
    // backend — it pairs with nothing, yet it would pull the scope's
    // live concepts out of the jaccard scan (embedded concepts ride
    // only the cosine branch, which for them yields nothing).
    let mut per_model: HashMap<&str, usize> = HashMap::new();
    for (_, model, _, _) in &live {
        *per_model.entry(*model).or_default() += 1;
    }
    let populated = per_model.values().any(|n| *n >= 2);
    // The same gate set the fusion composer uses (Task 5): Green and
    // Yellow proceed, Red/Disabled fall back — unless the scope's own
    // live fresh rows speak for the signal regardless of the backend.
    let healthy = matches!(
        embedding_state(),
        BackendState::Green | BackendState::Yellow
    );
    if !healthy && !populated {
        return (BTreeSet::new(), HashSet::new());
    }
    let mut groups: HashMap<(&str, u32), Vec<usize>> = HashMap::new();
    for (i, (_, model, dims, _)) in live.iter().enumerate() {
        groups.entry((model, *dims)).or_default().push(i);
    }
    let threshold = dedup_cosine();
    let mut out = BTreeSet::new();
    for idxs in groups.values() {
        if idxs.len() < 2 {
            continue;
        }
        // Each concept draws its NEIGHBORS_PER_CONCEPT nearest (cosine
        // desc, path asc on ties — deterministic).
        let mut drawn: BTreeSet<(String, String)> = BTreeSet::new();
        for &i in idxs {
            let mut others: Vec<(f32, &str)> = idxs
                .iter()
                .filter(|&&j| j != i)
                .map(|&j| {
                    (
                        mycelium_core::search::cosine(live[i].3, live[j].3),
                        live[j].0,
                    )
                })
                .collect();
            others.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.1.cmp(b.1))
            });
            for (_, path) in others.into_iter().take(NEIGHBORS_PER_CONCEPT) {
                drawn.insert(canonical_pair(live[i].0, path));
            }
        }
        for (x, &i) in idxs.iter().enumerate() {
            for &j in &idxs[x + 1..] {
                let pair = canonical_pair(live[i].0, live[j].0);
                if drawn.contains(&pair)
                    && mycelium_core::search::cosine(live[i].3, live[j].3) >= threshold
                {
                    out.insert(pair);
                }
            }
        }
    }
    (out, embedded)
}

/// The canonical form of an unordered pair: (min, max) by path.
fn canonical_pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

/// v1's title scan over one set of concepts: token-set Jaccard ≥
/// DUPLICATE_SIMILARITY, pairs canonicalized and deduped (the
/// BTreeSet is also the deterministic output order).
fn jaccard_pairs(concepts: &[&mycelium_core::Concept]) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for i in 0..concepts.len() {
        for j in (i + 1)..concepts.len() {
            let a = concepts[i].frontmatter.title.clone().unwrap_or_default();
            let b = concepts[j].frontmatter.title.clone().unwrap_or_default();
            if a.is_empty() || b.is_empty() {
                continue;
            }
            if jaccard(&a, &b) >= DUPLICATE_SIMILARITY {
                out.insert(canonical_pair(
                    &concepts[i].source_path,
                    &concepts[j].source_path,
                ));
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
