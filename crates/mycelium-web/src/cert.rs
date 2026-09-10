//! TLS certificate management: auto-generate a self-signed cert on first
//! run if none is provided (DESIGN decision). Admins can supply real certs
//! via config paths.

use std::path::Path;

use axum_server::tls_rustls::RustlsConfig;

#[derive(Debug, thiserror::Error)]
pub enum CertError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cert generation error: {0}")]
    Generate(String),
    #[error("cert load error: {0}")]
    Load(String),
}

/// Load TLS configuration. Priority:
/// 1. `cert_path`/`key_path` if both exist (admin-supplied).
/// 2. `<data_dir>/config/tls/{cert,key}.pem` if both exist (previously generated).
/// 3. Generate a fresh self-signed pair and persist it (0600 for the key).
pub async fn load_or_create_tls_config(
    data_dir: &Path,
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
) -> Result<RustlsConfig, CertError> {
    // Pin the rustls crypto provider (ring) — required when multiple
    // provider features are in the dependency graph.
    let _ = rustls::crypto::ring::default_provider().install_default();
    // 1. Admin-supplied.
    if let (Some(cert), Some(key)) = (cert_path, key_path)
        && cert.exists()
        && key.exists()
    {
        return RustlsConfig::from_pem_file(cert, key)
            .await
            .map_err(|e| CertError::Load(e.to_string()));
    }
    // 2/3. Managed pair under the data dir.
    let tls_dir = data_dir.join("config/tls");
    let cert = tls_dir.join("cert.pem");
    let key = tls_dir.join("key.pem");
    if !(cert.exists() && key.exists()) {
        generate_self_signed(&tls_dir, &cert, &key)?;
    }
    RustlsConfig::from_pem_file(&cert, &key)
        .await
        .map_err(|e| CertError::Load(e.to_string()))
}

fn generate_self_signed(
    tls_dir: &Path,
    cert_path: &Path,
    key_path: &Path,
) -> Result<(), CertError> {
    std::fs::create_dir_all(tls_dir)?;
    let subject_alt_names = vec!["localhost".to_string()];
    let cert = rcgen::generate_simple_self_signed(subject_alt_names)
        .map_err(|e| CertError::Generate(e.to_string()))?;
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();
    std::fs::write(cert_path, cert_pem)?;
    // Key with 0600 perms (create_new for atomicity).
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(key_path)?;
    f.write_all(key_pem.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_and_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        generate_self_signed(
            &tls_dir,
            &tls_dir.join("cert.pem"),
            &tls_dir.join("key.pem"),
        )
        .unwrap();
        assert!(tls_dir.join("cert.pem").exists());
        assert!(tls_dir.join("key.pem").exists());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(tls_dir.join("key.pem"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // Second generation on the same paths fails (create_new) — the
        // caller checks existence first.
        assert!(
            generate_self_signed(
                &tls_dir,
                &tls_dir.join("cert.pem"),
                &tls_dir.join("key.pem")
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn tls_config_loads_or_creates() {
        let dir = tempfile::tempdir().unwrap();
        let _config = load_or_create_tls_config(dir.path(), None, None)
            .await
            .unwrap();
        // Managed pair now exists; a second call reuses it.
        assert!(dir.path().join("config/tls/cert.pem").exists());
        let _ = load_or_create_tls_config(dir.path(), None, None)
            .await
            .unwrap();
    }
}
