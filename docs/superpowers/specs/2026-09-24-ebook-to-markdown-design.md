# Design: ebook-to-markdown — packaged ebook conversion skill

Date: 2026-09-24
Status: Approved (user approved approach 1 and design sections in chat, 2026-09-24)

## Goal

A second packaged book-conversion skill, `ebook-to-markdown`, shipped in the
mycelium2 global skills shelf alongside `pdf-to-markdown`. A fresh install gets
both skills on first boot with zero manual steps, so any user can convert an
**epub, mobi, azw3 or fb2 book** into the same ingest-ready enhanced markdown
the library ingest path expects — stable `{#ch-N-slug}` / `{#sec-N-M-slug}`
heading IDs, machine-readable TOC, YAML frontmatter, figure manifest.

This completes the original acceptance: "converting a PDF **or mobi** book
format into Markdown". The PDF half shipped 2026-09-23
(`2026-09-21-packaged-book-skills-design.md`); the seed mechanism it built is
reused unchanged apart from a version bump.

## Scope correction (user-approved, 2026-09-24)

This is a **public project**: the packaged skills exist so new users of a fresh
install can import books of their own. The engine must therefore handle the
whole real-world ebook ecosystem — epub2/epub3, all mobi dialects (KF7, KF8,
HUFF/CDIC), azw3, fb2 — not a private library's tame subset. DRM'd inputs must
fail cleanly with a clear message, never emit garbage.

## Approach (approved: approach 1 — calibre as the engine)

**calibre's `ebook-convert` is the conversion engine**, mirroring how the PDF
skill treats Docling: a heavy external converter driven by light deterministic
Python scripts. Rationale, in the order that mattered:

- **Reader robustness is the product.** calibre's readers are battle-tested
  against decades of real-world ebook mess; a hand-rolled KF7-only decoder
  (the rejected approach 2) would break public users' KF8/AZW3/HUFF-CDIC
  books. calibre also fails cleanly on DRM.
- **The install story keeps the shelf-materialization property.** calibre
  distributes an isolated no-root install
  (`linux-installer.sh … install_dir=~/calibre-bin isolated=y`; x86_64 and
  ARM; GLIBC 2.34+, libegl1/libopengl0 on headless servers). `setup_venv.sh`
  provisions both the Python venv and `~/calibre-bin` in userspace.
- **The deterministic postprocess layer is ours either way**, so calibre does
  not remove any of the work — it removes only reader risk.

Rejected alternatives: pure-Python venv-only readers (correctness treadmill
against the mobi zoo, wrong for public scope); pandoc for epub + custom mobi
(no mobi support, two engines, loses spine-order determinism).

## Pipeline

Same shape as the PDF skill: scripts in a dedicated venv
(`~/.venvs/ebook-to-markdown/`), each script prints exactly one JSON line on
stdout (`{"ok": true, …}` / `{"ok": false, "error": "…"}`) and progress to
stderr, run via the venv's python. A new skill directory materialized from the
shelf; nothing in the Docker image or server changes except the packaged set
and the seed version.

1. **`inspect_ebook.py --input <file>`** — parses the container directly with
   stdlib (`zipfile` + OPF for epub; record-0 header + EXTH for mobi):
   title, author, language, TOC presence (epub nav / NCX; mobi filepos /
   EXTH TOC), spine size, image count, **DRM status** (epub
   `META-INF/encryption.xml`; mobi encryption field ≠ 0), format dialect
   (mobis: file version, KF8 boundary flag). Reported to the user; DRM →
   stop with a clear error before any conversion.
2. **`convert.py --input <file> --output-dir <dir>`** — drives
   `ebook-convert <input> <out>/book.htmlz`, then unpacks the HTMLZ
   (`index.html` + `images/`) into `raw/` (layout below). The
   HTML→markdown pass and the enhancement pass are separate steps so
   each stays a single-purpose script. `ebook-convert` runs in the
   venv's Python so calibre's own dependencies are its isolated
   install's problem.
3. **`postprocess.py --raw <raw-dir> --output <book>.md`** — reads
   `raw/index.html` + spine structure and applies the enhancements:
   HTML→markdown, frontmatter, TOC, stable anchors, figure manifest.

### Output structure

Under `<output-dir>/<book-slug>/`, mirroring the PDF skill:

```
<book-slug>/
├── raw/
│   ├── index.html          # calibre HTMLZ unpacked
│   ├── images/
│   └── conversion.json     # provenance: source, calibre version, flags
├── <book>.md               # the enhanced readable book
└── figures/manifest.json
```

## Chapter & section structure (load-bearing)

Two-tier structure rule, same philosophy as the PDF skill — structure from the
book's own outline, not guesswork:

- **Tier 1 — container TOC (preferred).** Epub nav (`properties="nav"`) or
  NCX; mobi filepos chapter marks / EXTH TOC. Top-level TOC entries →
  chapters (`ch-N-slug`, N = TOC order 1-based); nested entries → sections
  (`sec-N-M-slug`). Aligned into the converted text by matching TOC titles
  against headings at the corresponding spine position — deterministic,
  unit-testable.
