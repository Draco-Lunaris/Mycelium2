#!/usr/bin/env python3
"""ebook_html — HTML→markdown core module (ebook-to-markdown stage 3).

Pure library: one spine file's (calibre-normalized) XHTML -> deterministic
markdown. No I/O, no printing; Task 5's postprocess.py imports
html_to_markdown() and cleanup().

Mapping (attributes dropped wholesale; only img@src is read): h1-h6 ->
#–###### (single line: br in a heading joins with a space), p -> blank-
line paragraphs, ul/ol -> "- "/"1. " items (block children of an item
content-indented, never fused; stray non-li content kept in place),
table -> GFM table (header = first row: th cells as-is, else bolded)
with "| --- |", blockquote -> "> " lines, pre -> fenced verbatim (br ->
newline; fence escalates past any fence-shaped run in the content),
em/i -> *…*, strong/b -> **…**, code -> `…`, br -> hard line break, img
-> ![Image](images/<flattened basename>) matching convert.py's images/
layout (angle-bracketed when spaced). head/title/style/script/svg/
template/noscript subtrees are dropped.

Prose escaping: paragraph lines never start with markdown structure —
#/>/bullets, ordered markers (separator-escaped: "1. " -> "1\\."),
setext/thematic runs (incl. ***/___), HTML-block <, leading |, 3+
backtick/tilde runs; a trailing backslash before a break is doubled.
Headings, fenced pre content, table cells exempt; hr's "---" intentional.

cleanup() is fence-aware per CommonMark: a line whose content, after
">" prefixes, list markers and all leading whitespace are stripped, is
solely a backtick run of N >= 3 opens a fence; only a bare run of >= N
closes — 4-backtick fences keep inner ``` verbatim, marker-riding
fences at any indent are tracked, prose ```-leading lines never desync.
Outside fences: zero-widths stripped, soft hyphens removed (split words
rejoined — never across a blank line or into a marker line; continuation
prefixes consumed), NBSP runs -> one space, 3+ newlines -> one blank paragraph. The walk leaves soft hyphens/zero-widths in its
output on purpose (a soft hyphen before a <br> is exactly what cleanup's
join rule expects), so Task 5 chains html_to_markdown -> cleanup.

HtmlError is raised only for totally unparseable input: lxml yields no
usable content and an html.parser fallback read also finds none.
"""

import posixpath
import re
from html.parser import HTMLParser
from urllib.parse import unquote

import lxml.html
from lxml.etree import LxmlError


class HtmlError(Exception):
    """The HTML input contained no parseable content at all."""


SKIP_TAGS = frozenset(("head", "title", "style", "script", "svg",
                       "template", "noscript"))  # subtrees dropped whole
HEADING_TAGS = frozenset(f"h{n}" for n in range(1, 7))
LIST_TAGS = frozenset(("ul", "ol"))
# li/tr/td/th too: cells/rows/items met inline are space-bounded.
BLOCK_TAGS = HEADING_TAGS | LIST_TAGS | frozenset((
    "p", "pre", "blockquote", "table", "div", "section", "article",
    "aside", "header", "footer", "nav", "figure", "figcaption", "main",
    "body", "html", "hr", "dl", "dt", "dd", "center",
    "li", "tr", "td", "th"))
INLINE_WRAP = {"em": "*", "i": "*", "strong": "**", "b": "**", "code": "`"}
ZERO_WIDTH = "\u200b\u200c\u200d\u2060\ufeff"
SOFT_HYPHEN_SPLIT = re.compile(r"\xad[ \t]*\n(?!\n)(?:[ \t]|>[ \t]?)*+(?![-*+][ \t]|\d{1,9}[.)][ \t])")
FENCE = "```"
FENCE_LINE = re.compile(r"^[ \t]{0,3}(`{3,})[ \t]*$")
QUOTE_PREFIX = re.compile(r"^[ \t]{0,3}>[ \t]?")
LIST_MARKER = re.compile(r"^[ \t]{0,3}(?:[-*+]|\d{1,9}[.)])[ \t]")
ORDERED_MARKER = re.compile(r"^(\d{1,9})([.)])([ \t])")
ESCAPE_SHAPES = (
    re.compile(r"^(?:#|>)"),                  # ATX heading / blockquote
    re.compile(r"^[-*+][ \t]"),               # bullet marker
    re.compile(r"^[-=][-=]*[ \t]*$"),         # setext run
    re.compile(r"^(?:[*_][ \t]*){3,}[ \t]*$"),  # thematic run (***, ___)
    re.compile(r"^<[A-Za-z/!?]"),             # CommonMark HTML block
    re.compile(r"^\|"),                       # pipe-table row shape
    re.compile(r"^[`~]{3,}"),                 # fence-shaped run
)


