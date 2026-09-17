//! Book library: `book://` references, anchors, and passage extraction.

use serde::{Deserialize, Serialize};

/// Maximum characters returned by a single passage read (mirrors original Mycelium).
pub const READ_PASSAGE_MAX_CHARS: usize = 128 * 1024;

/// A parsed `book://<slug>#<anchor>` reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct BookRef {
    pub slug: String,
    pub anchor: String,
}

/// Parse a `book://<slug>#<anchor>` resource string.
pub fn parse_book_ref(resource: &str) -> Option<BookRef> {
    let rest = resource.strip_prefix("book://")?;
    let (slug, anchor) = rest.split_once('#')?;
    if slug.is_empty() || anchor.is_empty() {
        return None;
    }
    Some(BookRef {
        slug: slug.to_string(),
        anchor: anchor.to_string(),
    })
}

/// A parsed anchor: `ch-<n>-<slug>` (chapter) or `sec-<n>-<m>-<slug>` (section).
/// The trailing slug is informational; matching is by index.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Anchor {
    Chapter {
        index: u32,
        slug: String,
    },
    Section {
        chapter: u32,
        section: u32,
        slug: String,
    },
    /// Tolerant fallback for unrecognized anchors.
    Other(String),
}

impl Anchor {
    pub fn parse(anchor: &str) -> Self {
        if let Some(rest) = anchor.strip_prefix("ch-") {
            let mut parts = rest.splitn(2, '-');
            if let Some(num) = parts.next()
                && let Ok(index) = num.parse::<u32>()
            {
                if index == 0 {
                    // Anchors are 1-based; zero is invalid.
                    return Anchor::Other(anchor.to_string());
                }
                return Anchor::Chapter {
                    index,
                    slug: parts.next().unwrap_or_default().to_string(),
                };
            }
        }
        if let Some(rest) = anchor.strip_prefix("sec-") {
            let mut parts = rest.splitn(3, '-');
            if let (Some(c), Some(s)) = (parts.next(), parts.next())
                && let (Ok(chapter), Ok(section)) = (c.parse::<u32>(), s.parse::<u32>())
            {
                if chapter == 0 || section == 0 {
                    // Anchors are 1-based; zero is invalid.
                    return Anchor::Other(anchor.to_string());
                }
                return Anchor::Section {
                    chapter,
                    section,
                    slug: parts.next().unwrap_or_default().to_string(),
                };
            }
        }
        Anchor::Other(anchor.to_string())
    }
}

/// An extracted passage from a book's full text.
#[derive(Debug, Clone, PartialEq)]
pub struct Passage {
    pub slug: String,
    pub anchor: String,
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PassageError {
    #[error("anchor not found in book text: {0}")]
    AnchorNotFound(String),
    #[error("book text is empty")]
    EmptyBook,
}

/// Extract a passage from full book markdown text for the given anchor.
///
/// - Chapter anchor (`ch-<n>-...`): text from the n-th `# ` heading up to the
///   next `# ` heading (or EOF).
/// - Section anchor (`sec-<n>-<m>-...`): text from the matching `## ` heading
///   (the m-th section of the n-th chapter) up to the next `## ` or `# ` heading.
/// - No anchor (empty string): whole text (discovery mode handled by callers).
/// - Other anchors: `AnchorNotFound`.
///
/// The result is truncated to `max_chars` (callers pass their configured
/// limit; `READ_PASSAGE_MAX_CHARS` is the default).
pub fn extract_passage(slug: &str, anchor: &str, full_text: &str) -> Result<Passage, PassageError> {
    extract_passage_capped(slug, anchor, full_text, READ_PASSAGE_MAX_CHARS)
}

/// `extract_passage` with an explicit cap (admin-configurable passage
/// limit; the caller clamps it to sane bounds).
pub fn extract_passage_capped(
    slug: &str,
    anchor: &str,
    full_text: &str,
    max_chars: usize,
) -> Result<Passage, PassageError> {
    if full_text.trim().is_empty() {
        return Err(PassageError::EmptyBook);
    }
    let text = match Anchor::parse(anchor) {
        Anchor::Chapter { index, .. } => extract_chapter(full_text, index)?,
        Anchor::Section {
            chapter, section, ..
        } => extract_section(full_text, chapter, section)?,
        Anchor::Other(a) if a.is_empty() => full_text.to_string(),
        Anchor::Other(a) => return Err(PassageError::AnchorNotFound(a)),
    };
    let text: String = text.chars().take(max_chars).collect();
    Ok(Passage {
        slug: slug.to_string(),
        anchor: anchor.to_string(),
        text,
    })
}

/// One recognized heading of a book's markdown: its level, the byte
/// offset where the line starts, and — when the heading carries a GFM
/// explicit ID (`## Title {#id}`) — that ID. v1 stacks embed `ch-` /
/// `sec-` IDs on their headings; identification is by ID, never by
/// counting plain headings.
struct BookHeading {
    level: u8,
    offset: usize,
    anchor_id: Option<String>,
}

/// A markdown heading line: `#{1,6} Title {#id}` — the ID optional.
/// Mirrors v1's HEADING_RE. Fences must already have been handled by
/// the caller.
fn parse_heading_line(line: &str) -> Option<(u8, Option<String>)> {
    let trimmed = line.trim_end();
    let hashes = trimmed.len() - trimmed.trim_start_matches('#').len();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.starts_with(' ') {
        return None;
    }
    let title = rest.trim_start();
    // Optional GFM explicit ID: trailing `{#…}`.
    let id = title
        .strip_suffix('}')
        .and_then(|t| t.rsplit_once("{#"))
        .map(|(before, id)| {
            (
                before.trim_end(),
                id.strip_suffix('}').unwrap_or(id).to_string(),
            )
        });
    match id {
        Some((_, id)) => Some((hashes as u8, Some(id))),
        None => Some((hashes as u8, None)),
    }
}

/// Collect the recognized headings of a book's markdown (skipping
/// fenced code blocks). Offsets are byte positions into `text`.
fn collect_headings(text: &str) -> Vec<BookHeading> {
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence
            && trimmed.starts_with('#')
            && let Some((level, id)) = parse_heading_line(trimmed)
        {
            out.push(BookHeading {
                level,
                offset,
                anchor_id: id,
            });
        }
        offset += line.len();
    }
    out
}

