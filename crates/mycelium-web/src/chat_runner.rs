//! Detached chat-turn runner: the live-run registry and the
//! persist-first event pipeline behind the /chat SSE streams (chat
//! session history spec §3.3).
//!
//! `spawn_chat_run` registers the turn in `ChatRunRegistry`, then
//! detaches: the spawned task owns its `Store` clone (scopes borrow
//! it, so they are built inside the task — the detached-task
//! pattern), rebuilds the LLM history from the session's persisted
//! rows, and drives `run_chat_streaming`. Every agent event is
//! persisted as a transcript row FIRST and only then broadcast (D4):
//! the transcript is the source of truth and the stream a view over
//! it — a lagged or vanished viewer recovers by replaying rows after
//! a cursor, never by buffering. A `RunGuard` closes the
//! died-without-finalizing failure mode: its Drop finalizes the turn
//! and clears the registry entry, so a `running` row can never wedge
//! a session.
//!
//! The runner is also the chat surface's single LLM-admission point
//! (LLM concurrency spec §3.3): before the agent runs it enters
//! `llm_gate::LLM_GATE` as a wait-class waiter bounded by the stored
//! `chat_wait_secs`, holds the permit across the run and its
//! persistence, and finalizes the turn `failed` with
//! [`CHAT_CAPACITY_MSG`] if no slot comes. Fresh submits (`chat_api`)
//! and resumed turns (`chat_resume`) both arrive here, so neither can
//! start a run without a slot. While it waits it broadcasts `queued`
//! (the observed queue position) and, on admission, `admitted` — the
//! one deliberate exception to persist-before-broadcast: queue state is
//! presentation, not lifecycle (spec §3.3), so these two events have no
//! transcript row, carry no `seq`, and never appear in a replay.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use mycelium_crypto::keys::{MasterKey, ServiceKey};
use mycelium_librarian::agent::{AgentError, AgentEvent, run_chat_streaming};
use mycelium_librarian::llm::{ConversationTurn, LlmClient, LlmConfig};
use mycelium_librarian::llm_gate::{GatePermit, GateTimeout, LLM_GATE};
use mycelium_store::ConceptStore;
use mycelium_store::Store;
use mycelium_store::chat::{ChatError, ChatRole, ChatStore, ChatTurnRow, ChatTurnStatus};

use crate::api::build_agent_scopes;

/// Broadcast channel capacity per running turn (spec §4: lag is
/// recovered by cursor reattach, not buffering).
const CHAT_CHANNEL_CAPACITY: usize = 256;

/// Agent-context rows fed back to the LLM (spec D11): the last 20
/// user/assistant pairs. Tool and error rows never reach the model.
const MAX_HISTORY_PAIRS: usize = 20;

/// The user-facing LLM-down message (the `api_chat` 503 wording —
/// one message everywhere).
const LLM_UNAVAILABLE: &str =
    "the librarian is unavailable (no LLM backend reachable) — configure one in the admin portal";

/// The user-facing message for a turn that never got an LLM slot within
/// `chat_wait_secs` (LLM concurrency spec §3.3: the runner finalizes the
/// turn as `failed` with a clear capacity message, persisted so a
/// reconnecting client sees it).
pub const CHAT_CAPACITY_MSG: &str = "LLM at capacity — turn not started (queue full)";

/// One live-stream event for a chat turn (spec §3.3 vocabulary).
///
/// `seq` is the transcript row the event was persisted as — every
/// broadcast of a PERSISTED event carries it, so clients dedup
/// snapshot/broadcast overlap by `seq` and a lagged receiver reattaches
/// with `?after=<seq>`. The two queue events
/// ([`ChatSseKind::Queued`]/[`ChatSseKind::Admitted`]) are
/// broadcast-only presentation with no row behind them: their `seq` is
/// unused and the serializer omits the field entirely (the clients
/// handle them by type, ahead of the dedup gate).
#[derive(Debug, Clone)]
pub struct ChatSseEvent {
    pub kind: ChatSseKind,
    pub seq: i64,
}

