#!/usr/bin/env python3
"""postprocess — raw/ unpack -> readable book (pipeline stage 4).

Turns convert.py's raw/ layout (index.html joined by "<!-- spine: i
href -->" markers, spine.json, toc.json, images/, conversion.json)
into the readable book next to --output's parent: book.md, images/
(copied from raw/images/), figures/manifest.json. Stdout: one JSON
line — {"ok": true, "readable_book", "figure_manifest",
"chapter_count", "front_matter_sections", "warnings"} exit 0; raw/
missing or malformed per the Task-3 contract -> {"ok": false,
"error"} exit 1, never a traceback; warnings go to stderr AND the
JSON array. Book: YAML frontmatter (title, author, language,
source_ebook, converted_date, converter "calibre <version>", mode:
ebook, chapter_count, toc[] of {id, title, level}), then "## Table
of Contents" (sections nested), then the body with pdf-to-markdown
anchors: chapters "# T {#ch-N-slug}", in-chapter h2s "## T {#sec-N-M-slug}",
pre-chapter front matter "## T {#sec-0-N-slug}" ("-2" id dedupe).

--toc-source forces the starting tier (auto = nav); a tier yielding no
chapters falls through. nav: level-0 toc entries -> chapters, N =
their 1-based level-0 ordinal (dropped ones leave gaps, never
renumbered), nav_label matched against headings at the entry's spine
boundary (normalized, exact then case-insensitive); nested
entries validate against their chapter's h2 sections. headings:
"#" scan, else "##" promoted. spine: one chapter per non-empty
segment (title = first heading else filename stem); empty segments
stay front matter — calibre's cover/titlepage. Metadata chains
("Unknown" — calibre's empty-metadata sentinel — counts as absent):
title = metadata -> source-filename stem -> slug directory ->
"Untitled"; author/language = metadata else null. Figure manifest:
{"book", "source_ebook", "figure_count", "figures": [{"alt", "src",
"page": null}]} — one entry per emitted ![Image](images/...) link outside code fences.
"""

import argparse
import json
import re
import shutil
import sys
import time
import traceback
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))
from ebook_html import HtmlError, cleanup, html_to_markdown  # noqa: E402

SLUG_MAX = 64
UNKNOWN = "Unknown"
FRONT_TITLE = "Front Matter"
MARKER_RE = re.compile(r"^<!-- spine: (\d+) (.*) -->\s*$")
HEADING_RE = re.compile(r"^(#{1,6}) (.+)$")
IMAGE_RE = re.compile(r"!\[([^\]]*)\]\(((?>(?:[^()]|\([^()]*\))+))\)")  # atomic: linear on '(+' runs
FENCE_LINE = re.compile(r"^[ \t]{0,3}(`{3,})[ \t]*$")
QUOTE_PREFIX = re.compile(r"^[ \t]{0,3}>[ \t]?")
LIST_MARKER = re.compile(r"^[ \t]{0,3}(?:[-*+]|\d{1,9}[.)])[ \t]")

TIERS = {"auto": ("nav", "headings", "spine"),
         "nav": ("nav", "headings", "spine"),
         "headings": ("headings", "spine"), "spine": ("spine",)}

_WARNINGS = []


class PostError(Exception):
    """A refusal: the raw/ layout does not meet the Task-3 contract."""


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def warn(msg):
    log(f"warning: {msg}")
    _WARNINGS.append(msg)


def emit(result, code):
    print(json.dumps(result, ensure_ascii=False))
    return code


def _clean_title(text):
    """Strip image markdown + emphasis/backticks (title matching/slugs)."""
    text = IMAGE_RE.sub("", text or "")
    text = re.sub(r"(\*\*|__|`)(.*?)\1", r"\2", text)
    return re.sub(r"[*_`]", "", text).strip()


def _norm(text):
    return re.sub(r"\s+", " ", _clean_title(text or "")).strip()


def _slugify(text):
    """pdf-to-markdown anchor slug rule; 'section' when empty."""
    text = re.sub(r"[^\w\s-]", "", _clean_title(text or "").strip().lower())
    text = re.sub(r"[\s_-]+", "-", text).strip("-")
    return text[:SLUG_MAX].rstrip("-") or "section"


