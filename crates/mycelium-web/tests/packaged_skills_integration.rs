//! Packaged-skills seeding: a fresh store gets the pdf-to-markdown
//! book-conversion skill in the global skills shelf on first boot; later
//! boots leave admin edits alone unless the packaged version bumps
//! (mirrors the assets scaffold contract).

use mycelium_core::search::SearchQuery;
use mycelium_store::{ConceptStore, Store};
use mycelium_web::packaged_skills::{self, PACKAGED_SKILLS};

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
