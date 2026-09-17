//! Chapter/section extraction and catalog building.
//!
//! Two cataloging paths:
//! - **Heuristic** (always available): parse `#`/`##` headings from the
//!   book text into an outline; build the book hub + chapter concepts
//!   from the outline. No LLM required.
//! - **LLM-assisted** (when the configured backend is reachable): the
//!   librarian asks the model for a structured catalog (title,
//!   description, chapter summaries); on any LLM failure the heuristic
//!   catalog is used unchanged (ROADMAP risk: "LLM extraction quality:
//!   librarian output may need fallback/heuristic parsing").

use mycelium_core::concept::{Concept, Frontmatter};
use serde::Deserialize;

use crate::llm::{LlmClient, LlmError, strip_code_fence};

/// One section parsed from the book text.
#[derive(Debug, Clone, PartialEq)]
pub struct SectionOutline {
    /// 1-based section index: for anchored books the REAL section
    /// number encoded in the ID (`sec-1-12-…` → 12), for plain books
    /// the encounter-order count.
    pub index: u32,
    /// Heading text (e.g. "Conventions").
    pub title: String,
    /// The REAL anchor ID (`sec-1-12-conceptual-overview`) when the
    /// heading carries a GFM `{#id}`; None for plain books.
    pub anchor_id: Option<String>,
}

/// One chapter parsed from the book text.
#[derive(Debug, Clone, PartialEq)]
pub struct ChapterOutline {
    /// 1-based chapter index (the number encoded in the anchor ID for
    /// anchored books, encounter order for plain ones).
    pub index: u32,
    /// Heading text (e.g. "Chapter One").
    pub title: String,
    /// Slugified anchor suffix (e.g. "chapter-one").
    pub slug: String,
    /// Sections within the chapter.
    pub sections: Vec<SectionOutline>,
    /// The REAL anchor ID embedded in the stack heading
    /// (`ch-1-chapter-1-basics`) when the book carries GFM `{#id}`
    /// anchors; None for count-derived chapters of plain books.
    pub anchor_id: Option<String>,
}

/// The parsed outline of a book.
#[derive(Debug, Clone, Default)]
pub struct BookOutline {
    pub chapters: Vec<ChapterOutline>,
}

/// The GFM explicit ID of a heading line, when it carries one:
/// `## Title {#anchor}` → `anchor`. Mirrors the core's heading parser.
fn heading_anchor_id(trimmed: &str) -> Option<String> {
    let title = trimmed.trim_start_matches('#').trim_start();
    let id = title
        .strip_suffix('}')
        .and_then(|t| t.rsplit_once("{#"))
        .map(|(_, id)| id.strip_suffix('}').unwrap_or(id))?;
    (!id.is_empty()).then(|| id.to_string())
}

/// Is this embedded anchor ID a chapter ID? v1's rule: `ch-` prefix
/// with a numeric chapter number (`ch-<n>-<slug>`; also matches
/// digit-leading titles like `ch-1-1-python-basics`).
fn is_chapter_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('-').collect();
    parts.len() >= 3
        && parts[0] == "ch"
        && !parts[1].is_empty()
        && parts[1].chars().all(|c| c.is_ascii_digit())
}

/// Is this embedded anchor ID a section ID? `sec-<n>-<m>-…` with both
/// numbers present (`sec-0-…` front matter included; the caller
/// filters by chapter number).
fn is_section_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('-').collect();
    parts.len() >= 4
        && parts[0] == "sec"
        && !parts[1].is_empty()
        && parts[1].chars().all(|c| c.is_ascii_digit())
        && !parts[2].is_empty()
        && parts[2].chars().all(|c| c.is_ascii_digit())
}

/// The `(chapter, section)` numbers encoded in a `sec-<c>-<s>-…` anchor
/// ID. Mirrors the core's `section_numbers_of` (sec-0 front matter
/// included; the caller filters it out).
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