def _unique_id(anchor, seen):
    """A8 dedupe: '-2', '-3', ... on id collisions."""
    n, cand = 1, anchor
    while cand in seen:
        n += 1
        cand = f"{anchor}-{n}"
    seen.add(cand)
    return cand


def _fence_run(line):
    """Backtick-run length of a fence line after quote/list prefixes are
    stripped (mirrors ebook_html's CommonMark fence rule)."""
    core = line
    while True:
        core = re.sub(r"^[ \t]+", "", core)
        m = QUOTE_PREFIX.match(core) or LIST_MARKER.match(core)
        if not m:
            break
        core = core[m.end():]
    m = FENCE_LINE.match(core)
    return len(m.group(1)) if m else 0


def _load_raw(raw_dir):
    """Validate the raw/ contract (A4): dir + required files, parseable.
    Returns (index_text, spine, toc, conv)."""
    raw = Path(raw_dir)
    if not raw.is_dir():
        raise PostError(f"raw directory not found: {raw_dir}")

    def read(name):
        p = raw / name
        if not p.is_file():
            raise PostError(f"required file missing: {p}")
        return p.read_text(encoding="utf-8")

    try:
        spine, toc, conv = (json.loads(read(name)) for name in
                            ("spine.json", "toc.json", "conversion.json"))
    except json.JSONDecodeError as e:
        raise PostError(f"raw JSON unparseable: {e}")
    if not isinstance(spine, list) or not spine:
        raise PostError("spine.json is not a non-empty list")
    if not isinstance(toc, list) or any(not isinstance(e, dict) for e in toc):
        raise PostError("toc.json is not a list of objects")
    if not isinstance(conv, dict) or not isinstance(conv.get("metadata"),
                                                    dict):
        raise PostError("conversion.json lacks its metadata object")
    return read("index.html"), spine, toc, conv


def _split_segments(index_text, spine):
    """Split index.html on its spine markers; validate against spine.json."""
    segs, cur, buf = [], None, []
    for line in index_text.splitlines():
        m = MARKER_RE.match(line)
        if m:
            if cur is not None:
                segs.append((cur[0], cur[1], "\n".join(buf)))
            elif any(b.strip() for b in buf):
                warn("index.html carries content before the first spine "
                     "marker — dropped")
            cur, buf = (int(m.group(1)), m.group(2)), []
        else:
            buf.append(line)
    if cur is not None:
        segs.append((cur[0], cur[1], "\n".join(buf)))
    if not segs or [s[0] for s in segs] != list(range(len(spine))):
        raise PostError("spine markers do not run 0..n-1 against spine.json")
    for (i, href, _html), sent in zip(segs, spine):
        if not isinstance(sent, dict) or sent.get("href") != href:
            raise PostError(f"spine marker {i} does not match spine.json")
    return segs


def _convert_assemble(segs):
    """Per-spine markdown — split FIRST (A2: lxml keeps only the first
    of multiple documents), converted + cleaned per segment, then
    concatenated in spine order (empty segments keep an empty slot)."""
    lines, bounds = [], []
    for _i, href, html in segs:
        try:
            md = cleanup(html_to_markdown(html))
        except HtmlError as e:
            warn(f"spine file {href}: no parseable content ({e}) — skipped")
            md = ""
        if not md.strip():
            bounds.append((len(lines), len(lines)))
            continue
        if lines and lines[-1].strip():
            lines.append("")
        bounds.append((len(lines), len(lines) + md.count("\n") + 1))
        lines.extend(md.split("\n"))
    return lines, bounds


def _scan_headings(lines):
    """Column-0 ATX headings outside code fences, in document order."""
    out, fence = [], 0
    for idx, line in enumerate(lines):
        run = _fence_run(line)
        if fence:
            if run >= fence:
                fence = 0
            continue
        if run:
            fence = run
            continue
        m = HEADING_RE.match(line)
        if m:
            out.append({"line": idx, "level": len(m.group(1)),
                        "title": m.group(2).strip()})
    return out


def _find_heading(headings, start, end, label, claimed, level=None):
    """First unclaimed heading in [start, end) matching label — normalized
    text, exact pass first, case-insensitive fallback (A3)."""
    want = _norm(label)
    pool = [h for h in headings
            if start <= h["line"] < end and h["line"] not in claimed
            and (level is None or h["level"] == level)]
    for key in (str, str.lower):  # exact pass first, case-insensitive (A3)
        w = key(want)
        for h in pool:
            if key(_norm(h["title"])) == w:
                return h
    return None


