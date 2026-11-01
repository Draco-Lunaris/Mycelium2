//! Health and metrics endpoints.

use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
}

/// The degraded threshold: 2× the drain's age deadline (spec §4.2).
/// Sourced from the worker's limits constant, never a bare literal.
/// Shared by the /health route and the /status page's state chips
/// (PR 4) so both derive "degraded" from one window.
pub(crate) fn oldest_degraded_at_secs() -> i64 {
    2 * mycelium_librarian::queue_worker::QueueLimits::default().age_deadline_secs
}

/// The system-wide values /health reports and the admin System and
/// Librarian status pages surface as StatTiles — ONE computation
/// shared by every call site (the PR-4 extraction of the previously
/// duplicated gathering).
///
/// The field types encode the historical None-rules the call sites
/// relied on: `user_count` is the `-1` sentinel only when the user
/// store fails; ANY queue-store error zeroes the queue fields (the
/// `db_ok` flag already reports that failure); an LLM success
/// timestamp of 0 (never succeeded) reads as `None`.
///
/// `pub` (widened from Task 1's `pub(crate)` in PR 4: the new
/// `pages::librarian_status_page` — a `pub fn` in the pub `pages`
/// module — takes the struct directly; the `system_stats` constructor
/// stays crate-only, so external code can name the type but only the
/// crate produces real values).
///
/// The LLM-gate fields (`llm_*`) come from ONE `LLM_GATE.stats()` read
/// per request (LLM-concurrency spec §3.6), so every consumer of a
/// given `SystemStats` sees one consistent point-in-time gate state.
/// They are observability only: no degraded rule consults them — the
/// gate never degrades health on its own.
///
/// `llm_timeouts_total` is the gate's CUMULATIVE wait-class timeout
/// count since process start (the admin System card's "recent
/// timeouts" readout — labeled there as "since start", because the gate
/// keeps no windowed counter). It rides the same single `stats()` read
/// as the three gauges beside it; it is DELIBERATELY not reported by
/// the `/health` JSON, whose gate fields stay the three spec §4.2
/// gauges.
///
/// The backend-health fields (`backend_*`, outage recovery spec §3.5)
/// come from ONE `LLM_HEALTH.snapshot()` read per request. Like the
/// gate fields they are observability only: no degraded rule consults
/// them — an unhealthy backend is weather the queue and chat surfaces
/// absorb, not illness of THIS service.
///
/// The embedding fields (`embedding_*`, memory-lifecycle §3.3) mirror
/// that posture exactly — one embeddings-snapshot read per request,
/// and again observability only: a red embeddings backend is the same
/// weather, and the vector queue's depth rides `/metrics` alone.
pub struct SystemStats {
    pub user_count: i64,
    pub queue_depth: i64,
    pub oldest_pending_age_seconds: Option<i64>,
    pub queue_dead_count: i64,
    pub llm_last_success_seconds: Option<i64>,
    pub llm_active: i64,
    pub llm_queued: i64,
    pub llm_capacity: i64,
    pub llm_timeouts_total: i64,
    /// The reported backend state name (`BackendState::as_str`:
    /// disabled/green/yellow/red).
    pub backend_state: &'static str,
    /// Seconds since the outage opened (the registry's `down_since`);
    /// `None` while not down — Green clears it, and a Disabled
    /// machine's frozen clock is NOT an active outage.
    pub backend_down_seconds: Option<i64>,
    /// Seconds since the last health observation (probe or model-call
    /// note); `None` when none was ever made. NOT gated on Disabled
    /// like `backend_down_seconds`: a Disabled machine's frozen last
    /// observation passes through as-is — "when health was last
    /// actually checked" is a fact, not an outage claim, so a renderer
    /// must not read it as a live liveness tick.
    pub backend_checked_seconds_ago: Option<i64>,
    /// The reported EMBEDDINGS state name (same mapping as
    /// `backend_state`; memory-lifecycle §3.3's `/health` fields).
    pub embedding_state: &'static str,
    /// Seconds since the embeddings outage opened; `None` while not
    /// down (the same Disabled-frozen-clock rule as
    /// `backend_down_seconds`).
    pub embedding_down_seconds: Option<i64>,
    /// Seconds since the last embeddings observation; `None` when none
    /// was ever made (the same not-gated rule as
    /// `backend_checked_seconds_ago`).
    pub embedding_checked_seconds_ago: Option<i64>,
    /// Turns parked `waiting_backend` or crash-interrupted and never
    /// claimed (`ChatStore::waiting_turn_count`; zeroed on read failure
    /// like the queue fields).
    pub chat_waiting_count: i64,
    pub db_ok: bool,
}