def _tag(el):
    """Element tag name; '' for comment/PI nodes (never block tags)."""
    return el.tag if isinstance(el.tag, str) else ""


def _norm_ws(text):
    """Text node -> whitespace runs collapsed to single spaces."""
    return re.sub(r"\s+", " ", text)


def _join_inline(pieces):
    """Inline pieces -> one stripped string (hard breaks survive)."""
    text = "".join(pieces)
    text = re.sub(r" {2,}", " ", text)
    text = re.sub(r" *\n *", "\n", text)
    text = re.sub(r"\n{3,}", "\n\n", text)
    return text.strip()


def _escape_line(line):
    """Escape a prose line's markdown-structure shape. Ordered markers
    escape the separator (digits aren't backslash-escapable in GFM)."""
    m = ORDERED_MARKER.match(line)
    if m:
        return m.group(1) + "\\" + m.group(2) + line[m.end(2):]
    for pattern in ESCAPE_SHAPES:
        if pattern.match(line):
            return "\\" + line
    return line


def _emit_text(text):
    """Paragraph text -> markdown-safe: lines escape-checked; a trailing
    backslash before a hard break doubled (GFM would else eat it)."""
    lines = text.split("\n")
    for i, l in enumerate(lines):
        l = _escape_line(l)
        if i < len(lines) - 1 and (len(l) - len(l.rstrip("\\"))) % 2:
            l += "\\"
        lines[i] = l
    return "\n".join(lines)


def _image_md(el):
    """img -> ![Image](images/<basename>) or '' when src has no name."""
    src = el.get("src") or ""
    base = posixpath.basename(unquote(src.split("#", 1)[0].split("?", 1)[0]))
    if not base:
        return ""
    target = f"images/{base}"
    if " " in target:
        target = f"<{target}>"  # keep spaced basenames one markdown link
    return f"![Image]({target})"


def _inline(el, out, hard_break=True):
    """Append el's inline content (own text, children, tails) to out.
    img/br are handled as the element itself (emit wherever met); a
    block child met inline is space-bounded — words never fuse."""
    tag = _tag(el)
    if tag == "img":
        md = _image_md(el)
        if md:
            out.append(md)
        return
    if tag == "br":
        out.append(" " if not hard_break else "\n")
        return
    if el.text:
        out.append(_norm_ws(el.text))
    for child in el:
        tag = _tag(child)
        if tag in INLINE_WRAP:
            inner = []
            _inline(child, inner, hard_break)
            content = _join_inline(inner)
            if content:
                out.append(INLINE_WRAP[tag] + content + INLINE_WRAP[tag])
        elif tag == "br":
            out.append(" " if not hard_break else "\n")
        elif tag in BLOCK_TAGS:
            out.append(" ")  # boundary: block content never fuses words
            _inline(child, out, hard_break)
            out.append(" ")
        elif tag and tag not in SKIP_TAGS:
            _inline(child, out, hard_break)  # span/font/a/sup/... transparent
        # comments and SKIP subtrees contribute nothing but their tail
        if child.tail:
            out.append(_norm_ws(child.tail))


def _paragraph(el, hard_break=True):
    """Block element's inline content as one paragraph string."""
    out = []
    _inline(el, out, hard_break)
    return _join_inline(out)


def _pre_text(el, parts):
    """pre content in document order; <br> -> newline (M2), SKIP
    subtrees contribute nothing but their tails."""
    if el.text:
        parts.append(el.text)
    for child in el:
        tag = _tag(child)
        if tag == "br":
            parts.append("\n")
        elif tag and tag not in SKIP_TAGS:
            _pre_text(child, parts)
        if child.tail:
            parts.append(child.tail)


def _pre_block(el):
    """pre -> fenced block, content verbatim (whitespace kept)."""
    parts = []
    _pre_text(el, parts)
    text = "".join(parts).strip("\n")
    if not text.strip():
        return ""
    fence = FENCE  # escalate past fence runs seen as cleanup sees them
    while any(_bare_line(l).startswith(fence) for l in text.split("\n")):
        fence += "`"
    return f"{fence}\n{text}\n{fence}"


