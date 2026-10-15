//! Default static assets: written to the assets directory on first boot
//! and served from disk (DESIGN decision). Contains the stylesheet, the
//! graph visualization script (dependency-free force-directed layout),
//! the shared client script (CSRF header injection), the confirm dialog
//! script (native `<dialog>` open/close wiring), and the mycelium-ui
//! hydrate bundle (wasm-bindgen output — `mycelium_ui.js` +
//! `mycelium_ui_bg.wasm`, regenerated from `crates/mycelium-ui` per the
//! sequence in `docs/deployment.md`).

use std::path::Path;

pub const STYLE_CSS: &str = r#"/* Mycelium2 v8 — token stylesheet (spec §5; sampled + pinned values) */
:root {
  /* Colors (sampled 2026-10-08) */
  --color-bg-page: #101312;
  --color-bg-rail: #0C0F0E;
  --color-surface: #171B19;
  --color-surface-raised: #1C2420;
  --color-accent: #8FD3A8;
  --color-accent-surface: #12201A;
  --color-text-heading: #E4E9E5;
  --color-text-label: #C9D2CC;
  --color-text-body: #9AA69F;
  --color-text-muted: #3A453F; /* decorative/disabled only — never prose */
  --color-border: #28302C;
  --color-danger: #F2978A;
  --color-danger-surface: #21130F;
  --color-warning: #E3B65F;
  --color-warning-surface: #1D1A10;
  --color-chip-surface: #1C221F;
  --color-chip-surface-alt: #202825;
  /* Type (pinned) */
  --text-page-title: 24px;
  --text-section: 17px;
  --text-body: 15px;
  --text-caption: 13px;
  /* Radius / spacing */
  --radius-card: 12px;
  --radius-control: 8px;
  --radius-pill: 9999px;
  --gap-1: 4px; --gap-2: 8px; --gap-3: 12px; --gap-4: 16px;
  --gap-5: 24px; --gap-6: 32px;
  --sidebar-width: 280px;
  --rail-width: 60px;
  --content-max: 1200px;
  /* Legacy v7 aliases (un-converted page bodies — removed in PRs 2-3) */
  --bg: var(--color-bg-page);
  --panel: var(--color-surface);
  --text: var(--color-text-body);
  --muted: var(--color-text-body);
  --accent: var(--color-accent);
  --border: var(--color-border);
  --danger: var(--color-danger);
  --ok: var(--color-accent);
}
* { box-sizing: border-box; }
body {
  margin: 0; background: var(--bg); color: var(--text);
  font-family: system-ui, -apple-system, sans-serif; line-height: 1.5;
}
:focus-visible { outline: 2px solid var(--color-accent); outline-offset: 1px; }
header {
  display: flex; align-items: center; gap: 1rem; padding: 0.6rem 1.2rem;
  background: var(--panel); border-bottom: 1px solid var(--border);
}
header a { color: var(--text); text-decoration: none; font-weight: 600; }
header nav { display: flex; gap: 1rem; margin-left: auto; }
header nav a { color: var(--muted); font-weight: 400; }
header nav a:hover { color: var(--accent); }
main { max-width: 60rem; margin: 0 auto; padding: 1.5rem 1.2rem; }
h1, h2, h3 { color: var(--accent); }
a { color: var(--accent); }
table { border-collapse: collapse; width: 100%; }
th, td { text-align: left; padding: 0.5rem 0.7rem; border-bottom: 1px solid var(--border); }
th { color: var(--muted); font-weight: 600; }
form label { display: block; color: var(--muted); margin: 0.6rem 0 0.2rem; }
input, textarea, select {
  width: 100%; padding: 0.5rem 0.7rem; background: var(--bg); color: var(--text);
  border: 1px solid var(--border); border-radius: 4px; font: inherit;
}
textarea { min-height: 18rem; font-family: ui-monospace, monospace; }
button {
  margin-top: 0.8rem; padding: 0.5rem 1.2rem; background: var(--accent);
  color: #10131a; border: none; border-radius: 4px; font-weight: 600; cursor: pointer;
}
button.danger { background: var(--danger); color: #fff; }
.flash { padding: 0.7rem 1rem; border-radius: 4px; margin-bottom: 1rem; }
.flash.ok { background: rgba(158,206,106,.15); border: 1px solid var(--ok); }
.flash.err { background: rgba(247,118,142,.15); border: 1px solid var(--danger); }
.muted { color: var(--muted); }
pre { background: var(--panel); padding: 1rem; border-radius: 6px; overflow-x: auto; }
#graph { width: 100%; height: 34rem; background: var(--panel); border-radius: 6px; }

/* Graph — full-viewport interactive layout (body.graph-page), mockup 06:
   PageHeader above the panel; the type legend (bottom-left) and the node
   info card (top-right) overlay the panel; the usage hint sits beneath. */
body.graph-page { display: flex; flex-direction: row; height: 100vh; height: 100dvh; }
body.graph-page main {
  display: flex; flex-direction: column; flex: 1;
  max-width: none; width: 100%; margin: 0; padding: 0 1.2rem;
  min-height: 0;
}
body.graph-page .page-header { margin: .8rem 0 .4rem; }
#graph-wrap { position: relative; flex: 1; min-height: 0; }
#graph {
  width: 100%; height: 100%; background: var(--color-surface);
  border-radius: var(--radius-card); overflow: hidden; cursor: grab; touch-action: none;
}
#graph.dragging { cursor: grabbing; }
#graph-info {
  position: absolute; top: .6rem; right: .6rem; z-index: 2;
  background: var(--color-bg-page); border: 1px solid var(--color-border);
  border-radius: var(--radius-card); padding: .6rem .8rem; max-width: 22rem;
  font-size: var(--text-body); color: var(--color-text-body);
  box-shadow: 0 2px 8px rgba(0,0,0,.3);
}
#graph-info h3 {
  margin: 0 0 .3rem; color: var(--color-text-heading);
  font-size: var(--text-body); font-weight: 600;
}
#graph-info .gi-meta { display: flex; align-items: center; gap: var(--gap-2); margin-bottom: .2rem; }
#graph-info .gi-path { font-family: ui-monospace, monospace; font-size: var(--text-caption); }
#graph-info a { display: block; margin-top: .3rem; }
.graph-legend {
  position: absolute; bottom: .6rem; left: .6rem; z-index: 2;
  background: var(--color-bg-page); border: 1px solid var(--color-border);
  border-radius: var(--radius-control); padding: .4rem .6rem;
  font-size: var(--text-caption); display: flex; flex-direction: column; gap: 2px;
}
/* display:flex above would override the UA's [hidden] rule — restore it. */
.graph-legend[hidden] { display: none; }
.graph-legend .row { display: flex; align-items: center; gap: var(--gap-2); }
.graph-legend .dot {
  display: inline-block; width: 10px; height: 10px; border-radius: 50%;
  background: var(--dot, var(--color-accent)); flex: 0 0 auto;
}
.graph-legend .dot--orphan { background: transparent; border: 1px solid var(--color-danger); }
.graph-hint { font-size: var(--text-caption); color: var(--color-text-body); margin: .2rem 0 .4rem; }

/* Librarian chat — full-viewport layout (body.chat-page) */
body.chat-page { display: flex; flex-direction: row; height: 100vh; height: 100dvh; }
body.chat-page main {
  display: flex; flex-direction: column; flex: 1;
  max-width: none; width: 100%; margin: 0; padding: 0;
  min-height: 0; /* allow children to shrink */
}
.chat-shell {
  display: flex; flex-direction: column; flex: 1;
  min-height: 0; width: 100%;
  /* Full-width chat (layout fix #3): no 60rem cap on the shell — the
     transcript pane scales with the browser window; readability is
     kept per-message instead (chat-msg max-width below). */
  max-width: none;
  padding: 1rem 1.2rem 1rem;
}
.chat-log {
  display: flex; flex-direction: column; gap: 0.8rem;
  padding: 1rem; margin-bottom: 0.8rem;
  flex: 1 1 auto; min-height: 4rem; overflow-y: auto;
  background: var(--panel); border: 1px solid var(--border);
  border-radius: 6px; overscroll-behavior: contain;
}
.chat-input-bar {
  position: sticky; bottom: 0; flex: 0 0 auto;
  display: flex; gap: 0.6rem; align-items: flex-end;
  padding: 0.6rem 0 0; background: var(--bg);
}
.chat-input-bar textarea {
  flex: 1; min-height: 4.2rem; max-height: 16rem; resize: vertical;
  font-family: inherit; line-height: 1.4;
}
.chat-input-bar button { margin: 0; flex: 0 0 auto; height: 2.6rem; }
.chat-msg {
  /* Readability cap on wide panes (layout fix #3): the pane is full
     width, but each bubble stays at most 80% of the pane and 48rem —
     the standard chat-app shape. */
  max-width: min(80%, 48rem); padding: 0.6rem 0.9rem; border-radius: 10px;
  white-space: pre-wrap; word-wrap: break-word; line-height: 1.45;
}
.chat-msg.user {
  align-self: flex-end; background: var(--accent); color: #10131a;
  border-bottom-right-radius: 2px;
}
.chat-msg.librarian {
  align-self: flex-start; background: var(--bg);
  border: 1px solid var(--border); border-bottom-left-radius: 2px;
}
.chat-msg.pending { color: var(--muted); font-style: italic; }
.chat-msg.librarian > *:first-child { margin-top: 0; }
.chat-msg.librarian > *:last-child { margin-bottom: 0; }
.chat-msg.librarian p { margin: 0.4rem 0; }
.chat-msg.librarian pre {
  background: var(--panel); padding: 0.6rem; margin: 0.4rem 0;
  font-size: 0.85rem; white-space: pre-wrap;
}
.chat-msg.librarian code {
  background: var(--panel); padding: 0.1rem 0.3rem; border-radius: 3px;
  font-size: 0.9em;
}
.chat-msg.librarian pre code { background: none; padding: 0; }
.chat-msg.librarian ul, .chat-msg.librarian ol { margin: 0.4rem 0; padding-left: 1.4rem; }
.chat-msg.librarian h3, .chat-msg.librarian h4, .chat-msg.librarian h5, .chat-msg.librarian h6 {
  margin: 0.6rem 0 0.2rem; font-size: 1rem;
}
.chat-msg.error {
  align-self: flex-start; background: rgba(247,118,142,.12);
  border: 1px solid var(--danger); color: var(--danger);
}
/* Two-pane chat (session history, spec §3.6): the sessions sidebar
   beside the transcript pane. Collapsible (layout fix #5): the
   toggle (in the pane's first slot) flips chat-sessions--collapsed,
   which hides the pane; chat.js persists the state per browser.
   Without JS the pane simply stays open. */