/// Gather the system stats. Pure data gathering — no status decisions
/// here; each caller applies its own degraded rule / rendering.
pub(crate) async fn system_stats(state: &AppState) -> SystemStats {
    let db_ok = sqlx::query("SELECT 1")
        .execute(state.store.pool())
        .await
        .is_ok();
    let user_count = state.users.count().await.unwrap_or(-1);
    // Queue surfaces (zeroed defaults when the store is unreachable —
    // db_ok already reports that failure).
    let (queue_depth, oldest_pending_age_seconds, queue_dead_count) =
        match state.store.queue_health_counts().await {
            Ok(h) => (
                h.depth as i64,
                h.oldest_pending_age_seconds,
                h.dead_count as i64,
            ),
            Err(_) => (0, None, 0),
        };
    let llm = mycelium_librarian::llm::LLM_LAST_SUCCESS.load(std::sync::atomic::Ordering::Relaxed);
    let llm_last_success_seconds = if llm == 0 {
        None
    } else {
        Some(chrono::Utc::now().timestamp() - llm as i64)
    };
    // ONE gate read per request (spec §3.6): a short internal mutex
    // acquisition, no I/O, shared by every field below.
    let gate = mycelium_librarian::llm_gate::LLM_GATE.stats();
    // Backend health (outage recovery spec §3.5): ONE snapshot read,
    // mutex-only, no I/O. Both backend ages derive from the single
    // `now` below (llm_last_success_seconds above reads its own clock;
    // the instants are µs apart). A Disabled machine's down_since is
    // FROZEN (notes are ignored while pinned), not an active outage —
    // disabled is "nothing to probe", so it reads as not-down.
    let now = chrono::Utc::now().timestamp();
    let health = mycelium_librarian::llm_health::LLM_HEALTH.snapshot();
    let backend_down_seconds =
        if health.state == mycelium_librarian::llm_health::BackendState::Disabled {
            None
        } else {
            health.down_since.map(|ts| now - ts as i64)
        };
    let backend_checked_seconds_ago = health.checked_at.map(|ts| now - ts as i64);
    // Embeddings health (memory-lifecycle §3.3): the same pattern, off
    // the ONE snapshot function for the embeddings machine, sharing the
    // same `now`.
    let emb = mycelium_librarian::llm_health::embedding_snapshot();
    let embedding_state = emb.state.as_str();
    let embedding_down_seconds =
        if emb.state == mycelium_librarian::llm_health::BackendState::Disabled {
            None
        } else {
            emb.down_since.map(|ts| now - ts as i64)
        };
    let embedding_checked_seconds_ago = emb.checked_at.map(|ts| now - ts as i64);
    // Chat waiting turns (same zero-on-failure convention as the queue
    // fields: db_ok already reports an unreachable store).
    let chat_waiting_count = state.chat_sessions.waiting_turn_count().await.unwrap_or(0);
    SystemStats {
        user_count,
        queue_depth,
        oldest_pending_age_seconds,
        queue_dead_count,
        llm_last_success_seconds,
        llm_active: gate.active as i64,
        llm_queued: gate.queued as i64,
        llm_capacity: gate.capacity as i64,
        llm_timeouts_total: gate.timeouts_total as i64,
        backend_state: health.state.as_str(),
        backend_down_seconds,
        backend_checked_seconds_ago,
        embedding_state,
        embedding_down_seconds,
        embedding_checked_seconds_ago,
        chat_waiting_count,
        db_ok,
    }
}

