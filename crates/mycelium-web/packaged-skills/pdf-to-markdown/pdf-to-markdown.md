---
type: Skill
title: pdf-to-markdown
description: 'Convert PDF books, textbooks, manuals and monographs into enhanced human- and LLM-readable markdown books: Docling raw conversion plus deterministic post-processing that adds stable heading IDs (chapters and sections), a machine-readable TOC, source page-range citations, and a figure manifest.'
tags:
- pdf-to-markdown
- docling
- pdf
- books
- conversion
timestamp: 2026-09-20T00:00:00Z
---

# pdf-to-markdown

Convert a PDF **book** into an enhanced, human-readable markdown book — a dedicated PDF→markdown converter's (Docling) raw markdown, post-processed with stable heading IDs, a machine-readable table of contents, source page-range citations, and a figure manifest, so both humans and LLMs can navigate and cite it.

The heavy lifting (text, tables, equations, reading order, figures) is done by **Docling** (`StandardPdfPipeline`, single engine for every book). This skill does NOT parse PDFs by hand — it drives Docling, then post-processes with deterministic Python scripts. Its whole job is PDF → markdown; it produces nothing else. (Replaces the older marker/surya stack, retired 2026-08-13; Docling runs its models inline — clean exit, no persistent GPU servers, no vLLM, no Docker.)

## When to use

The user wants a *book* (or long-form structured document — textbook, manual, monograph, report) turned into markdown, especially LLM/RAG-friendly output with navigable chapter structure.

## When NOT to use (use the `pdf` skill instead)

- Simple text/table extraction from a short PDF.
- Merging, splitting, rotating, encrypting, or watermarking PDFs.
- Filling PDF forms, creating new PDFs, or extracting a single image.
- Invoices, receipts, or single articles (2–5 pages).

## Prerequisites (one-time setup)

The scripts run in a dedicated venv at `~/.venvs/pdf-to-markdown/`. Set it up from the skill's scripts directory:

```bash
bash <skill-dir>/scripts/setup_venv.sh
```

Installs `uv` if missing, provisions a managed Python 3.12, and installs the requirements (`docling` + `onnxruntime` + `pypdfium2` + `pdfplumber` + `PyYAML`). Verify with `uv pip check --python ~/.venvs/pdf-to-markdown`.

The first Docling run downloads model weights (~2–3 GB: layout, table, OCR) to the docling and HuggingFace caches — one-time. On a GPU host Docling runs its models inline and auto-sets `TORCH_DEVICE=cuda`; OCR for scanned PDFs uses RapidOCR (onnxruntime, CPU-bound). Optional figure description via an Ollama cloud-vision model (`--use-ollama-vlm`, off by default).

The skill's scripts (`convert.py`, `postprocess.py`, `docling_page_span.py`, `inspect_pdf.py`, `html_cleanup.py`, `setup_venv.sh`) live in the skill directory on the conversion host.

## Workflow

All scripts print exactly one JSON line on stdout (`{"ok": true, ...}` / `{"ok": false, "error": "..."}`) and progress to stderr; run them via `~/.venvs/pdf-to-markdown/bin/python`.

1. **Inspect.** `inspect_pdf.py --input <pdf>` — parse the JSON: page count, has text layer, has PDF outline, likely scanned, image count. Report to the user.

2. **OCR decision** (auto in `convert.py` from the inspect profile): text layer & not scanned → OCR off (Docling extracts the text layer directly — fast); scanned / no text layer → OCR on (RapidOCR). `--force-ocr` overrides on; `--mode disable_ocr` overrides off (compat flag with the old marker CLI).

3. **Convert** (the slow step — monitor stderr):

   ```bash
   ~/.venvs/pdf-to-markdown/bin/python <skill-dir>/scripts/convert.py \
     --input <pdf> --output-dir <out> [--use-ollama-vlm] [--force-ocr] [--disable-image-extraction]
   ```

   Parse stdout JSON for `raw_md`, `meta_json`, `images_dir`, `conversion_json`. On OOM: `--disable-image-extraction`.

4. **Post-process** (fast):

   ```bash
   ~/.venvs/pdf-to-markdown/bin/python <skill-dir>/scripts/postprocess.py \
     --raw-md <…> --meta <…> --conversion <…> --output <book>.md
   ```

   Parse JSON for `readable_book` and `figure_manifest`. Chapter structure comes from the PDF's own embedded outline (`pypdfium2 get_toc`), aligned to page spans in the raw markdown.

