//! MCP tool implementations: memory query/add/update/status/maintain and
//! skill get/list. All tools operate on the calling user's private
//! encrypted bundle; skill tools additionally read the global skills
//! shelf (private first, then global).

use mycelium_core::concept::{Concept, Frontmatter};
use mycelium_core::graph::{self, GraphHealth};
use mycelium_core::search::SearchQuery;
use mycelium_store::concept_store::{ConceptStore, ConceptStoreError};
use schemars::JsonSchema;
use serde::Deserialize;
use uuid::Uuid;

use crate::McpState;
use crate::auth::McpUser;

/// The authenticated caller resolved from the bearer API key (via rmcp's
/// injected `http::request::Parts`).
pub(crate) fn caller(parts: &http::request::Parts) -> Result<McpUser, rmcp::ErrorData> {
    crate::auth::user_from_parts(parts).ok_or_else(|| {
        rmcp::ErrorData::internal_error("no authenticated user on request context", None)
    })
}

/// Open the caller's private concept store.
fn user_store<'a>(
    state: &'a McpState,
    user_id: Uuid,
    master: mycelium_crypto::keys::MasterKey,
) -> ConceptStore<'a> {
    ConceptStore::for_user(&state.store, user_id, master)
}

/// Open the global skills shelf (service-key scope, "skills" namespace).
fn global_skills<'a>(state: &'a McpState) -> ConceptStore<'a> {
    ConceptStore::for_service(
        &state.store,
        (*state.service_key).clone(),
        &state.store.skills_dir(),
        "skills",
    )
}

/// The librarian agent's LLM client from the runtime config (admin-
/// managed via ConfigStore key "llm"; Ollama default).
async fn agent_client(state: &McpState) -> Option<mycelium_librarian::llm::LlmClient> {
    let cfg: Option<mycelium_librarian::llm::LlmConfig> =
        state.config.get("llm").await.ok().flatten();
    let cfg = cfg.unwrap_or_default();
    Some(mycelium_librarian::llm::LlmClient::new(&cfg))
}