- **Tier 2 — fallbacks.** No container TOC → chapter boundaries from an
  `h1`/`h2` heading scan over the spine-ordered concatenated HTML. No
  headings at all (rare) → one chapter per spine file, titled from its first
  heading or the filename.
- **Front matter** (cover, title page, copyright, the TOC itself): content
  before the first chapter → `sec-0-N-*` anchors (PDF-conventions rule).
- **Sections when the TOC lists none**: from `h2`/`h3` headings inside the
  chapter, else the chapter is section-less (valid — ingest only requires
  chapters).

Ingest compatibility: `mycelium-librarian/src/extract.rs` parses `#`/`##`
headings and reads chapter identity from `ch-`/`sec-` anchor IDs in the stack
text — this design feeds it exactly that.

## HTML → markdown conversion & cleanup

calibre emits per-spine-file presentational HTML (`div`/`span` + inline
styles, `<br>` runs, soft hyphens). Deterministic Python on the unpacked
HTMLZ:

- **Parse with `lxml`** in spine order, concatenating with per-file
  boundaries tracked (the Section-2 alignment anchors).
- **Semantic mapping**: `h1–h6` → `#`–`######`; `p` → paragraphs;
  `ul/ol` → lists; `table` → markdown tables; `blockquote` → `>`;
  `pre/code` → fenced code; `em/strong` → `*…*`/`**…**`. Drop presentational
  attributes and inline styles.
- **Calibre-idiosyncrasy cleanup** (dedicated module, the analog of the PDF
  skill's `html_cleanup.py`): soft-hyphen joins, page-break noise,
  zero-width chars, nested-paragraph de-nesting; gotcha list grows from real
  books.
- **Images**: extracted to `raw/images/`, relative links
  (`![Image](images/…)`), manifest built in postprocess; cover kept but not
  in chapter body flow (it is front matter).
- **Whitespace/typography**: collapse blank-paragraph runs, strip `&nbsp;`
  runs; curly-quote normalization optional, **off by default** (do not
  mangle content).
- **Fail-soft**: an unparseable spine file → warning, skipped, its TOC entry
  dropped (chapter count shrinks accordingly) — never a broken
  half-document.

## Output contract vs. pdf-to-markdown conventions

Identical except where noted (the conventions doc states these):

- YAML frontmatter: `title`, `author`, `language`, `source_ebook` (path),
  `converted_date`, `converter` (calibre + version), `mode`, `chapter_count`,
  `toc` (id/title/level). **No `page_count`.**
- Machine-readable TOC linking `ch-*` anchors; nested for sections.
- Stable heading IDs `ch-N-slug` / `sec-N-M-slug` / `sec-0-N-slug` —
  identical dialect, same slug rules.
- **No page-range citations** — ebooks have no pages; the conventions doc
  documents the absence rather than emitting fake locators.
- Figure manifest as in the PDF skill.

## Packaging & seeding

- New repo directory `crates/mycelium-web/packaged-skills/ebook-to-markdown/`
  holding the concept set (main skill doc, conventions doc, scripts manifest,
  one concept per script — `inspect_ebook.py`, `convert.py`,
  `postprocess.py`, plus shared modules `ebook_html.py` and `mobi_probe.py`,
  `setup_venv.sh`, `requirements.txt` — and a license/provenance note),
  copied byte-exact from the staged set.
- `packaged_skills.rs` gains the second directory's `include_str!` table.
- **`SKILLS_SEED_VERSION` → "2"**: fresh boots seed both skills; existing
  deployments refresh the full packaged set on next boot (admin edits
  preserved between bumps — same contract).
- `setup_venv.sh` (skill-local): venv with `lxml` + PyYAML (+ the chosen
  HTML→md helper), calibre isolated install into `~/calibre-bin`, verify
  `ebook-convert --version`. Idempotent.

License note: calibre is GPL; the skill drives it as an external binary (no
code copied), so the skill text carries only calibre's install provenance —
the same relationship the PDF skill has with Docling.

## Testing

- **Unit tests (scripts)**: synthetic minimal epub (stdlib `zipfile` fixture)
  and synthetic KF7 mobi covering TOC alignment, all three fallback tiers,
  front matter, image extraction, DRM refusal, anchor uniqueness.
- **Corpus validation (pre-ship gate)**: the 168 epubs + 63 mobis in
  `/home/echo/documents/` run through convert + postprocess; success rate,
  chapter-count sanity, anchor uniqueness reported per file. Earns the
  public-robustness claim on real files before packaging.
- **Server-side TDD** (`packaged_skills_integration.rs` extended): seed
  produces the new packaged paths alongside the existing 12; byte-exactness
  guard extended to the new scripts' md5 manifest; `SKILLS_SEED_VERSION = 2`
  bump refresh proven.
- **Live verification** per the established deploy chain (CI green → GHCR →
  LXC deploy → shelf lists both skills as admin and non-admin, script
  concepts md5-verified).

## Non-goals

- No changes to PDF skill content (its own minors remain separately queued).
- No ingest-endpoint or schema changes — output is already ingest-ready
  markdown.
- No per-format sub-skills (one skill covers all calibre-readable ebook
  formats).
- No DRM circumvention of any kind: DRM is detected, reported, and refused.