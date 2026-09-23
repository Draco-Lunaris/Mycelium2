---
type: Skill
title: pdf-to-markdown script — html_cleanup.py
description: Verbatim copy of the pdf-to-markdown skill's python script `html_cleanup.py`. HTML-to-markdown cleanup for run-on HTML blobs (imported by postprocess.py). Extract the fenced block byte-exact; verify md5 `9fa07ded3cf2f877a12020f98f65995d` against the [scripts manifest](/pdf-to-markdown-scripts.md).
tags:
- pdf-to-markdown
- docling
- script
- packaged
timestamp: 2026-09-21T00:00:00Z
---

# pdf-to-markdown script — html_cleanup.py

Byte-exact copy of `html_cleanup.py` from the [pdf-to-markdown](/pdf-to-markdown.md) skill (source tree: `scripts/html_cleanup.py`). **no trailing newline**

Extract the fenced block below **exactly**: strip the opening ` ```` ` line and the closing ` ```` ` line, keep every byte between them — including the final line's missing newline if the manifest says "no trailing newline" (an editor that silently appends one is harmless for these scripts, but the md5 will differ).

`7938 bytes, md5 9fa07ded3cf2f877a12020f98f65995d`

````
#!/usr/bin/env python3
"""HTML-body cleanup for marker's degraded output (e.g. Python Crash Course,
where marker emits the body as one giant run-on line of <p>/<h1>/<i>/<b>/<pre>/
<table> HTML + entities). Converts to readable markdown. Leaves clean-markdown
books essentially unchanged (they have ~0 HTML tags); protects fenced code
blocks and <pre> so '<stdio.h>' / 'a < b' in code is not stripped as a fake tag.
"""
import re, html, sys

PAGE_SPAN_RE = re.compile(r"<span id=['\"]page-(\d+)-(\d+)['\"]>\s*</span>")
PRE_RE = re.compile(r"<pre[^>]*>(.*?)</pre>", re.S | re.I)
# Fences are matched line-by-line below (a real markdown fence is a full line
# starting with ``` or ~~~). A line-based scan avoids misreading marker's
# Python-traceback underlines ('~~~~~~~^^^') as a ~~~ fence delimiter, which
# would protect a huge HTML-body region verbatim.
H_RE = re.compile(r"<(h[1-6])[^>]*>(.*?)</\1>", re.S | re.I)
P_RE = re.compile(r"<p[^>]*>(.*?)</p>", re.S | re.I)
LI_RE = re.compile(r"<li[^>]*>(.*?)</li>", re.S | re.I)
BLOCKQUOTE_RE = re.compile(r"<blockquote[^>]*>(.*?)</blockquote>", re.S | re.I)
TABLE_RE = re.compile(r"<table[^>]*>(.*?)</table>", re.S | re.I)
ROW_RE = re.compile(r"<tr[^>]*>(.*?)</tr>", re.S | re.I)
CELL_RE = re.compile(r"<t[hd][^>]*>(.*?)</t[hd]>", re.S | re.I)
IMG_RE = re.compile(r"<img[^>]*src=['\"]([^'\"]+)['\"][^>]*>", re.I)
A_RE = re.compile(r"<a[^>]*href=['\"]([^'\"]+)['\"][^>]*>(.*?)</a>", re.S | re.I)
BR_RE = re.compile(r"<br\s*/?>", re.I)
# Strip only KNOWN HTML tags (a backstop for leftovers the block/inline
# converters miss). Must NOT match markdown autolinks <http://...> (scheme has
# ':/' not a tag name), <stdio.h> includes, or shell redirects < file: those
# start with a word that is not an HTML tag, so they are preserved.
KNOWN_TAGS = (r"p|h[1-6]|i|em|b|strong|code|a|img|pre|blockquote|ul|ol|li|"
              r"table|thead|tbody|tr|th|td|span|sup|sub|br|div|html|body|"
              r"figure|figcaption|hr|font|small|mark|u|s|del|ins|kbd|samp|"
              r"var|cite|q|abbr|address|article|section|header|footer|nav|"
              r"aside|main|details|summary|dl|dt|dd|caption|colgroup|col")
STRAY_TAG_RE = re.compile(r"</?(?:" + KNOWN_TAGS + r")\b[^>\n]*>", re.I)

# token scheme: \x00<n>\x00 ... use unique placeholders
def _protect(text, pattern, prefix):
    store = []
    def repl(m):
        store.append(m.group(0)); return f"\x00{prefix}{len(store)-1}\x00"
    return pattern.sub(repl, text), store

def _restore(text, prefix, store, transform=lambda s: s):
    for i, s in enumerate(store):
        text = text.replace(f"\x00{prefix}{i}\x00", transform(s))
    return text

def _inline(text):
    text = IMG_RE.sub(lambda m: f"![]({m.group(1)})", text)
    text = A_RE.sub(lambda m: f"[{m.group(2)}]({m.group(1)})", text)
    text = re.sub(r"<(i|em)\b[^>]*>(.*?)</\1>", r"*\2*", text, flags=re.S | re.I)
    text = re.sub(r"<(b|strong)\b[^>]*>(.*?)</\1>", r"**\2**", text, flags=re.S | re.I)
    text = re.sub(r"<code\b[^>]*>(.*?)</code>", r"`\1`", text, flags=re.S | re.I)
    text = re.sub(r"<sup\b[^>]*>(.*?)</sup>", r"\1", text, flags=re.S | re.I)
    text = BR_RE.sub("\n", text)
    return text

