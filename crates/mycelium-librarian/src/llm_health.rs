//! Process-global LLM backend-health registry: the one place that
//! decides whether the LLM backend is reachable right now (outage-
//! recovery spec §3.1).
//!
//! The machine tracks four reported states: `Disabled` (no stored `llm`
//! config row — nothing to probe; fresh submits fail fast with the
//! configure message instead of parking), `Green` (last observation ok),
//! `Yellow` (1–2 consecutive failures, or the staleness rule below), and
//! `Red` (≥3 consecutive failures). Two clocks run: `last_observation`
//! (any ok note — probe or model call) and `last_model_success` (the
//! existing [`crate::llm::LLM_LAST_SUCCESS`], real completions only).
//! The staleness rule keeps a flapping backend honest: when the machine
//! would otherwise report Green, runs are attempting
//! (`active + queued > 0` on [`crate::llm_gate::LLM_GATE`]), yet the
//! last *model-call* success is older than [`MODEL_SUCCESS_STALE_SECS`],
//! the reported state is Yellow — reachable plumbing is not the same
//! claim as completions flowing. `down_since` marks the transition into
//! Yellow/Red (the first failure) and clears on Green.
//!
//! Sources: the passive hooks in `llm.rs` (every `LlmClient` completion
//! notes ok at the `LLM_LAST_SUCCESS` choke point or failure when the
//! §3.2 classifier says the request never reached a working model) and
//! the background probe task (`mycelium_web::server::spawn_backend_probe`,
//! feeding [`probe_once`] every [`PROBE_INTERVAL_SECS`]). [`probe_once`]
//! itself is pure: it reports to its caller and never calls `note_*`, so
//! probe results are fed exactly once by whoever runs them.
//!
//! Two machines share this implementation: the chat-LLM health
//! ([`LLM_HEALTH`], the registry described above) and the embeddings
//! health ([`EMBEDDING_HEALTH`]). The machine is shared *by
//! instantiation* — same thresholds, disabled posture and counters;
//! only the inputs differ. The chat machine derives its staleness
//! inputs from the process globals inside [`LlmHealth::state`] /
//! [`LlmHealth::snapshot`]; every other instance feeds its own inputs
//! through the per-instance `_at` seams. For embeddings that means
//! [`EMB_LAST_SUCCESS`] (0 = unknown) and `busy: false` — embedding
//! calls share no wait queue, so the busy-staleness rule is
//! structurally unreachable for that instance.
//!
//! All state lives behind one mutex; nothing here does I/O. The public
//! methods stamp `Utc::now()` internally; the `_at` variants take
//! explicit unix-second clocks so tests pin the transition and
//! staleness rules deterministically.

use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use crate::llm::{LLM_LAST_SUCCESS, LlmConfig};

/// A model-call success older than this (seconds), while runs are
/// attempting, holds a would-be Green back at Yellow (spec §3.1).
pub const MODEL_SUCCESS_STALE_SECS: u64 = 600;
/// Consecutive failures at which the machine reports Red (below it,
/// 1–2 failures are Yellow).
pub const RED_FAILURE_THRESHOLD: u32 = 3;
/// Timeout for one active probe request (a hanging backend trips it —
/// exactly the slot-holding failure mode).
pub const PROBE_TIMEOUT_SECS: u64 = 5;
/// Interval between active probes in the background probe task.
pub const PROBE_INTERVAL_SECS: u64 = 30;

/// The chat-LLM registry (one of the two process-global machines —
/// the other is [`EMBEDDING_HEALTH`]). Surfaces and hooks feed / read
/// this; tests build their own [`LlmHealth::new`] instances instead.
pub static LLM_HEALTH: LazyLock<LlmHealth> = LazyLock::new(LlmHealth::new);

/// The embeddings backend's health machine: the second instance of the
/// shared [`LlmHealth`] implementation. Same thresholds, disabled
/// posture and counters as [`LLM_HEALTH`]; its staleness inputs come
/// from [`EMB_LAST_SUCCESS`] and a structurally false `busy` (see
/// [`embedding_state`]).
pub static EMBEDDING_HEALTH: LazyLock<LlmHealth> = LazyLock::new(LlmHealth::new);

