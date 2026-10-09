# Web UI redesign — design

- Issue: Draco-Lunaris/Mycelium2#2 ("Re-Design / Improve web UI").
- Status: design approved section-by-section in brainstorming, 2026-10-08;
  this document is the written consolidation of that approval.
- Visual source of record: the 15 mockups attached to issue #2 (approved
  2026-10-04). A distilled working reference lives at
  `/tmp/mycelium2-issue2-mockups/NOTES.md`; the load-bearing facts are
  inlined here (§5, §10) so this spec stands alone. **No session ever
  reads the mockup PNGs directly** — image inspection goes through
  fork subagents only (§9.4).
- The six locked decisions are also recorded in mycelium at
  `/projects/mycelium2-ui-redesign-issue-2.md`.

## 1. Goal and non-goals

Redesign the web interface so it is well organized, looks finished, and
covers the pages DESIGN.md §8 lists — built to the 15 approved mockups.

Non-goals (issue #2): REST API and MCP behavior; authentication logic
(this redesign adds the missing UI over the existing auth services); the
receipt queue itself (issue #1).

## 2. Locked decisions

Six user decisions override the issue's proposed defaults:

- **(a) Frontend stack = Leptos SSR + hydrate.** The `leptos` dependency
  stays (workspace `leptos = "0.8"`); a wasm32 hydrate build lands in CI
  and Docker. Overrides the issue's server-templates proposal.
- **(b) Concept editor Title/Type inputs dropped.** Title and type
  render in the preview only; frontmatter stays the single source of
  truth; no handler change.
- **(c) Graph = D3, vendored.** Self-hosted in `/assets`, pinned exact
  patch version, license file beside it. No CDN.
- **(d) Dark theme only.** All colors as CSS custom properties; a light
  theme later is a variable swap. No `prefers-color-scheme` branch now.
- **(e) Recovery-key card gains Regenerate**, grounded in
  `mycelium-crypto`'s `rotate_recovery_key`
  (crates/mycelium-crypto/src/seal.rs:152): current password required,
  new key shown once. Copy button with a script-free selectable
  fallback.
- **(f) Delivery = four phased PRs**: (1) tokens/components/shell,
  (2) user pages, (3) admin split, (4) new pages.

## 3. Architecture

Axum keeps routing, middleware, and handlers exactly as today (route
table in `server.rs`; middleware order csrf_protect →
require_login_gate → session_auth → security_headers). Leptos is the
view layer **inside the existing handlers**: each handler gathers data
as it does now, builds a view from shared components, and renders it to
an HTML string. Server-side form-POST flows are unchanged; CSRF hidden
fields render as ordinary form inputs.

**Integration shape: per-handler SSR with targeted hydration islands** —
not `leptos_routes_with_context`. Full route ownership would rewrite the
middleware/test surface the issue requires stay unchanged, for no gain
on form-and-table pages.

- Shared component modules compile for both targets: server SSR and
  wasm32 hydrate. One hydration entrypoint (`/assets/hydrate.js` + wasm —
  flat filenames, `serve_asset` rejects subdirectories) hydrates whatever
  island roots the current page has; pages without islands never load
  it. Missing bundle = SSR-only degradation (hydration is additive,
  never load-bearing for a form flow).
- First islands: the chat composer and the API-key copy button. The
  graph stays plain JS (D3 drives SVG into a container — a JS library
  concern, not a Leptos component), keeping the wasm surface small.
- CI gains a wasm32 build step in PR 1; Docker ships the bundle.
  Hand-rolled two-target build; no cargo-leptos toolchain dependency.

**Token system**: one hand-authored stylesheet (no framework, no CDN)
replacing the ~130-line string constant in `assets.rs`, still delivered
via `scaffold_defaults` + `?v=ASSETS_VERSION` cache-busting. All colors
are `:root` custom properties. Dark-only per decision (d).

**CSP**: one amendment, required by hydration: `script-src` gains
`'wasm-unsafe-eval'` (WebAssembly compilation is blocked without it, so
the hydrate bundle cannot load). The CSP header changes exactly once, in
PR 1:
`script-src 'self' 'wasm-unsafe-eval' 'nonce-…'`. Everything else
holds: every script loads from `/assets` as an external file covered by
`'self'`; no handler ever emits an inline script or a `style=`
attribute — components carry class names only. The existing
per-response nonce stays generated-and-unused (harmless
defense-in-depth); threading it into handlers is not needed.

**Build structure**: the component library is its own crate,
`crates/mycelium-ui`, because `mycelium-web` pulls sqlx/argon2 and
cannot compile for wasm32. `mycelium-ui` depends only on leptos (SSR
by default, hydrate+islands for the wasm build) and takes
primitive-only props (&str, bool, u64…). `mycelium-web` consumes its
SSR render helpers. The wasm entry is `#[wasm_bindgen(start)] →
leptos::mount::hydrate_islands()`, built with
`--no-default-features --features hydrate`, bundled with wasm-bindgen
(web target) to `/assets/hydrate.js` + `/assets/hydrate.wasm`. The
hydration traversal script is leptos's own `island_script.js`,
vendored alongside the bundle. Islands themselves are `#[island]`
components (the macro auto-generates the per-island exported functions
that the traversal script calls via `data-component`).

## 4. Component library

New crate `crates/mycelium-ui/` (compiled for both targets — see §3),
consumed by `mycelium-web`:

- `mod.rs` — re-exports
- `shell.rs` — Shell, NavGroup, NavItem, UserChip, PageHeader, AuthCard
- `layout.rs` — Card, CardHeader, DataTable, StatTile, Breadcrumb,
  TabBar, FormActions
- `primitives.rs` — Chip, Button, Banner, EmptyState, Field + inputs,
  ConfirmDialog
- `render.rs` — SSR render helpers (the `into_view().to_html()` bridge)

`pages.rs`'s 772 lines of `format!` strings are replaced wholesale: every
page becomes handler → data → component composition.

Inventory (mockup anchor in parens):

- **Shell** (01, 15, 13) — brand lockup, grouped nav, user chip +
  logout, content slot. One component, not a narrow variant: mockup 15
  is the same DOM re-flowed; CSS media queries collapse it to the ~60px
  icon rail and surface the top bar. The server never knows the
  breakpoint.
- **NavGroup / NavItem** (01) — uppercase letter-spaced group header;
  icon+label items in rounded pills; active = accent-tinted pill
  (`--color-accent-surface`) with brighter label.
- **PageHeader** — title left, actions right; becomes the narrow-width
  top bar.
- **AuthCard** (13) — outside-shell centered card for `/login` and
  `/setup`; same tokens, no separate palette.
- **Card / CardHeader** (14) — rounded surface; header row with title
  left, action right.
- **DataTable** (01, 15) — semantic `<table>`, hairline rows. Narrow
  re-flow with one DOM and no script: cells carry `data-label`; CSS
  hides `thead` and stacks cells with generated captions at narrow
  width — mockup 15's "list rows instead of a table" without duplicated
  markup or server-side branching.
- **StatTile** (04) — big number + small label.
- **Chip** — type/scope/state variants; accent/neutral/danger/warning
  color classes (no separate success class — accent covers it, §5).
- **Breadcrumb** (02); **TabBar** (admin sections, 03/11/12).
- **Banner** (14) — info/success/warning/danger, leading icon. Success
  is styled with accent tints (no separate success token exists, §5).
- **EmptyState** (14) — icon, heading, one-line body.
- **ConfirmDialog** (14) — native `<dialog>`: dimmed backdrop, title,
  body, cancel + danger confirm. Used by API-key revoke (09) and
  concept delete (02). A ~30-line external `confirm.js` (`'self'`,
  CSP-clean) wires open/cancel. Script-free fallback: without JS the
  trigger submits directly — the dialog is UX confirmation, not the
  security gate (the CSRF token is).
- **Forms** (14, 11) — `Field` (label tied to input by id — the a11y
  baseline; optional hint line for range hints; error state = danger
  border + message), styled TextInput/PasswordInput/NumberInput/
  Select/Checkbox, `Button` (primary: accent fill + dark text /
  secondary: outline / danger: coral / ghost), `FormActions`.
- Chat-local components (message block, tool-activity line, sources row,
  pinned composer — 08) stay in the chat module; they reuse
  Chip/Banner/Button but are not shared inventory.
- All icons: inline `<svg>` in components. No external imagery, no icon
  font.

## 5. Design tokens

Color values below are the source of record — pixel-sampled from
mockups 14/01/13 on 2026-10-08 (stdlib PNG decoder, ~153k samples per
image, text-only output; no image entered any model context). The
background family is **green-tinted charcoal, not navy** (an earlier
distillation pass read it as navy; the sampling corrects that).

| Token | Value | Use |
|---|---|---|
| `--color-bg-page` | `#101312` | page background |
| `--color-bg-rail` | `#0C0F0E` | sidebar rail (darker than page bg) |
| `--color-surface` | `#171B19` | cards / panels |
| `--color-surface-raised` | `#1C2420` | hover / overlay |
| `--color-accent` | `#8FD3A8` | primary buttons, links, active states, focus rings |
| `--color-accent-surface` | `#12201A` | active nav pill |
| `--color-text-heading` | `#E4E9E5` | headings |
| `--color-text-label` | `#C9D2CC` | labels |
| `--color-text-body` | `#9AA69F` | body text |
| `--color-text-muted` | `#3A453F` | muted text |
| `--color-border` | `#28302C` | hairline borders |
| `--color-danger` | `#F2978A` | danger (coral) |
| `--color-danger-surface` | `#21130F` | danger tinted background |
| `--color-warning` | `#E3B65F` | warning (amber) |
| `--color-warning-surface` | `#1D1A10` | warning tinted background |
| `--color-chip-surface` | `#1C221F` / `#202825` | type-chip tinted fills |

Success: no distinct green beyond the accent family — the success banner
reuses accent tints (design decision; not separately mocked).

Type / radius / spacing — pinned against mockup 14 by a vision fork on
2026-10-08 ("Pinned tokens (2026-10-08)" section of
`/tmp/mycelium2-issue2-mockups/NOTES.md`; the fork's readings sit within
the [e] estimation bands recorded there):

| Token | Pinned value |
|---|---|
| `--text-page-title` | 24px / 700 |
| `--text-section` | 17px / 600 |
| `--text-body` | 15px / 400 |
| `--text-caption` | 13px / 400 (nav group headers + table headers: uppercase, +0.08em) |
| font | system UI stack — zero external font requests |
| `--radius-card` / `--radius-control` / chip radius | 12px / 8px / full pill |
| spacing | 4px grid: 4, 8, 12, 16, 24, 32 (panel gaps ~16–24) |
| `--sidebar-width` / `--rail-width` / content max-width | 280px / 60px / ~1200px |
| focus | accent border on `:focus-visible` (ring treatment [u] at implementation) |
| modal backdrop | ~70% black (60–80% band); dialog ~400–480px wide |
| control heights | buttons ~38px; inputs ~40px; nav items ~38px (36–40 bands) |

Primary buttons carry dark text on the accent fill (14: accent fill is
light, `#8FD3A8`; dark text keeps ≥4.5:1).

**Muted-text token resolution:** sampled `--color-text-muted #3A453F`
measures ≈1.9:1 on `#101312` — it cannot serve as body text under the
4.5:1 baseline. Resolved: `--color-text-muted` is a **decorative /
disabled-only** token (borders, disabled labels, watermark text), never
prose text; readable secondary text uses `--color-text-body #9AA69F`
(≈7.4:1). The PR-1 contrast test asserts the three text tokens
(heading/label/body) and **excludes** the muted token from the 4.5:1
requirement (it is asserted decorative-only in the stylesheet).

## 6. Pages, routes, navigation

Sidebar groups (locked):

- **Knowledge** → Browse `/`, Search `/search`, Graph `/graph`, Skills
  `/skills`, Books `/books`
- **Librarian** → Chat `/chat`, Status `/status`
- **Account** → API keys `/keys`, Account security `/account`
- **Admin** → Users, Sign-in, LLM backend, Library, System

The Admin group and the Status item are admin-only; non-admins don't
see them (Status is an operational page showing cross-user jobs).

Route map:

| Page (mockup) | Route | Method | Notes |
|---|---|---|---|
| Browse (01) | `/` | GET | current "Home" is already the concept list — relabel, no new route; `?type=` filter = plain links |
| Concept editor (02) | `/concept` | GET/POST | `?path=`; no path = blank New-concept editor; `/concept/delete` POST unchanged |
| Search (05) | `/search` | GET | adds `?scope=` chips alongside `?q=` |
| Graph (06) | `/graph` | GET | restyled; `d3.min.js` + `d3-license.txt` flat in `/assets` |
| Skills | `/skills` | GET | restyled; existing hub grouping |
| Books + passages (07) | `/books` | GET | `?shelf=&book=&passage=` — all server-rendered, no script; books count = derived SQL column |
| Chat (08) | `/chat` | GET | composer = hydration island |
| Librarian status (04) | `/status` | GET, admin | tiles from `/health`'s four queue counts; state chips; ingest-jobs table (existing admin listing); receipt table = placeholder pending #1 |
| API keys (09) | `/keys` | GET/POST | mint + revoke POSTs unchanged; copy-button island + ConfirmDialog on revoke |
| Account security (10) | `/account` | GET | new; password card posts to existing `/password` POST; new POSTs: `/account/totp/enroll`, `/account/totp/verify`, `/account/totp/disable`, `/account/webauthn/register`, `/account/webauthn/remove`, `/account/recovery/regenerate`; plus GET `/account/webauthn/challenge` (JSON creation options for the enroll script) |
| Sign-in (13) | `/login`, `/setup` | GET/POST | AuthCard restyle; flows unchanged |
| Admin: Users (03) | `/admin/users` | GET | hosts existing POST `/admin/users` |
| Admin: Sign-in (11) | `/admin/sign-in` | GET | hosts existing POSTs `/admin/security` (range-hinted fields) + `/admin/oidc` |
| Admin: LLM backend | `/admin/llm` | GET | hosts existing POST `/admin/llm` |
| Admin: Library (12) | `/admin/library` | GET | hosts existing POSTs `/admin/bookshelves` + `/admin/upload-limits` (size-limit hint = the 32 MiB cap), the existing admin multipart upload flow, and a read-only global-skills listing linking to `/skills` |
| Admin: System | `/admin/system` | GET | backup download (existing POST `/admin/backup`), health/metrics summary |
| — | `/admin`, `/password` | GET | 307 redirects → `/admin/users`, `/account`; POST endpoints keep their paths |

Resolved deltas:

- **Forced-password-change gate**: redirect becomes
  `/account?forced=1`; the gate's allowed paths add `/account` (the
  password card posts to `/password` unchanged). One condition line in
  `require_login_gate` + tests, in PR 4.
- **Editor preview (02)**: static, server-rendered from the saved
  content, refreshed after each save; frontmatter errors re-render in
  the warning banner. No live-preview script — deliberate; live preview
  can come later as an island.
- **Top-bar divergence (01 vs 15)**: resolved by the one-Shell CSS
  re-flow — PageHeader becomes the top bar at narrow width; no second
  shell, no server branch.

Mockup-vs-handler deltas (the issue's list) resolve as: (1) editor
Title/Type → decision (b); (2) create-user form adds `password_confirm`
(PR 3); (3) Browse `?type=` + Search `?scope=` = plain query-param
links, no script; (4) books count = derived column (PR 2); (5) receipt
IDs/state names = placeholder pending #1 (PR 4, one panel); (6) copy
button = external script + selectable fallback (PR 2); (7)
recovery-key actions → decision (e).

## 7. Auth, CSRF, CSP interplay

**Existing protections unchanged and verified**: all four middleware
layers stay exactly as built; existing integration tests keep asserting
the gates (session auth, CSRF 403s, XSS escaping, forced-change
redirect) against route HTML, so the redesign cannot silently break a
protection. No external CDN.

**CSRF**: the six new `/account/*` POST endpoints follow the existing
pattern — hidden `csrf_token` input rendered by the form component,
double-submit verified by `csrf_protect`, 403 on missing/invalid token.
New endpoints get the same test class in their PR. Bearer callers
already have REST equivalents; browser flows are session + CSRF.

**Route publicity**: `require_login_gate`'s allowlist already covers
`/assets/*` — confirm.js, copy.js, and the hydrate bundle are
gate-exempt automatically; no new public paths are needed.

**Account security page (10)**:

- TOTP enroll: POST `/account/totp/enroll` renders the enrollment flow
  (secret + QR; QR via `img-src 'self' data:`, already permitted) →
  POST `/account/totp/verify` (code) → state chip flips to enabled.
  Verify is a single page-load form POST; a missed 30s window
  re-renders with a "code expired/invalid, try again" banner. SSR, no
  script.
- WebAuthn: registration inherently requires client script (the
  navigator.credentials ceremony) — a new, small external
  `/assets/webauthn.js` fetches creation options from GET
  `/account/webauthn/challenge` (JSON, session-auth) and POSTs the
  resulting credential to `/account/webauthn/register`. Without script
  the WebAuthn rows render state only (enroll control disabled) —
  honest degradation, unlike TOTP which is fully server-rendered.
- These handlers are thin UI wrappers over the existing
  LoginService/TOTP/WebAuthn services — no auth-logic changes.
- **Recovery regenerate (decision e)**: POST
  `/account/recovery/regenerate` with current password + CSRF →
  `rotate_recovery_key` → the new key renders in a shown-once card
  (same component as the API-key shown-once banner). Current password
  required per the crypto contract. Rotation does not invalidate
  sessions (only password change does — existing behavior).

## 8. Phased delivery

Four PRs against `master`, strictly ordered; each green (full cargo
gate) and through the judge + code-reviewer cycle per repo convention,
mergeable on its own.

### PR 1 — Foundation: tokens, components, shell

- `assets.rs`: new token stylesheet (§5), `ASSETS_VERSION` 7→8.
- `ui/` module tree (§4), SSR-rendered, compiled for both targets.
- Shell + grouped nav applied to every existing page. Page bodies stay
  as today's `format!` strings; the new stylesheet keeps styling the
  old body class names (legacy aliases) so un-converted pages render
  decently. PRs 2–3 delete aliases as each body converts. This is what
  lets the foundation merge without touching all 772 lines at once.
- CI gains the wasm32 build step; Docker ships the hydrate bundle
  (loaded by nothing yet — pipeline proven before any page depends on
  it, per decision (a)).
- Component library complete, including ConfirmDialog + confirm.js;
  admin stays one page in this PR.
- Tests: existing integration tests updated for the new nav markup;
  SSR render unit tests per component.

### PR 2 — User pages

Browse (`?type=` chip filter), concept editor (breadcrumb, static
preview, warning banner), Search (`?scope=` chips), Graph (**D3
vendored** — pinned exact patch, `d3.min.js` + `d3-license.txt` flat in
`/assets`; node info card, legend, usage hint per 06), Skills, Books
(`?shelf=&book=&passage=`, books-count derived column + its store
query), Chat (composer = first hydration island), API keys (shown-once
banner, copy-button island, ConfirmDialog on revoke). `/password`
restyled as a lone card — its merge into `/account` waits for PR 4.
Islands go live here: `hydrate.js` loads only on chat + keys.

### PR 3 — Admin split

`/admin/users` (table + side form, `password_confirm` added, role /
must-change chips) · `/admin/sign-in` (range-hinted settings form;
hosts the existing `/admin/security` + `/admin/oidc` POSTs) ·
`/admin/llm` · `/admin/library` (bookshelves CRUD with count, upload
card with the 32 MiB hint, read-only global-skills listing) ·
`/admin/system` (backup download, health/metrics StatTiles) — TabBar
across the five; `/admin` → 307 `/admin/users`. Per-route admin checks
and their tests carried from the old `admin_view`.

### PR 4 — New pages

Account security `/account` (password card → existing `/password`
handler; TOTP enroll/verify/disable; WebAuthn register/remove; recovery
regenerate with shown-once card) + `/password` → 307 + forced-change
gate retarget. Librarian status `/status` (admin): StatTiles from the
four `/health` queue counts, state chips, ingest-jobs table, and a
receipt panel that is an explicit placeholder pending issue #1 — the
only place #1's outcome (receipt ID format, state names) touches. Full
flow tests per feature.

Phase-order constraints: shell precedes pages; admin split precedes
`/account` (the gate change touches admin-tested paths).

## 9. Testing and verification

### 9.1 The gate

`cargo fmt --all --check` · `cargo check --workspace` · `cargo clippy
--workspace --all-targets -- -D warnings` · `cargo test --workspace` —
every PR, plus the judge + code-reviewer cycle per repo convention.

### 9.2 Existing tests

Every existing web integration test keeps asserting the same
protections (session gates, CSRF 403s, XSS escaping, forced-change
redirect — retargeted to `/account?forced=1` in PR 4). PR 1 updates
markup-dependent assertions once; no test is deleted, only retargeted.

### 9.3 New tests per PR

| PR | Test class |
|---|---|
| 1 | Component SSR render tests: structurally valid markup, zero `style=` attributes, every `<script>` carries `src`; nav renders groups, active state, hides Admin + Status for non-admins; wasm32 target compiles in CI (the build is the test) |
| 2 | Per-page flows: Browse `?type=`, Search `?scope=`, books-count column, editor preview + frontmatter-warning banner, keys shown-once + revoke (dialog markup present), chat renders (503 LLM-unreachable case kept); `d3.min.js` + license served from `/assets` |
| 3 | Admin split: five pages render, `/admin` 307s, per-page admin enforcement (non-admin → redirect), all existing `/admin/*` POSTs still pass through their new host pages |
| 4 | Full flows: TOTP enroll→verify→disable round-trip, WebAuthn register/remove, recovery regenerate (wrong password rejected; correct password → rotation + shown-once card rendered once), gate retarget, `/status` admin-only + tiles from the four queue counts |

### 9.4 Standing mechanical tests (land in PR 1, run forever)

- **CSP sweep**: fetch every page (admin session — widest surface),
  assert no inline `<script>` and no `style=` attribute. Mechanically
  enforces the issue's no-inline-styles/scripts requirement for the
  life of the codebase.
- **Label pairing**: every `Field` renders `label[for]`/input `id`
  pairs (the testable core of the a11y baseline).
- **Contrast**: a unit test computes WCAG ratios from the token values
  (body `#9AA69F` on `#101312`, etc.) and asserts ≥ 4.5:1 for text
  tokens — a future token edit that breaks contrast fails CI.

### 9.5 Mockup fidelity — image handling protocol

No main session ever reads the mockup PNGs (reading them into a main
session caused autocompact thrash; recorded in mycelium at
`/claude-code/large-image-distillation.md`). All image inspection is
fork-mediated (forks inherit the session model — real vision; cheaper
subagent tiers map to local backends with unverified vision):

1. **Implementation-time pinning**: DONE 2026-10-08 — the type/radius/
   spacing values are pinned (§5 tables record the results; details in
   `/tmp/mycelium2-issue2-mockups/NOTES.md`, "Pinned tokens
   (2026-10-08)"). Remaining [u] markers (e.g. exact modal copy, focus
   ring treatment) are settled in the PR that builds the component
   carrying them.
2. **Fidelity passes**: after PR 2 (user pages) and PR 4 (all pages),
   a fork screenshots the rendered pages and compares them against the
   mockups + NOTES.md, reporting deviations. Requires a browser in
   the dev environment — Chromium is not installed today; a one-time
   `playwright install chromium` is flagged to the user at that point,
   not silently done.

## 10. Appendix — per-page layout reference

Load-bearing layout facts from the mockup distillation (confirmed
readings only; uncertain fine-grained labels get pinned per §9.4).

**Shell (all shelled pages)**: dark theme; fixed left sidebar (~280px,
collapses to ~60px icon rail at narrow width); brand lockup top (teal/
mint mark + "Mycelium" wordmark); grouped nav (uppercase letter-spaced
group headers, icon+label pills); user chip + logout at bottom;
page-title row inside content with actions right. Cards: rounded
12–16px, dark slate fill, subtle 1px lighter borders.

- **01 Browse**: title row ("Browse" left; New-concept primary button +
  type filter right); concept list as a table — name/path, Type as
  color-coded chips, broken-links flag (amber/red badge on affected
  rows), timestamp column; dark slate rows, hairline separators.
- **02 Concept editor**: breadcrumb row; Save (primary) + Delete
  (danger) top-right; two-pane body ~equal widths — left monospace
  markdown source (incl. frontmatter), right rendered preview (title,
  type chip, description, headings); amber warning banner between
  title row and panes. No Title/Type inputs (decision b).
- **03 Admin: Users**: admin tab row across content top; users table
  (username, Role chip, Status chip incl. must-change state) +
  right-hand create-user side form (username, password, confirm
  password, role select, must-change checkbox, primary submit).
- **04 Librarian status**: title row; ~4 summary stat tiles (big
  number + small label); state-indicator chips (colored dot + label);
  job tables (receipt rows: ID, user, mode, state chip, attempts,
  age — placeholders pending #1); tiles + tables stacked.
- **05 Search**: query row (large input + accent submit); scope filter
  chips near it; results as a list — title link, path/snippet second
  line, scope chip per row; result-count text near the list head.
- **06 Graph**: full-height force-directed graph panel (accent nodes +
  edges on dark panel); type legend overlay (colored dot + label);
  node info card overlay; usage-hint text line.
- **07 Books**: bookshelves listing (shelf name, visibility chip,
  books count); chapter list for the selected book (numbered/titled
  rows); passage reader pane (heading + body in a reading card).
  All server-rendered.
- **08 Librarian chat**: message log as stacked role-labeled turns;
  tool-activity lines interleaved (smaller, dimmed); sources row after
  assistant answers (chips/links); pinned composer at bottom (wide
  input + accent send).
- **09 API keys**: title row (mint/create primary right); shown-once
  banner card with Copy button (value stays selectable); keys table
  (label, status chip, created/last-used); revoked rows visually
  distinct; per-row Revoke with modal confirm.
- **10 Account security**: password-change card (current/new/confirm,
  primary submit); 2FA rows — TOTP and WebAuthn, each with
  enrolled/not-enrolled state + Enroll/Manage action; recovery-key
  card (explanatory text + Regenerate/Copy per decision (e)).
- **11 Admin: Sign-in**: single settings card, labeled numeric fields
  (session lifetime / throttle class) each with a min–max range-hint
  line; at least one checkbox/toggle; primary Save at the bottom.
- **12 Admin: Library**: stack of section cards — bookshelves (create
  form + table with count + row actions), upload (file input +
  size-limit hint), global skills (read-only listing).
- **13 Sign in**: outside the shell — single centered card on the page
  background; brand lockup at card top; heading; labeled
  username/password inputs; one primary submit. Template for
  `/setup`.
- **14 Components/tokens**: the token sheet — colors (§5), type scale,
  buttons (primary solid accent / secondary outline / danger / ghost),
  fields (label above bordered input; focus = accent border; error =
  red border + message), chips, banners (info/success/warning with
  leading icons), empty state (icon + heading + one line), and the
  confirmation pattern (modal dialog: centered card, dimmed backdrop,
  title, body, cancel + confirm).
- **15 Browse, narrow**: sidebar → persistent icon rail (no hamburger);
  top bar appears (title left, search + primary action right); title +
  actions stack; filter chips wrap onto their own rows; table →
  full-width list rows (name as row title, chip, metadata +
  broken-link flag as secondary lines; no column headers); spacing
  tightens overall.