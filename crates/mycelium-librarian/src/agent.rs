//! The librarian agent: an LLM tool-call loop over a concept store —
//! the port of the original Mycelium's "Knowledge Keeper".
//!
//! All knowledge operations flow through here (v1 architecture):
//! queries synthesize grounded answers from search/read tools; writes
//! (add/update/maintain) search for overlap first, then enrich or
//! create concepts with deliberate placement and two-way linking.
//! The agent is read-only in query mode; mutation mode adds the write
//! tools. A step cap bounds the loop; the deterministic fallback keeps
//! every entry point working when no LLM backend is reachable.

use mycelium_core::concept::Concept;
use mycelium_core::search::SearchQuery;
use mycelium_store::{ConceptStore, ConceptStoreError};

use crate::llm::{ConversationTurn, LlmClient, StepOutput, ToolCall, tool_spec};

/// Maximum agentic steps per run (v1: MAX_STEPS = 12; raised for
/// cross-scope search over large libraries — the model needs room to
/// search, read, and synthesize across user + skills + library scopes).
pub const MAX_STEPS: u32 = 30;

/// Bundles larger than this get a compact tree summary in the prompt
/// (v1: LARGE_BUNDLE_THRESHOLD = 300).
const LARGE_BUNDLE_THRESHOLD: usize = 300;

/// Parse `book://<slug>#<anchor>` (anchor optional). Returns
/// `(slug, anchor)`; `None` when the string isn't a book URI or the
/// slug tries to escape (v1's parseBookUri rules).
fn parse_book_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("book://")?;
    let (slug, anchor) = match rest.split_once('#') {
        Some((s, a)) => (s, a),
        None => (rest, ""),
    };
    if slug.is_empty()
        || slug == "."
        || slug == ".."
        || slug.contains('/')
        || slug.contains('\\')
        || slug.contains('\0')
    {
        return None;
    }
    Some((slug.to_string(), anchor.to_string()))
}

/// List a book's chapter anchors (`ch-<n>-<slug> — Title` lines) from
/// its stack text, fence-aware, capped at 120 (v1 parity).
fn list_chapter_anchors(text: &str) -> String {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        // A heading with a GFM explicit ID: `#... Title {#anchor}`.
        if let Some(heading) = t.strip_prefix('#') {
            let heading = heading.trim_start();
            if let Some(brace) = heading.find("{#")
                && let Some(end) = heading[brace..].find('}')
            {
                let anchor = &heading[brace + 2..brace + end];
                let title = &heading[..brace];
                let title = title.trim();
                // Chapter anchors only: ch-<n>-<slug> (3 parts,
                // 3rd non-numeric) or ch-<n>-<m>-<slug> (section).
                let parts: Vec<&str> = anchor.split('-').collect();
                let is_chapter = parts.len() >= 3
                    && parts[0] == "ch"
                    && parts[1].chars().all(|c| c.is_ascii_digit())
                    && !parts[1].is_empty()
                    && (parts.len() == 3 && !parts[2].chars().all(|c| c.is_ascii_digit())
                        || parts.len() > 3);
                if is_chapter {
                    out.push(format!("- {anchor} — {title}"));
                }
            }
        }
        if out.len() >= 120 {
            break;
        }
    }
    if out.is_empty() {
        "(none found)".to_string()
    } else {
        out.join("\n")
    }
}

/// Additional stores and capabilities available to the librarian agent
/// beyond its primary user-scoped ConceptStore.  Enables cross-scope
/// search (global books/skills) and full-text book search.
pub struct AgentScopes<'a> {
    /// Global skills shelf (optional — None when not configured).
    pub skills: Option<ConceptStore<'a>>,
    /// Global library catalog (optional — None when not configured).
    pub library: Option<ConceptStore<'a>>,
    /// Full-text book stack reader (optional — enables the `read_passage` tool).
    pub read_stack_text: Option<Box<StackTextReader>>,
    /// Book slugs the caller may see (None = no restriction). The web/MCP
    /// layer computes this from bookshelf visibility: a library hit belongs
    /// to a book; only global-read shelves (or admins) may see it.
    pub visible_slugs: Option<std::collections::HashSet<String>>,
    /// Trace persistence (optional — None disables recording). The pool
    /// + scope id identify the caller's trace rows (`user:<uuid>`).
    pub trace: Option<TraceSink>,
}

/// Bookshelf-visibility check for a library catalog path: the book's
/// slug is the first path segment (`/<slug>/...`). `None` = the caller
/// is unrestricted (admin); otherwise only books on global-read
/// shelves pass. Mirrors the gating `search_knowledge` and
/// `read_passage` already apply.
fn library_path_visible(scopes: &AgentScopes<'_>, path: &str) -> bool {
    match &scopes.visible_slugs {
        None => true,
        Some(vis) => {
            let slug = path
                .trim_start_matches('/')
                .split('/')
                .next()
                .unwrap_or_default();
            vis.contains(slug)
        }
    }
}

/// A cross-scope hit before lifecycle enrichment: the scope tag and the
/// search result (spec §3.2a-6).
pub type ScopedHit = (String, mycelium_core::search::SearchResult);

/// The shared cross-scope hit helper behind BOTH the MCP `memory_query`
/// hits default (cap = the caller's `limit`, `query_top_k` when absent,
/// ≤ 64) and the agent's `search_knowledge` tool (cap 10, §3.2a-10):
/// the user's private bundle first, then the global skills shelf, then
/// the library catalog filtered by bookshelf visibility — exactly as
/// the agent arm collected before this consolidation. Sort: score
/// descending, ties broken by concept path ascending (deterministic),
/// truncation LAST. BM25 scores are corpus-relative (the scope tag is
/// provenance), so scoring is per-corpus and the tag carries which.
pub async fn cross_scope_hits(
    store: &ConceptStore<'_>,
    scopes: &AgentScopes<'_>,
    query: &SearchQuery,
    cap: usize,
) -> Vec<ScopedHit> {
    let mut all_hits: Vec<ScopedHit> = Vec::new();

    // 1. User's private bundle (always searched).
    for h in store.search(query).await.unwrap_or_default() {
        all_hits.push(("user".to_string(), h));
    }

    // 2. Global skills shelf (if available).
    if let Some(ref skills) = scopes.skills {
        for h in skills.search(query).await.unwrap_or_default() {
            all_hits.push(("skills".to_string(), h));
        }
    }

    // 3. Global library catalog (if available), filtered by the caller's
    //    bookshelf visibility.
    if let Some(ref library) = scopes.library {
        for h in library.search(query).await.unwrap_or_default() {
            if !library_path_visible(scopes, &h.concept_path) {
                continue;
            }
            all_hits.push(("library".to_string(), h));
        }
    }

    // Sort by score descending, then path ascending for determinism.
    // Cap the result set AFTER the sort: truncation must not change the
    // ordering rule.
    all_hits.sort_by(|a, b| {
        b.1.score
            .partial_cmp(&a.1.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.concept_path.cmp(&b.1.concept_path))
    });
    all_hits.truncate(cap);
    all_hits
}

/// The lifecycle/provenance fields of a hit's underlying concept (spec
/// §3.2a-6: the ten-field hit carries them beside the snippet). The
/// `description` is the FRONTMATTER's (never the snippet's text).
pub struct HitFields {
    pub description: Option<String>,
    pub timestamp: Option<String>,
    pub confirmed_at: Option<String>,
    pub status: Option<String>,
    pub superseded_by: Option<String>,
}

