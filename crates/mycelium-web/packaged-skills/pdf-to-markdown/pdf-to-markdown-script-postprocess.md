---
type: Skill
title: pdf-to-markdown script — postprocess.py
description: Verbatim copy of the pdf-to-markdown skill's python script `postprocess.py`. Adds frontmatter, TOC, stable heading IDs, page citations and the figure manifest (imports html_cleanup). Extract the fenced block byte-exact; verify md5 `ebce3858e7b192a14e376a70daf28284` against the [scripts manifest](/pdf-to-markdown-scripts.md).
tags:
- pdf-to-markdown
- docling
- script
- packaged
timestamp: 2026-09-21T00:00:00Z
---

# pdf-to-markdown script — postprocess.py

Byte-exact copy of `postprocess.py` from the [pdf-to-markdown](/pdf-to-markdown.md) skill (source tree: `scripts/postprocess.py`). **no trailing newline**

Extract the fenced block below **exactly**: strip the opening ` ```` ` line and the closing ` ```` ` line, keep every byte between them — including the final line's missing newline if the manifest says "no trailing newline" (an editor that silently appends one is harmless for these scripts, but the md5 will differ).

`30079 bytes, md5 ebce3858e7b192a14e376a70daf28284`

````
#!/usr/bin/env python3
"""postprocess_v2 — outline-driven chapter detection prototype.

Diff vs scripts/postprocess.py:
  * Chapter structure comes from the PDF's own embedded outline (pypdfium2),
    NOT marker's degraded markdown/HTML headings. The outline (clean titles +
    page indices) is aligned to marker's body by <span id="page-N-M"> page
    spans (marker and pypdfium2 share the same 0-based PDF page index).
  * For each outline chapter, a clean markdown '# Title {#ch-N-slug}' heading
    is placed at the chapter's page-span boundary: if a matching markdown
    heading already sits there, it is REPLACED (clean title, promoted to H1,
    id added); otherwise a new heading is INSERTED before the page span
    (handles marker's HTML <h1>-in-blob case, e.g. Python Crash Course ch3+).
  * Book title from PDF metadata Title (fixes PCC 'PRAISE FOR...' bug).
  * Falls back to the original marker-TOC / page-anchored-H1 heuristics when
    there is no source PDF, no outline, or the outline yields no chapters.

Same output contract as postprocess.py (one JSON line on stdout) plus
chapter_titles, chapter_method, book_title_source for test visibility.
"""
import argparse, json, re, sys, time
from pathlib import Path
import pypdfium2 as pdfium

# HTML-body cleanup (converts marker's run-on HTML-blob lines to readable
# markdown; leaves clean-markdown books untouched). See html_cleanup.py.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from html_cleanup import html_to_markdown


def log(msg):
    print(msg, file=sys.stderr, flush=True)


HTML_TAG_RE = re.compile(r"<[^>]+>")
EMPH_RE = re.compile(r"(\*\*|__|`)(.*?)\1")
CODEISH_RE = re.compile(r'[=`";]|//')
SLUG_MAX = 64
CHAPTER_CAP = 50
CHAPTER_CAP_SPANNED = 200

# ---------- outline chapter identification (oracle v4 logic, inlined) ----------
CHAP_N_RE   = re.compile(r"^\s*Chapter\s+\d+\b", re.IGNORECASE)
NUM_DOT_RE  = re.compile(r"^\s*\d+\.\s+\S")
BARE_NUM_RE = re.compile(r"^\s*\d+$")
APPX_RE     = re.compile(r"^\s*Appendix\b", re.IGNORECASE)
APPX_LET_RE = re.compile(r"^\s*[A-Z]\.\s+\S")
PART_RE     = re.compile(r"^\s*Part\s+", re.IGNORECASE)
SPAN_BOTH_RE = re.compile(r"<span id=['\"]page-(\d+)-\d+['\"]")