/// Parse the book outline. When headings carry GFM `{#id}` anchors
/// (v1 stacks — 430 H1s, 9 chapters), chapters are the `ch-`-anchored
/// headings (indexed by their embedded number, front matter excluded)
/// and sections the `sec-`-anchored headings grouped by chapter number;
/// plain unanchored headings fall back to counting `# ` chapters and
/// `## ` sections. Headings inside fenced code blocks are ignored in
/// both modes.
pub fn parse_outline(text: &str) -> BookOutline {
    // Collect (anchor_id, title, level) outside fences first: the
    // anchored path needs IDs at any heading level.
    let mut fenced: Vec<(Option<String>, String, u8)> = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let hashes = trimmed.len() - trimmed.trim_start_matches('#').len();
        if hashes == 0 || hashes > 6 || !trimmed[hashes..].starts_with(' ') {
            continue;
        }
        let full_title = trimmed[hashes..].trim_start().trim();
        // Strip the `{#id}` suffix from the title, as the core does:
        // the title alone ("Chapter 1: Basics") is what the catalog shows.
        let title = heading_anchor_id(trimmed)
            .and_then(|id| {
                full_title
                    .strip_suffix(&format!("{{#{id}}}"))
                    .map(|t| t.trim_end().to_string())
            })
            .unwrap_or_else(|| full_title.to_string());
        fenced.push((heading_anchor_id(trimmed), title, hashes as u8));
    }
    let anchored = fenced.iter().any(|(id, _, _)| id.is_some());
    if anchored {
        // v1 path: chapters and sections by embedded ID. The chapter's
        // index comes from its `ch-<n>` number; sections group by the
        // chapter number encoded in their ID and keep their REAL
        // section numbers (`sec-1-12` stays 12).
        let mut chapters: Vec<ChapterOutline> = Vec::new();
        let mut sections_by_chapter: std::collections::BTreeMap<u32, Vec<SectionOutline>> =
            std::collections::BTreeMap::new();
        for (id, title, _) in &fenced {
            let Some(id) = id else { continue };
            if is_chapter_id(id) {
                // `ch-<n>-<slug>`: the number after `ch-`.
                let n = id.split('-').nth(1).and_then(|p| p.parse::<u32>().ok());
                if let Some(n) = n.filter(|n| *n > 0) {
                    chapters.push(ChapterOutline {
                        index: n,
                        title: title.clone(),
                        slug: slugify(title),
                        sections: Vec::new(),
                        anchor_id: Some(id.clone()),
                    });
                }
            } else if is_section_id(id) {
                // `sec-<c>-<m>-<slug>`: chapter c, section m. sec-0 =
                // front matter, not part of any chapter.
                if let Some((c, m)) = section_numbers_of(id).filter(|(c, _)| *c > 0) {
                    sections_by_chapter
                        .entry(c)
                        .or_default()
                        .push(SectionOutline {
                            index: m,
                            title: title.clone(),
                            anchor_id: Some(id.clone()),
                        });
                }
            }
        }
        for ch in &mut chapters {
            if let Some(mut secs) = sections_by_chapter.remove(&ch.index) {
                secs.sort_by_key(|s| s.index);
                ch.sections = secs;
            }
        }
        chapters.sort_by_key(|c| c.index);
        return BookOutline { chapters };
    }
    // Fallback (no embedded anchors): count `# ` chapters / `## ` sections.
    let mut chapters: Vec<ChapterOutline> = Vec::new();
    for (_, title, level) in &fenced {
        match level {
            1 => {
                let index = chapters.len() as u32 + 1;
                chapters.push(ChapterOutline {
                    index,
                    title: title.clone(),
                    slug: slugify(title),
                    sections: Vec::new(),
                    anchor_id: None,
                });
            }
            2 => {
                if let Some(ch) = chapters.last_mut() {
                    let section_index = ch.sections.len() as u32 + 1;
                    ch.sections.push(SectionOutline {
                        index: section_index,
                        title: title.clone(),
                        anchor_id: None,
                    });
                }
            }
            _ => {}
        }
    }
    BookOutline { chapters }
}

