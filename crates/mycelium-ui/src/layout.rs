//! Layout and navigation components: Card, DataTable, StatTile,
//! Breadcrumb, TabBar, FormActions, NavGroup, NavItem. All components are
//! SSR-gated — they render HTML through the `crate::render` bridge.
//!
//! Escaping rule: plain-text slots (titles, labels, stat values) render
//! into `view!` TEXT interpolation positions, where leptos escapes them.
//! The rich-content slots — `data_table` cells and `card`'s body — are
//! TRUSTED-MARKUP slots: caller-composed markup built from escaped
//! fragments, interpolated raw via `inner_html` (the same contract as
//! the shell's page body — the caller owns escaping). Attribute values
//! are never built with `format!` on user input: classes come from
//! fixed strings, and hrefs/labels are caller-supplied primitives
//! passed through as typed attributes. Zero inline styles.
//!
//! `&str` props are cloned to owned at the boundary so the returned views
//! don't capture callers' lifetimes (the public signatures are
//! `impl IntoView` with no lifetime in bounds).
//!
//! Leptos 0.8's `view!` macro composes children as a fixed tuple of
//! homogeneous types, so a sequence whose element type varies (breadcrumb
//! separators + link/plain-span items, active vs. inactive tabs or nav
//! items) is built in plain Rust as a `Vec<AnyView>` — each item erased
//! via `IntoAny` — and passed as a single child. Same for the empty-slice
//! rules: an empty `nav_group` returns `()` (renders nothing) and an empty
//! `data_table` returns the primitive `empty_state` instead of the table;
//! both arms erase to `AnyView` so the return type unifies.

#[cfg(feature = "ssr")]
use crate::primitives::{button, empty_state};
#[cfg(feature = "ssr")]
use leptos::prelude::*;

/// `<section class="card"><div class="card__header"><h2>…</h2></div>
/// <div>{body}</div></section>`. Title-only this PR — the `card__header`
/// row keeps its flex space so the action slot drops in without a DOM
/// change. `body` is a trusted-markup slot: caller-composed markup from
/// escaped fragments, interpolated raw via `inner_html` (the caller owns
/// escaping — the shell's page-body contract).
#[cfg(feature = "ssr")]
pub fn card(title: &str, body: String) -> impl IntoView {
    let title = title.to_string();
    view! {
        <section class="card">
            <div class="card__header">
                <h2>{title}</h2>
            </div>
            <div inner_html=body/>
        </section>
    }
}

/// `<table class="table">` with a `<thead>` row and one `<tr>` per input
/// row; every `<td>` carries `data-label="{header}"` for the narrow
/// re-flow. Cells are trusted-markup slots: caller-composed markup from
/// escaped fragments, interpolated raw via `inner_html` (the caller owns
/// escaping — the same contract as `card`'s body). Empty `rows` renders
/// the primitive `empty_state` in the table slot instead.
#[cfg(feature = "ssr")]
pub fn data_table(headers: &[&str], rows: &[Vec<String>]) -> impl IntoView {
    if rows.is_empty() {
        return empty_state("No items", "Nothing here yet.").into_any();
    }
    let head_cells: Vec<_> = headers
        .iter()
        .map(|header| {
            let header = header.to_string();
            view! { <th>{header}</th> }
        })
        .collect();
    let body_rows: Vec<_> = rows
        .iter()
        .map(|row| {
            let cells: Vec<_> = row
                .iter()
                .enumerate()
                .map(|(col, cell)| {
                    let label = headers.get(col).copied().unwrap_or("").to_string();
                    view! { <td data-label={label} inner_html=cell.clone()/> }
                })
                .collect();
            view! { <tr>{cells}</tr> }
        })
        .collect();
    view! {
        <table class="table">
            <thead>
                <tr>{head_cells}</tr>
            </thead>
            <tbody>{body_rows}</tbody>
        </table>
    }
    .into_any()
}

/// `<div class="stat"><span class="stat__value">…</span>
/// <span class="stat__label">…</span></div>`.
#[cfg(feature = "ssr")]
pub fn stat_tile(label: &str, value: &str) -> impl IntoView {
    let label = label.to_string();
    let value = value.to_string();
    view! {
        <div class="stat">
            <span class="stat__value">{value}</span>
            <span class="stat__label">{label}</span>
        </div>
    }
}

/// `<nav class="breadcrumb">` of crumbs: `Some(href)` renders
/// `<a href>label</a>`, `None` renders a plain `<span>label</span>`;
/// crumbs are separated by `<span aria-hidden="true">/</span>`.
#[cfg(feature = "ssr")]
pub fn breadcrumb(parts: &[(&str, Option<&str>)]) -> impl IntoView {
    let crumbs: Vec<AnyView> = parts
        .iter()
        .enumerate()
        .flat_map(|(i, (label, href))| {
            let label = label.to_string();
            let sep = (i > 0).then(|| view! { <span aria-hidden="true">/</span> }.into_any());
            let item = match href {
                Some(href) => view! { <a href={href.to_string()}>{label}</a> }.into_any(),
                None => view! { <span>{label}</span> }.into_any(),
            };
            [sep, Some(item)].into_iter().flatten()
        })
        .collect();
    view! {
        <nav class="breadcrumb">{crumbs}</nav>
    }
}

