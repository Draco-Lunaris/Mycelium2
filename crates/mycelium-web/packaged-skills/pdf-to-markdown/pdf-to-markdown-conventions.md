---
type: Skill
title: pdf-to-markdown conventions
description: 'The output spec for books converted by the pdf-to-markdown skill: YAML frontmatter (title/author/source/toc), stable heading IDs ({#ch-N-slug} chapters, {#sec-N-M-slug} sections), source page-range citations, and the figure manifest.'
tags:
- pdf-to-markdown
- docling
- markdown
- conventions
- books
timestamp: 2026-09-20T00:00:00Z
---

# pdf-to-markdown conventions

The spec for the **enhanced readable book** (`<book>.md`) produced by the [pdf-to-markdown](/pdf-to-markdown.md) skill. The goal is a markdown file that is both pleasant for a human to read and easy for an LLM to index, navigate, cite, and chunk.

## What postprocess.py applies

`postprocess.py` takes the converter's raw markdown (with `<span id="page-N-M">` page spans) + `meta.json` + `conversion.json` and adds:

1. **YAML frontmatter**
2. **A machine-readable table of contents**
3. **Stable heading IDs** (`{#ch-n-slug}`, `{#sec-n-m-slug}`)
4. **Source page-range citations** (`<!-- src: pp. X-Y -->`)
5. **A figure manifest** (`figures/manifest.json`)

The body itself (tables, `$$` LaTeX, code blocks, image links, footnotes) is preserved verbatim — only headings are rewritten to carry stable IDs.

## 1. Frontmatter

```yaml
---
title: "The Art of X"
author: "Jane Doe"
source_pdf: "/abs/path/to/the-art-of-x.pdf"
converted_date: '2026-08-07'
converter: "docling 2.118.1"
mode: docling
page_count: 412
toc:
  - id: ch-1-introduction
    title: "Introduction"
    level: 2
  - id: ch-2-prior-work
    title: "Prior Work"
    level: 2
---
```

- `title`, `author` (if known), `source_pdf`, `converted_date` (ISO 8601), `converter` (docling + version; a `marker_version` fallback keeps old conversions readable), `mode`, `page_count`, and a `toc` array listing each chapter's stable `id`, `title`, and heading `level`.
- An LLM can index the whole book by reading only the frontmatter.

## 2. Machine-readable table of contents

Immediately after frontmatter:

```markdown
## Table of Contents

- [Introduction](#ch-1-introduction)
- [Prior Work](#ch-2-prior-work)
```

A nested list linking to the chapter anchor IDs — navigation without scanning the whole file.

## 3. Stable heading IDs

Every heading gets a GFM explicit ID appended:

```markdown
# The Mechanism {#ch-3-the-mechanism}
## How it works {#sec-3-1-how-it-works}
```

Rules:

- **Chapter headings**: `ch-{chapter_index}-{slug}` (e.g. `ch-3-the-mechanism`). Chapter structure comes from the PDF's own embedded outline; index is the outline position (1-based).
- **Section headings within a chapter**: `sec-{n}-{section_counter}-{slug}` (e.g. `sec-3-1-how-it-works`). The `sec-` prefix keeps sections distinct from chapters whose title starts with a digit (a chapter "1. Python Basics" → `ch-1-1-python-basics` is still a chapter).
- **Headings before the first chapter** (front matter, preface): `sec-0-{counter}-{slug}`.
- IDs are derived from **chapter index + title slug**, never from converter-internal block IDs — so they survive regeneration.

This is the load-bearing enhancement: it makes any chapter/section addressable by anchor, so memory systems and MCP callers can cite back with `book://<slug>#ch-3-the-mechanism` style resources.

**Reader semantics for these anchors** (chapter-level granularity): an anchor `ch-N-…` resolves to the whole N-th chapter regardless of the rest of the ID (section granularity needs the `sec-N-M` form). A book whose sections carry only `ch-N-M` IDs is therefore chapter-addressable, not section-addressable — re-convert with the current postprocess for `sec-` precision.

## 4. Source page-range citations

After each chapter heading, an HTML comment records the original PDF page span:

```markdown
# The Mechanism {#ch-3-the-mechanism}
<!-- src: pp. 45-78 -->
```

Page numbers are 1-indexed (printed page). The span runs from the chapter's first page to the page before the next chapter starts — computed from the PDF's own outline (page indices) aligned to the `<span id="page-N-M">` page-span offsets in the raw markdown. Lets an LLM cite the original page when answering from the KB — verifiable answers.

## 5. Figure manifest

`figures/manifest.json`:

```json
{
  "book": "The Art of X",
  "source_pdf": "/abs/path/to/the-art-of-x.pdf",
  "figure_count": 14,
  "figures": [
    {"alt": "Schematic of the mechanism", "src": "images/fig_3_1.png", "page": null}
  ]
}
```

`postprocess.py` scans the converter's image links (`![Image](images/...)`) into the manifest. Page attribution is best-effort. The readable book keeps the image links as emitted; the manifest is the authoritative figure index.

## Preserved as-is (from the converter)

- **Tables** — markdown tables, no transformation.
- **Equations** — block `$$..$$` and inline `$..$` LaTeX (Docling's base layout model emits LaTeX without enrichment).
- **Code blocks** — fenced code with language hints where detected.
- **Image links** — `![Image](images/...)`, relative paths.
- **Footnotes** — `[^id]` output preserved verbatim to avoid breaking references.

## Chunk-friendly sections

Chapters and sections are kept coherent and, where possible, under ~4000 tokens; long sections split on `###`. This makes per-chapter retrieval natural, and each chapter maps cleanly to a Chapter concept in memory systems.

## Enhancement checklist (and why each matters)

| enhancement | why |
|---|---|
| frontmatter | one-read indexing (title/author/pages/TOC) |
| machine-readable TOC | navigation without scanning the whole file |
| stable heading IDs | durable cross-references that survive re-conversion |
| source page citations | verifiable answers (cite the original PDF page) |
| figure manifest | reliable figure lookup by id |
| preserved tables/equations/code | no fidelity loss |
| chunk-friendly sections | natural retrieval boundaries |