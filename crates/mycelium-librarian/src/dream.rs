//! Dreaming (v1 parity): an autonomous consolidation pass over a
//! user's memory — what a brain does during sleep. Deterministic
//! signals decide whether there is anything to dream about — the four
//! v1 signals (orphans, broken links, likely duplicates, oversized
//! concepts) plus the lifecycle signals (stale pairings, expired
//! valid_to, broken supersedes, disorganized patch chains); the agent
//! then consolidates. No signals → no run, no tokens.
//!
//! Two phases (Ruling 3): the main-pass signals run the agent through
//! `store`; consolidation signals run a second, consolidation-only pass
//! through `cons_store` — the same scope viewed with the explicit
//! `consolidate` revision op, so body rewrites record "consolidated
//! body history" while every other write stays mechanically tagged.
//!
//! The web layer exposes it as an admin/user-triggered maintenance
//! action (v1 ran it on a cron-like schedule; the trigger wiring is
//! the caller's choice).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};

use mycelium_store::revisions::{consolidation_patches, dedup_cosine, recent_window, stale_days};
use mycelium_store::vectors::VectorStore;
use mycelium_store::{ConceptStore, ConceptStoreError};

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
///
/// `cons_store` is the SAME scope as `store`, built with
/// `.with_revision_op("consolidate")`: when consolidation signals
/// exist, their pass runs through it so its body rewrites record op
/// `consolidate` while `store`'s writes stay mechanically derived.
/// Both passes ride the ONE gate permit the caller already holds —
/// this entry acquires nothing (the caller's admission; Ruling 3).
pub async fn run_dream(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    cons_store: &ConceptStore<'_>,
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

    // The lifecycle signals (spec §3.4), appended after the v1 four in
    // FIXED order. All date work is on one `now` per pass.
    let now = Utc::now();
    let stale = stale_pairs(&concepts, now);
    if !stale.is_empty() {
        signals.push(format!(
            "STALE CONCEPTS (unchanged {} days, wording drifts). For each pair: read both; if \
             the recent concept covers the same ground, supersede the old one (status: \
             superseded + superseded_by pointing at the replacement) or retire it (status: \
             retired); if the old one still stands alone, leave it as is:\n{}",
            stale_days(),
            stale
                .iter()
                .map(|(old, overlappers)| format!(
                    "- {old} overlapped by {}",
                    overlappers.join(", ")
                ))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    let expired = expired_concepts(&concepts, now);
    if !expired.is_empty() {
        signals.push(format!(
            "EXPIRED CONCEPTS (valid_to passed). For each: retire it (status: retired) or move \
             its valid_to forward if the fact still holds:\n{}",
            expired
                .iter()
                .map(|p| format!("- {p}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    // A superseded_by target is broken iff the path is not in this
    // scope — the registry lists every existing path (any status: a
    // retired target still exists).
    let existing: HashSet<String> = entries.iter().map(|e| e.path.clone()).collect();
    let supersede_broken = broken_supersede(&concepts, &existing);
    if !supersede_broken.is_empty() {
        signals.push(format!(
            "BROKEN SUPERSEDE (target missing). For each: point superseded_by at the real \
             replacement path if it moved, or clear the supersede (frontmatter \
             {{\"superseded_by\": null, \"status\": null}}) if there is none:\n{}",
            supersede_broken
                .iter()
                .map(|(from, to)| format!("- {from} → {to}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    // The consolidation (intra-concept) signals ride a SECOND pass:
    // their rewrites must record op `consolidate` through `cons_store`
    // (Ruling 3), so they never join the main instruction.
    let revisions = store.revisions();
    let mut counts: Vec<(String, u32)> = Vec::with_capacity(concepts.len());
    for c in &concepts {
        let token = revisions.path_token_of(&c.source_path);
        let n = revisions
            .consecutive_update_count(&token)
            .await
            .map_err(ConceptStoreError::Revisions)?;
        counts.push((c.source_path.clone(), n));
    }
    let cluttered = disorganized(&counts);
    let cons_section = if cluttered.is_empty() {
        None
    } else {
        Some(format!(
            "DISORGANIZED CONCEPTS (many consecutive patch revisions). For each: rewrite the \
             body as a current-state summary, moving superseded/older detail under a final \
             '# History' section (history text is excluded from search). Use patch_concept \
             with the `body` argument — this pass's patches record as `consolidate` \
             revisions:\n{}",
            cluttered
                .iter()
                .map(|(path, n)| format!("- {path} ({n} consecutive patch revisions)"))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    };

    if signals.is_empty() && cons_section.is_none() {
        return Ok(DreamReport {
            ran: false,
            reason: Some(
                "no signals: no orphans, broken links, duplicates, oversized concepts, stale \
                 pairs, expired concepts, broken supersedes, or disorganized concepts"
                    .into(),
            ),
            summary: None,
            files_changed: None,
        });
    }

    // Hand each pass's signals to the agent as its mutation
    // instruction. A pass with no signals does not run (no-signals, no
    // tokens — also why a consolidation-only dream skips the empty
    // main instruction).
    let mut summary: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    if !signals.is_empty() {
        let instruction = format!(
            "This is a scheduled maintenance pass over the knowledge base (a 'dream'). Address each \
             signal below. Work through them in order; when done, summarize exactly what changed.\n\n{}",
            signals.join("\n\n")
        );
        let result = agent::run_mutation(client, store, scopes, &instruction).await?;
        summary.push(result.summary);
        files.extend(result.files_changed);
    }
    if let Some(cons_section) = cons_section {
        let instruction = format!(
            "This is the consolidation pass of a scheduled maintenance pass over the knowledge \
             base (a 'dream'): rewrite each listed concept's body with patch_concept's `body` \
             argument (whole-body replace). Work through them in order; when done, summarize \
             exactly what changed.\n\n{}",
            cons_section
        );
        let result = agent::run_mutation(client, cons_store, scopes, &instruction).await?;
        summary.push(result.summary);
        files.extend(result.files_changed);
    }
    files.sort();
    files.dedup();
    Ok(DreamReport {
        ran: true,
        reason: None,
        summary: Some(summary.join("\n\n")),
        files_changed: Some(files),
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

/// §3.0's lenient read of a concept's lifecycle status: absent or
/// unknown is active (matching retrieval's read-side); only the exact
/// lowercase `superseded`/`retired` exclude a concept — "dying" is
/// superseded, retired is retired.
fn is_active(c: &mycelium_core::Concept) -> bool {
    !matches!(
        c.frontmatter.status.as_deref(),
        Some("superseded") | Some("retired")
    )
}

/// RFC 3339 parser for the lifecycle timestamps. Absent or unparsable
/// → None (write-time validation makes that rare: only legacy rows can
/// carry odd values).
fn parse_stamp(value: Option<&str>) -> Option<DateTime<Utc>> {
    let stamp = chrono::DateTime::parse_from_rfc3339(value?).ok()?;
    Some(stamp.with_timezone(&Utc))
}

/// A concept's confirmation age at `now` — `confirmed_at`, with the
/// legacy `timestamp` fallback (§3.4; `timestamp` predates the
/// lifecycle fields and was stamped at create from the start, so an
/// unconfirmable read is confined to truly malformed rows).
fn confirmed_age(c: &mycelium_core::Concept, now: DateTime<Utc>) -> Option<Duration> {
    let stamp = parse_stamp(
        c.frontmatter
            .confirmed_at
            .as_deref()
            .or(c.frontmatter.timestamp.as_deref()),
    )?;
    Some(now.signed_duration_since(stamp))
}

/// The concept's title/tag token set — the staleness pairing's overlap
/// substrate (spec: "title/tag tokens"); lowercased and deduped, empty
/// when the concept says nothing to overlap on.
fn title_tag_tokens(c: &mycelium_core::Concept) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Some(title) = &c.frontmatter.title {
        out.extend(mycelium_core::search::tokenize(&title.to_lowercase()));
    }
    for tag in &c.frontmatter.tags {
        out.extend(mycelium_core::search::tokenize(&tag.to_lowercase()));
    }
    out
}

/// The staleness signal (spec §3.4): for each ACTIVE concept older than
/// [`stale_days`] whose title/tag token set non-emptily overlaps at
/// least one OTHER ACTIVE concept confirmed within the last
/// [`recent_window`], the old path paired with its recent overlappers.
/// Both sides read `confirmed_at` with the `timestamp` fallback; an
/// undateable concept is neither stale nor recent (the signal's premise
/// cannot be established). Retired/dying concepts are excluded on BOTH
/// sides — they are already decisions, not drift. Output: old paths
/// asc, each with its overlappers path asc — deterministic regardless
/// of input order.
fn stale_pairs(
    concepts: &[mycelium_core::Concept],
    now: DateTime<Utc>,
) -> Vec<(String, Vec<String>)> {
    let stale_bound = Duration::days(i64::from(stale_days()));
    let recent_bound = Duration::days(i64::from(recent_window()));
    // A concept may legitimately be both sides (configs where
    // stale_days ≤ recent_window): collect the two sides independently.
    let mut older: Vec<(&str, HashSet<String>)> = Vec::new();
    let mut newer: Vec<(&str, HashSet<String>)> = Vec::new();
    for c in concepts {
        if !is_active(c) {
            continue;
        }
        let Some(age) = confirmed_age(c, now) else {
            continue;
        };
        let tokens = title_tag_tokens(c);
        if tokens.is_empty() {
            continue;
        }
        if age > stale_bound {
            older.push((c.source_path.as_str(), tokens.clone()));
        }
        if age <= recent_bound {
            newer.push((c.source_path.as_str(), tokens));
        }
    }
    let mut out: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (old_path, old_tokens) in &older {
        let hits: BTreeSet<&str> = newer
            .iter()
            .filter(|(path, _)| *path != *old_path)
            .filter(|(_, tokens)| !tokens.is_disjoint(old_tokens))
            .map(|(path, _)| *path)
            .collect();
        if !hits.is_empty() {
            out.insert(old_path, hits);
        }
    }
    out.into_iter()
        .map(|(old, hits)| {
            (
                old.to_string(),
                hits.into_iter().map(|h| h.to_string()).collect(),
            )
        })
        .collect()
}

/// The expiry signal (spec §3.4): ACTIVE concepts whose valid_to lies
/// strictly in the past (an unparsable or absent valid_to never fires).
/// Output path asc, deterministic.
fn expired_concepts(concepts: &[mycelium_core::Concept], now: DateTime<Utc>) -> Vec<String> {
    concepts
        .iter()
        .filter(|c| is_active(c))
        .filter_map(|c| {
            let valid_to = parse_stamp(c.frontmatter.valid_to.as_deref())?;
            (valid_to < now).then(|| c.source_path.clone())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The repair-supersede signal (spec §3.4): ACTIVE concepts whose
/// `superseded_by` names a path absent from the scope — the chain the
/// field claims is broken. `existing` lists every path in the scope
/// (any status: a retired or superseded target still exists, so only a
/// truly missing path repair-alarms). Output (path, target) pairs,
/// path asc, deterministic.
fn broken_supersede(
    concepts: &[mycelium_core::Concept],
    existing: &HashSet<String>,
) -> Vec<(String, String)> {
    concepts
        .iter()
        .filter(|c| is_active(c))
        .filter_map(|c| {
            let target = c.frontmatter.superseded_by.as_deref()?;
            (!existing.contains(target)).then(|| (c.source_path.clone(), target.to_string()))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The intra-concept consolidation signal (spec §3.4, Ruling 2): a
/// concept whose trailing-`update` chain STRICTLY exceeds
/// [`consolidation_patches`] — equal fires nothing (the count tops out
/// at patches+1, the exact signal boundary). `counts` carries (path,
/// trailing update count) per concept. Output path asc, deterministic.
fn disorganized(counts: &[(String, u32)]) -> Vec<(String, u32)> {
    let limit = consolidation_patches();
    counts
        .iter()
        .filter(|(_, n)| *n > limit)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmConfig;
    use chrono::Duration;

    fn base_fm() -> mycelium_core::concept::Frontmatter {
        mycelium_core::concept::Frontmatter {
            concept_type: "Note".into(),
            ..Default::default()
        }
    }

    fn concept(path: &str, fm: mycelium_core::concept::Frontmatter) -> mycelium_core::Concept {
        mycelium_core::Concept::new(fm, "body".into(), path.into())
    }

    /// A stale concept whose tokens overlap a fresh one, built from the
    /// pieces each case tunes (the fresh overlap carries the same title
    /// token unless the case overrides it).
    fn stamped_concept(
        path: &str,
        title: &str,
        status: Option<&str>,
        confirmed_at: Option<String>,
        timestamp: Option<String>,
    ) -> mycelium_core::Concept {
        let mut fm = base_fm();
        fm.title = Some(title.into());
        fm.status = status.map(String::from);
        fm.confirmed_at = confirmed_at;
        fm.timestamp = timestamp;
        concept(path, fm)
    }

    /// The staleness pairing (§3.4): an old active concept pairs ONLY
    /// with an ACTIVE recent concept whose title/tag tokens overlap —
    /// fresh pairs, retired/dying exclusions on BOTH sides, and the
    /// call's determinism (unsorted input → path-asc output; repeated
    /// call, same order).
    #[test]
    fn stale_paired_only_with_recent_overlappers() {
        let now = Utc::now();
        let old_stamp = (now - Duration::days(400)).to_rfc3339();
        let fresh_stamp = (now - Duration::days(2)).to_rfc3339();
        let mid_stamp = (now - Duration::days(30)).to_rfc3339();

        let old = stamped_concept(
            "/old/quantum.md",
            "Quantum Flux",
            None,
            Some(old_stamp.clone()),
            None,
        );
        let recent = stamped_concept(
            "/recent/quantum.md",
            "Quantum Mirror",
            None,
            Some(fresh_stamp.clone()),
            None,
        );
        // 30 days: outside the 14-day recent window, inside the stale
        // bound — never a recent side.
        let mid = stamped_concept(
            "/mid/quantum.md",
            "Quantum Reborn",
            None,
            Some(mid_stamp.clone()),
            None,
        );

        // The pair fires; the not-recent sibling does not join.
        let pairs = stale_pairs(&[recent.clone(), mid.clone(), old.clone()], now);
        assert_eq!(
            pairs,
            vec![(
                "/old/quantum.md".to_string(),
                vec!["/recent/quantum.md".to_string()]
            )],
            "token overlap + both sides dateable pairs the old concept"
        );

        // No token overlap at all → no pair.
        let stranger = stamped_concept(
            "/recent/stranger.md",
            "Entanglement Probe",
            None,
            Some(fresh_stamp.clone()),
            None,
        );
        assert!(
            stale_pairs(&[old.clone(), stranger], now).is_empty(),
            "disjoint tokens never pair"
        );

        // Old but no recent overlapper (the 30-day sibling is not
        // fresh enough to be the recent side).
        assert!(
            stale_pairs(&[old.clone(), mid.clone()], now).is_empty(),
            "a non-recent token sibling is no recent side"
        );

        // Everyone fresh → no stale side at all.
        assert!(
            stale_pairs(&[recent.clone(), mid.clone()], now).is_empty(),
            "no concept older than stale_days: no pairs"
        );

        // Retired/dying on the RECENT side: excluded.
        let retired_recent = stamped_concept(
            "/recent/retired.md",
            "Quantum Retirement",
            Some("retired"),
            Some(fresh_stamp.clone()),
            None,
        );
        let dying_recent = stamped_concept(
            "/recent/dying.md",
            "Quantum Legacy",
            Some("superseded"),
            Some(fresh_stamp.clone()),
            None,
        );
        assert!(
            stale_pairs(&[old.clone(), retired_recent, dying_recent], now).is_empty(),
            "retired/dying overlappers never pair"
        );

        // Retired/dying on the OLD side: excluded too.
        let old_retired = stamped_concept(
            "/old/retired.md",
            "Quantum Flux",
            Some("retired"),
            Some(old_stamp.clone()),
            None,
        );
        let old_dying = stamped_concept(
            "/old/dying.md",
            "Quantum Flux",
            Some("superseded"),
            Some(old_stamp.clone()),
            None,
        );
        assert!(
            stale_pairs(&[old_retired, old_dying, recent.clone()], now).is_empty(),
            "retired/dying old concepts never pair"
        );

        // The legacy `timestamp` fallback carries either side (§3.4:
        // confirmed_at with the timestamp fallback).
        let old_ts = stamped_concept(
            "/old/ts-fallback.md",
            "Quantum Flux",
            None,
            None,
            Some(old_stamp.clone()),
        );
        let recent_ts = stamped_concept(
            "/recent/ts-fallback.md",
            "Quantum Echo",
            None,
            Some(fresh_stamp.clone()),
            None,
        );
        assert_eq!(
            stale_pairs(&[old_ts, recent_ts], now),
            vec![(
                "/old/ts-fallback.md".to_string(),
                vec!["/recent/ts-fallback.md".to_string()]
            )],
            "the timestamp fallback dates a confirmed_at-less concept"
        );

        // An undateable concept is never the old side (the premise
        // cannot be established).
        let undateable = stamped_concept("/old/undateable.md", "Quantum Flux", None, None, None);
        assert!(
            stale_pairs(&[undateable, recent.clone()], now).is_empty(),
            "no stamp, no staleness signal"
        );

        // Tag tokens participate: disjoint titles, one shared tag.
        let mut tag_old = base_fm();
        tag_old.title = Some("Distant Horizon".into());
        tag_old.tags = vec!["quantum".into()];
        tag_old.confirmed_at = Some(old_stamp.clone());
        let mut tag_recent = base_fm();
        tag_recent.title = Some("Unrelated Naming".into());
        tag_recent.tags = vec!["quantum".into()];
        tag_recent.confirmed_at = Some(fresh_stamp.clone());
        assert_eq!(
            stale_pairs(
                &[
                    concept("/old/tagged.md", tag_old),
                    concept("/recent/tagged.md", tag_recent)
                ],
                now
            ),
            vec![(
                "/old/tagged.md".to_string(),
                vec!["/recent/tagged.md".to_string()]
            )],
            "tag tokens overlap like title tokens"
        );

        // Determinism: two recent overlappers, scrambled input order,
        // output stable path-asc — and a repeated call agrees.
        let r1 = stamped_concept(
            "/recent/alpha.md",
            "Quantum One",
            None,
            Some(fresh_stamp.clone()),
            None,
        );
        let r2 = stamped_concept(
            "/recent/beta.md",
            "Quantum Two",
            None,
            Some(fresh_stamp.clone()),
            None,
        );
        let first = stale_pairs(&[r2.clone(), old.clone(), r1.clone()], now);
        let second = stale_pairs(&[old.clone(), r1.clone(), r2.clone()], now);
        assert_eq!(first, second, "repeated call, same output");
        assert_eq!(
            first,
            vec![(
                "/old/quantum.md".to_string(),
                vec![
                    "/recent/alpha.md".to_string(),
                    "/recent/beta.md".to_string()
                ]
            )],
            "overlappers and old paths both sort path-asc"
        );
    }

    /// Expiry (§3.4): an ACTIVE concept whose valid_to is in the past
    /// fires; future/absent/unparsable expiry stays silent, and a
    /// retired concept is never expired-active. Output path-asc, stable
    /// across calls.
    #[test]
    fn expired_concepts_signal_on_elapsed_valid_to() {
        let now = Utc::now();
        let elapsed = (now - Duration::days(1)).to_rfc3339();
        let future = (now + Duration::days(30)).to_rfc3339();

        let mut lapsed = base_fm();
        lapsed.valid_to = Some(elapsed.clone());
        let lapsed = concept("/expiry/lapsed.md", lapsed);

        let mut upcoming = base_fm();
        upcoming.valid_to = Some(future);
        let upcoming = concept("/expiry/upcoming.md", upcoming);

        let missing = concept("/expiry/missing.md", base_fm());

        let mut bogus = base_fm();
        bogus.valid_to = Some("not-a-date".into());
        let bogus = concept("/expiry/bogus.md", bogus);

        let mut retired = base_fm();
        retired.status = Some("retired".into());
        retired.valid_to = Some(elapsed.clone());
        let retired = concept("/expiry/retired.md", retired);

        let mut second = base_fm();
        second.valid_to = Some(elapsed.clone());
        let second = concept("/expiry/aa-second.md", second);

        assert_eq!(
            expired_concepts(
                &[
                    lapsed.clone(),
                    upcoming,
                    missing,
                    bogus,
                    retired,
                    second.clone()
                ],
                now
            ),
            vec![
                "/expiry/aa-second.md".to_string(),
                "/expiry/lapsed.md".to_string()
            ],
            "only elapsed valid_to on active concepts fires, path-asc"
        );
        // Determinism: repeated call, same order.
        assert_eq!(
            expired_concepts(&[second, lapsed], now),
            vec![
                "/expiry/aa-second.md".to_string(),
                "/expiry/lapsed.md".to_string()
            ]
        );
    }

    /// Repair-supersede (§3.4): an ACTIVE concept whose superseded_by
    /// target is absent from the scope fires; an existing (restored)
    /// target and an absent field stay silent. Output path-asc, stable
    /// across calls.
    #[test]
    fn broken_supersede_signal_on_missing_target() {
        let mut broken = base_fm();
        broken.superseded_by = Some("/gone.md".into());
        let broken = concept("/supersede/broken.md", broken);

        let mut pointing = base_fm();
        pointing.superseded_by = Some("/real.md".into());
        let pointing = concept("/supersede/pointing.md", pointing);

        let missing = concept("/supersede/missing.md", base_fm());

        let mut broken_retired = base_fm();
        broken_retired.status = Some("retired".into());
        broken_retired.superseded_by = Some("/also-gone.md".into());
        let broken_retired = concept("/supersede/retired.md", broken_retired);

        let mut second = base_fm();
        second.superseded_by = Some("/aa-gone.md".into());
        let second = concept("/supersede/aa-broken.md", second);

        // Nothing exists — every named target is absent, including
        // /real.md (the scope is empty): all three active chains fire,
        // path-asc. The retired one stays silent.
        let empty: HashSet<String> = HashSet::new();
        let got = broken_supersede(
            &[
                broken,
                pointing.clone(),
                missing,
                broken_retired,
                second.clone(),
            ],
            &empty,
        );
        assert_eq!(
            got,
            vec![
                (
                    "/supersede/aa-broken.md".to_string(),
                    "/aa-gone.md".to_string()
                ),
                ("/supersede/broken.md".to_string(), "/gone.md".to_string()),
                ("/supersede/pointing.md".to_string(), "/real.md".to_string()),
            ],
            "missing targets fire on active concepts only, path-asc"
        );
        // Determinism: repeated call, same order.
        assert_eq!(broken_supersede(&[second], &empty), vec![got[0].clone()]);

        // The target EXISTS (restored): chain intact → no signal for it
        // (the empty-scope run above fired the same chain).
        let existing: HashSet<String> = HashSet::from(["/real.md".to_string()]);
        let restored = broken_supersede(&[pointing], &existing);
        assert!(
            restored.is_empty(),
            "an existing supersede target is no broken chain"
        );
    }

    /// The consolidation threshold (Ruling 2): the trailing-update
    /// count must STRICTLY exceed consolidation_patches — equal fires
    /// nothing, patches+1 fires, and below stays silent. Deterministic
    /// path-asc output regardless of input order.
    #[test]
    fn consolidation_signal_strictly_exceeds_patches() {
        let patches = mycelium_store::revisions::consolidation_patches();
        let counts = vec![
            ("/zz-at-threshold.md".to_string(), patches),
            ("/aa-over-threshold.md".to_string(), patches + 1),
            ("/mm-under-threshold.md".to_string(), patches - 1),
        ];
        let got = disorganized(&counts);
        assert_eq!(
            got,
            vec![("/aa-over-threshold.md".to_string(), patches + 1)],
            "== fires nothing, > fires, < stays silent; path-asc"
        );
        // Scrambled input, same output.
        let scrambled = vec![counts[2].clone(), counts[0].clone(), counts[1].clone()];
        assert_eq!(disorganized(&scrambled), got);
    }

    /// The no-signals early return (zero total signals) names every
    /// signal family the dream watches — the old four plus the new
    /// staleness/expiry/supersede/consolidation families.
    #[tokio::test]
    async fn no_signals_reason_names_all_signal_families() {
        let dir = tempfile::tempdir().unwrap();
        let store = mycelium_store::Store::open(dir.path()).await.unwrap();
        let master = mycelium_crypto::generate_master_key();
        let cs = ConceptStore::for_user(&store, uuid::Uuid::new_v4(), master);
        let llm = LlmClient::new(&LlmConfig::default());
        let scopes = AgentScopes {
            skills: None,
            library: None,
            embeddings: None,
            read_stack_text: None,
            visible_slugs: None,
            trace: None,
        };
        let report = run_dream(&llm, &cs, &cs, &scopes, None).await.unwrap();
        assert!(!report.ran, "an empty scope has nothing to dream about");
        let reason = report.reason.expect("a skipped run explains itself");
        for family in [
            "orphans",
            "broken links",
            "duplicates",
            "oversized",
            "stale",
            "expired",
            "supersede",
            "disorganized",
        ] {
            assert!(reason.contains(family), "reason names {family}: {reason}");
        }
    }
}