/// The event vocabulary (a superset of the pre-session-era stream).
#[derive(Debug, Clone)]
pub enum ChatSseKind {
    /// Draft-create: the session id for a brand-new session (the
    /// stream's first event; emitted by the submit endpoint, not the
    /// runner).
    Session(Uuid),
    /// The turn is waiting for an LLM slot, at this 0-based queue
    /// position (LLM concurrency spec §3.3/§3.4: emitted on enqueue
    /// and on every position change). Presentation only — never a
    /// transcript row, so never replayed; positions are advisory
    /// (§3.4: the current observation, not a promise).
    Queued { position: usize },
    /// The turn got its LLM slot and the run begins. Emitted only for a
    /// turn that actually queued — a fast-path admission never
    /// announced a wait, so it has nothing to clear.
    Admitted,
    /// The agent invoked a tool (progress).
    Tool { name: String, detail: String },
    /// The run finished with the assistant's reply.
    Done { reply: String },
    /// Replay-only: a persisted assistant row (the reattach stream's
    /// replay vocabulary — `done` means a live turn finished, so a
    /// replayed reply says `assistant`; never broadcast live).
    Assistant { reply: String },
    /// The run failed (the error row's message).
    Error { error: String },
}

// The §3.3 wire shape is flat (`{"type":"tool","name":…,"detail":…,
// "seq":…}`) — a derive cannot express the kind-specific fields plus
// the shared trailing `seq`, so the mapping is written out.
impl serde::Serialize for ChatSseEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(4))?;
        match &self.kind {
            // The queue events return early: they have no transcript
            // row, so no `seq` (§3.3 — presentation, not lifecycle).
            ChatSseKind::Queued { position } => {
                map.serialize_entry("type", "queued")?;
                map.serialize_entry("position", position)?;
                return map.end();
            }
            ChatSseKind::Admitted => {
                map.serialize_entry("type", "admitted")?;
                return map.end();
            }
            ChatSseKind::Session(id) => {
                map.serialize_entry("type", "session")?;
                map.serialize_entry("session_id", id)?;
            }
            ChatSseKind::Tool { name, detail } => {
                map.serialize_entry("type", "tool")?;
                map.serialize_entry("name", name)?;
                map.serialize_entry("detail", detail)?;
            }
            ChatSseKind::Done { reply } => {
                map.serialize_entry("type", "done")?;
                map.serialize_entry("reply", reply)?;
            }
            ChatSseKind::Assistant { reply } => {
                map.serialize_entry("type", "assistant")?;
                map.serialize_entry("reply", reply)?;
            }
            ChatSseKind::Error { error } => {
                map.serialize_entry("type", "error")?;
                map.serialize_entry("error", error)?;
            }
        }
        map.serialize_entry("seq", &self.seq)?;
        map.end()
    }
}

/// Live chat runs: one broadcast channel per in-flight turn.
///
/// The submit endpoint (via `spawn_chat_run`) creates the entry
/// BEFORE spawning the runner; the runner removes it at finalization,
/// and the drop guard removes it if the task dies first. Removing the
/// entry drops the channel's only sender — live receivers see the
/// channel close, which is the reattach stream's end-of-run signal.
#[derive(Default)]
pub struct ChatRunRegistry {
    inner: std::sync::Mutex<HashMap<Uuid, broadcast::Sender<ChatSseEvent>>>,
}

impl ChatRunRegistry {
    /// Create + insert the turn's channel, returning a receiver (the
    /// submitting client's live tail; see `subscribe` for reattach).
    pub fn register(&self, turn_id: Uuid) -> broadcast::Receiver<ChatSseEvent> {
        let (tx, rx) = broadcast::channel(CHAT_CHANNEL_CAPACITY);
        self.inner.lock().unwrap().insert(turn_id, tx);
        rx
    }

    /// A receiver for the turn's live events, or None when the turn
    /// has no live entry (finished or never spawned) — the endpoint
    /// then replays rows and closes (poll semantics, spec §3.3).
    ///
    /// The returned receiver sees only events sent from now on; the
    /// caller snapshots the transcript AFTER subscribing so the
    /// snapshot/broadcast overlap is closed by `seq` dedup.
    pub fn subscribe(&self, turn_id: Uuid) -> Option<broadcast::Receiver<ChatSseEvent>> {
        self.inner
            .lock()
            .unwrap()
            .get(&turn_id)
            .map(|tx| tx.subscribe())
    }

    /// Remove the turn's entry (finalization; drops the sender, which
    /// closes live streams).
    pub fn remove(&self, turn_id: Uuid) {
        self.inner.lock().unwrap().remove(&turn_id);
    }