/// Read [`HitFields`] for `path` from `cs` (`cs.get` → frontmatter). A
/// read or parse FAULT does not drop the hit — every field reads as
/// `None` and the fault logs (muted, as a warning) — the
/// revisions-browser `(no title)` precedent: search results stay honest
/// about what matched even when a file is corrupt.
pub async fn hit_fields(cs: &ConceptStore<'_>, path: &str) -> HitFields {
    match cs.get(path).await {
        Ok(c) => HitFields {
            description: c.frontmatter.description.clone(),
            timestamp: c.frontmatter.timestamp.clone(),
            confirmed_at: c.frontmatter.confirmed_at.clone(),
            status: c.frontmatter.status.clone(),
            superseded_by: c.frontmatter.superseded_by.clone(),
        },
        Err(e) => {
            tracing::warn!(path, error = %e, "hit fields unavailable: hit kept, fields None");
            HitFields {
                description: None,
                timestamp: None,
                confirmed_at: None,
                status: None,
                superseded_by: None,
            }
        }
    }
}

/// The ten-field enriched hit the MCP `memory_query` hits default
/// returns (spec §3.2a-6); the field order is the serde output order.
#[derive(Debug, serde::Serialize)]
pub struct QueryHit {
    pub scope: String,
    pub path: String,
    pub title: String,
    pub description: Option<String>,
    pub snippet: String,
    pub score: f32,
    pub timestamp: Option<String>,
    pub confirmed_at: Option<String>,
    pub status: Option<String>,
    pub superseded_by: Option<String>,
}

/// Where agent-run traces are persisted (DB-backed; v1 .traces/ parity).
#[derive(Clone)]
pub struct TraceSink {
    pub pool: sqlx::SqlitePool,
    pub scope_id: String,
}

/// Read the full text of a book's stack file by slug (async).  Returns an
/// error string on failure so the agent can report it cleanly to the model.
pub type StackTextReader = dyn Fn(&str) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
    + Send
    + Sync;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("store error: {0}")]
    Store(#[from] ConceptStoreError),
    #[error("LLM error: {0}")]
    Llm(#[from] crate::llm::LlmError),
    #[error("agent step cap exceeded")]
    StepCapExceeded,
    #[error("tool {name} returned invalid arguments: {reason}")]
    BadToolArgs { name: String, reason: String },
}

impl AgentError {
    /// Backend-down classification (outage-recovery spec §3.2): true only
    /// for an LLM error that never reached a working model. Step cap,
    /// tool-arg and store errors are real run outcomes and keep their
    /// terminal handling.
    #[must_use]
    pub fn is_backend_down(&self) -> bool {
        match self {
            AgentError::Llm(e) => e.is_backend_down(),
            AgentError::Store(_) | AgentError::StepCapExceeded | AgentError::BadToolArgs { .. } => {
                false
            }
        }
    }
}

/// The result of a query run: the grounded answer.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub answer: String,
    pub steps: u32,
}

/// The result of a mutation run.
#[derive(Debug, Clone)]
pub struct MutationResult {
    pub summary: String,
    pub files_changed: Vec<String>,
    pub steps: u32,
}

/// The agent's operating mode (v1: query / mutate / chat).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    Query,
    Mutate,
    Chat,
}

/// Run the agent in read-only query mode.
///
/// Admission is the caller's: this performs none. Surfaces enter
/// [`crate::llm_gate::LLM_GATE`] with their own class and wait bound
/// and hold the permit across this call.
pub async fn run_query(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    scopes: &AgentScopes<'_>,
    question: &str,
) -> Result<QueryResult, AgentError> {
    let system = build_system_prompt(store, AgentMode::Query).await?;
    let tools = read_tool_specs();
    let answer = agent_loop(
        client, store, scopes, &system, question, &tools, false, // read-only
        0.2,
    )
    .await?;
    Ok(QueryResult { answer, steps: 0 })
}

/// Run the agent in mutation mode (write tools enabled) at the
/// default `MAX_STEPS` cap. Admission is the caller's — this acquires
/// nothing (see [`run_query`]).
/// Intentional public API — the inline mutation entry point: the dream
/// maintenance pass (`dream::run_dream`, POST /api/v1/dream) calls it
/// in production, under its own gate admission. The queue drain does
/// NOT use it — it runs `run_mutation_capped` instead, under its own
/// gate permit and step cap.
pub async fn run_mutation(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    scopes: &AgentScopes<'_>,
    instruction: &str,
) -> Result<MutationResult, AgentError> {
    let system = build_system_prompt(store, AgentMode::Mutate).await?;
    let tools = all_tool_specs();
    let (summary, files) = agent_loop_tracked(
        client,
        store,
        scopes,
        &system,
        instruction,
        &tools,
        true,
        0.2,
        MAX_STEPS,
    )
    .await?;
    Ok(MutationResult {
        summary,
        files_changed: files,
        steps: 0,
    })
}

/// Mutation run with a caller-supplied step cap (the queue drain's
/// entry: the drain itself holds the gate permit from its capacity
/// check through the mark, so this acquires none — as no agent entry
/// point does).
pub async fn run_mutation_capped(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    scopes: &AgentScopes<'_>,
    instruction: &str,
    max_steps: u32,
) -> Result<MutationResult, AgentError> {
    let system = build_system_prompt(store, AgentMode::Mutate).await?;
    let tools = all_tool_specs();
    let (summary, files) = agent_loop_tracked(
        client,
        store,
        scopes,
        &system,
        instruction,
        &tools,
        true,
        0.2,
        max_steps,
    )
    .await?;
    Ok(MutationResult {
        summary,
        files_changed: files,
        steps: 0,
    })
}

/// A progress event emitted during a streaming agent run.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// The agent is invoking a tool (name + short argument summary).
    Tool { name: String, detail: String },
    /// The final answer.
    Done(String),
    /// The run failed.
    Failed(String),
}

/// Run the agent in chat mode (write tools enabled, full history).
/// Returns the assistant's reply.
pub async fn run_chat(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    scopes: &AgentScopes<'_>,
    history: &[ConversationTurn],
) -> Result<String, AgentError> {
    run_chat_streaming(client, store, scopes, history, None).await
}