/// Slugify a heading into the anchor-suffix form: lowercase, alphanumerics
/// and dashes, collapsed whitespace, no leading/trailing dash.
pub fn slugify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_dash = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The catalog the librarian writes: one hub concept + chapter concepts.
#[derive(Debug, Clone)]
pub struct BookCatalog {
    pub slug: String,
    pub title: String,
    pub description: String,
    /// Per-chapter summaries (parallel to the outline's chapters).
    pub chapter_summaries: Vec<String>,
}

/// Build the book hub concept (`/<slug>/book.md`).
pub fn build_hub_concept(catalog: &BookCatalog, outline: &BookOutline) -> Concept {
    let mut body = String::new();
    body.push_str("# About\n\n");
    if catalog.description.is_empty() {
        body.push_str(&format!(
            "Catalog for **{}**. Full text lives in the shared library stacks.\n",
            catalog.title
        ));
    } else {
        body.push_str(&catalog.description);
        body.push('\n');
    }
    body.push_str("\n# Chapters\n\n");
    for ch in &outline.chapters {
        // Link the chapter concept's REAL source path: the embedded
        // anchor ID on anchored books, count-derived on plain ones.
        body.push_str(&format!(
            "- [{}](/{}/{}.md)\n",
            ch.title,
            catalog.slug,
            chapter_anchor_id(ch)
        ));
    }
    body.push_str("\n# Related Concepts\n\n");
    body.push_str("Chapters link back to this hub.\n");
    Concept::new(
        Frontmatter {
            concept_type: "Book".into(),
            title: Some(catalog.title.clone()),
            description: Some(format!("Book catalog: {}", catalog.title)),
            resource: Some(format!("book://{}", catalog.slug)),
            ..Default::default()
        },
        body,
        format!("/{}/book.md", catalog.slug),
    )
}

/// Build one chapter concept (`/<slug>/<anchor_id>.md` for anchored
/// books, `/<slug>/ch-<n>-<slug>.md` for plain ones). All book://
/// references use the REAL anchor IDs from the stack, so every link
/// resolves.
#[allow(clippy::needless_pass_by_value)]
pub fn build_chapter_concept(
    catalog: &BookCatalog,
    _outline: &BookOutline,
    chapter: &ChapterOutline,
) -> Concept {
    let summary = catalog
        .chapter_summaries
        .get(chapter.index as usize - 1)
        .cloned()
        .unwrap_or_default();
    let chapter_anchor = chapter_anchor_id(chapter);
    let chapter_uri = format!("book://{}#{}", catalog.slug, chapter_anchor);
    let mut body = String::new();
    body.push_str(&format!("# {}\n\n", chapter.title));
    if !summary.is_empty() {
        body.push_str("# Summary\n\n");
        body.push_str(&summary);
        body.push_str("\n\n");
    }
    if !chapter.sections.is_empty() {
        body.push_str("# Sections\n\n");
        for section in &chapter.sections {
            body.push_str(&format!(
                "- [{}](book://{}#{})\n",
                section.title,
                catalog.slug,
                section_anchor_id(chapter, section)
            ));
        }
        body.push('\n');
    }
    body.push_str("# Full passage\n\n");
    body.push_str(&format!("Read the full chapter via `{chapter_uri}`.\n"));
    Concept::new(
        Frontmatter {
            concept_type: "Chapter".into(),
            title: Some(chapter.title.clone()),
            description: Some(format!("Chapter {} of {}", chapter.index, catalog.title)),
            resource: Some(chapter_uri.clone()),
            book: Some(format!("/{}/book.md", catalog.slug)),
            chapter_index: Some(chapter.index),
            ..Default::default()
        },
        body,
        format!("/{}/{}.md", catalog.slug, chapter_anchor),
    )
}

