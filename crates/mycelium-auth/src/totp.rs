//! TOTP enrollment and verification (optional second factor).

use totp_rs::{Algorithm, Builder, Secret, Totp};
use uuid::Uuid;

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

/// Store a user's TOTP secret (enrollment complete).
pub async fn set_secret(
    pool: &sqlx::SqlitePool,
    user_id: Uuid,
    secret: &str,
) -> Result<(), TotpError> {
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query("UPDATE users SET totp_secret = ?, updated_at = ? WHERE id = ?")
        .bind(secret)
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
pub async fn verify(pool: &sqlx::SqlitePool, user_id: Uuid, code: &str) -> Result<bool, TotpError> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT totp_secret FROM users WHERE id = ?")
            .bind(user_id.to_string())
            .fetch_optional(pool)
            .await?;
    let Some((Some(secret),)) = row else {
        // No secret enrolled: TOTP not required for this user.
        return Ok(true);
    };
    let totp = make_totp(&secret, "", "Mycelium2")?;
    // check_current returns None on mismatch (Option, not Result).
    Ok(totp.check_current(code).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mycelium_store::Store;

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
        assert!(verify(&pool, user_id, "000000").await.unwrap());

        // Enroll.
        let secret = generate_secret();
        assert!(!secret.is_empty());
        set_secret(&pool, user_id, &secret).await.unwrap();
        let url = enrollment_url(&secret, "t", "Mycelium2").unwrap();
        assert!(url.starts_with("otpauth://"));

        // A valid current code verifies.
        let totp = make_totp(&secret, "t", "Mycelium2").unwrap();
        let code = totp.generate_current().to_string();
        assert!(verify(&pool, user_id, &code).await.unwrap());
        // A wrong code fails.
        assert!(!verify(&pool, user_id, "000000").await.unwrap());

        // Unenroll.
        clear_secret(&pool, user_id).await.unwrap();
        assert!(verify(&pool, user_id, "000000").await.unwrap());
    }
}