    /// Whether the turn has a live entry (the drop guard's defusal
    /// check: the runner removes the entry on the normal path, so a
    /// later Drop finds this false and no-ops).
    pub fn contains(&self, turn_id: Uuid) -> bool {
        self.inner.lock().unwrap().contains_key(&turn_id)
    }

    /// Broadcast an event to the turn's subscribers. The single place
    /// that locks the map and sends — `let _ =` because a receiver
    /// being gone (or lagged) is fine: lag is recovered by cursor
    /// reattach, not buffering.
    fn send_broadcast(&self, turn_id: Uuid, event: &ChatSseEvent) {
        if let Some(tx) = self.inner.lock().unwrap().get(&turn_id) {
            let _ = tx.send(event.clone());
        }
    }
}

/// Everything the detached runner task owns (cheap clones of what
/// `AppState` holds; built by the submit endpoints and the resume
/// worker).
#[derive(Clone)]
pub struct ChatRunContext {
    /// The task's own store clone (scopes borrow it inside the task).
    pub store: Store,
    pub service_key: ServiceKey,
    /// The transcript store (sealed chat_* rows).
    pub chat: ChatStore,
    /// The live-run registry shared with the endpoints.
    pub runs: Arc<ChatRunRegistry>,
    /// Admin-configured LLM backend (decrypt at submit time; the task
    /// builds its own `LlmClient`).
    pub llm: LlmConfig,
    /// Bookshelf-visibility slug set for the agent's library scope
    /// (None = unrestricted, i.e. admin) — computed before the spawn
    /// (`visible_library_slugs` is async and borrows `AppState`).
    pub scopes_visible: Option<HashSet<String>>,
}

/// Register the turn in the live-run registry, then detach its runner
/// (spec §3.3: the entry exists before the task, so a reattach racing
/// the spawn finds it). Returns the turn id (the caller's handle for
/// `subscribe`).
// Signature pinned by the plan (Task 4/5 call sites); registration and
// spawn are both synchronous, so the async form never awaits — pinned
// anyway to keep the callers' shape stable.
#[allow(clippy::unused_async)]
pub async fn spawn_chat_run(
    ctx: ChatRunContext,
    user_id: Uuid,
    master: MasterKey,
    session_id: Uuid,
    turn: ChatTurnRow,
    resumed: bool,
) -> Uuid {
    let turn_id = turn.id;
    ctx.runs.register(turn_id);
    tokio::spawn(run_chat_turn(
        ctx, user_id, master, session_id, turn, resumed,
    ));
    turn_id
}

