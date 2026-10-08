#!/usr/bin/env python3
"""
ebook-to-markdown skill — pre-conversion ebook profile + DRM gate.

Profiles an ebook from container metadata and record-0 header bytes ONLY:
epub via the zip container + OPF; mobi/azw3 via the PDB header and record 0
(PalmDOC header + MOBI header + EXTH — the EXTH lives inside record 0); fb2
via its leading bytes. No text records are ever decoded — all text
extraction belongs to calibre in convert.py. DRM-protected inputs are
detected and refused before any conversion can start; this skill never
circumvents DRM.

Prints exactly one JSON line on stdout; progress/diagnostics go to stderr.

On success:
    {"ok": true, "format": "epub"|"mobi"|"azw3"|"fb2", "title": str|null,
     "author": str|null, "language": str|null,
     "toc_kind": "nav"|"ncx"|"filepos"|"none",
     "spine_count": int, "image_count": int, "drm": false}
On DRM, unsupported format, missing file, or unreadable input:
    {"ok": false, "error": str} and exit code 1.

Record-0 offsets below are verified against real calibre 9.15.0 output and
calibre's own sources (src/calibre/ebooks/mobi/writer2/main.py record-0
layout comments; src/calibre/ebooks/mobi/debug/headers.py MOBIHeader).

Usage:
    python inspect_ebook.py --input /path/to/book.epub
"""

import argparse
import json
import struct
import sys
import xml.etree.ElementTree as ET
import zipfile
from pathlib import Path

# XML namespaces used by the epub container and OPF.
CONTAINER_NS = "urn:oasis:names:tc:opendocument:xmlns:container"
OPF_NS = "http://www.idpf.org/2007/opf"
DC_NS = "http://purl.org/dc/elements/1.1/"

# PDB header (absolute file offsets).
PDB_IDENT = 60          # 8 bytes: type (4) + creator (4)
PDB_NUM_RECORDS = 76    # u16 record count
PDB_RECORD_LIST = 78    # 8 bytes per record; u32 record offset is first

NULL_INDEX = 0xFFFFFFFF  # mobi's "no such record" sentinel everywhere

# Record-0 PalmDOC header (offsets absolute within record 0).
R0_COMPRESSION = 0        # u16: 1 none, 2 PalmDOC, 17480 HUFF/CDIC
R0_TEXT_RECORD_COUNT = 8  # u16: closest record-0 notion of "spine size"
R0_ENCRYPTION = 12        # u16: 0 none, 1 old Mobipocket, 2 Mobipocket
R0_MOBI_MAGIC = 16        # b"MOBI" when a MOBI header follows

# MOBI header fields (record-0 absolute offsets; magic sits at 16).
M_HEADER_LENGTH = 20    # u32
M_ENCODING = 28        # u32: 1252 cp1252, 65001 utf-8
M_FILE_VERSION = 36    # u32: 6 = KF7 mobi, >= 8 = KF8/azw3
M_FULLNAME_OFFSET = 84  # u32, offset of the full name within record 0
M_FULLNAME_LENGTH = 88  # u32
M_EXTH_FLAGS = 128      # u32: bit 0x40 -> EXTH header at 16 + header length
M_INDEX_RECORD = 244    # u32 primary index record ("NCX index"/ncxidx):
                        # the KF7 filepos TOC pointer; NULL_INDEX = no index

MOBI_ENCODINGS = {1252: "cp1252", 65001: "utf-8"}
EXTH_AUTHOR, EXTH_TITLE, EXTH_LANGUAGE = 100, 503, 524


def log(msg):
    print(msg, file=sys.stderr, flush=True)


class EbookError(Exception):
    """A refusal: the input must not (or cannot) be converted."""


def refuse_drm(fmt):
    raise EbookError(
        f"DRM-protected {fmt} book — refusing to convert "
        "(DRM is detected and refused, never circumvented)"
    )


def _first_text(root, tag):
    """Text of the first matching element, stripped; None when empty."""
    for el in root.iter(tag):
        text = (el.text or "").strip()
        return text or None
    return None