.chat-two-pane { flex-direction: row; }
.chat-sessions {
  width: 15rem; flex: 0 0 auto; overflow-y: auto;
  display: flex; flex-direction: column; gap: 0.3rem;
  border-right: 1px solid var(--border); padding: 1rem 0.6rem;
}
.chat-sessions--collapsed { display: none; }
.chat-sidebar-toggle {
  align-self: flex-end; margin: 0 0 0.4rem; padding: 0.25rem 0.35rem;
  background: none; border: 1px solid var(--border); border-radius: 6px;
  color: var(--muted); cursor: pointer; line-height: 0;
}
.chat-sidebar-toggle:hover { color: var(--accent); border-color: var(--accent); }
.chat-session-row { display: flex; align-items: center; gap: 0.15rem; border-radius: 6px; }
.chat-session-row.active { background: var(--panel); }
.chat-session-link {
  flex: 1 1 auto; min-width: 0; padding: 0.35rem 0.5rem;
  border-radius: 6px; text-decoration: none;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.chat-main {
  flex: 1; display: flex; flex-direction: column;
  min-width: 0; padding-left: 1.2rem;
}
/* The delete trigger is a 14px icon button at the start of each row
   (layout fix #4): muted by default, danger on hover. The form wrapper
   is a layout non-entity so the row's flex flow owns the geometry. */
.chat-session-delete { display: contents; }
.chat-delete-btn {
  flex: 0 0 auto; display: inline-flex; align-items: center; justify-content: center;
  width: 1.7rem; height: 1.7rem; padding: 0;
  background: none; border: none; border-radius: 4px;
  color: var(--muted); cursor: pointer; line-height: 0;
}
.chat-delete-btn:hover { color: var(--danger); background: rgba(247,118,142,.12); }
.chat-status-note {
  align-self: center; font-size: 0.8rem; color: var(--muted);
  font-style: italic;
}
.chat-msg.tool-note {
  align-self: flex-start; font-size: 0.8rem; color: var(--muted);
  padding: 0.2rem 0.6rem;
}
/* Islands (Task 7): the leptos-island root is a pure hydration marker
   (never a layout box — display:contents keeps the composer's flex
   row and the minted banner's inline flow exactly as without it). */
leptos-island { display: contents; }
/* Copy button (keys minted banner): a quiet pill beside the
   shown-once secret. */
.copy-button {
  display: inline-flex; align-items: center;
  margin: 0 0 0 var(--gap-2); padding: 0.15rem 0.7rem;
  background: var(--color-chip-surface); color: var(--color-text-body);
  border: 1px solid var(--color-border); border-radius: var(--radius-pill);
  font-size: var(--text-caption); font-weight: 400; cursor: pointer;
}
.copy-button:hover { border-color: var(--color-accent); }

/* Keys page (Task 8, mockup 09): the mint form sits compact in the
   page header's actions slot (inline label + input + primary button);
   the shown-once value row keeps the key selectable beside its copy
   button (the value stays plain text — copy is enhancement, never the
   only path). */
.mint-form {
  display: flex; align-items: center; gap: var(--gap-2); margin: 0;
}
.mint-form label {
  display: inline; margin: 0;
  color: var(--color-text-label); font-size: var(--text-caption);
  white-space: nowrap;
}
.mint-form input { width: 12rem; min-height: 38px; }
.mint-form button { margin: 0; }
.shown-once { margin: 0; word-break: break-all; }
.shown-once code {
  background: var(--color-chip-surface); color: var(--color-text-body);
  border: 1px solid var(--color-border); border-radius: var(--radius-control);
  padding: 0.25rem 0.5rem; font-size: var(--text-body);
}

/* --- Components (Tasks 4-6) — token-only, no raw literals ----------- */

.btn {
  display: inline-flex; align-items: center; justify-content: center;
  gap: var(--gap-2); margin: 0;
  min-height: 38px; padding: 0.4rem 1rem;
  border: 1px solid var(--color-border); border-radius: var(--radius-control);
  background: var(--color-surface-raised); color: var(--color-text-label);
  font: inherit; font-weight: 600; text-decoration: none; cursor: pointer;
}
.btn--primary {
  background: var(--color-accent); border-color: var(--color-accent);
  color: var(--color-bg-page);
}
.btn--secondary {
  background: transparent; border-color: var(--color-border);
  color: var(--color-text-label);
}
.btn--danger {
  background: var(--color-danger); border-color: var(--color-danger);
  color: var(--color-bg-page);
}
.btn--ghost {
  background: transparent; border-color: transparent;
  color: var(--color-text-body);
}
.btn:hover { border-color: var(--color-accent); color: var(--color-text-heading); }
.btn--primary:hover, .btn--danger:hover { color: var(--color-bg-page); }
.btn:disabled { opacity: .5; cursor: not-allowed; }

.card {
  background: var(--color-surface); border: 1px solid var(--color-border);
  border-radius: var(--radius-card); padding: var(--gap-4);
  margin-bottom: var(--gap-4);
}
.card__header {
  display: flex; align-items: center; justify-content: space-between;
  gap: var(--gap-3); margin-bottom: var(--gap-3);
}
.card__header h1, .card__header h2, .card__header h3 {
  margin: 0; color: var(--color-text-heading);
  font-size: var(--text-section); font-weight: 600;
}

.chip {
  display: inline-flex; align-items: center;
  padding: 0.15rem 0.6rem; border-radius: var(--radius-pill);
  background: var(--color-chip-surface); color: var(--color-text-body);
  font-size: var(--text-caption); white-space: nowrap;
}
.chip--accent { background: var(--color-accent-surface); color: var(--color-accent); }
.chip--neutral { background: var(--color-chip-surface-alt); color: var(--color-text-label); }
.chip--danger { background: var(--color-danger-surface); color: var(--color-danger); }
.chip--warning { background: var(--color-warning-surface); color: var(--color-warning); }

/* Broken-links flag (Browse, mockup 01): the amber mini-badge a table
   row carries when its concept links outside the bundle. */
.flag--warning { color: var(--color-warning); background: var(--color-warning-surface); border-radius: var(--radius-pill); padding: 2px var(--gap-2); font-size: var(--text-caption); }

/* Banners: leading icon = the first child (inline svg), space reserved by
   the flex gap; success reuses accent tints (spec §5 — no success token). */
.banner {
  display: flex; align-items: flex-start; gap: var(--gap-2);
  padding: 0.7rem 1rem;
  border: 1px solid var(--color-border); border-radius: var(--radius-control);
  margin-bottom: var(--gap-3);
}
.banner svg { flex: 0 0 auto; margin-top: 0.15rem; }
.banner p { margin: 0; }
.banner--info { background: var(--color-surface); color: var(--color-text-body); }
.banner--success {
  background: var(--color-accent-surface); border-color: var(--color-accent-surface);
  color: var(--color-accent);
}
.banner--warning {
  background: var(--color-warning-surface); border-color: var(--color-warning-surface);
  color: var(--color-warning);
}
.banner--danger {
  background: var(--color-danger-surface); border-color: var(--color-danger-surface);
  color: var(--color-danger);
}

.empty {
  display: flex; flex-direction: column; align-items: center; gap: var(--gap-2);
  text-align: center; padding: var(--gap-6) var(--gap-4);
  color: var(--color-text-muted);
}
.empty h3 { margin: 0; color: var(--color-text-label); font-size: var(--text-section); }
.empty p { margin: 0; color: var(--color-text-body); font-size: var(--text-body); }

/* Field: label above the control; error state = danger border + message. */
.field { margin: var(--gap-3) 0; }
.field label {
  display: block; margin: 0 0 var(--gap-1);
  color: var(--color-text-label); font-size: var(--text-body);
}
.field input, .field select, .field textarea {
  min-height: 40px; border-radius: var(--radius-control);
}
.field__hint {
  margin: var(--gap-1) 0 0;
  color: var(--color-text-body); font-size: var(--text-caption);
}
.field--error input, .field--error select, .field--error textarea {
  border-color: var(--color-danger);
}
.field__error {
  margin: var(--gap-1) 0 0;
  color: var(--color-danger); font-size: var(--text-caption);
}

/* DataTable: hairline rows; narrow re-flow stacks cells with generated
   captions from data-label — one DOM, no script (spec §4). */
.table { width: 100%; border-collapse: collapse; }
.table th, .table td {
  text-align: left; padding: 0.6rem var(--gap-3);
  border-bottom: 1px solid var(--color-border);
}
.table th {
  color: var(--color-text-muted); font-size: var(--text-caption);
  font-weight: 600; text-transform: uppercase; letter-spacing: 0.08em;
}
.table td { color: var(--color-text-body); }
@media (max-width: 720px) {
  .table thead { display: none; }
  .table tr {
    display: block; margin-bottom: var(--gap-3);
    border: 1px solid var(--color-border); border-radius: var(--radius-control);
  }
  .table td {
    display: flex; align-items: baseline; gap: var(--gap-3);
    border: none;
  }
  .table td::before {
    content: attr(data-label);
    margin-right: auto;
    color: var(--color-text-muted); font-size: var(--text-caption);
    text-transform: uppercase; letter-spacing: 0.08em;
  }
}

/* Keys page (Task 8): revoked rows dim — data_table owns the <tr>, so
   every cell of a revoked row wraps its content in this class (the row
   state rides on the composed cell markup, dimming the whole visible
   row). */
.row--revoked { opacity: 0.6; }

/* Skills hub groups (Task 6): companions and script labels indented
   beneath their nested skill's hub row. */
.group-indent { padding-left: var(--gap-4); }

.stat {
  background: var(--color-surface); border: 1px solid var(--color-border);
  border-radius: var(--radius-card); padding: var(--gap-4);
}
.stat__value {
  display: block; color: var(--color-text-heading);
  font-size: var(--text-page-title); font-weight: 700;
}
.stat__label {
  display: block; margin-top: var(--gap-1);
  color: var(--color-text-muted); font-size: var(--text-caption);
}

.breadcrumb {
  display: flex; flex-wrap: wrap; align-items: center; gap: var(--gap-1);
  margin-bottom: var(--gap-3); font-size: var(--text-caption);
}
.breadcrumb a { color: var(--color-text-body); text-decoration: none; }
.breadcrumb a:hover { color: var(--color-accent); }
.breadcrumb span { color: var(--color-text-label); }
.breadcrumb span[aria-hidden="true"] { color: var(--color-text-muted); }

.tab {
  display: inline-flex; align-items: center;
  padding: 0.35rem var(--gap-3);
  border: 1px solid transparent; border-radius: var(--radius-pill);
  color: var(--color-text-body); text-decoration: none; font-size: var(--text-body);
}
.tab:hover { color: var(--color-text-heading); }
.tab--active { background: var(--color-accent-surface); color: var(--color-accent); }

/* Sidebar (spec §5: 280px; collapses to a 60px rail under 720px via the
   Shell's own layout — same tokens, one DOM). */
.sidebar {
  width: var(--sidebar-width); flex: 0 0 var(--sidebar-width);
  background: var(--color-bg-rail); border-right: 1px solid var(--color-border);
  padding: var(--gap-4) var(--gap-3);
}
.sidebar__group { margin-bottom: var(--gap-4); }
.sidebar__group-label {
  margin: 0 0 var(--gap-2); padding: 0 var(--gap-2);
  color: var(--color-text-muted); font-size: var(--text-caption);
  text-transform: uppercase; letter-spacing: 0.08em;
}
.nav-item {
  display: flex; align-items: center; gap: var(--gap-2);
  min-height: 38px; padding: 0.4rem var(--gap-3);
  border-radius: var(--radius-pill);
  color: var(--color-text-body); text-decoration: none; font-size: var(--text-body);
}
.nav-item svg { flex: 0 0 auto; }
.nav-item:hover {
  background: var(--color-surface-raised); color: var(--color-text-heading);
}
.nav-item--active { background: var(--color-accent-surface); color: var(--color-accent); }

/* PageHeader: title left, actions right; wraps to a top bar under 720px. */
.page-header {
  display: flex; align-items: center; justify-content: space-between;
  gap: var(--gap-3); margin-bottom: var(--gap-4);
}
.page-header h1 {
  margin: 0; color: var(--color-text-heading);
  font-size: var(--text-page-title); font-weight: 700;
}
.page-header .actions { display: flex; gap: var(--gap-2); }
@media (max-width: 720px) {
  .page-header { flex-wrap: wrap; }
}

/* Browse header small-search (Task 8, mockup 01): the compact GET
   search form beside the type chips — the base `input { width: 100% }`
   would stretch it across the actions row, so the field gets the
   mint-form's small control shape (same width/height, token-only).
   Enter submits; no submit button, no script. */
.search-mini { margin: 0; }
.search-mini input { width: 12rem; min-height: 38px; }

/* Concept editor (Task 2): source/preview grid — two equal columns,
   one under 720px (the shell's breakpoint); the preview pane is fully
   server-rendered from escaped fragments (static, no client script). */
.editor-grid { display: grid; grid-template-columns: 1fr 1fr; gap: var(--gap-4); }
@media (max-width: 720px) {
  .editor-grid { grid-template-columns: 1fr; }
}
.editor-pane textarea { min-height: 24rem; font-family: ui-monospace, monospace; }
.editor-preview {
  padding: var(--gap-3); background: var(--color-surface);
  border: 1px solid var(--color-border); border-radius: var(--radius-card);
}

/* Books passage reader (Task 4): the reading card — wider than the
   editor preview (46rem) for long-form text. */
.reader-pane { padding: var(--gap-3); background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-card); max-width: 46rem; }

