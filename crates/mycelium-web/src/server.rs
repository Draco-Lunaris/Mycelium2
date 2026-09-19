//! HTTPS server assembly: router, TLS listener, HTTP:80 redirect,
//! graceful shutdown.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::middleware as axum_mw;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};

use crate::api;
use crate::cert;
use crate::health;
use crate::middleware;
use crate::state::AppState;

/// Build the full application router.
pub fn build_router(state: AppState) -> Router {
    build_router_with_shutdown(state, tokio_util::sync::CancellationToken::new())
}

/// Build the full application router with a shutdown token wired into
/// the MCP service (cancels in-flight MCP requests on graceful shutdown).
pub fn build_router_with_shutdown(
    state: AppState,
    shutdown: tokio_util::sync::CancellationToken,
) -> Router {
    let api_routes = api::api_routes();

    // MCP endpoint: shares the store + service key + master-key cache
    // with the web state so both layers see the same unwrapped keys.
    let mcp_state = mycelium_mcp::McpState {
        store: Arc::clone(&state.store),
        service_key: Arc::clone(&state.service_key),
        master_keys: Arc::clone(&state.master_keys),
        config: Arc::clone(&state.config),
    };
    // Seed memory (v1 parity): build the global overview once at boot
    // so the first initialize/tools/list already carries it. Refreshed
    // after mutations and ingests. Spawned — the router builder is
    // sync and the seed is best-effort.
    {
        let seed_store = Arc::clone(&state.store);
        let seed_key = Arc::clone(&state.service_key);
        tokio::spawn(async move {
            mycelium_mcp::seed::refresh_seed(&seed_store, &seed_key).await;
        });
    }

    Router::new()
        // Public.
        .route("/login", get(api::login_page).post(api::login_submit))
        .route("/setup", get(api::setup_page).post(api::setup_submit))
        .route("/assets/{*path}", get(serve_asset))
        // MCP (bearer API-key auth inside the MCP router).
        .merge(mycelium_mcp::mcp_router_with_shutdown(mcp_state, shutdown))
        // Authenticated pages.
        .route("/", get(api::home))
        .route("/logout", get(api::logout))
        .route("/concept", get(api::concept_view).post(api::concept_submit))
        .route("/concept/delete", post(api::concept_delete))
        .route("/search", get(api::search_view))
        .route("/graph", get(api::graph_view))
        .route("/skills", get(api::skills_view))
        .route("/books", get(api::books_view))
        .route("/chat", get(api::chat_view))
        .route(
            "/password",
            get(api::password_view).post(api::password_submit),
        )
        .route("/keys", get(api::keys_view).post(api::keys_mint))
        .route("/keys/revoke", post(api::keys_revoke))
        // Admin.
        .route("/admin", get(api::admin_view))
        .route("/admin/users", post(api::admin_create_user))
        .route("/admin/oidc", post(api::admin_save_oidc))
        .route("/admin/llm", post(api::admin_save_llm))
        .route("/admin/upload-limits", post(api::admin_save_upload_limits))
        .route("/admin/security", post(api::admin_save_security))
        .route("/admin/bookshelves", post(api::admin_create_bookshelf))
        .route("/admin/backup", post(api::admin_backup))
        // API + health + metrics.
        .nest("/api/v1", api_routes)
        .merge(health::routes())
        // Middleware (outermost last).
        .layer(axum_mw::from_fn_with_state(
            state.clone(),
            middleware::csrf_protect,
        ))
        .layer(axum_mw::from_fn_with_state(
            state.clone(),
            require_login_gate,
        ))
        .layer(axum_mw::from_fn_with_state(
            state.clone(),
            middleware::session_auth,
        ))
        .layer(axum_mw::from_fn(middleware::security_headers))
        .with_state(state)
}

