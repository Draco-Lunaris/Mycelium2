//! Shared application state: store, auth services, key caches, metrics.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mycelium_auth::login::LoginService;
use mycelium_auth::session::SessionManager;
use mycelium_auth::users::UserStore;
use mycelium_auth::webauthn::WebAuthnManager;
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

/// Security settings, stored in ConfigStore (key `"security"`).
/// Admin-editable within guardrails — every field is clamped to sane
/// bounds on read AND on save, so a compromised admin cannot turn a
/// guardrail off, only tune it within safe limits. Defaults are the
/// project's original values.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SecurityConfig {
    /// Web session TTL in minutes (default 720 = 12h; min 15, max 10080).
    pub session_ttl_minutes: u64,
    /// Login failures before the first throttle deny (default 3; min 3, max 10).
    pub login_max_failures: u32,
    /// Login backoff cap in seconds (default 30; min 30, max 3600).
    pub login_lockout_seconds: u64,
    /// Minimum password length (default 20; min 12, max 128).
    pub min_password_length: usize,
    /// Passage extraction cap in chars (default 131072 = 128k; min 16384, max 1048576).
    pub passage_max_chars: usize,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            session_ttl_minutes: 720,
            login_max_failures: 3,
            login_lockout_seconds: 30,
            min_password_length: 20,
            passage_max_chars: 128 * 1024,
        }
    }
}

/// Guardrail bounds for SecurityConfig (enforced on read and on save).
pub mod security_bounds {
    pub const SESSION_TTL_MIN: u64 = 15;
    pub const SESSION_TTL_MAX: u64 = 10080;
    pub const LOGIN_FAILURES_MIN: u32 = 3;
    pub const LOGIN_FAILURES_MAX: u32 = 10;
    pub const LOCKOUT_SECS_MIN: u64 = 30;
    pub const LOCKOUT_SECS_MAX: u64 = 3600;
    pub const PASSWORD_LEN_MIN: usize = 12;
    pub const PASSWORD_LEN_MAX: usize = 128;
    pub const PASSAGE_CHARS_MIN: usize = 16 * 1024;
    pub const PASSAGE_CHARS_MAX: usize = 1024 * 1024;
}