/* ConfirmDialog: native <dialog>, dimmed backdrop ~70% (spec §5 band). */
.modal {
  width: min(440px, calc(100vw - 2 * var(--gap-4)));
  background: var(--color-surface); color: var(--color-text-body);
  border: 1px solid var(--color-border); border-radius: var(--radius-card);
  padding: var(--gap-4);
}
.modal::backdrop { background: var(--color-bg-page); opacity: .7; }
.modal h2, .modal h3 {
  margin: 0 0 var(--gap-2); color: var(--color-text-heading);
  font-size: var(--text-section); font-weight: 600;
}
.modal p { margin: 0 0 var(--gap-4); }

/* Shell (Task 6): sidebar + content row. body.shelled is the flex-row
   hook the shell emits for signed-in pages; the sidebar is sticky so
   long content scrolls beside it. */
body.shelled { display: flex; min-height: 100vh; min-height: 100dvh; }
body.shelled main { flex: 1 1 auto; min-width: 0; }
.sidebar {
  display: flex; flex-direction: column;
  position: sticky; top: 0; height: 100vh; height: 100dvh;
  overflow-y: auto;
}
.brand {
  display: inline-flex; align-items: center; gap: var(--gap-2);
  margin: 0 0 var(--gap-4); padding: 0 var(--gap-2);
  color: var(--color-text-heading); text-decoration: none;
  font-size: var(--text-section); font-weight: 700;
}
.brand:hover { color: var(--color-accent); }
.sidebar__footer {
  margin-top: auto; display: flex; align-items: center; gap: var(--gap-3);
  padding: var(--gap-3) var(--gap-2) 0;
}
.user-chip {
  display: inline-flex; align-items: center; gap: var(--gap-2);
  min-width: 0; color: var(--color-text-label);
}
.user-chip__avatar {
  flex: 0 0 auto; display: inline-flex; align-items: center;
  justify-content: center;
  width: 28px; height: 28px; border-radius: var(--radius-pill);
  background: var(--color-accent-surface); color: var(--color-accent);
  font-size: var(--text-caption); font-weight: 600;
}
.user-chip__name {
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  font-size: var(--text-caption);
}
.sidebar__logout {
  margin-left: auto; color: var(--color-text-body);
  font-size: var(--text-caption); text-decoration: none; white-space: nowrap;
}
.sidebar__logout:hover { color: var(--color-danger); }

/* Auth shell (login/setup): centered card on the page background. */
body.auth { display: flex; min-height: 100vh; }
body.auth main {
  display: flex; flex: 1; align-items: center; justify-content: center;
  max-width: none; width: 100%; margin: 0;
  padding: var(--gap-5) var(--gap-4);
}
.auth-card { width: 100%; max-width: 26rem; margin: 0; }
.auth-card .brand { margin: 0 0 var(--gap-4); }

/* Narrow (720px): the sidebar collapses to the 60px icon rail — labels
   hide, icons stay; the shell keeps one DOM (spec §10, mockup 15). */
@media (max-width: 720px) {
  .sidebar {
    width: var(--rail-width); flex:0 0 var(--rail-width);
    padding: var(--gap-3) var(--gap-1); align-items: center;
  }
  .brand { margin: 0 0 var(--gap-3); padding: 0; }
  .brand svg + span, .sidebar__group-label, .nav-item span,
  .user-chip__name, .sidebar__logout span { display: none; }
  .nav-item { justify-content: center; padding: 0.4rem 0; }
  .sidebar__footer { padding: var(--gap-3) 0 0; flex-direction: column; gap: var(--gap-2); }
  .sidebar__logout { margin: 0; }
}

/* Admin split (PR 3): the users page's table + create-user side form
   (mockup 03 — table left, form right, stacked under 720px), and the
   system page's StatTile row (a responsive tile grid). */
.split-grid {
  display: grid; grid-template-columns: 2fr 1fr;
  gap: var(--gap-4); align-items: start;
}
.split-grid .card { margin-bottom: 0; }
@media (max-width: 720px) {
  .split-grid { grid-template-columns: 1fr; }
  .split-grid .card { margin-bottom: var(--gap-4); }
}
.stat-row {
  display: grid; gap: var(--gap-3);
  grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr));
  margin-bottom: var(--gap-4);
}
.stat-row .stat { margin-bottom: 0; }

/* Librarian status (PR 4, mockup 04): the state-chip row — the chips
   are inline-flex spans, so the row wrapper supplies the gap, the
   narrow-width wrap, and the section spacing. */
.chip-row {
  display: flex; gap: var(--gap-2); flex-wrap: wrap;
  margin-bottom: var(--gap-4);
}

/* Account security (PR 4, mockup 10): the 2FA rows — label left,
   chip + count + action right; a hairline separates siblings and the
   row wraps at narrow widths. */
.qa-row {
  display: flex; gap: var(--gap-3); flex-wrap: wrap;
  align-items: center; justify-content: space-between;
  padding: var(--gap-3) 0; border-bottom: 1px solid var(--color-border);
}
.qa-row:last-child { border-bottom: none; }
.qa-row__side { display: flex; gap: var(--gap-2); align-items: center; flex-wrap: wrap; }
.qa-row__side form { margin: 0; }
"#;

