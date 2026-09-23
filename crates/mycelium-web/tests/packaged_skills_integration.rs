//! Packaged-skills seeding: a fresh store gets the pdf-to-markdown
//! book-conversion skill in the global skills shelf on first boot; later
//! boots leave admin edits alone unless the packaged version bumps
//! (mirrors the assets scaffold contract).

use mycelium_core::concept::Concept;
use mycelium_core::search::SearchQuery;
use mycelium_store::{ConceptStore, Store};
use mycelium_web::packaged_skills::{self, PACKAGED_SKILLS, SKILLS_SEED_VERSION};

/// Fresh store + service key; returns (tempdir guard, store, key, skills dir).
/// The tempdir guard must stay bound for the whole test.
async fn boot() -> (
    tempfile::TempDir,
    Store,
    mycelium_crypto::keys::ServiceKey,
    std::path::PathBuf,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let service_key =
        mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();
    let skills_dir = dir.path().join("skills");
    (dir, store, service_key, skills_dir)
}

#[tokio::test]
async fn fresh_seed_puts_all_packaged_concepts() {
    let (_dir, store, service_key, skills_dir) = boot().await;
    packaged_skills::seed_packaged_skills(&store, &service_key, &skills_dir)
        .await
        .unwrap();

    let cs = ConceptStore::for_service(&store, service_key.clone(), &skills_dir, "skills");
    let list = cs.list().await.unwrap();
    assert_eq!(list.len(), PACKAGED_SKILLS.len());
    for (name, _) in PACKAGED_SKILLS {
        let path = format!("/{name}");
        assert!(
            list.iter().any(|e| e.path == path),
            "packaged concept missing from shelf: {path}"
        );
    }

    // Script concepts keep their Skill type through the round trip.
    let script = cs
        .get("/pdf-to-markdown-script-requirements.md")
        .await
        .unwrap();
    assert_eq!(script.frontmatter.concept_type, "Skill");

    // The shelf is searchable (the main skill doc discusses docling).
    let hits = cs
        .search(&SearchQuery::new(vec!["docling".to_string()]))
        .await
        .unwrap();
    assert!(!hits.is_empty());
}

#[tokio::test]
async fn seed_twice_does_not_duplicate() {
    let (_dir, store, service_key, skills_dir) = boot().await;
    packaged_skills::seed_packaged_skills(&store, &service_key, &skills_dir)
        .await
        .unwrap();
    packaged_skills::seed_packaged_skills(&store, &service_key, &skills_dir)
        .await
        .unwrap();
    let cs = ConceptStore::for_service(&store, service_key, &skills_dir, "skills");
    assert_eq!(cs.list().await.unwrap().len(), PACKAGED_SKILLS.len());
}

#[tokio::test]
async fn same_version_preserves_admin_edits() {
    let (_dir, store, service_key, skills_dir) = boot().await;
    packaged_skills::seed_packaged_skills(&store, &service_key, &skills_dir)
        .await
        .unwrap();

    // Simulate an admin edit through the shelf itself.
    let cs = ConceptStore::for_service(&store, service_key.clone(), &skills_dir, "skills");
    let edited = "---\ntype: Skill\ntitle: edited\n---\n\nadmin customization\n";
    cs.put(
        &Concept::parse("/pdf-to-markdown-conventions.md", edited).unwrap(),
    )
    .await
    .unwrap();

    // Second boot with the same packaged version: hands off.
    packaged_skills::seed_packaged_skills(&store, &service_key, &skills_dir)
        .await
        .unwrap();
    let got = cs.get("/pdf-to-markdown-conventions.md").await.unwrap();
    assert_eq!(got.frontmatter.title.as_deref(), Some("edited"));
    assert!(got.body.contains("admin customization"));
    assert_eq!(
        std::fs::read_to_string(skills_dir.join(".seed-version"))
            .unwrap()
            .trim(),
        SKILLS_SEED_VERSION
    );
}

#[tokio::test]
async fn version_bump_refreshes_packaged_content() {
    let (_dir, store, service_key, skills_dir) = boot().await;
    packaged_skills::seed_packaged_skills(&store, &service_key, &skills_dir)
        .await
        .unwrap();

    let cs = ConceptStore::for_service(&store, service_key.clone(), &skills_dir, "skills");
    let edited = "---\ntype: Skill\ntitle: stale\n---\n\nold content\n";
    cs.put(&Concept::parse("/pdf-to-markdown-license.md", edited).unwrap())
        .await
        .unwrap();

    // A stale marker (older packaged version) triggers a full refresh.
    std::fs::write(skills_dir.join(".seed-version"), "0").unwrap();
    packaged_skills::seed_packaged_skills(&store, &service_key, &skills_dir)
        .await
        .unwrap();

    let got = cs.get("/pdf-to-markdown-license.md").await.unwrap();
    assert_ne!(got.frontmatter.title.as_deref(), Some("stale"));
    assert_eq!(
        std::fs::read_to_string(skills_dir.join(".seed-version"))
            .unwrap()
            .trim(),
        SKILLS_SEED_VERSION
    );
}
