//! Packaged skills: skills shipped with the system and seeded into the
//! global skills shelf on boot. Mirrors the assets scaffold contract
//! (`assets::scaffold_defaults`): first boot writes everything; the
//! version marker decides refresh; admin edits survive between bumps.

use std::path::Path;

use mycelium_core::concept::Concept;
use mycelium_crypto::keys::ServiceKey;
use mycelium_store::{ConceptStore, Store};

/// The packaged skills' content version. Bumped when the packaged skill
/// content changes; a stale (or missing) marker triggers a full refresh
/// of the packaged paths — extra admin-created skills are never touched.
pub const SKILLS_SEED_VERSION: &str = "1";

/// The packaged skill concepts: (repo filename, full OKF file contents),
/// embedded at compile time. Shelf paths are `/<filename>`. This is the
/// byte-exact, md5-verified packaging of the pdf-to-markdown
/// book-conversion skill.
pub const PACKAGED_SKILLS: &[(&str, &str)] = &[
    (
        "pdf-to-markdown.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown.md"),
    ),
    (
        "pdf-to-markdown-conventions.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown-conventions.md"),
    ),
    (
        "pdf-to-markdown-scripts.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown-scripts.md"),
    ),
    (
        "pdf-to-markdown-script-convert.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown-script-convert.md"),
    ),
    (
        "pdf-to-markdown-script-postprocess.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown-script-postprocess.md"),
    ),
    (
        "pdf-to-markdown-script-docling-page-span.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown-script-docling-page-span.md"),
    ),
    (
        "pdf-to-markdown-script-html-cleanup.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown-script-html-cleanup.md"),
    ),
    (
        "pdf-to-markdown-script-inspect-pdf.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown-script-inspect-pdf.md"),
    ),
    (
        "pdf-to-markdown-script-setup-venv.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown-script-setup-venv.md"),
    ),
    (
        "pdf-to-markdown-script-requirements.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown-script-requirements.md"),
    ),
    (
        "pdf-to-markdown-docling-options.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown-docling-options.md"),
    ),
    (
        "pdf-to-markdown-license.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown-license.md"),
    ),
];

/// Seed the packaged skills into the global skills shelf. A missing or
/// stale `.seed-version` marker re-puts every packaged concept (one
/// batched write, one index.md regeneration) and writes the marker; a
/// matching marker is a no-op. Extra admin-created skills are never
/// touched. Errors propagate (fail-fast, like `assets::scaffold_defaults`).
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
    let mut concepts = Vec::with_capacity(PACKAGED_SKILLS.len());
    for (name, contents) in PACKAGED_SKILLS {
        concepts.push(Concept::parse(&format!("/{name}"), contents)?);
    }
    let cs = ConceptStore::for_service(store, service_key.clone(), skills_dir, "skills");
    cs.put_batch(&concepts).await?;
    std::fs::create_dir_all(skills_dir)?;
    std::fs::write(&marker, SKILLS_SEED_VERSION)?;
    Ok(())
}
