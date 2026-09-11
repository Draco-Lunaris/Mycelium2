//! MCP server handler: rmcp `ServerHandler` exposing the mycelium2 tools
//! over the 2026-07-28 stateless protocol.

use std::borrow::Cow;

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ProtocolVersion, ServerInfo};
use rmcp::tool;
use rmcp::tool_handler;
use rmcp::tool_router;
use rmcp::{ErrorData, ServerHandler};

use crate::McpState;
use crate::tools;

/// The MCP server. One instance per request (stateless); the shared
/// `McpState` travels in the struct, the per-request user identity
/// travels in rmcp's injected `http::request::Parts`.
#[derive(Clone)]
pub struct MyceliumMcpServer {
    state: McpState,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

impl MyceliumMcpServer {
    pub fn new(state: McpState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl MyceliumMcpServer {
    /// Answer a question from the calling user's private knowledge
    /// base. The librarian agent synthesizes a grounded answer (with
    /// sources); without an LLM backend the tool returns ranked
    /// keyword-search hits instead.
    #[tool(
        name = "mycelium2_memory_query",
        description = "Answer a natural-language question from the user's private knowledge base. The librarian agent searches, reads the relevant concepts, and synthesizes a grounded answer citing bundle paths. (Without an LLM backend configured, returns ranked keyword-search hits instead.)"
    )]
    async fn memory_query(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        args: Parameters<tools::MemoryQueryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = tools::caller(&parts)?;
        let output = tools::memory_query(&self.state, &user, &args.0)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "memory tool internal error");
                ErrorData::internal_error("internal storage error", None)
            })?;
        match output {
            tools::QueryOutput::Answer(a) => {
                Ok(CallToolResult::success(vec![ContentBlock::text(a.answer)]))
            }
            tools::QueryOutput::Hits(results) => {
                if results.is_empty() {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        "no concepts matched the question",
                    )]));
                }
                let text = results
                    .iter()
                    .map(|r| {
                        format!(
                            "## {}\npath: {}\nscore: {:.2}\n{}\n",
                            r.title, r.concept_path, r.score, r.snippet
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
            }
        }
    }

    /// Record new knowledge in the calling user's private bundle.
    #[tool(
        name = "mycelium2_memory_add",
        description = "Record new knowledge (facts, docs, decisions, runbooks) in the user's private knowledge base."
    )]
    async fn memory_add(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        args: Parameters<tools::MemoryAddArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = tools::caller(&parts)?;
        match tools::memory_add(&self.state, &user, &args.0).await {
            Ok(path) => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "recorded at {path}"
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                tools::render_store_error(&e),
            )])),
        }
    }

    /// Apply a change to existing knowledge in the calling user's bundle.
    #[tool(
        name = "mycelium2_memory_update",
        description = "Apply a change to existing knowledge: correct a fact, deprecate a concept, or restructure."
    )]
    async fn memory_update(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        args: Parameters<tools::MemoryUpdateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = tools::caller(&parts)?;
        match tools::memory_update(&self.state, &user, &args.0).await {
            Ok(path) => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "updated {path}"
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                tools::render_store_error(&e),
            )])),
        }
    }

    /// Report the health of the calling user's bundle graph.
    #[tool(
        name = "mycelium2_memory_status",
        description = "Report the health of the user's knowledge base: concept, edge, broken-link, and orphan counts."
    )]
    async fn memory_status(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        _args: Parameters<tools::MemoryStatusArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = tools::caller(&parts)?;
        let health = tools::memory_status(&self.state, &user)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "memory tool internal error");
                ErrorData::internal_error("internal storage error", None)
            })?;
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "concepts: {}\nedges: {}\nbroken links: {}\norphans: {}",
            health.concept_count, health.edge_count, health.broken_link_count, health.orphan_count
        ))]))
    }

    /// Health-check and repair the calling user's bundle graph.
    #[tool(
        name = "mycelium2_memory_maintain",
        description = "Health-check and repair the user's knowledge-base graph: wire orphaned concepts into related concepts and flag broken links."
    )]
    async fn memory_maintain(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        _args: Parameters<tools::MemoryMaintainArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = tools::caller(&parts)?;
        match tools::memory_maintain(&self.state, &user).await {
            Ok(summary) => Ok(CallToolResult::success(vec![ContentBlock::text(summary)])),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                tools::render_store_error(&e),
            )])),
        }
    }

    /// Fetch a skill by name — private first, then the global shelf.
    #[tool(
        name = "mycelium2_skill_get",
        description = "Fetch a skill by name from the user's private skills, falling back to the global skills shelf."
    )]
    async fn skill_get(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        args: Parameters<tools::SkillGetArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = tools::caller(&parts)?;
        match tools::skill_get(&self.state, &user, &args.0).await {
            Ok(Some(skill)) => {
                let title = skill
                    .frontmatter
                    .title
                    .clone()
                    .unwrap_or_else(|| skill.source_path.clone());
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "# {title}\n\n{}",
                    skill.body
                ))]))
            }
            Ok(None) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "skill {:?} not found",
                args.0.name
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
                tools::render_store_error(&e),
            )])),
        }
    }

    /// List available skills — private plus global.
    #[tool(
        name = "mycelium2_skill_list",
        description = "List available skills from the user's private bundle and the global skills shelf."
    )]
    async fn skill_list(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        _args: Parameters<tools::SkillListArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let user = tools::caller(&parts)?;
        let skills = tools::skill_list(&self.state, &user).await.map_err(|e| {
            tracing::error!(error = %e, "memory tool internal error");
            ErrorData::internal_error("internal storage error", None)
        })?;
        if skills.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "no skills available",
            )]));
        }
        let text = skills
            .iter()
            .map(|s| format!("- {} ({})", s.title, s.path))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MyceliumMcpServer {
    fn get_info(&self) -> ServerInfo {
        use rmcp::model::{Implementation, ServerCapabilities};
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mycelium2", env!("CARGO_PKG_VERSION")))
    }

    /// Advertise ONLY 2026-07-28: this server implements the stateless
    /// per-request-metadata protocol and nothing older.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[ProtocolVersion::V_2026_07_28])
    }
}
