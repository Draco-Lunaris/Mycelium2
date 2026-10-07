//! Task 4 integration tests: structural skill grouping over concept
//! entries (spec §Data model grouping rules).

use mycelium_store::ConceptEntry;
use mycelium_store::skill_group::group_skills;

fn e(path: &str, title: &str, ty: &str) -> ConceptEntry {
    ConceptEntry {
        path: path.into(),
        title: title.into(),
        concept_type: ty.into(),
        updated_at: "2026-10-07T00:00:00Z".into(),
    }
}

#[test]
fn groups_skill_dirs_and_leaves_the_rest_flat() {
    let entries = vec![
        e("/deploy-rust/skill.md", "deploy-rust", "Skill"),
        e("/deploy-rust/notes.md", "notes", "Note"),
        e("/pdf-to-markdown/skill.md", "pdf-to-markdown", "Skill"),
        e("/standalone.md", "Standalone", "Skill"), // legacy flat
        e("/orphan/x.md", "orphan x", "Skill"),     // dir without skill.md
        e("/deploy-rust/index.md", "idx", "index"), // system file — excluded
    ];
    let out = group_skills(&entries);
    let slugs: Vec<&str> = out.grouped.iter().map(|g| g.slug.as_str()).collect();
    assert_eq!(slugs, ["deploy-rust", "pdf-to-markdown"]); // sorted, no orphan group
    let g = &out.grouped[0];
    assert_eq!(g.hub.title, "deploy-rust");
    assert_eq!(g.members.len(), 1, "index.md excluded");
    assert_eq!(g.members[0].path, "/deploy-rust/notes.md");
    let flat: Vec<&str> = out.flat.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(flat, ["/orphan/x.md", "/standalone.md"]); // sorted
}

#[test]
fn system_files_at_skill_root_are_excluded_from_members() {
    let entries = vec![
        e("/s/skill.md", "s", "Skill"),
        e("/s/index.md", "idx", "index"),
        e("/s/log.md", "log", "log"),
        e("/s/info.md", "info", "info"),
    ];
    let out = group_skills(&entries);
    assert_eq!(out.grouped.len(), 1);
    assert!(out.grouped[0].members.is_empty());
}

#[test]
fn empty_and_single_entry_inputs() {
    assert!(group_skills(&[]).grouped.is_empty() && group_skills(&[]).flat.is_empty());
    let out = group_skills(&[e("/s/skill.md", "s", "Skill")]);
    assert_eq!(out.grouped.len(), 1);
    assert!(out.grouped[0].members.is_empty());
}

#[test]
fn deeper_paths_group_under_their_top_level_slug() {
    // Depth guard (controller ruling): only the first path segment is
    // treated as the skill dir — `/slug/a/b.md` belongs to `<slug>`.
    let entries = vec![
        e("/pdf-to-markdown/skill.md", "pdf-to-markdown", "Skill"),
        e("/pdf-to-markdown/references/ch-1.md", "Ch 1", "Note"),
        e("/pdf-to-markdown/references/ch-2.md", "Ch 2", "Note"),
        e("/pdf-to-markdown/scripts/index.md", "idx", "index"), // system file at depth — excluded
        e("/orphan/a/b.md", "deep orphan", "Skill"),            // hubless dir at depth — flat
    ];
    let out = group_skills(&entries);
    assert_eq!(out.grouped.len(), 1);
    assert_eq!(out.grouped[0].slug, "pdf-to-markdown");
    let member_paths: Vec<&str> = out.grouped[0]
        .members
        .iter()
        .map(|m| m.path.as_str())
        .collect();
    assert_eq!(
        member_paths,
        [
            "/pdf-to-markdown/references/ch-1.md",
            "/pdf-to-markdown/references/ch-2.md",
        ]
    );
    let flat: Vec<&str> = out.flat.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(flat, ["/orphan/a/b.md"]);
}