/// Streaming chat: same loop as run_chat, but emits progress events to
/// the caller's channel (the web layer streams them as SSE). The
/// final reply is still returned. Admission is the caller's — this
/// acquires nothing (see [`run_query`]).
pub async fn run_chat_streaming(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    scopes: &AgentScopes<'_>,
    history: &[ConversationTurn],
    events: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>,
) -> Result<String, AgentError> {
    let emit = |event: AgentEvent| {
        if let Some(tx) = &events {
            let _ = tx.send(event);
        }
    };
    let system = build_system_prompt(store, AgentMode::Chat).await?;
    let tools = all_tool_specs();
    let mut recorder = crate::trace::TraceRecorder::new();
    // The user turn that started this run, for the trace record.
    let input = history
        .iter()
        .rev()
        .find_map(|t| match t {
            ConversationTurn::User(u) => Some(u.clone()),
            _ => None,
        })
        .unwrap_or_default();
    // Seed the loop with the provided history (the last turn is the
    // new user message).
    let mut conversation: Vec<ConversationTurn> = history.to_vec();
    for _ in 0..MAX_STEPS {
        let output = client
            .chat_with_tools(&system, &conversation, &tools, 0.2)
            .await?;
        match output {
            StepOutput::Text(answer) => {
                let trace = recorder.finalize(
                    crate::trace::TraceKind::Chat,
                    &input,
                    &answer,
                    crate::trace::TraceOutcome::Success,
                );
                if let Some(sink) = &scopes.trace {
                    crate::trace::save_trace(&sink.pool, &sink.scope_id, &trace).await;
                }
                emit(AgentEvent::Done(answer.clone()));
                return Ok(answer);
            }
            StepOutput::ToolCalls(calls) => {
                // OpenAI protocol: echo the assistant tool_calls turn.
                conversation.push(ConversationTurn::AssistantToolCalls(calls.clone()));
                for call in calls {
                    emit(AgentEvent::Tool {
                        name: call.function.name.clone(),
                        detail: call.function.arguments.chars().take(120).collect(),
                    });
                    // Tool errors go back to the model (recovery), same
                    // as the query/mutation loops.
                    let result =
                        match execute_tool_tracked(store, scopes, &call, true, &mut recorder).await
                        {
                            Ok((result, _)) => result,
                            Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
                        };
                    conversation.push(ConversationTurn::ToolResult {
                        call_id: call.id.clone(),
                        result,
                    });
                }
            }
        }
    }
    let err = AgentError::StepCapExceeded.to_string();
    let trace = recorder.finalize(
        crate::trace::TraceKind::Chat,
        &input,
        &err,
        crate::trace::TraceOutcome::Failed,
    );
    if let Some(sink) = &scopes.trace {
        crate::trace::save_trace(&sink.pool, &sink.scope_id, &trace).await;
    }
    emit(AgentEvent::Failed(err.clone()));
    Err(AgentError::StepCapExceeded)
}