pub const APP_JS: &str = r#"// CSRF: attach the session's CSRF token to every fetch/form request.
(function () {
  var meta = document.querySelector('meta[name="csrf-token"]');
  var token = meta ? meta.getAttribute("content") : "";
  var origFetch = window.fetch;
  window.fetch = function (input, init) {
    init = init || {};
    init.headers = new Headers(init.headers || {});
    init.headers.set("x-csrf-token", token);
    return origFetch(input, init);
  };
  document.addEventListener("submit", function (e) {
    var form = e.target;
    if (!form.querySelector('input[name="csrf_token"]')) {
      var hidden = document.createElement("input");
      hidden.type = "hidden"; hidden.name = "csrf_token"; hidden.value = token;
      form.appendChild(hidden);
    }
  });
})();
"#;

pub const GRAPH_JS: &str = r##"// Interactive force-directed graph for /graph: drag nodes (the
// layout re-settles around them, d3-style), wheel-zoom, background-pan,
// click-to-open, hover info panel, per-type palette + legend (v8 token
// restyle, mockup 06 — the layout algorithm itself is unchanged).
(function () {
  var el = document.getElementById("graph");
  var info = document.getElementById("graph-info");
  var legend = document.getElementById("graph-legend");
  if (!el) return;

  fetch("/api/v1/graph")
    .then(function (r) { return r.json(); })
    .then(function (data) { render(data); })
    .catch(function (e) { el.textContent = "graph load failed: " + e; });

  // Type → color mapping (v8 tokens, pinned hexes sampled 2026-10-08):
  // types in first-seen order (v1 semantics) draw from the readable token
  // tones — accent mint first (a single-type bundle reads as mockup 06's
  // mint nodes), then the heading/body/label grays, ordered to maximize
  // adjacent-pair separation. Fixed order; wraps past slot 4 (v1 cycled
  // past 8). Type identity is never color-alone: every node carries a
  // text label, the legend repeats the mapping, and the info card names
  // the type as a chip.
  var PALETTE = ["#8FD3A8", "#E4E9E5", "#9AA69F", "#C9D2CC"];
  var EDGE = "#8FD3A8";    // accent — mockup 06's graph edges
  var EDGE_FADE = "0.4";   // recessive: relationships sit behind nodes
  var STROKE = "#171B19";  // panel surface — a node's rim ring
  var ORPHAN = "#F2978A";  // danger — the unlinked-node highlight ring
  var LABEL = "#C9D2CC";   // label text token

  function render(data) {
    var nodes = data.nodes.map(function (n) {
      return { id: n.id, title: n.title, type: n.type || "unknown",
               x: 400 + (Math.random() - 0.5) * 200,
               y: 300 + (Math.random() - 0.5) * 200,
               vx: 0, vy: 0, fixed: false, degree: 0 };
    });
    var byId = {}; nodes.forEach(function (n) { byId[n.id] = n; });
    var links = data.edges.map(function (e) {
      return { source: byId[e.from], target: byId[e.to] };
    }).filter(function (l) { return l.source && l.target; });
    links.forEach(function (l) { l.source.degree++; l.target.degree++; });

    // In/out link counts for the info card (from the loaded edges).
    var inCount = {}, outCount = {};
    links.forEach(function (l) {
      outCount[l.source.id] = (outCount[l.source.id] || 0) + 1;
      inCount[l.target.id] = (inCount[l.target.id] || 0) + 1;
    });

    // Type colors: palette order by first-seen type (v1 semantics).
    var typeColors = {};
    nodes.forEach(function (n) {
      if (!(n.type in typeColors)) {
        typeColors[n.type] = PALETTE[Object.keys(typeColors).length % PALETTE.length];
      }
    });

    var svgNS = "http://www.w3.org/2000/svg";
    var root = document.createElementNS(svgNS, "svg");
    root.setAttribute("width", "100%");
    root.setAttribute("height", "100%");
    el.appendChild(root);
    var view = document.createElementNS(svgNS, "g");
    root.appendChild(view);
    var edgeGroup = document.createElementNS(svgNS, "g");
    var nodeGroup = document.createElementNS(svgNS, "g");
    var labelGroup = document.createElementNS(svgNS, "g");
    view.appendChild(edgeGroup); view.appendChild(nodeGroup); view.appendChild(labelGroup);

    var lines = links.map(function (l) {
      var line = document.createElementNS(svgNS, "line");
      line.setAttribute("stroke", EDGE);
      line.setAttribute("stroke-opacity", EDGE_FADE);
      edgeGroup.appendChild(line); return line;
    });
    var circles = nodes.map(function (n) {
      var g = document.createElementNS(svgNS, "g");
      var c = document.createElementNS(svgNS, "circle");
      c.setAttribute("r", Math.max(5, 4 + Math.sqrt(n.degree) * 2));
      c.setAttribute("fill", typeColors[n.type]);
      c.setAttribute("stroke", STROKE); c.setAttribute("stroke-width", "1");
      c.style.cursor = "grab";
      // Orphan highlight (v1): unlinked nodes get a red ring.
      if (n.degree === 0) {
        var ring = document.createElementNS(svgNS, "circle");
        ring.setAttribute("r", Math.max(5, 4 + Math.sqrt(n.degree) * 2) + 3);
        ring.setAttribute("fill", "none");
        ring.setAttribute("stroke", ORPHAN);
        ring.setAttribute("stroke-width", "1.5");
        ring.setAttribute("opacity", "0.8");
        g.appendChild(ring);
      }
      var t = document.createElementNS(svgNS, "title");
      t.textContent = n.title + " (" + n.id + ")";
      g.appendChild(c); g.appendChild(t);
      g.addEventListener("click", function () {
        if (dragMoved) return; // it was a drag, not a click
        window.location.href = "/concept?path=" + encodeURIComponent(n.id);
      });
      g.addEventListener("mouseenter", function () { showInfo(n); });
      g.addEventListener("mouseleave", hideInfo);
      nodeGroup.appendChild(g); return g;
    });
    var labels = nodes.map(function (n) {
      var t = document.createElementNS(svgNS, "text");
      t.setAttribute("fill", LABEL); t.setAttribute("font-size", "10");
      t.setAttribute("text-anchor", "middle");
      t.setAttribute("pointer-events", "none");
      t.textContent = n.title.length > 24 ? n.title.slice(0, 23) + "\u2026" : n.title;
      labelGroup.appendChild(t); return t;
    });

    // Legend (mockup 06): one dot + label per type present in the
    // loaded data, built into the page's #graph-legend overlay; the
    // orphan marker explains the danger ring when unlinked nodes exist.
    // Static styling lives in style.css (.row/.dot classes); only the
    // data-driven dot color rides a --dot custom property.
    if (legend) {
      Object.keys(typeColors).forEach(function (t) {
        var row = document.createElement("div");
        row.className = "row";
        var sw = document.createElement("span");
        sw.className = "dot";
        sw.style.setProperty("--dot", typeColors[t]);
        var name = document.createElement("span");
        name.textContent = t;
        row.appendChild(sw); row.appendChild(name);
        legend.appendChild(row);
      });
      if (nodes.some(function (n) { return n.degree === 0; })) {
        var row = document.createElement("div");
        row.className = "row";
        var sw = document.createElement("span");
        sw.className = "dot dot--orphan";
        var name = document.createElement("span");
        name.textContent = "orphan (unlinked)";
        row.appendChild(sw); row.appendChild(name);
        legend.appendChild(row);
      }
      if (legend.firstChild) legend.hidden = false;
    }

    // Node info card (mockup 06): title, type chip, in/out link counts,
    // path, and the open link \u2014 DOM-built (no innerHTML with data).
    function showInfo(n) {
      if (!info) return;
      info.hidden = false;
      info.innerHTML = "";
      var h = document.createElement("h3"); h.textContent = n.title;
      info.appendChild(h);
      var meta = document.createElement("div");
      meta.className = "gi-meta";
      var chip = document.createElement("span");
      chip.className = "chip chip--neutral";
      chip.textContent = n.type ? n.type : "concept";
      meta.appendChild(chip);
      var counts = document.createElement("span");
      counts.textContent = (inCount[n.id] || 0) + " in \u00b7 " + (outCount[n.id] || 0) + " out";
      meta.appendChild(counts);
      info.appendChild(meta);
      var path = document.createElement("div");
      path.className = "gi-path"; path.textContent = n.id;
      info.appendChild(path);
      var a = document.createElement("a");
      a.href = "/concept?path=" + encodeURIComponent(n.id);
      a.textContent = "open concept \u2192";
      info.appendChild(a);
    }
    function hideInfo() { if (info) info.hidden = true; }

    // --- pan / zoom ---
    var scale = 1, panX = 0, panY = 0;
    function applyView() {
      view.setAttribute("transform", "translate(" + panX + "," + panY + ") scale(" + scale + ")");
    }
    var panning = false, lastPX = 0, lastPY = 0;
    el.addEventListener("mousedown", function (e) {
      // A press on a node (or anything inside the node layer) is a node
      // drag, never a pan.
      if (nodeGroup.contains(e.target)) return;
      panning = true; lastPX = e.clientX; lastPY = e.clientY;
      el.classList.add("dragging");
    });
    window.addEventListener("mousemove", function (e) {
      if (!panning) return;
      panX += e.clientX - lastPX; panY += e.clientY - lastPY;
      lastPX = e.clientX; lastPY = e.clientY;
      applyView();
    });
    window.addEventListener("mouseup", function () {
      panning = false; el.classList.remove("dragging");
    });
    el.addEventListener("wheel", function (e) {
      e.preventDefault();
      var rect = el.getBoundingClientRect();
      var mx = e.clientX - rect.left, my = e.clientY - rect.top;
      var factor = e.deltaY < 0 ? 1.12 : 1 / 1.12;
      var ns = Math.min(4, Math.max(0.2, scale * factor));
      panX = mx - (mx - panX) * (ns / scale);
      panY = my - (my - panY) * (ns / scale);
      scale = ns; applyView();
    }, { passive: false });

    // --- node dragging (d3-style: reheat the simulation) ---
    var dragged = null, dragMoved = false;
    var alpha = 1, tickScheduled = false;
    circles.forEach(function (g, i) {
      g.addEventListener("mousedown", function (e) {
        dragged = nodes[i];
        dragged.fixed = true;
        dragMoved = false;
        alpha = Math.max(alpha, 0.3); // reheat
        scheduleTick();
        e.stopPropagation();
        e.preventDefault();
      });
    });
    window.addEventListener("mousemove", function (e) {
      if (!dragged) return;
      dragMoved = true;
      var rect = el.getBoundingClientRect();
      dragged.x = (e.clientX - rect.left - panX) / scale;
      dragged.y = (e.clientY - rect.top - panY) / scale;
      dragged.vx = 0; dragged.vy = 0;
      alpha = Math.max(alpha, 0.25); // keep the layout live while holding
    });
    window.addEventListener("mouseup", function () {
      if (dragged) {
        dragged.fixed = false;
        dragged = null;
        alpha = Math.max(alpha, 0.3); // reheat: let the graph settle
        scheduleTick();
      }
    });

    // --- simulation (restartable) ---
    function scheduleTick() {
      if (tickScheduled) return;
      tickScheduled = true;
      requestAnimationFrame(function () { tickScheduled = false; tick(); });
    }
    function tick() {
      alpha *= 0.99;
      var W = el.clientWidth || 900, H = el.clientHeight || 500;
      for (var i = 0; i < nodes.length; i++) {
        var a = nodes[i];
        for (var j = i + 1; j < nodes.length; j++) {
          var b = nodes[j];
          var dx = b.x - a.x, dy = b.y - a.y;
          var d2 = dx * dx + dy * dy || 0.01;
          var f = (1200 / d2) * alpha;
          var d = Math.sqrt(d2);
          a.vx -= (dx / d) * f; a.vy -= (dy / d) * f;
          b.vx += (dx / d) * f; b.vy += (dy / d) * f;
        }
      }
      links.forEach(function (l) {
        var dx = l.target.x - l.source.x, dy = l.target.y - l.source.y;
        var d = Math.sqrt(dx * dx + dy * dy) || 0.01;
        var f = ((d - 120) * 0.02) * alpha;
        l.source.vx += (dx / d) * f; l.source.vy += (dy / d) * f;
        l.target.vx -= (dx / d) * f; l.target.vy -= (dy / d) * f;
      });
      nodes.forEach(function (n) {
        if (n.fixed) return; // the dragged node is positioned by the cursor
        n.vx += (W / 2 - n.x) * 0.002 * alpha;
        n.vy += (H / 2 - n.y) * 0.002 * alpha;
        n.x += n.vx * 0.5; n.y += n.vy * 0.5;
        n.vx *= 0.85; n.vy *= 0.85;
        n.x = Math.max(20, Math.min(W - 20, n.x));
        n.y = Math.max(20, Math.min(H - 20, n.y));
      });
      links.forEach(function (l, i) {
        lines[i].setAttribute("x1", l.source.x); lines[i].setAttribute("y1", l.source.y);
        lines[i].setAttribute("x2", l.target.x); lines[i].setAttribute("y2", l.target.y);
      });
      nodes.forEach(function (n, i) {
        circles[i].setAttribute("transform", "translate(" + n.x + "," + n.y + ")");
        labels[i].setAttribute("x", n.x);
        labels[i].setAttribute("y", n.y + 18);
      });
      // Stay alive while the layout is hot OR a node is held.
      if (alpha > 0.02 || dragged) scheduleTick();
    }
    scheduleTick();
  }
})();
"##;

