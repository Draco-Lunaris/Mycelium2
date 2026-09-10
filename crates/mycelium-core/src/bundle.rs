//! Bundle walker: recursively load OKF concepts from a directory tree.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::concept::{Concept, ConceptError, is_reserved_filename};
use crate::reserved_names;

/// A parsed OKF bundle: concepts plus reserved-file bookkeeping.
#[derive(Debug, Clone, Default)]
pub struct Bundle {
    /// Canonical root path the bundle was walked from.
    pub root: PathBuf,
    /// All parsed concepts, keyed by canonical path (leading slash, forward slashes).
    pub concepts: Vec<Concept>,
    /// Shelf metadata from `info.md`, if present.
    pub shelf_info: Option<ShelfInfo>,
    /// Reserved files encountered (basenames), for diagnostics.
    pub reserved_files_seen: Vec<String>,
    /// Non-kebab-case concept filenames (warnings, not errors).
    pub naming_warnings: Vec<String>,
}

/// Per-shelf metadata from the reserved `info.md` file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShelfInfo {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Book shelves carry `kind: book`.
    #[serde(default)]
    pub kind: Option<String>,
}

impl ShelfInfo {
    pub fn is_book_shelf(&self) -> bool {
        self.kind.as_deref() == Some("book")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("io error walking bundle: {0}")]
    Walk(#[from] walkdir::Error),
    #[error("io error reading file: {0}")]
    Io(#[from] std::io::Error),
    #[error("concept parse error in {path}: {source}")]
    Concept {
        path: String,
        #[source]
        source: ConceptError,
    },
    #[error("shelf info parse error: {0}")]
    ShelfInfo(#[from] serde_yaml_ng::Error),
}

/// Walk a bundle directory and parse all concepts.
///
/// - `index.md` / `log.md` / `info.md` are excluded from concepts.
/// - Hidden files and directories (leading `.`) are skipped.
/// - Symlinks are not followed.
pub fn walk_bundle(root: &Path) -> Result<Bundle, BundleError> {
    let mut bundle = Bundle {
        root: root.to_path_buf(),
        ..Bundle::default()
    };

    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            // Prune hidden files/dirs before descending (skips .git, .traces, etc.).
            e.depth() == 0 || !e.file_name().to_str().is_some_and(|s| s.starts_with('.'))
        })
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let file_name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| std::io::Error::other("non-utf8 filename"))?
            .to_string();

        // Skip hidden files (and anything inside hidden dirs is never reached
        // because walkdir still descends into them — filter by path segments).
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if rel
            .components()
            .any(|c| c.as_os_str().to_str().is_some_and(|s| s.starts_with('.')))
        {
            continue;
        }

        if file_name == reserved_names::INFO_MD {
            let contents = std::fs::read_to_string(entry.path())?;
            let info: ShelfInfo = serde_yaml_ng::from_str(&contents)?;
            bundle.shelf_info = Some(info);
            bundle.reserved_files_seen.push(file_name);
            continue;
        }
        if is_reserved_filename(&file_name) {
            bundle.reserved_files_seen.push(file_name);
            continue;
        }
        if !file_name.ends_with(".md") {
            continue;
        }

        let canonical = canonical_path(rel);
        if !is_kebab_case(&file_name) {
            bundle
                .naming_warnings
                .push(format!("{canonical}: not kebab-case"));
        }

        let contents = std::fs::read_to_string(entry.path())?;
        let concept =
            Concept::parse(&canonical, &contents).map_err(|source| BundleError::Concept {
                path: canonical.clone(),
                source,
            })?;
        bundle.concepts.push(concept);
    }

    Ok(bundle)
}

/// Canonicalize a relative path into bundle-relative form:
/// forward slashes, leading slash, no `.` segments.
pub fn canonical_path(rel: &Path) -> String {
    let mut out = String::new();
    for comp in rel.components() {
        if let std::path::Component::Normal(seg) = comp {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(&seg.to_string_lossy());
        }
    }
    format!("/{out}")
}

fn is_kebab_case(file_name: &str) -> bool {
    let stem = file_name.strip_suffix(".md").unwrap_or(file_name);
    stem.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_path_normalizes() {
        assert_eq!(
            canonical_path(Path::new("tables/users.md")),
            "/tables/users.md"
        );
        assert_eq!(canonical_path(Path::new("a/b/c.md")), "/a/b/c.md");
    }

    #[test]
    fn kebab_case_check() {
        assert!(is_kebab_case("auth-model.md"));
        assert!(is_kebab_case("ch-1-intro.md"));
        assert!(!is_kebab_case("AuthModel.md"));
    }
}
