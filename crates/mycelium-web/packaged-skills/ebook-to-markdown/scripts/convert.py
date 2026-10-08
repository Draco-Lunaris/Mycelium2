#!/usr/bin/env python3
"""
ebook-to-markdown — calibre normalize + epub unpack (pipeline stage 2).

The stage-1 probe (inspect_ebook.inspect, imported from this directory)
gates first: DRM or unknown inputs are refused before any conversion
starts. Surviving inputs are normalized by calibre's ebook-convert to a
calibre-produced EPUB — spine order, metadata and a container TOC are
preserved for every input dialect — and unpacked to
<output-dir>/<slug>/raw/:

    book.epub        the normalized epub (kept as provenance)
    index.html       spine-ordered XHTML concatenation, files joined by
                     "<!-- spine: <i> <href> -->" markers (postprocess
                     splits on these)
    images/          manifest image/* members, flattened basenames
                     (collision -> "-2" suffix + warning)
    spine.json       [{"href", "title"}] — readable survivors only
                     (title = <title> else first <h1>-<h6>, else "")
    toc.json         [{"nav_label", "content_href", "spine_index", "level"}]
                     — nav epub:type="toc" preferred, else NCX navPoints
                     (nesting depth -> level); content outside the spine
                     -> spine_index -1
    conversion.json  {"source", "sha256", "calibre_version",
                     "elapsed_seconds", "warnings", "metadata": {"title",
                     "author", "language"}} — from the normalized epub's
                     OPF, never from the input

Slug: --book-slug wins (non-empty, no path separators); else the slugified
filename stem (lowercase, non-alphanumeric -> "-", runs collapsed, max 64)
— metadata is never consulted. Exactly one JSON line on stdout; all
diagnostics go to stderr. Success: {"ok": true, "slug", "raw_dir",
"epub_path", "conversion_json", "spine_count", "toc_entries"}, exit 0;
refusal: {"ok": false, "error": str}, exit 1.

calibre 9.15.0 facts (empirically verified): ebook-convert logs
diagnostics to STDOUT (stderr carries only failure tracebacks), so both
streams are captured; output is always EPUB 2 with a generated NCX, a
titlepage spine file and a default cover; absent dc:title/dc:creator
become "Unknown" in the normalized OPF.
"""

import argparse
import hashlib
import json
import os
import posixpath
import re
import shutil
import subprocess
import sys
import tempfile
import time
import traceback
import xml.etree.ElementTree as ET
import zipfile
from html import unescape
from pathlib import Path
from urllib.parse import unquote

# convert.py lives next to inspect_ebook.py; make that importable however
# this file is invoked.
sys.path.insert(0, str(Path(__file__).resolve().parent))
import inspect_ebook  # noqa: E402  (stage-1 probe: DRM/format gate)

DC_FIELDS = (("title", "title"), ("creator", "author"),
             ("language", "language"))  # (dc: element, metadata key)
EPUB_TYPE_ATTR = "{http://www.idpf.org/2007/ops}type"
HEADING_TAGS = ("h1", "h2", "h3", "h4", "h5", "h6")
NCX_MEDIA_TYPE = "application/x-dtbncx+xml"

SLUG_MAX = 64        # pdf-to-markdown slug rule cap
CALIBRE_TAIL = 2000  # chars of calibre output kept in a failure error
CALIBRE_TIMEOUT = 600  # seconds — Task 6's per-book corpus budget


class ConvertError(Exception):
    """A refusal: the input must not (or cannot) be converted."""


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def emit(result, code):
    print(json.dumps(result, ensure_ascii=False))
    return code


def slugify(text):
    """Filename stem -> slug: lowercase, non-alphanumeric -> "-", runs
    collapsed, edges stripped, max 64 chars (the pdf-to-markdown rule)."""
    text = "".join(c if c.isalnum() else "-"
                   for c in (text or "").strip().lower())
    text = re.sub(r"-{2,}", "-", text).strip("-")
    return text[:SLUG_MAX].rstrip("-")


