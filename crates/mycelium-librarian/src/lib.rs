//! In-process librarian agent for book ingest and cataloging.

pub mod agent;
pub mod dream;
pub mod extract;
pub mod hot_memory;
pub mod ingest;
pub mod llm;
pub mod query_cache;
pub mod trace;
pub mod worker;

pub use extract::{
    BookCatalog, BookOutline, ChapterOutline, build_catalog, parse_outline, slugify,
};
pub use ingest::{
    IngestError, delete_book, ingest_book, read_stack_text, stack_path_for, write_stack_text,
};
pub use llm::{LlmClient, LlmConfig, LlmError};
pub use worker::{JobStatus, LibrarianWorker, SubmittedJob, WorkerError};
