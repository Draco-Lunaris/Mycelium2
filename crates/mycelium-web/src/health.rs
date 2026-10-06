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
fn oldest_degraded_at_secs() -> i64 {
    2 * mycelium_librarian::queue_worker::QueueLimits::default().age_deadline_secs
}

/// GET /health — detailed status (DESIGN decision).
///
/// Degraded posture (spec §4.2), DELIBERATELY widened from the old
/// 503-only-on-DB-failure rule: dead queue items or an overdue pending
/// item now also 503 — this is the spec's new contract, not a
/// regression.
async fn health(State(state): State<AppState>) -> Response {
    let db_ok = sqlx::query("SELECT 1")
        .execute(state.store.pool())
        .await
        .is_ok();
    let user_count = state.users.count().await.unwrap_or(-1);
    // Queue surfaces (zeroed defaults when the store is unreachable —
    // db_ok already reports that failure).
    let (queue_depth, oldest, dead_count) = match state.store.queue_health_counts().await {
        Ok(h) => (h.depth, h.oldest_pending_age_seconds, h.dead_count),
        Err(_) => (0, None, 0),
    };
    let llm = mycelium_librarian::llm::LLM_LAST_SUCCESS.load(std::sync::atomic::Ordering::Relaxed);
    let llm_secs = if llm == 0 {
        None
    } else {
        Some(chrono::Utc::now().timestamp() - llm as i64)
    };
    let degraded =
        !db_ok || dead_count > 0 || oldest.is_some_and(|age| age > oldest_degraded_at_secs());
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
            "database": db_ok,
            "users": user_count,
            "version": env!("CARGO_PKG_VERSION"),
            "queue_depth": queue_depth,
            "oldest_pending_age_seconds": oldest,
            "queue_dead_count": dead_count,
            "llm_last_success_seconds": llm_secs,
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
