//! Server-rendered pages (Leptos SSR, no hydration needed for the
//! form-driven UI; the graph page uses graph.js).

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use mycelium_auth::rbac::{Role, SessionUser};
use mycelium_core::concept::Concept;
use mycelium_ui::TrustedHtml;
use uuid::Uuid;

use crate::middleware::SessionId;

/// Common page chrome — a thin wrapper over the mycelium-ui shell.
/// Every page is server-rendered; app.js (CSRF header injection) loads
/// on every page, graph.js only on the graph page. `active_path` is the
/// page's own route constant (drives the sidebar highlight), never
/// derived from the title.
pub fn layout(
    title: &str,
    user: Option<&SessionUser>,
    csrf: &str,
    active_path: &str,
    body: String,
) -> Html<String> {
    layout_with_scripts(title, user, csrf, active_path, body, &[])
}

/// Layout with extra script sources (e.g. graph.js) — no module scripts.
pub fn layout_with_scripts(
    title: &str,
    user: Option<&SessionUser>,
    csrf: &str,
    active_path: &str,
    body: String,
    extra_scripts: &[&str],
) -> Html<String> {
    layout_full(title, user, csrf, active_path, body, extra_scripts, &[], "")
}

/// Full-control layout: extra classic scripts + module scripts (the
/// hydrate bundle) + a body class (e.g. the chat page's full-viewport
/// mode). Adapts the session user to the shell's primitive
/// `(name, is_admin)` pair and delegates the whole document to
/// `mycelium_ui::shell::shell`.
#[allow(clippy::too_many_arguments)]
pub fn layout_full(
    title: &str,
    user: Option<&SessionUser>,
    csrf: &str,
    active_path: &str,
    body: String,
    extra_scripts: &[&str],
    module_scripts: &[&str],
    body_class: &str,
) -> Html<String> {
    let user = user.map(|u| (u.username.as_str(), u.role == Role::Admin));
    Html(mycelium_ui::shell::shell(
        title,
        user,
        csrf,
        active_path,
        body,
        extra_scripts,
        module_scripts,
        body_class,
    ))
}

/// Flash message rendering.
pub fn flash(ok: Option<&str>, err: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(m) = ok {
        out.push_str(&format!(
            r#"<div class="flash ok">{}</div>"#,
            html_escape(m)
        ));
    }
    if let Some(m) = err {
        out.push_str(&format!(
            r#"<div class="flash err">{}</div>"#,
            html_escape(m)
        ));
    }
    out
}

/// The login page — rendered through the outside-shell auth card
/// (`csrf` is accepted for signature stability and always empty here:
/// no session exists yet, so there is no token to embed).
pub fn login_page(csrf: &str, error: Option<&str>) -> Html<String> {
    let _ = csrf;
    let body = format!(
        r#"<h1>Login</h1>
{}
<form method="post" action="/login">
  <label>Username</label><input name="username" required autofocus>
  <label>Password</label><input type="password" name="password" required>
  <label>TOTP code (if enrolled)</label><input name="totp" inputmode="numeric" autocomplete="one-time-code">
  <button type="submit">Log in</button>
</form>"#,
        flash(None, error)
    );
    Html(mycelium_ui::shell::auth_shell("Login", body))
}

/// First-run setup: create the initial admin. No CSRF token — no session
/// exists yet (mirrors /login; app.js still injects an empty field, which
/// the middleware's no-session path exemption ignores).
pub fn setup_page(username: &str, min_len: usize, error: Option<&str>) -> Html<String> {
    let body = format!(
        r#"<h1>Initial setup</h1>
<p>Create the administrator account. This page is available only until the first account exists.</p>
{}<form method="post" action="/setup">
  <label>Admin username</label><input name="username" value="{}" required autofocus>
  <label>Password (min {min_len} characters)</label><input type="password" name="password" minlength="{min_len}" required>
  <label>Confirm password</label><input type="password" name="password_confirm" required>
  <button type="submit">Create admin account</button>
</form>"#,
        flash(None, error),
        html_escape(username)
    );
    Html(mycelium_ui::shell::auth_shell("Setup", body))
}

/// Post-setup success: the recovery key is shown exactly once, in-page
/// (never in a URL — same rule as admin user creation).
pub fn setup_created(username: &str, recovery_key: &str) -> Html<String> {
    let body = format!(
        r#"<h1>Setup complete</h1>
<div class="flash ok">Admin account <b>{}</b> created. Recovery key (shown ONCE — store it now; it cannot be retrieved later):</div>
<pre>{}</pre>
<p><a href="/login">Go to login</a></p>"#,
        html_escape(username),
        html_escape(recovery_key)
    );
    Html(mycelium_ui::shell::auth_shell("Setup complete", body))
}

/// Browse: the user's private bundle listing (mockup 01) — the page
/// title row with the New-concept action and the type-chip filter,
/// then the concept table (title link, type chip, broken-links flag,
/// updated timestamp) or the empty state. `entries` is the FULL
/// listing: the chip row derives from the types present in it, and
/// `type_filter` (validated by the handler against those types)
/// narrows the table rows; `broken` carries the paths whose concept
/// links point outside the bundle. Cell and action strings are
/// server-composed trusted markup — user text (titles, paths, types)
/// is `html_escape`d here before composition, then interpolated raw
/// by the components (the shell's page-body contract).
pub fn home_page(
    user: &SessionUser,
    csrf: &str,
    entries: &[mycelium_store::ConceptEntry],
    broken: &std::collections::HashSet<String>,
    type_filter: Option<&str>,
    ok: Option<&str>,
    err: Option<&str>,
) -> Html<String> {
    // Type-chip set: one chip per type present in the bundle (sorted
    // for deterministic output) plus the "All" chip. The active chip:
    // the filter when it names a present type, otherwise "All" (a
    // filter value no concept carries falls back to the unfiltered
    // list — Review Focus 2 — so "All" is the honest active state).
    let mut types: Vec<&str> = entries
        .iter()
        .map(|e| e.concept_type.as_str())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    types.sort_unstable();
    // The active chip also carries `aria-current="true"` — filter
    // semantics, distinct from the tab bar/nav items' navigation
    // semantics (`aria-current="page"`). Inactive chips get neither.
    let filter_chip = |label: &str, href: &str, active: bool| {
        format!(
            r#"<a class="chip{}" href="{}"{}>{}</a>"#,
            if active { " chip--accent" } else { "" },
            href,
            if active {
                r#" aria-current="true""#
            } else {
                ""
            },
            html_escape(label)
        )
    };
    let mut chips = filter_chip("All", "/", type_filter.is_none());
    for t in types {
        chips.push_str(&filter_chip(
            t,
            &format!("/?type={}", urlencoding_encode(t)),
            type_filter == Some(t),
        ));
    }
    // Actions: the New-concept primary button (an anchor — works
    // without script), the filter chips, and mockup 01's small search
    // field — a plain GET form aimed at /search (Enter submits; no
    // submit button, no script), one row.
    let actions = format!(
        r#"<a class="{}" href="/concept?new=1">New concept</a>{chips}<form class="search-mini" method="get" action="/search"><input name="q" placeholder="Search…" aria-label="Search"></form>"#,
        mycelium_ui::button_class("primary")
    );
    // Table rows: the filter narrows the listing; each row's cells are
    // composed from escaped fragments (the link label) and trusted
    // component output (the type chip, the constant flag span).
    let rows: Vec<Vec<TrustedHtml>> = entries
        .iter()
        .filter(|e| type_filter.is_none_or(|t| e.concept_type == t))
        .map(|e| {
            let title_link = format!(
                r#"<a href="/concept?path={}">{}</a>"#,
                urlencoding_encode(&e.path),
                html_escape(&e.title)
            );
            let type_chip =
                mycelium_ui::render::render(mycelium_ui::chip(&e.concept_type, "neutral"));
            let flag = if broken.contains(&e.path) {
                r#"<span class="flag flag--warning">broken links</span>"#.to_string()
            } else {
                String::new()
            };
            vec![
                TrustedHtml::from_markup(title_link),
                TrustedHtml::from_markup(type_chip),
                TrustedHtml::from_markup(flag),
                TrustedHtml::from_markup(html_escape(&e.updated_at)),
            ]
        })
        .collect();
    let content = if rows.is_empty() {
        mycelium_ui::render::render(mycelium_ui::empty_state(
            "No concepts yet",
            "Create your first concept to begin.",
        ))
    } else {
        mycelium_ui::render::render(mycelium_ui::data_table(
            &["Title", "Type", "Broken links", "Updated"],
            &rows,
        ))
    };
    let body = format!(
        "{}{}{}",
        mycelium_ui::render::render(mycelium_ui::page_header(
            "Browse",
            TrustedHtml::from_markup(actions),
        )),
        flash(ok, err),
        content
    );
    layout("Browse", Some(user), csrf, "/", body)
}

/// How many characters of each text the restore-confirm page
/// previews (both sides stay bounded; the full text is one click
/// away in the snapshot diff and the live editor).
const RESTORE_PREVIEW_CHARS: usize = 480;

/// Summary-cell cap in characters (a long operation summary truncates
/// with an ellipsis, cut on a character boundary).
const SUMMARY_CHARS: usize = 120;

/// The concept editor page's revisions extras (Task 4): the optional
/// revisions surface (what the block renders) and the "Set status:
/// active" offer (a live concept whose status is paused).
pub struct ConceptExtras {
    /// The revisions surface — present only when the concept is
    /// revision-tracked (its chain is non-empty); `Some` renders the
    /// block. Handler-built (the handler gathers, the page composes).
    pub revisions: Option<RevisionsSurface>,
    /// The live concept is retired/superseded AND editable — render
    /// the activate form.
    pub activation: bool,
}

/// The revisions surface for one concept page: the chain plus which
/// mode the block renders (plain list, the restore-confirm
/// interstitial, or the diff view).
pub struct RevisionsSurface {
    /// The concept's canonical path (the compare form and links are
    /// page-composed from it).
    pub path: String,
    /// The scope short param for links/forms ("user" | "skills" |
    /// "library").
    pub scope_param: &'static str,
    /// The scope may restore (user own / skills admin) — restore
    /// links and the confirm interstitial exist only when true; the
    /// library is always false.
    pub restore_ready: bool,
    /// The concept is deleted (its page is the history, the editor is
    /// off; when the scope may also restore, restoring "re-creates"
    /// the path — a non-restoring dead surface never implies that).
    pub dead: bool,
    /// The chain, newest first (RevisionStore::list's order).
    pub entries: Vec<mycelium_store::RevisionEntry>,
    pub mode: RevisionsMode,
}

/// What the revisions block renders.
pub enum RevisionsMode {
    /// The newest-first rows + compare form (the editor above it
    /// stays).
    List,
    /// The restore-confirm interstitial (replaces the editor region).
    Confirm(ConfirmData),
    /// The unified diff view (replaces the editor region; the list
    /// stays below).
    Diff(DiffData),
}

/// The restore-confirm interstitial's data: what is restored and what
/// it replaces.
pub struct ConfirmData {
    pub n: u64,
    /// The row's operation (the "recorded" line).
    pub op: String,
    pub summary: String,
    /// The snapshot being restored (raw markdown — the page escapes).
    pub snapshot_text: String,
    /// The live concept's markdown when the concept exists (None on a
    /// dead path — nothing to replace, restore re-creates).
    pub current_text: Option<String>,
}

/// The diff view's data: the line list is PRE-ESCAPE data — the page
/// wraps it escaped in `<pre>`.
pub struct DiffData {
    /// The rendered from-side label ("current" or "revision #N").
    pub from_label: String,
    pub to_label: String,
    /// Unified diff lines (the `---`/`+++` headers, `@@` hunks, and
    /// context/+/- body lines).
    pub lines: Vec<String>,
    /// True when the diff passed the page's line cap.
    pub truncated: bool,
}

/// A short relative date for revision rows: "just now", then the
/// seconds/minutes/hours/days bands, then an absolute `YYYY-MM-DD`
/// past 30 days. An unparsable timestamp renders itself (a clock
/// quirk degrades to raw text, never a panic).
fn relative_date(rfc3339: &str) -> String {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(rfc3339) else {
        return rfc3339.to_string();
    };
    let age = chrono::Utc::now().signed_duration_since(parsed);
    let secs = age.num_seconds().max(0);
    if secs < 5 {
        return "just now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    if secs < 3600 {
        return format!("{}m ago", secs / 60);
    }
    if secs < 86400 {
        return format!("{}h ago", secs / 3600);
    }
    if secs < 30 * 86400 {
        return format!("{}d ago", secs / 86400);
    }
    parsed.format("%Y-%m-%d").to_string()
}

/// Char-boundary-safe summary cell: cap, ellipsis, then escape.
fn truncate_summary(summary: &str) -> String {
    if summary.chars().count() <= SUMMARY_CHARS {
        return html_escape(summary);
    }
    let cut: String = summary.chars().take(SUMMARY_CHARS).collect();
    format!("{}\u{2026}", html_escape(&cut))
}

/// Preview slice for the confirm page (character-boundary-safe).
fn preview_slice(text: &str) -> String {
    if text.chars().count() <= RESTORE_PREVIEW_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(RESTORE_PREVIEW_CHARS).collect();
    format!("{cut}\n\u{2026}")
}

/// The operation chip's severity: create accents, delete is danger,
/// everything else neutral.
fn revision_op_class(op: &str) -> &'static str {
    match op {
        "create" => "accent",
        "delete" => "danger",
        _ => "neutral",
    }
}

