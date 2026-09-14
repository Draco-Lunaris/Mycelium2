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
}

impl McpState {
    /// Fetch (or recover) a user's master key: cache → service-key seal
    /// (mirrors mycelium-web's AppState::master_key_for).
    pub async fn master_key_for(&self, user_id: Uuid) -> Result<MasterKey, sqlx::Error> {
        if let Some(key) = self.master_keys.get(user_id) {
            return Ok(key);
        }
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT master_key_service_sealed FROM users WHERE id = ?")
                .bind(user_id.to_string())
                .fetch_optional(self.store.pool())
                .await?;
        let Some((Some(sealed_hex),)) = row else {
            return Err(sqlx::Error::RowNotFound);
        };
        let sealed_bytes = hex::decode(&sealed_hex)
            .map_err(|_| sqlx::Error::Decode("bad service seal hex".into()))?;
        let master = mycelium_crypto::aead::aead_open(
            &sealed_bytes,
            b"mycelium2/seal/service/v1",
            &service_seal_dek(&self.service_key),
        )
        .map_err(|_| sqlx::Error::Decode("service seal unseal failed".into()))?;
        let key = MasterKey::from_bytes(&master)
            .map_err(|_| sqlx::Error::Decode("bad master key length".into()))?;
        self.master_keys.insert(user_id, key.clone());
        Ok(key)
    }
}

/// Derive the DEK used for service-key seals (purpose-bound; identical to
/// the web layer's derivation so seals interoperate).
fn service_seal_dek(service_key: &ServiceKey) -> mycelium_crypto::keys::Dek {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, service_key.as_bytes());
    let mut material = [0u8; 32];
    let _ = hk.expand(b"mycelium2/service-seal-dek/v1", &mut material);
    mycelium_crypto::keys::Dek::from_bytes(&material).expect("32 bytes")
}