impl SecurityConfig {
    /// Clamp every field into its guardrail bounds (used on read AND
    /// on save — defense in depth against out-of-range DB values).
    pub fn clamped(mut self) -> Self {
        use security_bounds::*;
        self.session_ttl_minutes = self
            .session_ttl_minutes
            .clamp(SESSION_TTL_MIN, SESSION_TTL_MAX);
        self.login_max_failures = self
            .login_max_failures
            .clamp(LOGIN_FAILURES_MIN, LOGIN_FAILURES_MAX);
        self.login_lockout_seconds = self
            .login_lockout_seconds
            .clamp(LOCKOUT_SECS_MIN, LOCKOUT_SECS_MAX);
        self.min_password_length = self
            .min_password_length
            .clamp(PASSWORD_LEN_MIN, PASSWORD_LEN_MAX);
        self.passage_max_chars = self
            .passage_max_chars
            .clamp(PASSAGE_CHARS_MIN, PASSAGE_CHARS_MAX);
        self
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

    /// The effective security config: ConfigStore value (clamped) or
    /// the default. Clamping on read means even a hand-edited DB row
    /// cannot produce an unsafe value.
    pub async fn security_config(&self) -> SecurityConfig {
        self.config
            .get::<SecurityConfig>("security")
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
            .clamped()
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
    /// The mutation-queue drain worker (Phase 11).
    pub queue_worker: Arc<mycelium_librarian::queue_worker::QueueWorker>,
    /// Assets directory (CSS/JS served from disk).
    pub assets_dir: std::path::PathBuf,
    /// The server's data directory — the root that holds `config/tls/`
    /// (the managed cert pair) and backups. Kept separate from
    /// `assets_dir` rather than derived from it: the two are independent
    /// boot inputs, and reaching the cert dir through an asset path's
    /// parent is fragile.
    pub data_dir: std::path::PathBuf,
    /// WebAuthn ceremony manager — built once per process (Task 4
    /// hoists what Task 3 constructed per request). RP config:
    /// [`webauthn_origin_from_env`].
    pub webauthn: Arc<WebAuthnManager>,
    /// Durable chat transcripts (sealed `chat_*` rows; chat session
    /// history spec §3.1).
    pub chat_sessions: Arc<mycelium_store::chat::ChatStore>,
    /// Live chat runs: per-turn broadcast channels (spec §3.3) — the
    /// submit endpoints register entries before spawning runners;
    /// the runners remove them at finalization.
    pub chat_runs: Arc<crate::chat_runner::ChatRunRegistry>,
    /// The live TLS config handle (None in tests that never serve TLS).
    /// Both values returned by
    /// [`crate::cert::load_or_create_tls_config`] are clones of one
    /// Arc-backed handle, so admin reloads through this Arc swap the
    /// cert the running HTTPS listener serves. `None` means the admin
    /// TLS controls cannot hot-reload (restart required).
    pub tls: Option<Arc<axum_server::tls_rustls::RustlsConfig>>,
}

impl AppState {
    pub fn new(
        store: Store,
        service_key: ServiceKey,
        login: LoginService,
        assets_dir: std::path::PathBuf,
        data_dir: std::path::PathBuf,
        tls: Option<Arc<axum_server::tls_rustls::RustlsConfig>>,
    ) -> Self {
        let pool = store.pool().clone();
        let store = Arc::new(store);
        let config = Arc::new(ConfigStore::new(pool.clone()));
        let service_key =
            Arc::new(ServiceKey::from_bytes(service_key.as_bytes()).expect("32 bytes"));
        let librarian = Arc::new(mycelium_librarian::LibrarianWorker::new(
            Arc::clone(&store),
            Arc::clone(&service_key),
            Arc::clone(&config),
        ));
        // One master-key cache per process: login, API keys, and the
        // queue worker's recovery path all see the same unwrapped keys.
        let master_keys = Arc::new(MasterKeyCache::default());
        // Mutation-queue drain worker (spec: Phase 11). Recovery seam =
        // the shared master-key cache + service key.
        let recovery: Arc<mycelium_librarian::queue_worker::MasterKeyRecovery> = {
            let cache = Arc::clone(&master_keys);
            let s = Arc::clone(&store);
            let k = Arc::clone(&service_key);
            Arc::new(move |uid| {
                let cache = cache.clone();
                let s = s.clone();
                let k = k.clone();
                Box::pin(async move { cache.for_user(&s, &k, uid).await })
            })
        };
        let queue_worker = Arc::new(mycelium_librarian::queue_worker::QueueWorker::new(
            Arc::clone(&store),
            Arc::clone(&config),
            recovery,
            Arc::clone(&service_key),
            mycelium_librarian::queue_worker::QueueLimits::default(),
        ));
        // TOTP secrets encrypt at rest under the service key: attach it to
        // the login service before it is shared out.
        let login = login.with_service_key((*service_key).clone());
        // WebAuthn RP (hoisted — Task-3 review Minor 4): built once per
        // process instead of per request. The origin comes from
        // MYCELIUM2_WEBAUTHN_ORIGIN (see the helper's rationale); a bad
        // explicit value refuses to boot (fail fast, like every other
        // operator-facing boot config — bad bind addresses, bad TLS
        // paths).
        let webauthn = Arc::new(
            WebAuthnManager::from_origin(pool.clone(), &webauthn_origin_from_env()).expect(
                "invalid MYCELIUM2_WEBAUTHN_ORIGIN — must be an https:// URL with a DNS host",
            ),
        );
        // Chat session history (spec §3.3): the sealed-transcript row
        // store and the live-run registry share the pool with the
        // other stores.
        let chat_sessions = Arc::new(mycelium_store::chat::ChatStore::new(pool.clone()));
        let chat_runs = Arc::new(crate::chat_runner::ChatRunRegistry::default());
        Self {
            store,
            users: Arc::new(UserStore::new(pool.clone())),
            sessions: Arc::new(SessionManager::new(pool.clone())),
            config,
            login: Arc::new(login),
            service_key,
            master_keys,
            metrics: Arc::new(Metrics::default()),
            librarian,
            queue_worker,
            assets_dir,
            data_dir,
            webauthn,
            chat_sessions,
            chat_runs,
            tls,
        }
    }

    /// Fetch (or recover) a user's master key (delegates to the cache).
    pub async fn master_key_for(&self, user_id: Uuid) -> Result<MasterKey, sqlx::Error> {
        self.master_keys
            .for_user(&self.store, &self.service_key, user_id)
            .await
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
            &mycelium_crypto::keys::service_seal_dek(&self.service_key),
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

/// Derive the DEK used for encrypted config values (purpose-bound).
pub fn config_dek(service_key: &ServiceKey) -> mycelium_crypto::keys::Dek {
    mycelium_store::config::config_dek(service_key)
}

/// Seal a config value for the config table (hex envelope; JSON +
/// AEAD, purpose `mycelium2/config-{key}/v1`). The web layer uses this
/// for every sensitive admin config (OIDC secret, LLM api_key).
pub fn seal_config<T: serde::Serialize>(service_key: &ServiceKey, key: &str, value: &T) -> String {
    mycelium_store::config::seal_config(service_key, key, value)
}

/// Read + decrypt an encrypted config value (hex envelope) stored under
/// `key`; None when absent or corrupt. Legacy plaintext-JSON rows (pre-
/// sealing) stay readable.
pub async fn decrypt_config<T: serde::de::DeserializeOwned>(
    config: &ConfigStore,
    service_key: &ServiceKey,
    key: &str,
) -> Option<T> {
    mycelium_store::config::get_sealed(config, service_key, key).await
}

/// The WebAuthn relying-party origin: `MYCELIUM2_WEBAUTHN_ORIGIN` (a
/// full origin URL, e.g. `https://mycelium.example.com`), default
/// `https://localhost`.
///
/// Why an env var and not the request Host or the config table:
/// AppState is built once at boot, before any request exists (no Host
/// is available at build time), and `AppState::new` is sync while
/// ConfigStore reads are async — so neither dynamic surface can
/// configure the RP at construction. The env override follows the
/// `MYCELIUM2_SERVICE_KEY` precedent (operator-provided boot config).
/// The default matches the self-signed-cert dev posture (HTTPS on
/// localhost); real deployments set the env var to their origin. The
/// RP id derives from the origin's DNS host (see
/// `WebAuthnManager::from_origin`).
fn webauthn_origin_from_env() -> String {
    match std::env::var("MYCELIUM2_WEBAUTHN_ORIGIN") {
        Ok(origin) => {
            let trimmed = origin.trim().to_string();
            if trimmed.is_empty() {
                "https://localhost".to_string()
            } else {
                trimmed
            }
        }
        Err(_) => "https://localhost".to_string(),
    }
}