def html_to_markdown(text):
    # 1. protect fenced code blocks (clean-markdown books use these). Line-based
    #    scan: a fence is a full line starting with ``` or ~~~, closed by a line
    #    that is just the same token. This does not match marker's mid-line
    #    traceback underlines ('~~~').
    fence_store = []
    lines = text.split("\n")
    out, buf = [], []
    in_fence, fence_tok = False, None
    for line in lines:
        stripped = line.strip()
        m = re.match(r"^(```|~~~)", stripped)
        if not in_fence:
            if m:
                in_fence, fence_tok, buf = True, m.group(1), [line]
            else:
                out.append(line)
        else:
            buf.append(line)
            if re.match(r"^" + re.escape(fence_tok) + r"\s*$", stripped):
                fence_store.append("\n".join(buf))
                out.append(f"\x00F{len(fence_store) - 1}\x00")
                in_fence, fence_tok, buf = False, None, []
    if in_fence:  # unterminated -> put it back unchanged
        out.extend(buf)
    text = "\n".join(out)
    # 2. protect <pre> blocks (marker HTML-blob books); decode entities inside -> fenced code
    pre_store = []
    def pre_repl(m):
        inner = html.unescape(m.group(1))
        pre_store.append(inner); return f"\x00PRE{len(pre_store)-1}\x00"
    text = PRE_RE.sub(pre_repl, text)
    # 3. protect page spans (needed for outline alignment) -> restore verbatim
    pg_store = []
    def pg_repl(m):
        pg_store.append(m.group(0)); return f"\x00PG{len(pg_store)-1}\x00"
    text = PAGE_SPAN_RE.sub(pg_repl, text)
    # 4. decode entities in the remaining (non-protected) text
    text = html.unescape(text)
    # 5. block-level conversions (each emits blank-line-separated blocks)
    def h_repl(m):
        n = int(m.group(1)[1]); return f"\n\n{'#' * n} {_inline(m.group(2)).strip()}\n\n"
    text = H_RE.sub(h_repl, text)
    # <p>: non-capture approach (robust on giant run-on lines where content may
    # contain '<' from decoded entities like &lt;module&gt;). Opening tag and
    # closing tag become paragraph breaks; inline tags in the content are
    # handled by the global _inline pass at step 6.
    text = re.sub(r"<p\b[^>]*>", "\n\n", text, flags=re.I)
    text = re.sub(r"</p\s*>", "\n\n", text, flags=re.I)
    text = LI_RE.sub(lambda m: f"\n- {_inline(m.group(1)).strip()}", text)
    text = BLOCKQUOTE_RE.sub(lambda m: "\n" + "\n".join("> " + ln for ln in _inline(m.group(1)).strip().splitlines() and _inline(m.group(1)).strip().splitlines()), text)
    # tables -> markdown pipe tables
    def tbl_repl(m):
        rows = []
        for rm in ROW_RE.finditer(m.group(1)):
            cells = [_inline(c).strip() for c in (cm.group(1) for cm in CELL_RE.finditer(rm.group(1)))]
            if cells:
                rows.append("| " + " | ".join(cells) + " |")
        if not rows:
            return ""
        # insert header separator after first row (assume first row is thead)
        if len(rows) >= 1:
            sep = "| " + " | ".join(["---"] * (rows[0].count("|") - 1)) + " |"
            rows.insert(1, sep)
        return "\n\n" + "\n".join(rows) + "\n\n"
    text = TABLE_RE.sub(tbl_repl, text)
    # 6. inline leftovers + strip stray HTML tags (guarded: must start with a
    #    letter, so 'a < b' / '<stdio.h>' inside code are protected anyway, and
    #    stray '<' in prose won't match)
    text = _inline(text)
    text = STRAY_TAG_RE.sub("", text)
    # catch any unclosed/malformed <p> or </p> P_RE missed (word-boundary after
    # p so '<prompt>', '<pre>', '<param>' in prose/code are NOT stripped)
    text = re.sub(r"</?p\b[^>\n]*>", "", text, flags=re.I)
    # final entity decode: tag removal can assemble entities that weren't
    # present at step 4 (e.g. '<sup>&</sup>lt;' -> '&lt;'). Fences/pre are still
    # tokens here, so only body prose/entities are affected (correct: '&lt;' in
    # prose is a literal '<').
    text = html.unescape(text)
    # 7. restore protected spans, pre (as fenced code), fences
    text = _restore(text, "PG", pg_store)
    text = _restore(text, "PRE", pre_store, transform=lambda s: "\n```\n" + s.strip("\n") + "\n```\n")
    text = _restore(text, "F", fence_store)
    # 8. tidy whitespace
    text = re.sub(r"[ \t]+\n", "\n", text)          # trailing spaces
    text = re.sub(r"\n{3,}", "\n\n", text)            # collapse blank runs
    return text.strip() + "\n"

if __name__ == "__main__":
    src = sys.argv[1]
    out = sys.argv[2] if len(sys.argv) > 2 else None
    t = open(src, encoding="utf-8").read()
    res = html_to_markdown(t)
    if out:
        open(out, "w", encoding="utf-8").write(res)
        print(f"wrote {len(res)} chars to {out}")
    else:
        print(res[:4000])

````