def profile_epub(path):
    """Profile an epub from its zip container: OPF metadata + manifest."""
    with zipfile.ZipFile(path) as z:
        names = set(z.namelist())
        if "META-INF/container.xml" not in names:
            raise EbookError("zip file has no META-INF/container.xml — not an epub")
        # DRM gate — checked before anything else in the container is read.
        if "META-INF/encryption.xml" in names:
            if b"EncryptedData" in z.read("META-INF/encryption.xml"):
                refuse_drm("epub")
        container = ET.fromstring(z.read("META-INF/container.xml"))
        opf_path = None
        rootfiles = container.find(f"{{{CONTAINER_NS}}}rootfiles")
        for rootfile in ([] if rootfiles is None
                         else rootfiles.findall(f"{{{CONTAINER_NS}}}rootfile")):
            opf_path = rootfile.get("full-path")
            break
        if not opf_path or opf_path not in names:
            raise EbookError("container.xml does not point at an OPF package")
        opf = ET.fromstring(z.read(opf_path))

    title = _first_text(opf, f"{{{DC_NS}}}title")
    author = _first_text(opf, f"{{{DC_NS}}}creator")
    language = _first_text(opf, f"{{{DC_NS}}}language")

    spine = opf.find(f"{{{OPF_NS}}}spine")
    spine_count = (len(spine.findall(f"{{{OPF_NS}}}itemref"))
                   if spine is not None else 0)

    toc_kind, image_count, has_ncx = "none", 0, False
    for item in opf.iter(f"{{{OPF_NS}}}item"):
        media_type = item.get("media-type", "")
        if media_type.startswith("image/"):
            image_count += 1
        if media_type == "application/x-dtbncx+xml":
            has_ncx = True
        if "nav" in (item.get("properties") or "").split():
            toc_kind = "nav"
    if toc_kind == "none" and has_ncx:
        toc_kind = "ncx"
    log(f"epub: toc={toc_kind} spine={spine_count} images={image_count}")

    return {
        "format": "epub", "title": title, "author": author,
        "language": language, "toc_kind": toc_kind,
        "spine_count": spine_count, "image_count": image_count, "drm": False,
    }


def _exth_metadata(rec0, exth_offset):
    """EXTH records 100/503/524 (author/updated title/language) from
    record 0. EXTH is part of record 0, so this stays header-only."""
    meta = {}
    if rec0[exth_offset:exth_offset + 4] != b"EXTH":
        return meta
    exth_length, count = struct.unpack_from(">II", rec0, exth_offset + 4)
    end = min(exth_offset + exth_length, len(rec0))
    (enc,) = struct.unpack_from(">I", rec0, M_ENCODING)
    codec = MOBI_ENCODINGS.get(enc, "cp1252")
    pos = exth_offset + 12
    for _ in range(count):
        if pos + 8 > end:
            break
        rec_type, rec_len = struct.unpack_from(">II", rec0, pos)
        if rec_len < 8 or pos + rec_len > end:
            break
        if rec_type in (EXTH_AUTHOR, EXTH_TITLE, EXTH_LANGUAGE):
            content = rec0[pos + 8:pos + rec_len]
            meta[rec_type] = content.decode(codec, "replace").strip()
        pos += rec_len
    return meta


def _fullname(rec0):
    """PDB full name, stored inside record 0 (title fallback)."""
    off, length = struct.unpack_from(">II", rec0, M_FULLNAME_OFFSET)
    if not length or off + length > len(rec0):
        return None
    (enc,) = struct.unpack_from(">I", rec0, M_ENCODING)
    codec = MOBI_ENCODINGS.get(enc, "cp1252")
    text = rec0[off:off + length].decode(codec, "replace").strip()
    return text or None


