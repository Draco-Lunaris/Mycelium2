//! App shell: the full-document wrapper every shelled page renders
//! through — head tokens (csrf meta, title, stylesheet) composed with
//! `format!`, a Leptos-rendered sidebar fragment (brand lockup, grouped
//! nav, user chip + logout), and the page body landing inside `<main>`.
//! Plus the outside-shell auth document (login/setup) and the nav-group
//! data driving the sidebar.
//!
//! Escaping rule: the `format!`-composed head attributes (csrf token,
//! title) go through the local [`escape`] helper — mycelium-ui cannot
//! depend on mycelium-web's `html_escape`. Everything rendered through
//! Leptos (nav labels, group names, the user name) goes into `view!`
//! TEXT positions where leptos escapes it. The page body is
//! server-composed trusted markup and is interpolated raw by design
//! (same contract as the old `layout_full`). `scripts` entries and
//! `body_class` are caller-supplied constants, never user input.

#[cfg(feature = "ssr")]
use crate::layout::nav_group;
#[cfg(feature = "ssr")]
use crate::render::render;
#[cfg(feature = "ssr")]
use crate::trusted::TrustedHtml;
#[cfg(feature = "ssr")]
use leptos::prelude::*;

/// The default assets' content version — mycelium-ui's own mirror of
/// mycelium-web's `assets::ASSETS_VERSION` (the shell references the
/// assets it emits). The two bump in lockstep, pinned by mycelium-web's
/// `assets_versions_lockstep` unit test (only that crate sees both
/// constants), so a one-sided bump fails CI.
#[cfg(feature = "ssr")]
pub const ASSETS_VERSION: &str = "29";

