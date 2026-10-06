//! MCP 2026-07-28 stateless server: rmcp Streamable HTTP transport with
//! per-user bearer API-key auth. Tools operate on the caller's private
//! encrypted bundle (plus the global skills shelf for skill tools).

pub mod auth;
pub mod handler;
pub mod router;
pub mod seed;
pub mod tools;

pub use router::{mcp_router, mcp_router_with_shutdown};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mycelium_crypto::keys::{MasterKey, ServiceKey};
use mycelium_store::Store;
use uuid::Uuid;

/// Unwrapped master keys keyed by user id (shared by the web and MCP
/// layers so a user's key is unsealed once per process, not once per
/// layer). Populated on first use from the service-key seal. Never
/// serialized, never logged.
#[derive(Default)]
pub struct MasterKeyCache {
    inner: Mutex<HashMap<Uuid, MasterKey>>,
}

impl MasterKeyCache {
    pub fn insert(&self, user_id: Uuid, key: MasterKey) {
        self.inner.lock().unwrap().insert(user_id, key);
    }

    pub fn get(&self, user_id: Uuid) -> Option<MasterKey> {
        self.inner.lock().unwrap().get(&user_id).cloned()
    }

    pub fn remove(&self, user_id: Uuid) {
        self.inner.lock().unwrap().remove(&user_id);
    }

    /// Fetch (or recover) a user's master key: cache → service-key
    /// seal. One body for the web and MCP layers (was duplicated in
    /// McpState::master_key_for and AppState::master_key_for).
    pub async fn for_user(
        &self,
        store: &Store,
        service_key: &ServiceKey,
        user_id: Uuid,
    ) -> Result<MasterKey, sqlx::Error> {
        if let Some(key) = self.get(user_id) {
            return Ok(key);
        }
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT master_key_service_sealed FROM users WHERE id = ?")
                .bind(user_id.to_string())
                .fetch_optional(store.pool())
                .await?;
        let Some((Some(sealed_hex),)) = row else {
            return Err(sqlx::Error::RowNotFound);
        };
        let sealed_bytes = hex::decode(&sealed_hex)
            .map_err(|_| sqlx::Error::Decode("bad service seal hex".into()))?;
        let master = mycelium_crypto::aead::aead_open(
            &sealed_bytes,
            b"mycelium2/seal/service/v1",
            &mycelium_crypto::keys::service_seal_dek(service_key),
        )
        .map_err(|_| sqlx::Error::Decode("service seal unseal failed".into()))?;
        let key = MasterKey::from_bytes(&master)
            .map_err(|_| sqlx::Error::Decode("bad master key length".into()))?;
        self.insert(user_id, key.clone());
        Ok(key)
    }
}

/// Shared state for the MCP server (a lean sibling of the web AppState —
/// no web/frontend concerns, just store + keys + config).
#[derive(Clone)]
pub struct McpState {
    pub store: Arc<Store>,
    pub service_key: Arc<ServiceKey>,
    pub master_keys: Arc<MasterKeyCache>,
    /// Runtime config source (LLM backend for the librarian agent).
    pub config: Arc<mycelium_store::ConfigStore>,
    /// Notify handle for the drain worker (set at construction by
    /// main.rs; the enqueue path fires it so items integrate promptly).
    pub notify: Arc<tokio::sync::Notify>,
    /// Queue depth cap for enqueue_capped (spec §5: 50/user; the drain
    /// worker owns the full QueueLimits — the MCP layer needs only this).
    pub enqueue_depth_cap: u32,
}

impl McpState {
    /// Fetch (or recover) a user's master key (delegates to the shared
    /// cache; kept for the existing tools' call sites).
    pub async fn master_key_for(&self, user_id: Uuid) -> Result<MasterKey, sqlx::Error> {
        self.master_keys
            .for_user(&self.store, &self.service_key, user_id)
            .await
    }
}

#[cfg(test)]
mod master_key_cache_tests {
    use super::*;

    #[tokio::test]
    async fn for_user_roundtrip_and_cache_hit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let service_key =
            mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();
        let master = mycelium_crypto::generate_master_key();
        let user = uuid::Uuid::new_v4();
        // Persist a service seal for the user (the row the method reads).
        let sealed = mycelium_crypto::aead::aead_seal(
            master.as_bytes(),
            b"mycelium2/seal/service/v1",
            &mycelium_crypto::keys::service_seal_dek(&service_key),
        )
        .unwrap();
        sqlx::query("INSERT INTO users (id, username, email, role, auth_provider, password_hash, sealed_master_key, master_key_service_sealed, created_at, updated_at) VALUES (? , 'u', 'u@x', 'user', 'local', '', '', ?, ?, ?)")
            .bind(user.to_string())
            .bind(hex::encode(sealed))
            .bind(chrono::Utc::now().to_rfc3339())
            .bind(chrono::Utc::now().to_rfc3339())
            .execute(store.pool()).await.unwrap();
        let cache = MasterKeyCache::default();
        let key = cache.for_user(&store, &service_key, user).await.unwrap();
        assert_eq!(key.as_bytes(), master.as_bytes());
        // Cache path: remove the row, ask again — must still answer.
        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(user.to_string())
            .execute(store.pool())
            .await
            .unwrap();
        let again = cache.for_user(&store, &service_key, user).await.unwrap();
        assert_eq!(again.as_bytes(), master.as_bytes());
    }
}
