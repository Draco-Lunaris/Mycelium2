//! Deterministic mutation fallbacks (moved from mycelium-mcp/src/tools.rs).
//!
//! These run when the librarian agent can't (LLM unreachable, step cap
//! exhausted, age deadline passed) or after the queue's retries: direct
//! concept write on add, dated addendum on update, graph wiring+flagging
//! on maintain. No LLM, no agent — pure store operations with hot-memory
//! coherence hooks.

use mycelium_core::concept::{Concept, Frontmatter};
use mycelium_core::graph;
use mycelium_core::search::SearchQuery;
use mycelium_store::concept_store::{ConceptStore, ConceptStoreError};

/// Deterministic add: direct concept write (the pre-agent behavior of
/// memory_add). Slugified concept at a derived path with collision
/// disambiguation — same content keeps the path (idempotent re-record),
/// different content appends `-2`, `-3`, … instead of clobbering.
pub async fn direct_write_add(
    cs: &ConceptStore<'_>,
    content: &str,
    path_hint: Option<&str>,
    shelf: Option<&str>,
    concept_type: Option<&str>,
) -> Result<String, ConceptStoreError> {
    let base_path = derive_path(path_hint, shelf, content);
    let title = derive_title(content);
    let concept_type = concept_type
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Note");

    // Disambiguate on collision: if the path exists with different
    // content, append -2, -3, … instead of clobbering.
    let mut path = base_path.clone();
    let mut suffix = 2u32;
    loop {
        match cs.get(&path).await {
            Ok(existing) if existing.body.trim() == content.trim() => {
                // Same content: idempotent re-record, keep the path.
                break;
            }
            Ok(_) => {
                let stem = base_path.trim_end_matches(".md");
                path = format!("{stem}-{suffix}.md");
                suffix += 1;
            }
            Err(ConceptStoreError::NotFound(_)) => break,
            Err(e) => return Err(e),
        }
    }

    let concept = Concept::new(
        Frontmatter {
            concept_type: concept_type.to_string(),
            title: Some(title),
            timestamp: Some(now_rfc3339()),
            ..Default::default()
        },
        content.to_string(),
        path,
    );
    cs.put(&concept).await?;
    // Hot memory (v1 parity): the direct write joins the hot set.
    crate::hot_memory::record_hot_write(&cs.scope_id(), &concept.source_path);
    Ok(concept.source_path)
}

/// Deterministic update: dated addendum (the pre-agent behavior of
/// memory_update). Resolve the target concept by explicit path or best
/// search match and append the instruction as a dated addendum —
/// corrections are recorded, not silently rewritten.
pub async fn dated_addendum_update(
    cs: &ConceptStore<'_>,
    instruction: &str,
    path_hint: Option<&str>,
) -> Result<String, ConceptStoreError> {
    // Resolve the target concept: explicit path, or best search match.
    let path = match path_hint {
        Some(p) => canonical(p),
        None => {
            let query = SearchQuery::new(search_terms(instruction));
            let hits = cs.search(&query).await?;
            hits.first()
                .map(|h| h.concept_path.clone())
                .ok_or(ConceptStoreError::NotFound("no matching concept".into()))?
        }
    };

    let mut concept = cs.get(&path).await?;
    // Append the update as a dated addendum (the agent-facing semantic:
    // corrections are recorded, not silently rewritten).
    let stamp = now_rfc3339();
    concept.body = format!(
        "{}\n\n<!-- mycelium2:update:{stamp} -->\n{}\n",
        concept.body.trim_end(),
        instruction
    );
    if let Some(t) = concept.frontmatter.timestamp.as_mut() {
        *t = stamp.clone();
    } else {
        concept.frontmatter.timestamp = Some(stamp);
    }
    cs.put(&concept).await?;
    // Hot memory (v1 parity): the direct write joins the hot set.
    crate::hot_memory::record_hot_write(&cs.scope_id(), &path);
    Ok(path)
}

