//! Packaged skills: skills shipped with the system and seeded into the
//! global skills shelf on boot. Mirrors the assets scaffold contract
//! (`assets::scaffold_defaults`): first boot writes everything; the
//! version marker decides refresh; admin edits survive between bumps.

use std::path::Path;

use mycelium_core::concept::Concept;
use mycelium_crypto::keys::ServiceKey;
use mycelium_store::{ConceptStore, Store};

/// The packaged skills' content version. Bumped when the packaged skill
/// content changes; a stale (or missing) marker triggers a full refresh
/// of the packaged paths — extra admin-created skills are never touched.
pub const SKILLS_SEED_VERSION: &str = "1";

/// The packaged skill concepts (seed v1, flat layout): (shelf
/// filename, full OKF file contents). A frozen byte-exact inline copy
/// of the former `packaged-skills/pdf-to-markdown/*.md` flat files —
/// those repo assets were repackaged into the nested layout consumed
/// by [`PACKAGED_CONCEPTS`] below, so the old `include_str!` targets no
/// longer exist. `seed_packaged_skills` keeps seeding this v1 table
/// (behavior unchanged) until seed v2 replaces it; the fence + md5
/// pins in `tests/packaged_skills_integration.rs` guard these bytes.
pub const PACKAGED_SKILLS: &[(&str, &str)] = &[
    (
        "pdf-to-markdown.md",
        r#"---
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
- [pdf-to-markdown conventions](/pdf-to-markdown-conventions.md) — the full output spec (frontmatter, heading IDs, citations, manifest)."#,
    ),
    (
        "pdf-to-markdown-conventions.md",
        r#"---
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
| chunk-friendly sections | natural retrieval boundaries |"#,
    ),
    (
        "pdf-to-markdown-scripts.md",
        r#"---
type: Skill
title: pdf-to-markdown scripts
description: The executable scripts of the pdf-to-markdown skill (convert.py, postprocess.py, docling_page_span.py, html_cleanup.py, inspect_pdf.py, setup_venv.sh, requirements.txt) stored verbatim so the skill is self-contained. Extract each fenced block to a file of the same name and md5-verify.
tags:
- pdf-to-markdown
- docling
- scripts
- packaged
timestamp: 2026-09-21T00:00:00Z
---

# pdf-to-markdown scripts

The **executable half** of the [pdf-to-markdown](/pdf-to-markdown.md) skill, stored verbatim so the skill is self-contained and functional. Each script lives in its own sibling concept; extract each fenced block **exactly** (byte-exact copies) into files of the names below, in one directory, and the skill runs.

## File manifest (md5-verified)

| file | bytes | md5 | ends with newline |
|---|---|---|---|
| [`convert.py`](/pdf-to-markdown-script-convert.md) | 12601 | `80ddf66fe575e11e0e1c8c64b2bb3b6f` | no — strip the fence, do not add one |
| [`postprocess.py`](/pdf-to-markdown-script-postprocess.md) | 30079 | `ebce3858e7b192a14e376a70daf28284` | no — strip the fence, do not add one |
| [`docling_page_span.py`](/pdf-to-markdown-script-docling-page-span.md) | 4887 | `d9d548923cfed4b2dc9b0e0679f69bf0` | no — strip the fence, do not add one |
| [`html_cleanup.py`](/pdf-to-markdown-script-html-cleanup.md) | 7938 | `9fa07ded3cf2f877a12020f98f65995d` | no — strip the fence, do not add one |
| [`inspect_pdf.py`](/pdf-to-markdown-script-inspect-pdf.md) | 5890 | `75e95808510774038a9523fe5a3a7d87` | no — strip the fence, do not add one |
| [`setup_venv.sh`](/pdf-to-markdown-script-setup-venv.md) | 1604 | `9a881a2b5d93e09cbd7ec18f09bd8faf` | no — strip the fence, do not add one |
| [`requirements.txt`](/pdf-to-markdown-script-requirements.md) | 735 | `3daed4196aca9d9ecf6fae04840f7000` | no — strip the fence, do not add one |
| LICENSE.txt | 592 | `ac22751348351ef471066f455e1ba8d8` | no |

The reference doc (`docling-options.md`, the only file under `references/`) is in [pdf-to-markdown docling options](/pdf-to-markdown-docling-options.md). The conventions spec is a normal concept: [pdf-to-markdown conventions](/pdf-to-markdown-conventions.md).

## Layout after extraction

```
pdf-to-markdown/
├── LICENSE.txt         # from the license concept
├── references/
│   └── docling-options.md   # from the docling-options concept
└── scripts/
    ├── convert.py            # drives Docling (needs docling_page_span.py)
    ├── postprocess.py        # adds TOC/IDs/citations/manifest (needs html_cleanup.py)
    ├── docling_page_span.py  # the page-span serializer adapter (imported by convert.py)
    ├── html_cleanup.py       # HTML-to-markdown cleanup (imported by postprocess.py)
    ├── inspect_pdf.py        # PDF profile: pages/text/outline/scanned/images
    ├── setup_venv.sh         # one-time venv bootstrap (uv + Python 3.12 + requirements)
    └── requirements.txt      # docling[rapidocr,remote-serving] + onnxruntime + pypdfium2 + pdfplumber + PyYAML
```

## Extraction rules

1. Every fenced block in the script concepts is a **byte-exact copy**. Strip exactly the opening and closing fence lines and keep every byte between them. Where the manifest says the file ends **without** a newline, the fenced content's last line has no trailing newline — writing the file with the fence content verbatim (no added final newline) reproduces the md5.
2. Verify each extracted file: `md5sum <file>` against the manifest.
3. Then run `bash scripts/setup_venv.sh` and use `~/.venvs/pdf-to-markdown/bin/python` per the [main skill](/pdf-to-markdown.md) workflow.
4. Keep `LICENSE.txt` (license concept's fenced block) next to the skill — it is a proprietary Moon-Dragon license.
"#,
    ),
    (
        "pdf-to-markdown-script-convert.md",
        r#"---
type: Skill
title: pdf-to-markdown script — convert.py
description: Verbatim copy of the pdf-to-markdown skill's python script `convert.py`. Drives Docling to convert a PDF to raw markdown with page spans (imports docling_page_span). Extract the fenced block byte-exact; verify md5 `80ddf66fe575e11e0e1c8c64b2bb3b6f` against the [scripts manifest](/pdf-to-markdown-scripts.md).
tags:
- pdf-to-markdown
- docling
- script
- packaged
timestamp: 2026-09-21T00:00:00Z
---

# pdf-to-markdown script — convert.py

Byte-exact copy of `convert.py` from the [pdf-to-markdown](/pdf-to-markdown.md) skill (source tree: `scripts/convert.py`). **no trailing newline**

Extract the fenced block below **exactly**: strip the opening ` ```` ` line and the closing ` ```` ` line, keep every byte between them — including the final line's missing newline if the manifest says "no trailing newline" (an editor that silently appends one is harmless for these scripts, but the md5 will differ).

`12601 bytes, md5 80ddf66fe575e11e0e1c8c64b2bb3b6f`

````
#!/usr/bin/env python3
"""
pdf-to-markdown skill — Docling conversion driver.

Drives Docling's DocumentConverter once and writes the raw markdown (with
``<span id="page-N-M">`` page spans), metadata, extracted images, and a
provenance conversion.json. OCR is auto-decided from a fast PDF profile
(RapidOCR for scanned PDFs) unless overridden. An optional Ollama cloud-vision
VLM can describe figures. Prints exactly one JSON line on stdout; progress to
stderr.

Replaces the former marker/surya/vLLM driver. postprocess.py is unchanged: it
consumes the same three-file contract (raw markdown with page spans + meta.json
+ conversion.json with source_pdf).

Usage:
    python convert.py --input book.pdf --output-dir /out
        [--force-ocr] [--strip-existing-ocr]
        [--use-ollama-vlm] [--ollama-vlm-model gemma4:31b-cloud]
        [--disable-image-extraction] [--lang en] [--book-slug <slug>]
        [--page-range "0,5-10,20"]   # accepted but not yet supported (warns)
"""

import argparse
import hashlib
import json
import os
import re
import sys
import time
from pathlib import Path


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def slugify(text):
    text = (text or "").strip().lower()
    text = re.sub(r"[^\w\s-]", "", text)
    text = re.sub(r"[\s_-]+", "-", text).strip("-")
    return text or "book"


def sha256_file(path, chunk=1 << 20):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(chunk), b""):
            h.update(block)
    return h.hexdigest()


def pdf_metadata(pdf_path):
    """Return (page_count, title, author) via pypdfium2 (no torch)."""
    import pypdfium2 as pdfium
    d = pdfium.PdfDocument(str(pdf_path))
    page_count = len(d)
    md = d.get_metadata_dict()
    title = md.get("Title") or ""
    author = md.get("Author") or ""
    d.close()
    return page_count, title, author


def decide_ocr(args, input_path):
    """True if OCR should run.

    Resolution order: --force-ocr / --strip-existing-ocr override everything;
    else an explicit --mode (compat with the old marker CLI) decides; else the
    auto-inspect profile decides (RapidOCR for scanned PDFs, off for born-digital).
    """
    if args.force_ocr or args.strip_existing_ocr:
        return True
    mode = getattr(args, "mode", "auto")
    if mode == "disable_ocr":
        return False
    if mode in ("fast", "balanced"):
        return True  # compat: these old modes meant OCR-on
    # auto
    try:
        sys.path.insert(0, str(Path(__file__).resolve().parent))
        from inspect_pdf import profile_pdf
        profile = profile_pdf(input_path, sample_pages=8)
        scanned = bool(profile.get("likely_scanned"))
        log(f"inspect: {profile.get('page_count')} pages, text_layer="
            f"{profile.get('has_text_layer')}, scanned={scanned} -> do_ocr={scanned}")
        return scanned
    except Exception as e:
        log(f"WARN: auto-inspect failed ({e}); defaulting to do_ocr=False.")
        return False


def build_pipeline_options(args, do_ocr):
    from docling.datamodel.pipeline_options import (
        PdfPipelineOptions, RapidOcrOptions, PictureDescriptionApiOptions, OcrMode,
    )

    # OCR options
    if do_ocr:
        ocr_kwargs = {}
        if args.strip_existing_ocr:
            ocr_kwargs["mode"] = OcrMode.FULL_PAGE  # re-OCR every page, ignore embedded text
        if args.lang:
            ocr_kwargs["lang"] = [args.lang]
        try:
            ocr_options = RapidOcrOptions(**ocr_kwargs)
        except Exception as e:
            log(f"WARN: RapidOcrOptions({ocr_kwargs}) rejected ({e}); using default.")
            ocr_options = RapidOcrOptions()
    else:
        ocr_options = None

    opts = PdfPipelineOptions(
        do_ocr=do_ocr,
        ocr_options=ocr_options if do_ocr else RapidOcrOptions(),
        do_table_structure=True,
        do_formula_enrichment=False,      # base formula/LaTeX detection still runs
        generate_picture_images=not args.disable_image_extraction,
        images_scale=2.0,                 # accuracy priority
    )

    if args.use_ollama_vlm:
        opts.enable_remote_services = True
        opts.do_picture_description = True
        url = os.environ.get("OLLAMA_VLM_URL", "http://localhost:11434/v1/chat/completions")
        model = args.ollama_vlm_model or os.environ.get("OLLAMA_VLM_MODEL", "gemma4:31b-cloud")
        headers = {}
        if os.environ.get("OLLAMA_API_KEY"):
            headers["Authorization"] = f"Bearer {os.environ['OLLAMA_API_KEY']}"
        opts.picture_description_options = PictureDescriptionApiOptions(
            url=url,
            params={"model": model},
            headers=headers,
            timeout=float(os.environ.get("OLLAMA_VLM_TIMEOUT", "120")),
            concurrency=int(os.environ.get("OLLAMA_VLM_CONCURRENCY", "2")),
            prompt=(
                "Describe this figure/diagram/chart concisely for a technical reader: "
                "what it shows, its axes/labels, and its key takeaway. Output one paragraph."
            ),
        )
        log(f"Ollama VLM picture description enabled: model={model} url={url}")
    return opts


def main():
    ap = argparse.ArgumentParser(description="Drive Docling DocumentConverter -> raw markdown + meta + images.")
    ap.add_argument("--input", required=True, help="Path to the PDF.")
    ap.add_argument("--output-dir", required=True, help="Output directory.")
    ap.add_argument("--mode", default="auto",
                    choices=["auto", "disable_ocr", "fast", "balanced"],
                    help="Compat with the old marker CLI. auto (default) inspects the PDF; "
                         "disable_ocr forces OCR off; fast/balanced force OCR on. "
                         "balanced does NOT auto-enable the Ollama VLM (use --use-ollama-vlm).")
    ap.add_argument("--force-ocr", action="store_true", help="Force OCR on all pages.")
    ap.add_argument("--strip-existing-ocr", action="store_true",
                    help="Discard embedded OCR text and re-OCR every page (full-page OCR).")
    ap.add_argument("--use-ollama-vlm", action="store_true",
                    help="Enable Ollama cloud-vision VLM for figure descriptions.")
    ap.add_argument("--ollama-vlm-model", default=None,
                    help="Ollama vision model (default gemma4:31b-cloud; env OLLAMA_VLM_MODEL).")
    ap.add_argument("--disable-image-extraction", action="store_true", help="Skip image extraction.")
    ap.add_argument("--lang", default=None, help="OCR language hint (e.g. 'en').")
    ap.add_argument("--book-slug", default=None, help="Override the derived book slug.")
    ap.add_argument("--page-range", default=None,
                    help='Page range, e.g. "0,5-10,20" (accepted but not yet supported; warns).')
    args = ap.parse_args()

    input_path = Path(args.input).expanduser().resolve()
    if not input_path.exists():
        print(json.dumps({"ok": False, "error": f"file not found: {input_path}"}))
        return 2

    if args.page_range:
        log("WARN: --page-range is not yet supported by the Docling driver; "
            "converting the full document.")

    # Docling auto-detects CUDA; mirror the old behaviour for robustness.
    if not os.environ.get("TORCH_DEVICE"):
        try:
            import torch
            if torch.cuda.is_available():
                os.environ["TORCH_DEVICE"] = "cuda"
        except Exception:
            pass

    out_dir = Path(args.output_dir).expanduser().resolve()
    out_dir.mkdir(parents=True, exist_ok=True)

    try:
        do_ocr = decide_ocr(args, input_path)
        log(f"do_ocr={do_ocr}")

        page_count, pdf_title, pdf_author = pdf_metadata(input_path)
        # Slug from the filename stem (matches full_import.py's local_slug and the
        # existing ingest-test dirs). PDF metadata titles are often cover-page
        # garbage (e.g. 'ToolBox_cover_13'); the displayed book title still comes
        # from postprocess.py (PDF outline / metadata), unaffected by the slug.
        book_slug = args.book_slug or slugify(input_path.stem)

        raw_dir = out_dir / book_slug / "raw"
        images_dir = raw_dir / "images"
        raw_md_path = raw_dir / f"{book_slug}.raw.md"
        meta_json_path = raw_dir / f"{book_slug}.meta.json"
        conversion_json_path = raw_dir / "conversion.json"
        raw_dir.mkdir(parents=True, exist_ok=True)

        from docling.document_converter import DocumentConverter, PdfFormatOption
        from docling.datamodel.base_models import InputFormat
        from docling_page_span import export_to_markdown_with_page_spans

        opts = build_pipeline_options(args, do_ocr)
        converter = DocumentConverter(
            format_options={InputFormat.PDF: PdfFormatOption(pipeline_options=opts)}
        )

        t0 = time.time()
        log("Converting with Docling (first run downloads ~2-3 GB of models) ...")
        res = converter.convert(str(input_path))
        doc = res.document
        log(f"Converted {len(doc.pages)} pages in {time.time()-t0:.1f}s; serializing ...")

        md_text = export_to_markdown_with_page_spans(
            doc,
            raw_md_path=raw_md_path,
            images_dir=images_dir,
            traverse_pictures=False,
            escape_html=True,
            disable_images=args.disable_image_extraction,
        )
        raw_md_path.write_text(md_text, encoding="utf-8")
        elapsed = time.time() - t0

        image_count = 0
        if not args.disable_image_extraction and images_dir.exists():
            image_count = len(list(images_dir.glob("image_*.png")))

        # meta.json: postprocess.py reads table_of_contents (fallback), page_stats
        # (page_count fallback), author, title. The PRIMARY chapter source is the
        # PDF outline, which postprocess reads itself via conversion.json.source_pdf.
        meta = {
            "title": pdf_title or None,
            "author": pdf_author or None,
            "page_count": page_count,
            "page_stats": [{"page_id": i} for i in range(page_count)],
            "table_of_contents": [],
        }
        meta_json_path.write_text(json.dumps(meta, ensure_ascii=False, indent=2), encoding="utf-8")

        # Docling version (best effort)
        docling_version = None
        try:
            import importlib.metadata as md
            docling_version = md.version("docling")
        except Exception:
            pass

        vlm_info = {"enabled": False}
        if args.use_ollama_vlm:
            vlm_info = {
                "enabled": True,
                "provider": "ollama",
                "model": args.ollama_vlm_model or os.environ.get("OLLAMA_VLM_MODEL", "gemma4:31b-cloud"),
            }

        provenance = {
            "source_pdf": str(input_path),
            "source_pdf_sha256": sha256_file(input_path),
            "book_slug": book_slug,
            "mode": "docling",
            # Compat shim: postprocess.py:575 reads marker_version for the
            # converter frontmatter label. Kept so postprocess needs no change.
            "marker_version": f"docling {docling_version}" if docling_version else "docling",
            "engine": "docling",
            "engine_version": docling_version,
            "ocr": do_ocr,
            "ocr_engine": "rapidocr" if do_ocr else "none",
            "vlm": vlm_info,
            "force_ocr": bool(args.force_ocr),
            "strip_existing_ocr": bool(args.strip_existing_ocr),
            "disable_image_extraction": bool(args.disable_image_extraction),
            "image_count": image_count,
            "elapsed_s": round(elapsed, 2),
            "converted_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        }
        conversion_json_path.write_text(
            json.dumps(provenance, ensure_ascii=False, indent=2), encoding="utf-8"
        )

        print(json.dumps({
            "ok": True,
            "book_slug": book_slug,
            "mode": "docling",
            "raw_md": str(raw_md_path),
            "meta_json": str(meta_json_path),
            "images_dir": str(images_dir),
            "image_count": image_count,
            "conversion_json": str(conversion_json_path),
            "ocr": do_ocr,
            "elapsed_s": round(elapsed, 2),
        }, ensure_ascii=False))
        return 0
    except Exception as e:
        import traceback
        log(traceback.format_exc())
        print(json.dumps({"ok": False, "error": str(e)}, ensure_ascii=False))
        return 1


if __name__ == "__main__":
    sys.exit(main())

````
"#,
    ),
    (
        "pdf-to-markdown-script-postprocess.md",
        r###"---
type: Skill
title: pdf-to-markdown script — postprocess.py
description: Verbatim copy of the pdf-to-markdown skill's python script `postprocess.py`. Adds frontmatter, TOC, stable heading IDs, page citations and the figure manifest (imports html_cleanup). Extract the fenced block byte-exact; verify md5 `ebce3858e7b192a14e376a70daf28284` against the [scripts manifest](/pdf-to-markdown-scripts.md).
tags:
- pdf-to-markdown
- docling
- script
- packaged
timestamp: 2026-09-21T00:00:00Z
---

# pdf-to-markdown script — postprocess.py

Byte-exact copy of `postprocess.py` from the [pdf-to-markdown](/pdf-to-markdown.md) skill (source tree: `scripts/postprocess.py`). **no trailing newline**

Extract the fenced block below **exactly**: strip the opening ` ```` ` line and the closing ` ```` ` line, keep every byte between them — including the final line's missing newline if the manifest says "no trailing newline" (an editor that silently appends one is harmless for these scripts, but the md5 will differ).

`30079 bytes, md5 ebce3858e7b192a14e376a70daf28284`

````
#!/usr/bin/env python3
"""postprocess_v2 — outline-driven chapter detection prototype.

Diff vs scripts/postprocess.py:
  * Chapter structure comes from the PDF's own embedded outline (pypdfium2),
    NOT marker's degraded markdown/HTML headings. The outline (clean titles +
    page indices) is aligned to marker's body by <span id="page-N-M"> page
    spans (marker and pypdfium2 share the same 0-based PDF page index).
  * For each outline chapter, a clean markdown '# Title {#ch-N-slug}' heading
    is placed at the chapter's page-span boundary: if a matching markdown
    heading already sits there, it is REPLACED (clean title, promoted to H1,
    id added); otherwise a new heading is INSERTED before the page span
    (handles marker's HTML <h1>-in-blob case, e.g. Python Crash Course ch3+).
  * Book title from PDF metadata Title (fixes PCC 'PRAISE FOR...' bug).
  * Falls back to the original marker-TOC / page-anchored-H1 heuristics when
    there is no source PDF, no outline, or the outline yields no chapters.

Same output contract as postprocess.py (one JSON line on stdout) plus
chapter_titles, chapter_method, book_title_source for test visibility.
"""
import argparse, json, re, sys, time
from pathlib import Path
import pypdfium2 as pdfium

# HTML-body cleanup (converts marker's run-on HTML-blob lines to readable
# markdown; leaves clean-markdown books untouched). See html_cleanup.py.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from html_cleanup import html_to_markdown


def log(msg):
    print(msg, file=sys.stderr, flush=True)


HTML_TAG_RE = re.compile(r"<[^>]+>")
EMPH_RE = re.compile(r"(\*\*|__|`)(.*?)\1")
CODEISH_RE = re.compile(r'[=`";]|//')
SLUG_MAX = 64
CHAPTER_CAP = 50
CHAPTER_CAP_SPANNED = 200

# ---------- outline chapter identification (oracle v4 logic, inlined) ----------
CHAP_N_RE   = re.compile(r"^\s*Chapter\s+\d+\b", re.IGNORECASE)
NUM_DOT_RE  = re.compile(r"^\s*\d+\.\s+\S")
BARE_NUM_RE = re.compile(r"^\s*\d+$")
APPX_RE     = re.compile(r"^\s*Appendix\b", re.IGNORECASE)
APPX_LET_RE = re.compile(r"^\s*[A-Z]\.\s+\S")
PART_RE     = re.compile(r"^\s*Part\s+", re.IGNORECASE)
SPAN_BOTH_RE = re.compile(r"<span id=['\"]page-(\d+)-\d+['\"]")

FRONT_BACK_PREFIX = (
    "cover","front cover","back cover","title page","copyright","dedication",
    "about the author","about the authors","about the author and technical reviewer",
    "about the reviewer","about the reviewers","contributors",
    "brief contents","contents in detail","table of contents","contents",
    "foreword","preface","acknowledgemen","acknowledgment","introduction","index",
    "colophon","resources","praise for","bibliography","updates","front matter",
    "about this book","who is this book for","who this book is for",
    "what this book covers","to get the most out of this book",
    "how to contact us","get in touch","o'reilly online learning","oreilly online learning",
    "conventions used","using code examples","safari enabled",
    "what is programming","what is python","common myths about programming",
    "new to the third edition","online resources","why python",
    "what can you expect to learn",
)
PACKT_PROMO_PREFIX = (
    "unlock access","other books you may enjoy","why subscribe","packt is searching",
    "share your thoughts","get in touch","free benefits with your book",
    "download the example code","download the color images","need help",
    "step 1","step 2","step 3",
)


def _fbnorm(t):
    t = (t or "").strip()
    t = re.sub(r"\s+", " ", t).rstrip(" .")
    return t.lower()

def _is_frontback(title):
    t = _fbnorm(title)
    if not t:
        return True
    for kw in FRONT_BACK_PREFIX:
        if t == kw or t.startswith(kw):
            return True
    return False

def _is_appendix(title):
    return bool(APPX_RE.match(title) or APPX_LET_RE.match(title))

def _is_packt_promo(title):
    t = _fbnorm(title)
    for kw in PACKT_PROMO_PREFIX:
        if t == kw or t.startswith(kw):
            return True
    return False


def load_outline(pdf_path):
    """Return (book_title, chapters, method, appendices, page_count).
    chapters = [{title, page}] in reading order (page = 0-based PDF page index)."""
    try:
        doc = pdfium.PdfDocument(pdf_path)
    except Exception as e:
        log(f"WARN: could not open PDF for outline: {e}")
        return None, [], "none", [], 0
    page_count = len(doc)
    book_title = None
    try:
        book_title = (doc.get_metadata_dict() or {}).get("Title") or None
    except Exception:
        pass
    toc = list(doc.get_toc())
    if not toc:
        return book_title, [], "none", [], page_count

    entries, stack = [], []
    for b in toc:
        title = (b.get_title() or "").strip()
        lvl = b.level
        try:
            pidx = b.get_dest().get_index()
        except Exception:
            pidx = None
        while stack and stack[-1][0] >= lvl:
            stack.pop()
        parent = stack[-1][1] if stack else None
        stack.append((lvl, title))
        entries.append({"level": lvl, "page": pidx, "title": title, "parent": parent})

    parts = [e for e in entries if e["level"] == 0 and PART_RE.match(e["title"])]
    appendices = [e for e in entries if _is_appendix(e["title"])]
    chapters, method = [], "none"

    chap_n = [e for e in entries if CHAP_N_RE.match(e["title"])]
    if chap_n:
        chapters, method = chap_n, "chapter_n"
    else:
        l0 = [e for e in entries if e["level"] == 0]
        packt = []
        for i, e in enumerate(l0):
            if BARE_NUM_RE.match(e["title"]) and i + 1 < len(l0) \
               and not BARE_NUM_RE.match(l0[i+1]["title"]) \
               and not _is_packt_promo(l0[i+1]["title"]) and not _is_appendix(l0[i+1]["title"]):
                packt.append(l0[i+1])
        if len(packt) >= 2:
            chapters, method = packt, "packt"
        else:
            numbered = [e for e in entries if NUM_DOT_RE.match(e["title"]) and not _is_appendix(e["title"])]
            if parts:
                numbered = [e for e in numbered if e["parent"] is not None and PART_RE.match(e["parent"] or "")]
            if numbered:
                chapters, method = numbered, "numbered"
            else:
                body = [e for e in entries if e["level"] == 0 and not _is_frontback(e["title"])
                        and not _is_appendix(e["title"]) and not BARE_NUM_RE.match(e["title"])
                        and not PART_RE.match(e["title"])]
                if len(body) >= 2:
                    chapters, method = body, "topic_l0"
                elif len(body) == 1:
                    title_entry = body[0]
                    children = [e for e in entries if e["parent"] == title_entry["title"] and e["level"] == 1]
                    if len(children) >= 2:
                        chapters, method = children, "topic_l1_children"
                    else:
                        chapters, method = [], "none"
                else:
                    chapters, method = [], "none"

    # Keep chapters even with page=None (broken bookmark); the alignment loop's
    # title fallback locates them by heading text in that case.
    return book_title, chapters, method, appendices, page_count


# ---------- shared helpers (unchanged from postprocess.py) ----------
def strip_html(text):
    text = HTML_TAG_RE.sub(" ", text or "")
    return (text.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")
            .replace("&quot;", '"').replace("&#39;", "'").replace("&nbsp;", " "))

def clean_title(text):
    text = strip_html(text)
    text = EMPH_RE.sub(r"\2", text)
    text = re.sub(r"[*_`]", "", text)
    return re.sub(r"\s+", " ", text).strip()

def looks_like_heading_title(text):
    t = clean_title(text)
    if not t or len(t) > 120:
        return False
    return not CODEISH_RE.search(t)

def slugify(text):
    text = clean_title(text).strip().lower()
    text = re.sub(r"[^\w\s-]", "", text)
    text = re.sub(r"[\s_-]+", "-", text).strip("-")
    if len(text) > SLUG_MAX:
        text = text[:SLUG_MAX].rstrip("-")
    return text or "section"

def norm_title(t):
    return re.sub(r"\s+", " ", clean_title(t).strip().lower()).rstrip(".")

HEADING_RE = re.compile(r"^(#{1,6})\s+(.*?)\s*({#[^}]+})?\s*$")
IMAGE_RE = re.compile(r"!\[([^\]]*)\]\(([^)]+)\)")

def load_json(path):
    if path is None:
        return {}
    p = Path(path)
    if not p.exists():
        return {}
    return json.loads(p.read_text(encoding="utf-8"))

def parse_headings(md_text):
    headings = []
    in_fence = False
    fence_tok = None
    for i, line in enumerate(md_text.splitlines()):
        m = re.match(r"^\s*(```|~~~)", line)
        if m and not in_fence:
            in_fence = True; fence_tok = m.group(1); continue
        if in_fence:
            if re.match(r"^\s*" + re.escape(fence_tok) + r"\s*$", line):
                in_fence = False; fence_tok = None
            continue
        m = HEADING_RE.match(line)
        if m and looks_like_heading_title(m.group(2)):
            headings.append({"line": i, "level": len(m.group(1)),
                             "title": clean_title(m.group(2)), "raw": line})
    return headings

def build_toc_index(meta):
    toc = []
    if not isinstance(meta, dict):
        return toc, {}
    raw_toc = meta.get("table_of_contents") or []
    by_title = {}
    for e in raw_toc:
        if not isinstance(e, dict):
            continue
        title = e.get("title")
        if not title:
            continue
        entry = {"title": clean_title(title), "heading_level": e.get("heading_level"),
                 "page_id": e.get("page_id")}
        toc.append(entry)
        nt = norm_title(title)
        if nt not in by_title:
            by_title[nt] = entry
    return toc, by_title


# ---------- outline-driven alignment helpers ----------
def _match_norm(t):
    """Aggressive normalize for title matching: strip 'Chapter N' prefix,
    strip markup/emphasis, remove ALL whitespace, lowercase."""
    t = re.sub(r"^\s*Chapter\s+\d+\b[:.]?\s*", "", t or "", flags=re.IGNORECASE)
    t = re.sub(r"<[^>]+>", "", t)
    t = re.sub(r"[*_`]", "", t)
    t = re.sub(r"\s+", "", t).lower()
    return t

def is_chapter_id(hid):
    """Chapter ids are 'ch-<n>-<slug>'; section ids use the 'sec-' prefix so a
    chapter title that starts with a digit (e.g. '1. Python Basics' ->
    slug '1-python-basics' -> id 'ch-1-1-python-basics') is NOT mistaken for a
    section 'ch-<n>-<m>-<slug>'."""
    return hid.startswith("ch-")


_FRONT_TITLE_PREFIX = (
    "cover", "copyright", "table of contents", "contents", "preface",
    "foreword", "acknowledgemen", "acknowledgment", "introduction",
    "dedication", "about the author", "praise", "brief contents",
    "contents in detail", "title page", "front cover", "back cover",
)

def is_letter_spaced(t):
    """True if a title looks like all-caps letter-spaced ('R E A L W O R L D')."""
    toks = (t or "").split()
    if len(toks) < 4:
        return False
    single = sum(1 for tk in toks if len(tk) == 1)
    return single >= len(toks) * 0.6

def first_clean_h1_title(headings):
    """First non-letter-spaced, non-front-matter markdown H1 — a clean book
    title when the PDF metadata Title is letter-spaced (e.g. REAL-WORLD PYTHON
    while metadata says 'R E A L W O R L D PYTHON')."""
    for h in headings:
        if h["level"] != 1:
            continue
        t = h["title"].strip()
        if not t or is_letter_spaced(t):
            continue
        low = re.sub(r"\s+", " ", t).strip().lower()
        if any(low.startswith(k) for k in _FRONT_TITLE_PREFIX):
            continue
        return t
    return None

def page_span_spans(text):
    """All page spans as (page, char_offset) in text order."""
    return [(int(m.group(1)), m.start()) for m in SPAN_BOTH_RE.finditer(text)]

def line_starts_of(text):
    """Char offset of the start of each line (line 0 = 0). len == #lines + 1."""
    starts = [0]
    pos = 0
    for ln in text.splitlines():
        pos += len(ln) + 1
        starts.append(pos)
    return starts

def find_boundary(spans, page, next_page=None):
    """Char offset of a span for `page` (exact preferred), else the first span
    with page in [page, next_page) — i.e. within this chapter's page range, so
    a chapter with no own span does not jump to a far later chapter's span
    (which would cluster misplaced headings). Returns None if none in range."""
    # exact match first
    for (pg, off) in spans:
        if pg == page:
            return off
    # within-range match (avoid leaping past next chapter's page)
    hi = next_page if next_page is not None else page + 50
    for (pg, off) in spans:
        if page <= pg < hi:
            return off
    return None

def find_candidate_heading(headings, line_starts, boundary_off, outline_title, max_lines=8):
    """Nearest markdown heading within max_lines of the boundary whose normalized
    title matches the outline title. Returns the heading or None."""
    # line index of boundary char
    import bisect
    bli = bisect.bisect_right(line_starts, boundary_off) - 1
    target = _match_norm(outline_title)
    if not target:
        return None
    best = None
    for h in headings:
        if abs(h["line"] - bli) > max_lines:
            continue
        if _match_norm(h["title"]) == target:
            if best is None or abs(h["line"] - bli) < abs(best["line"] - bli):
                best = h
    return best


def main():
    ap = argparse.ArgumentParser(description="Post-process marker markdown (outline-driven).")
    ap.add_argument("--raw-md", required=True)
    ap.add_argument("--meta", required=True)
    ap.add_argument("--conversion", required=True)
    ap.add_argument("--output", required=True)
    ap.add_argument("--split-by-chapter", action="store_true")
    args = ap.parse_args()

    try:
        raw_path = Path(args.raw_md).expanduser().resolve()
        meta = load_json(args.meta)
        conv = load_json(args.conversion)
        md_text = raw_path.read_text(encoding="utf-8")
        # Convert marker's HTML-blob body to readable markdown BEFORE chapter
        # detection. Page spans <span id='page-N-M'></span> are preserved, so
        # outline-to-page-span alignment still works; formerly-HTML <h1>
        # chapter headings become markdown headings that get replaced (no
        # duplicate). Clean-markdown books are essentially unchanged.
        md_text = html_to_markdown(md_text)

        toc, toc_by_title = build_toc_index(meta)
        page_stats = meta.get("page_stats") if isinstance(meta, dict) else None
        page_count_meta = len(page_stats) if isinstance(page_stats, list) else None

        headings = parse_headings(md_text)
        line_starts = line_starts_of(md_text)
        spans = page_span_spans(md_text)

        source_pdf = conv.get("source_pdf", "")
        book_title, outline_chs, method, outline_appx, pdf_page_count = (None, [], "none", [], 0)
        used_outline = False
        if source_pdf and Path(source_pdf).exists():
            book_title, outline_chs, method, outline_appx, pdf_page_count = load_outline(source_pdf)

        chapter_marks = []  # {hid, title, src, line(after edit)} for output TOC
        chapter_titles_out = []
        book_title_source = "marker"

        if outline_chs:
            used_outline = True
            book_title_source = "pdf_metadata" if book_title else "marker"
            # Build chapter heading edits.
            edits = []  # (pos, end, replacement) applied in reverse pos order
            n = len(outline_chs)
            last_line = -1  # line index of the previous chapter's edit (for page-None fallback)
            for i, ch in enumerate(outline_chs):
                title = ch["title"]
                hid = f"ch-{i+1}-{slugify(title)}"
                cpage = ch["page"]
                if cpage is not None:
                    start_d = cpage + 1
                    end_d = (outline_chs[i+1]["page"]) if i+1 < n and outline_chs[i+1]["page"] is not None else (pdf_page_count or page_count_meta or start_d)
                    if end_d < start_d:
                        end_d = start_d
                else:
                    start_d = end_d = 0
                src = f"<!-- src: pp. {start_d}-{end_d} -->" if cpage is not None else ""
                cand = None
                boff = None
                if cpage is not None:
                    nxt = outline_chs[i+1]["page"] if i+1 < n and outline_chs[i+1]["page"] is not None else None
                    boff = find_boundary(spans, cpage, nxt)
                    if boff is not None:
                        cand = find_candidate_heading(headings, line_starts, boff, title)
                # page-None (broken bookmark) or no span: fall back to a title match
                # in a heading AFTER the previous chapter's line.
                if cand is None:
                    target = _match_norm(title)
                    for h in headings:
                        if h["line"] > last_line and _match_norm(h["title"]) == target:
                            cand = h
                            break
                if cand is None and boff is None:
                    log(f"WARN: cannot locate chapter {i+1} '{title}' (page={cpage}, no span/title match); skipping.")
                    continue
                heading_line = f"# {title} {{#{hid}}}"
                if cand is not None:
                    # REPLACE: consumes the whole heading line (incl. trailing \n).
                    block = heading_line + "\n" + (src + "\n" if src else "")
                    ls = line_starts[cand["line"]]
                    le = line_starts[cand["line"]+1] if cand["line"]+1 < len(line_starts) else len(md_text)
                    edits.append((ls, le, block))
                    last_line = cand["line"]
                else:
                    # INSERT mid-blob: lead with \n so the heading is on its own
                    # line (markdown headings must start a line, and downstream
                    # tooling matches line-anchored headings).
                    block = "\n" + heading_line + "\n" + (src + "\n" if src else "")
                    edits.append((boff, boff, block))
                    import bisect
                    last_line = bisect.bisect_right(line_starts, boff) - 1
                chapter_marks.append({"hid": hid, "title": title, "src": src})
                chapter_titles_out.append(title)

            # Fallback for pagination gaps: if span/title alignment was
            # incomplete (e.g. marker paginated only the first/last pages and
            # emitted no recognizable titles for the middle) but marker's H1
            # headings match the outline chapter count, order-match: the k-th
            # outline chapter takes the k-th H1's position (marker's H1s are
            # chapter starts in order). Only triggers when incomplete AND counts
            # match, so complete-pagination books are unaffected. Recovers TDD.
            if 0 < len(chapter_marks) < len(outline_chs):
                h1s = [h for h in headings if h["level"] == 1 and not _is_frontback(h["title"])]
                if len(h1s) == len(outline_chs):
                    log(f"WARN: span/title alignment placed {len(chapter_marks)}/{len(outline_chs)}; "
                        f"H1 count matches outline ({len(h1s)}); order-matching outline titles to H1 positions.")
                    edits = []; chapter_marks = []; chapter_titles_out = []
                    for i, ch in enumerate(outline_chs):
                        title = ch["title"]; hid = f"ch-{i+1}-{slugify(title)}"
                        cpage = ch["page"]
                        if cpage is not None:
                            nxt = outline_chs[i+1]["page"] if i+1 < n and outline_chs[i+1]["page"] is not None else None
                            start_d = cpage + 1
                            end_d = nxt if nxt is not None else (pdf_page_count or page_count_meta or start_d)
                            if end_d < start_d: end_d = start_d
                            src = f"<!-- src: pp. {start_d}-{end_d} -->"
                        else:
                            src = ""
                        h = h1s[i]
                        block = f"# {title} {{#{hid}}}\n" + (src + "\n" if src else "")
                        ls = line_starts[h["line"]]
                        le = line_starts[h["line"]+1] if h["line"]+1 < len(line_starts) else len(md_text)
                        edits.append((ls, le, block))
                        chapter_marks.append({"hid": hid, "title": title, "src": src})
                        chapter_titles_out.append(title)

            # Apply edits in reverse position order.
            edits.sort(key=lambda e: e[0], reverse=True)
            for (pos, end, rep) in edits:
                md_text = md_text[:pos] + rep + md_text[end:]

            is_chapter_line = None  # will derive from re-parsed headings
        else:
            # ---- fallback: original marker-TOC / page-anchored-H1 heuristics ----
            chapter_start_indices = []
            heading_page_ids = [None] * len(headings)
            for hi, h in enumerate(headings):
                e = toc_by_title.get(norm_title(h["title"]))
                if e is not None and e.get("heading_level") == 1:
                    heading_page_ids[hi] = e.get("page_id")
                    chapter_start_indices.append(hi)
            if not chapter_start_indices and headings:
                level1 = [hi for hi, h in enumerate(headings) if h["level"] == 1]
                spanned = [hi for hi in level1 if '<span id="page-' in headings[hi]["raw"]]
                if 0 < len(spanned) <= CHAPTER_CAP_SPANNED:
                    chapter_start_indices = spanned
                elif 0 < len(level1) <= CHAPTER_CAP:
                    chapter_start_indices = level1
                elif len(level1) > CHAPTER_CAP:
                    log(f"WARN: TOC unusable and {len(level1)} top-level headings (>{CHAPTER_CAP}); "
                        f"writing a single whole-book concept.")
                else:
                    log("WARN: no chapter structure; writing a single whole-book concept.")
            is_chapter = set(chapter_start_indices)
            # rewrite heading ids (original behaviour)
            lines = md_text.splitlines()
            heading_ids = [None] * len(headings)
            current_chapter = 0; chapter_idx = 0; sec_counter = 0
            for hi, h in enumerate(headings):
                slug = slugify(h["title"])
                if hi in is_chapter:
                    chapter_idx += 1; current_chapter = chapter_idx; sec_counter = 0
                    hid = f"ch-{chapter_idx}-{slug}"
                else:
                    sec_counter += 1
                    hid = f"sec-{current_chapter}-{sec_counter}-{slug}" if current_chapter else f"sec-0-{hi+1}-{slug}"
                heading_ids[hi] = hid
                lines[h["line"]] = f"{'#' * h['level']} {h['title']} {{#{hid}}}"
            md_text = "\n".join(lines)
            # chapter_marks for TOC/frontmatter
            for hi in chapter_start_indices:
                chapter_marks.append({"hid": heading_ids[hi], "title": headings[hi]["title"], "src": ""})
                chapter_titles_out.append(headings[hi]["title"])

        # ---- re-parse modified text; assign section ids to non-chapter headings ----
        new_headings = parse_headings(md_text)
        lines = md_text.splitlines()
        current_chapter = 0
        sec_counter = 0
        for h in new_headings:
            m = HEADING_RE.match(h["raw"])
            existing_id = m.group(3) if m and m.group(3) else None
            if existing_id and existing_id.startswith("{#") and existing_id.endswith("}"):
                hid = existing_id[2:-1]
                if is_chapter_id(hid):
                    try:
                        current_chapter = int(hid.split("-")[1])
                    except Exception:
                        pass
                    sec_counter = 0
                    continue  # chapter heading already has its id; keep line as-is
            sec_counter += 1
            slug = slugify(h["title"])
            hid = (f"sec-{current_chapter}-{sec_counter}-{slug}" if current_chapter
                   else f"sec-0-{h['line']+1}-{slug}")
            lines[h["line"]] = f"{'#' * h['level']} {h['title']} {{#{hid}}}"
        md_text = "\n".join(lines)

        # ---- figures ----
        figures = []
        for m in IMAGE_RE.finditer(raw_path.read_text(encoding="utf-8")):
            figures.append({"alt": m.group(1).strip(), "src": m.group(2).strip(), "page": None})

        # ---- frontmatter book title ----
        # Resolve a title from PDF metadata, then marker TOC / first heading /
        # slug. THEN, if whatever we ended up with is letter-spaced all-caps
        # (marker/metadata title-page artifact), replace it with the first clean
        # non-front-matter marker H1.
        if not book_title:
            if toc:
                book_title = next((e["title"] for e in toc if e.get("heading_level") == 0), None)
            if not book_title and headings:
                book_title = headings[0]["title"]
            if not book_title:
                book_title = conv.get("book_slug") or "Untitled"
            book_title_source = "marker"
        if book_title and is_letter_spaced(book_title):
            clean = first_clean_h1_title(headings)
            if clean:
                book_title = clean; book_title_source = "marker_h1"
        author = meta.get("author") if isinstance(meta, dict) else None
        source_pdf = conv.get("source_pdf", "")
        mode = conv.get("mode", "")
        marker_version = conv.get("marker_version")
        # New Docling conversions carry engine/engine_version; old marker
        # conversions carry only marker_version. Label either correctly.
        engine = conv.get("engine") or "marker-pdf"
        engine_ver = conv.get("engine_version") or (marker_version if engine == "marker-pdf" else None)
        if engine == "docling":
            converter_label = f"docling {engine_ver}" if engine_ver else "docling"
        else:
            converter_label = f"marker-pdf {marker_version}" if marker_version else "marker-pdf"
        converted_date = conv.get("converted_at") or time.strftime("%Y-%m-%d", time.gmtime())
        page_count = page_count_meta or pdf_page_count

        toc_for_fm = [{"id": cm["hid"], "title": cm["title"], "level": 1} for cm in chapter_marks]
        fm_lines = ["---",
                    f"title: {json.dumps(book_title, ensure_ascii=False)}"]
        if author:
            fm_lines.append(f"author: {json.dumps(author, ensure_ascii=False)}")
        fm_lines.append(f"source_pdf: {json.dumps(source_pdf, ensure_ascii=False)}")
        fm_lines.append(f"converted_date: '{converted_date}'")
        fm_lines.append(f"converter: {json.dumps(converter_label, ensure_ascii=False)}")
        fm_lines.append(f"mode: {mode}")
        if page_count:
            fm_lines.append(f"page_count: {page_count}")
        fm_lines.append("toc:")
        for e in toc_for_fm:
            fm_lines.append(f"  - id: {e['id']}")
            fm_lines.append(f"    title: {json.dumps(e['title'], ensure_ascii=False)}")
            fm_lines.append(f"    level: {e['level']}")
        fm_lines.append("---")

        toc_block = ["", "## Table of Contents", ""]
        for cm in chapter_marks:
            toc_block.append("- [" + cm["title"] + "](#" + cm["hid"] + ")")
        toc_block.append("")

        out_path = Path(args.output).expanduser().resolve()
        out_path.parent.mkdir(parents=True, exist_ok=True)
        body = md_text
        full = "\n".join(fm_lines) + "\n" + "\n".join(toc_block) + "\n" + body + "\n"
        out_path.write_text(full, encoding="utf-8")

        figures_dir = out_path.parent / "figures"
        figures_dir.mkdir(parents=True, exist_ok=True)
        manifest = {"book": book_title, "source_pdf": source_pdf,
                     "figure_count": len(figures), "figures": figures}
        (figures_dir / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2), encoding="utf-8")

        # optional per-chapter split (by inserted chapter heading ids)
        chapter_files = []
        if args.split_by_chapter and chapter_marks:
            ch_dir = out_path.parent / "chapters"
            ch_dir.mkdir(parents=True, exist_ok=True)
            body_lines = body.splitlines()
            id_to_line = {}
            for i, ln in enumerate(body_lines):
                mm = re.search(r"\{#(ch-\d+-[^}]+)\}", ln)
                if mm:
                    id_to_line.setdefault(mm.group(1), i)
            for ci, cm in enumerate(chapter_marks):
                hid = cm["hid"]
                start = id_to_line.get(hid, 0)
                end = id_to_line.get(chapter_marks[ci+1]["hid"], len(body_lines)) if ci+1 < len(chapter_marks) else len(body_lines)
                chunk = "\n".join(body_lines[start:end]).strip()
                cf = ch_dir / f"{hid}.md"
                cf.write_text(f"# {cm['title']}\n\n{chunk}\n", encoding="utf-8")
                chapter_files.append(str(cf))

        print(json.dumps({
            "ok": True,
            "readable_book": str(out_path),
            "figure_manifest": str(figures_dir / "manifest.json"),
            "figure_count": len(figures),
            "chapter_count": len(chapter_marks),
            "chapter_titles": chapter_titles_out,
            "chapter_method": method,
            "used_outline": used_outline,
            "book_title": book_title,
            "book_title_source": book_title_source,
        }, ensure_ascii=False))
        return 0
    except Exception as e:
        import traceback
        log(traceback.format_exc())
        print(json.dumps({"ok": False, "error": str(e)}, ensure_ascii=False))
        return 1


if __name__ == "__main__":
    sys.exit(main())

````
"###,
    ),
    (
        "pdf-to-markdown-script-docling-page-span.md",
        r#"---
type: Skill
title: pdf-to-markdown script — docling_page_span.py
description: Verbatim copy of the pdf-to-markdown skill's python script `docling_page_span.py`. MarkdownDocSerializer subclass replacing Docling page-break tokens with page-span <span> markers (imported by convert.py). Extract the fenced block byte-exact; verify md5 `d9d548923cfed4b2dc9b0e0679f69bf0` against the [scripts manifest](/pdf-to-markdown-scripts.md).
tags:
- pdf-to-markdown
- docling
- script
- packaged
timestamp: 2026-09-21T00:00:00Z
---

# pdf-to-markdown script — docling_page_span.py

Byte-exact copy of `docling_page_span.py` from the [pdf-to-markdown](/pdf-to-markdown.md) skill (source tree: `scripts/docling_page_span.py`). **no trailing newline**

Extract the fenced block below **exactly**: strip the opening ` ```` ` line and the closing ` ```` ` line, keep every byte between them — including the final line's missing newline if the manifest says "no trailing newline" (an editor that silently appends one is harmless for these scripts, but the md5 will differ).

`4887 bytes, md5 d9d548923cfed4b2dc9b0e0679f69bf0`

````
#!/usr/bin/env python3
"""Page-span adapter for the pdf-to-markdown skill (Docling engine).

postprocess.py aligns the PDF's own outline chapters to the markdown body using
``<span id="page-N-M">`` page spans (SPAN_BOTH_RE, postprocess.py:49), where N is
the 0-based PDF page index. Docling's document model carries 1-based ``page_no``
provenance on every element and emits page-break tokens at page boundaries, but
the stock markdown serializer discards the page number (it replaces every token
with one static string). This module subclasses the serializer to recover the
per-page number and emit the exact span format postprocess.py expects.

Emitted span format: ``<span id="page-{next_page-1}-{idx}"></span>``
  - next_page: the 1-based Docling page number of the content following the break
  - next_page-1: the 0-based PDF page index (pypdfium2 outline is 0-based)
  - idx: a sequential counter (ignored by postprocess.py; only N matters)
A leading ``page-0-0`` span is prepended because Docling emits no page-break
token before page-1 content, so page 0 would otherwise be unaddressable.

Referenced image links are produced by materializing pictures via
``_make_copy_with_refmode`` (the same helper DoclingDocument.save_as_markdown
uses) with ``reference_path`` set, so links are relative (``images/...``).
"""

from pathlib import Path

from docling_core.transforms.serializer.markdown import (
    MarkdownDocSerializer,
    MarkdownParams,
)
from docling_core.transforms.serializer.common import create_ser_result
from docling_core.types.doc.base import ImageRefMode

# Docling's page-break token (docling_core/.../serializer/common.py:713):
#   #_#_DOCLING_DOC_PAGE_BREAK_{prev_page}_{next_page}_#_#
# _get_page_breaks() (common.py:715) yields (full_match, prev_page, next_page).


class PageSpanMarkdownSerializer(MarkdownDocSerializer):
    """MarkdownDocSerializer that emits ``<span id="page-N-M">`` at each page
    boundary instead of a static page-break placeholder.

    The stock serializer (markdown.py:947) replaces every page-break token with
    ``self.params.page_break_placeholder or ""``, discarding prev/next page
    numbers. We override serialize_doc to format each token individually.
    """

    def serialize_doc(self, *, parts, **kwargs):
        text_res = "\n\n".join(p.text for p in parts if p.text)
        if self.requires_page_break():
            idx = 0
            for full_match, _prev_page, next_page in self._get_page_breaks(text=text_res):
                page_index_0based = next_page - 1  # Docling 1-based -> PDF 0-based
                span = f'<span id="page-{page_index_0based}-{idx}"></span>'
                text_res = text_res.replace(full_match, span, 1)
                idx += 1
            # Page 0 has no preceding page-break token; make it addressable.
            text_res = f'<span id="page-0-0"></span>\n\n' + text_res
        return create_ser_result(text=text_res, span_source=parts)


def export_to_markdown_with_page_spans(
    doc,
    *,
    raw_md_path: Path,
    images_dir: Path,
    traverse_pictures: bool = False,
    escape_html: bool = True,
    disable_images: bool = False,
) -> str:
    """Serialize ``doc`` to markdown with ``<span id="page-N-M">`` page spans.

    Pictures are materialized to ``images_dir`` and referenced by relative links
    (``images/...``) unless ``disable_images`` is True, in which case image
    placeholders are emitted and no files are written.

    Args:
        doc: the converted DoclingDocument.
        raw_md_path: destination path of the raw markdown (its parent is the
            reference_path for relative image URIs).
        images_dir: absolute directory to write image files into.
        traverse_pictures: recurse into PictureItems (needed for full-page
            OCR'd pages where text is nested under a picture).
        escape_html: passed through to MarkdownParams (spans are inserted after
            per-part escaping, so they survive unescaped either way).
        disable_images: emit ``<!-- image -->`` placeholders, write no files.
    """
    reference_path = raw_md_path.parent

    if disable_images:
        new_doc = doc
        image_mode = ImageRefMode.PLACEHOLDER
    else:
        images_dir.mkdir(parents=True, exist_ok=True)
        new_doc = doc._make_copy_with_refmode(
            artifacts_dir=images_dir,
            image_mode=ImageRefMode.REFERENCED,
            page_no=None,
            reference_path=reference_path,
        )
        image_mode = ImageRefMode.REFERENCED

    params = MarkdownParams(
        image_mode=image_mode,
        image_placeholder="<!-- image -->",
        page_break_placeholder="",  # non-None => page-break nodes are emitted
        escape_html=escape_html,
        traverse_pictures=traverse_pictures,
    )
    return PageSpanMarkdownSerializer(doc=new_doc, params=params).serialize().text

````
"#,
    ),
    (
        "pdf-to-markdown-script-html-cleanup.md",
        r#"---
type: Skill
title: pdf-to-markdown script — html_cleanup.py
description: Verbatim copy of the pdf-to-markdown skill's python script `html_cleanup.py`. HTML-to-markdown cleanup for run-on HTML blobs (imported by postprocess.py). Extract the fenced block byte-exact; verify md5 `9fa07ded3cf2f877a12020f98f65995d` against the [scripts manifest](/pdf-to-markdown-scripts.md).
tags:
- pdf-to-markdown
- docling
- script
- packaged
timestamp: 2026-09-21T00:00:00Z
---

# pdf-to-markdown script — html_cleanup.py

Byte-exact copy of `html_cleanup.py` from the [pdf-to-markdown](/pdf-to-markdown.md) skill (source tree: `scripts/html_cleanup.py`). **no trailing newline**

Extract the fenced block below **exactly**: strip the opening ` ```` ` line and the closing ` ```` ` line, keep every byte between them — including the final line's missing newline if the manifest says "no trailing newline" (an editor that silently appends one is harmless for these scripts, but the md5 will differ).

`7938 bytes, md5 9fa07ded3cf2f877a12020f98f65995d`

````
#!/usr/bin/env python3
"""HTML-body cleanup for marker's degraded output (e.g. Python Crash Course,
where marker emits the body as one giant run-on line of <p>/<h1>/<i>/<b>/<pre>/
<table> HTML + entities). Converts to readable markdown. Leaves clean-markdown
books essentially unchanged (they have ~0 HTML tags); protects fenced code
blocks and <pre> so '<stdio.h>' / 'a < b' in code is not stripped as a fake tag.
"""
import re, html, sys

PAGE_SPAN_RE = re.compile(r"<span id=['\"]page-(\d+)-(\d+)['\"]>\s*</span>")
PRE_RE = re.compile(r"<pre[^>]*>(.*?)</pre>", re.S | re.I)
# Fences are matched line-by-line below (a real markdown fence is a full line
# starting with ``` or ~~~). A line-based scan avoids misreading marker's
# Python-traceback underlines ('~~~~~~~^^^') as a ~~~ fence delimiter, which
# would protect a huge HTML-body region verbatim.
H_RE = re.compile(r"<(h[1-6])[^>]*>(.*?)</\1>", re.S | re.I)
P_RE = re.compile(r"<p[^>]*>(.*?)</p>", re.S | re.I)
LI_RE = re.compile(r"<li[^>]*>(.*?)</li>", re.S | re.I)
BLOCKQUOTE_RE = re.compile(r"<blockquote[^>]*>(.*?)</blockquote>", re.S | re.I)
TABLE_RE = re.compile(r"<table[^>]*>(.*?)</table>", re.S | re.I)
ROW_RE = re.compile(r"<tr[^>]*>(.*?)</tr>", re.S | re.I)
CELL_RE = re.compile(r"<t[hd][^>]*>(.*?)</t[hd]>", re.S | re.I)
IMG_RE = re.compile(r"<img[^>]*src=['\"]([^'\"]+)['\"][^>]*>", re.I)
A_RE = re.compile(r"<a[^>]*href=['\"]([^'\"]+)['\"][^>]*>(.*?)</a>", re.S | re.I)
BR_RE = re.compile(r"<br\s*/?>", re.I)
# Strip only KNOWN HTML tags (a backstop for leftovers the block/inline
# converters miss). Must NOT match markdown autolinks <http://...> (scheme has
# ':/' not a tag name), <stdio.h> includes, or shell redirects < file: those
# start with a word that is not an HTML tag, so they are preserved.
KNOWN_TAGS = (r"p|h[1-6]|i|em|b|strong|code|a|img|pre|blockquote|ul|ol|li|"
              r"table|thead|tbody|tr|th|td|span|sup|sub|br|div|html|body|"
              r"figure|figcaption|hr|font|small|mark|u|s|del|ins|kbd|samp|"
              r"var|cite|q|abbr|address|article|section|header|footer|nav|"
              r"aside|main|details|summary|dl|dt|dd|caption|colgroup|col")
STRAY_TAG_RE = re.compile(r"</?(?:" + KNOWN_TAGS + r")\b[^>\n]*>", re.I)

# token scheme: \x00<n>\x00 ... use unique placeholders
def _protect(text, pattern, prefix):
    store = []
    def repl(m):
        store.append(m.group(0)); return f"\x00{prefix}{len(store)-1}\x00"
    return pattern.sub(repl, text), store

def _restore(text, prefix, store, transform=lambda s: s):
    for i, s in enumerate(store):
        text = text.replace(f"\x00{prefix}{i}\x00", transform(s))
    return text

def _inline(text):
    text = IMG_RE.sub(lambda m: f"![]({m.group(1)})", text)
    text = A_RE.sub(lambda m: f"[{m.group(2)}]({m.group(1)})", text)
    text = re.sub(r"<(i|em)\b[^>]*>(.*?)</\1>", r"*\2*", text, flags=re.S | re.I)
    text = re.sub(r"<(b|strong)\b[^>]*>(.*?)</\1>", r"**\2**", text, flags=re.S | re.I)
    text = re.sub(r"<code\b[^>]*>(.*?)</code>", r"`\1`", text, flags=re.S | re.I)
    text = re.sub(r"<sup\b[^>]*>(.*?)</sup>", r"\1", text, flags=re.S | re.I)
    text = BR_RE.sub("\n", text)
    return text

def html_to_markdown(text):
    # 1. protect fenced code blocks (clean-markdown books use these). Line-based
    #    scan: a fence is a full line starting with ``` or ~~~, closed by a line
    #    that is just the same token. This does not match marker's mid-line
    #    traceback underlines ('~~~').
    fence_store = []
    lines = text.split("\n")
    out, buf = [], []
    in_fence, fence_tok = False, None
    for line in lines:
        stripped = line.strip()
        m = re.match(r"^(```|~~~)", stripped)
        if not in_fence:
            if m:
                in_fence, fence_tok, buf = True, m.group(1), [line]
            else:
                out.append(line)
        else:
            buf.append(line)
            if re.match(r"^" + re.escape(fence_tok) + r"\s*$", stripped):
                fence_store.append("\n".join(buf))
                out.append(f"\x00F{len(fence_store) - 1}\x00")
                in_fence, fence_tok, buf = False, None, []
    if in_fence:  # unterminated -> put it back unchanged
        out.extend(buf)
    text = "\n".join(out)
    # 2. protect <pre> blocks (marker HTML-blob books); decode entities inside -> fenced code
    pre_store = []
    def pre_repl(m):
        inner = html.unescape(m.group(1))
        pre_store.append(inner); return f"\x00PRE{len(pre_store)-1}\x00"
    text = PRE_RE.sub(pre_repl, text)
    # 3. protect page spans (needed for outline alignment) -> restore verbatim
    pg_store = []
    def pg_repl(m):
        pg_store.append(m.group(0)); return f"\x00PG{len(pg_store)-1}\x00"
    text = PAGE_SPAN_RE.sub(pg_repl, text)
    # 4. decode entities in the remaining (non-protected) text
    text = html.unescape(text)
    # 5. block-level conversions (each emits blank-line-separated blocks)
    def h_repl(m):
        n = int(m.group(1)[1]); return f"\n\n{'#' * n} {_inline(m.group(2)).strip()}\n\n"
    text = H_RE.sub(h_repl, text)
    # <p>: non-capture approach (robust on giant run-on lines where content may
    # contain '<' from decoded entities like &lt;module&gt;). Opening tag and
    # closing tag become paragraph breaks; inline tags in the content are
    # handled by the global _inline pass at step 6.
    text = re.sub(r"<p\b[^>]*>", "\n\n", text, flags=re.I)
    text = re.sub(r"</p\s*>", "\n\n", text, flags=re.I)
    text = LI_RE.sub(lambda m: f"\n- {_inline(m.group(1)).strip()}", text)
    text = BLOCKQUOTE_RE.sub(lambda m: "\n" + "\n".join("> " + ln for ln in _inline(m.group(1)).strip().splitlines() and _inline(m.group(1)).strip().splitlines()), text)
    # tables -> markdown pipe tables
    def tbl_repl(m):
        rows = []
        for rm in ROW_RE.finditer(m.group(1)):
            cells = [_inline(c).strip() for c in (cm.group(1) for cm in CELL_RE.finditer(rm.group(1)))]
            if cells:
                rows.append("| " + " | ".join(cells) + " |")
        if not rows:
            return ""
        # insert header separator after first row (assume first row is thead)
        if len(rows) >= 1:
            sep = "| " + " | ".join(["---"] * (rows[0].count("|") - 1)) + " |"
            rows.insert(1, sep)
        return "\n\n" + "\n".join(rows) + "\n\n"
    text = TABLE_RE.sub(tbl_repl, text)
    # 6. inline leftovers + strip stray HTML tags (guarded: must start with a
    #    letter, so 'a < b' / '<stdio.h>' inside code are protected anyway, and
    #    stray '<' in prose won't match)
    text = _inline(text)
    text = STRAY_TAG_RE.sub("", text)
    # catch any unclosed/malformed <p> or </p> P_RE missed (word-boundary after
    # p so '<prompt>', '<pre>', '<param>' in prose/code are NOT stripped)
    text = re.sub(r"</?p\b[^>\n]*>", "", text, flags=re.I)
    # final entity decode: tag removal can assemble entities that weren't
    # present at step 4 (e.g. '<sup>&</sup>lt;' -> '&lt;'). Fences/pre are still
    # tokens here, so only body prose/entities are affected (correct: '&lt;' in
    # prose is a literal '<').
    text = html.unescape(text)
    # 7. restore protected spans, pre (as fenced code), fences
    text = _restore(text, "PG", pg_store)
    text = _restore(text, "PRE", pre_store, transform=lambda s: "\n```\n" + s.strip("\n") + "\n```\n")
    text = _restore(text, "F", fence_store)
    # 8. tidy whitespace
    text = re.sub(r"[ \t]+\n", "\n", text)          # trailing spaces
    text = re.sub(r"\n{3,}", "\n\n", text)            # collapse blank runs
    return text.strip() + "\n"

if __name__ == "__main__":
    src = sys.argv[1]
    out = sys.argv[2] if len(sys.argv) > 2 else None
    t = open(src, encoding="utf-8").read()
    res = html_to_markdown(t)
    if out:
        open(out, "w", encoding="utf-8").write(res)
        print(f"wrote {len(res)} chars to {out}")
    else:
        print(res[:4000])

````
"#,
    ),
    (
        "pdf-to-markdown-script-inspect-pdf.md",
        r#"---
type: Skill
title: pdf-to-markdown script — inspect_pdf.py
description: 'Verbatim copy of the pdf-to-markdown skill''s python script `inspect_pdf.py`. Profiles a PDF: page count, text layer, outline, scanned guess, image count. Feeds the OCR decision. Extract the fenced block byte-exact; verify md5 `75e95808510774038a9523fe5a3a7d87` against the [scripts manifest](/pdf-to-markdown-scripts.md).'
tags:
- pdf-to-markdown
- docling
- script
- packaged
timestamp: 2026-09-21T00:00:00Z
---

# pdf-to-markdown script — inspect_pdf.py

Byte-exact copy of `inspect_pdf.py` from the [pdf-to-markdown](/pdf-to-markdown.md) skill (source tree: `scripts/inspect_pdf.py`). **no trailing newline**

Extract the fenced block below **exactly**: strip the opening ` ```` ` line and the closing ` ```` ` line, keep every byte between them — including the final line's missing newline if the manifest says "no trailing newline" (an editor that silently appends one is harmless for these scripts, but the md5 will differ).

`5890 bytes, md5 75e95808510774038a9523fe5a3a7d87`

````
#!/usr/bin/env python3
"""
pdf-to-markdown skill — fast pre-conversion PDF profile.

Lightweight (pypdfium2 + pdfplumber, no torch/marker) inspection that drives
mode selection in convert.py. Prints exactly one JSON line on stdout; progress
goes to stderr.

Usage:
    python inspect_pdf.py --input /path/to/book.pdf [--sample-pages 8]
"""

import argparse
import hashlib
import json
import sys
from pathlib import Path


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def sha256_file(path, chunk=1 << 20):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(chunk), b""):
            h.update(block)
    return h.hexdigest()


def spread_sample(n, k):
    """Pick up to k page indices spread across the document (first/middle/last)."""
    if n <= 0:
        return []
    if n <= k:
        return list(range(n))
    # Always include first and last; fill with evenly spaced middles.
    idxs = {0, n - 1}
    if k > 2:
        step = n / (k - 1)
        for i in range(1, k - 1):
            idxs.add(int(round(i * step)))
    return sorted(idxs)


def profile_pdf(path, sample_pages):
    profile = {
        "path": str(path),
        "sha256": sha256_file(path),
        "page_count": 0,
        "has_outline": False,
        "outline": [],
        "has_text_layer": False,
        "text_ratio": 0.0,
        "likely_scanned": False,
        "has_images": False,
        "image_count_estimate": 0,
        "page_sizes": [],
        "title": None,
    }

    # pypdfium2 for page count + outline (bookmarks/TOC).
    import pypdfium2 as pdfium

    doc = pdfium.PdfDocument(path)
    profile["page_count"] = len(doc)
    try:
        meta = doc.get_metadata_dict()
        if meta:
            profile["title"] = meta.get("Title") or None
    except Exception:
        pass
    try:
        # get_toc() returns a generator of PdfBookmark objects (truthy even
        # when empty), so materialize it and inspect the entries.
        bookmarks = list(doc.get_toc())
        outline = []
        for b in bookmarks:
            # pypdfium2 PdfBookmark: .level (int), .get_title() (str),
            # .get_dest() -> PdfDest with .get_index() (0-based page).
            lvl = getattr(b, "level", None)
            title = None
            if hasattr(b, "get_title"):
                try:
                    title = b.get_title()
                except Exception:
                    title = None
            elif hasattr(b, "title"):
                title = b.title
            page = None
            try:
                dest = b.get_dest()
                if dest is not None and hasattr(dest, "get_index"):
                    page = dest.get_index()
            except Exception:
                page = None
            # Older pypdfium2 returned (level, title, page) tuples.
            if title is None and isinstance(b, (list, tuple)) and len(b) >= 2:
                lvl, title = b[0], b[1]
                if len(b) >= 3:
                    page = b[2]
            if not title:
                continue
            outline.append({
                "level": int(lvl) if lvl is not None else 0,
                "title": str(title),
                "page": int(page) if page is not None else None,
            })
        if outline:
            profile["has_outline"] = True
            profile["outline"] = outline
    except Exception:
        pass

    # pdfplumber for text layer + images on a sample of pages.
    import pdfplumber

    sample_idx = spread_sample(profile["page_count"], sample_pages)
    text_hits = 0
    img_total = 0
    with pdfplumber.open(path) as pdf:
        for i in sample_idx:
            if i >= len(pdf.pages):
                continue
            page = pdf.pages[i]
            try:
                txt = page.extract_text() or ""
            except Exception:
                txt = ""
            if len(txt.strip()) > 50:
                text_hits += 1
            try:
                imgs = page.images or []
            except Exception:
                imgs = []
            if imgs:
                profile["has_images"] = True
            img_total += len(imgs)
            profile["page_sizes"].append(
                [round(float(page.width), 1), round(float(page.height), 1)]
            )

    if sample_idx:
        profile["text_ratio"] = round(text_hits / len(sample_idx), 3)
        profile["has_text_layer"] = profile["text_ratio"] > 0.6
        profile["likely_scanned"] = (
            not profile["has_text_layer"] and profile["has_images"]
        )
        profile["image_count_estimate"] = img_total

    # Recommended mode for convert.py.
    if profile["has_text_layer"] and not profile["likely_scanned"]:
        profile["recommended_mode"] = "disable_ocr"
    else:
        profile["recommended_mode"] = "fast"

    return profile


def main():
    ap = argparse.ArgumentParser(
        description="Fast PDF profile (page count, text layer, outline, images)."
    )
    ap.add_argument("--input", required=True, help="Path to the PDF.")
    ap.add_argument(
        "--sample-pages", type=int, default=8,
        help="Number of pages to sample for the text/image check (default 8).",
    )
    args = ap.parse_args()

    path = Path(args.input).expanduser().resolve()
    if not path.exists():
        print(json.dumps({"ok": False, "error": f"file not found: {path}"}))
        return 2
    if path.suffix.lower() != ".pdf":
        log(f"WARN: {path.name} does not end in .pdf")

    try:
        log(f"Profiling {path} ...")
        profile = profile_pdf(path, args.sample_pages)
        print(json.dumps({"ok": True, "profile": profile}, ensure_ascii=False))
        return 0
    except Exception as e:
        print(json.dumps({"ok": False, "error": str(e)}, ensure_ascii=False))
        return 1


if __name__ == "__main__":
    sys.exit(main())

````
"#,
    ),
    (
        "pdf-to-markdown-script-setup-venv.md",
        r#"---
type: Skill
title: pdf-to-markdown script — setup_venv.sh
description: 'Verbatim copy of the pdf-to-markdown skill''s shell script `setup_venv.sh`. One-time venv setup: uv + managed Python 3.12 + requirements.txt. Extract the fenced block byte-exact; verify md5 `9a881a2b5d93e09cbd7ec18f09bd8faf` against the [scripts manifest](/pdf-to-markdown-scripts.md).'
tags:
- pdf-to-markdown
- docling
- script
- packaged
timestamp: 2026-09-21T00:00:00Z
---

# pdf-to-markdown script — setup_venv.sh

Byte-exact copy of `setup_venv.sh` from the [pdf-to-markdown](/pdf-to-markdown.md) skill (source tree: `scripts/setup_venv.sh`). **no trailing newline**

Extract the fenced block below **exactly**: strip the opening ` ```` ` line and the closing ` ```` ` line, keep every byte between them — including the final line's missing newline if the manifest says "no trailing newline" (an editor that silently appends one is harmless for these scripts, but the md5 will differ).

`1604 bytes, md5 9a881a2b5d93e09cbd7ec18f09bd8faf`

````
#!/usr/bin/env bash
#
# pdf-to-markdown skill — one-time venv setup (Docling engine).
#
# Uses uv to provision a Python 3.12 venv and install Docling + RapidOCR +
# onnxruntime + pypdfium2 + pdfplumber. Python 3.12 matches the validated
# environment (docling 2.118.x); uv downloads a managed CPython 3.12 so no
# system Python change is needed. After setup, `uv pip check` reports all
# packages compatible.
#
# First Docling run downloads model weights (~2-3 GB: layout, table, OCR) to
# ~/.cache/docling/models (and HuggingFace cache). One-time.
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VENV="${PDF2MD_VENV:-$HOME/.venvs/pdf-to-markdown}"

# 1. Install uv if it is not already on PATH.
if ! command -v uv >/dev/null 2>&1; then
  echo "Installing uv ..."
  # ~/.local/bin is not always writable on this host; use ~/.cargo/bin.
  export UV_INSTALL_DIR="$HOME/.cargo/bin"
  curl -LsSf https://astral.sh/uv/install.sh | sh
fi
export PATH="$HOME/.cargo/bin:$PATH"

# 2. Ensure a managed CPython 3.12 is available (uv downloads it on demand).
echo "Ensuring Python 3.12 is available ..."
uv python install 3.12 >/dev/null 2>&1 || true

# 3. Create the venv and install requirements.
echo "Creating venv at $VENV ..."
uv venv "$VENV" --python 3.12
uv pip install --python "$VENV" -r "$SCRIPT_DIR/requirements.txt"

echo
echo "Setup complete: $VENV ($("$VENV/bin/python" --version 2>&1))"
echo "Convert with:"
echo "  $VENV/bin/python $SCRIPT_DIR/convert.py --input book.pdf --output-dir /out"
echo
echo "Verify with:  uv pip check --python $VENV   (expect: all packages compatible)"

````
"#,
    ),
    (
        "pdf-to-markdown-script-requirements.md",
        r#"---
type: Skill
title: pdf-to-markdown script — requirements.txt
description: Verbatim copy of the pdf-to-markdown skill's python requirements `requirements.txt`. Python dependencies for the skill venv. Extract the fenced block byte-exact; verify md5 `3daed4196aca9d9ecf6fae04840f7000` against the [scripts manifest](/pdf-to-markdown-scripts.md).
tags:
- pdf-to-markdown
- docling
- script
- packaged
timestamp: 2026-09-21T00:00:00Z
---

# pdf-to-markdown script — requirements.txt

Byte-exact copy of `requirements.txt` from the [pdf-to-markdown](/pdf-to-markdown.md) skill (source tree: `scripts/requirements.txt`). **no trailing newline**

Extract the fenced block below **exactly**: strip the opening ` ```` ` line and the closing ` ```` ` line, keep every byte between them — including the final line's missing newline if the manifest says "no trailing newline" (an editor that silently appends one is harmless for these scripts, but the md5 will differ).

`735 bytes, md5 3daed4196aca9d9ecf6fae04840f7000`

````
# pdf-to-markdown skill — Docling engine (replaces marker/surya/vLLM).
#
# Docling runs its layout/table/OCR models inline (in-process, clean exit — no
# surya-style persistent GPU servers, no vLLM, no Docker). The [rapidocr] extra
# pulls RapidOCR for scanned-PDF OCR; [remote-serving] enables the optional
# Ollama cloud-vision VLM for figure description (--use-ollama-vlm).
#
# NOTE: onnxruntime is the RapidOCR inference backend. rapidocr does NOT declare
# onnxruntime as a hard dependency (it is a multi-backend package), so it must be
# listed explicitly or OCR crashes with:
#   ImportError: onnxruntime is not installed.
docling[rapidocr,remote-serving]>=2.118.1
onnxruntime>=1.28.0
pypdfium2>=4.0
pdfplumber>=0.10
PyYAML>=6.0

````
"#,
    ),
    (
        "pdf-to-markdown-docling-options.md",
        r#"---
type: Skill
title: pdf-to-markdown docling options
description: 'Reference doc for the pdf-to-markdown skill''s convert.py: OCR selection, the Ollama VLM, formula handling, images, page spans, GPU/CPU, and the known risks. Read before running convert.py on scanned, math-heavy or multi-column PDFs.'
tags:
- pdf-to-markdown
- docling
- ocr
- ollama
- reference
timestamp: 2026-09-21T00:00:00Z
---

# pdf-to-markdown docling options

Read before running `convert.py` when the PDF is scanned, math-heavy, multi-column, or the user asks about GPU / OCR / the Ollama VLM. Verbatim reference from the [pdf-to-markdown](/pdf-to-markdown.md) skill (`references/docling-options.md`; no trailing newline in the source).

````
# docling-options.md

When to read this: before running `convert.py` if the PDF is scanned, math-heavy,
multi-column, or the user asks about GPU / OCR / the Ollama VLM.

## Engine

The skill uses **Docling** (`pip install docling`), specifically its
`StandardPdfPipeline`, as a single engine for every book. Docling does layout
detection (Heron/rt-detr), table-structure recognition, optional OCR (RapidOCR),
optional formula enrichment, and optional picture description — all **inline,
in-process**, with a clean exit. There are no persistent model servers, so there
is no VRAM-leak (this replaces the old marker/surya stack, whose detached surya
servers leaked GPU memory), and no vLLM/Docker prerequisite.

`postprocess.py` is unchanged: it consumes the raw markdown (with
`<span id="page-N-M">` page spans) + `meta.json` + `conversion.json`, and derives
chapter structure from the PDF's own embedded outline (`pypdfium2.get_toc()`).

## OCR

Docling auto-decides OCR from `inspect_pdf.py`'s profile:

- **Born-digital** (has a text layer, not scanned) → `do_ocr=False`. Docling
  extracts the text layer directly via docling-parse. Fast.
- **Scanned / no text layer** → `do_ocr=True` with **RapidOCR** (onnxruntime
  backend). RapidOCR is pip-native — no system tesseract required.

Flags:
- `--force-ocr` — force OCR on every page (overrides the auto decision).
- `--strip-existing-ocr` — discard embedded OCR text and re-OCR every page
  (maps to Docling's full-page OCR mode).
- `--lang <hint>` — OCR language hint passed to RapidOcrOptions (e.g. `en`).
- `--mode disable_ocr|fast|balanced` — **compat only** with the old marker CLI.
  `disable_ocr` forces OCR off; `fast`/`balanced` force OCR on. `auto` (default)
  inspects the PDF. `balanced` does **not** auto-enable the Ollama VLM.

## Ollama VLM (picture description) — `--use-ollama-vlm`

Opt-in. Routes each extracted figure to an OpenAI-compatible chat-completions
endpoint served by **Ollama** to generate a concise description (alt text /
caption), which flows into the figure manifest and the markdown image links.

Configuration (env vars, all optional):
- `OLLAMA_VLM_URL` — default `http://localhost:11434/v1/chat/completions`
  (Ollama on this host, kore; LAN alternative `http://192.168.1.33:11434/v1`).
- `OLLAMA_VLM_MODEL` — default `gemma4:31b-cloud`. Cloud vision models available
  on this host's Ollama (routed to `ollama.com`, no local GPU): `gemma4:31b-cloud`,
  `qwen3.5:397b-cloud`, `kimi-k3:cloud`, `mistral-large-3:675b-cloud`. For complex
  diagrams, prefer `qwen3.5:397b-cloud` or `kimi-k3:cloud` (accuracy priority).
- `OLLAMA_API_KEY` — sent as `Authorization: Bearer <key>` if set (the `:cloud`
  model tag is usually sufficient; the local Ollama daemon handles cloud auth).
- `OLLAMA_VLM_TIMEOUT` — default `120` (cloud models can be slow).
- `OLLAMA_VLM_CONCURRENCY` — default `2`.

Default is **off** so conversions work offline. Enable for figure-heavy books
where caption quality matters. The VLM is used **only for picture description** —
not for table structure (Docling's inline table model handles that) and not for
formula enrichment (see below).

## Formula / math

Docling's base layout model **detects formulas and emits LaTeX** (`$$..$$` block,
`$..$` inline) with `do_formula_enrichment=False` (the default). This covers most
prose/code books. `do_formula_enrichment=True` would run an **inline HuggingFace
VLM** to refine equation LaTeX — this is a local model download, **not**
Ollama-routable, and is left off by default. If math accuracy on a specific book
is poor, consider enabling it as a one-off (requires a code change; not exposed on
the CLI yet).

## Images

`generate_picture_images=True` with `images_scale=2.0` (accuracy priority).
Images are written to `raw/images/` and referenced by **relative** links
(`![Image](images/...)`) in the raw markdown — portable if the output dir moves.
`--disable-image-extraction` skips extraction (faster, smaller output).

## Page spans

`<span id="page-N-M">` page spans (N = 0-based PDF page index) are **always**
emitted by the page-span adapter (`docling_page_span.py`) — no `--paginate` flag
needed. These are what `postprocess.py` uses to align the PDF outline's chapters
to positions in the body.

## GPU / CPU

Docling runs torch inline for the layout/OCR/table models. `TORCH_DEVICE=cuda` is
auto-set if a GPU is available (this host has an RTX 4070 SUPER). RapidOCR runs on
onnxruntime (CPU-bound even when the layout model uses the GPU). **No vLLM, no
Docker, no surya, no `SURYA_INFERENCE_URL`, no leaked processes.**

## Risks

- **First-run model download (~2–3 GB)** to `~/.cache/docling/models` and the
  HuggingFace cache. One-time; the dedicated venv keeps it isolated.
- **Scanned books are slower** (OCR is CPU-bound). 142 hand-drawn pages took ~90s.
  Do a small spot check first for very large scanned books.
- **Ollama VLM** depends on the Ollama daemon running + cloud quota; off by default.
- **`--page-range` is not yet supported** by the Docling driver (accepted but
  warns + converts the full document). Docling has no native page-range option.
- **`meta.json.table_of_contents` is empty** by default; `postprocess.py` uses the
  PDF outline (via `conversion.json.source_pdf`) as the primary chapter source, so
  this is fine. The fallback path is rarely hit.

````
"#,
    ),
    (
        "pdf-to-markdown-license.md",
        r#"---
type: Skill
title: pdf-to-markdown license
description: 'The proprietary license (LICENSE.txt) governing the pdf-to-markdown skill bundle: personal/internal use on Moon-Dragon hosts; redistribution or commercial use needs written permission.'
tags:
- pdf-to-markdown
- license
timestamp: 2026-09-21T00:00:00Z
---

# pdf-to-markdown license

The proprietary license shipped with the [pdf-to-markdown](/pdf-to-markdown.md) skill bundle (`LICENSE.txt`; no trailing newline in the source). Save the fenced content as `LICENSE.txt` next to the skill.

````
Proprietary License

Copyright (c) 2026 Moon-Dragon. All rights reserved.

This skill bundle (the "Software") is licensed for use by the authorized
user on Moon-Dragon hosts. You may use, copy, and modify the Software for
personal and internal purposes. Redistribution to third parties, public
disclosure, or commercial use requires express written permission from the
copyright holder.

The Software is provided "AS IS", without warranty of any kind, express or
implied. In no event shall the authors be liable for any claim, damages, or
other liability arising from the use of the Software.

````
"#,
    ),
];

