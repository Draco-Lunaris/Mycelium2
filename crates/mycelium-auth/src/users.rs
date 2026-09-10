//! User accounts: creation with key seals, authentication, password change.

use chrono::{DateTime, Utc};
use mycelium_crypto::seal::{
    SealedMasterKey, change_password as reseal_password, seal_master_key, unseal_with_password,
};
use mycelium_crypto::{MasterKey, generate_master_key, generate_recovery_key};
use uuid::Uuid;

use crate::password::{hash_password, verify_password};
use crate::rbac::Role;

#[derive(Debug, thiserror::Error)]
pub enum UsersError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("username or email already exists")]
    Duplicate,
    #[error("user not found")]
    NotFound,
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("password policy: {0}")]
    Password(#[from] crate::password::PasswordError),
    #[error("crypto error: {0}")]
    Crypto(#[from] mycelium_crypto::CryptoError),
    #[error("stored seal record is corrupt")]
    CorruptSeal,
}

/// Raw user row shape shared by get_by_username / get_by_id.
type UserRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    i64,
    Option<String>,
    String,
);

/// A user row (auth-relevant fields).
#[derive(Debug, Clone)]
pub struct UserRecord {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub role: Role,
    pub auth_provider: String,
    pub password_hash: Option<String>,
    pub sealed_master_key: String,
    pub must_change_password: bool,
    pub totp_secret: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Result of a successful local login: the user plus their unwrapped
/// master key (held only for the request lifetime).
pub struct AuthenticatedUser {
    pub record: UserRecord,
    pub master_key: MasterKey,
}

/// User account operations over the store pool.
#[derive(Debug, Clone)]
pub struct UserStore {
    pool: sqlx::SqlitePool,
}

/// A newly created local user: the recovery key is shown once. The master
/// key is exposed so the caller can persist a service-key seal (the web
/// layer does this at creation so restarts can serve the user's data).
pub struct CreatedUser {
    pub record: UserRecord,
    pub recovery_key: String,
    pub master_key: MasterKey,
}

impl UserStore {
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }

