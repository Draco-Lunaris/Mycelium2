//! In-process librarian agent for book ingest and cataloging.

pub mod agent;
pub mod dream;
pub mod extract;
pub mod fallback;
pub mod hot_memory;
pub mod ingest;
pub mod llm;
pub mod query_cache;
pub mod trace;
pub mod worker;

pub use extract::{
    BookCatalog, BookOutline, ChapterOutline, build_catalog, parse_outline, slugify,
};
// `fallback::slugify` is NOT re-exported at the root: the name is taken
// by `extract::slugify` (the Unicode-aware book-anchor slugifier the web
// admin pages use). The path-slug variant stays at
// `mycelium_librarian::fallback::slugify` — the import path the queue
// worker and MCP wiring use for all fallback fns.
pub use fallback::{
    canonical, dated_addendum_update, derive_title, direct_write_add, wire_and_flag_maintain,
};
pub use ingest::{
    IngestError, delete_book, ingest_book, read_stack_text, stack_path_for, write_stack_text,
};
pub use llm::{LlmClient, LlmConfig, LlmError};
pub use worker::{JobStatus, LibrarianWorker, SubmittedJob, WorkerError};