/// Unix seconds of the last successful embedding call; `0` = never
/// observed (unknown) — the embeddings analogue of
/// [`crate::llm::LLM_LAST_SUCCESS`]. Read by [`embedding_state`] /
/// [`embedding_snapshot`]; the embedding call sites write it.
pub static EMB_LAST_SUCCESS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Dimensions of the embeddings backend's vectors once a call has
/// succeeded; `0` = unknown (declared with the machine it belongs to;
/// the embedding call sites fill it once a backend reports one).
pub static EMBEDDING_DIMS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Backend health states (spec §3.1). Grey dot = Disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendState {
    /// No stored `llm` config row — no probe runs, fresh submits fail
    /// fast. Notes are ignored until a config row is seen again.
    Disabled,
    /// The last observation was ok (and the staleness rule does not
    /// apply).
    Green,
    /// 1–2 consecutive failures, or a stale-while-busy Green.
    Yellow,
    /// ≥3 consecutive failures.
    Red,
}

impl BackendState {
    /// Stable wire/metric name.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            BackendState::Disabled => "disabled",
            BackendState::Green => "green",
            BackendState::Yellow => "yellow",
            BackendState::Red => "red",
        }
    }
}

/// A point-in-time read of the registry for `/health`, `/metrics` and
/// the admin system card. All timestamps are unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthSnapshot {
    /// The reported state (staleness rule already applied).
    pub state: BackendState,
    /// When the machine entered Yellow/Red (first failure); `None`
    /// while Green/Disabled.
    pub down_since: Option<u64>,
    /// Timestamp of the most recent ok observation (probe or model
    /// call); `None` if never observed.
    pub last_observation: Option<u64>,
    /// Consecutive failure observations since the last ok.
    pub consecutive_failures: u32,
    /// Timestamp of the most recent health observation (any `note_ok`
    /// / `note_failure` call updates it).
    pub checked_at: Option<u64>,
}

/// Everything the registry owns, under one lock.
struct Inner {
    /// A stored `llm` config row has been seen. `false` pins Disabled.
    configured: bool,
    consecutive_failures: u32,
    down_since: Option<u64>,
    last_observation: Option<u64>,
    checked_at: Option<u64>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            configured: true,
            consecutive_failures: 0,
            down_since: None,
            last_observation: None,
            checked_at: None,
        }
    }
}

impl Inner {
    /// State from the counters alone (no staleness derivation).
    fn base_state(&self) -> BackendState {
        if !self.configured {
            return BackendState::Disabled;
        }
        match self.consecutive_failures {
            0 => BackendState::Green,
            n if n < RED_FAILURE_THRESHOLD => BackendState::Yellow,
            _ => BackendState::Red,
        }
    }

    /// The reported state: the base state, with the staleness rule
    /// applied — but only when the machine would otherwise be Green
    /// (Disabled and Yellow/Red are reported as-is).
    fn effective_state(
        &self,
        now: u64,
        last_model_success: Option<u64>,
        busy: bool,
    ) -> BackendState {
        let base = self.base_state();
        if base == BackendState::Green
            && busy
            && match last_model_success {
                // No model success ever recorded: older than any bound.
                None => true,
                Some(ts) => now.saturating_sub(ts) > MODEL_SUCCESS_STALE_SECS,
            }
        {
            BackendState::Yellow
        } else {
            base
        }
    }
}

/// The backend-health state machine. Cheap to construct for tests; the
/// production instance is [`LLM_HEALTH`].
pub struct LlmHealth {
    inner: Mutex<Inner>,
}

impl Default for LlmHealth {
    fn default() -> Self {
        Self::new()
    }
}

fn unix_now() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

impl LlmHealth {
    /// A fresh, configured, Green registry with no observations yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The registry's lock. Poisoning is recovered rather than
    /// propagated: a panicking observer must not wedge health notes.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The reported state, deriving the staleness inputs from the
    /// process globals: model-call success from
    /// [`crate::llm::LLM_LAST_SUCCESS`], busy-ness from
    /// [`crate::llm_gate::LLM_GATE`] (`active + queued > 0`).
    #[must_use]
    pub fn state(&self) -> BackendState {
        let last_model_success = match LLM_LAST_SUCCESS.load(std::sync::atomic::Ordering::Relaxed) {
            0 => None,
            ts => Some(ts),
        };
        let gate = crate::llm_gate::LLM_GATE.stats();
        let busy = gate.active + gate.queued > 0;
        self.state_at(unix_now(), last_model_success, busy)
    }

    /// Per-instance seam: the production `LLM_HEALTH` reads the
    /// chat-LLM inputs inside `state()`/`snapshot()`; every other
    /// instance (the embeddings health) feeds its own inputs here.
    /// Explicit clocks (deterministic tests).
    #[must_use]
    pub fn state_at(&self, now: u64, last_model_success: Option<u64>, busy: bool) -> BackendState {
        self.lock().effective_state(now, last_model_success, busy)
    }

