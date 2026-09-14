//! MCP bearer API-key authentication: resolve `Authorization: Bearer
//! myc2-…` to the calling user, injected into request extensions so rmcp
//! tool handlers can read it from `http::request::Parts`.

use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use mycelium_auth::api_key::ApiKeyManager;
use uuid::Uuid;

/// The authenticated MCP caller (injected into request extensions).
#[derive(Debug, Clone)]
pub struct McpUser {
    pub user_id: Uuid,
    pub username: String,
    pub role: mycelium_auth::rbac::Role,
}

/// Axum middleware: require a valid bearer API key on every /mcp request.
/// 401 (with a `WWW-Authenticate` challenge per the MCP authorization spec)
/// when the token is missing or invalid.
pub async fn require_bearer(
    axum::extract::State(pool): axum::extract::State<sqlx::SqlitePool>,
    request: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Response {
    let (mut parts, body) = request.into_parts();
    match resolve_user(&parts, &pool).await {
        Some(user) => {
            parts.extensions.insert(user);
            let request = axum::http::Request::from_parts(parts, body);
            next.run(request).await
        }
        None => (
            StatusCode::UNAUTHORIZED,
            [
                (
                    header::WWW_AUTHENTICATE,
                    HeaderValue::from_static("Bearer realm=\"mycelium2\""),
                ),
                (header::CONTENT_TYPE, HeaderValue::from_static("text/plain")),
            ],
            "unauthorized: missing or invalid API key",
        )
            .into_response(),
    }
}

/// Resolve the bearer token in `parts` to a user via the API-key table.
async fn resolve_user(parts: &Parts, pool: &sqlx::SqlitePool) -> Option<McpUser> {
    let auth = parts.headers.get(header::AUTHORIZATION)?;
    let auth = auth.to_str().ok()?;
    let token = auth.strip_prefix("Bearer ")?;
    if token.is_empty() {
        return None;
    }
    let manager = ApiKeyManager::new(pool.clone());
    let record = manager.verify(token).await.ok()?;
    let users = mycelium_auth::users::UserStore::new(pool.clone());
    let user = users.get_by_id(record.user_id).await.ok()?;
    Some(McpUser {
        user_id: user.id,
        username: user.username,
        role: user.role,
    })
}

/// Extract the authenticated user from rmcp's injected `http::request::Parts`.
pub fn user_from_parts(parts: &Parts) -> Option<McpUser> {
    parts.extensions.get::<McpUser>().cloned()
}
