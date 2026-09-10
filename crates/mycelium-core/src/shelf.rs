//! Shelf / bookshelf model.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum ShelfKind {
    #[serde(rename = "global")]
    Global,
    #[serde(rename = "topic")]
    Topic,
    #[serde(rename = "user")]
    User,
    #[serde(rename = "book")]
    Book,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Shelf {
    pub name: String,
    pub kind: ShelfKind,
    pub is_global_read: bool,
}

impl Shelf {
    pub fn new(name: impl Into<String>, kind: ShelfKind) -> Self {
        Self {
            name: name.into(),
            kind,
            is_global_read: matches!(kind, ShelfKind::Global),
        }
    }

    /// A book shelf holds book catalog concepts (Book/Chapter).
    pub fn is_book_shelf(&self) -> bool {
        matches!(self.kind, ShelfKind::Book)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn book_shelf_detected() {
        let shelf = Shelf::new("rust", ShelfKind::Book);
        assert!(shelf.is_book_shelf());
    }

    #[test]
    fn topic_shelf_is_not_book_shelf() {
        let shelf = Shelf::new("general", ShelfKind::Topic);
        assert!(!shelf.is_book_shelf());
    }

    #[test]
    fn global_shelf_is_global_read() {
        let shelf = Shelf::new("shared", ShelfKind::Global);
        assert!(shelf.is_global_read);
        assert!(!shelf.is_book_shelf());
    }
}