/// Minimal HTML escaping for the `format!`-composed head attributes
/// (csrf token, title) — the html-escape equivalent mycelium-ui carries
/// for itself. Text positions rendered through Leptos are escaped by
/// leptos; this helper is used ONLY for those head attributes.
#[cfg(feature = "ssr")]
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Sidebar groups (spec §6): Knowledge, Librarian (Chat for everyone;
/// Status admin-only), Account, and the admin-only Admin group.
/// `active` is derived per item by comparing its href against
/// `active_path` (matching `nav_group`'s item shape). `None` user → no
/// nav (login/setup render through `auth_shell` instead); a non-admin
/// → no Admin group and no Status item (spec §6: the operational page
/// shows cross-user jobs). The Account group (API keys + Account
/// security, PR 4) renders for every logged-in user — account
/// security is per-user, never admin-gated (spec §6).
#[cfg(feature = "ssr")]
// The return shape is pinned by the brief verbatim — (group name,
// [(label, href, active)]) feeding Task 5's `nav_group` slice — so the
// lint is scoped here instead of hidden behind a type alias.
#[allow(clippy::type_complexity)]
pub fn nav_groups(
    user: Option<(&str, bool)>,
    active_path: &str,
) -> Vec<(&'static str, Vec<(&'static str, &'static str, bool)>)> {
    if user.is_none() {
        return Vec::new();
    }
    let item = |label: &'static str, href: &'static str| (label, href, href == active_path);
    let is_admin = matches!(user, Some((_, true)));
    // Librarian: Chat for everyone; Status admin-only (spec §6 — the
    // operational page shows cross-user jobs).
    let librarian = if is_admin {
        vec![item("Chat", "/chat"), item("Status", "/status")]
    } else {
        vec![item("Chat", "/chat")]
    };
    let mut groups = vec![
        (
            "Knowledge",
            vec![
                item("Browse", "/"),
                item("Search", "/search"),
                item("Graph", "/graph"),
                item("Skills", "/skills"),
                item("Books", "/books"),
            ],
        ),
        ("Librarian", librarian),
        (
            "Account",
            vec![
                item("API keys", "/keys"),
                item("Account security", "/account"),
            ],
        ),
    ];
    if is_admin {
        groups.push((
            "Admin",
            vec![
                item("Users", "/admin/users"),
                item("Sign-in", "/admin/sign-in"),
                item("LLM backend", "/admin/llm"),
                item("Library", "/admin/library"),
                item("System", "/admin/system"),
            ],
        ));
    }
    groups
}

/// Avatar circle (the user's initial) + name — the sidebar identity
/// chip. The sibling logout link lives in the shell's sidebar footer
/// beside this chip.
#[cfg(feature = "ssr")]
pub fn user_chip(name: &str) -> impl IntoView {
    let name = name.to_string();
    let initial = name
        .chars()
        .next()
        .and_then(|c| c.to_uppercase().next())
        .unwrap_or('?')
        .to_string();
    view! {
        <div class="user-chip">
            <span class="user-chip__avatar" aria-hidden="true">{initial}</span>
            <span class="user-chip__name">{name}</span>
        </div>
    }
}

/// The mint-mark path (a sprout — the nav glyph's shape at brand size).
#[cfg(feature = "ssr")]
const BRAND_PATH: &str = "M8 14V8m0 0c0-3 2-5 5-5 0 3-2 5-5 5Zm0 0c0-3-2-5-5-5 0 3 2 5 5 5Z";

/// The sign-out glyph (arrow leaving a door frame) for the logout link.
#[cfg(feature = "ssr")]
const LOGOUT_PATH: &str = "M6 8h8m0 0-3-3m3 3-3 3M9 3H4v10h5";

/// Brand lockup: inline mint-mark svg + "Mycelium" wordmark, linking
/// home. Shared by the sidebar (top) and the auth card (card top).
#[cfg(feature = "ssr")]
fn brand_lockup() -> impl IntoView {
    view! {
        <a class="brand" href="/">
            <svg viewBox="0 0 16 16" width="20" height="20" aria-hidden="true">
                <path
                    d={BRAND_PATH}
                    fill="none"
                    stroke="currentColor"
                    stroke-width="1.5"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                />
            </svg>
            <span>Mycelium</span>
        </a>
    }
}

/// The sidebar fragment: brand lockup, grouped nav, and the user chip +
/// logout footer pinned to the bottom. Rendered only for signed-in
/// users; the logout link carries an icon so the narrow icon-rail
/// re-flow (labels hidden) keeps it reachable.
#[cfg(feature = "ssr")]
fn sidebar(name: &str, user: Option<(&str, bool)>, active_path: &str) -> impl IntoView {
    let groups = nav_groups(user, active_path);
    let group_views: Vec<AnyView> = groups
        .iter()
        .map(|(group_name, items)| nav_group(group_name, items).into_any())
        .collect();
    view! {
        <aside class="sidebar">
            {brand_lockup()}
            {group_views}
            <footer class="sidebar__footer">
                {user_chip(name)}
                <a class="sidebar__logout" href="/logout" title="Log out">
                    <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
                        <path
                            d={LOGOUT_PATH}
                            fill="none"
                            stroke="currentColor"
                            stroke-width="2"
                            stroke-linecap="round"
                            stroke-linejoin="round"
                        />
                    </svg>
                    <span>Log out</span>
                </a>
            </footer>
        </aside>
    }
}

/// `<header class="page-header"><h1>{title}</h1><div class="actions">…</div>
/// </header>` — the page title row: title left, actions right (wraps to a
/// top bar under 720px). The class names match the `.page-header` block
/// in mycelium-web's assets stylesheet (the actions container is
/// `.actions`, not a BEM modifier). The title goes into a `view!` TEXT
/// position, where leptos escapes it; `actions` is a trusted-markup
/// slot typed `TrustedHtml` — caller-composed markup from escaped
/// fragments, interpolated raw via `inner_html` (the caller owns
/// escaping, same contract as `card`'s body and the shell's page body;
/// a plain `String` here does not compile).
#[cfg(feature = "ssr")]
pub fn page_header(title: &str, actions: TrustedHtml) -> impl IntoView {
    let title = title.to_string();
    let actions = actions.as_str().to_string();
    view! {
        <header class="page-header">
            <h1>{title}</h1>
            <div class="actions" inner_html=actions/>
        </header>
    }
}

/// The full HTML document for a shelled page: head tokens (csrf meta,
/// title, stylesheet), then the body — sidebar shell (brand lockup,
/// grouped nav, user chip + logout; no sidebar at all without a user),
/// the page body inside `<main>`, and the app.js + extra script tags at
/// body end: classic scripts first (`scripts`), then module scripts
/// (`module_scripts` — the hydrate bundle; `type="module"` defers their
/// execution until after the classic scripts, and the standing-guards
/// scan accepts either attribute order). A signed-in page's body class
/// is prefixed with `shelled` (the flex-row layout hook); `None` user
/// renders no nav — auth pages use `auth_shell`.
#[cfg(feature = "ssr")]
#[allow(clippy::too_many_arguments)]
pub fn shell(
    title: &str,
    user: Option<(&str, bool)>,
    csrf: &str,
    active_path: &str,
    body: String,
    scripts: &[&str],
    module_scripts: &[&str],
    body_class: &str,
) -> String {
    let sidebar_html = match user {
        Some((name, _)) => render(sidebar(name, user, active_path)),
        None => String::new(),
    };
    let body_class = match (user.is_some(), body_class.is_empty()) {
        (true, true) => "shelled".to_string(),
        (true, false) => format!("shelled {body_class}"),
        (false, _) => body_class.to_string(),
    };
    let extra_scripts = scripts
        .iter()
        .map(|s| format!(r#"<script src="{s}?v={ASSETS_VERSION}"></script>"#))
        .collect::<String>();
    // Module scripts (the hydrate bundle): `type="module"` first in
    // the tag, `src` second — the guards scan accepts either order,
    // and module deferral keeps them executing after the classic
    // scripts above.
    let module_script_tags = module_scripts
        .iter()
        .map(|s| format!(r#"<script type="module" src="{s}?v={ASSETS_VERSION}"></script>"#))
        .collect::<String>();
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="csrf-token" content="{}">
<title>{} — Mycelium2</title>
<link rel="stylesheet" href="/assets/style.css?v={version}">
</head>
<body class="{}">
{}
<main>{}</main>
<script src="/assets/app.js?v={version}"></script>{extra_scripts}{module_script_tags}
</body>
</html>"#,
        escape(csrf),
        escape(title),
        body_class,
        sidebar_html,
        body,
        version = ASSETS_VERSION,
    )
}

/// The outside-shell document for `/login` and `/setup`: a centered
/// AuthCard with the brand lockup at card top, the same head tokens
/// (no nav, no user chip). The csrf meta is emitted empty — no session
/// exists on these pages (the middleware's no-session exemption covers
/// their POSTs; app.js's injected empty field is ignored), exactly as
/// the old layout rendered them.
#[cfg(feature = "ssr")]
pub fn auth_shell(title: &str, body: String) -> String {
    let brand = render(brand_lockup());
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="csrf-token" content="">
<title>{} — Mycelium2</title>
<link rel="stylesheet" href="/assets/style.css?v={version}">
</head>
<body class="auth">
<main><div class="card auth-card">{brand}{}</div></main>
<script src="/assets/app.js?v={version}"></script>
</body>
</html>"#,
        escape(title),
        body,
        brand = brand,
        version = ASSETS_VERSION,
    )
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::*;
    use crate::render::render;

    #[test]
    fn shell_renders_groups_and_active_state() {
        let user = Some(("echo", true));
        let html = shell("Browse", user, "tok", "/", String::new(), &[], &[], "");
        assert!(html.contains("Knowledge"), "{html}");
        assert!(html.contains("Librarian"), "{html}");
        assert!(html.contains("Account"), "{html}");
        assert!(html.contains("Admin"), "{html}");
        assert!(html.contains(r#"aria-current="page""#), "{html}");
        assert!(
            html.contains(r#"<main></main>"#),
            "body lands in main: {html}"
        );
    }

    #[test]
    fn admin_group_is_the_five_spec_items_with_per_page_active() {
        // Spec §6: the Admin group is five items, not the PR-1-era
        // single link — exact (label, href) list, in order.
        let groups = nav_groups(Some(("echo", true)), "/");
        let admin = &groups
            .iter()
            .find(|(name, _)| *name == "Admin")
            .expect("admin group for an admin")
            .1;
        let got: Vec<(&str, &str)> = admin
            .iter()
            .map(|(label, href, _)| (*label, *href))
            .collect();
        assert_eq!(
            got,
            vec![
                ("Users", "/admin/users"),
                ("Sign-in", "/admin/sign-in"),
                ("LLM backend", "/admin/llm"),
                ("Library", "/admin/library"),
                ("System", "/admin/system"),
            ]
        );
        // Active is derived per item by href == active_path: nothing is
        // active off the admin routes, exactly one item on one of them.
        assert!(admin.iter().all(|(_, _, active)| !*active));
        let on_system = nav_groups(Some(("echo", true)), "/admin/system")
            .into_iter()
            .find(|(name, _)| *name == "Admin")
            .expect("admin group")
            .1;
        let active: Vec<&str> = on_system
            .iter()
            .filter(|(_, _, active)| *active)
            .map(|(_, href, _)| *href)
            .collect();
        assert_eq!(active, vec!["/admin/system"]);

        // Rendered both ways: the active item carries `aria-current="page"`
        // + the active class, a sibling carries neither. (Leptos emits
        // `href`, then `aria-current`, then `class`.)
        let html = shell(
            "Users",
            Some(("echo", true)),
            "tok",
            "/admin/users",
            String::new(),
            &[],
            &[],
            "",
        );
        assert!(
            html.contains(
                r#"<a href="/admin/users" aria-current="page" class="nav-item nav-item--active">"#
            ),
            "Users item must be active on /admin/users: {html}"
        );
        assert!(
            html.contains(r#"<a href="/admin/system" class="nav-item">"#),
            "System item must render plain (no aria-current, no active class): {html}"
        );
    }

    #[test]
    fn librarian_group_status_is_admin_gated() {
        // Spec §6: the Librarian group carries Chat for everyone and
        // Status for admins only (the operational page shows
        // cross-user jobs) — exact (label, href) lists per role, and
        // the per-item active derivation holds for the new item.
        let admin_groups = nav_groups(Some(("echo", true)), "/");
        let librarian = &admin_groups
            .iter()
            .find(|(name, _)| *name == "Librarian")
            .expect("librarian group for an admin")
            .1;
        let got: Vec<(&str, &str)> = librarian
            .iter()
            .map(|(label, href, _)| (*label, *href))
            .collect();
        assert_eq!(got, vec![("Chat", "/chat"), ("Status", "/status")]);
        let on_status = nav_groups(Some(("echo", true)), "/status")
            .into_iter()
            .find(|(name, _)| *name == "Librarian")
            .expect("librarian group")
            .1;
        let active: Vec<&str> = on_status
            .iter()
            .filter(|(_, _, active)| *active)
            .map(|(_, href, _)| *href)
            .collect();
        assert_eq!(active, vec!["/status"]);
        // Non-admin: Chat only — no Status item renders.
        let user_groups = nav_groups(Some(("echo", false)), "/");
        let librarian = &user_groups
            .iter()
            .find(|(name, _)| *name == "Librarian")
            .expect("librarian group for a user")
            .1;
        let got: Vec<(&str, &str)> = librarian
            .iter()
            .map(|(label, href, _)| (*label, *href))
            .collect();
        assert_eq!(got, vec![("Chat", "/chat")]);
    }

    #[test]
    fn account_group_carries_security_for_every_user() {
        // Spec §6: the Account group is API keys + Account security,
        // NOT admin-gated — every logged-in user manages their own
        // account security. Exact (label, href) lists per role, and
        // the per-item active derivation holds on /account (PR 4,
        // Task 3).
        for is_admin in [true, false] {
            let groups = nav_groups(Some(("echo", is_admin)), "/");
            let account = &groups
                .iter()
                .find(|(name, _)| *name == "Account")
                .expect("account group")
                .1;
            let got: Vec<(&str, &str)> = account
                .iter()
                .map(|(label, href, _)| (*label, *href))
                .collect();
            assert_eq!(
                got,
                vec![("API keys", "/keys"), ("Account security", "/account")],
                "is_admin = {is_admin}"
            );
        }
        // Active is derived per item by href == active_path: on
        // /account the security item carries the active flag.
        let on_account = nav_groups(Some(("echo", false)), "/account")
            .into_iter()
            .find(|(name, _)| *name == "Account")
            .expect("account group")
            .1;
        let active: Vec<&str> = on_account
            .iter()
            .filter(|(_, _, active)| *active)
            .map(|(_, href, _)| *href)
            .collect();
        assert_eq!(active, vec!["/account"]);
    }

    #[test]
    fn shell_hides_admin_group_for_non_admin() {
        let user = Some(("echo", false));
        let html = shell("Browse", user, "tok", "/", String::new(), &[], &[], "");
        assert!(!html.contains("Admin"), "{html}");
    }

    #[test]
    fn shell_no_user_renders_no_nav() {
        let html = shell("Browse", None, "tok", "/", String::new(), &[], &[], "");
        assert!(!html.contains("sidebar__group-label"), "{html}");
    }

    #[test]
    fn auth_shell_is_centered_card() {
        let html = auth_shell("Sign in", String::from("<form></form>"));
        assert!(html.contains("auth"), "{html}");
        assert!(!html.contains("nav-item"), "{html}");
    }

    #[test]
    fn page_header_renders_title_and_actions_slot() {
        let html = render(page_header("Browse", TrustedHtml::from_markup("")));
        assert!(html.contains("page-header"), "{html}");
        assert!(html.contains("Browse"), "{html}");
        assert!(!html.contains("style="), "{html}");
        // The actions slot is a trusted-markup slot: composed markup
        // passes through raw (the caller escapes user text first).
        let actions = r#"<a class="btn btn--primary" href="/concept?new=1">New concept</a>"#;
        let html = render(page_header("Browse", TrustedHtml::from_markup(actions)));
        assert!(
            html.contains(actions),
            "actions markup must stay raw: {html}"
        );
    }

    #[test]
    fn shell_renders_full_document_with_unique_ids() {
        // Review Focus 1: two fields with distinct ids — both label/input
        // pairs must resolve, and no id may repeat within the document.
        let body = format!(
            "{}{}",
            render(crate::primitives::field("f1", "A", "text", "", "")),
            render(crate::primitives::field("f2", "B", "text", "", ""))
        );
        let html = shell(
            "Browse",
            Some(("echo", false)),
            "tok",
            "/",
            body,
            &[],
            &[],
            "",
        );
        assert!(html.matches("id=\"f1\"").count() == 1, "{html}");
        assert!(html.matches("id=\"f2\"").count() == 1, "{html}");
        assert!(html.matches("for=\"f1\"").count() == 1, "{html}");
    }
}
