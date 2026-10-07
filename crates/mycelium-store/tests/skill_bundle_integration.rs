//! Task 7 integration tests: skill bundle file assembly (files, no
//! zip) — deterministic order, manifest md5 verification, missing
//! payloads, and traversal rejection, over a real booted skills scope.

use std::path::Path;

use mycelium_core::concept::{
    Concept, ConceptError, Frontmatter, SkillFile, SkillManifest, SkillRole,
};
use mycelium_crypto::keys::ServiceKey;
use mycelium_store::skill_bundle::{SkillBundleError, bundle_skill_files};
use mycelium_store::{ConceptStore, FileRepo, FileRepoError, Scope, Store};

/// Boot a fresh store + service key. The tempdir guard must stay bound
/// for the whole test.
async fn boot() -> (tempfile::TempDir, Store, ServiceKey) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let key = ServiceKey::from_bytes(&[7u8; 32]).unwrap();
    (dir, store, key)
}

/// The global skills scope the way the web layer opens it: the concept
/// store plus the raw payload repo over the same base directory (the
/// skills dir IS the scope's FileRepo base — the packaged-skills
/// precedent: concepts through `cs`, raw payloads through `repo`).
fn skills<'a>(
    store: &'a Store,
    svc: &ServiceKey,
    dir: &Path,
) -> (ConceptStore<'a>, FileRepo, Scope) {
    (
        ConceptStore::for_service(store, svc.clone(), dir, "skills"),
        FileRepo::new(dir),
        Scope::Service(svc.clone()),
    )
}

/// md5 hex digest (the workspace idiom: md-5 + hex crates).
fn md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// A plain companion concept at `path`.
fn concept(path: &str, title: &str) -> Concept {
    Concept::new(
        Frontmatter {
            concept_type: "Note".into(),
            title: Some(title.into()),
            ..Default::default()
        },
        format!("# {title}\n\ncompanion body"),
        path.into(),
    )
}

/// The `/<slug>/skill.md` hub, optionally carrying the bundle manifest.
fn hub(slug: &str, manifest: Option<SkillManifest>) -> Concept {
    Concept::new(
        Frontmatter {
            concept_type: "Skill".into(),
            title: Some(slug.into()),
            skill: manifest,
            ..Default::default()
        },
        format!("# {slug}\n\nhub body"),
        format!("/{slug}/skill.md"),
    )
}

fn manifest(files: Vec<SkillFile>) -> SkillManifest {
    SkillManifest { version: 1, files }
}

fn skill_file(path: &str, role: SkillRole, md5: Option<String>) -> SkillFile {
    SkillFile {
        path: path.into(),
        role,
        md5,
    }
}