FRONT_BACK_PREFIX = (
    "cover","front cover","back cover","title page","copyright","dedication",
    "about the author","about the authors","about the author and technical reviewer",
    "about the reviewer","about the reviewers","contributors",
    "brief contents","contents in detail","table of contents","contents",
    "foreword","preface","acknowledgemen","acknowledgment","introduction","index",
    "colophon","resources","praise for","bibliography","updates","front matter",
    "about this book","who is this book for","who this book is for",
    "what this book covers","to get the most out of this book",
    "how to contact us","get in touch","o'reilly online learning","oreilly online learning",
    "conventions used","using code examples","safari enabled",
    "what is programming","what is python","common myths about programming",
    "new to the third edition","online resources","why python",
    "what can you expect to learn",
)
PACKT_PROMO_PREFIX = (
    "unlock access","other books you may enjoy","why subscribe","packt is searching",
    "share your thoughts","get in touch","free benefits with your book",
    "download the example code","download the color images","need help",
    "step 1","step 2","step 3",
)


def _fbnorm(t):
    t = (t or "").strip()
    t = re.sub(r"\s+", " ", t).rstrip(" .")
    return t.lower()

def _is_frontback(title):
    t = _fbnorm(title)
    if not t:
        return True
    for kw in FRONT_BACK_PREFIX:
        if t == kw or t.startswith(kw):
            return True
    return False

def _is_appendix(title):
    return bool(APPX_RE.match(title) or APPX_LET_RE.match(title))

def _is_packt_promo(title):
    t = _fbnorm(title)
    for kw in PACKT_PROMO_PREFIX:
        if t == kw or t.startswith(kw):
            return True
    return False


def load_outline(pdf_path):
    """Return (book_title, chapters, method, appendices, page_count).
    chapters = [{title, page}] in reading order (page = 0-based PDF page index)."""
    try:
        doc = pdfium.PdfDocument(pdf_path)
    except Exception as e:
        log(f"WARN: could not open PDF for outline: {e}")
        return None, [], "none", [], 0
    page_count = len(doc)
    book_title = None
    try:
        book_title = (doc.get_metadata_dict() or {}).get("Title") or None
    except Exception:
        pass
    toc = list(doc.get_toc())
    if not toc:
        return book_title, [], "none", [], page_count

    entries, stack = [], []
    for b in toc:
        title = (b.get_title() or "").strip()
        lvl = b.level
        try:
            pidx = b.get_dest().get_index()
        except Exception:
            pidx = None
        while stack and stack[-1][0] >= lvl:
            stack.pop()
        parent = stack[-1][1] if stack else None
        stack.append((lvl, title))
        entries.append({"level": lvl, "page": pidx, "title": title, "parent": parent})

    parts = [e for e in entries if e["level"] == 0 and PART_RE.match(e["title"])]
    appendices = [e for e in entries if _is_appendix(e["title"])]
    chapters, method = [], "none"

    chap_n = [e for e in entries if CHAP_N_RE.match(e["title"])]
    if chap_n:
        chapters, method = chap_n, "chapter_n"
    else:
        l0 = [e for e in entries if e["level"] == 0]
        packt = []
        for i, e in enumerate(l0):
            if BARE_NUM_RE.match(e["title"]) and i + 1 < len(l0) \
               and not BARE_NUM_RE.match(l0[i+1]["title"]) \
               and not _is_packt_promo(l0[i+1]["title"]) and not _is_appendix(l0[i+1]["title"]):
                packt.append(l0[i+1])
        if len(packt) >= 2:
            chapters, method = packt, "packt"
        else:
            numbered = [e for e in entries if NUM_DOT_RE.match(e["title"]) and not _is_appendix(e["title"])]
            if parts:
                numbered = [e for e in numbered if e["parent"] is not None and PART_RE.match(e["parent"] or "")]
            if numbered:
                chapters, method = numbered, "numbered"
            else:
                body = [e for e in entries if e["level"] == 0 and not _is_frontback(e["title"])
                        and not _is_appendix(e["title"]) and not BARE_NUM_RE.match(e["title"])
                        and not PART_RE.match(e["title"])]
                if len(body) >= 2:
                    chapters, method = body, "topic_l0"
                elif len(body) == 1:
                    title_entry = body[0]
                    children = [e for e in entries if e["parent"] == title_entry["title"] and e["level"] == 1]
                    if len(children) >= 2:
                        chapters, method = children, "topic_l1_children"
                    else:
                        chapters, method = [], "none"
                else:
                    chapters, method = [], "none"

    # Keep chapters even with page=None (broken bookmark); the alignment loop's
    # title fallback locates them by heading text in that case.
    return book_title, chapters, method, appendices, page_count


