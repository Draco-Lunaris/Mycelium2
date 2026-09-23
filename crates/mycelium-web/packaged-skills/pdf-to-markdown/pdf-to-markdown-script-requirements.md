---
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