    /// Point-in-time snapshot; the `state` field matches [`LlmHealth::state`].
    #[must_use]
    pub fn snapshot(&self) -> HealthSnapshot {
        let last_model_success = match LLM_LAST_SUCCESS.load(std::sync::atomic::Ordering::Relaxed) {
            0 => None,
            ts => Some(ts),
        };
        let gate = crate::llm_gate::LLM_GATE.stats();
        let busy = gate.active + gate.queued > 0;
        self.snapshot_at(unix_now(), last_model_success, busy)
    }

    /// Per-instance seam: the production `LLM_HEALTH` reads the
    /// chat-LLM inputs inside `state()`/`snapshot()`; every other
    /// instance (the embeddings health) feeds its own inputs here.
    /// Explicit clocks (deterministic tests); `now` only feeds the
    /// staleness derivation, the stored clocks pass through unchanged.
    #[must_use]
    pub fn snapshot_at(
        &self,
        now: u64,
        last_model_success: Option<u64>,
        busy: bool,
    ) -> HealthSnapshot {
        let inner = self.lock();
        HealthSnapshot {
            state: inner.effective_state(now, last_model_success, busy),
            down_since: inner.down_since,
            last_observation: inner.last_observation,
            consecutive_failures: inner.consecutive_failures,
            checked_at: inner.checked_at,
        }
    }

    /// Record a healthy observation: Green, counters cleared. Ignored
    /// while Disabled (the pin holds until [`LlmHealth::note_configured`]).
    pub fn note_ok(&self) {
        self.note_ok_at(unix_now());
    }

    /// Per-instance seam: the production `LLM_HEALTH` reads the
    /// chat-LLM inputs inside `state()`/`snapshot()`; every other
    /// instance (the embeddings health) feeds its own inputs here.
    /// Explicit clock (deterministic tests).
    pub fn note_ok_at(&self, now: u64) {
        let mut inner = self.lock();
        if !inner.configured {
            return;
        }
        inner.consecutive_failures = 0;
        inner.down_since = None;
        inner.last_observation = Some(now);
        inner.checked_at = Some(now);
    }

    /// Record a failed observation. The first failure (from Green)
    /// sets `down_since`; further failures leave it unchanged.
    /// Ignored while Disabled.
    pub fn note_failure(&self) {
        self.note_failure_at(unix_now());
    }

    /// Per-instance seam: the production `LLM_HEALTH` reads the
    /// chat-LLM inputs inside `state()`/`snapshot()`; every other
    /// instance (the embeddings health) feeds its own inputs here.
    /// Explicit clock (deterministic tests).
    pub fn note_failure_at(&self, now: u64) {
        let mut inner = self.lock();
        if !inner.configured {
            return;
        }
        if inner.consecutive_failures == 0 {
            inner.down_since = Some(now);
        }
        inner.consecutive_failures = inner.consecutive_failures.saturating_add(1);
        inner.checked_at = Some(now);
    }

    /// Pin the machine Disabled (no stored `llm` row): probes do not
    /// run and notes are ignored until [`LlmHealth::note_configured`].
    /// Counters are frozen, not cleared — [`LlmHealth::note_configured`]
    /// starts fresh.
    pub fn set_disabled(&self) {
        self.lock().configured = false;
    }

    /// A stored `llm` config row was seen: lift Disabled and start
    /// fresh (Green, cleared counters — nothing was observed while
    /// pinned). A no-op when already configured.
    pub fn note_configured(&self) {
        let mut inner = self.lock();
        if inner.configured {
            return;
        }
        *inner = Inner::default();
    }

    /// Global-state hygiene hook for integration tests: reset the
    /// registry to a fresh Green. Returns the fresh snapshot *under the
    /// same lock*, so a test can assert the reset without racing
    /// unrelated tests' passive health notes.
    #[doc(hidden)]
    pub fn reset_for_test(&self) -> HealthSnapshot {
        let mut inner = self.lock();
        *inner = Inner::default();
        HealthSnapshot {
            // Fresh Green; no staleness inputs apply to an empty clock.
            state: BackendState::Green,
            down_since: None,
            last_observation: None,
            consecutive_failures: 0,
            checked_at: None,
        }
    }
}

/// The embeddings machine's last-success input: [`EMB_LAST_SUCCESS`]
/// with the `0` (never observed) → `None` mapping.
#[must_use]
fn emb_last_success() -> Option<u64> {
    match EMB_LAST_SUCCESS.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        ts => Some(ts),
    }
}

