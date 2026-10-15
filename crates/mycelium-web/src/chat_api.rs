//! Session-scoped chat API (spec §3.5): the sidebar list + meta, the
//! draft-create and submit SSE streams, reattach, and session delete
//! (JSON + native-form variants).
//!
//! All knowledge operations run in the detached runner
//! (`chat_runner::spawn_chat_run`); every endpoint here only rows the
//! turn in and hands back a stream. Submit and reattach share ONE
//! stream shape (`chat_stream_response`): subscribe first, snapshot
//! the transcript after a cursor, replay agent rows (user rows never
//! replay — the submitting client already has them), then tail the
//! live broadcast until the terminal event. A turn with no live entry
//! replays and closes immediately — poll semantics, so a
//! poll-shaped reopen is well-defined.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};

use mycelium_auth::rbac::SessionUser;
use mycelium_crypto::keys::MasterKey;
use mycelium_store::chat::{ChatError, ChatRole};
use uuid::Uuid;

use crate::api::{
    ChatMessageForm, IntoResponseWithBytesStream, error_response, visible_library_slugs,
};
use crate::chat_runner::{ChatRunContext, ChatSseEvent, ChatSseKind, spawn_chat_run};
use crate::middleware::SessionId;
use crate::state::AppState;

/// GET /api/v1/chat/sessions — the user's sessions, newest-first (the
/// sidebar list). Titles decrypt under the caller's master key; a
/// store/decrypt failure is a generic 500 (house convention).
pub async fn api_chat_sessions(State(state): State<AppState>, user: SessionUser) -> Response {
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    };
    match state
        .chat_sessions
        .list_sessions(user.user_id, &master)
        .await
    {
        Ok(sessions) => Json(SessionListJson {
            sessions: sessions
                .into_iter()
                .map(|s| SessionSummaryJson {
                    id: s.id,
                    title: s.title,
                    updated_at: s.updated_at,
                })
                .collect(),
        })
        .into_response(),
        Err(e) => chat_error_response(&e),
    }
}

/// GET /api/v1/chat/sessions/{id} — session meta (the JS liveness
/// poll input): title, turn status, resumability, max seq. Foreign or
/// unknown ids are 404 with no oracle (D10).
pub async fn api_chat_session_meta(
    State(state): State<AppState>,
    user: SessionUser,
    AxumPath(session_id): AxumPath<Uuid>,
) -> Response {
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    };
    match state
        .chat_sessions
        .session_meta(user.user_id, &master, session_id)
        .await
    {
        Ok(meta) => Json(SessionMetaJson {
            id: meta.id,
            title: meta.title,
            turn_status: meta.turn_status.map(|s| s.as_str().to_string()),
            turn_resumable: meta.turn_resumable,
            max_seq: meta.max_seq,
        })
        .into_response(),
        Err(e) => chat_error_response(&e),
    }
}

/// POST /api/v1/chat/sessions — the draft flow: create the session
/// (title derived from the first message) plus its first turn, then
/// the submit stream. The stream's FIRST event is `session` with the
/// new id (emitted here, not by the runner) so the client can fix its
/// URL.
pub async fn api_chat_create_session(
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
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    };
    let title = mycelium_librarian::fallback::derive_title(&form.message);
    let session_id = match state
        .chat_sessions
        .create_session(user.user_id, &master, &title)
        .await
    {
        Ok(id) => id,
        Err(e) => return chat_error_response(&e),
    };
    submit_turn(
        &state,
        &user,
        master,
        session_id,
        Some(ChatSseEvent {
            kind: ChatSseKind::Session(session_id),
            seq: 0,
        }),
        form.message,
    )
    .await
}

/// POST /api/v1/chat/sessions/{id}/messages — submit into an existing
/// session: row the turn (the unique running-turn index is the
/// double-submit guard → 409), detach the runner, stream.
pub async fn api_chat_submit_session(
    State(state): State<AppState>,
    user: SessionUser,
    _session: SessionId,
    AxumPath(session_id): AxumPath<Uuid>,
    axum::Form(form): axum::Form<ChatMessageForm>,
) -> Response {
    if form.message.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "empty message");
    }
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    };
    submit_turn(&state, &user, master, session_id, None, form.message).await
}

