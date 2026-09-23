---
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
