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

/// One chapter parsed from the book text.
#[derive(Debug, Clone, PartialEq)]
pub struct ChapterOutline {
    /// 1-based chapter index.
    pub index: u32,
    /// Heading text (e.g. "Chapter One").
    pub title: String,
    /// Slugified anchor suffix (e.g. "chapter-one").
    pub slug: String,
    /// Sections within the chapter (1-based index + heading text).
    pub sections: Vec<(u32, String)>,
}

/// The parsed outline of a book.
#[derive(Debug, Clone, Default)]
pub struct BookOutline {
    pub chapters: Vec<ChapterOutline>,
}

/// Parse `# ` (chapter) and `## ` (section) headings from book text.
/// Headings inside fenced code blocks are ignored.
pub fn parse_outline(text: &str) -> BookOutline {
    let mut chapters: Vec<ChapterOutline> = Vec::new();
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
        if let Some(title) = trimmed.strip_prefix("# ") {
            let index = chapters.len() as u32 + 1;
            chapters.push(ChapterOutline {
                index,
                title: title.trim().to_string(),
                slug: slugify(title),
                sections: Vec::new(),
            });
        } else if let Some(title) = trimmed.strip_prefix("## ")
            && let Some(ch) = chapters.last_mut()
        {
            let section_index = ch.sections.len() as u32 + 1;
            ch.sections.push((section_index, title.trim().to_string()));
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
        body.push_str(&format!(
            "- [{}](/{}/ch-{}-{}.md)\n",
            ch.title, catalog.slug, ch.index, ch.slug
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

/// Build one chapter concept (`/<slug>/ch-<n>-<slug>.md`).
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
    let mut body = String::new();
    body.push_str(&format!("# {}\n\n", chapter.title));
    if !summary.is_empty() {
        body.push_str("# Summary\n\n");
        body.push_str(&summary);
        body.push_str("\n\n");
    }
    if !chapter.sections.is_empty() {
        body.push_str("# Sections\n\n");
        for (i, title) in &chapter.sections {
            body.push_str(&format!(
                "- [{}](book://{}#sec-{}-{}-{})\n",
                title,
                catalog.slug,
                chapter.index,
                i,
                slugify(title)
            ));
        }
        body.push('\n');
    }
    body.push_str("# Full passage\n\n");
    body.push_str(&format!(
        "Read the full chapter via `book://{}#ch-{}-{}`.\n",
        catalog.slug, chapter.index, chapter.slug
    ));
    Concept::new(
        Frontmatter {
            concept_type: "Chapter".into(),
            title: Some(chapter.title.clone()),
            description: Some(format!("Chapter {} of {}", chapter.index, catalog.title)),
            resource: Some(format!(
                "book://{}#ch-{}-{}",
                catalog.slug, chapter.index, chapter.slug
            )),
            book: Some(format!("/{}/book.md", catalog.slug)),
            chapter_index: Some(chapter.index),
            ..Default::default()
        },
        body,
        format!("/{}/ch-{}-{}.md", catalog.slug, chapter.index, chapter.slug),
    )
}

/// The structured catalog the LLM is asked to produce.
#[derive(Debug, Clone, Deserialize)]
struct LlmCatalog {
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    chapters: Vec<LlmChapter>,
}

#[derive(Debug, Clone, Deserialize)]
struct LlmChapter {
    index: u32,
    #[serde(default)]
    summary: String,
}

/// Build a catalog with LLM assistance, falling back to the heuristic
/// outline on any LLM failure (unreachable endpoint, bad JSON, missing
/// fields). The fallback is total: the heuristic catalog is complete
/// and correct on its own; the LLM only enriches title/description/
/// summaries.
pub async fn build_catalog(
    client: Option<&LlmClient>,
    slug: &str,
    title_hint: &str,
    outline: &BookOutline,
) -> BookCatalog {
    let mut catalog = heuristic_catalog(slug, title_hint, outline);
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
            // Merge summaries by chapter index (1-based); ignore
            // out-of-range entries.
            for ch in llm.chapters {
                if ch.index >= 1
                    && !ch.summary.trim().is_empty()
                    && let Some(slot) = catalog.chapter_summaries.get_mut(ch.index as usize - 1)
                {
                    *slot = ch.summary.trim().to_string();
                }
            }
        }
        Err(e) => {
            tracing::warn!(slug, error = %e, "LLM cataloging failed — using heuristic catalog");
        }
    }
    catalog
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
         - \"description\": 2-3 sentences describing the book\n\
         - \"chapters\": an array of objects {{\"index\": <1-based chapter number>, \"title\": \"<chapter title>\", \"summary\": \"<2-3 sentence summary>\"}}\n\n\
         Respond with ONLY the JSON object, no markdown fences, no commentary.\n\
         All links in any output must be absolute leading-slash paths like /{slug}/book.md — never relative ./ paths."
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
        assert_eq!(outline.chapters[0].sections[0], (1, "Section 1.1".into()));
        assert_eq!(outline.chapters[1].sections[0], (1, "Section 2.1".into()));
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
        let catalog = build_catalog(Some(&client), "my-book", "My Book", &outline).await;
        assert_eq!(catalog.title, "My Book");
        assert!(catalog.chapter_summaries.iter().all(|s| s.is_empty()));
    }
}