/// The passage anchor for a chapter concept: the stack's REAL anchor ID
/// (`ch-1-chapter-1-basics`) when the book carries `{#id}` anchors, the
/// count-derived `ch-<n>-<slug>` for plain books (which is exactly what
/// the core's count-based extractor resolves).
fn chapter_anchor_id(chapter: &ChapterOutline) -> String {
    match &chapter.anchor_id {
        Some(id) => id.clone(),
        None => format!("ch-{}-{}", chapter.index, chapter.slug),
    }
}

/// The passage anchor for one section link. Anchored books: the REAL
/// `sec-<c>-<m>-<slug>` ID. Plain books: the count-derived anchor the
/// core resolves.
fn section_anchor_id(chapter: &ChapterOutline, section: &SectionOutline) -> String {
    match &section.anchor_id {
        Some(id) => id.clone(),
        None => format!(
            "sec-{}-{}-{}",
            chapter.index,
            section.index,
            slugify(&section.title)
        ),
    }
}

/// The structured catalog the LLM is asked to produce: book-level only.
#[derive(Debug, Clone, Deserialize)]
struct LlmCatalog {
    title: String,
    #[serde(default)]
    description: String,
}

/// Build a catalog with LLM assistance for the BOOK-LEVEL title and
/// description only, falling back to the heuristic outline on any LLM
/// failure (unreachable endpoint, bad JSON, missing fields). The
/// fallback is total: the heuristic catalog is complete and correct on
/// its own.
///
/// Chapter descriptions are DETERMINISTIC — extracted from each
/// chapter's actual opening prose in the book text (v1's card-catalog
/// approach). The LLM is never asked to summarize chapters: asking for
/// hundreds of summaries from titles alone is a hallucination lottery
/// that poisons the catalog (observed: one generic summary smeared
/// across 697 concepts).
pub async fn build_catalog(
    client: Option<&LlmClient>,
    slug: &str,
    title_hint: &str,
    outline: &BookOutline,
    text: &str,
) -> BookCatalog {
    let mut catalog = heuristic_catalog(slug, title_hint, outline);
    // Deterministic per-chapter descriptions from the real text.
    catalog.chapter_summaries = chapter_descriptions(outline, text);
    let Some(client) = client else {
        return catalog;
    };
    match llm_catalog(client, slug, title_hint, outline).await {
        Ok(llm) => {
            if !llm.title.trim().is_empty() {
                catalog.title = llm.title.trim().to_string();
            }
            if !llm.description.trim().is_empty() {
                catalog.description = llm.description.trim().to_string();
            }
        }
        Err(e) => {
            tracing::warn!(slug, error = %e, "LLM cataloging failed — using heuristic catalog");
        }
    }
    catalog
}

