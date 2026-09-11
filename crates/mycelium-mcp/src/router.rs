//! axum router for the MCP Streamable HTTP endpoint (`/mcp`): bearer
//! API-key auth middleware in front of rmcp's stateless
//! `StreamableHttpService`.

use std::sync::Arc;

use axum::Router;
use axum::middleware as axum_mw;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

use crate::McpState;
use crate::auth::require_bearer;
use crate::handler::MyceliumMcpServer;

/// The rmcp Streamable HTTP service for the mycelium2 MCP server.
///
/// The 2026-07-28 protocol posture (per DESIGN):
/// - `legacy_session_mode = false` — no sessions, no initialize handshake.
/// - `stateless_protocol_metadata_required = true` — every request must
///   carry `MCP-Protocol-Version` + `_meta` per-request signals.
/// - `json_response = true` — simple request/response over JSON (SSE
///   fallback only if a handler emits intermediate messages).
/// - `allowed_hosts` disabled: rmcp's default allowlist is loopback-only,
///   which would 403 any production deployment on a real hostname. The
///   DNS-rebinding threat that allowlist defends against is a
///   browser-origin attack; this endpoint is (a) HTTPS-only with
///   certificate validation, (b) bearer-token authenticated with a
///   secret a rebinding page cannot know, and (c) not reachable from
///   ambient browser credentials. The outer HTTPS listener is the
///   host-policy boundary.
pub fn mcp_service(
    state: &McpState,
    shutdown: tokio_util::sync::CancellationToken,
) -> StreamableHttpService<MyceliumMcpServer, LocalSessionManager> {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_stateless_protocol_metadata_required(true)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .disable_allowed_hosts()
        .with_cancellation_token(shutdown);

    let inner_state = McpState {
        store: Arc::clone(&state.store),
        service_key: Arc::clone(&state.service_key),
        master_keys: Arc::clone(&state.master_keys),
        config: Arc::clone(&state.config),
    };
    let service_key = (*state.service_key).clone();
    StreamableHttpService::new(
        move || {
            Ok(MyceliumMcpServer::new(McpState {
                store: Arc::clone(&inner_state.store),
                service_key: Arc::new(service_key.clone()),
                master_keys: Arc::clone(&inner_state.master_keys),
                config: Arc::clone(&inner_state.config),
            }))
        },
        Default::default(),
        config,
    )
}

/// Build the `/mcp` router: bearer auth middleware → stateless rmcp
/// service. Returns `Router<S>` generic over the parent router's state —
/// the auth middleware's state (the SqlitePool) is baked in, so the
/// router composes into any parent.
pub fn mcp_router<S: Clone + Send + Sync + 'static>(state: McpState) -> Router<S> {
    mcp_router_with_shutdown(state, tokio_util::sync::CancellationToken::new())
}

/// `mcp_router` with a shutdown token wired into the service (cancels
/// in-flight MCP requests on graceful shutdown).
pub fn mcp_router_with_shutdown<S: Clone + Send + Sync + 'static>(
    state: McpState,
    shutdown: tokio_util::sync::CancellationToken,
) -> Router<S> {
    let pool = state.store.pool().clone();
    Router::new()
        .nest_service("/mcp", mcp_service(&state, shutdown))
        .layer(axum_mw::from_fn_with_state(pool, require_bearer))
}
