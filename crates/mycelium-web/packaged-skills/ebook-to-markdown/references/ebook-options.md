---
type: Skill
title: ebook-to-markdown ebook options
description: 'Reference doc for the ebook-to-markdown skill''s calibre engine: the isolated no-root install, the headless GL libraries, the no-text probe, the DRM gate, the normalize-to-epub conversion, invocation facts, corpus-derived facts and risks — read before running convert.py on mobi/azw3/fb2 inputs, DRM-uncertain books or headless hosts.'
tags:
- ebook-to-markdown
- calibre
- drm
- normalization
- reference
timestamp: 2026-10-02T00:00:00Z
---

# ebook-options.md

When to read this: before running `convert.py` on mobi/azw3/fb2 inputs, on
DRM-uncertain books, or on headless hosts — and whenever calibre's install,
version, or invocation comes up.

## The engine — calibre, driven as an external binary

The skill uses **calibre** (`ebook-convert`) as its single conversion engine,
exactly the way the pdf-to-markdown skill uses Docling: a heavy, battle-tested
external converter driven by light deterministic scripts. calibre's readers
handle decades of real-world ebook mess — every mobi dialect (KF7, KF8/azw3,
HUFF/CDIC), epub2/epub3, fb2 — so this skill never decodes book text itself.

Install facts:

- The venv does **not** contain calibre. `~/.venvs/ebook-to-markdown/` holds
  only `lxml` + `PyYAML` (uv-managed Python 3.12; 3.12.14 at validation
  time); calibre installs **isolated, no-root** into `~/calibre-bin/` via its
  official `linux-installer.sh` (x86_64 and aarch64; GLIBC 2.34+).
- `convert.py` execs the binary at `~/calibre-bin/ebook-convert`; override
  with the `EBOOK2MD_CALIBRE` env var (`EBOOK2MD_VENV` and
  `EBOOK2MD_CALIBRE_DIR` steer the setup script).
- Every fact in this file was verified against **calibre 9.15.0**.

## Headless hosts: the GL libraries

calibre fails to start on headless Linux hosts missing its OpenGL libraries —
`ebook-convert` dies at startup with a GL/EGL error before any conversion work
starts. Fix: `sudo apt install -y libegl1 libopengl0` (calibre's documented
requirement). This is the one environment failure that looks like a calibre
bug but is a missing package.

## The probe reads no text

`inspect_ebook.py` profiles the book from container metadata and header bytes
ONLY: epub via the zip container + OPF; mobi/azw3 via the PDB header and
record 0 (PalmDOC + MOBI header + EXTH — the EXTH lives inside record 0); fb2
via its leading bytes. No text records are ever decoded — all text extraction
belongs to calibre. Record-0 offsets are verified against real calibre 9.15.0
output and calibre's own sources. For mobi/azw3 the probe reports
`spine_count` as the record-0 text-record count and `image_count: 0`; the
normalized epub carries the real structure.

## DRM — detected, refused, never circumvented

- epub: `META-INF/encryption.xml` containing `EncryptedData` → refused.
- mobi/azw3: record-0 encryption field ≠ 0 → refused.

The refusal happens in the probe, before any conversion can start, and
`convert.py` re-runs the same gate before invoking calibre. There is no
override and no circumvention anywhere in this skill — a hard limit, by
design. Pinned by synthetic fixtures (epub encryption.xml; the mobi encryption
flag); the 231-book corpus contained zero DRM books.

## Normalize to epub — the one conversion

`convert.py` runs exactly:

```
ebook-convert <input> <tmp>/book.epub
```

bare — no profile or tuning flags; calibre's defaults apply. Empirically
(9.15.0), the output is always EPUB 2 with a generated NCX and an ensured
cover: calibre generates a titlepage + default cover when the source has
neither, and a source with its own cover/title file keeps it (103/231 corpus
books carry a titlepage member). Absent `dc:title`/`dc:creator` become the
literal "Unknown" in the normalized OPF (postprocess treats that sentinel
as absent metadata).

