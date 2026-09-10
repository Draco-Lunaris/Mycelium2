//! Mycelium2 core OKF engine.

pub mod bundle;
pub mod concept;
pub mod graph;
pub mod library;
pub mod links;
pub mod reserved;
pub mod search;
pub mod shelf;

pub use bundle::{Bundle, BundleError, ShelfInfo};
pub use concept::{Concept, ConceptError, Frontmatter};
pub use graph::{BrokenLink, Edge, Graph, GraphHealth, Node, build_graph};
pub use library::{
    Anchor, BookRef, Passage, PassageError, READ_PASSAGE_MAX_CHARS, extract_passage, parse_book_ref,
};
pub use links::scan_links;
pub use search::{
    InMemoryIndex, SearchIndex, SearchQuery, SearchResult, searchable_text, tokenize,
};

/// Reserved filenames in an OKF bundle. These are never concepts:
/// `index.md` and `log.md` are auto-maintained by the system;
/// `info.md` is per-shelf metadata.
pub mod reserved_names {
    pub const INDEX_MD: &str = "index.md";
    pub const LOG_MD: &str = "log.md";
    pub const INFO_MD: &str = "info.md";
}
