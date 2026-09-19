//! TOTP enrollment and verification (optional second factor).
//!
//! Secrets are stored encrypted under the server's service key (AES-256-GCM
//! envelope, hex, `enc1:` prefix) when a service key is available; legacy
//! plaintext rows still verify transparently.

use totp_rs::{Algorithm, Builder, Secret, Totp};
use uuid::Uuid;

use mycelium_crypto::{Dek, ServiceKey, aead_open, aead_seal, derive_service_dek};

const TOTP_AAD: &[u8] = b"totp-secret:v1";
const ENC_PREFIX: &str = "enc1:";

/// Seal a TOTP secret under the service key as `enc1:<hex-envelope>`.
fn seal_secret(secret: &str, key: &Dek) -> String {
    let envelope = aead_seal(secret.as_bytes(), TOTP_AAD, key).expect("aead seal");
    format!("{ENC_PREFIX}{}", hex::encode(envelope))
}

/// Open a stored TOTP secret. `enc1:` rows are decrypted; anything else is
/// treated as legacy plaintext and passed through unchanged.
fn open_secret(stored: &str, key: &Dek) -> Result<String, TotpError> {
    let Some(hex_part) = stored.strip_prefix(ENC_PREFIX) else {
        return Ok(stored.to_string());
    };
    let bytes = hex::decode(hex_part).map_err(|_| TotpError::CorruptSecret)?;
    let plain = aead_open(&bytes, TOTP_AAD, key).map_err(|_| TotpError::CorruptSecret)?;
    String::from_utf8(plain).map_err(|_| TotpError::CorruptSecret)
}

/// The usable secret behind a stored value, or `None` when the row is
/// encrypted and no service key is available (fail closed — the caller
/// rejects the login).
pub(crate) fn stored_secret(
    stored: &str,
    service_key: Option<&ServiceKey>,
) -> Result<Option<String>, TotpError> {
    if stored.starts_with(ENC_PREFIX) {
        let Some(key) = service_key else {
            return Ok(None);
        };
        open_secret(stored, &derive_service_dek(key, TOTP_AAD)).map(Some)
    } else {
        Ok(Some(stored.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TotpError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("user not found")]
    NotFound,
    #[error("invalid TOTP secret stored")]
    CorruptSecret,
    #[error("invalid code")]
    InvalidCode,
}

/// Generate a new TOTP secret (base32 string for authenticator enrollment;
/// stored by the caller via `set_secret`).
pub fn generate_secret() -> String {
    Secret::generate().to_base32()
}

/// The otpauth URL for QR-code enrollment.
pub fn enrollment_url(secret: &str, username: &str, issuer: &str) -> Result<String, TotpError> {
    let totp = make_totp(secret, username, issuer)?;
    totp.to_url().map_err(|_| TotpError::CorruptSecret)
}
fn make_totp(secret: &str, username: &str, issuer: &str) -> Result<Totp, TotpError> {
    let secret = Secret::try_from_base32(secret).map_err(|_| TotpError::CorruptSecret)?;
    Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(6)
        .with_skew(1)
        .with_step_duration(30)
        .with_secret(secret)
        .with_account_name(username)
        .with_issuer(Some(issuer))
        .build()
        .map_err(|_| TotpError::CorruptSecret)
}

/// Build a Totp for verification (account/issuer don't affect code
/// generation, but pass real values for consistency).
pub(crate) fn make_totp_for_verify(secret: &str) -> Result<Totp, TotpError> {
    make_totp(secret, "verify", "Mycelium2")
}

/// Store a user's TOTP secret (enrollment complete). With a service key,
/// the secret is sealed at rest; without one, it is stored as-is (legacy).
pub async fn set_secret(
    pool: &sqlx::SqlitePool,
    user_id: Uuid,
    secret: &str,
    service_key: Option<&ServiceKey>,
) -> Result<(), TotpError> {
    let stored = match service_key {
        Some(key) => seal_secret(secret, &derive_service_dek(key, TOTP_AAD)),
        None => secret.to_string(),
    };
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query("UPDATE users SET totp_secret = ?, updated_at = ? WHERE id = ?")
        .bind(stored)
        .bind(&now)
        .bind(user_id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(TotpError::NotFound);
    }
    Ok(())
}

/// Clear a user's TOTP secret (unenroll).
pub async fn clear_secret(pool: &sqlx::SqlitePool, user_id: Uuid) -> Result<(), TotpError> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query("UPDATE users SET totp_secret = NULL, updated_at = ? WHERE id = ?")
        .bind(&now)
        .bind(user_id.to_string())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(TotpError::NotFound);
    }
    Ok(())
}

