//! Book ingest orchestration.

pub struct IngestJob {
    pub id: uuid::Uuid,
    pub book_slug: String,
}

impl IngestJob {
    pub fn new(book_slug: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            book_slug: book_slug.into(),
        }
    }
}