#[tokio::test]
async fn assembles_deterministic_bundle_from_nested_skill() {
    let (dir, store, svc) = boot().await;
    let skills_dir = dir.path().join("skills");
    let (cs, repo, scope) = skills(&store, &svc, &skills_dir);

    let script = b"print('verify ok')\n".to_vec();
    let license = b"MIT License\n".to_vec();
    // The manifest mirrors the real pdf-to-markdown hub: companions as
    // reference entries (no md5 — they are concepts, not payloads),
    // scripts with declared md5s.
    let man = manifest(vec![
        skill_file("conventions.md", SkillRole::Reference, None),
        skill_file("references/docling-options.md", SkillRole::Reference, None),
        skill_file("LICENSE.txt", SkillRole::Reference, Some(md5_hex(&license))),
        skill_file(
            "scripts/verify.py",
            SkillRole::Script,
            Some(md5_hex(&script)),
        ),
    ]);
    cs.put(&hub("my-skill", Some(man))).await.unwrap();
    cs.put(&concept("/my-skill/conventions.md", "conventions"))
        .await
        .unwrap();
    cs.put(&concept(
        "/my-skill/references/docling-options.md",
        "docling options",
    ))
    .await
    .unwrap();
    // Raw payload writes (never concepts): the scripts the manifest names.
    repo.write("/my-skill/LICENSE.txt", &license, &scope)
        .await
        .unwrap();
    repo.write("/my-skill/scripts/verify.py", &script, &scope)
        .await
        .unwrap();

    let files = bundle_skill_files(&cs, &repo, &scope, "my-skill")
        .await
        .unwrap();
    let names: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
    // SKILL.md first, then companions sorted by rel path, then scripts
    // sorted by rel path. The manifest-named `conventions.md` is also a
    // grouping member — it appears exactly once.
    assert_eq!(
        names,
        vec![
            "SKILL.md",
            "conventions.md",
            "references/docling-options.md",
            "LICENSE.txt",
            "scripts/verify.py",
        ]
    );
    assert_eq!(names.iter().filter(|n| **n == "conventions.md").count(), 1);

    // SKILL.md carries the hub's markdown under Claude Code's entry
    // filename — the verbatim bytes the viewer renders.
    let stored_hub = cs.get("/my-skill/skill.md").await.unwrap();
    assert_eq!(files[0].bytes, stored_hub.to_markdown().unwrap().as_bytes());
    assert!(files[0].bytes.starts_with(b"---\n"));
    // Companions carry their concepts' markdown.
    let stored_conv = cs.get("/my-skill/conventions.md").await.unwrap();
    assert_eq!(
        files[1].bytes,
        stored_conv.to_markdown().unwrap().as_bytes()
    );
    // Scripts carry the raw payload bytes (the call succeeded, so every
    // declared md5 matched).
    let by_name = |p: &str| files.iter().find(|f| f.rel_path == p).unwrap();
    assert_eq!(by_name("scripts/verify.py").bytes, script);
    assert_eq!(by_name("LICENSE.txt").bytes, license);
}

#[tokio::test]
async fn md5_mismatch_fails_loudly() {
    let (dir, store, svc) = boot().await;
    let skills_dir = dir.path().join("skills");
    let (cs, repo, scope) = skills(&store, &svc, &skills_dir);

    // The manifest declares the md5 of DIFFERENT bytes than the payload.
    let man = manifest(vec![skill_file(
        "scripts/verify.py",
        SkillRole::Script,
        Some(md5_hex(b"not the payload\n")),
    )]);
    cs.put(&hub("my-skill", Some(man))).await.unwrap();
    repo.write("/my-skill/scripts/verify.py", b"print('ok')\n", &scope)
        .await
        .unwrap();

    let err = bundle_skill_files(&cs, &repo, &scope, "my-skill")
        .await
        .unwrap_err();
    assert!(
        matches!(err, SkillBundleError::ChecksumMismatch { path } if path == "scripts/verify.py")
    );
}

#[tokio::test]
async fn traversal_manifest_is_rejected() {
    let (dir, store, svc) = boot().await;
    let skills_dir = dir.path().join("skills");
    let (cs, repo, scope) = skills(&store, &svc, &skills_dir);

    // A manifest naming a path that escapes the skill dir — rejected
    // before any payload read.
    let man = manifest(vec![skill_file("../escape", SkillRole::Script, None)]);
    cs.put(&hub("evil", Some(man))).await.unwrap();

    let err = bundle_skill_files(&cs, &repo, &scope, "evil")
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        SkillBundleError::Traversal(ConceptError::InvalidSkillPath { path })
            if path == "../escape"
    ));
}

#[tokio::test]
async fn legacy_flat_slug_is_not_bundleable() {
    let (dir, store, svc) = boot().await;
    let skills_dir = dir.path().join("skills");
    let (cs, repo, scope) = skills(&store, &svc, &skills_dir);

    // Only the flat legacy concept exists — no /legacy-skill/skill.md.
    cs.put(&concept("/legacy-skill.md", "Legacy Skill"))
        .await
        .unwrap();

    let err = bundle_skill_files(&cs, &repo, &scope, "legacy-skill")
        .await
        .unwrap_err();
    assert!(matches!(err, SkillBundleError::NotBundle(slug) if slug == "legacy-skill"));
}

