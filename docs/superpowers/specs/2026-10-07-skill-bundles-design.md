# Design: Skills as installable bundles

Date: 2026-10-07
Status: Approved in chat (user approved design 2026-10-07); spec pending user review

## Goal

A "skill" in Mycelium2 is currently indistinguishable from its files: the
skills page lists one row per markdown file, MCP `skill_list` returns one
entry per file, and there is no way to get a skill out of the store as an
actual usable bundle (markdown + executable scripts). This design makes a
skill a first-class grouped entity — one hub, its companion docs, its
scripts — grouped on the page, grouped in the MCP catalog, and installable
as a zip bundle (Claude Code–compatible layout).

The consumption contract decided in chat: **installable bundles** — the
store becomes a skill distribution point, not just a display surface.

## Background and prior decisions (all user-approved in chat 2026-10-07)

- Groups like books: each skill gets a directory (`/<slug>/skill.md` hub +
  members), mirroring the library's `/<slug>/book.md` + `list_prefix`
  pattern (user-approved).
- Scripts are raw payload files, not fenced-block markdown concepts
  (user-approved). The existing `pdf-to-markdown-scripts.md` fenced-block
  mechanism is retired; verification moves into the hub manifest.
- Zip is the bundle format (chosen over tar.gz for no-flag unzipability on
  Windows/macOS/Linux; tar+flate2 already exist in `mycelium-store` for
  backups, zip needs a new dependency — `zip` crate, default-features off,
  `deflate` only).
