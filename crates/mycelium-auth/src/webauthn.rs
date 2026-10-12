//! WebAuthn second-factor enrollment and verification.
//!
//! Second factor ONLY (DESIGN decision): a passkey supplements a password;
//! it is never a passwordless primary. Registration/authentication state is
//! stored server-side in SQLite (single-use, expiring); passkeys persist
//! per user (serializable by design).

use chrono::{DateTime, Utc};
use uuid::Uuid;
use webauthn_rs::prelude::*;

/// The browser's registration response (re-exported so the web layer
/// can accept it as a JSON body without depending on webauthn-rs
/// directly — mycelium-auth owns the whole WebAuthn surface).
pub use webauthn_rs::prelude::RegisterPublicKeyCredential;

#[derive(Debug, thiserror::Error)]
pub enum WebAuthnError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("webauthn error: {0}")]
    WebAuthn(#[from] WebauthnError),
    #[error("user not found")]
    NotFound,
    #[error("challenge not found or expired")]
    ChallengeNotFound,
    #[error("credential already registered")]
    Duplicate,
    #[error("serialization error: {0}")]
    Serde(String),
    #[error("invalid relying-party configuration")]
    BadRpConfig,
}

fn serde_err(e: serde_json::Error) -> WebAuthnError {
    WebAuthnError::Serde(e.to_string())
}

/// A persisted credential row for display (the /account
/// per-credential Remove list): the credential id (base64url — the
/// only identifier; nicknames were never stored) and its creation
/// time. The public-key blob is deliberately not surfaced: the render
/// path never needs key material.
#[derive(Debug, Clone)]
pub struct CredentialRow {
    pub id: String,
    pub created_at: DateTime<Utc>,
}

/// WebAuthn state machine over the store pool.
pub struct WebAuthnManager {
    pool: sqlx::SqlitePool,
    rp: Webauthn,
}

impl WebAuthnManager {
    /// Build a manager for a relying party (origin + id, e.g.
    /// `https://mycelium.example.com` + `mycelium.example.com`).
    pub fn new(
        pool: sqlx::SqlitePool,
        rp_name: &str,
        rp_origin: &str,
        rp_id: &str,
    ) -> Result<Self, WebAuthnError> {
        let origin = url::Url::parse(rp_origin).map_err(|_| WebAuthnError::BadRpConfig)?;
        let _ = rp_name; // rp_name is optional in the builder; kept for API clarity
        let rp = WebauthnBuilder::new(rp_id, &origin)
            .map_err(|_| WebAuthnError::BadRpConfig)?
            .build()
            .map_err(|_| WebAuthnError::BadRpConfig)?;
        Ok(Self { pool, rp })
    }

    /// Build a manager from a full origin URL (e.g.
    /// `https://mycelium.example.com`): the RP id is the origin's DNS
    /// host — the registrable domain a credential binds to, with any
    /// port dropped (ports are not part of an RP id) — and the
    /// allowed origin is the URL as given (port kept; port checks
    /// stay strict, so a ported deployment configures its exact
    /// origin).
    ///
    /// IP-literal hosts are refused (`WebauthnBuilder` requires a
    /// DNS domain — an RP id must be a registrable domain the browser
    /// can bind the credential to): `BadRpConfig`, never a silent
    /// default.
    pub fn from_origin(pool: sqlx::SqlitePool, origin: &str) -> Result<Self, WebAuthnError> {
        let url = url::Url::parse(origin).map_err(|_| WebAuthnError::BadRpConfig)?;
        match url.host() {
            Some(url::Host::Domain(host)) => Self::new(pool, "Mycelium2", origin, host),
            _ => Err(WebAuthnError::BadRpConfig),
        }
    }

