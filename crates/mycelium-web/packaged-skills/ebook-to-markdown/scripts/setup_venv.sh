#!/usr/bin/env bash
#
# ebook-to-markdown skill — one-time setup: Python venv + calibre.
#
# 1. uv provisions ~/.venvs/ebook-to-markdown (lxml + PyYAML).
# 2. calibre installs via its official isolated no-root installer into
#    ~/calibre-bin (x86_64 and aarch64; GLIBC 2.34+). calibre provides
#    ebook-convert — the conversion engine. On headless hosts install
#    libegl1/libopengl0 (calibre's documented OpenGL requirement) if
#    ebook-convert fails to start.
# Idempotent: re-running recreates nothing that already works.
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VENV="${EBOOK2MD_VENV:-$HOME/.venvs/ebook-to-markdown}"
CALIBRE_DIR="${EBOOK2MD_CALIBRE_DIR:-$HOME/calibre-bin}"

# 1. uv + venv
if ! command -v uv >/dev/null 2>&1; then
  echo "Installing uv ..."
  export UV_INSTALL_DIR="$HOME/.cargo/bin"
  curl -LsSf https://astral.sh/uv/install.sh | sh
fi
export PATH="$HOME/.cargo/bin:$PATH"
uv python install 3.12 >/dev/null 2>&1 || true
if [ ! -x "$VENV/bin/python" ]; then
  echo "Recreating venv at $VENV ..."
  rm -rf "$VENV"
  uv venv "$VENV" --python 3.12
else
  echo "Venv at $VENV is present; leaving it alone."
fi
uv pip install --python "$VENV" -r "$SCRIPT_DIR/requirements.txt"

# 2. calibre (isolated, no root)
CALIBRE_BIN="$CALIBRE_DIR/calibre/ebook-convert"
if [ ! -x "$CALIBRE_BIN" ]; then
  echo "Installing calibre (isolated) into $CALIBRE_DIR ..."
  mkdir -p "$CALIBRE_DIR"
  wget -nv -O- https://download.calibre-ebook.com/linux-installer.sh \
    | sh /dev/stdin "install_dir=$CALIBRE_DIR" isolated=y
fi
# Create convenience symlink at CALIBRE_DIR/ebook-convert if not present
if [ ! -x "$CALIBRE_DIR/ebook-convert" ]; then
  ln -sf "$CALIBRE_BIN" "$CALIBRE_DIR/ebook-convert"
fi
"$CALIBRE_DIR/ebook-convert" --version

echo
echo "Setup complete: venv=$VENV  calibre=$CALIBRE_DIR"
echo "Convert with:"
echo "  $VENV/bin/python $SCRIPT_DIR/convert.py --input book.epub --output-dir /out"
