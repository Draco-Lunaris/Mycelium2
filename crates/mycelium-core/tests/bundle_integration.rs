//! End-to-end integration test: walk → parse → graph → search over the fixture bundle.

use mycelium_core::bundle::walk_bundle;
use mycelium_core::graph::build_graph;
use mycelium_core::library::{extract_passage, parse_book_ref};
use mycelium_core::search::{InMemoryIndex, SearchIndex, SearchQuery};

fn fixture_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-bundle")
}

#[test]
fn walk_fixture_bundle() {
    let bundle = walk_bundle(&fixture_root()).unwrap();

    // 7 concepts: auth-model, read-passage, users, book, ch-1-intro, skill, broken.
    assert_eq!(bundle.concepts.len(), 7);

    let paths: Vec<&str> = bundle
        .concepts
        .iter()
        .map(|c| c.source_path.as_str())
        .collect();
    assert!(paths.contains(&"/decisions/auth-model.md"));
    assert!(paths.contains(&"/apis/read-passage.md"));
    assert!(paths.contains(&"/tables/users.md"));
    assert!(paths.contains(&"/books/my-book/book.md"));
    assert!(paths.contains(&"/books/my-book/ch-1-intro.md"));
    assert!(paths.contains(&"/skills/deploy-rust-service.md"));
    assert!(paths.contains(&"/broken/concept-with-broken-link.md"));

    // Reserved files excluded from concepts but recorded.
    assert!(!paths.contains(&"/index.md"));
    assert!(!paths.contains(&"/log.md"));
    assert!(bundle.reserved_files_seen.contains(&"index.md".to_string()));
    assert!(bundle.reserved_files_seen.contains(&"log.md".to_string()));
    assert!(bundle.reserved_files_seen.contains(&"info.md".to_string()));

    // Shelf info parsed.
    let info = bundle.shelf_info.as_ref().unwrap();
    assert_eq!(info.name.as_deref(), Some("sample-bundle"));
    assert!(!info.is_book_shelf());

    // Hidden dir skipped.
    assert!(!paths.iter().any(|p| p.contains(".traces")));

    // Skill concept parsed with type Skill.
    let skill = bundle
        .concepts
        .iter()
        .find(|c| c.source_path == "/skills/deploy-rust-service.md")
        .unwrap();
    assert_eq!(skill.frontmatter.concept_type, "Skill");
}

#[test]
fn graph_over_fixture_bundle() {
    let bundle = walk_bundle(&fixture_root()).unwrap();
    let graph = build_graph(&bundle);
    let health = graph.health();

    // Nodes: 7 concepts.
    assert_eq!(health.concept_count, 7);

    // Edges (deduplicated):
    //   auth-model -> read-passage, users
    //   read-passage -> auth-model
    //   ch-1-intro -> auth-model
    //   skill -> auth-model
    //   book -> ch-1-intro
    assert_eq!(health.edge_count, 6);

    // Broken: broken/concept-with-broken-link.md -> /nonexistent.md
    assert_eq!(health.broken_link_count, 1);
    assert_eq!(
        graph.broken_links[0].from,
        "/broken/concept-with-broken-link.md"
    );
    assert_eq!(graph.broken_links[0].to, "/nonexistent.md");

    // Orphans: none — users.md has an in-edge from auth-model, and every
    // other concept has links in or out.
    assert_eq!(health.orphan_count, 0);
}

#[test]
fn search_over_fixture_bundle() {
    let bundle = walk_bundle(&fixture_root()).unwrap();
    let mut index = InMemoryIndex::new();
    for concept in &bundle.concepts {
        index.add(concept).ok();
    }

    // Skill is searchable.
    let results = index
        .search(&SearchQuery::new(vec!["rust".into()]))
        .unwrap();
    assert!(
        results
            .iter()
            .any(|r| r.concept_path == "/skills/deploy-rust-service.md")
    );

    // Auth concept findable by title token.
    let results = index
        .search(&SearchQuery::new(vec!["authentication".into()]))
        .unwrap();
    assert!(
        results
            .iter()
            .any(|r| r.concept_path == "/decisions/auth-model.md")
    );
}

#[test]
fn book_refs_and_passages_over_fixture() {
    let bundle = walk_bundle(&fixture_root()).unwrap();
    let chapter = bundle
        .concepts
        .iter()
        .find(|c| c.source_path == "/books/my-book/ch-1-intro.md")
        .unwrap();
    let resource = chapter.frontmatter.resource.as_deref().unwrap();
    let book_ref = parse_book_ref(resource).unwrap();
    assert_eq!(book_ref.slug, "my-book");
    assert_eq!(book_ref.anchor, "ch-1-intro");

    // Passage extraction over a synthetic stack text.
    let stack_text = "# Chapter One\n\nIntro body.\n\n# Chapter Two\n\nLater.\n";
    let passage = extract_passage(&book_ref.slug, "ch-1-one", stack_text).unwrap();
    assert!(passage.text.contains("Intro body."));
    assert!(!passage.text.contains("Chapter Two"));
}