/// The chapter index encoded in a `ch-<n>-…` or `sec-<n>-…` anchor ID,
/// when well-formed (v1: `chapterNumberOf`).
fn chapter_number_of(id: &str) -> Option<u32> {
    let parts: Vec<&str> = id.split('-').collect();
    if parts.len() >= 2 && !parts[1].is_empty() && parts[1].chars().all(|c| c.is_ascii_digit()) {
        parts[1].parse().ok()
    } else {
        None
    }
}

/// The passage slice for an anchor: from its heading to the boundary
/// heading. Section anchors (`sec-…`) end at the next
/// same-or-higher-level heading of any kind. Books whose headings
/// carry no `{#id}` anchors fall back to count-based extraction.
fn anchored_section(text: &str, chapter: u32, section: u32) -> Option<String> {
    let headings = collect_headings(text);
    if headings.iter().all(|h| h.anchor_id.is_none()) {
        return None; // no anchor IDs anywhere → count-based fallback
    }
    let start = headings.iter().position(|h| {
        matches!(
            h.anchor_id.as_deref(),
            Some(id) if section_numbers_of(id) == Some((chapter, section))
        )
    })?;
    let level = headings[start].level;
    let end = headings[start + 1..]
        .iter()
        .find(|h| h.level <= level)
        .map(|h| h.offset)
        .unwrap_or(text.len());
    Some(text[headings[start].offset..end].to_string())
}

/// The `(chapter, section)` encoded in a `sec-<c>-<s>-…` anchor ID.
fn section_numbers_of(id: &str) -> Option<(u32, u32)> {
    let rest = id.strip_prefix("sec-")?;
    let mut parts = rest.splitn(3, '-');
    let c = parts.next()?;
    let s = parts.next()?;
    if c.is_empty()
        || s.is_empty()
        || !c.chars().all(|x| x.is_ascii_digit())
        || !s.chars().all(|x| x.is_ascii_digit())
    {
        return None;
    }
    Some((c.parse().ok()?, s.parse().ok()?))
}