def _tier_nav(toc, headings, bounds, n_lines):
    """Tier 1: level-0 toc entries -> chapters aligned at their spine
    boundary; N = 1-based level-0 ordinal (dropped entries leave gaps)."""
    top = [e for e in toc if e.get("level") == 0]
    valid = []
    for ti, e in enumerate(top):
        si = e.get("spine_index")
        if isinstance(si, int) and 0 <= si < len(bounds):
            valid.append((ti, e, si))
        else:
            warn(f"TOC entry {e.get('nav_label')!r}: spine_index {si!r} "
                 f"outside the spine — dropped")
    chapters, claimed = [], set()
    for j, (ti, e, si) in enumerate(valid):
        end = next((bounds[si2][0] for _p, _e, si2 in valid[j + 1:]
                   if si2 > si), n_lines)
        h = _find_heading(headings, bounds[si][0], end,
                          e.get("nav_label") or "", claimed)
        if h is None:
            warn(f"TOC entry {e.get('nav_label')!r} (spine {si}): no "
                 f"matching heading — dropped")
            continue
        claimed.add(h["line"])
        chapters.append({"n": ti + 1, "line": h["line"], "title": h["title"],
                         "level": h["level"], "synth": False})
    return chapters


def _validate_nested(toc, headings, chapters):
    """A1: nested toc entries validate against their parent chapter's
    h2 sections; unmatched -> warning (never removes content)."""
    by_n = {c["n"]: c for c in chapters}
    chapter_lines = {c["line"] for c in chapters}
    li, last, claimed = 0, None, set(chapter_lines)
    for e in toc:
        label = e.get("nav_label") or ""
        if e.get("level") == 0:
            li += 1
            last, claimed = by_n.get(li), set(chapter_lines)
        elif last is None:
            warn(f"TOC entry {label!r}: parent chapter not matched — dropped")
        else:
            h = _find_heading(headings, last["line"], last["end"], label,
                             claimed, level=2)
            if h is None:
                warn(f"TOC entry {label!r}: no matching section heading in "
                     f"chapter {last['n']} — dropped")
            else:
                claimed.add(h["line"])


def _tier_headings(headings):
    """Tier 2: every '#' heading is a chapter (else every '##' promoted)."""
    for want in (1, 2):
        got = [h for h in headings if h["level"] == want]
        if got:
            return [{"n": i + 1, "line": h["line"], "title": h["title"],
                     "level": h["level"], "synth": False}
                    for i, h in enumerate(got)]
    return []


def _tier_spine(bounds, headings, segs):
    """Tier 3: one chapter per non-empty segment; title = first heading
    else filename stem (empty segments stay front matter)."""
    chapters = []
    for i, (start, end) in enumerate(bounds):
        if end <= start:
            continue
        seg_h = next((h for h in headings if start <= h["line"] < end), None)
        if seg_h is not None:
            chapters.append({"n": len(chapters) + 1, "line": seg_h["line"],
                             "title": seg_h["title"],
                             "level": seg_h["level"], "synth": False})
        else:
            stem = Path(segs[i][1]).stem or f"spine-{i}"
            chapters.append({"n": len(chapters) + 1, "line": start,
                             "title": stem, "level": 1, "synth": True})
    return chapters


def _spans(chapters, n_lines):
    """Chapters sorted by body position, each with its 'end' boundary."""
    ordered = sorted(chapters, key=lambda c: c["line"])
    for i, ch in enumerate(ordered):
        ch["end"] = (ordered[i + 1]["line"] if i + 1 < len(ordered)
                     else n_lines)
    return ordered


