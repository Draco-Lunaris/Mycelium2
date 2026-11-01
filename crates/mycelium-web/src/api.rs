//! REST API under /api/v1 plus the form-driven page handlers.

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use uuid::Uuid;

use mycelium_auth::rbac::{Role, SessionUser};
use mycelium_core::concept::{Concept, LifecycleStatus};
use mycelium_core::search::{SearchMode, SearchQuery};
use mycelium_store::{
    ConceptStore, FileRepo, RevisionPathEntry, RevisionStore, RevisionsError, Scope,
    SkillBundleError, SkillGroup, bundle_skill_files, chat::ChatError, group_skills,
};

use crate::middleware::SessionId;
use crate::pages;
use crate::state::AppState;

/// The optional `mode` query parameter (GET /api/v1/search + /search)
/// parsed into the composer's [`SearchMode`]: `auto` (the default —
/// fused when the embeddings backend is configured and healthy) /
/// `lexical` / `vector_only`. Unknown values → HTTP 400 (this is an
/// API-level misuse, not a degradation — the degrade rules cover
/// backend weather, never a caller's spelling).
pub(crate) fn parse_mode_param(
    mode: &Option<String>,
) -> Result<mycelium_core::search::SearchMode, StatusCode> {
    match mode.as_deref().unwrap_or("auto") {
        "auto" => Ok(SearchMode::Auto),
        "lexical" => Ok(SearchMode::Lexical),
        "vector_only" => Ok(SearchMode::VectorOnly),
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

/// Construct the librarian agent's cross-scope capabilities from a store
/// and service key (global skills + library + full-text book search).
/// `visible_slugs` filters library access by bookshelf visibility
/// (None = unrestricted, i.e. admin).
pub(crate) fn build_agent_scopes<'a>(
    store: &'a mycelium_store::Store,
    service_key: &'a mycelium_crypto::keys::ServiceKey,
    visible_slugs: Option<std::collections::HashSet<String>>,
    user_id: uuid::Uuid,
) -> mycelium_librarian::agent::AgentScopes<'a> {
    let store_arc = store.clone();
    let sk = service_key.clone();
    let read_stack_text: Option<Box<mycelium_librarian::agent::StackTextReader>> =
        Some(Box::new(move |slug| {
            let s = store_arc.clone();
            let k = sk.clone();
            let slug = slug.to_string();
            Box::pin(async move {
                mycelium_librarian::read_stack_text(&s, &k, &slug)
                    .await
                    .map_err(|e| e.to_string())
            })
        }));
    mycelium_librarian::agent::AgentScopes {
        skills: Some(ConceptStore::for_service(
            store,
            service_key.clone(),
            &store.skills_dir(),
            "skills",
        )),
        library: Some(ConceptStore::for_service(
            store,
            service_key.clone(),
            &store.library_dir(),
            "library",
        )),
        // Scoped OUT of the hybrid-retrieval stream (memory-lifecycle
        // §3.3 wires the two retrieval surfaces: MCP memory_query hits
        // + the REST search): this scope list serves the chat-runner
        // agent and the mutation-queue drain-fallback — deterministic
        // add/delete work, and a try-class worker must not take
        // embeddings latency on its critical path (the query-priority
        // invariant).
        embeddings: None,
        read_stack_text,
        visible_slugs,
        trace: Some(mycelium_librarian::agent::TraceSink {
            pool: store.pool().clone(),
            scope_id: format!("user:{user_id}"),
        }),
    }
}

/// The set of book slugs the user may see in the library: books on
/// global-read shelves. Admins get None (unrestricted). `pub(crate)`:
/// the chat submit endpoints resolve it once at submit time for the
/// detached runner's context.
pub(crate) async fn visible_library_slugs(
    state: &AppState,
    user: &SessionUser,
) -> Option<std::collections::HashSet<String>> {
    if user.role == Role::Admin {
        return None;
    }
    let shelves = state
        .store
        .list_bookshelves_detailed()
        .await
        .unwrap_or_default();
    let mut out = std::collections::HashSet::new();
    for (id, _name, global_read) in shelves {
        if !global_read {
            continue;
        }
        for (slug, _title) in state.store.books_on_shelf(id).await.unwrap_or_default() {
            out.insert(slug);
        }
    }
    Some(out)
}

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
        .route("/dream", post(api_dream))
        .route("/chat", post(api_chat))
        // Session-scoped chat (spec §3.5): the sidebar list + meta, the
        // draft-create and submit SSE streams, reattach, and delete
        // (JSON + native-form variants). The legacy stateless stream
        // route is gone (D9); POST /api/v1/chat above stays stateless.
        .route(
            "/chat/sessions",
            get(crate::chat_api::api_chat_sessions).post(crate::chat_api::api_chat_create_session),
        )
        .route(
            "/chat/sessions/{id}",
            get(crate::chat_api::api_chat_session_meta)
                .delete(crate::chat_api::api_chat_delete_json),
        )
        .route(
            "/chat/sessions/{id}/messages",
            post(crate::chat_api::api_chat_submit_session),
        )
        .route(
            "/chat/sessions/{id}/stream",
            get(crate::chat_api::api_chat_reattach),
        )
        .route(
            "/chat/sessions/{id}/delete",
            post(crate::chat_api::api_chat_delete_redirect),
        )
        // Skill bundle download: deterministic zip of a nested skill
        // (global shelf for everyone, private skills owner-only).
        .route("/skills/{slug}/bundle", get(skill_bundle))
        // Concept revisions (spec §3.2a-2): the undo log's mechanical
        // REST surfaces — the per-path index (live AND dead paths), one
        // path's chain, one snapshot, and the restore. Query-param
        // shaped (axum 0.8 wildcards cannot sit mid-path, so `?path=`
        // + `?scope=` carry the concept identity; the same convention
        // as the /concept pages). NO era or confirmation logic here —
        // the UI's confirm interstitial is the pages layer (Task 4).
        .route("/revisions", get(api_revisions_index))
        .route("/revisions/list", get(api_revisions_list))
        .route("/revisions/snapshot", get(api_revisions_snapshot))
        .route("/revisions/restore", post(api_revisions_restore))
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
    let mut known = std::collections::HashSet::new();
    for entry in cs.list().await.unwrap_or_default() {
        known.insert(entry.path.clone());
        nodes.push(GraphNode {
            id: entry.path.clone(),
            title: entry.title.clone(),
            concept_type: entry.concept_type.clone(),
        });
    }
    // Deduplicate edges (a concept linking to the same target twice is one
    // relationship) and sort for deterministic output — matching the core
    // graph builder.
    let mut edge_set = std::collections::HashSet::new();
    for entry_path in known.iter() {
        if let Ok(concept) = cs.get(entry_path).await {
            for target in mycelium_core::links::scan_links(&concept.body) {
                if known.contains(&target) {
                    edge_set.insert((concept.source_path.clone(), target));
                }
            }
        }
    }
    let mut edges: Vec<GraphEdge> = edge_set
        .into_iter()
        .map(|(from, to)| GraphEdge { from, to })
        .collect();
    edges.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.to.cmp(&b.to)));
    Json(GraphResponse { nodes, edges }).into_response()
}

#[derive(Deserialize)]
pub struct SearchParams {
    #[serde(default)]
    q: String,
    #[serde(default)]
    global: Option<String>,
    /// Optional retrieval mode: `auto` (default) / `lexical` /
    /// `vector_only`. Unknown names → HTTP 400 (parse_mode_param).
    #[serde(default)]
    mode: Option<String>,
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
/// default on the web) the global skills shelf and the library catalogs.
/// The composition itself is the SHARED librarian composer
/// (`cross_scope_hits`) — one fused pipeline, not a web-local copy: the
/// scope order, RRF weights, caps and sort rule match the MCP hit path
/// exactly. Library hits from admin-private bookshelves are filtered
/// out for non-admins (visibility post-filter baked into the composer
/// via the scope list's `visible_slugs`). The user scope is `None` when
/// the master key is unusable — the global shelves stay reachable,
/// exactly the pre-fusion posture. Web passes NO per-hit cap
/// (`usize::MAX`): the page ranked everything, and the fusion keeps
/// that (its pools are bounded per scope).
async fn search_all_scopes(
    state: &AppState,
    user: &SessionUser,
    query: &SearchQuery,
    include_global: bool,
) -> Vec<ScopedResult> {
    let master = state.master_key_for(user.user_id).await.ok();
    let user_cs = master.map(|m| ConceptStore::for_user(&state.store, user.user_id, m));
    let (skills, library) = if include_global {
        (
            Some(ConceptStore::for_service(
                &state.store,
                (*state.service_key).clone(),
                &state.store.skills_dir(),
                "skills",
            )),
            Some(ConceptStore::for_service(
                &state.store,
                (*state.service_key).clone(),
                &state.store.library_dir(),
                "library",
            )),
        )
    } else {
        (None, None)
    };
    // One embeddings client per request, from the sealed `embeddings`
    // row, reusing the SHARED lifecycle composer (the MCP hit path
    // passes the same shape). Row absent/undecryptable → None → the
    // composer degrades to lexical, never an error (never a fabricated
    // default client — unwrap_or_default would point at
    // localhost:11434).
    let embeddings =
        crate::state::decrypt_config::<mycelium_librarian::embeddings::EmbeddingsConfig>(
            &state.config,
            &state.service_key,
            "embeddings",
        )
        .await
        .map(|cfg| mycelium_librarian::embeddings::EmbeddingsClient::new(&cfg.clamp()));
    let scopes = mycelium_librarian::agent::AgentScopes {
        skills,
        library,
        embeddings,
        read_stack_text: None,
        // Bookshelf visibility, precomputed as a slug set (the same
        // tables book_by_slug consulted per hit before) — None for
        // admins (the option IS the allowlist-or-unrestricted shape).
        visible_slugs: visible_library_slugs(state, user).await,
        trace: None,
    };
    mycelium_librarian::agent::cross_scope_hits(
        user_cs.as_ref(),
        &scopes,
        query,
        usize::MAX,
        scopes.embeddings.as_ref(),
    )
    .await
    .into_iter()
    .map(|(tag, hit)| ScopedResult {
        concept_path: hit.concept_path,
        title: hit.title,
        snippet: hit.snippet,
        score: hit.score,
        scope: match tag.as_str() {
            "user" => "user",
            "skills" => "skills",
            _ => "library",
        },
    })
    .collect()
}

/// GET /api/v1/search?q=...&global=1&mode=auto|lexical|vector_only —
/// user bundle (+ global shelves when global=1, the web default per
/// DESIGN). Unknown `mode` values → 400.
async fn api_search(
    State(state): State<AppState>,
    user: SessionUser,
    Query(params): Query<SearchParams>,
) -> Response {
    let mode = match parse_mode_param(&params.mode) {
        Ok(mode) => mode,
        Err(status) => {
            return error_response(status, "invalid mode (auto | lexical | vector_only)");
        }
    };
    let terms = params
        .q
        .split_whitespace()
        .map(|t| t.to_string())
        .collect::<Vec<_>>();
    let mut query = SearchQuery::new(terms);
    query.include_global = params.global.as_deref() != Some("0");
    query.mode = mode;
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
    let cs = ConceptStore::for_user(&state.store, user.user_id, master)
        .with_provenance(format!("web:{}", user.username));
    match cs.put(&concept).await {
        Ok(()) => {
            crate::state::Metrics::inc(&state.metrics.concepts_written);
            (StatusCode::CREATED, "created").into_response()
        }
        Err(e) => match &e {
            // A write-time lifecycle reject (the put path's
            // `Frontmatter::validated` guard) is the client's bad
            // input, not a server failure: 400 with the concept error
            // text — the message the form path's warning banner
            // carries, so both surfaces stay in parity.
            mycelium_store::ConceptStoreError::Parse(
                mycelium_core::ConceptError::InvalidLifecycleStatus { .. }
                | mycelium_core::ConceptError::InvalidSupersededBy { .. }
                | mycelium_core::ConceptError::InvalidValidTo { .. },
            ) => error_response(StatusCode::BAD_REQUEST, &e.to_string()),
            _ => {
                tracing::error!(error = %e, "concept put failed");
                error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        },
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
///
/// Since the lifecycle store (Task 2) fails loudly on a missing
/// concept's delete (the re-delete contract), a NotFound maps to 404 —
/// the legacy FileRepo swallow (which answered 204 for a path that was
/// never there) is gone at this layer.
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
        Err(mycelium_store::ConceptStoreError::NotFound(_)) => {
            error_response(StatusCode::NOT_FOUND, "not found")
        }
        Err(e) => {
            tracing::error!(error = %e, "concept delete failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

// ---------- Revisions REST (spec §3.2a-2) ----------
//
// Query-param shaped: axum 0.8 `{*path}` wildcards cannot sit mid-path,
// so the concept identity rides as `?scope=` + `?path=` (the /concept
// pages' convention). Every route is mechanical — the era/confirmation
// logic is the UI's (Task 4), never the API's. Perms (mirroring
// concept_view's scope posture):
//   READ (index/list/snapshot): user scope = the caller's own bundle;
//   skills list/snapshot = any user; library = shelf visibility
//   (concept_view's library posture); the INDEX of a service scope is
//   admin only.
//   WRITE (restore): user own / skills admin / library never.

/// The revisions routes' refusal, before it becomes a `Response`: a
/// (status, message) pair. Every refusal on these routes is a static
/// status + text pair, so the small pair rides the `Result`'s Err
/// variant and `revisions_reject` materializes it at the return site —
/// a whole `Response` there would be a large-error return
/// (clippy::result_large_err; the same shape as chat_api's `Admission`
/// enum).
type RevRejection = (StatusCode, &'static str);

/// Materialize a refusal at its return site.
fn revisions_reject((status, message): RevRejection) -> Response {
    error_response(status, message)
}

/// One of the three revisions scopes (query-param values).
#[derive(Debug, Clone, Copy)]
enum RevScope {
    User,
    Skills,
    Library,
}

impl RevScope {
    /// Parse the `?scope=` value; absent defaults to the user's own
    /// bundle (the ConceptQuery convention). Unknown → 400.
    fn parse(raw: Option<&str>) -> Result<Self, RevRejection> {
        match raw.unwrap_or("user") {
            "user" => Ok(RevScope::User),
            "skills" => Ok(RevScope::Skills),
            "library" => Ok(RevScope::Library),
            _ => Err((
                StatusCode::BAD_REQUEST,
                "unknown scope (user | skills | library)",
            )),
        }
    }

    /// The redirect/query-param spelling (library never redirects —
    /// restores are refused there — so no string is needed).
    fn as_param(self) -> &'static str {
        match self {
            RevScope::User => "user",
            RevScope::Skills => "skills",
            RevScope::Library => "library",
        }
    }
}

#[derive(Deserialize)]
struct RevisionsQuery {
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    path: Option<String>,
    /// The revision number — parsed manually so a missing/non-numeric/0
    /// value answers the spec's 400 (a typed extractor would reject the
    /// request with an uncontrolled body).
    #[serde(default)]
    n: Option<String>,
}

/// Canonicalize the query's concept path (leading slash, like every
/// other handler); absent → 400.
fn revisions_canonical(raw: Option<&str>) -> Result<String, RevRejection> {
    let Some(path) = raw else {
        return Err((StatusCode::BAD_REQUEST, "missing path"));
    };
    Ok(format!("/{}", path.trim_start_matches('/')))
}

/// Parse the 1-based revision number: missing/non-numeric/0 → 400.
fn revisions_revision_no(raw: Option<&str>) -> Result<u64, RevRejection> {
    let Some(text) = raw else {
        return Err((StatusCode::BAD_REQUEST, "missing revision number"));
    };
    let no: u64 = text
        .trim()
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid revision number"))?;
    if no == 0 {
        return Err((StatusCode::BAD_REQUEST, "revision numbers are 1-based"));
    }
    Ok(no)
}

/// Map a RevisionStore failure: a missing revision is a clean 404;
/// everything else is a server-side fault (logged, generic body — the
/// file's convention).
fn revisions_error_response(e: RevisionsError, ctx: &'static str) -> Response {
    match &e {
        RevisionsError::NotFound { .. } => error_response(StatusCode::NOT_FOUND, "not found"),
        other => {
            tracing::error!(error = %other, ctx);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// Whether `user` may read `canonical` in the library scope: the
/// concept's book slug must sit on a global-read shelf (or the reader
/// is an admin) — concept_view's library posture, extracted so the
/// revisions routes share it.
async fn library_path_visible(state: &AppState, user: &SessionUser, canonical: &str) -> bool {
    let slug = canonical
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or_default()
        .trim_end_matches(".md")
        .to_string();
    match state.store.book_by_slug(&slug).await {
        Ok(Some(book)) => {
            let global_read = state
                .store
                .bookshelf_is_global_read(book.bookshelf_id)
                .await
                .unwrap_or(false);
            global_read || user.role == Role::Admin
        }
        _ => false,
    }
}

/// GET /api/v1/revisions?scope= — the scope's per-path revisions index
/// (live AND dead paths; metadata only, no payload text). User scope:
/// the caller's own bundle; service scopes: admin only (spec §3.2a-2).
async fn api_revisions_index(
    State(state): State<AppState>,
    user: SessionUser,
    Query(q): Query<RevisionsQuery>,
) -> Response {
    let scope = match RevScope::parse(q.scope.as_deref()) {
        Ok(s) => s,
        Err(rej) => return revisions_reject(rej),
    };
    let store = match revisions_index_store(&state, &user, scope).await {
        Ok(store) => store,
        Err(rej) => return revisions_reject(rej),
    };
    match store.list_paths().await {
        Ok(entries) => Json(entries).into_response(),
        Err(e) => revisions_error_response(e, "revisions index failed"),
    }
}

/// The index route's store: user scope = own bundle (no extra gate —
/// the session user reads their own undo log); service scopes = admin
/// only. The returned store borrows from `state`.
async fn revisions_index_store<'a>(
    state: &'a AppState,
    user: &SessionUser,
    scope: RevScope,
) -> Result<RevisionStore<'a>, RevRejection> {
    match scope {
        RevScope::User => {
            let master = state
                .master_key_for(user.user_id)
                .await
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "key unavailable"))?;
            Ok(RevisionStore::for_user(
                state.store.pool(),
                user.user_id,
                &master,
            ))
        }
        service @ (RevScope::Skills | RevScope::Library) => {
            if user.role != Role::Admin {
                return Err((StatusCode::FORBIDDEN, "admins only"));
            }
            let namespace = if matches!(service, RevScope::Skills) {
                "skills"
            } else {
                "library"
            };
            Ok(RevisionStore::for_service(
                state.store.pool(),
                &state.service_key,
                namespace,
            ))
        }
    }
}

/// GET /api/v1/revisions/list?scope=&path= — one path's chain, newest
/// first (metadata only). Read perms per concept: user own / skills any
/// / library shelf visibility.
async fn api_revisions_list(
    State(state): State<AppState>,
    user: SessionUser,
    Query(q): Query<RevisionsQuery>,
) -> Response {
    let canonical = match revisions_canonical(q.path.as_deref()) {
        Ok(c) => c,
        Err(rej) => return revisions_reject(rej),
    };
    let store = match read_gate_store_scope(&state, &user, q.scope.as_deref(), &canonical).await {
        Ok(store) => store,
        Err(rej) => return revisions_reject(rej),
    };
    match store.list(&canonical).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => revisions_error_response(e, "revisions list failed"),
    }
}

/// GET /api/v1/revisions/snapshot?scope=&path=&n= — one recorded
/// revision's raw markdown (the same text/markdown posture as
/// api_get_concept). Read perms as list; `n=0` → 400 (1-based).
async fn api_revisions_snapshot(
    State(state): State<AppState>,
    user: SessionUser,
    Query(q): Query<RevisionsQuery>,
) -> Response {
    let n = match revisions_revision_no(q.n.as_deref()) {
        Ok(n) => n,
        Err(rej) => return revisions_reject(rej),
    };
    let canonical = match revisions_canonical(q.path.as_deref()) {
        Ok(c) => c,
        Err(rej) => return revisions_reject(rej),
    };
    let store = match read_gate_store_scope(&state, &user, q.scope.as_deref(), &canonical).await {
        Ok(store) => store,
        Err(rej) => return revisions_reject(rej),
    };
    match store.get(&canonical, n).await {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "text/markdown")],
            )
                .into_response_with_body(text),
            Err(e) => {
                tracing::error!(error = %e, "revision snapshot is not utf-8");
                error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        },
        Err(e) => revisions_error_response(e, "revision snapshot failed"),
    }
}

/// The shared read gate + store for the list/snapshot routes: resolves
/// the scope (unknown → 400), applies the per-concept read perms, and
/// opens the scope's RevisionStore (borrowing from `state`). The user
/// scope needs the caller's master key; skills needs none (any user
/// reads); library needs shelf visibility (concept_view's posture).
async fn read_gate_store_scope<'a>(
    state: &'a AppState,
    user: &SessionUser,
    scope_raw: Option<&str>,
    canonical: &str,
) -> Result<RevisionStore<'a>, RevRejection> {
    let scope = RevScope::parse(scope_raw)?;
    match scope {
        RevScope::User => {
            let master = state
                .master_key_for(user.user_id)
                .await
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "key unavailable"))?;
            Ok(RevisionStore::for_user(
                state.store.pool(),
                user.user_id,
                &master,
            ))
        }
        RevScope::Skills => Ok(RevisionStore::for_service(
            state.store.pool(),
            &state.service_key,
            "skills",
        )),
        RevScope::Library => {
            if !library_path_visible(state, user, canonical).await {
                return Err((StatusCode::FORBIDDEN, "bookshelf is not global-read"));
            }
            Ok(RevisionStore::for_service(
                state.store.pool(),
                &state.service_key,
                "library",
            ))
        }
    }
}

