//! REST API under /api/v1 plus the form-driven page handlers.

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use mycelium_auth::rbac::{Role, SessionUser};
use mycelium_core::concept::Concept;
use mycelium_core::search::SearchQuery;
use mycelium_store::ConceptStore;

use crate::middleware::SessionId;
use crate::pages;
use crate::state::AppState;

/// API error shape.
#[derive(Serialize)]
pub struct ApiError {
    pub error: String,
}

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/graph", get(graph_data))
        .route("/search", get(api_search))
        .route(
            "/concepts/{*path}",
            get(api_get_concept)
                .put(api_put_concept)
                .delete(api_delete_concept),
        )
        .route("/health", get(health_detail))
        // Book ingest + passages. The route-level body limit is a
        // generous DoS ceiling (256 MiB); the EFFECTIVE book limit is
        // the admin-configured UploadConfig (ConfigStore, default
        // 32 MiB) enforced by the CSRF middleware's multipart buffer
        // and the handler's size check.
        .route(
            "/ingest",
            get(api_ingest_jobs)
                .post(api_upload_book)
                .layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024)),
        )
        .route("/ingest/{id}", get(api_ingest_job))
        .route("/passages", get(api_passage))
        .route("/chat", post(api_chat))
        .route("/chat/stream", post(api_chat_stream))
}

// ---------- JSON API ----------

#[derive(Serialize)]
struct GraphResponse {
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
}

#[derive(Serialize)]
struct GraphNode {
    id: String,
    title: String,
    #[serde(rename = "type")]
    concept_type: String,
}

#[derive(Serialize)]
struct GraphEdge {
    from: String,
    to: String,
}

/// GET /api/v1/graph — the user's private bundle graph (DESIGN: graph scope
/// is the user's bundle only).
async fn graph_data(State(state): State<AppState>, user: SessionUser) -> Response {
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "key unavailable"),
    };
    let cs = ConceptStore::for_user(&state.store, user.user_id, master);
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut known = std::collections::HashSet::new();
    for entry in cs.list().await.unwrap_or_default() {
        known.insert(entry.path.clone());
        nodes.push(GraphNode {
            id: entry.path.clone(),
            title: entry.title.clone(),
            concept_type: entry.concept_type.clone(),
        });
    }
    for entry_path in known.iter() {
        if let Ok(concept) = cs.get(entry_path).await {
            for target in mycelium_core::links::scan_links(&concept.body) {
                if known.contains(&target) {
                    edges.push(GraphEdge {
                        from: concept.source_path.clone(),
                        to: target,
                    });
                }
            }
        }
    }
    Json(GraphResponse { nodes, edges }).into_response()
}

#[derive(Deserialize)]
pub struct SearchParams {
    q: String,
    #[serde(default)]
    global: Option<String>,
}

/// One merged search result, tagged with the scope it came from so the
/// UI can route to the right viewer (user bundle, library, skills).
#[derive(serde::Serialize)]
pub struct ScopedResult {
    pub concept_path: String,
    pub title: String,
    pub snippet: String,
    pub score: f32,
    pub scope: &'static str,
}

/// Run a cross-scope search: the user's private bundle plus (by
/// default on the web) the global skills shelf and the library
/// catalogs. Library hits from admin-private bookshelves are filtered
/// out for non-admins (visibility post-filter — the index has no
/// shelf metadata, so the DB decides).
async fn search_all_scopes(
    state: &AppState,
    user: &SessionUser,
    query: &SearchQuery,
    include_global: bool,
) -> Vec<ScopedResult> {
    let mut results: Vec<ScopedResult> = Vec::new();
    if let Ok(master) = state.master_key_for(user.user_id).await {
        let cs = ConceptStore::for_user(&state.store, user.user_id, master);
        for r in cs.search(query).await.unwrap_or_default() {
            results.push(ScopedResult {
                concept_path: r.concept_path,
                title: r.title,
                snippet: r.snippet,
                score: r.score,
                scope: "user",
            });
        }
    }
    if include_global {
        let skills = ConceptStore::for_service(
            &state.store,
            (*state.service_key).clone(),
            &state.store.skills_dir(),
            "skills",
        );
        for r in skills.search(query).await.unwrap_or_default() {
            results.push(ScopedResult {
                concept_path: r.concept_path,
                title: r.title,
                snippet: r.snippet,
                score: r.score,
                scope: "skills",
            });
        }
        let library = ConceptStore::for_service(
            &state.store,
            (*state.service_key).clone(),
            &state.store.library_dir(),
            "library",
        );
        for r in library.search(query).await.unwrap_or_default() {
            // Visibility post-filter: a library hit belongs to a book;
            // only global-read shelves (or admins) may see it.
            let slug = r
                .concept_path
                .trim_start_matches('/')
                .split('/')
                .next()
                .unwrap_or_default()
                .trim_end_matches(".md")
                .to_string();
            let visible = match state.store.book_by_slug(&slug).await {
                Ok(Some(book)) => {
                    let global_read = state
                        .store
                        .bookshelf_is_global_read(book.bookshelf_id)
                        .await
                        .unwrap_or(false);
                    global_read || user.role == Role::Admin
                }
                _ => false, // no DB row → not a book catalog → hide
            };
            if visible {
                results.push(ScopedResult {
                    concept_path: r.concept_path,
                    title: r.title,
                    snippet: r.snippet,
                    score: r.score,
                    scope: "library",
                });
            }
        }
    }
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.concept_path.cmp(&b.concept_path))
    });
    results
}

/// GET /api/v1/search?q=...&global=1 — user bundle (+ global shelves when
/// global=1, the web default per DESIGN).
async fn api_search(
    State(state): State<AppState>,
    user: SessionUser,
    Query(params): Query<SearchParams>,
) -> Response {
    let terms = params
        .q
        .split_whitespace()
        .map(|t| t.to_string())
        .collect::<Vec<_>>();
    let mut query = SearchQuery::new(terms);
    query.include_global = params.global.as_deref() == Some("1");
    let results = search_all_scopes(&state, &user, &query, query.include_global).await;
    crate::state::Metrics::inc(&state.metrics.searches_total);
    Json(results).into_response()
}

#[derive(Deserialize)]
struct PutConcept {
    markdown: String,
}