/// Render a store error for the caller: expected failures (not found,
/// invalid input) become caller-visible messages; internal failures
/// (database, crypto) are logged and reduced to a generic message so no
/// internal detail leaks to the client.
pub(crate) fn render_store_error(e: &ConceptStoreError) -> String {
    match e {
        ConceptStoreError::NotFound(what) => format!("not found: {what}"),
        ConceptStoreError::Parse(err) => format!("invalid concept: {err}"),
        other => {
            tracing::error!(error = %other, "memory tool internal error");
            "internal storage error".to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// memory_query
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct MemoryQueryArgs {
    /// The natural-language question to answer from the knowledge base.
    #[schemars(description = "Natural-language question to answer from the knowledge base")]
    pub question: String,
    /// Maximum number of results to return (default 10).
    #[schemars(description = "Maximum number of results to return (default 10)")]
    pub limit: Option<usize>,
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

/// The agent's answer to a query: grounded text (the agent embeds a
/// "Sources:" line per the system prompt).
#[derive(serde::Serialize)]
pub struct AgentAnswer {
    pub answer: String,
}

/// Answer a question from the caller's private bundle. The librarian
/// agent (LLM tool-call loop: search → read → synthesize, citing
/// paths) handles the query; when no LLM backend is reachable the
/// deterministic keyword search is the fallback (ranked hits, no
/// synthesis) — the tool never hard-fails on a missing LLM.
pub async fn memory_query(
    state: &McpState,
    user: &McpUser,
    args: &MemoryQueryArgs,
) -> Result<QueryOutput, ConceptStoreError> {
    let master = state
        .master_key_for(user.user_id)
        .await
        .map_err(ConceptStoreError::Db)?;
    let cs = user_store(state, user.user_id, master);

    // Agent path: the v1 architecture — all queries flow through the
    // librarian. Unreachable LLM → deterministic fallback below.
    if let Some(client) = agent_client(state).await {
        match mycelium_librarian::agent::run_query(&client, &cs, &args.question).await {
            Ok(result) => {
                return Ok(QueryOutput::Answer(AgentAnswer {
                    answer: result.answer,
                }));
            }
            Err(e) => {
                tracing::warn!(error = %e, "librarian agent query failed — falling back to search");
            }
        }
    }

    // Deterministic fallback: ranked keyword hits (pre-agent behavior).
    let query = SearchQuery::new(search_terms(&args.question));
    // MCP query stays private by default (DESIGN: web search spans
    // user + global; MCP queries the user bundle only).
    let mut results = cs.search(&query).await?;
    let limit = args.limit.unwrap_or(10).min(100);
    results.truncate(limit);
    Ok(QueryOutput::Hits(results))
}

/// The query tool's output: either the agent's grounded answer or the
/// deterministic search fallback.
#[derive(serde::Serialize)]
#[serde(untagged)]
pub enum QueryOutput {
    Answer(AgentAnswer),
    Hits(Vec<mycelium_core::search::SearchResult>),
}

// ---------------------------------------------------------------------------
// memory_add
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct MemoryAddArgs {
    /// Free-form knowledge to record, in any prose form.
    #[schemars(description = "The knowledge to record, in any prose form")]
    pub content: String,
    /// Optional bundle path hint, e.g. `rust/async-basics`. Derived from
    /// the content when absent.
    #[schemars(description = "Optional bundle path hint (e.g. 'rust/async-basics')")]
    pub path: Option<String>,
    /// Optional shelf name to scope this knowledge under.
    #[schemars(description = "Optional shelf name to scope this knowledge under")]
    pub shelf: Option<String>,
    /// Optional OKF concept type (default `Note`; use `Skill` to record a
    /// skill, `How-To`, `Decision`, etc. for other kinds).
    #[schemars(description = "Optional OKF concept type (default 'Note'; use 'Skill' for skills)")]
    pub concept_type: Option<String>,
}

/// Store new knowledge in the caller's private bundle. The librarian
/// agent (v1 architecture) searches for overlap, then enriches an
/// existing concept or creates a deliberately-placed, two-way-linked
/// new one. Deterministic fallback (no LLM): slugified concept at a
/// derived path with collision disambiguation.
pub async fn memory_add(
    state: &McpState,
    user: &McpUser,
    args: &MemoryAddArgs,
) -> Result<String, ConceptStoreError> {
    let master = state
        .master_key_for(user.user_id)
        .await
        .map_err(ConceptStoreError::Db)?;
    let cs = user_store(state, user.user_id, master);

    // Agent path: wrap the payload as an explicit persist directive
    // (v1 rule — bare content reads as chat and gets answered, not
    // stored).
    if let Some(client) = agent_client(state).await {
        let mut instruction = format!(
            "Persist the following knowledge into the knowledge base. First search for \
             related or owning concepts. If this is an attribute or detail of an \
             existing concept, patch it into that concept rather than creating a new \
             one. Only a distinct stand-alone entity or substantial topic gets its own \
             concept — and then you must also patch the related existing concepts to \
             link back to it. This is content to store, not a message to answer — you \
             must use the write tools.\n\nKNOWLEDGE TO RECORD:\n{}",
            args.content
        );
        if let Some(path) = &args.path {
            instruction.push_str(&format!("\n\nIf it fits, place new content at {path}."));
        }
        if let Some(kind) = &args.concept_type {
            instruction.push_str(&format!(
                "\n\nThe knowledge is of kind/type \"{kind}\" — use it as the concept `type`."
            ));
        }
        match mycelium_librarian::agent::run_mutation(&client, &cs, &instruction).await {
            Ok(result) => {
                return Ok(if result.files_changed.is_empty() {
                    result.summary
                } else {
                    format!(
                        "{}\n\nFiles changed:\n{}",
                        result.summary,
                        result
                            .files_changed
                            .iter()
                            .map(|f| format!("- {f}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "librarian agent add failed — falling back to direct write");
            }
        }
    }

    // Deterministic fallback: direct concept write (pre-agent behavior).
    let base_path = derive_path(&args.path, &args.shelf, &args.content);
    let title = derive_title(&args.content);
    let concept_type = args
        .concept_type
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Note");

    // Disambiguate on collision: if the path exists with different
    // content, append -2, -3, … instead of clobbering.
    let mut path = base_path.clone();
    let mut suffix = 2u32;
    loop {
        match cs.get(&path).await {
            Ok(existing) if existing.body.trim() == args.content.trim() => {
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
        args.content.clone(),
        path,
    );
    cs.put(&concept).await?;
    Ok(concept.source_path)
}

/// Derive a canonical bundle path for a new concept. Path hints keep
/// their directory structure (`rust/async-basics` → `/rust/async-basics.md`).
fn derive_path(path_hint: &Option<String>, shelf: &Option<String>, content: &str) -> String {
    let slug = path_hint
        .as_deref()
        .map(path_slug)
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| slugify(&derive_title(content)));
    let shelf_part = shelf
        .as_deref()
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
fn derive_title(content: &str) -> String {
    let first = content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("untitled");
    let title: String = first.chars().take(80).collect();
    title.trim().to_string()
}

/// Kebab-case a string for use in a bundle path.
fn slugify(s: &str) -> String {
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

// ---------------------------------------------------------------------------
// memory_update
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct MemoryUpdateArgs {
    /// What to change, in natural language (correct a fact, deprecate a
    /// concept, restructure, etc.).
    #[schemars(description = "What to change, in natural language")]
    pub instruction: String,
    /// Optional bundle path of the concept to update. When absent, the
    /// instruction is searched and the best match is updated.
    #[schemars(description = "Optional bundle path of the concept to update")]
    pub path: Option<String>,
}

/// Apply a change to existing knowledge in the caller's private bundle.
/// The librarian agent locates the concepts and applies targeted edits
/// (v1 architecture); deterministic fallback (no LLM): resolve the
/// target by path or best search match and append a dated addendum.
pub async fn memory_update(
    state: &McpState,
    user: &McpUser,
    args: &MemoryUpdateArgs,
) -> Result<String, ConceptStoreError> {
    let master = state
        .master_key_for(user.user_id)
        .await
        .map_err(ConceptStoreError::Db)?;
    let cs = user_store(state, user.user_id, master);

    // Agent path: the instruction is a change directive.
    if let Some(client) = agent_client(state).await {
        let mut instruction = format!(
            "Apply the following change to the knowledge base. Locate the concept(s) \
             the change concerns (search first, read them), then apply targeted edits \
             with the write tools. This is a change to apply, not a message to answer.\n\n\
             CHANGE TO APPLY:\n{}",
            args.instruction
        );
        if let Some(path) = &args.path {
            instruction.push_str(&format!("\n\nThe target concept is at {path}."));
        }
        match mycelium_librarian::agent::run_mutation(&client, &cs, &instruction).await {
            Ok(result) => {
                return Ok(if result.files_changed.is_empty() {
                    result.summary
                } else {
                    format!(
                        "{}\n\nFiles changed:\n{}",
                        result.summary,
                        result
                            .files_changed
                            .iter()
                            .map(|f| format!("- {f}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "librarian agent update failed — falling back to addendum");
            }
        }
    }

    // Deterministic fallback: dated addendum (pre-agent behavior).
    // Resolve the target concept: explicit path, or best search match.
    let path = match &args.path {
        Some(p) => canonical(p),
        None => {
            let query = SearchQuery::new(search_terms(&args.instruction));
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
        args.instruction
    );
    if let Some(t) = concept.frontmatter.timestamp.as_mut() {
        *t = stamp.clone();
    } else {
        concept.frontmatter.timestamp = Some(stamp);
    }
    cs.put(&concept).await?;
    Ok(path)
}

/// Canonicalize a user-supplied path to `/foo/bar.md` form.
fn canonical(p: &str) -> String {
    let trimmed = p.trim().trim_matches('/');
    if trimmed.is_empty() {
        "/untitled.md".to_string()
    } else if trimmed.ends_with(".md") {
        format!("/{trimmed}")
    } else {
        format!("/{trimmed}.md")
    }
}

// ---------------------------------------------------------------------------
// memory_status
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct MemoryStatusArgs {}

/// Report the health of the caller's private bundle: concept count,
/// edges, broken links, orphans.
pub async fn memory_status(
    state: &McpState,
    user: &McpUser,
) -> Result<GraphHealth, ConceptStoreError> {
    let master = state
        .master_key_for(user.user_id)
        .await
        .map_err(ConceptStoreError::Db)?;
    let cs = user_store(state, user.user_id, master);
    let entries = cs.list().await?;
    let mut concepts = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Ok(c) = cs.get(&entry.path).await {
            concepts.push(c);
        }
    }
    let g = graph::build_graph_from_concepts(&concepts);
    Ok(g.health())
}

// ---------------------------------------------------------------------------
// memory_maintain
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct MemoryMaintainArgs {}

/// Health-check and repair the caller's bundle graph: wire orphaned
/// concepts into related concepts and fix broken links. The librarian
/// agent performs the repairs (v1 architecture — it reads the orphans
/// and wires them into genuinely related concepts); deterministic
/// fallback (no LLM): title-overlap wiring + broken-link flagging.
pub async fn memory_maintain(
    state: &McpState,
    user: &McpUser,
) -> Result<String, ConceptStoreError> {
    let master = state
        .master_key_for(user.user_id)
        .await
        .map_err(ConceptStoreError::Db)?;
    let cs = user_store(state, user.user_id, master);
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

    // Agent path: hand the orphans + broken links to the librarian
    // (v1's maintain instruction).
    if let Some(client) = agent_client(state).await {
        let orphan_list = g
            .orphans
            .iter()
            .map(|o| format!("- {o}"))
            .collect::<Vec<_>>()
            .join("\n");
        let broken_list = g
            .broken_links
            .iter()
            .map(|b| format!("- {} → {} (missing)", b.from, b.to))
            .collect::<Vec<_>>()
            .join("\n");
        let instruction = format!(
            "Repair the knowledge graph. This is a maintenance task — use the write tools.\n\n\
             ORPHANED CONCEPTS (no other concept links to them). For each, read it and the \
             concepts it relates to, then wire it in: patch a genuinely related concept to \
             reference it, and/or add outbound links from it to related concepts. Do NOT \
             invent relationships that don't exist — if an orphan genuinely relates to \
             nothing, leave it.\n{}\n\n\
             BROKEN LINKS (target does not exist). Fix the path if the target was renamed/moved, \
             or remove the link if the target is gone.\n{}\n\n\
             Follow the enrich / link-both-ways rules. Read concepts before editing.",
            if orphan_list.is_empty() {
                "(none)".to_string()
            } else {
                orphan_list
            },
            if broken_list.is_empty() {
                "(none)".to_string()
            } else {
                broken_list
            },
        );
        match mycelium_librarian::agent::run_mutation(&client, &cs, &instruction).await {
            Ok(result) => {
                // Re-measure after the agent's repairs.
                let entries2 = cs.list().await?;
                let mut concepts2 = Vec::with_capacity(entries2.len());
                for entry in &entries2 {
                    if let Ok(c) = cs.get(&entry.path).await {
                        concepts2.push(c);
                    }
                }
                let after = graph::build_graph_from_concepts(&concepts2).health();
                return Ok(format!(
                    "{}\n\nGraph health: orphans {} → {}, broken links {} → {}.",
                    result.summary,
                    health.orphan_count,
                    after.orphan_count,
                    health.broken_link_count,
                    after.broken_link_count,
                ));
            }
            Err(e) => {
                tracing::warn!(error = %e, "librarian agent maintain failed — falling back to deterministic repair");
            }
        }
    }

    // Deterministic fallback: title-overlap wiring + link flagging.
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

// ---------------------------------------------------------------------------
// skill_get / skill_list
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct SkillGetArgs {
    /// The skill name to fetch (e.g. `deploy-rust-service`).
    #[schemars(description = "The skill name to fetch")]
    pub name: String,
}

/// Fetch a skill by name — the caller's private skills first, then the
/// global skills shelf. Returns the skill's full markdown.
pub async fn skill_get(
    state: &McpState,
    user: &McpUser,
    args: &SkillGetArgs,
) -> Result<Option<Concept>, ConceptStoreError> {
    let master = state
        .master_key_for(user.user_id)
        .await
        .map_err(ConceptStoreError::Db)?;
    let cs = user_store(state, user.user_id, master);
    let name = slugify(&args.name);
    let path = format!("/{name}.md");

    // Private first. A NotFound falls through to the global shelf; any
    // other error (corruption, crypto) propagates rather than masking.
    match cs.get(&path).await {
        Ok(c) if c.frontmatter.concept_type == "Skill" => return Ok(Some(c)),
        Ok(_) => {}
        Err(ConceptStoreError::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    // Then global.
    let global = global_skills(state);
    match global.get(&path).await {
        Ok(c) if c.frontmatter.concept_type == "Skill" => return Ok(Some(c)),
        Ok(_) => {}
        Err(ConceptStoreError::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    Ok(None)
}

#[derive(Deserialize, JsonSchema)]
pub struct SkillListArgs {}

/// List available skills: the caller's private skills plus global skills.
pub async fn skill_list(
    state: &McpState,
    user: &McpUser,
) -> Result<Vec<mycelium_store::concept_store::ConceptEntry>, ConceptStoreError> {
    let master = state
        .master_key_for(user.user_id)
        .await
        .map_err(ConceptStoreError::Db)?;
    let cs = user_store(state, user.user_id, master);
    let mut skills = cs
        .list()
        .await?
        .into_iter()
        .filter(|e| e.concept_type == "Skill")
        .collect::<Vec<_>>();
    let global = global_skills(state);
    let mut global_skills = global
        .list()
        .await?
        .into_iter()
        .filter(|e| e.concept_type == "Skill")
        .collect::<Vec<_>>();
    skills.append(&mut global_skills);
    skills.sort_by(|a, b| a.path.cmp(&b.path));
    skills.dedup_by(|a, b| a.path == b.path);
    Ok(skills)
}