/// POST /api/v1/revisions/restore?scope=&path=&n= — apply a recorded
/// revision through the store's normal stamped write path. WRITE perms
/// precede the store: user own / skills admin / library ALWAYS 403.
/// Content-Type application/json answers `{"restored_to": n}`; a form
/// POST answers 303 to the concept viewer (the CSRF middleware gates
/// the form variant). A snapshot that no longer parses is the caller's
/// bad input → 400 with the concept parse error text (the same posture
/// as the PUT-concept route's lifecycle rejects).
async fn api_revisions_restore(
    State(state): State<AppState>,
    user: SessionUser,
    headers: axum::http::HeaderMap,
    Query(q): Query<RevisionsQuery>,
) -> Response {
    let is_form = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));
    let scope = match RevScope::parse(q.scope.as_deref()) {
        Ok(s) => s,
        Err(rej) => return revisions_reject(rej),
    };
    let canonical = match revisions_canonical(q.path.as_deref()) {
        Ok(c) => c,
        Err(rej) => return revisions_reject(rej),
    };
    let n = match revisions_revision_no(q.n.as_deref()) {
        Ok(n) => n,
        Err(rej) => return revisions_reject(rej),
    };
    // The WRITE gate doubles as the store dispatch: a forbidden scope
    // answers 403 before any 404/other store signal (no oracle through
    // the permission gate).
    let result = match scope {
        RevScope::User => {
            let master = match state.master_key_for(user.user_id).await {
                Ok(m) => m,
                Err(_) => {
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, "key unavailable");
                }
            };
            ConceptStore::for_user(&state.store, user.user_id, master)
                .with_provenance(format!("web:{}", user.username))
                .restore(&canonical, n)
                .await
        }
        RevScope::Skills if user.role == Role::Admin => {
            ConceptStore::for_service(
                &state.store,
                (*state.service_key).clone(),
                &state.store.skills_dir(),
                "skills",
            )
            .with_provenance(format!("web:{}", user.username))
            .restore(&canonical, n)
            .await
        }
        RevScope::Skills => return error_response(StatusCode::FORBIDDEN, "admins only"),
        RevScope::Library => return error_response(StatusCode::FORBIDDEN, "library is read-only"),
    };
    match result {
        Ok(()) => {
            crate::state::Metrics::inc(&state.metrics.concepts_written);
            if is_form {
                Redirect::to(&format!(
                    "/concept?path={}&scope={}",
                    pages::urlencoding_encode(&canonical),
                    scope.as_param() // library never restores, so never "library"
                ))
                .into_response()
            } else {
                Json(serde_json::json!({ "restored_to": n })).into_response()
            }
        }
        Err(mycelium_store::ConceptStoreError::Revisions(RevisionsError::NotFound { .. }))
        | Err(mycelium_store::ConceptStoreError::NotFound(_)) => {
            error_response(StatusCode::NOT_FOUND, "not found")
        }
        Err(
            e @ (mycelium_store::ConceptStoreError::Parse(_)
            | mycelium_store::ConceptStoreError::Reserved(_)
            | mycelium_store::ConceptStoreError::QueuePath(_)),
        ) => error_response(StatusCode::BAD_REQUEST, &e.to_string()),
        Err(e) => {
            tracing::error!(error = %e, "revision restore failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

// ---------- Revisions browser page (Task 5: GET /revisions) ----------

#[derive(Deserialize)]
pub struct RevisionsPageQuery {
    /// The scope to browse — absent → the caller's own bundle (the
    /// ConceptQuery convention).
    #[serde(default)]
    pub scope: Option<String>,
}

/// GET /revisions — the scope-level revisions browser page (spec §3.2:
/// "owner for user scopes; admin for service scopes" listing every path
/// with revisions, live or deleted). The revisions REST INDEX's exact
/// perm matrix ([`revisions_index_store`]): user scope = the session
/// user's own bundle; skills/library = admin only, answered IN PLACE as
/// 403 — never a redirect, which would signal which scopes exist. Rows
/// in path order, each linking into Task 4's per-concept page (which
/// now serves dead paths' history).
pub async fn revisions_view(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
    Query(q): Query<RevisionsPageQuery>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let scope = match RevScope::parse(q.scope.as_deref()) {
        Ok(s) => s,
        Err(rej) => return revisions_reject(rej),
    };
    let rev_store = match revisions_index_store(&state, &user, scope).await {
        Ok(s) => s,
        Err(rej) => return revisions_reject(rej),
    };
    // The registry-title fallback's concept store (see
    // [`revisions_row_title`]): the same scope's live concepts.
    let cs = match scope {
        RevScope::User => {
            let master = match state.master_key_for(user.user_id).await {
                Ok(m) => m,
                Err(e) => {
                    tracing::error!(error = %e, "revisions browser: master key unavailable");
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, "key unavailable");
                }
            };
            ConceptStore::for_user(&state.store, user.user_id, master)
        }
        RevScope::Skills => ConceptStore::for_service(
            &state.store,
            (*state.service_key).clone(),
            &state.store.skills_dir(),
            "skills",
        ),
        RevScope::Library => ConceptStore::for_service(
            &state.store,
            (*state.service_key).clone(),
            &state.store.library_dir(),
            "library",
        ),
    };
    let paths = match rev_store.list_paths().await {
        Ok(paths) => paths,
        Err(e) => {
            // A store fault answers in place: the page shell with the
            // error banner (the books page's posture) — not a 500, not
            // a bare error body.
            tracing::error!(error = %e, "revisions browser listing failed");
            return pages::revisions_page(
                &user,
                &csrf,
                scope.as_param(),
                &[],
                Some("The revision history could not be loaded."),
            )
            .into_response();
        }
    };
    let mut rows = Vec::with_capacity(paths.len());
    for entry in &paths {
        let title = revisions_row_title(&rev_store, &cs, entry).await;
        rows.push(pages::RevisionPathRow {
            path: entry.path.clone(),
            live: entry.live,
            title,
            last_op: entry.last_op.clone(),
            last_op_at: entry.last_op_at.clone(),
        });
    }
    pages::revisions_page(&user, &csrf, scope.as_param(), &rows, None).into_response()
}

/// One browser row's title (Task 5's "latest title"): the NEWEST
/// recorded payload's frontmatter title, decoded through the same
/// `get` + parse path the snapshot REST route uses (§3.2a-3's payload
/// convention — create rows carry the created content, every other row
/// the pre-mutation state, so this is the latest title the undo log
/// recorded). The registry-title fallback is EXCLUSIVE to the
/// empty-payload row: `get` returning EMPTY bytes (after the AEAD open
/// this is the same fact [`RevisionEntry::payload_empty`] derives from
/// the envelope's stored length — an empty plaintext seals to exactly
/// the envelope's empty overhead — i.e. the §3.2a-3 "recorded no
/// content" marker, the undelete-restore row by design) on a LIVE row
/// means the log recorded nothing, so the live registry may supply the
/// title; a dead row has none. Anything ELSE — a titled or an untitled
/// frontmatter, non-UTF8 bytes, a parse fault, a decode fault — answers
/// from the payload only: an undecodable or simply untitled snapshot
/// shows the muted `(no title)` placeholder, never a live-title
/// substitution that would pretend the log recorded what it didn't.
/// Faults log per row and never page-fatal.
async fn revisions_row_title(
    rev_store: &RevisionStore<'_>,
    cs: &ConceptStore<'_>,
    entry: &RevisionPathEntry,
) -> Option<String> {
    match rev_store.get(&entry.path, entry.latest_no).await {
        // The §3.2a-3 empty-payload row (the undelete-restore marker):
        // the parse path is never entered (no frontmatter warn for the
        // designed case), and a live row alone may fall back to the
        // registry — the only fallback that exists.
        Ok(bytes) if bytes.is_empty() => {
            if !entry.live {
                return None;
            }
            match cs.get(&entry.path).await {
                Ok(concept) => concept.frontmatter.title.filter(|t| !t.is_empty()),
                Err(e) => {
                    tracing::warn!(path = entry.path, error = %e, "browser row registry title");
                    None
                }
            }
        }
        // A recorded payload answers for itself — titled (the
        // snapshot's title, however stale) or untitled (muted). Decode
        // or parse faults degrade that row to None with a log; the
        // registry is never substituted for a payload we could read.
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => match Concept::parse(&entry.path, &text) {
                Ok(concept) => concept.frontmatter.title.filter(|t| !t.is_empty()),
                Err(e) => {
                    tracing::warn!(
                        path = entry.path,
                        error = %e,
                        "browser row snapshot frontmatter"
                    );
                    None
                }
            },
            Err(e) => {
                tracing::warn!(path = entry.path, error = %e, "browser row snapshot bytes");
                None
            }
        },
        Err(e) => {
            tracing::warn!(
                path = entry.path,
                n = entry.latest_no,
                error = %e,
                "browser row snapshot decode"
            );
            None
        }
    }
}

/// GET /api/v1/skills/{slug}/bundle — download a skill as a
/// Claude-Code-shaped zip with deterministic bytes (fixed entry
/// order, mtime, perms — see `skill_zip`). Same visibility as the
/// skills page: the global skills shelf for everyone, private skills
/// only from the caller's own bundle. A slug naming only a legacy flat
/// skill (or nothing at all) is 404 — indistinguishable, never an
/// oracle. Integrity errors (traversal manifest, md5 mismatch, missing
/// payload, key recovery, zip failure) are logged server-side and
/// answered with a generic 500; no internal detail reaches the client.
pub async fn skill_bundle(
    State(state): State<AppState>,
    user: SessionUser,
    AxumPath(slug): AxumPath<String>,
) -> Response {
    // 1. Resolve the slug to a bundle: the global shelf (service key)
    //    first, then the caller's own bundle. NotBundle ("no
    //    /<slug>/skill.md here") falls through to the next surface; a
    //    hard error does not — the shelf is corrupt, which is a
    //    server-side problem, not a "try elsewhere".
    let global_cs = ConceptStore::for_service(
        &state.store,
        (*state.service_key).clone(),
        &state.store.skills_dir(),
        "skills",
    );
    let global_repo = FileRepo::new(state.store.skills_dir());
    let global_scope = Scope::Service((*state.service_key).clone());
    let files = match bundle_skill_files(&global_cs, &global_repo, &global_scope, &slug).await {
        Ok(files) => files,
        Err(SkillBundleError::NotBundle(_)) => {
            let master = match state.master_key_for(user.user_id).await {
                Ok(m) => m,
                Err(e) => {
                    tracing::error!(
                        slug = %slug,
                        user_id = %user.user_id,
                        error = %e,
                        "master key unavailable for private bundle lookup"
                    );
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
                }
            };
            let cs = ConceptStore::for_user(&state.store, user.user_id, master.clone());
            let repo = FileRepo::new(state.store.user_dir(user.user_id));
            match bundle_skill_files(&cs, &repo, &Scope::User(master), &slug).await {
                Ok(files) => files,
                Err(SkillBundleError::NotBundle(_)) => {
                    return error_response(StatusCode::NOT_FOUND, "not found");
                }
                Err(e) => {
                    tracing::error!(slug = %slug, error = %e, "skill bundle assembly failed");
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
                }
            }
        }
        Err(e) => {
            tracing::error!(slug = %slug, error = %e, "skill bundle assembly failed");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };

    // 2. Deterministic zip. In-memory (a Vec cursor) and effectively
    //    infallible, but never leaked: logged, generic 500.
    let bytes = match crate::skill_zip::zip_files(&files) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(slug = %slug, error = %e, "skill zip failed");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };

    // 3. Download response. The filename embeds the slug, so keep only
    //    filename-safe ASCII: a single route segment can still carry
    //    %-decoded quotes/backslashes (Content-Disposition attribute
    //    injection) or non-ASCII (an invalid HeaderValue — only after
    //    the archive was already built). no-store: the backup-download
    //    precedent — never sit in a shared/CDN cache.
    let safe: String = slug
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    let filename = if safe.is_empty() {
        "skill.zip".to_string()
    } else {
        format!("{safe}-skill.zip")
    };
    let mut response = axum::response::Response::new(axum::body::Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/zip"),
    );
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    let disposition = format!("attachment; filename=\"{filename}\"");
    match axum::http::HeaderValue::from_str(&disposition) {
        Ok(v) => {
            response
                .headers_mut()
                .insert(axum::http::header::CONTENT_DISPOSITION, v);
        }
        Err(e) => {
            tracing::error!(slug = %slug, error = %e, "content-disposition build failed");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    }
    response
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

/// API error response (also shared by the session-scoped chat module).
pub(crate) fn error_response(status: StatusCode, message: &str) -> Response {
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
/// `pub(crate)`: the session-scoped chat streams (chat_api.rs) share it.
pub(crate) trait IntoResponseWithBytesStream: Sized {
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
                // Task 5's merge: the forced-change destination is the
                // account page (the password card's home) — the login
                // gate's redirect targets the same URL.
                Redirect::to("/account?forced=1").into_response()
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

#[derive(Deserialize)]
pub struct SetupForm {
    pub username: String,
    pub password: String,
    pub password_confirm: Option<String>,
}

/// GET /setup — first-run setup page; unreachable once any user exists.
pub async fn setup_page(State(state): State<AppState>) -> Response {
    // Fail closed: an unreadable count is treated as "users exist".
    if state.users.count().await.unwrap_or(1) > 0 {
        return Redirect::to("/login").into_response();
    }
    let min_len = state.security_config().await.min_password_length;
    pages::setup_page("admin", min_len, None).into_response()
}

/// POST /setup — create the initial admin from operator-chosen
/// credentials. No throttle: the endpoint dies after the first account;
/// the password policy is the defense. The recovery key is shown once,
/// in the response body only (never a URL — history/referrer leakage).
pub async fn setup_submit(
    State(state): State<AppState>,
    axum::Form(form): axum::Form<SetupForm>,
) -> Response {
    // Count re-check + UNIQUE(username): one setup wins, losers redirect.
    if state.users.count().await.unwrap_or(1) > 0 {
        return Redirect::to("/login").into_response();
    }
    let username = form.username.trim().to_string();
    let min_len = state.security_config().await.min_password_length;
    if username.is_empty() {
        return pages::setup_page("admin", min_len, Some("Username must not be empty"))
            .into_response();
    }
    if let Some(confirm) = &form.password_confirm
        && confirm != &form.password
    {
        return pages::setup_page(&username, min_len, Some("Passwords do not match"))
            .into_response();
    }
    if let Err(e) = mycelium_auth::password::check_password_policy_min(&form.password, min_len) {
        return pages::setup_page(&username, min_len, Some(&e.to_string())).into_response();
    }
    let email = format!("{username}@localhost.local");
    match state
        .users
        .create_local(&username, &email, &form.password, Role::Admin)
        .await
    {
        Ok(created) => {
            if let Err(e) = state
                .persist_service_seal(created.record.id, &created.master_key)
                .await
            {
                tracing::error!(
                    user_id = %created.record.id,
                    error = %e,
                    "failed to persist service-key seal after setup — resealed on first login"
                );
            }
            state
                .master_keys
                .insert(created.record.id, created.master_key);
            tracing::info!(username = %username, "initial admin account created via /setup");
            pages::setup_created(&username, &created.recovery_key).into_response()
        }
        Err(mycelium_auth::UsersError::Duplicate) => Redirect::to("/login").into_response(),
        Err(e) => pages::setup_page(&username, min_len, Some(&e.to_string())).into_response(),
    }
}

/// GET /logout — delete the session and clear the cookie. Public: works
/// with or without a session (extracted via headers directly).
pub async fn logout(State(state): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    if let Some(session_id) = crate::middleware::session_id_from_headers(&headers) {
        let _ = state.sessions.delete(session_id).await;
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
        Err(_) => {
            // Re-render the editor with the edit preserved in the
            // textarea: the warning banner carries the parser's own
            // message (the preview builder re-parses with the same
            // `Concept::parse`, so its error is exactly what the save
            // was rejected with).
            let csrf = pages::current_csrf(&state, &session).await;
            let (preview, frontmatter_error) = pages::build_preview_html(&form.markdown);
            return pages::concept_editor(
                &user,
                &csrf,
                &canonical,
                &form.markdown,
                preview,
                frontmatter_error.as_deref(),
                form.scope.as_deref(),
                true,
                None,
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
            )
            .with_provenance(format!("web:{}", user.username));
            match cs.put(&concept).await {
                Ok(()) => Redirect::to(&format!(
                    "/concept?path={}&scope=skills",
                    pages::urlencoding_encode(&canonical)
                ))
                .into_response(),
                Err(e) => {
                    // A store failure re-renders the editor with the
                    // edit preserved; the message rides the warning
                    // banner (the page's single error slot).
                    let csrf = pages::current_csrf(&state, &session).await;
                    let (preview, _) = pages::build_preview_html(&form.markdown);
                    pages::concept_editor(
                        &user,
                        &csrf,
                        &canonical,
                        &form.markdown,
                        preview,
                        Some(&e.to_string()),
                        Some("skills"),
                        true,
                        None,
                    )
                    .into_response()
                }
            }
        }
        // User-facing write routes refuse the library scope; the
        // librarian's ingest flow (including a replace re-ingest's
        // catalog delete) is the library's writer.
        Some("library") => error_response(StatusCode::FORBIDDEN, "library is read-only"),
        // Default: the user's private bundle.
        _ => {
            let master = match state.master_key_for(user.user_id).await {
                Ok(m) => m,
                Err(_) => return pages::not_found().into_response(),
            };
            let cs = ConceptStore::for_user(&state.store, user.user_id, master)
                .with_provenance(format!("web:{}", user.username));
            match cs.put(&concept).await {
                Ok(()) => Redirect::to(&format!(
                    "/concept?path={}",
                    pages::urlencoding_encode(&canonical)
                ))
                .into_response(),
                Err(e) => {
                    // A store failure re-renders the editor with the
                    // edit preserved; the message rides the warning
                    // banner (the page's single error slot).
                    let csrf = pages::current_csrf(&state, &session).await;
                    let (preview, _) = pages::build_preview_html(&form.markdown);
                    pages::concept_editor(
                        &user,
                        &csrf,
                        &canonical,
                        &form.markdown,
                        preview,
                        Some(&e.to_string()),
                        None,
                        true,
                        None,
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
    // The revisions surface (Task 4). All four are lenient
    // Option<String> rather than typed numbers ON PURPOSE: the
    // compare form legitimately submits blank values (`diff_from=`
    // is the "" HEAD sentinel option), which a `u64`/`u8` extractor
    // would 400-reject; the page answers by falling back to the plain
    // view instead. See `page_revision_param`.
    /// Any Some>0 → render the revisions block (list/confirm/diff).
    #[serde(default)]
    pub revs: Option<String>,
    /// Diff "from" side: blank/absent = HEAD (current content).
    #[serde(default)]
    pub diff_from: Option<String>,
    /// Diff "to" side: blank/absent = HEAD (current content).
    #[serde(default)]
    pub diff_to: Option<String>,
    /// Show the restore-confirm interstitial for this revision.
    #[serde(default)]
    pub restore_confirm: Option<String>,
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
        // New-concept editor: empty path + the scope's template, the
        // same concept_editor fn (no delete — nothing is saved yet).
        let (preview, _) = pages::build_preview_html(template);
        return pages::concept_editor(
            &user,
            &csrf,
            "",
            template,
            preview,
            None,
            query.scope.as_deref(),
            true,
            None,
        )
        .into_response();
    }
    let Some(path) = &query.path else {
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
            let rev_store =
                RevisionStore::for_service(state.store.pool(), &state.service_key, "skills");
            match cs.get(&canonical).await {
                Ok(concept) => {
                    let markdown = concept.to_markdown().unwrap_or_default();
                    let editable = user.role == Role::Admin;
                    let (preview, _) = pages::build_preview_html(&markdown);
                    let surface = revisions_surface(
                        &rev_store,
                        &canonical,
                        "skills",
                        editable,
                        Some(&markdown),
                        &query,
                    )
                    .await;
                    // The activate offer computes like the user branch's:
                    // the plan's per-concept rule ("Set status: active"
                    // when the status is paused) is unqualified by scope,
                    // and /concept/activate already accepts skills admins —
                    // the page merely offers what the endpoint does. (The
                    // dead branch stays false: a dead path has no
                    // lifecycle status — its flow is the restore
                    // interstitial. Library stays false: read-only.)
                    let activation = concept_is_paused(&canonical, &markdown);
                    let extras = pages::ConceptExtras {
                        revisions: surface,
                        activation,
                    };
                    pages::concept_editor(
                        &user,
                        &csrf,
                        &canonical,
                        &markdown,
                        preview,
                        None,
                        Some("skills"),
                        editable,
                        Some(&extras),
                    )
                    .into_response()
                }
                Err(mycelium_store::ConceptStoreError::NotFound(_)) => {
                    // Deleted but revision-tracked → the dead-path
                    // revisions page (restore links = re-create);
                    // untracked paths keep the plain 404.
                    match revisions_surface(
                        &rev_store,
                        &canonical,
                        "skills",
                        user.role == Role::Admin,
                        None,
                        &query,
                    )
                    .await
                    {
                        Some(surface) => {
                            let extras = pages::ConceptExtras {
                                revisions: Some(surface),
                                activation: false,
                            };
                            let (preview, _) = pages::build_preview_html("");
                            pages::concept_editor(
                                &user,
                                &csrf,
                                &canonical,
                                "",
                                preview,
                                None,
                                Some("skills"),
                                false,
                                Some(&extras),
                            )
                            .into_response()
                        }
                        None => pages::not_found().into_response(),
                    }
                }
                Err(_) => pages::not_found().into_response(),
            }
        }
        // Library catalogs: read-only, shelf-visibility gated.
        Some("library") => {
            // Visibility: the same gate the revisions routes use —
            // the concept's book slug must be on a global-read shelf
            // (or the reader is an admin).
            if !library_path_visible(&state, &user, &canonical).await {
                return error_response(StatusCode::FORBIDDEN, "bookshelf is not global-read");
            }
            let cs = ConceptStore::for_service(
                &state.store,
                (*state.service_key).clone(),
                &state.store.library_dir(),
                "library",
            );
            let rev_store =
                RevisionStore::for_service(state.store.pool(), &state.service_key, "library");
            match cs.get(&canonical).await {
                Ok(concept) => {
                    let markdown = concept.to_markdown().unwrap_or_default();
                    let (preview, _) = pages::build_preview_html(&markdown);
                    // The library is read-only: rows render (a history
                    // is still a read), restore never does.
                    let surface = revisions_surface(
                        &rev_store,
                        &canonical,
                        "library",
                        false,
                        Some(&markdown),
                        &query,
                    )
                    .await;
                    let extras = pages::ConceptExtras {
                        revisions: surface,
                        activation: false,
                    };
                    pages::concept_editor(
                        &user,
                        &csrf,
                        &canonical,
                        &markdown,
                        preview,
                        None,
                        Some("library"),
                        false,
                        Some(&extras),
                    )
                    .into_response()
                }
                Err(mycelium_store::ConceptStoreError::NotFound(_)) => {
                    // Dead library revision rows are ROUTINE: a replace
                    // re-ingest deletes the old chapters through
                    // `delete_book`, and every `ConceptStore::delete`
                    // records a write-ahead revision row — the
                    // /revisions browser lists them and links here. So
                    // the dead path mirrors the skills/user arms: build
                    // the dead surface (no live markdown — no HEAD side
                    // to diff or restore against) with the library's
                    // hard-false restore flag. The page is a history
                    // read, full stop: restore/confirm never offer
                    // (restore 403s for the scope), no editor grid,
                    // undelete not implied. Untracked paths and store
                    // faults keep the plain 404. The visibility gate ran
                    // above, so a global-read reader gets the same
                    // read-only posture the live branch gives it.
                    match revisions_surface(&rev_store, &canonical, "library", false, None, &query)
                        .await
                    {
                        Some(surface) => {
                            let extras = pages::ConceptExtras {
                                revisions: Some(surface),
                                activation: false,
                            };
                            let (preview, _) = pages::build_preview_html("");
                            pages::concept_editor(
                                &user,
                                &csrf,
                                &canonical,
                                "",
                                preview,
                                None,
                                Some("library"),
                                false,
                                Some(&extras),
                            )
                            .into_response()
                        }
                        None => pages::not_found().into_response(),
                    }
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
            // The revision store borrows the master key; build it
            // before ConceptStore::for_user moves it.
            let rev_store = RevisionStore::for_user(state.store.pool(), user.user_id, &master);
            let cs = ConceptStore::for_user(&state.store, user.user_id, master);
            match cs.get(&canonical).await {
                Ok(concept) => {
                    let markdown = concept.to_markdown().unwrap_or_default();
                    let (preview, _) = pages::build_preview_html(&markdown);
                    let surface = revisions_surface(
                        &rev_store,
                        &canonical,
                        "user",
                        true,
                        Some(&markdown),
                        &query,
                    )
                    .await;
                    let activation = concept_is_paused(&canonical, &markdown);
                    let extras = pages::ConceptExtras {
                        revisions: surface,
                        activation,
                    };
                    pages::concept_editor(
                        &user,
                        &csrf,
                        &canonical,
                        &markdown,
                        preview,
                        None,
                        None,
                        true,
                        Some(&extras),
                    )
                    .into_response()
                }
                Err(mycelium_store::ConceptStoreError::NotFound(_)) => {
                    // Deleted but revision-tracked → the dead-path
                    // revisions page (restore = re-create at the
                    // path). Never-tracked paths keep the legacy
                    // new-concept editor seeded with the default Note
                    // template (the legacy new_concept_page behavior).
                    match revisions_surface(&rev_store, &canonical, "user", true, None, &query)
                        .await
                    {
                        Some(surface) => {
                            let extras = pages::ConceptExtras {
                                revisions: Some(surface),
                                activation: false,
                            };
                            let (preview, _) = pages::build_preview_html("");
                            pages::concept_editor(
                                &user,
                                &csrf,
                                &canonical,
                                "",
                                preview,
                                None,
                                None,
                                false,
                                Some(&extras),
                            )
                            .into_response()
                        }
                        None => {
                            let template = "---\ntype: Note\ntitle: New concept\ndescription: \ntags: []\n---\n\n";
                            let (preview, _) = pages::build_preview_html(template);
                            pages::concept_editor(
                                &user, &csrf, "", template, preview, None, None, true, None,
                            )
                            .into_response()
                        }
                    }
                }
                Err(_) => pages::not_found().into_response(),
            }
        }
    }
}

// ---------- Concept-page revisions (Task 4: list, diff, confirm) ----------

/// Max lines of a rendered unified diff on the concept page (an
/// extremely long diff truncates with a note instead of ballooning the
/// page).
const DIFF_MAX_LINES: usize = 4000;

/// Lenient parse of the concept page's revision params: absent, blank,
/// or non-(1-based)-numeric all mean "not given". The compare form
/// submits blank values (its `""` HEAD sentinel option), so a typed
/// `u64` extractor would 400-reject a plain form submit; the page
/// answers by falling back to the plain view instead — the same
/// unknown-input rationale as [`revisions_revision_no`], softer
/// because this is a page, not the API.
fn page_revision_param(raw: Option<&str>) -> Option<u64> {
    let v = raw?;
    if v.is_empty() {
        return None;
    }
    v.parse::<u64>().ok().filter(|n| *n > 0)
}

/// The live concept "awaits revival": status retired or superseded
/// (the two pausing statuses — `active` is the unpause target). A
/// frontmatter that no longer parses is simply not paused.
fn concept_is_paused(source_path: &str, markdown: &str) -> bool {
    matches!(
        Concept::parse(source_path, markdown).ok().and_then(|c| {
            c.frontmatter
                .status
                .as_deref()
                .and_then(LifecycleStatus::parse)
        }),
        Some(LifecycleStatus::Superseded) | Some(LifecycleStatus::Retired)
    )
}

/// Build the concept page's revisions surface (Task 4) from a
/// revision-tracked path: the list of entries plus the mode —
/// restore-confirm (when a confirmable revision was asked for and the
/// scope may restore), diff (when at least one side names a revision),
/// or the plain list. `None` on an empty chain or a store fault — the
/// caller falls back to the legacy page (no revisions block on an
/// untracked path; a history-view fault must not block the editor).
///
/// `live_markdown` is the live concept's markdown when the concept
/// exists (the diff's HEAD side and the confirm page's "current"
/// preview); `None` marks a deleted path (no HEAD side to diff or
/// restore-against — dead chains diff only revision↔revision, and the
/// confirm page's copy says the restore re-creates the concept).
///
/// `revs` gates the block only on LIVE pages (no `revs` → `None`: the
/// plain editor, the everyday view). A deleted path always shows its
/// history instead of the editor — the editor would clobber a chain
/// the restore flow is meant to revive.
async fn revisions_surface(
    rev_store: &RevisionStore<'_>,
    canonical: &str,
    scope_param: &'static str,
    restore_ready: bool,
    live_markdown: Option<&str>,
    query: &ConceptQuery,
) -> Option<pages::RevisionsSurface> {
    if live_markdown.is_some() && page_revision_param(query.revs.as_deref()).is_none() {
        return None;
    }
    let entries = match rev_store.list(canonical).await {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(
                path = canonical,
                error = %e,
                "revisions list for the concept page failed"
            );
            return None;
        }
    };
    if entries.is_empty() {
        return None;
    }
    let dead = live_markdown.is_none();

    // Mode: confirm first (it is the page's answer to a per-row
    // restore click), then diff, then the plain list. An empty-payload
    // revision (the undelete-restore row) is NOT confirmable — its
    // restore post is the snapshot-parse 400, so a confirm URL for one
    // falls back to the list, like an unknown revision number.
    if restore_ready
        && let Some(n) = page_revision_param(query.restore_confirm.as_deref())
        && entries
            .iter()
            .any(|e| e.revision_no == n && !e.payload_empty)
    {
        // The row's labels first (the entries move into the surface
        // after the borrow ends).
        let (op, summary) = match entries.iter().find(|e| e.revision_no == n) {
            Some(row) => (row.op.clone(), row.summary.clone()),
            None => (String::new(), String::new()),
        };
        match rev_store.get(canonical, n).await {
            Ok(bytes) => {
                return Some(pages::RevisionsSurface {
                    path: canonical.to_string(),
                    scope_param,
                    restore_ready,
                    dead,
                    entries,
                    mode: pages::RevisionsMode::Confirm(pages::ConfirmData {
                        n,
                        op,
                        summary,
                        snapshot_text: String::from_utf8_lossy(&bytes).into_owned(),
                        current_text: live_markdown.map(str::to_string),
                    }),
                });
            }
            Err(e) => {
                tracing::warn!(
                    path = canonical,
                    n,
                    error = %e,
                    "restore-confirm snapshot read failed"
                );
            }
        }
    }

    let from = page_revision_param(query.diff_from.as_deref());
    let to = page_revision_param(query.diff_to.as_deref());
    if (from.is_some() || to.is_some())
        && let Some(mode) = build_diff_mode(rev_store, canonical, live_markdown, from, to).await
    {
        return Some(pages::RevisionsSurface {
            path: canonical.to_string(),
            scope_param,
            restore_ready,
            dead,
            entries,
            mode,
        });
    }

    Some(pages::RevisionsSurface {
        path: canonical.to_string(),
        scope_param,
        restore_ready,
        dead,
        entries,
        mode: pages::RevisionsMode::List,
    })
}

/// One diff side's text: HEAD (the live concept's markdown) or a
/// revision's snapshot. An unresolvable side (unknown revision, or the
/// HEAD side of a dead path) → None — the caller falls back to the
/// plain list rather than showing a broken compare.
async fn diff_side_text(
    rev_store: &RevisionStore<'_>,
    canonical: &str,
    live_markdown: Option<&str>,
    side: Option<u64>,
) -> Option<String> {
    match side {
        None => live_markdown.map(str::to_string),
        Some(n) => match rev_store.get(canonical, n).await {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) => {
                tracing::warn!(
                    path = canonical,
                    n = n,
                    error = %e,
                    "diff snapshot read failed"
                );
                None
            }
        },
    }
}

/// Compute the unified diff of the two resolved sides (`similar`'s
/// `TextDiff` in unified form, context radius 3). The page renders the
/// lines ESCAPED in a `<pre>` — here they are data, not markup. Over
/// [`DIFF_MAX_LINES`] the line list truncates (the page says so).
async fn build_diff_mode(
    rev_store: &RevisionStore<'_>,
    canonical: &str,
    live_markdown: Option<&str>,
    from: Option<u64>,
    to: Option<u64>,
) -> Option<pages::RevisionsMode> {
    let from_text = diff_side_text(rev_store, canonical, live_markdown, from).await?;
    let to_text = diff_side_text(rev_store, canonical, live_markdown, to).await?;
    let diff = TextDiff::from_lines(&from_text, &to_text);
    let label = |side: Option<u64>, head: &str| match side {
        Some(n) => format!("revision #{n}"),
        None => head.to_string(),
    };
    let from_label = label(from, "current");
    let to_label = label(to, "current");
    let rendered = diff
        .unified_diff()
        .context_radius(3)
        .header(&from_label, &to_label)
        .to_string();
    let all_lines: Vec<&str> = rendered.lines().collect();
    let truncated = all_lines.len() > DIFF_MAX_LINES;
    let lines: Vec<String> = all_lines
        .into_iter()
        .take(DIFF_MAX_LINES)
        .map(ToOwned::to_owned)
        .collect();
    Some(pages::RevisionsMode::Diff(pages::DiffData {
        from_label,
        to_label,
        lines,
        truncated,
    }))
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
        // User-facing write routes refuse the library scope; the
        // librarian's ingest flow (including a replace re-ingest's
        // catalog delete) is the library's writer.
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

/// One selected-shelf chapter row for the Books page: the book it
/// belongs to, its 1-based chapter number (parsed from the catalog
/// concept path), the passage anchor (the concept path's stem — the
/// REAL anchor ingest wrote, anchored and plain books alike), and the
/// chapter's title. The handler gathers; the page composes (this PR's
/// architecture), so the row crosses the boundary as plain data.
pub struct ChapterRow {
    pub book_slug: String,
    pub index: u32,
    pub anchor: String,
    pub title: String,
}

// ---------- Concept activation (Task 4) ----------

/// POST /concept/activate — "Set status: active" on a paused concept
/// (lifecycle spec §4): clears `status` and `superseded_by` (the
/// concept's other lifecycle history — `valid_to` — is untouched) and
/// writes the result through the NORMAL stamped put, so provenance
/// applies and the activation itself records an update revision.
/// Session-authorized, editable scopes only (skills: admin; the
/// library never); a missing concept answers 404.
pub async fn concept_activate(
    State(state): State<AppState>,
    user: SessionUser,
    axum::Form(form): axum::Form<DeleteForm>,
) -> Response {
    let canonical = format!("/{}", form.path.trim_start_matches('/'));
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
            )
            .with_provenance(format!("web:{}", user.username));
            activate_and_redirect(cs, &canonical, "skills").await
        }
        // User-facing write routes refuse the library scope; the
        // librarian's ingest flow (including a replace re-ingest's
        // catalog delete) is the library's writer.
        Some("library") => error_response(StatusCode::FORBIDDEN, "library is read-only"),
        // Default: the user's private bundle.
        _ => {
            let master = match state.master_key_for(user.user_id).await {
                Ok(m) => m,
                Err(_) => return pages::not_found().into_response(),
            };
            let cs = ConceptStore::for_user(&state.store, user.user_id, master)
                .with_provenance(format!("web:{}", user.username));
            activate_and_redirect(cs, &canonical, "user").await
        }
    }
}

/// The activate tail both scopes share: read the concept (its page
/// offered this button only while paused — a direct call against any
/// live concept is a no-op clear), drop the pausing fields, and re-put
/// through the stamped store. Redirects to the concept viewer, the
/// same viewer redirect the restore form answers.
async fn activate_and_redirect(
    cs: ConceptStore<'_>,
    canonical: &str,
    scope_param: &'static str,
) -> Response {
    match cs.get(canonical).await {
        Ok(mut concept) => {
            concept.frontmatter.status = None;
            concept.frontmatter.superseded_by = None;
            match cs.put(&concept).await {
                Ok(()) => Redirect::to(&format!(
                    "/concept?path={}&scope={}",
                    pages::urlencoding_encode(canonical),
                    scope_param
                ))
                .into_response(),
                Err(e) => concept_activate_error(e),
            }
        }
        Err(mycelium_store::ConceptStoreError::NotFound(_)) => {
            error_response(StatusCode::NOT_FOUND, "not found")
        }
        Err(e) => concept_activate_error(e),
    }
}

/// Activate writes are normal puts: the lifecycle/parse rejects ride
/// the PUT-concept posture (400 with the parse text); everything else
/// is a server fault (logged, generic body).
fn concept_activate_error(e: mycelium_store::ConceptStoreError) -> Response {
    match &e {
        mycelium_store::ConceptStoreError::Parse(_) => {
            error_response(StatusCode::BAD_REQUEST, &e.to_string())
        }
        _ => {
            tracing::error!(error = %e, "concept activate failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// GET /books — browse bookshelves (mockup 07). Users see global-read
/// shelves; admins all. `?shelf=` selects a shelf (only a name the
/// page shows — a visible row; anything else is the unselected view,
/// the unknown-filter fallback), `?book=` narrows the chapter list to
/// one book on the selected shelf, and `?passage=` resolves a
/// `book://slug#anchor` resource server-side through the same gates
/// and extraction as GET /api/v1/passages (resolve_passage) — a
/// resolution failure renders as the page's warning banner, never a
/// 500 (RF 4).
pub async fn books_view(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
    Query(params): Query<std::collections::HashMap<String, String>>,
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
    // ?shelf= selects only a shelf the page will show (a visible row;
    // anything else is the unselected view — the unknown-filter
    // fallback); ?book= narrows to one book ON the selected shelf.
    let selected_shelf_books: Option<(&str, &[(String, String)])> =
        params.get("shelf").and_then(|wanted| {
            shelf_data
                .iter()
                .find(|(name, _, _)| name.as_str() == wanted.as_str())
                .map(|(name, _, books)| (name.as_str(), books.as_slice()))
        });
    let selected_shelf: Option<String> = selected_shelf_books.map(|(name, _)| name.to_string());
    let selected_book: Option<String> = selected_shelf_books.and_then(|(_, books)| {
        params
            .get("book")
            .and_then(|wanted| {
                books
                    .iter()
                    .find(|(slug, _)| slug.as_str() == wanted.as_str())
            })
            .map(|(slug, _)| slug.to_string())
    });
    // The selected shelf's chapter list: the library-namespace catalog
    // concepts under `/<slug>/` — `ch-<n>-<anchor>.md` children with a
    // 1-based index (the hub `book.md` and any non-chapter file stay
    // out; the anchor is the file stem — the REAL anchor ingest wrote,
    // the passage link carries it verbatim). Narrowed to the selected
    // book when `?book=` names one; ordered by book slug then chapter
    // index for deterministic output (list_prefix is path-ordered, so
    // ch-10 would sort before ch-2 without the index re-sort).
    let mut chapters: Vec<ChapterRow> = Vec::new();
    if let Some((_, books)) = selected_shelf_books {
        let cs = ConceptStore::for_service(
            &state.store,
            (*state.service_key).clone(),
            &state.store.library_dir(),
            "library",
        );
        for (slug, _title) in books {
            if selected_book.as_deref().is_some_and(|b| b != slug.as_str()) {
                continue;
            }
            let prefix = format!("/{slug}/");
            for entry in cs.list_prefix(&prefix).await.unwrap_or_default() {
                let Some(stem) = entry
                    .path
                    .strip_prefix(prefix.as_str())
                    .and_then(|file| file.strip_suffix(".md"))
                else {
                    continue;
                };
                let Some(rest) = stem.strip_prefix("ch-") else {
                    continue;
                };
                let Some((n, _tail)) = rest.split_once('-') else {
                    continue;
                };
                let Ok(index) = n.parse::<u32>() else {
                    continue;
                };
                if index == 0 {
                    continue; // anchors are 1-based; zero is invalid
                }
                chapters.push(ChapterRow {
                    book_slug: slug.clone(),
                    index,
                    anchor: stem.to_string(),
                    title: entry.title,
                });
            }
        }
        chapters
            .sort_by(|a, b| (a.book_slug.as_str(), a.index).cmp(&(b.book_slug.as_str(), b.index)));
    }
    // ?passage= — a `book://slug#anchor` resource resolved server-side
    // through the shared gates + extraction; the page renders the
    // passage or the resolution error as its warning banner (200 — RF 4).
    let (passage_html, passage_err) = match params.get("passage") {
        Some(resource) => match resolve_passage(&state, &user, resource).await {
            Ok(passage) => (Some(pages::build_passage_html(&passage.text)), None),
            Err((_status, message)) => (None, Some(message)),
        },
        None => (None, None),
    };
    pages::books_page(
        &user,
        &csrf,
        &shelf_data,
        selected_shelf.as_deref(),
        selected_book.as_deref(),
        &chapters,
        passage_html,
        passage_err.as_deref(),
    )
    .into_response()
}

/// GET / — Browse: the user's bundle listing (mockup 01), with an
/// optional `?type=` filter. A filter naming a type that has no
/// concepts is unknown → the unfiltered list with the "All" chip
/// active (Review Focus 2), never an error or an empty result.
pub async fn home(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        Err(_) => return pages::not_found().into_response(),
    };
    let cs = ConceptStore::for_user(&state.store, user.user_id, master);
    let entries = cs.list().await.unwrap_or_default();
    // Broken links, mirroring graph_data's walk: a concept whose body
    // links to a path that is not in the bundle is flagged on its row.
    let known: std::collections::HashSet<String> = entries.iter().map(|e| e.path.clone()).collect();
    let mut broken = std::collections::HashSet::new();
    for entry in &entries {
        if let Ok(concept) = cs.get(&entry.path).await {
            for target in mycelium_core::links::scan_links(&concept.body) {
                if !known.contains(&target) {
                    broken.insert(entry.path.clone());
                }
            }
        }
    }
    // The filter only applies when it names a type present in the
    // bundle; anything else renders the unfiltered list.
    let known_types: std::collections::HashSet<&str> =
        entries.iter().map(|e| e.concept_type.as_str()).collect();
    let type_filter = match params.get("type") {
        Some(t) if known_types.contains(t.as_str()) => Some(t.as_str()),
        _ => None,
    };
    pages::home_page(&user, &csrf, &entries, &broken, type_filter, None, None).into_response()
}

/// GET /search — search page, with an optional `?scope=` filter
/// (mockup 05's scope chips): a value naming a real scope
/// (`user|skills|library`) narrows the merged results to that scope;
/// an absent or unknown value renders the unfiltered list with "All"
/// active (Task 1's unknown-filter fallback — never an error).
pub async fn search_view(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let query = params.get("q").map(String::as_str).unwrap_or("");
    let mode = match parse_mode_param(&params.get("mode").cloned()) {
        Ok(mode) => mode,
        // Parsed BEFORE the empty-query early-return: a bad `mode` is
        // 400 even on a blank page — the caller's spelling, not a
        // search outcome.
        Err(status) => {
            return error_response(status, "invalid mode (auto | lexical | vector_only)");
        }
    };
    // The scope filter only applies when it names a real scope;
    // anything else renders the unfiltered list.
    let scope = match params.get("scope") {
        Some(s) if matches!(s.as_str(), "user" | "skills" | "library") => Some(s.as_str()),
        _ => None,
    };
    if query.is_empty() {
        return pages::search_page(&user, &csrf, "", scope, &[]).into_response();
    }
    let terms = query
        .split_whitespace()
        .map(|t| t.to_string())
        .collect::<Vec<_>>();
    let mut search = SearchQuery::new(terms);
    search.include_global = true; // web default: bundle + global shelves
    search.mode = mode;
    let mut results = search_all_scopes(&state, &user, &search, true).await;
    // Server-side scope filter: the page renders what the chips promise.
    if let Some(scope) = scope {
        results.retain(|r| r.scope == scope);
    }
    crate::state::Metrics::inc(&state.metrics.searches_total);
    pages::search_page(&user, &csrf, query, scope, &results).into_response()
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

/// One scope's skills-page listing (spec §"Web: skills page"):
/// grouped skills, flat rows, and manifest script labels parallel to
/// the groups. Grouping runs over the FULL entry list so non-Skill
/// companions stay inside their group; groups whose hub is not
/// `type: Skill` are dropped, and flat rows keep the legacy
/// `type: Skill` filter. Script labels are the hub manifest's
/// non-`.md` paths, sorted — display-only rows (payload files are
/// not concepts).
async fn skill_sections(
    cs: &ConceptStore<'_>,
) -> (
    Vec<SkillGroup>,
    Vec<mycelium_store::ConceptEntry>,
    Vec<Vec<String>>,
) {
    let all = cs.list().await.unwrap_or_default();
    let listings = group_skills(&all);
    let mut groups = Vec::new();
    let mut scripts = Vec::new();
    for group in listings.grouped {
        if group.hub.concept_type != "Skill" {
            continue;
        }
        let labels = cs
            .get(&group.hub.path)
            .await
            .ok()
            .and_then(|c| c.frontmatter.skill)
            .map(|manifest| {
                let mut paths: Vec<String> = manifest
                    .files
                    .iter()
                    .filter(|f| !f.path.ends_with(".md"))
                    .map(|f| f.path.clone())
                    .collect();
                paths.sort();
                paths
            })
            .unwrap_or_default();
        groups.push(group);
        scripts.push(labels);
    }
    let flat: Vec<mycelium_store::ConceptEntry> = listings
        .flat
        .into_iter()
        .filter(|e| e.concept_type == "Skill")
        .collect();
    (groups, flat, scripts)
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
    let (private_groups, private_flat, private_scripts) = skill_sections(&cs).await;
    let global_cs = ConceptStore::for_service(
        &state.store,
        (*state.service_key).clone(),
        &state.store.skills_dir(),
        "skills",
    );
    let (global_groups, global_flat, global_scripts) = skill_sections(&global_cs).await;
    pages::skills_page(
        &user,
        &csrf,
        &private_groups,
        &private_flat,
        &private_scripts,
        &global_groups,
        &global_flat,
        &global_scripts,
    )
    .into_response()
}

#[derive(Deserialize)]
pub struct PasswordForm {
    pub old: String,
    pub new: String,
    pub repeat: String,
}

/// POST /password — change password (invalidates sessions, forces
/// re-login). The route and the success arm are UNCHANGED by Task 5's
/// merge — the /account password card posts here — but the error
/// re-renders land on the account page's composition now (the old
/// bare /password page is gone; `account_password_error` below).
pub async fn password_submit(
    State(state): State<AppState>,
    session: SessionId,
    axum::Form(form): axum::Form<PasswordForm>,
) -> Response {
    if form.new != form.repeat {
        return account_password_error(&state, &session, "Passwords do not match").await;
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
        Err(e) => account_password_error(&state, &session, &e.to_string()).await,
    }
}

/// The password form's error re-render: the full /account composition
/// with the failure riding the page's banner slot (a danger banner
/// above the password card) — Task 5's merge rule: the error lands on
/// the account page's layout, not a bare /password page. The banner
/// primitive's text position escapes the message (PasswordError's
/// Display strings — no user text rides it).
///
/// CSRF (Task 7): the re-render threads the session's LIVE token via
/// `pages::current_csrf` — the concept_submit pattern. An empty token
/// would strand the user: the retry they just clicked would 403 at the
/// CSRF gate, so every form on the error page (the password card and
/// the head meta app.js reads) must carry a token that verifies.
async fn account_password_error(state: &AppState, session: &SessionId, message: &str) -> Response {
    let Ok(sess) = state.sessions.get(session.0).await else {
        return Redirect::to("/login").into_response();
    };
    let Ok(record) = state.users.get_by_id(sess.user_id).await else {
        return Redirect::to("/login").into_response();
    };
    let enrolled = record.totp_secret.is_some();
    let user = SessionUser {
        user_id: record.id,
        username: record.username,
        role: record.role,
    };
    let (count, credentials) = webauthn_rows(state, record.id).await;
    let csrf = pages::current_csrf(state, session).await;
    pages::account_page(
        &user,
        &csrf,
        pages::TotpSection::Row { enrolled },
        pages::RecoverySection::Resting,
        count,
        &credentials,
        Some(("danger", message)),
    )
    .into_response()
}

/// GET /password — a TRUE 307 to /account (Task 5's merge: the password
/// card's home is the account page, so bookmarked /password URLs keep
/// working — the /admin split's precedent; axum's `Redirect::to` is a
/// 303, so the redirect is built from the status + Location header
/// directly). The login gate still runs first: an anonymous GET
/// redirects to /login exactly as before, and a must-change user lands
/// on /account?forced=1 (the banner) via the gate's allowed set.
pub async fn password_view() -> Response {
    (StatusCode::TEMPORARY_REDIRECT, [("Location", "/account")]).into_response()
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

// ---------- Account security (PR 4, Task 3: /account + TOTP) ----------

#[derive(Deserialize)]
pub struct TotpVerifyForm {
    pub code: String,
}

/// The /account render's WebAuthn inputs (Task 4): the credential
/// count plus the per-credential rows, both from the HOISTED manager
/// (built once in AppState::new — Task-3 review Minor 4; the RP
/// config lives there, see state's `webauthn_origin_from_env`).
async fn webauthn_rows(
    state: &AppState,
    user_id: Uuid,
) -> (i64, Vec<mycelium_auth::webauthn::CredentialRow>) {
    let count = state.webauthn.credential_count(user_id).await.unwrap_or(0);
    // Skip the list query on the common no-credentials path.
    let credentials = if count > 0 {
        state
            .webauthn
            .list_credentials(user_id)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    (count, credentials)
}

/// GET /account — the Account security page (mockup 10). Session-
/// gated for every logged-in user (the nav item is not admin-gated,
/// spec §6); the resting TOTP state reads `UserRecord.totp_secret`
/// (`get_by_id` already carries the column — no new query shape).
/// `?forced=1` — the login redirect's and the login gate's target for
/// must-change users (Task 5's merge) — renders the warning banner
/// above the password card.
pub async fn account_view(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let enrolled = state
        .users
        .get_by_id(user.user_id)
        .await
        .map(|r| r.totp_secret.is_some())
        .unwrap_or(false);
    let (count, credentials) = webauthn_rows(&state, user.user_id).await;
    // The forced-change marker: pinned to the exact "1" the redirects
    // send — any other value renders the plain page.
    let banner = if params.get("forced").is_some_and(|f| f == "1") {
        Some(("warning", "You must change your password before continuing"))
    } else {
        None
    };
    pages::account_page(
        &user,
        &csrf,
        pages::TotpSection::Row { enrolled },
        pages::RecoverySection::Resting,
        count,
        &credentials,
        banner,
    )
    .into_response()
}

/// POST /account/totp/enroll — generate a secret, persist it, and
/// render the enrollment card (secret + otpauth URL + verify form).
///
/// TOTP semantics (pinned by reading mycelium-auth's totp.rs in
/// full): `set_secret`'s third parameter is the at-rest encoding
/// choice ONLY — `Some(&ServiceKey)` seals the secret as an `enc1:`
/// envelope (the same key AppState::new attaches to the login service,
/// so login decrypts transparently), `None` stores legacy plaintext —
/// and the store has NO pending/confirmed state: the column set_secret
/// writes is the exact column login's verify reads, so the second
/// factor goes LIVE at this POST (login requires a code from here on —
/// mycelium-auth's `totp_enrolled_requires_code` pins it). The
/// brief's alternative design (persist pending, flip to confirmed on
/// verify) is therefore NOT what the store supports; this handler
/// matches reality: enroll makes it live, and the verify POST is UX
/// confirmation (the enrollment card says "already active" so the
/// user is never misled).
///
/// Accepted consequence, documented: a POST that persists but whose
/// response never renders leaves the user enrolled with a secret they
/// never saw (password-only login then fails). The way back is the
/// in-session Disable → Enroll again, or operator DB surgery on the
/// totp_secret column — NOT the recovery key: a recovery key cannot
/// log a user in (no recovery-login path exists; login.rs takes
/// username + password + TOTP only, and `unseal_with_recovery` has no
/// production caller). Adding a pending-enrollment column would be an
/// auth-logic change — spec §1's non-goal for these UI wrappers.
/// Between the two imperfect options the fail-closed one is chosen.
pub async fn account_totp_enroll(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let secret = mycelium_auth::totp::generate_secret();
    let url = match mycelium_auth::totp::enrollment_url(&secret, &user.username, "Mycelium2") {
        Ok(u) => u,
        Err(e) => {
            // Nothing persisted yet — fail honestly without state.
            tracing::error!(user_id = %user.user_id, error = %e, "totp enrollment_url failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };
    let pool = state.store.pool();
    if let Err(e) =
        mycelium_auth::totp::set_secret(pool, user.user_id, &secret, Some(&*state.service_key))
            .await
    {
        tracing::error!(user_id = %user.user_id, error = %e, "totp set_secret failed");
        return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
    }
    let (count, credentials) = webauthn_rows(&state, user.user_id).await;
    pages::account_page(
        &user,
        &csrf,
        pages::TotpSection::Enrolling { secret, url },
        pages::RecoverySection::Resting,
        count,
        &credentials,
        None,
    )
    .into_response()
}

/// POST /account/totp/verify — confirm the enrollment with a code.
/// A correct code → the resting enrolled row + the success banner; a
/// wrong or stale code → the spec §7 "code expired/invalid, try
/// again" banner + the verify form again (the live enrollment is
/// untouched — see `account_totp_enroll` for why "pending" cannot
/// exist). A verify with nothing enrolled renders the plain
/// not-enrolled page: `totp::verify` passes unenrolled users (login
/// semantics — the factor is optional per user), which here would
/// otherwise paint a false "verified" state.
pub async fn account_totp_verify(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
    axum::Form(form): axum::Form<TotpVerifyForm>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let (count, credentials) = webauthn_rows(&state, user.user_id).await;
    let enrolled = state
        .users
        .get_by_id(user.user_id)
        .await
        .map(|r| r.totp_secret.is_some())
        .unwrap_or(false);
    if !enrolled {
        return pages::account_page(
            &user,
            &csrf,
            pages::TotpSection::Row { enrolled: false },
            pages::RecoverySection::Resting,
            count,
            &credentials,
            None,
        )
        .into_response();
    }
    let pool = state.store.pool();
    match mycelium_auth::totp::verify(pool, user.user_id, &form.code, Some(&*state.service_key))
        .await
    {
        Ok(true) => pages::account_page(
            &user,
            &csrf,
            pages::TotpSection::Row { enrolled: true },
            pages::RecoverySection::Resting,
            count,
            &credentials,
            Some(("success", "Authenticator app verified")),
        )
        .into_response(),
        Ok(false) => pages::account_page(
            &user,
            &csrf,
            pages::TotpSection::VerifyFailed,
            pages::RecoverySection::Resting,
            count,
            &credentials,
            None,
        )
        .into_response(),
        Err(e) => {
            // The enrollment's live state is unaffected — re-read it so
            // the row never lies, and surface the failure.
            tracing::error!(user_id = %user.user_id, error = %e, "totp verify failed");
            let enrolled = state
                .users
                .get_by_id(user.user_id)
                .await
                .map(|r| r.totp_secret.is_some())
                .unwrap_or(false);
            pages::account_page(
                &user,
                &csrf,
                pages::TotpSection::Row { enrolled },
                pages::RecoverySection::Resting,
                count,
                &credentials,
                Some(("danger", "TOTP verification failed — try again")),
            )
            .into_response()
        }
    }
}

/// POST /account/totp/disable — clear the stored secret (unenroll) and
/// render the not-enrolled row.
pub async fn account_totp_disable(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let (count, credentials) = webauthn_rows(&state, user.user_id).await;
    let pool = state.store.pool();
    match mycelium_auth::totp::clear_secret(pool, user.user_id).await {
        Ok(()) => pages::account_page(
            &user,
            &csrf,
            pages::TotpSection::Row { enrolled: false },
            pages::RecoverySection::Resting,
            count,
            &credentials,
            Some(("success", "Authenticator app disabled")),
        )
        .into_response(),
        Err(e) => {
            tracing::error!(user_id = %user.user_id, error = %e, "totp clear_secret failed");
            let enrolled = state
                .users
                .get_by_id(user.user_id)
                .await
                .map(|r| r.totp_secret.is_some())
                .unwrap_or(false);
            pages::account_page(
                &user,
                &csrf,
                pages::TotpSection::Row { enrolled },
                pages::RecoverySection::Resting,
                count,
                &credentials,
                Some(("danger", "could not disable the authenticator")),
            )
            .into_response()
        }
    }
}

// ---------- Account security (PR 4, Task 4: recovery + WebAuthn) ----

#[derive(Deserialize)]
pub struct RecoveryRegenerateForm {
    pub current_password: String,
}

/// POST /account/recovery/regenerate — rotate the recovery key.
///
/// Flow (binding order): fetch the record → parse the sealed JSON →
/// `unseal_with_password` — THE password check, running BEFORE any key
/// generation or persistence, so a wrong password leaves the stored
/// row byte-identical (no rotation, no persist, only the banner) →
/// `generate_recovery_key` → `rotate_recovery_key` (re-wraps the
/// master key under the new recovery key; the password seal and all
/// file ciphertext are untouched) → `UserStore::update_sealed`
/// (compare-and-swap persist: the PRE-READ record's JSON is the
/// UPDATE's condition, so a concurrent rotation that already won the
/// row makes this write match zero rows — the response then renders
/// a warning banner, never a shown-once card for an already-dead key)
/// → `persist_service_seal` (the same master key re-sealed under the
/// service key) → the shown-once card (the keys page's treatment —
/// the key rides the response body, never a URL).
///
/// Master-key and cache invariants, documented for the reviewer: the
/// rotation changes SEALS ONLY — the master key never changes — so
/// `MasterKeyCache` needs NO invalidation (the cached key still
/// decrypts everything, and a fresh unseal yields the same bytes).
/// The service-key seal re-persist is defensive idempotence: the
/// restart-recovery row already holds the same master key; this write
/// just keeps `updated_at` and the row in step. Sessions are
/// deliberately untouched (unlike password change, which invalidates
/// them): a recovery key alone can never log anyone in (no
/// recovery-login path exists), so rotation is not a
/// credential-compromise event.
pub async fn account_recovery_regenerate(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
    axum::Form(form): axum::Form<RecoveryRegenerateForm>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let (count, credentials) = webauthn_rows(&state, user.user_id).await;
    let record = match state.users.get_by_id(user.user_id).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(user_id = %user.user_id, error = %e, "recovery: user fetch failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };
    let sealed: mycelium_crypto::SealedMasterKey =
        match serde_json::from_str(&record.sealed_master_key) {
            Ok(s) => s,
            Err(e) => {
                // An unparseable record cannot be rotated — fail
                // loudly, touch nothing.
                tracing::error!(
                    user_id = %user.user_id,
                    error = %e,
                    "recovery: sealed record unparseable — refusing to rotate"
                );
                return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
            }
        };
    // THE password check — before any generation or persistence.
    let master = match mycelium_crypto::unseal_with_password(&sealed, &form.current_password) {
        Ok(m) => m,
        Err(_) => {
            // Banner, no rotation, no persist: the row stays as it was.
            return pages::account_page(
                &user,
                &csrf,
                pages::TotpSection::Row {
                    enrolled: record.totp_secret.is_some(),
                },
                pages::RecoverySection::WrongPassword,
                count,
                &credentials,
                Some(("danger", "Current password is incorrect")),
            )
            .into_response();
        }
    };
    let new_key = mycelium_crypto::generate_recovery_key();
    let rotated =
        match mycelium_crypto::rotate_recovery_key(&sealed, &form.current_password, &new_key) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(user_id = %user.user_id, error = %e, "recovery rotation failed");
                return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
            }
        };
    match state
        .users
        .update_sealed(user.user_id, &record.sealed_master_key, &rotated)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            // CAS lost: a concurrent rotation won the row (or the user
            // vanished). This request's key never became current —
            // showing it would render an already-dead "current" key, so
            // re-read the state and render the resting page with a
            // retry banner. Nothing was written by this request.
            tracing::warn!(
                user_id = %user.user_id,
                "recovery rotation lost a concurrent race — no key shown"
            );
            let enrolled = state
                .users
                .get_by_id(user.user_id)
                .await
                .map(|r| r.totp_secret.is_some())
                .unwrap_or(false);
            return pages::account_page(
                &user,
                &csrf,
                pages::TotpSection::Row { enrolled },
                pages::RecoverySection::Resting,
                count,
                &credentials,
                Some((
                    "warning",
                    "recovery key changed concurrently — retry to see the current key",
                )),
            )
            .into_response();
        }
        Err(e) => {
            tracing::error!(user_id = %user.user_id, error = %e, "update_sealed failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    }
    // See the doc comment: idempotent defensive re-persist; the master
    // key is unchanged, so no cache invalidation is needed anywhere.
    if let Err(e) = state.persist_service_seal(user.user_id, &master).await {
        tracing::error!(
            user_id = %user.user_id,
            error = %e,
            "service seal re-persist failed after rotation — existing seal still valid"
        );
    }
    pages::account_page(
        &user,
        &csrf,
        pages::TotpSection::Row {
            enrolled: record.totp_secret.is_some(),
        },
        pages::RecoverySection::Rotated { key: new_key },
        count,
        &credentials,
        None,
    )
    .into_response()
}

/// GET /account/webauthn/challenge — start passkey enrollment:
/// `start_enrollment`'s creation options serialized as JSON for
/// webauthn.js (`{"publicKey": {...}}` — the wire shape
/// webauthn-rs-proto defines: camelCase keys, base64url-no-pad
/// strings). The ceremony state is stored server-side (single-use,
/// 5-minute expiry); the browser only ever sees the options.
pub async fn account_webauthn_challenge(
    State(state): State<AppState>,
    user: SessionUser,
) -> Response {
    match state
        .webauthn
        .start_enrollment(user.user_id, &user.username, &user.username)
        .await
    {
        Ok(challenge) => Json(challenge).into_response(),
        Err(e) => {
            tracing::error!(
                user_id = %user.user_id,
                error = %e,
                "webauthn start_enrollment failed"
            );
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// POST /account/webauthn/register — complete the ceremony: webauthn.js
/// POSTs the browser's credential (JSON, x-csrf-token header);
/// `finish_enrollment` verifies it against the stored single-use
/// challenge state and persists the passkey. JSON response — the
/// client reloads on success so the row re-renders with the new
/// count. `RegisterPublicKeyCredential` is re-exported by
/// mycelium-auth so this crate needs no direct webauthn dependency.
/// The body extractor is `Result<Json<_>, JsonRejection>`: a
/// malformed body renders the endpoint-wide JSON error
/// (`400 {"error":"invalid body"}`) instead of axum's default
/// plain-text rejection echoing the serde error string (an
/// internal-detail leak — the same class the Phase-11 review closed).
pub async fn account_webauthn_register(
    State(state): State<AppState>,
    user: SessionUser,
    body: Result<
        axum::Json<mycelium_auth::webauthn::RegisterPublicKeyCredential>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Response {
    let axum::Json(cred) = match body {
        Ok(json) => json,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid body"),
    };
    match state.webauthn.finish_enrollment(user.user_id, &cred).await {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(mycelium_auth::webauthn::WebAuthnError::ChallengeNotFound) => {
            // No live challenge (never started, already consumed, or
            // expired): fail closed, tell the client to restart.
            error_response(
                StatusCode::BAD_REQUEST,
                "no active enrollment challenge — restart registration",
            )
        }
        Err(mycelium_auth::webauthn::WebAuthnError::Db(e)) => {
            tracing::error!(
                user_id = %user.user_id,
                error = %e,
                "webauthn finish_enrollment db failure"
            );
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
        Err(e) => {
            // Verification failure: client-input problem (bad
            // attestation, origin mismatch, duplicate credential).
            tracing::warn!(user_id = %user.user_id, error = %e, "webauthn registration rejected");
            error_response(StatusCode::BAD_REQUEST, "registration failed")
        }
    }
}

#[derive(Deserialize)]
pub struct WebauthnRemoveForm {
    pub credential_id: String,
}

/// POST /account/webauthn/remove — unenroll one passkey (the
/// per-credential Remove form, native POST + hidden csrf_token).
/// `remove_credential` scopes the delete to the logged-in user's id,
/// so a foreign or unknown credential id is a no-op. Renders the page
/// back with the fresh rows.
pub async fn account_webauthn_remove(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
    axum::Form(form): axum::Form<WebauthnRemoveForm>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let removed = state
        .webauthn
        .remove_credential(user.user_id, &form.credential_id)
        .await;
    // NotFound (unknown/foreign id) is the no-op path — not an error.
    // Anything else is a real failure: log it.
    if let Err(e) = &removed
        && !matches!(e, mycelium_auth::webauthn::WebAuthnError::NotFound)
    {
        tracing::error!(user_id = %user.user_id, error = %e, "webauthn remove_credential failed");
    }
    let (count, credentials) = webauthn_rows(&state, user.user_id).await;
    let enrolled = state
        .users
        .get_by_id(user.user_id)
        .await
        .map(|r| r.totp_secret.is_some())
        .unwrap_or(false);
    pages::account_page(
        &user,
        &csrf,
        pages::TotpSection::Row { enrolled },
        pages::RecoverySection::Resting,
        count,
        &credentials,
        if removed.is_ok() {
            Some(("success", "Passkey removed"))
        } else {
            None
        },
    )
    .into_response()
}

// ---------- Admin handlers ----------

#[derive(Deserialize)]
pub struct CreateUserForm {
    pub username: String,
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub password_confirm: Option<String>,
    pub role: String,
}

/// POST /admin/users — create a user (admin only). Every failure
/// bounces back to the users page as a `?error=` banner (the
/// handler's flash mechanism — only the host page changed); success
/// renders the users page directly with the shown-once recovery-key
/// card (the key rides the response body, never a URL — history and
/// referrer leakage).
///
/// Session-only: user creation is a browser-flow admin action; bearer
/// API keys (MCP/REST data credentials) are deliberately not accepted
/// as provisioning credentials. A bearer request passes the CSRF
/// middleware's bearer exemption and RequireAdmin (both see the
/// bearer-injected SessionUser), but the `SessionId` extractor —
/// required because the success path renders the page with the
/// creating session's real CSRF token — 401s session-less requests
/// before the handler runs.
pub async fn admin_create_user(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    session: SessionId,
    axum::Form(form): axum::Form<CreateUserForm>,
) -> Response {
    let bounce = |msg: String| {
        Redirect::to(&format!(
            "/admin/users?error={}",
            pages::urlencoding_encode(&msg)
        ))
        .into_response()
    };
    // Confirm box: a mismatch is a client-side typo, not a policy
    // failure — bounce back with a clear message.
    if let Some(confirm) = &form.password_confirm
        && confirm != &form.password
    {
        return bounce("passwords do not match".to_string());
    }
    // Enforce the ADMIN-CONFIGURED minimum (not the hardcoded default)
    // — the form's minlength mirrors this value.
    let min_len = state.security_config().await.min_password_length;
    if let Err(e) = mycelium_auth::password::check_password_policy_min(&form.password, min_len) {
        return bounce(e.to_string());
    }
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
            // Render the users page with the creating admin's identity,
            // the fresh table, and the recovery-key shown-once card.
            let csrf = pages::current_csrf(&state, &session).await;
            let admin = request_admin_user(&state, &session).await;
            let users = state.users.list_all_detailed().await.unwrap_or_default();
            pages::admin_users_page(
                &admin,
                &csrf,
                &users,
                min_len,
                Some((form.username.as_str(), created.recovery_key.as_str())),
                None,
                None,
            )
            .into_response()
        }
        Err(e) => bounce(e.to_string()),
    }
}

/// GET /admin — a TRUE 307 to the users page (spec §6's route map;
/// axum's `Redirect::to` is a 303, so the redirect is built from the
/// status + Location header directly). RequireAdmin stays on the
/// route: the extractor runs before the handler, so a non-admin GET
/// keeps the gate's exact 403 (the behavior every admin route pins).
pub async fn admin_view(_admin: mycelium_auth::rbac::RequireAdmin) -> Response {
    (
        StatusCode::TEMPORARY_REDIRECT,
        [("Location", "/admin/users")],
    )
        .into_response()
}

/// One row of the System page's per-item waiting table (outage-
/// recovery spec §3.5): METADATA ONLY — username, kind, what, state,
/// waited time, bounded error text. Never transcript or concept
/// content; the page HTML-escapes every cell.
#[derive(Debug)]
pub struct WaitingItemRow {
    /// The item class rendered in the Kind column: "chat turn",
    /// "queued write", or "ingest job".
    pub kind: &'static str,
    pub user: String,
    /// A short human handle: session id (chat), tool (queue), slug
    /// (ingest) — never payload.
    pub what: String,
    pub state: String,
    pub waited_seconds: i64,
    pub last_error: Option<String>,
}

/// The /health fields the System page surfaces as StatTiles — the
/// page's rendering DTO, mapped from health::system_stats (the ONE
/// shared computation). Renamed SystemPageStats in PR 4 (the Task-1
/// review's ruling) so only the health-side struct carries the
/// SystemStats name.
///
/// The four `llm_*` gate values carry the LLM-concurrency spec §3.6
/// readout card. They ride through as the `i64`s `SystemStats` already
/// holds (no second conversion) and all four come from that struct's
/// ONE `LLM_GATE.stats()` read — neither this DTO nor the page reads
/// the gate again, so the card cannot disagree with the tiles beside
/// it.
///
/// The `backend_*` fields (outage-recovery spec §3.5) ride through
/// from `SystemStats` the same way — ONE `LLM_HEALTH.snapshot()` read,
/// so the indicator cannot disagree with /health.
#[derive(Debug)]
pub struct SystemPageStats {
    pub users: i64,
    pub queue_depth: usize,
    pub oldest_pending_age_seconds: Option<i64>,
    pub queue_dead_count: usize,
    pub llm_last_success_seconds: Option<i64>,
    pub llm_active: i64,
    pub llm_queued: i64,
    pub llm_capacity: i64,
    pub llm_timeouts_total: i64,
    pub backend_state: &'static str,
    pub backend_down_seconds: Option<i64>,
    pub backend_checked_seconds_ago: Option<i64>,
    pub chat_waiting_count: i64,
    /// The per-item waiting table's rows (chat turns + queued writes +
    /// pending ingest jobs, oldest first). The summary line's per-kind
    /// counts derive from these — no separate counters.
    pub waiting_items: Vec<WaitingItemRow>,
}

/// GET /admin/users — the users table + create-user side card
/// (mockup 03); hosts POST /admin/users.
pub async fn admin_users_view(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    session: SessionId,
    Query(params): Query<AdminParams>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let users = state.users.list_all_detailed().await.unwrap_or_default();
    let min_len = state.security_config().await.min_password_length;
    let admin = request_admin_user(&state, &session).await;
    pages::admin_users_page(
        &admin,
        &csrf,
        &users,
        min_len,
        None,
        params.ok.as_deref(),
        params.error.as_deref(),
    )
    .into_response()
}

/// GET /admin/sign-in — the security settings + OIDC cards
/// (mockup 11); hosts POST /admin/security + POST /admin/oidc.
pub async fn admin_sign_in_view(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    session: SessionId,
    Query(params): Query<AdminParams>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let security = state.security_config().await;
    let oidc = crate::state::decrypt_config::<mycelium_auth::oidc::OidcConfig>(
        &state.config,
        &state.service_key,
        "oidc",
    )
    .await
    .is_some();
    let admin = request_admin_user(&state, &session).await;
    pages::admin_sign_in_page(
        &admin,
        &csrf,
        &security,
        oidc,
        params.ok.as_deref(),
        params.error.as_deref(),
    )
    .into_response()
}

/// GET /admin/llm — the LLM backend card on its own page; hosts
/// POST /admin/llm.
pub async fn admin_llm_view(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    session: SessionId,
    Query(params): Query<AdminParams>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    // Clamped for rendering: a stale (or hand-edited) out-of-range row
    // must not put an unenforceable value back in the box.
    let llm =
        crate::state::decrypt_config::<crate::LlmConfig>(&state.config, &state.service_key, "llm")
            .await
            .unwrap_or_default()
            .clamp();
    let admin = request_admin_user(&state, &session).await;
    pages::admin_llm_page(
        &admin,
        &csrf,
        &llm,
        params.ok.as_deref(),
        params.error.as_deref(),
    )
    .into_response()
}

/// GET /admin/library — the bookshelves/upload/limits/global-skills
/// stack (mockup 12); hosts POST /admin/bookshelves +
/// POST /admin/upload-limits, and carries the multipart upload form.
pub async fn admin_library_view(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    session: SessionId,
    Query(params): Query<AdminParams>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    // Shelves with the books count — the PR-2 books page's derived
    // column pattern (books_on_shelf per shelf).
    let shelves = state
        .store
        .list_bookshelves_detailed()
        .await
        .unwrap_or_default();
    let mut shelf_rows: Vec<(String, bool, usize)> = Vec::new();
    for (id, name, is_global) in shelves {
        let count = state
            .store
            .books_on_shelf(id)
            .await
            .unwrap_or_default()
            .len();
        shelf_rows.push((name, is_global, count));
    }
    let upload = state.upload_config().await;
    // Read-only global-skills listing: hub + flat titles, sorted for
    // deterministic output (management lives on the Skills page).
    let global_cs = ConceptStore::for_service(
        &state.store,
        (*state.service_key).clone(),
        &state.store.skills_dir(),
        "skills",
    );
    let (groups, flat, _scripts) = skill_sections(&global_cs).await;
    let mut titles: Vec<String> = groups.iter().map(|g| g.hub.title.clone()).collect();
    titles.extend(flat.iter().map(|e| e.title.clone()));
    titles.sort();
    let admin = request_admin_user(&state, &session).await;
    pages::admin_library_page(
        &admin,
        &csrf,
        &shelf_rows,
        upload.max_book_mib,
        &titles,
        params.ok.as_deref(),
        params.error.as_deref(),
    )
    .into_response()
}

/// GET /admin/system — the operational view: /health's fields as
/// StatTiles and the backup download. The ingest-jobs table moved to
/// /status (spec §8's PR-4 ruling — /status is the jobs-table home;
/// the PR-3 placement here was a stopgap pending that page).
pub async fn admin_system_view(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    session: SessionId,
    Query(params): Query<AdminParams>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    // The /health values — ONE computation shared with the health
    // route and /status (the PR-4 SystemStats extraction); mapped into
    // the page's StatTile DTO, which stays a rendering concern.
    let s = crate::health::system_stats(&state).await;
    // The waiting table's rows (outage-recovery spec §3.5): chat turns
    // owed an answer, queued writes not yet integrated, and pending
    // ingest jobs — metadata only, each source zeroed on read failure
    // like the queue fields beside it (db_ok already reports an
    // unreachable store). Waited seconds parse from each row's RFC
    // 3339 created_at; a parse failure degrades to 0 (the same
    // convention as queue_health_counts's age).
    let now = chrono::Utc::now();
    let waited = |ts: &str| {
        chrono::DateTime::parse_from_rfc3339(ts)
            .map(|t| now.signed_duration_since(t).num_seconds().max(0))
            .unwrap_or(0)
    };
    let mut waiting_items: Vec<WaitingItemRow> = Vec::new();
    if let Ok(turns) = state.chat_sessions.admin_waiting_turns().await {
        for t in turns {
            waiting_items.push(WaitingItemRow {
                kind: "chat turn",
                user: t.username,
                what: t.session_id.to_string(),
                state: t.status.as_str().to_string(),
                waited_seconds: waited(&t.created_at),
                last_error: t.last_error,
            });
        }
    }
    if let Ok(rows) = state.store.queue_waiting_rows().await {
        for r in rows {
            waiting_items.push(WaitingItemRow {
                kind: "queued write",
                user: r.username,
                what: r.tool.as_str().to_string(),
                state: r.status.as_str().to_string(),
                waited_seconds: waited(&r.created_at),
                last_error: r.detail,
            });
        }
    }
    if let Ok(jobs) = state.store.pending_ingest_job_rows().await {
        for j in jobs {
            waiting_items.push(WaitingItemRow {
                kind: "ingest job",
                user: j.username,
                what: j.slug,
                state: "pending".to_string(),
                waited_seconds: waited(&j.created_at),
                last_error: None,
            });
        }
    }
    let stats = SystemPageStats {
        users: s.user_count,
        queue_depth: s.queue_depth as usize,
        oldest_pending_age_seconds: s.oldest_pending_age_seconds,
        queue_dead_count: s.queue_dead_count as usize,
        llm_last_success_seconds: s.llm_last_success_seconds,
        llm_active: s.llm_active,
        llm_queued: s.llm_queued,
        llm_capacity: s.llm_capacity,
        llm_timeouts_total: s.llm_timeouts_total,
        backend_state: s.backend_state,
        backend_down_seconds: s.backend_down_seconds,
        backend_checked_seconds_ago: s.backend_checked_seconds_ago,
        chat_waiting_count: s.chat_waiting_count,
        waiting_items,
    };
    let admin = request_admin_user(&state, &session).await;
    // The TLS card's inputs: the ACTIVE cert's parsed details and the
    // outstanding CSR (both read from the data dir on every page view —
    // cheap, and always current after an upload/reset/discard).
    let cert = crate::tls_admin::active_cert_details(&state.data_dir);
    let pending = crate::tls_admin::read_pending(&state.data_dir);
    pages::admin_system_page(
        &admin,
        &csrf,
        &stats,
        cert.as_ref(),
        pending.as_ref(),
        params.ok.as_deref(),
        params.error.as_deref(),
    )
    .into_response()
}

/// GET /status — the Librarian status page (mockup 04; admin-only per
/// spec §6 — the operational page shows cross-user jobs): the
/// mutation queue's /health surfaces as StatTiles with state chips,
/// the ingest-jobs table (moved here from the PR-3 System page per
/// spec §8's ruling), and the receipts panel over the most recent queue
/// rows of every state (`queue_recent_rows`; the issue-#1 placeholder
/// it replaced is retired).
pub async fn status_view(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    session: SessionId,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    // The /health values — ONE computation shared with the health
    // route and the System page (the PR-4 SystemStats extraction).
    let stats = crate::health::system_stats(&state).await;
    // Ingest jobs (slug, status, detail, created, shelf name) — the
    // System page's job_rows build, moved here per spec §8's ruling.
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
    // The receipts panel's rows: the 25 most recent queue items of every
    // state (terminal `done`/`dead` included), newest enqueue first —
    // metadata only. A read failure zeroes the panel like the queue
    // StatTiles beside it (health.rs's documented rule; the "degraded"
    // chip already reports an unreachable store).
    let receipts = state.store.queue_recent_rows(25).await.unwrap_or_default();
    let admin = request_admin_user(&state, &session).await;
    pages::librarian_status_page(&admin, &csrf, &stats, &job_rows, &receipts).into_response()
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
    pub ok: Option<String>,
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
                "/admin/sign-in?error={}",
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
                "/admin/sign-in?error={}",
                pages::urlencoding_encode(&e.to_string())
            ))
            .into_response();
        }
    };
    let _ = state.config.set("oidc", &hex::encode(sealed)).await;
    Redirect::to("/admin/sign-in?ok=OIDC%20config%20saved").into_response()
}

#[derive(Deserialize)]
pub struct LlmForm {
    pub url: String,
    pub model: String,
    /// Blank = keep the currently stored key (no approved way to clear
    /// a key exists; re-saving the config without a key is done by the
    /// admin re-entering the URL/model with the key field cleared only
    /// from a keyless state).
    pub api_key: Option<String>,
    /// The five concurrency tunables (LLM concurrency spec §3.2/§3.6).
    /// Carried as raw strings, not `u32`: a missing field (an old
    /// cached form without the inputs) and a non-numeric one must both
    /// mean "keep the stored value", never a 422/500 that loses the
    /// url/model edit. Out-of-range values are clamped, not rejected.
    pub max_parallel: Option<String>,
    pub per_user_active: Option<String>,
    pub query_wait_secs: Option<String>,
    pub chat_wait_secs: Option<String>,
    pub chat_queue_depth: Option<String>,
}

/// A submitted tunable, or `stored` when the field is absent, blank, or
/// not a number. Range enforcement is [`crate::LlmConfig::clamp`]'s job.
fn llm_field(submitted: Option<&String>, stored: u32) -> u32 {
    submitted
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .unwrap_or(stored)
}

/// POST /admin/llm — save the LLM config (admin only). The whole config
/// (api_key is the sensitive part) seals under the service key before
/// storage, mirroring the OIDC secret handling. A blank api_key keeps
/// the stored key; the key is never rendered back to the browser.
///
/// The concurrency tunables are clamped BEFORE sealing (a stored row can
/// never hold an out-of-range limit) and pushed onto the process-global
/// gate immediately after a successful save (spec §3.2: "the admin sees
/// the effect without waiting for traffic. No restart.") — raising
/// `max_parallel` admits queued waiters on the spot.
pub async fn admin_save_llm(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    axum::Form(form): axum::Form<LlmForm>,
) -> Response {
    // The existing config (blank-key keep + legacy row migration both
    // read through it) or the default.
    let existing =
        crate::state::decrypt_config::<crate::LlmConfig>(&state.config, &state.service_key, "llm")
            .await
            .unwrap_or_default();
    let api_key = match form.api_key.as_deref().map(str::trim) {
        None | Some("") => existing.api_key.clone(),
        Some(k) => Some(k.to_string()),
    };
    // Any tunable the form did not carry (or carried as junk) keeps the
    // stored value, so an old cached form still saves url/model without
    // resetting the gate.
    let cfg = crate::LlmConfig {
        url: form.url.trim().to_string(),
        model: form.model.trim().to_string(),
        api_key,
        max_parallel: llm_field(form.max_parallel.as_ref(), existing.max_parallel),
        per_user_active: llm_field(form.per_user_active.as_ref(), existing.per_user_active),
        query_wait_secs: llm_field(form.query_wait_secs.as_ref(), existing.query_wait_secs),
        chat_wait_secs: llm_field(form.chat_wait_secs.as_ref(), existing.chat_wait_secs),
        chat_queue_depth: llm_field(form.chat_queue_depth.as_ref(), existing.chat_queue_depth),
    }
    // Clamp before sealing: the row at rest is always in range.
    .clamp();
    // Seal (serialize + AEAD); a failure must not land plaintext.
    let sealed = crate::state::seal_config(&state.service_key, "llm", &cfg);
    if let Err(e) = state.config.set("llm", &sealed).await {
        return Redirect::to(&format!(
            "/admin/llm?error={}",
            pages::urlencoding_encode(&e.to_string())
        ))
        .into_response();
    }
    // Only a persisted config becomes live (a failed write returned
    // above): the gate and the stored row never disagree.
    cfg.sync_gate();
    Redirect::to("/admin/llm?ok=LLM%20config%20saved").into_response()
}

/// POST /admin/llm/test — probe the LLM backend with one minimal
/// completion using the submitted form values (blank api_key = the
/// stored key, mirroring the save semantics). Nothing is persisted.
/// The redirect verdict carries the backend's status + body on failure
/// so a mis-pasted key or wrong URL is visible in seconds — the chat
/// path only says "librarian unavailable", never why.
pub async fn admin_test_llm(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    axum::Form(form): axum::Form<LlmForm>,
) -> Response {
    // Same blank-key semantics as admin_save_llm: the stored key when
    // the field is blank/absent, so testing what is saved works.
    let existing =
        crate::state::decrypt_config::<crate::LlmConfig>(&state.config, &state.service_key, "llm")
            .await
            .unwrap_or_default();
    let api_key = match form.api_key.as_deref().map(str::trim) {
        None | Some("") => existing.api_key.clone(),
        Some(k) => Some(k.to_string()),
    };
    // The probe is a single direct call: the submitted concurrency
    // tunables are deliberately ignored here (the gate is not entered,
    // nothing is persisted, and `sync_gate` is never called — a test
    // connection must not reconfigure a running server).
    let client = mycelium_librarian::llm::LlmClient::new(&crate::LlmConfig {
        url: form.url.trim().to_string(),
        model: form.model.trim().to_string(),
        api_key,
        ..existing
    });
    let verdict = match client
        .chat_system(Some("You are a connectivity probe."), "Reply with OK", 0.0)
        .await
    {
        Ok(reply) => format!("ok=connected%20%E2%80%94%20backend%20responded:{reply}",),
        Err(e) => {
            tracing::warn!(error = %e, "LLM test connection failed");
            format!(
                "error={}",
                pages::urlencoding_encode(&format!("test connection failed: {e}"))
            )
        }
    };
    Redirect::to(&format!("/admin/llm?{verdict}")).into_response()
}

/// GET /admin/embeddings — the embeddings-backend card on its own
/// page; hosts POST /admin/embeddings.
pub async fn admin_embeddings_view(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    session: SessionId,
    Query(params): Query<AdminParams>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    // Clamped for rendering: a stale (or hand-edited) out-of-range row
    // must not put an unenforceable value back in the box.
    let embeddings =
        crate::state::decrypt_config::<mycelium_librarian::embeddings::EmbeddingsConfig>(
            &state.config,
            &state.service_key,
            "embeddings",
        )
        .await
        .unwrap_or_default()
        .clamp();
    let admin = request_admin_user(&state, &session).await;
    pages::admin_embeddings_page(
        &admin,
        &csrf,
        &embeddings,
        params.ok.as_deref(),
        params.error.as_deref(),
    )
    .into_response()
}

#[derive(Deserialize)]
pub struct EmbeddingsForm {
    pub base_url: String,
    pub model: String,
    /// Blank = keep the currently stored key (no approved way to clear
    /// a key exists; mirrors LlmForm — the key is never rendered back,
    /// so blank is how an edit re-saves its key).
    pub api_key: Option<String>,
    /// Carried as raw strings, not `u32`: a missing field (an old
    /// cached form) and a non-numeric one must both mean "keep the
    /// stored value", never a 422/500 that loses the base_url/model
    /// edit. Out-of-range values are clamped, not rejected.
    pub timeout_secs: Option<String>,
    pub batch: Option<String>,
}

/// A submitted tunable, or `stored` when the field is absent, blank, or
/// not a number. Range enforcement is `EmbeddingsConfig::clamp`'s job
/// (the [`llm_field`] twin).
fn embeddings_field(submitted: Option<&String>, stored: u32) -> u32 {
    submitted
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .unwrap_or(stored)
}

/// POST /admin/embeddings — save the embeddings config (admin only).
/// The whole config (api_key is the sensitive part) seals under the
/// service key before storage, mirroring the LLM config handling. A
/// blank api_key keeps the stored key; the key is never rendered back
/// to the browser. The tunables are clamped BEFORE sealing (the row at
/// rest is always in range). No gate call: embeddings has none.
pub async fn admin_save_embeddings(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    axum::Form(form): axum::Form<EmbeddingsForm>,
) -> Response {
    // The existing config (blank-key keep reads through it) or the
    // default (a first save from a keyless store keeps no key).
    let existing =
        crate::state::decrypt_config::<mycelium_librarian::embeddings::EmbeddingsConfig>(
            &state.config,
            &state.service_key,
            "embeddings",
        )
        .await
        .unwrap_or_default();
    let api_key = match form.api_key.as_deref().map(str::trim) {
        None | Some("") => existing.api_key.clone(),
        Some(k) => Some(k.to_string()),
    };
    // Any tunable the form did not carry (or carried as junk) keeps the
    // stored value — an old cached form still saves base_url/model
    // without resetting the tunables.
    let cfg = mycelium_librarian::embeddings::EmbeddingsConfig {
        base_url: form.base_url.trim().to_string(),
        model: form.model.trim().to_string(),
        api_key,
        timeout_secs: embeddings_field(form.timeout_secs.as_ref(), existing.timeout_secs),
        batch: embeddings_field(form.batch.as_ref(), existing.batch),
    }
    // Clamp before sealing: the row at rest is always in range.
    .clamp();
    // Seal (serialize + AEAD); a failure must not land plaintext.
    let sealed = crate::state::seal_config(&state.service_key, "embeddings", &cfg);
    if let Err(e) = state.config.set("embeddings", &sealed).await {
        return Redirect::to(&format!(
            "/admin/embeddings?error={}",
            pages::urlencoding_encode(&e.to_string())
        ))
        .into_response();
    }
    Redirect::to("/admin/embeddings?ok=Embeddings%20config%20saved").into_response()
}

/// POST /admin/embeddings/test — probe the embeddings backend with one
/// real vector request (one `embed_batch`). Row-first: after a save
/// the CURRENT STORED row is probed (the page's hint says so — save
/// first, then test); with no stored row the submitted form values
/// drive the probe (first-run test-before-save). Nothing is persisted.
/// The failure reason is bounded and stripped of any upstream body —
/// only the status code rides the redirect (full detail logged
/// server-side).
pub async fn admin_test_embeddings(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    axum::Form(form): axum::Form<EmbeddingsForm>,
) -> Response {
    let stored = crate::state::decrypt_config::<mycelium_librarian::embeddings::EmbeddingsConfig>(
        &state.config,
        &state.service_key,
        "embeddings",
    )
    .await;
    if stored.is_none() && (form.base_url.trim().is_empty() || form.model.trim().is_empty()) {
        return Redirect::to(&format!(
            "/admin/embeddings?error={}",
            pages::urlencoding_encode(
                "nothing to test: no saved embeddings config and the form carries no URL/model",
            )
        ))
        .into_response();
    }
    // Row-first; no row → the submitted form against the default
    // tunables (a single direct call ignores the submitted tunables,
    // mirroring admin_test_llm).
    let cfg = stored.unwrap_or_else(|| mycelium_librarian::embeddings::EmbeddingsConfig {
        base_url: form.base_url.trim().to_string(),
        model: form.model.trim().to_string(),
        api_key: form
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(str::to_string),
        ..Default::default()
    });
    let client = mycelium_librarian::embeddings::EmbeddingsClient::new(&cfg);
    let verdict = match client.embed_batch(&["test probe".to_string()]).await {
        Ok(vectors) => {
            let dims = vectors.first().map_or(0, |v| v.len());
            format!("ok=connected%20%E2%80%94%20response%20dimensions:{dims}")
        }
        Err(e) => {
            let reason = embedding_test_reason(&e);
            tracing::warn!(error = %e, "embeddings test connection failed");
            format!(
                "error={}",
                pages::urlencoding_encode(&format!(
                    "test connection failed: {}",
                    mycelium_librarian::llm_health::short_detail(reason),
                ))
            )
        }
    };
    Redirect::to(&format!("/admin/embeddings?{verdict}")).into_response()
}

/// The bounded, non-leaking test-connection failure reason: the
/// upstream STATUS surfaces (a mis-pasted URL or revoked key is
/// visible in seconds), but never its body — an upstream body can
/// carry anything, and the flash text rides a URL. Transport failures
/// say "did not answer"; a malformed response is this module's own
/// diagnostic. The caller bounds the composed string through
/// `llm_health::short_detail`.
fn embedding_test_reason(e: &mycelium_librarian::embeddings::EmbeddingError) -> String {
    match e {
        mycelium_librarian::embeddings::EmbeddingError::Status { status, .. } => {
            format!("HTTP {status}")
        }
        mycelium_librarian::embeddings::EmbeddingError::Request(_) => {
            "the backend did not answer".to_string()
        }
        mycelium_librarian::embeddings::EmbeddingError::Malformed(m) => m.clone(),
    }
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
    Redirect::to("/admin/library?ok=Upload%20limit%20saved").into_response()
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
    Redirect::to("/admin/sign-in?ok=Security%20settings%20saved").into_response()
}

/// POST /admin/bookshelves — create a bookshelf (admin only).
///
/// The store insert is NOT swallowed (the carried Phase-9 follow-up:
/// `let _ =` made a failed insert — e.g. a duplicate name, rejected by
/// `bookshelves.name TEXT NOT NULL UNIQUE` — answer with the success
/// banner while creating nothing). `Err` logs the real detail
/// server-side and bounces the fixed generic literal
/// `?error=bookshelf creation failed`; the store's Display never rides
/// the URL (the Phase-9 convention — history/referrer leakage). The
/// success arm is unchanged.
pub async fn admin_create_bookshelf(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    axum::Form(form): axum::Form<BookshelfForm>,
) -> Response {
    let is_global = form.global == "1";
    match state.store.create_bookshelf(&form.name, is_global).await {
        Ok(_) => Redirect::to("/admin/library?ok=Bookshelf%20created").into_response(),
        Err(e) => {
            tracing::warn!(
                name = %form.name,
                error = %e,
                "bookshelf creation failed"
            );
            Redirect::to("/admin/library?error=bookshelf%20creation%20failed").into_response()
        }
    }
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

// ---------- TLS management (admin) ----------

/// Flash redirect back to /admin/system with an error message (the TLS
/// card's error banner). Shared by the TLS admin POST handlers.
fn tls_flash_error(msg: &str) -> Response {
    Redirect::to(&format!(
        "/admin/system?error={}",
        pages::urlencoding_encode(msg)
    ))
    .into_response()
}

/// POST /admin/tls/upload — install a third-party cert(+chain)/key.
///
/// Validation-before-write (spec): every check — parse, key match,
/// pending-CSR key match — runs against the in-memory pair BEFORE any
/// disk write, so a rejected upload leaves no state. The three writes
/// (cert, key, sidecar) then happen in one synchronous sequence with no
/// await between them, so the download handler and the boot precedence
/// can never observe a half-written pair. If the live reload fails the
/// just-written files are removed again — a pair the live listener
/// cannot load is worthless and must not survive as "active".
///
/// Multipart fields: `cert_pem` (file), `key_pem` (file), `label` (text,
/// optional). Flashes to /admin/system.
pub async fn tls_upload(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    mut multipart: axum::extract::Multipart,
) -> Response {
    // Per-field size cap: a cert chain plus its key is a few KiB, so 2
    // MiB is already generous. axum's own 2 MiB default body limit
    // normally trips first (surfaced through the same flash below); the
    // explicit check keeps the message accurate if that limit is ever
    // raised for this route.
    const MAX_TLS_UPLOAD_BYTES: usize = 2 * 1024 * 1024;
    let mut cert_pem: Option<String> = None;
    let mut key_pem: Option<String> = None;
    let mut label = String::new();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                return tls_flash_error("uploaded file too large (max 2 MiB)");
            }
            Err(e) => {
                tracing::warn!(error = %e, "TLS upload multipart parse failed");
                return tls_flash_error("upload could not be read");
            }
        };
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "cert_pem" | "key_pem" => {
                let is_cert = name == "cert_pem";
                let bytes = match field.bytes().await {
                    Ok(bytes) => bytes,
                    Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                        return tls_flash_error("uploaded file too large (max 2 MiB)");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "TLS upload field read failed");
                        return tls_flash_error("upload could not be read");
                    }
                };
                if bytes.len() > MAX_TLS_UPLOAD_BYTES {
                    return tls_flash_error("uploaded file too large (max 2 MiB)");
                }
                let text = String::from_utf8_lossy(&bytes).to_string();
                if is_cert {
                    cert_pem = Some(text);
                } else {
                    key_pem = Some(text);
                }
            }
            // The label is display metadata for the TLS card; a long one
            // is truncated rather than refused (it never affects the
            // pair's validity).
            "label" => {
                label = field.text().await.unwrap_or_default();
                label = label.trim().chars().take(120).collect();
            }
            _ => {}
        }
    }
    let (cert_pem, key_pem) = match (cert_pem, key_pem) {
        (Some(c), Some(k)) => (c, k),
        _ => return tls_flash_error("both cert and key files are required"),
    };
    // Pending CSR key, when one exists (Decision 1: the uploaded key
    // must be exactly the key the CSR was generated with). Fail CLOSED:
    // a pending key that exists but cannot be read must not be treated
    // as "no pending CSR" — that would silently open the one check that
    // binds the upload to the key the CSR was made for.
    let tls_dir = state.data_dir.join("config/tls");
    let pending = match crate::tls_admin::read_pending_csr_key(&tls_dir) {
        Ok(pending) => pending,
        Err(e) => {
            tracing::error!(error = %e, "pending CSR key unreadable — refusing TLS upload");
            return tls_flash_error(
                "the pending CSR key could not be read — resolve it before uploading",
            );
        }
    };
    let validation =
        match crate::tls_admin::validate_upload(&cert_pem, &key_pem, pending.as_deref()) {
            Ok(v) => v,
            // The exact parse/mismatch error is user-facing (spec): it tells
            // the admin which half of the pair is wrong.
            Err(e) => return tls_flash_error(&format!("upload rejected: {e}")),
        };
    // The sidecar body is built BEFORE any write: serializing it cannot
    // realistically fail (four string fields), and building it up front
    // means a failure there leaves nothing on disk at all.
    let meta = serde_json::json!({
        "label": label,
        "uploaded_at": chrono::Utc::now().to_rfc3339(),
        "fingerprint_sha256": validation.fingerprint_sha256,
        "source": "uploaded",
    });
    let meta = match serde_json::to_string(&meta) {
        Ok(meta) => meta,
        Err(e) => {
            tracing::error!(error = %e, "uploaded sidecar serialization failed");
            return tls_flash_error("internal error");
        }
    };
    // ---- Writes: one synchronous sequence, cert → key → sidecar. ----
    // No awaits in this block: the pair-invariant (both files exist
    // together, or neither) holds for the download handler and boot.
    //
    // Concurrency guarantee, stated precisely: validation→write is NOT
    // atomic across requests, so two concurrent uploads can interleave at
    // the await points before this block (both validate, then both write).
    // What the synchronous sequence guarantees is that a reader never
    // observes a cert/key PAIR that does not match: every failure exit
    // below removes all three files together, and the last writer's three
    // files are mutually consistent. A concurrent request's orphan key can
    // briefly coexist with this request's sidecar; the serving pair never
    // mismatches.
    let up_cert = tls_dir.join("uploaded-cert.pem");
    let up_key = tls_dir.join("uploaded-key.pem");
    let up_meta = tls_dir.join("uploaded.json");
    if let Err(e) = std::fs::create_dir_all(&tls_dir) {
        tracing::error!(error = %e, "TLS upload dir creation failed");
        return tls_flash_error("internal error");
    }
    // One rollback path for every failure exit: a partially written
    // upload must leave NOTHING behind. A truncated uploaded-cert.pem
    // would otherwise be served by tls_cert_download (it prefers that
    // file whenever it exists), and an orphan 0600 key is a stale secret
    // with no owner.
    let rollback = || {
        for path in crate::tls_admin::remove_uploaded_pair(&tls_dir) {
            tracing::error!(path = %path.display(), "TLS upload rollback could not remove a file");
        }
    };
    if let Err(e) = std::fs::write(&up_cert, &cert_pem) {
        tracing::error!(error = %e, "uploaded cert write failed");
        rollback();
        return tls_flash_error("internal error");
    }
    // Key 0600: replace any prior key, then create fresh with the mode
    // (create_new so the mode can never be inherited from an existing
    // file). The key is never served, rendered, or logged.
    if let Err(e) = write_key_0600(&up_key, &key_pem) {
        tracing::error!(error = %e, "uploaded key write failed");
        rollback();
        return tls_flash_error("internal error");
    }
    if let Err(e) = std::fs::write(&up_meta, &meta) {
        tracing::error!(error = %e, "uploaded sidecar write failed");
        rollback();
        return tls_flash_error("internal error");
    }
    // ---- Live hot-swap (the Arc handle is shared with the listener). ----
    // No partial state: if the reload fails, remove the just-written
    // files and flash the error — the files are worthless if the live
    // listener cannot load them.
    //
    // `state.tls` is None only when the server was not started through
    // `serve` (no TLS listener to hot-swap — tests). The install is then
    // real but not live: flash the restart-required variant rather than
    // plain success, which would claim an effect that has not happened.
    let mut restart_required = false;
    match &state.tls {
        Some(tls) => {
            if let Err(e) = tls.reload_from_pem_file(&up_cert, &up_key).await {
                tracing::error!(error = %e, "TLS reload after upload failed");
                rollback();
                return tls_flash_error("upload rejected: the server could not load the pair");
            }
        }
        None => restart_required = true,
    }
    // The pair is installed (live, or pending a restart when there is no
    // listener): a pending CSR it matched is now fulfilled. Cleared only
    // after success — a failed reload must not cost the admin the CSR key
    // they would need to retry.
    if pending.is_some() {
        let _ = std::fs::remove_file(tls_dir.join("pending-csr-key.pem"));
        let _ = std::fs::remove_file(tls_dir.join("pending.json"));
    }
    let mut msg = format!(
        "TLS certificate installed — fingerprint {}…{}",
        // SHA-256 hex is always 64 chars (validate_upload's fingerprint).
        &validation.fingerprint_sha256[..16],
        if restart_required {
            " — takes effect after restart (no live TLS listener in this process)"
        } else {
            ""
        }
    );
    for w in &validation.warnings {
        msg.push_str(&format!(" WARNING: {w}"));
    }
    Redirect::to(&format!(
        "/admin/system?ok={}",
        pages::urlencoding_encode(&msg)
    ))
    .into_response()
}

/// Write the uploaded private key with mode 0600: remove any prior key,
/// then create fresh with the mode (never reuse an existing file's
/// permissions).
fn write_key_0600(path: &std::path::Path, key_pem: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(key_pem.as_bytes())
}

#[derive(Deserialize)]
pub struct CsrForm {
    pub cn: String,
    #[serde(default)]
    pub sans: String,
}

/// The CSR download response (POST generate and GET re-download share
/// it): the PEM body with download headers, `no-store` — the CSR is
/// pending state that discard/fulfill change under the admin's feet.
fn csr_download_response(csr_pem: String) -> Response {
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "application/x-pem-file"),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"csr.pem\"",
            ),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        csr_pem,
    )
        .into_response()
}

/// POST /admin/tls/csr — generate a CSR for cn + sans; the response IS
/// the CSR PEM download. Persists the pending key + metadata (Decision
/// 1: the key stays on disk, 0600, until the matching cert is uploaded
/// or the admin discards it). Replaces any prior pending CSR
/// (last-one-wins).
pub async fn tls_csr(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    axum::Form(form): axum::Form<CsrForm>,
) -> Response {
    let sans: Vec<String> = form
        .sans
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    // The localhost default for an empty list is applied HERE, before
    // persisting, so pending.json records what the CSR actually contains
    // (the card renders it; the rebuild path re-applies it defensively).
    // A non-empty list is verbatim — never force-merged.
    let sans = if sans.is_empty() {
        vec![crate::tls_admin::DEFAULT_SAN.to_string()]
    } else {
        sans
    };
    let cn = form.cn.trim();
    let (csr_pem, key_pem) = match crate::tls_admin::build_csr(cn, &sans) {
        Ok(pair) => pair,
        // The Display strings are user-facing (missing CN, parse
        // failure) — same convention as the upload flash.
        Err(e) => return tls_flash_error(&format!("CSR generation failed: {e}")),
    };
    let meta = crate::tls_admin::PendingCsr {
        cn: cn.to_string(),
        sans,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    if let Err(e) = crate::tls_admin::save_pending(&state.data_dir, &meta, &key_pem) {
        // save_pending writes the key first and the meta last, so a
        // failure here means the key write failed and the PREVIOUS
        // pending pair (if any) is fully intact — the new CSR simply
        // never became pending. Log the detail, flash the generic (the
        // io error is not actionable for the admin).
        tracing::error!(error = %e, "pending CSR persist failed");
        return tls_flash_error("the pending CSR could not be saved");
    }
    csr_download_response(csr_pem)
}

/// GET /admin/tls/csr — re-download the outstanding pending CSR. The
/// CSR PEM is not persisted; it is rebuilt from the pending key +
/// metadata (rcgen's serialize_request is not byte-stable, so the
/// rebuilt CSR is the same request, not the same bytes).
pub async fn tls_csr_download(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
) -> Response {
    let tls_dir = state.data_dir.join("config/tls");
    let Some(meta) = crate::tls_admin::read_pending(&state.data_dir) else {
        return tls_flash_error("no CSR is pending");
    };
    // Absent key → "no CSR is pending"; present-but-unreadable → its own
    // message (same fail-closed posture as the upload path — a broken
    // key is never silently treated as no key).
    let key_pem = match crate::tls_admin::read_pending_csr_key(&tls_dir) {
        Ok(Some(key)) => key,
        Ok(None) => return tls_flash_error("no CSR is pending"),
        Err(e) => {
            tracing::error!(error = %e, "pending CSR key unreadable");
            return tls_flash_error("the pending CSR key could not be read");
        }
    };
    match crate::tls_admin::build_csr_from_pending(&key_pem, &meta.cn, &meta.sans) {
        Ok(csr_pem) => csr_download_response(csr_pem),
        Err(e) => {
            tracing::error!(error = %e, "pending CSR rebuild failed");
            tls_flash_error("the pending CSR could not be rebuilt")
        }
    }
}

/// POST /admin/tls/pending/discard — drop the outstanding CSR (key +
/// metadata). Idempotent: discarding nothing pending is fine.
pub async fn tls_pending_discard(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
) -> Response {
    crate::tls_admin::clear_pending(&state.data_dir);
    Redirect::to(&format!(
        "/admin/system?ok={}",
        pages::urlencoding_encode("pending CSR discarded")
    ))
    .into_response()
}

/// The regenerate form: a comma-separated SAN list (empty → localhost
/// via sans_from_env's semantics — trim, dedup, order-stable).
#[derive(Deserialize)]
pub struct RegenerateForm {
    #[serde(default)]
    pub sans: String,
}

/// POST /admin/tls/regenerate — regenerate the managed self-signed
/// pair with a fresh key and the requested SANs. Identical SANs still
/// mint a fresh key (the action is never a silent no-op).
///
/// Write ordering mirrors the upload path: the new pair + sans.txt
/// sidecar land on disk in one synchronous sequence (no await between
/// the writes) so no reader can observe a half-regenerated pair. The
/// live reload runs last; a reload failure leaves the new pair on disk
/// with the restart-required flash (the pair is the fallback either
/// way — unlike a failed upload, nothing needs rolling back).
///
/// When an uploaded cert is active, the regenerated pair becomes the
/// fallback only: the live listener keeps serving the uploaded cert,
/// and the flash says so.
pub async fn tls_regenerate(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
    axum::Form(form): axum::Form<RegenerateForm>,
) -> Response {
    let tls_dir = state.data_dir.join("config/tls");
    // The localhost default for an empty list is applied HERE (same
    // convention as tls_csr), so the sidecar records what the cert
    // actually carries; a non-empty list is verbatim.
    let sans = crate::tls_admin::sans_from_env(Some(&form.sans));
    // Cap the untrusted input BEFORE any regeneration (security review
    // F1): a maxed-out form body must not mint a multi-megabyte
    // permanent cert. The message is user-facing (validate_sans).
    if let Err(e) = crate::tls_admin::validate_sans(&sans) {
        return tls_flash_error(&format!("regeneration rejected: {e}"));
    }
    if let Err(e) = crate::cert::regenerate_managed_pair(&tls_dir, &sans) {
        tracing::error!(error = %e, "generated pair regeneration failed");
        return tls_flash_error("the generated certificate could not be regenerated");
    }
    // Which pair is live now?
    let uploaded_active =
        tls_dir.join("uploaded-cert.pem").exists() && tls_dir.join("uploaded-key.pem").exists();
    let mut restart_required = false;
    match (&state.tls, uploaded_active) {
        // Generated pair is live → hot-swap to the fresh pair.
        (Some(tls), false) => {
            let (cert, key) = (tls_dir.join("cert.pem"), tls_dir.join("key.pem"));
            if let Err(e) = tls.reload_from_pem_file(&cert, &key).await {
                tracing::error!(error = %e, "TLS reload after regenerate failed");
                return tls_flash_error("the server could not load the regenerated pair");
            }
        }
        // Uploaded cert is active → it keeps serving; the fresh pair is
        // the fallback. No reload, and the flash must say so.
        (Some(_), true) => {}
        // No live listener in this process (tests / a non-serving
        // process): the disk state is real but takes effect at boot.
        (None, _) => restart_required = true,
    }
    // Flash the new generated pair's fingerprint. NOT
    // active_cert_details (it prefers the uploaded pair when one is
    // active, and the flash must name the pair the action minted).
    let fingerprint =
        crate::tls_admin::generated_cert_fingerprint(&state.data_dir).unwrap_or_default();
    let msg = format!(
        "self-signed certificate regenerated with {} — fingerprint {}…{}",
        sans.join(", "),
        // SHA-256 hex is always 64 chars; the same 16-char prefix the
        // upload flash shows.
        &fingerprint[..16.min(fingerprint.len())],
        if uploaded_active {
            " — the uploaded certificate stays active; the regenerated pair is its fallback"
        } else if restart_required {
            " — takes effect after restart (no live TLS listener in this process)"
        } else {
            ""
        }
    );
    Redirect::to(&format!(
        "/admin/system?ok={}",
        pages::urlencoding_encode(&msg)
    ))
    .into_response()
}

/// POST /admin/tls/reset — roll back to the managed generated pair:
/// remove the uploaded pair + sidecar and reload the live listener from
/// `cert.pem`/`key.pem`. The generated pair is NEVER deleted by this
/// action (spec), and the pending CSR is untouched (its lifecycle is
/// discard/fulfill, not reset).
pub async fn tls_reset(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
) -> Response {
    let tls_dir = state.data_dir.join("config/tls");
    let leftovers = crate::tls_admin::remove_uploaded_pair(&tls_dir);
    if !leftovers.is_empty() {
        // An undeletable uploaded file would keep being served (the
        // download handler and boot prefer the uploaded pair whenever it
        // exists), so this is a real failure, not a warning.
        for path in &leftovers {
            tracing::error!(path = %path.display(), "TLS reset could not remove an uploaded file");
        }
        return tls_flash_error("the uploaded certificate could not be removed");
    }
    // Reload from the generated pair. `state.tls` is None only when the
    // server was not started through `serve` (tests): the rollback on
    // disk is real but not live — flash the restart-required variant
    // (Task 4's convention) rather than claim an effect that has not
    // happened.
    let (cert, key) = (tls_dir.join("cert.pem"), tls_dir.join("key.pem"));
    match &state.tls {
        Some(tls) => {
            if let Err(e) = tls.reload_from_pem_file(&cert, &key).await {
                tracing::error!(error = %e, "TLS reload after reset failed");
                return tls_flash_error("the server could not load the generated pair");
            }
        }
        None => {
            return Redirect::to(&format!(
                "/admin/system?ok={}",
                pages::urlencoding_encode(
                    "uploaded certificate removed — takes effect after restart (no live TLS listener in this process)"
                )
            ))
            .into_response();
        }
    }
    Redirect::to(&format!(
        "/admin/system?ok={}",
        pages::urlencoding_encode("reverted to the generated certificate")
    ))
    .into_response()
}

/// GET /admin/tls/cert — download the ACTIVE cert PEM: the uploaded
/// cert when one is in effect, else the managed generated pair. Only
/// the certificate is ever served — the private key is not readable
/// through any route. `no-store`: the file is the live serving
/// material's public half and changes under the admin's feet.
pub async fn tls_cert_download(
    State(state): State<AppState>,
    _admin: mycelium_auth::rbac::RequireAdmin,
) -> Response {
    let tls_dir = state.data_dir.join("config/tls");
    let uploaded = tls_dir.join("uploaded-cert.pem");
    let path = if uploaded.exists() {
        uploaded
    } else {
        tls_dir.join("cert.pem")
    };
    match tokio::fs::read_to_string(&path).await {
        Ok(pem) => (
            StatusCode::OK,
            [
                (axum::http::header::CONTENT_TYPE, "application/x-pem-file"),
                // Attachment disposition (same pattern as the CSR
                // download): the PEM body is attacker-influenced when an
                // uploaded cert is active, so never let a browser render
                // it inline in the admin's tab.
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    "attachment; filename=\"cert.pem\"",
                ),
                (axum::http::header::CACHE_CONTROL, "no-store"),
            ],
            pem,
        )
            .into_response(),
        Err(e) => {
            // The path is server-side config; log the detail, serve a
            // generic body (the endpoint-wide convention).
            tracing::error!(error = %e, "TLS cert read failed");
            error_response(StatusCode::NOT_FOUND, "certificate not found")
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
    let mut replace = false;
    let mut file: Option<Vec<u8>> = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "bookshelf" => bookshelf = field.text().await.unwrap_or_default(),
            "slug" => slug = field.text().await.unwrap_or_default(),
            "title" => title = field.text().await.unwrap_or_default(),
            "replace" => replace = field.text().await.unwrap_or_default() == "1",
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
    // Re-ingest: remove the existing DB row, catalog concepts, and
    // stack text first, then submit creates everything fresh.
    if replace {
        let _ = state.store.delete_book_row(&slug).await;
        let _ = mycelium_librarian::delete_book(&state.store, &state.service_key, &slug).await;
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
                // Seed refresh (v1 parity): the library overview changed.
                mycelium_mcp::seed::refresh_seed(&state.store, &state.service_key).await;
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

/// A passage resolution failure: the HTTP status + message the API
/// endpoint reports. The Books page renders the message as its
/// warning banner (RF 4) — same gates, same wording, different
/// surface.
type PassageResolveError = (StatusCode, String);

/// Resolve a `book://<slug>#<anchor>` resource for `user`: the book
/// must exist, its shelf must be global-read (or the requester is an
/// admin), and the passage must extract from the shared stacks
/// (admin-configurable cap, pre-clamped). The ONE resolution path
/// behind both GET /api/v1/passages (reports the status) and the Books
/// page's `?passage=` reader pane (renders the message as a banner) —
/// the visibility gates and extraction are shared, never forked.
async fn resolve_passage(
    state: &AppState,
    user: &SessionUser,
    resource: &str,
) -> Result<mycelium_core::library::Passage, PassageResolveError> {
    let Some(book_ref) = mycelium_core::library::parse_book_ref(resource) else {
        return Err((
            StatusCode::BAD_REQUEST,
            "invalid book:// resource".to_string(),
        ));
    };
    // The book must exist and its shelf must be global-read (or the
    // requester is an admin).
    let Some(book) = state
        .store
        .book_by_slug(&book_ref.slug)
        .await
        .unwrap_or(None)
    else {
        return Err((StatusCode::NOT_FOUND, "book not found".to_string()));
    };
    let shelf_row: Option<(i64,)> =
        sqlx::query_as("SELECT is_global_read FROM bookshelves WHERE id = ?")
            .bind(book.bookshelf_id.to_string())
            .fetch_optional(state.store.pool())
            .await
            .ok()
            .flatten();
    let Some((is_global,)) = shelf_row else {
        return Err((StatusCode::NOT_FOUND, "bookshelf not found".to_string()));
    };
    if is_global == 0 && user.role != Role::Admin {
        return Err((
            StatusCode::FORBIDDEN,
            "bookshelf is not global-read".to_string(),
        ));
    }
    // Read the stack text and extract the passage (admin-configurable
    // cap, pre-clamped).
    let text =
        match mycelium_librarian::read_stack_text(&state.store, &state.service_key, &book_ref.slug)
            .await
        {
            Ok(t) => t,
            Err(_) => {
                return Err((StatusCode::NOT_FOUND, "book text unavailable".to_string()));
            }
        };
    let cap = state.security_config().await.passage_max_chars;
    mycelium_core::library::extract_passage_capped(&book_ref.slug, &book_ref.anchor, &text, cap)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))
}

/// GET /api/v1/passages?resource=book://slug#anchor — read a passage
/// from the shared library stacks. Global-read bookshelves are
/// readable by all users; admin-private ones by admins only.
pub async fn api_passage(
    State(state): State<AppState>,
    user: SessionUser,
    Query(params): Query<PassageParams>,
) -> Response {
    match resolve_passage(&state, &user, &params.resource).await {
        Ok(passage) => Json(serde_json::json!({
            "slug": passage.slug,
            "anchor": passage.anchor,
            "text": passage.text,
        }))
        .into_response(),
        Err((status, message)) => error_response(status, &message),
    }
}

// ---------- Librarian chat (v1 parity: the agent behind the UI) ----------

#[derive(Deserialize)]
pub struct ChatMessageForm {
    /// The user's message.
    pub message: String,
}

/// The capacity refusal both unbuffered LLM surfaces return when no
/// gate slot comes free within their wait bound (LLM concurrency spec
/// §3.3). Deliberately distinct from the LLM-unreachable 503 wording:
/// "come back" is not "nothing is configured".
pub const LLM_CAPACITY_MSG: &str = "LLM at capacity — try again shortly";

/// The gate-timeout refusal: 429 + `Retry-After: 5` with the
/// endpoint-wide `{"error": …}` shape (spec §3.3 — retryable, and no
/// internal detail on the wire).
fn llm_capacity_response() -> Response {
    let mut response = error_response(StatusCode::TOO_MANY_REQUESTS, LLM_CAPACITY_MSG);
    response.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        axum::http::HeaderValue::from_static("5"),
    );
    response
}

/// POST /api/v1/dream — run a maintenance/consolidation pass over the
/// caller's private bundle (v1 parity: orphans, broken links, likely
/// duplicates, oversized concepts). Deterministic signals decide
/// whether the agent runs at all; no signals → no tokens. CSRF is
/// enforced by the /api/v1 middleware layer.
pub async fn api_dream(State(state): State<AppState>, user: SessionUser) -> Response {
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "key unavailable"),
    };
    let cs = ConceptStore::for_user(&state.store, user.user_id, master)
        // The dream agent may write concepts: label its store (spec
        // §3.1 `dream` — system provenance; nothing model-authored
        // survives the put).
        .with_provenance("dream");
    // The config load is also the gate's sync point (spec §3.2): the
    // clamped row is the single source of both the gate's limits and
    // this surface's wait bound — no separate plumbing.
    let llm =
        crate::state::decrypt_config::<crate::LlmConfig>(&state.config, &state.service_key, "llm")
            .await
            .unwrap_or_default()
            .clamp();
    llm.sync_gate();
    let client = mycelium_librarian::llm::LlmClient::new(&llm);
    let scopes = build_agent_scopes(
        &state.store,
        &state.service_key,
        visible_library_slugs(&state, &user).await,
        user.user_id,
    );
    // Dream is wait class with the CHAT bound (spec §5): admin-triggered
    // and rare, so it may wait for a slot — but it never jumps the
    // queue, and it never hangs unbounded.
    let permit = match mycelium_librarian::llm_gate::LLM_GATE
        .enter(
            Some(user.user_id),
            std::time::Duration::from_secs(u64::from(llm.chat_wait_secs)),
            |_| {},
        )
        .await
    {
        Ok(permit) => permit,
        Err(e) => {
            tracing::warn!(
                error = %e,
                wait_secs = llm.chat_wait_secs,
                user_id = %user.user_id,
                "llm gate: no slot for dream within the wait bound"
            );
            return llm_capacity_response();
        }
    };
    let outcome = mycelium_librarian::dream::run_dream(&client, &cs, &scopes).await;
    // The slot is free the moment the run is: the response body is
    // built after the release, not under the permit.
    drop(permit);
    match outcome {
        Ok(report) => Json(report).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "dream failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// POST /api/v1/chat — the stateless single-turn endpoint (spec D9):
/// one message in, one reply out, no cross-call memory (the
/// session-scoped endpoints under /chat/sessions own multi-turn
/// history). The agent (same one the MCP tools use) answers over the
/// caller's private bundle; chat mode also allows write tools, so
/// "record that ..." messages persist knowledge. Returns the reply as
/// JSON.
pub async fn api_chat(
    State(state): State<AppState>,
    user: SessionUser,
    _session: SessionId,
    axum::Form(form): axum::Form<ChatMessageForm>,
) -> Response {
    if form.message.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "empty message");
    }
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "key unavailable"),
    };
    let cs = ConceptStore::for_user(&state.store, user.user_id, master)
        // The chat-mode agent may write concepts: label its store (spec
        // §3.1 — system provenance; nothing model-authored survives the
        // put).
        .with_provenance("chat");
    // Config load doubles as the gate sync point (spec §3.2).
    let llm =
        crate::state::decrypt_config::<crate::LlmConfig>(&state.config, &state.service_key, "llm")
            .await
            .unwrap_or_default()
            .clamp();
    llm.sync_gate();
    let client = mycelium_librarian::llm::LlmClient::new(&llm);
    // Stateless by design (spec D9): single-shot — this message only.
    let history = vec![mycelium_librarian::llm::ConversationTurn::User(
        form.message.clone(),
    )];
    let scopes = build_agent_scopes(
        &state.store,
        &state.service_key,
        visible_library_slugs(&state, &user).await,
        user.user_id,
    );
    // Wait class, bounded by `query_wait_secs` (spec §3.3): this
    // endpoint holds an HTTP request open, so its bound is the short
    // one. No slot in time → 429 + Retry-After, which the caller can
    // tell apart from the 503 below (nothing reachable to ask).
    let permit = match mycelium_librarian::llm_gate::LLM_GATE
        .enter(
            Some(user.user_id),
            std::time::Duration::from_secs(u64::from(llm.query_wait_secs)),
            |_| {},
        )
        .await
    {
        Ok(permit) => permit,
        Err(e) => {
            tracing::warn!(
                error = %e,
                wait_secs = llm.query_wait_secs,
                user_id = %user.user_id,
                "llm gate: no slot for stateless chat within the wait bound"
            );
            return llm_capacity_response();
        }
    };
    let outcome = mycelium_librarian::agent::run_chat(&client, &cs, &scopes, &history).await;
    // Released before the response is built — see `api_dream`.
    drop(permit);
    match outcome {
        Ok(reply) => Json(serde_json::json!({ "reply": reply })).into_response(),
        Err(mycelium_librarian::agent::AgentError::Llm(e)) => {
            tracing::warn!(error = %e, "librarian chat LLM failure");
            // Same honesty rule as the runner's failure rows
            // (`chat_runner::llm_failure_detail`): a reachable backend's
            // refusal carries its real error text; the configure-me
            // wording is reserved for a backend that is not configured
            // at all. The 503 status itself is unchanged (API contract).
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &crate::chat_runner::llm_failure_detail(
                    &e,
                    mycelium_librarian::llm_health::LLM_HEALTH.state(),
                ),
            )
        }
        Err(e) => {
            tracing::error!(error = %e, "librarian chat failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// GET /chat (?s=<id>|new) — the chat page with the session sidebar
/// and the selected session's transcript (spec §3.6, SSR). No `?s` →
/// the newest session (newest-by-default; an empty history renders the
/// draft); `?s=new` → the draft (NO session row created — D2);
/// foreign/unknown/non-UUID ids → 303 to `/chat` (the page redirects
/// where the API 404s — the same "not yours" with no oracle
/// difference). A master-key failure degrades to the sidebar-less
/// shell renderable (house convention: key-unavailable pages still
/// answer), never a 500.
pub async fn chat_view(
    State(state): State<AppState>,
    user: SessionUser,
    session: SessionId,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let csrf = pages::current_csrf(&state, &session).await;
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        // Degraded but renderable: the empty sidebar + draft log.
        Err(_) => return pages::chat_page(&user, &csrf, &[], None).into_response(),
    };
    let sessions = state
        .chat_sessions
        .list_sessions(user.user_id, &master)
        .await
        .unwrap_or_default();
    let active = match params.get("s").map(|s| s.as_str()) {
        // Absent → newest-by-default: sessions[0] when the history is
        // non-empty, the draft otherwise. `new` → always the draft.
        None => match sessions.first() {
            Some(newest) => load_active(&state, user.user_id, &master, newest.id).await,
            None => None,
        },
        Some("new") => None,
        Some(id) => match Uuid::parse_str(id) {
            Ok(id) => match load_active(&state, user.user_id, &master, id).await {
                Some(view) => Some(view),
                // Unknown/foreign: the store's NotFound (both ids land
                // here) → redirect; the sidebar-less renderable path is
                // only for the degraded key case above.
                None => return Redirect::to("/chat").into_response(),
            },
            Err(_) => return Redirect::to("/chat").into_response(),
        },
    };
    pages::chat_page(&user, &csrf, &sessions, active.as_ref()).into_response()
}

/// Load one session's transcript view for the page (meta + full
/// transcript). Errors (including NotFound — a session deleted between
/// the list and this read) collapse to None after the caller's
/// redirect; here they mean a store/crypto failure → the draft view on
/// a live page is wrong, so the NotFound-vs-error split is moot only
/// for the absent-`?s` newest path, where the same redirect would
/// recurse — instead None renders the draft.
///
/// The caller handles the redirect for an explicit `?s=<id>`; this
/// helper returns None for both NotFound (explicit-id path redirects)
/// and absent-path failures (renders the draft — a fresh boot with an
/// unreadable newest session degrades to an empty log, never a
/// redirect loop).
async fn load_active(
    state: &AppState,
    user_id: Uuid,
    master: &mycelium_crypto::keys::MasterKey,
    session_id: Uuid,
) -> Option<pages::ChatActiveView> {
    let meta = match state
        .chat_sessions
        .session_meta(user_id, master, session_id)
        .await
    {
        Ok(m) => m,
        // NotFound (a session deleted between the list and this read,
        // or a foreign id the caller redirects) is a normal outcome;
        // anything else (store/crypto failure) is logged server-side —
        // the 303/empty behaviors below stay identical either way.
        Err(ChatError::NotFound) => return None,
        Err(e) => {
            tracing::warn!(
                user_id = %user_id,
                session_id = %session_id,
                error = %e,
                "chat session load failed"
            );
            return None;
        }
    };
    let messages = state
        .chat_sessions
        .messages_after(user_id, master, session_id, 0)
        .await
        .unwrap_or_default();
    Some(pages::ChatActiveView {
        id: meta.id,
        title: meta.title,
        messages,
        turn_status: meta.turn_status,
        turn_resumable: meta.turn_resumable,
        max_seq: meta.max_seq,
    })
}