def _calibre_binary():
    return os.environ.get("EBOOK2MD_CALIBRE") or str(
        Path.home() / "calibre-bin" / "ebook-convert")


def _warn(warnings, msg):
    log(f"warning: {msg}")
    warnings.append(msg)


def _xml_root(data):
    """ElementTree root of XML *bytes* (UTF-8 BOM tolerated). Bytes, never
    str: ET rejects str carrying an <?xml encoding?> declaration."""
    if data.startswith(b"\xef\xbb\xbf"):
        data = data[3:]
    return ET.fromstring(data)


def _local(tag):
    """Lowercased local name of an ElementTree tag ('{ns}navMap' ->
    'navmap'); '' for comments/PIs. All comparisons against it use
    lowercase literals."""
    return tag.rsplit("}", 1)[-1].lower() if isinstance(tag, str) else ""


def _resolve(opf_dir, href):
    """href relative to the OPF -> zip member path, or None. Percent-decoded,
    fragment-stripped, normalized against the OPF's directory."""
    href = (href or "").strip()
    if not href or "://" in href:
        return None
    href = unquote(href.split("#", 1)[0])
    if not href:
        return None
    return posixpath.normpath(posixpath.join(opf_dir, href))


def _first_text(root, *tags):
    """First non-empty itertext of the first element matching by local
    name (document order), or None."""
    for el in root.iter():
        if _local(el.tag) in tags:
            text = "".join(el.itertext()).strip()
            if text:
                return text
    return None


def _regex_title(text):
    """Regex fallback for spine titles when the file is not well-formed
    XML (content is still kept; only the title is at stake)."""
    for pattern in (r"<title[^>]*>(.*?)</title>",
                    r"<h[1-6][^>]*>(.*?)</h[1-6]>"):
        m = re.search(pattern, text, re.I | re.S)
        if m:
            t = unescape(re.sub(r"<[^>]+>", "", m.group(1))).strip()
            if t:
                return t
    return ""


def _file_title(text):
    """Per-spine title: <title> else first <h1>-<h6>, else ""."""
    try:
        return _first_text(_xml_root(text.encode("utf-8")),
                           "title", *HEADING_TAGS) or ""
    except ET.ParseError:
        return _regex_title(text)


def _nav_toc(z, manifest, opf_dir, names):
    """TOC entries from the EPUB3 nav (manifest item with properties="nav",
    <nav epub:type="toc">). Preferred source; [] when there is none —
    calibre 9.15.0 always emits EPUB 2, so production normally falls to
    the NCX. Returns [(label, href, level)]."""
    href = None
    for _iid, (mhref, _mtype, props) in manifest.items():
        if "nav" in props:
            href = mhref
            break
    member = _resolve(opf_dir, href) if href else None
    if not member or member not in names:
        return []
    try:
        root = _xml_root(z.read(member))
    except (ET.ParseError, zipfile.BadZipFile):
        return []
    nav = None
    for el in root.iter():
        if (_local(el.tag) == "nav"
                and (el.get(EPUB_TYPE_ATTR) == "toc"
                     or el.get("epub:type") == "toc")):
            nav = el
            break
    if nav is None:
        return []
    entries = []
    _walk_nav(nav, 0, entries)
    return entries


def _walk_nav(parent, level, out):
    """EPUB3 nav: each direct <ol> is a level; each <li> with a direct
    <a href> child is an entry; a nested <ol> inside an <li> is level+1."""
    for ol in parent:
        if _local(ol.tag) != "ol":
            continue
        for li in ol:
            if _local(li.tag) != "li":
                continue
            for child in li:  # direct children only: no nested-list links
                if _local(child.tag) == "a" and child.get("href"):
                    out.append(("".join(child.itertext()).strip() or "",
                                child.get("href"), level))
                    break
            _walk_nav(li, level + 1, out)


