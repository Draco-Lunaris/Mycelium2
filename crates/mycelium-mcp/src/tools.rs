//! MCP tool implementations: memory query/add/update/status/maintain and
//! skill get/list. All tools operate on the calling user's private
//! encrypted bundle; skill tools additionally read the global skills
//! shelf (private first, then global).

use mycelium_core::concept::{Concept, Frontmatter};
use mycelium_core::graph::{self, GraphHealth};
use mycelium_core::search::SearchQuery;
use mycelium_store::concept_store::{ConceptStore, ConceptStoreError};
use mycelium_store::file_repo::{FileRepo, Scope};
use mycelium_store::models::{QueueStatus, QueueTool};
use mycelium_store::mutation_queue::{self, MutationPayload};
use mycelium_store::{ConceptEntry, group_skills};
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

/// Construct the agent's cross-scope capabilities (global skills, library,
/// and full-text book search), filtered by the caller's bookshelf
/// visibility: admins are unrestricted, others see only global-read
/// shelves' books.
async fn agent_scopes<'a>(
    state: &'a McpState,
    user: &McpUser,
) -> mycelium_librarian::agent::AgentScopes<'a> {
    let store = state.store.clone();
    let service_key = (*state.service_key).clone();
    let read_stack_text: Option<Box<mycelium_librarian::agent::StackTextReader>> =
        Some(Box::new(move |slug| {
            let store = store.clone();
            let service_key = service_key.clone();
            let slug = slug.to_string();
            Box::pin(async move {
                mycelium_librarian::read_stack_text(&store, &service_key, &slug)
                    .await
                    .map_err(|e| e.to_string())
            })
        }));
    let visible_slugs = if user.role == mycelium_auth::rbac::Role::Admin {
        None
    } else {
        let mut out = std::collections::HashSet::new();
        if let Ok(shelves) = state.store.list_bookshelves_detailed().await {
            for (id, _name, global_read) in shelves {
                if !global_read {
                    continue;
                }
                if let Ok(books) = state.store.books_on_shelf(id).await {
                    for (slug, _title) in books {
                        out.insert(slug);
                    }
                }
            }
        }
        Some(out)
    };
    mycelium_librarian::agent::AgentScopes {
        skills: Some(global_skills(state)),
        library: Some(ConceptStore::for_service(
            &state.store,
            (*state.service_key).clone(),
            &state.store.library_dir(),
            "library",
        )),
        read_stack_text,
        visible_slugs,
        trace: Some(mycelium_librarian::agent::TraceSink {
            pool: state.store.pool().clone(),
            scope_id: format!("user:{}", user.user_id),
        }),
    }
}