/// Extract the n-th chapter (1-based): from its `# ` heading to the next
/// `# ` heading. Headings inside fenced code blocks are ignored (mirrors
/// the librarian's `parse_outline`, so anchors line up with the catalog).
fn extract_chapter(text: &str, index: u32) -> Result<String, PassageError> {
    // v1 semantics: the anchor's chapter number picks the heading whose
    // embedded `{#ch-<n>-…}` ID encodes it — NOT the n-th `# ` heading
    // (real stacks carry hundreds of H1 section headings inside
    // chapters).
    let headings = collect_headings(text);
    if headings.iter().any(|h| h.anchor_id.is_some()) {
        let start = headings.iter().position(|h| {
            matches!(
                h.anchor_id.as_deref(),
                Some(id) if id.starts_with("ch-") && chapter_number_of(id) == Some(index)
            )
        });
        if let Some(pos) = start {
            let end = headings[pos + 1..]
                .iter()
                .find(|h| {
                    h.anchor_id
                        .as_deref()
                        .is_some_and(|id| id.starts_with("ch-"))
                })
                .map(|h| h.offset)
                .unwrap_or(text.len());
            return Ok(text[headings[pos].offset..end].to_string());
        }
        // Anchored book but no such chapter → not found (no fallback:
        // the count-based answer would be wrong on an anchored book).
        return Err(PassageError::AnchorNotFound(format!("ch-{index}")));
    }
    // Fallback (no embedded anchors): the n-th `# ` heading.
    count_based_chapter(text, index)
}

fn count_based_chapter(text: &str, index: u32) -> Result<String, PassageError> {
    let mut seen = 0u32;
    let mut start = None;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut offset = 0usize;
    let mut in_fence = false;
    for line in &lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence && trimmed.starts_with("# ") {
            seen += 1;
            if seen == index {
                start = Some(offset);
            } else if seen > index {
                // Defensive: start is None only if index == 0, which Anchor::parse
                // already rejects — but never unwrap a None.
                return start
                    .map(|s| text[s..offset].to_string())
                    .ok_or_else(|| PassageError::AnchorNotFound(format!("ch-{index}")));
            }
        }
        offset += line.len();
    }
    start
        .map(|s| text[s..].to_string())
        .ok_or_else(|| PassageError::AnchorNotFound(format!("ch-{index}")))
}

/// Extract the m-th section (1-based) of the n-th chapter (1-based).
/// Headings inside fenced code blocks are ignored (mirrors `parse_outline`).
fn extract_section(text: &str, chapter: u32, section: u32) -> Result<String, PassageError> {
    // v1 semantics: match by embedded `{#sec-<n>-<m>-…}` ID; the section
    // ends at the next same-or-higher-level heading of any kind.
    if let Some(passage) = anchored_section(text, chapter, section) {
        return Ok(passage);
    }
    // Fallback (no embedded anchors): the m-th `## ` within the n-th
    // `# ` heading's span.
    count_based_section(text, chapter, section)
}