def _cell(c):
    """th/td -> single-line cell text, pipes escaped."""
    return _paragraph(c).replace("\n", " ").replace("|", "\\|")


def _table_block(el):
    """table -> GFM table: header row (th cells, else bolded first row),
    | --- | separator, then body rows; ragged rows padded with empty cells."""
    rows = [(any(_tag(c) == "th" for c in tr),
             [_cell(c) for c in tr if _tag(c) in ("th", "td")])
            for tr in el.iter("tr")]
    rows = [row for row in rows if row[1]]
    if not rows:
        return ""
    ncols = max(len(cells) for _has_th, cells in rows)

    def pad(cells):
        return cells + [""] * (ncols - len(cells))

    def line(cells):
        return "| " + " | ".join(cells) + " |"

    has_th, header = rows[0]
    if not has_th:
        header = [f"**{cell}**" for cell in header]
    lines = [line(pad(header)), "| " + " | ".join(["---"] * ncols) + " |"]
    lines += [line(pad(cells)) for _has_th, cells in rows[1:]]
    return "\n".join(lines)


def _list_block(el, indent=""):
    """ul/ol -> items; content parts (text, block children, nested
    lists) render in document order — first part rides the marker
    line, the rest content-indented. Stray non-li content kept."""
    ordered = el.tag == "ol"
    lines = []
    buf = []  # stray inline content between/around items

    def flush_stray():
        if buf:
            text = _join_inline(buf)
            buf.clear()
            if text:
                lines.append(indent + _emit_text(text))

    if el.text:
        buf.append(_norm_ws(el.text))
    n = 0
    for child in el:
        tag = _tag(child)
        if tag == "li":
            flush_stray()
            n += 1
            marker = f"{n}. " if ordered else "- "
            pad = indent + " " * len(marker)
            parts = []
            item_buf = []

            def flush():
                if item_buf:
                    text = _join_inline(item_buf)
                    item_buf.clear()
                    if text:
                        parts.append(("text", _emit_text(text).split("\n")))

            if child.text:
                item_buf.append(_norm_ws(child.text))
            for c in child:
                ctag = _tag(c)
                if ctag in BLOCK_TAGS:
                    flush()
                    if ctag in LIST_TAGS:
                        sub = _list_block(c, pad)
                        # strip the pad we asked for, re-added at render
                        parts.append(("block", [l[len(pad):]
                                                for l in sub.split("\n")
                                                if l] if sub else []))
                    else:
                        tmp = []
                        _block(c, tmp)
                        parts.append(("block",
                                      [l for b in tmp for l in b.split("\n")]))
                elif ctag and ctag not in SKIP_TAGS:
                    _inline(c, item_buf)
                if c.tail:
                    item_buf.append(_norm_ws(c.tail))
            flush()
            first = True
            for _kind, payload in parts:
                for bare in payload:
                    lines.append((indent + marker if first else pad) + bare)
                    first = False
        elif tag in BLOCK_TAGS:  # stray block between items (kept, N7)
            flush_stray()
            tmp = []
            _block(child, tmp)
            for b in tmp:
                lines.extend((indent + l) if l.strip() else ""
                              for l in b.split("\n"))
        elif tag and tag not in SKIP_TAGS:
            _inline(child, buf)
        if child.tail:
            buf.append(_norm_ws(child.tail))
    flush_stray()
    return "\n".join(lines)


def _children_blocks(el, blocks):
    """Container's blocks in document order; inline runs between block
    children (stray text, spans, images) flush into paragraphs."""
    buf = []

    def flush():
        if not buf:
            return
        text = _join_inline(buf)
        buf.clear()
        if text:
            blocks.append(_emit_text(text))

    if el.text:
        buf.append(_norm_ws(el.text))
    for child in el:
        tag = _tag(child)
        if tag in BLOCK_TAGS:
            flush()
            _block(child, blocks)
        elif tag and tag not in SKIP_TAGS:
            _inline(child, buf)
        if child.tail:
            buf.append(_norm_ws(child.tail))
    flush()


