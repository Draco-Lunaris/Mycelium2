//! Password hashing (Argon2id) and policy.

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{CustomizedPasswordHasher, PasswordVerifier};
use argon2::{Argon2, Params};

use crate::MIN_PASSWORD_LEN;

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("password must be at least {0} characters")]
    TooShort(usize),
    #[error("password hashing failed")]
    HashFailed,
    #[error("invalid password hash stored")]
    InvalidHash,
}

/// Enforce the password policy: minimum length only (DESIGN decision).
/// Uses the default minimum (`MIN_PASSWORD_LEN`).
pub fn check_password_policy(password: &str) -> Result<(), PasswordError> {
    check_password_policy_min(password, MIN_PASSWORD_LEN)
}

/// Enforce the password policy with an explicit minimum
/// (admin-configurable; callers clamp to sane bounds).
pub fn check_password_policy_min(password: &str, min_len: usize) -> Result<(), PasswordError> {
    if password.chars().count() < min_len {
        return Err(PasswordError::TooShort(min_len));
    }
    Ok(())
}

/// Hash a password with Argon2id (random 16-byte salt). Returns a PHC string.
pub fn hash_password(password: &str) -> Result<String, PasswordError> {
    let mut salt_bytes = [0u8; 16];
    use rand::RngCore;
    rand::rng().fill_bytes(&mut salt_bytes);
    let hash = Argon2::default()
        .hash_password_customized(
            password.as_bytes(),
            &salt_bytes,
            None, // default algorithm = Argon2id
            None, // default version
            Params::default(),
        )
        .map_err(|_| PasswordError::HashFailed)?;
    Ok(hash.to_string())
}

/// Verify a password against a stored PHC string.
///
/// Constant-time by construction (Argon2 comparison); a malformed stored
/// hash fails closed rather than panicking.
pub fn verify_password(password: &str, stored: &str) -> Result<bool, PasswordError> {
    let parsed = PasswordHash::new(stored).map_err(|_| PasswordError::InvalidHash)?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: &str = "correct horse battery staple";

    #[test]
    fn policy_rejects_short() {
        assert!(check_password_policy("short").is_err());
        assert!(check_password_policy(&"x".repeat(19)).is_err());
        assert!(check_password_policy(&"x".repeat(20)).is_ok());
        assert!(check_password_policy(LONG).is_ok());
    }

    #[test]
    fn hash_and_verify_round_trip() {
        let hash = hash_password(LONG).unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password(LONG, &hash).unwrap());
        assert!(!verify_password("wrong password entirely!!", &hash).unwrap());
    }

    #[test]
    fn malformed_hash_fails_closed() {
        assert!(matches!(
            verify_password(LONG, "garbage"),
            Err(PasswordError::InvalidHash)
        ));
    }

    #[test]
    fn hashes_are_salted() {
        let a = hash_password(LONG).unwrap();
        let b = hash_password(LONG).unwrap();
        assert_ne!(a, b);
    }
}