/// The core loop shared by query and mutation runs. Returns the final
/// text. `allow_writes` gates the write tools (query mode is read-only).
#[allow(clippy::too_many_arguments)]
async fn agent_loop(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    scopes: &AgentScopes<'_>,
    system: &str,
    prompt: &str,
    tools: &[crate::llm::ToolSpec<'_>],
    allow_writes: bool,
    temperature: f32,
) -> Result<String, AgentError> {
    let (text, _) = agent_loop_tracked(
        client,
        store,
        scopes,
        system,
        prompt,
        tools,
        allow_writes,
        temperature,
        MAX_STEPS,
    )
    .await?;
    Ok(text)
}

/// The core loop, also returning the set of files written (mutation
/// mode). Tool results are appended as conversation turns.
#[allow(clippy::too_many_arguments)]
async fn agent_loop_tracked(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    scopes: &AgentScopes<'_>,
    system: &str,
    prompt: &str,
    tools: &[crate::llm::ToolSpec<'_>],
    allow_writes: bool,
    temperature: f32,
    max_steps: u32,
) -> Result<(String, Vec<String>), AgentError> {
    let kind = if allow_writes {
        crate::trace::TraceKind::Mutation
    } else {
        crate::trace::TraceKind::Query
    };
    let mut recorder = crate::trace::TraceRecorder::new();
    let mut conversation = vec![ConversationTurn::User(prompt.to_string())];
    let mut files_changed: Vec<String> = Vec::new();
    for _ in 0..max_steps {
        let output = client
            .chat_with_tools(system, &conversation, tools, temperature)
            .await?;
        match output {
            StepOutput::Text(answer) => {
                let trace =
                    recorder.finalize(kind, prompt, &answer, crate::trace::TraceOutcome::Success);
                if let Some(sink) = &scopes.trace {
                    crate::trace::save_trace(&sink.pool, &sink.scope_id, &trace).await;
                }
                return Ok((answer, files_changed));
            }
            StepOutput::ToolCalls(calls) => {
                // OpenAI protocol: the assistant's tool_calls turn MUST
                // precede the tool results in the conversation.
                conversation.push(ConversationTurn::AssistantToolCalls(calls.clone()));
                for call in calls {
                    // Tool execution errors (bad args, not-found,
                    // read-only violations) go back to the model as
                    // error results — the agent can recover and try a
                    // different approach. Only LLM transport errors
                    // abort the run (propagated by chat_with_tools).
                    let (result, written) = match execute_tool_tracked(
                        store,
                        scopes,
                        &call,
                        allow_writes,
                        &mut recorder,
                    )
                    .await
                    {
                        Ok((result, written)) => (result, written),
                        Err(e) => (
                            serde_json::json!({ "error": e.to_string() }).to_string(),
                            None,
                        ),
                    };
                    if let Some(path) = written {
                        files_changed.push(path);
                    }
                    conversation.push(ConversationTurn::ToolResult {
                        call_id: call.id.clone(),
                        result,
                    });
                }
            }
        }
    }
    let trace = recorder.finalize(
        kind,
        prompt,
        "agent exceeded step cap",
        crate::trace::TraceOutcome::Failed,
    );
    if let Some(sink) = &scopes.trace {
        crate::trace::save_trace(&sink.pool, &sink.scope_id, &trace).await;
    }
    Err(AgentError::StepCapExceeded)
}

/// `include_inactive` from a model-supplied tool-call argument: absent,
/// null or non-boolean all mean `false` — history queries stay opt-in, so
/// every existing caller keeps the default active-only filtering. Takes
/// the args map (the `as_object` guard rebinds `args` to a map before the
/// arms run); the whole-payload null case only exists pre-guard.
fn include_inactive_arg(args: &serde_json::Map<String, serde_json::Value>) -> bool {
    args.get("include_inactive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Execute one tool call; returns (JSON result for the model, path
/// written when the tool mutated a concept). Records the step on the
/// run's trace recorder.
async fn execute_tool_tracked(
    store: &ConceptStore<'_>,
    scopes: &AgentScopes<'_>,
    call: &ToolCall,
    allow_writes: bool,
    recorder: &mut crate::trace::TraceRecorder,
) -> Result<(String, Option<String>), AgentError> {
    let name = call.function.name.as_str();
    let args: serde_json::Value =
        serde_json::from_str(&call.function.arguments).map_err(|e| AgentError::BadToolArgs {
            name: name.to_string(),
            reason: e.to_string(),
        })?;
    // Tool args must be a JSON object: every arm below reads named keys on
    // it. A non-object payload (a bare string, array, or number) is a
    // recoverable bad-args error — never an index panic (a `Map` index on a
    // missing key panics, which is what wedged the drain worker on
    // 2026-10-31).
    let Some(args) = args.as_object() else {
        return Err(AgentError::BadToolArgs {
            name: name.to_string(),
            reason: "arguments must be a JSON object".into(),
        });
    };
    let outcome = match name {
        "search_knowledge" => {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            // Term cap mirrors the MCP layer's MAX_SEARCH_TERMS: the
            // encrypted index issues one lookup per term, so an
            // unbounded (LLM-controlled) term count is a
            // query-amplification DoS vector.
            const MAX_SEARCH_TERMS: usize = 32;
            let terms: Vec<String> = query
                .split_whitespace()
                .take(MAX_SEARCH_TERMS)
                .map(|t| t.to_string())
                .collect();
            let mut q = SearchQuery::new(terms);
            // History queries are opt-in: without the flag the search
            // keeps the default active-only filter, so retired and
            // superseded concepts stay out of ordinary retrieval.
            if include_inactive_arg(args) {
                q.status_filter = mycelium_core::search::StatusFilter::All;
            }

            // Cross-scope collection via the shared helper — the MCP
            // hits default runs the same code with the caller's cap
            // (§3.2a-6/§3.3). Cap 10 is this tool's own (§3.2a-10: it
            // does NOT use the admin-tunable query_top_k): hundreds of
            // catalog concepts would drown the model and burn its step
            // budget reading them one by one.
            let all_hits = cross_scope_hits(store, scopes, &q, 10).await;

            let hits_json: Vec<serde_json::Value> = all_hits
                .iter()
                .map(|(scope, h)| {
                    serde_json::json!({
                        "path": h.concept_path,
                        "title": h.title,
                        "snippet": h.snippet,
                        "score": h.score,
                        "scope": scope,
                    })
                })
                .collect();

            if !hits_json.is_empty() {
                Ok((serde_json::json!({ "hits": hits_json }).to_string(), None))
            } else {
                // Keyword miss ≠ knowledge absent (v1 rule): hand the
                // model the layout so its next step is reading
                // plausible concepts, not giving up.
                let tree = format_tree(store).await?;
                Ok((
                    serde_json::json!({
                        "hits": [],
                        "notice": "No keyword matches — but this search is literal, not semantic. The knowledge may exist under different wording. Retry with synonyms or broader terms, review the layout below, and read_concept ANY concept whose type, name, or description could plausibly relate.",
                        "bundle_layout": tree,
                    })
                    .to_string(),
                    None,
                ))
            }
        }
        "read_concept" => {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            // Scope-aware: the path may live in the user's bundle, the
            // global skills shelf, or the global library catalog (search
            // results are scope-tagged). Try the user's bundle first,
            // then the other scopes in order.
            let result = store.get(path).await;
            let concept = match result {
                Ok(c) => c,
                Err(_) => {
                    let mut found = None;
                    if let Some(ref skills) = scopes.skills
                        && let Ok(c) = skills.get(path).await
                    {
                        found = Some(c);
                    }
                    if found.is_none()
                        && let Some(ref library) = scopes.library
                        && library_path_visible(scopes, path)
                        && let Ok(c) = library.get(path).await
                    {
                        found = Some(c);
                    }
                    match found {
                        Some(c) => c,
                        None => {
                            return Err(AgentError::Store(ConceptStoreError::NotFound(
                                path.to_string(),
                            )));
                        }
                    }
                }
            };
            let markdown = concept
                .to_markdown()
                .map_err(|e| AgentError::Store(ConceptStoreError::Parse(e)))?;
            Ok((
                serde_json::json!({ "path": concept.source_path, "markdown": markdown })
                    .to_string(),
                None,
            ))
        }
        "list_directory" => {
            let tree = format_tree(store).await?;
            Ok((serde_json::json!({ "layout": tree }).to_string(), None))
        }
        "lint_knowledge" => {
            // Graph health over the caller's bundle: orphans + broken
            // links (deterministic, no LLM).
            let entries = store.list().await?;
            let mut concepts = Vec::new();
            for entry in &entries {
                if let Ok(c) = store.get(&entry.path).await {
                    concepts.push(c);
                }
            }
            let bundle = mycelium_core::bundle::Bundle {
                root: std::path::PathBuf::new(),
                concepts,
                shelf_info: None,
                reserved_files_seen: Vec::new(),
                naming_warnings: Vec::new(),
            };
            let graph = mycelium_core::graph::build_graph(&bundle);
            let health = graph.health();
            Ok((
                serde_json::json!({
                    "concepts": health.concept_count,
                    "links": health.edge_count,
                    "orphans": graph.orphans,
                    "broken_links": graph.broken_links.iter().map(|b| format!("{} → {}", b.from, b.to)).collect::<Vec<_>>(),
                })
                .to_string(),
                None,
            ))
        }
        "read_passage" => {
            // v1 card-catalog semantics: fetch a chapter/section's full
            // text from the library stacks by its `book://<slug>#<anchor>`
            // resource. On a missing anchor, return the book's chapter
            // list so the model can pick a valid one.
            let resource = args
                .get("resource")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if resource.is_empty() {
                return Err(AgentError::BadToolArgs {
                    name: name.into(),
                    reason: "resource is required".into(),
                });
            }
            let Some((slug, anchor)) = parse_book_uri(&resource) else {
                return Ok((
                    serde_json::json!({
                        "resource": resource,
                        "error": "Not a book:// URI — expected book://<slug>#<anchor>, taken from a concept's `resource` field.",
                    })
                    .to_string(),
                    None,
                ));
            };
            let reader = match &scopes.read_stack_text {
                Some(r) => r,
                None => {
                    return Ok((
                        serde_json::json!({
                            "resource": resource,
                            "error": "No library configured on this instance; cannot read passages.",
                        })
                        .to_string(),
                        None,
                    ));
                }
            };
            // Bookshelf visibility: a non-admin caller may only read
            // passages of books on global-read shelves.
            if let Some(vis) = &scopes.visible_slugs
                && !vis.contains(&slug)
            {
                return Ok((
                    serde_json::json!({
                        "resource": resource,
                        "error": "Book not found in library.",
                    })
                    .to_string(),
                    None,
                ));
            }
            match reader(&slug).await {
                Ok(text) => {
                    if anchor.is_empty() {
                        // No anchor: list the book's chapter anchors.
                        let anchors = list_chapter_anchors(&text);
                        return Ok((
                            serde_json::json!({
                                "resource": resource,
                                "passage": format!(
                                    "No anchor given. Chapter anchors in this book:\n{}",
                                    anchors
                                ),
                            })
                            .to_string(),
                            None,
                        ));
                    }
                    match mycelium_core::library::extract_passage(&slug, &anchor, &text) {
                        Ok(p) => Ok((
                            serde_json::json!({
                                "resource": resource,
                                "passage": p.text,
                            })
                            .to_string(),
                            None,
                        )),
                        Err(_) => {
                            let anchors = list_chapter_anchors(&text);
                            Ok((
                                serde_json::json!({
                                    "resource": resource,
                                    "passage": format!(
                                        "No passage for anchor \"{anchor}\" in book \"{slug}\". Chapter anchors in this book:\n{anchors}"
                                    ),
                                })
                                .to_string(),
                                None,
                            ))
                        }
                    }
                }
                Err(e) => Ok((
                    serde_json::json!({
                        "resource": resource,
                        "error": e,
                    })
                    .to_string(),
                    None,
                )),
            }
        }
        "write_concept" => {
            if !allow_writes {
                return Err(AgentError::BadToolArgs {
                    name: name.into(),
                    reason: "write tool in read-only mode".into(),
                });
            }
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let body = args
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            // A non-object frontmatter takes the same None/skip path an
            // absent one does.
            let frontmatter = args.get("frontmatter").and_then(|v| v.as_object());
            let concept_type = frontmatter
                .and_then(|fm| fm.get("type"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .unwrap_or("Note")
                .to_string();
            let title = frontmatter
                .and_then(|fm| fm.get("title"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let description = frontmatter
                .and_then(|fm| fm.get("description"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let tags: Vec<String> = frontmatter
                .and_then(|fm| fm.get("tags"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let concept = Concept::new(
                mycelium_core::concept::Frontmatter {
                    concept_type,
                    title,
                    description,
                    tags,
                    ..Default::default()
                },
                body.to_string(),
                path,
            );
            store.put(&concept).await?;
            Ok((
                serde_json::json!({ "written": concept.source_path }).to_string(),
                Some(concept.source_path),
            ))
        }
        "patch_concept" => {
            if !allow_writes {
                return Err(AgentError::BadToolArgs {
                    name: name.into(),
                    reason: "write tool in read-only mode".into(),
                });
            }
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let mut concept = store.get(path).await?;
            // Frontmatter merge: null deletes a key (v1 semantics).
            if let Some(fm) = args.get("frontmatter").and_then(|v| v.as_object()) {
                for (key, value) in fm {
                    match key.as_str() {
                        "title" => {
                            concept.frontmatter.title = value.as_str().map(String::from);
                        }
                        "description" => {
                            concept.frontmatter.description = value.as_str().map(String::from);
                        }
                        "resource" => {
                            concept.frontmatter.resource = value.as_str().map(String::from);
                        }
                        // Lifecycle fields (rule 8): same string-or-null
                        // semantics as `title`. No validation here —
                        // `Frontmatter::validated()` at put rejects bad
                        // values and the error reaches the model.
                        "status" => {
                            concept.frontmatter.status = value.as_str().map(String::from);
                        }
                        "superseded_by" => {
                            concept.frontmatter.superseded_by = value.as_str().map(String::from);
                        }
                        "valid_to" => {
                            concept.frontmatter.valid_to = value.as_str().map(String::from);
                        }
                        "tags" => {
                            concept.frontmatter.tags = value
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|t| t.as_str().map(String::from))
                                        .collect()
                                })
                                .unwrap_or_default();
                        }
                        _ => {} // unknown keys ignored (lenient)
                    }
                }
            }
            // Section replace or full body replace.
            if let Some(section) = args.get("replace_section").and_then(|v| v.as_object()) {
                // Missing keys resolve to empty strings (the Value-miss
                // semantics these `unwrap_or_default()`s were written for)
                // — a lenient no-op-ish patch, never a Map-index panic.
                let heading = section
                    .get("heading")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let content = section
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                concept.body = replace_section(&concept.body, heading, content);
            } else if let Some(body) = args.get("replace_body").and_then(|v| v.as_str()) {
                concept.body = body.to_string();
            }
            store.put(&concept).await?;
            Ok((
                serde_json::json!({ "patched": concept.source_path }).to_string(),
                Some(concept.source_path),
            ))
        }
        "delete_concept" => {
            if !allow_writes {
                return Err(AgentError::BadToolArgs {
                    name: name.into(),
                    reason: "write tool in read-only mode".into(),
                });
            }
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            store.delete(path).await?;
            Ok((
                serde_json::json!({ "deleted": path }).to_string(),
                Some(path.to_string()),
            ))
        }
        other => Err(AgentError::BadToolArgs {
            name: other.to_string(),
            reason: "unknown tool".into(),
        }),
    };
    // Trace the step (telemetry — never fails the call).
    match &outcome {
        Ok((result, written)) => {
            let summary = match name {
                "search_knowledge" | "read_passage" => args
                    .get(if name == "search_knowledge" {
                        "query"
                    } else {
                        "resource"
                    })
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                _ => args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            };
            let mut paths = Vec::new();
            if let Some(p) = written {
                paths.push(p.clone());
            }
            if name == "search_knowledge"
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(result)
                && let Some(hits) = v.get("hits").and_then(|h| h.as_array())
            {
                paths = hits
                    .iter()
                    .filter_map(|h| h.get("path").and_then(|p| p.as_str()).map(String::from))
                    .collect();
            }
            recorder.record(name, &summary, paths, written.is_some());
            // Hot memory (v1 parity): a write moves the concept to the
            // front of the hot set and purges recent Q&As (the write
            // may contradict them); a delete removes it.
            if let Some(p) = written {
                if name == "delete_concept" {
                    crate::hot_memory::record_hot_delete(&store.scope_id(), p);
                } else {
                    crate::hot_memory::record_hot_write(&store.scope_id(), p);
                }
            }
        }
        Err(_) => {
            recorder.record(name, "(error)", vec![], false);
        }
    }
    outcome
}

/// Replace one top-level `# Section` in a markdown body (v1
/// patch_concept semantics). Appends the section when absent.
fn replace_section(body: &str, heading: &str, content: &str) -> String {
    let section_start = format!("# {heading}");
    let mut out = String::new();
    let mut replaced = false;
    let mut in_target = false;
    for line in body.lines() {
        if line.trim() == section_start {
            in_target = true;
            replaced = true;
            out.push_str(&section_start);
            out.push('\n');
            out.push_str(content);
            out.push('\n');
            continue;
        }
        if in_target && line.starts_with("# ") {
            in_target = false;
        }
        if !in_target {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        // Append the section at the end.
        if !out.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&section_start);
        out.push('\n');
        out.push_str(content);
        out.push('\n');
    }
    out
}

/// The bundle layout as an indented tree (v1 formatTree) — one line
/// per concept: path, type, description.
pub async fn format_tree(store: &ConceptStore<'_>) -> Result<String, AgentError> {
    let entries = store.list().await?;
    let mut lines = vec!["/".to_string()];
    let mut sorted = entries;
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    for entry in sorted {
        lines.push(format!(
            "  {}  [{}] {}",
            entry.path, entry.concept_type, entry.title
        ));
    }
    Ok(lines.join("\n"))
}

/// The OKF operating-rules block the system prompt interpolates verbatim
/// (v1's rules, adapted). It lives here rather than inline in the prompt
/// builder so the block has one home the content tests can pin — rule 8
/// carries the knowledge-lifecycle guidance. `{types_list}` is the only
/// slot the builder fills in.
const OPERATING_RULES: &str = "1. SEARCH FIRST. Before adding anything, search for existing concepts the new knowledge relates to.
2. ENRICH OVER CREATE. A fact that is an attribute of an existing concept gets patched INTO that concept — not filed as its own concept.
3. LINK BOTH WAYS. A new concept must be wired into the graph: link it to related concepts AND patch those to reference it back.
4. REUSE TYPES. Prefer a type already in use. Types currently in the bundle: {types_list}.
5. PLACE DELIBERATELY. Choose directories by subject area; reuse existing directories; short kebab-case filenames.
6. WRITE FOR THE NEXT READER. One-line descriptions; concise, factual, self-contained bodies.
7. PREFER PATCH OVER REWRITE. Use patch_concept for small changes.
8. DEPRECATE, DON'T DELETE. When knowledge is outdated: patch the OLD concept — status: superseded + superseded_by: <new-concept-path> — or status: retired. Never leave a live concept contradicting a new one. Use delete_concept only for wrong/harmful content or on explicit request.
9. CITE WHEN ANSWERING. Ground every claim in concepts you actually read; list their paths. If the knowledge base doesn't contain the answer, say so plainly — never invent knowledge.";

/// Render the operating-rules block for the given live-type list.
fn operating_rules(types_list: &str) -> String {
    // `replacen(…, 1)` encodes the exactly-one-slot invariant: the
    // const has a single `{types_list}` slot, and a second one should
    // fail the prompt test (raw slot leaks to the model), not get
    // silently substituted.
    OPERATING_RULES.replacen("{types_list}", types_list, 1)
}

/// The per-mode task section the system prompt appends after the layout.
fn mode_section(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Query => {
            "## Your task mode: QUERY (read-only)

Answer the user's question from the knowledge base. Search, read the relevant concepts, then answer. End your answer with a \"Sources:\" line listing the bundle paths you used.

RETRIEVAL PROTOCOL — search is keyword-based, not semantic, so one empty search proves nothing:
1. Search with the question's key terms.
2. On a miss, retry once or twice with synonyms, broader terms, or related entities.
3. Still nothing? Check the bundle layout (above, or via list_directory) and read_concept EVERY concept whose type, name, or description could plausibly relate.
4. Only after steps 1-3 may you answer that the knowledge base has no coverage; then suggest what concept could be added."
        }
        AgentMode::Mutate => {
            "## Your task mode: MUTATE

The input is knowledge to persist or a change to apply — NOT a message to reply to. Do not respond conversationally. You MUST act with the write tools.

WRITE PROTOCOL:
1. Search for concepts the knowledge relates to or belongs to; read the strongest candidates.
2. CHECK FOR CONTRADICTION. If the new knowledge conflicts with an existing concept, update it — never leave two contradictory claims standing. When replacing a concept entirely, set its status to superseded and its superseded_by to the new concept's path (rule 8).
3. ENRICH OVER CREATE: a fact that belongs to an existing concept gets patched into it; only a distinct entity gets its own concept.
4. LINK BOTH WAYS: new concepts must be wired into the graph — link related concepts and patch them to reference back.
5. Finish with a one-sentence summary of what changed and the paths touched."
        }
        AgentMode::Chat => {
            "## Your task mode: CHAT

You are conversing with the user about their knowledge base. Answer questions (search and read first, cite paths), and when the user asks to record or change knowledge, use the write tools following the MUTATE protocol. Be concise."
        }
    }
}

/// Build the system prompt (v1's OKF operating rules, adapted).
pub async fn build_system_prompt(
    store: &ConceptStore<'_>,
    mode: AgentMode,
) -> Result<String, AgentError> {
    let entries = store.list().await?;
    let mut types: Vec<String> = entries
        .iter()
        .map(|e| e.concept_type.clone())
        .filter(|t| !t.is_empty())
        .collect();
    types.sort();
    types.dedup();
    let tree = if entries.len() > LARGE_BUNDLE_THRESHOLD {
        format!(
            "(large bundle: {} concepts — use search/read tools to navigate)",
            entries.len()
        )
    } else {
        format_tree(store).await?
    };
    let types_list = if types.is_empty() {
        "(none yet — you set the precedent; choose short, reusable names)".to_string()
    } else {
        types.join(", ")
    };
    let rules = operating_rules(&types_list);
    let mode_text = mode_section(mode);
    Ok(format!(
        "You are the Knowledge Keeper — an agent that manages a knowledge base conforming to the Open Knowledge Format (OKF).

## The OKF format

- The knowledge base is a directory tree of markdown files; every concept is one .md file with YAML frontmatter.
- The only REQUIRED frontmatter field is `type` (free-form, e.g. \"Decision\", \"How-To\", \"Playbook\"). Recommended: `title`, `description` (one line), `resource`, `tags` (list).
- `index.md` and `log.md` are RESERVED — never create concepts with those names.
- Cross-link related concepts with bundle-relative markdown links: `[Customers table](/tables/customers.md)`. Link liberally; broken links are tolerated.
- **Book library (card catalog)**: some shelves catalog books. A `Book` or `Chapter` concept has a small summary body and a `resource: book://<slug>#<anchor>` field — the FULL text lives in the library stacks, NOT in the concept body. To read a chapter's full text, call the `read_passage` tool with that `resource`. Never assume a chapter concept's body is the whole chapter.

## Operating rules

{rules}

## Current bundle layout

{tree}

{mode_text}"
    ))
}

/// A 'static JSON schema, cached per distinct schema (the tool set is
/// fixed and tiny — each schema is built and leaked at most once per
/// process, no matter how many agent runs execute).
fn static_schema(v: serde_json::Value) -> &'static serde_json::Value {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<u64, &'static serde_json::Value>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let key = serde_json::to_string(&v).map(|s| {
        // Simple content hash: fnv over the serialized form.
        s.bytes().fold(1469598103934665603u64, |h, b| {
            (h ^ b as u64).wrapping_mul(1099511628211)
        })
    });
    let Ok(key) = key else {
        return Box::leak(Box::new(v));
    };
    let mut guard = cache.lock().unwrap();
    if let Some(existing) = guard.get(&key) {
        return existing;
    }
    let leaked: &'static serde_json::Value = Box::leak(Box::new(v));
    guard.insert(key, leaked);
    leaked
}

/// Tool specs (JSON schemas) for the read-only set. Built once per
/// process (OnceLock) — every agent run clones the cheap Vec.
pub fn read_tool_specs() -> Vec<crate::llm::ToolSpec<'static>> {
    static SPECS: std::sync::OnceLock<Vec<crate::llm::ToolSpec<'static>>> =
        std::sync::OnceLock::new();
    SPECS
        .get_or_init(|| {
            vec![
                tool_spec(
                    "search_knowledge",
                    "Search the knowledge base by keywords. Returns ranked hits with paths and snippets. NOTE: matching is keyword-based, not semantic — a miss does NOT mean the knowledge is absent.",
                    static_schema(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "query": { "type": "string", "description": "Keywords to search for" },
                            "include_inactive": {"type": "boolean", "description": "Include retired/superseded concepts (history queries). Default false."}
                        },
                        "required": ["query"]
                    })),
                ),
                tool_spec(
                    "read_passage",
                    "Read a full passage from a book in the library stacks. A catalog `Book`/`Chapter` concept has a small summary body and a `resource` field like `book://<slug>#<anchor>` — call this with that resource to fetch the full chapter/section text on demand. Do NOT assume a chapter concept's body is the whole chapter; it is only a summary — use read_passage for the full text.",
                    static_schema(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "resource": { "type": "string", "description": "A book://<slug>#<anchor> URI, taken from a concept's `resource` field" }
                        },
                        "required": ["resource"]
                    })),
                ),
                tool_spec(
                    "read_concept",
                    "Read one concept document in full: frontmatter and markdown body.",
                    static_schema(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Bundle-relative path starting with /, ending in .md" }
                        },
                        "required": ["path"]
                    })),
                ),
                tool_spec(
                    "list_directory",
                    "List the bundle's concepts with types and titles. Use to understand structure and decide where new concepts belong.",
                    static_schema(serde_json::json!({ "type": "object", "properties": {} })),
                ),
                tool_spec(
                    "lint_knowledge",
                    "Graph health check: orphaned concepts (nothing links to them) and broken links.",
                    static_schema(serde_json::json!({ "type": "object", "properties": {} })),
                ),
            ]
        })
        .clone()
}