5. **Report.** Output paths (the readable book, the figure manifest, the provenance `meta.json`) plus a summary: title, pages, chapters found, mode used.

## Output structure

A conversion run produces, under `<output-dir>/<book-slug>/`:

```
<book-slug>/
├── raw/                       # Docling's raw output
│   ├── <book>.raw.md
│   ├── <book>.meta.json       # metadata (page_stats, author, title)
│   ├── images/
│   └── conversion.json        # provenance (source, mode, flags, versions)
├── <book>.md                  # the enhanced readable book
└── figures/
    └── manifest.json          # figure id -> page, caption, alt
```

The readable book follows the [pdf-to-markdown conventions](/pdf-to-markdown-conventions.md).

## Decisions you make

- **Is this a book?** A few pages or clearly not a book → steer to the `pdf` skill.
- **OCR:** auto — off for born-digital, on (RapidOCR) for scanned; `--force-ocr` overrides.
- **Ollama VLM:** off by default; enable with `--use-ollama-vlm` for figure-heavy books (env `OLLAMA_VLM_URL`, `OLLAMA_VLM_MODEL`, `OLLAMA_API_KEY`, `OLLAMA_VLM_TIMEOUT`, `OLLAMA_VLM_CONCURRENCY`).
- **Page range:** full book by default. `--page-range` is accepted but NOT yet supported by the Docling driver (warns + converts the full document).
- **Split:** one readable file by default; `postprocess.py --split-by-chapter` writes one file per chapter for very large books.

## Gotchas (each cost real time once)

- **Page spans are load-bearing**: `docling_page_span.py` replaces Docling's page-break tokens with `<span id="page-N-M">` spans (Docling page numbers are 1-based, the pypdfium2 outline is 0-based → subtract 1; a `page-0-0` span is prepended). `postprocess.py` aligns the PDF outline's chapters to these spans. Spans are always emitted — no flag needed.
- **Dedup at ingest**: search-based dedup is unreliable (it matches content not slug, and result lists are capped) — dedup against the target shelf's book catalog, not a search endpoint.
- **Slug from the filename stem**, never PDF Title metadata (it is often garbage, e.g. `'ToolBox_cover_13'`). The displayed title comes from postprocess (outline/metadata).
- **Anchor dialect**: current postprocess emits `{#ch-N-slug}` chapters and `{#sec-N-M-slug}` sections. Books converted before 2026-08-13 may carry `{#ch-N-M-slug}` section IDs — readers treat `ch-N-M` as a chapter anchor (whole-chapter text), so re-convert for section-level precision.
- **Images** are written to `raw/images/` with relative links (`![Image](images/...)`) — portable if the output dir moves. The figure manifest is the authoritative index.
- **RapidOCR needs onnxruntime** (it is in requirements; an `ImportError: onnxruntime` means a broken venv — install into it).

## Packaged with this skill (all in this shelf)

The skill is **self-contained in Mycelium2's global skills shelf** — the executable scripts, their manifest, the reference doc and the license are sibling concepts:

- [pdf-to-markdown scripts](/pdf-to-markdown-scripts.md) — the file manifest (bytes + md5 per file) and the extraction instructions. Start here when materializing the skill on a host.
- [pdf-to-markdown script — convert.py](/pdf-to-markdown-script-convert.md), [postprocess.py](/pdf-to-markdown-script-postprocess.md), [docling_page_span.py](/pdf-to-markdown-script-docling-page-span.md), [html_cleanup.py](/pdf-to-markdown-script-html-cleanup.md), [inspect_pdf.py](/pdf-to-markdown-script-inspect-pdf.md), [setup_venv.sh](/pdf-to-markdown-script-setup-venv.md), [requirements.txt](/pdf-to-markdown-script-requirements.md) — byte-exact fenced copies; extract, md5-verify, run.
- [pdf-to-markdown docling options](/pdf-to-markdown-docling-options.md) — the `references/docling-options.md` reference (OCR, Ollama VLM, formulas, images, page spans, GPU).
- [pdf-to-markdown license](/pdf-to-markdown-license.md) — the proprietary LICENSE.txt.
- [pdf-to-markdown conventions](/pdf-to-markdown-conventions.md) — the full output spec (frontmatter, heading IDs, citations, manifest).