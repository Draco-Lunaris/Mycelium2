//! OKF concept: markdown file with YAML frontmatter.

use serde::{Deserialize, Serialize};

use crate::reserved_names;

/// Frontmatter of an OKF concept. `type` is the only required field;
/// all others are optional and unknown fields are ignored (lenient parsing).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Frontmatter {
    #[serde(rename = "type", default)]
    pub concept_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Book catalog concepts: the hub path this concept belongs to
    /// (`/<slug>/book.md` on chapter concepts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub book: Option<String>,
    /// Chapter concepts: 1-based chapter index within the book.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chapter_index: Option<u32>,
}

/// A parsed OKF concept: frontmatter + markdown body + canonical path.
#[derive(Debug, Clone, PartialEq)]
pub struct Concept {
    pub frontmatter: Frontmatter,
    pub body: String,
    /// Canonical bundle-relative path with leading slash, e.g. `/tables/users.md`.
    pub source_path: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConceptError {
    #[error("missing frontmatter block")]
    MissingFrontmatter,
    #[error("frontmatter is missing the required `type` field")]
    MissingTypeField,
    #[error("frontmatter YAML error: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
}

impl Concept {
    pub fn new(frontmatter: Frontmatter, body: String, source_path: String) -> Self {
        Self {
            frontmatter,
            body,
            source_path,
        }
    }

    /// Parse a concept from file contents at a canonical bundle-relative path.
    pub fn parse(source_path: &str, contents: &str) -> Result<Self, ConceptError> {
        let (frontmatter_raw, body) = split_frontmatter(contents)?;
        let frontmatter: Frontmatter = serde_yaml_ng::from_str(&frontmatter_raw)?;
        if frontmatter.concept_type.trim().is_empty() {
            return Err(ConceptError::MissingTypeField);
        }
        Ok(Self {
            frontmatter,
            body,
            source_path: source_path.to_string(),
        })
    }

    /// Serialize back to frontmatter + body markdown.
    pub fn to_markdown(&self) -> Result<String, ConceptError> {
        let mut yaml = serde_yaml_ng::to_string(&self.frontmatter)?;
        // serde_yaml_ng emits a leading "---\n"; normalize to our exact format.
        yaml = yaml.trim_start_matches("---\n").to_string();
        let mut out = String::with_capacity(yaml.len() + self.body.len() + 8);
        out.push_str("---\n");
        out.push_str(yaml.trim_end());
        out.push_str("\n---\n");
        if !self.body.is_empty() {
            out.push_str(&self.body);
        }
        Ok(out)
    }
}

/// Split `---\n...\n---\n` frontmatter from the body. Returns (yaml, body).
///
/// Known limitation: a literal `---` line inside a YAML block scalar is
/// treated as the closing delimiter (naive scan, mirrors original Mycelium).
fn split_frontmatter(contents: &str) -> Result<(String, String), ConceptError> {
    let normalized = if contents.contains("\r\n") {
        contents.replace("\r\n", "\n")
    } else {
        contents.to_string()
    };
    // Strip a UTF-8 BOM if present (Windows editors emit it).
    let normalized = normalized.strip_prefix('\u{FEFF}').unwrap_or(&normalized);
    let rest = normalized
        .strip_prefix("---\n")
        .ok_or(ConceptError::MissingFrontmatter)?;
    // Find the closing delimiter on its own line.
    let mut yaml_len = None;
    let mut body_start = None;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            yaml_len = Some(offset);
            body_start = Some(offset + line.len());
            break;
        }
        offset += line.len();
    }
    let (yl, bs) = yaml_len
        .zip(body_start)
        .ok_or(ConceptError::MissingFrontmatter)?;
    Ok((rest[..yl].to_string(), rest[bs..].to_string()))
}

