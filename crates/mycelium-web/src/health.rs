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

/// GET /health — detailed status (DESIGN decision).
async fn health(State(state): State<AppState>) -> Response {
    let db_ok = sqlx::query("SELECT 1")
        .execute(state.store.pool())
        .await
        .is_ok();
    let user_count = state.users.count().await.unwrap_or(-1);
    let status = if db_ok { "ok" } else { "degraded" };
    let code = if db_ok {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        axum::Json(serde_json::json!({
            "status": status,
            "database": db_ok,
            "users": user_count,
            "version": env!("CARGO_PKG_VERSION"),
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
