---
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
