//! Server-rendered pages (Leptos SSR, no hydration needed for the
//! form-driven UI; the graph page uses graph.js).

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use mycelium_auth::rbac::SessionUser;

use crate::middleware::SessionId;

/// Common page chrome. Every page is server-rendered; app.js (CSRF header
/// injection) loads on every page, graph.js only on the graph page.
pub fn layout(title: &str, user: Option<&SessionUser>, csrf: &str, body: String) -> Html<String> {
    layout_with_scripts(title, user, csrf, body, &[])
}

/// Layout with extra script sources (e.g. graph.js).
pub fn layout_with_scripts(
    title: &str,
    user: Option<&SessionUser>,
    csrf: &str,
    body: String,
    extra_scripts: &[&str],
) -> Html<String> {
    let nav = match user {
        Some(u) => format!(
            r#"<nav>
                <a href="/">Home</a>
                <a href="/search">Search</a>
                <a href="/graph">Graph</a>
                <a href="/skills">Skills</a>
                <a href="/keys">API Keys</a>
                <a href="/password">Password</a>
                {}
                <a href="/logout">Logout</a>
            </nav>"#,
            if u.role == mycelium_auth::rbac::Role::Admin {
                r#"<a href="/admin">Admin</a>"#
            } else {
                ""
            }
        ),
        None => r#"<nav><a href="/login">Login</a></nav>"#.to_string(),
    };
    let scripts = extra_scripts
        .iter()
        .map(|s| format!(r#"<script src="{s}"></script>"#))
        .collect::<String>();
    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="csrf-token" content="{}">
<title>{} — Mycelium2</title>
<link rel="stylesheet" href="/assets/style.css">
</head>
<body>
<header><a href="/">Mycelium2</a>{}</header>
<main>{}</main>
<script src="/assets/app.js"></script>{scripts}
</body>
</html>"#,
        html_escape(csrf),
        html_escape(title),
        nav,
        body
    );
    Html(html)
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

/// The login page.
pub fn login_page(csrf: &str, error: Option<&str>) -> Html<String> {
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
    layout("Login", None, csrf, body)
}

/// Home: the user's private bundle listing.
pub fn home_page(
    user: &SessionUser,
    csrf: &str,
    entries: &[mycelium_store::ConceptEntry],
    ok: Option<&str>,
    err: Option<&str>,
) -> Html<String> {
    let rows = entries
        .iter()
        .map(|e| {
            format!(
                r#"<tr><td><a href="/concept?path={}">{}</a></td><td>{}</td><td>{}</td></tr>"#,
                urlencoding_encode(&e.path),
                html_escape(&e.title),
                html_escape(&e.concept_type),
                html_escape(&e.updated_at)
            )
        })
        .collect::<String>();
    let body = format!(
        r#"<h1>Your bundle</h1>
{}
<p class="muted">{} concepts. <a href="/concept?new=1">New concept</a></p>
<table><tr><th>Title</th><th>Type</th><th>Updated</th></tr>{rows}</table>"#,
        flash(ok, err),
        entries.len()
    );
    layout("Home", Some(user), csrf, body)
}

/// Concept view/edit page (plain textarea with the raw markdown).
pub fn concept_page(
    user: &SessionUser,
    csrf: &str,
    path: &str,
    markdown: &str,
    ok: Option<&str>,
    err: Option<&str>,
) -> Html<String> {
    let body = format!(
        r#"<h1>{}</h1>
{}
<form method="post" action="/concept">
  <input type="hidden" name="path" value="{}">
  <label>Markdown (frontmatter + body)</label>
  <textarea name="markdown" spellcheck="false">{}</textarea>
  <button type="submit">Save</button>
  <button type="submit" name="delete" value="1" class="danger" formaction="/concept/delete">Delete</button>
</form>"#,
        html_escape(path),
        flash(ok, err),
        html_escape(path),
        textarea_escape(markdown)
    );
    layout("Concept", Some(user), csrf, body)
}

/// New-concept page.
pub fn new_concept_page(user: &SessionUser, csrf: &str, err: Option<&str>) -> Html<String> {
    let template = "---\ntype: Note\ntitle: New concept\ndescription: \ntags: []\n---\n\n";
    let body = format!(
        r#"<h1>New concept</h1>
{}
<form method="post" action="/concept">
  <label>Path (e.g. /notes/my-note.md)</label>
  <input name="path" required pattern="/.*\.md" placeholder="/notes/my-note.md">
  <label>Markdown</label>
  <textarea name="markdown" spellcheck="false">{template}</textarea>
  <button type="submit">Create</button>
</form>"#,
        flash(None, err)
    );
    layout("New concept", Some(user), csrf, body)
}

/// Search page.
pub fn search_page(
    user: &SessionUser,
    csrf: &str,
    query: &str,
    results: &[mycelium_core::search::SearchResult],
) -> Html<String> {
    let rows = results
        .iter()
        .map(|r| {
            format!(
                r#"<li><a href="/concept?path={}">{}</a> <span class="muted">(score {:.1})</span><br><span class="muted">{}</span></li>"#,
                urlencoding_encode(&r.concept_path),
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
    layout("Search", Some(user), csrf, body)
}

/// Graph page (loads graph.js for the force-directed visualization).
pub fn graph_page(user: &SessionUser, csrf: &str) -> Html<String> {
    let body = r#"<h1>Graph</h1><div id="graph"></div>"#.to_string();
    layout_with_scripts("Graph", Some(user), csrf, body, &["/assets/graph.js"])
}

/// Skills page: private skills + global skills (read-only for users).
pub fn skills_page(
    user: &SessionUser,
    csrf: &str,
    private: &[mycelium_store::ConceptEntry],
    global: &[mycelium_store::ConceptEntry],
) -> Html<String> {
    let list = |entries: &[mycelium_store::ConceptEntry]| {
        entries
            .iter()
            .map(|e| {
                format!(
                    r#"<li><a href="/concept?path={}&scope=skills">{}</a></li>"#,
                    urlencoding_encode(&e.path),
                    html_escape(&e.title)
                )
            })
            .collect::<String>()
    };
    let body = format!(
        r#"<h1>Skills</h1>
<h2>Your private skills</h2>
<ul>{}</ul>
<p><a href="/concept?new=1&scope=skills">New private skill</a></p>
<h2>Global skills</h2>
<ul>{}</ul>"#,
        list(private),
        list(global)
    );
    layout("Skills", Some(user), csrf, body)
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
    layout("Password", None, csrf, body)
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
            format!(
                r#"<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td>
<td><form method="post" action="/keys/revoke"><input type="hidden" name="id" value="{}"><button class="danger">Revoke</button></form></td></tr>"#,
                html_escape(&k.label),
                status,
                k.created_at.format("%Y-%m-%d"),
                k.last_used_at.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
                k.id
            )
        })
        .collect::<String>();
    let minted_html = minted
        .map(|t| format!(r#"<div class="flash ok">New key (shown once): <code>{t}</code></div>"#))
        .unwrap_or_default();
    let body = format!(
        r#"<h1>API keys</h1>
{minted_html}
<table><tr><th>Label</th><th>Status</th><th>Created</th><th>Last used</th><th></th></tr>{rows}</table>
<h2>Mint a key</h2>
<form method="post" action="/keys">
  <label>Label</label><input name="label" required>
  <button type="submit">Mint</button>
</form>"#,
        rows = rows,
        minted_html = minted_html
    );
    layout("API keys", Some(user), csrf, body)
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
    let body = format!(
        r#"<h1>Admin</h1>
{}
<h2>Users</h2>
<table><tr><th>Username</th><th>Email</th><th>Role</th><th>Provider</th></tr>{user_rows}</table>
<h2>Create user</h2>
<form method="post" action="/admin/users">
  <label>Username</label><input name="username" required>
  <label>Email</label><input name="email" type="email" required>
  <label>Password (min 20 chars)</label><input name="password" type="password" required minlength="20">
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
<form method="post" action="/admin/llm">
  <label>OpenAI-compatible base URL</label><input name="url" value="{}">
  <label>Model</label><input name="model" value="{}">
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
<p class="muted">Markdown (.md) up to 32 MiB. The librarian catalogs it onto the bookshelf (LLM-assisted when the configured backend is reachable; heuristic otherwise).</p>
<form method="post" action="/api/v1/ingest" enctype="multipart/form-data">
  <label>Bookshelf</label><select name="bookshelf" required>{shelf_options}</select>
  <label>Slug</label><input name="slug" required pattern="[a-zA-Z0-9-]+" placeholder="my-book">
  <label>Title</label><input name="title" required>
  <label>Book file (.md)</label><input name="file" type="file" accept=".md,text/markdown" required>
  <button type="submit">Upload and ingest</button>
</form>
<h2>Ingest jobs</h2>
<table><tr><th>Book</th><th>Status</th><th>Detail</th><th>Created</th><th>Shelf</th></tr>{job_rows}</table>
<h2>Maintenance</h2>
<form method="post" action="/admin/backup"><button type="submit">Download backup</button></form>"#,
        flash(ok, err),
        html_escape(llm_url),
        html_escape(llm_model)
    );
    layout("Admin", Some(user), csrf, body)
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