    /// Create a local user with a fresh master key sealed under the
    /// password and a generated recovery key. Caller must persist the
    /// recovery key nowhere — it is shown to the user exactly once.
    pub async fn create_local(
        &self,
        username: &str,
        email: &str,
        password: &str,
        role: Role,
    ) -> Result<CreatedUser, UsersError> {
        crate::password::check_password_policy(password)?;
        let password_hash = hash_password(password)?;
        let master_key = generate_master_key();
        let recovery_key = generate_recovery_key();
        let sealed = seal_master_key(&master_key, password, &recovery_key)?;
        let sealed_json = serde_json::to_string(&sealed).map_err(|_| UsersError::CorruptSeal)?;

        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        let role_str = role.as_str();
        let res = sqlx::query(
            "INSERT INTO users (id, username, email, role, auth_provider, password_hash, sealed_master_key, must_change_password, created_at, updated_at)
             VALUES (?, ?, ?, ?, 'local', ?, ?, 0, ?, ?)",
        )
        .bind(id.to_string())
        .bind(username)
        .bind(email)
        .bind(role_str)
        .bind(&password_hash)
        .bind(&sealed_json)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => {}
            Err(sqlx::Error::Database(e)) if e.message().contains("UNIQUE") => {
                return Err(UsersError::Duplicate);
            }
            Err(e) => return Err(e.into()),
        }
        let record = UserRecord {
            id,
            username: username.to_string(),
            email: email.to_string(),
            role,
            auth_provider: "local".into(),
            password_hash: Some(password_hash),
            sealed_master_key: sealed_json,
            must_change_password: false,
            totp_secret: None,
            created_at: Utc::now(),
        };
        Ok(CreatedUser {
            record,
            recovery_key,
            master_key,
        })
    }

    /// Fetch a user by username (case-insensitive).
    pub async fn get_by_username(&self, username: &str) -> Result<UserRecord, UsersError> {
        let row: Option<UserRow> = sqlx::query_as(
            "SELECT id, username, email, role, auth_provider, password_hash, sealed_master_key, must_change_password, totp_secret, created_at
             FROM users WHERE username = ? COLLATE NOCASE",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;
        let row = row.ok_or(UsersError::NotFound)?;
        Ok(UserRecord {
            id: Uuid::parse_str(&row.0).map_err(|_| UsersError::CorruptSeal)?,
            username: row.1,
            email: row.2,
            role: Role::parse(&row.3).ok_or(UsersError::CorruptSeal)?,
            auth_provider: row.4,
            password_hash: row.5,
            sealed_master_key: row.6,
            must_change_password: row.7 != 0,
            totp_secret: row.8,
            created_at: DateTime::parse_from_rfc3339(&row.9)
                .map_err(|_| UsersError::CorruptSeal)?
                .with_timezone(&Utc),
        })
    }

    /// Authenticate a local user with username + password. Returns the user
    /// and their unwrapped master key. TOTP (if enrolled) is checked by the
    /// caller via `totp::verify` — this function is password-only.
    pub async fn authenticate_local(
        &self,
        username: &str,
        password: &str,
    ) -> Result<AuthenticatedUser, UsersError> {
        let record = self.get_by_username(username).await?;
        let Some(stored_hash) = &record.password_hash else {
            // OIDC-only account: no local password to verify.
            return Err(UsersError::InvalidCredentials);
        };
        if !verify_password(password, stored_hash)? {
            return Err(UsersError::InvalidCredentials);
        }
        let sealed: SealedMasterKey =
            serde_json::from_str(&record.sealed_master_key).map_err(|_| UsersError::CorruptSeal)?;
        let master_key = unseal_with_password(&sealed, password)?;
        Ok(AuthenticatedUser { record, master_key })
    }

    /// Change a user's password: verifies the old password, re-wraps the
    /// master key (file ciphertext untouched), updates the hash.
    pub async fn change_password(
        &self,
        user_id: Uuid,
        old_password: &str,
        new_password: &str,
    ) -> Result<(), UsersError> {
        crate::password::check_password_policy(new_password)?;
        let record = self.get_by_id(user_id).await?;
        let Some(stored_hash) = &record.password_hash else {
            return Err(UsersError::InvalidCredentials);
        };
        if !verify_password(old_password, stored_hash)? {
            return Err(UsersError::InvalidCredentials);
        }
        let sealed: SealedMasterKey =
            serde_json::from_str(&record.sealed_master_key).map_err(|_| UsersError::CorruptSeal)?;
        let resealed = reseal_password(&sealed, old_password, new_password)?;
        let resealed_json =
            serde_json::to_string(&resealed).map_err(|_| UsersError::CorruptSeal)?;
        let new_hash = hash_password(new_password)?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE users SET password_hash = ?, sealed_master_key = ?, must_change_password = 0, updated_at = ? WHERE id = ?",
        )
        .bind(&new_hash)
        .bind(&resealed_json)
        .bind(&now)
        .bind(user_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch a user by id.
    pub async fn get_by_id(&self, user_id: Uuid) -> Result<UserRecord, UsersError> {
        let row: Option<UserRow> = sqlx::query_as(
            "SELECT id, username, email, role, auth_provider, password_hash, sealed_master_key, must_change_password, totp_secret, created_at
             FROM users WHERE id = ?",
        )
        .bind(user_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        let row = row.ok_or(UsersError::NotFound)?;
        Ok(UserRecord {
            id: Uuid::parse_str(&row.0).map_err(|_| UsersError::CorruptSeal)?,
            username: row.1,
            email: row.2,
            role: Role::parse(&row.3).ok_or(UsersError::CorruptSeal)?,
            auth_provider: row.4,
            password_hash: row.5,
            sealed_master_key: row.6,
            must_change_password: row.7 != 0,
            totp_secret: row.8,
            created_at: DateTime::parse_from_rfc3339(&row.9)
                .map_err(|_| UsersError::CorruptSeal)?
                .with_timezone(&Utc),
        })
    }

    /// Count users (for bootstrap: only create the first admin when zero).
    pub async fn count(&self) -> Result<i64, UsersError> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }

    /// List all users (admin portal): (username, email, role, provider).
    pub async fn list_all(&self) -> Result<Vec<(String, String, String, String)>, UsersError> {
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT username, email, role, auth_provider FROM users ORDER BY username",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// List a user's API keys (delegates to ApiKeyManager).
    pub async fn list_api_keys(
        &self,
        user_id: Uuid,
    ) -> Result<Vec<crate::api_key::ApiKeyRecord>, UsersError> {
        let manager = crate::api_key::ApiKeyManager::new(self.pool.clone());
        manager.list_for_user(user_id).await.map_err(|e| match e {
            crate::api_key::ApiKeyError::Db(db) => db.into(),
            _ => UsersError::NotFound,
        })
    }

    /// Delete a user (cascades sessions/api_keys/webauthn). The caller is
    /// responsible for removing their file-repo directory and index rows.
    pub async fn delete(&self, user_id: Uuid) -> Result<(), UsersError> {
        let res = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(user_id.to_string())
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(UsersError::NotFound);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mycelium_store::Store;

    async fn test_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        (dir, store)
    }

    const PW: &str = "correct horse battery staple";

    #[tokio::test]
    async fn create_and_authenticate() {
        let (_dir, store) = test_store().await;
        let users = UserStore::new(store.pool().clone());
        let created = users
            .create_local("alice", "alice@example.com", PW, Role::User)
            .await
            .unwrap();
        assert!(created.recovery_key.starts_with("myc2-recovery-"));
        let auth = users.authenticate_local("alice", PW).await.unwrap();
        assert_eq!(auth.record.username, "alice");
        assert_eq!(auth.record.role, Role::User);
    }

    #[tokio::test]
    async fn duplicate_rejected() {
        let (_dir, store) = test_store().await;
        let users = UserStore::new(store.pool().clone());
        users
            .create_local("alice", "alice@example.com", PW, Role::User)
            .await
            .unwrap();
        assert!(matches!(
            users
                .create_local("alice", "other@example.com", PW, Role::User)
                .await,
            Err(UsersError::Duplicate)
        ));
    }

    #[tokio::test]
    async fn wrong_password_rejected() {
        let (_dir, store) = test_store().await;
        let users = UserStore::new(store.pool().clone());
        users
            .create_local("alice", "alice@example.com", PW, Role::User)
            .await
            .unwrap();
        assert!(matches!(
            users
                .authenticate_local("alice", "wrong password entirely!!")
                .await,
            Err(UsersError::InvalidCredentials)
        ));
    }

    #[tokio::test]
    async fn short_password_rejected() {
        let (_dir, store) = test_store().await;
        let users = UserStore::new(store.pool().clone());
        assert!(matches!(
            users
                .create_local("alice", "alice@example.com", "short", Role::User)
                .await,
            Err(UsersError::Password(
                crate::password::PasswordError::TooShort
            ))
        ));
    }

    #[tokio::test]
    async fn password_change_keeps_master_key() {
        let (_dir, store) = test_store().await;
        let users = UserStore::new(store.pool().clone());
        let created = users
            .create_local("alice", "alice@example.com", PW, Role::User)
            .await
            .unwrap();
        let before = users.authenticate_local("alice", PW).await.unwrap();
        let new_pw = "new password with 20 chars";
        users
            .change_password(created.record.id, PW, new_pw)
            .await
            .unwrap();
        let after = users.authenticate_local("alice", new_pw).await.unwrap();
        // Same master key unwrapped under the new password.
        assert_eq!(after.master_key.as_bytes(), before.master_key.as_bytes());
        // Old password rejected.
        assert!(users.authenticate_local("alice", PW).await.is_err());
    }

    #[tokio::test]
    async fn delete_cascades() {
        let (_dir, store) = test_store().await;
        let users = UserStore::new(store.pool().clone());
        let created = users
            .create_local("bob", "bob@example.com", PW, Role::User)
            .await
            .unwrap();
        users.delete(created.record.id).await.unwrap();
        assert!(matches!(
            users.get_by_id(created.record.id).await,
            Err(UsersError::NotFound)
        ));
    }
}
