//! In-process librarian agent for book ingest and cataloging.

pub mod extract;
pub mod ingest;
pub mod llm;
pub mod worker;

pub use worker::LibrarianWorker;
