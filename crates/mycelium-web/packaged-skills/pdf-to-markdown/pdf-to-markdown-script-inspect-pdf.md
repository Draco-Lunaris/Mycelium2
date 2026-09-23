---
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
