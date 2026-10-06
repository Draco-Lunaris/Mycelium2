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

/// Derive the purpose-bound DEK used for encrypted config values (the
/// config table contract: sensitive values are encrypted by the caller,
/// keyed off the service key). Shared by every writer/reader of a
/// sealed config value.
pub fn config_dek(service_key: &mycelium_crypto::keys::ServiceKey) -> mycelium_crypto::keys::Dek {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, service_key.as_bytes());
    let mut material = [0u8; 32];
    let _ = hk.expand(b"mycelium2/config-dek/v1", &mut material);
    mycelium_crypto::keys::Dek::from_bytes(&material).expect("32 bytes")
}

/// Seal a config value (JSON + AES-256-GCM, purpose `mycelium2/config-{key}/v1`),
/// returning the hex envelope to store in the config table.
///
/// Panics on a crypto failure: the envelope primitives are infallible for
/// in-memory keys, and a config secret that cannot be sealed must fail
/// the save loudly rather than land plaintext.
pub fn seal_config<T: Serialize>(
    service_key: &mycelium_crypto::keys::ServiceKey,
    key: &str,
    value: &T,
) -> String {
    let json = serde_json::to_vec(value).expect("serialize config value");
    let aad = format!("mycelium2/config-{key}/v1");
    let sealed =
        mycelium_crypto::aead::aead_seal(&json, aad.as_bytes(), &config_dek(service_key))
            .expect("seal config value");
    hex::encode(sealed)
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

    /// The raw column text for `key` (the config table stores every
    /// value JSON-encoded, so this is the JSON-encoded form). Escape
    /// hatch for readers that must distinguish storage shapes — namely
    /// `get_sealed` (a sealed value is a JSON string, a legacy value is
    /// a JSON document); production code should prefer `get`.
    pub async fn raw_get(&self, key: &str) -> Result<Option<String>, ConfigError> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM config WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.0))
    }
}

/// Read + decrypt a sealed config value under `key`; None when absent
/// or undecryptable (wrong key / corrupt row).
///
/// Two storage shapes exist, side by side historically:
/// - **Sealed** (post-llm-api-key): `set(key, &seal_config(...))` — the
///   column is a JSON string wrapping the hex envelope.
/// - **Legacy** (the first releases): `set(key, &T)` — the column is
///   the JSON document itself (the live `{url, model}` LLM rows).
/// New writes are always sealed; legacy rows stay readable.
pub async fn get_sealed<T: DeserializeOwned>(
    config: &ConfigStore,
    service_key: &mycelium_crypto::keys::ServiceKey,
    key: &str,
) -> Option<T> {
    let raw = config.raw_get(key).await.ok().flatten()?;
    // Sealed shape: a JSON string holding hex → decrypt + parse.
    if let Ok(hex_envelope) = serde_json::from_str::<String>(&raw)
        && let Ok(sealed) = hex::decode(&hex_envelope)
    {
        let aad = format!("mycelium2/config-{key}/v1");
        let plain = mycelium_crypto::aead::aead_open(&sealed, aad.as_bytes(), &config_dek(service_key)).ok()?;
        return serde_json::from_slice(&plain).ok();
    }
    // Legacy shape: the column is the JSON document itself.
    serde_json::from_str(&raw).ok()
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

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Sealed {
        url: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key: Option<String>,
    }

    #[tokio::test]
    async fn sealed_round_trip_and_no_plaintext_at_rest() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let config = ConfigStore::new(store.pool().clone());
        let key = mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();

        let value = Sealed {
            url: "http://gateway/v1".into(),
            model: "m".into(),
            api_key: Some("secret-key".into()),
        };
        config.set("llm", &seal_config(&key, "llm", &value)).await.unwrap();
        // The stored row is the hex envelope — the plaintext never lands.
        let (row,): (String,) =
            sqlx::query_as("SELECT value FROM config WHERE key = 'llm'")
                .fetch_one(&store.pool() as &sqlx::Pool<sqlx::Sqlite>)
                .await
                .unwrap();
        assert!(!row.contains("secret-key"));
        assert!(!row.contains("http://gateway"));

        // Round trip.
        assert_eq!(
            get_sealed::<Sealed>(&config, &key, "llm").await,
            Some(value)
        );
    }

    #[tokio::test]
    async fn sealed_wrong_key_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let config = ConfigStore::new(store.pool().clone());
        let key = mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();
        // A different service key (wrong DEK → AEAD open fails).
        use mycelium_crypto::keys::{ServiceKey, generate_master_key};
        let wrong = ServiceKey::from_bytes(generate_master_key().as_bytes()).expect("32 bytes");

        let value = Sealed {
            url: "u".into(),
            model: "m".into(),
            api_key: Some("k".into()),
        };
        config.set("llm", &seal_config(&key, "llm", &value)).await.unwrap();
        assert_eq!(get_sealed::<Sealed>(&config, &wrong, "llm").await, None);
    }

    #[tokio::test]
    async fn legacy_plaintext_row_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let config = ConfigStore::new(store.pool().clone());
        let key = mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();
        // What the early releases wrote: plain JSON, no api_key field.
        config
            .set(
                "llm",
                &serde_json::json!({ "url": "http://backend/v1", "model": "mycelium" }),
            )
            .await
            .unwrap();
        let got = get_sealed::<Sealed>(&config, &key, "llm").await.unwrap();
        assert_eq!(got.url, "http://backend/v1");
        assert_eq!(got.model, "mycelium");
        assert_eq!(got.api_key, None);
    }
}