def profile_pdb(path, head):
    """Profile a mobi/azw3/PalmDOC book from PDB + record-0 header bytes.

    Reads only the PDB header (already in head), record-list entries 0 and
    1, and record 0 itself — never the text records.
    """
    ident = head[PDB_IDENT:PDB_IDENT + 8].upper()
    if ident not in (b"BOOKMOBI", b"TEXTREAD"):
        raise EbookError(f"unsupported PDB type/creator: {ident!r}")
    if len(head) < PDB_RECORD_LIST + 12:
        raise EbookError("truncated PDB header")
    (num_records,) = struct.unpack_from(">H", head, PDB_NUM_RECORDS)
    if num_records < 1:
        raise EbookError("corrupt PDB record list")
    # Record-list entry i is 8 bytes at 78 + 8*i: entry 0 holds record 0's
    # absolute file offset, entry 1 record 1's (record 0's extent).
    (rec0_off,) = struct.unpack_from(">I", head, PDB_RECORD_LIST)
    with open(path, "rb") as f:
        f.seek(0, 2)
        size = f.tell()
        if num_records > 1:
            (rec0_end,) = struct.unpack_from(">I", head, PDB_RECORD_LIST + 8)
        else:
            rec0_end = size
        if not rec0_off < rec0_end <= size:
            raise EbookError("corrupt PDB record offsets")
        f.seek(rec0_off)
        rec0 = f.read(rec0_end - rec0_off)
    if len(rec0) < 16:
        raise EbookError("truncated PalmDOC header in record 0")

    (compression,) = struct.unpack_from(">H", rec0, R0_COMPRESSION)
    (encryption,) = struct.unpack_from(">H", rec0, R0_ENCRYPTION)
    (text_records,) = struct.unpack_from(">H", rec0, R0_TEXT_RECORD_COUNT)
    log(f"palmDOC: compression={compression} encryption={encryption} "
        f"text_records={text_records}")

    fmt, title, author, language, toc_kind = "mobi", None, None, None, "none"
    has_mobi = rec0[R0_MOBI_MAGIC:R0_MOBI_MAGIC + 4] == b"MOBI"
    if has_mobi:
        # Bounds fact for the fixed-offset reads below: a crafted record 0
        # can carry the magic yet be truncated, so refuse rather than let
        # the reads fail. rec0 must reach the file version @36, cover the
        # EXTH flags @128 (largest fixed field read) and the whole declared
        # header (16 + header_len); the ncxidx @244 guard and the EXTH and
        # fullname readers check their own extents.
        if len(rec0) < M_FILE_VERSION + 4:
            raise EbookError("truncated MOBI header in record 0")
        (header_len,) = struct.unpack_from(">I", rec0, M_HEADER_LENGTH)
        (file_version,) = struct.unpack_from(">I", rec0, M_FILE_VERSION)
        if (len(rec0) < M_EXTH_FLAGS + 4
                or len(rec0) < R0_MOBI_MAGIC + header_len):
            raise EbookError("truncated MOBI header in record 0")
        if file_version >= 8:
            fmt = "azw3"
    # DRM gate — refuse once the dialect is known, before anything else in
    # record 0 is profiled.
    if encryption != 0:
        refuse_drm(fmt)
    if has_mobi:
        (exth_flags,) = struct.unpack_from(">I", rec0, M_EXTH_FLAGS)
        if exth_flags & 0x40:
            # EXTH sits at record-0 offset 16 + header_len (calibre's own
            # reader: EXTHHeader(raw[16 + self.length])).
            exth = _exth_metadata(rec0, R0_MOBI_MAGIC + header_len)
        else:
            exth = {}
        # EXTH 503 is the definitive title (calibre's own reader rule);
        # the PDB full name is the pre-EXTH fallback.
        title = exth.get(EXTH_TITLE) or _fullname(rec0)
        author = exth.get(EXTH_AUTHOR)
        language = exth.get(EXTH_LANGUAGE)
        # KF7 filepos TOC: the primary index record pointer. calibre writes
        # NULL_INDEX when the book has no index, and the pointer is never
        # record 0, so a real filepos TOC exists iff 0 < ncxidx < NULL_INDEX.
        ncxidx = NULL_INDEX
        # The field exists only in MOBI headers long enough to hold it
        # (calibre writes 0xE8 for KF7; ancient shorter headers lack it).
        if (len(rec0) >= M_INDEX_RECORD + 4
                and header_len >= M_INDEX_RECORD - R0_MOBI_MAGIC):
            (ncxidx,) = struct.unpack_from(">I", rec0, M_INDEX_RECORD)
        if 0 < ncxidx < NULL_INDEX:
            toc_kind = "filepos"
        log(f"mobi: file_version={file_version} index_record={ncxidx} "
            f"toc={toc_kind}")
    return {
        "format": fmt, "title": title, "author": author,
        "language": language, "toc_kind": toc_kind,
        # A mobi has no spine; the PalmDOC text record count is the closest
        # record-0 notion. Record 0 carries no image count (counting images
        # would require reading non-header records, which the probe never
        # does); convert.py reports the real count from the normalized epub.
        "spine_count": text_records, "image_count": 0, "drm": False,
    }


def profile_fb2():
    """fb2: detected from leading bytes; metadata comes from the calibre
    normalization in convert.py, not from the probe's 1 KB sniff."""
    log("fb2: format detected; metadata comes from calibre normalization")
    return {
        "format": "fb2", "title": None, "author": None, "language": None,
        "toc_kind": "none", "spine_count": 0, "image_count": 0, "drm": False,
    }


def _looks_like_fb2(head):
    if head.startswith(b"\xef\xbb\xbf"):
        head = head[3:]
    return head[:1] == b"<" and b"<FictionBook" in head


def _inspect(path):
    """Profile one ebook; returns the success profile or raises EbookError."""
    if not path.is_file():
        raise EbookError(f"file not found: {path}")
    with open(path, "rb") as f:
        head = f.read(1024)
    if head[:2] == b"PK":
        return profile_epub(path)
    ident = head[PDB_IDENT:PDB_IDENT + 8].upper()
    if ident in (b"BOOKMOBI", b"TEXTREAD"):
        return profile_pdb(path, head)
    if _looks_like_fb2(head):
        return profile_fb2()
    raise EbookError(
        f"unsupported format (not epub/mobi/azw3/fb2): {path.name}")


def inspect(path):
    """JSON-contract entry point (also imported by convert.py).

    Returns {"ok": true, ...profile} on success or {"ok": false, "error"}
    on any refusal — never raises for expected refusals.
    """
    try:
        return {"ok": True, **_inspect(Path(path).expanduser())}
    except EbookError as e:
        log(f"refused: {e}")
        return {"ok": False, "error": str(e)}
    except Exception as e:  # corrupt/unreadable input: refuse, never crash
        log(f"unreadable input: {e!r}")
        return {"ok": False, "error": f"unreadable input: {e}"}


def main():
    ap = argparse.ArgumentParser(
        description="Profile an ebook (epub/mobi/azw3/fb2) and gate on DRM.")
    ap.add_argument("--input", required=True, help="Path to the ebook.")
    args = ap.parse_args()

    log(f"Inspecting {args.input} ...")
    result = inspect(args.input)
    print(json.dumps(result, ensure_ascii=False))
    return 0 if result["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