def _ncx_toc(z, manifest, opf_dir, names, spine_toc_id):
    """TOC entries from the NCX navMap (spine@toc idref preferred, else the
    manifest's NCX). navPoint nesting depth -> level. [(label, href, level)]."""
    href = None
    if spine_toc_id and spine_toc_id in manifest:
        href = manifest[spine_toc_id][0]
    if not href:
        for _iid, (mhref, mtype, _props) in manifest.items():
            if mtype == NCX_MEDIA_TYPE:
                href = mhref
                break
    member = _resolve(opf_dir, href) if href else None
    if not member or member not in names:
        return []
    try:
        root = _xml_root(z.read(member))
    except (ET.ParseError, zipfile.BadZipFile):
        return []
    nav_map = next((el for el in root.iter()
                    if _local(el.tag) == "navmap"), None)
    if nav_map is None:
        return []
    out = []
    for np in nav_map:
        if _local(np.tag) == "navpoint":
            _walk_ncx(np, 0, out)
    return out


def _walk_ncx(nav_point, level, out):
    label, src = "", None
    for child in nav_point:  # direct children only: no nested-point bleed
        lt = _local(child.tag)
        if lt == "navlabel":
            for el in child.iter():
                if _local(el.tag) == "text":
                    label = "".join(el.itertext()).strip()
                    break
        elif lt == "content":
            src = child.get("src")
    out.append((label, src, level))
    for child in nav_point:
        if _local(child.tag) == "navpoint":
            _walk_ncx(child, level + 1, out)