/// The librarian agent's LLM client from the runtime config (admin-
/// managed via ConfigStore key "llm"; Ollama default). Sealed rows
/// decrypt under the service key; legacy plaintext rows read as before.
async fn agent_client(state: &McpState) -> Option<mycelium_librarian::llm::LlmClient> {
    let cfg = mycelium_store::config::get_sealed::<mycelium_librarian::llm::LlmConfig>(
        &state.config,
        &state.service_key,
        "llm",
    )
    .await
    .unwrap_or_default();
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
        // Capacity is expected backpressure under a full queue, not an
        // internal failure — the caller sees a plain retry hint.
        ConceptStoreError::Queue(mycelium_store::QueueError::Capacity) => {
            "queue is at capacity — retry later".to_string()
        }
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
    // The cached wrapper (v1 parity) answers identical repeats from
    // the fingerprint-invalidated cache — no agent run, no tokens.
    if let Some(client) = agent_client(state).await {
        match mycelium_librarian::query_cache::run_query_cached(
            &client,
            &cs,
            &agent_scopes(state, user).await,
            &args.question,
        )
        .await
        {
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

/// Store new knowledge in the caller's private bundle: enqueue-only
/// (spec D1/D4). Validates the content, enqueues the mutation, returns
/// the receipt — the librarian drain integrates it in the background,
/// and a staging note makes the content searchable immediately.
pub async fn memory_add(
    state: &McpState,
    user: &McpUser,
    args: &MemoryAddArgs,
) -> Result<String, ConceptStoreError> {
    if args.content.trim().is_empty() {
        return Err(ConceptStoreError::NotFound(
            "content must not be empty".into(),
        ));
    }
    let args_json = serde_json::json!({
        "path": args.path, "shelf": args.shelf, "concept_type": args.concept_type,
    })
    .to_string();
    let id = enqueue_mutation(state, user, QueueTool::Add, &args.content, args_json).await?;
    let mut receipt = receipt_text(id);
    receipt.push_str(&format!(
        "\nstaging: {}",
        mutation_queue::stage_note_path(id)
    ));
    Ok(receipt)
}

/// The literal receipt line (spec §4.1: parseable `receipt=<uuid>`).
fn receipt_text(id: Uuid) -> String {
    format!(
        "accepted, queued receipt={id} — the librarian will integrate this in the background; \
         check with memory_status(receipt_id='{id}')"
    )
}

/// Enqueue one mutation: row (capped) → encrypted payload blob →
/// staging note (add only) → flip to pending → notify the drain
/// worker. Returns the receipt id. Queue and store failures surface as
/// ConceptStoreError (render_store_error maps capacity to a caller-
/// visible retry hint, everything else to the internal-error arm). A
/// failure in a step after the row exists deads the row best-effort
/// (`abandon_row`) so a stranded staging row can't hold a depth-cap
/// slot until the next boot sweep.
async fn enqueue_mutation(
    state: &McpState,
    user: &McpUser,
    tool: QueueTool,
    content: &str,
    args_json: String,
) -> Result<Uuid, ConceptStoreError> {
    let master = state
        .master_key_for(user.user_id)
        .await
        .map_err(ConceptStoreError::Db)?;

    // 1. Row first, capped: a full queue rejects before any write.
    let id = state
        .store
        .enqueue_capped(user.user_id, tool, state.enqueue_depth_cap)
        .await?;

    // 2. Payload blob (user scope, raw FileRepo payload — content
    //    never in SQLite, never in the registry or index).
    let payload = MutationPayload {
        tool,
        args_json,
        content: content.to_string(),
    };
    let repo = FileRepo::new(state.store.user_dir(user.user_id));
    if let Err(e) = mutation_queue::write_payload(&repo, &master, id, &payload).await {
        // The row exists but can never become runnable: dead it
        // best-effort so the depth cap isn't held until the boot sweep
        // (the row stays — the receipt resolves to the dead state).
        abandon_row(
            state,
            user.user_id,
            &master,
            id,
            "enqueue failed: payload blob unwritable",
        )
        .await;
        return Err(e.into());
    }

    // 3. Staging note for add (spec D4): the content is searchable
    //    immediately while the integration runs in the background.
    //    The internal bypass — /mutation-queue/ is reserved for
    //    exactly this path.
    if tool == QueueTool::Add {
        let cs = user_store(state, user.user_id, master.clone());
        let note_path = mutation_queue::stage_note_path(id);
        let note = Concept::new(
            Frontmatter {
                concept_type: "Note".into(),
                title: Some(mycelium_librarian::fallback::derive_title(content)),
                tags: vec!["queue-staging".into()],
                timestamp: Some(now_rfc3339()),
                ..Default::default()
            },
            content.to_string(),
            note_path.clone(),
        );
        if let Err(e) = cs.put_batch_internal(&[note]).await {
            abandon_row(
                state,
                user.user_id,
                &master,
                id,
                "enqueue failed: staging note unwritable",
            )
            .await;
            return Err(e);
        }
        // Hot memory (spec §3.3): the staging write joins the hot set,
        // mirroring the fallback's per-write registration.
        mycelium_librarian::hot_memory::record_hot_write(&cs.scope_id(), &note_path);
    }

    // 4. Flip to pending; the row precedes the file on crash (the
    //    boot sweep reconciles either direction).
    state.store.activate(id).await?;

    // 5. Wake the drain worker.
    state.notify.notify_one();

    Ok(id)
}

/// Best-effort terminal cleanup for an enqueue whose row was created
/// but a later step failed. Without this the row strands in `staging`,
/// holding a depth-cap slot (and skewing /health) until the boot sweep.
/// Also drops the payload blob any earlier step managed to write — a
/// dead row's blob is never re-read (the recovery is a fresh enqueue),
/// so it is deleted here rather than left on disk forever. Never
/// deletes the row (the receipt still resolves to the dead state) and
/// never masks the original error returned to the caller.
async fn abandon_row(
    state: &McpState,
    user_id: Uuid,
    master: &mycelium_crypto::keys::MasterKey,
    id: Uuid,
    why: &str,
) {
    if let Err(e) = state.store.mark_dead(id, why).await {
        tracing::warn!(error = %e, receipt = %id, "cannot dead stranded enqueue row");
    }
    let repo = FileRepo::new(state.store.user_dir(user_id));
    if let Err(e) = repo
        .delete(
            &mutation_queue::payload_path(id),
            &Scope::User(master.clone()),
        )
        .await
    {
        tracing::warn!(error = %e, receipt = %id, "stranded enqueue payload delete failed");
    }
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

/// Apply a change to existing knowledge in the caller's private
/// bundle: enqueue-only (spec D1/D4). Validates the instruction,
/// enqueues the mutation, returns the receipt — the librarian drain
/// locates the concepts and applies the targeted edits in the
/// background.
pub async fn memory_update(
    state: &McpState,
    user: &McpUser,
    args: &MemoryUpdateArgs,
) -> Result<String, ConceptStoreError> {
    if args.instruction.trim().is_empty() {
        return Err(ConceptStoreError::NotFound(
            "instruction must not be empty".into(),
        ));
    }
    let args_json = serde_json::json!({ "path": args.path }).to_string();
    let id = enqueue_mutation(state, user, QueueTool::Update, &args.instruction, args_json).await?;
    Ok(receipt_text(id))
}

// ---------------------------------------------------------------------------
// memory_status
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct MemoryStatusArgs {
    /// Enqueue receipt id (the `receipt=<uuid>` from an add/update/;
    /// maintain call) — look up that item's integration state instead
    /// of reporting graph health.
    pub receipt_id: Option<String>,
}

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

/// Integration state for one of the caller's receipts. Another user's
/// id (or an absent id) resolves to state "unknown" — never leaks
/// existence (spec §4.1).
pub async fn receipt_view(
    state: &McpState,
    user: &McpUser,
    id: &str,
) -> Result<String, ConceptStoreError> {
    // Unparseable ids are simply unknown (no error — probing is cheap
    // and erroring would leak validation behavior).
    let Ok(uuid) = Uuid::parse_str(id.trim()) else {
        return Ok(serde_json::json!({ "state": "unknown" }).to_string());
    };
    // User scoping lives in the SQL (`WHERE id = ? AND user_id = ?`):
    // a foreign id is indistinguishable from an absent one.
    let item = mycelium_store::Store::queue_item(state.store.as_ref(), user.user_id, uuid)
        .await
        .map_err(ConceptStoreError::Queue)?;
    let Some(item) = item else {
        return Ok(serde_json::json!({ "state": "unknown" }).to_string());
    };
    // Receipt views never say staging (spec §4.1): enqueue-phase rows
    // render as pending. Everything else maps 1:1.
    let status_str = match item.status {
        QueueStatus::Staging => "pending",
        QueueStatus::Pending => "pending",
        QueueStatus::Running => "running",
        QueueStatus::Done => "done",
        QueueStatus::Dead => "dead",
    };
    // final_paths is the raw column string (written only by mark_done
    // as a JSON array); None or a parse failure both render null.
    let final_paths = item
        .final_paths
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok());
    Ok(serde_json::json!({
        "state": status_str,
        "tool": item.tool.as_str(),
        "attempts": item.attempts,
        "created_at": item.created_at,
        "updated_at": item.updated_at,
        "final_paths": final_paths,
        "detail": item.detail,
    })
    .to_string())
}

// ---------------------------------------------------------------------------
// memory_maintain
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct MemoryMaintainArgs {}

/// Health-check and repair the caller's bundle graph: enqueue-only
/// (spec D1/D4). A healthy graph returns early (nothing to repair);
/// an unhealthy one is enqueued and the librarian drain performs the
/// repairs in the background — the call returns the receipt.
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

    // An unhealthy graph is REQUIRED to enqueue: enqueueing a healthy
    // graph would waste an agent run (the early return above).
    let id = enqueue_mutation(state, user, QueueTool::Maintain, "", "{}".to_string()).await?;
    Ok(receipt_text(id))
}

// ---------------------------------------------------------------------------
// skill_get / skill_list
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct SkillGetArgs {
    /// The skill to fetch: a bare slug (resolved to the bundle hub
    /// `/{slug}/skill.md`, falling back to the flat `/{slug}.md`) or an
    /// exact bundle path ending in `.md`.
    #[schemars(
        description = "The skill to fetch: a bare slug (resolves the bundle hub /{slug}/skill.md, falling back to the flat /{slug}.md) or an exact bundle path ending in .md"
    )]
    pub name: String,
}