# ---------- shared helpers (unchanged from postprocess.py) ----------
def strip_html(text):
    text = HTML_TAG_RE.sub(" ", text or "")
    return (text.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")
            .replace("&quot;", '"').replace("&#39;", "'").replace("&nbsp;", " "))

def clean_title(text):
    text = strip_html(text)
    text = EMPH_RE.sub(r"\2", text)
    text = re.sub(r"[*_`]", "", text)
    return re.sub(r"\s+", " ", text).strip()

def looks_like_heading_title(text):
    t = clean_title(text)
    if not t or len(t) > 120:
        return False
    return not CODEISH_RE.search(t)

def slugify(text):
    text = clean_title(text).strip().lower()
    text = re.sub(r"[^\w\s-]", "", text)
    text = re.sub(r"[\s_-]+", "-", text).strip("-")
    if len(text) > SLUG_MAX:
        text = text[:SLUG_MAX].rstrip("-")
    return text or "section"

def norm_title(t):
    return re.sub(r"\s+", " ", clean_title(t).strip().lower()).rstrip(".")

HEADING_RE = re.compile(r"^(#{1,6})\s+(.*?)\s*({#[^}]+})?\s*$")
IMAGE_RE = re.compile(r"!\[([^\]]*)\]\(([^)]+)\)")

def load_json(path):
    if path is None:
        return {}
    p = Path(path)
    if not p.exists():
        return {}
    return json.loads(p.read_text(encoding="utf-8"))

def parse_headings(md_text):
    headings = []
    in_fence = False
    fence_tok = None
    for i, line in enumerate(md_text.splitlines()):
        m = re.match(r"^\s*(```|~~~)", line)
        if m and not in_fence:
            in_fence = True; fence_tok = m.group(1); continue
        if in_fence:
            if re.match(r"^\s*" + re.escape(fence_tok) + r"\s*$", line):
                in_fence = False; fence_tok = None
            continue
        m = HEADING_RE.match(line)
        if m and looks_like_heading_title(m.group(2)):
            headings.append({"line": i, "level": len(m.group(1)),
                             "title": clean_title(m.group(2)), "raw": line})
    return headings

def build_toc_index(meta):
    toc = []
    if not isinstance(meta, dict):
        return toc, {}
    raw_toc = meta.get("table_of_contents") or []
    by_title = {}
    for e in raw_toc:
        if not isinstance(e, dict):
            continue
        title = e.get("title")
        if not title:
            continue
        entry = {"title": clean_title(title), "heading_level": e.get("heading_level"),
                 "page_id": e.get("page_id")}
        toc.append(entry)
        nt = norm_title(title)
        if nt not in by_title:
            by_title[nt] = entry
    return toc, by_title


# ---------- outline-driven alignment helpers ----------
def _match_norm(t):
    """Aggressive normalize for title matching: strip 'Chapter N' prefix,
    strip markup/emphasis, remove ALL whitespace, lowercase."""
    t = re.sub(r"^\s*Chapter\s+\d+\b[:.]?\s*", "", t or "", flags=re.IGNORECASE)
    t = re.sub(r"<[^>]+>", "", t)
    t = re.sub(r"[*_`]", "", t)
    t = re.sub(r"\s+", "", t).lower()
    return t

def is_chapter_id(hid):
    """Chapter ids are 'ch-<n>-<slug>'; section ids use the 'sec-' prefix so a
    chapter title that starts with a digit (e.g. '1. Python Basics' ->
    slug '1-python-basics' -> id 'ch-1-1-python-basics') is NOT mistaken for a
    section 'ch-<n>-<m>-<slug>'."""
    return hid.startswith("ch-")


_FRONT_TITLE_PREFIX = (
    "cover", "copyright", "table of contents", "contents", "preface",
    "foreword", "acknowledgemen", "acknowledgment", "introduction",
    "dedication", "about the author", "praise", "brief contents",
    "contents in detail", "title page", "front cover", "back cover",
)

def is_letter_spaced(t):
    """True if a title looks like all-caps letter-spaced ('R E A L W O R L D')."""
    toks = (t or "").split()
    if len(toks) < 4:
        return False
    single = sum(1 for tk in toks if len(tk) == 1)
    return single >= len(toks) * 0.6

