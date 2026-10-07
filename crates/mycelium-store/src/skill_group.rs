//! Structural skill grouping over concept entries. Rules (spec §Data
//! model): an entry at `/<slug>/…` belongs to skill `<slug>` iff
//! `/<slug>/skill.md` exists; everything else is flat. System files
//! (`index.md`, `log.md`, `info.md`) never appear.

use crate::ConceptEntry;

const SYSTEM_BASENAMES: [&str; 3] = ["index.md", "log.md", "info.md"];

#[derive(Debug, Clone, PartialEq)]
pub struct SkillGroup {
    /// Top-level directory name of the skill.
    pub slug: String,
    /// The `/<slug>/skill.md` entry.
    pub hub: ConceptEntry,
    /// Companion concepts under `/<slug>/` (system files excluded),
    /// sorted by path.
    pub members: Vec<ConceptEntry>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SkillListings {
    /// Skill directories with a `skill.md` hub, sorted by slug.
    pub grouped: Vec<SkillGroup>,
    /// Everything else — legacy flat skills, orphans — sorted by path.
    pub flat: Vec<ConceptEntry>,
}

pub fn group_skills(entries: &[ConceptEntry]) -> SkillListings {
    // Filter system files, partition by first path segment.
    //
    // (deterministic sort; BTreeMap keeps slug order stable)
    let mut flat: Vec<&ConceptEntry> = Vec::new();
    // slug → (hub entry, member entries), insertion-order members.
    let mut dirs: std::collections::BTreeMap<String, (Option<&ConceptEntry>, Vec<&ConceptEntry>)> =
        std::collections::BTreeMap::new();

    for entry in entries {
        // System files are excluded everywhere — they never appear in
        // groups, members, or flat listings.
        let basename = entry.path.rsplit('/').next().unwrap_or_default();
        if SYSTEM_BASENAMES.contains(&basename) {
            continue;
        }
        let stripped = entry.path.strip_prefix('/').unwrap_or(&entry.path);
        match stripped.split_once('/') {
            // Under a top-level directory: first segment is the slug,
            // at ANY depth below it (depth guard — only the top level is
            // a skill dir, so `/slug/a/b.md` belongs to `<slug>`).
            Some((slug, rest)) => {
                let slot = dirs.entry(slug.to_string()).or_default();
                if rest == "skill.md" {
                    slot.0 = Some(entry);
                } else {
                    slot.1.push(entry);
                }
            }
            // No directory part — root-level entry, always flat.
            None => flat.push(entry),
        }
    }

    // Groups exist only where the dir has a `/<slug>/skill.md` hub;
    // hubless-dir entries fall through to flat.
    let mut grouped = Vec::with_capacity(dirs.len());
    for (slug, (hub, members)) in dirs {
        match hub {
            Some(hub) => {
                let mut members: Vec<ConceptEntry> = members.iter().map(|e| (*e).clone()).collect();
                members.sort_by(|a, b| a.path.cmp(&b.path));
                grouped.push(SkillGroup {
                    slug,
                    hub: hub.clone(),
                    members,
                });
            }
            None => flat.extend(members),
        }
    }

    let mut flat: Vec<ConceptEntry> = flat.into_iter().cloned().collect();
    flat.sort_by(|a, b| a.path.cmp(&b.path));
    SkillListings { grouped, flat }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(path: &str, title: &str, ty: &str) -> ConceptEntry {
        ConceptEntry {
            path: path.into(),
            title: title.into(),
            concept_type: ty.into(),
            updated_at: "2026-10-07T00:00:00Z".into(),
        }
    }

    #[test]
    fn root_skill_md_is_flat_not_a_group() {
        // `/skill.md` has no directory segment — it is a legacy flat
        // entry, not a hub for a group named "".
        let out = group_skills(&[e("/skill.md", "root skill", "Skill")]);
        assert!(out.grouped.is_empty());
        assert_eq!(out.flat.len(), 1);
        assert_eq!(out.flat[0].path, "/skill.md");
    }

    #[test]
    fn nested_skill_md_is_a_member_not_a_hub() {
        // Only `/<slug>/skill.md` at depth one is a hub;
        // `/slug/sub/skill.md` is a plain member (depth guard).
        let entries = vec![
            e("/s/skill.md", "s", "Skill"),
            e("/s/sub/skill.md", "nested", "Skill"),
        ];
        let out = group_skills(&entries);
        assert_eq!(out.grouped.len(), 1);
        assert_eq!(out.grouped[0].members.len(), 1);
        assert_eq!(out.grouped[0].members[0].path, "/s/sub/skill.md");
    }
}
