---
type: Skill
title: pdf-to-markdown script — docling_page_span.py
description: Verbatim copy of the pdf-to-markdown skill's python script `docling_page_span.py`. MarkdownDocSerializer subclass replacing Docling page-break tokens with page-span <span> markers (imported by convert.py). Extract the fenced block byte-exact; verify md5 `d9d548923cfed4b2dc9b0e0679f69bf0` against the [scripts manifest](/pdf-to-markdown-scripts.md).
tags:
- pdf-to-markdown
- docling
- script
- packaged
timestamp: 2026-09-21T00:00:00Z
---

# pdf-to-markdown script — docling_page_span.py

Byte-exact copy of `docling_page_span.py` from the [pdf-to-markdown](/pdf-to-markdown.md) skill (source tree: `scripts/docling_page_span.py`). **no trailing newline**

Extract the fenced block below **exactly**: strip the opening ` ```` ` line and the closing ` ```` ` line, keep every byte between them — including the final line's missing newline if the manifest says "no trailing newline" (an editor that silently appends one is harmless for these scripts, but the md5 will differ).

`4887 bytes, md5 d9d548923cfed4b2dc9b0e0679f69bf0`

````
#!/usr/bin/env python3
"""Page-span adapter for the pdf-to-markdown skill (Docling engine).

postprocess.py aligns the PDF's own outline chapters to the markdown body using
``<span id="page-N-M">`` page spans (SPAN_BOTH_RE, postprocess.py:49), where N is
the 0-based PDF page index. Docling's document model carries 1-based ``page_no``
provenance on every element and emits page-break tokens at page boundaries, but
the stock markdown serializer discards the page number (it replaces every token
with one static string). This module subclasses the serializer to recover the
per-page number and emit the exact span format postprocess.py expects.

Emitted span format: ``<span id="page-{next_page-1}-{idx}"></span>``
  - next_page: the 1-based Docling page number of the content following the break
  - next_page-1: the 0-based PDF page index (pypdfium2 outline is 0-based)
  - idx: a sequential counter (ignored by postprocess.py; only N matters)
A leading ``page-0-0`` span is prepended because Docling emits no page-break
token before page-1 content, so page 0 would otherwise be unaddressable.

Referenced image links are produced by materializing pictures via
``_make_copy_with_refmode`` (the same helper DoclingDocument.save_as_markdown
uses) with ``reference_path`` set, so links are relative (``images/...``).
"""

from pathlib import Path

from docling_core.transforms.serializer.markdown import (
    MarkdownDocSerializer,
    MarkdownParams,
)
from docling_core.transforms.serializer.common import create_ser_result
from docling_core.types.doc.base import ImageRefMode

# Docling's page-break token (docling_core/.../serializer/common.py:713):
#   #_#_DOCLING_DOC_PAGE_BREAK_{prev_page}_{next_page}_#_#
# _get_page_breaks() (common.py:715) yields (full_match, prev_page, next_page).


class PageSpanMarkdownSerializer(MarkdownDocSerializer):
    """MarkdownDocSerializer that emits ``<span id="page-N-M">`` at each page
    boundary instead of a static page-break placeholder.

    The stock serializer (markdown.py:947) replaces every page-break token with
    ``self.params.page_break_placeholder or ""``, discarding prev/next page
    numbers. We override serialize_doc to format each token individually.
    """

    def serialize_doc(self, *, parts, **kwargs):
        text_res = "\n\n".join(p.text for p in parts if p.text)
        if self.requires_page_break():
            idx = 0
            for full_match, _prev_page, next_page in self._get_page_breaks(text=text_res):
                page_index_0based = next_page - 1  # Docling 1-based -> PDF 0-based
                span = f'<span id="page-{page_index_0based}-{idx}"></span>'
                text_res = text_res.replace(full_match, span, 1)
                idx += 1
            # Page 0 has no preceding page-break token; make it addressable.
            text_res = f'<span id="page-0-0"></span>\n\n' + text_res
        return create_ser_result(text=text_res, span_source=parts)


def export_to_markdown_with_page_spans(
    doc,
    *,
    raw_md_path: Path,
    images_dir: Path,
    traverse_pictures: bool = False,
    escape_html: bool = True,
    disable_images: bool = False,
) -> str:
    """Serialize ``doc`` to markdown with ``<span id="page-N-M">`` page spans.

    Pictures are materialized to ``images_dir`` and referenced by relative links
    (``images/...``) unless ``disable_images`` is True, in which case image
    placeholders are emitted and no files are written.

    Args:
        doc: the converted DoclingDocument.
        raw_md_path: destination path of the raw markdown (its parent is the
            reference_path for relative image URIs).
        images_dir: absolute directory to write image files into.
        traverse_pictures: recurse into PictureItems (needed for full-page
            OCR'd pages where text is nested under a picture).
        escape_html: passed through to MarkdownParams (spans are inserted after
            per-part escaping, so they survive unescaped either way).
        disable_images: emit ``<!-- image -->`` placeholders, write no files.
    """
    reference_path = raw_md_path.parent

    if disable_images:
        new_doc = doc
        image_mode = ImageRefMode.PLACEHOLDER
    else:
        images_dir.mkdir(parents=True, exist_ok=True)
        new_doc = doc._make_copy_with_refmode(
            artifacts_dir=images_dir,
            image_mode=ImageRefMode.REFERENCED,
            page_no=None,
            reference_path=reference_path,
        )
        image_mode = ImageRefMode.REFERENCED

    params = MarkdownParams(
        image_mode=image_mode,
        image_placeholder="<!-- image -->",
        page_break_placeholder="",  # non-None => page-break nodes are emitted
        escape_html=escape_html,
        traverse_pictures=traverse_pictures,
    )
    return PageSpanMarkdownSerializer(doc=new_doc, params=params).serialize().text

````
