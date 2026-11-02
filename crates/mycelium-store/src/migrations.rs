//! sqlx migrations: embedded, automatic on startup.

use sqlx::SqlitePool;

/// Run all pending migrations (embedded at compile time from `migrations/`).
///
/// Applied migrations are FROZEN: sqlx stores a checksum per applied
/// version and `run` fails with `MigrateError::VersionMismatch` on any
/// difference — even a `--` comment edit in an .sql that existing
/// databases already applied boots them no more. Wording fixes to a
/// shipped migration belong in the Rust docs that describe it (or a later
/// migration), never in the frozen file.
pub async fn run_migrations(pool: &SqlitePool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}
