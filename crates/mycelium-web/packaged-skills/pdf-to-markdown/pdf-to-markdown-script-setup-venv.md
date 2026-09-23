---
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