/// GET /health — detailed status (DESIGN decision).
///
/// Degraded posture (spec §4.2), DELIBERATELY widened from the old
/// 503-only-on-DB-failure rule: dead queue items or an overdue pending
/// item now also 503 — this is the spec's new contract, not a
/// regression.
///
/// The gate fields (`llm_active`/`llm_queued`/`llm_capacity`) are
/// reported beside the write-queue fields but are DELIBERATELY absent
/// from the degraded rule below (spec §3.6): a saturated or queued gate
/// is load, not ill health. The backend-health and embedding fields
/// (the `embedding_state`/`embedding_down_seconds`/
/// `embedding_checked_seconds_ago` keys) are the same exclusion by
/// design: backend weather is observability, and an unhealthy backend
/// 503s nothing.
async fn health(State(state): State<AppState>) -> Response {
    let stats = system_stats(&state).await;
    let degraded = !stats.db_ok
        || stats.queue_dead_count > 0
        || stats
            .oldest_pending_age_seconds
            .is_some_and(|age| age > oldest_degraded_at_secs());
    let status = if degraded { "degraded" } else { "ok" };
    let code = if degraded {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    } else {
        axum::http::StatusCode::OK
    };
    (
        code,
        axum::Json(serde_json::json!({
            "status": status,
            "database": stats.db_ok,
            "users": stats.user_count,
            "version": env!("CARGO_PKG_VERSION"),
            "queue_depth": stats.queue_depth,
            "oldest_pending_age_seconds": stats.oldest_pending_age_seconds,
            "queue_dead_count": stats.queue_dead_count,
            "llm_last_success_seconds": stats.llm_last_success_seconds,
            "llm_active": stats.llm_active,
            "llm_queued": stats.llm_queued,
            "llm_capacity": stats.llm_capacity,
            "backend_state": stats.backend_state,
            "backend_down_seconds": stats.backend_down_seconds,
            "backend_checked_seconds_ago": stats.backend_checked_seconds_ago,
            "embedding_state": stats.embedding_state,
            "embedding_down_seconds": stats.embedding_down_seconds,
            "embedding_checked_seconds_ago": stats.embedding_checked_seconds_ago,
            "chat_waiting_count": stats.chat_waiting_count,
        })),
    )
        .into_response()
}

