//! The chat resume worker (chat session history spec §3.4): completes
//! chat turns interrupted by a server restart, each exactly ONCE.
//!
//! Resumable turns only ever appear at boot — the boot sweep (called
//! once by main.rs BEFORE this worker spawns) flips every `running`
//! turn to `interrupted` with `attempts = 0` — so the wake loop is a
//! plain 60 s tick with NO enqueue Notify (the resume supply is not
//! event-driven; the tick absorbs key-recovery latency). Each pass
//! walks resumable turns oldest-first, grouped by user, through three
//! gates before a run is ever spawned:
//!
//! 1. **Key gate** — `master_key_for` (cache → service-key seal
//!    recovery); on error the WHOLE user's turns are skipped this pass
//!    (zero LLM cost; next tick retries).
//! 2. **Capacity gate** — `llm_gate::LLM_GATE.try_enter()` only; on
//!    None the turn is skipped unclaimed (interactive use wins — the
//!    Phase-11 query-priority invariant; nothing is ever held while
//!    waiting: try_enter comes BEFORE the claim).
//! 3. **Claim gate** — `claim_resumable` (the store's atomic
//!    attempts-guard, 0 → 1); on `false` the turn was raced or already
//!    claimed — skipped.
//!
//! Claimed turns run through the SAME detached runner
//! (`spawn_chat_run`, `resumed: true`) registered in the SAME live-run
//! registry the submit endpoints use — a user opening the session
//! mid-resume watches it live. The runner's terminal handling is
//! path-dependent (spec D7): a resumed turn that runs but fails on the
//! LLM finalizes `interrupted` (attempts already 1 → final, no loop);
//! a hard error (step cap — the run genuinely ran) finalizes `failed`
//! + error row.

use std::sync::Arc;
use std::time::Duration;

use mycelium_auth::rbac::SessionUser;
use mycelium_librarian::llm::LlmConfig;
use mycelium_store::chat::{ChatError, ChatTurnRow};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::api::visible_library_slugs;
use crate::chat_runner::{ChatRunContext, spawn_chat_run};
use crate::state::AppState;

/// The periodic resume sweep (spec §4 tunables: 60 s — resumable turns
/// only appear at boot; the tick absorbs key-recovery latency).
const RESUME_TICK_SECS: u64 = 60;

/// The resume worker: a state handle over what one pass needs
/// (`AppState` is `Clone` — stored as the clone; the pass borrows it).
pub struct ChatResumeWorker {
    state: AppState,
}

