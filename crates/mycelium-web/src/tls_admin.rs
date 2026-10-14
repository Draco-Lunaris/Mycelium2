//! TLS admin operations: SAN configuration, cert display parsing,
//! third-party cert upload validation, CSR generation, and the
//! pending-CSR lifecycle. See docs/superpowers/specs/2026-10-14-tls-management-design.md.

use sha2::{Digest, Sha256};

pub const DEFAULT_SAN: &str = "localhost";

/// Errors from the TLS admin pipeline (upload validation; Task 5's CSR
/// flow extends this enum). The Display strings are user-visible: they
/// are flashed verbatim on the /admin/system TLS card.
#[derive(Debug, thiserror::Error)]
pub enum TlsAdminError {
    #[error("certificate PEM could not be parsed: {0}")]
    CertParse(String),
    #[error("private key PEM could not be parsed: {0}")]
    KeyParse(String),
    #[error("the private key does not match the certificate's public key")]
    KeyMismatch,
    #[error(
        "the uploaded key does not match the pending CSR key — the certificate was issued for a different key"
    )]
    PendingKeyMismatch,
}

/// The result of validating an uploaded cert/key pair: display strings
/// for the flash + sidecar, and the warn-only findings (Decision 2).
#[derive(Debug)]
pub struct UploadValidation {
    pub not_before: String,
    pub not_after: String,
    pub fingerprint_sha256: String,
    pub warnings: Vec<String>,
}

