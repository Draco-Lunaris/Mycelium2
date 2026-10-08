---
type: Skill
title: ebook-to-markdown
description: 'Convert an ebook (epub, mobi, azw3 or fb2) into an enhanced, human- and LLM-readable markdown book: calibre raw conversion plus deterministic post-processing that adds stable heading IDs, a machine-readable table of contents and a figure manifest, with DRM-protected books detected and refused.'
tags:
- ebook-to-markdown
- calibre
- ebooks
- books
- conversion
skill:
  version: 1
  files:
    - {path: conventions.md, role: reference}
    - {path: license.md, role: reference}
    - {path: references/ebook-options.md, role: reference}
    - {path: scripts/inspect_ebook.py, role: script, md5: d88d659106ebdeee38e512a254b37b9a}
    - {path: scripts/convert.py, role: script, md5: 87ff45ad5c677a73c42a8ecd4bd4c1d9}
    - {path: scripts/ebook_html.py, role: script, md5: 651ec736f5720d7c198a2c9d92f127e4}
    - {path: scripts/postprocess.py, role: script, md5: db7cf49b8d5c25ad8b1499541f1ff435}
    - {path: scripts/setup_venv.sh, role: script, md5: c9db2d0847b03957c712e13c26d7a83c}
    - {path: scripts/requirements.txt, role: script, md5: a1a52f55056f5770d4119526403377d9}
timestamp: 2026-10-02T00:00:00Z
---

# ebook-to-markdown

Convert an **ebook** (epub, mobi, azw3 or fb2) into an enhanced, human-readable markdown book — calibre's raw conversion plus deterministic post-processing that adds stable heading IDs, a machine-readable table of contents, and a figure manifest, so both humans and LLMs can navigate and cite it. The output follows the [ebook-to-markdown conventions](/ebook-to-markdown/conventions.md) — the same anchor dialect as [pdf-to-markdown](/pdf-to-markdown/skill.md) books, so one ingest path serves both skills.

The heavy lifting (text extraction, spine order, metadata, container TOC) is done by **calibre** (`ebook-convert`, driven as an external binary — the same relationship the PDF skill has with Docling). This skill does NOT parse ebook internals: the probe reads only container/header metadata, calibre normalizes every input to an epub, and deterministic Python scripts post-process that into the readable book. DRM-protected books are detected and refused before any conversion — never circumvented. Its whole job is ebook → markdown; it produces nothing else.

Corpus-validated before packaging: 231 real books (168 epub + 63 mobi), 100% conversion success, median 2.6 s per book.

## When to use

The user wants an *ebook* — a book or long-form structured document in epub, mobi, azw3 or fb2 — turned into markdown, especially LLM/RAG-friendly output with navigable chapter structure, matching what [pdf-to-markdown](/pdf-to-markdown/skill.md) produces for PDFs.

## When NOT to use (use `pdf-to-markdown` for PDFs)

- A PDF input → [pdf-to-markdown](/pdf-to-markdown/skill.md).
- A DRM-protected book → this skill refuses it, by design; there is no override. Tell the user plainly.
- Converting between ebook formats for reading (epub → mobi for an e-reader), editing ebooks, or library management — calibre's own GUI does that; this skill only produces markdown.
- Short documents that are not books (a single article, a man page, a README).

## Prerequisites (one-time setup)

The scripts run in a dedicated venv at `~/.venvs/ebook-to-markdown/`, and calibre lives in an isolated no-root install at `~/calibre-bin/`. Set both up from the skill's scripts directory:

```bash
bash <skill-dir>/scripts/setup_venv.sh
```