/// Fetch a skill: the caller's private skills first, then the global
/// skills shelf. A name ending in `.md` is an exact bundle path;
/// anything else is a slug, resolving the nested bundle hub
/// `/{slug}/skill.md` first with the legacy flat `/{slug}.md` as
/// fallback. Returns the skill's full markdown.
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
    let global = global_skills(state);

    // The candidate paths: an exact `.md` name is looked up as given; a
    // bare slug tries the bundle hub, then the legacy flat root.
    let candidates: Vec<String> = if args.name.ends_with(".md") {
        vec![format!("/{}", args.name.trim_start_matches('/'))]
    } else {
        let slug = slugify(&args.name);
        vec![format!("/{slug}/skill.md"), format!("/{slug}.md")]
    };

    // Private first, then global. A NotFound falls through to the next
    // candidate; any other error (corruption, crypto) propagates rather
    // than masking. Every hit still requires `type: Skill`.
    for store in [&cs, &global] {
        for path in &candidates {
            match store.get(path).await {
                Ok(c) if c.frontmatter.concept_type == "Skill" => return Ok(Some(c)),
                Ok(_) => {}
                Err(ConceptStoreError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(None)
}

#[derive(Deserialize, JsonSchema)]
pub struct SkillListArgs {}

/// List available skills, one entry per logical skill: each nested
/// bundle appears as its hub (`/{slug}/skill.md`); legacy flat skills
/// appear as themselves. The caller's private skills plus global skills.
pub async fn skill_list(
    state: &McpState,
    user: &McpUser,
) -> Result<Vec<mycelium_store::concept_store::ConceptEntry>, ConceptStoreError> {
    let master = state
        .master_key_for(user.user_id)
        .await
        .map_err(ConceptStoreError::Db)?;
    let cs = user_store(state, user.user_id, master);
    let global = global_skills(state);

    // Group BOTH scopes structurally; the catalog is hubs only plus
    // flat Skill roots — companions and non-Skill entries never list.
    let mut out: Vec<ConceptEntry> = Vec::new();
    for entries in [cs.list().await?, global.list().await?] {
        let listings = group_skills(&entries);
        for g in listings.grouped {
            if g.hub.concept_type == "Skill" {
                // The hub entry IS the skill: its title, its path.
                out.push(g.hub);
            }
        }
        out.extend(
            listings
                .flat
                .into_iter()
                .filter(|e| e.concept_type == "Skill"),
        );
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    Ok(out)
}
