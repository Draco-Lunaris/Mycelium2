//! Embedded storage: SQLite metadata + encrypted file repository.

pub mod concept_store;
pub mod config;
pub mod file_repo;
pub mod migrations;
pub mod models;
pub mod search_index;

pub use concept_store::{ConceptEntry, ConceptScope, ConceptStore, ConceptStoreError};
pub use config::ConfigError;
pub use config::ConfigStore;
pub use file_repo::{FileRepo, FileRepoError, Scope};
pub use migrations::run_migrations;
pub use models::{ApiKey, Book, Bookshelf, IngestJob, IngestStatus, Session, User};
pub use search_index::{EncryptedIndex, IndexError};

use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// An open store: SQLite pool + data-directory layout.
#[derive(Debug, Clone)]
pub struct Store {
    pool: sqlx::SqlitePool,
    /// Root data directory (`/opt/mycelium2/data` in production).
    data_dir: std::path::PathBuf,
}

impl Store {
    /// Open (and migrate) the store under `data_dir`.
    ///
    /// Creates the directory layout if missing:
    /// `config/`, `db/`, `users/`, `library/`, `skills/`, `assets/`.
    /// SQLite runs with WAL and enforced foreign keys.
    pub async fn open(data_dir: &Path) -> Result<Self, StoreError> {
        for sub in ["config", "db", "users", "library", "skills", "assets"] {
            std::fs::create_dir_all(data_dir.join(sub))?;
        }
        let db_path = data_dir.join("db/mycelium2.sqlite3");
        let options = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;
        run_migrations(&pool).await?;
        Ok(Self {
            pool,
            data_dir: data_dir.to_path_buf(),
        })
    }

    pub fn pool(&self) -> &sqlx::SqlitePool {
        &self.pool
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Directory for a user's encrypted bundle files.
    ///
    /// Takes a `Uuid` so unvalidated strings (usernames, OIDC claims) can
    /// never be path-joined into the filesystem.
    pub fn user_dir(&self, user_id: uuid::Uuid) -> std::path::PathBuf {
        self.data_dir.join("users").join(user_id.to_string())
    }

    /// Directory for the shared encrypted library stacks (service key).
    pub fn library_dir(&self) -> std::path::PathBuf {
        self.data_dir.join("library")
    }

    /// Directory for the global skills shelf (service key).
    pub fn skills_dir(&self) -> std::path::PathBuf {
        self.data_dir.join("skills")
    }

    /// Create a bookshelf (admin action). Returns its id.
    pub async fn create_bookshelf(
        &self,
        name: &str,
        is_global_read: bool,
    ) -> Result<uuid::Uuid, StoreError> {
        let id = uuid::Uuid::new_v4();
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO bookshelves (id, name, description, is_global_read, created_at) VALUES (?, ?, '', ?, ?)",
        )
        .bind(id.to_string())
        .bind(name)
        .bind(is_global_read as i64)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List bookshelves: (name, is_global_read).
    pub async fn list_bookshelves(&self) -> Result<Vec<(String, bool)>, StoreError> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT name, is_global_read FROM bookshelves ORDER BY name")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|(n, g)| (n, g != 0)).collect())
    }

    /// Run a closure inside a transaction; rolls back on error.
    pub async fn with_tx<F, T, E>(&self, f: F) -> Result<T, E>
    where
        F: for<'c> FnOnce(&'c mut sqlx::SqliteConnection) -> BoxFuture<'c, Result<T, E>>,
        E: From<sqlx::Error>,
    {
        let mut tx = self.pool.begin().await?;
        let result = f(&mut tx).await?;
        tx.commit().await?;
        Ok(result)
    }
}

/// Alias for the boxed-future type used by [`Store::with_tx`].
pub type BoxFuture<'a, T> = futures_core::future::BoxFuture<'a, T>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn opens_creates_layout_and_migrates() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        for sub in ["config", "db", "users", "library", "skills", "assets"] {
            assert!(dir.path().join(sub).is_dir(), "missing {sub}");
        }
        // Re-open: migrations are idempotent.
        let store2 = Store::open(dir.path()).await.unwrap();
        assert!(store2.pool().acquire().await.is_ok());
        let _ = store;
    }

    #[tokio::test]
    async fn with_tx_commits_and_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        // Commit path.
        store
            .with_tx(|conn| {
                Box::pin(async move {
                    sqlx::query(
                        "INSERT INTO config (key, value, updated_at) VALUES ('a', '1', 'now')",
                    )
                    .execute(&mut *conn)
                    .await?;
                    Ok::<_, sqlx::Error>(())
                })
            })
            .await
            .unwrap();
        // Rollback path (error propagates, insert discarded).
        let res: Result<(), sqlx::Error> = store
            .with_tx(|conn| {
                Box::pin(async move {
                    sqlx::query(
                        "INSERT INTO config (key, value, updated_at) VALUES ('b', '2', 'now')",
                    )
                    .execute(&mut *conn)
                    .await?;
                    Err(sqlx::Error::RowNotFound) // force rollback
                })
            })
            .await;
        assert!(res.is_err());
        let a: (String,) = sqlx::query_as("SELECT value FROM config WHERE key = 'a'")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(a.0, "1");
        let b: Option<(String,)> = sqlx::query_as("SELECT value FROM config WHERE key = 'b'")
            .fetch_optional(store.pool())
            .await
            .unwrap();
        assert!(b.is_none());
    }
}