/// True if the file basename is a reserved OKF filename.
pub fn is_reserved_filename(file_name: &str) -> bool {
    matches!(
        file_name,
        reserved_names::INDEX_MD | reserved_names::LOG_MD | reserved_names::INFO_MD
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "---\ntype: Decision\ntitle: \"Auth Model\"\ndescription: How auth works\nresource: book://auth#ch-1-intro\ntags:\n  - auth\n  - security\ntimestamp: 2026-09-10\n---\n\nBody text here.\n";

    #[test]
    fn parse_full_concept() {
        let c = Concept::parse("/decisions/auth-model.md", FULL).unwrap();
        assert_eq!(c.frontmatter.concept_type, "Decision");
        assert_eq!(c.frontmatter.title.as_deref(), Some("Auth Model"));
        assert_eq!(c.frontmatter.tags, vec!["auth", "security"]);
        assert_eq!(c.body.trim(), "Body text here.");
        assert_eq!(c.source_path, "/decisions/auth-model.md");
    }

    #[test]
    fn parse_minimal_concept() {
        let c = Concept::parse("/x.md", "---\ntype: Note\n---\n\nhi\n").unwrap();
        assert_eq!(c.frontmatter.concept_type, "Note");
        assert!(c.frontmatter.title.is_none());
        assert!(c.frontmatter.tags.is_empty());
    }

    #[test]
    fn unknown_fields_ignored() {
        let c = Concept::parse(
            "/x.md",
            "---\ntype: Note\nfuture_field: whatever\n---\n\nhi\n",
        )
        .unwrap();
        assert_eq!(c.frontmatter.concept_type, "Note");
    }

    #[test]
    fn missing_type_is_error() {
        let err = Concept::parse("/x.md", "---\ntitle: No Type\n---\n\nhi\n").unwrap_err();
        assert!(matches!(err, ConceptError::MissingTypeField));
    }

    #[test]
    fn empty_type_is_error() {
        let err = Concept::parse("/x.md", "---\ntype: \"\"\n---\n\nhi\n").unwrap_err();
        assert!(matches!(err, ConceptError::MissingTypeField));
    }

    #[test]
    fn no_frontmatter_is_error() {
        let err = Concept::parse("/x.md", "Just body text.\n").unwrap_err();
        assert!(matches!(err, ConceptError::MissingFrontmatter));
    }

    #[test]
    fn round_trip() {
        let c = Concept::parse("/decisions/auth-model.md", FULL).unwrap();
        let md = c.to_markdown().unwrap();
        let c2 = Concept::parse("/decisions/auth-model.md", &md).unwrap();
        assert_eq!(c, c2);
    }

    #[test]
    fn crlf_handled() {
        let crlf = FULL.replace('\n', "\r\n");
        let c = Concept::parse("/decisions/auth-model.md", &crlf).unwrap();
        assert_eq!(c.frontmatter.concept_type, "Decision");
        assert_eq!(c.body.trim(), "Body text here.");
    }

    #[test]
    fn bom_handled() {
        let bom = format!("\u{FEFF}{FULL}");
        let c = Concept::parse("/decisions/auth-model.md", &bom).unwrap();
        assert_eq!(c.frontmatter.concept_type, "Decision");
    }

    #[test]
    fn reserved_filenames() {
        assert!(is_reserved_filename("index.md"));
        assert!(is_reserved_filename("log.md"));
        assert!(is_reserved_filename("info.md"));
        assert!(!is_reserved_filename("users.md"));
    }

    #[test]
    fn book_catalog_fields_round_trip() {
        let src = "---\ntype: Chapter\ntitle: Chapter One\nbook: /my-book/book.md\nchapter_index: 1\n---\n\nBody.\n";
        let c = Concept::parse("/my-book/ch-1-one.md", src).unwrap();
        assert_eq!(c.frontmatter.book.as_deref(), Some("/my-book/book.md"));
        assert_eq!(c.frontmatter.chapter_index, Some(1));
        let md = c.to_markdown().unwrap();
        assert!(md.contains("book: /my-book/book.md"));
        assert!(md.contains("chapter_index: 1"));
        let c2 = Concept::parse("/my-book/ch-1-one.md", &md).unwrap();
        assert_eq!(c, c2);
    }

    #[test]
    fn optional_fields_omitted_when_absent() {
        let c = Concept::parse("/x.md", "---\ntype: Note\n---\n\nhi\n").unwrap();
        let md = c.to_markdown().unwrap();
        assert!(!md.contains("title:"));
        assert!(!md.contains("tags:"));
        assert!(!md.contains("book:"));
        assert!(!md.contains("chapter_index:"));
    }
}
