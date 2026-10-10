//! Server-rendered pages (Leptos SSR, no hydration needed for the
//! form-driven UI; the graph page uses graph.js).

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use mycelium_auth::rbac::{Role, SessionUser};
use mycelium_core::concept::Concept;

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

/// Layout with extra script sources (e.g. graph.js).
pub fn layout_with_scripts(
    title: &str,
    user: Option<&SessionUser>,
    csrf: &str,
    active_path: &str,
    body: String,
    extra_scripts: &[&str],
) -> Html<String> {
    layout_full(title, user, csrf, active_path, body, extra_scripts, "")
}

/// Full-control layout: extra scripts + a body class (e.g. the chat
/// page's full-viewport mode). Adapts the session user to the shell's
/// primitive `(name, is_admin)` pair and delegates the whole document
/// to `mycelium_ui::shell::shell`.
pub fn layout_full(
    title: &str,
    user: Option<&SessionUser>,
    csrf: &str,
    active_path: &str,
    body: String,
    extra_scripts: &[&str],
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
    let filter_chip = |label: &str, href: &str, active: bool| {
        format!(
            r#"<a class="chip{}" href="{}">{}</a>"#,
            if active { " chip--accent" } else { "" },
            href,
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
    // without script) plus the filter chips, one row (mockup 01).
    let actions = format!(
        r#"<a class="{}" href="/concept?new=1">New concept</a>{chips}"#,
        mycelium_ui::button_class("primary")
    );
    // Table rows: the filter narrows the listing; each row's cells are
    // composed from escaped fragments (the link label) and trusted
    // component output (the type chip, the constant flag span).
    let rows: Vec<Vec<String>> = entries
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
            vec![title_link, type_chip, flag, html_escape(&e.updated_at)]
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
        mycelium_ui::render::render(mycelium_ui::page_header("Browse", actions)),
        flash(ok, err),
        content
    );
    layout("Browse", Some(user), csrf, "/", body)
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
) -> Html<String> {
    let is_new = path.is_empty();
    let title: &str = if is_new { "New concept" } else { path };
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
    let show_delete = editable && !is_new;
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
                r#"{scope_note}<button type="submit" class="btn btn--primary" form="concept-editor-form">Save</button>{delete_form}"#
            ),
            dialog,
            true,
        )
    } else if editable {
        // New concept: Save only — nothing is saved to delete yet.
        (
            format!(
                r#"{scope_note}<button type="submit" class="btn btn--primary" form="concept-editor-form">Save</button>"#
            ),
            String::new(),
            false,
        )
    } else {
        (scope_note.to_string(), String::new(), false)
    };
    let header = mycelium_ui::render::render(mycelium_ui::page_header(title, actions));
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
    let pane = if editable {
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
    let body = format!(
        "{breadcrumb}{header}{warning}\
<div class=\"editor-grid\">{pane}<div class=\"editor-preview\">{preview_html}</div></div>{dialog}"
    );
    let page_title = if is_new { "New concept" } else { "Concept" };
    let scripts: &[&str] = if confirm_script {
        &["/assets/confirm.js"]
    } else {
        &[]
    };
    layout_with_scripts(page_title, Some(user), csrf, "/concept", body, scripts)
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
            render_minimal_body(strip_frontmatter_block(markdown)),
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
/// naive scan mirroring the core splitter's delimiter rule; without a
/// closing `---` line nothing is stripped — the whole input previews.
fn strip_frontmatter_block(markdown: &str) -> &str {
    let Some(rest) = markdown.strip_prefix("---\n") else {
        return markdown;
    };
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return &rest[offset + line.len()..];
        }
        offset += line.len();
    }
    markdown
}

/// Search page.
pub fn search_page(
    user: &SessionUser,
    csrf: &str,
    query: &str,
    results: &[crate::api::ScopedResult],
) -> Html<String> {
    let rows = results
        .iter()
        .map(|r| {
            // Scope-aware links: user bundle and library/skills concepts
            // open in the right viewer.
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
                r#"<li><a href="{href}">{}</a> <span class="muted">({label}, score {:.1})</span><br><span class="muted">{}</span></li>"#,
                html_escape(&r.title),
                r.score,
                html_escape(&r.snippet)
            )
        })
        .collect::<String>();
    let body = format!(
        r#"<h1>Search</h1>
<form method="get" action="/search">
  <input name="q" value="{}" placeholder="Search your bundle and global shelves" autofocus>
  <button type="submit">Search</button>
</form>
<ul>{rows}</ul>"#,
        html_escape(query)
    );
    layout("Search", Some(user), csrf, "/search", body)
}