impl ChatResumeWorker {
    /// Built from the app state (the pass needs the chat store, the
    /// master-key cache, the user store, the config, and the service
    /// key — all behind the `AppState` clone).
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state: (*state).clone(),
        }
    }

    /// The wake loop (the queue-worker shape): tick → one pass; the
    /// shutdown token breaks it. No Notify — resumable turns only
    /// appear at boot (spec §3.4); the main.rs boot sweep has already
    /// run by the time this spawns.
    pub fn spawn(self: Arc<Self>, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(RESUME_TICK_SECS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    _ = shutdown.cancelled() => {
                        // Bounded shutdown, no new guarantee: claimed
                        // turns still in flight finalize via the
                        // runner's own paths; unclaimed interrupted
                        // turns are the boot sweep's business on the
                        // next start (spec §3.4).
                        tracing::info!("chat resume worker stopped");
                        return;
                    }
                }
                if let Err(e) = self.run_pending().await {
                    tracing::error!(error = %e, "chat resume pass failed");
                }
            }
        })
    }

    /// One pass: resumable turns oldest-first → grouped by user → per
    /// user recover the key (error: skip the user) → per turn
    /// try_acquire a permit (None: skip unclaimed) → atomic claim
    /// (false: raced/skipped) → spawn the resumed run. Returns when
    /// every eligible turn this pass decided on is dispatched or
    /// deliberately skipped.
    pub async fn run_pending(&self) -> Result<(), ChatError> {
        let turns = self.state.chat_sessions.resumable_turns().await?;
        if turns.is_empty() {
            return Ok(());
        }
        // Group by user (the pass's user-scoped work — key recovery,
        // visibility — happens once), preserving each user's turns in
        // the query's oldest-first order. A BTreeMap keeps the user
        // iteration deterministic in between passes.
        let mut groups: std::collections::BTreeMap<Uuid, Vec<ChatTurnRow>> =
            std::collections::BTreeMap::new();
        for (user_id, turn) in turns {
            groups.entry(user_id).or_default().push(turn);
        }
        for (user_id, group) in groups {
            // Per-user work (key recovery + visibility) happens at
            // most once per pass even with several queued turns.
            let master = match self.state.master_key_for(user_id).await {
                Ok(m) => m,
                Err(e) => {
                    // Wrong/unrecoverable seal: skip this user's turns
                    // — zero LLM cost, retried next tick (spec §3.4).
                    // Unclaimed turns cost nothing else: the row stays
                    // `interrupted`, visible in the transcript.
                    tracing::warn!(user_id = %user_id, error = %e,
                        "chat resume: master key unrecoverable — user's turns skipped this pass");
                    continue;
                }
            };
            // Visibility is a per-user property: computed ONCE per pass
            // (the runner task must not borrow AppState, so the slug
            // set is resolved here, before the spawn).
            let rec = match self.state.users.get_by_id(user_id).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(user_id = %user_id, error = %e,
                        "chat resume: user record unreadable — turns skipped this pass");
                    continue;
                }
            };
            let session_user = SessionUser {
                user_id,
                username: rec.username.clone(),
                role: rec.role,
            };
            let scopes_visible = visible_library_slugs(&self.state, &session_user).await;
            let llm = crate::state::decrypt_config::<LlmConfig>(
                &self.state.config,
                &self.state.service_key,
                "llm",
            )
            .await
            .unwrap_or_default();
            for turn in group {
                let session_id = turn.session_id;
                // Capacity BEFORE claim (the self-review rule): a
                // claimed turn with no permit coming would sit
                // `running` forever — no drop guard exists for a spawn
                // that never happens. try_enter only — the
                // query-priority invariant: interactive use wins a
                // full pool (and anything already queued wins too);
                // the turn stays interrupted (attempts 0) for a later
                // pass.
                let Some(permit) = mycelium_librarian::llm_gate::LLM_GATE.try_enter() else {
                    continue;
                };
                // The once-guard: exactly the first claimer wins
                // (attempts 0 → 1). `false` = raced (a fresh submit
                // abandoned it) or already resumed — skip.
                let claimed = self
                    .state
                    .chat_sessions
                    .claim_resumable(user_id, session_id, turn.id)
                    .await?;
                if !claimed {
                    drop(permit);
                    continue;
                }
                let ctx = ChatRunContext {
                    store: (*self.state.store).clone(),
                    service_key: (*self.state.service_key).clone(),
                    chat: (*self.state.chat_sessions).clone(),
                    runs: Arc::clone(&self.state.chat_runs),
                    llm: llm.clone(),
                    scopes_visible: scopes_visible.clone(),
                };
                spawn_chat_run(ctx, user_id, master.clone(), session_id, turn, true).await;
                // Held from try_enter to here: the claim + spawn window
                // (both non-blocking awaits). Dropped past the spawn:
                // the resume's permit is a capacity check, not a run
                // permit (queue_worker.rs's per-item hold is the
                // mutation drain's different shape). The detached
                // runner's own admission is the chat surface's, wired
                // where it spawns the run — not here.
                drop(permit);
            }
        }
        Ok(())
    }
}

// Behavior is pinned end-to-end by tests/chat_resume_integration.rs
// (all six spec §6 cases): the gates' atomic shapes are the store's
// own tests (claim once-only) and the agent's (permits, step cap).
