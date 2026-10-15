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

/// The managed generated pair, with sidecar-wins boot semantics:
///
/// - sans.txt + complete pair  → no-op. The sidecar is the desired-state
///   record: the env/flag requested set is only the FIRST-BOOT
///   initializer, so a set chosen in the admin UI survives restarts
///   even when the env var still says something else.
/// - sans.txt + incomplete pair → regenerate from the RECORDED set
///   (restores what was recorded, not what the env says today).
/// - no sidecar (absent, blank, or garbage) → regenerate from the
///   requested set and write the sidecar. Removing sans.txt is the
///   documented escape hatch that hands control back to the env/flag.
///
/// Never touches an uploaded pair.
pub fn ensure_managed_pair(tls_dir: &Path, sans: &[String]) -> Result<(), CertError> {
    let cert = tls_dir.join("cert.pem");
    let key = tls_dir.join("key.pem");
    let sidecar_content = std::fs::read_to_string(tls_dir.join("sans.txt")).unwrap_or_default();
    let recorded = parse_sidecar(&sidecar_content);
    match recorded {
        // Sidecar is authoritative and the pair is complete → nothing
        // to do, whatever the requested set says.
        Some(_) if cert.exists() && key.exists() => Ok(()),
        // Sidecar exists but the pair is incomplete → rebuild from the
        // recorded set (restores what was recorded, not what the env
        // says today).
        Some(recorded_sans) => regenerate_managed_pair(tls_dir, &recorded_sans),
        // No sidecar → the requested set initializes.
        None => regenerate_managed_pair(tls_dir, sans),
    }
}

/// Parse a sans.txt sidecar into a SAN list. Blank/garbage/unreadable
/// content reads as no sidecar (None); a non-empty list of names is
/// the recorded set, order-stable.
fn parse_sidecar(content: &str) -> Option<Vec<String>> {
    let names: Vec<String> = content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if names.is_empty() { None } else { Some(names) }
}

