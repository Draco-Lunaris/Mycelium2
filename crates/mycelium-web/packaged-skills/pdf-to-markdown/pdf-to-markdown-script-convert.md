---
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
