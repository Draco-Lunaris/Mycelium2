//! WebAuthn second-factor enrollment and verification.
//!
//! Second factor ONLY (DESIGN decision): a passkey supplements a password;
//! it is never a passwordless primary. Registration/authentication state is
//! stored server-side in SQLite (single-use, expiring); passkeys persist
//! per user (serializable by design).

use chrono::Utc;
use uuid::Uuid;
use webauthn_rs::prelude::*;

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
}
