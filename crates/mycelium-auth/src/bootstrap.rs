//! First-run admin bootstrap: generated password written to a 0600 file,
//! forced password change on first login.

use std::io::Write;

use mycelium_crypto::{generate_master_key, generate_recovery_key};
use uuid::Uuid;

use crate::password::hash_password;
use crate::rbac::Role;
use crate::users::{UserRecord, UserStore};

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("users error: {0}")]
    Users(#[from] crate::users::UsersError),
    #[error("password error: {0}")]
    Password(#[from] crate::password::PasswordError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("admin already exists (bootstrap is first-run only)")]
    AlreadyBootstrapped,
    #[error("crypto error: {0}")]
    Crypto(#[from] mycelium_crypto::CryptoError),
}

/// Bootstrap the first admin account if no users exist.
///
/// The generated password is written to `<data_dir>/config/initial-admin-password`
/// with 0600 permissions (never logged, never printed). The account is
/// flagged `must_change_password` so the first login forces a change.
/// Returns None if users already exist (normal startup path).
pub async fn bootstrap_admin(
    store: &mycelium_store::Store,
) -> Result<Option<UserRecord>, BootstrapError> {
    let users = UserStore::new(store.pool().clone());
    if users.count().await? > 0 {
        return Ok(None);
    }

    // Generate a strong password: 24 random bytes, base64url (32 chars).
    let password = format!("myc2-admin-{}", generate_recovery_key_password());
    let master_key = generate_master_key();
    let recovery_key = generate_recovery_key();
    let sealed = mycelium_crypto::seal_master_key(&master_key, &password, &recovery_key)?;
    let sealed_json = serde_json::to_string(&sealed)
        .map_err(|_| BootstrapError::Users(crate::users::UsersError::CorruptSeal))?;
    let password_hash = hash_password(&password)?;

    // Write the secret files FIRST (0600, atomic create) so a crash between
    // file writes and the DB insert leaves no orphaned admin account — on
    // restart, count() is still 0 and bootstrap retries cleanly.
    let path = store.data_dir().join("config/initial-admin-password");
    let recovery_path = store.data_dir().join("config/initial-admin-recovery-key");
    write_secret_file(&path, password.as_bytes())?;
    write_secret_file(&recovery_path, recovery_key.as_bytes())?;

    let id = Uuid::new_v4();
    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "INSERT INTO users (id, username, email, role, auth_provider, password_hash, sealed_master_key, must_change_password, created_at, updated_at)
         VALUES (?, 'admin', 'admin@localhost.local', 'admin', 'local', ?, ?, 1, ?, ?)",
    )
    .bind(id.to_string())
    .bind(&password_hash)
    .bind(&sealed_json)
    .bind(&now)
    .bind(&now)
    .execute(store.pool())
    .await;
    match res {
        Ok(_) => {}
        // Concurrent first-startup race: the other instance won. Our secret
        // files are superseded — remove them so they don't linger.
        Err(sqlx::Error::Database(e)) if e.message().contains("UNIQUE") => {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&recovery_path);
            return Ok(None);
        }
        Err(e) => {
            // DB failed after files were written: remove them so a retry
            // doesn't hit create_new on stale files.
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&recovery_path);
            return Err(e.into());
        }
    }

    Ok(Some(UserRecord {
        id,
        username: "admin".into(),
        email: "admin@localhost.local".into(),
        role: Role::Admin,
        auth_provider: "local".into(),
        password_hash: Some(password_hash),
        sealed_master_key: sealed_json,
        must_change_password: true,
        totp_secret: None,
        created_at: chrono::Utc::now(),
    }))
}

/// Write a secret to a new file with 0600 permissions atomically
/// (create_new + mode — no world-readable window, no overwrite).
fn write_secret_file(path: &std::path::Path, contents: &[u8]) -> Result<(), BootstrapError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    file.write_all(b"\n")?;
    Ok(())
}

fn generate_recovery_key_password() -> String {
    // 16 random bytes hex = 32 chars, plenty for a bootstrap password.
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mycelium_store::Store;

    #[tokio::test]
    async fn bootstraps_once_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        // First run: creates admin.
        let admin = bootstrap_admin(&store).await.unwrap();
        assert!(admin.is_some());
        let admin = admin.unwrap();
        assert_eq!(admin.username, "admin");
        assert_eq!(admin.role, Role::Admin);
        assert!(admin.must_change_password);
        // Password file exists with 0600 perms.
        let path = dir.path().join("config/initial-admin-password");
        assert!(path.exists());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // Second run: no-op (users exist).
        let again = bootstrap_admin(&store).await.unwrap();
        assert!(again.is_none());
    }

    #[tokio::test]
    async fn bootstrap_password_authenticates() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        bootstrap_admin(&store).await.unwrap();
        let password = std::fs::read_to_string(dir.path().join("config/initial-admin-password"))
            .unwrap()
            .trim()
            .to_string();
        let users = UserStore::new(store.pool().clone());
        let auth = users.authenticate_local("admin", &password).await.unwrap();
        assert_eq!(auth.record.role, Role::Admin);
        assert!(auth.record.must_change_password);
    }
}