/// Graph page (loads graph.js for the force-directed visualization).
pub fn graph_page(user: &SessionUser, csrf: &str) -> Html<String> {
    let body = r#"<h1>Graph</h1>
<div id="graph-wrap">
  <div id="graph-info" hidden></div>
  <div id="graph"></div>
</div>
<div class="muted" id="graph-legend">drag nodes to rearrange · scroll to zoom · drag background to pan · click a node to open</div>"#
        .to_string();
    layout_full(
        "Graph",
        Some(user),
        csrf,
        "/graph",
        body,
        &["/assets/graph.js"],
        "graph-page",
    )
}

/// Skills page: private skills + global skills (read-only for users,
/// editable by admins). Nested skills (`/<slug>/skill.md` hubs) render
/// as one group: the hub row with companions and manifest script
/// labels indented beneath it; legacy root-level skills stay flat
/// rows exactly as before.
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
    // labels indented beneath it. Scripts are label-only rows — raw
    // payload files, not concepts, so they carry no link.
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
            r#"<li>{}<ul>{members}{labels}</ul></li>"#,
            entry_link(&g.hub, scope)
        )
    };
    // One section: grouped blocks (slug order), then flat rows.
    let list = |groups: &[mycelium_store::SkillGroup],
                scripts: &[Vec<String>],
                flat: &[mycelium_store::ConceptEntry],
                scope: &str| {
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
    let global_section = if user.role == Role::Admin {
        format!(
            r#"<h2>Global skills</h2>
<ul>{}</ul>
<p><a href="/concept?new=1&scope=skills">New global skill</a> (admin)</p>"#,
            list(global_groups, global_scripts, global_flat, "skills")
        )
    } else {
        format!(
            r#"<h2>Global skills</h2>
<ul>{}</ul>"#,
            list(global_groups, global_scripts, global_flat, "skills")
        )
    };
    let body = format!(
        r#"<h1>Skills</h1>
<h2>Your private skills</h2>
<ul>{}</ul>
<p><a href="/concept?new=1&scope=skills-private">New private skill</a></p>
{global_section}"#,
        list(private_groups, private_scripts, private_flat, "user"),
    );
    layout("Skills", Some(user), csrf, "/skills", body)
}

/// Books browse page: shelves with their books. Each book links to its
/// catalog hub (library scope concept viewer).
pub fn books_page(
    user: &SessionUser,
    csrf: &str,
    shelves: &[crate::api::ShelfBrowse],
) -> Html<String> {
    let sections = shelves
        .iter()
        .map(|(name, is_global, books)| {
            let visibility = if *is_global { "global-read" } else { "admin-private" };
            let rows = if books.is_empty() {
                r#"<p class="muted">No books yet.</p>"#.to_string()
            } else {
                let items = books
                    .iter()
                    .map(|(slug, title)| {
                        format!(
                            r#"<li><a href="/concept?path=/{slug}/book.md&scope=library">{}</a> <span class="muted">({slug})</span></li>"#,
                            html_escape(title)
                        )
                    })
                    .collect::<String>();
                format!("<ul>{items}</ul>")
            };
            format!(
                r#"<h2>{}</h2>
<p class="muted">{visibility}</p>
{rows}"#,
                html_escape(name)
            )
        })
        .collect::<String>();
    let body = format!(
        r#"<h1>Bookshelves</h1>
{sections}"#
    );
    layout("Books", Some(user), csrf, "/books", body)
}

/// Chat page: talk to the librarian agent (the same agent behind the
/// MCP tools). Full-viewport layout: the conversation log fills the
/// screen and scrolls; the input is pinned to the bottom.
pub fn chat_page(user: &SessionUser, csrf: &str) -> Html<String> {
    let body = r#"<div class="chat-shell">
  <div class="chat-intro">
    <h1>Librarian</h1>
    <p class="muted">Chat with the librarian agent over your private bundle. It searches, reads, and cites your concepts — and can record or change knowledge when you ask. Requires a reachable LLM backend (admin portal → LLM backend).</p>
  </div>
  <div id="chat-log" class="chat-log" aria-live="polite"></div>
  <form id="chat-form" class="chat-input-bar">
    <textarea id="chat-input" rows="1" placeholder="Ask about your knowledge base, or say 'record that ...' (Enter to send, Shift+Enter for a new line)" required></textarea>
    <button type="submit">Send</button>
  </form>
</div>"#
        .to_string();
    layout_full(
        "Librarian",
        Some(user),
        csrf,
        "/chat",
        body,
        &["/assets/chat.js"],
        "chat-page",
    )
}