/// The chat page's client logic (external asset — CSP-safe: the site
/// policy is script-src 'self' 'wasm-unsafe-eval' + nonce, so inline
/// scripts are blocked; /assets/chat.js loads under 'self').
///
/// Chat-session-history architecture, session-scoped: the page's
/// #chat-log `data-*` attrs (Task 6's SSR contract) are the load-time
/// snapshot — `data-session` (UUID, or "new" for the draft),
/// `data-max-seq` (the transcript cursor), `data-turn-status`
/// (running/done/failed/interrupted/none), `data-csrf` (the token the
/// POSTs replay). What lives here: the session state, the shared SSE
/// pump + transcript-seq dedup (the server double-sends across the
/// snapshot/live overlap by design — the client drops duplicates by
/// `seq`), the draft flow (POST /api/v1/chat/sessions; the stream's
/// FIRST event is `session` and flips the URL via history.replaceState),
/// submit into an existing session (POST
/// /api/v1/chat/sessions/{id}/messages), load-time reattach (a turn
/// the SSR page says is `running` reopens GET
/// /api/v1/chat/sessions/{id}/stream?after=<cursor>; user rows never
/// replay, and the pending bubble pre-exists from SSR), the
/// stream-drop reopen loop (≤ 3 attempts, 2 s apart — the transcript
/// is the fallback truth), and the meta liveness poll (GET
/// /api/v1/chat/sessions/{id}, 5 s interval) that notices the resume
/// worker's claim (turn_status → running → reopen the stream with the
/// cursor) or a turn finishing elsewhere (done/failed → one final
/// replay fetch collects the rows the page missed). The composer's
/// EVENT wiring (submit binding, Enter-to-send, textarea auto-grow)
/// lives in the chat_composer hydration island (mycelium-ui
/// islands.rs), which hands each send to `window.myceliumChatSend` —
/// `true` when the transport accepted the message, `false` when it
/// refused (busy, or empty after trim); the island clears the composer
/// only on `true`, so input typed while the librarian streams stays in
/// the textarea. The log's DOM structure (addMsg + the XSS-safe
/// markdown renderer) is unchanged from the pre-session chat.js.
/// Without the hydrate bundle the composer is an inert form and the
/// helper is never called — the page degrades to SSR-only (the
/// sidebar, the transcript, and the native delete forms all work
/// without JS).
pub const CHAT_JS: &str = r#"// Librarian chat: session-scoped SSE transport + log management
// for /api/v1/chat/sessions (spec §3.5/§3.6). The chat_composer
// hydration island owns the composer's event wiring and calls
// window.myceliumChatSend; this file owns the session state, the
// fetches, the SSE pump, the reattach/reopen/liveness-poll machinery,
// and the log rendering.
(function () {
  var log = document.getElementById("chat-log");
  if (!log) return;
  var form = document.getElementById("chat-form");
  var sendBtn = form ? form.querySelector("button[type=submit]") : null;

  // Session state. The SSR data-* attrs are the load-time snapshot;
  // everything below re-derives from streams and meta responses.
  var sessionId = log.dataset.session || "new";
  var lastSeq = parseInt(log.dataset.maxSeq, 10) || 0;
  var turnStatus = log.dataset.turnStatus || "none";
  var busy = false;
  var seen = {}; // transcript-seq dedup across every stream
  var pending = null; // the live "thinking…" bubble, if any
  var steps = []; // its tool-progress lines
  var reopenAttempts = 0;
  var reopenTimer = null;
  var pollTimer = null;

  // Tunables (spec §4): ≤ 3 reopen attempts, 2 s apart; 5 s meta poll.
  var REOPEN_MAX = 3;
  var REOPEN_DELAY_MS = 2000;
  var POLL_INTERVAL_MS = 5000;

  // ---- Layout fixes (2026-10-15) -------------------------------------
  // Sidebar collapse (fix #5): the toggle flips the pane's
  // chat-sessions--collapsed class; the state persists per browser
  // (localStorage, per-viewer convenience only). Without JS the pane
  // simply stays open.
  var SIDEBAR_KEY = "myc2-chat-sidebar";
  var sidebarPane = document.querySelector(".chat-sessions");
  var sidebarToggle = document.getElementById("chat-sidebar-toggle");
  function applySidebar(collapsed) {
    if (!sidebarPane) return;
    if (collapsed) sidebarPane.classList.add("chat-sessions--collapsed");
    else sidebarPane.classList.remove("chat-sessions--collapsed");
    if (sidebarToggle) sidebarToggle.setAttribute("aria-expanded", collapsed ? "false" : "true");
  }
  if (sidebarToggle) {
    sidebarToggle.addEventListener("click", function () {
      var collapsed = !(sidebarPane && sidebarPane.classList.contains("chat-sessions--collapsed"));
      applySidebar(collapsed);
      try { localStorage.setItem(SIDEBAR_KEY, collapsed ? "1" : "0"); } catch (e) { /* private mode */ }
    });
  }
  try {
    if (localStorage.getItem(SIDEBAR_KEY) === "1") applySidebar(true);
  } catch (e) { /* private mode: default open */ }

  // Composer reset (fix #2 companion): the island fires
  // chat-composer-reset when a send is accepted — drop any explicit
  // height so the auto-grow ratchet starts fresh (a manual drag is
  // the user's to keep until the next accepted send).
  var composer = document.getElementById("chat-input");
  if (composer) {
    composer.addEventListener("chat-composer-reset", function () {
      composer.style.height = "";
    });
  }
  // --------------------------------------------------------------------

  function setBusy(state) {
    busy = state;
    if (sendBtn) sendBtn.disabled = state;
  }
  function addMsg(text, who) {
    var div = document.createElement("div");
    div.className = "chat-msg " + who;
    if (who.indexOf("librarian") === 0) {
      renderMarkdown(div, text);
    } else {
      div.textContent = text;
    }
    log.appendChild(div);
    log.scrollTop = log.scrollHeight;
    return div;
  }
  // Minimal XSS-safe markdown renderer: builds DOM nodes only (never
  // innerHTML with model text). Supports: fenced code blocks, headings,
  // bullet lists, inline code, bold, and [text](/path) links (relative
  // or same-origin only — no javascript:, no external hrefs).
  function renderMarkdown(container, text) {
    var lines = text.split("\n");
    var i = 0;
    var list = null;
    function flushList() { if (list) { list = null; } }
    function inline(parent, str) {
      // Split on `code`, **bold**, and [text](url) — build nodes.
      var re = /(`[^`]+`|\*\*[^*]+\*\*|\[[^\]]+\]\([^)]+\))/g;
      var last = 0, m;
      while ((m = re.exec(str)) !== null) {
        if (m.index > last) parent.appendChild(document.createTextNode(str.slice(last, m.index)));
        var tok = m[0];
        if (tok.charAt(0) === "`") {
          var code = document.createElement("code");
          code.textContent = tok.slice(1, -1);
          parent.appendChild(code);
        } else if (tok.charAt(0) === "*") {
          var b = document.createElement("strong");
          b.textContent = tok.slice(2, -2);
          parent.appendChild(b);
        } else {
          var lm = /^\[([^\]]+)\]\(([^)]+)\)$/.exec(tok);
          if (lm) {
            var url = lm[2];
            var ok = url.charAt(0) === "/" || url.indexOf("https://") === 0;
            if (ok) {
              var a = document.createElement("a");
              a.textContent = lm[1];
              a.href = url;
              if (url.indexOf("http") === 0) a.rel = "noopener noreferrer";
              parent.appendChild(a);
            } else {
              parent.appendChild(document.createTextNode(lm[1] + " (" + url + ")"));
            }
          } else {
            parent.appendChild(document.createTextNode(tok));
          }
        }
        last = m.index + tok.length;
      }
      if (last < str.length) parent.appendChild(document.createTextNode(str.slice(last)));
    }
    while (i < lines.length) {
      var line = lines[i];
      if (line.indexOf("```") === 0) {
        flushList();
        var pre = document.createElement("pre");
        var codeEl = document.createElement("code");
        i++;
        var codeLines = [];
        while (i < lines.length && lines[i].indexOf("```") !== 0) {
          codeLines.push(lines[i]);
          i++;
        }
        i++; // skip closing fence
        codeEl.textContent = codeLines.join("\n");
        pre.appendChild(codeEl);
        container.appendChild(pre);
        continue;
      }
      var h = /^(#{1,4}) (.*)$/.exec(line);
      if (h) {
        flushList();
        var heading = document.createElement("h" + (h[1].length + 2 > 6 ? 6 : h[1].length + 2));
        inline(heading, h[2]);
        container.appendChild(heading);
      } else if (/^[-*] /.test(line)) {
        if (!list) { list = document.createElement("ul"); container.appendChild(list); }
        var li = document.createElement("li");
        inline(li, line.slice(2));
        list.appendChild(li);
      } else if (/^(\d+)\. /.test(line)) {
        if (!list) { list = document.createElement("ol"); container.appendChild(list); }
        var oli = document.createElement("li");
        inline(oli, line.replace(/^\d+\. /, ""));
        list.appendChild(oli);
      } else if (line.trim() === "") {
        flushList();
      } else {
        flushList();
        var p = document.createElement("p");
        inline(p, line);
        container.appendChild(p);
      }
      i++;
    }
  }

  // The pending bubble. send() creates a fresh one (it owns the turn);
  // a reattach/reopen stream adopts the pre-existing one — SSR renders
  // it for a running turn, and a mid-run reopen inherits the send()'s
  // element via `pending` staying set. adoptPending() re-queries the
  // SSR-rendered bubble when the module-level var is stale (page
  // loaded mid-run: pending is null) — every terminal-branch removal
  // goes through pendingRemove's fallback, so a zero-tool turn's
  // replayed/collections reply still replaces the stuck SSR bubble.
  function adoptPending() {
    if (pending && pending.isConnected) return;
    pending = log.querySelector(".chat-msg.librarian.pending");
  }
  function renderPending() {
    if (!pending) return;
    var text = "The librarian is thinking…";
    if (steps.length) text += "\n\n" + steps.join("\n");
    pending.textContent = text;
    log.scrollTop = log.scrollHeight;
  }
  function pendingRemove() {
    adoptPending();
    if (pending) { pending.remove(); pending = null; }
    steps = [];
  }
  function clearStatusNote() {
    var note = log.querySelector(".chat-msg.chat-status-note");
    if (note) note.remove();
  }

  // Shared event handling for every stream (submit, reattach, final
  // replay). `state.sawTerminal` marks a replayed terminal row — its
  // producing turn finalized, so this stream's end is not a drop.
  function handleEvent(ev, state) {
    // The draft flow's `session` event is handled by TYPE, before the
    // seq gate: its seq 0 is an endpoint-generated sentinel (never a
    // transcript row, never replayed), and keying the falsy 0 into
    // the dedup map would special-case it. Only the draft flow emits
    // this event.
    if (ev.type === "session") {
      sessionId = ev.session_id;
      history.replaceState(null, "", "/chat?s=" + sessionId);
      return;
    }
    // The dedup contract (spec §3.3): events double-send across the
    // snapshot/live overlap by design — the transcript row's `seq` is
    // the client-side dedup key.
    if (ev.seq === undefined || ev.seq === null || seen[ev.seq]) return;
    seen[ev.seq] = true;
    if (ev.seq > lastSeq) lastSeq = ev.seq;
    if (ev.type === "tool") {
      adoptPending();
      steps.push("· " + ev.name + (ev.detail ? " (" + ev.detail + ")" : ""));
      renderPending();
    } else if (ev.type === "assistant") {
      // Replay vocabulary (a persisted reply row). Render like a live
      // reply; it also proves the turn finalized — if the page still
      // holds busy, settle it at the stream's end instead of
      // reopening.
      pendingRemove();
      addMsg(ev.reply, "librarian");
      state.sawTerminal = true;
    } else if (ev.type === "done") {
      pendingRemove();
      addMsg(ev.reply, "librarian");
      setBusy(false);
    } else if (ev.type === "error") {
      pendingRemove();
      addMsg(ev.error, "error");
      setBusy(false);
      state.sawTerminal = true;
    }
  }

  // SSE pump: split frames, parse each data line, dispatch.
  function readStream(r, state) {
    var reader = r.body.getReader();
    var decoder = new TextDecoder();
    var buf = "";
    function pump() {
      return reader.read().then(function (chunk) {
        if (chunk.done) return;
        buf += decoder.decode(chunk.value, { stream: true });
        var parts = buf.split("\n\n");
        buf = parts.pop();
        parts.forEach(function (part) {
          var line = part.replace(/^data: /, "");
          if (!line) return;
          var ev;
          try { ev = JSON.parse(line); } catch (e) { return; }
          handleEvent(ev, state);
        });
        return pump();
      });
    }
    return pump();
  }

  // A stream ended. The transcript is the truth: a close with the
  // turn still busy (and no replayed terminal row) is a drop —
  // reopen against the store cursor; anything else settled inside
  // the stream.
  function onStreamEnd(state) {
    if (!busy) return;
    if (state.sawTerminal) {
      // The producing turn finalized between the SSR render and the
      // reattach stream (the terminal row replayed; the live Done was
      // broadcast before this client could subscribe).
      setBusy(false);
      return;
    }
    scheduleReopen();
  }

  // Stream-drop reopen (spec §3.6/§4): ≤ 3 attempts, 2 s apart; the
  // transcript fetched against the ?after= cursor is the fallback
  // truth. Exhausted → the meta poll takes over.
  function scheduleReopen() {
    if (reopenTimer) return; // one chain at a time
    if (reopenAttempts >= REOPEN_MAX) {
      // Budget exhausted: the meta poll takes over (3 attempts gone).
      turnStatus = "unknown";
      startPoll();
      return;
    }
    reopenAttempts += 1;
    reopenTimer = setTimeout(function () {
      reopenTimer = null;
      openStream();
    }, REOPEN_DELAY_MS);
  }

  // The reattach/reopen stream: replay every transcript row after the
  // cursor (tool → pending steps, assistant/error → rendered bubbles,
  // user rows never replay), then tail the live turn. A turn with no
  // live entry replays and closes immediately (poll semantics).
  function openStream() {
    var url =
      "/api/v1/chat/sessions/" + encodeURIComponent(sessionId) +
      "/stream?after=" + lastSeq;
    fetch(url, { headers: { "accept": "text/event-stream" } })
      .then(function (r) {
        if (!r.ok || !r.body) {
          // Permanent: the endpoint refused (gone/owned-by-nobody/
          // server error) — reopening never fixes it. Surface once,
          // settle, and stop.
          return r
            .json()
            .catch(function () { return {}; })
            .then(function (j) {
              throw { permanent: true, message: (j && j.error) || ("HTTP " + r.status) };
            });
        }
        var state = { sawTerminal: false };
        return readStream(r, state).then(function () { onStreamEnd(state); });
      })
      .catch(function (err) {
        if (err && err.permanent) {
          pendingRemove();
          setBusy(false);
          addMsg(err.message || "the chat session is unavailable", "error");
          return;
        }
        // A network drop mid-stream (or a failed connect): transient
        // — the reopen loop owns it.
        onStreamEnd({ sawTerminal: false });
      });
  }

  // Liveness poll (spec §3.6): watch the session meta until the turn
  // moves — how an open page notices the resume worker's claim (then
  // it reopens the stream with the cursor) or a turn finishing
  // elsewhere (then one final replay collects the missed rows).
  function startPoll() {
    if (pollTimer) return;
    pollTimer = setInterval(pollMeta, POLL_INTERVAL_MS);
  }
  function stopPoll() {
    if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
  }
  function metaUrl() {
    return "/api/v1/chat/sessions/" + encodeURIComponent(sessionId);
  }
  function pollMeta() {
    if (!sessionId || sessionId === "new") return; // drafts have no meta
    // THIS poll's replay cursor, captured before the max_seq refresh:
    // rows persist before the status flips, so the terminal row's seq
    // is ≤ the refreshed max_seq — a replay against the refreshed
    // cursor would replay nothing (messages_after is strict >). The
    // seen map absorbs any overlap with rows already streamed.
    var cursor = lastSeq;
    fetch(metaUrl(), { headers: { "accept": "application/json" } })
      .then(function (r) {
        if (r.status === 404) throw { gone: true, message: "not found" };
        if (!r.ok) throw { transient: true };
        return r.json();
      })
      .then(function (meta) {
        if (meta.max_seq > lastSeq) lastSeq = meta.max_seq;
        var st = meta.turn_status;
        if (st === "running") {
          // The resume worker claimed the turn: reattach with the
          // cursor; the pending bubble pre-exists. Fresh reopen
          // budget for the claimed turn.
          stopPoll();
          turnStatus = "running";
          reopenAttempts = 0;
          setBusy(true);
          clearStatusNote();
          openStream();
        } else if (st === "done" || st === "failed") {
          // The turn finished elsewhere: one replay fetch closes
          // immediately (poll semantics) and collects every row
          // missed since the PRE-BUMP cursor (rows persist before the
          // status flips, so the terminal row sits at or below the
          // refreshed max_seq — the refreshed cursor would replay
          // nothing), then the page settles. The seen map absorbs any
          // overlap with rows already streamed.
          stopPoll();
          finalReplay(cursor);
        } else if (st === "interrupted" && meta.turn_resumable === false) {
          // The worker will never claim this one (its resume attempt
          // was spent): stop probing — a manual submit or a reload
          // re-engages. Rows the spent attempt persisted (e.g. the
          // LLM-down error row) are still collected — replayed
          // against the PRE-BUMP cursor (strict >; the seen map
          // absorbs re-delivered rows); the turn stays interrupted,
          // so the SSR note stays (finalReplay's note-clearing is
          // only for a terminal transition).
          stopPoll();
          if (meta.max_seq > cursor) {
            fetch(metaUrl() + "/stream?after=" + cursor, {
              headers: { "accept": "text/event-stream" }
            })
              .then(function (r) {
                if (!r.ok || !r.body) return;
                return readStream(r, { sawTerminal: false });
              })
              .catch(function () { /* the next reload re-syncs */ });
          } else {
            setBusy(false);
          }
        }
        // interrupted+resumable (or unknown/null): keep polling.
      })
      .catch(function (err) {
        if (err && err.gone) {
          // Deleted elsewhere — the same surfaced body both paths
          // show ("not found", the endpoint's generic body).
          stopPoll();
          pendingRemove();
          setBusy(false);
          addMsg(err.message || "this chat session is no longer available", "error");
          return;
        }
        // Transient (network down mid-restart, server blip): the
        // interval keeps probing; a restart heals it.
      });
  }
  // The poll's terminal transition: one replay fetch (closes
  // immediately — no live turn) against the CALLER's pre-bump cursor,
  // then settle: release the composer, clear the resume note.
  function finalReplay(cursor) {
    fetch(metaUrl() + "/stream?after=" + cursor, {
      headers: { "accept": "text/event-stream" }
    })
      .then(function (r) {
        if (!r.ok || !r.body) return;
        return readStream(r, { sawTerminal: false });
      })
      .catch(function () { /* the next reload re-syncs */ })
      .then(function () {
        setBusy(false);
        clearStatusNote();
      });
  }

  // Returns true when the transport accepted the message (logged +
  // fetch kicked off), false when it refused (busy). The composer
  // island clears the textarea only on true.
  function send(msg) {
    if (busy) return false;
    var draft = !sessionId || sessionId === "new";
    var url = draft
      ? "/api/v1/chat/sessions"
      : "/api/v1/chat/sessions/" + encodeURIComponent(sessionId) + "/messages";
    // The submit stream carries this turn's live tail itself: drop any
    // liveness poll (a superseded interrupted turn is not watched) and
    // any pending reopen timer, with a fresh reopen budget.
    stopPoll();
    if (reopenTimer) { clearTimeout(reopenTimer); reopenTimer = null; }
    reopenAttempts = 0;
    addMsg(msg, "user");
    setBusy(true);
    pending = addMsg("The librarian is thinking…", "librarian pending");
    steps = [];
    var csrfMeta = document.querySelector('meta[name="csrf-token"]');
    var token = log.dataset.csrf || (csrfMeta ? csrfMeta.content : "");
    var body = new URLSearchParams();
    body.set("message", msg);
    body.set("csrf_token", token);
    fetch(url, {
      method: "POST",
      headers: {
        "content-type": "application/x-www-form-urlencoded",
        "x-csrf-token": token,
        "accept": "text/event-stream"
      },
      body: body.toString()
    })
      .then(function (r) {
        if (!r.ok || !r.body) {
          return r
            .json()
            .catch(function () { return {}; })
            .then(function (j) {
              throw new Error((j && j.error) || ("HTTP " + r.status));
            });
        }
        var state = { sawTerminal: false };
        return readStream(r, state).then(function () {
          onStreamEnd(state);
        });
      })
      .catch(function (err) {
        pendingRemove();
        setBusy(false);
        addMsg(err.message || "request failed", "error");
      });
    return true;
  }

  // The composer island's entry point (Task 7): each trimmed message
  // arrives here. Returns the transport's verdict — false when busy
  // or empty after trim — so the island clears the composer only on
  // acceptance and typed-during-streaming input stays put. Without
  // the hydrate bundle this is never called.
  window.myceliumChatSend = function (msg) {
    var m = String(msg || "").trim();
    if (!m) return false;
    return send(m);
  };

  // Load-time liveness: a turn the SSR page says is running reattaches
  // immediately (the pending bubble pre-exists; the composer holds
  // busy), an interrupted turn starts the meta poll (the resume worker
  // owns the claim — its transition to running flips this page back to
  // the stream), and done/failed/none need nothing.
  if (turnStatus === "running") {
    setBusy(true);
    openStream();
  } else if (turnStatus === "interrupted") {
    startPoll();
  }
})();
"#;