/// PUT /api/v1/concepts/{path} — create/replace a concept.
async fn api_put_concept(
    State(state): State<AppState>,
    user: SessionUser,
    AxumPath(path): AxumPath<String>,
    Json(body): Json<PutConcept>,
) -> Response {
    let canonical = format!("/{}", path.trim_start_matches('/'));
    let concept = match Concept::parse(&canonical, &body.markdown) {
        Ok(c) => c,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "key unavailable"),
    };
    let cs = ConceptStore::for_user(&state.store, user.user_id, master);
    match cs.put(&concept).await {
        Ok(()) => {
            crate::state::Metrics::inc(&state.metrics.concepts_written);
            (StatusCode::CREATED, "created").into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "concept put failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// GET /api/v1/concepts/{path} — fetch a concept's markdown.
async fn api_get_concept(
    State(state): State<AppState>,
    user: SessionUser,
    AxumPath(path): AxumPath<String>,
) -> Response {
    let canonical = format!("/{}", path.trim_start_matches('/'));
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "key unavailable"),
    };
    let cs = ConceptStore::for_user(&state.store, user.user_id, master);
    match cs.get(&canonical).await {
        Ok(concept) => {
            crate::state::Metrics::inc(&state.metrics.concepts_read);
            match concept.to_markdown() {
                Ok(md) => (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/markdown")],
                )
                    .into_response_with_body(md),
                Err(e) => {
                    tracing::error!(error = %e, "concept markdown render failed");
                    error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
                }
            }
        }
        Err(mycelium_store::ConceptStoreError::NotFound(_)) => {
            error_response(StatusCode::NOT_FOUND, "not found")
        }
        Err(e) => {
            tracing::error!(error = %e, "concept get failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// DELETE /api/v1/concepts/{path}.
async fn api_delete_concept(
    State(state): State<AppState>,
    user: SessionUser,
    AxumPath(path): AxumPath<String>,
) -> Response {
    let canonical = format!("/{}", path.trim_start_matches('/'));
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "key unavailable"),
    };
    let cs = ConceptStore::for_user(&state.store, user.user_id, master);
    match cs.delete(&canonical).await {
        Ok(()) => (StatusCode::NO_CONTENT, "").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "concept delete failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// GET /api/v1/health — detailed status (DESIGN decision).
async fn health_detail(State(state): State<AppState>) -> Response {
    let db_ok = sqlx::query("SELECT 1")
        .execute(state.store.pool())
        .await
        .is_ok();
    let user_count = state.users.count().await.unwrap_or(-1);
    Json(serde_json::json!({
        "status": if db_ok { "ok" } else { "degraded" },
        "database": db_ok,
        "users": user_count,
        "version": env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(ApiError {
            error: message.to_string(),
        }),
    )
        .into_response()
}

/// Helper trait for a text body response.
trait IntoResponseWithBody: Sized {
    fn into_response_with_body(self, body: String) -> Response;
}

impl IntoResponseWithBody
    for (
        StatusCode,
        [(axum::http::header::HeaderName, &'static str); 1],
    )
{
    fn into_response_with_body(self, body: String) -> Response {
        let (status, headers) = self;
        let mut response = Response::new(axum::body::Body::from(body));
        *response.status_mut() = status;
        for (name, value) in headers {
            response
                .headers_mut()
                .insert(name, axum::http::HeaderValue::from_static(value));
        }
        response
    }
}

/// Helper trait for a streaming (SSE) body response with two headers.
trait IntoResponseWithBytesStream: Sized {
    fn into_response_with_bytes_stream(self, body: axum::body::Body) -> Response;
}

impl IntoResponseWithBytesStream
    for (
        StatusCode,
        [(axum::http::header::HeaderName, &'static str); 2],
    )
{
    fn into_response_with_bytes_stream(self, body: axum::body::Body) -> Response {
        let (status, headers) = self;
        let mut response = Response::new(body);
        *response.status_mut() = status;
        for (name, value) in headers {
            response
                .headers_mut()
                .insert(name, axum::http::HeaderValue::from_static(value));
        }
        response
    }
}

/// Helper trait for a binary (bytes) body response with three headers.
trait IntoResponseWithBytes3: Sized {
    fn into_response_with_bytes(self, body: Vec<u8>) -> Response;
}

impl IntoResponseWithBytes3
    for (
        StatusCode,
        [(axum::http::header::HeaderName, &'static str); 3],
    )
{
    fn into_response_with_bytes(self, body: Vec<u8>) -> Response {
        let (status, headers) = self;
        let mut response = Response::new(axum::body::Body::from(body));
        *response.status_mut() = status;
        for (name, value) in headers {
            response
                .headers_mut()
                .insert(name, axum::http::HeaderValue::from_static(value));
        }
        response
    }
}

// ---------- Form handlers (page routes) ----------

#[derive(Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
    pub totp: Option<String>,
}

/// POST /login — form login; sets the session cookie.
pub async fn login_submit(
    State(state): State<AppState>,
    axum::Form(form): axum::Form<LoginForm>,
) -> Response {
    // Clone the service out of the mutex so no guard is held across await.
    let login = state.login.clone();
    // Runtime security settings (admin-configurable, pre-clamped).
    let sec = state.security_config().await;
    let options = mycelium_auth::login::LoginOptions {
        session_ttl: Some(chrono::Duration::minutes(sec.session_ttl_minutes as i64)),
        login_max_failures: Some(sec.login_max_failures),
        login_lockout: Some(std::time::Duration::from_secs(sec.login_lockout_seconds)),
        min_password_length: Some(sec.min_password_length),
    };
    let cookie_max_age = sec.session_ttl_minutes * 60;
    match login
        .login_with_options(
            &form.username,
            &form.password,
            form.totp.as_deref(),
            &options,
        )
        .await
    {
        Ok(success) => {
            crate::state::Metrics::inc(&state.metrics.logins_total);
            // Persist the service-key seal so restarts can recover the key.
            // A failure here means the user's data is unrecoverable after
            // restart (until they log in again) — log loudly.
            if let Err(e) = state
                .persist_service_seal(success.user.record.id, &success.user.master_key)
                .await
            {
                tracing::error!(
                    user_id = %success.user.record.id,
                    error = %e,
                    "failed to persist service-key seal — data at risk after restart"
                );
            }
            state
                .master_keys
                .insert(success.user.record.id, success.user.master_key);
            let cookie = format!(
                "myc2_session={}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age={cookie_max_age}",
                success.session.id
            );
            let mut response = if success.must_change_password {
                Redirect::to("/password?forced=1").into_response()
            } else {
                Redirect::to("/").into_response()
            };
            response.headers_mut().insert(
                axum::http::header::SET_COOKIE,
                axum::http::HeaderValue::from_str(&cookie).expect("valid cookie"),
            );
            response
        }
        Err(e) => {
            crate::state::Metrics::inc(&state.metrics.logins_failed);
            let msg = match e {
                mycelium_auth::login::LoginError::Throttled { retry_after_secs } => {
                    format!("Too many attempts; retry in {retry_after_secs}s")
                }
                _ => "Invalid credentials".to_string(),
            };
            pages::login_page("", Some(&msg)).into_response()
        }
    }
}

/// GET /login — render the login page.
pub async fn login_page(State(_state): State<AppState>) -> Response {
    pages::login_page("", None).into_response()
}

/// GET /logout — delete the session and clear the cookie. Public: works
/// with or without a session (extracted via headers directly).
pub async fn logout(State(state): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    if let Some(session_id) = crate::middleware::session_id_from_headers(&headers) {
        let _ = state.sessions.delete(session_id).await;
        // Chat history is session-scoped ephemeral state — drop it.
        state.chat_history.clear(session_id);
    }
    let mut response = Redirect::to("/login").into_response();
    response.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_static(
            "myc2_session=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0",
        ),
    );
    response
}

#[derive(Deserialize)]
pub struct ConceptForm {
    pub path: Option<String>,
    pub markdown: String,
    /// Which store to write to: "user" (default), "skills" (global
    /// skills shelf — admin only), "library" (read-only).
    #[serde(default)]
    pub scope: Option<String>,
}

/// POST /concept — create or save a concept (form).
pub async fn concept_submit(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
    axum::Form(form): axum::Form<ConceptForm>,
) -> Response {
    let Some(path) = form.path else {
        return pages::not_found().into_response();
    };
    let canonical = format!("/{}", path.trim_start_matches('/'));
    let concept = match Concept::parse(&canonical, &form.markdown) {
        Ok(c) => c,
        Err(e) => {
            let csrf = pages::current_csrf(&state, &session).await;
            return pages::concept_page(
                &user,
                &csrf,
                &canonical,
                &form.markdown,
                None,
                Some(&e.to_string()),
            )
            .into_response();
        }
    };
    match form.scope.as_deref() {
        // Global skills shelf: admin-only writes.
        Some("skills") => {
            if user.role != Role::Admin {
                return error_response(StatusCode::FORBIDDEN, "admins only");
            }
            let cs = ConceptStore::for_service(
                &state.store,
                (*state.service_key).clone(),
                &state.store.skills_dir(),
                "skills",
            );
            match cs.put(&concept).await {
                Ok(()) => Redirect::to(&format!(
                    "/concept?path={}&scope=skills",
                    pages::urlencoding_encode(&canonical)
                ))
                .into_response(),
                Err(e) => {
                    let csrf = pages::current_csrf(&state, &session).await;
                    pages::concept_page(
                        &user,
                        &csrf,
                        &canonical,
                        &form.markdown,
                        None,
                        Some(&e.to_string()),
                    )
                    .into_response()
                }
            }
        }
        // The library is read-only (ingest is the only writer).
        Some("library") => error_response(StatusCode::FORBIDDEN, "library is read-only"),
        // Default: the user's private bundle.
        _ => {
            let master = match state.master_key_for(user.user_id).await {
                Ok(m) => m,
                Err(_) => return pages::not_found().into_response(),
            };
            let cs = ConceptStore::for_user(&state.store, user.user_id, master);
            match cs.put(&concept).await {
                Ok(()) => Redirect::to(&format!(
                    "/concept?path={}",
                    pages::urlencoding_encode(&canonical)
                ))
                .into_response(),
                Err(e) => {
                    let csrf = pages::current_csrf(&state, &session).await;
                    pages::concept_page(
                        &user,
                        &csrf,
                        &canonical,
                        &form.markdown,
                        None,
                        Some(&e.to_string()),
                    )
                    .into_response()
                }
            }
        }
    }
}

#[derive(Deserialize)]
pub struct ConceptQuery {
    pub path: Option<String>,
    pub new: Option<String>,
    /// Which store to read from: "user" (default), "skills" (global
    /// skills shelf), "library" (book catalogs), "skills-private"
    /// (new private skill in the user bundle).
    #[serde(default)]
    pub scope: Option<String>,
}

/// GET /concept — view/edit (path given) or new (new=1).
pub async fn concept_view(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
    Query(query): Query<ConceptQuery>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    if query.new.is_some() {
        let template = match query.scope.as_deref() {
            Some("skills") if user.role == Role::Admin => {
                "---\ntype: Skill\ntitle: New global skill\ndescription: \ntags: []\n---\n\n"
            }
            Some("skills") => {
                return error_response(StatusCode::FORBIDDEN, "admins only");
            }
            Some("skills-private") | None => {
                "---\ntype: Skill\ntitle: New private skill\ndescription: \ntags: []\n---\n\n"
            }
            Some("library") => {
                return error_response(StatusCode::FORBIDDEN, "library is read-only");
            }
            _ => "---\ntype: Note\ntitle: New concept\ndescription: \ntags: []\n---\n\n",
        };
        return pages::new_concept_page_with(&user, &csrf, template, query.scope.as_deref())
            .into_response();
    }
    let Some(path) = query.path else {
        return Redirect::to("/").into_response();
    };
    let canonical = format!("/{}", path.trim_start_matches('/'));
    match query.scope.as_deref() {
        // Global skills shelf: readable by all, editable by admins.
        Some("skills") => {
            let cs = ConceptStore::for_service(
                &state.store,
                (*state.service_key).clone(),
                &state.store.skills_dir(),
                "skills",
            );
            match cs.get(&canonical).await {
                Ok(concept) => {
                    let markdown = concept.to_markdown().unwrap_or_default();
                    let editable = user.role == Role::Admin;
                    pages::concept_page_scoped(
                        &user, &csrf, &canonical, &markdown, editable, "skills",
                    )
                    .into_response()
                }
                Err(_) => pages::not_found().into_response(),
            }
        }
        // Library catalogs: read-only, shelf-visibility gated.
        Some("library") => {
            // Visibility: the concept's book slug must be on a
            // global-read shelf (or the reader is an admin).
            let slug = canonical
                .trim_start_matches('/')
                .split('/')
                .next()
                .unwrap_or_default()
                .trim_end_matches(".md")
                .to_string();
            let visible = match state.store.book_by_slug(&slug).await {
                Ok(Some(book)) => {
                    let global_read = state
                        .store
                        .bookshelf_is_global_read(book.bookshelf_id)
                        .await
                        .unwrap_or(false);
                    global_read || user.role == Role::Admin
                }
                _ => false,
            };
            if !visible {
                return error_response(StatusCode::FORBIDDEN, "bookshelf is not global-read");
            }
            let cs = ConceptStore::for_service(
                &state.store,
                (*state.service_key).clone(),
                &state.store.library_dir(),
                "library",
            );
            match cs.get(&canonical).await {
                Ok(concept) => {
                    let markdown = concept.to_markdown().unwrap_or_default();
                    pages::concept_page_scoped(
                        &user, &csrf, &canonical, &markdown, false, "library",
                    )
                    .into_response()
                }
                Err(_) => pages::not_found().into_response(),
            }
        }
        // Default: the user's private bundle.
        _ => {
            let master = match state.master_key_for(user.user_id).await {
                Ok(m) => m,
                Err(_) => return pages::not_found().into_response(),
            };
            let cs = ConceptStore::for_user(&state.store, user.user_id, master);
            match cs.get(&canonical).await {
                Ok(concept) => {
                    let markdown = concept.to_markdown().unwrap_or_default();
                    pages::concept_page(&user, &csrf, &canonical, &markdown, None, None)
                        .into_response()
                }
                Err(mycelium_store::ConceptStoreError::NotFound(_)) => {
                    pages::new_concept_page(&user, &csrf, None).into_response()
                }
                Err(_) => pages::not_found().into_response(),
            }
        }
    }
}

#[derive(Deserialize)]
pub struct DeleteForm {
    pub path: String,
    #[serde(default)]
    pub scope: Option<String>,
}

/// POST /concept/delete — delete a concept (form).
pub async fn concept_delete(
    State(state): State<AppState>,
    user: SessionUser,
    axum::Form(form): axum::Form<DeleteForm>,
) -> Response {
    let canonical = format!("/{}", form.path.trim_start_matches('/'));
    match form.scope.as_deref() {
        // Global skills shelf: admin-only deletes.
        Some("skills") => {
            if user.role != Role::Admin {
                return error_response(StatusCode::FORBIDDEN, "admins only");
            }
            let cs = ConceptStore::for_service(
                &state.store,
                (*state.service_key).clone(),
                &state.store.skills_dir(),
                "skills",
            );
            let _ = cs.delete(&canonical).await;
            Redirect::to("/skills").into_response()
        }
        // The library is read-only (ingest is the only writer).
        Some("library") => error_response(StatusCode::FORBIDDEN, "library is read-only"),
        // Default: the user's private bundle.
        _ => {
            let master = match state.master_key_for(user.user_id).await {
                Ok(m) => m,
                Err(_) => return pages::not_found().into_response(),
            };
            let cs = ConceptStore::for_user(&state.store, user.user_id, master);
            let _ = cs.delete(&canonical).await;
            Redirect::to("/").into_response()
        }
    }
}

/// One shelf's browse data: (name, is_global_read, books as (slug, title)).
pub type ShelfBrowse = (String, bool, Vec<(String, String)>);

/// GET /books — browse global bookshelves. Users see global-read
/// shelves; admins see all. Each shelf lists its books (from the DB),
/// each book linking to its catalog hub.
pub async fn books_view(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let shelves = state
        .store
        .list_bookshelves_detailed()
        .await
        .unwrap_or_default();
    let mut shelf_data: Vec<ShelfBrowse> = Vec::new();
    for (id, name, is_global) in shelves {
        // Visibility: users see global-read shelves only; admins all.
        if !is_global && user.role != Role::Admin {
            continue;
        }
        let books = state.store.books_on_shelf(id).await.unwrap_or_default();
        shelf_data.push((name, is_global, books));
    }
    pages::books_page(&user, &csrf, &shelf_data).into_response()
}

/// GET / — home: the user's bundle listing.
pub async fn home(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        Err(_) => return pages::not_found().into_response(),
    };
    let cs = ConceptStore::for_user(&state.store, user.user_id, master);
    let entries = cs.list().await.unwrap_or_default();
    pages::home_page(&user, &csrf, &entries, None, None).into_response()
}

/// GET /search — search page.
pub async fn search_view(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
    Query(params): Query<SearchParams>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    if params.q.is_empty() {
        return pages::search_page(&user, &csrf, "", &[]).into_response();
    }
    let terms = params
        .q
        .split_whitespace()
        .map(|t| t.to_string())
        .collect::<Vec<_>>();
    let mut query = SearchQuery::new(terms);
    query.include_global = true; // web default: bundle + global shelves
    let results = search_all_scopes(&state, &user, &query, true).await;
    crate::state::Metrics::inc(&state.metrics.searches_total);
    pages::search_page(&user, &csrf, &params.q, &results).into_response()
}

/// GET /graph — graph page.
pub async fn graph_view(
    user: SessionUser,
    session: SessionId,
    State(state): State<AppState>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    pages::graph_page(&user, &csrf).into_response()
}

/// GET /skills — skills page (private + global).
pub async fn skills_view(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        Err(_) => return pages::not_found().into_response(),
    };
    let cs = ConceptStore::for_user(&state.store, user.user_id, master);
    // Private skills: only `type: Skill` concepts from the user bundle.
    let private: Vec<mycelium_store::ConceptEntry> = cs
        .list()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.concept_type == "Skill")
        .collect();
    let global_cs = ConceptStore::for_service(
        &state.store,
        (*state.service_key).clone(),
        &state.store.skills_dir(),
        "skills",
    );
    // Global skills: same `type: Skill` filter as the private section
    // (the shelf is admin-managed, but a stray non-Skill concept
    // should not surface on the Skills page).
    let global: Vec<mycelium_store::ConceptEntry> = global_cs
        .list()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.concept_type == "Skill")
        .collect();
    pages::skills_page(&user, &csrf, &private, &global).into_response()
}

#[derive(Deserialize)]
pub struct PasswordForm {
    pub old: String,
    pub new: String,
    pub repeat: String,
}

/// POST /password — change password (invalidates sessions, forces re-login).
pub async fn password_submit(
    State(state): State<AppState>,
    session: SessionId,
    axum::Form(form): axum::Form<PasswordForm>,
) -> Response {
    if form.new != form.repeat {
        return pages::password_page("", None, Some("Passwords do not match")).into_response();
    }
    let sess = match state.sessions.get(session.0).await {
        Ok(s) => s,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let login = state.login.clone();
    // Admin-configurable minimum length (pre-clamped).
    let min_len = state.security_config().await.min_password_length;
    match login
        .change_password_min(sess.user_id, &form.old, &form.new, min_len)
        .await
    {
        Ok(()) => {
            // All sessions invalidated — clear the cookie, re-login.
            let mut response = Redirect::to("/login?changed=1").into_response();
            response.headers_mut().insert(
                axum::http::header::SET_COOKIE,
                axum::http::HeaderValue::from_static(
                    "myc2_session=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0",
                ),
            );
            response
        }
        Err(e) => pages::password_page("", None, Some(&e.to_string())).into_response(),
    }
}

/// GET /password — change-password page.
pub async fn password_view(session: SessionId, State(state): State<AppState>) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    pages::password_page(&csrf, None, None).into_response()
}

/// GET /keys — API keys page.
pub async fn keys_view(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let keys = state
        .users
        .list_api_keys(user.user_id)
        .await
        .unwrap_or_default();
    pages::keys_page(&user, &csrf, &keys, None).into_response()
}

#[derive(Deserialize)]
pub struct MintKeyForm {
    pub label: String,
}

/// POST /keys — mint an API key (shown once).
pub async fn keys_mint(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
    axum::Form(form): axum::Form<MintKeyForm>,
) -> Response {
    let manager = mycelium_auth::api_key::ApiKeyManager::new(state.store.pool().clone());
    let minted = manager.mint(user.user_id, &form.label).await;
    let csrf = pages::current_csrf(&state, &session).await;
    match minted {
        Ok(m) => {
            let keys = state
                .users
                .list_api_keys(user.user_id)
                .await
                .unwrap_or_default();
            pages::keys_page(&user, &csrf, &keys, Some(&m.token)).into_response()
        }
        Err(_) => pages::keys_page(&user, &csrf, &[], None).into_response(),
    }
}

#[derive(Deserialize)]
pub struct RevokeKeyForm {
    pub id: String,
}

/// POST /keys/revoke — revoke an API key.
pub async fn keys_revoke(
    State(state): State<AppState>,
    user: SessionUser,
    axum::Form(form): axum::Form<RevokeKeyForm>,
) -> Response {
    let manager = mycelium_auth::api_key::ApiKeyManager::new(state.store.pool().clone());
    if let Ok(id) = Uuid::parse_str(&form.id) {
        // Only revoke if the key belongs to this user.
        if let Ok(keys) = state.users.list_api_keys(user.user_id).await
            && keys.iter().any(|k| k.id == id)
        {
            let _ = manager.revoke(id).await;
        }
    }
    Redirect::to("/keys").into_response()
}

// ---------- Admin handlers ----------

#[derive(Deserialize)]
pub struct CreateUserForm {
    pub username: String,
    pub email: String,
    pub password: String,
    pub role: String,
}

/// POST /admin/users — create a user (admin only).
pub async fn admin_create_user(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    axum::Form(form): axum::Form<CreateUserForm>,
) -> Response {
    let role = Role::parse(&form.role).unwrap_or(Role::User);
    let login = state.login.clone();
    match login
        .create_user(&form.username, &form.email, &form.password, role)
        .await
    {
        Ok(created) => {
            // Persist the service seal so the server can serve the user's
            // data after restart. Failure = data at risk — log loudly.
            if let Err(e) = state
                .persist_service_seal(created.record.id, &created.master_key)
                .await
            {
                tracing::error!(
                    user_id = %created.record.id,
                    error = %e,
                    "failed to persist service-key seal — data at risk after restart"
                );
            }
            // Show the recovery key in-page (never in a URL — history and
            // referrer leakage).
            let csrf = String::new(); // admin_view re-renders with its own csrf
            let _ = csrf;
            let body = format!(
                r#"<h1>User created</h1>
<div class="flash ok">User <b>{}</b> created. Recovery key (shown ONCE — give it to the user now):</div>
<pre>{}</pre>
<p><a href="/admin">Back to admin</a></p>"#,
                pages::html_escape(&form.username),
                pages::html_escape(&created.recovery_key)
            );
            let admin = SessionUser {
                user_id: created.record.id,
                username: form.username.clone(),
                role: Role::Admin,
            };
            // Render with the creating admin's identity for the nav.
            pages::layout("User created", Some(&admin), "", body).into_response()
        }
        Err(e) => Redirect::to(&format!(
            "/admin?error={}",
            pages::urlencoding_encode(&e.to_string())
        ))
        .into_response(),
    }
}

/// GET /admin — the admin portal (admin only).
pub async fn admin_view(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    session: SessionId,
    Query(params): Query<AdminParams>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let users = state.users.list_all().await.unwrap_or_default();
    let oidc = crate::state::decrypt_config::<mycelium_auth::oidc::OidcConfig>(
        &state.config,
        &state.service_key,
        "oidc",
    )
    .await
    .is_some();
    let llm: Option<crate::LlmConfig> = state.config.get("llm").await.unwrap_or(None);
    let llm = llm.unwrap_or_default();
    let upload = state.upload_config().await;
    let security = state.security_config().await;
    let shelves = state.store.list_bookshelves().await.unwrap_or_default();
    let shelf_tuples: Vec<(String, bool)> = shelves.into_iter().collect();
    // Ingest jobs (slug, status, detail, created, shelf name).
    let jobs = state.store.list_ingest_jobs(20).await.unwrap_or_default();
    let mut job_rows: Vec<(String, String, String, String, String)> = Vec::new();
    for j in &jobs {
        let shelf_name = state
            .store
            .bookshelf_name(j.bookshelf_id)
            .await
            .unwrap_or_default();
        let slug = state.store.book_slug(j.book_id).await.unwrap_or_default();
        job_rows.push((
            slug,
            j.status.as_str().to_string(),
            j.detail.clone(),
            j.created_at.to_rfc3339(),
            shelf_name,
        ));
    }
    // RequireAdmin guarantees the session user is an admin; fetch for render.
    let admin = request_admin_user(&state, &session).await;
    pages::admin_page(
        &admin,
        &csrf,
        &users,
        oidc,
        &llm.url,
        &llm.model,
        upload.max_book_mib,
        &security,
        &shelf_tuples,
        &job_rows,
        params.created.as_deref(),
        params.error.as_deref(),
    )
    .into_response()
}

/// Fetch the SessionUser for an admin session (RequireAdmin already
/// verified the role; this is for page rendering).
async fn request_admin_user(state: &AppState, session: &SessionId) -> SessionUser {
    let sess = state
        .sessions
        .get(session.0)
        .await
        .expect("session verified by middleware");
    let user = state
        .users
        .get_by_id(sess.user_id)
        .await
        .expect("user verified by middleware");
    SessionUser {
        user_id: user.id,
        username: user.username,
        role: user.role,
    }
}

#[derive(Deserialize)]
pub struct AdminParams {
    pub created: Option<String>,
    pub error: Option<String>,
}

#[derive(Deserialize)]
pub struct OidcForm {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
}

/// POST /admin/oidc — save the OIDC config (admin only). The client
/// secret is encrypted under the service key before storage (the config
/// table contract: sensitive values encrypted by the caller).
pub async fn admin_save_oidc(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    axum::Form(form): axum::Form<OidcForm>,
) -> Response {
    let cfg = mycelium_auth::oidc::OidcConfig {
        issuer_url: form.issuer_url,
        client_id: form.client_id,
        client_secret: form.client_secret,
        redirect_uri: form.redirect_uri,
        auto_provision: true,
    };
    // Encrypt the whole config (the secret is the sensitive part) under
    // the service key with a purpose-bound DEK.
    let json = match serde_json::to_vec(&cfg) {
        Ok(j) => j,
        Err(e) => {
            return Redirect::to(&format!(
                "/admin?error={}",
                pages::urlencoding_encode(&e.to_string())
            ))
            .into_response();
        }
    };
    let sealed = match mycelium_crypto::aead::aead_seal(
        &json,
        b"mycelium2/config-oidc/v1",
        &crate::state::config_dek(&state.service_key),
    ) {
        Ok(s) => s,
        Err(e) => {
            return Redirect::to(&format!(
                "/admin?error={}",
                pages::urlencoding_encode(&e.to_string())
            ))
            .into_response();
        }
    };
    let _ = state.config.set("oidc", &hex::encode(sealed)).await;
    Redirect::to("/admin").into_response()
}

#[derive(Deserialize)]
pub struct LlmForm {
    pub url: String,
    pub model: String,
}

/// POST /admin/llm — save the LLM config (admin only).
pub async fn admin_save_llm(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    axum::Form(form): axum::Form<LlmForm>,
) -> Response {
    let _ = state
        .config
        .set(
            "llm",
            &crate::LlmConfig {
                url: form.url,
                model: form.model,
            },
        )
        .await;
    Redirect::to("/admin").into_response()
}

#[derive(Deserialize)]
pub struct BookshelfForm {
    pub name: String,
    pub global: String,
}

#[derive(Deserialize)]
pub struct UploadLimitsForm {
    /// Maximum book upload in MiB. Clamped to 1..=255 (the route-level
    /// DoS ceiling is 256 MiB; a value above it would be unreachable).
    pub max_book_mib: u64,
}

/// POST /admin/upload-limits — save the upload limits (admin only).
/// Stored in ConfigStore (key "upload") — runtime-editable, no env.
pub async fn admin_save_upload_limits(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    axum::Form(form): axum::Form<UploadLimitsForm>,
) -> Response {
    let max_book_mib = form.max_book_mib.clamp(1, 255);
    let _ = state
        .config
        .set("upload", &crate::state::UploadConfig { max_book_mib })
        .await;
    Redirect::to("/admin").into_response()
}

#[derive(Deserialize)]
pub struct SecuritySettingsForm {
    pub session_ttl_minutes: u64,
    pub login_max_failures: u32,
    pub login_lockout_seconds: u64,
    pub min_password_length: usize,
    pub passage_max_chars: usize,
}

/// POST /admin/security — save the security settings (admin only).
/// Stored in ConfigStore (key "security"), clamped to guardrail bounds
/// on save AND on read — settings can be tuned within safe limits but
/// never disabled.
pub async fn admin_save_security(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    axum::Form(form): axum::Form<SecuritySettingsForm>,
) -> Response {
    let cfg = crate::state::SecurityConfig {
        session_ttl_minutes: form.session_ttl_minutes,
        login_max_failures: form.login_max_failures,
        login_lockout_seconds: form.login_lockout_seconds,
        min_password_length: form.min_password_length,
        passage_max_chars: form.passage_max_chars,
    }
    .clamped();
    let _ = state.config.set("security", &cfg).await;
    Redirect::to("/admin").into_response()
}

/// POST /admin/bookshelves — create a bookshelf (admin only).
pub async fn admin_create_bookshelf(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    axum::Form(form): axum::Form<BookshelfForm>,
) -> Response {
    let is_global = form.global == "1";
    let _ = state.store.create_bookshelf(&form.name, is_global).await;
    Redirect::to("/admin").into_response()
}

/// POST /admin/backup — full data-directory backup (admin only).
/// Streams a tar.gz of the data directory. Best-effort snapshot of a
/// live database; the CLI `backup` documents the stop-server path for
/// guaranteed consistency.
pub async fn admin_backup(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
) -> Response {
    let data_dir = state.store.data_dir().to_path_buf();
    // Build the archive on the blocking pool (file I/O + compression).
    let build =
        tokio::task::spawn_blocking(move || mycelium_store::backup::backup_tar_gz(&data_dir)).await;
    match build {
        Ok(Ok(bytes)) => {
            crate::state::Metrics::inc(&state.metrics.backups_taken);
            // no-store: the archive contains the service key + sealed
            // master keys — it must never sit in a shared/CDN cache.
            (
                StatusCode::OK,
                [
                    (axum::http::header::CONTENT_TYPE, "application/gzip"),
                    (
                        axum::http::header::CONTENT_DISPOSITION,
                        "attachment; filename=\"mycelium2-backup.tar.gz\"",
                    ),
                    (
                        axum::http::header::CACHE_CONTROL,
                        "no-store, no-cache, must-revalidate",
                    ),
                ],
            )
                .into_response_with_bytes(bytes)
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "backup build failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "backup failed")
        }
        Err(e) => {
            tracing::error!(error = %e, "backup task join failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "backup failed")
        }
    }
}

// ---------- Phase 7: book ingest + passages ----------

#[derive(Deserialize)]
pub struct UploadForm {
    pub bookshelf: String,
    pub slug: String,
    pub title: String,
}

/// POST /api/v1/ingest — multipart book upload (admin only).
///
/// Fields: `bookshelf` (name), `slug`, `title`, `file` (the .md text),
/// plus the CSRF token (verified by the middleware's multipart path).
/// Creates the book + job rows and runs the ingest inline (the job
/// status is visible via GET /api/v1/ingest/{id}).
pub async fn api_upload_book(
    State(state): State<AppState>,
    user: SessionUser,
    _admin: mycelium_auth::rbac::RequireAdmin,
    mut multipart: axum::extract::Multipart,
) -> Response {
    let mut bookshelf = String::new();
    let mut slug = String::new();
    let mut title = String::new();
    let mut file: Option<Vec<u8>> = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "bookshelf" => bookshelf = field.text().await.unwrap_or_default(),
            "slug" => slug = field.text().await.unwrap_or_default(),
            "title" => title = field.text().await.unwrap_or_default(),
            "file" => match field.bytes().await {
                Ok(bytes) => file = Some(bytes.to_vec()),
                Err(e) => {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        &format!("file read failed: {e}"),
                    );
                }
            },
            _ => {}
        }
    }
    let Some(file) = file else {
        return error_response(StatusCode::BAD_REQUEST, "missing file field");
    };
    // The effective limit is admin-configured (ConfigStore key
    // "upload", default 32 MiB — the original Mycelium's default).
    let upload_cfg = state.upload_config().await;
    let max_bytes = upload_cfg.max_book_mib * 1024 * 1024;
    if file.len() as u64 > max_bytes {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            &format!("book exceeds {} MiB", upload_cfg.max_book_mib),
        );
    }
    let Some(text) = String::from_utf8(file).ok() else {
        return error_response(StatusCode::BAD_REQUEST, "book must be UTF-8 markdown");
    };
    if text.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "book text is empty");
    }
    // Resolve the bookshelf by name.
    let shelf_id = match state.store.bookshelf_id_by_name(&bookshelf).await {
        Ok(Some(id)) => id,
        _ => return error_response(StatusCode::BAD_REQUEST, "unknown bookshelf"),
    };
    let user_id = user.user_id;
    let slug = mycelium_librarian::slugify(&slug);
    if slug.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "invalid slug");
    }
    match state
        .librarian
        .submit(shelf_id, user_id, &slug, &title, &text)
        .await
    {
        Ok(job) => {
            // Run only the submitted job inline (one job per user; the
            // request blocks until THIS catalog is written — never the
            // whole queue, which could stall for minutes).
            let _ = state.librarian.run_job(job.job_id).await;
            let status = state.librarian.job_status(job.job_id).await;
            let status = status.ok().flatten();
            let (status_str, detail) = status
                .map(|s| (s.status.as_str(), s.detail))
                .unwrap_or(("unknown", String::new()));
            if status_str == "done" {
                crate::state::Metrics::inc(&state.metrics.books_ingested);
            } else if status_str == "failed" {
                crate::state::Metrics::inc(&state.metrics.ingest_failures);
            }
            let body = serde_json::json!({
                "job_id": job.job_id,
                "book_id": job.book_id,
                "slug": job.slug,
                "status": status_str,
                "detail": detail,
            });
            (StatusCode::ACCEPTED, Json(body)).into_response()
        }
        Err(mycelium_librarian::WorkerError::UserBusy) => error_response(
            StatusCode::CONFLICT,
            "user already has an ingest job running",
        ),
        Err(mycelium_librarian::WorkerError::Store(mycelium_store::BooksError::DuplicateSlug(
            s,
        ))) => error_response(StatusCode::CONFLICT, &format!("duplicate book slug: {s}")),
        Err(e) => {
            tracing::error!(error = %e, "book submit failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// GET /api/v1/ingest — list recent ingest jobs (admin only).
pub async fn api_ingest_jobs(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
) -> Response {
    let jobs = state.store.list_ingest_jobs(50).await.unwrap_or_default();
    let body: Vec<serde_json::Value> = jobs
        .iter()
        .map(|j| {
            serde_json::json!({
                "id": j.id,
                "bookshelf_id": j.bookshelf_id,
                "book_id": j.book_id,
                "status": j.status.as_str(),
                "detail": j.detail,
                "created_at": j.created_at.to_rfc3339(),
            })
        })
        .collect();
    Json(body).into_response()
}

/// GET /api/v1/ingest/{id} — one job's status (admin only).
pub async fn api_ingest_job(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Ok(id) = uuid::Uuid::parse_str(&id) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid job id");
    };
    match state.librarian.job_status(id).await {
        Ok(Some(status)) => Json(serde_json::json!({
            "id": status.id,
            "status": status.status.as_str(),
            "detail": status.detail,
        }))
        .into_response(),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "job not found"),
        Err(e) => {
            tracing::error!(error = %e, "job status lookup failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

#[derive(Deserialize)]
pub struct PassageParams {
    /// A `book://<slug>#<anchor>` resource string.
    pub resource: String,
}

/// GET /api/v1/passages?resource=book://slug#anchor — read a passage
/// from the shared library stacks. Global-read bookshelves are
/// readable by all users; admin-private ones by admins only.
pub async fn api_passage(
    State(state): State<AppState>,
    user: SessionUser,
    Query(params): Query<PassageParams>,
) -> Response {
    let Some(book_ref) = mycelium_core::library::parse_book_ref(&params.resource) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid book:// resource");
    };
    // The book must exist and its shelf must be global-read (or the
    // requester is an admin).
    let Some(book) = state
        .store
        .book_by_slug(&book_ref.slug)
        .await
        .unwrap_or(None)
    else {
        return error_response(StatusCode::NOT_FOUND, "book not found");
    };
    let shelf_row: Option<(i64,)> =
        sqlx::query_as("SELECT is_global_read FROM bookshelves WHERE id = ?")
            .bind(book.bookshelf_id.to_string())
            .fetch_optional(state.store.pool())
            .await
            .ok()
            .flatten();
    let Some((is_global,)) = shelf_row else {
        return error_response(StatusCode::NOT_FOUND, "bookshelf not found");
    };
    if is_global == 0 && user.role != Role::Admin {
        return error_response(StatusCode::FORBIDDEN, "bookshelf is not global-read");
    }
    // Read the stack text and extract the passage (admin-configurable
    // cap, pre-clamped).
    let text =
        match mycelium_librarian::read_stack_text(&state.store, &state.service_key, &book_ref.slug)
            .await
        {
            Ok(t) => t,
            Err(_) => return error_response(StatusCode::NOT_FOUND, "book text unavailable"),
        };
    let cap = state.security_config().await.passage_max_chars;
    match mycelium_core::library::extract_passage_capped(
        &book_ref.slug,
        &book_ref.anchor,
        &text,
        cap,
    ) {
        Ok(passage) => Json(serde_json::json!({
            "slug": passage.slug,
            "anchor": passage.anchor,
            "text": passage.text,
        }))
        .into_response(),
        Err(e) => error_response(StatusCode::NOT_FOUND, &e.to_string()),
    }
}

// ---------- Librarian chat (v1 parity: the agent behind the UI) ----------

#[derive(Deserialize)]
pub struct ChatMessageForm {
    /// The user's message.
    pub message: String,
}

/// POST /api/v1/chat — send a message to the librarian agent. The agent
/// (same one the MCP tools use) answers over the caller's private
/// bundle; chat mode also allows write tools, so "record that ..."
/// messages persist knowledge. Returns the reply as JSON (the page
/// renders it); streaming arrives with a streaming LLM client later.
pub async fn api_chat(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
    axum::Form(form): axum::Form<ChatMessageForm>,
) -> Response {
    if form.message.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "empty message");
    }
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "key unavailable"),
    };
    let cs = ConceptStore::for_user(&state.store, user.user_id, master);
    let llm: Option<crate::LlmConfig> = state.config.get("llm").await.unwrap_or(None);
    let llm = llm.unwrap_or_default();
    let client = mycelium_librarian::llm::LlmClient::new(&llm);
    // Multi-turn memory: the session's prior turns + this message.
    let mut history = state.chat_history.get(session.0);
    history.push(mycelium_librarian::llm::ConversationTurn::User(
        form.message.clone(),
    ));
    match mycelium_librarian::agent::run_chat(&client, &cs, &history).await {
        Ok(reply) => {
            state.chat_history.push(
                session.0,
                mycelium_librarian::llm::ConversationTurn::User(form.message.clone()),
            );
            state.chat_history.push(
                session.0,
                mycelium_librarian::llm::ConversationTurn::Assistant(reply.clone()),
            );
            Json(serde_json::json!({ "reply": reply })).into_response()
        }
        Err(mycelium_librarian::agent::AgentError::Llm(e)) => {
            tracing::warn!(error = %e, "librarian chat LLM failure");
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "the librarian is unavailable (no LLM backend reachable) — configure one in the admin portal",
            )
        }
        Err(e) => {
            tracing::error!(error = %e, "librarian chat failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// POST /api/v1/chat/stream — the streaming variant: server-sent
/// events with the agent's progress (tool invocations) and the final
/// reply. Event format (one JSON object per SSE `data:` line):
///   {"type":"tool","name":"search_knowledge","detail":"..."}
///   {"type":"done","reply":"..."}
///   {"type":"error","error":"..."}
pub async fn api_chat_stream(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
    axum::Form(form): axum::Form<ChatMessageForm>,
) -> Response {
    if form.message.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "empty message");
    }
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "key unavailable"),
    };
    let llm: Option<crate::LlmConfig> = state.config.get("llm").await.unwrap_or(None);
    let llm = llm.unwrap_or_default();
    let client = mycelium_librarian::llm::LlmClient::new(&llm);
    // Multi-turn memory: the session's prior turns + this message.
    let mut history = state.chat_history.get(session.0);
    history.push(mycelium_librarian::llm::ConversationTurn::User(
        form.message.clone(),
    ));
    state.chat_history.push(
        session.0,
        mycelium_librarian::llm::ConversationTurn::User(form.message.clone()),
    );

    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<mycelium_librarian::agent::AgentEvent>();
    let run_tx = tx.clone();
    // The agent runs in a task owning its own Store clone (Store is
    // cheap-clone: pool handle + path) so the ConceptStore's borrow
    // lives inside the task. The handler streams events as they
    // arrive; transport errors that abort before any event surface as
    // a Failed event.
    let task_store = (*state.store).clone();
    tokio::spawn(async move {
        let cs = ConceptStore::for_user(&task_store, user.user_id, master);
        let result = mycelium_librarian::agent::run_chat_streaming(
            &client,
            &cs,
            &history,
            Some(run_tx.clone()),
        )
        .await;
        if let Err(e) = result {
            let msg = if matches!(e, mycelium_librarian::agent::AgentError::Llm(_)) {
                "the librarian is unavailable (no LLM backend reachable) — configure one in the admin portal"
            } else {
                "internal error"
            };
            let _ = run_tx.send(mycelium_librarian::agent::AgentEvent::Failed(
                msg.to_string(),
            ));
        }
    });

    let stream = async_stream::stream! {
        while let Some(event) = rx.recv().await {
            let payload = match &event {
                mycelium_librarian::agent::AgentEvent::Tool { name, detail } => {
                    serde_json::json!({ "type": "tool", "name": name, "detail": detail })
                }
                mycelium_librarian::agent::AgentEvent::Done(reply) => {
                    // Multi-turn memory: record the assistant reply.
                    state.chat_history.push(
                        session.0,
                        mycelium_librarian::llm::ConversationTurn::Assistant(reply.clone()),
                    );
                    serde_json::json!({ "type": "done", "reply": reply })
                }
                mycelium_librarian::agent::AgentEvent::Failed(error) => {
                    serde_json::json!({ "type": "error", "error": error })
                }
            };
            yield Ok::<String, std::convert::Infallible>(format!("data: {payload}\n\n"));
            if matches!(event, mycelium_librarian::agent::AgentEvent::Done(_) | mycelium_librarian::agent::AgentEvent::Failed(_)) {
                break;
            }
        }
    };
    let body = axum::body::Body::from_stream(stream);
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/event-stream"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
    )
        .into_response_with_bytes_stream(body)
}

/// GET /chat — the chat page (librarian agent over the user's bundle).
pub async fn chat_view(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    pages::chat_page(&user, &csrf).into_response()
}