/// Tool specs for the full (read + write) set. Built once per process.
pub fn all_tool_specs() -> Vec<crate::llm::ToolSpec<'static>> {
    static SPECS: std::sync::OnceLock<Vec<crate::llm::ToolSpec<'static>>> =
        std::sync::OnceLock::new();
    SPECS
        .get_or_init(|| {
            let mut tools = read_tool_specs();
            tools.push(tool_spec(
                "write_concept",
                "Create a new concept or fully overwrite an existing one. Frontmatter must include a non-empty 'type'.",
                static_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Bundle-relative path starting with /, ending in .md" },
                        "frontmatter": {
                            "type": "object",
                            "properties": {
                                "type": { "type": "string", "description": "Concept kind, e.g. 'Decision'. Required." },
                                "title": { "type": "string" },
                                "description": { "type": "string" },
                                "tags": { "type": "array", "items": { "type": "string" } }
                            },
                            "required": ["type"]
                        },
                        "body": { "type": "string", "description": "Markdown body (no frontmatter block)" }
                    },
                    "required": ["path", "frontmatter", "body"]
                })),
            ));
            tools.push(tool_spec(
                "patch_concept",
                "Targeted update of an existing concept: merge frontmatter keys (null deletes a key) and/or replace one top-level '# Section' body section, or replace the whole body. Prefer this over write_concept for small edits.",
                static_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Bundle-relative path starting with /, ending in .md" },
                        "frontmatter": { "type": "object", "description": "Frontmatter keys to merge; set a key to null to remove it" },
                        "replace_section": {
                            "type": "object",
                            "properties": {
                                "heading": { "type": "string", "description": "Top-level heading name, e.g. 'Schema'" },
                                "content": { "type": "string", "description": "New content for that section" }
                            }
                        },
                        "replace_body": { "type": "string", "description": "Replace the entire markdown body (frontmatter untouched)" }
                    },
                    "required": ["path"]
                })),
            ));
            tools.push(tool_spec(
                "delete_concept",
                "Permanently delete a concept. Prefer lifecycle retirement instead: patch the concept with status: retired (or status: superseded + superseded_by pointing at the replacement). Use delete_concept only when content is wrong/harmful or deletion was explicitly requested.",
                static_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Bundle-relative path starting with /, ending in .md" }
                    },
                    "required": ["path"]
                })),
            ));
            tools
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_replace_replaces_existing() {
        let body = "# Title\n\nIntro.\n\n# Schema\n\nold schema\n\n# Other\n\nstuff\n";
        let out = replace_section(body, "Schema", "new schema");
        assert!(out.contains("new schema"));
        assert!(!out.contains("old schema"));
        assert!(out.contains("# Other"));
        assert!(out.contains("Intro."));
    }

    #[test]
    fn section_replace_appends_when_missing() {
        let body = "# Title\n\nIntro.\n";
        let out = replace_section(body, "Schema", "the schema");
        assert!(out.contains("# Schema"));
        assert!(out.contains("the schema"));
        assert!(out.contains("Intro."));
    }

    // ---- lifecycle prompt / tool-description content pins (spec §3.1) ----
    //
    // These strings are copy-pinned from the lifecycle spec: the rule,
    // sentence and description below must match it byte-for-byte.

    #[test]
    fn rule8_pins_lifecycle_instructions() {
        let rules = operating_rules("Note, Decision");
        assert!(rules.contains("status: superseded"));
        assert!(rules.contains("superseded_by"));
        assert!(
            rules.contains(
                "8. DEPRECATE, DON'T DELETE. When knowledge is outdated: patch the OLD concept — status: superseded + superseded_by: <new-concept-path> — or status: retired. Never leave a live concept contradicting a new one. Use delete_concept only for wrong/harmful content or on explicit request."
            ),
            "rule 8 drifted from the lifecycle wording:\n{rules}"
        );
    }

    #[test]
    fn delete_concept_description_pins_retirement() {
        let spec = spec_json("delete_concept");
        let description = spec["function"]["description"].as_str().unwrap();
        assert!(description.contains("status: retired"));
        assert_eq!(
            description,
            "Permanently delete a concept. Prefer lifecycle retirement instead: patch the concept with status: retired (or status: superseded + superseded_by pointing at the replacement). Use delete_concept only when content is wrong/harmful or deletion was explicitly requested."
        );
    }

    #[test]
    fn search_knowledge_schema_has_include_inactive() {
        let spec = spec_json("search_knowledge");
        let prop = &spec["function"]["parameters"]["properties"]["include_inactive"];
        assert_eq!(prop["type"], "boolean");
        assert_eq!(
            prop["description"],
            "Include retired/superseded concepts (history queries). Default false."
        );
        // History queries stay opt-in: the arg must not be required.
        let required = &spec["function"]["parameters"]["required"];
        assert!(
            required
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r != "include_inactive"),
            "include_inactive must default false, not be required"
        );
    }

    #[test]
    fn mutate_step2_pins_supersede_hint() {
        let section = mode_section(AgentMode::Mutate);
        assert!(section.contains("2. CHECK FOR CONTRADICTION."));
        assert!(
            section.contains(
                "When replacing a concept entirely, set its status to superseded and its superseded_by to the new concept's path (rule 8)."
            ),
            "MUTATE step 2 lost its supersede hint:\n{section}"
        );
    }

    #[test]
    fn include_inactive_arg_defaults_false() {
        for args in [
            serde_json::json!({}),
            serde_json::json!({"query": "x"}),
            serde_json::json!({"include_inactive": null}),
            serde_json::json!({"include_inactive": "true"}),
            serde_json::json!({"include_inactive": 1}),
            serde_json::json!({"include_inactive": false}),
        ] {
            // Post-guard the payload is always an object; `.as_object()`
            // is the same conversion the execute path applies.
            assert!(
                !include_inactive_arg(args.as_object().unwrap()),
                "expected false for {args}"
            );
        }
        assert!(include_inactive_arg(
            serde_json::json!({ "include_inactive": true })
                .as_object()
                .unwrap()
        ));
    }

    // ---- patch_concept lifecycle merge (F1) ----
    //
    // Rule 8/MUTATE step 2 have the model supersede/retire by patching
    // `status` / `superseded_by` / `valid_to` — the merge must accept
    // them. String-or-null mirrors `title`: a JSON string sets the
    // field, `null` clears it; validation happens at put (never here).

    /// Patch tool call in wire shape (`ToolCall`'s private serde
    /// defaults reconstruct it), executed through the real dispatch.
    async fn run_patch(
        cs: &ConceptStore<'_>,
        args: serde_json::Value,
    ) -> Result<(String, Option<String>), AgentError> {
        let call: crate::llm::ToolCall = serde_json::from_value(serde_json::json!({
            "id": "call-patch-test",
            "function": {
                "name": "patch_concept",
                "arguments": serde_json::to_string(&args).unwrap(),
            }
        }))
        .unwrap();
        let scopes = AgentScopes {
            skills: None,
            library: None,
            read_stack_text: None,
            visible_slugs: None,
            trace: None,
        };
        let mut recorder = crate::trace::TraceRecorder::new();
        execute_tool_tracked(cs, &scopes, &call, true, &mut recorder).await
    }

    /// Same store setup as the prompt test below, plus a `/old.md` note.
    async fn patch_store_setup() -> ConceptStore<'static> {
        let dir = tempfile::tempdir().unwrap();
        let store = Box::leak(Box::new(
            mycelium_store::Store::open(dir.path()).await.unwrap(),
        ));
        let _dir = Box::leak(Box::new(dir));
        let master = mycelium_crypto::generate_master_key();
        let cs = mycelium_store::ConceptStore::for_user(&*store, uuid::Uuid::new_v4(), master);
        let concept = Concept::new(
            mycelium_core::concept::Frontmatter {
                concept_type: "Note".into(),
                ..Default::default()
            },
            "old fact".into(),
            "/old.md".into(),
        );
        cs.put(&concept).await.unwrap();
        cs
    }

    #[tokio::test]
    async fn patch_concept_merges_lifecycle_frontmatter_fields() {
        let cs = patch_store_setup().await;
        let (result, written) = run_patch(
            &cs,
            serde_json::json!({
                "path": "/old.md",
                "frontmatter": { "status": "superseded", "superseded_by": "/new.md" },
            }),
        )
        .await
        .expect("patch with lifecycle fields must succeed");
        assert_eq!(
            result,
            serde_json::json!({ "patched": "/old.md" }).to_string()
        );
        assert_eq!(written.as_deref(), Some("/old.md"));
        let stored = cs.get("/old.md").await.unwrap();
        assert_eq!(stored.frontmatter.status.as_deref(), Some("superseded"));
        assert_eq!(stored.frontmatter.superseded_by.as_deref(), Some("/new.md"));
        assert_eq!(stored.body, "old fact"); // merge touched frontmatter only
    }

    #[tokio::test]
    async fn patch_concept_null_status_clears_the_field() {
        let cs = patch_store_setup().await;
        let superseded = Concept::new(
            mycelium_core::concept::Frontmatter {
                concept_type: "Note".into(),
                status: Some("superseded".into()),
                superseded_by: Some("/new.md".into()),
                ..Default::default()
            },
            "old fact".into(),
            "/old.md".into(),
        );
        cs.put(&superseded).await.unwrap();
        run_patch(
            &cs,
            serde_json::json!({ "path": "/old.md", "frontmatter": { "status": null } }),
        )
        .await
        .expect("null status merge must succeed");
        let stored = cs.get("/old.md").await.unwrap();
        assert_eq!(stored.frontmatter.status, None); // cleared, field dropped
        // The sibling field is untouched — null clears only its own key.
        assert_eq!(stored.frontmatter.superseded_by.as_deref(), Some("/new.md"));
    }

    #[tokio::test]
    async fn patch_concept_rejects_invalid_status() {
        let cs = patch_store_setup().await;
        let err = run_patch(
            &cs,
            serde_json::json!({ "path": "/old.md", "frontmatter": { "status": "Bogus" } }),
        )
        .await
        .expect_err("bad status must fail the put, not merge to a success reply");
        match &err {
            AgentError::Store(ConceptStoreError::Parse(
                mycelium_core::ConceptError::InvalidLifecycleStatus { value },
            )) => assert_eq!(value, "Bogus"),
            other => panic!("expected Put(Parse(InvalidLifecycleStatus)), got {other:?}"),
        }
        // The error result reaching the model names the bad value —
        // enough for recovery; it must not be a success reply.
        assert!(err.to_string().contains("invalid lifecycle status: Bogus"));
    }

    #[tokio::test]
    async fn patch_concept_rejects_bad_superseded_by_shape() {
        let cs = patch_store_setup().await;
        let err = run_patch(
            &cs,
            serde_json::json!({
                "path": "/old.md",
                "frontmatter": { "superseded_by": "not-a-valid-shape" },
            }),
        )
        .await
        .expect_err("bad superseded_by shape must fail the put");
        match &err {
            AgentError::Store(ConceptStoreError::Parse(
                mycelium_core::ConceptError::InvalidSupersededBy { value },
            )) => assert_eq!(value, "not-a-valid-shape"),
            other => panic!("expected Put(Parse(InvalidSupersededBy)), got {other:?}"),
        }
    }

    /// The assembled prompt (not just its building blocks) carries the
    /// lifecycle rule and the MUTATE hint: the builder formats FROM the
    /// rules const and the mode section, so this pins the wiring too.
    #[tokio::test]
    async fn system_prompt_formats_lifecycle_rule_and_supersede_hint() {
        let dir = tempfile::tempdir().unwrap();
        let store = mycelium_store::Store::open(dir.path()).await.unwrap();
        let master = mycelium_crypto::generate_master_key();
        let cs = mycelium_store::ConceptStore::for_user(&store, uuid::Uuid::new_v4(), master);

        let query_prompt = build_system_prompt(&cs, AgentMode::Query).await.unwrap();
        assert!(query_prompt.contains("## Operating rules"));
        assert!(query_prompt.contains("8. DEPRECATE, DON'T DELETE."));
        assert!(query_prompt.contains("status: superseded + superseded_by: <new-concept-path>"));
        assert!(query_prompt.contains("Types currently in the bundle:"));
        // The placeholder actually got substituted: a drifted replace
        // target would pass every contains() check while serving the
        // raw slot to the model.
        assert!(!query_prompt.contains("{types_list}"));
        assert!(!query_prompt.contains("Prefer tagging `deprecated`"));

        let mutate_prompt = build_system_prompt(&cs, AgentMode::Mutate).await.unwrap();
        assert!(mutate_prompt.contains("2. CHECK FOR CONTRADICTION. If the new knowledge conflicts with an existing concept, update it — never leave two contradictory claims standing. When replacing a concept entirely, set its status to superseded and its superseded_by to the new concept's path (rule 8)."));
    }

    /// The serialized (wire-shape) spec of a tool by name — `ToolSpec`
    /// fields are private, the JSON the model sees is not.
    fn spec_json(name: &str) -> serde_json::Value {
        all_tool_specs()
            .iter()
            .map(|spec| serde_json::to_value(spec).unwrap())
            .find(|spec| spec["function"]["name"] == name)
            .unwrap_or_else(|| panic!("no tool spec named {name}"))
    }
    // ---------- execute_tool_tracked args handling (drain-wedge panic) ----------

    /// Fresh temp-dir store, mirroring `tests/queue_worker_integration.rs`.
    async fn test_store() -> (tempfile::TempDir, mycelium_store::Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = mycelium_store::Store::open(dir.path()).await.unwrap();
        (dir, store)
    }

    /// AgentScopes with no extra stores (bare user-scope run).
    fn bare_scopes<'a>() -> AgentScopes<'a> {
        AgentScopes {
            skills: None,
            library: None,
            read_stack_text: None,
            visible_slugs: None,
            trace: None,
        }
    }

    /// A model-shaped tool call (the wire `type` field is private on
    /// `ToolCall`, so build it through serde like the llm layer does).
    fn tool_call(name: &str, args: &str) -> ToolCall {
        serde_json::from_value(serde_json::json!({
            "id": "0",
            "type": "function",
            "function": { "name": name, "arguments": args },
        }))
        .unwrap()
    }

    // A bare-string args payload must yield a recoverable tool error, not a panic.
    #[tokio::test]
    async fn non_object_tool_args_become_bad_tool_args_error() {
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let cs = ConceptStore::for_user(&store, uuid::Uuid::new_v4(), master);
        let scopes = bare_scopes();
        let call = tool_call("search_knowledge", "\"just a string\"");
        let mut rec = crate::trace::TraceRecorder::new();
        let res = execute_tool_tracked(&cs, &scopes, &call, true, &mut rec).await;
        assert!(matches!(
            res,
            Err(AgentError::BadToolArgs { ref name, .. }) if name == "search_knowledge"
        ));
    }

    // patch_concept with replace_section lacking "heading"/"content" must be a
    // lenient success — the Value-on-object-miss semantics the code intends
    // (missing keys resolve to the same empty strings an explicit
    // `{"heading":"","content":""}` produces) — NOT the Map-index panic that
    // wedged the drain worker. (replace_section with an empty heading matches
    // no section and appends its stub — pre-existing semantics, pinned here.)
    #[tokio::test]
    async fn patch_replace_section_missing_keys_is_lenient() {
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let cs = ConceptStore::for_user(&store, uuid::Uuid::new_v4(), master);
        let scopes = bare_scopes();
        let body = "# A\n\nalpha\n\n# B\n\nbeta\n";
        cs.put(&Concept::new(
            mycelium_core::concept::Frontmatter {
                concept_type: "Note".into(),
                title: Some("Probe".into()),
                ..Default::default()
            },
            body.to_string(),
            "/probe.md".into(),
        ))
        .await
        .unwrap();
        let call = tool_call(
            "patch_concept",
            r#"{"path":"/probe.md","replace_section":{}}"#,
        );
        let mut rec = crate::trace::TraceRecorder::new();
        let (out, written) = execute_tool_tracked(&cs, &scopes, &call, true, &mut rec)
            .await
            .expect("missing replace_section keys must be a lenient success, not an error");
        assert_eq!(
            out,
            serde_json::json!({ "patched": "/probe.md" }).to_string()
        );
        assert_eq!(written.as_deref(), Some("/probe.md"));
        let patched = cs.get("/probe.md").await.unwrap();
        assert_eq!(patched.body, replace_section(body, "", ""));
    }
}