Installs `uv` if missing, provisions a managed Python 3.12 venv (`lxml` + `PyYAML` only — the venv does NOT contain calibre), and installs calibre (~195 MB download, ~640 MB installed, one-time) via its official isolated installer. Verify with `~/calibre-bin/ebook-convert --version`; `convert.py` finds the binary at `~/calibre-bin/ebook-convert` (override with `EBOOK2MD_CALIBRE`). No model downloads (unlike pdf-to-markdown's Docling weights).

On headless Linux hosts, calibre will not start without the GL libraries `libegl1` and `libopengl0` (its documented OpenGL requirement): `sudo apt install -y libegl1 libopengl0`.

The skill's scripts (`inspect_ebook.py`, `convert.py`, `ebook_html.py`, `postprocess.py`, `setup_venv.sh`) live in the skill directory on the conversion host.

## Workflow

All scripts print exactly one JSON line on stdout (`{"ok": true, ...}` / `{"ok": false, "error": "..."}`) and progress/warnings to stderr; run them via `~/.venvs/ebook-to-markdown/bin/python`.

1. **Inspect.** `inspect_ebook.py --input <book>` — parse the JSON: format (epub/mobi/azw3/fb2), title, author, language, `toc_kind` (nav/ncx/filepos/none), spine size, image count. Report to the user. A DRM'd input never profiles — it fails with an error naming DRM; stop there. For mobi/azw3, `spine_count` is the record-0 text-record count and `image_count` is 0 — the probe never reads beyond record 0; the normalized epub carries the real structure.

2. **Convert** (the slow step — monitor stderr):

   ```bash
   ~/.venvs/ebook-to-markdown/bin/python <skill-dir>/scripts/convert.py \
     --input <book> --output-dir <out> [--book-slug <slug>]
   ```

   calibre normalizes the input to an epub (600 s timeout → clean refusal), which convert.py unpacks into `<out>/<slug>/raw/` (`book.epub`, spine-joined `index.html`, `spine.json`, `toc.json`, `conversion.json`, `images/`). Parse stdout JSON for `raw_dir`, `spine_count`, `toc_entries`.

3. **Post-process** (fast):

   ```bash
   ~/.venvs/ebook-to-markdown/bin/python <skill-dir>/scripts/postprocess.py \
     --raw <raw-dir> --output <book>.md [--toc-source auto|nav|headings|spine]
   ```

   Parse JSON for `readable_book`, `figure_manifest`, `chapter_count`, `front_matter_sections`, `warnings`. Chapter structure comes from the book's own container TOC (nav/NCX), aligned against body headings; `warnings` lists TOC entries dropped by that alignment — check its size before reporting success.

4. **Report.** Output paths (the readable book, the figure manifest, the provenance `raw/conversion.json`) plus a summary: title, author, format, chapters found, structure tier used, dropped-entry count.

## Output structure

A conversion run produces, under `<output-dir>/<book-slug>/`:

```
<book-slug>/
├── raw/                   # the calibre-normalized epub, unpacked
│   ├── book.epub         # normalized epub (kept as provenance)
│   ├── index.html        # spine-ordered XHTML, "<!-- spine: i href -->" markers
│   ├── spine.json        # readable spine files: [{href, title}]
│   ├── toc.json          # container TOC: [{nav_label, content_href, spine_index, level}]
│   ├── conversion.json  # provenance (source, sha256, calibre version, metadata, warnings)
│   └── images/           # manifest image members, flattened basenames
├── book.md               # the enhanced readable book
├── images/               # copied next to book.md
└── figures/
    └── manifest.json     # figure src/alt index (page: null always)
```

The readable book follows the [ebook-to-markdown conventions](/ebook-to-markdown/conventions.md).

## Decisions you make

- **Is this an ebook at all?** A PDF → [pdf-to-markdown](/pdf-to-markdown/skill.md); a few pages or clearly not a book → say so.
- **DRM:** no decision to make — inspect refuses DRM inputs before any conversion, and no override exists.
- **Structure tier:** `auto` by default (container TOC → heading scan → one chapter per spine file). `--toc-source nav|headings|spine` forces the starting tier; a tier that yields no chapters falls through to the next.
- **Slug:** from the sanitized filename stem, never book metadata (ebook metadata is often garbage). `--book-slug` overrides (non-empty, no path separators).
- **Whole book only:** no page- or chapter-range flag — ebooks have no pages; convert the whole book.

## Gotchas (each cost real time once)

- **TOC alignment is the dominant quality observation.** Container TOC labels (nav/NCX) and body headings disagree constantly. In the 231-book corpus validation, *every* book dropped at least one TOC entry — 66,333 dropped-entry warnings corpus-wide (min 6, median 259, max 1,691 per book). A dropped entry leaves **no chapter anchor**, but its content still flows into the book as `sec-N-M` sections — never lost, never renumbered. Don't chase zero warnings; check `chapter_count` and skim the machine TOC instead. Corpus-wide chapter counts run min 1 / median 6 / max 155; 28/231 books end with `chapter_count ≤ 2` — that is TOC noise, not data loss.
- **Split stage output on "\n" only, never `str.splitlines()`**: real metadata carries Unicode line/paragraph separators (U+2028/U+2029 — the corpus had one inside a Rust book's *title*), and `json.dumps(ensure_ascii=False)` emits them raw inside JSON strings; `splitlines()` over-splits the one-JSON-line contract into phantom lines and misparses. Real newlines are always escaped by `json.dumps`, so plain "\n" splitting is safe.
- **`author: null` means missing metadata, not failure**: calibre writes the literal "Unknown" for absent `dc:creator`; postprocess treats that sentinel as absent → `author: null` (4 of 231 corpus books). `title` never goes null — it falls back metadata → source filename stem → slug directory → "Untitled".
- **Headless GL**: if `ebook-convert` fails at startup with a GL/EGL error, the host is missing `libegl1`/`libopengl0` — `sudo apt install -y libegl1 libopengl0` (calibre's documented OpenGL requirement), then re-run.
- **Images** are written to `images/` next to `book.md` with relative links (`![Image](images/...)`) — portable if the output dir moves. The figure manifest is the authoritative index; `page` is always `null` (ebooks have no fixed pagination — cite by anchor instead). 45,854 links across the corpus, zero dangling.
- **Tier falls are designed behavior**: 5 falls in 4 of 231 books (`nav` → `headings`, one book further to `spine`); a spine-order book is valid output. A `chapter_count: 1` book with over a hundred `sec-1-M` sections just means only one TOC entry aligned.
- **Dedup at ingest**: the same title often arrives in two formats (the corpus holds epub *and* mobi copies of the same books). Search-based dedup is unreliable — dedup against the target shelf's book catalog, not a search endpoint.
- **DRM is a hard limit**: inspect flags it, convert refuses before calibre runs, and no circumvention exists anywhere in this skill. Zero DRM books in the corpus, but the gate is load-bearing for public users.

## Packaged with this skill (all in this shelf)

The skill is **self-contained in Mycelium2's global skills shelf** — this hub, its companion concepts and the raw script payloads live under `/ebook-to-markdown/`:

- [ebook-to-markdown conventions](/ebook-to-markdown/conventions.md) — the full output spec (frontmatter, heading IDs, the no-page-citations absence, figure manifest).
- [ebook-to-markdown license](/ebook-to-markdown/license.md) — the provenance note (calibre GPL-3.0 external binary; the skill itself MIT OR Apache-2.0).
- [ebook-to-markdown ebook options](/ebook-to-markdown/references/ebook-options.md) — the `references/ebook-options.md` reference (the calibre invocation, normalization rationale, DRM gate, isolated install, corpus facts).
- Scripts and their md5s live in the bundle; download via the skills page.