def _block(el, blocks):
    """Append el's markdown blocks to blocks (document-order dispatch)."""
    tag = _tag(el)
    if not tag or tag in SKIP_TAGS:
        return
    if tag in HEADING_TAGS:
        text = _paragraph(el, hard_break=False)
        if text:
            blocks.append("#" * int(tag[1]) + " " + text)
    elif tag in ("p", "dd"):
        text = _paragraph(el)
        if text:
            blocks.append(_emit_text(text))
    elif tag == "dt":
        text = _paragraph(el)
        if text:
            blocks.append("**" + _emit_text(text) + "**")
    elif tag == "pre":
        text = _pre_block(el)
        if text:
            blocks.append(text)
    elif tag == "blockquote":
        inner = []
        _children_blocks(el, inner)
        if inner:
            blocks.append("\n".join(
                "> " + line if line else ">"
                for line in "\n\n".join(inner).split("\n")))
    elif tag in LIST_TAGS:
        text = _list_block(el)
        if text:
            blocks.append(text)
    elif tag == "table":
        text = _table_block(el)
        if text:
            blocks.append(text)
    elif tag == "hr":
        blocks.append("---")  # intentional thematic break, never escaped
    elif tag == "img":  # img can be the root element itself
        md = _image_md(el)
        if md:
            blocks.append(md)
    else:
        _children_blocks(el, blocks)


class _TextSink(HTMLParser):
    """Fallback reader: collects decoded character data, nothing else."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.pieces = []

    def handle_data(self, data):
        self.pieces.append(data)


def _fallback_text(html_text):
    """html.parser-based salvage read; '' when it finds nothing either."""
    sink = _TextSink()
    try:
        sink.feed(html_text)
        sink.close()
    except Exception:
        return ""
    return "".join(sink.pieces).strip()


def _usable(root):
    """Real text (zero-width-only doesn't count) or a usable image."""
    text = "".join(root.itertext())
    for ch in ZERO_WIDTH:
        text = text.replace(ch, "")
    return bool(text.strip()) or any(_image_md(el) for el in root.iter("img"))


def html_to_markdown(html_text):
    """Ebook XHTML -> markdown (mapping: module docstring). One
    document/fragment per call (an unsplit spine index is the caller's
    to split); a leading XML declaration is dropped. HtmlError only
    when lxml yields nothing usable and the fallback finds none."""
    root = None
    try:
        root = lxml.html.fromstring(
            re.sub(r"^\s*<\?xml[^>]*\?>", "", html_text, count=1))
    except (LxmlError, ValueError):
        root = None
    if root is not None and _usable(root):
        blocks = []
        _block(root, blocks)  # works for bare element or wrapper div
        return ("\n\n".join(blocks) + "\n") if blocks else ""
    text = _fallback_text(html_text)
    if text:
        return text
    raise HtmlError(
        f"no parseable HTML content ({len(html_text)} chars input)")


def _clean_segment(text):
    """cleanup rules for one outside-fence segment (order is pinned)."""
    for ch in ZERO_WIDTH:
        text = text.replace(ch, "")
    text = SOFT_HYPHEN_SPLIT.sub("", text)  # rejoin words split at EOL
    text = text.replace("\xad", "")         # remaining soft hyphens
    text = re.sub(r"\xa0+", " ", text)      # NBSP runs -> one space
    text = re.sub(r"\n{3,}", "\n\n", text)  # blank-paragraph runs -> one
    return text


def _bare_line(line):
    """Line minus leading whitespace, quote prefixes and list markers —
    the fence core shared by cleanup and _pre_block's escalation."""
    core = line
    while True:
        core = re.sub(r"^[ \t]+", "", core)  # all indents: R1/R2 view parity
        m = QUOTE_PREFIX.match(core) or LIST_MARKER.match(core)
        if not m:
            return core
        core = core[m.end():]


def _fence_run(line):
    """Backtick-run length after quote prefixes and list markers are
    stripped (marker-riding fences: N2); 0 otherwise (I2 parity)."""
    m = FENCE_LINE.match(_bare_line(line))
    return len(m.group(1)) if m else 0


def cleanup(markdown):
    """Post-process markdown; fenced code passes through verbatim.
    Length-aware (CommonMark): open on a bare run of N >= 3, close only
    on a run of >= N; quoted and marker-riding fences count too."""
    lines = markdown.split("\n")
    out, seg = [], []
    fence = 0  # open fence's run length; 0 = outside any fence

    def flush():
        if seg:
            out.extend(_clean_segment("\n".join(seg)).split("\n"))
            seg.clear()

    for line in lines:
        run = _fence_run(line)
        if fence:
            if run >= fence:  # close
                fence = 0
                out.append(line)
            else:
                out.append(line)  # verbatim content of the open fence
        elif run:
            flush()
            fence = run
            out.append(line)
        else:
            seg.append(line)
    flush()
    return "\n".join(out)