def _front_matter_sections(ordered, bounds, lines, headings):
    """sec-0 sections for the front matter: whole segments before the
    first chapter's segment plus its lines before the chapter heading;
    a unit's title = its first heading else 'Front Matter' (an empty
    segment — calibre's cover/titlepage — still yields its section)."""
    k = len(bounds)
    if ordered:
        k = next((i for i, (s, e) in enumerate(bounds)
                  if s <= ordered[0]["line"] < e), len(bounds))
    units = list(bounds[:k])
    if k < len(bounds) and bounds[k][0] < ordered[0]["line"]:
        units.append((bounds[k][0], ordered[0]["line"]))
    sections = []
    for s, e in units:
        u_hs = [h for h in headings if s <= h["line"] < e]
        first_nb = next((i for i in range(s, e) if lines[i].strip()), None)
        if not u_hs:
            sections.append({"line": s if first_nb is None else first_nb,
                             "title": FRONT_TITLE, "synth": True})
            continue
        if first_nb is not None and first_nb < u_hs[0]["line"]:
            sections.append({"line": first_nb, "title": FRONT_TITLE,
                             "synth": True})
        sections.extend({"line": h["line"], "title": h["title"],
                         "synth": False} for h in u_hs)
    return sections


def _assign_sections(ordered, headings):
    """A1: every h2 in a chapter becomes a section (M 1-based, body order)."""
    chapter_lines = {c["line"] for c in ordered}
    secs = []
    for ch in ordered:
        m = 0
        for h in headings:
            if (ch["line"] < h["line"] < ch["end"] and h["level"] == 2
                    and h["line"] not in chapter_lines):
                m += 1
                secs.append({"n": ch["n"], "m": m, "h": h})
    return secs


def _rewrite_lines(ordered, secs, fm_sections, seen):
    """Rewrite map + insertions for chapter/section/front-matter anchors
    (chapters '#', sections and front matter '##')."""
    rewrites, insertions = {}, {}
    for i, sec in enumerate(fm_sections, 1):
        sid = _unique_id(f"sec-0-{i}-{_slugify(sec['title'])}", seen)
        target = f"## {sec['title']} {{#{sid}}}"
        if sec["synth"]:
            insertions.setdefault(sec["line"], []).extend([target, ""])
        else:
            rewrites[sec["line"]] = target
    for ch in ordered:
        ch["id"] = _unique_id(f"ch-{ch['n']}-{_slugify(ch['title'])}", seen)
        target = f"# {ch['title']} {{#{ch['id']}}}"
        if ch["synth"]:
            insertions.setdefault(ch["line"], []).extend([target, ""])
        else:
            rewrites[ch["line"]] = target
    for sec in secs:
        sec["id"] = _unique_id(
            f"sec-{sec['n']}-{sec['m']}-{_slugify(sec['h']['title'])}", seen)
        rewrites[sec["h"]["line"]] = f"## {sec['h']['title']} {{#{sec['id']}}}"
    return rewrites, insertions


def _scan_figures(out_lines):
    """Figure-manifest entries from emitted image links, fence-aware; src
    captures ONE balanced paren pair (deeper/unbalanced = malformed link)."""
    figures, fence = [], 0
    for line in out_lines:
        run = _fence_run(line)
        if fence:
            if run >= fence:
                fence = 0
            continue
        if run:
            fence = run
            continue
        for m in IMAGE_RE.finditer(line):
            src = m.group(2).strip()
            if src.startswith("<") and src.endswith(">"):
                src = src[1:-1]  # angle-bracket wrapping is link syntax
            figures.append({"alt": m.group(1).strip(), "src": src,
                            "page": None})
    return figures


def _meta_val(meta, key):
    """A5: the literal 'Unknown' (calibre's empty-metadata fallback)
    counts as absent."""
    v = meta.get(key)
    if isinstance(v, str) and v.strip() and v.strip() != UNKNOWN:
        return v.strip()
    return None


def _book_title(meta, conv, raw_dir):
    """Title chain (A5): metadata title -> source-filename stem -> slug
    directory -> 'Untitled'."""
    title = _meta_val(meta, "title")
    if title:
        return title
    src = conv.get("source")
    if isinstance(src, str) and Path(src).stem.strip():
        return Path(src).stem.strip()
    return Path(raw_dir).resolve().parent.name or "Untitled"


