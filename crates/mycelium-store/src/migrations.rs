//! sqlx migrations: embedded, automatic on startup.

use sqlx::SqlitePool;

/// Run all pending migrations (embedded at compile time from `migrations/`).
pub async fn run_migrations(pool: &SqlitePool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}