/// GET /api/v1/chat/sessions/{id}/stream?after=n — reattach. `after`
/// is the client's last-seen seq (absent = live-only: replay nothing,
/// just tail). A running turn is tailed live; a finished (or never
/// spawned) turn replays and closes — poll semantics.
pub async fn api_chat_reattach(
    State(state): State<AppState>,
    user: SessionUser,
    AxumPath(session_id): AxumPath<Uuid>,
    Query(after): Query<AfterParam>,
) -> Response {
    let master = match state.master_key_for(user.user_id).await {
        Ok(m) => m,
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    };
    // Ownership + liveness in one user-scoped query: a foreign or
    // unknown id is NotFound; no running turn → replay-and-close.
    let turn = match state
        .chat_sessions
        .running_turn(user.user_id, session_id)
        .await
    {
        Ok(t) => t,
        Err(e) => return chat_error_response(&e),
    };
    // Absent cursor = live-only: an unreachable seq replays nothing.
    let cursor = after.after.unwrap_or(i64::MAX);
    chat_stream_response(
        &state,
        &user,
        &master,
        session_id,
        None,
        cursor,
        turn.map(|t| t.id),
    )
    .await
}

/// DELETE /api/v1/chat/sessions/{id} — delete (409 while a turn runs;
/// 404 foreign/unknown; success `{"ok":true}`).
pub async fn api_chat_delete_json(
    State(state): State<AppState>,
    user: SessionUser,
    AxumPath(session_id): AxumPath<Uuid>,
) -> Response {
    match state
        .chat_sessions
        .delete_session(user.user_id, session_id)
        .await
    {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => chat_error_response(&e),
    }
}

/// POST /api/v1/chat/sessions/{id}/delete — the native sidebar form's
/// variant: same store call, success redirects to /chat (303). The
/// error statuses render the same JSON bodies as the DELETE variant —
/// the no-JS path can show them; the JS path never sees them.
pub async fn api_chat_delete_redirect(
    State(state): State<AppState>,
    user: SessionUser,
    AxumPath(session_id): AxumPath<Uuid>,
) -> Response {
    match state
        .chat_sessions
        .delete_session(user.user_id, session_id)
        .await
    {
        Ok(()) => Redirect::to("/chat").into_response(),
        Err(e) => chat_error_response(&e),
    }
}

/// The shared submit path (draft-create and {id} submit): create the
/// turn row (the double-submit guard), build the detached runner's
/// context (LLM config + library visibility resolve HERE — they borrow
/// `AppState`, which the task must not), spawn the run, then stream.
/// `first_event` is the draft flow's `session` event.
async fn submit_turn(
    state: &AppState,
    user: &SessionUser,
    master: MasterKey,
    session_id: Uuid,
    first_event: Option<ChatSseEvent>,
    message: String,
) -> Response {
    let turn = match state
        .chat_sessions
        .create_turn(user.user_id, &master, session_id, &message)
        .await
    {
        Ok(t) => t,
        Err(e) => return chat_error_response(&e),
    };
    // The runner's own copies of everything AppState holds (the task
    // outlives the request). LLM failures never 503 here — the run is
    // detached; failures arrive as events (spec §5).
    let llm =
        crate::state::decrypt_config::<crate::LlmConfig>(&state.config, &state.service_key, "llm")
            .await
            .unwrap_or_default();
    let ctx = ChatRunContext {
        store: (*state.store).clone(),
        service_key: (*state.service_key).clone(),
        chat: (*state.chat_sessions).clone(),
        runs: Arc::clone(&state.chat_runs),
        llm,
        scopes_visible: visible_library_slugs(state, user).await,
    };
    // The replay cursor is this turn's user row: only THIS turn's
    // agent rows replay — a later submit in a session must not re-emit
    // prior turns' replies (the submitting client already rendered
    // them). At submit time the window is empty; it exists purely to
    // close the subscribe→snapshot race (an event persisted in
    // between must not be lost).
    let cursor = turn.user_seq;
    // Registers the turn in the registry BEFORE spawning (spec §3.3:
    // the entry exists before the task, so a reattach racing the spawn
    // finds it).
    let turn_id = spawn_chat_run(ctx, user.user_id, master.clone(), session_id, turn, false).await;
    chat_stream_response(
        state,
        user,
        &master,
        session_id,
        first_event,
        cursor,
        Some(turn_id),
    )
    .await
}

