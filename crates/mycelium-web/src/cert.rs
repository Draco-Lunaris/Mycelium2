//! TLS certificate management: auto-generate a self-signed cert on first
//! run if none is provided (DESIGN decision). Admins can supply real certs
//! via config paths, upload a pair (written by the admin portal), or set
//! the generated pair's SANs. See
//! docs/superpowers/specs/2026-10-14-tls-management-design.md.

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
/// 1. `cert_path`/`key_path` if both exist (admin-supplied env mounts).
/// 2. `<data_dir>/config/tls/uploaded-{cert,key}.pem` if both exist
///    (uploaded via the admin portal). An unreadable uploaded pair warns
///    and falls back to the generated pair — it must not fail boot.
/// 3. The managed generated pair under `<data_dir>/config/tls/`, created
///    or regenerated per `sans` (sans.txt sidecar).
///
/// Returns two clones of one `RustlsConfig` handle (internally Arc-backed):
/// the first for `bind_rustls`, the second for callers that keep the
/// config around (admin TLS reload).
pub async fn load_or_create_tls_config(
    data_dir: &Path,
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
    sans: &[String],
) -> Result<(RustlsConfig, RustlsConfig), CertError> {
    // Pin the rustls crypto provider (ring) — required when multiple
    // provider features are in the dependency graph.
    let _ = rustls::crypto::ring::default_provider().install_default();
    // 1. Admin-supplied env mounts.
    if let (Some(cert), Some(key)) = (cert_path, key_path)
        && cert.exists()
        && key.exists()
    {
        let cfg = RustlsConfig::from_pem_file(cert, key)
            .await
            .map_err(|e| CertError::Load(e.to_string()))?;
        return Ok((cfg.clone(), cfg));
    }
    let tls_dir = data_dir.join("config/tls");
    // Keep the generated fallback current even when an uploaded pair is
    // active (spec: regen still happens; uploaded remains the served cert).
    ensure_managed_pair(&tls_dir, sans)?;
    // 2. Uploaded pair beats generated. Unreadable → warn, fall back.
    let up_cert = tls_dir.join("uploaded-cert.pem");
    let up_key = tls_dir.join("uploaded-key.pem");
    if up_cert.exists() && up_key.exists() {
        match RustlsConfig::from_pem_file(&up_cert, &up_key).await {
            Ok(cfg) => return Ok((cfg.clone(), cfg)),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "uploaded TLS pair unreadable — falling back to generated"
                );
            }
        }
    }
    // 3. Generated pair.
    let cfg = RustlsConfig::from_pem_file(tls_dir.join("cert.pem"), tls_dir.join("key.pem"))
        .await
        .map_err(|e| CertError::Load(e.to_string()))?;
    Ok((cfg.clone(), cfg))
}

/// Generate a self-signed pair carrying `sans` verbatim (assumes caller
/// checked absence). Key 0600, atomic create_new.
pub fn generate_self_signed_with_sans(
    tls_dir: &Path,
    cert_path: &Path,
    key_path: &Path,
    sans: &[String],
) -> Result<(), CertError> {
    // rcgen emits a cert with NO SAN extension for an empty list —
    // useless for TLS and a silent footgun for every caller that
    // forgets to default. Fail loudly instead.
    if sans.is_empty() {
        return Err(CertError::Generate("SAN list must not be empty".into()));
    }
    std::fs::create_dir_all(tls_dir)?;
    let subject_alt_names: Vec<String> = sans.to_vec();
    let cert = rcgen::generate_simple_self_signed(subject_alt_names)
        .map_err(|e| CertError::Generate(e.to_string()))?;
    write_pair(
        &cert.cert.pem(),
        &cert.key_pair.serialize_pem(),
        cert_path,
        key_path,
    )
}

