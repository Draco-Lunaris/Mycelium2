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

/// Maximum agentic steps per run (v1: MAX_STEPS = 12).
pub const MAX_STEPS: u32 = 12;
/// Bundles larger than this get a compact tree summary in the prompt
/// (v1: LARGE_BUNDLE_THRESHOLD = 300).
const LARGE_BUNDLE_THRESHOLD: usize = 300;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("store error: {0}")]
    Store(#[from] ConceptStoreError),
    #[error("LLM error: {0}")]
    Llm(#[from] crate::llm::LlmError),
    #[error("agent exceeded {MAX_STEPS} steps without finishing")]
    StepCapExceeded,
    #[error("tool {name} returned invalid arguments: {reason}")]
    BadToolArgs { name: String, reason: String },
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
pub async fn run_query(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    question: &str,
) -> Result<QueryResult, AgentError> {
    let system = build_system_prompt(store, AgentMode::Query).await?;
    let tools = read_tool_specs();
    let answer = agent_loop(
        client, store, &system, question, &tools, false, // read-only
        0.2,
    )
    .await?;
    Ok(QueryResult { answer, steps: 0 })
}

/// Run the agent in mutation mode (write tools enabled).
pub async fn run_mutation(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    instruction: &str,
) -> Result<MutationResult, AgentError> {
    let system = build_system_prompt(store, AgentMode::Mutate).await?;
    let tools = all_tool_specs();
    let (summary, files) =
        agent_loop_tracked(client, store, &system, instruction, &tools, true, 0.2).await?;
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
    history: &[ConversationTurn],
) -> Result<String, AgentError> {
    run_chat_streaming(client, store, history, None).await
}

/// Streaming chat: same loop as run_chat, but emits progress events to
/// the caller's channel (the web layer streams them as SSE). The
/// final reply is still returned.
pub async fn run_chat_streaming(
    client: &LlmClient,
    store: &ConceptStore<'_>,
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
    // Seed the loop with the provided history (the last turn is the
    // new user message).
    let mut conversation: Vec<ConversationTurn> = history.to_vec();
    for _ in 0..MAX_STEPS {
        let output = client
            .chat_with_tools(&system, &conversation, &tools, 0.2)
            .await?;
        match output {
            StepOutput::Text(answer) => {
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
                    let result = match execute_tool_tracked(store, &call, true).await {
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
    emit(AgentEvent::Failed(err.clone()));
    Err(AgentError::StepCapExceeded)
}

/// The core loop shared by query and mutation runs. Returns the final
/// text. `allow_writes` gates the write tools (query mode is read-only).
async fn agent_loop(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    system: &str,
    prompt: &str,
    tools: &[crate::llm::ToolSpec<'_>],
    allow_writes: bool,
    temperature: f32,
) -> Result<String, AgentError> {
    let (text, _) = agent_loop_tracked(
        client,
        store,
        system,
        prompt,
        tools,
        allow_writes,
        temperature,
    )
    .await?;
    Ok(text)
}

/// The core loop, also returning the set of files written (mutation
/// mode). Tool results are appended as conversation turns.
async fn agent_loop_tracked(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    system: &str,
    prompt: &str,
    tools: &[crate::llm::ToolSpec<'_>],
    allow_writes: bool,
    temperature: f32,
) -> Result<(String, Vec<String>), AgentError> {
    let mut conversation = vec![ConversationTurn::User(prompt.to_string())];
    let mut files_changed: Vec<String> = Vec::new();
    for _ in 0..MAX_STEPS {
        let output = client
            .chat_with_tools(system, &conversation, tools, temperature)
            .await?;
        match output {
            StepOutput::Text(answer) => return Ok((answer, files_changed)),
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
                    let (result, written) =
                        match execute_tool_tracked(store, &call, allow_writes).await {
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
    Err(AgentError::StepCapExceeded)
}

/// Execute one tool call; returns (JSON result for the model, path
/// written when the tool mutated a concept).
async fn execute_tool_tracked(
    store: &ConceptStore<'_>,
    call: &ToolCall,
    allow_writes: bool,
) -> Result<(String, Option<String>), AgentError> {
    let name = call.function.name.as_str();
    let args: serde_json::Value =
        serde_json::from_str(&call.function.arguments).map_err(|e| AgentError::BadToolArgs {
            name: name.to_string(),
            reason: e.to_string(),
        })?;
    match name {
        "search_knowledge" => {
            let query = args["query"].as_str().unwrap_or_default();
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
            q.include_global = false;
            let hits = store.search(&q).await?;
            let hits_json: Vec<serde_json::Value> = hits
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "path": h.concept_path,
                        "title": h.title,
                        "snippet": h.snippet,
                        "score": h.score,
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
            let path = args["path"].as_str().unwrap_or_default();
            let concept = store.get(path).await?;
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
        "write_concept" => {
            if !allow_writes {
                return Err(AgentError::BadToolArgs {
                    name: name.into(),
                    reason: "write tool in read-only mode".into(),
                });
            }
            let path = args["path"].as_str().unwrap_or_default().to_string();
            let body = args["body"].as_str().unwrap_or_default();
            let concept_type = args["frontmatter"]["type"]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .unwrap_or("Note")
                .to_string();
            let title = args["frontmatter"]["title"].as_str().map(String::from);
            let description = args["frontmatter"]["description"]
                .as_str()
                .map(String::from);
            let tags: Vec<String> = args["frontmatter"]["tags"]
                .as_array()
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
            let path = args["path"].as_str().unwrap_or_default();
            let mut concept = store.get(path).await?;
            // Frontmatter merge: null deletes a key (v1 semantics).
            if let Some(fm) = args["frontmatter"].as_object() {
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
            if let Some(section) = args["replace_section"].as_object() {
                let heading = section["heading"].as_str().unwrap_or_default();
                let content = section["content"].as_str().unwrap_or_default();
                concept.body = replace_section(&concept.body, heading, content);
            } else if let Some(body) = args["replace_body"].as_str() {
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
            let path = args["path"].as_str().unwrap_or_default();
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
    }
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
    let mode_section = match mode {
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
2. CHECK FOR CONTRADICTION. If the new knowledge conflicts with an existing concept, update it — never leave two contradictory claims standing.
3. ENRICH OVER CREATE: a fact that belongs to an existing concept gets patched into it; only a distinct entity gets its own concept.
4. LINK BOTH WAYS: new concepts must be wired into the graph — link related concepts and patch them to reference back.
5. Finish with a one-sentence summary of what changed and the paths touched."
        }
        AgentMode::Chat => {
            "## Your task mode: CHAT

You are conversing with the user about their knowledge base. Answer questions (search and read first, cite paths), and when the user asks to record or change knowledge, use the write tools following the MUTATE protocol. Be concise."
        }
    };
    Ok(format!(
        "You are the Knowledge Keeper — an agent that manages a knowledge base conforming to the Open Knowledge Format (OKF).

## The OKF format

- The knowledge base is a directory tree of markdown files; every concept is one .md file with YAML frontmatter.
- The only REQUIRED frontmatter field is `type` (free-form, e.g. \"Decision\", \"How-To\", \"Playbook\"). Recommended: `title`, `description` (one line), `resource`, `tags` (list).
- `index.md` and `log.md` are RESERVED — never create concepts with those names.
- Cross-link related concepts with bundle-relative markdown links: `[Customers table](/tables/customers.md)`. Link liberally; broken links are tolerated.

## Operating rules

1. SEARCH FIRST. Before adding anything, search for existing concepts the new knowledge relates to.
2. ENRICH OVER CREATE. A fact that is an attribute of an existing concept gets patched INTO that concept — not filed as its own concept.
3. LINK BOTH WAYS. A new concept must be wired into the graph: link it to related concepts AND patch those to reference it back.
4. REUSE TYPES. Prefer a type already in use. Types currently in the bundle: {types_list}.
5. PLACE DELIBERATELY. Choose directories by subject area; reuse existing directories; short kebab-case filenames.
6. WRITE FOR THE NEXT READER. One-line descriptions; concise, factual, self-contained bodies.
7. PREFER PATCH OVER REWRITE. Use patch_concept for small changes.
8. DEPRECATE, DON'T DELETE. Prefer tagging `deprecated` over delete_concept; delete only when wrong/harmful or explicitly asked.
9. CITE WHEN ANSWERING. Ground every claim in concepts you actually read; list their paths. If the knowledge base doesn't contain the answer, say so plainly — never invent knowledge.

## Current bundle layout

{tree}

{mode_section}"
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
                            "query": { "type": "string", "description": "Keywords to search for" }
                        },
                        "required": ["query"]
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
                "Permanently delete a concept. Prefer deprecation (tag 'deprecated' via patch_concept) unless content is wrong/harmful or deletion was explicitly requested.",
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
}