/// The packaged skill concepts in the nested layout: (repo-relative
/// path, full OKF file contents), embedded at compile time. Shelf paths
/// are `/<repo-relative path>`. The hub `skill.md` carries the bundle
/// manifest (file roles + md5s) in its frontmatter.
pub const PACKAGED_CONCEPTS: &[(&str, &str)] = &[
    (
        "pdf-to-markdown/skill.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown/skill.md"),
    ),
    (
        "pdf-to-markdown/conventions.md",
        include_str!("../packaged-skills/pdf-to-markdown/pdf-to-markdown/conventions.md"),
    ),
    (
        "pdf-to-markdown/references/docling-options.md",
        include_str!(
            "../packaged-skills/pdf-to-markdown/pdf-to-markdown/references/docling-options.md"
        ),
    ),
];

/// The packaged skill's raw script payload files — real bytes, never
/// concepts: (path relative to the skill dir, bytes). Paths match the
/// hub manifest's `skill.files[].path` entries exactly; `asset_tests`
/// pins each payload's md5 against the manifest.
pub const PACKAGED_SCRIPTS_PAYLOADS: &[(&str, &[u8])] = &[
    (
        "LICENSE.txt",
        include_bytes!("../packaged-skills/pdf-to-markdown/pdf-to-markdown/LICENSE.txt"),
    ),
    (
        "scripts/convert.py",
        include_bytes!("../packaged-skills/pdf-to-markdown/pdf-to-markdown/scripts/convert.py"),
    ),
    (
        "scripts/postprocess.py",
        include_bytes!("../packaged-skills/pdf-to-markdown/pdf-to-markdown/scripts/postprocess.py"),
    ),
    (
        "scripts/docling_page_span.py",
        include_bytes!(
            "../packaged-skills/pdf-to-markdown/pdf-to-markdown/scripts/docling_page_span.py"
        ),
    ),
    (
        "scripts/html_cleanup.py",
        include_bytes!(
            "../packaged-skills/pdf-to-markdown/pdf-to-markdown/scripts/html_cleanup.py"
        ),
    ),
    (
        "scripts/inspect_pdf.py",
        include_bytes!("../packaged-skills/pdf-to-markdown/pdf-to-markdown/scripts/inspect_pdf.py"),
    ),
    (
        "scripts/setup_venv.sh",
        include_bytes!("../packaged-skills/pdf-to-markdown/pdf-to-markdown/scripts/setup_venv.sh"),
    ),
    (
        "scripts/requirements.txt",
        include_bytes!(
            "../packaged-skills/pdf-to-markdown/pdf-to-markdown/scripts/requirements.txt"
        ),
    ),
];

