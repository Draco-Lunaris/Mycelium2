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
/// is load, not ill health.
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
    // Absent until the first LLM success (spec §4.3); 0 means none yet.
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
