//! Book ingest orchestration: write the full text to the shared
//! encrypted library stacks, then write the catalog (hub + chapters)
//! to the library namespace of the service-key scope.

use mycelium_crypto::keys::ServiceKey;
use mycelium_store::{ConceptStore, ConceptStoreError, FileRepo, Scope, Store};

use crate::extract::{BookCatalog, BookOutline, build_chapter_concept, build_hub_concept};

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("store error: {0}")]
    Store(#[from] ConceptStoreError),
    #[error("file repo error: {0}")]
    Repo(#[from] mycelium_store::FileRepoError),
    #[error("book text is empty")]
    EmptyBook,
    #[error("book has no chapters (no `# ` headings)")]
    NoChapters,
}

/// Where the full text of a book lives in the shared stacks.
pub fn stack_path_for(slug: &str) -> String {
    format!("/library/{slug}.md")
}

/// Write the full book text to the shared library stacks (service-key
/// scope). The stored copy is the single source for `book://` passages.
///
/// The stack text is a raw FileRepo payload — deliberately NOT a
/// concept: it never enters the scope_files registry or the search
/// index (a 32 MiB book body would poison both; passages are read via
/// `book://` anchors, not search).
pub async fn write_stack_text(
    store: &Store,
    service_key: &ServiceKey,
    slug: &str,
    text: &str,
) -> Result<(), IngestError> {
    if text.trim().is_empty() {
        return Err(IngestError::EmptyBook);
    }
    let repo = FileRepo::new(store.library_dir());
    repo.write(
        &stack_path_for(slug),
        text.as_bytes(),
        &Scope::Service(service_key.clone()),
    )
    .await?;
    Ok(())
}

/// Read the full book text back from the shared stacks.
pub async fn read_stack_text(
    store: &Store,
    service_key: &ServiceKey,
    slug: &str,
) -> Result<String, IngestError> {
    let repo = FileRepo::new(store.library_dir());
    let bytes = repo
        .read(&stack_path_for(slug), &Scope::Service(service_key.clone()))
        .await?;
    String::from_utf8(bytes)
        .map_err(|_| IngestError::Store(ConceptStoreError::NotFound(stack_path_for(slug))))
}

/// Write the catalog (hub + chapter concepts) to the library namespace
/// of the service-key scope. Catalogs for all books live there under
/// `/<slug>/`.
pub async fn write_catalog(
    store: &Store,
    service_key: &ServiceKey,
    catalog: &BookCatalog,
    outline: &BookOutline,
) -> Result<Vec<String>, IngestError> {
    if outline.chapters.is_empty() {
        return Err(IngestError::NoChapters);
    }
    let cs = ConceptStore::for_service(store, service_key.clone(), &store.library_dir(), "library");
    let mut written = Vec::new();
    let hub = build_hub_concept(catalog, outline);
    cs.put(&hub).await?;
    written.push(hub.source_path.clone());
    for chapter in &outline.chapters {
        let concept = build_chapter_concept(catalog, outline, chapter);
        cs.put(&concept).await?;
        written.push(concept.source_path.clone());
    }
    Ok(written)
}

/// Convenience: full ingest for one book (stack text + catalog).
pub async fn ingest_book(
    store: &Store,
    service_key: &ServiceKey,
    slug: &str,
    _title_hint: &str,
    text: &str,
    catalog: &BookCatalog,
) -> Result<Vec<String>, IngestError> {
    write_stack_text(store, service_key, slug, text).await?;
    let outline = crate::extract::parse_outline(text);
    write_catalog(store, service_key, catalog, &outline).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOK: &str = "\
# Chapter One

Intro text.

## Section 1.1

Details one.

# Chapter Two

Second chapter text.
";

    #[tokio::test]
    async fn ingest_writes_stack_and_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let service = ServiceKey::from_bytes(&[7u8; 32]).unwrap();

        let catalog = crate::extract::BookCatalog {
            slug: "my-book".into(),
            title: "My Book".into(),
            description: "A test book.".into(),
            chapter_summaries: vec!["Chapter one summary.".into(), String::new()],
        };
        let written = ingest_book(&store, &service, "my-book", "My Book", BOOK, &catalog)
            .await
            .unwrap();

        assert_eq!(
            written,
            vec![
                "/my-book/book.md",
                "/my-book/ch-1-chapter-one.md",
                "/my-book/ch-2-chapter-two.md",
            ]
        );

        // Stack text round-trips.
        let text = read_stack_text(&store, &service, "my-book").await.unwrap();
        assert!(text.starts_with("# Chapter One"));

        // The stack text is NOT a concept: absent from the registry and
        // the search index (passages are read via book:// anchors).
        let cs =
            ConceptStore::for_service(&store, service.clone(), &store.library_dir(), "library");
        let listing = cs.list().await.unwrap();
        assert!(
            !listing.iter().any(|e| e.path == "/library/my-book.md"),
            "stack text must not appear in the registry"
        );
        let q = mycelium_core::search::SearchQuery::new(vec!["intro".into()]);
        let hits = cs.search(&q).await.unwrap();
        assert!(
            !hits.iter().any(|h| h.concept_path == "/library/my-book.md"),
            "stack text must not be searchable"
        );

        // Catalog concepts are retrievable and well-formed.
        let hub = cs.get("/my-book/book.md").await.unwrap();
        assert_eq!(hub.frontmatter.concept_type, "Book");
        let ch1 = cs.get("/my-book/ch-1-chapter-one.md").await.unwrap();
        assert_eq!(ch1.frontmatter.chapter_index, Some(1));
        assert!(ch1.body.contains("Chapter one summary."));

        // Passages extract from the stack text via book:// anchors.
        let passage =
            mycelium_core::library::extract_passage("my-book", "ch-1-chapter-one", &text).unwrap();
        assert!(passage.text.contains("Intro text."));
        let sec = mycelium_core::library::extract_passage("my-book", "sec-1-1-details-one", &text)
            .unwrap();
        assert!(sec.text.contains("Details one."));
    }

    #[tokio::test]
    async fn empty_book_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let service = ServiceKey::from_bytes(&[7u8; 32]).unwrap();
        let err = write_stack_text(&store, &service, "b", "   ").await;
        assert!(matches!(err, Err(IngestError::EmptyBook)));
    }

    #[tokio::test]
    async fn no_chapters_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let service = ServiceKey::from_bytes(&[7u8; 32]).unwrap();
        let outline = crate::extract::parse_outline("no headings");
        let catalog = crate::extract::BookCatalog {
            slug: "b".into(),
            title: "B".into(),
            description: String::new(),
            chapter_summaries: vec![],
        };
        let err = write_catalog(&store, &service, &catalog, &outline).await;
        assert!(matches!(err, Err(IngestError::NoChapters)));
    }
}