/// The shared submit/reattach stream builder (spec §3.3's race-free
/// shape): (1) subscribe to the turn's live channel FIRST, (2)
/// snapshot the transcript after the cursor, (3) yield the first
/// event (draft-create's session id), the replayed agent rows (user
/// rows never replay — the submitting client already has them), then
/// the live tail until Done/Error. `turn_id: None` (no live entry —
/// finished or never spawned) means the stream closes after replay:
/// poll semantics, so a poll-shaped reopen is well-defined.
///
/// The subscribe-before-snapshot order makes the race harmless: an
/// event persisted between the two is seen by both paths and the
/// CLIENT drops the duplicate by `seq` (spec: no server-side dedup
/// burden); an event persisted before the subscribe is only in the
/// snapshot; one after is only live. Nothing is lost either way.
async fn chat_stream_response(
    state: &AppState,
    user: &SessionUser,
    master: &MasterKey,
    session_id: Uuid,
    first_event: Option<ChatSseEvent>,
    cursor: i64,
    turn_id: Option<Uuid>,
) -> Response {
    // (1) Subscribe first (see the ordering rationale above).
    let live = turn_id.and_then(|id| state.chat_runs.subscribe(id));
    // (2) Snapshot after the cursor (also the ownership gate: a
    // foreign or unknown id 404s here, not in the stream).
    let rows = match state
        .chat_sessions
        .messages_after(user.user_id, master, session_id, cursor)
        .await
    {
        Ok(rows) => rows,
        Err(e) => return chat_error_response(&e),
    };
    // (3) The stream: first event → replay → live tail.
    let stream = async_stream::stream! {
        if let Some(event) = first_event {
            yield sse_frame(&event);
        }
        for row in rows {
            let event = match row.role {
                // User rows never replay (spec §3.3).
                ChatRole::User => continue,
                // Tool rows store sealed JSON {"name":…,"detail":…}.
                ChatRole::Tool => {
                    let (name, detail) = parse_tool_row(&row.content);
                    ChatSseEvent { kind: ChatSseKind::Tool { name, detail }, seq: row.seq }
                }
                // Replay vocabulary: an assistant ROW is "assistant",
                // never "done" — done means a live turn finished.
                ChatRole::Assistant => ChatSseEvent {
                    kind: ChatSseKind::Assistant { reply: row.content },
                    seq: row.seq,
                },
                ChatRole::Error => ChatSseEvent {
                    kind: ChatSseKind::Error { error: row.content },
                    seq: row.seq,
                },
            };
            yield sse_frame(&event);
        }
        if let Some(mut rx) = live {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let terminal = matches!(
                            event.kind,
                            ChatSseKind::Done { .. } | ChatSseKind::Error { .. }
                        );
                        yield sse_frame(&event);
                        if terminal {
                            break;
                        }
                    }
                    // Lag: events were missed — end the stream; the
                    // client reopens with ?after=<last-seen-seq> (lag is
                    // recovered by cursor reattach, not buffering).
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                    // The runner dropped its sender (finalized): the
                    // terminal event was yielded above, or the turn
                    // ended without one — the transcript is the truth.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
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

/// Map a store error to its endpoint response: NotFound → 404 (no
/// oracle — foreign equals unknown), Conflict → 409 with the
/// running-turn message, everything else → generic 500 logged
/// server-side (house convention).
fn chat_error_response(e: &ChatError) -> Response {
    match e {
        ChatError::NotFound => error_response(StatusCode::NOT_FOUND, "not found"),
        ChatError::Conflict => error_response(
            StatusCode::CONFLICT,
            "a turn is already running in this session",
        ),
        ChatError::Db(err) => {
            tracing::error!(error = %err, "chat store failure");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
        ChatError::Crypto(err) => {
            tracing::error!(error = %err, "chat store failure");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// One SSE frame (the legacy stream's exact wire form): a single
/// `data: <json>` line followed by a blank line.
fn sse_frame(event: &ChatSseEvent) -> Result<String, std::convert::Infallible> {
    let payload = serde_json::to_string(event).expect("sse event serializes");
    Ok(format!("data: {payload}\n\n"))
}

/// Parse a persisted tool row's sealed JSON payload back to its
/// `{"name","detail"}` parts. A row that fails to parse (corrupt or
/// hand-written) degrades to the raw content as the detail — one
/// message bubble, never a broken stream.
fn parse_tool_row(content: &str) -> (String, String) {
    match serde_json::from_str::<serde_json::Value>(content) {
        Ok(v) => (
            v.get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string(),
            v.get("detail")
                .and_then(|d| d.as_str())
                .unwrap_or_default()
                .to_string(),
        ),
        Err(_) => (String::new(), content.to_string()),
    }
}

/// The reattach stream's `after` cursor (absent = live-only).
#[derive(serde::Deserialize)]
pub struct AfterParam {
    pub after: Option<i64>,
}

/// `GET /api/v1/chat/sessions` body.
#[derive(serde::Serialize)]
struct SessionListJson {
    sessions: Vec<SessionSummaryJson>,
}

/// One sidebar entry.
#[derive(serde::Serialize)]
struct SessionSummaryJson {
    id: Uuid,
    title: String,
    updated_at: String,
}

/// `GET /api/v1/chat/sessions/{id}` body: the JS liveness poll input.
/// `turn_status` serializes as the status string or `null` (no
/// interesting turn).
#[derive(serde::Serialize)]
struct SessionMetaJson {
    id: Uuid,
    title: String,
    turn_status: Option<String>,
    turn_resumable: bool,
    max_seq: i64,
}