def _unpack(epub_path, raw_dir, warnings):
    """Unpack the normalized epub into raw_dir (already holds book.epub).
    Returns (spine, toc, metadata, calibre_version)."""
    try:
        zf = zipfile.ZipFile(epub_path)
    except zipfile.BadZipFile as e:
        raise ConvertError(f"normalized epub is not a readable zip: {e}")
    with zf as z:
        names = set(z.namelist())
        try:
            container = _xml_root(z.read("META-INF/container.xml"))
        except (KeyError, ET.ParseError, zipfile.BadZipFile):
            raise ConvertError(
                "normalized epub: no readable META-INF/container.xml")
        opf_path = next((el.get("full-path") for el in container.iter()
                         if _local(el.tag) == "rootfile"
                         and el.get("full-path")), None)
        if not opf_path:
            raise ConvertError("normalized epub: container.xml names no OPF")
        opf_path = posixpath.normpath(unquote(opf_path))
        if opf_path not in names:
            raise ConvertError("normalized epub: OPF missing from the zip")
        try:
            opf = _xml_root(z.read(opf_path))
        except (ET.ParseError, zipfile.BadZipFile):
            raise ConvertError(f"normalized epub: unreadable OPF {opf_path!r}")
        opf_dir = posixpath.dirname(opf_path)

        # Manifest: id -> (href, media-type, properties set)
        manifest = {}
        for el in opf.iter():
            if _local(el.tag) == "item" and el.get("id"):
                manifest[el.get("id")] = (el.get("href"),
                                          el.get("media-type", ""),
                                          set((el.get("properties")
                                               or "").split()))
        spine_el = next((el for el in opf.iter()
                         if _local(el.tag) == "spine"), None)
        if spine_el is None:
            raise ConvertError("normalized epub: OPF has no spine")
        spine_toc_id = spine_el.get("toc")
        idrefs = [ir.get("idref") for ir in spine_el
                  if _local(ir.tag) == "itemref"]

        # Spine: resolve each itemref to a readable member. Fail-soft:
        # unreadable entries are dropped with a warning; spine.json and the
        # index.html markers list only the survivors (marker i == the i-th
        # survivor — the numbering postprocess.py aligns against).
        spine, texts = [], []
        for n, idref in enumerate(idrefs):
            if not idref or idref not in manifest:
                _warn(warnings, f"spine itemref {n}: no manifest item "
                                f"{idref!r} — dropped")
                continue
            member = _resolve(opf_dir, manifest[idref][0])
            if not member or member not in names:
                _warn(warnings, f"spine itemref {n}: {idref!r} target not "
                                f"in the zip — dropped")
                continue
            try:
                text = z.read(member).decode("utf-8", "replace")
            except zipfile.BadZipFile as e:
                _warn(warnings, f"spine file {member}: unreadable ({e}) — "
                                f"dropped")
                continue
            if not text.strip():
                _warn(warnings, f"spine file {member}: empty — dropped")
                continue
            spine.append({"href": member, "title": _file_title(text)})
            texts.append(text)
        if not spine:
            raise ConvertError("normalized epub: no readable spine files")

        with open(raw_dir / "index.html", "w", encoding="utf-8") as f:
            for i, (entry, text) in enumerate(zip(spine, texts)):
                f.write(f"<!-- spine: {i} {entry['href']} -->\n")
                f.write(text if text.endswith("\n") else text + "\n")

        # images/: every manifest image/* member, flattened basenames;
        # collision -> "-2" suffix (+ "-3", ...) with a warning.
        images_dir = raw_dir / "images"
        images_dir.mkdir(exist_ok=True)
        used = set()
        for iid, (mhref, mtype, _props) in manifest.items():
            if not (mtype or "").startswith("image/"):
                continue
            member = _resolve(opf_dir, mhref)
            if not member or member not in names:
                _warn(warnings, f"image {iid}: target not in the zip — "
                                f"skipped")
                continue
            try:
                data = z.read(member)
            except zipfile.BadZipFile as e:
                _warn(warnings, f"image {member}: unreadable ({e}) — skipped")
                continue
            base = posixpath.basename(member)
            if not base:
                _warn(warnings, f"image {iid}: no usable filename — skipped")
                continue
            stem, ext = posixpath.splitext(base)
            candidate, n = base, 2
            while candidate in used:
                candidate = f"{stem}-{n}{ext}"
                n += 1
            if candidate != base:
                _warn(warnings, f"image name collision: {base} -> {candidate}")
            used.add(candidate)
            (images_dir / candidate).write_bytes(data)

        # toc.json: nav preferred, else NCX navPoints.
        entries = _nav_toc(z, manifest, opf_dir, names)
        if not entries:
            entries = _ncx_toc(z, manifest, opf_dir, names, spine_toc_id)
        index_of = {}
        for i, entry in enumerate(spine):
            index_of.setdefault(entry["href"], i)
        toc = []
        for label, href, level in entries:
            member = _resolve(opf_dir, href)
            toc.append({
                "nav_label": label or "",
                "content_href": member if member else (href or ""),
                "spine_index": index_of.get(member, -1) if member else -1,
                "level": level,
            })

        metadata = {key: _first_text(opf, dc) for dc, key in DC_FIELDS}
        calibre_version = "unknown"
        for el in opf.iter():
            if _local(el.tag) == "contributor":
                m = re.search(r"calibre \(([^)]+)\)", "".join(el.itertext()))
                if m:
                    calibre_version = m.group(1)
                    break
    return spine, toc, metadata, calibre_version