/// The /account passkey enrollment wiring (external asset — CSP-safe
/// like chat.js; loads under script-src 'self'). Fetches the
/// creation options from GET /account/webauthn/challenge, runs
/// `navigator.credentials.create`, and POSTs the resulting credential
/// to /account/webauthn/register as JSON, then reloads so the row
/// re-renders with the new count. Without this script (or on a
/// browser without WebAuthn support) the Register button stays in its
/// SSR state — disabled — and the page degrades to state-only rows:
/// the honest script-less degradation spec §7 pins. Encoding: the
/// server speaks base64url without padding (base64urlsafedata's wire
/// form); the helpers convert between that and the browser's
/// ArrayBuffer.
pub const WEBAUTHN_JS: &str = r#"// Passkey enrollment on /account: fetch the creation options, run the
// browser ceremony, POST the credential, reload. SSR renders the
// Register button disabled — this script (when WebAuthn is available)
// enables it, so no script means no ceremony, never a broken one.
(function () {
  var btn = document.querySelector("[data-webauthn-register]");
  if (!btn || !navigator.credentials || !window.PublicKeyCredential) return;
  btn.disabled = false;
  btn.addEventListener("click", function (e) {
    e.preventDefault();
    btn.disabled = true;
    var csrfMeta = document.querySelector('meta[name="csrf-token"]');
    fetch("/account/webauthn/challenge")
      .then(function (r) { if (!r.ok) throw new Error("challenge failed"); return r.json(); })
      .then(function (opts) {
        var pk = opts.publicKey;
        pk.challenge = toBytes(pk.challenge);
        pk.user.id = toBytes(pk.user.id);
        return navigator.credentials.create({ publicKey: pk });
      })
      .then(function (cred) {
        return fetch("/account/webauthn/register", {
          method: "POST",
          headers: {
            "content-type": "application/json",
            "x-csrf-token": csrfMeta ? csrfMeta.content : ""
          },
          body: JSON.stringify({
            id: cred.id,
            rawId: b64url(cred.rawId),
            response: {
              attestationObject: b64url(cred.response.attestationObject),
              clientDataJSON: b64url(cred.response.clientDataJSON)
            },
            type: cred.type
          })
        });
      })
      .then(function (r) {
        if (!r.ok) throw new Error("registration failed");
        window.location.reload();
      })
      .catch(function (err) {
        alert("passkey enrollment failed: " + (err && err.message ? err.message : "cancelled"));
        btn.disabled = false;
      });
  });
  function toBytes(b64) {
    var s = b64.replace(/-/g, "+").replace(/_/g, "/");
    while (s.length % 4) s += "=";
    s = atob(s);
    var out = new Uint8Array(s.length);
    for (var i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
    return out;
  }
  function b64url(buf) {
    var bytes = new Uint8Array(buf);
    var bin = "";
    for (var i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
    return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }
})();
"#;

/// Confirm-dialog wiring (external asset — CSP-safe like chat.js):
/// `[data-confirm-dialog]` triggers open the page's named native
/// `<dialog>`. Before `showModal()`, the trigger's `data-key-id` (when
/// present) is copied into the dialog form's hidden `id` input, and the
/// trigger's own form's `action` is copied onto the dialog form —
/// per-row action URLs (the chat sidebar's `.../sessions/{id}/delete`)
/// point the dialog at the SAME row the clicked button belongs to,
/// while the static-dialog pages (concept delete, keys revoke) set the
/// same action on both so the copy is a no-op. Cancel buttons
/// (`[data-confirm-cancel]`) and the backdrop close without
/// submitting. The confirm button is a plain form submit — the form's
/// own action + hidden fields (including the CSRF token) carry the
/// POST, so the CSRF token remains the security gate.
pub const CONFIRM_JS: &str = r#"// Confirm dialogs: [data-confirm-dialog] triggers open the named
// native <dialog> (copying data-key-id into the form's hidden id
// input, and the trigger's form action onto the dialog form, first);
// cancel/backdrop close without submitting. The CSRF token in the
// form remains the security gate.
(function () {
  document.addEventListener("click", function (e) {
    var t = e.target.closest("[data-confirm-dialog]");
    if (t) {
      e.preventDefault();
      var d = document.getElementById(t.dataset.confirmDialog);
      if (d) {
        var form = d.querySelector("form");
        var triggerForm = t.closest("form");
        if (form && triggerForm && triggerForm.action) {
          form.setAttribute("action", triggerForm.action);
        }
        var idInput = d.querySelector('input[name="id"]');
        if (idInput && t.dataset.keyId) idInput.value = t.dataset.keyId;
        d.showModal();
      }
    }
    if (e.target.matches("[data-confirm-cancel]")) {
      e.target.closest("dialog").close();
    }
    if (e.target.matches("dialog")) {
      e.target.close();
    }
  });
})();
"#;

/// The islands traversal script (external asset — CSP-safe like
/// chat.js). Adapted from leptos's own `island_script.js` (islands
/// mode): leptos's SSR helper would inline it, but the site CSP
/// blocks inline scripts, so it ships as an external classic script
/// (classic scripts run at parse time; the hydrate module they load
/// executes deferred, after chat.js has exposed its transport). It
/// finds the page's hydrate-bundle module tag (the shell emits it
/// only on island pages), derives the cache-busted `.wasm` URL from
/// its `src` — the wasm-bindgen wrapper's own default would resolve
/// the wasm WITHOUT the `?v=` query, a stale-cache hazard on version
/// bumps — initializes the module (which runs the wasm entry,
/// `hydrate_islands`), then walks the `<leptos-island>` roots and
/// calls each island's exported wasm hydrate function (`data-
/// component` names the export). Hand-vendored: regenerate only when
/// the leptos island protocol changes — the per-island exports
/// themselves are regenerated with the wasm bundle.
///
/// Dropped upstream behaviors (deliberate): this traversal does NOT
/// support islands that take `children` props (upstream threads a
/// leptos-children on-hydrate callback through the walk — the repo's
/// islands take only serialized props) and does NOT await async
/// (thenable) exports — an `#[island(lazy)]` would mis-hydrate here.
/// A future island needing either must grow this script, not just
/// the island.
pub const ISLANDS_JS: &str = r#"// Islands traversal: initialize the hydrate bundle, then walk the
// page's <leptos-island> roots and call each island's exported wasm
// hydrate function (data-component names the export). Deferred to
// DOMContentLoaded because this classic script runs at parse time,
// BEFORE the module tag below it has been parsed; module scripts
// execute before DOMContentLoaded, so chat.js's transport helper is
// available by the time any island sends.
(function () {
  function start() {
    var tag = document.querySelector('script[type="module"][src^="/assets/mycelium_ui.js"]');
    if (!tag) {
      console.warn("islands.js: no hydrate bundle module tag on this page — islands stay server-rendered");
      return;
    }
    var src = tag.getAttribute("src");
    var wasmUrl = src.replace("mycelium_ui.js", "mycelium_ui_bg.wasm");
    import(src).then(function (mod) {
      mod.default({ module_or_path: wasmUrl }).then(function () {
        function traverse(node) {
          if (node.nodeType === Node.ELEMENT_NODE) {
            if (node.tagName.toLowerCase() === "leptos-island") {
              var id = node.dataset.component;
              if (id && mod[id]) {
                mod[id](node);
              } else {
                console.warn("islands.js: no exported hydrate function for island '" + id + "' — stale bundle?");
              }
            }
            var children = node.children;
            for (var i = 0; i < children.length; i++) traverse(children[i]);
          }
        }
        traverse(document.body);
      });
    });
  }
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start);
  } else {
    start();
  }
})();
"#;