- Bundles are Claude-Code-shaped: the hub is emitted as `SKILL.md`
  (Claude Code's skill-entry filename), companions keep their markdown
  names, scripts unpack verbatim at their manifest paths.

## Data model

### Directory layout (skills shelf)

```
/<slug>/skill.md            hub concept, type: Skill — THE skill
/<slug>/<companion>.md      supporting concepts (reference / options / license)
/<slug>/scripts/<name>      raw script payload (real bytes; NOT a concept)
```

- The hub is identified structurally: the concept `/<slug>/skill.md`.
  No frontmatter flag needed — mirrors books' `book.md`.
- Grouping rule for every listing surface: entries whose path is
  `/<slug>/*.md` belong to skill `<slug>` iff `/<slug>/skill.md` exists.
  Paths under a directory without `skill.md`, and root-level `.md` files,
  remain flat entries (compat: admin-created extras, legacy private
  skills, legacy packaged v1 content until migrated).
- Reserved-name note: `index.md` at any level is system-maintained
  (ConceptStore regenerates it); a hub dir listing must skip it.

### Hub manifest (new known frontmatter field)

`Frontmatter` currently ignores unknown keys on parse (lenient), so a
one-off `skill:` manifest inside packaged bytes would not survive
`Concept::parse` for programmatic consumers. Instead, extend `Frontmatter`
with a known field — same precedent as `book:`/`chapter_index:`:

```yaml
skill:
  version: 1
  files:
    - {path: conventions.md, role: reference}
    - {path: scripts/convert.py, role: script, md5: 0f34…}
```

- `files[].path` is relative to the skill dir; MUST resolve inside it
  (reject `..` segments, absolute paths, empty). `.md` paths named in the
  manifest are companions (also listed by the directory scan); non-`.md`
  paths are raw script payloads. Bundle assembly (below) treats the
  manifest as the authoritative list for scripts; manifest entries that
  name nonexistent payloads fail loudly at assembly.
- `role` values: `script` | `reference` | `example` (display only; not
  enforced).
- `md5` is optional per file; when present, bundle install/verify checks
  it (this replaces the md5 manifest concept `pdf-to-markdown-scripts.md`).
- A hub without `skill.files` is legal: grouping still works structurally
  (directory scan), the bundle just contains its `.md` members only.

### New public types (mycelium-core, `concept.rs`)

- `SkillManifest { version: u32, files: Vec<SkillFile> }`,
  `SkillFile { path: String, role: SkillRole, md5: Option<String> }` —
  serde structs; `Frontmatter.skill: Option<SkillManifest>` with
  `#[serde(default, skip_serializing_if = "Option::is_none")]`.
- `SkillRole` enum with `#[serde(other)]`-style tolerance or fallback to
  `reference` on unknown values (lenient, matching OKF posture).
- Validation helper `SkillManifest::validated(base: &str) -> Result<_,
  ConceptError>`: per-file path checks (must be relative, no `..`, no
  backslashes, resolves under base, non-empty, no NUL).

## Packaged-seed migration (seed v2)

`SKILLS_SEED_VERSION` → `"2"`. On stale/missing marker:

1. Build the new layout concepts from the packaged assets (repo files
   reorganized under `packaged-skills/pdf-to-markdown/` into the nested
   shape below) and one batched `put_batch`.
2. Script bytes move from fenced blocks inside
   `pdf-to-markdown-script-*.md` concepts to raw FileRepo payloads
   written to the skills scope's FileRepo at
   `/pdf-to-markdown/scripts/<name>` (7 payloads: convert.py,
   postprocess.py, docling_page_span.py, html_cleanup.py, inspect_pdf.py,
   setup_venv.sh, requirements.txt). Payloads are written directly via
   `FileRepo::write` with the service-key scope — NOT via ConceptStore
   (`put` rejects non-concepts by design; index/registry/search stay
   clean, the exact property stack text already relies on).
3. Delete the 12 legacy flat paths `/pdf-to-markdown*.md` via the
   existing delete API (idempotent: NotFound is OK — fresh boots have
   nothing to delete). The legacy `scripts.md` fenced-block extraction
   rules concept dies with the migration.
4. Write the marker.

New packaged layout (repo + shelf paths identical):

```
packaged-skills/pdf-to-markdown/
  pdf-to-markdown/skill.md             # former pdf-to-markdown.md + manifest
  pdf-to-markdown/conventions.md       # former …-conventions.md
  pdf-to-markdown/docling-options.md   # former …-docling-options.md
  pdf-to-markdown/license.md           # former …-license.md
  pdf-to-markdown/scripts/convert.py …  # raw payloads (from script concepts)
```

The hub `skill.md` frontmatter carries the manifest with md5s for all 7
scripts (same md5s the legacy `scripts.md` concept held). Companion `.md`
byte-content is unchanged apart from the hub's filename-specific links
(`/pdf-to-markdown-conventions.md` → `/pdf-to-markdown/conventions.md`)
and the retired scripts concept's removal from the body/index.

Semantics unchanged from v1: fail-fast on error, admin edits to packaged
paths survive marker match, all-or-nothing on refresh, extra admin skills
never touched. `seed_packaged_skills` stays the single entry point
(`main.rs:75` call site unchanged in shape).

## Web: skills page (grouped display)

`api.rs::skills_view` keeps one `list()` per scope but hands the page the
flat entries; grouping happens in `pages.rs`:

- Partition entries by top-level directory. A partition with a `skill.md`
  renders as one group: hub row (title, link, admin edit/delete) with
  companions and manifest scripts indented beneath it. Scripts render as
  plain labels from the manifest (path — no decryption needed to name
  them; they are not concepts).
- Root-level entries and non-skill directories render exactly as today
  (flat rows) — zero regression for private skills / legacy content.
- Depth guard: only one nesting level is treated as a skill dir
  (`/<slug>/`); deeper paths belong to the nearest top-level skill or
  render flat.

## MCP surfaces

- `skill_list` → one entry per logical skill: for the skills shelf, hubs
  only (`/pdf-to-markdown/skill.md`), plus flat `type: Skill` roots;
  private side unchanged in shape but subject to the same grouping rule
  so a user bundle gets identical treatment. Title returned is the hub
  title. Old flat entries disappear from the catalog — the breaking
  change this design intends.
- `skill_get` → resolve `/{slug}/skill.md` first (when the arg has no
  `.md` suffix); fall back to legacy `/{slug}.md` exact-path lookup, then
  private-shelf then global-shelf ordering as today. An arg that already
  names an exact path (`/pdf-to-markdown/skill.md` or any companion)
  resolves to that concept directly. Companions remain readable via
  `read_concept` — no new MCP tool for scripts/scripts-content (YAGNI).

## Install / export

### Web: `GET /api/v1/skills/{slug}/bundle`

- Auth: session (GET, CSRF-exempt like all GETs), the same visibility
  rule as the skills page — any user may bundle a global skill; a
  private skill only from its own bundle.
- Response: `application/zip`, `Cache-Control: no-store`,
  filename `<slug>-skill.zip`.
- Contents (deterministic — fixed order: `SKILL.md`, companions sorted,
  scripts sorted; fixed mtime; no extra metadata):
  - `SKILL.md` — hub markdown verbatim.
  - Companions listed under the skill dir — markdown verbatim.
  - Scripts at their manifest paths (relative to the skill root) — raw
    payload bytes, md5-verified during assembly when the manifest lists
    one (md5 mismatch → 500 with a logged detail, never a bad bundle).
- Hub without a manifest: bundles its `.md` members, no scripts.
- Slug that resolves only to a legacy flat skill: 404 (bundles are a
  nested-layout feature; the flat skill remains viewable/copyable by
  hand). Rationale: a flat "skill" carries no scripts, so bundling adds
  nothing over the concept viewer.

### CLI: `mycelium2-cli skill export <slug> [--data-dir] <out-dir>`

- Admin tool over direct store access (backup/verify precedent). Writes
  the same layout to disk (outdir root: `SKILL.md`, companions, scripts).
- Same manifest validation + md5 discipline as the web bundle; refuses
  legacy/flat slugs with the same rationale.

### Zip dependency

`zip` crate, newest available version (verify at implementation; pin
that), default features OFF, `deflate` only (no bzip2/zstd/aes).
Determinism contract regardless of version: explicit file options —
`last_modified_time` fixed, unix permissions fixed 0644, fixed file
order (SKILL.md, companions sorted, scripts sorted) — asserted by an
md5-equality test over two builds of the same skill.

## Security

- Manifest paths validated (traversal, backslashes, NUL) before any
  payload write, bundle assembly, or CLI export; validation lives in
  mycelium-core so all three surfaces share one implementation.
- The server only ever *builds* bundles from store reads — no upload,
  no extraction of user-supplied archives anywhere in this design.
- Scripts stay raw payloads: never in `scope_files`, never in the search
  index, never surfaced as concepts (same contract as library stack
  text).
- Bundle endpoint inherits session auth; zip served with
  `Cache-Control: no-store` (backup-download precedent); zip filename
  built from the validated slug, never raw user input.
- Script bytes are served back verbatim to any user who can see the
  skill — scripts are part of the skill's public content by design
  (global shelf is global-read). Private-bundle skills' scripts are
  readable only by that user.

## Testing

- **core**: manifest parse/reject cases (traversal `../`, absolute,
  backslash, empty, duplicate paths, unknown role tolerance); round-trip
  with `skill:` field via `Concept::parse`/serialize.
- **web/packaged_skills_integration**: v2 migration on a v1-marked shelf
  (legacy 12 gone, nested present, scripts materialize byte-exact w/
  md5), fresh-boot seed (nested only, idempotent, marker written),
  admin edit survives same-version re-seed, extra admin skill untouched.
- **web/pages** (unit): grouped rendering — hub row + indented
  companions/script labels; flat and legacy entries unchanged; depth
  guard.
- **web/mcp integration** (`mcp_integration.rs` + tools tests):
  `skill_list` one-entry-per-skill; `skill_get` hub-slug resolution +
  legacy fallback + exact companion path; two-user isolation unchanged.
- **web bundle endpoint** (`web_integration.rs`): headers, zip bytes
  round-trip (unzip → `SKILL.md` + script bytes md5-equal), no-store,
  404 flat/legacy, private-skill visibility (other user 404), traversal
  manifest rejected at seed (hub with bad path never lands),
  deterministic rebuild — two bundle builds of the same skill are
  byte-identical.
- **store**: script payload write/read/delete in the skills scope
  directly via FileRepo + service key (mirrors stack-text test shape).
- **cli**: export writes layout, md5 verification failure refuses
  (nonzero exit), legacy slug refusal.

## Docs

- `docs/skills.md` (or the relevant guide): skill = directory + hub +
  manifest; bundle download usage; Claude Code install snippet
  (unzip into `~/.claude/skills/<slug>/`); CLI export.
- `docs/deployment.md`: seed v2 migration note (one-time; legacy flat
  paths deleted, scripts become payloads).
- `DESIGN.md`: skills-bundle paragraph replacing/extending the packaged
  skills one.
- AGENTS.md workspace blurb updated when implemented.

## Non-goals

- No skill *import*/upload in this design (server-side zip ingestion is a
  separate feature with its own security surface; the CLI + page cover
  authoring today).
- No streaming chat/trace work, no MCP tool for script contents, no
  mobi/epub pipeline.
- No migration for user private bundles (legacy flat private skills keep
  working; users can re-save them into a nested dir if they want
  bundling).

## Verification plan

Full verify suite per AGENTS.md → judge subagent (spec-vs-impl) →
code-reviewer → re-verify. Live check on the dev deployment: `/skills`
shows one grouped entry for pdf-to-markdown; download the zip, unzip
into `~/.claude/skills/`, run the skill end-to-end (convert.pdf);
`skill_list` over MCP shows one entry.