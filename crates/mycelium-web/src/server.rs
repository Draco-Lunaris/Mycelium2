//! HTTPS server assembly: router, TLS listener, HTTP:80 redirect,
//! graceful shutdown.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

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
        // The drain worker's wake handle: enqueues fire it, the drain
        // loop awaits it — one Notify for both sides. The enqueue
        // depth cap is the drain's own QueueLimits constant (single
        // source of truth), not an independent literal.
        notify: state.queue_worker.notify(),
        enqueue_depth_cap: mycelium_librarian::queue_worker::QueueLimits::default().depth_cap,
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
        .route("/concept/activate", post(api::concept_activate))
        .route("/concept/delete", post(api::concept_delete))
        .route("/search", get(api::search_view))
        .route("/graph", get(api::graph_view))
        .route("/skills", get(api::skills_view))
        // Revisions browser (Task 5, spec §3.2): the scope-level index
        // of every revision-tracked path beside the skills page.
        .route("/revisions", get(api::revisions_view))
        .route("/books", get(api::books_view))
        .route("/chat", get(api::chat_view))
        // Librarian status (PR 4, spec §6/§8): the admin-only
        // operational page — session-gated here, RequireAdmin in the
        // handler (the same gate shape as the /admin pages).
        .route("/status", get(api::status_view))
        // /password (Task 5's merge): the GET 307s to /account (the
        // password card's home); the POST stays the change form's
        // unchanged target (the account card posts here).
        .route(
            "/password",
            get(api::password_view).post(api::password_submit),
        )
        .route("/keys", get(api::keys_view).post(api::keys_mint))
        .route("/keys/revoke", post(api::keys_revoke))
        // Account security (PR 4, mockup 10): GET /account for every
        // logged-in user; the TOTP POSTs are thin wrappers over
        // mycelium-auth's totp service (spec §6's exact route names);
        // the password card posts the EXISTING /password handler (its
        // POST unchanged — Task 5's merge made the /password GET a 307
        // here and moved the forced-change gate to /account). Task 4
        // adds the
        // recovery-key regenerate POST and the WebAuthn ceremony routes
        // (challenge GET + register/remove POSTs) — all session-gated
        // behind the same middleware stack.
        .route("/account", get(api::account_view))
        .route("/account/totp/enroll", post(api::account_totp_enroll))
        .route("/account/totp/verify", post(api::account_totp_verify))
        .route("/account/totp/disable", post(api::account_totp_disable))
        .route(
            "/account/recovery/regenerate",
            post(api::account_recovery_regenerate),
        )
        .route(
            "/account/webauthn/challenge",
            get(api::account_webauthn_challenge),
        )
        .route(
            "/account/webauthn/register",
            post(api::account_webauthn_register),
        )
        .route(
            "/account/webauthn/remove",
            post(api::account_webauthn_remove),
        )
        // Admin: five pages under a TabBar (spec §6); /admin is a 307
        // to the users page (bookmarks keep working). The POST routes
        // keep their exact paths — each renders back to its host page.
        .route("/admin", get(api::admin_view))
        .route(
            "/admin/users",
            get(api::admin_users_view).post(api::admin_create_user),
        )
        .route("/admin/sign-in", get(api::admin_sign_in_view))
        .route(
            "/admin/llm",
            get(api::admin_llm_view).post(api::admin_save_llm),
        )
        .route("/admin/llm/test", post(api::admin_test_llm))
        .route("/admin/library", get(api::admin_library_view))
        .route("/admin/system", get(api::admin_system_view))
        .route("/admin/oidc", post(api::admin_save_oidc))
        .route("/admin/upload-limits", post(api::admin_save_upload_limits))
        .route("/admin/security", post(api::admin_save_security))
        .route("/admin/bookshelves", post(api::admin_create_bookshelf))
        .route("/admin/backup", post(api::admin_backup))
        .route("/admin/tls/cert", get(api::tls_cert_download))
        // Third-party cert upload. The route body limit (5 MiB) is just
        // above 2 × the handler's 2 MiB per-field cap plus multipart
        // framing, so the handler's check — with its clear flash — is
        // what rejects an oversized field (axum's 2 MiB default would
        // otherwise reject the whole request first).
        .route(
            "/admin/tls/upload",
            post(api::tls_upload).layer(axum::extract::DefaultBodyLimit::max(5 * 1024 * 1024)),
        )
        .route(
            "/admin/tls/csr",
            get(api::tls_csr_download).post(api::tls_csr),
        )
        .route("/admin/tls/pending/discard", post(api::tls_pending_discard))
        .route("/admin/tls/reset", post(api::tls_reset))
        .route("/admin/tls/regenerate", post(api::tls_regenerate))
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
/// /account (the password card's home), POST /password (the card's
/// unchanged form target), and /logout until they change it.
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
    // Forced password change gate (Task 5's merge): a must-change user
    // reaches /account — the password card's home, where the ?forced=1
    // banner renders — plus POST /password, the card's UNCHANGED form
    // target (dropping /password from the allowed set would brick the
    // very flow the gate exists to force: the change form could never
    // submit). Logout stays public (handled above). Bearer API-key
    // clients are exempt — the flag is a browser-flow concern, and API
    // keys are minted deliberately by the user.
    let bearer = request
        .extensions()
        .get::<crate::middleware::BearerAuth>()
        .is_some();
    if path != "/account" && path != "/password" && !bearer {
        let record = state.users.get_by_id(user.user_id).await;
        if matches!(record, Ok(ref r) if r.must_change_password) {
            return Redirect::to("/account?forced=1").into_response();
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
                Some("wasm") => "application/wasm",
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

/// Push the stored `"llm"` row's limits onto the process-global LLM
/// admission gate (LLM concurrency spec §3.2), clamped by
/// [`crate::LlmConfig::clamp`] inside `sync_gate`.
///
/// The ordering guarantee is the CALLER's: whoever boots the process
/// must call this before anything can admit on the gate — the
/// mutation-queue boot drain pass, the drain worker, and the chat
/// resume worker all use try-class admission, and a `tokio::time::
/// interval`'s first tick fires immediately, so a worker spawned before
/// this call would take its first pass against `DEFAULT_CAPACITY`
/// instead of the operator's `max_parallel`. `mycelium-server`'s `main`
/// calls it right after `AppState` is built (before every boot sweep and
/// every spawn); [`serve`] calls it again, which is idempotent and
/// covers embedders that call `serve` directly (the test harness does).
///
/// A missing or undecryptable row simply means the defaults; nothing
/// here can fail.
pub async fn sync_llm_gate_from_store(state: &AppState) {
    let llm_cfg =
        crate::state::decrypt_config::<crate::LlmConfig>(&state.config, &state.service_key, "llm")
            .await
            .unwrap_or_default();
    llm_cfg.sync_gate();
    tracing::info!(
        capacity = mycelium_librarian::llm_gate::LLM_GATE.capacity(),
        per_user_active = mycelium_librarian::llm_gate::LLM_GATE.per_user_active(),
        chat_queue_depth = mycelium_librarian::llm_gate::LLM_GATE.chat_queue_depth_limit(),
        "LLM gate synced from the stored config"
    );
}

/// One probe tick against the CURRENT stored `"llm"` row, feeding the
/// process-global [`LLM_HEALTH`](mycelium_librarian::llm_health::LLM_HEALTH)
/// exactly once (outage recovery spec §3.1 —
/// [`probe_once`](mycelium_librarian::llm_health::probe_once) is pure
/// and never self-feeds, so this tick is the feeding site).
///
/// The three postures:
/// - **Absent row** → `set_disabled`: nothing to probe; the machine
///   pins Disabled and ignores notes until a row comes back.
/// - **Present row** → `note_configured` lifts any Disabled pin (its
///   contract is row *presence*, so it happens even when the row then
///   fails to decrypt), then the probe's verdict feeds `note_ok` /
///   `note_failure`.
/// - **Undecryptable row** → NO observation is fed at all (deliberate,
///   and the subtle one): a corrupt/foreign-sealed row is a config
///   problem, not backend weather. Feeding `note_failure` would drive
///   the dot Red and pause the resume worker on a claim about a backend
///   we never contacted; feeding `set_disabled` would claim "no `llm`
///   row" when one plainly exists (wrong fail-fast message to users,
///   grey dot for a configured system). Skipping the note claims
///   nothing: the machine keeps its last real observation, and its
///   `checked_at` clock ages honestly ("last checked Ns ago"). The
///   passive `LlmClient` hooks correct from real traffic within a few
///   attempts, and the warn fires every tick.
///
/// A raw-read failure (DB error) skips the tick entirely — even
/// presence is unknown, so neither pin nor lift is honest.
async fn probe_tick(state: &AppState) {
    let raw = match state.config.raw_get("llm").await {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(error = %e, "backend probe: llm config row unreadable — tick skipped");
            return;
        }
    };
    let Some(_raw) = raw else {
        mycelium_librarian::llm_health::LLM_HEALTH.set_disabled();
        return;
    };
    mycelium_librarian::llm_health::LLM_HEALTH.note_configured();
    let Some(cfg) =
        crate::state::decrypt_config::<crate::LlmConfig>(&state.config, &state.service_key, "llm")
            .await
    else {
        tracing::warn!(
            "backend probe: llm config row present but undecryptable — \
             no health observation this tick (config problem, not backend weather)"
        );
        return;
    };
    if mycelium_librarian::llm_health::probe_once(&cfg).await {
        mycelium_librarian::llm_health::LLM_HEALTH.note_ok();
    } else {
        mycelium_librarian::llm_health::LLM_HEALTH.note_failure();
    }
}

/// Spawn the background backend probe: the periodic `LLM_HEALTH`
/// feeder (outage recovery spec §3.1; llm_health's Sources note names
/// this task as the probe-side feeder).
///
/// The wake loop mirrors the established worker shape (queue worker /
/// chat resume worker): a [`tokio::time::interval`] whose **first tick
/// fires immediately** — so the task's first pass is also the boot
/// health seed (absent row → Disabled, present row → probed) — with
/// `MissedTickBehavior::Delay` so a slow probe (the 5 s timeout) never
/// bursts catch-up ticks, and a `tokio::select!` shutdown arm that
/// info-logs and returns (defused by the token, no spin).
///
/// Each tick re-reads the CURRENT config row, so an admin's config swap
/// (URL, key, or deleting the row) applies within one interval — no
/// restart, no task restart. Production passes
/// [`mycelium_librarian::llm_health::PROBE_INTERVAL_SECS`] (30 s);
/// tests pass short intervals to keep the transition paths observable.
pub async fn spawn_backend_probe(
    state: AppState,
    shutdown: tokio_util::sync::CancellationToken,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.cancelled() => {
                    // Nothing owed: the registry is in-memory only, and
                    // the next boot's first tick re-derives it.
                    tracing::info!("backend probe task stopped");
                    return;
                }
            }
            probe_tick(&state).await;
        }
    })
}