/// The detached runner loop: rebuild history, drive the agent, persist
/// every event before broadcasting it, finalize the turn.
///
/// Terminal handling is path-dependent (spec §3.4/D7): a fresh submit
/// finalizes `failed` on any error; a resumed turn finalizes
/// `interrupted` (attempts already 1 → final, no loop) when the LLM
/// was unreachable — transient, no side-effect doubt — and `failed`
/// on a hard error (the run genuinely ran).
async fn run_chat_turn(
    ctx: ChatRunContext,
    user_id: Uuid,
    master: MasterKey,
    session_id: Uuid,
    turn: ChatTurnRow,
    resumed: bool,
) {
    let turn_id = turn.id;
    // Panic/abort guard, in place before anything that can fail: its
    // Drop finalizes + records + unregisters ONLY when the registry
    // still holds the turn (the normal path removes the entry first,
    // defusing it).
    let _guard = RunGuard::new(
        Arc::clone(&ctx.runs),
        ctx.chat.clone(),
        master.clone(),
        user_id,
        session_id,
        turn_id,
        resumed,
    );

    // History from the persisted transcript (D11): user/assistant
    // rows up to and including this turn's trigger message, last 20
    // pairs. Tool rows never feed the model.
    let history =
        match rebuild_history(&ctx.chat, user_id, &master, session_id, turn.user_seq).await {
            Ok(h) => h,
            Err(e) => {
                // Unreadable transcript = hard error: the run cannot even
                // be posed. (A store that cannot read usually cannot
                // write either — the calls below are attempted and their
                // failures logged, never propagated.)
                tracing::error!(error = %e, turn_id = %turn_id, "chat history rebuild failed");
                finish_turn(
                    &ctx,
                    &master,
                    user_id,
                    session_id,
                    turn_id,
                    ChatTurnStatus::Failed,
                    Some("the librarian could not read the session transcript"),
                )
                .await;
                return;
            }
        };

    // Gate admission (LLM concurrency spec §3.3/§3.5). This is the
    // chat surface's SINGLE admission point: fresh submits and resumed
    // turns both arrive here, so neither can start an agent run without
    // a slot. The stored tunables take effect on the way in (R12: the
    // runner's own config row is the admin's, clamped — no new
    // plumbing), and the permit is held across the whole run + persist
    // loop below, released by its Drop on every exit including a panic.
    let llm = ctx.llm.clamp();
    llm.sync_gate();
    let bound = Duration::from_secs(u64::from(llm.chat_wait_secs));
    // Queue events (§3.3): every observed position is broadcast as
    // `queued` — presentation only, so nothing is persisted and the
    // broadcast happens on the runner's own task (the gate's callback
    // only pushes onto a channel, never blocks under the gate lock).
    // `queued_seen` keeps `admitted` honest: a fast-path admission
    // never announced a wait, so it announces no admission either.
    let mut queued_seen = false;
    let admission = admit(user_id, bound, |position| {
        queued_seen = true;
        ctx.runs.send_broadcast(
            turn_id,
            &ChatSseEvent {
                kind: ChatSseKind::Queued { position },
                seq: 0,
            },
        );
    })
    .await;
    let _permit = match admission {
        Ok(p) => {
            if queued_seen {
                ctx.runs.send_broadcast(
                    turn_id,
                    &ChatSseEvent {
                        kind: ChatSseKind::Admitted,
                        seq: 0,
                    },
                );
            }
            p
        }
        Err(GateTimeout) => {
            // No slot within the bound: the run never started, so there
            // is no side-effect doubt — the turn finalizes `failed`
            // with the capacity message on BOTH paths (a resumed turn
            // already carries attempts = 1, so `interrupted` would not
            // buy it a retry; `interrupted` stays reserved for a run
            // that reached an unreachable LLM).
            tracing::warn!(
                turn_id = %turn_id, user_id = %user_id, resumed,
                "chat turn gave up waiting for an LLM slot"
            );
            finish_turn(
                &ctx,
                &master,
                user_id,
                session_id,
                turn_id,
                ChatTurnStatus::Failed,
                Some(CHAT_CAPACITY_MSG),
            )
            .await;
            return;
        }
    };

    let client = LlmClient::new(&llm);
    let cs = ConceptStore::for_user(&ctx.store, user_id, master.clone());
    // AgentScopes borrows the store — built here, inside the task that
    // owns it (the detached-task pattern).
    let scopes = build_agent_scopes(
        &ctx.store,
        &ctx.service_key,
        ctx.scopes_visible.clone(),
        user_id,
    );

    // The agent emits progress on an unbounded channel (sends never
    // block or drop); the loop below persists+broadcasts each event as
    // it arrives. The single sender lives in the run — when it drops,
    // the channel closes and the loop drains to the result.
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let run = run_chat_streaming(&client, &cs, &scopes, &history, Some(tx));
    tokio::pin!(run);

    let mut done_seen = false;
    let mut failed_seen = false;
    let result: Result<String, AgentError> = loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(ev) => {
                        emit_event(&ctx, &master, user_id, session_id, turn_id, &ev).await;
                        match ev {
                            AgentEvent::Done(_) => done_seen = true,
                            AgentEvent::Failed(_) => failed_seen = true,
                            AgentEvent::Tool { .. } => {}
                        }
                    }
                    // All senders dropped: the run has finished and its
                    // buffered events were all consumed above.
                    None => break (&mut run).await,
                }
            }
            res = &mut run => {
                // The run resolved; its terminal event (if any) was
                // sent before resolving and may still be buffered
                // while we were persisting an earlier event — drain
                // the remainder (unbounded channel: nothing was
                // dropped).
                while let Ok(ev) = rx.try_recv() {
                    emit_event(&ctx, &master, user_id, session_id, turn_id, &ev).await;
                    match ev {
                        AgentEvent::Done(_) => done_seen = true,
                        AgentEvent::Failed(_) => failed_seen = true,
                        AgentEvent::Tool { .. } => {}
                    }
                }
                break res;
            }
        }
    };

    match result {
        Ok(reply) => {
            if !done_seen {
                // Defensive: run_chat_streaming always emits Done
                // before returning Ok (agent.rs) — if that contract
                // ever broke, the transcript would silently miss the
                // reply. Persist + broadcast it here so a successful
                // turn always lands exactly one assistant row and one
                // Done event.
                emit_event(
                    &ctx,
                    &master,
                    user_id,
                    session_id,
                    turn_id,
                    &AgentEvent::Done(reply),
                )
                .await;
            }
            finish_turn(
                &ctx,
                &master,
                user_id,
                session_id,
                turn_id,
                ChatTurnStatus::Done,
                None,
            )
            .await;
        }
        Err(AgentError::Llm(e)) => {
            // LLM failures emit no agent event — the error row and its
            // broadcast are created here. Warn, not error: an
            // unreachable backend is an operator condition, not a
            // server bug (this also fixes the api.rs logging gap).
            tracing::warn!(error = %e, turn_id = %turn_id, "chat run LLM failure");
            let status = if resumed {
                ChatTurnStatus::Interrupted
            } else {
                ChatTurnStatus::Failed
            };
            finish_turn(
                &ctx,
                &master,
                user_id,
                session_id,
                turn_id,
                status,
                Some(LLM_UNAVAILABLE),
            )
            .await;
        }
        Err(e) => {
            // Hard error (step cap, …). A run that emitted its own
            // Failed event already persisted + broadcast the error
            // row in the loop above — a second row here would double
            // it, so the detail is appended only when the run did NOT
            // emit its own event (`failed_seen` false = the error took
            // a path with no event, e.g. an LLM-transport class error
            // surfaced as a hard error): exactly one error row either
            // way (D4 — Task 5's step-cap pin caught the polarity).
            tracing::error!(error = %e, turn_id = %turn_id, "chat run failed");
            let row = (!failed_seen).then(|| e.to_string());
            finish_turn(
                &ctx,
                &master,
                user_id,
                session_id,
                turn_id,
                ChatTurnStatus::Failed,
                row.as_deref(),
            )
            .await;
        }
    }
}