fn count_based_section(text: &str, chapter: u32, section: u32) -> Result<String, PassageError> {
    // Find the chapter's start offset first.
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut seen_ch = 0u32;
    let mut ch_start_offset = None;
    let mut offset = 0usize;
    let mut in_fence = false;
    for line in &lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence && trimmed.starts_with("# ") {
            seen_ch += 1;
            if seen_ch == chapter {
                ch_start_offset = Some(offset);
                break;
            }
        }
        offset += line.len();
    }
    let ch_start =
        ch_start_offset.ok_or_else(|| PassageError::AnchorNotFound(format!("ch-{chapter}")))?;

    // Scan the chapter's text slice: find the m-th `## ` heading, stopping at
    // the next `# ` heading (or end of the chapter slice).
    let chapter_text = &text[ch_start..];
    let ch_lines: Vec<&str> = chapter_text.split_inclusive('\n').collect();
    let mut seen_sec = 0u32;
    let mut sec_start = None;
    let mut offset = 0usize;
    let mut in_fence = false;
    for line in &ch_lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence && offset > 0 && trimmed.starts_with("# ") {
            // Next chapter begins — return up to here (or not found).
            return sec_start
                .map(|s| chapter_text[s..offset].to_string())
                .ok_or_else(|| PassageError::AnchorNotFound(format!("sec-{chapter}-{section}")));
        } else if !in_fence && trimmed.starts_with("## ") {
            seen_sec += 1;
            if seen_sec == section {
                sec_start = Some(offset);
            } else if seen_sec > section {
                // Defensive: sec_start is None only if section == 0, which
                // Anchor::parse already rejects — but never unwrap a None.
                return sec_start
                    .map(|s| chapter_text[s..offset].to_string())
                    .ok_or_else(|| {
                        PassageError::AnchorNotFound(format!("sec-{chapter}-{section}"))
                    });
            }
        }
        offset += line.len();
    }
    sec_start
        .map(|s| chapter_text[s..].to_string())
        .ok_or_else(|| PassageError::AnchorNotFound(format!("sec-{chapter}-{section}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOK: &str = "\
# Chapter One

Intro text.

## Section 1.1

Details one.

## Section 1.2

Details two.

# Chapter Two

Second chapter text.

## Section 2.1

More details.
";

    #[test]
    fn parse_book_refs() {
        let r = parse_book_ref("book://my-book#ch-3-intro").unwrap();
        assert_eq!(r.slug, "my-book");
        assert_eq!(r.anchor, "ch-3-intro");
        assert!(parse_book_ref("not-a-book://x#y").is_none());
        assert!(parse_book_ref("book://#y").is_none());
        assert!(parse_book_ref("book://x#").is_none());
    }

    #[test]
    fn parse_anchors() {
        assert_eq!(
            Anchor::parse("ch-3-intro"),
            Anchor::Chapter {
                index: 3,
                slug: "intro".into()
            }
        );
        assert_eq!(
            Anchor::parse("sec-2-4-details"),
            Anchor::Section {
                chapter: 2,
                section: 4,
                slug: "details".into()
            }
        );
        assert_eq!(Anchor::parse("weird"), Anchor::Other("weird".into()));
    }

    #[test]
    fn extract_chapter_one() {
        let p = extract_passage("b", "ch-1-chapter-one", BOOK).unwrap();
        assert!(p.text.starts_with("# Chapter One"));
        assert!(p.text.contains("Section 1.2"));
        assert!(!p.text.contains("# Chapter Two"));
    }

    #[test]
    fn extract_chapter_two() {
        let p = extract_passage("b", "ch-2-chapter-two", BOOK).unwrap();
        assert!(p.text.starts_with("# Chapter Two"));
        assert!(p.text.contains("Section 2.1"));
    }

    #[test]
    fn extract_section() {
        let p = extract_passage("b", "sec-1-2-details-two", BOOK).unwrap();
        assert!(p.text.starts_with("## Section 1.2"));
        assert!(p.text.contains("Details two."));
        assert!(!p.text.contains("# Chapter Two"));
    }

    #[test]
    fn extract_section_in_later_chapter() {
        // Regression: sections of chapters >= 2 must not be corrupted by
        // offsets from earlier chapters.
        let p = extract_passage("b", "sec-2-1-more-details", BOOK).unwrap();
        assert!(p.text.starts_with("## Section 2.1"));
        assert!(p.text.contains("More details."));
        assert!(!p.text.contains("Chapter One"));
        assert!(!p.text.contains("Section 1.1"));
    }

    #[test]
    fn extract_missing_anchor_errors() {
        assert!(extract_passage("b", "ch-9-nope", BOOK).is_err());
        assert!(extract_passage("b", "sec-9-9-nope", BOOK).is_err());
        assert!(extract_passage("b", "garbage", BOOK).is_err());
    }

    #[test]
    fn empty_book_errors() {
        assert!(extract_passage("b", "ch-1-x", "   ").is_err());
    }

    #[test]
    fn cap_truncates() {
        let big = format!(
            "# Chapter One\n\n{}",
            "x".repeat(READ_PASSAGE_MAX_CHARS + 100)
        );
        let p = extract_passage("b", "ch-1-one", &big).unwrap();
        assert!(p.text.chars().count() <= READ_PASSAGE_MAX_CHARS);
    }

    #[test]
    fn empty_anchor_returns_whole_text() {
        let p = extract_passage("b", "", BOOK).unwrap();
        assert_eq!(p.text, BOOK);
    }

    #[test]
    fn fenced_headings_are_ignored() {
        // A `# ` line inside a fenced code block must not count as a
        // chapter boundary — mirrors the librarian's parse_outline so
        // book:// anchors line up with the catalog.
        let book = "\
# Chapter One

Intro.

```
# not a chapter
## not a section
```

## Section 1.1

Details.

# Chapter Two

Second.
";
        // Chapter 2 is the SECOND real heading, not the fenced one.
        let p = extract_passage("b", "ch-2-chapter-two", book).unwrap();
        assert!(p.text.starts_with("# Chapter Two"));
        assert!(!p.text.contains("not a chapter"));
        // Section 1.1 is the first real section of chapter 1.
        let s = extract_passage("b", "sec-1-1-section-1-1", book).unwrap();
        assert!(s.text.starts_with("## Section 1.1"));
        // The fenced fake section must not shift section numbering.
        assert!(extract_passage("b", "sec-1-2-x", book).is_err());
    }

    // v1-format book: `# ` headings carry GFM `{#id}` anchors; top-level
    // `# sec-N-M-…` headings live INSIDE chapters (marker-pdf emits them
    // as H1). 430 H1 headings, 9 chapters on the real books.
    const V1_BOOK: &str = "\
---
title: A Real Book
---

# Front Matter {#sec-0-1-front-matter}

Publisher junk.

# Chapter 1: Basics {#ch-1-chapter-1-basics}
<!-- src: pp. 1-9 -->

# Introduction {#sec-1-1-introduction}

Chances are that if you've worked on Linux you know grep.

## Conventions {#sec-1-2-conventions}

Details of conventions.

# Conceptual Overview {#sec-1-12-conceptual-overview}

The conceptual overview section.

# Chapter 2: Advanced {#ch-2-chapter-2-advanced}

# Deep Dive {#sec-2-1-deep-dive}

Deep content here.
";

    #[test]
    fn v1_chapter_extraction_spans_embedded_h1_sections() {
        // THE BUG: v1 stacks carry `# sec-…` H1 headings inside chapters;
        // chapter extraction must run to the NEXT ch- ANCHORED heading,
        // not the next `# ` line.
        let p = extract_passage("b", "ch-1-chapter-1-basics", V1_BOOK).unwrap();
        assert!(p.text.contains("# Chapter 1: Basics"));
        assert!(p.text.contains("Chances are that if you've worked"));
        assert!(p.text.contains("Conceptual Overview"));
        assert!(p.text.contains("Details of conventions."));
        assert!(!p.text.contains("# Chapter 2: Advanced"));
    }

    #[test]
    fn v1_chapter_skips_front_matter_and_unanchored_headings() {
        // Unanchored / sec-0 headings (title page, "Brief Contents") are
        // NOT chapters: the anchor's chapter number picks the chapter,
        // not the heading count.
        let p = extract_passage("b", "ch-2-chapter-2-advanced", V1_BOOK).unwrap();
        assert!(p.text.contains("# Chapter 2: Advanced"));
        assert!(p.text.contains("Deep content here."));
        assert!(!p.text.contains("Front Matter"));
    }

    #[test]
    fn v1_section_extraction_by_embedded_id() {
        // Sections match by their embedded {#sec-N-M} anchor; end at the
        // next heading of ANY level that is same-or-higher level.
        let p = extract_passage("b", "sec-1-12-conceptual-overview", V1_BOOK).unwrap();
        assert!(p.text.contains("# Conceptual Overview"));
        assert!(p.text.contains("The conceptual overview section."));
        assert!(!p.text.contains("# Chapter 2"));
    }

    #[test]
    fn v1_h1_section_inside_chapter_is_a_section_not_a_chapter() {
        // `# Introduction {#sec-1-1-…}` is section 1 of chapter 1 even
        // though it is an H1 — matching is by anchor ID, not level.
        let p = extract_passage("b", "sec-1-1-introduction", V1_BOOK).unwrap();
        assert!(p.text.contains("# Introduction"));
        assert!(p.text.contains("Chances are that if you've worked"));
        assert!(p.text.contains("Details of conventions."));
        assert!(!p.text.contains("# Conceptual Overview"));
    }

    #[test]
    fn v1_missing_anchor_errors() {
        assert!(extract_passage("b", "ch-9-nope", V1_BOOK).is_err());
        assert!(extract_passage("b", "sec-9-9-nope", V1_BOOK).is_err());
    }

    // Fallback: books whose headings carry NO {#id} anchors keep the
    // count-based semantics (mycelium2's own synthetic/simple books).
    #[test]
    fn plain_book_still_extracts_by_count() {
        let p = extract_passage("b", "ch-1-chapter-one", BOOK).unwrap();
        assert!(p.text.starts_with("# Chapter One"));
        assert!(p.text.contains("Section 1.2"));
        assert!(!p.text.contains("# Chapter Two"));
        let p2 = extract_passage("b", "ch-2-chapter-two", BOOK).unwrap();
        assert!(p2.text.starts_with("# Chapter Two"));
        assert!(p2.text.contains("Section 2.1"));
    }

    #[test]
    fn tilde_fences_are_ignored() {
        let book = "\
# Chapter One

~~~
# fake
~~~

Real text.
";
        let p = extract_passage("b", "ch-1-chapter-one", book).unwrap();
        assert!(p.text.contains("Real text."));
        assert!(extract_passage("b", "ch-2-fake", book).is_err());
    }
}