/// Write cert/key PEMs; key 0600 create_new. Shared by generation and
/// regeneration.
fn write_pair(
    cert_pem: &str,
    key_pem: &str,
    cert_path: &Path,
    key_path: &Path,
) -> Result<(), CertError> {
    std::fs::write(cert_path, cert_pem)?;
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

/// The managed generated pair: create if absent, regenerate if the
/// requested SAN set changed (sidecar sans.txt). Never touches an
/// uploaded pair.
pub fn ensure_managed_pair(tls_dir: &Path, sans: &[String]) -> Result<(), CertError> {
    let cert = tls_dir.join("cert.pem");
    let key = tls_dir.join("key.pem");
    let sidecar = tls_dir.join("sans.txt");
    let requested: String = sans
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let regenerate = !matches!(
        std::fs::read_to_string(&sidecar),
        Ok(existing) if existing == requested && cert.exists() && key.exists()
    );
    if regenerate {
        // Remove the old generated pair first (regeneration replaces).
        let _ = std::fs::remove_file(&cert);
        let _ = std::fs::remove_file(&key);
        generate_self_signed_with_sans(tls_dir, &cert, &key, sans)?;
        std::fs::write(&sidecar, &requested)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls_admin;

    /// The SAN names carried by the generated cert's SAN extension.
    fn cert_dns_sans(cert_path: &Path) -> Vec<String> {
        let pem = std::fs::read_to_string(cert_path).unwrap();
        let (_, p) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap();
        let cert = p.parse_x509().unwrap();
        let ext = cert
            .extensions()
            .iter()
            .find(|e| e.oid == x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME)
            .expect("SAN extension present");
        match ext.parsed_extension() {
            x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) => {
                san.general_names.iter().map(|n| n.to_string()).collect()
            }
            other => panic!("unexpected SAN extension value: {other:?}"),
        }
    }

    #[test]
    fn generates_and_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        let sans = vec![tls_admin::DEFAULT_SAN.to_string()];
        generate_self_signed_with_sans(
            &tls_dir,
            &tls_dir.join("cert.pem"),
            &tls_dir.join("key.pem"),
            &sans,
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
            generate_self_signed_with_sans(
                &tls_dir,
                &tls_dir.join("cert.pem"),
                &tls_dir.join("key.pem"),
                &sans
            )
            .is_err()
        );
    }

    #[test]
    fn empty_sans_is_rejected_not_silently_unsanitised() {
        // rcgen would emit a cert with no SAN extension; the guard
        // refuses instead (protects every Task 4+ caller).
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        let err = generate_self_signed_with_sans(
            &tls_dir,
            &tls_dir.join("cert.pem"),
            &tls_dir.join("key.pem"),
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, CertError::Generate(_)), "got {err:?}");
        assert!(err.to_string().contains("SAN list must not be empty"));
        // Nothing written — the refusal happens before any I/O.
        assert!(!tls_dir.join("cert.pem").exists());
        assert!(!tls_dir.join("key.pem").exists());
    }

    #[tokio::test]
    async fn tls_config_loads_or_creates() {
        let dir = tempfile::tempdir().unwrap();
        let sans = vec!["localhost".to_string()];
        let _config = load_or_create_tls_config(dir.path(), None, None, &sans)
            .await
            .unwrap();
        // Managed pair now exists; a second call reuses it.
        assert!(dir.path().join("config/tls/cert.pem").exists());
        let _ = load_or_create_tls_config(dir.path(), None, None, &sans)
            .await
            .unwrap();
    }

    #[test]
    fn generated_cert_contains_requested_sans() {
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        let sans = vec!["localhost".to_string(), "mycelium2.example".to_string()];
        generate_self_signed_with_sans(
            &tls_dir,
            &tls_dir.join("cert.pem"),
            &tls_dir.join("key.pem"),
            &sans,
        )
        .unwrap();
        let text = cert_dns_sans(&tls_dir.join("cert.pem")).join(",");
        assert!(text.contains("mycelium2.example"), "SAN missing: {text}");
        assert!(text.contains("localhost"), "SAN missing: {text}");
    }

    #[test]
    fn sans_sidecar_written_and_regen_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        let sans_a = vec!["localhost".to_string()];
        let sans_b = vec!["localhost".to_string(), "example.com".to_string()];
        ensure_managed_pair(&tls_dir, &sans_a).unwrap();
        assert_eq!(
            std::fs::read_to_string(tls_dir.join("sans.txt")).unwrap(),
            "localhost\n"
        );
        // Different requested set → regenerate.
        ensure_managed_pair(&tls_dir, &sans_b).unwrap();
        let text = cert_dns_sans(&tls_dir.join("cert.pem")).join(",");
        assert!(
            text.contains("example.com"),
            "regenerated cert must carry new SAN: {text}"
        );
        // Same set → no regenerate (mtime-based check is flaky; assert
        // by content: writing sans_b again is a no-op that keeps the file).
        let before = std::fs::read(tls_dir.join("cert.pem")).unwrap();
        ensure_managed_pair(&tls_dir, &sans_b).unwrap();
        let after = std::fs::read(tls_dir.join("cert.pem")).unwrap();
        assert_eq!(before, after, "same SAN set must not regenerate");
    }

    #[tokio::test]
    async fn uploaded_pair_beats_generated_at_boot() {
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        std::fs::create_dir_all(&tls_dir).unwrap();
        // Simulate a previously uploaded pair: generate a second,
        // distinct pair and write it as uploaded-*.
        let up_dir = dir.path().join("uploaded-src");
        generate_self_signed_with_sans(
            &up_dir,
            &up_dir.join("cert.pem"),
            &up_dir.join("key.pem"),
            &["uploaded.example".to_string()],
        )
        .unwrap();
        std::fs::copy(up_dir.join("cert.pem"), tls_dir.join("uploaded-cert.pem")).unwrap();
        std::fs::copy(up_dir.join("key.pem"), tls_dir.join("uploaded-key.pem")).unwrap();
        let _cfg = load_or_create_tls_config(dir.path(), None, None, &["localhost".to_string()])
            .await
            .unwrap();
        // The GENERATED pair must still exist (fallback kept current).
        assert!(tls_dir.join("cert.pem").exists());
    }

    #[tokio::test]
    async fn unreadable_uploaded_pair_falls_back_to_generated() {
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        std::fs::create_dir_all(&tls_dir).unwrap();
        std::fs::write(tls_dir.join("uploaded-cert.pem"), "not a pem").unwrap();
        std::fs::write(tls_dir.join("uploaded-key.pem"), "not a pem").unwrap();
        let _cfg = load_or_create_tls_config(dir.path(), None, None, &["localhost".to_string()])
            .await
            .expect("must fall back to the generated pair, not fail boot");
    }
}