/// Gate that allows public paths through without a session but keeps the
/// require-login redirect for protected pages. Also enforces the forced
/// password change: users with must_change_password can only reach
/// /password (and /logout) until they change it.
async fn require_login_gate(
    State(state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let path = request.uri().path();
    let public = matches!(
        path,
        "/login" | "/setup" | "/logout" | "/assets/style.css" | "/assets/app.js" | "/assets/graph.js"
    ) || path.starts_with("/assets/")
        || path.starts_with("/api/v1/health")
        || path.starts_with("/metrics")
        || path.starts_with("/health")
        // The MCP endpoint enforces its own bearer API-key auth (401 +
        // WWW-Authenticate); the browser-flow gates must not intercept it.
        || path == "/mcp";
    if public {
        return next.run(request).await;
    }
    // Must be logged in from here (session cookie or bearer API key).
    let Some(user) = request
        .extensions()
        .get::<mycelium_auth::rbac::SessionUser>()
        .cloned()
    else {
        return Redirect::to("/login").into_response();
    };
    // Forced password change gate: only /password (and logout, handled
    // above) is reachable until the password is changed. Bearer API-key
    // clients are exempt — the flag is a browser-flow concern, and API
    // keys are minted deliberately by the user.
    let bearer = request
        .extensions()
        .get::<crate::middleware::BearerAuth>()
        .is_some();
    if path != "/password" && !bearer {
        let record = state.users.get_by_id(user.user_id).await;
        if matches!(record, Ok(ref r) if r.must_change_password) {
            return Redirect::to("/password?forced=1").into_response();
        }
    }
    next.run(request).await
}

/// Serve a static asset from the assets dir (no directory traversal).
async fn serve_asset(
    axum::extract::Path(path): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::response::Response {
    // Reject traversal: only a single path component, no dots.
    if path.contains("..") || path.contains('/') || path.contains('\\') {
        return (axum::http::StatusCode::NOT_FOUND, "not found").into_response();
    }
    let full = state.assets_dir.join(&path);
    match tokio::fs::read(&full).await {
        Ok(bytes) => {
            let mime = match path.rsplit('.').next() {
                Some("css") => "text/css",
                Some("js") => "application/javascript",
                Some("svg") => "image/svg+xml",
                Some("png") => "image/png",
                _ => "application/octet-stream",
            };
            let mut response = axum::response::Response::new(axum::body::Body::from(bytes));
            *response.status_mut() = axum::http::StatusCode::OK;
            response.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static(mime),
            );
            // Cache aggressively: the page references assets with a
            // ?v=<ASSETS_VERSION> query, so a version bump changes the
            // URL and busts the cache on upgrade.
            response.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("public, max-age=604800"),
            );
            response
        }
        Err(_) => (axum::http::StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Serve HTTPS on `https_addr`, redirecting HTTP on `http_addr` to it.
pub async fn serve(
    state: AppState,
    data_dir: &Path,
    https_addr: SocketAddr,
    http_addr: SocketAddr,
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let router = build_router_with_shutdown(state, shutdown.clone());
    let tls = cert::load_or_create_tls_config(data_dir, cert_path, key_path).await?;

    // HTTP:80 → HTTPS redirect. Preserves the request Host (so the
    // redirect targets the hostname the client actually used) and the
    // path + query.
    let redirect_addr = http_addr;
    let https_port = https_addr.port();
    let http_redirect = tokio::spawn(async move {
        let app = Router::new().fallback(
            move |headers: axum::http::HeaderMap, uri: axum::http::Uri| async move {
                // Host header (strip any client-supplied port — the
                // redirect port is the server's https_port).
                let host = headers
                    .get(axum::http::header::HOST)
                    .and_then(|h| h.to_str().ok())
                    .map(|h| {
                        h.rsplit_once(':')
                            .map(|(name, _)| name)
                            .unwrap_or(h)
                            .to_string()
                    })
                    .unwrap_or_else(|| "localhost".to_string());
                let path_and_query = uri
                    .path_and_query()
                    .map(|pq| pq.as_str().to_string())
                    .unwrap_or_else(|| "/".to_string());
                Redirect::to(&format!("https://{host}:{https_port}{path_and_query}"))
            },
        );
        match tokio::net::TcpListener::bind(redirect_addr).await {
            Ok(listener) => {
                let _ = axum::serve(listener, app).await;
            }
            Err(e) => {
                tracing::warn!("HTTP redirect listener failed to bind {redirect_addr}: {e}");
            }
        }
    });

    // HTTPS listener with graceful shutdown.
    let handle = axum_server::Handle::new();
    let shutdown_handle = handle.clone();
    let shutdown_token = shutdown.clone();
    tokio::spawn(async move {
        shutdown_token.cancelled().await;
        shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
    });
    tracing::info!("listening on https://{https_addr}");
    axum_server::bind_rustls(https_addr, tls)
        .handle(handle)
        .serve(router.into_make_service())
        .await?;
    http_redirect.abort();
    Ok(())
}
