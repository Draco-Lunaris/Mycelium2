//! Packaged skills: skills shipped with the system and seeded into the
//! global skills shelf on boot. Mirrors the assets scaffold contract
//! (`assets::scaffold_defaults`): first boot writes everything; the
//! version marker decides refresh; admin edits survive between bumps.
//!
//! Seed v2 installs the pdf-to-markdown skill as a nested bundle: the
//! hub (`/pdf-to-markdown/skill.md`, carrying the bundle manifest) and
//! its companion concepts under `/pdf-to-markdown/`, plus the scripts
//! and license as raw FileRepo payloads (never concepts). A live v1
//! shelf's flat `/pdf-to-markdown*.md` concepts are deleted
//! (idempotently) so the migration is clean.

use std::path::Path;

use mycelium_core::concept::{Concept, SkillManifest};
use mycelium_crypto::keys::ServiceKey;
use mycelium_store::{ConceptStore, ConceptStoreError, FileRepo, Scope, Store};

/// The packaged skills' content version. Bumped when the packaged skill
/// content changes; a stale (or missing) marker triggers a full refresh
/// of the packaged paths — extra admin-created skills are never touched.
pub const SKILLS_SEED_VERSION: &str = "2";

/// The packaged skill concepts in the nested layout: (repo-relative
/// path, full OKF file contents), embedded at compile time. Shelf paths
/// are `/<repo-relative path>`. The hub `skill.md` carries the bundle
/// manifest (file roles + md5s) in its frontmatter.
pub const PACKAGED_CONCEPTS: &[(&str, &str)] = &[
    (
        "pdf-to-markdown/skill.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown/skill.md"),
    ),
    (
        "pdf-to-markdown/conventions.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown/conventions.md"),
    ),
    (
        "pdf-to-markdown/references/docling-options.md",
        include_str!(
            "../packaged-skills/pdf-to-markdown/pdf-to-markdown/references/docling-options.md"
        ),
    ),
];

/// The packaged skill's raw script payload files — real bytes, never
/// concepts: (path relative to the skill dir, bytes). Paths match the
/// hub manifest's `skill.files[].path` entries exactly; `asset_tests`
/// pins each payload's md5 against the manifest.
pub const PACKAGED_SCRIPTS_PAYLOADS: &[(&str, &[u8])] = &[
    (
        "LICENSE.txt",
        include_bytes!("../packaged-skills/pdf-to-markdown/pdf-to-markdown/LICENSE.txt"),
    ),
    (
        "scripts/convert.py",
        include_bytes!("../packaged-skills/pdf-to-markdown/pdf-to-markdown/scripts/convert.py"),
    ),
    (
        "scripts/postprocess.py",
        include_bytes!("../packaged-skills/pdf-to-markdown/pdf-to-markdown/scripts/postprocess.py"),
    ),
    (
        "scripts/docling_page_span.py",
        include_bytes!(
            "../packaged-skills/pdf-to-markdown/pdf-to-markdown/scripts/docling_page_span.py"
        ),
    ),
    (
        "scripts/html_cleanup.py",
        include_bytes!(
            "../packaged-skills/pdf-to-markdown/pdf-to-markdown/scripts/html_cleanup.py"
        ),
    ),
    (
        "scripts/inspect_pdf.py",
        include_bytes!("../packaged-skills/pdf-to-markdown/pdf-to-markdown/scripts/inspect_pdf.py"),
    ),
    (
        "scripts/setup_venv.sh",
        include_bytes!("../packaged-skills/pdf-to-markdown/pdf-to-markdown/scripts/setup_venv.sh"),
    ),
    (
        "scripts/requirements.txt",
        include_bytes!(
            "../packaged-skills/pdf-to-markdown/pdf-to-markdown/scripts/requirements.txt"
        ),
    ),
];

/// Legacy flat paths from seed v1, deleted on v2 migration (idempotent
/// — deleting a path that no longer exists is fine).
const LEGACY_FLAT_PATHS: &[&str] = &[
    "/pdf-to-markdown.md",
    "/pdf-to-markdown-conventions.md",
    "/pdf-to-markdown-scripts.md",
    "/pdf-to-markdown-script-convert.md",
    "/pdf-to-markdown-script-postprocess.md",
    "/pdf-to-markdown-script-docling-page-span.md",
    "/pdf-to-markdown-script-html-cleanup.md",
    "/pdf-to-markdown-script-inspect-pdf.md",
    "/pdf-to-markdown-script-setup-venv.md",
    "/pdf-to-markdown-script-requirements.md",
    "/pdf-to-markdown-docling-options.md",
    "/pdf-to-markdown-license.md",
];