/// Wait-class gate admission for one chat turn, reporting every queue
/// position it observes to `on_position`.
///
/// The gate's own position callback is synchronous and runs under the
/// gate's lock, so it must not block: it only pushes onto an unbounded
/// channel (sends never block or drop), and the positions are consumed
/// here — by the same `select!` that awaits admission, which calls
/// `on_position` (the runner broadcasts the `queued` SSE event from
/// there). Dropping this future (an aborted runner) removes the waiter
/// from the queue.
async fn admit(
    user_id: Uuid,
    bound: Duration,
    mut on_position: impl FnMut(usize),
) -> Result<GatePermit<'static>, GateTimeout> {
    let (pos_tx, mut pos_rx) = mpsc::unbounded_channel::<usize>();
    let gate: &'static _ = &*LLM_GATE;
    let enter = gate.enter(Some(user_id), bound, move |pos| {
        let _ = pos_tx.send(pos);
    });
    tokio::pin!(enter);
    loop {
        tokio::select! {
            res = &mut enter => return res,
            pos = pos_rx.recv() => match pos {
                Some(pos) => on_position(pos),
                // The gate dropped this waiter's callback (admitted, or
                // left the queue): nothing more to forward — the
                // admission future alone decides the outcome, and
                // polling a closed receiver again would spin.
                None => return enter.await,
            },
        }
    }
}

/// Persist one agent event as a transcript row, THEN broadcast it
/// (D4's ordering — the transcript is never behind the stream).
/// Returns the row's seq, or None when the persist failed (nothing is
/// broadcast for a row that does not exist; the gap is closed by the
/// client's cursor replay once the store recovers).
async fn emit_event(
    ctx: &ChatRunContext,
    master: &MasterKey,
    user_id: Uuid,
    session_id: Uuid,
    turn_id: Uuid,
    event: &AgentEvent,
) -> Option<i64> {
    // Tool rows store sealed JSON (§3.2); user/assistant/error rows
    // store sealed plain text.
    let (role, content, kind) = match event {
        AgentEvent::Tool { name, detail } => (
            ChatRole::Tool,
            serde_json::json!({ "name": name, "detail": detail }).to_string(),
            ChatSseKind::Tool {
                name: name.clone(),
                detail: detail.clone(),
            },
        ),
        AgentEvent::Done(reply) => (
            ChatRole::Assistant,
            reply.clone(),
            ChatSseKind::Done {
                reply: reply.clone(),
            },
        ),
        AgentEvent::Failed(msg) => (
            ChatRole::Error,
            msg.clone(),
            ChatSseKind::Error { error: msg.clone() },
        ),
    };
    match ctx
        .chat
        .append_message(user_id, master, session_id, role, &content)
        .await
    {
        Ok(seq) => {
            ctx.runs
                .send_broadcast(turn_id, &ChatSseEvent { kind, seq });
            Some(seq)
        }
        Err(e) => {
            tracing::error!(error = %e, turn_id = %turn_id, "chat event persist failed — not broadcast");
            None
        }
    }
}