/// Seed the packaged skills into the global skills shelf. A missing or
/// stale `.seed-version` marker re-puts every packaged concept (one
/// batched write, one index.md regeneration) and writes the marker; a
/// matching marker is a no-op. Extra admin-created skills are never
/// touched. Errors propagate (fail-fast, like `assets::scaffold_defaults`).
pub async fn seed_packaged_skills(
    store: &Store,
    service_key: &ServiceKey,
    skills_dir: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let marker = skills_dir.join(".seed-version");
    let current = std::fs::read_to_string(&marker).unwrap_or_default();
    if current.trim() == SKILLS_SEED_VERSION {
        return Ok(());
    }
    let mut concepts = Vec::with_capacity(PACKAGED_SKILLS.len());
    for (name, contents) in PACKAGED_SKILLS {
        concepts.push(Concept::parse(&format!("/{name}"), contents)?);
    }
    let cs = ConceptStore::for_service(store, service_key.clone(), skills_dir, "skills");
    cs.put_batch(&concepts).await?;
    std::fs::create_dir_all(skills_dir)?;
    std::fs::write(&marker, SKILLS_SEED_VERSION)?;
    Ok(())
}

#[cfg(test)]
mod asset_tests {
    use super::*;

    /// The embedded repo assets must match the hub manifest: every
    /// payload in `PACKAGED_SCRIPTS_PAYLOADS` md5s to its declared
    /// manifest value on the `skill.md` hub.
    #[test]
    fn embedded_payloads_match_hub_manifest_md5s() {
        use md5::{Digest, Md5};

        let (_, hub_md) = PACKAGED_CONCEPTS
            .iter()
            .find(|(p, _)| *p == "pdf-to-markdown/skill.md")
            .expect("hub present");
        let hub = mycelium_core::concept::Concept::parse("/pdf-to-markdown/skill.md", hub_md)
            .expect("hub parses");
        let man = hub
            .frontmatter
            .skill
            .as_ref()
            .expect("hub carries manifest");
        man.validated().expect("manifest paths valid");
        for f in &man.files {
            if f.md5.is_none() {
                continue;
            }
            let (_, bytes) = PACKAGED_SCRIPTS_PAYLOADS
                .iter()
                .find(|(p, _)| *p == f.path)
                .unwrap_or_else(|| panic!("payload {} present", f.path));
            let mut h = Md5::new();
            h.update(*bytes);
            let digest = hex::encode(h.finalize());
            assert_eq!(digest, f.md5.as_deref().unwrap(), "{}", f.path);
        }
    }

    /// Every packaged concept in the nested layout parses as a valid OKF
    /// concept (frontmatter + body) at its shelf path — catches a
    /// malformed companion at package time, not at seed time.
    #[test]
    fn all_packaged_concepts_parse() {
        for &(path, md) in PACKAGED_CONCEPTS {
            mycelium_core::concept::Concept::parse(&format!("/{path}"), md)
                .unwrap_or_else(|e| panic!("/{path} parses: {e}"));
        }
    }
}
