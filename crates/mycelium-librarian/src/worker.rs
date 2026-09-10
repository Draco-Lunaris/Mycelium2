//! In-process async librarian worker.

use tokio::sync::Mutex;

pub struct LibrarianWorker {
    _lock: Mutex<()>,
}

impl Default for LibrarianWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl LibrarianWorker {
    pub fn new() -> Self {
        Self {
            _lock: Mutex::new(()),
        }
    }
}