/// Terminal bookkeeping shared by every exit path: optionally persist
/// and broadcast an error row, finalize the turn (the store's
/// transition is conditional and idempotent), then remove the
/// registry entry — which also defuses the drop guard. Store failures
/// are logged, never propagated: the registry entry is removed
/// regardless so a finished run can never leak a live entry.
async fn finish_turn(
    ctx: &ChatRunContext,
    master: &MasterKey,
    user_id: Uuid,
    session_id: Uuid,
    turn_id: Uuid,
    status: ChatTurnStatus,
    error_row: Option<&str>,
) {
    if let Some(message) = error_row {
        match ctx
            .chat
            .append_message(user_id, master, session_id, ChatRole::Error, message)
            .await
        {
            Ok(seq) => {
                ctx.runs.send_broadcast(
                    turn_id,
                    &ChatSseEvent {
                        kind: ChatSseKind::Error {
                            error: message.to_string(),
                        },
                        seq,
                    },
                );
            }
            Err(e) => {
                tracing::error!(error = %e, turn_id = %turn_id, "chat error-row persist failed");
            }
        }
    }
    if let Err(e) = ctx.chat.finalize_turn(user_id, turn_id, status).await {
        tracing::error!(error = %e, turn_id = %turn_id, "chat turn finalize failed");
    }
    ctx.runs.remove(turn_id);
}

/// Rebuild the LLM conversation from the session's persisted rows
/// (D11): user/assistant rows with `seq <= user_seq` — this turn's
/// trigger message included, as the final turn — keeping the last
/// `MAX_HISTORY_PAIRS` complete pairs. Tool and error rows never feed
/// the model; an assistant row without a pending user (orphan left by
/// a filtered error row) is skipped.
async fn rebuild_history(
    chat: &ChatStore,
    user_id: Uuid,
    master: &MasterKey,
    session_id: Uuid,
    user_seq: i64,
) -> Result<Vec<ConversationTurn>, ChatError> {
    let rows = chat.messages_after(user_id, master, session_id, 0).await?;
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut pending: Option<String> = None;
    for row in rows {
        if row.seq > user_seq {
            break; // rows arrive in seq order; nothing later qualifies
        }
        match row.role {
            ChatRole::User => pending = Some(row.content),
            ChatRole::Assistant => {
                if let Some(user_msg) = pending.take() {
                    pairs.push((user_msg, row.content));
                }
            }
            // Tool/error rows are transcript-only (never model input).
            ChatRole::Tool | ChatRole::Error => {}
        }
    }
    let start = pairs.len().saturating_sub(MAX_HISTORY_PAIRS);
    let mut history = Vec::with_capacity((pairs.len() - start) * 2 + 1);
    for (user_msg, reply) in &pairs[start..] {
        history.push(ConversationTurn::User(user_msg.clone()));
        history.push(ConversationTurn::Assistant(reply.clone()));
    }
    // The trigger message (this turn's user row) closes the history.
    if let Some(user_msg) = pending {
        history.push(ConversationTurn::User(user_msg));
    }
    Ok(history)
}

/// Panic/abort safety for a chat run (Review Focus 2): if the runner
/// task dies without finalizing — a panic anywhere in the loop, or a
/// runtime teardown — Drop finalizes the turn (the same
/// path-dependent rule the loop applies: `failed` fresh, `interrupted`
/// resumed), records the error row, and removes the registry entry.
/// Defused on the normal path: the runner removes the entry first, so
/// `contains` is false when Drop runs and the guard no-ops.
struct RunGuard {
    runs: Arc<ChatRunRegistry>,
    chat: ChatStore,
    master: MasterKey,
    user_id: Uuid,
    session_id: Uuid,
    turn_id: Uuid,
    resumed: bool,
}