/// Deterministic maintain: title-overlap wiring + link flagging (the
/// pre-agent behavior of memory_maintain). Wire orphans into the most-
/// related concept so the graph stays connected; flag links to
/// nonexistent concepts. Returns the repair summary (or the healthy
/// string when there is nothing to repair).
pub async fn wire_and_flag_maintain(cs: &ConceptStore<'_>) -> Result<String, ConceptStoreError> {
    let entries = cs.list().await?;
    let mut concepts = Vec::with_capacity(entries.len());
    for entry in &entries {
        if let Ok(c) = cs.get(&entry.path).await {
            concepts.push(c);
        }
    }
    let g = graph::build_graph_from_concepts(&concepts);
    let health = g.health();
    if health.orphan_count == 0 && health.broken_link_count == 0 {
        return Ok(format!(
            "Memory is healthy — {} concepts, {} links, no orphans, no broken links. Nothing to repair.",
            health.concept_count, health.edge_count
        ));
    }

    // Repair 1: wire orphans into the most-related concept (title
    // overlap) so the graph stays connected. Link text strips bracket
    // characters so a hostile title cannot break out of the markdown.
    let mut wired = 0usize;
    for orphan in &g.orphans {
        if let (Some(target), Ok(mut c)) = (best_related(&concepts, orphan), cs.get(orphan).await) {
            let safe_title: String = target
                .1
                .chars()
                .filter(|ch| !matches!(ch, '[' | ']' | '(' | ')'))
                .collect();
            let link = format!("\n\nRelated: [{}](/{})\n", safe_title, target.0);
            if !c.body.contains(&format!("](/{})", target.0)) {
                c.body.push_str(&link);
                if cs.put(&c).await.is_ok() {
                    wired += 1;
                    // Hot memory (v1 parity): the modified concept
                    // joins the hot set.
                    crate::hot_memory::record_hot_write(&cs.scope_id(), &c.source_path);
                }
            }
        }
    }

    // Repair 2: broken links — flag links to nonexistent concepts. The
    // marker goes AFTER the link's closing paren so the markdown link
    // stays well-formed and the scanner still sees the (broken) target
    // on the next health check. (BrokenLink.to carries the leading
    // slash, matching the scanned target verbatim.)
    let mut fixed_links = 0usize;
    for broken in &g.broken_links {
        if let Ok(mut c) = cs.get(&broken.from).await {
            let pattern = format!("]({})", broken.to);
            if let Some(idx) = c.body.find(&pattern) {
                let after = idx + pattern.len();
                c.body.insert_str(after, " <!-- mycelium2:broken-link -->");
                if cs.put(&c).await.is_ok() {
                    fixed_links += 1;
                    // Hot memory (v1 parity): the modified concept
                    // joins the hot set.
                    crate::hot_memory::record_hot_write(&cs.scope_id(), &c.source_path);
                }
            }
        }
    }

    Ok(format!(
        "maintained {} concepts: wired {} orphans, flagged {} broken links \
         (graph: {} concepts, {} edges, {} broken, {} orphans before)",
        entries.len(),
        wired,
        fixed_links,
        health.concept_count,
        health.edge_count,
        health.broken_link_count,
        health.orphan_count,
    ))
}

/// Derive a canonical bundle path for a new concept. Path hints keep
/// their directory structure (`rust/async-basics` → `/rust/async-basics.md`).
fn derive_path(path_hint: Option<&str>, shelf: Option<&str>, content: &str) -> String {
    let slug = path_hint
        .map(path_slug)
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| slugify(&derive_title(content)));
    let shelf_part = shelf
        .map(slugify)
        .filter(|s| !s.is_empty())
        .map(|s| format!("{s}/"))
        .unwrap_or_default();
    format!("/{shelf_part}{slug}.md")
}

/// Slugify a path hint, preserving `/` separators between segments.
fn path_slug(p: &str) -> String {
    p.trim_matches('/')
        .split('/')
        .map(slugify)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// Derive a short title from content (first line, trimmed, capped).
pub fn derive_title(content: &str) -> String {
    let first = content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("untitled");
    let title: String = first.chars().take(80).collect();
    title.trim().to_string()
}

/// Kebab-case a string for use in a bundle path.
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = true;
    for c in s.chars().take(64) {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Cap on search terms per query — the encrypted index issues one lookup
/// per term, so an unbounded term count is a query-amplification DoS
/// vector. 32 terms is far beyond any useful natural-language question.
const MAX_SEARCH_TERMS: usize = 32;

/// Split a natural-language question into capped search terms.
fn search_terms(question: &str) -> Vec<String> {
    question
        .split_whitespace()
        .take(MAX_SEARCH_TERMS)
        .map(|t| t.to_string())
        .collect()
}

/// Canonicalize a user-supplied path to `/foo/bar.md` form.
pub fn canonical(p: &str) -> String {
    let trimmed = p.trim().trim_matches('/');
    if trimmed.is_empty() {
        "/untitled.md".to_string()
    } else if trimmed.ends_with(".md") {
        format!("/{trimmed}")
    } else {
        format!("/{trimmed}.md")
    }
}

/// Find the most-related concept to `path` by title-token overlap.
fn best_related(concepts: &[Concept], path: &str) -> Option<(String, String)> {
    let self_c = concepts.iter().find(|c| c.source_path == path)?;
    let self_tokens = tokenize(&self_c.frontmatter.title.clone().unwrap_or_default());
    let mut best: Option<(f32, String, String)> = None;
    for other in concepts {
        if other.source_path == path {
            continue;
        }
        let other_tokens = tokenize(&other.frontmatter.title.clone().unwrap_or_default());
        let overlap = self_tokens
            .iter()
            .filter(|t| other_tokens.contains(t))
            .count() as f32;
        if overlap > 0.0 && best.as_ref().is_none_or(|(b, _, _)| overlap > *b) {
            let title = other
                .frontmatter
                .title
                .clone()
                .unwrap_or_else(|| other.source_path.clone());
            best = Some((overlap, other.source_path.clone(), title));
        }
    }
    best.map(|(_, p, t)| (p.trim_start_matches('/').to_string(), t))
}

fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_string())
        .collect()
}