/// The concept editor/viewer (Task 2 rebuild — the one page fn the
/// three legacy editors collapsed into): breadcrumb, page header with
/// Save + Delete (the keys page's confirm-dialog pattern), the
/// frontmatter warning banner, and the two-pane body — the markdown
/// source textarea beside the static, server-rendered preview.
///
/// Escaping: `markdown` round-trips through [`textarea_escape`] into
/// the textarea; `preview_html` is a TRUSTED-markup slot — it is
/// built by [`build_preview_html`] from html-escaped fragments only
/// (escape-then-compose, the spec's body-interpolation contract), so
/// it is interpolated raw here. All other user text (path, scope, the
/// banner message) is escaped at composition.
///
/// `editable=false` renders the read-only viewer (library always;
/// global skills for non-admins). An empty `path` renders the
/// new-concept form (path input + template, no delete — nothing is
/// saved yet). `scope` keeps the write aimed at the right store (the
/// hidden form field, and the delete form + dialog carry it too) and
/// shows a muted scope note in the header actions.
///
/// `extras` (Task 4) carries the revisions surface and the activate
/// offer: `Some(surface)` adds the Revisions card (list mode) or
/// REPLACES the editor region with the confirm/diff interstitial; a
/// dead concept's page is the history alone (the editor is off — its
/// restoration path is the revisions block). The "Set status: active"
/// button renders in the header actions while `activation` is set —
/// offer and editor actions live only on modes that show the editor.
#[allow(clippy::too_many_arguments)]
pub fn concept_editor(
    user: &SessionUser,
    csrf: &str,
    path: &str,
    markdown: &str,
    preview_html: String,
    frontmatter_error: Option<&str>,
    scope: Option<&str>,
    editable: bool,
    extras: Option<&ConceptExtras>,
) -> Html<String> {
    let is_new = path.is_empty();
    let title: &str = if is_new { "New concept" } else { path };
    // The revisions surface: which mode renders — and whether the
    // editor grid is the front region at all (confirm/diff replace it;
    // dead paths hide it, their page is the history).
    let surface = extras.and_then(|ex| ex.revisions.as_ref());
    let interstitial = matches!(
        surface.map(|s| &s.mode),
        Some(RevisionsMode::Confirm(_) | RevisionsMode::Diff(_))
    );
    let dead = surface.is_some_and(|s| s.dead);
    let show_grid = !interstitial && !dead;
    // Breadcrumb: Browse → this concept (a plain label, no href).
    let breadcrumb = mycelium_ui::render::render(mycelium_ui::breadcrumb(&[
        ("Browse", Some("/")),
        (title, None),
    ]));
    // Header actions: a muted scope note, then Save (+ Delete when a
    // concept is saved). The Delete trigger sits inside its own
    // per-item form (hidden csrf + path + scope): without script,
    // clicking it POSTs /concept/delete directly; with confirm.js, the
    // click is intercepted and the shared dialog opens instead.
    let scope_note = match scope {
        Some("library") => r#"<span class="muted">(library — read-only)</span>"#,
        Some("skills") => r#"<span class="muted">(global skills)</span>"#,
        _ => "",
    };
    let show_delete = editable && !is_new && !interstitial;
    // The activate offer ("Set status: active", lifecycle spec §4): a
    // form-driven POST, the same hidden-fields pattern as delete — no
    // JS. Its fields: csrf + path (+ the scope when the page carries
    // one), so the handler aims the write exactly as concept_delete.
    let activate_form = if extras.is_some_and(|ex| ex.activation)
        && editable
        && !interstitial
        && !is_new
    {
        let mut fields = format!(
            r#"<input type="hidden" name="csrf_token" value="{}"><input type="hidden" name="path" value="{}">"#,
            html_escape(csrf),
            html_escape(path)
        );
        if let Some(s) = scope {
            fields.push_str(&format!(
                r#"<input type="hidden" name="scope" value="{}">"#,
                html_escape(s)
            ));
        }
        format!(
            r#"<form method="post" action="/concept/activate">{fields}<button type="submit" class="btn btn--secondary">Set status: active</button></form>"#
        )
    } else {
        String::new()
    };
    let (actions, dialog, confirm_script) = if show_delete {
        let mut delete_fields = format!(
            r#"<input type="hidden" name="csrf_token" value="{}"><input type="hidden" name="path" value="{}">"#,
            html_escape(csrf),
            html_escape(path)
        );
        if let Some(s) = scope {
            delete_fields.push_str(&format!(
                r#"<input type="hidden" name="scope" value="{}">"#,
                html_escape(s)
            ));
        }
        let delete_form = format!(
            r#"<form method="post" action="/concept/delete">{delete_fields}<button type="submit" class="btn btn--danger" data-confirm-dialog="delete">Delete</button></form>"#
        );
        // One shared dialog: the path (and scope) are server-rendered
        // into its hidden fields — one concept per page — so confirm.js
        // needs no per-trigger copying. The hidden values render
        // through leptos attribute positions (escaped there).
        let mut dialog_fields = vec![
            ("path".to_string(), path.to_string()),
            ("csrf_token".to_string(), csrf.to_string()),
        ];
        if let Some(s) = scope {
            dialog_fields.push(("scope".to_string(), s.to_string()));
        }
        let dialog = mycelium_ui::render::render(mycelium_ui::confirm_dialog(
            "delete",
            "Delete this concept?",
            "This cannot be undone.",
            "Delete",
            "danger",
            "/concept/delete",
            &dialog_fields,
        ));
        (
            format!(
                r#"{scope_note}<button type="submit" class="btn btn--primary" form="concept-editor-form">Save</button>{activate_form}{delete_form}"#
            ),
            dialog,
            true,
        )
    } else if editable && !interstitial {
        // New concept: Save only — nothing is saved to delete yet.
        (
            format!(
                r#"{scope_note}<button type="submit" class="btn btn--primary" form="concept-editor-form">Save</button>{activate_form}"#
            ),
            String::new(),
            false,
        )
    } else {
        (format!("{scope_note}{activate_form}"), String::new(), false)
    };
    let header = mycelium_ui::render::render(mycelium_ui::page_header(
        title,
        TrustedHtml::from_markup(actions),
    ));
    let warning = frontmatter_error
        .map(|m| mycelium_ui::render::render(mycelium_ui::banner("warning", m)))
        .unwrap_or_default();
    // The source pane: the save form posts /concept with the path,
    // scope, csrf, and markdown; the hidden csrf_token keeps the
    // native form path working without app.js. The Save button lives
    // in the header and submits this form via the HTML5 `form=`
    // attribute (valid without script).
    let scope_input = scope
        .map(|s| {
            format!(
                r#"<input type="hidden" name="scope" value="{}">"#,
                html_escape(s)
            )
        })
        .unwrap_or_default();
    let path_input = if is_new {
        r#"<label>Path (e.g. /notes/my-note.md)</label>
  <input name="path" required pattern="/.*\.md" placeholder="/notes/my-note.md">"#
            .to_string()
    } else {
        format!(
            r#"<input type="hidden" name="path" value="{}">"#,
            html_escape(path)
        )
    };
    let pane = if show_grid && editable {
        format!(
            r#"<form class="editor-pane" id="concept-editor-form" method="post" action="/concept">
  {path_input}
  {scope_input}
  <input type="hidden" name="csrf_token" value="{}">
  <label>Markdown (frontmatter + body)</label>
  <textarea name="markdown" class="editor-source" spellcheck="false">{}</textarea>
</form>"#,
            html_escape(csrf),
            textarea_escape(markdown)
        )
    } else {
        format!(
            r#"<div class="editor-pane">
  <label>Markdown (read-only)</label>
  <textarea class="editor-source" readonly spellcheck="false">{}</textarea>
</div>"#,
            textarea_escape(markdown)
        )
    };
    let grid = format!(
        r#"<div class="editor-grid">{pane}<div class="editor-preview">{preview_html}</div></div>"#
    );
    // The front region: the restore-confirm/diff interstitials replace
    // the editor grid; a dead concept shows the history alone — its
    // read-only grid is suppressed for ANY mode it would have rendered
    // (the editor is off; its restoration path is the revisions block
    // below). Below it, the Revisions card rides list/diff modes
    // (never confirm — the interstitial's Back link covers navigation
    // there).
    let center: String = match surface {
        Some(s) => match &s.mode {
            RevisionsMode::Confirm(data) => mycelium_ui::render::render(mycelium_ui::card(
                &format!("Restore revision #{}", data.n),
                revisions_confirm_body(data, s, csrf),
            )),
            RevisionsMode::Diff(data) => mycelium_ui::render::render(mycelium_ui::card(
                &format!("Diff: from {} to {}", data.from_label, data.to_label),
                revisions_diff_body(data, s),
            )),
            RevisionsMode::List if s.dead => String::new(),
            RevisionsMode::List => grid.clone(),
        },
        None => grid,
    };
    let below: String = match surface {
        Some(s) => match &s.mode {
            RevisionsMode::Confirm(_) => String::new(),
            _ => {
                mycelium_ui::render::render(mycelium_ui::card("Revisions", revisions_list_body(s)))
            }
        },
        None => String::new(),
    };
    let body = format!("{breadcrumb}{header}{warning}{center}{below}{dialog}");
    let page_title = if is_new { "New concept" } else { "Concept" };
    let scripts: &[&str] = if confirm_script {
        &["/assets/confirm.js"]
    } else {
        &[]
    };
    layout_with_scripts(page_title, Some(user), csrf, "/concept", body, scripts)
}

