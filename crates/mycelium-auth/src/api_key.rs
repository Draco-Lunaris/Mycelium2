//! Per-user API keys: opaque random tokens, SHA-256 hashed at rest,
//! shown once at creation. Used for MCP bearer auth.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ApiKeyError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("api key not found or revoked")]
    NotFound,
}

/// A stored API key row.
#[derive(Debug, Clone)]
pub struct ApiKeyRecord {
    pub id: Uuid,
    pub user_id: Uuid,
    pub label: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// A newly minted API key: the plaintext token is shown exactly once.
pub struct MintedKey {
    pub record: ApiKeyRecord,
    pub token: String,
}

/// Raw API key row shape.
type ApiKeyRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

/// API key operations over the store pool.
#[derive(Debug, Clone)]
pub struct ApiKeyManager {
    pool: sqlx::SqlitePool,
}

impl ApiKeyManager {
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }

    /// Mint a new API key for a user. Format: `myc2-<32 hex>` (128 bits).
    pub async fn mint(&self, user_id: Uuid, label: &str) -> Result<MintedKey, ApiKeyError> {
        let token = format!("myc2-{}", hex::encode(random_bytes(16)));
        let hash = hash_token(&token);
        let id = Uuid::new_v4();
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO api_keys (id, user_id, key_hash, label, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(user_id.to_string())
        .bind(&hash)
        .bind(label)
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(MintedKey {
            record: ApiKeyRecord {
                id,
                user_id,
                label: label.to_string(),
                created_at: now,
                last_used_at: None,
                revoked_at: None,
            },
            token,
        })
    }

    /// Resolve a bearer token to its (unrevoked) API key record and update
    /// last_used_at. Returns NotFound for unknown or revoked keys.
    pub async fn verify(&self, token: &str) -> Result<ApiKeyRecord, ApiKeyError> {
        let hash = hash_token(token);
        let row: Option<ApiKeyRow> = sqlx::query_as(
            "SELECT id, user_id, label, created_at, last_used_at, revoked_at
                 FROM api_keys WHERE key_hash = ?",
        )
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await?;
        let Some((id_s, user_s, label, created_s, last_used_s, revoked_s)) = row else {
            return Err(ApiKeyError::NotFound);
        };
        if revoked_s.is_some() {
            return Err(ApiKeyError::NotFound);
        }
        let now = Utc::now().to_rfc3339();
        let _ = sqlx::query("UPDATE api_keys SET last_used_at = ? WHERE id = ?")
            .bind(&now)
            .bind(&id_s)
            .execute(&self.pool)
            .await;
        Ok(ApiKeyRecord {
            id: Uuid::parse_str(&id_s).map_err(|_| ApiKeyError::NotFound)?,
            user_id: Uuid::parse_str(&user_s).map_err(|_| ApiKeyError::NotFound)?,
            label,
            created_at: DateTime::parse_from_rfc3339(&created_s)
                .map_err(|_| ApiKeyError::NotFound)?
                .with_timezone(&Utc),
            last_used_at: last_used_s
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|d| d.with_timezone(&Utc)),
            revoked_at: None,
        })
    }

    /// Revoke an API key (idempotent).
    pub async fn revoke(&self, key_id: Uuid) -> Result<(), ApiKeyError> {
        let now = Utc::now().to_rfc3339();
        sqlx::query("UPDATE api_keys SET revoked_at = COALESCE(revoked_at, ?) WHERE id = ?")
            .bind(&now)
            .bind(key_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// List a user's active API keys (no hashes returned).
    pub async fn list_for_user(&self, user_id: Uuid) -> Result<Vec<ApiKeyRecord>, ApiKeyError> {
        let rows: Vec<ApiKeyRow> = sqlx::query_as(
            "SELECT id, user_id, label, created_at, last_used_at, revoked_at
                 FROM api_keys WHERE user_id = ? ORDER BY created_at",
        )
        .bind(user_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(id_s, user_s, label, created_s, last_used_s, revoked_s)| {
                Ok(ApiKeyRecord {
                    id: Uuid::parse_str(&id_s).map_err(|_| ApiKeyError::NotFound)?,
                    user_id: Uuid::parse_str(&user_s).map_err(|_| ApiKeyError::NotFound)?,
                    label,
                    created_at: DateTime::parse_from_rfc3339(&created_s)
                        .map_err(|_| ApiKeyError::NotFound)?
                        .with_timezone(&Utc),
                    last_used_at: last_used_s
                        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                        .map(|d| d.with_timezone(&Utc)),
                    revoked_at: revoked_s
                        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                        .map(|d| d.with_timezone(&Utc)),
                })
            })
            .collect()
    }
}

/// SHA-256 hash of a token (hex). SHA-256 is appropriate here: tokens are
/// high-entropy random values, not user-chosen passwords.
fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

fn random_bytes(n: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use mycelium_store::Store;

    /// Insert a minimal user row so FK constraints are satisfied.
    async fn seed_user(pool: &sqlx::SqlitePool) -> Uuid {
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO users (id, username, email, role, auth_provider, sealed_master_key, created_at, updated_at)
             VALUES (?, ?, ?, 'user', 'local', '{}', ?, ?)",
        )
        .bind(id.to_string())
        .bind(format!("u-{id}"))
        .bind(format!("u-{id}@t"))
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn mint_verify_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let keys = ApiKeyManager::new(store.pool().clone());
        let user = seed_user(store.pool()).await;
        let minted = keys.mint(user, "test key").await.unwrap();
        assert!(minted.token.starts_with("myc2-"));
        // Verify resolves to the user.
        let record = keys.verify(&minted.token).await.unwrap();
        assert_eq!(record.user_id, user);
        assert_eq!(record.label, "test key");
        // Revoke; verify now fails.
        keys.revoke(record.id).await.unwrap();
        assert!(matches!(
            keys.verify(&minted.token).await,
            Err(ApiKeyError::NotFound)
        ));
    }

    #[tokio::test]
    async fn unknown_token_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let keys = ApiKeyManager::new(store.pool().clone());
        assert!(matches!(
            keys.verify("myc2-doesnotexist").await,
            Err(ApiKeyError::NotFound)
        ));
    }

    #[tokio::test]
    async fn hash_not_stored_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let keys = ApiKeyManager::new(store.pool().clone());
        let user = seed_user(store.pool()).await;
        let minted = keys.mint(user, "l").await.unwrap();
        let (stored,): (String,) = sqlx::query_as("SELECT key_hash FROM api_keys")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_ne!(stored, minted.token);
        assert!(!stored.contains(&minted.token));
    }
}