/// The reported embeddings-backend state: [`EMBEDDING_HEALTH`] fed with
/// its own inputs — [`emb_last_success`] and `busy: false` (embedding
/// calls share no wait queue, so the busy-staleness rule is
/// structurally unreachable for this instance). The chat-LLM globals
/// are never read here.
#[must_use]
pub fn embedding_state() -> BackendState {
    EMBEDDING_HEALTH.state_at(unix_now(), emb_last_success(), false)
}

/// [`embedding_state`] as a full [`HealthSnapshot`] for
/// `/health`-style reads.
#[must_use]
pub fn embedding_snapshot() -> HealthSnapshot {
    EMBEDDING_HEALTH.snapshot_at(unix_now(), emb_last_success(), false)
}

/// One active probe of the backend: `GET {url}/models` with a
/// [`PROBE_TIMEOUT_SECS`] timeout and the configured bearer header.
/// Healthy = any HTTP response except 5xx and 401/403 (404 still means
/// *reachable* per D1; 5xx = backend/router or its upstream, 401/403 =
/// misconfigured credentials — operator-actionable, not weather); a
/// transport error or timeout is unhealthy.
///
/// Deliberately raw `reqwest` (not `LlmClient`) so probe traffic never
/// double-counts through the passive hooks, and **this function itself
/// never calls `note_*`** — it reports to its caller, and the probe
/// task feeds the result exactly once.
#[must_use]
pub async fn probe_once(cfg: &LlmConfig) -> bool {
    let url = format!("{}/models", cfg.url.trim_end_matches('/'));
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(PROBE_TIMEOUT_SECS))
        .build()
    else {
        return false;
    };
    let mut request = client.get(&url);
    if let Some(key) = &cfg.api_key {
        request = request.bearer_auth(key);
    }
    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            !(status >= 500 || status == 401 || status == 403)
        }
        Err(_) => false,
    }
}

/// Cap on error text persisted into `mutation_queue.detail` (and
/// re-served through receipts): LLM status errors embed upstream HTTP
/// bodies, so the drain bounds what it persists.
const DETAIL_CAP: usize = 512;

/// Bound an error's Display text for the `detail` column: byte-safe
/// truncation at the cap, cut on a char boundary, with the marker
/// appended only when something was actually cut. Short errors pass
/// through unchanged.
pub fn short_detail(e: impl std::fmt::Display) -> String {
    let s = e.to_string();
    if s.len() <= DETAIL_CAP {
        return s;
    }
    let mut end = DETAIL_CAP;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated]", &s[..end])
}