def first_clean_h1_title(headings):
    """First non-letter-spaced, non-front-matter markdown H1 — a clean book
    title when the PDF metadata Title is letter-spaced (e.g. REAL-WORLD PYTHON
    while metadata says 'R E A L W O R L D PYTHON')."""
    for h in headings:
        if h["level"] != 1:
            continue
        t = h["title"].strip()
        if not t or is_letter_spaced(t):
            continue
        low = re.sub(r"\s+", " ", t).strip().lower()
        if any(low.startswith(k) for k in _FRONT_TITLE_PREFIX):
            continue
        return t
    return None

def page_span_spans(text):
    """All page spans as (page, char_offset) in text order."""
    return [(int(m.group(1)), m.start()) for m in SPAN_BOTH_RE.finditer(text)]

def line_starts_of(text):
    """Char offset of the start of each line (line 0 = 0). len == #lines + 1."""
    starts = [0]
    pos = 0
    for ln in text.splitlines():
        pos += len(ln) + 1
        starts.append(pos)
    return starts

def find_boundary(spans, page, next_page=None):
    """Char offset of a span for `page` (exact preferred), else the first span
    with page in [page, next_page) — i.e. within this chapter's page range, so
    a chapter with no own span does not jump to a far later chapter's span
    (which would cluster misplaced headings). Returns None if none in range."""
    # exact match first
    for (pg, off) in spans:
        if pg == page:
            return off
    # within-range match (avoid leaping past next chapter's page)
    hi = next_page if next_page is not None else page + 50
    for (pg, off) in spans:
        if page <= pg < hi:
            return off
    return None

def find_candidate_heading(headings, line_starts, boundary_off, outline_title, max_lines=8):
    """Nearest markdown heading within max_lines of the boundary whose normalized
    title matches the outline title. Returns the heading or None."""
    # line index of boundary char
    import bisect
    bli = bisect.bisect_right(line_starts, boundary_off) - 1
    target = _match_norm(outline_title)
    if not target:
        return None
    best = None
    for h in headings:
        if abs(h["line"] - bli) > max_lines:
            continue
        if _match_norm(h["title"]) == target:
            if best is None or abs(h["line"] - bli) < abs(best["line"] - bli):
                best = h
    return best