/// The mycelium-ui hydrate bundle's JS wrapper — wasm-bindgen output
/// (`--target web`), NOT hand-written: do not edit. It fetches its
/// sibling `mycelium_ui_bg.wasm` by filename, so the pair's names are
/// load-bearing. The `&str` embedding is only for the scaffold — the
/// bundle is served from disk like every other asset.
pub const HYDRATE_JS: &str = include_str!("../assets/mycelium_ui.js");

/// The mycelium-ui hydrate bundle's compiled wasm — wasm-bindgen
/// output, the sibling module `HYDRATE_JS` fetches at runtime.
pub static HYDRATE_WASM: &[u8] = include_bytes!("../assets/mycelium_ui_bg.wasm");

/// The default assets' content version. Bumped when the built-in
/// defaults change; a mismatching (or missing) marker file triggers a
/// refresh, so upgrades deliver new defaults while admins can still
/// customize (delete the marker to opt out of refreshes, or restore it
/// to re-opt-in on the next boot).
pub const ASSETS_VERSION: &str = "29";

/// Write the default assets to `assets_dir`. First boot writes
/// everything; later boots refresh the defaults when the version
/// marker is stale (upgrade path) — a present, matching marker means
/// leave the files alone (admin customization preserved).
pub fn scaffold_defaults(assets_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(assets_dir)?;
    let marker = assets_dir.join(".defaults-version");
    let current = std::fs::read_to_string(&marker).unwrap_or_default();
    let refresh = current.trim() != ASSETS_VERSION;
    let files = [
        ("style.css", STYLE_CSS),
        ("app.js", APP_JS),
        ("graph.js", GRAPH_JS),
        ("chat.js", CHAT_JS),
        ("confirm.js", CONFIRM_JS),
        ("islands.js", ISLANDS_JS),
        ("webauthn.js", WEBAUTHN_JS),
        // The hydrate bundle is a bindgen output pair — the JS wrapper
        // fetches `mycelium_ui_bg.wasm` by filename, so both names are
        // load-bearing. The wasm is written separately after the loop
        // (the array is `&str` pairs).
        ("mycelium_ui.js", HYDRATE_JS),
    ];
    for (name, contents) in files {
        let path = assets_dir.join(name);
        if !path.exists() || refresh {
            std::fs::write(path, contents)?;
        }
    }
    let wasm_path = assets_dir.join("mycelium_ui_bg.wasm");
    if !wasm_path.exists() || refresh {
        std::fs::write(&wasm_path, HYDRATE_WASM)?;
    }
    if refresh {
        std::fs::write(&marker, ASSETS_VERSION)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffolds_once() {
        let dir = tempfile::tempdir().unwrap();
        scaffold_defaults(dir.path()).unwrap();
        assert!(dir.path().join("style.css").exists());
        assert!(dir.path().join("app.js").exists());
        assert!(dir.path().join("graph.js").exists());
        assert!(dir.path().join("chat.js").exists());
        assert!(dir.path().join("confirm.js").exists());
        assert!(dir.path().join("islands.js").exists());
        assert!(dir.path().join("webauthn.js").exists());
        // Same version: does not overwrite (admin customization safe).
        std::fs::write(dir.path().join("style.css"), "custom").unwrap();
        scaffold_defaults(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("style.css")).unwrap(),
            "custom"
        );
    }

    #[test]
    fn scaffolds_hydrate_bundle() {
        let dir = tempfile::tempdir().unwrap();
        scaffold_defaults(dir.path()).unwrap();
        let js = std::fs::read_to_string(dir.path().join("mycelium_ui.js")).unwrap();
        assert!(js.contains("wasm"), "{js}");
        assert!(dir.path().join("mycelium_ui_bg.wasm").exists());
    }

    #[test]
    fn version_bump_refreshes_defaults() {
        let dir = tempfile::tempdir().unwrap();
        scaffold_defaults(dir.path()).unwrap();
        // Simulate an old-version deployment with customized assets.
        std::fs::write(dir.path().join("style.css"), "old custom").unwrap();
        std::fs::write(dir.path().join(".defaults-version"), "1").unwrap();
        scaffold_defaults(dir.path()).unwrap();
        // Refreshed to the current defaults + marker updated.
        assert!(
            std::fs::read_to_string(dir.path().join("style.css"))
                .unwrap()
                .contains("--bg:")
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".defaults-version")).unwrap(),
            ASSETS_VERSION
        );
        // And idempotent again.
        std::fs::write(dir.path().join("style.css"), "new custom").unwrap();
        scaffold_defaults(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("style.css")).unwrap(),
            "new custom"
        );
    }

    #[test]
    fn v8_stylesheet_defines_every_spec_token() {
        for token in [
            "--color-bg-page",
            "--color-bg-rail",
            "--color-surface",
            "--color-surface-raised",
            "--color-accent",
            "--color-accent-surface",
            "--color-text-heading",
            "--color-text-label",
            "--color-text-body",
            "--color-text-muted",
            "--color-border",
            "--color-danger",
            "--color-danger-surface",
            "--color-warning",
            "--color-warning-surface",
            "--color-chip-surface",
            "--color-chip-surface-alt",
            "--text-page-title",
            "--text-section",
            "--text-body",
            "--text-caption",
            "--radius-card",
            "--radius-control",
            "--radius-pill",
            "--gap-1",
            "--sidebar-width",
            "--rail-width",
            "--content-max",
        ] {
            assert!(
                STYLE_CSS.contains(&format!("{token}:")),
                "missing token {token}"
            );
        }
    }

    #[test]
    fn v8_stylesheet_has_no_external_refs() {
        assert!(!STYLE_CSS.contains("http"));
        assert!(!STYLE_CSS.contains("@import"));
    }

    #[test]
    fn v8_legacy_aliases_present() {
        // Every v7 selector family must still be styled after the swap.
        for selector in [
            ".flash",
            ".flash.ok",
            ".flash.err",
            ".muted",
            "body.graph-page",
            "#graph-wrap",
            "#graph-info",
            ".graph-legend",
            ".graph-hint",
            "body.chat-page",
            ".chat-shell",
            ".chat-log",
            ".chat-input-bar",
            ".chat-msg",
        ] {
            assert!(
                STYLE_CSS.contains(selector),
                "missing legacy alias {selector}"
            );
        }
    }
}
