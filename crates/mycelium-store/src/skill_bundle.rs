//! Assemble a skill's installable file set. This module never touches
//! zip — it returns plain files; the web layer zips, the CLI writes
//! them to disk (keeps the zip dependency out of mycelium-store).

use std::collections::HashSet;

use md5::{Digest, Md5};
use mycelium_core::concept::{SkillFile, SkillManifest, SkillRole};
use thiserror::Error;

use crate::concept_store::{ConceptStore, ConceptStoreError};
use crate::file_repo::{FileRepo, FileRepoError, Scope};
use crate::skill_group::group_skills;

#[derive(Debug, Error)]
pub enum SkillBundleError {
    #[error("'{0}' is not a nested skill bundle (no {0}/skill.md)")]
    NotBundle(String),
    #[error("invalid skill manifest: {0}")]
    Traversal(#[from] mycelium_core::concept::ConceptError),
    #[error("payload md5 mismatch: {path}")]
    ChecksumMismatch { path: String },
    #[error(transparent)]
    Store(#[from] ConceptStoreError),
    #[error(transparent)]
    Io(#[from] FileRepoError),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SkillBundleFile {
    /// Path relative to the bundle root — `SKILL.md` (Claude Code's
    /// skill entry filename) or the manifest-declared companion/script path.
    pub rel_path: String,
    pub bytes: Vec<u8>,
}

/// md5 hex digest (the workspace idiom: md-5 + hex crates).
fn md5_hex(bytes: &[u8]) -> String {
    let mut h = Md5::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Re-check one produced rel path against the manifest path rules —
/// defense in depth after prefix-stripping: a registry or manifest path
/// that would escape the bundle root is rejected here, never silently
/// emitted as a bundle member.
fn ensure_safe_rel(rel: &str) -> Result<(), SkillBundleError> {
    SkillManifest {
        files: vec![SkillFile {
            path: rel.to_string(),
            role: SkillRole::Reference,
            md5: None,
        }],
        ..Default::default()
    }
    .validated()?;
    Ok(())
}

/// Assemble a nested skill's installable file set (plain files, no zip).
///
/// Reads the `/<slug>/skill.md` hub — missing → [`SkillBundleError::NotBundle`]
/// (a legacy flat skill is not bundleable). The hub's `skill:` manifest,
/// when present, is validated first and is authoritative for the
/// non-`.md` payloads (read as raw bytes from `payload_repo`, every
/// declared md5 verified); companion `.md` concepts come from the
/// structural grouping scan, bundled as their serialized markdown.
/// Output order is deterministic: `SKILL.md`, companions sorted by rel
/// path, then scripts sorted by rel path — and a rel path is never
/// emitted twice.
pub async fn bundle_skill_files(
    cs: &ConceptStore<'_>,
    payload_repo: &FileRepo,
    payload_scope: &Scope,
    slug: &str,
) -> Result<Vec<SkillBundleFile>, SkillBundleError> {
    // Hub: no /<slug>/skill.md → not a nested skill bundle.
    let hub = cs
        .get(&format!("/{slug}/skill.md"))
        .await
        .map_err(|e| match e {
            ConceptStoreError::NotFound(_) => SkillBundleError::NotBundle(slug.to_string()),
            other => other.into(),
        })?;

    // Manifest (authoritative for scripts): validated before any read —
    // a traversal path in the manifest never reaches the FileRepo.
    let manifest = hub.frontmatter.skill.clone();
    if let Some(man) = &manifest {
        man.validated()?;
    }

    // Companions: the structural grouping scan (not the manifest's .md
    // list — the registry is the source of truth for concepts).
    let listings = group_skills(&cs.list().await?);
    let mut companions: Vec<(String, Vec<u8>)> = Vec::new();
    if let Some(group) = listings.grouped.iter().find(|g| g.slug == slug) {
        let prefix = format!("/{slug}/");
        for member in &group.members {
            let rel = member
                .path
                .strip_prefix(&prefix)
                .unwrap_or(&member.path)
                .to_string();
            ensure_safe_rel(&rel)?;
            let concept = cs.get(&member.path).await?;
            companions.push((rel, concept.to_markdown()?.into_bytes()));
        }
    }
    companions.sort_by(|a, b| a.0.cmp(&b.0));

    // Scripts: manifest-declared non-.md files, read as raw payload
    // bytes (never concepts) and md5-verified when a digest is declared.
    let mut scripts: Vec<(String, Vec<u8>)> = Vec::new();
    if let Some(man) = &manifest {
        for f in &man.files {
            if f.path.ends_with(".md") {
                continue; // companion concepts come from the grouping scan
            }
            ensure_safe_rel(&f.path)?;
            let bytes = payload_repo
                .read(&format!("/{slug}/{}", f.path), payload_scope)
                .await?;
            if let Some(expected) = &f.md5
                && expected != &md5_hex(&bytes)
            {
                return Err(SkillBundleError::ChecksumMismatch {
                    path: f.path.clone(),
                });
            }
            scripts.push((f.path.clone(), bytes));
        }
    }
    scripts.sort_by(|a, b| a.0.cmp(&b.0));

    // Deterministic order: SKILL.md, companions, scripts. Deduplicate
    // on rel_path — the first producer of a path wins, so every path is
    // emitted exactly once (a zip member or written file never collides).
    let mut files = Vec::with_capacity(1 + companions.len() + scripts.len());
    files.push(SkillBundleFile {
        // The literal is obviously safe (Claude Code's entry filename).
        rel_path: "SKILL.md".to_string(),
        bytes: hub.to_markdown()?.into_bytes(),
    });
    let mut seen: HashSet<String> = HashSet::from(["SKILL.md".to_string()]);
    for (rel, bytes) in companions.into_iter().chain(scripts) {
        if seen.insert(rel.clone()) {
            files.push(SkillBundleFile {
                rel_path: rel,
                bytes,
            });
        }
    }
    Ok(files)
}