/// Verify a 6-digit code against the user's stored secret.
///
/// Users without an enrolled secret pass (TOTP is optional per user).
/// Encrypted (`enc1:`) rows require the service key; legacy plaintext
/// rows verify regardless.
pub async fn verify(
    pool: &sqlx::SqlitePool,
    user_id: Uuid,
    code: &str,
    service_key: Option<&ServiceKey>,
) -> Result<bool, TotpError> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT totp_secret FROM users WHERE id = ?")
            .bind(user_id.to_string())
            .fetch_optional(pool)
            .await?;
    let Some((Some(stored),)) = row else {
        // No secret enrolled: TOTP not required for this user.
        return Ok(true);
    };
    let secret = match stored_secret(&stored, service_key)? {
        Some(s) => s,
        // Encrypted row but no service key: fail closed.
        None => return Ok(false),
    };
    let totp = make_totp(&secret, "", "Mycelium2")?;
    // check_current returns None on mismatch (Option, not Result).
    Ok(totp.check_current(code).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mycelium_crypto::ServiceKey;
    use mycelium_store::Store;

    fn service_key() -> ServiceKey {
        ServiceKey::from_bytes(&[9u8; 32]).unwrap()
    }

    #[tokio::test]
    async fn seal_open_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let pool = store.pool().clone();
        let user_id = Uuid::new_v4();
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO users (id, username, email, role, auth_provider, sealed_master_key, created_at, updated_at)
             VALUES (?, 't', 't@t', 'user', 'local', '{}', ?, ?)",
        )
        .bind(user_id.to_string())
        .bind(&now)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();

        let secret = generate_secret();
        set_secret(&pool, user_id, &secret, Some(&service_key()))
            .await
            .unwrap();

        // Stored encrypted: enc1-prefixed envelope, raw secret absent.
        let (stored,): (String,) = sqlx::query_as("SELECT totp_secret FROM users WHERE id = ?")
            .bind(user_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(stored.starts_with("enc1:"));
        assert!(!stored.contains(&secret));

        // Verification decrypts transparently.
        let totp = make_totp(&secret, "t", "Mycelium2").unwrap();
        let code = totp.generate_current().to_string();
        assert!(
            verify(&pool, user_id, &code, Some(&service_key()))
                .await
                .unwrap()
        );
        assert!(
            !verify(&pool, user_id, "000000", Some(&service_key()))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn legacy_plaintext_secret_still_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let pool = store.pool().clone();
        let user_id = Uuid::new_v4();
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO users (id, username, email, role, auth_provider, sealed_master_key, created_at, updated_at)
             VALUES (?, 'legacy', 'l@t', 'user', 'local', '{}', ?, ?)",
        )
        .bind(user_id.to_string())
        .bind(&now)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();

        // Row written by an older build: plaintext base32 in the column.
        let secret = generate_secret();
        set_secret(&pool, user_id, &secret, None).await.unwrap();
        let (stored,): (String,) = sqlx::query_as("SELECT totp_secret FROM users WHERE id = ?")
            .bind(user_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, secret, "no service key → plaintext fallback");

        // With a service key present, the legacy row still verifies
        // (open_secret passes plaintext through).
        let totp = make_totp(&secret, "l", "Mycelium2").unwrap();
        let code = totp.generate_current().to_string();
        assert!(
            verify(&pool, user_id, &code, Some(&service_key()))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn enroll_and_verify() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let pool = store.pool().clone();
        // Create a user row directly (users module tested separately).
        let user_id = Uuid::new_v4();
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO users (id, username, email, role, auth_provider, sealed_master_key, created_at, updated_at)
             VALUES (?, 't', 't@t', 'user', 'local', '{}', ?, ?)",
        )
        .bind(user_id.to_string())
        .bind(&now)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();

        // Not enrolled: verify passes (TOTP optional).
        assert!(verify(&pool, user_id, "000000", None).await.unwrap());

        // Enroll.
        let secret = generate_secret();
        assert!(!secret.is_empty());
        set_secret(&pool, user_id, &secret, None).await.unwrap();
        let url = enrollment_url(&secret, "t", "Mycelium2").unwrap();
        assert!(url.starts_with("otpauth://"));

        // A valid current code verifies.
        let totp = make_totp(&secret, "t", "Mycelium2").unwrap();
        let code = totp.generate_current().to_string();
        assert!(verify(&pool, user_id, &code, None).await.unwrap());
        // A wrong code fails.
        assert!(!verify(&pool, user_id, "000000", None).await.unwrap());

        // Unenroll.
        clear_secret(&pool, user_id).await.unwrap();
        assert!(verify(&pool, user_id, "000000", None).await.unwrap());
    }
}