/// Seed the packaged skills into the global skills shelf. A missing or
/// stale `.seed-version` marker migrates to the nested layout: delete
/// every legacy flat concept (idempotent), re-put the packaged
/// concepts (one batched write, one index.md regeneration), write the
/// raw payload files under the skills scope's FileRepo, then write the
/// marker. A matching marker is a no-op. Extra admin-created skills are
/// never touched. Errors propagate (fail-fast, like
/// `assets::scaffold_defaults`) — the server never boots half-seeded.
pub async fn seed_packaged_skills(
    store: &Store,
    service_key: &ServiceKey,
    skills_dir: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let marker = skills_dir.join(".seed-version");
    let current = std::fs::read_to_string(&marker).unwrap_or_default();
    if current.trim() == SKILLS_SEED_VERSION {
        return Ok(());
    }
    // Concepts: parse validates frontmatter up-front (fail-fast before
    // any store write). Manifest paths validated so a bad embedded hub
    // can never seed (Review Focus #1).
    let concepts = PACKAGED_CONCEPTS
        .iter()
        .map(|(p, md)| Concept::parse(&format!("/{p}"), md))
        .collect::<Result<Vec<_>, _>>()?;
    for c in &concepts {
        c.frontmatter
            .skill
            .as_ref()
            .map(SkillManifest::validated)
            .transpose()?;
    }
    let cs = ConceptStore::for_service(store, service_key.clone(), skills_dir, "skills");
    for legacy in LEGACY_FLAT_PATHS {
        match cs.delete(legacy).await {
            Ok(()) | Err(ConceptStoreError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    cs.put_batch(&concepts).await?;
    // Payloads: raw FileRepo writes in the SKILLS scope (skills_dir IS
    // the scope's FileRepo base — index.md precedent — so no ConceptStore
    // call is involved). Payloads are NEVER concepts: they never enter
    // the registry or the search index.
    let repo = FileRepo::new(skills_dir);
    for (rel, bytes) in PACKAGED_SCRIPTS_PAYLOADS {
        repo.write(
            &format!("/pdf-to-markdown/{rel}"),
            bytes,
            &Scope::Service(service_key.clone()),
        )
        .await?;
    }
    std::fs::create_dir_all(skills_dir)?;
    std::fs::write(&marker, SKILLS_SEED_VERSION)?;
    Ok(())
}

#[cfg(test)]
mod asset_tests {
    use super::*;

    /// The embedded repo assets must match the hub manifest: every
    /// payload in `PACKAGED_SCRIPTS_PAYLOADS` md5s to its declared
    /// manifest value on the `skill.md` hub.
    #[test]
    fn embedded_payloads_match_hub_manifest_md5s() {
        use md5::{Digest, Md5};

        let (_, hub_md) = PACKAGED_CONCEPTS
            .iter()
            .find(|(p, _)| *p == "pdf-to-markdown/skill.md")
            .expect("hub present");
        let hub = mycelium_core::concept::Concept::parse("/pdf-to-markdown/skill.md", hub_md)
            .expect("hub parses");
        let man = hub
            .frontmatter
            .skill
            .as_ref()
            .expect("hub carries manifest");
        man.validated().expect("manifest paths valid");
        for f in &man.files {
            if f.md5.is_none() {
                continue;
            }
            let (_, bytes) = PACKAGED_SCRIPTS_PAYLOADS
                .iter()
                .find(|(p, _)| *p == f.path)
                .unwrap_or_else(|| panic!("payload {} present", f.path));
            let mut h = Md5::new();
            h.update(*bytes);
            let digest = hex::encode(h.finalize());
            assert_eq!(digest, f.md5.as_deref().unwrap(), "{}", f.path);
        }
    }

    /// Every packaged concept in the nested layout parses as a valid OKF
    /// concept (frontmatter + body) at its shelf path — catches a
    /// malformed companion at package time, not at seed time.
    #[test]
    fn all_packaged_concepts_parse() {
        for &(path, md) in PACKAGED_CONCEPTS {
            mycelium_core::concept::Concept::parse(&format!("/{path}"), md)
                .unwrap_or_else(|e| panic!("/{path} parses: {e}"));
        }
    }
}