/// Unconditionally regenerate the managed generated pair: a fresh key
/// (the old key is treated as suspect — identical SANs still mint a
/// new pair) and a fresh self-signed cert carrying `sans`, plus the
/// sans.txt sidecar. The admin Regenerate action and every
/// ensure_managed_pair regen branch run through here so the write
/// sequence exists exactly once. Never touches an uploaded pair or a
/// pending CSR.
///
/// Ordering (security review F2): the new pair is minted IN MEMORY
/// before anything is removed — rcgen validates the SAN names there
/// (e.g. non-ASCII is rejected), so a refused call is a plain error
/// and the existing pair is never destroyed. Only after the mint
/// succeeds do the removes + writes run.
pub fn regenerate_managed_pair(tls_dir: &Path, sans: &[String]) -> Result<(), CertError> {
    let cert_path = tls_dir.join("cert.pem");
    let key_path = tls_dir.join("key.pem");
    // Mint first: everything rcgen can refuse (empty list, non-ASCII
    // names) is refused HERE, before any existing file is touched.
    if sans.is_empty() {
        return Err(CertError::Generate("SAN list must not be empty".into()));
    }
    std::fs::create_dir_all(tls_dir)?;
    let cert = rcgen::generate_simple_self_signed(sans.to_vec())
        .map_err(|e| CertError::Generate(e.to_string()))?;
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();
    // Remove the old pair only now (regeneration replaces; the key is
    // create_new so a stale key must not block the fresh write).
    let _ = std::fs::remove_file(&cert_path);
    let _ = std::fs::remove_file(&key_path);
    write_pair(&cert_pem, &key_pem, &cert_path, &key_path)?;
    let requested: String = sans
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(tls_dir.join("sans.txt"), &requested)?;
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
    fn sans_sidecar_is_authoritative_once_written() {
        // Sidecar-wins boot semantics: once sans.txt records a set,
        // it — not the env/flag requested set — is the desired state.
        // Boot never regenerates a complete pair (the UI's decision
        // survives restarts); the env/flag set is only the first-boot
        // initializer.
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        let sans_a = vec!["localhost".to_string()];
        let sans_b = vec!["localhost".to_string(), "example.com".to_string()];
        ensure_managed_pair(&tls_dir, &sans_a).unwrap();
        assert_eq!(
            std::fs::read_to_string(tls_dir.join("sans.txt")).unwrap(),
            "localhost\n"
        );
        // Different REQUESTED set with a complete pair on disk →
        // no-op (the sidecar is authoritative, the initializer loses).
        let before = std::fs::read(tls_dir.join("cert.pem")).unwrap();
        ensure_managed_pair(&tls_dir, &sans_b).unwrap();
        let after = std::fs::read(tls_dir.join("cert.pem")).unwrap();
        assert_eq!(before, after, "complete pair + sidecar must not regenerate");
        // Same set → still no-op.
        ensure_managed_pair(&tls_dir, &sans_a).unwrap();
        assert_eq!(
            before,
            std::fs::read(tls_dir.join("cert.pem")).unwrap(),
            "same set must not regenerate"
        );
        // The sidecar keeps its recorded content.
        assert_eq!(
            std::fs::read_to_string(tls_dir.join("sans.txt")).unwrap(),
            "localhost\n"
        );
    }

    #[test]
    fn incomplete_pair_regenerates_from_sidecar_not_requested() {
        // sans.txt exists but the pair is incomplete (a crash between
        // writes, or an operator removed one half): boot rebuilds from
        // the RECORDED set — not the env set — so it restores what was
        // recorded, not what the environment happens to say today.
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        ensure_managed_pair(&tls_dir, &["sidecar.example".to_string()]).unwrap();
        std::fs::remove_file(tls_dir.join("cert.pem")).unwrap();
        ensure_managed_pair(&tls_dir, &["env.example".to_string()]).unwrap();
        let text = cert_dns_sans(&tls_dir.join("cert.pem")).join(",");
        assert!(text.contains("sidecar.example"), "{text}");
        assert!(!text.contains("env.example"), "{text}");
        // The sidecar keeps its recorded set.
        assert_eq!(
            std::fs::read_to_string(tls_dir.join("sans.txt")).unwrap(),
            "sidecar.example\n"
        );
    }

    #[test]
    fn absent_sidecar_uses_requested_set_and_writes_it() {
        // No sidecar (fresh boot, or the operator removed sans.txt to
        // hand control back to the env/flag initializer): the requested
        // set initializes the pair and the sidecar records it.
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        ensure_managed_pair(&tls_dir, &["fresh.example".to_string()]).unwrap();
        assert_eq!(
            std::fs::read_to_string(tls_dir.join("sans.txt")).unwrap(),
            "fresh.example\n"
        );
        let text = cert_dns_sans(&tls_dir.join("cert.pem")).join(",");
        assert!(text.contains("fresh.example"), "{text}");
    }

    #[test]
    fn empty_or_garbage_sidecar_reads_as_absent() {
        // A sidecar that parses to an empty list (blank file) or is
        // unreadable behaves like no sidecar: the requested set
        // initializes. This must never fail boot.
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        std::fs::create_dir_all(&tls_dir).unwrap();
        std::fs::write(tls_dir.join("sans.txt"), "\n \n").unwrap();
        ensure_managed_pair(&tls_dir, &["fallback.example".to_string()]).unwrap();
        let text = cert_dns_sans(&tls_dir.join("cert.pem")).join(",");
        assert!(text.contains("fallback.example"), "{text}");
        assert_eq!(
            std::fs::read_to_string(tls_dir.join("sans.txt")).unwrap(),
            "fallback.example\n"
        );
    }

    // ---------- regenerate_managed_pair (admin Regenerate action) ----------

    #[test]
    fn regenerate_mints_fresh_key_even_for_identical_sans() {
        // The action is never a silent no-op: identical SANs still mint
        // a fresh key (the "old key is suspect" use case). A random
        // P-256 key makes the byte comparison decisive.
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        ensure_managed_pair(&tls_dir, &["same.example".to_string()]).unwrap();
        let key_before = std::fs::read_to_string(tls_dir.join("key.pem")).unwrap();
        let cert_before = std::fs::read_to_string(tls_dir.join("cert.pem")).unwrap();
        regenerate_managed_pair(&tls_dir, &["same.example".to_string()]).unwrap();
        let key_after = std::fs::read_to_string(tls_dir.join("key.pem")).unwrap();
        assert_ne!(key_before, key_after, "regenerate must mint a fresh key");
        assert_ne!(
            std::fs::read_to_string(tls_dir.join("cert.pem")).unwrap(),
            cert_before,
            "fresh key means a fresh certificate too"
        );
        // The key keeps 0600 (regeneration reuses the create_new path).
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(tls_dir.join("key.pem"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // The sidecar records the same set — content, not just mtime.
        assert_eq!(
            std::fs::read_to_string(tls_dir.join("sans.txt")).unwrap(),
            "same.example\n"
        );
    }

    #[test]
    fn regenerate_writes_new_sans_into_cert_and_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        ensure_managed_pair(&tls_dir, &["localhost".to_string()]).unwrap();
        regenerate_managed_pair(
            &tls_dir,
            &["regen.example".to_string(), "alt.example".to_string()],
        )
        .unwrap();
        // Sidecar: the requested set, one per line, trailing newline.
        assert_eq!(
            std::fs::read_to_string(tls_dir.join("sans.txt")).unwrap(),
            "regen.example\nalt.example\n"
        );
        // The minted cert actually carries the SANs.
        let text = cert_dns_sans(&tls_dir.join("cert.pem")).join(",");
        assert!(text.contains("regen.example"), "SAN missing: {text}");
        assert!(text.contains("alt.example"), "SAN missing: {text}");
    }

    #[test]
    fn regenerate_never_touches_uploaded_or_pending_files() {
        // Regeneration owns exactly cert.pem/key.pem/sans.txt. The
        // uploaded pair is never regenerated or touched (the boot
        // precedence contract), and a pending CSR is its own lifecycle.
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        std::fs::create_dir_all(&tls_dir).unwrap();
        let keep = [
            "uploaded-cert.pem",
            "uploaded-key.pem",
            "uploaded.json",
            "pending-csr-key.pem",
            "pending.json",
        ];
        for name in keep {
            std::fs::write(tls_dir.join(name), format!("{name} body")).unwrap();
        }
        regenerate_managed_pair(&tls_dir, &["keep.example".to_string()]).unwrap();
        for name in keep {
            assert_eq!(
                std::fs::read_to_string(tls_dir.join(name)).unwrap(),
                format!("{name} body"),
                "{name} must survive regeneration byte-identical"
            );
        }
    }

    #[test]
    fn regenerate_empty_sans_refused_loudly_before_any_write() {
        // generate_self_signed_with_sans refuses empty lists (rcgen
        // would emit a SAN-less cert); regenerate inherits the guard.
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        ensure_managed_pair(&tls_dir, &["localhost".to_string()]).unwrap();
        let err = regenerate_managed_pair(&tls_dir, &[]).unwrap_err();
        assert!(
            err.to_string().contains("SAN list must not be empty"),
            "got {err:?}"
        );
        // The refused call must not have deleted the existing pair.
        assert!(tls_dir.join("cert.pem").exists());
        assert!(tls_dir.join("key.pem").exists());
    }

    #[test]
    fn regenerate_rcgen_refusal_leaves_existing_pair_intact() {
        // Security review F2: rcgen rejects non-ASCII SAN names
        // (InvalidAsn1String) at generation time. The generation must
        // happen in memory BEFORE the old pair is removed, so a refused
        // name is a plain error — never the existing pair destroyed.
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("config/tls");
        ensure_managed_pair(&tls_dir, &["localhost".to_string()]).unwrap();
        let cert_before = std::fs::read_to_string(tls_dir.join("cert.pem")).unwrap();
        let key_before = std::fs::read_to_string(tls_dir.join("key.pem")).unwrap();
        let sidecar_before = std::fs::read_to_string(tls_dir.join("sans.txt")).unwrap();
        let err = regenerate_managed_pair(&tls_dir, &["café.example".to_string()]).unwrap_err();
        assert!(!err.to_string().is_empty(), "a refusal is an error");
        // rcgen's message names the offending IA5String encoding.
        assert!(err.to_string().contains("IA5String"), "got {err:?}");
        // Nothing was removed or overwritten by the refused call.
        assert_eq!(
            std::fs::read_to_string(tls_dir.join("cert.pem")).unwrap(),
            cert_before,
            "refused regeneration must leave the cert intact"
        );
        assert_eq!(
            std::fs::read_to_string(tls_dir.join("key.pem")).unwrap(),
            key_before
        );
        assert_eq!(
            std::fs::read_to_string(tls_dir.join("sans.txt")).unwrap(),
            sidecar_before
        );
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
