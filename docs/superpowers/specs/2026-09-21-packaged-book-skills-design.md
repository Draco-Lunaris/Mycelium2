# Design: Packaged book-conversion skills, seeded on first boot

Date: 2026-09-21
Status: Approved (user approved design in chat 2026-09-21)

## Goal

A brand-new Mycelium2 Docker deployment must have working book-conversion
functionality out of the box: the `pdf-to-markdown` skill present in the
global skills shelf on first boot, with zero manual steps. The skill converts
a PDF book into ingest-ready enhanced markdown (stable `{#ch-N-slug}` /
`{#sec-N-M-slug}` heading IDs, TOC, page-range citations, figure manifest) —
the exact format the library ingest path expects.

Mobi/epub conversion is explicitly out of scope for this design; it becomes a
separate follow-up project (no pipeline exists today) and will slot into the
same mechanism when authored.

## Background and prior decisions (all user-approved)

- The global skills shelf is a service-key `ConceptStore` (namespace
  `"skills"`): readable by every user, writable only by admins.
- Skills are OKF concepts with explicit `type: Skill` frontmatter; a concept
  without it defaults to `Note` and is skill-invisible.
- "Packaged with the system" means the skill is self-contained in the shelf:
  the main skill doc plus byte-exact script concepts and an md5 manifest, so
  a user can materialize the working skill by reading the shelf alone. This
  was proven end-to-end on the test LXC (all 7 scripts md5-verified as a
  non-admin reader). The 12-concept set is the canonical content.
- The repo already has the upgrade-delivery precedent: `assets.rs`
  scaffolds built-in `style.css`/`app.js`/etc. at boot with a version marker
  — write-once, refresh-on-bump, admin customization preserved between
  bumps. The skills seeding mirrors it.

## Approach (approved: approach 1)

The 12 skill concept files are embedded in the server binary and seeded into
the skills shelf at boot. No loose files in the Docker image (the image is
data-free; all state lives in `MYCELIUM2_DATA_DIR`), no CLI seeding step
(fresh deployments must not require one).

### Repo layout

```
crates/mycelium-web/packaged-skills/pdf-to-markdown/
  pdf-to-markdown.md                  # main skill doc
  pdf-to-markdown-conventions.md      # output spec
  pdf-to-markdown-scripts.md          # md5 manifest + extraction rules
  pdf-to-markdown-script-convert.md   # byte-exact scripts (7)
  pdf-to-markdown-script-postprocess.md
  pdf-to-markdown-script-docling-page-span.md
  pdf-to-markdown-script-html-cleanup.md
  pdf-to-markdown-script-inspect-pdf.md
  pdf-to-markdown-script-setup-venv.md
  pdf-to-markdown-script-requirements.md
  pdf-to-markdown-docling-options.md  # docling reference doc
  pdf-to-markdown-license.md          # LICENSE.txt of the skill
```

12 files, copied byte-exact from the proven staged set
(`/tmp/myc2-skills/`). New module `crates/mycelium-web/src/packaged_skills.rs`
embeds them with `include_str!` and holds the static table
`[(repo_filename, contents)]`. The concept path stored in the shelf is
`/<repo_filename>` (e.g. `pdf-to-markdown-script-convert.md` →
`/pdf-to-markdown-script-convert.md`), matching the paths already live on
the test LXC.

### Seed function

```rust
pub const SKILLS_SEED_VERSION: &str = "1";
pub fn seed_packaged_skills(
    store: &Store, service_key: &ServiceKey, skills_dir: &Path,
) -> Result<(), Box<dyn Error + Send + Sync>>;
```

Called in `crates/mycelium-server/src/main.rs` immediately after
`scaffold_defaults` (assets scaffold). Semantics:

1. Marker file `<skills_dir>/.seed-version`.
2. Marker missing or different from `SKILLS_SEED_VERSION`:
   parse each embedded file into a `Concept` (frontmatter is already valid
   and carries `type: Skill`), one `ConceptStore::for_service(store,
   service_key, skills_dir, "skills").put_batch(&concepts)` — a single
   index.md regeneration — then write the marker.
3. Marker matches: no-op. Admin edits to packaged paths survive; packaged
   concepts an admin deleted stay deleted until a version bump restores them
   (all-or-nothing refresh, same contract as the assets defaults).
4. Dotfile safety: the marker is a dotfile inside `skills/`; `FileRepo::list`
   filters dotfiles (`file_repo.rs`), so it never appears as a concept.
5. Errors propagate — fail-fast, identical contract to `scaffold_defaults`
   (server does not boot half-seeded).

### `Concept` fields used

`Concept::parse(path, contents)` — each packaged file already carries valid
YAML frontmatter with `type: Skill`, `title`, `description`, `tags`,
`timestamp`. No frontmatter rewriting; the seed stores what ships.

## Behavior per scenario

| Scenario | Behavior |
|---|---|
| Brand-new Docker deploy (empty shelf) | All 12 seeded on boot; zero-touch. Every user sees the skill under `/skills`; admins can edit. |
| Deploy this build to the test LXC (shelf already holds the 12 from manual upload) | Marker absent → seed re-puts all 12 (identical bytes → no observable duplicates; count stays 12). |
| Later release with improved skill content | `SKILLS_SEED_VERSION` bump → full refresh of the packaged set on next boot. |
| Admin-created extra skills (e.g. future mobi skill seeded by a later version) | Untouched by this seed — it only writes the packaged paths. |

## Testing (TDD — `crates/mycelium-web/tests/packaged_skills_integration.rs`)

Boot helper mirrors the existing integration boot (temp data dir,
`scaffold_defaults`, store open, service key) minus any user creation; seed
is invoked explicitly, matching the unit-of-work under test.

1. **Fresh seed**: seed a fresh store → skills shelf lists exactly the 12
   packaged paths; reading a script concept returns `type: Skill`; shelf
   search for "docling" returns hits.
2. **Idempotent**: seed twice → still exactly 12, no duplicates.
3. **Same-version hands off**: edit one packaged-path concept (admin-style
   edit), re-seed with matching marker → edit preserved.
4. **Bump refreshes**: marker set to a stale version, concept edited →
   re-seed → packaged bytes restored, marker now `SKILLS_SEED_VERSION`.
5. **CI smoke** (`ci.yml` fresh-boot check): assert `<data>/skills/.seed-version`
   exists after a boot (proves boot wiring; content covered by 1–4).

Byte-exactness guard: the embedded script-concept bodies must match the
upstream skill's md5 manifest concept (`pdf-to-markdown-scripts.md`) — test 1
asserts the manifest's listed md5s are what the script concepts actually
carry, so a stale copy in the repo fails CI instead of shipping.

## Docs

- `docs/deployment.md`: first boot seeds the packaged pdf-to-markdown skill
  into the global skills shelf; upgrade semantics (version bump → refresh,
  admin edits preserved between bumps).
- `DESIGN.md`: one paragraph under the skills-shelf section noting packaged
  skills and the seeding contract.

## Non-goals

- No new HTTP endpoints, no schema changes, no upload-flow changes.
- No mobi/epub pipeline (separate follow-up project).
- No per-concept admin opt-out beyond the documented "same version hands
  off" contract.

## Verification plan

Full verify suite per AGENTS.md (`cargo check`, `cargo fmt --check`,
`cargo clippy -D warnings`, `cargo test --workspace`) → commit-means-push →
CI green → GHCR image → deploy to LXC 192.168.2.97 → live verify: `/skills`
lists the skill as admin and as a non-admin (kwslavens), a script concept
reads 200, re-run the 7-script md5 materialization proof, shelf count is 12.