impl RunGuard {
    fn new(
        runs: Arc<ChatRunRegistry>,
        chat: ChatStore,
        master: MasterKey,
        user_id: Uuid,
        session_id: Uuid,
        turn_id: Uuid,
        resumed: bool,
    ) -> Self {
        Self {
            runs,
            chat,
            master,
            user_id,
            session_id,
            turn_id,
            resumed,
        }
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        // Defusal check: the normal path already finalized + removed.
        if !self.runs.contains(self.turn_id) {
            return;
        }
        tracing::error!(
            turn_id = %self.turn_id,
            "chat runner dropped before finalizing — drop guard finalizing"
        );
        let status = if self.resumed {
            ChatTurnStatus::Interrupted
        } else {
            ChatTurnStatus::Failed
        };
        // Drop is sync — block on the two async store calls (the
        // search_index.rs `block_on` precedent; sqlx sqlite futures
        // are driven to completion on the current thread). Finalize
        // first (conditional on 'running': if the loop somehow
        // finalized before dying, this no-ops), then the error row,
        // then unregister — the entry is removed regardless so the
        // registry never leaks.
        if let Err(e) =
            futures_executor::block_on(self.chat.finalize_turn(self.user_id, self.turn_id, status))
        {
            tracing::error!(error = %e, turn_id = %self.turn_id, "drop guard finalize failed");
        }
        if let Err(e) = futures_executor::block_on(self.chat.append_message(
            self.user_id,
            &self.master,
            self.session_id,
            ChatRole::Error,
            "the librarian stopped unexpectedly",
        )) {
            tracing::error!(error = %e, turn_id = %self.turn_id, "drop guard error-row append failed");
        }
        self.runs.remove(self.turn_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;

    use mycelium_store::Store;
    use mycelium_store::chat::{ChatRole, ChatStore, ChatTurnStatus};
    use uuid::Uuid;

    /// A user row to satisfy the FK (users belong to the auth layer;
    /// tests insert directly — the chat_store_integration.rs pattern).
    async fn seed_user(store: &Store, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO users (id, username, email, role, auth_provider, password_hash, \
             sealed_master_key, must_change_password, totp_secret, created_at, updated_at) \
             VALUES (?, ?, ?, 'user', 'local', NULL, ?, 0, NULL, ?, ?)",
        )
        .bind(id.to_string())
        .bind(name)
        .bind(format!("{name}@t.local"))
        .bind("placeholder-sealed-master-key")
        .bind(&now)
        .bind(&now)
        .execute(store.pool())
        .await
        .unwrap();
        id
    }

    #[test]
    fn register_subscribe_remove_lifecycle() {
        let reg = ChatRunRegistry::default();
        let turn = Uuid::new_v4();
        let mut rx = reg.register(turn);
        assert!(reg.contains(turn));
        let mut second = reg.subscribe(turn).expect("live entry");
        let ev = ChatSseEvent {
            kind: ChatSseKind::Done {
                reply: "hi".to_string(),
            },
            seq: 3,
        };
        reg.send_broadcast(turn, &ev);
        let got = rx.try_recv().expect("first receiver gets the event");
        assert_eq!(got.seq, 3);
        let got = second.try_recv().expect("second receiver gets the event");
        assert_eq!(got.seq, 3);
        reg.remove(turn);
        assert!(!reg.contains(turn));
        assert!(reg.subscribe(turn).is_none());
        // The sender is gone: receivers see the channel close (this is
        // the reattach stream's close signal).
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Closed)
        ));
    }

    #[test]
    fn lagged_receiver_is_recv_error_not_panic() {
        let reg = ChatRunRegistry::default();
        let turn = Uuid::new_v4();
        let mut slow = reg.register(turn);
        for i in 0..(CHAT_CHANNEL_CAPACITY + 10) {
            reg.send_broadcast(
                turn,
                &ChatSseEvent {
                    kind: ChatSseKind::Tool {
                        name: "x".to_string(),
                        detail: String::new(),
                    },
                    seq: i as i64,
                },
            );
        }
        assert!(matches!(
            slow.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
    }

    #[tokio::test]
    async fn drop_guard_finalizes_on_panic() {
        // Review Focus 2: a runner that panics after registering must
        // leave the turn failed with an error row, and the registry
        // entry removed — the session never wedges.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let user_id = seed_user(&store, "guard").await;
        let master = mycelium_crypto::generate_master_key();
        let chat = ChatStore::new(store.pool().clone());
        let session_id = chat
            .create_session(user_id, &master, "guard test")
            .await
            .unwrap();
        let turn = chat
            .create_turn(user_id, &master, session_id, "hello")
            .await
            .unwrap();
        let turn_id = turn.id;
        let reg = std::sync::Arc::new(ChatRunRegistry::default());
        // The runner registers the turn before spawning its task.
        reg.register(turn_id);
        let (reg2, chat2, master2) = (std::sync::Arc::clone(&reg), chat.clone(), master.clone());
        let handle = tokio::spawn(async move {
            let _guard = RunGuard::new(
                reg2, chat2, master2, user_id, session_id, turn_id, /* resumed */ false,
            );
            panic!("runner explodes");
        });
        assert!(handle.await.is_err(), "task must have panicked");
        assert!(!reg.contains(turn_id));
        // The turn row reads failed (no running turn remains).
        assert!(
            chat.running_turn(user_id, session_id)
                .await
                .unwrap()
                .is_none()
        );
        let latest = chat
            .latest_turn(user_id, session_id)
            .await
            .unwrap()
            .expect("turn row survives");
        assert_eq!(latest.status, ChatTurnStatus::Failed);
        // An error row with the pinned message exists (rows decrypt
        // under the same master key that created them).
        let rows = chat
            .messages_after(user_id, &master, session_id, 0)
            .await
            .unwrap();
        let error_rows: Vec<_> = rows
            .iter()
            .filter(|r| matches!(r.role, ChatRole::Error))
            .collect();
        assert_eq!(error_rows.len(), 1);
        assert_eq!(error_rows[0].content, "the librarian stopped unexpectedly");
    }

    #[test]
    fn sse_event_wire_shape() {
        // Spec §3.3 vocabulary: one pin per event kind, seq always last.
        let session = serde_json::to_value(ChatSseEvent {
            kind: ChatSseKind::Session(Uuid::nil()),
            seq: 0,
        })
        .unwrap();
        assert_eq!(
            session,
            serde_json::json!({
                "type": "session",
                "session_id": Uuid::nil().to_string(),
                "seq": 0
            })
        );
        let tool = serde_json::to_value(ChatSseEvent {
            kind: ChatSseKind::Tool {
                name: "search_knowledge".to_string(),
                detail: "{}".to_string(),
            },
            seq: 2,
        })
        .unwrap();
        assert_eq!(
            tool,
            serde_json::json!({
                "type": "tool",
                "name": "search_knowledge",
                "detail": "{}",
                "seq": 2
            })
        );
        let done = serde_json::to_value(ChatSseEvent {
            kind: ChatSseKind::Done {
                reply: "answer".to_string(),
            },
            seq: 5,
        })
        .unwrap();
        assert_eq!(
            done,
            serde_json::json!({ "type": "done", "reply": "answer", "seq": 5 })
        );
        // Replay vocabulary: an assistant ROW replays as "assistant",
        // never "done" (done means a live turn finished).
        let assistant = serde_json::to_value(ChatSseEvent {
            kind: ChatSseKind::Assistant {
                reply: "answer".to_string(),
            },
            seq: 5,
        })
        .unwrap();
        assert_eq!(
            assistant,
            serde_json::json!({ "type": "assistant", "reply": "answer", "seq": 5 })
        );
        let error = serde_json::to_value(ChatSseEvent {
            kind: ChatSseKind::Error {
                error: "nope".to_string(),
            },
            seq: 6,
        })
        .unwrap();
        assert_eq!(
            error,
            serde_json::json!({ "type": "error", "error": "nope", "seq": 6 })
        );
        // Queue vocabulary (LLM concurrency spec §3.3/§3.4): the two
        // queue events are broadcast-only presentation — never
        // transcript rows — so they carry NO `seq` on the wire (the
        // struct's field is ignored for them, which the `seq: 7` below
        // pins), and chat.js handles them before its seq-dedup gate.
        let queued = serde_json::to_value(ChatSseEvent {
            kind: ChatSseKind::Queued { position: 2 },
            seq: 0,
        })
        .unwrap();
        assert_eq!(
            queued,
            serde_json::json!({ "type": "queued", "position": 2 })
        );
        let admitted = serde_json::to_value(ChatSseEvent {
            kind: ChatSseKind::Admitted,
            seq: 7,
        })
        .unwrap();
        assert_eq!(admitted, serde_json::json!({ "type": "admitted" }));
    }
}