/// Serializes the tests that *feed or read the global* `LLM_HEALTH`
/// against every test whose client drives a passive health note:
/// `llm_health::tests::public_wrappers_stamp_the_real_clock` and
/// `probe_refused_is_unhealthy_and_never_feeds_the_machine`,
/// `llm.rs::passive_hooks_feed_health`, `unreachable_endpoint_errors`,
/// `bearer_header_sent_when_key_configured`, `no_header_without_key`,
/// `extract.rs::llm_failure_falls_back`, and the seven client-driving
/// `worker.rs` tests (`submit_run_and_read`, `one_job_per_user_at_a_time`,
/// `different_users_can_submit_concurrently`, `failed_job_records_detail`,
/// `concurrent_runners_cannot_double_run`, `pending_job_runs_on_later_tick`,
/// `spawn_wakes_on_notify` — `run_pending` → `ingest_one` hits the
/// default URL and notes backend-down, or ok on a host with Ollama).
/// Success-path tests only note ok, which no health test's assertion
/// depends on. An async mutex because the guarded tests hold it
/// across awaits; tokio's mutex never poisons, so one failed test cannot
/// wedge the rest.
#[cfg(test)]
pub(crate) static HEALTH_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) async fn health_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    HEALTH_TEST_LOCK.lock().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use std::sync::Arc;

    #[test]
    fn as_str_names_are_stable() {
        assert_eq!(BackendState::Disabled.as_str(), "disabled");
        assert_eq!(BackendState::Green.as_str(), "green");
        assert_eq!(BackendState::Yellow.as_str(), "yellow");
        assert_eq!(BackendState::Red.as_str(), "red");
    }

    /// The transition list from the brief: ok clears, 1–2 failures are
    /// Yellow, ≥3 Red, down_since is set on the first failure only and
    /// cleared on Green, and every note stamps the clocks.
    #[test]
    fn state_machine_transitions() {
        let h = LlmHealth::new();
        let fresh = h.snapshot_at(100, None, false);
        assert_eq!(fresh.state, BackendState::Green);
        assert_eq!(fresh.consecutive_failures, 0);
        assert_eq!(fresh.down_since, None);
        assert_eq!(fresh.last_observation, None);
        assert_eq!(fresh.checked_at, None);

        h.note_failure_at(1000);
        let s = h.snapshot_at(1000, Some(1000), true);
        assert_eq!(s.state, BackendState::Yellow);
        assert_eq!(s.consecutive_failures, 1);
        assert_eq!(s.down_since, Some(1000), "first failure sets down_since");
        assert_eq!(s.checked_at, Some(1000));

        h.note_failure_at(1010);
        let s = h.snapshot_at(1010, Some(1000), true);
        assert_eq!(s.state, BackendState::Yellow, "failures 2 stay Yellow");
        assert_eq!(s.consecutive_failures, 2);
        assert_eq!(
            s.down_since,
            Some(1000),
            "down_since unchanged on re-failure"
        );
        assert_eq!(s.checked_at, Some(1010));

        h.note_failure_at(1020);
        let s = h.snapshot_at(1020, Some(1000), true);
        assert_eq!(s.state, BackendState::Red, "the 3rd failure is Red");
        assert_eq!(s.consecutive_failures, 3);
        assert_eq!(s.down_since, Some(1000));

        // Red + ok -> Green, everything cleared.
        h.note_ok_at(1030);
        let s = h.snapshot_at(1030, Some(1000), true);
        assert_eq!(s.state, BackendState::Green);
        assert_eq!(s.consecutive_failures, 0);
        assert_eq!(s.down_since, None, "Green clears down_since");
        assert_eq!(s.last_observation, Some(1030));
        assert_eq!(s.checked_at, Some(1030));

        // A second Yellow episode re-stamps down_since (fresh transition).
        h.note_failure_at(2000);
        assert_eq!(h.snapshot_at(2000, None, false).down_since, Some(2000));
    }

    /// Disabled pins the state against notes of either kind;
    /// note_configured lifts it back to a fresh Green.
    #[test]
    fn disabled_pins_until_configured() {
        let h = LlmHealth::new();
        h.set_disabled();
        h.note_failure_at(1000);
        h.note_ok_at(1010);
        let s = h.snapshot_at(1020, None, true);
        assert_eq!(s.state, BackendState::Disabled);
        assert_eq!(
            s.consecutive_failures, 0,
            "notes are ignored while Disabled"
        );
        assert_eq!(s.checked_at, None, "ignored notes stamp nothing");

        h.note_configured();
        let s = h.snapshot_at(1030, None, false);
        assert_eq!(
            s.state,
            BackendState::Green,
            "note_configured lifts Disabled"
        );
        assert_eq!(s.consecutive_failures, 0);

        // Lifting is a fresh start, not a replay of pre-disable state.
        let h = LlmHealth::new();
        h.note_failure_at(1000);
        h.note_failure_at(1010);
        h.note_failure_at(1020);
        h.set_disabled();
        h.note_configured();
        let s = h.snapshot_at(1030, None, false);
        assert_eq!(s.state, BackendState::Green);
        assert_eq!(s.consecutive_failures, 0, "stale counters are cleared");
        assert_eq!(s.down_since, None);

        // note_configured on an already-configured machine is a no-op.
        let h = LlmHealth::new();
        h.note_failure_at(1000);
        h.note_configured();
        assert_eq!(
            h.snapshot_at(1010, None, false).state,
            BackendState::Yellow,
            "note_configured only lifts Disabled — it never heals failures"
        );
    }

    /// The staleness rule: Green + busy + last model success older than
    /// the bound -> Yellow. Read-only (nothing is stored), Green-only
    /// (Yellow/Red/Disabled report as-is), and never fires when idle.
    #[test]
    fn staleness_rule() {
        let h = LlmHealth::new();
        let now = 10_000;

        // Fresh (or zero) last_model_success stamps: not stale.
        assert_eq!(h.state_at(now, Some(now), true), BackendState::Green);
        assert_eq!(
            h.state_at(now, Some(now - MODEL_SUCCESS_STALE_SECS), true),
            BackendState::Green,
            "exactly at the bound is not yet older than it"
        );
        assert_eq!(
            h.state_at(now, Some(now - MODEL_SUCCESS_STALE_SECS - 1), true),
            BackendState::Yellow,
            "stale model success while busy downgrades a Green"
        );
        assert_eq!(
            h.state_at(now, None, true),
            BackendState::Yellow,
            "no model success ever, while busy, is stale"
        );
        assert_eq!(
            h.state_at(now, None, false),
            BackendState::Green,
            "an idle machine is never stale — nobody is trying to use it"
        );
        assert_eq!(
            h.state_at(now, Some(now - MODEL_SUCCESS_STALE_SECS - 1), false),
            BackendState::Green,
            "stale but idle: no downgrade"
        );

        // The rule is a read-time view only: counters and clocks are
        // untouched, and a real Green note re-stamps last_observation.
        let s = h.snapshot_at(now, None, true);
        assert_eq!(s.state, BackendState::Yellow);
        assert_eq!(s.consecutive_failures, 0, "staleness stores no failure");
        assert_eq!(s.down_since, None, "staleness does not open an outage");

        // Yellow/Red/Disabled are reported as-is (a fresh model success
        // does not lift them either).
        h.note_failure_at(now);
        assert_eq!(h.state_at(now, Some(now), true), BackendState::Yellow);
        h.note_failure_at(now);
        h.note_failure_at(now);
        assert_eq!(h.state_at(now, Some(now), true), BackendState::Red);
        h.set_disabled();
        assert_eq!(
            h.state_at(now, Some(now), true),
            BackendState::Disabled,
            "Disabled reports as-is regardless of clocks"
        );
    }

    /// The public wrappers stamp the real clock (unix seconds), and the
    /// global static round-trips notes through it. Guarded: a
    /// failure-noting test running concurrently would perturb the
    /// counters asserted here.
    #[tokio::test]
    async fn public_wrappers_stamp_the_real_clock() {
        let _guard = health_test_guard().await;
        LLM_HEALTH.reset_for_test();
        // Pin the staleness input: a concurrent drain worker can
        // transiently hold an LLM_GATE slot, and snapshot() must not
        // read a stale/zero LLM_LAST_SUCCESS as a stale Green.
        LLM_LAST_SUCCESS.store(unix_now(), std::sync::atomic::Ordering::Relaxed);
        let before = unix_now();
        LLM_HEALTH.note_failure();
        let s = LLM_HEALTH.snapshot();
        assert_eq!(s.state, BackendState::Yellow);
        assert_eq!(s.consecutive_failures, 1);
        let stamped = s.checked_at.expect("a note must stamp checked_at");
        assert!(
            stamped >= before && stamped <= unix_now() + 1,
            "checked_at should be ~now in unix seconds, got {stamped} (before {before})"
        );
        assert_eq!(
            s.down_since,
            Some(stamped),
            "first failure stamps down_since"
        );
        assert_eq!(s.last_observation, None, "a failure is not an observation");

        LLM_HEALTH.note_ok();
        let s = LLM_HEALTH.snapshot();
        assert_eq!(s.state, BackendState::Green);
        assert_eq!(s.consecutive_failures, 0);
        assert!(
            s.last_observation.is_some(),
            "an ok note stamps last_observation"
        );

        let fresh = LLM_HEALTH.reset_for_test();
        assert_eq!(fresh.state, BackendState::Green);
        assert_eq!(fresh.consecutive_failures, 0);
        assert_eq!(fresh.checked_at, None, "reset clears the clocks");
    }

    // ---- the second machine (embeddings) ----

    /// The machine is shared by instantiation: `EMBEDDING_HEALTH` is a
    /// second, independent instance — its notes move its counters only,
    /// and its input sentinels start at the unknown `0`.
    #[tokio::test]
    async fn the_two_health_instances_transition_independently() {
        let _guard = health_test_guard().await;
        LLM_HEALTH.reset_for_test();
        let fresh = EMBEDDING_HEALTH.reset_for_test();
        assert_eq!(fresh.state, BackendState::Green);
        assert_eq!(fresh.consecutive_failures, 0);
        assert_eq!(fresh.checked_at, None, "reset clears the clocks");

        LLM_HEALTH.note_failure();
        let llm = LLM_HEALTH.snapshot();
        assert_eq!(llm.state, BackendState::Yellow, "the chat machine moved");
        assert_eq!(llm.consecutive_failures, 1);
        let emb = embedding_snapshot();
        assert_eq!(
            emb.consecutive_failures, 0,
            "the embeddings instance never moves with the chat machine's notes"
        );
        assert_eq!(emb.state, BackendState::Green);
        assert_eq!(emb.down_since, None);

        EMBEDDING_HEALTH.note_failure();
        let emb = embedding_snapshot();
        assert_eq!(
            emb.state,
            BackendState::Yellow,
            "the embeddings instance moves on its own note"
        );
        assert_eq!(emb.consecutive_failures, 1);
        assert!(
            emb.down_since.is_some(),
            "its first failure opens its own outage window"
        );
        assert_eq!(
            LLM_HEALTH.snapshot().consecutive_failures,
            1,
            "the chat machine's counter is unchanged by the embeddings note"
        );

        assert_eq!(
            EMB_LAST_SUCCESS.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "no embedding success recorded: the sentinel reads 0 (unknown)"
        );
        assert_eq!(
            EMBEDDING_DIMS.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "no embedding dims recorded: 0 (unknown)"
        );
    }

    /// The wired embeddings reads feed from `EMB_LAST_SUCCESS` and a
    /// structurally false `busy` — never from the chat-LLM globals.
    /// The divergence is observed through the chat side: a busy gate
    /// with no model success ever recorded is a stale-while-busy Green
    /// (Yellow) for `LLM_HEALTH.state()`, yet `embedding_state()` stays
    /// Green; and a fresh stamp on one global never heals the *other*
    /// machine's stale view (the globals are disjoint, not aliased).
    #[tokio::test]
    async fn embedding_state_uses_the_embedding_inputs_and_never_the_llm_ones() {
        let _guard = health_test_guard().await;
        LLM_HEALTH.reset_for_test();
        EMBEDDING_HEALTH.reset_for_test();
        LLM_LAST_SUCCESS.store(0, std::sync::atomic::Ordering::Relaxed);
        EMB_LAST_SUCCESS.store(0, std::sync::atomic::Ordering::Relaxed);

        // Hold one gate slot so the process gate reads busy. The try
        // class never queues, so this cannot starve anyone; the retries
        // only cover a concurrent drain test transiently holding both
        // slots (drain permits are held for fast-failing calls).
        let mut busy_permit = None;
        for _ in 0..40 {
            if let Some(permit) = crate::llm_gate::LLM_GATE.try_enter() {
                busy_permit = Some(permit);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let _busy =
            busy_permit.expect("a gate slot frees within 200ms (capacity 2, only drain holds any)");

        // Sanity: the staleness rule itself is what the chat side shows.
        let now = unix_now();
        assert_eq!(
            LLM_HEALTH.state_at(now, None, true),
            BackendState::Yellow,
            "busy + no success ever is a stale Green (the rule the chat wiring reads)"
        );
        assert_eq!(
            LLM_HEALTH.state(),
            BackendState::Yellow,
            "the wired chat fn sees LLM_LAST_SUCCESS = 0 (never) + the busy gate"
        );
        assert_eq!(
            embedding_state(),
            BackendState::Green,
            "the wired embeddings fn ignores both chat inputs: unknown success + never-busy stays Green"
        );

        // A fresh embedding success never heals the chat machine's
        // stale view — stamping EMB_LAST_SUCCESS must leave the chat
        // staleness (its own zeroed global, busy gate) exactly Yellow.
        EMB_LAST_SUCCESS.store(now, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(embedding_state(), BackendState::Green);
        assert_eq!(
            LLM_HEALTH.state(),
            BackendState::Yellow,
            "a fresh EMB_LAST_SUCCESS must not reach the chat machine's inputs"
        );

        // And the reverse: a fresh chat stamp heals only the chat
        // machine, while the embedding sentinel stays 0 (unknown).
        LLM_LAST_SUCCESS.store(now, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            LLM_HEALTH.state(),
            BackendState::Green,
            "fresh model success while busy is unstale"
        );
        let s = embedding_snapshot();
        assert_eq!(s.state, BackendState::Green);
        assert_eq!(
            s.consecutive_failures, 0,
            "the embeddings snapshot never saw the chat machine's notes"
        );

        // Leave EMB_LAST_SUCCESS at the 0 sentinel (as found) so the
        // sibling initial-value assertions hold in any test order.
        EMB_LAST_SUCCESS.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    // ---- probe ----

    /// Mock backend answering `/models` with a fixed status, after an
    /// optional delay, echoing the request's Authorization header.
    async fn models_mock(
        status: u16,
        delay: Duration,
    ) -> (String, Arc<tokio::sync::Mutex<Option<String>>>) {
        let state: Arc<tokio::sync::Mutex<Option<String>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let handler_state = Arc::clone(&state);
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let state = Arc::clone(&handler_state);
                async move {
                    let auth = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(|v| v.to_string());
                    *state.lock().await = auth;
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        "probe body",
                    )
                        .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/v1"), state)
    }

    fn cfg_for(url: String, api_key: Option<&str>) -> LlmConfig {
        LlmConfig {
            url,
            model: "m".into(),
            api_key: api_key.map(str::to_string),
            ..Default::default()
        }
    }

    /// D1 reachability table: 200 and 404 are reachable (healthy);
    /// 502 is the backend/upstream down; 401 is misconfigured
    /// credentials.
    #[tokio::test]
    async fn probe_status_table() {
        for (status, healthy) in [(200u16, true), (404, true), (502, false), (401, false)] {
            let (url, _captured) = models_mock(status, Duration::ZERO).await;
            assert_eq!(
                probe_once(&cfg_for(url, None)).await,
                healthy,
                "HTTP {status} must be healthy={healthy}"
            );
        }
    }

    /// A refused connection is unhealthy — and the probe itself must
    /// never touch the health machine (the probe task feeds results;
    /// double-counting would lie about consecutive failures). Guarded
    /// so a passive failure note from another test can't masquerade as
    /// a hook this probe made.
    #[tokio::test]
    async fn probe_refused_is_unhealthy_and_never_feeds_the_machine() {
        let _guard = health_test_guard().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // port now refuses connections

        LLM_HEALTH.reset_for_test();
        // Pin the staleness input (same race as the wrappers test):
        // the Green assertion must be a clean read of the base state.
        LLM_LAST_SUCCESS.store(unix_now(), std::sync::atomic::Ordering::Relaxed);
        assert!(
            !probe_once(&cfg_for(format!("http://{addr}/v1"), None)).await,
            "connection refused must be unhealthy"
        );
        let s = LLM_HEALTH.snapshot();
        assert_eq!(
            s.state,
            BackendState::Green,
            "probe_once must not call note_* — the machine was fed directly"
        );
        assert_eq!(s.consecutive_failures, 0);
    }

    /// A hanging backend trips the 5 s probe timeout: unhealthy, and
    /// within roughly the probe bound (not the 300 s client default).
    #[tokio::test]
    async fn probe_timeout_bounds_a_hanging_backend() {
        // The handler sleeps 6 s; the probe must give up at ~5 s.
        let (url, _captured) = models_mock(200, Duration::from_secs(6)).await;
        let start = std::time::Instant::now();
        let healthy = probe_once(&cfg_for(url, None)).await;
        let elapsed = start.elapsed();
        assert!(!healthy, "a hanging backend is unhealthy");
        assert!(
            elapsed >= Duration::from_millis(4500) && elapsed < Duration::from_secs(9),
            "probe should give up at the {PROBE_TIMEOUT_SECS}s timeout, took {elapsed:?}"
        );
    }

    /// The bearer header rides the probe when configured, and nothing
    /// is sent for a keyless endpoint. A trailing `/` on the config url
    /// is trimmed so the request lands on `/models`, not `//models`.
    #[tokio::test]
    async fn probe_forwards_bearer_and_trims_trailing_slash() {
        let (url, captured) = models_mock(200, Duration::ZERO).await;
        assert!(probe_once(&cfg_for(url.clone(), Some("probe-key"))).await);
        assert_eq!(
            captured.lock().await.as_deref(),
            Some("Bearer probe-key"),
            "the probe must carry the configured bearer"
        );

        let (url, captured) = models_mock(200, Duration::ZERO).await;
        assert!(probe_once(&cfg_for(url, None)).await);
        assert_eq!(
            captured.lock().await.as_deref(),
            None,
            "no Authorization header for a keyless endpoint"
        );

        let (url, _captured) = models_mock(200, Duration::ZERO).await;
        assert!(
            probe_once(&cfg_for(format!("{url}/"), None)).await,
            "a trailing slash on cfg.url must be trimmed, not double-slashed into the path"
        );
    }

    // ---- short_detail (moved from queue_worker) ----

    #[test]
    fn short_detail_caps_byte_safe() {
        // Under the cap: unchanged, no marker.
        assert_eq!(short_detail("boom"), "boom");
        assert_eq!(short_detail("x".repeat(512)), "x".repeat(512));
        // Over the cap (ASCII): cut at the cap + marker.
        let out = short_detail("x".repeat(600));
        assert_eq!(out.len(), 512 + "… [truncated]".len());
        assert!(out.ends_with("[truncated]"));
        // Over the cap (multibyte): the cut backs off to a char
        // boundary — no panic, never past the cap + marker. "€" is
        // 3 bytes, so byte 512 lands mid-char and the back-off loop
        // must move the cut to 510.
        let out = short_detail("€".repeat(200)); // 600 bytes
        assert!(out.ends_with("[truncated]"));
        assert_eq!(out.len(), 510 + "… [truncated]".len());
        assert!(out.starts_with(&"€".repeat(100)));
    }
}