def _run(args):
    _WARNINGS.clear()
    raw_dir = Path(args.raw).expanduser()
    out_path = Path(args.output).expanduser()
    index_text, spine, toc, conv = _load_raw(raw_dir)
    segs = _split_segments(index_text, spine)
    lines, bounds = _convert_assemble(segs)
    headings = _scan_headings(lines)
    n_lines = len(lines)

    chapters, won = [], None
    for tier in TIERS[args.toc_source]:
        if tier == "nav":
            chapters = _tier_nav(toc, headings, bounds, n_lines)
        elif tier == "headings":
            chapters = _tier_headings(headings)
        else:
            chapters = _tier_spine(bounds, headings, segs)
        if chapters:
            won = tier
            break
        warn(f"structure tier {tier!r} yielded no chapters — falling back")
    ordered = _spans(chapters, n_lines)
    if won == "nav" and ordered:
        _validate_nested(toc, headings, ordered)
    secs = _assign_sections(ordered, headings)
    fm_sections = _front_matter_sections(ordered, bounds, lines, headings)

    rewrites, insertions = _rewrite_lines(ordered, secs, fm_sections, set())
    out = []
    for i in range(n_lines):
        out.extend(insertions.get(i, ()))
        out.append(rewrites.get(i, lines[i]))
    out.extend(insertions.get(n_lines, ()))  # sections at the very end

    toc_entries = []
    for ch in ordered:
        toc_entries.append({"id": ch["id"], "title": ch["title"], "level": 1})
        toc_entries.extend({"id": s["id"], "title": s["h"]["title"],
                            "level": 2} for s in secs if s["n"] == ch["n"])

    meta = conv.get("metadata") or {}
    src = conv.get("source") if isinstance(conv.get("source"), str) else None
    ver = conv.get("calibre_version")
    ver = ver.strip() if isinstance(ver, str) else ""
    fm = {
        "title": _book_title(meta, conv, raw_dir),
        "author": _meta_val(meta, "author"),
        "language": _meta_val(meta, "language"),
        "source_ebook": src,
        "converted_date": time.strftime("%Y-%m-%d", time.gmtime()),
        "converter": f"calibre {ver}" if ver and ver != "unknown"
                     else "calibre",
        "mode": "ebook",
        "chapter_count": len(ordered),
        "toc": toc_entries,
    }
    toc_lines = ["## Table of Contents", ""]
    for e in toc_entries:
        text = e["title"].replace("[", "\\[").replace("]", "\\]")
        toc_lines.append(("" if e["level"] == 1 else "  ")
                        + f"- [{text}](#{e['id']})")
    body = "\n".join(out)
    if body.strip():
        body = body.rstrip("\n") + "\n"
    book = ("---\n" + yaml.safe_dump(fm, allow_unicode=True, sort_keys=False)
            + "---\n\n" + "\n".join(toc_lines) + "\n\n" + body)

    out_dir = out_path.parent
    out_dir.mkdir(parents=True, exist_ok=True)
    out_path.write_text(book, encoding="utf-8")
    if (raw_dir / "images").is_dir():
        try:
            shutil.rmtree(out_dir / "images", ignore_errors=True)  # M2: replace
            shutil.copytree(raw_dir / "images", out_dir / "images")
        except OSError as e:
            warn(f"could not copy raw images/: {e}")
    else:
        warn("raw images/ directory missing — no images copied")
    figures = _scan_figures(out)
    fig_path = out_dir / "figures" / "manifest.json"
    fig_path.parent.mkdir(parents=True, exist_ok=True)
    fig_path.write_text(json.dumps({
        "book": fm["title"], "source_ebook": src,
        "figure_count": len(figures), "figures": figures,
    }, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    return emit({
        "ok": True,
        "readable_book": str(out_path),
        "figure_manifest": str(fig_path),
        "chapter_count": len(ordered),
        "front_matter_sections": len(fm_sections),
        "warnings": _WARNINGS,
    }, 0)


def main():
    ap = argparse.ArgumentParser(
        description="Turn convert.py's raw/ unpack into a readable book.")
    ap.add_argument("--raw", required=True,
                    help="raw/ dir from convert.py's stdout JSON.")
    ap.add_argument("--output", required=True, help="book.md output path.")
    ap.add_argument("--toc-source", choices=TIERS, default="auto",
                    help="starting structure tier (default: auto).")
    args = ap.parse_args()
    try:
        return _run(args)
    except PostError as e:
        log(f"refused: {e}")
        return emit({"ok": False, "error": str(e)}, 1)
    except Exception as e:  # never crash without the JSON contract
        log(f"unexpected failure: {e!r}\n{traceback.format_exc()}")
        return emit({"ok": False, "error": f"postprocess failed: {e}"}, 1)


if __name__ == "__main__":
    sys.exit(main())