/// Change-password page.
pub fn password_page(csrf: &str, ok: Option<&str>, err: Option<&str>) -> Html<String> {
    let body = format!(
        r#"<h1>Change password</h1>
{}
<form method="post" action="/password">
  <label>Current password</label><input type="password" name="old" required>
  <label>New password (min 20 chars)</label><input type="password" name="new" required minlength="20">
  <label>Repeat new password</label><input type="password" name="repeat" required minlength="20">
  <button type="submit">Change</button>
</form>"#,
        flash(ok, err)
    );
    layout("Password", None, csrf, "/password", body)
}

/// API keys page.
pub fn keys_page(
    user: &SessionUser,
    csrf: &str,
    keys: &[mycelium_auth::api_key::ApiKeyRecord],
    minted: Option<&str>,
) -> Html<String> {
    let rows = keys
        .iter()
        .map(|k| {
            let status = if k.revoked_at.is_some() { "revoked" } else { "active" };
            // The trigger is a plain submit inside its own row form:
            // without JS, clicking it POSTs /keys/revoke directly (the
            // server-rendered csrf_token is the security gate); with
            // JS, confirm.js's preventDefault opens the dialog instead.
            format!(
                r#"<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td>
<td><form method="post" action="/keys/revoke"><input type="hidden" name="csrf_token" value="{}"><input type="hidden" name="id" value="{}"><button type="submit" class="btn btn--danger" data-confirm-dialog="revoke" data-key-id="{}">Revoke</button></form></td></tr>"#,
                html_escape(&k.label),
                status,
                k.created_at.format("%Y-%m-%d"),
                k.last_used_at.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
                html_escape(csrf),
                k.id,
                k.id
            )
        })
        .collect::<String>();
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
    let minted_html = minted
        .map(|t| format!(r#"<div class="flash ok">New key (shown once): <code>{t}</code></div>"#))
        .unwrap_or_default();
    let body = format!(
        r#"<h1>API keys</h1>
{minted_html}
<table><tr><th>Label</th><th>Status</th><th>Created</th><th>Last used</th><th></th></tr>{rows}</table>
{dialog}
<h2>Mint a key</h2>
<form method="post" action="/keys">
  <label>Label</label><input name="label" required>
  <button type="submit">Mint</button>
</form>"#,
        rows = rows,
        dialog = dialog,
        minted_html = minted_html
    );
    layout_with_scripts(
        "API keys",
        Some(user),
        csrf,
        "/keys",
        body,
        &["/assets/confirm.js"],
    )
}

/// Admin portal.
#[allow(clippy::too_many_arguments)]
pub fn admin_page(
    user: &SessionUser,
    csrf: &str,
    users: &[(String, String, String, String)],
    oidc_configured: bool,
    llm_url: &str,
    llm_model: &str,
    llm_api_key_set: bool,
    max_book_mib: u64,
    security: &crate::state::SecurityConfig,
    bookshelves: &[(String, bool)],
    ingest_jobs: &[(String, String, String, String, String)],
    ok: Option<&str>,
    err: Option<&str>,
) -> Html<String> {
    let user_rows = users
        .iter()
        .map(|(u, email, role, provider)| {
            format!(
                r#"<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>"#,
                html_escape(u),
                html_escape(email),
                html_escape(role),
                html_escape(provider)
            )
        })
        .collect::<String>();
    let shelf_rows = bookshelves
        .iter()
        .map(|(name, global)| {
            format!(
                r#"<tr><td>{}</td><td>{}</td></tr>"#,
                html_escape(name),
                if *global { "global-read" } else { "private" }
            )
        })
        .collect::<String>();
    let shelf_options = bookshelves
        .iter()
        .map(|(name, _)| {
            format!(
                r#"<option value="{}">{}</option>"#,
                html_escape(name),
                html_escape(name)
            )
        })
        .collect::<String>();
    let job_rows = ingest_jobs
        .iter()
        .map(|(slug, status, detail, created, shelf)| {
            format!(
                r#"<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>"#,
                html_escape(slug),
                html_escape(status),
                html_escape(detail),
                html_escape(created),
                html_escape(shelf)
            )
        })
        .collect::<String>();
    let oidc_status = if oidc_configured {
        "configured"
    } else {
        "not configured"
    };
    // Status only — the key itself is never rendered back to the browser.
    let llm_api_key_status = if llm_api_key_set { "stored" } else { "not set" };
    let body = format!(
        r#"<h1>Admin</h1>
{}
<h2>Users</h2>
<table><tr><th>Username</th><th>Email</th><th>Role</th><th>Provider</th></tr>{user_rows}</table>
<h2>Create user</h2>
<form method="post" action="/admin/users">
  <label>Username</label><input name="username" required>
  <label>Email</label><input name="email" type="email" required>
  <label>Password (min {} chars)</label><input name="password" type="password" required minlength="{}">
  <label>Confirm password</label><input name="password_confirm" type="password" required minlength="{}">
  <label>Role</label><select name="role"><option value="user">user</option><option value="admin">admin</option></select>
  <button type="submit">Create</button>
</form>
<h2>OIDC SSO</h2>
<p class="muted">Status: {oidc_status}</p>
<form method="post" action="/admin/oidc">
  <label>Issuer URL</label><input name="issuer_url" placeholder="https://sso.example.com/realms/main">
  <label>Client ID</label><input name="client_id">
  <label>Client secret</label><input name="client_secret" type="password">
  <label>Redirect URI</label><input name="redirect_uri" placeholder="https://host/auth/oidc/callback">
  <button type="submit">Save OIDC config</button>
</form>
<h2>LLM backend</h2>
<p class="muted">API key: {llm_api_key_status}</p>
<form method="post" action="/admin/llm">
  <label>OpenAI-compatible base URL</label><input name="url" value="{}">
  <label>Model</label><input name="model" value="{}">
  <label>API key (sent as a Bearer header; leave blank to keep the current one)</label><input name="api_key" type="password" autocomplete="off">
  <button type="submit">Save LLM config</button>
</form>
<h2>Bookshelves</h2>
<table><tr><th>Name</th><th>Visibility</th></tr>{shelf_rows}</table>
<form method="post" action="/admin/bookshelves">
  <label>Name</label><input name="name" required>
  <label>Global read</label><select name="global"><option value="0">private</option><option value="1">global-read</option></select>
  <button type="submit">Create bookshelf</button>
</form>
<h2>Upload book</h2>
<p class="muted">Markdown (.md) up to {max_book_mib} MiB (adjustable below). The librarian catalogs it onto the bookshelf (LLM-assisted when the configured backend is reachable; heuristic otherwise).</p>
<form method="post" action="/api/v1/ingest" enctype="multipart/form-data">
  <label>Bookshelf</label><select name="bookshelf" required>{shelf_options}</select>
  <label>Slug</label><input name="slug" required pattern="[a-zA-Z0-9-]+" placeholder="my-book">
  <label>Title</label><input name="title" required>
  <label>Book file (.md)</label><input name="file" type="file" accept=".md,text/markdown" required>
  <button type="submit">Upload and ingest</button>
</form>
<h3>Upload limits</h3>
<form method="post" action="/admin/upload-limits">
  <label>Max book size (MiB, 1–255)</label><input name="max_book_mib" type="number" min="1" max="255" value="{max_book_mib}" required>
  <button type="submit">Save limit</button>
</form>
<h2>Security settings</h2>
<p class="muted">Tunable within guardrails — values outside the ranges below are clamped. Session TTL applies to new logins; existing sessions keep their expiry.</p>
<form method="post" action="/admin/security">
  <label>Session TTL (minutes, 15–10080; 720 = 12h)</label><input name="session_ttl_minutes" type="number" min="15" max="10080" value="{}" required>
  <label>Login failures before lockout (3–10)</label><input name="login_max_failures" type="number" min="3" max="10" value="{}" required>
  <label>Lockout backoff cap (seconds, 30–3600)</label><input name="login_lockout_seconds" type="number" min="30" max="3600" value="{}" required>
  <label>Minimum password length (12–128)</label><input name="min_password_length" type="number" min="12" max="128" value="{}" required>
  <label>Passage cap (chars, 16384–1048576; 131072 = 128k)</label><input name="passage_max_chars" type="number" min="16384" max="1048576" value="{}" required>
  <button type="submit">Save security settings</button>
</form>
<h2>Ingest jobs</h2>
<table><tr><th>Book</th><th>Status</th><th>Detail</th><th>Created</th><th>Shelf</th></tr>{job_rows}</table>
<h2>Global skills</h2>
<p class="muted">The global skills shelf is readable by all users; only admins can edit. Manage skills on the <a href="/skills">Skills page</a> (edit links appear there for admins).</p>
<h2>Maintenance</h2>
<form method="post" action="/admin/backup"><button type="submit">Download backup</button></form>"#,
        flash(ok, err),
        security.min_password_length,
        security.min_password_length,
        security.min_password_length,
        html_escape(llm_url),
        html_escape(llm_model),
        security.session_ttl_minutes,
        security.login_max_failures,
        security.login_lockout_seconds,
        security.min_password_length,
        security.passage_max_chars
    );
    layout("Admin", Some(user), csrf, "/admin", body)
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
        let html = concept_editor(&user, "", "/notes/x.md", hostile, preview, None, None, true).0;
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
}
