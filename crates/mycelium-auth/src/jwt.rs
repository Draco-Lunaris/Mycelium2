//! Ed25519 JWT key management and access tokens.
//!
//! Keys are generated on first run and persisted under the data dir with
//! 0600 permissions (PEM). Access tokens are short-lived; sessions are the
//! primary web auth (SQLite), so JWTs serve API/service use cases.

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use base64::Engine as _;

#[derive(Debug, thiserror::Error)]
pub enum JwtError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("jwt error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("key files are corrupt")]
    CorruptKeys,
}

/// Claims for a Mycelium2 access token.
#[derive(Debug, Serialize, Deserialize)]
pub struct AccessTokenClaims {
    pub sub: String, // user id
    pub role: String,
    pub exp: i64,
    pub iat: i64,
}

/// Ed25519 signing/verification key pair.
#[derive(Clone)]
pub struct JwtKeys {
    encoding: EncodingKey,
    decoding: DecodingKey,
}

impl std::fmt::Debug for JwtKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JwtKeys(<redacted>)")
    }
}

impl JwtKeys {
    /// Load keys from `<data_dir>/config/jwt-ed25519.{pem}`, generating a
    /// fresh pair on first run (0600 perms).
    pub fn load_or_create(data_dir: &std::path::Path) -> Result<Self, JwtError> {
        let dir = data_dir.join("config");
        std::fs::create_dir_all(&dir)?;
        let signing_path = dir.join("jwt-ed25519-private.pem");
        let verify_path = dir.join("jwt-ed25519-public.pem");

        if signing_path.exists() && verify_path.exists() {
            let signing = std::fs::read_to_string(&signing_path)?;
            let verify = std::fs::read_to_string(&verify_path)?;
            return Ok(Self {
                encoding: EncodingKey::from_ed_pem(signing.as_bytes())?,
                decoding: DecodingKey::from_ed_pem(verify.as_bytes())?,
            });
        }

        // Generate a real Ed25519 key pair (ed25519-dalek) and persist as PEM.
        let mut csprng = rand_08::thread_rng();
        let signing_key = ed25519_dalek::SigningKey::generate(&mut csprng);
        let verifying_key: ed25519_dalek::VerifyingKey = signing_key.verifying_key();
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let signing_pem = signing_key
            .to_pkcs8_pem(pkcs8::LineEnding::LF)
            .map_err(|_| JwtError::CorruptKeys)?
            .to_string();
        let verify_pem = public_key_to_pem(verifying_key.as_bytes());
        // Create with 0600 perms atomically (no world-readable window).
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&signing_path)?;
        f.write_all(signing_pem.as_bytes())?;
        drop(f);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&verify_path)?;
        f.write_all(verify_pem.as_bytes())?;
        drop(f);
        Ok(Self {
            encoding: EncodingKey::from_ed_pem(signing_pem.as_bytes())?,
            decoding: DecodingKey::from_ed_pem(verify_pem.as_bytes())?,
        })
    }

    /// Mint an access token for a user (exp in seconds from now). The role
    /// is a typed `Role` — arbitrary strings cannot be minted.
    pub fn mint_access_token(
        &self,
        user_id: Uuid,
        role: crate::rbac::Role,
        expires_in_secs: i64,
    ) -> Result<String, JwtError> {
        let now = chrono::Utc::now().timestamp();
        let claims = AccessTokenClaims {
            sub: user_id.to_string(),
            role: role.as_str().to_string(),
            iat: now,
            exp: now + expires_in_secs,
        };
        Ok(jsonwebtoken::encode(
            &Header::new(Algorithm::EdDSA),
            &claims,
            &self.encoding,
        )?)
    }

    /// Verify and decode an access token. The role claim is validated
    /// against the known set; unknown roles are rejected.
    pub fn verify_access_token(&self, token: &str) -> Result<AccessTokenClaims, JwtError> {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.validate_exp = true;
        let claims = jsonwebtoken::decode::<AccessTokenClaims>(token, &self.decoding, &validation)?;
        if crate::rbac::Role::parse(&claims.claims.role).is_none() {
            // Treat an unknown role as an invalid token (fail closed).
            return Err(JwtError::Jwt(jsonwebtoken::errors::Error::from(
                jsonwebtoken::errors::ErrorKind::InvalidToken,
            )));
        }
        Ok(claims.claims)
    }
}

// Minimal SPKI PEM helper for the Ed25519 public key (the private key uses
// ed25519-dalek's built-in PKCS#8 encoder).
fn public_key_to_pem(key: &[u8; 32]) -> String {
    // SPKI Ed25519 public key: fixed DER prefix + 32-byte key.
    const PREFIX: &[u8] = &[
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let mut der = Vec::with_capacity(PREFIX.len() + key.len());
    der.extend_from_slice(PREFIX);
    der.extend_from_slice(key);
    format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
        base64::engine::general_purpose::STANDARD.encode(der)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_and_verify_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let keys = JwtKeys::load_or_create(dir.path()).unwrap();
        let user = Uuid::new_v4();
        let token = keys
            .mint_access_token(user, crate::rbac::Role::Admin, 60)
            .unwrap();
        let claims = keys.verify_access_token(&token).unwrap();
        assert_eq!(claims.sub, user.to_string());
        assert_eq!(claims.role, "admin");
    }

    #[test]
    fn expired_token_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let keys = JwtKeys::load_or_create(dir.path()).unwrap();
        // Beyond the default 60s leeway.
        let token = keys
            .mint_access_token(Uuid::new_v4(), crate::rbac::Role::User, -120)
            .unwrap();
        assert!(keys.verify_access_token(&token).is_err());
    }

    #[test]
    fn keys_persist_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let keys1 = JwtKeys::load_or_create(dir.path()).unwrap();
        let keys2 = JwtKeys::load_or_create(dir.path()).unwrap();
        let token = keys1
            .mint_access_token(Uuid::new_v4(), crate::rbac::Role::User, 60)
            .unwrap();
        // keys2 can verify keys1's token (same persisted pair).
        assert!(keys2.verify_access_token(&token).is_ok());
    }

    #[test]
    fn debug_is_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let keys = JwtKeys::load_or_create(dir.path()).unwrap();
        assert!(format!("{keys:?}").contains("<redacted>"));
    }
}
