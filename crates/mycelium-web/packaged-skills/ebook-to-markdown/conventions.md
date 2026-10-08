---
type: Skill
title: ebook-to-markdown conventions
description: 'The output spec for books converted by the ebook-to-markdown skill: YAML frontmatter (title/author/language/toc), stable heading IDs ({#ch-N-slug} chapters and {#sec-N-M-slug} sections), a machine-readable table of contents, no page citations (a stated absence) and the figure manifest.'
tags:
- ebook-to-markdown
- calibre
- markdown
- conventions
- books
timestamp: 2026-10-02T00:00:00Z
---

# ebook-to-markdown conventions

The spec for the **enhanced readable book** (`book.md`) produced by the [ebook-to-markdown](/ebook-to-markdown/skill.md) skill. It is the [pdf-to-markdown conventions](/pdf-to-markdown/conventions.md) applied to ebooks — identical except where ebooks differ from PDFs. Every difference is stated below — including three deliberate absences, each stated rather than silently dropped: no page-range citations (§4), no `page_count` field (§1), and no size-based chunk splitting (its own section below). The goal is unchanged: a markdown file that is both pleasant for a human to read and easy for an LLM to index, navigate, cite, and chunk.

## What postprocess.py applies

`postprocess.py` takes the calibre-normalized unpack (`raw/`: spine-joined `index.html`, `spine.json`, `toc.json`, `conversion.json`) and adds:

1. **YAML frontmatter**
2. **A machine-readable table of contents**
3. **Stable heading IDs** (`{#ch-n-slug}`, `{#sec-n-m-slug}`) — identical dialect to the PDF skill
4. **No page-range citations** — a documented absence (see §4)
5. **A figure manifest** (`figures/manifest.json`)

The body itself (tables, code blocks, lists, image links, footnotes) is preserved from the calibre-converted spine — only chapter and section headings are rewritten to carry stable IDs. In chapters, headings deeper than `##` pass through untouched; in front matter, headings of any level are rewritten as `##` sec-0 sections.

## 1. Frontmatter

```yaml
---
title: The Art of X
author: Jane Doe
language: en
source_ebook: /abs/path/to/the-art-of-x.epub
converted_date: '2026-10-02'
converter: calibre 9.15.0
mode: ebook
chapter_count: 12
toc:
- id: ch-1-introduction
  title: Introduction
  level: 1
- id: sec-1-1-scope
  title: Scope
  level: 2
---
```

