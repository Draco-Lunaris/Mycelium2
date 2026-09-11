//! Shared application state: store, auth services, key caches, metrics.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mycelium_auth::login::LoginService;
use mycelium_auth::session::SessionManager;
use mycelium_auth::users::UserStore;
use mycelium_crypto::keys::{MasterKey, ServiceKey};
use mycelium_store::{ConfigStore, Store};
use uuid::Uuid;

/// Unwrapped master keys, keyed by user id — shared with the MCP layer
/// (mycelium_mcp::MasterKeyCache) so a user's key is unsealed once per
/// process. Populated at login (and at bootstrap/creation) by unsealing
/// the service-key seal; the server needs the master key to serve data,
/// and it holds the service key either way. Never serialized, never
/// logged.
pub use mycelium_mcp::MasterKeyCache;

/// Prometheus-style counters (exposed on /metrics).
#[derive(Default)]
pub struct Metrics {
    pub logins_total: AtomicU64,
    pub logins_failed: AtomicU64,
    pub concepts_written: AtomicU64,
    pub concepts_read: AtomicU64,
    pub searches_total: AtomicU64,
    pub requests_total: AtomicU64,
    pub books_ingested: AtomicU64,
    pub ingest_failures: AtomicU64,
    pub backups_taken: AtomicU64,
}

impl Metrics {
    pub fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Upload limits, stored in ConfigStore (key `"upload"`) so they are
/// admin-editable at runtime — no env config (DESIGN: SQLite-stored
/// runtime config). Defaults match the original Mycelium (32 MiB).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UploadConfig {
    /// Maximum accepted book upload, in MiB.
    pub max_book_mib: u64,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self { max_book_mib: 32 }
    }
}

impl AppState {
    /// The effective upload config: ConfigStore value or the default.
    pub async fn upload_config(&self) -> UploadConfig {
        self.config
            .get::<UploadConfig>("upload")
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
    }
}

/// The axum router state.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub users: Arc<UserStore>,
    pub sessions: Arc<SessionManager>,
    pub config: Arc<ConfigStore>,
    pub login: Arc<LoginService>,
    pub service_key: Arc<ServiceKey>,
    pub master_keys: Arc<MasterKeyCache>,
    pub metrics: Arc<Metrics>,
    /// The in-process librarian (book ingest worker).
    pub librarian: Arc<mycelium_librarian::LibrarianWorker>,
    /// Assets directory (CSS/JS served from disk).
    pub assets_dir: std::path::PathBuf,
}

impl AppState {
    pub fn new(
        store: Store,
        service_key: ServiceKey,
        login: LoginService,
        assets_dir: std::path::PathBuf,
    ) -> Self {
        let pool = store.pool().clone();
        let store = Arc::new(store);
        let config = Arc::new(ConfigStore::new(pool.clone()));
        let librarian = Arc::new(mycelium_librarian::LibrarianWorker::new(
            Arc::clone(&store),
            Arc::new(service_key.clone()),
            Arc::clone(&config),
        ));
        Self {
            store,
            users: Arc::new(UserStore::new(pool.clone())),
            sessions: Arc::new(SessionManager::new(pool.clone())),
            config,
            login: Arc::new(login),
            service_key: Arc::new(service_key),
            master_keys: Arc::new(MasterKeyCache::default()),
            metrics: Arc::new(Metrics::default()),
            librarian,
            assets_dir,
        }
    }

    /// Fetch (or recover) a user's master key: cache → service-key seal.
    pub async fn master_key_for(&self, user_id: Uuid) -> Result<MasterKey, sqlx::Error> {
        if let Some(key) = self.master_keys.get(user_id) {
            return Ok(key);
        }
        // Recover from the service-key seal (column added in migration 0002).
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
            &service_dek(&self.service_key),
        )
        .map_err(|_| sqlx::Error::Decode("service seal unseal failed".into()))?;
        let key = MasterKey::from_bytes(&master)
            .map_err(|_| sqlx::Error::Decode("bad master key length".into()))?;
        self.master_keys.insert(user_id, key.clone());
        Ok(key)
    }

    /// Seal a user's master key under the service key and persist it
    /// (called at user creation and login so restarts can recover keys).
    pub async fn persist_service_seal(
        &self,
        user_id: Uuid,
        master_key: &MasterKey,
    ) -> Result<(), sqlx::Error> {
        let sealed = mycelium_crypto::aead::aead_seal(
            master_key.as_bytes(),
            b"mycelium2/seal/service/v1",
            &service_dek(&self.service_key),
        )
        .map_err(|e| sqlx::Error::Decode(format!("seal failed: {e}").into()))?;
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query("UPDATE users SET master_key_service_sealed = ?, updated_at = ? WHERE id = ?")
            .bind(hex::encode(sealed))
            .bind(&now)
            .bind(user_id.to_string())
            .execute(self.store.pool())
            .await?;
        Ok(())
    }
}

/// Derive the DEK used for service-key seals (purpose-bound).
fn service_dek(service_key: &ServiceKey) -> mycelium_crypto::keys::Dek {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, service_key.as_bytes());
    let mut material = [0u8; 32];
    let _ = hk.expand(b"mycelium2/service-seal-dek/v1", &mut material);
    mycelium_crypto::keys::Dek::from_bytes(&material).expect("32 bytes")
}

/// Derive the DEK used for encrypted config values (purpose-bound).
pub fn config_dek(service_key: &ServiceKey) -> mycelium_crypto::keys::Dek {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, service_key.as_bytes());
    let mut material = [0u8; 32];
    let _ = hk.expand(b"mycelium2/config-dek/v1", &mut material);
    mycelium_crypto::keys::Dek::from_bytes(&material).expect("32 bytes")
}

/// Read + decrypt an encrypted config value (hex envelope) stored under
/// `key`; None when absent or corrupt.
pub async fn decrypt_config<T: serde::de::DeserializeOwned>(
    config: &ConfigStore,
    service_key: &ServiceKey,
    key: &str,
) -> Option<T> {
    let sealed_hex: Option<String> = config.get(key).await.ok().flatten();
    let sealed = hex::decode(sealed_hex?).ok()?;
    let aad = format!("mycelium2/config-{key}/v1");
    let plain =
        mycelium_crypto::aead::aead_open(&sealed, aad.as_bytes(), &config_dek(service_key)).ok()?;
    serde_json::from_slice(&plain).ok()
}
