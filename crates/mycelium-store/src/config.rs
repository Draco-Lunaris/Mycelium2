//! Runtime configuration KV store (admin-managed settings in SQLite).

use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::SqlitePool;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("config value for {key} is corrupt: {reason}")]
    Corrupt { key: String, reason: String },
}

/// Typed KV access over the `config` table. Values are JSON.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    pool: SqlitePool,
}

impl ConfigStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Set a config value (JSON-serialized).
    pub async fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<(), ConfigError> {
        let json = serde_json::to_string(value).map_err(|e| ConfigError::Corrupt {
            key: key.to_string(),
            reason: e.to_string(),
        })?;
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO config (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(json)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get a config value (JSON-deserialized); None if unset.
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, ConfigError> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM config WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            None => Ok(None),
            Some((json,)) => {
                serde_json::from_str(&json)
                    .map(Some)
                    .map_err(|e| ConfigError::Corrupt {
                        key: key.to_string(),
                        reason: e.to_string(),
                    })
            }
        }
    }

    /// Delete a config value.
    pub async fn delete(&self, key: &str) -> Result<(), ConfigError> {
        sqlx::query("DELETE FROM config WHERE key = ?")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// List all config keys (sorted).
    pub async fn keys(&self) -> Result<Vec<String>, ConfigError> {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT key FROM config ORDER BY key")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|k| k.0).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    #[tokio::test]
    async fn set_get_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let config = ConfigStore::new(store.pool().clone());

        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Llm {
            url: String,
            model: String,
        }

        // Absent → None.
        assert!(config.get::<Llm>("llm").await.unwrap().is_none());
        // Set → get round-trips.
        let llm = Llm {
            url: "http://localhost:11434".into(),
            model: "test-model".into(),
        };
        config.set("llm", &llm).await.unwrap();
        assert_eq!(config.get::<Llm>("llm").await.unwrap(), Some(llm));
        // Overwrite.
        let llm2 = Llm {
            url: "http://elsewhere".into(),
            model: "other".into(),
        };
        config.set("llm", &llm2).await.unwrap();
        assert_eq!(config.get::<Llm>("llm").await.unwrap(), Some(llm2));
        // Delete → None.
        config.delete("llm").await.unwrap();
        assert!(config.get::<Llm>("llm").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn keys_are_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let config = ConfigStore::new(store.pool().clone());
        config.set("zebra", &1u32).await.unwrap();
        config.set("alpha", &2u32).await.unwrap();
        config.set("middle", &3u32).await.unwrap();
        assert_eq!(
            config.keys().await.unwrap(),
            vec!["alpha", "middle", "zebra"]
        );
    }
}