/// Validate an uploaded cert(+chain)/key pair. Pure: no I/O.
/// `pending_key` is the outstanding CSR's key PEM when a pending CSR
/// exists (Decision 1: the uploaded key must be exactly that key).
///
/// The leaf is the FIRST `CERTIFICATE` block of the PEM (a real CA chain
/// is leaf-first, RFC 4366-style ordering); the remaining blocks are the
/// issuer chain and are not validated here — rustls parses the full file
/// at load/reload time, and a chain that cannot load fails the reload
/// (the handler then rolls the write back).
pub fn validate_upload(
    cert_pem: &str,
    key_pem: &str,
    pending_key: Option<&str>,
) -> Result<UploadValidation, TlsAdminError> {
    // Decode every PEM block with x509-parser's own PEM reader (base64 +
    // labels handled); the leaf is the FIRST block that parses as an
    // X.509 certificate. Blocks that are not certificates (a stray key
    // block, garbage) are skipped — "no CERTIFICATE block found" is then
    // the user-facing error. The leaf's DER is kept for the fingerprint.
    let blocks: Vec<Vec<u8>> = x509_parser::pem::Pem::iter_from_buffer(cert_pem.as_bytes())
        .filter_map(Result::ok)
        .map(|block| block.contents)
        .collect();
    let mut leaf: Option<(&[u8], x509_parser::certificate::X509Certificate<'_>)> = None;
    for der in &blocks {
        if let Ok((_, cert)) = x509_parser::parse_x509_certificate(der) {
            leaf = Some((der.as_slice(), cert));
            break;
        }
    }
    let (leaf_der, leaf) =
        leaf.ok_or_else(|| TlsAdminError::CertParse("no CERTIFICATE block found".to_string()))?;
    // Key: rcgen KeyPair::from_pem parses PKCS#8 private PEMs (ECDSA,
    // Ed25519, RSA — whatever the ring provider can load).
    let key =
        rcgen::KeyPair::from_pem(key_pem).map_err(|e| TlsAdminError::KeyParse(e.to_string()))?;
    // Cert/key match: compare SPKI DER to SPKI DER (both sides are the
    // RFC 5280 SubjectPublicKeyInfo encoding — see spki_matches).
    if !spki_matches(leaf.public_key().raw, &key) {
        return Err(TlsAdminError::KeyMismatch);
    }
    // Pending-key exact-match (Decision 1 flow): the uploaded key must
    // be the CSR's key, byte for byte after PKCS#8 re-serialization.
    if let Some(pending) = pending_key {
        let pending_kp = rcgen::KeyPair::from_pem(pending)
            .map_err(|e| TlsAdminError::KeyParse(format!("pending CSR key: {e}")))?;
        if pending_kp.serialize_pem() != key.serialize_pem() {
            return Err(TlsAdminError::PendingKeyMismatch);
        }
    }
    // Expiry: warn-only (Decision 2) — an expired cert still installs,
    // with notAfter in the flash.
    let mut warnings = Vec::new();
    let now = time::OffsetDateTime::now_utc();
    let not_after = leaf.validity().not_after.to_datetime();
    if not_after < now {
        warnings.push(format!("certificate expired on {not_after}"));
    } else if not_after - now < time::Duration::days(30) {
        warnings.push(format!("certificate expires soon: {not_after}"));
    }
    // Fingerprint: SHA-256 over the leaf certificate's DER — the same
    // material `openssl x509 -fingerprint -sha256` reports, so admins
    // can cross-check the CA's published fingerprint.
    let fingerprint = hex::encode(Sha256::digest(leaf_der));
    Ok(UploadValidation {
        not_before: leaf.validity().not_before.to_string(),
        not_after: leaf.validity().not_after.to_string(),
        fingerprint_sha256: fingerprint,
        warnings,
    })
}

/// Cert/key public-key match: `cert_spki` (the leaf's
/// SubjectPublicKeyInfo DER, x509-parser's `public_key().raw`) against
/// the key pair's own SPKI DER (`rcgen::KeyPair::public_key_der()` —
/// the same RFC 5280 §4.1 encoding, produced by the very code that
/// emitted the cert in the common case). If the encodings ever diverge
/// in shape (a third-party cert whose DER differs from rcgen's
/// re-encoding), fall back to comparing the SPKI's BIT STRING payload
/// against `public_key_raw()` (the raw public-key bytes).
fn spki_matches(cert_spki: &[u8], key: &rcgen::KeyPair) -> bool {
    use x509_parser::der_parser::asn1_rs::FromDer;
    if cert_spki == key.public_key_der() {
        return true;
    }
    match x509_parser::x509::SubjectPublicKeyInfo::from_der(cert_spki) {
        // The cert's SPKI re-encoded differently: compare the public-key
        // BIT STRING payload against the key pair's raw public bytes.
        Ok((_, spki)) => spki.subject_public_key.data.as_ref() == key.public_key_raw(),
        Err(_) => false,
    }
}

/// The three files an installed upload owns, in write order: the cert
/// (+chain), the 0600 key, and the sidecar. One list so every failure
/// path removes exactly the same set — a leftover `uploaded-cert.pem`
/// would otherwise be served by the download handler (it prefers that
/// file whenever it exists) and a leftover key is a stale secret.
pub fn uploaded_pair_paths(tls_dir: &std::path::Path) -> [std::path::PathBuf; 3] {
    [
        tls_dir.join("uploaded-cert.pem"),
        tls_dir.join("uploaded-key.pem"),
        tls_dir.join("uploaded.json"),
    ]
}

/// Remove every uploaded-pair file. Missing files are fine (a failure
/// can leave any subset). Returns the paths whose removal failed for
/// another reason — the caller logs those; a leftover is an operator
/// problem, never a user-facing one.
pub fn remove_uploaded_pair(tls_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    uploaded_pair_paths(tls_dir)
        .into_iter()
        .filter(|path| match std::fs::remove_file(path) {
            Ok(()) => false,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => true,
        })
        .collect()
}

/// Read the pending CSR key (Decision 1). `Ok(None)` only when no CSR is
/// pending (the file is absent); `Err` when the file exists but cannot be
/// read — the caller must fail closed, because treating an unreadable
/// pending key as "none pending" would silently skip the one check that
/// binds the upload to the key the CSR was generated with.
pub fn read_pending_csr_key(tls_dir: &std::path::Path) -> Result<Option<String>, std::io::Error> {
    match std::fs::read_to_string(tls_dir.join("pending-csr-key.pem")) {
        Ok(pem) => Ok(Some(pem)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Parse the MYCELIUM2_TLS_SANS / --tls-sans value: comma-separated,
/// trimmed, empties dropped, order-stable dedup. None/empty → the
/// `localhost` default; a configured set is returned verbatim.
pub fn sans_from_env(raw: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.into_iter().flat_map(|raw| raw.split(',')) {
        let s = part.trim();
        if !s.is_empty() && !out.iter().any(|e| e == s) {
            out.push(s.to_string());
        }
    }
    if out.is_empty() {
        out.push(DEFAULT_SAN.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sans_none_is_localhost_only() {
        assert_eq!(sans_from_env(None), vec!["localhost"]);
    }

    #[test]
    fn sans_split_trim_dedup() {
        assert_eq!(
            sans_from_env(Some(
                "mycelium2.example, 192.0.2.10 ,localhost, mycelium2.example"
            )),
            vec!["mycelium2.example", "192.0.2.10", "localhost"]
        );
    }

    #[test]
    fn sans_set_taken_literally_no_forced_additions() {
        // Environment-agnostic (public-repo rule): a configured SAN set is
        // the operator's, verbatim — never force-merged with anything.
        assert_eq!(sans_from_env(Some("example.com")), vec!["example.com"]);
    }

    #[test]
    fn sans_empty_entries_dropped() {
        assert_eq!(sans_from_env(Some(" , ,a,, b,")), vec!["a", "b"]);
    }

    #[test]
    fn sans_empty_string_is_localhost_only() {
        // An empty value (env var set to "") is the unset case: the
        // localhost default, not an empty verbatim list.
        assert_eq!(sans_from_env(Some("")), vec!["localhost"]);
        assert_eq!(sans_from_env(Some(" , ,")), vec!["localhost"]);
    }

    /// A self-signed cert/key pair for `san` (rcgen as a stand-in for a
    /// CA-issued pair: same PEM shapes, same key-match math).
    fn test_pair() -> (String, String) {
        test_pair_alg(&rcgen::PKCS_ECDSA_P256_SHA256, "upload.example")
    }

    fn test_pair_alg(alg: &'static rcgen::SignatureAlgorithm, san: &str) -> (String, String) {
        let kp = rcgen::KeyPair::generate_for(alg).unwrap();
        let mut params = rcgen::CertificateParams::new(vec![san.to_string()]).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, san);
        let cert = params.self_signed(&kp).unwrap();
        (cert.pem(), kp.serialize_pem())
    }

    #[test]
    fn upload_validation_accepts_matching_pair() {
        let (cert, key) = test_pair();
        let v = validate_upload(&cert, &key, None).unwrap();
        assert_eq!(v.fingerprint_sha256.len(), 64);
        assert!(!v.not_after.is_empty());
        assert!(!v.not_before.is_empty());
        // A fresh 90-day rcgen cert carries no warnings.
        assert!(v.warnings.is_empty(), "{:?}", v.warnings);
    }

    #[test]
    fn upload_validation_rejects_mismatched_key() {
        let (cert, _key) = test_pair();
        let other = rcgen::KeyPair::generate().unwrap();
        let err = validate_upload(&cert, &other.serialize_pem(), None).unwrap_err();
        assert!(matches!(err, TlsAdminError::KeyMismatch));
    }

    #[test]
    fn upload_validation_rejects_garbage() {
        let err = validate_upload("not a pem", "also not", None).unwrap_err();
        assert!(matches!(err, TlsAdminError::CertParse(_)), "got {err:?}");
        // A parseable cert with a garbage key is a KEY parse error.
        let (cert, _key) = test_pair();
        let err = validate_upload(&cert, "also not", None).unwrap_err();
        assert!(matches!(err, TlsAdminError::KeyParse(_)), "got {err:?}");
    }

    #[test]
    fn upload_validation_pending_key_match_and_cross_pair() {
        let (cert, key) = test_pair();
        // Pending key = the CSR key; cert matches → ok, cleared.
        validate_upload(&cert, &key, Some(&key)).unwrap();
        // Pending key differs from the uploaded key → explicit error.
        let other = rcgen::KeyPair::generate().unwrap();
        let err = validate_upload(&cert, &key, Some(&other.serialize_pem())).unwrap_err();
        assert!(
            matches!(err, TlsAdminError::PendingKeyMismatch),
            "got {err:?}"
        );
    }

    #[test]
    fn upload_validation_parses_leaf_from_chain() {
        let (leaf, key) = test_pair();
        let (inter, _k2) = test_pair(); // stand-in second block
        let chain = format!("{leaf}{inter}");
        let v = validate_upload(&chain, &key, None).unwrap();
        assert!(!v.fingerprint_sha256.is_empty());
        // The fingerprint is the LEAF's, not the chain's: it equals the
        // SHA-256 of the leaf's DER (what openssl x509 -fingerprint shows).
        let leaf_der = x509_parser::pem::Pem::iter_from_buffer(leaf.as_bytes())
            .next()
            .unwrap()
            .unwrap()
            .contents;
        let expected = hex::encode(Sha256::digest(&leaf_der));
        assert_eq!(v.fingerprint_sha256, expected);
    }

    #[test]
    fn upload_validation_expired_cert_warns_not_rejects() {
        let kp = rcgen::KeyPair::generate().unwrap();
        let mut params =
            rcgen::CertificateParams::new(vec!["expired.example".to_string()]).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "expired.example");
        params.not_before = time::OffsetDateTime::UNIX_EPOCH;
        params.not_after = time::OffsetDateTime::from_unix_timestamp(1).unwrap();
        let cert = params.self_signed(&kp).unwrap();
        let v = validate_upload(&cert.pem(), &kp.serialize_pem(), None).unwrap();
        assert!(
            v.warnings.iter().any(|w| w.contains("expired")),
            "{:?}",
            v.warnings
        );
    }

    #[test]
    fn upload_validation_expires_soon_warns_not_rejects() {
        let kp = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["soon.example".to_string()]).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "soon.example");
        params.not_before = time::OffsetDateTime::now_utc();
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(10);
        let cert = params.self_signed(&kp).unwrap();
        let v = validate_upload(&cert.pem(), &kp.serialize_pem(), None).unwrap();
        assert!(
            v.warnings.iter().any(|w| w.contains("expires soon")),
            "{:?}",
            v.warnings
        );
    }

    #[test]
    fn upload_validation_accepts_ed25519_pair() {
        // A second key type pins the SPKI comparison across types
        // (rcgen's ring provider cannot generate RSA keys, so Ed25519
        // is the cheap cross-type fixture).
        let (cert, key) = test_pair_alg(&rcgen::PKCS_ED25519, "ed.example");
        let v = validate_upload(&cert, &key, None).unwrap();
        assert_eq!(v.fingerprint_sha256.len(), 64);
        // And a mismatched Ed25519 key is still a KeyMismatch.
        let other = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let err = validate_upload(&cert, &other.serialize_pem(), None).unwrap_err();
        assert!(matches!(err, TlsAdminError::KeyMismatch));
    }

    #[test]
    fn uploaded_pair_paths_are_the_three_names() {
        // The names are a contract with the download handler and boot
        // precedence (Task 2/3), so they are pinned here, not just used.
        let paths = uploaded_pair_paths(std::path::Path::new("/d/config/tls"));
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["uploaded-cert.pem", "uploaded-key.pem", "uploaded.json"]
        );
    }

    #[test]
    fn remove_uploaded_pair_removes_exactly_its_three_files() {
        // The rollback's file set is the whole guarantee (Review I-2):
        // every failure exit must leave NOTHING of the upload, and must
        // not touch anything else in the TLS dir — the managed pair and
        // the pending CSR live there too.
        let dir = tempfile::tempdir().unwrap();
        let tls = dir.path();
        let upload = ["uploaded-cert.pem", "uploaded-key.pem", "uploaded.json"];
        let keep = ["cert.pem", "key.pem", "pending-csr-key.pem", "pending.json"];
        for name in upload.iter().chain(keep.iter()) {
            std::fs::write(tls.join(name), "x").unwrap();
        }
        let leftovers = remove_uploaded_pair(tls);
        assert!(leftovers.is_empty(), "unexpected leftovers: {leftovers:?}");
        for name in upload {
            assert!(!tls.join(name).exists(), "{name} must be gone");
        }
        for name in keep {
            assert!(tls.join(name).exists(), "{name} must survive");
        }
        // Idempotent: a partial set (any subset, or none) is not an error.
        std::fs::write(tls.join("uploaded.json"), "x").unwrap();
        assert!(remove_uploaded_pair(tls).is_empty());
        assert!(remove_uploaded_pair(tls).is_empty());
    }

    #[test]
    fn read_pending_csr_key_absent_is_none_but_unreadable_is_err() {
        // Review M-2: the pending-key check must never silently open.
        let dir = tempfile::tempdir().unwrap();
        let tls = dir.path();
        // Absent → Ok(None) ("no CSR pending").
        assert_eq!(read_pending_csr_key(tls).unwrap(), None);
        // Readable → Ok(Some(pem)).
        std::fs::write(tls.join("pending-csr-key.pem"), "-----BEGIN KEY-----\n").unwrap();
        assert!(
            read_pending_csr_key(tls)
                .unwrap()
                .unwrap()
                .contains("BEGIN KEY")
        );
        // Present but unreadable → Err (fail closed). Running as root
        // would defeat the chmod, so that half is skipped there.
        use std::os::unix::fs::PermissionsExt;
        if !running_as_root() {
            std::fs::set_permissions(
                tls.join("pending-csr-key.pem"),
                std::fs::Permissions::from_mode(0o000),
            )
            .expect("chmod fixture");
            let err = read_pending_csr_key(tls);
            assert!(err.is_err(), "unreadable pending key must fail closed");
            // Restore so the tempdir can be cleaned up.
            let _ = std::fs::set_permissions(
                tls.join("pending-csr-key.pem"),
                std::fs::Permissions::from_mode(0o600),
            );
        }
    }

    /// True when the effective uid is 0 (a chmod-000 file is readable).
    fn running_as_root() -> bool {
        // No libc dep in this crate: read the uid from /proc/self/status.
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("Uid:"))
                    .and_then(|l| l.split_whitespace().nth(1).map(|v| v == "0"))
            })
            .unwrap_or(false)
    }

    #[test]
    fn spki_matches_primary_and_fallback_paths() {
        let kp = rcgen::KeyPair::generate().unwrap();
        let spki = kp.public_key_der();
        // Primary path: identical SPKI DER.
        assert!(spki_matches(&spki, &kp));
        // Fallback path: a differently-encoded SPKI whose BIT STRING
        // payload is still the key's raw public bytes → matches. Built by
        // perturbing a byte inside the AlgorithmIdentifier's curve OID
        // (offset 20 of an ECDSA P-256 SPKI — never the trailing BIT
        // STRING, whose payload must stay intact, and never its
        // unused-bits byte). The two assertions below make the fixture
        // self-validating: DER differs, parsed BIT STRING does not.
        use x509_parser::der_parser::asn1_rs::FromDer;
        let mut perturbed = spki.clone();
        perturbed[20] ^= 0x01;
        assert_ne!(perturbed, spki, "fixture must differ in DER");
        let (_, p_spki) =
            x509_parser::x509::SubjectPublicKeyInfo::from_der(&perturbed).expect("still an SPKI");
        assert_eq!(
            p_spki.subject_public_key.data.as_ref(),
            kp.public_key_raw(),
            "fixture must keep the BIT STRING payload"
        );
        assert!(
            spki_matches(&perturbed, &kp),
            "BIT STRING fallback must match (spki layout: {spki:?})"
        );
        // A different key's SPKI never matches, and garbage never matches.
        let other = rcgen::KeyPair::generate().unwrap();
        assert!(!spki_matches(&other.public_key_der(), &kp));
        assert!(!spki_matches(b"garbage", &kp));
    }
}