/// GET /metrics — Prometheus text format.
async fn metrics(State(state): State<AppState>) -> Response {
    use std::fmt::Write;
    let m = &state.metrics;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# TYPE mycelium2_requests_total counter\nmycelium2_requests_total {}",
        m.requests_total.load(std::sync::atomic::Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_logins_total counter\nmycelium2_logins_total {}",
        m.logins_total.load(std::sync::atomic::Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_logins_failed_total counter\nmycelium2_logins_failed_total {}",
        m.logins_failed.load(std::sync::atomic::Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_concepts_written_total counter\nmycelium2_concepts_written_total {}",
        m.concepts_written
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_concepts_read_total counter\nmycelium2_concepts_read_total {}",
        m.concepts_read.load(std::sync::atomic::Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_searches_total counter\nmycelium2_searches_total {}",
        m.searches_total.load(std::sync::atomic::Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_books_ingested_total counter\nmycelium2_books_ingested_total {}",
        m.books_ingested.load(std::sync::atomic::Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_ingest_failures_total counter\nmycelium2_ingest_failures_total {}",
        m.ingest_failures.load(std::sync::atomic::Ordering::Relaxed)
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_backups_total counter\nmycelium2_backups_total {}",
        m.backups_taken.load(std::sync::atomic::Ordering::Relaxed)
    );
    // Mutation-queue surfaces (spec §4.3). Depth/oldest from the live
    // scan; counters from all-rows SQL (durable across restarts).
    let (depth, oldest, _qdead) = match state.store.queue_health_counts().await {
        Ok(h) => (h.depth, h.oldest_pending_age_seconds, h.dead_count),
        Err(_) => (0, None, 0),
    };
    let _ = writeln!(
        out,
        "# TYPE mycelium2_mutation_queue_depth gauge\nmycelium2_mutation_queue_depth {depth}"
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_mutation_queue_oldest_pending gauge\nmycelium2_mutation_queue_oldest_pending {}",
        oldest.unwrap_or(0)
    );
    let totals = state
        .store
        .queue_totals()
        .await
        .unwrap_or_else(|_| mycelium_store::QueueTotals {
            queued_per_tool: vec![
                (mycelium_store::QueueTool::Add, 0),
                (mycelium_store::QueueTool::Update, 0),
                (mycelium_store::QueueTool::Maintain, 0),
            ],
            integrated: 0,
            fallback: 0,
            dead: 0,
        });
    let _ = writeln!(out, "# TYPE mycelium2_mutation_queued_total counter");
    for (tool, n) in &totals.queued_per_tool {
        let _ = writeln!(
            out,
            "mycelium2_mutation_queued_total{{tool=\"{}\"}} {n}",
            tool.as_str()
        );
    }
    let _ = writeln!(
        out,
        "# TYPE mycelium2_mutation_integrated_total counter\nmycelium2_mutation_integrated_total {}",
        totals.integrated
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_mutation_fallback_total counter\nmycelium2_mutation_fallback_total {}",
        totals.fallback
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_mutation_dead_total counter\nmycelium2_mutation_dead_total {}",
        totals.dead
    );
    // LLM-gate surfaces (LLM-concurrency spec §3.6). ONE gate read per
    // request feeds all seven lines, so the gauges and counters below
    // describe one consistent point in time.
    let gate = mycelium_librarian::llm_gate::LLM_GATE.stats();
    let _ = writeln!(
        out,
        "# TYPE mycelium2_llm_gate_active gauge\nmycelium2_llm_gate_active {}",
        gate.active
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_llm_gate_queued gauge\nmycelium2_llm_gate_queued {}",
        gate.queued
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_llm_gate_capacity gauge\nmycelium2_llm_gate_capacity {}",
        gate.capacity
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_llm_gate_admissions_total counter\nmycelium2_llm_gate_admissions_total {}",
        gate.admissions_total
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_llm_gate_timeouts_total counter\nmycelium2_llm_gate_timeouts_total {}",
        gate.timeouts_total
    );
    // Cumulative queue wait + its sample count: average wait =
    // wait_seconds_total / wait_count. Fixed precision keeps the text
    // deterministic (the crate-wide output rule).
    let _ = writeln!(
        out,
        "# TYPE mycelium2_llm_gate_wait_seconds_total counter\nmycelium2_llm_gate_wait_seconds_total {:.6}",
        gate.wait_seconds_total
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_llm_gate_wait_count counter\nmycelium2_llm_gate_wait_count {}",
        gate.wait_count
    );
    // Backend health (outage recovery spec §3.5). ONE snapshot read
    // feeds both lines. `backend_up` is a four-state gauge:
    // 0=disabled, 1=red, 2=yellow, 3=green. `backend_down_seconds` is
    // 0 when not down — Green clears down_since, and Disabled's frozen
    // clock is "nothing to probe", not an outage.
    let health = mycelium_librarian::llm_health::LLM_HEALTH.snapshot();
    let up = match health.state {
        mycelium_librarian::llm_health::BackendState::Disabled => 0,
        mycelium_librarian::llm_health::BackendState::Red => 1,
        mycelium_librarian::llm_health::BackendState::Yellow => 2,
        mycelium_librarian::llm_health::BackendState::Green => 3,
    };
    let _ = writeln!(
        out,
        "# TYPE mycelium2_backend_up gauge\nmycelium2_backend_up {up}"
    );
    let now = chrono::Utc::now().timestamp();
    let down_seconds = if health.state == mycelium_librarian::llm_health::BackendState::Disabled {
        0
    } else {
        health.down_since.map_or(0, |ts| now - ts as i64)
    };
    let _ = writeln!(
        out,
        "# TYPE mycelium2_backend_down_seconds gauge\nmycelium2_backend_down_seconds {down_seconds}"
    );
    // Embeddings health + the vector drain surfaces (memory-lifecycle
    // §3.3). Same four-state mapping as `backend_up`; the vector
    // queue's own drop reasons are log-only (the queue and the drop
    // warns, not metrics).
    let emb = mycelium_librarian::llm_health::embedding_snapshot();
    let emb_up = match emb.state {
        mycelium_librarian::llm_health::BackendState::Disabled => 0,
        mycelium_librarian::llm_health::BackendState::Red => 1,
        mycelium_librarian::llm_health::BackendState::Yellow => 2,
        mycelium_librarian::llm_health::BackendState::Green => 3,
    };
    let _ = writeln!(
        out,
        "# TYPE mycelium2_embedding_up gauge\nmycelium2_embedding_up {emb_up}"
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_vector_queue_depth gauge\nmycelium2_vector_queue_depth {}",
        mycelium_store::vector_queue_depth()
    );
    let _ = writeln!(
        out,
        "# TYPE mycelium2_vector_processed_total counter\nmycelium2_vector_processed_total {}",
        mycelium_store::VECTOR_PROCESSED_TOTAL.load(std::sync::atomic::Ordering::Relaxed)
    );
    // Chat turns owed an answer (weather-parked + unclaimed crash),
    // zeroed on read failure like the queue surfaces beside it.
    let chat_waiting = state.chat_sessions.waiting_turn_count().await.unwrap_or(0);
    let _ = writeln!(
        out,
        "# TYPE mycelium2_chat_waiting_turns gauge\nmycelium2_chat_waiting_turns {chat_waiting}"
    );
    // Absent until the first LLM success (spec §4.3); 0 means none yet.
    // Model-call-only by design: the health probe never feeds it, so a
    // reachable-but-completion-less backend never looks "recently
    // successful" here.
    let llm = mycelium_librarian::llm::LLM_LAST_SUCCESS.load(std::sync::atomic::Ordering::Relaxed);
    if llm > 0 {
        let _ = writeln!(
            out,
            "# TYPE mycelium2_llm_last_success_timestamp gauge\nmycelium2_llm_last_success_timestamp {llm}"
        );
    }
    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        out,
    )
        .into_response()
}