/// A row of tab links: `(label, href, active)` per entry; the active tab
/// renders `class="tab tab--active"` with `aria-current="page"`.
#[cfg(feature = "ssr")]
pub fn tab_bar(tabs: &[(&str, &str, bool)]) -> impl IntoView {
    let tabs_: Vec<AnyView> = tabs
        .iter()
        .map(|(label, href, active)| {
            let label = label.to_string();
            let href = href.to_string();
            if *active {
                view! {
                    <a class="tab tab--active" href={href} aria-current="page">{label}</a>
                }
                .into_any()
            } else {
                view! {
                    <a class="tab" href={href}>{label}</a>
                }
                .into_any()
            }
        })
        .collect();
    view! {
        <nav>{tabs_}</nav>
    }
}

/// A form action row composed of the primitive [`button`]: each entry is
/// `(label, variant)` (variant per `button_class`), rendered in order.
#[cfg(feature = "ssr")]
pub fn form_actions(actions: &[(&str, &str)]) -> impl IntoView {
    actions
        .iter()
        .map(|(label, variant)| button(label, variant).into_any())
        .collect::<Vec<_>>()
}

/// `<div class="sidebar__group"><p class="sidebar__group-label">…</p>` +
/// one `nav_item` per entry: `(label, href, active)`. An empty `items`
/// slice renders nothing — no bare group heading.
#[cfg(feature = "ssr")]
pub fn nav_group(name: &str, items: &[(&str, &str, bool)]) -> impl IntoView {
    if items.is_empty() {
        return ().into_any();
    }
    let name = name.to_string();
    let items_: Vec<AnyView> = items
        .iter()
        .map(|(label, href, active)| nav_item(label, href, *active).into_any())
        .collect();
    view! {
        <div class="sidebar__group">
            <p class="sidebar__group-label">{name}</p>
            {items_}
        </div>
    }
    .into_any()
}

/// Inline 16x16 nav glyph (a sprout) — the first of the per-page glyphs;
/// distinct icons land with PR 2's pages.
#[cfg(feature = "ssr")]
const NAV_ICON_PATH: &str = "M8 14V8m0 0c0-3 2-5 5-5 0 3-2 5-5 5Zm0 0c0-3-2-5-5-5 0 3 2 5 5 5Z";

/// `<a class="nav-item{ nav-item--active}" href{ aria-current="page"}>`
/// with a leading inline `<svg>` glyph and a `<span>` label.
#[cfg(feature = "ssr")]
pub fn nav_item(label: &str, href: &str, active: bool) -> impl IntoView {
    let label = label.to_string();
    let href = href.to_string();
    let icon = view! {
        <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
            <path
                d={NAV_ICON_PATH}
                fill="none"
                stroke="currentColor"
                stroke-width="2"
                stroke-linecap="round"
                stroke-linejoin="round"
            />
        </svg>
    };
    if active {
        view! {
            <a class="nav-item nav-item--active" href={href} aria-current="page">
                {icon}
                <span>{label}</span>
            </a>
        }
        .into_any()
    } else {
        view! {
            <a class="nav-item" href={href}>
                {icon}
                <span>{label}</span>
            </a>
        }
        .into_any()
    }
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::*;
    use crate::render::render;

    #[test]
    fn nav_escapes_hostile_labels() {
        let items = [("Hostile <script>", "/", false)];
        let html = render(nav_group("Group", &items));
        assert!(html.contains("Hostile &lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn empty_nav_group_renders_nothing() {
        let html = render(nav_group("Group", &[]));
        assert!(!html.contains("sidebar__group-label"), "{html}");
        assert!(html.is_empty() || !html.contains("Group"), "{html}");
    }

    #[test]
    fn data_table_carries_data_label_and_trusted_markup_cells() {
        // Cells are trusted caller-composed markup (the raw-slot
        // contract): markup passes through unescaped, so callers escape
        // user text before composing a cell. The end-to-end escaping
        // posture is pinned by mycelium-web's XSS integration test
        // (a hostile concept title renders escaped through the full
        // Browse page).
        let headers = ["Name", "Type"];
        let rows = vec![vec![
            // A pre-escaped fragment (how callers pass user text):
            // stays escaped — no double-escaping, no raw script.
            "Title &lt;script&gt;".into(),
            // Trusted markup (chip/flag/link cells): passes through raw.
            r#"<span class="chip chip--neutral">Note</span>"#.into(),
        ]];
        let html = render(data_table(&headers, &rows));
        assert!(html.contains(r#"data-label="Name""#), "{html}");
        assert!(
            html.contains("Title &lt;script&gt;"),
            "escaped fragments stay escaped: {html}"
        );
        assert!(!html.contains("&amp;lt;"), "no double-escaping: {html}");
        assert!(
            html.contains(r#"<span class="chip chip--neutral">Note</span>"#),
            "markup passes through raw: {html}"
        );
    }

    #[test]
    fn card_body_is_a_trusted_markup_slot() {
        let html = render(card("Section", r#"<p>a <em>body</em></p>"#.to_string()));
        assert!(html.contains("card__header"), "{html}");
        assert!(
            html.contains("<em>body</em>"),
            "body markup must stay raw: {html}"
        );
        assert!(!html.contains("&lt;em&gt;"), "{html}");
    }

    #[test]
    fn empty_table_renders_empty_state() {
        let headers = ["Name"];
        let rows: Vec<Vec<String>> = vec![];
        let html = render(data_table(&headers, &rows));
        assert!(html.contains("empty"), "{html}");
    }

    #[test]
    fn tab_bar_marks_active() {
        let tabs = [
            ("Users", "/admin/users", true),
            ("System", "/admin/system", false),
        ];
        let html = render(tab_bar(&tabs));
        assert!(html.contains("aria-current=\"page\""), "{html}");
        assert!(html.contains("tab--active"), "{html}");
    }
}