def _run(args):
    input_path = Path(args.input).expanduser()
    out_root = Path(args.output_dir).expanduser()

    # Stage-1 gate: profile + DRM/unknown refusal BEFORE anything is written.
    profile = inspect_ebook.inspect(str(input_path))
    if not profile.get("ok"):
        error = str(profile.get("error", "refused by the inspection probe"))
        log(f"refused by probe: {error}")
        return emit({"ok": False, "error": error}, 1)
    log(f"probe ok: format={profile['format']} "
        f"toc={profile.get('toc_kind')}")

    # Slug: --book-slug wins; else the filename stem. Metadata is never
    # consulted.
    if args.book_slug is not None:
        slug = args.book_slug.strip()
        if not slug or "/" in slug or "\\" in slug or slug in (".", ".."):
            raise ConvertError(f"invalid --book-slug: {args.book_slug!r}")
    else:
        slug = slugify(input_path.stem)
        if not slug:
            raise ConvertError(
                f"cannot derive a book slug from filename "
                f"{input_path.name!r} — pass --book-slug")

    binary = _calibre_binary()
    if not (os.path.isfile(binary) and os.access(binary, os.X_OK)):
        raise ConvertError(
            f"calibre binary not found or not executable: {binary}")

    with open(input_path, "rb") as f:
        sha = hashlib.file_digest(f, "sha256").hexdigest()

    with tempfile.TemporaryDirectory(prefix="ebook2md-") as tmp:
        tmp_epub = Path(tmp) / "book.epub"
        log(f"calibre: {input_path} -> {tmp_epub}")
        t0 = time.monotonic()
        try:
            proc = subprocess.run([binary, str(input_path), str(tmp_epub)],
                                  capture_output=True,
                                  timeout=CALIBRE_TIMEOUT)
        except subprocess.TimeoutExpired:
            # subprocess.run kills and reaps the child before re-raising.
            raise ConvertError(
                f"calibre conversion timed out after {CALIBRE_TIMEOUT} s")
        elapsed = time.monotonic() - t0
        out_text = proc.stdout.decode("utf-8", "replace")
        err_text = proc.stderr.decode("utf-8", "replace")
        # calibre 9.15.0 logs diagnostics to stdout; stderr carries only
        # tracebacks on failure — capture both, neither reaches our stdout.
        warnings = [l for l in out_text.splitlines() if l.strip()]
        warnings += [l for l in err_text.splitlines() if l.strip()]
        if proc.returncode != 0 or not tmp_epub.is_file():
            tail = (out_text + "\n" + err_text)[-CALIBRE_TAIL:].strip()
            raise ConvertError(f"calibre conversion failed: {tail}")
        raw_dir = out_root / slug / "raw"
        raw_dir.mkdir(parents=True, exist_ok=True)
        shutil.move(str(tmp_epub), str(raw_dir / "book.epub"))

    spine, toc, metadata, calibre_version = _unpack(
        raw_dir / "book.epub", raw_dir, warnings)

    conversion = {
        "source": str(input_path),
        "sha256": sha,
        "calibre_version": calibre_version,
        "elapsed_seconds": round(elapsed, 3),
        "warnings": warnings,
        "metadata": metadata,
    }
    for name, payload in (("spine.json", spine), ("toc.json", toc),
                          ("conversion.json", conversion)):
        (raw_dir / name).write_text(
            json.dumps(payload, ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8")

    log(f"unpacked: {len(spine)} spine files, {len(toc)} toc entries")
    return emit({
        "ok": True,
        "slug": slug,
        "raw_dir": str(raw_dir),
        "epub_path": str(raw_dir / "book.epub"),
        "conversion_json": str(raw_dir / "conversion.json"),
        "spine_count": len(spine),
        "toc_entries": len(toc),
    }, 0)


def main():
    ap = argparse.ArgumentParser(
        description="Normalize an ebook via calibre and unpack the epub.")
    ap.add_argument("--input", required=True, help="Path to the ebook.")
    ap.add_argument("--output-dir", required=True,
                    help="Directory that receives <slug>/raw/.")
    ap.add_argument("--book-slug", default=None,
                    help="Output slug (default: slugified filename stem; "
                         "metadata is never consulted).")
    args = ap.parse_args()

    try:
        return _run(args)
    except ConvertError as e:
        log(f"refused: {e}")
        return emit({"ok": False, "error": str(e)}, 1)
    except Exception as e:  # never crash without the JSON contract
        log(f"unexpected failure: {e!r}")
        log(traceback.format_exc())
        return emit({"ok": False, "error": f"conversion failed: {e}"}, 1)


if __name__ == "__main__":
    sys.exit(main())