Why normalize instead of converting straight to markdown: calibre's pipeline
is unified — mobi/azw3/fb2 → epub → HTML spine. Only the normalized form gives
every input dialect the same shape: spine order, metadata, and a container
TOC (nav, else NCX — mobi filepos TOCs survive *because* calibre rebuilds
them), which is what makes deterministic postprocessing possible. The
produced epub is kept as provenance (`raw/book.epub`) and unpacked by
`convert.py` itself: spine-joined `index.html` (`<!-- spine: i href -->`
markers), `spine.json`, `toc.json`, `conversion.json`, `images/`.

Invocation facts:

- **Timeout: 600 s.** Past it, `convert.py` kills calibre and reports a clean
  refusal ("calibre conversion timed out after 600 s"). Corpus reality:
  median 2.6 s per book — the timeout is a safety net for pathological
  inputs, not a tuning knob.
- **Both output streams are captured.** calibre 9.15.0 logs conversion
  diagnostics to STDOUT (stderr carries only failure tracebacks); neither
  stream reaches the script's own stdout, and every diagnostic line lands in
  `conversion.json.warnings`.
- **Diagnostics are noisy by design** — normal successful conversions emit
  many lines; keep them as warnings, don't chase them.

## Corpus-derived facts (231 real books, calibre 9.15.0)

Validated against a 231-book real corpus: 168 epub + 63 mobi, no azw3 or fb2
present (those dialects are covered by probe detection + calibre's readers,
not by corpus evidence; all 63 corpus mobis are KF7, file_version 6).

- 231/231 converted — 100% success, 715.7 s total (median 2.6 s/book;
  largest input a 293 MB epub, slowest book 20.1 s — the Ghidra Book epub;
  largest mobi 174 MB).
- TOC-alignment degradation is the dominant quality observation: nav/NCX
  labels vs body headings disagree constantly — 66,333 dropped-entry
  warnings corpus-wide; every book dropped ≥1 entry (per-book min 6,
  median 259, max 1,691). Dropped entries produce no chapter anchor; their
  content still flows into the book as `sec-N-M` sections.
- Corpus-wide chapter counts: min 1, median 6, max 155; 28/231 books end
  with `chapter_count ≤ 2`. Anchor uniqueness held in all 231 books; every
  book carried front-matter sections (min 1, median 5, max 228 — every book
  carries cover/title-page front matter: 103/231 normalized epubs include a
  titlepage member, the rest keep the source's own cover/title page).
- Tier falls: 5 falls in 4 books — three fell nav → headings, one fell
  nav → headings → spine. Spine-order books are designed fallback output,
  not failures.
- Container TOCs: 121 epubs with a nav, 47 with a legacy NCX only, all 63
  mobis with filepos TOCs (rebuilt as NCX by normalization) — every corpus
  book had a container TOC; the headings/spine tiers were reached only via
  falls.
- Metadata: calibre's "Unknown" sentinel appeared in real books (→
  `author: null`); one real title contained U+2029 (paragraph separator) —
  safe in the JSON contract only if consumers split stage output on "\n"
  (see the main doc's gotchas).
- Images: 45,854 `![Image](images/…)` links corpus-wide, zero dangling.
- DRM: zero DRM books in the corpus.

## Risks

- **calibre version drift**: the EPUB 2 / NCX / ensured-cover / "Unknown"
  facts above are empirical, verified on 9.15.0. A newer calibre may change
  them — re-validate against real books before trusting new behavior.
- **Pathological inputs**: the 600 s timeout turns a hung conversion into a
  clean refusal; a corrupt normalized epub (bad zip/OPF/spine) is refused as
  a ConvertError after `book.epub` has already been moved into `raw/` — that
  one file may remain as residue. No partial JSONs are written (they land
  only after a successful unpack); delete the directory and re-run.
- **GL on headless hosts** (above) is the one install failure that mimics a
  calibre bug.