    /// Start passkey enrollment: returns the creation challenge (sent to
    /// the browser) and stores the server-side state single-use.
    pub async fn start_enrollment(
        &self,
        user_id: Uuid,
        username: &str,
        display_name: &str,
    ) -> Result<CreationChallengeResponse, WebAuthnError> {
        // Purge expired challenges for this user first.
        let now = Utc::now().to_rfc3339();
        sqlx::query("DELETE FROM webauthn_challenges WHERE user_id = ? AND expires_at < ?")
            .bind(user_id.to_string())
            .bind(&now)
            .execute(&self.pool)
            .await?;
        let (challenge, state) =
            self.rp
                .start_passkey_registration(user_id, username, display_name, None)?;
        let id = Uuid::new_v4();
        let expires = (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
        sqlx::query(
            "INSERT INTO webauthn_challenges (id, user_id, challenge, purpose, created_at, expires_at) VALUES (?, ?, ?, 'register', ?, ?)",
        )
        .bind(id.to_string())
        .bind(user_id.to_string())
        .bind(serde_json::to_vec(&state).map_err(serde_err)?)
        .bind(&now)
        .bind(&expires)
        .execute(&self.pool)
        .await?;
        Ok(challenge)
    }

    /// Complete passkey enrollment: verifies the browser response against
    /// the stored state and persists the passkey. The challenge is consumed
    /// atomically (delete-first) and must not be expired.
    pub async fn finish_enrollment(
        &self,
        user_id: Uuid,
        reg: &RegisterPublicKeyCredential,
    ) -> Result<(), WebAuthnError> {
        // Atomically consume the newest unexpired challenge (delete-first
        // prevents double-use races).
        let state_bytes = self
            .consume_challenge(user_id, "register")
            .await?
            .ok_or(WebAuthnError::ChallengeNotFound)?;
        let state: PasskeyRegistration =
            serde_json::from_slice(&state_bytes).map_err(|_| WebAuthnError::ChallengeNotFound)?;
        let passkey = self.rp.finish_passkey_registration(reg, &state)?;
        // Persist the passkey (serializable Credential).
        let key_bytes = serde_json::to_vec(&passkey).map_err(serde_err)?;
        let cred_id = base64_url::encode(passkey.cred_id().as_slice());
        let res = sqlx::query(
            "INSERT INTO webauthn_credentials (id, user_id, public_key, counter, transports, created_at) VALUES (?, ?, ?, 0, '', ?)",
        )
        .bind(&cred_id)
        .bind(user_id.to_string())
        .bind(&key_bytes)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(e)) if e.message().contains("UNIQUE") => {
                Err(WebAuthnError::Duplicate)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Start second-factor authentication: returns the request challenge
    /// and stores the auth state single-use.
    pub async fn start_auth(
        &self,
        user_id: Uuid,
    ) -> Result<RequestChallengeResponse, WebAuthnError> {
        let passkeys = self.load_passkeys(user_id).await?;
        if passkeys.is_empty() {
            return Err(WebAuthnError::NotFound);
        }
        let (challenge, state) = self.rp.start_passkey_authentication(&passkeys)?;
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        let expires = (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
        sqlx::query(
            "INSERT INTO webauthn_challenges (id, user_id, challenge, purpose, created_at, expires_at) VALUES (?, ?, ?, 'login', ?, ?)",
        )
        .bind(id.to_string())
        .bind(user_id.to_string())
        .bind(serde_json::to_vec(&state).map_err(serde_err)?)
        .bind(&now)
        .bind(&expires)
        .execute(&self.pool)
        .await?;
        Ok(challenge)
    }

    /// Complete second-factor authentication. The challenge is consumed
    /// atomically and must not be expired; the signature counter is
    /// persisted for replay protection.
    pub async fn finish_auth(
        &self,
        user_id: Uuid,
        auth: &PublicKeyCredential,
    ) -> Result<(), WebAuthnError> {
        let state_bytes = self
            .consume_challenge(user_id, "login")
            .await?
            .ok_or(WebAuthnError::ChallengeNotFound)?;
        let state: PasskeyAuthentication =
            serde_json::from_slice(&state_bytes).map_err(|_| WebAuthnError::ChallengeNotFound)?;
        let result = self.rp.finish_passkey_authentication(auth, &state)?;
        // Persist the signature counter for replay protection (cloned-
        // authenticator defense). needs_update() is true when the counter
        // advanced or backup state changed.
        if result.needs_update() {
            let cred_id = base64_url::encode(auth.raw_id.as_slice());
            sqlx::query("UPDATE webauthn_credentials SET counter = ? WHERE id = ?")
                .bind(result.counter())
                .bind(&cred_id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Atomically consume the newest unexpired challenge for a user+purpose:
    /// delete-first so concurrent finishes cannot both read it.
    async fn consume_challenge(
        &self,
        user_id: Uuid,
        purpose: &str,
    ) -> Result<Option<Vec<u8>>, WebAuthnError> {
        let now = Utc::now().to_rfc3339();
        let mut tx = self.pool.begin().await?;
        // Delete the newest unexpired challenge and return its bytes.
        let row: Option<(String, Vec<u8>)> = sqlx::query_as(
            "SELECT id, challenge FROM webauthn_challenges
             WHERE user_id = ? AND purpose = ? AND expires_at > ?
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(user_id.to_string())
        .bind(purpose)
        .bind(&now)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((id, bytes)) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        sqlx::query("DELETE FROM webauthn_challenges WHERE id = ?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(bytes))
    }

    async fn load_passkeys(&self, user_id: Uuid) -> Result<Vec<Passkey>, WebAuthnError> {
        let rows: Vec<(Vec<u8>,)> =
            sqlx::query_as("SELECT public_key FROM webauthn_credentials WHERE user_id = ?")
                .bind(user_id.to_string())
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|(bytes,)| {
                serde_json::from_slice(&bytes).map_err(|e| WebAuthnError::Serde(e.to_string()))
            })
            .collect()
    }

    /// Count a user's enrolled passkeys.
    pub async fn credential_count(&self, user_id: Uuid) -> Result<i64, WebAuthnError> {
        let (n,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM webauthn_credentials WHERE user_id = ?")
                .bind(user_id.to_string())
                .fetch_one(&self.pool)
                .await?;
        Ok(n)
    }

    /// List a user's credentials for the per-credential Remove rows:
    /// oldest first (deterministic order — the id tiebreaks equal
    /// timestamps). Nothing here parses key material.
    pub async fn list_credentials(
        &self,
        user_id: Uuid,
    ) -> Result<Vec<CredentialRow>, WebAuthnError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, created_at FROM webauthn_credentials WHERE user_id = ? ORDER BY created_at ASC, id ASC",
        )
        .bind(user_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(id, created)| {
                let created_at = DateTime::parse_from_rfc3339(&created)
                    .map_err(|_| WebAuthnError::Serde("bad created_at".into()))?
                    .with_timezone(&Utc);
                Ok(CredentialRow { id, created_at })
            })
            .collect()
    }

    /// Remove a passkey (unenroll).
    pub async fn remove_credential(
        &self,
        user_id: Uuid,
        credential_id: &str,
    ) -> Result<(), WebAuthnError> {
        let res = sqlx::query("DELETE FROM webauthn_credentials WHERE user_id = ? AND id = ?")
            .bind(user_id.to_string())
            .bind(credential_id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(WebAuthnError::NotFound);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mycelium_store::Store;
    use webauthn_rs_core::proto::{
        AuthenticatorAttestationResponseRaw, RegistrationExtensionsClientOutputs,
    };

    fn manager(pool: sqlx::SqlitePool) -> WebAuthnManager {
        WebAuthnManager::new(pool, "Mycelium2", "https://localhost", "localhost").unwrap()
    }

    #[tokio::test]
    async fn manager_constructs() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        assert!(
            manager(store.pool().clone())
                .credential_count(Uuid::new_v4())
                .await
                .unwrap()
                == 0
        );
    }

    #[tokio::test]
    async fn start_auth_without_credentials_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let mgr = manager(store.pool().clone());
        assert!(matches!(
            mgr.start_auth(Uuid::new_v4()).await,
            Err(WebAuthnError::NotFound)
        ));
    }

    #[tokio::test]
    async fn finish_enrollment_without_challenge_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let mgr = manager(store.pool().clone());
        // No challenge stored: must fail closed.
        assert!(matches!(
            mgr.finish_enrollment(
                Uuid::new_v4(),
                &RegisterPublicKeyCredential {
                    id: String::new(),
                    raw_id: Base64UrlSafeData::from(Vec::new()),
                    response: AuthenticatorAttestationResponseRaw {
                        attestation_object: Base64UrlSafeData::from(Vec::new()),
                        client_data_json: Base64UrlSafeData::from(Vec::new()),
                        transports: None,
                    },
                    type_: "public-key".to_string(),
                    extensions: RegistrationExtensionsClientOutputs::default(),
                }
            )
            .await,
            Err(WebAuthnError::ChallengeNotFound)
        ));
    }

    /// Minimal user row so FK-protected inserts (challenges,
    /// credentials) have an owner — the totp.rs tests' shape.
    async fn seed_user(pool: &sqlx::SqlitePool, id: Uuid) {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO users (id, username, email, role, auth_provider, sealed_master_key, created_at, updated_at) VALUES (?, 't', 't@t', 'user', 'local', '{}', ?, ?)",
        )
        .bind(id.to_string())
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Task 4 (PR 4): from_origin derives the RP id from the origin's
    /// DNS host (any port dropped) — the challenge it issues carries
    /// exactly that binding, so a browser on that host accepts the
    /// ceremony.
    #[tokio::test]
    async fn from_origin_binds_challenge_to_the_origins_host() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let user = Uuid::new_v4();
        seed_user(store.pool(), user).await;
        let mgr =
            WebAuthnManager::from_origin(store.pool().clone(), "https://mycelium.example.com")
                .unwrap();
        let ch = mgr
            .start_enrollment(user, "claire", "Claire")
            .await
            .unwrap();
        assert_eq!(ch.public_key.rp.id, "mycelium.example.com");
        // A ported origin: the port is not part of the RP id.
        let mgr =
            WebAuthnManager::from_origin(store.pool().clone(), "https://mycelium.example.com:8443")
                .unwrap();
        let ch = mgr
            .start_enrollment(user, "claire", "Claire")
            .await
            .unwrap();
        assert_eq!(ch.public_key.rp.id, "mycelium.example.com");
    }

    /// from_origin refuses IP-literal origins (no registrable domain to
    /// bind the credential to — WebauthnBuilder requires one) and
    /// unparseable strings: BadRpConfig, never a silent default.
    #[tokio::test]
    async fn from_origin_rejects_ip_hosts_and_bad_urls() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        for bad in ["https://127.0.0.1:8443", "not a url"] {
            assert!(
                matches!(
                    WebAuthnManager::from_origin(store.pool().clone(), bad),
                    Err(WebAuthnError::BadRpConfig)
                ),
                "origin {bad:?} must be refused"
            );
        }
    }

    /// list_credentials: (id, created_at) rows, oldest first
    /// (deterministic ordering), empty for a user with none.
    #[tokio::test]
    async fn list_credentials_returns_rows_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let mgr = manager(store.pool().clone());
        let user = Uuid::new_v4();
        seed_user(store.pool(), user).await;
        assert!(mgr.list_credentials(user).await.unwrap().is_empty());
        // Seed out of chronological order — created_at drives the order.
        for (id, stamp) in [
            ("b-cred", "2020-01-01T00:03:20Z"),
            ("a-cred", "2020-01-01T00:01:40Z"),
        ] {
            sqlx::query(
                "INSERT INTO webauthn_credentials (id, user_id, public_key, counter, transports, created_at) VALUES (?, ?, ?, 0, '', ?)",
            )
            .bind(id)
            .bind(user.to_string())
            .bind(b"k" as &[u8])
            .bind(stamp)
            .execute(store.pool())
            .await
            .unwrap();
        }
        let rows = mgr.list_credentials(user).await.unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["a-cred", "b-cred"]);
        let expected = chrono::DateTime::parse_from_rfc3339("2020-01-01T00:01:40Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(rows[0].created_at, expected, "created_at round-trips");
    }
}