/// The revisions card's body (list mode, below the editor): the
/// compare form, then the newest-first table — op chip, source,
/// relative date, bounded summary, and per-row restore links when the
/// scope may restore.
fn revisions_list_body(surface: &RevisionsSurface) -> TrustedHtml {
    let mut h = String::from(r#"<p class="muted">Revision history (newest first).</p>"#);
    if surface.dead {
        // The restore claim rides the affordance: only a viewer whose
        // scope may actually restore (restore_ready) is told that
        // restoring re-creates the path. The library's dead pages are a
        // history read, full stop — restore hard-false, undelete
        // impossible — and a non-admin viewer of a skills chain cannot
        // restore either, so neither surface may imply it.
        if surface.restore_ready {
            h.push_str(&format!(
                r#"<p class="muted">This concept is deleted — restoring a revision re-creates it at {}.</p>"#,
                html_escape(&surface.path)
            ));
        } else {
            h.push_str(
                r#"<p class="muted">This concept is deleted — its revision history is preserved below.</p>"#,
            );
        }
    }
    h.push_str(&revisions_compare_form(surface));
    let headers = ["Revision", "Operation", "Source", "Date", "Summary", ""];
    let rows: Vec<Vec<TrustedHtml>> = surface
        .entries
        .iter()
        .map(|e| revision_row_cells(e, surface))
        .collect();
    h.push_str(&mycelium_ui::render::render(mycelium_ui::data_table(
        &headers, &rows,
    )));
    TrustedHtml::from_markup(h)
}

/// One table row's inner cells (the data_table primitive renders the
/// `<td data-label>` wrappers): #no, the op chip, the provenance (or a
/// muted "unlabeled"), the relative date, the bounded summary, and the
/// restore anchor when the scope may restore AND the payload is
/// non-empty — the empty-payload rows (spec §3.2a-3's undelete-restore
/// convention) have nothing to restore; their POST is the snapshot-
/// parse 400 the page must not invite.
fn revision_row_cells(
    entry: &mycelium_store::RevisionEntry,
    surface: &RevisionsSurface,
) -> Vec<TrustedHtml> {
    let action_cell = if surface.restore_ready && !entry.payload_empty {
        format!(
            r#"<a href="/concept?path={path}&scope={sp}&revs=1&restore_confirm={no}">restore</a>"#,
            path = urlencoding_encode(&surface.path),
            sp = surface.scope_param,
            no = entry.revision_no
        )
    } else {
        String::new()
    };
    [
        format!("#{}", entry.revision_no),
        mycelium_ui::render::render(mycelium_ui::chip(&entry.op, revision_op_class(&entry.op))),
        if entry.source.is_empty() {
            r#"<span class="muted">unlabeled</span>"#.to_string()
        } else {
            html_escape(&entry.source)
        },
        html_escape(&relative_date(&entry.created_at)),
        truncate_summary(&entry.summary),
        action_cell,
    ]
    .into_iter()
    .map(TrustedHtml::from_markup)
    .collect()
}

/// The compare form: a from- and a to-select (the `""` HEAD sentinel
/// option named "current"), the diff button, and the hidden path/scope/
/// revs fields that aim the GET back at this page's revisions view.
/// Options run ascending; the to-select defaults to the chain's head
/// (HEAD vs newest is the natural first compare). All four params are
/// Option<String> on the query — the sentinels are blanks, which a
/// typed extractor would 400-reject (see page_revision_param).
fn revisions_compare_form(surface: &RevisionsSurface) -> String {
    let mut hidden = format!(
        r#"<input type="hidden" name="path" value="{}">"#,
        html_escape(&surface.path)
    );
    // The user shelf compares via the absent scope; the service scopes
    // aim the GET at their branch.
    if surface.scope_param != "user" {
        hidden.push_str(&format!(
            r#"<input type="hidden" name="scope" value="{}">"#,
            html_escape(surface.scope_param)
        ));
    }
    hidden.push_str(r#"<input type="hidden" name="revs" value="1">"#);
    let newest = surface.entries.first().map(|e| e.revision_no);
    // Options ascending; `selected` marks the prechosen option.
    let mut nos: Vec<u64> = surface.entries.iter().map(|e| e.revision_no).collect();
    nos.sort_unstable();
    let select = |field: &str, label: &str, selected: Option<u64>| {
        let mut options = String::from(r#"<option value="">current</option>"#);
        for n in nos.iter().copied() {
            let sel = if Some(n) == selected { " selected" } else { "" };
            options.push_str(&format!(r#"<option value="{n}"{sel}>rev {n}</option>"#));
        }
        format!(r#"<label>{label} <select name="{field}">{options}</select></label>"#)
    };
    format!(
        r#"<form method="get" action="/concept" class="concept-revisions-diff">{hidden}{}<button type="submit" class="btn btn--secondary">diff</button></form>"#,
        select("diff_from", "from", None) + &select("diff_to", "to", newest)
    )
}

/// The restore-confirm interstitial's body: what is restored (and
/// what it replaces — the current concept, or its deleted position), a
/// bounded preview of both sides, the actual restore POST to Task 3's
/// route, and the back link. The snapshot text rides the page as
/// DATA: both previews are escaped at composition.
fn revisions_confirm_body(
    data: &ConfirmData,
    surface: &RevisionsSurface,
    csrf: &str,
) -> TrustedHtml {
    let mut h = String::new();
    if surface.dead {
        h.push_str(&format!(
            r#"<p class="muted">The concept is deleted — this restore re-creates it at {}.</p>"#,
            html_escape(&surface.path)
        ));
    } else {
        h.push_str(&format!(
            r#"<p>Restoring replaces the current concept with revision #{}'s snapshot.</p>"#,
            data.n
        ));
    }
    h.push_str(&format!(
        r#"<p class="muted">recorded: {}.</p>"#,
        html_escape(&data.summary)
    ));
    if let Some(current) = &data.current_text {
        h.push_str(&format!(
            r#"<h3>Current (will be replaced)</h3><pre>{}</pre>"#,
            html_escape(&preview_slice(current))
        ));
    }
    h.push_str(&format!(
        r#"<h3>Snapshot (restored instead)</h3><pre>{}</pre>"#,
        html_escape(&preview_slice(&data.snapshot_text))
    ));
    h.push_str(&format!(
        r#"<form method="post" action="/api/v1/revisions/restore?scope={sp}&path={path}&n={n}"><input type="hidden" name="csrf_token" value="{csrf}"><button type="submit" class="btn btn--danger">Restore revision #{n}</button></form>
<p><a href="/concept?path={path}&scope={sp}&revs=1">Back to revisions</a></p>"#,
        sp = surface.scope_param,
        path = urlencoding_encode(&surface.path),
        n = data.n,
        csrf = html_escape(csrf)
    ));
    TrustedHtml::from_markup(h)
}

/// The diff view's body: the unified diff lines (handler-computed
/// `similar` output) wrapped ESCAPED in a `<pre>`, a truncation note
/// on capped diffs, and the back links — the plain editor and the
/// revisions list.
fn revisions_diff_body(data: &DiffData, surface: &RevisionsSurface) -> TrustedHtml {
    let mut h = String::new();
    if data.truncated {
        h.push_str(r#"<p class="muted">Diff truncated — long compares stop at the cap (both texts are visible on the list/editor pages).</p>"#);
    }
    h.push_str(&format!(
        r#"<pre>{}</pre>"#,
        data.lines
            .iter()
            .map(|l| html_escape(l))
            .collect::<Vec<_>>()
            .join("\n")
    ));
    h.push_str(&format!(
        r#"<p><a href="/concept?path={path}&scope={sp}&revs=1">Back to revisions</a> <a href="/concept?path={path}&scope={sp}">Editor</a></p>"#,
        sp = surface.scope_param,
        path = urlencoding_encode(&surface.path)
    ));
    TrustedHtml::from_markup(h)
}

/// One row of the scope-level revisions browser (Task 5) — gathered BY
/// the handler (which alone can decode payloads), composed BY the page:
/// the path, the registry state, the newest recorded payload's title,
/// and the chain's last op + time (`RevisionStore::list_paths`'s
/// finding, plus the decode).
pub struct RevisionPathRow {
    pub path: String,
    /// A `scope_files` row exists right now (the store fn's live flag).
    pub live: bool,
    /// The newest recorded snapshot's frontmatter title — `None` when
    /// no payload could supply one (the page renders a muted
    /// placeholder).
    pub title: Option<String>,
    pub last_op: String,
    pub last_op_at: String,
}

/// The scope-level revisions browser (Task 5, spec §3.2 "…a scope-level
/// Revisions browser page (owner for user scopes; admin for service
/// scopes) listing every path with revisions, live or deleted (latest
/// title + last op per path)"). Breadcrumb (Browse → Revisions), the
/// scope-switch chips (admins only — a non-admin can reach only the
/// user scope, so offering the others would be dead links), and the
/// data table: Path (the link into Task 4's per-path view + the
/// live/deleted badge), decoded latest title, last-op chip, last-op
/// date. No forms, no script — pure anchors for no-JS navigation;
/// `err` renders as the page's banner (a store fault answers in place,
/// the books page's posture).
pub fn revisions_page(
    user: &SessionUser,
    csrf: &str,
    scope_param: &str,
    rows: &[RevisionPathRow],
    err: Option<&str>,
) -> Html<String> {
    let breadcrumb = mycelium_ui::render::render(mycelium_ui::breadcrumb(&[
        ("Browse", Some("/")),
        ("Revisions", None),
    ]));
    // Scope chips, the home_page filter-chip convention (accent +
    // aria-current on the active one; hrefs stay percent-clean — the
    // three param spellings are fixed words).
    let actions = if user.role == Role::Admin {
        let chip_link = |param: &str, label: &str| {
            let active = param == scope_param;
            format!(
                r#"<a class="chip{}" href="/revisions?scope={param}"{}>{}</a>"#,
                if active { " chip--accent" } else { "" },
                if active {
                    r#" aria-current="true""#
                } else {
                    ""
                },
                html_escape(label)
            )
        };
        format!(
            "{}{}{}",
            chip_link("user", "Your bundle"),
            chip_link("skills", "Global skills"),
            chip_link("library", "Library"),
        )
    } else {
        String::new()
    };
    let content = if rows.is_empty() {
        mycelium_ui::render::render(mycelium_ui::empty_state(
            "No revisions yet",
            "Concepts appear here after they are created, edited, or deleted.",
        ))
    } else {
        let table_rows: Vec<Vec<TrustedHtml>> = rows
            .iter()
            .map(|r| revision_path_row_cells(r, scope_param))
            .collect();
        mycelium_ui::render::render(mycelium_ui::data_table(
            &["Path", "Title", "Last op", "Last op at"],
            &table_rows,
        ))
    };
    let body = format!(
        "{}{}{}{}",
        breadcrumb,
        mycelium_ui::render::render(mycelium_ui::page_header(
            "Revisions",
            TrustedHtml::from_markup(actions)
        )),
        flash(None, err),
        content
    );
    layout("Revisions", Some(user), csrf, "/revisions", body)
}

/// One browser row's four cells: the path anchor (into the per-path
/// page, always scope-tagged) with its live/deleted badge, the escaped
/// title (or the muted placeholder), the last-op chip (the per-concept
/// list's palette), and the last-op date. User-derived text (the path,
/// the title, the op) is `html_escape`d / rendered through the chip at
/// composition — the page is a trusted-markup slot over escaped
/// fragments only.
fn revision_path_row_cells(row: &RevisionPathRow, scope_param: &str) -> Vec<TrustedHtml> {
    let badge = if row.live {
        r#"<span class="chip chip--neutral">live</span>"#.to_string()
    } else {
        r#"<span class="chip chip--danger">deleted</span>"#.to_string()
    };
    let path_cell = format!(
        r#"<a href="/concept?path={path}&scope={sp}">{}</a> {badge}"#,
        html_escape(&row.path),
        path = urlencoding_encode(&row.path),
        sp = scope_param
    );
    vec![
        TrustedHtml::from_markup(path_cell),
        TrustedHtml::from_markup(match &row.title {
            Some(title) => html_escape(title),
            None => r#"<span class="muted">(no title)</span>"#.to_string(),
        }),
        TrustedHtml::from_markup(mycelium_ui::render::render(mycelium_ui::chip(
            &row.last_op,
            revision_op_class(&row.last_op),
        ))),
        TrustedHtml::from_markup(html_escape(&relative_date(&row.last_op_at))),
    ]
}
/// Build the editor's static preview pane from the concept's markdown.
/// Returns `(preview_html, frontmatter_error)`: the preview is composed
/// from html-escaped fragments ONLY (escape-then-compose — the spec's
/// body-interpolation contract), and `frontmatter_error` carries
/// [`Concept::parse`]'s own message — the exact parser the save path
/// and `ConceptStore::get` validate with, so the banner's wording is
/// always the error a save would reject with (error parity).
///
/// Renderer choice (per the brief): no markdown→HTML library and no
/// client script are in scope for this pane, so the body goes through
/// a deliberately minimal server-side conversion —
/// [`render_minimal_body`]: blank-line paragraphs plus `#`/`##`/`###`
/// headings, every fragment escaped. A parse error means there is no
/// frontmatter to feature, so the pane minimally renders the raw
/// content (frontmatter block stripped when a closing `---` line
/// exists) and the warning banner explains the rest.
pub fn build_preview_html(markdown: &str) -> (String, Option<String>) {
    match Concept::parse("/preview.md", markdown) {
        Ok(concept) => {
            let mut out = String::new();
            if let Some(title) = &concept.frontmatter.title
                && !title.is_empty()
            {
                out.push_str(&format!("<h2>{}</h2>", html_escape(title)));
            }
            out.push_str(&mycelium_ui::render::render(mycelium_ui::chip(
                &concept.frontmatter.concept_type,
                "neutral",
            )));
            if let Some(description) = &concept.frontmatter.description
                && !description.is_empty()
            {
                out.push_str(&format!("<p>{}</p>", html_escape(description)));
            }
            out.push_str(&render_minimal_body(&concept.body));
            (out, None)
        }
        Err(e) => (
            render_minimal_body(&strip_frontmatter_block(markdown)),
            Some(e.to_string()),
        ),
    }
}

/// Minimal markdown→HTML for the preview pane: the body splits on blank
/// lines into `<p>` paragraphs; a line starting `# `/`## `/`### `
/// renders as `<h2>`/`<h3>`/`<h4>` (prefix stripped). Every fragment is
/// html-escaped BEFORE composition — hostile markup can never become
/// executable HTML here.
fn render_minimal_body(body: &str) -> String {
    fn flush(out: &mut String, para: &mut Vec<&str>) {
        if para.is_empty() {
            return;
        }
        out.push_str(&format!("<p>{}</p>", html_escape(&para.join("\n"))));
        para.clear();
    }
    let mut out = String::new();
    let mut para: Vec<&str> = Vec::new();
    for line in body.split('\n') {
        if let Some((tag, rest)) = heading_of(line) {
            flush(&mut out, &mut para);
            out.push_str(&format!("<{tag}>{}</{tag}>", html_escape(rest)));
        } else if line.trim().is_empty() {
            flush(&mut out, &mut para);
        } else {
            para.push(line);
        }
    }
    flush(&mut out, &mut para);
    out
}

/// `# `/`## `/`### ` → (`h2`|`h3`|`h4`, rest-of-line); every other line
/// is paragraph text (longer heading runs like `#### ` stay prose).
fn heading_of(line: &str) -> Option<(&'static str, &str)> {
    if let Some(rest) = line.strip_prefix("### ") {
        Some(("h4", rest))
    } else if let Some(rest) = line.strip_prefix("## ") {
        Some(("h3", rest))
    } else if let Some(rest) = line.strip_prefix("# ") {
        Some(("h2", rest))
    } else {
        None
    }
}

/// Strip a leading `---\n…\n---\n` frontmatter block. Used only on the
/// parse-error path (where `Concept::parse` could not split it): a
/// naive scan mirroring the core splitter's rules — CRLF normalized to
/// LF and a leading BOM stripped first (the same normalization
/// concept.rs applies before splitting, so a CRLF/BOM document previews
/// like its LF twin), then the opening `---\n` + closing `---`-line
/// match; without a closing `---` line nothing is stripped — the whole
/// input previews as typed. Owned: the normalization may copy.
fn strip_frontmatter_block(markdown: &str) -> String {
    let normalized = if markdown.contains("\r\n") {
        markdown.replace("\r\n", "\n")
    } else {
        markdown.to_string()
    };
    let normalized = normalized.strip_prefix('\u{feff}').unwrap_or(&normalized);
    let Some(rest) = normalized.strip_prefix("---\n") else {
        return markdown.to_string();
    };
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return rest[offset + line.len()..].to_string();
        }
        offset += line.len();
    }
    markdown.to_string()
}

/// Search page (Task 3 rebuild — mockup 05): the query row, the
/// scope-chip filter, and the merged cross-scope result list.
///
/// `scope` is the ACTIVE chip — the handler has already validated it
/// (`user|skills|library`; an absent or unknown value arrives as `None`
/// and renders "All" active, Task 1's unknown-filter fallback) and has
/// already narrowed `results` to that scope.
///
/// Body order: page header, the query row (a plain GET form — a
/// `field`-shaped input carrying the query in its value attribute plus
/// the primary submit, no script), the scope chips (GET links
/// `?q=…&scope=…`, the active one accented), then the count line and
/// the result list — or the empty state when a query returned nothing.
/// With no query at all the page stops at the query row.
///
/// Escaping: the query is user input — `html_escape` wherever it
/// renders as text (the value attribute, the count line) and
/// `urlencoding_encode` inside the chip hrefs; result titles and
/// snippets are escaped at composition, concept paths are
/// percent-encoded into hrefs, and every chip label is a fixed literal
/// (the chip component escapes its text anyway). The lowercase row
/// labels ("your bundle", "global skills", "library") keep the
/// existing scope-label logic; the chip row carries the mockup's
/// capitalized labels ("Your bundle", "Skills", "Books") — the casing
/// split is deliberate so a scope-filtered page can assert on the
/// lowercase row labels without matching the chip row.
pub fn search_page(
    user: &SessionUser,
    csrf: &str,
    query: &str,
    scope: Option<&str>,
    results: &[crate::api::ScopedResult],
) -> Html<String> {
    let queried = !query.is_empty();
    // Query row: plain GET form. The input re-carries the query
    // (escaped into the value attribute) so a refinement keeps the
    // text; the field shape gives it the label-above-input styling and
    // the primary submit needs no script.
    let query_row = format!(
        r#"<form method="get" action="/search">
  <div class="field">
    <label for="search-q">Query</label>
    <input id="search-q" type="text" name="q" value="{}" placeholder="Search across bundle, skills, and books" autofocus>
  </div>
  <button type="submit" class="btn btn--primary">Search</button>
</form>"#,
        html_escape(query)
    );
    // Scope chips: GET links that re-run the query narrowed to one
    // scope — "All" drops the scope param. The active chip mirrors the
    // scope the handler kept (None → "All").
    let chips = if queried {
        let enc = urlencoding_encode(query);
        // Active chip: `chip--accent` plus `aria-current="true"` (filter
        // semantics, not the nav/tab `aria-current="page"`).
        let chip = |label: &str, href: String, active: bool| {
            format!(
                r#"<a class="chip{}" href="{}"{}>{}</a>"#,
                if active { " chip--accent" } else { "" },
                href,
                if active {
                    r#" aria-current="true""#
                } else {
                    ""
                },
                html_escape(label)
            )
        };
        let mut chips = vec![chip("All", format!("/search?q={enc}"), scope.is_none())];
        for (param, label) in [
            ("user", "Your bundle"),
            ("skills", "Skills"),
            ("library", "Books"),
        ] {
            chips.push(chip(
                label,
                format!("/search?q={enc}&scope={param}"),
                scope == Some(param),
            ));
        }
        format!(r#"<p class="scope-chips">{}</p>"#, chips.join(" "))
    } else {
        String::new()
    };
    // Count + results — only for a query that ran. Zero hits render
    // the empty state instead of an empty list.
    let content = if !queried {
        String::new()
    } else if results.is_empty() {
        mycelium_ui::render::render(mycelium_ui::empty_state(
            "No results",
            "Try a different search or scope.",
        ))
    } else {
        let rows = results
            .iter()
            .map(|r| {
                // Scope-aware links + labels (the existing routing
                // logic): user bundle, global skills, and library
                // concepts open in the right viewer.
                let (href, label) = match r.scope {
                    "library" => (
                        format!(
                            "/concept?path={}&scope=library",
                            urlencoding_encode(&r.concept_path)
                        ),
                        "library",
                    ),
                    "skills" => (
                        format!(
                            "/concept?path={}&scope=skills",
                            urlencoding_encode(&r.concept_path)
                        ),
                        "global skills",
                    ),
                    _ => (
                        format!("/concept?path={}", urlencoding_encode(&r.concept_path)),
                        "your bundle",
                    ),
                };
                format!(
                    r#"<li><a href="{href}">{}</a> {}<br><span class="muted">{}</span></li>"#,
                    html_escape(&r.title),
                    mycelium_ui::render::render(mycelium_ui::chip(label, "neutral")),
                    html_escape(&r.snippet)
                )
            })
            .collect::<String>();
        format!(
            r#"<p class="muted">{} results for "{}"</p><ul>{rows}</ul>"#,
            results.len(),
            html_escape(query)
        )
    };
    let body = format!(
        "{}{query_row}{chips}{content}",
        mycelium_ui::render::render(mycelium_ui::page_header(
            "Search",
            TrustedHtml::from_markup(""),
        ))
    );
    layout("Search", Some(user), csrf, "/search", body)
}

/// Graph page (loads graph.js for the force-directed visualization).
/// Mockup 06: PageHeader above the full-height panel; the type legend
/// (`#graph-legend`, bottom-left overlay — populated client-side from
/// the loaded data) and the node info card (`#graph-info`, top-right
/// overlay) anchor to the `position: relative` `#graph-wrap`; the
/// usage hint sits under the panel.
pub fn graph_page(user: &SessionUser, csrf: &str) -> Html<String> {
    let header = mycelium_ui::render::render(mycelium_ui::page_header(
        "Graph",
        TrustedHtml::from_markup(""),
    ));
    let body = format!(
        r#"{header}
<div id="graph-wrap">
  <div id="graph-info" hidden></div>
  <div id="graph"></div>
  <div id="graph-legend" class="graph-legend" hidden></div>
</div>
<div class="graph-hint">drag nodes to rearrange · scroll to zoom · drag background to pan · click a node to open</div>"#
    );
    layout_full(
        "Graph",
        Some(user),
        csrf,
        "/graph",
        body,
        &["/assets/graph.js"],
        &[],
        "graph-page",
    )
}

/// Skills page (Task 6 rebuild): the PageHeader with the
/// New-private-skill primary action, then two cards — "Your private
/// skills" and "Global skills". Nested skills (`/<slug>/skill.md`
/// hubs) render as one group per card: the hub row with companions
/// and manifest script labels indented beneath it (`.group-indent`);
/// legacy root-level skills stay flat rows. An empty section renders
/// the empty state inside its card; the admin-only "New global
/// skill" link stays below the global list (the card header's
/// action slot lands in a later PR — the card is title-only here).
///
/// Escaping: the card bodies are trusted-markup slots composed from
/// escaped fragments — entry titles `html_escape`d, hrefs
/// percent-encoded via [`urlencoding_encode`], script paths
/// `html_escape`d (the raw-slot contract: escape at composition).
#[allow(clippy::too_many_arguments)]
pub fn skills_page(
    user: &SessionUser,
    csrf: &str,
    private_groups: &[mycelium_store::SkillGroup],
    private_flat: &[mycelium_store::ConceptEntry],
    private_scripts: &[Vec<String>],
    global_groups: &[mycelium_store::SkillGroup],
    global_flat: &[mycelium_store::ConceptEntry],
    global_scripts: &[Vec<String>],
) -> Html<String> {
    let entry_link = |e: &mycelium_store::ConceptEntry, scope: &str| {
        format!(
            r#"<a href="/concept?path={}&scope={scope}">{}</a>"#,
            urlencoding_encode(&e.path),
            html_escape(&e.title)
        )
    };
    // One nested skill: hub row, then companions and manifest script
    // labels indented beneath it (`group-indent` on the nested list —
    // one class indents member rows and label rows alike). Scripts
    // are label-only rows — raw payload files, not concepts, so they
    // carry no link.
    let group_block = |g: &mycelium_store::SkillGroup, scripts: &[String], scope: &str| {
        let members = g
            .members
            .iter()
            .map(|m| format!("<li>{}</li>", entry_link(m, scope)))
            .collect::<String>();
        let labels = scripts
            .iter()
            .map(|p| format!(r#"<li class="muted">{}</li>"#, html_escape(p)))
            .collect::<String>();
        format!(
            r#"<li>{}<ul class="group-indent">{members}{labels}</ul></li>"#,
            entry_link(&g.hub, scope)
        )
    };
    // One section body: grouped blocks (slug order), then flat rows;
    // an empty section renders the empty state instead of a bare
    // list.
    let section_body = |groups: &[mycelium_store::SkillGroup],
                        scripts: &[Vec<String>],
                        flat: &[mycelium_store::ConceptEntry],
                        scope: &str| {
        if groups.is_empty() && flat.is_empty() {
            return mycelium_ui::render::render(mycelium_ui::empty_state(
                "No skills yet",
                "Create or install a skill to begin.",
            ));
        }
        let rows = groups
            .iter()
            .zip(scripts.iter())
            .map(|(g, s)| group_block(g, s, scope))
            .chain(
                flat.iter()
                    .map(|e| format!("<li>{}</li>", entry_link(e, scope))),
            )
            .collect::<String>();
        format!("<ul>{rows}</ul>")
    };
    let global_list = section_body(global_groups, global_scripts, global_flat, "skills");
    // The admin's create link stays below the global list (the
    // existing conditional — users see none of it).
    let global_body = if user.role == Role::Admin {
        format!(
            r#"{global_list}<p><a href="/concept?new=1&scope=skills">New global skill</a> (admin)</p>"#
        )
    } else {
        global_list
    };
    // Actions: the New-private-skill primary button (an anchor —
    // works without script).
    let actions = format!(
        r#"<a class="{}" href="/concept?new=1&scope=skills-private">New private skill</a>"#,
        mycelium_ui::button_class("primary")
    );
    let body = format!(
        "{}{}{}",
        mycelium_ui::render::render(mycelium_ui::page_header(
            "Skills",
            TrustedHtml::from_markup(actions)
        )),
        mycelium_ui::render::render(mycelium_ui::card(
            "Your private skills",
            TrustedHtml::from_markup(section_body(
                private_groups,
                private_scripts,
                private_flat,
                "user"
            )),
        )),
        mycelium_ui::render::render(mycelium_ui::card(
            "Global skills",
            TrustedHtml::from_markup(global_body),
        )),
    );
    layout("Skills", Some(user), csrf, "/skills", body)
}

/// Books page (Task 4 rebuild — mockup 07): the shelves table with the
/// books-count column, the selected shelf's chapter list, and the
/// passage reader pane — all server-rendered, no script.
///
/// Body order: breadcrumb (Books → shelf → book once a shelf is
/// selected — the concept editor's pattern), the page header, the
/// passage warning banner (RF 4 — a resolution failure renders
/// in-page as a banner, never a 500), the shelves `data_table` (Shelf,
/// Visibility chip, Books count, the row's Browse link), the selected
/// shelf's chapter list (`<ol class="chapter-list">` of numbered rows,
/// each linking `?shelf=…&passage=book://slug%23anchor`), then the
/// reader pane (class `reader-pane`, the passage's server-composed
/// HTML interpolated raw — trusted markup from escaped fragments).
///
/// Escaping: `data_table` cells and the reader pane are raw-markup
/// slots — shelf names, book titles, and chapter titles are
/// `html_escape`d at composition (the Task-1 contract), href values
/// percent-encoded via [`urlencoding_encode`]. The passage query
/// value keeps its `book://` scheme literal with only the slug and
/// anchor percent-encoded (`#` → `%23`, so the resource string stays
/// ONE query value); `chapters` arrives as plain data from the
/// handler (this PR's gather/compose split), carrying each row's
/// per-book chapter number and the catalog's REAL anchor.
#[allow(clippy::too_many_arguments)]
pub fn books_page(
    user: &SessionUser,
    csrf: &str,
    shelves: &[crate::api::ShelfBrowse],
    selected_shelf: Option<&str>,
    selected_book: Option<&str>,
    chapters: &[crate::api::ChapterRow],
    passage_html: Option<String>,
    passage_err: Option<&str>,
) -> Html<String> {
    // Breadcrumb: Books → shelf → book (the shelf crumb keeps the
    // selection; the book crumb is the leaf).
    let shelf_href = selected_shelf.map(|s| format!("/books?shelf={}", urlencoding_encode(s)));
    let mut crumbs: Vec<(&str, Option<&str>)> = vec![("Books", Some("/books"))];
    if let Some(shelf) = selected_shelf {
        crumbs.push((shelf, shelf_href.as_deref()));
    }
    if let Some(book) = selected_book {
        crumbs.push((book, None));
    }
    let breadcrumb = if crumbs.len() > 1 {
        mycelium_ui::render::render(mycelium_ui::breadcrumb(&crumbs))
    } else {
        String::new()
    };
    // Shelves table: the count column is the SQL book listing's length
    // (RF 3 — the count must match what the row's Browse link opens).
    let rows: Vec<Vec<TrustedHtml>> = shelves
        .iter()
        .map(|(name, is_global, books)| {
            let vis_chip = mycelium_ui::render::render(mycelium_ui::chip(
                if *is_global { "global-read" } else { "private" },
                if *is_global { "accent" } else { "neutral" },
            ));
            let browse = format!(
                r#"<a href="/books?shelf={}">Browse</a>"#,
                urlencoding_encode(name)
            );
            vec![
                TrustedHtml::from_markup(html_escape(name)),
                TrustedHtml::from_markup(vis_chip),
                TrustedHtml::from_markup(books.len().to_string()),
                TrustedHtml::from_markup(browse),
            ]
        })
        .collect();
    let table = mycelium_ui::render::render(mycelium_ui::data_table(
        &["Shelf", "Visibility", "Books", ""],
        &rows,
    ));
    // The selected shelf's chapters, one numbered row per chapter.
    // `value` keeps the row's number the chapter's REAL index, so a
    // narrowed (`?book=`) or multi-book list stays per-book correct.
    let chapters_section = selected_shelf
        .map(|shelf| {
            let shelf_enc = urlencoding_encode(shelf);
            let list = if chapters.is_empty() {
                mycelium_ui::render::render(mycelium_ui::empty_state(
                    "No chapters",
                    "This shelf holds no ingested books yet.",
                ))
            } else {
                let items = chapters
                    .iter()
                    .map(|ch| {
                        let href = format!(
                            "/books?shelf={shelf_enc}&passage=book://{}%23{}",
                            urlencoding_encode(&ch.book_slug),
                            urlencoding_encode(&ch.anchor)
                        );
                        format!(
                            r#"<li value="{}"><a href="{}">{}</a></li>"#,
                            ch.index,
                            href,
                            html_escape(&ch.title)
                        )
                    })
                    .collect::<String>();
                format!(r#"<ol class="chapter-list">{items}</ol>"#)
            };
            format!("<h2>Chapters</h2>{list}")
        })
        .unwrap_or_default();
    // The reader pane: trusted server-composed markup (escaped
    // fragments — build_passage_html), interpolated raw. A resolution
    // failure renders the warning banner instead (RF 4 — 200 + banner).
    let reader = passage_html
        .map(|html| format!(r#"<div class="reader-pane">{html}</div>"#))
        .unwrap_or_default();
    let warning = passage_err
        .map(|m| mycelium_ui::render::render(mycelium_ui::banner("warning", m)))
        .unwrap_or_default();
    let header = mycelium_ui::render::render(mycelium_ui::page_header(
        "Books",
        TrustedHtml::from_markup(""),
    ));
    let body = format!("{breadcrumb}{header}{warning}{table}{chapters_section}{reader}");
    layout("Books", Some(user), csrf, "/books", body)
}

/// Build the Books page's passage-reader HTML from an extracted
/// passage's markdown: the same deliberately minimal server-side
/// conversion as the editor preview — [`render_minimal_body`]'s
/// blank-line paragraphs + `#`/`##`/`###` headings, every fragment
/// html-escaped BEFORE composition (escape-then-compose, the spec's
/// body-interpolation contract — hostile book text can never become
/// executable markup). The reader pane interpolates the result raw:
/// trusted server-composed markup.
pub fn build_passage_html(passage_text: &str) -> String {
    render_minimal_body(passage_text)
}

/// The chat page's selected-session state (the transcript pane's input).
/// The handler builds it from `ChatStore::session_meta` +
/// `messages_after(0)`; `None` is the draft (`?s=new`, or no sessions
/// yet) — an empty log, no row created.
pub struct ChatActiveView {
    pub id: Uuid,
    pub title: String,
    pub messages: Vec<mycelium_store::chat::ChatMessageRow>,
    pub turn_status: Option<mycelium_store::chat::ChatTurnStatus>,
    pub turn_resumable: bool,
    pub max_seq: i64,
}

/// Chat page (two-pane, spec §3.6 — all SSR-first): the sessions
/// sidebar (newest-first links, the active one highlighted, a native
/// per-session delete form — works without JS, and with confirm.js the
/// shared delete dialog guards the destructive click) and the selected
/// session's transcript rendered in seq order with the SAME bubble
/// markup chat.js builds, so JS appends seamlessly. The composer is
/// the untouched chat_composer hydration island; its SSR markup IS
/// the form, and with the hydrate bundle the island owns the
/// submit/Enter/auto-grow wiring, handing sends to chat.js's
/// `window.myceliumChatSend`.
///
/// Row rendering mirrors the client: user rows escape-then-set (the
/// client's `textContent`), assistant rows go through
/// [`render_minimal_body`] (escape-then-compose — the same minimal
/// markdown `build_passage_html` uses, structurally what the client's
/// renderer produces for plain paragraphs), tool rows render as `·
/// name (detail)` notes (their persisted content is sealed JSON,
/// parsed then escaped field-by-field), error rows escape-then-set.
/// Status bubbles: a `running` turn renders the pending "thinking…"
/// bubble; an `interrupted` turn renders the restart note (resumability
/// wording per D6/D7); a `failed` turn's error rows already carry the
/// message — no separate note. The `data-*` attrs on #chat-log are
/// chat.js's session state (Task 7's contract). `active: None` (the
/// `?s=new` draft) renders an emptied log with `data-session="new"` and
/// an un-highlighted sidebar.
pub fn chat_page(
    user: &SessionUser,
    csrf: &str,
    sessions: &[mycelium_store::chat::ChatSessionSummary],
    active: Option<&ChatActiveView>,
) -> Html<String> {
    let csrf_esc = html_escape(csrf);
    // Sidebar: "+ New chat" then one row per session (the list arrives
    // newest-first from `list_sessions`). Each row is [trash icon]
    // [title link] — the delete trigger is a compact icon button
    // (aria-label carries the accessible name; the SVG is decorative)
    // riding the keys-page confirm-dialog pattern
    // (`data-confirm-dialog` + `data-key-id`): without JS it POSTs the
    // form directly (the per-session action URL in the path); with JS,
    // confirm.js's preventDefault opens the shared dialog, copying the
    // trigger's form action (the SAME per-session URL) onto the dialog
    // form before submitting. The pane carries the `chat-sessions`
    // collapse hook (chat.js flips `chat-sessions--collapsed` + persists
    // per browser); the toggle sits at the pane's top so it stays
    // reachable in both states.
    let mut sidebar = String::from(
        r#"<button type="button" id="chat-sidebar-toggle" class="chat-sidebar-toggle" aria-label="Toggle chat history sidebar" title="Toggle chat history"><svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><path d="M10 2 4 8l6 6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg></button><a class="chat-new" href="/chat?s=new">+ New chat</a>"#,
    );
    for session in sessions {
        let is_active = active.is_some_and(|a| a.id == session.id);
        sidebar.push_str(&format!(
            r#"<div class="chat-session-row{}"><a class="chat-session-link" href="/chat?s={}">{}</a><form class="chat-session-delete" method="post" action="/api/v1/chat/sessions/{}/delete"><input type="hidden" name="csrf_token" value="{csrf_esc}"><button type="submit" class="chat-delete-btn" aria-label="Delete chat" title="Delete chat" data-confirm-dialog="confirm-delete-chat" data-key-id="{}"><svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><path d="M2.5 4.5h11m-9 0v9a1 1 0 0 0 1 1h5a1 1 0 0 0 1-1v-9m-6.5 0v-1.5a1 1 0 0 1 1-1h3a1 1 0 0 1 1 1v1.5m-4 3v5m2.5-5v5" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg></button></form></div>"#,
            if is_active { " active" } else { "" },
            session.id,
            html_escape(&session.title),
            session.id,
            session.id
        ));
    }

    // Transcript: the rows in seq order, then the status bubble for the
    // turn's state (never a row — status lives in chat_turns).
    let mut log = String::new();
    let mut status_bubble = String::new();
    if let Some(view) = active {
        for row in &view.messages {
            match row.role {
                mycelium_store::chat::ChatRole::User => {
                    log.push_str(&format!(
                        r#"<div class="chat-msg user">{}</div>"#,
                        html_escape(&row.content)
                    ));
                }
                mycelium_store::chat::ChatRole::Assistant => {
                    log.push_str(&format!(
                        r#"<div class="chat-msg librarian">{}</div>"#,
                        render_minimal_body(&row.content)
                    ));
                }
                mycelium_store::chat::ChatRole::Tool => {
                    let (name, detail) = parse_tool_row(&row.content);
                    log.push_str(&format!(
                        r#"<div class="chat-msg tool-note">· {} ({})</div>"#,
                        html_escape(&name),
                        html_escape(&detail)
                    ));
                }
                mycelium_store::chat::ChatRole::Error => {
                    log.push_str(&format!(
                        r#"<div class="chat-msg error">{}</div>"#,
                        html_escape(&row.content)
                    ));
                }
            }
        }
        match view.turn_status {
            Some(mycelium_store::chat::ChatTurnStatus::Running) => {
                status_bubble =
                    r#"<div class="chat-msg librarian pending">The librarian is thinking…</div>"#
                        .to_string();
            }
            // Parked during an outage (spec §3.3): the resume worker
            // will retry — the note promises it, and the meta poll's
            // `turn_resumable: true` keeps the page live until then.
            // Same note class as the interrupted bubble (Task 6
            // ruling: no new CSS).
            Some(mycelium_store::chat::ChatTurnStatus::WaitingBackend) => {
                status_bubble = r#"<div class="chat-msg chat-status-note">Waiting for the librarian backend — it will resume automatically.</div>"#
                    .to_string();
            }
            Some(mycelium_store::chat::ChatTurnStatus::Interrupted) => {
                status_bubble = if view.turn_resumable {
                    r#"<div class="chat-msg chat-status-note">Interrupted by a server restart — it will resume automatically.</div>"#
                } else {
                    r#"<div class="chat-msg chat-status-note">Interrupted by a server restart.</div>"#
                }
                .to_string();
            }
            // Done/failed: the transcript already shows the reply or the
            // error row — no status bubble.
            _ => {}
        }
    }

    let (session_attr, max_seq, status_attr) = match active {
        Some(view) => (
            view.id.to_string(),
            view.max_seq.to_string(),
            view.turn_status
                .map(|s| s.as_str().to_string())
                .unwrap_or_else(|| "none".to_string()),
        ),
        // The draft: no session row exists (spec D2) — `new` + a zero
        // cursor + no turn.
        None => ("new".to_string(), "0".to_string(), "none".to_string()),
    };

    let composer = mycelium_ui::render::render(mycelium_ui::chat_composer(
        "Ask about your knowledge base, or say 'record that ...' (Enter to send, Shift+Enter for a new line)",
    ));
    // One delete dialog for the whole sidebar: the hidden csrf_token
    // rides server-rendered; the `id` hidden field starts empty and
    // confirm.js copies the clicked trigger's `data-key-id` into it,
    // and (chat-only) the trigger's own per-row form action onto the
    // dialog form — one dialog serves every row (the keys-page
    // pattern with the action copy chat's per-row URLs require).
    // Without script the dialog never opens and the trigger's native
    // form POSTs directly (the concept-delete precedent).
    let dialog = mycelium_ui::render::render(mycelium_ui::confirm_dialog(
        "confirm-delete-chat",
        "Delete this chat?",
        "This cannot be undone.",
        "Delete",
        "danger",
        "",
        &[
            ("id".to_string(), String::new()),
            ("csrf_token".to_string(), csrf.to_string()),
        ],
    ));
    let body = format!(
        r#"<div class="chat-shell chat-two-pane">
  <nav class="chat-sessions" aria-label="Chat sessions">{sidebar}</nav>
  <section class="chat-main">
    <div id="chat-log" class="chat-log" aria-live="polite" data-session="{session_attr}" data-max-seq="{max_seq}" data-turn-status="{status_attr}" data-csrf="{csrf_esc}">{log}{status_bubble}</div>
    {composer}
  </section>
</div>{dialog}"#
    );
    layout_full(
        "Librarian",
        Some(user),
        csrf,
        "/chat",
        body,
        &[
            "/assets/chat.js",
            "/assets/confirm.js",
            "/assets/islands.js",
        ],
        &["/assets/mycelium_ui.js"],
        "chat-page",
    )
}

/// Parse a persisted tool row's sealed JSON payload back to its
/// `{"name","detail"}` parts (the SSR transcript's tool-note bubbles —
/// chat_api's stream parser owns the same shape). A row that fails to
/// parse degrades to the raw content as the detail, mirroring the
/// stream side.
fn parse_tool_row(content: &str) -> (String, String) {
    match serde_json::from_str::<serde_json::Value>(content) {
        Ok(v) => (
            v.get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string(),
            v.get("detail")
                .and_then(|d| d.as_str())
                .unwrap_or_default()
                .to_string(),
        ),
        Err(_) => (String::new(), content.to_string()),
    }
}

/// API keys page (Task 8 rebuild — mockup 09): the page header's actions
/// slot carries the mint form (label input + primary Mint key button,
/// with its own server-rendered hidden CSRF token — the row-form
/// pattern); a successful mint renders the shown-once banner card (the
/// key in `<code id="minted-key">` beside the copy-button island, plus
/// the explanatory copy — the value stays selectable text); the keys
/// `data_table` lists label, status chip (active = accent, revoked =
/// neutral), created, last used, and the row action.
///
/// Revoked rows render dimmed instead of live: `data_table` owns the
/// `<tr>`, so a revoked row's composed CELL markup carries the
/// `row--revoked` class (every cell's content wraps in it, dimming the
/// whole visible row), and the action cell shows plain "Revoked" text —
/// no live button (revocation is idempotent; re-revoking buys nothing).
/// Active rows keep PR-1's exact revoke pattern: a per-row form (hidden
/// csrf + id) whose plain submit doubles as the confirm-dialog trigger
/// (`data-confirm-dialog` + `data-key-id`) — without JS it POSTs
/// /keys/revoke directly; with JS, confirm.js's preventDefault opens
/// the one shared dialog and copies the trigger's `data-key-id` into
/// the form's hidden id input.
///
/// Escaping: `data_table` cells and `card` bodies are raw-markup slots
/// — key labels are `html_escape`d at composition, the hidden CSRF
/// tokens are `html_escape`d, ids/dates are primitives (the token is
/// server-generated `myc2-<hex>` — no escaping positions inside it).
pub fn keys_page(
    user: &SessionUser,
    csrf: &str,
    keys: &[mycelium_auth::api_key::ApiKeyRecord],
    minted: Option<&str>,
) -> Html<String> {
    let csrf_esc = html_escape(csrf);
    let rows: Vec<Vec<TrustedHtml>> = keys
        .iter()
        .map(|k| {
            let revoked = k.revoked_at.is_some();
            // A revoked row's cells each wrap their content in the
            // dimming class — the only channel data_table gives a row
            // to carry state (cells are raw; the `<tr>` is not).
            let wrap = |cell: String| {
                if revoked {
                    format!(r#"<span class="row--revoked">{cell}</span>"#)
                } else {
                    cell
                }
            };
            let status_chip = mycelium_ui::render::render(mycelium_ui::chip(
                if revoked { "revoked" } else { "active" },
                if revoked { "neutral" } else { "accent" },
            ));
            // The trigger is a plain submit inside its own row form:
            // without JS, clicking it POSTs /keys/revoke directly (the
            // server-rendered csrf_token is the security gate); with
            // JS, confirm.js's preventDefault opens the dialog
            // instead. A revoked row gets no trigger at all — dimmed
            // "Revoked" text in its place.
            let action = if revoked {
                r#"<span class="row--revoked">Revoked</span>"#.to_string()
            } else {
                format!(
                    r#"<form method="post" action="/keys/revoke"><input type="hidden" name="csrf_token" value="{}"><input type="hidden" name="id" value="{}"><button type="submit" class="btn btn--danger" data-confirm-dialog="revoke" data-key-id="{}">Revoke</button></form>"#,
                    csrf_esc, k.id, k.id
                )
            };
            vec![
                TrustedHtml::from_markup(wrap(html_escape(&k.label))),
                TrustedHtml::from_markup(wrap(status_chip)),
                TrustedHtml::from_markup(wrap(k.created_at.format("%Y-%m-%d").to_string())),
                TrustedHtml::from_markup(wrap(
                    k.last_used_at
                        .map(|d| d.format("%Y-%m-%d").to_string())
                        .unwrap_or_default(),
                )),
                TrustedHtml::from_markup(action),
            ]
        })
        .collect();
    let table = mycelium_ui::render::render(mycelium_ui::data_table(
        &["Label", "Status", "Created", "Last used", ""],
        &rows,
    ));
    // One revoke dialog per page: confirm.js opens it from any row's
    // trigger, copying that row's data-key-id into the form's hidden
    // id input. The form's own action + hidden fields (including the
    // CSRF token) carry the confirm POST — no inline scripts.
    let dialog = mycelium_ui::render::render(mycelium_ui::confirm_dialog(
        "revoke",
        "Revoke this key?",
        "Revoking a key takes effect immediately — any tool using it stops working. This cannot be undone.",
        "Revoke",
        "danger",
        "/keys/revoke",
        &[
            ("id".to_string(), String::new()),
            ("csrf_token".to_string(), csrf.to_string()),
        ],
    ));
    // The shown-once banner (Task 7's copy island rides here): the key
    // value stays selectable text (its id anchors the island's copy
    // target); without the hydrate bundle the button is inert. The
    // `shown-once` class marks the value row (the e2e pin).
    let minted_html = minted
        .map(|t| {
            let copy = mycelium_ui::render::render(mycelium_ui::copy_button("minted-key"));
            let body = format!(
                r#"<p>Copy this key now — it is shown only once and cannot be retrieved later.</p><p class="shown-once"><code id="minted-key">{t}</code>{copy}</p>"#
            );
            mycelium_ui::render::render(mycelium_ui::card(
                "New key — shown once",
                TrustedHtml::from_markup(body),
            ))
        })
        .unwrap_or_default();
    // Actions slot: the mint form, compact in the title row (mockup 09
    // — mint/create primary right). Its hidden csrf_token follows the
    // row-form pattern (app.js skips injection when the field exists).
    let actions = format!(
        r#"<form method="post" action="/keys" class="mint-form"><input type="hidden" name="csrf_token" value="{csrf_esc}"><label for="mint-label">Label</label><input id="mint-label" name="label" required><button type="submit" class="btn btn--primary">Mint key</button></form>"#
    );
    let header = mycelium_ui::render::render(mycelium_ui::page_header(
        "API keys",
        TrustedHtml::from_markup(actions),
    ));
    let body = format!("{header}{minted_html}{table}{dialog}");
    layout_full(
        "API keys",
        Some(user),
        csrf,
        "/keys",
        body,
        &["/assets/confirm.js", "/assets/islands.js"],
        &["/assets/mycelium_ui.js"],
        "",
    )
}

// ---------- Account security (PR 4, Task 3) ----------

/// The TOTP half of the Two-factor card — one variant per /account
/// handler response. The resting state is the user's stored secret
/// (`UserRecord.totp_secret` — `get_by_id` already carries the column,
/// so no new query shape exists); the enrollment states render the
/// enroll/verify flow (see `account_totp_enroll` in api.rs for the
/// pinned live-at-enroll semantics).
pub enum TotpSection {
    /// GET /account, a successful verify, a disable: the state chip
    /// plus the Enroll (not enrolled) / Disable (enrolled) action.
    Row { enrolled: bool },
    /// POST /account/totp/enroll: the secret + the otpauth URL + the
    /// verify form (the factor is already live — the card says so;
    /// verification is confirmation, not the state flip).
    Enrolling { secret: String, url: String },
    /// POST /account/totp/verify with a wrong or stale code: the
    /// spec §7 try-again banner + the verify form again. The secret
    /// is deliberately not re-rendered — the enroll response already
    /// showed it (and the authenticator app holds it);
    /// mycelium-auth exposes no decrypt-for-display path and adding
    /// one is an auth-surface change (spec §1 non-goal).
    VerifyFailed,
}

/// The verify form shared by the enrollment card and the failed-code
/// re-render — a single page-load POST (spec §7: SSR, no script; the
/// hidden csrf_token follows the row-form pattern).
fn totp_verify_form(csrf_esc: &str) -> String {
    let actions = mycelium_ui::render::render(mycelium_ui::form_actions(&[("Verify", "primary")]));
    format!(
        r#"<form method="post" action="/account/totp/verify">
  <input type="hidden" name="csrf_token" value="{csrf_esc}">
  <div class="field">
    <label for="totp-code">Verification code</label>
    <input id="totp-code" type="text" name="code" inputmode="numeric" autocomplete="one-time-code" required>
  </div>
  {actions}
</form>"#
    )
}

/// The Recovery key card's state — one variant per /account response
/// (Task 4). The resting card carries the regenerate form (current
/// password; the hidden csrf_token follows the row-form pattern); a
/// failed password check re-renders the same form plus a banner; a
/// successful rotation renders the shown-once key card (the keys
/// page's treatment: the key in `<code id="new-recovery-key">` beside
/// the copy-button island — the key rides the response body, never a
/// URL).
pub enum RecoverySection {
    /// The resting card: explanatory text + the Regenerate form.
    Resting,
    /// The wrong-password check failed: nothing was rotated or
    /// persisted; the danger banner rides the page's banner slot.
    WrongPassword,
    /// The rotation succeeded and persisted: the shown-once card.
    Rotated { key: String },
}

/// Account security (mockup 10): three cards — Password (the PR-2
/// lone card's body moved here verbatim: three fields posting the
/// EXISTING /password handler — its POST unchanged by Task 5's merge,
/// which retargeted that handler's error renders to this page),
/// Two-factor (the TOTP section per `totp` + the WebAuthn rows:
/// credential count + the per-credential Remove list — nickname-less
/// base64url ids with a Remove form each — and the Register button,
/// disabled in SSR until webauthn.js enables it), and Recovery key
/// (the regenerate flow: password-gated rotation, the new key shown
/// exactly once, sessions untouched).
///
/// QR deviation (documented): the enrollment card renders the
/// otpauth URL as selectable text beside the secret. The repo has no
/// QR generator, no QR dependency exists in the workspace, and taking
/// one unilaterally is not this task's decision — the data:-URI QR
/// lands when a QR dependency decision lands (CSP `img-src` already
/// allows `data:`).
///
/// Escaping: card bodies are raw `TrustedHtml` slots composed from
/// escaped fragments — the secret is base32 (no escapable characters)
/// but escapes anyway, the otpauth URL carries the username and
/// escapes, CSRF tokens escape, the recovery key is hex+prefix but
/// escapes anyway, credential ids are base64url but escape anyway,
/// and every banner message is a handler-side constant riding the
/// banner primitive's TEXT position.
#[allow(clippy::too_many_arguments)]
pub fn account_page(
    user: &SessionUser,
    csrf: &str,
    totp: TotpSection,
    recovery: RecoverySection,
    webauthn_count: i64,
    webauthn_credentials: &[mycelium_auth::webauthn::CredentialRow],
    banner: Option<(&str, &str)>,
) -> Html<String> {
    let csrf_esc = html_escape(csrf);
    let banner_html = banner
        .map(|(kind, msg)| mycelium_ui::render::render(mycelium_ui::banner(kind, msg)))
        .unwrap_or_default();

    // Password card: the PR-2 password page's card body, moved as-is
    // (flash slot dropped — /password's error renders and the
    // ?forced=1 marker ride this page's banner slot since Task 5's
    // merge).
    let pw_actions =
        mycelium_ui::render::render(mycelium_ui::form_actions(&[("Change", "primary")]));
    let password_body = format!(
        r#"<form method="post" action="/password">
  <input type="hidden" name="csrf_token" value="{csrf_esc}">
  <div class="field">
    <label for="pw-old">Current password</label>
    <input id="pw-old" type="password" name="old" required>
  </div>
  <div class="field">
    <label for="pw-new">New password (min 20 chars)</label>
    <input id="pw-new" type="password" name="new" required minlength="20">
  </div>
  <div class="field">
    <label for="pw-repeat">Repeat new password</label>
    <input id="pw-repeat" type="password" name="repeat" required minlength="20">
  </div>
  {pw_actions}
</form>"#
    );

    // TOTP row / enrollment flow.
    let totp_html = match &totp {
        TotpSection::Row { enrolled } => {
            let chip = mycelium_ui::render::render(mycelium_ui::chip(
                if *enrolled {
                    "enrolled"
                } else {
                    "not enrolled"
                },
                if *enrolled { "accent" } else { "neutral" },
            ));
            let action = if *enrolled {
                format!(
                    r#"<form method="post" action="/account/totp/disable"><input type="hidden" name="csrf_token" value="{csrf_esc}"><button type="submit" class="btn">Disable</button></form>"#
                )
            } else {
                format!(
                    r#"<form method="post" action="/account/totp/enroll"><input type="hidden" name="csrf_token" value="{csrf_esc}"><button type="submit" class="btn btn--primary">Enroll</button></form>"#
                )
            };
            format!(
                r#"<div class="qa-row"><div class="qa-row__label"><span>Authenticator app (TOTP)</span></div><div class="qa-row__side">{chip}{action}</div></div>"#
            )
        }
        TotpSection::Enrolling { secret, url } => {
            let form = totp_verify_form(&csrf_esc);
            format!(
                r#"<p>Enter this secret into your authenticator app, then confirm with its current 6-digit code. Your second factor is already active — verification confirms your app is in sync, and you will need a code from your authenticator app at every sign-in from now on — don't lose the secret.</p>
  <p>Secret: <code id="totp-secret">{secret}</code></p>
  <p>Enrollment URL: <code>{url}</code></p>
  {form}"#,
                secret = html_escape(secret),
                url = html_escape(url),
            )
        }
        TotpSection::VerifyFailed => {
            let warn = mycelium_ui::render::render(mycelium_ui::banner(
                "warning",
                "code expired/invalid, try again",
            ));
            format!("{warn}{}", totp_verify_form(&csrf_esc))
        }
    };

    // WebAuthn rows: the count and the per-credential list are real
    // (the hoisted manager's queries); the Register button renders
    // DISABLED in SSR and webauthn.js enables it when the browser has
    // WebAuthn support — without script the rows stay state-only with
    // a disabled action (the honest degradation spec §7 pins). Each
    // enrolled credential gets its own Remove form (nickname-less:
    // the base64url credential id is the only identifier the store
    // ever kept).
    let wa_enrolled = webauthn_count > 0;
    let wa_chip = mycelium_ui::render::render(mycelium_ui::chip(
        if wa_enrolled {
            "enrolled"
        } else {
            "not enrolled"
        },
        if wa_enrolled { "accent" } else { "neutral" },
    ));
    let wa_count = format!(
        "{webauthn_count} credential{}",
        if webauthn_count == 1 { "" } else { "s" }
    );
    let webauthn_row = format!(
        r#"<div class="qa-row"><div class="qa-row__label"><span>Passkey (WebAuthn)</span></div><div class="qa-row__side">{wa_chip}<span>{wa_count}</span><button class="btn" disabled data-webauthn-register>Register</button></div></div>"#
    );
    let webauthn_creds = webauthn_credentials
        .iter()
        .map(|c| {
            format!(
                r#"<div class="qa-row"><div class="qa-row__label"><span class="muted">Added {}</span></div><div class="qa-row__side"><code>{}</code><form method="post" action="/account/webauthn/remove"><input type="hidden" name="csrf_token" value="{csrf_esc}"><input type="hidden" name="credential_id" value="{}"><button type="submit" class="btn btn--danger">Remove</button></form></div></div>"#,
                c.created_at.format("%Y-%m-%d"),
                html_escape(&c.id),
                html_escape(&c.id),
            )
        })
        .collect::<String>();

    let twofa_body = format!("{totp_html}{webauthn_row}{webauthn_creds}");

    // Recovery key card (mockup 10's letter + the regenerate flow).
    let recovery_body = match &recovery {
        RecoverySection::Resting | RecoverySection::WrongPassword => {
            // WrongPassword's banner rides the page banner slot; the
            // card itself is identical to the resting state.
            let regen_actions = mycelium_ui::render::render(mycelium_ui::form_actions(&[(
                "Regenerate",
                "primary",
            )]));
            format!(
                r#"<p>Your recovery key is the last-resort way back into your account if you lose your password and your second factor.</p>
  <p>The key was shown once when your account was created. Regenerating it requires your current password, and the new key is shown only once. Rotation does not sign you out.</p>
  <form method="post" action="/account/recovery/regenerate">
    <input type="hidden" name="csrf_token" value="{csrf_esc}">
    <div class="field">
      <label for="regen-password">Current password</label>
      <input id="regen-password" type="password" name="current_password" required>
    </div>
    {regen_actions}
  </form>"#
            )
        }
        RecoverySection::Rotated { key } => {
            // The shown-once card: the keys page's treatment — the key
            // in a selectable <code> beside the copy-button island
            // (islands.js + the hydrate bundle load for this render).
            let copy = mycelium_ui::render::render(mycelium_ui::copy_button("new-recovery-key"));
            format!(
                r#"<p>Copy this recovery key now — it is shown only once and cannot be retrieved later. The previous key stopped working the moment this one was created.</p><p class="shown-once"><code id="new-recovery-key">{}</code>{copy}</p>"#,
                html_escape(key)
            )
        }
    };

    let header = mycelium_ui::render::render(mycelium_ui::page_header(
        "Account security",
        TrustedHtml::from_markup(""),
    ));
    let pw_card = mycelium_ui::render::render(mycelium_ui::card(
        "Change password",
        TrustedHtml::from_markup(password_body),
    ));
    let twofa_card = mycelium_ui::render::render(mycelium_ui::card(
        "Two-factor",
        TrustedHtml::from_markup(twofa_body),
    ));
    let recovery_card = mycelium_ui::render::render(mycelium_ui::card(
        "Recovery key",
        TrustedHtml::from_markup(recovery_body),
    ));
    let body = format!("{header}{banner_html}{pw_card}{twofa_card}{recovery_card}");
    // webauthn.js loads on every /account render (the Register button
    // wiring); the copy-button island's hydrate bundle loads only for
    // the shown-once render — the resting page stays light, and the
    // value stays selectable text without the bundle either way.
    let (extra, modules) = if matches!(recovery, RecoverySection::Rotated { .. }) {
        (
            &["/assets/webauthn.js", "/assets/islands.js"][..],
            &["/assets/mycelium_ui.js"][..],
        )
    } else {
        (&["/assets/webauthn.js"][..], &[][..])
    };
    layout_full(
        "Account security",
        Some(user),
        csrf,
        "/account",
        body,
        extra,
        modules,
        "",
    )
}

// ---------- Admin pages (PR 3: the five-page split) ----------

/// The admin section's TabBar (spec §6's exact labels) with the
/// current page's tab active — rendered under the page header on
/// every admin page.
fn admin_tabs(active: &str) -> String {
    mycelium_ui::render::render(mycelium_ui::tab_bar(&[
        ("Users", "/admin/users", active == "/admin/users"),
        ("Sign-in", "/admin/sign-in", active == "/admin/sign-in"),
        ("LLM backend", "/admin/llm", active == "/admin/llm"),
        ("Library", "/admin/library", active == "/admin/library"),
        ("System", "/admin/system", active == "/admin/system"),
    ]))
}

/// The admin pages' flash banners: the `ok`/`error` query params the
/// admin POST handlers redirect back with — the handlers' flash
/// mechanism, unchanged from the old `/admin?error=` flow (only the
/// host page differs). Messages ride leptos TEXT positions in the
/// banner primitive, so user-derived error text escapes there.
fn admin_flash(ok: Option<&str>, err: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(m) = ok {
        out.push_str(&mycelium_ui::render::render(mycelium_ui::banner(
            "success", m,
        )));
    }
    if let Some(m) = err {
        out.push_str(&mycelium_ui::render::render(mycelium_ui::banner(
            "danger", m,
        )));
    }
    out
}

/// Admin: Users (mockup 03) — the TabBar, the users data_table
/// (username, email, Role chip, provider, Status chip), and the
/// right-hand create-user side card (the existing POST /admin/users
/// form: the confirm field kept, the minlength mirroring
/// security_config kept). `created` renders the shown-once
/// recovery-key card the create flow answers with — the key rides the
/// response body, never a URL.
///
/// Escaping: every cell is a raw-markup slot composed from escaped
/// fragments (usernames, emails, providers `html_escape`d) and
/// trusted component output (the role/status chips); the created
/// card's username and key are escaped at composition.
pub fn admin_users_page(
    user: &SessionUser,
    csrf: &str,
    users: &[mycelium_auth::users::UserRow],
    min_password_len: usize,
    created: Option<(&str, &str)>,
    ok: Option<&str>,
    err: Option<&str>,
) -> Html<String> {
    let rows: Vec<Vec<TrustedHtml>> = users
        .iter()
        .map(|u| {
            // Role chip: admin accented, anything else neutral (the
            // CHECK constraint keeps the stored string in range).
            let role_chip = mycelium_ui::render::render(mycelium_ui::chip(
                &u.role,
                if u.role == "admin" {
                    "accent"
                } else {
                    "neutral"
                },
            ));
            // Status chip: the forced-change flag is the warning
            // state; a healthy account reads "ok".
            let status_chip = if u.must_change_password {
                mycelium_ui::render::render(mycelium_ui::chip("must change password", "warning"))
            } else {
                mycelium_ui::render::render(mycelium_ui::chip("ok", "neutral"))
            };
            vec![
                TrustedHtml::from_markup(html_escape(&u.username)),
                TrustedHtml::from_markup(html_escape(&u.email)),
                TrustedHtml::from_markup(role_chip),
                TrustedHtml::from_markup(html_escape(&u.auth_provider)),
                TrustedHtml::from_markup(status_chip),
            ]
        })
        .collect();
    let table = mycelium_ui::render::render(mycelium_ui::data_table(
        &["Username", "Email", "Role", "Provider", "Status"],
        &rows,
    ));
    // The create-user side card — the existing form, plus the
    // recovery-key note (the flow's honest companion: the success
    // answer shows the key exactly once).
    let form = format!(
        r#"<form method="post" action="/admin/users">
  <label>Username</label><input name="username" required>
  <label>Email</label><input name="email" type="email" required>
  <label>Password (min {min_password_len} chars)</label><input name="password" type="password" required minlength="{min_password_len}">
  <label>Confirm password</label><input name="password_confirm" type="password" required minlength="{min_password_len}">
  <label>Role</label><select name="role"><option value="user">user</option><option value="admin">admin</option></select>
  <p class="muted">A recovery key is generated on creation and shown once — hand it to the user now.</p>
  <button type="submit">Create</button>
</form>"#
    );
    let side = mycelium_ui::render::render(mycelium_ui::card(
        "Create user",
        TrustedHtml::from_markup(form),
    ));
    // The shown-once recovery-key card: the old page's exact in-page
    // treatment (flash line + <pre>), both fragments escaped.
    let created_card = created
        .map(|(username, key)| {
            let body = format!(
                r#"<div class="flash ok">User <b>{}</b> created. Recovery key (shown ONCE — give it to the user now):</div>
<pre>{}</pre>"#,
                html_escape(username),
                html_escape(key)
            );
            mycelium_ui::render::render(mycelium_ui::card(
                "User created",
                TrustedHtml::from_markup(body),
            ))
        })
        .unwrap_or_default();
    let header = mycelium_ui::render::render(mycelium_ui::page_header(
        "Users",
        TrustedHtml::from_markup(""),
    ));
    let body = format!(
        "{header}{tabs}{flash}{created_card}<div class=\"split-grid\">{table}{side}</div>",
        tabs = admin_tabs("/admin/users"),
        flash = admin_flash(ok, err),
    );
    layout("Users", Some(user), csrf, "/admin/users", body)
}

/// Admin: Sign-in (mockup 11) — the security settings card (the
/// existing POST /admin/security form; each field is a `.field` with
/// a plain label and its clamp range as a small `.field__hint` line
/// under the input — the range text verbatim from the old labels) and
/// the OIDC card (the existing POST /admin/oidc form). Both POSTs
/// redirect back here with ?ok=/?error=.
pub fn admin_sign_in_page(
    user: &SessionUser,
    csrf: &str,
    security: &crate::state::SecurityConfig,
    oidc_configured: bool,
    ok: Option<&str>,
    err: Option<&str>,
) -> Html<String> {
    let security_body = format!(
        r#"<p class="muted">Tunable within guardrails — values outside the ranges below are clamped. Session TTL applies to new logins; existing sessions keep their expiry.</p>
<form method="post" action="/admin/security">
  <div class="field">
    <label>Session TTL</label><input name="session_ttl_minutes" type="number" min="15" max="10080" value="{}" required>
    <p class="field__hint">(minutes, 15–10080; 720 = 12h)</p>
  </div>
  <div class="field">
    <label>Login failures before lockout</label><input name="login_max_failures" type="number" min="3" max="10" value="{}" required>
    <p class="field__hint">(3–10)</p>
  </div>
  <div class="field">
    <label>Lockout backoff cap</label><input name="login_lockout_seconds" type="number" min="30" max="3600" value="{}" required>
    <p class="field__hint">(seconds, 30–3600)</p>
  </div>
  <div class="field">
    <label>Minimum password length</label><input name="min_password_length" type="number" min="12" max="128" value="{}" required>
    <p class="field__hint">(12–128)</p>
  </div>
  <div class="field">
    <label>Passage cap</label><input name="passage_max_chars" type="number" min="16384" max="1048576" value="{}" required>
    <p class="field__hint">(chars, 16384–1048576; 131072 = 128k)</p>
  </div>
  <button type="submit">Save security settings</button>
</form>"#,
        security.session_ttl_minutes,
        security.login_max_failures,
        security.login_lockout_seconds,
        security.min_password_length,
        security.passage_max_chars
    );
    let oidc_status = if oidc_configured {
        "configured"
    } else {
        "not configured"
    };
    let oidc_body = format!(
        r#"<p class="muted">Status: {oidc_status}</p>
<form method="post" action="/admin/oidc">
  <label>Issuer URL</label><input name="issuer_url" placeholder="https://sso.example.com/realms/main">
  <label>Client ID</label><input name="client_id">
  <label>Client secret</label><input name="client_secret" type="password">
  <label>Redirect URI</label><input name="redirect_uri" placeholder="https://host/auth/oidc/callback">
  <button type="submit">Save OIDC config</button>
</form>"#
    );
    let header = mycelium_ui::render::render(mycelium_ui::page_header(
        "Sign-in",
        TrustedHtml::from_markup(""),
    ));
    let body = format!(
        "{header}{tabs}{flash}{}{}",
        mycelium_ui::render::render(mycelium_ui::card(
            "Security settings",
            TrustedHtml::from_markup(security_body),
        )),
        mycelium_ui::render::render(mycelium_ui::card(
            "OIDC SSO",
            TrustedHtml::from_markup(oidc_body),
        )),
        tabs = admin_tabs("/admin/sign-in"),
        flash = admin_flash(ok, err),
    );
    layout("Sign-in", Some(user), csrf, "/admin/sign-in", body)
}

/// Admin: LLM backend — the LLM card on its own page (the existing
/// POST /admin/llm form). The api-key STATUS renders; the key itself
/// is never rendered back to the browser.
/// `llm` is rendered as given — the caller hands in the CLAMPED config,
/// so the boxes always show a value the gate would actually accept.
pub fn admin_llm_page(
    user: &SessionUser,
    csrf: &str,
    llm: &crate::LlmConfig,
    ok: Option<&str>,
    err: Option<&str>,
) -> Html<String> {
    use mycelium_librarian::llm::{
        CHAT_QUEUE_DEPTH_RANGE, CHAT_WAIT_SECS_RANGE, MAX_PARALLEL_RANGE, PER_USER_ACTIVE_RANGE,
        QUERY_WAIT_SECS_RANGE,
    };
    let key_status = if llm.api_key.is_some() {
        "stored"
    } else {
        "not set"
    };
    // One labelled number input per tunable: the range is both a
    // browser-enforced min/max and a visible hint (server-side the
    // value is clamped, never rejected — LLM concurrency spec §3.2).
    let number_field = |label: &str, name: &str, value: u32, range: (u32, u32)| {
        format!(
            r#"  <label>{label} ({lo}&ndash;{hi})</label><input name="{name}" type="number" min="{lo}" max="{hi}" value="{value}">
"#,
            label = html_escape(label),
            name = html_escape(name),
            lo = range.0,
            hi = range.1,
            value = html_escape(&value.to_string()),
        )
    };
    let concurrency = [
        number_field(
            "Max parallel LLM runs",
            "max_parallel",
            llm.max_parallel,
            MAX_PARALLEL_RANGE,
        ),
        number_field(
            "Active runs per user",
            "per_user_active",
            llm.per_user_active,
            PER_USER_ACTIVE_RANGE,
        ),
        number_field(
            "Query wait bound (seconds)",
            "query_wait_secs",
            llm.query_wait_secs,
            QUERY_WAIT_SECS_RANGE,
        ),
        number_field(
            "Chat wait bound (seconds)",
            "chat_wait_secs",
            llm.chat_wait_secs,
            CHAT_WAIT_SECS_RANGE,
        ),
        number_field(
            "Max queued requests",
            "chat_queue_depth",
            llm.chat_queue_depth,
            CHAT_QUEUE_DEPTH_RANGE,
        ),
    ]
    .concat();
    let body_form = format!(
        r#"<p class="muted">API key: {key_status}</p>
<form method="post" action="/admin/llm">
  <label>OpenAI-compatible base URL</label><input name="url" value="{url}">
  <label>Model</label><input name="model" value="{model}">
  <label>API key (sent as a Bearer header; leave blank to keep the current one)</label><input name="api_key" type="password" autocomplete="off">
  <p class="muted">Concurrency — out-of-range values are clamped on save and applied to the running server immediately.</p>
{concurrency}  <button type="submit">Save LLM config</button>
  <button type="submit" formaction="/admin/llm/test">Test connection</button>
</form>"#,
        url = html_escape(&llm.url),
        model = html_escape(&llm.model),
    );
    let header = mycelium_ui::render::render(mycelium_ui::page_header(
        "LLM backend",
        TrustedHtml::from_markup(""),
    ));
    let body = format!(
        "{header}{tabs}{flash}{}",
        mycelium_ui::render::render(mycelium_ui::card(
            "LLM backend",
            TrustedHtml::from_markup(body_form),
        )),
        tabs = admin_tabs("/admin/llm"),
        flash = admin_flash(ok, err),
    );
    layout("LLM backend", Some(user), csrf, "/admin/llm", body)
}

/// Admin: Library (mockup 12) — the stack of section cards:
/// bookshelves (create form + table with the books-count column, the
/// PR-2 books page's derived-count pattern), the upload card (the
/// existing multipart POST /api/v1/ingest form with the shelf
/// select), the upload limits (existing POST /admin/upload-limits),
/// and the read-only global-skills listing (a link to /skills; no
/// edit affordances here).
pub fn admin_library_page(
    user: &SessionUser,
    csrf: &str,
    shelves: &[(String, bool, usize)],
    max_book_mib: u64,
    global_skills: &[String],
    ok: Option<&str>,
    err: Option<&str>,
) -> Html<String> {
    let rows: Vec<Vec<TrustedHtml>> = shelves
        .iter()
        .map(|(name, global, count)| {
            let vis_chip = mycelium_ui::render::render(mycelium_ui::chip(
                if *global { "global-read" } else { "private" },
                if *global { "accent" } else { "neutral" },
            ));
            vec![
                TrustedHtml::from_markup(html_escape(name)),
                TrustedHtml::from_markup(vis_chip),
                TrustedHtml::from_markup(count.to_string()),
            ]
        })
        .collect();
    let shelf_table = mycelium_ui::render::render(mycelium_ui::data_table(
        &["Name", "Visibility", "Books"],
        &rows,
    ));
    let shelves_body = format!(
        r#"{shelf_table}
<form method="post" action="/admin/bookshelves">
  <label>Name</label><input name="name" required>
  <label>Global read</label><select name="global"><option value="0">private</option><option value="1">global-read</option></select>
  <button type="submit">Create bookshelf</button>
</form>"#
    );
    // The upload form's shelf select: every shelf name, escaped for
    // both the value attribute and the label text.
    let shelf_options = shelves
        .iter()
        .map(|(name, _, _)| {
            format!(
                r#"<option value="{}">{}</option>"#,
                html_escape(name),
                html_escape(name)
            )
        })
        .collect::<String>();
    let upload_body = format!(
        r#"<p class="muted">Markdown (.md) up to {max_book_mib} MiB (adjustable below). The librarian catalogs it onto the bookshelf (LLM-assisted when the configured backend is reachable; heuristic otherwise).</p>
<form method="post" action="/api/v1/ingest" enctype="multipart/form-data">
  <label>Bookshelf</label><select name="bookshelf" required>{shelf_options}</select>
  <label>Slug</label><input name="slug" required pattern="[a-zA-Z0-9-]+" placeholder="my-book">
  <label>Title</label><input name="title" required>
  <label>Book file (.md)</label><input name="file" type="file" accept=".md,text/markdown" required>
  <button type="submit">Upload and ingest</button>
</form>"#
    );
    let limits_body = format!(
        r#"<form method="post" action="/admin/upload-limits">
  <label>Max book size (MiB, 1–255)</label><input name="max_book_mib" type="number" min="1" max="255" value="{max_book_mib}" required>
  <button type="submit">Save limit</button>
</form>"#
    );
    // Read-only global-skills listing: names only — management lives
    // on the Skills page.
    let skills_list = if global_skills.is_empty() {
        mycelium_ui::render::render(mycelium_ui::empty_state(
            "No global skills",
            "Install a skill to begin.",
        ))
    } else {
        let items = global_skills
            .iter()
            .map(|t| format!("<li>{}</li>", html_escape(t)))
            .collect::<String>();
        format!("<ul>{items}</ul>")
    };
    let skills_body = format!(
        r#"{skills_list}<p class="muted">The global skills shelf is readable by all users; only admins can edit. Manage skills on the <a href="/skills">Skills page</a> (edit links appear there for admins).</p>"#
    );
    let header = mycelium_ui::render::render(mycelium_ui::page_header(
        "Library",
        TrustedHtml::from_markup(""),
    ));
    let body = format!(
        "{header}{tabs}{flash}{}{}{}{}",
        mycelium_ui::render::render(mycelium_ui::card(
            "Bookshelves",
            TrustedHtml::from_markup(shelves_body),
        )),
        mycelium_ui::render::render(mycelium_ui::card(
            "Upload book",
            TrustedHtml::from_markup(upload_body),
        )),
        mycelium_ui::render::render(mycelium_ui::card(
            "Upload limits",
            TrustedHtml::from_markup(limits_body),
        )),
        mycelium_ui::render::render(mycelium_ui::card(
            "Global skills",
            TrustedHtml::from_markup(skills_body),
        )),
        tabs = admin_tabs("/admin/library"),
        flash = admin_flash(ok, err),
    );
    layout("Library", Some(user), csrf, "/admin/library", body)
}

/// The TLS card on the admin System page: the ACTIVE certificate's
/// parsed details (source chip, subject/issuer/SANs, validity with an
/// expiry chip, SHA-256 fingerprint), the download link, the pending
/// CSR block (re-download + discard) when one is outstanding, the
/// revert-to-generated button when an upload is active, and the three
/// forms (upload pair, generate CSR, regenerate self-signed).
///
/// Escaping: every dynamic value — including the attacker-influenced
/// subject/issuer/SAN strings of an uploaded cert — goes through
/// `html_escape` here; the card body is composed only from escaped
/// fragments and constant markup.
pub fn tls_card(
    details: Option<&crate::tls_admin::CertDetails>,
    pending: Option<&crate::tls_admin::PendingCsr>,
    csrf: &str,
) -> String {
    let csrf_field = format!(
        r#"<input type="hidden" name="csrf_token" value="{}">"#,
        html_escape(csrf)
    );
    let row = |label: &str, value: String| {
        format!(
            r#"<p><strong>{label}</strong> <span>{value}</span></p>"#,
            label = label,
            value = value
        )
    };
    let mut body = String::new();
    match details {
        Some(d) => {
            let source = match d.source.as_str() {
                "uploaded" => match &d.label {
                    Some(l) if !l.is_empty() => format!(
                        "{} {}",
                        mycelium_ui::render::render(mycelium_ui::chip("uploaded", "accent")),
                        html_escape(l)
                    ),
                    _ => mycelium_ui::render::render(mycelium_ui::chip("uploaded", "accent")),
                },
                _ => mycelium_ui::render::render(mycelium_ui::chip("generated", "neutral")),
            };
            let expiry_chip = if d.expired {
                mycelium_ui::render::render(mycelium_ui::chip("expired", "danger"))
            } else if d.expiring_soon {
                mycelium_ui::render::render(mycelium_ui::chip("expires soon", "warning"))
            } else {
                String::new()
            };
            body.push_str(&row("Source", source));
            body.push_str(&row("Subject", html_escape(&d.subject)));
            body.push_str(&row("Issuer", html_escape(&d.issuer)));
            body.push_str(&row("SANs", html_escape(&d.sans.join(", "))));
            body.push_str(&row("Valid from", html_escape(&d.not_before)));
            body.push_str(&row(
                "Valid until",
                format!("{} {expiry_chip}", html_escape(&d.not_after)),
            ));
            body.push_str(&row(
                "SHA-256 fingerprint",
                format!("<code>{}</code>", html_escape(&d.fingerprint_sha256)),
            ));
            body.push_str(
                r#"<p><a href="/admin/tls/cert">Download certificate</a></p>"#,
            );
            if d.source == "uploaded" {
                body.push_str(&format!(
                    r#"<form method="post" action="/admin/tls/reset">{csrf_field}<button type="submit">Revert to generated certificate</button></form>"#
                ));
            }
        }
        None => body.push_str(
            r#"<p class="muted">No certificate could be parsed — the server may not have finished booting, or the certificate files are unreadable.</p>"#,
        ),
    }
    if let Some(p) = pending {
        body.push_str(&format!(
            r#"<h3>Pending CSR</h3>
{cn}
{sans}
{created}
<p class="muted">The matching private key stays on this server at
<code>&lt;data_dir&gt;/config/tls/pending-csr-key.pem</code> — the upload
form needs it alongside the issued certificate.</p>
<p><a href="/admin/tls/csr">Download CSR</a></p>
<form method="post" action="/admin/tls/pending/discard">{csrf_field}<button type="submit">Discard CSR</button></form>"#,
            cn = row("CN", html_escape(&p.cn)),
            sans = row("SANs", html_escape(&p.sans.join(", "))),
            created = row("Created", html_escape(&p.created_at)),
        ));
    }
    body.push_str(&format!(
        r#"<h3>Upload certificate</h3>
<p class="muted">A third-party cert (+ optional chain, leaf first) and its private key. The pair is validated before it is installed, and the live listener hot-swaps on success.</p>
<form method="post" action="/admin/tls/upload" enctype="multipart/form-data">
  {csrf_field}
  <label>Certificate (PEM)</label><input name="cert_pem" type="file" accept=".pem,application/x-pem-file" required>
  <label>Private key (PEM)</label><input name="key_pem" type="file" accept=".pem,application/x-pem-file" required>
  <label>Label (optional)</label><input name="label" maxlength="120">
  <button type="submit">Upload certificate</button>
</form>
<h3>Generate CSR</h3>
<p class="muted">Generates a fresh key (kept server-side until the matching cert is uploaded) and downloads the CSR for your CA. See <a href="https://github.com/Draco-Lunaris/Mycelium2/blob/master/docs/tls.md">the TLS guide</a>.</p>
<form method="post" action="/admin/tls/csr">
  {csrf_field}
  <label>Common name</label><input name="cn" required>
  <label>SANs (comma-separated; empty = localhost)</label><input name="sans">
  <button type="submit">Generate CSR</button>
</form>
<h3>Regenerate self-signed certificate</h3>
<p class="muted">Mints a fresh self-signed key and certificate carrying the given SANs (comma-separated; empty = localhost) — even if the SANs are unchanged. The choice survives restarts; if an uploaded certificate is active it stays active and the fresh pair becomes its fallback. Clients that trusted the old certificate must re-trust the new one (its fingerprint changes).</p>
<form method="post" action="/admin/tls/regenerate">
  {csrf_field}
  <label>SANs (comma-separated; empty = localhost)</label><input name="sans">
  <button type="submit">Regenerate certificate</button>
</form>"#
    ));
    mycelium_ui::render::render(mycelium_ui::card(
        "TLS certificate",
        TrustedHtml::from_markup(body),
    ))
}

/// The LLM-gate readout card on the admin System page (LLM-concurrency
/// spec §3.6): the live admission state — runs holding a slot, waiters
/// queued, the configured slot count — plus the gate's timeout counter.
///
/// "Recent timeouts" is rendered HONESTLY as the cumulative count since
/// process start: the gate keeps no time-windowed counter, so the label
/// says "since start" rather than implying a window that does not
/// exist.
///
/// The values arrive already read: all four come from the single
/// `LLM_GATE.stats()` read inside `health::system_stats`, carried here
/// through `SystemPageStats`. This function never touches the gate, so
/// the card and the StatTiles above it describe the same instant.
///
/// Escaping: every value is a typed `i64` formatted by this function —
/// no caller-, config- or network-influenced text reaches the markup,
/// so there is no attacker-controlled string to escape (contrast
/// `tls_card`, which escapes its certificate strings). Follows that
/// card's composition: a constant-label row helper, existing CSS
/// (`card`, `muted`), no new classes.
pub fn llm_card(stats: &crate::api::SystemPageStats) -> String {
    let row = |label: &str, value: i64| {
        format!(r#"<p><strong>{label}</strong> <span>{value}</span></p>"#)
    };
    let mut body = String::new();
    body.push_str(&row("Active", stats.llm_active));
    body.push_str(&row("Queued", stats.llm_queued));
    body.push_str(&row("Capacity", stats.llm_capacity));
    body.push_str(&row(
        "Capacity timeouts (since start)",
        stats.llm_timeouts_total,
    ));
    body.push_str(
        r#"<p class="muted">Active runs hold one of the configured slots; queued runs are waiting for one. A timeout is a run whose wait bound elapsed before a slot came free — chat turns surface it to the user, searches fall back to keywords. Active may briefly exceed capacity after the limit is lowered: the gate never preempts a run in flight.</p>"#,
    );
    mycelium_ui::render::render(mycelium_ui::card(
        "LLM runs",
        TrustedHtml::from_markup(body),
    ))
}

/// Format an age in seconds the way the System page reads durations:
/// under a minute "45s", under an hour "5m", hours-and-minutes "1h 0m".
/// Deterministic, integer-only (no clock reads here — callers pass the
/// already-derived seconds).
fn fmt_waited(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// The backend-health indicator + waiting-summary line (outage-recovery
/// spec §3.5), rendered ABOVE the StatTile grid on /admin/system. The
/// values arrive pre-derived through `SystemPageStats` (the ONE
/// `LLM_HEALTH.snapshot()` read in `health::system_stats`), so the
/// indicator cannot disagree with /health.
///
/// The dot is never the only signal: it carries `aria-label` AND a
/// visible state word beside it (WCAG — color alone must not encode
/// state). Disabled reports no outage clause: a Disabled machine's
/// frozen down_since is "nothing to probe", not a live outage (the
/// same rule /health applies to `backend_down_seconds`).
///
/// The summary sentence counts derive from `waiting_items` — chat
/// turns + queued writes only (the kinds the sentence promises to
/// resume); zero-count parts drop, and with no waiting items the
/// sentence is absent entirely.
///
/// Escaping: the state word is `BackendState::as_str()` (a constant),
/// the ages are i64s — nothing attacker-controlled reaches the markup.
fn backend_health_block(stats: &crate::api::SystemPageStats) -> String {
    let state = stats.backend_state;
    let mut line = format!(
        r#"<p><span class="health-dot health-dot--{state}" role="img" aria-label="Backend health: {state}"></span><strong>Backend {state}</strong>"#
    );
    if let Some(down) = stats.backend_down_seconds {
        line.push_str(&format!(" <span>down for {}</span>", fmt_waited(down)));
    }
    match stats.backend_checked_seconds_ago {
        Some(ago) => {
            line.push_str(&format!(
                " <span>last checked {} ago</span>",
                fmt_waited(ago)
            ));
        }
        None => line.push_str(" <span>never checked</span>"),
    }
    line.push_str("</p>");

    let chat = stats
        .waiting_items
        .iter()
        .filter(|i| i.kind == "chat turn")
        .count();
    let writes = stats
        .waiting_items
        .iter()
        .filter(|i| i.kind == "queued write")
        .count();
    if chat + writes > 0 {
        let mut parts = Vec::new();
        if chat > 0 {
            parts.push(format!(
                "{chat} chat {}",
                if chat == 1 { "turn" } else { "turns" }
            ));
        }
        if writes > 0 {
            parts.push(format!(
                "{writes} queued {}",
                if writes == 1 { "write" } else { "writes" }
            ));
        }
        line.push_str(&format!(
            r#"<p class="muted">{} waiting — they will resume automatically when the backend returns.</p>"#,
            parts.join(" and ")
        ));
    }
    line
}

/// The per-item waiting table (outage-recovery spec §3.5): every item
/// waiting on the backend — parked chat turns, un-integrated queued
/// writes, pending ingest jobs. METADATA ONLY (username, kind, handle,
/// state, waited, bounded last_error) — never transcript or concept
/// content. Every cell goes through `html_escape` (the page's shared
/// helper) before entering its TrustedHtml slot, matching the /status
/// jobs table's exact composition.
fn waiting_items_card(stats: &crate::api::SystemPageStats) -> String {
    let rows: Vec<Vec<TrustedHtml>> = stats
        .waiting_items
        .iter()
        .map(|item| {
            vec![
                TrustedHtml::from_markup(html_escape(item.kind)),
                TrustedHtml::from_markup(html_escape(&item.user)),
                TrustedHtml::from_markup(html_escape(&item.what)),
                TrustedHtml::from_markup(html_escape(&item.state)),
                TrustedHtml::from_markup(html_escape(&fmt_waited(item.waited_seconds))),
                TrustedHtml::from_markup(html_escape(item.last_error.as_deref().unwrap_or(""))),
            ]
        })
        .collect();
    let table = mycelium_ui::render::render(mycelium_ui::data_table(
        &["Kind", "User", "What", "State", "Waited", "Last error"],
        &rows,
    ));
    mycelium_ui::render::render(mycelium_ui::card(
        "Waiting items",
        TrustedHtml::from_markup(table),
    ))
}

/// Admin: System — the operational view: the backend-health indicator,
/// waiting summary and per-item waiting table above the StatTiles
/// (outage-recovery spec §3.5), then /health's fields as StatTiles (a
/// tile per value), the LLM-gate readout card, the TLS card, and the
/// backup download (the existing POST /admin/backup button with its
/// no-store rationale). The ingest-jobs table moved to /status (spec
/// §8's PR-4 ruling — /status is the jobs-table home; the PR-3
/// placement here was a stopgap pending that page).
///
/// The page self-refreshes every 30 s via a `<meta http-equiv="refresh">`
/// — the only page that does (the waiting table is a live picture).
/// The head belongs to the mycelium-ui shell, so the tag is
/// post-inserted here rather than threaded through ui: every other
/// page's document stays byte-identical.
pub fn admin_system_page(
    user: &SessionUser,
    csrf: &str,
    stats: &crate::api::SystemPageStats,
    cert: Option<&crate::tls_admin::CertDetails>,
    pending: Option<&crate::tls_admin::PendingCsr>,
    ok: Option<&str>,
    err: Option<&str>,
) -> Html<String> {
    // A StatTile per /health value; absent optionals read "—".
    let absent = "—";
    let tiles = [
        ("Users", stats.users.to_string()),
        ("Queue depth", stats.queue_depth.to_string()),
        (
            "Oldest pending age (s)",
            stats
                .oldest_pending_age_seconds
                .map(|v| v.to_string())
                .unwrap_or_else(|| absent.to_string()),
        ),
        ("Dead queue items", stats.queue_dead_count.to_string()),
        (
            "LLM last success (s ago)",
            stats
                .llm_last_success_seconds
                .map(|v| v.to_string())
                .unwrap_or_else(|| absent.to_string()),
        ),
    ];
    let stat_row = tiles
        .iter()
        .map(|(label, value)| mycelium_ui::render::render(mycelium_ui::stat_tile(label, value)))
        .collect::<String>();
    let stats_html = format!(r#"<div class="stat-row">{stat_row}</div>"#);
    let backup_body = r#"<form method="post" action="/admin/backup"><button type="submit">Download backup</button></form>
<p class="muted">The download is served with Cache-Control: no-store — the archive contains the service key and sealed master keys, so it must never sit in a shared or CDN cache.</p>"#;
    let header = mycelium_ui::render::render(mycelium_ui::page_header(
        "System",
        TrustedHtml::from_markup(""),
    ));
    // The backend indicator + waiting summary + waiting table sit
    // ABOVE the StatTile grid (spec §3.5: the admin's first read is
    // "is the backend down, and what is waiting on it").
    let backend_html = format!(
        "<section class=\"card\">{}</section>",
        backend_health_block(stats)
    );
    let waiting_html = waiting_items_card(stats);
    // The LLM card sits directly under the queue/health tiles (spec
    // §3.6: "next to the existing queue table"), ahead of the TLS and
    // maintenance cards.
    let body = format!(
        "{header}{tabs}{flash}{backend_html}{waiting_html}{stats_html}{}{}{}",
        llm_card(stats),
        tls_card(cert, pending, csrf),
        mycelium_ui::render::render(mycelium_ui::card(
            "Maintenance",
            TrustedHtml::from_markup(backup_body),
        )),
        tabs = admin_tabs("/admin/system"),
        flash = admin_flash(ok, err),
    );
    let mut page = layout("System", Some(user), csrf, "/admin/system", body).0;
    // 30 s auto-refresh — /admin/system only (the head belongs to the
    // mycelium-ui shell; inserting before `</head>` keeps every other
    // page's document byte-identical).
    page = page.replacen(
        "</head>",
        "<meta http-equiv=\"refresh\" content=\"30\">\n</head>",
        1,
    );
    Html(page)
}

/// The receipts card (`/status`): the most recent mutation-queue items
/// of EVERY state — including the terminal `done`/`dead` the System
/// page's waiting table deliberately hides — newest enqueue first,
/// bounded by the caller's fetch (`queue_recent_rows`). It replaces the
/// mockup's placeholder banner, which promised receipt ids and state
/// names the queue could not yet name (issue #1); the panel shows real
/// rows or says the queue is empty — never a fabricated receipt.
///
/// METADATA ONLY (receipt id, tool, state, owner, attempts, age, the
/// drain's bounded `short_detail` text) — never the encrypted payload
/// or args JSON. Every cell goes through `html_escape` (the page's
/// shared helper) before entering its TrustedHtml slot, the waiting
/// table's exact composition; an absent detail renders blank (the same
/// empty-detail convention). The age derives from `created_at` through
/// `fmt_waited`; a timestamp that fails to parse reads 0 — the same
/// degradation as the waiting table's waited seconds.
fn receipts_card(rows: &[mycelium_store::QueueWaitingRow]) -> String {
    if rows.is_empty() {
        return mycelium_ui::render::render(mycelium_ui::card(
            "Receipts",
            TrustedHtml::from_markup(r#"<p class="muted">no queue items</p>"#),
        ));
    }
    let now = chrono::Utc::now();
    let age = |ts: &str| {
        let secs = chrono::DateTime::parse_from_rfc3339(ts)
            .map(|t| now.signed_duration_since(t).num_seconds().max(0))
            .unwrap_or(0);
        fmt_waited(secs)
    };
    let cells: Vec<Vec<TrustedHtml>> = rows
        .iter()
        .map(|row| {
            vec![
                TrustedHtml::from_markup(html_escape(&row.id.to_string())),
                TrustedHtml::from_markup(html_escape(row.tool.as_str())),
                TrustedHtml::from_markup(html_escape(row.status.as_str())),
                TrustedHtml::from_markup(html_escape(&row.username)),
                TrustedHtml::from_markup(html_escape(&row.attempts.to_string())),
                TrustedHtml::from_markup(html_escape(&age(&row.created_at))),
                TrustedHtml::from_markup(html_escape(row.detail.as_deref().unwrap_or(""))),
            ]
        })
        .collect();
    let table = mycelium_ui::render::render(mycelium_ui::data_table(
        &[
            "Receipt", "Tool", "Status", "User", "Attempts", "Age", "Detail",
        ],
        &cells,
    ));
    mycelium_ui::render::render(mycelium_ui::card(
        "Receipts",
        TrustedHtml::from_markup(table),
    ))
}

/// Librarian status (mockup 04; admin-only per spec §6 — the page
/// shows cross-user jobs): the mutation queue's /health surfaces as
/// StatTiles, state chips (the /health degraded rule mirrored over the
/// same values, plus the LLM chip on the same staleness window), the
/// ingest-jobs table (moved here from the PR-3 System page — spec §8
/// names /status the jobs-table home), and the receipts panel over real
/// queue rows (`receipts_card` — the issue-#1 placeholder it replaced is
/// retired).
///
/// Escaping: tile labels/values ride leptos TEXT positions inside the
/// stat_tile primitive; the chips' texts are constants; every jobs and
/// receipt cell is an escaped fragment in a TrustedHtml slot (the System
/// page's exact job-row composition).
pub fn librarian_status_page(
    user: &SessionUser,
    csrf: &str,
    stats: &crate::health::SystemStats,
    jobs: &[(String, String, String, String, String)],
    receipts: &[mycelium_store::QueueWaitingRow],
) -> Html<String> {
    // A StatTile per queue field; absent optionals read "—".
    let absent = "—";
    let tiles = [
        ("Queue depth", stats.queue_depth.to_string()),
        (
            "Oldest pending",
            stats
                .oldest_pending_age_seconds
                .map(|v| v.to_string())
                .unwrap_or_else(|| absent.to_string()),
        ),
        ("Dead", stats.queue_dead_count.to_string()),
        (
            "LLM last success",
            stats
                .llm_last_success_seconds
                .map(|v| v.to_string())
                .unwrap_or_else(|| absent.to_string()),
        ),
    ];
    let stat_row = tiles
        .iter()
        .map(|(label, value)| mycelium_ui::render::render(mycelium_ui::stat_tile(label, value)))
        .collect::<String>();
    let stats_html = format!(r#"<div class="stat-row">{stat_row}</div>"#);
    // State chips — the /health degraded rule mirrored over the same
    // values (db reachable, no dead items, no overdue pending item).
    let degraded = !stats.db_ok
        || stats.queue_dead_count > 0
        || stats
            .oldest_pending_age_seconds
            .is_some_and(|age| age > crate::health::oldest_degraded_at_secs());
    let state_chip = mycelium_ui::render::render(mycelium_ui::chip(
        if degraded { "degraded" } else { "ok" },
        if degraded { "danger" } else { "neutral" },
    ));
    // The LLM chip on the same window: llm_last_success_seconds is an
    // AGE in seconds (None = never succeeded); older than the shared
    // oldest_degraded_at_secs() window (the same threshold that flags
    // an overdue pending queue item) reads unreachable.
    let llm_unreachable = stats
        .llm_last_success_seconds
        .is_none_or(|age| age > crate::health::oldest_degraded_at_secs());
    let llm_chip = mycelium_ui::render::render(mycelium_ui::chip(
        if llm_unreachable {
            "LLM unreachable"
        } else {
            "LLM reachable"
        },
        if llm_unreachable { "danger" } else { "accent" },
    ));
    let chips_html = format!(r#"<div class="chip-row">{state_chip}{llm_chip}</div>"#);
    // The jobs table — the System page's exact build, moved here.
    let jobs_rows: Vec<Vec<TrustedHtml>> = jobs
        .iter()
        .map(|(slug, status, detail, created, shelf)| {
            vec![
                TrustedHtml::from_markup(html_escape(slug)),
                TrustedHtml::from_markup(html_escape(status)),
                TrustedHtml::from_markup(html_escape(detail)),
                TrustedHtml::from_markup(html_escape(created)),
                TrustedHtml::from_markup(html_escape(shelf)),
            ]
        })
        .collect();
    let jobs_body = mycelium_ui::render::render(mycelium_ui::data_table(
        &["Book", "Status", "Detail", "Created", "Shelf"],
        &jobs_rows,
    ));
    // The receipt panel — real queue rows newest-first, or the honest
    // empty line (receipts_card).
    let receipts_html = receipts_card(receipts);
    let header = mycelium_ui::render::render(mycelium_ui::page_header(
        "Librarian status",
        TrustedHtml::from_markup(""),
    ));
    let body = format!(
        "{header}{stats_html}{chips_html}{}{}",
        mycelium_ui::render::render(mycelium_ui::card(
            "Ingest jobs",
            TrustedHtml::from_markup(jobs_body),
        )),
        receipts_html,
    );
    layout("Librarian status", Some(user), csrf, "/status", body)
}

/// Minimal percent-encoding for query-string values.
pub fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Minimal HTML escaping.
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Textarea-safe escaping: HTML-escape, then neutralize `</textarea>`
/// breakouts (html_escape already handles < and >, this is belt-and-
/// braces for the specific sequence).
pub fn textarea_escape(s: &str) -> String {
    html_escape(s)
}

/// Helper for handlers: the CSRF token for the current session.
pub async fn current_csrf(state: &crate::state::AppState, session: &SessionId) -> String {
    state
        .sessions
        .get(session.0)
        .await
        .map(|s| s.csrf_token)
        .unwrap_or_default()
}

/// 404 page.
pub fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the page body must render inside `<main>`, and the
    /// app.js `<script>` tag must carry a clean `?v=` version — not the body.
    /// (A swapped `format!` argument once put the whole body into the script
    /// tag and left `<main>` holding the bare asset version.) Drives a real
    /// shelled page through the mycelium-ui shell.
    #[test]
    fn layout_renders_body_into_main_not_script() {
        let user = SessionUser {
            user_id: uuid::Uuid::new_v4(),
            username: "test".to_string(),
            role: Role::User,
        };
        let entries: Vec<mycelium_store::ConceptEntry> = Vec::new();
        let broken = std::collections::HashSet::new();
        let html = home_page(&user, "", &entries, &broken, None, None, None).0;
        // Mockup 01 titles the page "Browse" (retargeted from the
        // legacy "Your bundle" heading — spec §9.2 markup retargeting).
        assert!(
            html.contains(r#"<main><header class="page-header"><h1>Browse</h1>"#),
            "body should open inside <main>: {html}"
        );
        assert!(
            html.contains(r#"<script src="/assets/app.js?v="#),
            "app.js script tag should be well-formed: {html}"
        );
        // The body must not leak into the script src attribute.
        assert!(
            !html.contains("app.js?v=<h1"),
            "body leaked into the app.js script tag: {html}"
        );
        assert!(
            !html.contains(&format!("<main>{}", crate::assets::ASSETS_VERSION)),
            "<main> should not hold the bare asset version: {html}"
        );
    }

    /// Review Focus 3: the concept editor round-trips user markdown
    /// through a textarea — a hostile `</textarea><script>` sequence must
    /// be neutralized (escaped, never executable markup), matching the
    /// XSS integration-test posture. (Rebuilt editor, Task 2: the same
    /// guarantee through `concept_editor` + its preview builder.)
    #[test]
    fn concept_editor_neutralizes_textarea_breakout() {
        let user = SessionUser {
            user_id: uuid::Uuid::new_v4(),
            username: "test".to_string(),
            role: Role::User,
        };
        let hostile = "</textarea><script>alert(1)</script>";
        let (preview, fm_err) = build_preview_html(hostile);
        let html = concept_editor(
            &user,
            "",
            "/notes/x.md",
            hostile,
            preview,
            fm_err.as_deref(),
            None,
            true,
            None,
        )
        .0;
        assert!(
            !html.contains("<script>alert(1)</script>"),
            "raw script tag leaked through the editor textarea: {html}"
        );
        assert!(
            html.contains("&lt;/textarea&gt;"),
            "escaped form missing: {html}"
        );
    }

    /// Task 2: the preview pane renders hostile saved markdown ESCAPED.
    /// The minimal renderer composes from html-escaped fragments only
    /// (escape-then-compose, the spec's body-interpolation contract), so
    /// a stored `<script>` can never become executable markup in the
    /// preview — the in-module mirror of the
    /// `editor_breadcrumb_preview_hostile` integration test.
    #[test]
    fn editor_preview_escapes_hostile_markdown() {
        let user = SessionUser {
            user_id: uuid::Uuid::new_v4(),
            username: "test".to_string(),
            role: Role::User,
        };
        let hostile =
            "---\ntype: Note\ntitle: <script>alert(1)</script>\n---\n\n<script>alert(1)</script>\n";
        let (preview, fm_err) = build_preview_html(hostile);
        assert!(
            fm_err.is_none(),
            "hostile-but-valid frontmatter must parse: {fm_err:?}"
        );
        let html = concept_editor(
            &user,
            "",
            "/notes/x.md",
            hostile,
            preview,
            None,
            None,
            true,
            None,
        )
        .0;
        assert!(
            html.contains(r#"<div class="editor-preview">"#),
            "preview pane present: {html}"
        );
        assert!(
            !html.contains("<script>alert(1)</script>"),
            "raw script tag leaked into the preview: {html}"
        );
        assert!(
            html.contains("&lt;script&gt;"),
            "escaped form missing in the preview: {html}"
        );
    }

    /// Review fix (Minor): the parse-error preview strips the
    /// frontmatter block from CRLF and BOM documents too — the core
    /// splitter normalizes both before splitting, and the strip path
    /// mirrors it, so a broken-frontmatter CRLF/BOM document never
    /// previews its frontmatter as visible prose.
    #[test]
    fn preview_strips_frontmatter_block_on_crlf_and_bom() {
        // Missing `type` (parse error) in CRLF form: the preview must
        // show the body, not the frontmatter block.
        let crlf = "---\r\ntitle: No Type\r\n---\r\n\r\nBody text.\r\n";
        let (preview, err) = build_preview_html(crlf);
        assert!(err.is_some(), "missing type must fail parse: {err:?}");
        assert!(preview.contains("Body text."), "{preview}");
        assert!(
            !preview.contains("title: No Type"),
            "frontmatter block must strip on CRLF: {preview}"
        );
        // BOM-prefixed twin: identical behavior.
        let bom = format!("\u{feff}{crlf}");
        let (preview, err) = build_preview_html(&bom);
        assert!(err.is_some(), "missing type must fail parse: {err:?}");
        assert!(preview.contains("Body text."), "{preview}");
        assert!(
            !preview.contains("title: No Type"),
            "frontmatter block must strip on BOM+CRLF: {preview}"
        );
    }

    /// mycelium-ui's shell emits asset URLs with its own version constant;
    /// it must equal this crate's scaffold version, so a one-sided bump
    /// fails here. Lives in mycelium-web (the only crate seeing both).
    #[test]
    fn assets_versions_lockstep() {
        assert_eq!(
            crate::assets::ASSETS_VERSION,
            mycelium_ui::ASSETS_VERSION,
            "mycelium-ui and mycelium-web asset versions must bump together"
        );
    }

    /// Task-7 fix pin: artifact-vs-markup lockstep. The checked-in
    /// hydrate bundle (`crates/mycelium-web/assets/mycelium_ui.js`,
    /// the wasm-bindgen wrapper) exports one hydrate function per
    /// island (`export function <Name>_<hash>(el)`); the SSR roots
    /// carry the same name in `data-component="..."`. Every exported
    /// name must appear in the rendered island markup — a one-sided
    /// bundle regeneration (stale artifact, changed island signature)
    /// ships islands the traversal script can't hydrate, and this
    /// fails instead. Parses the wrapper with the bindgen emission
    /// shape (`export function {Name}_{digits}(el)`); the SSR roots
    /// come from the mycelium-ui island wrappers (the same fns the
    /// pages render).
    #[test]
    fn hydrate_bundle_exports_match_island_roots() {
        let wrapper = include_str!("../assets/mycelium_ui.js");
        let mut exported = Vec::new();
        let mut scan = 0usize;
        while let Some(rel) = wrapper[scan..].find("export function ") {
            let start = scan + rel + "export function ".len();
            let rest = &wrapper[start..];
            let end = rest.find('(').expect("export must open its args");
            let name = rest[..end].trim();
            // Only the per-island hydrate exports: <Component>_<hash>.
            if name.contains('_')
                && name
                    .chars()
                    .rev()
                    .take_while(|c| c.is_ascii_digit())
                    .count()
                    > 0
            {
                exported.push(name.to_string());
            }
            scan = start + end;
        }
        assert!(
            !exported.is_empty(),
            "no island hydrate exports found in the checked-in bundle"
        );
        let rendered = [
            mycelium_ui::render::render(mycelium_ui::chat_composer("x")),
            mycelium_ui::render::render(mycelium_ui::copy_button("y")),
        ]
        .join("\n");
        for name in &exported {
            assert!(
                rendered.contains(&format!(r#"data-component="{name}""#)),
                "bundle exports hydrate fn {name} but no SSR island root carries it — \
                 regenerate the bundle or the island changed under it"
            );
        }
    }
}