/// Extract a deterministic description for each chapter: the first
/// ~200 chars of the chapter's own prose (skipping the heading line and
/// any sub-headings), whitespace-collapsed. Mirrors v1's pdf-to-markdown
/// sidecar descriptions — real text, never invented.
///
/// Positional for plain books: slices the text between chapter N's `# `
/// heading and chapter N+1's heading. Anchored books: between the
/// heading whose REAL `{#ch-<n>-…}` ID matches and the next `ch-`
/// heading (hundreds of H1 section headings lie between two real
/// chapters — count-position is wrong there).
fn chapter_descriptions(outline: &BookOutline, text: &str) -> Vec<String> {
    // Fence-aware walk: (line index, level, anchor_id) per heading.
    let mut headings: Vec<(usize, u8, Option<String>)> = Vec::new();
    let mut in_fence = false;
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence && t.starts_with('#') {
            let hashes = t.len() - t.trim_start_matches('#').len();
            if hashes > 0 && hashes <= 6 && t[hashes..].starts_with(' ') {
                headings.push((i, hashes as u8, heading_anchor_id(t)));
            }
        }
    }
    outline
        .chapters
        .iter()
        .map(|ch| {
            // The chapter slice = the same span the passage extractor
            // returns: on anchored books from the heading carrying the
            // REAL ch- ID to the next ch- heading; on plain books from
            // the n-th H1 to the next H1 (or EOF). The description is
            // the chapter's real prose inside that span. `pos` indexes
            // the `headings` vec; `line` is the heading's line number.
            let found = match &ch.anchor_id {
                Some(id) => headings
                    .iter()
                    .position(|(_, _, h_id)| h_id.as_deref() == Some(id.as_str()))
                    .map(|p| (p, headings[p].0)),
                None => headings
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, level, _))| *level == 1)
                    .nth(ch.index as usize - 1)
                    .map(|(p, (i, _, _))| (p, *i)),
            };
            let Some((pos, line)) = found else {
                return String::new();
            };
            let end = match &ch.anchor_id {
                Some(_) => headings[pos + 1..]
                    .iter()
                    .find(|(_, _, h_id)| h_id.as_deref().is_some_and(|x| x.starts_with("ch-")))
                    .map(|(i, _, _)| *i)
                    .unwrap_or(text.lines().count()),
                None => headings[pos + 1..]
                    .iter()
                    .find(|(_, level, _)| *level == 1)
                    .map(|(i, _, _)| *i)
                    .unwrap_or(text.lines().count()),
            };
            let body = text
                .lines()
                .skip(line + 1) // the chapter heading line
                .take(end.saturating_sub(line + 1))
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect::<Vec<_>>()
                .join(" ");
            let mut out = String::new();
            for word in body.split_whitespace() {
                if out.len() + word.len() + 1 > 200 {
                    break;
                }
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(word);
            }
            out
        })
        .collect()
}