def main():
    ap = argparse.ArgumentParser(description="Post-process marker markdown (outline-driven).")
    ap.add_argument("--raw-md", required=True)
    ap.add_argument("--meta", required=True)
    ap.add_argument("--conversion", required=True)
    ap.add_argument("--output", required=True)
    ap.add_argument("--split-by-chapter", action="store_true")
    args = ap.parse_args()

    try:
        raw_path = Path(args.raw_md).expanduser().resolve()
        meta = load_json(args.meta)
        conv = load_json(args.conversion)
        md_text = raw_path.read_text(encoding="utf-8")
        # Convert marker's HTML-blob body to readable markdown BEFORE chapter
        # detection. Page spans <span id='page-N-M'></span> are preserved, so
        # outline-to-page-span alignment still works; formerly-HTML <h1>
        # chapter headings become markdown headings that get replaced (no
        # duplicate). Clean-markdown books are essentially unchanged.
        md_text = html_to_markdown(md_text)

        toc, toc_by_title = build_toc_index(meta)
        page_stats = meta.get("page_stats") if isinstance(meta, dict) else None
        page_count_meta = len(page_stats) if isinstance(page_stats, list) else None

        headings = parse_headings(md_text)
        line_starts = line_starts_of(md_text)
        spans = page_span_spans(md_text)

        source_pdf = conv.get("source_pdf", "")
        book_title, outline_chs, method, outline_appx, pdf_page_count = (None, [], "none", [], 0)
        used_outline = False
        if source_pdf and Path(source_pdf).exists():
            book_title, outline_chs, method, outline_appx, pdf_page_count = load_outline(source_pdf)

        chapter_marks = []  # {hid, title, src, line(after edit)} for output TOC
        chapter_titles_out = []
        book_title_source = "marker"

        if outline_chs:
            used_outline = True
            book_title_source = "pdf_metadata" if book_title else "marker"
            # Build chapter heading edits.
            edits = []  # (pos, end, replacement) applied in reverse pos order
            n = len(outline_chs)
            last_line = -1  # line index of the previous chapter's edit (for page-None fallback)
            for i, ch in enumerate(outline_chs):
                title = ch["title"]
                hid = f"ch-{i+1}-{slugify(title)}"
                cpage = ch["page"]
                if cpage is not None:
                    start_d = cpage + 1
                    end_d = (outline_chs[i+1]["page"]) if i+1 < n and outline_chs[i+1]["page"] is not None else (pdf_page_count or page_count_meta or start_d)
                    if end_d < start_d:
                        end_d = start_d
                else:
                    start_d = end_d = 0
                src = f"<!-- src: pp. {start_d}-{end_d} -->" if cpage is not None else ""
                cand = None
                boff = None
                if cpage is not None:
                    nxt = outline_chs[i+1]["page"] if i+1 < n and outline_chs[i+1]["page"] is not None else None
                    boff = find_boundary(spans, cpage, nxt)
                    if boff is not None:
                        cand = find_candidate_heading(headings, line_starts, boff, title)
                # page-None (broken bookmark) or no span: fall back to a title match
                # in a heading AFTER the previous chapter's line.
                if cand is None:
                    target = _match_norm(title)
                    for h in headings:
                        if h["line"] > last_line and _match_norm(h["title"]) == target:
                            cand = h
                            break
                if cand is None and boff is None:
                    log(f"WARN: cannot locate chapter {i+1} '{title}' (page={cpage}, no span/title match); skipping.")
                    continue
                heading_line = f"# {title} {{#{hid}}}"
                if cand is not None:
                    # REPLACE: consumes the whole heading line (incl. trailing \n).
                    block = heading_line + "\n" + (src + "\n" if src else "")
                    ls = line_starts[cand["line"]]
                    le = line_starts[cand["line"]+1] if cand["line"]+1 < len(line_starts) else len(md_text)
                    edits.append((ls, le, block))
                    last_line = cand["line"]
                else:
                    # INSERT mid-blob: lead with \n so the heading is on its own
                    # line (markdown headings must start a line, and downstream
                    # tooling matches line-anchored headings).
                    block = "\n" + heading_line + "\n" + (src + "\n" if src else "")
                    edits.append((boff, boff, block))
                    import bisect
                    last_line = bisect.bisect_right(line_starts, boff) - 1
                chapter_marks.append({"hid": hid, "title": title, "src": src})
                chapter_titles_out.append(title)

            # Fallback for pagination gaps: if span/title alignment was
            # incomplete (e.g. marker paginated only the first/last pages and
            # emitted no recognizable titles for the middle) but marker's H1
            # headings match the outline chapter count, order-match: the k-th
            # outline chapter takes the k-th H1's position (marker's H1s are
            # chapter starts in order). Only triggers when incomplete AND counts
            # match, so complete-pagination books are unaffected. Recovers TDD.
            if 0 < len(chapter_marks) < len(outline_chs):
                h1s = [h for h in headings if h["level"] == 1 and not _is_frontback(h["title"])]
                if len(h1s) == len(outline_chs):
                    log(f"WARN: span/title alignment placed {len(chapter_marks)}/{len(outline_chs)}; "
                        f"H1 count matches outline ({len(h1s)}); order-matching outline titles to H1 positions.")
                    edits = []; chapter_marks = []; chapter_titles_out = []
                    for i, ch in enumerate(outline_chs):
                        title = ch["title"]; hid = f"ch-{i+1}-{slugify(title)}"
                        cpage = ch["page"]
                        if cpage is not None:
                            nxt = outline_chs[i+1]["page"] if i+1 < n and outline_chs[i+1]["page"] is not None else None
                            start_d = cpage + 1
                            end_d = nxt if nxt is not None else (pdf_page_count or page_count_meta or start_d)
                            if end_d < start_d: end_d = start_d
                            src = f"<!-- src: pp. {start_d}-{end_d} -->"
                        else:
                            src = ""
                        h = h1s[i]
                        block = f"# {title} {{#{hid}}}\n" + (src + "\n" if src else "")
                        ls = line_starts[h["line"]]
                        le = line_starts[h["line"]+1] if h["line"]+1 < len(line_starts) else len(md_text)
                        edits.append((ls, le, block))
                        chapter_marks.append({"hid": hid, "title": title, "src": src})
                        chapter_titles_out.append(title)

            # Apply edits in reverse position order.
            edits.sort(key=lambda e: e[0], reverse=True)
            for (pos, end, rep) in edits:
                md_text = md_text[:pos] + rep + md_text[end:]

            is_chapter_line = None  # will derive from re-parsed headings
        else:
            # ---- fallback: original marker-TOC / page-anchored-H1 heuristics ----
            chapter_start_indices = []
            heading_page_ids = [None] * len(headings)
            for hi, h in enumerate(headings):
                e = toc_by_title.get(norm_title(h["title"]))
                if e is not None and e.get("heading_level") == 1:
                    heading_page_ids[hi] = e.get("page_id")
                    chapter_start_indices.append(hi)
            if not chapter_start_indices and headings:
                level1 = [hi for hi, h in enumerate(headings) if h["level"] == 1]
                spanned = [hi for hi in level1 if '<span id="page-' in headings[hi]["raw"]]
                if 0 < len(spanned) <= CHAPTER_CAP_SPANNED:
                    chapter_start_indices = spanned
                elif 0 < len(level1) <= CHAPTER_CAP:
                    chapter_start_indices = level1
                elif len(level1) > CHAPTER_CAP:
                    log(f"WARN: TOC unusable and {len(level1)} top-level headings (>{CHAPTER_CAP}); "
                        f"writing a single whole-book concept.")
                else:
                    log("WARN: no chapter structure; writing a single whole-book concept.")
            is_chapter = set(chapter_start_indices)
            # rewrite heading ids (original behaviour)
            lines = md_text.splitlines()
            heading_ids = [None] * len(headings)
            current_chapter = 0; chapter_idx = 0; sec_counter = 0
            for hi, h in enumerate(headings):
                slug = slugify(h["title"])
                if hi in is_chapter:
                    chapter_idx += 1; current_chapter = chapter_idx; sec_counter = 0
                    hid = f"ch-{chapter_idx}-{slug}"
                else:
                    sec_counter += 1
                    hid = f"sec-{current_chapter}-{sec_counter}-{slug}" if current_chapter else f"sec-0-{hi+1}-{slug}"
                heading_ids[hi] = hid
                lines[h["line"]] = f"{'#' * h['level']} {h['title']} {{#{hid}}}"
            md_text = "\n".join(lines)
            # chapter_marks for TOC/frontmatter
            for hi in chapter_start_indices:
                chapter_marks.append({"hid": heading_ids[hi], "title": headings[hi]["title"], "src": ""})
                chapter_titles_out.append(headings[hi]["title"])

        # ---- re-parse modified text; assign section ids to non-chapter headings ----
        new_headings = parse_headings(md_text)
        lines = md_text.splitlines()
        current_chapter = 0
        sec_counter = 0
        for h in new_headings:
            m = HEADING_RE.match(h["raw"])
            existing_id = m.group(3) if m and m.group(3) else None
            if existing_id and existing_id.startswith("{#") and existing_id.endswith("}"):
                hid = existing_id[2:-1]
                if is_chapter_id(hid):
                    try:
                        current_chapter = int(hid.split("-")[1])
                    except Exception:
                        pass
                    sec_counter = 0
                    continue  # chapter heading already has its id; keep line as-is
            sec_counter += 1
            slug = slugify(h["title"])
            hid = (f"sec-{current_chapter}-{sec_counter}-{slug}" if current_chapter
                   else f"sec-0-{h['line']+1}-{slug}")
            lines[h["line"]] = f"{'#' * h['level']} {h['title']} {{#{hid}}}"
        md_text = "\n".join(lines)

        # ---- figures ----
        figures = []
        for m in IMAGE_RE.finditer(raw_path.read_text(encoding="utf-8")):
            figures.append({"alt": m.group(1).strip(), "src": m.group(2).strip(), "page": None})

        # ---- frontmatter book title ----
        # Resolve a title from PDF metadata, then marker TOC / first heading /
        # slug. THEN, if whatever we ended up with is letter-spaced all-caps
        # (marker/metadata title-page artifact), replace it with the first clean
        # non-front-matter marker H1.
        if not book_title:
            if toc:
                book_title = next((e["title"] for e in toc if e.get("heading_level") == 0), None)
            if not book_title and headings:
                book_title = headings[0]["title"]
            if not book_title:
                book_title = conv.get("book_slug") or "Untitled"
            book_title_source = "marker"
        if book_title and is_letter_spaced(book_title):
            clean = first_clean_h1_title(headings)
            if clean:
                book_title = clean; book_title_source = "marker_h1"
        author = meta.get("author") if isinstance(meta, dict) else None
        source_pdf = conv.get("source_pdf", "")
        mode = conv.get("mode", "")
        marker_version = conv.get("marker_version")
        # New Docling conversions carry engine/engine_version; old marker
        # conversions carry only marker_version. Label either correctly.
        engine = conv.get("engine") or "marker-pdf"
        engine_ver = conv.get("engine_version") or (marker_version if engine == "marker-pdf" else None)
        if engine == "docling":
            converter_label = f"docling {engine_ver}" if engine_ver else "docling"
        else:
            converter_label = f"marker-pdf {marker_version}" if marker_version else "marker-pdf"
        converted_date = conv.get("converted_at") or time.strftime("%Y-%m-%d", time.gmtime())
        page_count = page_count_meta or pdf_page_count

        toc_for_fm = [{"id": cm["hid"], "title": cm["title"], "level": 1} for cm in chapter_marks]
        fm_lines = ["---",
                    f"title: {json.dumps(book_title, ensure_ascii=False)}"]
        if author:
            fm_lines.append(f"author: {json.dumps(author, ensure_ascii=False)}")
        fm_lines.append(f"source_pdf: {json.dumps(source_pdf, ensure_ascii=False)}")
        fm_lines.append(f"converted_date: '{converted_date}'")
        fm_lines.append(f"converter: {json.dumps(converter_label, ensure_ascii=False)}")
        fm_lines.append(f"mode: {mode}")
        if page_count:
            fm_lines.append(f"page_count: {page_count}")
        fm_lines.append("toc:")
        for e in toc_for_fm:
            fm_lines.append(f"  - id: {e['id']}")
            fm_lines.append(f"    title: {json.dumps(e['title'], ensure_ascii=False)}")
            fm_lines.append(f"    level: {e['level']}")
        fm_lines.append("---")

        toc_block = ["", "## Table of Contents", ""]
        for cm in chapter_marks:
            toc_block.append("- [" + cm["title"] + "](#" + cm["hid"] + ")")
        toc_block.append("")

        out_path = Path(args.output).expanduser().resolve()
        out_path.parent.mkdir(parents=True, exist_ok=True)
        body = md_text
        full = "\n".join(fm_lines) + "\n" + "\n".join(toc_block) + "\n" + body + "\n"
        out_path.write_text(full, encoding="utf-8")

        figures_dir = out_path.parent / "figures"
        figures_dir.mkdir(parents=True, exist_ok=True)
        manifest = {"book": book_title, "source_pdf": source_pdf,
                     "figure_count": len(figures), "figures": figures}
        (figures_dir / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2), encoding="utf-8")

        # optional per-chapter split (by inserted chapter heading ids)
        chapter_files = []
        if args.split_by_chapter and chapter_marks:
            ch_dir = out_path.parent / "chapters"
            ch_dir.mkdir(parents=True, exist_ok=True)
            body_lines = body.splitlines()
            id_to_line = {}
            for i, ln in enumerate(body_lines):
                mm = re.search(r"\{#(ch-\d+-[^}]+)\}", ln)
                if mm:
                    id_to_line.setdefault(mm.group(1), i)
            for ci, cm in enumerate(chapter_marks):
                hid = cm["hid"]
                start = id_to_line.get(hid, 0)
                end = id_to_line.get(chapter_marks[ci+1]["hid"], len(body_lines)) if ci+1 < len(chapter_marks) else len(body_lines)
                chunk = "\n".join(body_lines[start:end]).strip()
                cf = ch_dir / f"{hid}.md"
                cf.write_text(f"# {cm['title']}\n\n{chunk}\n", encoding="utf-8")
                chapter_files.append(str(cf))

        print(json.dumps({
            "ok": True,
            "readable_book": str(out_path),
            "figure_manifest": str(figures_dir / "manifest.json"),
            "figure_count": len(figures),
            "chapter_count": len(chapter_marks),
            "chapter_titles": chapter_titles_out,
            "chapter_method": method,
            "used_outline": used_outline,
            "book_title": book_title,
            "book_title_source": book_title_source,
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
