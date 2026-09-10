//! Service key management: the server-wide key encrypting global
//! bookshelves, shared library stacks, and the global skills shelf.
//!
//! The key is 32 random bytes, hex-encoded in a file with 0600 permissions.
//! It is loaded from `MYCELIUM2_SERVICE_KEY` (hex) if set, otherwise loaded
//! from or created at `<data_dir>/config/service.key`. It is NEVER baked
//! into the Docker image.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::keys::{ServiceKey, generate_master_key};

const SERVICE_KEY_FILE: &str = "config/service.key";

#[derive(Debug, thiserror::Error)]
pub enum ServiceKeyError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "service key file exists but is unreadable, malformed, or has permissive permissions: {path}"
    )]
    Malformed { path: String },
    #[error("MYCELIUM2_SERVICE_KEY env var is not 64 hex chars")]
    BadEnv,
}

/// Load the service key, creating it on first run if needed.
///
/// Priority: `env_hex` override (64 hex chars, from `MYCELIUM2_SERVICE_KEY`
/// in production) → `<data_dir>/config/service.key` (created with 0600
/// perms if absent).
pub fn load_or_create_service_key_with(
    data_dir: &Path,
    env_hex: Option<&str>,
) -> Result<ServiceKey, ServiceKeyError> {
    if let Some(hex_str) = env_hex {
        return parse_key_hex(hex_str).map_err(|_| ServiceKeyError::BadEnv);
    }
    let path = data_dir.join(SERVICE_KEY_FILE);
    if path.exists() {
        // The file may be mid-write by a concurrent first-run creator —
        // a single unparseable read must retry, not fail permanently.
        return load_existing_key_with_retry(&path);
    }
    // First run: generate and persist with restrictive permissions.
    // create_new is atomic — if we lose the race, reload the winner's key.
    let key = generate_master_key();
    let parent = path.parent().ok_or_else(|| ServiceKeyError::Malformed {
        path: path.display().to_string(),
    })?;
    fs::create_dir_all(parent)?;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(hex::encode(key.as_bytes()).as_bytes())?;
            file.write_all(b"\n")?;
            drop(file);
            Ok(ServiceKey::from_bytes(key.as_bytes()).expect("32 bytes"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another thread/process created it first: load theirs (with
            // retry — they may still be writing).
            load_existing_key_with_retry(&path)
        }
        Err(e) => Err(e.into()),
    }
}

/// Read an existing key file, retrying briefly if it is unparseable
/// (a concurrent creator may be mid-write). Bounded: ~500ms total.
///
/// Rejects files with group/other permission bits set — a service key
/// readable by anyone but the service user is a configuration error.
fn load_existing_key_with_retry(path: &Path) -> Result<ServiceKey, ServiceKeyError> {
    let last_err = ServiceKeyError::Malformed {
        path: path.display().to_string(),
    };
    for _ in 0..50 {
        if let Ok(contents) = fs::read_to_string(path)
            && let Ok(key) = parse_key_hex(contents.trim())
        {
            check_permissions(path)?;
            return Ok(key);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Err(last_err)
}

/// Reject a key file with group/other access bits set.
fn check_permissions(path: &Path) -> Result<(), ServiceKeyError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(ServiceKeyError::Malformed {
            path: path.display().to_string(),
        });
    }
    Ok(())
}

/// Production entry point: reads `MYCELIUM2_SERVICE_KEY` from the environment.
pub fn load_or_create_service_key(data_dir: &Path) -> Result<ServiceKey, ServiceKeyError> {
    let env_hex = std::env::var("MYCELIUM2_SERVICE_KEY").ok();
    load_or_create_service_key_with(data_dir, env_hex.as_deref())
}

fn parse_key_hex(hex_str: &str) -> Result<ServiceKey, ()> {
    let bytes = hex::decode(hex_str).map_err(|_| ())?;
    ServiceKey::from_bytes(&bytes).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn creates_and_reloads_key() {
        let dir = tempfile::tempdir().unwrap();
        let key1 = load_or_create_service_key_with(dir.path(), None).unwrap();
        let key2 = load_or_create_service_key_with(dir.path(), None).unwrap();
        assert_eq!(key1.as_bytes(), key2.as_bytes());
        // File has 0600 perms.
        let meta = fs::metadata(dir.path().join(SERVICE_KEY_FILE)).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn env_override_takes_priority() {
        let key = generate_master_key();
        let dir = tempfile::tempdir().unwrap();
        let loaded =
            load_or_create_service_key_with(dir.path(), Some(&hex::encode(key.as_bytes())))
                .unwrap();
        assert_eq!(loaded.as_bytes(), key.as_bytes());
        // No file created when the override is set.
        assert!(!dir.path().join(SERVICE_KEY_FILE).exists());
    }

    #[test]
    fn bad_env_override_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            load_or_create_service_key_with(dir.path(), Some("not-hex")),
            Err(ServiceKeyError::BadEnv)
        ));
    }

    #[test]
    fn malformed_file_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SERVICE_KEY_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "garbage").unwrap();
        assert!(matches!(
            load_or_create_service_key_with(dir.path(), None),
            Err(ServiceKeyError::Malformed { .. })
        ));
    }

    #[test]
    fn permissive_permissions_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SERVICE_KEY_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let key = generate_master_key();
        fs::write(&path, hex::encode(key.as_bytes())).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&path, perms).unwrap();
        assert!(matches!(
            load_or_create_service_key_with(dir.path(), None),
            Err(ServiceKeyError::Malformed { .. })
        ));
    }

    #[test]
    fn concurrent_creation_converges() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dir.path().to_path_buf();
        let p2 = dir.path().to_path_buf();
        let (a, b) = std::thread::scope(|s| {
            let h1 = s.spawn(move || load_or_create_service_key_with(&p1, None));
            let h2 = s.spawn(move || load_or_create_service_key_with(&p2, None));
            (h1.join().unwrap().unwrap(), h2.join().unwrap().unwrap())
        });
        // Both threads must end with the same key: one creates, the other
        // either reads the created file or loses create_new and retries.
        assert_eq!(a.as_bytes(), b.as_bytes());
    }
}