#[tokio::test]
async fn hub_without_manifest_bundles_companions_only() {
    // A hub with no `skill:` manifest is legal: companions only, no
    // payloads (the manifest is what makes scripts readable).
    let (dir, store, svc) = boot().await;
    let skills_dir = dir.path().join("skills");
    let (cs, repo, scope) = skills(&store, &svc, &skills_dir);

    cs.put(&hub("my-skill", None)).await.unwrap();
    cs.put(&concept("/my-skill/conventions.md", "conventions"))
        .await
        .unwrap();

    let files = bundle_skill_files(&cs, &repo, &scope, "my-skill")
        .await
        .unwrap();
    let names: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
    assert_eq!(names, vec!["SKILL.md", "conventions.md"]);
}

#[tokio::test]
async fn hub_with_empty_manifest_files_bundles_companions_only() {
    // An empty manifest file list is legal: validated trivially, no
    // scripts to read.
    let (dir, store, svc) = boot().await;
    let skills_dir = dir.path().join("skills");
    let (cs, repo, scope) = skills(&store, &svc, &skills_dir);

    cs.put(&hub("my-skill", Some(manifest(vec![]))))
        .await
        .unwrap();
    cs.put(&concept("/my-skill/conventions.md", "conventions"))
        .await
        .unwrap();

    let files = bundle_skill_files(&cs, &repo, &scope, "my-skill")
        .await
        .unwrap();
    let names: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
    assert_eq!(names, vec!["SKILL.md", "conventions.md"]);
}

#[tokio::test]
async fn hub_only_skill_bundles_skill_md_alone() {
    // A lone hub (a group with zero members) yields the single entry.
    let (dir, store, svc) = boot().await;
    let skills_dir = dir.path().join("skills");
    let (cs, repo, scope) = skills(&store, &svc, &skills_dir);

    cs.put(&hub("my-skill", None)).await.unwrap();

    let files = bundle_skill_files(&cs, &repo, &scope, "my-skill")
        .await
        .unwrap();
    let names: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
    assert_eq!(names, vec!["SKILL.md"]);
}

#[tokio::test]
async fn missing_payload_fails_loudly() {
    // A manifest entry naming a nonexistent payload is a
    // shelf-integrity problem — surfaced as Io (ruling: the #[from]
    // covers it; the endpoint turns it into a logged 500, the CLI into
    // a nonzero exit).
    let (dir, store, svc) = boot().await;
    let skills_dir = dir.path().join("skills");
    let (cs, repo, scope) = skills(&store, &svc, &skills_dir);

    let man = manifest(vec![skill_file(
        "scripts/ghost.py",
        SkillRole::Script,
        Some(md5_hex(b"never written\n")),
    )]);
    cs.put(&hub("my-skill", Some(man))).await.unwrap();
    // No repo.write for scripts/ghost.py — the payload does not exist.

    let err = bundle_skill_files(&cs, &repo, &scope, "my-skill")
        .await
        .unwrap_err();
    match err {
        SkillBundleError::Io(FileRepoError::Io(io)) => {
            assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("expected Io(NotFound), got {other:?}"),
    }
}

#[tokio::test]
async fn companion_with_traversal_path_is_rejected() {
    // Defense-in-depth: a registry entry whose path escapes the skill
    // dir (constructible — the write path validates only the leading
    // slash) is caught by the re-validation gate after
    // prefix-stripping, never silently emitted.
    let (dir, store, svc) = boot().await;
    let skills_dir = dir.path().join("skills");
    let (cs, repo, scope) = skills(&store, &svc, &skills_dir);

    cs.put(&hub("my-skill", None)).await.unwrap();
    cs.put(&concept("/my-skill/../escape.md", "escape"))
        .await
        .unwrap();

    let err = bundle_skill_files(&cs, &repo, &scope, "my-skill")
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        SkillBundleError::Traversal(ConceptError::InvalidSkillPath { path })
            if path == "../escape.md"
    ));
}
