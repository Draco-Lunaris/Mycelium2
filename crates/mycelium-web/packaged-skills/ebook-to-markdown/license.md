---
type: Skill
title: ebook-to-markdown license
description: 'The licensing and provenance note for the ebook-to-markdown skill: calibre is GPL-3.0 software driven only as an external binary (no code copied), while the skill''s own scripts and docs are MIT OR Apache-2.0.'
tags:
- ebook-to-markdown
- calibre
- license
timestamp: 2026-10-02T00:00:00Z
---

# ebook-to-markdown license

The license and provenance note for the [ebook-to-markdown](/ebook-to-markdown/skill.md) skill. Unlike the pdf-to-markdown bundle, this skill ships **no proprietary LICENSE.txt** — this note is the whole of its licensing story, and the skill's own files are open source under the repository's licenses.

## calibre (the engine) — GPL-3.0, driven as an external binary

The conversion engine, **calibre**, is free software under the **GNU General Public License v3**. This skill does not copy, link with, or derive from calibre: it executes the standalone `ebook-convert` binary (isolated no-root install at `~/calibre-bin/`) as a separate subprocess and consumes its output files. No calibre source code appears anywhere in this skill's scripts or docs. Driving an external binary is the same relationship the [pdf-to-markdown](/pdf-to-markdown/skill.md) skill has with Docling — the GPL governs calibre, not this skill.

## The skill itself — MIT OR Apache-2.0

This skill's scripts and documentation (`inspect_ebook.py`, `convert.py`, `ebook_html.py`, `postprocess.py`, `setup_venv.sh`, `requirements.txt`, and every doc in this shelf) are licensed **MIT OR Apache-2.0**, at your option — the same dual license as the mycelium2 repository (`LICENSE-MIT`, `LICENSE-APACHE`).

## Output books belong to their authors

A converted `book.md` is a derivative work of the source book; its copyright remains with the book's authors and publishers. This skill converts books at their owner's request, refuses DRM-protected books outright, and grants no rights over the books it processes.