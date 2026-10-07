//! OKF concept: markdown file with YAML frontmatter.

use serde::{Deserialize, Serialize};

use crate::reserved_names;

/// Role of a file inside a skill bundle. Display/ordering only —
/// never enforced. Unknown role strings deserialize to `Reference`
/// (lenient — `#[serde(other)]` can't express that fallback with
/// rename_all, so the enum gets a small manual `Deserialize` impl;
/// mirroring how `type` is a plain lenient String).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub enum SkillRole {
    #[serde(rename = "script")]
    Script,
    #[default]
    #[serde(rename = "reference")]
    Reference,
    #[serde(rename = "example")]
    Example,
}

impl<'de> Deserialize<'de> for SkillRole {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.as_str() {
            "script" => SkillRole::Script,
            "example" => SkillRole::Example,
            _ => SkillRole::Reference, // "reference" + unknown → lenient
        })
    }
}

/// One file of a skill bundle. `path` is relative to the skill
/// directory (`/<slug>/`) and must resolve inside it — checked by
/// [`SkillManifest::validated`]. Non-`.md` paths are raw payload files
/// (never concepts); `.md` paths are companion concepts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillFile {
    pub path: String,
    #[serde(default)]
    pub role: SkillRole,
    /// Optional integrity check verified at bundle time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
}

/// Skills manifest carried on the `/<slug>/skill.md` hub concept
/// (`skill:` frontmatter key). Known field on `Frontmatter` — unknown
/// keys are dropped by the lenient parser, so a one-off key would not
/// survive `Concept::parse` (precedent: `book`/`chapter_index`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SkillManifest {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub files: Vec<SkillFile>,
}

impl SkillManifest {
    /// Validate every file path: relative (no leading `/`), no `..`
    /// OR `.` segments (traversal/escapes), no backslashes (Windows
    /// separators), no NUL, non-empty. Paths are relative to the skill
    /// dir; the caller joins them only after this passes.
    pub fn validated(&self) -> Result<(), ConceptError> {
        for f in &self.files {
            let p = &f.path;
            let ok = !p.is_empty()
                && !p.starts_with('/')
                && !p.contains('\\')
                && !p.contains('\0')
                && p.split('/')
                    .all(|seg| seg != ".." && seg != "." && !seg.is_empty());
            if !ok {
                return Err(ConceptError::InvalidSkillPath { path: p.clone() });
            }
        }
        Ok(())
    }
}

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
    /// Skill-bundle manifest carried by a `/<slug>/skill.md` hub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<SkillManifest>,
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
    #[error("skill manifest path escapes the skill directory: {path}")]
    InvalidSkillPath { path: String },
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

    #[test]
    fn skill_manifest_parses_from_hub_frontmatter() {
        let md = "---\ntype: Skill\ntitle: t\nskill:\n  version: 1\n  files:\n    - {path: scripts/convert.py, role: script, md5: abc}\n    - {path: conventions.md, role: reference}\n---\n\nbody";
        let c = Concept::parse("/s/skill.md", md).unwrap();
        let m = c.frontmatter.skill.expect("manifest parsed");
        assert_eq!(m.version, 1);
        assert_eq!(m.files.len(), 2);
        assert_eq!(m.files[0].path, "scripts/convert.py");
        assert_eq!(m.files[0].role, SkillRole::Script);
        assert_eq!(m.files[0].md5.as_deref(), Some("abc"));
        assert_eq!(m.files[1].role, SkillRole::Reference);
    }

    #[test]
    fn skill_manifest_roundtrips_without_altering_other_fields() {
        let md = "---\ntype: Skill\ntitle: t\nskill:\n  version: 1\n  files:\n    - {path: a.md, role: reference}\n---\n\nbody";
        let c1 = Concept::parse("/s/skill.md", md).unwrap();
        let out = c1.to_markdown().unwrap();
        let c2 = Concept::parse("/s/skill.md", &out).unwrap();
        assert_eq!(c1.frontmatter.skill, c2.frontmatter.skill);
        assert_eq!(c1.frontmatter.title, c2.frontmatter.title);
        // A concept WITHOUT the manifest serializes exactly as before.
        let plain = "---\ntype: Skill\ntitle: t\n---\n\nbody";
        assert_eq!(
            Concept::parse("/s.md", plain)
                .unwrap()
                .to_markdown()
                .unwrap(),
            plain
        );
    }

    #[test]
    fn skill_manifest_unknown_role_is_lenient() {
        let md = "---\ntype: Skill\nskill:\n  version: 1\n  files:\n    - {path: x.md, role: mysterious}\n---\n\nb";
        let c = Concept::parse("/s.md", md).unwrap();
        assert_eq!(
            c.frontmatter.skill.unwrap().files[0].role,
            SkillRole::Reference
        );
    }

    #[test]
    fn skill_manifest_validation_rejects_traversal() {
        for bad in [
            "./out.md",
            "../escape.md",
            "..\\win.md",
            "/absolute.md",
            "",
            "a/../../escape",
            "scripts/../../escape.md",
            "a\0b",
        ] {
            let m = SkillManifest {
                version: 1,
                files: vec![SkillFile {
                    path: bad.into(),
                    role: SkillRole::Script,
                    md5: None,
                }],
            };
            assert!(m.validated().is_err(), "must reject {bad:?}");
        }
        let good = SkillManifest {
            version: 1,
            files: vec![
                SkillFile {
                    path: "scripts/convert.py".into(),
                    role: SkillRole::Script,
                    md5: None,
                },
                SkillFile {
                    path: "LICENSE.txt".into(),
                    role: SkillRole::Reference,
                    md5: None,
                },
            ],
        };
        assert!(good.validated().is_ok());
    }
}