- `title`, `author`, `language` (`author` is `null` when unknown — see below), `source_ebook` (absolute input path), `converted_date` (ISO 8601 date), `converter` (`calibre <version>`, parsed from the normalized epub's OPF), `mode: ebook`, `chapter_count`, and a `toc` array listing each chapter's and section's stable `id`, `title`, and heading `level`.
- `toc[]` carries chapters at `level: 1` and their sections at `level: 2`, so `chapter_count` equals the number of `level: 1` entries. Front-matter sections (`sec-0-*`) are body-only — they never appear in `toc[]`.
- **Missing metadata.** calibre writes the literal "Unknown" for an absent `dc:creator`; postprocess treats that sentinel as absent, so `author: null` is normal output (4 of 231 corpus books). An absent `dc:language` becomes calibre's `und` code and passes through as `language: und` — not `null`. `title` never goes null: metadata title → source-filename stem → slug directory name → "Untitled".
- **No `page_count`** — a deliberate absence: ebooks have no fixed pagination, so the PDF conventions' `page_count` field has no ebook counterpart here; `chapter_count` is the structural count.
- **YAML-safe by construction**: frontmatter is emitted by PyYAML `safe_dump` (Unicode allowed, key order fixed), so titles containing `:` or other YAML-significant characters are quoted automatically — an exotic title can never break the frontmatter block.
- An LLM can index the whole book by reading only the frontmatter.

## 2. Machine-readable table of contents

Immediately after frontmatter:

```markdown
## Table of Contents

- [Introduction](#ch-1-introduction)
  - [Scope](#sec-1-1-scope)
```

Every `toc[]` entry as a link to its anchor — chapters top-level, sections indented two spaces; link text escapes `[` and `]`. Navigation without scanning the whole file.

## 3. Stable heading IDs

Every chapter and section heading gets a GFM explicit ID appended — **the same anchor dialect as the PDF skill**:

```markdown
# The Mechanism {#ch-3-the-mechanism}
## How it works {#sec-3-1-how-it-works}
## Front Matter {#sec-0-1-front-matter}
```

Rules:

- **Chapter headings**: `ch-{n}-{slug}`. `n` is the 1-based ordinal of the level-0 container-TOC entry (nav/NCX; mobi filepos TOCs are rebuilt as NCX by calibre's normalization). Dropped TOC entries leave gaps in `n` and are **never renumbered** — a corpus book whose first two TOC entries failed to align starts at `ch-3`, and that is correct output: anchors must survive re-conversion, so renumbering is forbidden.
- **Section headings within a chapter**: `sec-{n}-{m}-{slug}`. Sections are body-derived: every `##` heading inside a chapter becomes a section, `m` 1-based in body order. Nested TOC entries are *validated* against these sections (unmatched ones warn) but never create or renumber them. The `sec-` prefix keeps sections distinct from chapters whose title starts with a digit.
- **Headings before the first chapter** (cover, title page, copyright, praise pages): `sec-0-{n}-{slug}` sections. Each front-matter unit yields one section per heading it contains (title = heading text), plus a leading "Front Matter" section when content precedes its first heading; a heading-less unit yields a single "Front Matter" section. Every book carries cover/title-page front matter — calibre ensures a cover, generating a titlepage when the source has none, and keeps the source's own cover/title file otherwise — so every book has at least one such section (corpus: min 1, median 5, max 228).
- **IDs derive from chapter index + title slug**, never from converter-internal IDs — so they survive regeneration. Collisions dedupe with `-2`, `-3`, … suffixes. The slug rule is identical to the PDF skill's (lowercase, non-word → `-`, max 64 chars, `section` when empty).

This is the load-bearing enhancement: it makes any chapter/section addressable by anchor, so memory systems and MCP callers can cite back with `book://<slug>#ch-3-the-mechanism` style resources. **Reader semantics** (chapter-level granularity, as in the PDF skill): an anchor `ch-N-…` resolves to the whole N-th chapter regardless of the rest of the ID; section granularity needs the `sec-N-M` form.

## 4. No page-range citations — a documented absence

The PDF conventions emit `<!-- src: pp. X-Y -->` after each chapter heading. **This skill emits nothing of the kind, deliberately.** Ebooks paginate dynamically — the same book shows different "page" numbers on every device, font size, and reader — so a page number is not a stable locator, and emitting one would fabricate precision the source does not contain. Cite by anchor instead: `book://<slug>#ch-3-the-mechanism` (chapter) or `#sec-3-1-how-it-works` (section) is the verifiable citation. If you need page-stable citations, you need the book's PDF ([pdf-to-markdown](/pdf-to-markdown/skill.md)), not its ebook.

## 5. Figure manifest

`figures/manifest.json`:

```json
{
  "book": "The Art of X",
  "source_ebook": "/abs/path/to/the-art-of-x.epub",
  "figure_count": 14,
  "figures": [
    {"alt": "Image", "src": "images/cover.png", "page": null}
  ]
}
```

`postprocess.py` scans the emitted image links (`![Image](images/...)`, outside code fences) into the manifest — the same shape as the PDF skill's manifest. `page` is **always null**: ebooks have no fixed pagination, so there is nothing to attribute (in the PDF skill this field carries best-effort page attribution). `alt` mirrors the emitted link text ("Image" — converted XHTML images carry no alt text into the pipeline); `src` is the relative path under the book's directory. The readable book keeps the image links as emitted; the manifest is the authoritative figure index.

## Preserved as-is (from the converter)

- **Tables** — GFM tables: header from `th` cells (else the first row, bolded), ragged rows padded, pipes escaped.
- **Code blocks** — fenced verbatim from `pre`; the fence escalates past any fence-shaped run inside the content.
- **Lists and blockquotes** — nested lists, ordered markers, `>` quote lines.
- **Emphasis / inline code** — `*…*`, `**…**`, `` `…` ``.
- **Image links** — `![Image](images/...)`, relative paths.
- **Footnotes** — whatever the source carries (epub notes pages) comes through as ordinary body text; no footnote renumbering.
- Cleanup only removes noise: soft hyphens rejoined, zero-width characters stripped, NBSP runs collapsed to one space, blank-paragraph runs collapsed.

## Chunk-friendly sections — a stated absence

The PDF conventions keep chapters and sections under ~4000 tokens where possible, splitting long sections on `###`. This skill does no size-based splitting: sections are exactly the book's own `##` headings, however long they come out. Retrieval boundaries are the book's own structure; token-budget chunking belongs to the consumer.

## Enhancement checklist (and why each matters)

| enhancement | why |
|---|---|
| frontmatter | one-read indexing (title/author/language/TOC) |
| machine-readable TOC | navigation without scanning the whole file |
| stable heading IDs | durable cross-references that survive re-conversion |
| no page citations (documented absence) | anchors — not fabricated page numbers — are the locators |
| no `page_count`, no size-based chunk splitting (documented absences) | ebooks have no pages; section boundaries are the book's own headings |
| figure manifest | reliable figure lookup by src |
| preserved tables/code/images | no fidelity loss |