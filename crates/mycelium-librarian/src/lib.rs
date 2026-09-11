//! In-process librarian agent for book ingest and cataloging.

pub mod extract;
pub mod ingest;
pub mod llm;
pub mod worker;

pub use extract::{
    BookCatalog, BookOutline, ChapterOutline, build_catalog, parse_outline, slugify,
};
pub use ingest::{IngestError, ingest_book, read_stack_text, stack_path_for, write_stack_text};
pub use llm::{LlmClient, LlmConfig, LlmError};
pub use worker::{JobStatus, LibrarianWorker, SubmittedJob, WorkerError};
