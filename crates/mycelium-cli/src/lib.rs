//! Library surface of the admin CLI: the operation behind the
//! `skill-export` subcommand, extracted so tests can drive it directly
//! against a temp data dir (the crate's tests are in-module; the
//! binary stays thin — flag parsing and dispatch only).

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Export a nested skill from the global skills shelf as an
/// install-ready directory tree (`SKILL.md`, companions, scripts) at
/// `out`.
///
/// Fail-fast and side-effect-free on error: the slug must be a single
/// path segment, `out` must not already exist, and nothing is written
/// until the whole bundle has assembled and md5-verified — a bad slug,
/// a missing service key (load-only: export never creates one), a
/// legacy flat slug, a traversal manifest, or an md5 mismatch leaves
/// no partial tree behind. Written paths are returned (and written) in
/// the assembler's deterministic bundle order. Permissions are
/// explicit and umask-proof: directories 0755, files 0644, `.sh`
/// scripts 0755 (skills run their scripts).
pub async fn export_skill(data_dir: &Path, out: &Path, slug: &str) -> anyhow::Result<Vec<PathBuf>> {
    // A slug is one path segment: it names a top-level skill dir in
    // the shelf, so anything that could escape or misroute it
    // (separators, traversal, empty) is rejected up-front — before
    // any store work.
    if slug.is_empty()
        || slug == "."
        || slug == ".."
        || slug.contains('/')
        || slug.contains('\\')
        || slug.contains('\0')
    {
        anyhow::bail!(
            "invalid skill slug {slug:?}: must be a single path segment (no '/', '\\', '..', or empty)"
        );
    }
    // Fresh output only: an existing directory (or file) is refused,
    // never merged into — a half-written tree from a previous run must
    // not be silently patched.
    if out.exists() {
        anyhow::bail!("output path already exists: {}", out.display());
    }
    // Load-only: export must never MINT a service key — creating one
    // is the server's first-boot job, and a fresh key here would
    // decrypt every payload to wrong bytes (md5 mismatch at best).
    let key = mycelium_crypto::load_service_key(data_dir)?;
    let store = mycelium_store::Store::open(data_dir).await?;
    let cs = mycelium_store::ConceptStore::for_service(
        &store,
        key.clone(),
        &store.skills_dir(),
        "skills",
    );
    let repo = mycelium_store::FileRepo::new(store.skills_dir());
    let scope = mycelium_store::Scope::Service(key);
    // Assemble and verify the WHOLE bundle before the first write: a
    // flat slug (NotBundle), a traversal manifest, or an md5
    // mismatch leaves no partial tree behind. The file order is the
    // assembler's deterministic bundle order, and rel paths are
    // pre-validated there (never re-derived here).
    let files = mycelium_store::bundle_skill_files(&cs, &repo, &scope, slug).await?;

    // The tree is entirely ours (`out` was absent a moment ago), so
    // explicit umask-proof permissions are safe to set. Ancestors of
    // `out` are created by create_dir_all but left at their default
    // permissions — they are not part of the export.
    std::fs::create_dir_all(out)?;
    let mut dirs: Vec<PathBuf> = vec![out.to_path_buf()];
    let mut seen: HashSet<PathBuf> = HashSet::from([out.to_path_buf()]);
    for f in &files {
        let mut dir = out.to_path_buf();
        if let Some(parent) = Path::new(&f.rel_path).parent() {
            for seg in parent.components() {
                dir = dir.join(seg.as_os_str());
                if seen.insert(dir.clone()) {
                    dirs.push(dir.clone());
                }
            }
        }
    }
    for dir in &dirs {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    }

    let mut written = Vec::with_capacity(files.len());
    for f in &files {
        let path = out.join(&f.rel_path);
        std::fs::write(&path, &f.bytes)?;
        // `.sh` scripts are executable (skills run their scripts);
        // every other file is a plain readable document.
        let mode = if f.rel_path.ends_with(".sh") {
            0o755
        } else {
            0o644
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
        written.push(path);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    use md5::{Digest, Md5};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// The process environment is shared mutable state: every
    /// `export_skill` call reads `MYCELIUM2_SERVICE_KEY` (via the
    /// load-only loader), and each test CLEARS that variable —
    /// unsound with in-flight reads on other threads. Each test holds
    /// this lock for its whole body (across awaits), so every env
    /// access in this suite is serialized. A tokio Mutex on purpose:
    /// holding a std MutexGuard across an await is unsound
    /// (clippy await_holding_lock).
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Take the suite's env lock, and clear the service-key override:
    /// the loader reads the env var FIRST, so an ambient
    /// `MYCELIUM2_SERVICE_KEY` in a developer's shell would otherwise
    /// have every test decrypting with the wrong key. Cleared under
    /// the lock, every test then exercises the key-FILE path
    /// regardless of ambient state.
    async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
        let guard = ENV_LOCK.lock().await;
        // SAFETY: serialized by ENV_LOCK — every env access in this
        // suite happens while the lock is held.
        unsafe { std::env::remove_var("MYCELIUM2_SERVICE_KEY") };
        guard
    }

    /// Boot a seeded data dir: temp store + service key + the packaged
    /// skills (the production first-boot sequence, minus the server).
    /// Mirrors the web skill-bundle integration harness.
    async fn seeded() -> (
        tempfile::TempDir,
        mycelium_store::Store,
        mycelium_crypto::ServiceKey,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = mycelium_store::Store::open(dir.path()).await.unwrap();
        let key = mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();
        mycelium_web::packaged_skills::seed_packaged_skills(&store, &key, &store.skills_dir())
            .await
            .unwrap();
        (dir, store, key)
    }

    /// md5 hex digest (the workspace idiom: md-5 + hex crates).
    fn md5_hex(bytes: &[u8]) -> String {
        let mut h = Md5::new();
        h.update(bytes);
        hex::encode(h.finalize())
    }

    /// Permission bits (masked to 0777) of a path on disk.
    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The full contract: a seeded packaged skill exports to an
    /// install directory with the assembler's deterministic order,
    /// byte-faithful payloads (md5 against the WRITTEN manifest, not a
    /// duplicated constant), and the pinned permission scheme.
    #[tokio::test]
    async fn export_writes_skill_layout_to_disk() {
        let _env = env_lock().await;
        let (dir, _store, _key) = seeded().await;
        let out = dir.path().join("out/pdf-to-markdown");
        let written = export_skill(dir.path(), &out, "pdf-to-markdown")
            .await
            .unwrap();

        // Written and returned in the assembler's deterministic order.
        let rel: Vec<String> = written
            .iter()
            .map(|p| p.strip_prefix(&out).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            rel,
            [
                "SKILL.md",
                "conventions.md",
                "references/docling-options.md",
                "LICENSE.txt",
                "scripts/convert.py",
                "scripts/docling_page_span.py",
                "scripts/html_cleanup.py",
                "scripts/inspect_pdf.py",
                "scripts/postprocess.py",
                "scripts/requirements.txt",
                "scripts/setup_venv.sh",
            ]
        );
        assert!(out.join("SKILL.md").is_file());
        assert!(out.join("scripts/convert.py").is_file());

        // Byte-faithful payload: the written script's md5 equals the
        // digest declared in the WRITTEN hub manifest — the manifest is
        // the oracle, no constant is duplicated here.
        let hub = fs::read_to_string(out.join("SKILL.md")).unwrap();
        let concept =
            mycelium_core::concept::Concept::parse("/pdf-to-markdown/skill.md", &hub).unwrap();
        let expected = concept
            .frontmatter
            .skill
            .as_ref()
            .expect("hub carries a manifest")
            .files
            .iter()
            .find(|f| f.path == "scripts/convert.py")
            .and_then(|f| f.md5.as_deref())
            .expect("manifest declares convert.py's md5");
        let written_py = fs::read(out.join("scripts/convert.py")).unwrap();
        assert_eq!(md5_hex(&written_py), expected);

        // Umask-proof permissions: dirs 0755, files 0644, `.sh` 0755.
        assert_eq!(mode(&out), 0o755);
        assert_eq!(mode(&out.join("scripts")), 0o755);
        assert_eq!(mode(&out.join("references")), 0o755);
        assert_eq!(mode(&out.join("SKILL.md")), 0o644);
        assert_eq!(mode(&out.join("scripts/convert.py")), 0o644);
        assert_eq!(mode(&out.join("scripts/setup_venv.sh")), 0o755);
    }

    /// A legacy flat skill (root-level concept, no hub) is not
    /// bundleable, and hostile slugs (multi-segment, traversal, empty)
    /// are rejected up-front — neither ever touches the output path.
    #[tokio::test]
    async fn export_refuses_flat_and_bad_slugs() {
        let _env = env_lock().await;
        let (dir, store, key) = seeded().await;
        let cs =
            mycelium_store::ConceptStore::for_service(&store, key, &store.skills_dir(), "skills");
        cs.put(
            &mycelium_core::concept::Concept::parse(
                "/my-flat-skill.md",
                "---\ntype: Skill\ntitle: my-flat-skill\n---\n\nlegacy flat skill",
            )
            .unwrap(),
        )
        .await
        .unwrap();

        let out = dir.path().join("out/my-flat-skill");
        let err = export_skill(dir.path(), &out, "my-flat-skill")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("not a nested skill bundle"),
            "{err}"
        );
        assert!(!out.exists(), "a refused export writes nothing");

        for slug in ["foo/bar", "foo\\bar", "..", ".", ""] {
            let err = export_skill(dir.path(), &dir.path().join("out/x"), slug)
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("invalid skill slug"),
                "{slug:?}: {err}"
            );
        }
    }

    /// Export is load-only: a data dir without a service key is an
    /// error, and the failure must not create one (that is the
    /// server's first-boot job).
    #[tokio::test]
    async fn export_fails_when_service_key_absent() {
        // env_lock() has already cleared any ambient
        // MYCELIUM2_SERVICE_KEY override, so the loader must find the
        // key FILE — and there is none.
        let _env = env_lock().await;
        let dir = tempfile::tempdir().unwrap();
        let _store = mycelium_store::Store::open(dir.path()).await.unwrap();
        let out = dir.path().join("out/pdf-to-markdown");
        let err = export_skill(dir.path(), &out, "pdf-to-markdown")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("service key"), "{err}");
        assert!(
            !dir.path().join("config/service.key").exists(),
            "export must never create a service key"
        );
        assert!(!out.exists());
    }

    /// The output directory must be fresh: an existing directory (or
    /// file) at the output path is refused, never merged into.
    #[tokio::test]
    async fn export_refuses_existing_output_directory() {
        let _env = env_lock().await;
        let (dir, _store, _key) = seeded().await;
        let out = dir.path().join("out/pdf-to-markdown");
        fs::create_dir_all(&out).unwrap();
        let err = export_skill(dir.path(), &out, "pdf-to-markdown")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        assert!(
            out.read_dir().unwrap().next().is_none(),
            "the existing directory is left untouched"
        );

        // A file squatting on the output path is refused the same way.
        let squat = dir.path().join("out/as-file");
        fs::write(&squat, b"occupied").unwrap();
        let err = export_skill(dir.path(), &squat, "pdf-to-markdown")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }

    /// A tampered payload (bytes no longer matching the manifest's
    /// declared md5) refuses to export — nothing partial is written.
    #[tokio::test]
    async fn export_refuses_payload_md5_mismatch() {
        let _env = env_lock().await;
        let (dir, store, key) = seeded().await;
        // Overwrite one payload in place, in the same scope the
        // export reads — the manifest still declares the real md5.
        mycelium_store::FileRepo::new(store.skills_dir())
            .write(
                "/pdf-to-markdown/scripts/convert.py",
                b"print('tampered')\n",
                &mycelium_store::Scope::Service(key),
            )
            .await
            .unwrap();
        let out = dir.path().join("out/pdf-to-markdown");
        let err = export_skill(dir.path(), &out, "pdf-to-markdown")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("md5 mismatch"), "{err}");
        assert!(!out.exists(), "a refused export writes nothing");
    }

    /// A hostile hub (manifest path escaping the skill dir) never
    /// yields a tree on disk — the assembler rejects it before the
    /// first write.
    #[tokio::test]
    async fn export_refuses_traversal_manifest() {
        let _env = env_lock().await;
        let (dir, store, key) = seeded().await;
        let cs =
            mycelium_store::ConceptStore::for_service(&store, key, &store.skills_dir(), "skills");
        cs.put(
            &mycelium_core::concept::Concept::parse(
                "/evil/skill.md",
                "---\ntype: Skill\ntitle: evil\nskill:\n  version: 1\n  files:\n    - {path: ../escape, role: script}\n---\n\nHostile hub.",
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let out = dir.path().join("out/evil");
        let err = export_skill(dir.path(), &out, "evil").await.unwrap_err();
        assert!(err.to_string().contains("invalid skill manifest"), "{err}");
        assert!(!out.exists(), "a refused export writes nothing");
    }
}