/// Serve HTTPS on `https_addr`, redirecting HTTP on `http_addr` to it.
/// `sans` is the SAN set for the generated self-signed pair (verbatim;
/// `localhost` only when empty).
// Boot-config list; collapses into AppState fields in the TLS-management
// follow-up tasks.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    state: AppState,
    data_dir: &Path,
    https_addr: SocketAddr,
    http_addr: SocketAddr,
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
    sans: Vec<String>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tls, tls_handle) =
        cert::load_or_create_tls_config(data_dir, cert_path, key_path, &sans).await?;
    // Both returns are clones of one Arc-backed handle: stash the
    // second into AppState so admin TLS reloads go through it, then
    // build the router over the state that carries it.
    let state = AppState {
        tls: Some(Arc::new(tls_handle)),
        ..state
    };

    // LLM admission gate (LLM concurrency spec §3.2): the stored limits
    // are live before this listener accepts a request. `main` already
    // synced them ahead of the boot sweeps and the worker spawns — this
    // call is the idempotent backstop for an embedder that calls `serve`
    // directly without its own sync, and it claims nothing about work
    // started before `serve` was entered.
    sync_llm_gate_from_store(&state).await;

    let router = build_router_with_shutdown(state, shutdown.clone());

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
        // Release every queued LLM waiter BEFORE the graceful window
        // opens (LLM concurrency spec §3.5): a chat runner still
        // sitting on the gate would otherwise hold its SSE connection
        // open for the rest of its wait bound — 600 s by default, up
        // to 7200 — and burn the whole 10 s window before it could
        // finalize its turn.
        // Released waiters get `GateError::Closed`, which the chat
        // runner finalizes as `interrupted` (resumable on the next
        // boot). `close` is not a latch: admitted runs keep their
        // slots and the gate stays usable for whatever is still
        // draining.
        mycelium_librarian::llm_gate::LLM_GATE.close();
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