/// The heuristic catalog: title hint + first-paragraph description +
/// empty summaries.
fn heuristic_catalog(slug: &str, title_hint: &str, outline: &BookOutline) -> BookCatalog {
    let description = outline
        .chapters
        .iter()
        .map(|c| c.title.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    BookCatalog {
        slug: slug.to_string(),
        title: title_hint.to_string(),
        description: if outline.chapters.is_empty() {
            String::new()
        } else {
            format!("Chapters: {description}")
        },
        chapter_summaries: vec![String::new(); outline.chapters.len()],
    }
}

/// Ask the LLM for a structured catalog. The prompt repeats the
/// absolute-link rule (the original Mycelium's cataloging prompt
/// learned that `./`-style links produce disconnected catalogs).
async fn llm_catalog(
    client: &LlmClient,
    slug: &str,
    title_hint: &str,
    outline: &BookOutline,
) -> Result<LlmCatalog, LlmError> {
    let chapter_list = outline
        .chapters
        .iter()
        .map(|c| format!("{}. {}", c.index, c.title))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "You are a librarian cataloging a book for a knowledge base.\n\
         Book slug: {slug}\n\
         Title hint: {title_hint}\n\
         Chapter outline:\n{chapter_list}\n\n\
         Produce a JSON object with exactly these fields:\n\
         - \"title\": the book's title\n\
         - \"description\": 2-3 sentences describing the book\n\n\
         Respond with ONLY the JSON object, no markdown fences, no commentary."
    );
    let raw = client.chat(&prompt).await?;
    let json = strip_code_fence(&raw);
    serde_json::from_str(&json).map_err(|e| {
        // Parse failures are LLM-quality failures, not transport ones.
        tracing::warn!(slug, error = %e, "LLM returned unparseable catalog JSON");
        LlmError::EmptyResponse
    })
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
    fn outline_parses_chapters_and_sections() {
        let outline = parse_outline(BOOK);
        assert_eq!(outline.chapters.len(), 2);
        assert_eq!(outline.chapters[0].title, "Chapter One");
        assert_eq!(outline.chapters[0].index, 1);
        assert_eq!(outline.chapters[0].slug, "chapter-one");
        assert_eq!(outline.chapters[0].sections.len(), 2);
        assert_eq!(outline.chapters[0].sections[0].index, 1);
        assert_eq!(outline.chapters[0].sections[0].title, "Section 1.1");
        assert_eq!(outline.chapters[0].sections[0].anchor_id, None);
        assert_eq!(outline.chapters[1].sections[0].index, 1);
        assert_eq!(outline.chapters[1].sections[0].title, "Section 2.1");
    }

    // v1-format book: `# ` headings carry GFM `{#id}` anchors and H1
    // `sec-` sections live INSIDE chapters. Chapters are the `ch-`
    // anchored headings; sections the `sec-` anchored ones.
    const V1_BOOK: &str = "\
# Front Matter {#sec-0-1-front-matter}

Publisher junk.

# Chapter 1: Basics {#ch-1-chapter-1-basics}

# Introduction {#sec-1-1-introduction}

Intro prose.

## Conventions {#sec-1-2-conventions}

Details of conventions.

# Conceptual Overview {#sec-1-12-conceptual-overview}

Overview text.

# Chapter 2: Advanced {#ch-2-chapter-2-advanced}

# Deep Dive {#sec-2-1-deep-dive}

Deep content.
";

    #[test]
    fn v1_outline_chapters_by_anchor_id_not_by_count() {
        // 430 H1s, 9 chapters on the real books: ONLY ch-anchored
        // headings are chapters; sec-0 (front matter) is not one.
        let outline = parse_outline(V1_BOOK);
        assert_eq!(outline.chapters.len(), 2, "got: {outline:?}");
        assert_eq!(outline.chapters[0].title, "Chapter 1: Basics");
        assert_eq!(outline.chapters[0].index, 1);
        assert_eq!(outline.chapters[1].title, "Chapter 2: Advanced");
        assert_eq!(outline.chapters[1].index, 2);
    }

    #[test]
    fn v1_outline_sections_from_embedded_ids() {
        // Sections group by the chapter number encoded in the sec- ID:
        // `# Introduction {#sec-1-1-…}` and `# Conceptual Overview
        // {#sec-1-12-…}` are BOTH chapter 1 sections despite the gap;
        // `sec-0` front matter belongs to no chapter.
        let outline = parse_outline(V1_BOOK);
        let ch1 = &outline.chapters[0];
        assert_eq!(ch1.sections.len(), 3, "sections: {:?}", ch1.sections);
        assert_eq!(ch1.sections[0].index, 1);
        assert_eq!(ch1.sections[0].title, "Introduction");
        assert_eq!(ch1.sections[1].index, 2);
        assert_eq!(ch1.sections[1].title, "Conventions");
        assert_eq!(ch1.sections[2].index, 12, "REAL number, not renumbered");
        assert_eq!(ch1.sections[2].title, "Conceptual Overview");
        assert_eq!(outline.chapters[1].sections.len(), 1);
        assert_eq!(outline.chapters[1].sections[0].index, 1);
        assert_eq!(outline.chapters[1].sections[0].title, "Deep Dive");
    }

    #[test]
    fn v1_outline_section_anchors_carry_real_ids() {
        // Chapter-concept section links must use the stack's REAL
        // `sec-` anchor IDs, not renumbered count-derived ones.
        let outline = parse_outline(V1_BOOK);
        assert_eq!(
            outline.chapters[0].sections[2].anchor_id,
            Some("sec-1-12-conceptual-overview".into())
        );
        assert_eq!(
            outline.chapters[1].sections[0].anchor_id,
            Some("sec-2-1-deep-dive".into())
        );
    }

    #[test]
    fn v1_outline_chapter_anchors_carry_real_ids() {
        // Catalog chapter concepts must reference the stack's REAL
        // anchor IDs (`ch-1-chapter-1-basics`), not count-derived ones.
        let outline = parse_outline(V1_BOOK);
        assert_eq!(
            outline.chapters[0].anchor_id,
            Some("ch-1-chapter-1-basics".into())
        );
        assert_eq!(
            outline.chapters[1].anchor_id,
            Some("ch-2-chapter-2-advanced".into())
        );
    }

    #[test]
    fn outline_ignores_fenced_headings() {
        let text = "# Real\n\n```\n# Fake Chapter\n## Fake Section\n```\n\n# Real Two\n";
        let outline = parse_outline(text);
        assert_eq!(outline.chapters.len(), 2);
        assert_eq!(outline.chapters[0].title, "Real");
        assert_eq!(outline.chapters[1].title, "Real Two");
    }

    #[test]
    fn slugify_basics() {
        assert_eq!(
            slugify("Chapter One: The Beginning!"),
            "chapter-one-the-beginning"
        );
        assert_eq!(slugify("  spaces   collapse  "), "spaces-collapse");
        assert_eq!(slugify("Ünïcödé"), "ünïcödé");
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn hub_concept_shape() {
        let outline = parse_outline(BOOK);
        let catalog = heuristic_catalog("my-book", "My Book", &outline);
        let hub = build_hub_concept(&catalog, &outline);
        assert_eq!(hub.source_path, "/my-book/book.md");
        assert_eq!(hub.frontmatter.concept_type, "Book");
        assert_eq!(hub.frontmatter.resource.as_deref(), Some("book://my-book"));
        assert!(
            hub.body
                .contains("[Chapter One](/my-book/ch-1-chapter-one.md)")
        );
        assert!(
            hub.body
                .contains("[Chapter Two](/my-book/ch-2-chapter-two.md)")
        );
        // Hub links are absolute leading-slash (graph-scannable).
        let links = mycelium_core::links::scan_links(&hub.body);
        assert_eq!(
            links,
            vec![
                "/my-book/ch-1-chapter-one.md",
                "/my-book/ch-2-chapter-two.md"
            ]
        );
    }

    #[test]
    fn chapter_concept_shape() {
        let outline = parse_outline(BOOK);
        let catalog = heuristic_catalog("my-book", "My Book", &outline);
        let ch = build_chapter_concept(&catalog, &outline, &outline.chapters[0]);
        assert_eq!(ch.source_path, "/my-book/ch-1-chapter-one.md");
        assert_eq!(ch.frontmatter.concept_type, "Chapter");
        assert_eq!(ch.frontmatter.book.as_deref(), Some("/my-book/book.md"));
        assert_eq!(ch.frontmatter.chapter_index, Some(1));
        assert_eq!(
            ch.frontmatter.resource.as_deref(),
            Some("book://my-book#ch-1-chapter-one")
        );
        assert!(ch.body.contains("book://my-book#ch-1-chapter-one"));
        // Round-trips through Concept::parse.
        let md = ch.to_markdown().unwrap();
        let parsed = Concept::parse(&ch.source_path, &md).unwrap();
        assert_eq!(parsed.frontmatter.chapter_index, Some(1));
    }

    #[test]
    fn empty_outline_catalog() {
        let outline = parse_outline("no headings at all");
        assert!(outline.chapters.is_empty());
        let catalog = heuristic_catalog("b", "T", &outline);
        let hub = build_hub_concept(&catalog, &outline);
        assert!(hub.body.contains("# Chapters"));
    }

    #[tokio::test]
    async fn llm_failure_falls_back() {
        let outline = parse_outline(BOOK);
        let client = LlmClient::new(&crate::llm::LlmConfig {
            url: "http://127.0.0.1:1/v1".into(),
            model: "m".into(),
        });
        let catalog = build_catalog(Some(&client), "my-book", "My Book", &outline, BOOK).await;
        assert_eq!(catalog.title, "My Book");
        // Deterministic descriptions extracted from the real text
        // (the chapter's own prose, not an LLM's guess).
        assert_eq!(
            catalog.chapter_summaries[0],
            "Intro text. Details one. Details two."
        );
        assert_eq!(
            catalog.chapter_summaries[1],
            "Second chapter text. More details."
        );
    }
}
