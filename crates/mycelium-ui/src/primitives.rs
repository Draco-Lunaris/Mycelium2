//! Primitive components: Button, Chip, Banner, EmptyState, Field,
//! ConfirmDialog. All components are SSR-gated — they render HTML through
//! the `crate::render` bridge; the variant-class helper is pinned here so
//! both builds have a stable surface.
//!
//! Escaping rule: user text goes into `view!` TEXT interpolation positions
//! only (`{label}`, `{message}`) — leptos escapes those automatically.
//! Attribute values are never built with `format!` on user input: classes
//! come from fixed-string matches, and ids/types are caller-supplied
//! primitives passed through as typed attributes.
//!
//! `&str` props are cloned to owned at the boundary so the returned views
//! don't capture callers' lifetimes (the public signatures are
//! `impl IntoView` with no lifetime in bounds).
//!
//! Conditional children (hint/error paragraphs) are computed in plain
//! Rust as `Option<…>` values and passed as single children — leptos
//! renders `None` as nothing — instead of inline if-expressions inside
//! `view!`.

#[cfg(feature = "ssr")]
use leptos::prelude::*;

#[cfg(feature = "ssr")]
pub fn button_class(variant: &str) -> String {
    match variant {
        "secondary" => "btn btn--secondary".into(),
        "danger" => "btn btn--danger".into(),
        "ghost" => "btn btn--ghost".into(),
        _ => "btn btn--primary".into(),
    }
}

/// `<button class="btn btn--{variant}">{label}</button>`. The variant is
/// matched to a fixed class via [`button_class`] (token classes only —
/// no inline styles); the label renders into a text position, where
/// leptos escapes it.
#[cfg(feature = "ssr")]
pub fn button(label: &str, variant: &str) -> impl IntoView {
    let label = label.to_string();
    view! {
        <button class={button_class(variant)}>{label}</button>
    }
}

/// `<span class="chip chip--{class}">{text}</span>` — classes
/// `accent|neutral|danger|warning`; anything else renders as neutral.
#[cfg(feature = "ssr")]
pub fn chip(text: &str, class: &str) -> impl IntoView {
    let text = text.to_string();
    let cls = match class {
        "accent" => "chip chip--accent",
        "danger" => "chip chip--danger",
        "warning" => "chip chip--warning",
        _ => "chip chip--neutral",
    };
    view! {
        <span class={cls}>{text}</span>
    }
}

/// Banner icon background shapes (16x16 viewBox).
#[cfg(feature = "ssr")]
const BANNER_BG_CIRCLE: &str = "M1 8a7 7 0 1 0 14 0a7 7 0 1 0-14 0";
#[cfg(feature = "ssr")]
const BANNER_BG_TRIANGLE: &str = "M8 2 15 14H1z";

/// `<div class="banner banner--{kind}" role="{status|alert}">` with a
/// leading inline SVG glyph and a message paragraph. Kinds
/// `info|success|warning|danger` (anything else renders as info);
/// info and success carry `role="status"`, warning and danger carry
/// `role="alert"`. Every kind renders the same element tree — background
/// shape + stroke glyph + optional dot — with per-kind path data, so no
/// conditional branches are needed inside `view!`.
#[cfg(feature = "ssr")]
pub fn banner(kind: &str, message: &str) -> impl IntoView {
    let (cls, role, bg, glyph, dot) = match kind {
        "success" => (
            "banner banner--success",
            "status",
            BANNER_BG_CIRCLE,
            "M4.5 8.5l2.5 2.5 4.5-5",
            None,
        ),
        "warning" => (
            "banner banner--warning",
            "alert",
            BANNER_BG_TRIANGLE,
            "M8 6.5v3",
            Some(("8", "11.8", "0.9")),
        ),
        "danger" => (
            "banner banner--danger",
            "alert",
            BANNER_BG_CIRCLE,
            "M8 4v4.5",
            Some(("8", "11.5", "1")),
        ),
        _ => (
            "banner banner--info",
            "status",
            BANNER_BG_CIRCLE,
            "M8 7v4.5",
            Some(("8", "4.5", "1.1")),
        ),
    };
    let message = message.to_string();
    let dot = dot.map(|(cx, cy, r)| {
        view! {
            <circle cx={cx} cy={cy} r={r} fill="currentColor" />
        }
    });
    view! {
        <div class={cls} role={role}>
            <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
                <path d={bg} fill="currentColor" fill-opacity="0.15" />
                <path
                    d={glyph}
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                />
                {dot}
            </svg>
            <p>{message}</p>
        </div>
    }
}

/// Centered empty-state block: icon, heading, one line of body text.
#[cfg(feature = "ssr")]
pub fn empty_state(heading: &str, body: &str) -> impl IntoView {
    let heading = heading.to_string();
    let body = body.to_string();
    view! {
        <div class="empty">
            <svg viewBox="0 0 48 48" width="48" height="48" aria-hidden="true">
                <rect
                    x="8"
                    y="16"
                    width="32"
                    height="24"
                    rx="3"
                    fill="currentColor"
                    fill-opacity="0.15"
                />
                <path
                    d="M8 18v-4a3 3 0 0 1 3-3h8l4 5h14a3 3 0 0 1 3 3"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2"
                    stroke-linejoin="round"
                />
            </svg>
            <h3>{heading}</h3>
            <p>{body}</p>
        </div>
    }
}

/// Labeled input field: `<div class="field">` with `label[for]` above
/// `<input id type>`. An empty `hint` string omits the hint paragraph;
/// a non-empty `error` adds the `field--error` modifier class and the
/// error paragraph below the input.
#[cfg(feature = "ssr")]
pub fn field(id: &str, label: &str, input_type: &str, hint: &str, error: &str) -> impl IntoView {
    let id = id.to_string();
    let label = label.to_string();
    let input_type = input_type.to_string();
    let hint = hint.to_string();
    let error = error.to_string();
    let cls = if error.is_empty() {
        "field"
    } else {
        "field field--error"
    };
    let hint_el = if hint.is_empty() {
        None
    } else {
        Some(view! { <p class="field__hint">{hint}</p> })
    };
    let error_el = if error.is_empty() {
        None
    } else {
        Some(view! { <p class="field__error">{error}</p> })
    };
    view! {
        <div class={cls}>
            <label r#for={id.clone()}>{label}</label>
            <input id={id} r#type={input_type} />
            {hint_el}
            {error_el}
        </div>
    }
}

/// Native `<dialog id class="modal">` with a title, body, and a
/// `method="post"` form holding a plain `type="button"` cancel (closed
/// by `confirm.js`, Task 7) and the confirm submit button
/// (`btn btn--{confirm_class}`).
#[cfg(feature = "ssr")]
pub fn confirm_dialog(
    id: &str,
    title: &str,
    body: &str,
    confirm_label: &str,
    confirm_class: &str,
) -> impl IntoView {
    let id = id.to_string();
    let title = title.to_string();
    let body = body.to_string();
    let confirm_label = confirm_label.to_string();
    view! {
        <dialog id={id} class="modal">
            <h3>{title}</h3>
            <p>{body}</p>
            <form method="post">
                <button r#type="button">Cancel</button>
                <button r#type="submit" class={button_class(confirm_class)}>{confirm_label}</button>
            </form>
        </dialog>
    }
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::*;
    use crate::render::render;

    #[test]
    fn button_variants_and_escaping() {
        let html = render(button("Save <>&\"'", "primary"));
        assert!(html.contains("btn btn--primary"), "{html}");
        assert!(
            !html.contains("Save <"),
            "user text must be escaped: {html}"
        );
        assert!(!html.contains("style="), "no inline styles: {html}");
    }

    #[test]
    fn field_ids_pair_label_and_input() {
        let html = render(field("pw", "Current password", "password", "hint", ""));
        assert!(html.contains(r#"label for="pw""#), "{html}");
        assert!(html.contains(r#"id="pw""#), "{html}");
        assert!(html.contains("field__hint"), "{html}");
        assert!(!html.contains("style="), "{html}");
    }

    #[test]
    fn field_error_state_renders() {
        let html = render(field("u", "Username", "text", "", "required"));
        assert!(html.contains("field--error"), "{html}");
        assert!(html.contains("field__error"), "{html}");
    }

    #[test]
    fn banner_kinds_render_icons() {
        for kind in ["info", "success", "warning", "danger"] {
            let html = render(banner(kind, "msg"));
            assert!(html.contains(&format!("banner--{kind}")), "{html}");
            assert!(html.contains("<svg"), "{html}");
        }
    }

    #[test]
    fn empty_state_composition() {
        let html = render(empty_state("Nothing here", "Create your first concept"));
        assert!(html.contains("empty"), "{html}");
        assert!(html.contains("Nothing here"), "{html}");
    }

    #[test]
    fn confirm_dialog_is_native_dialog_element() {
        let html = render(confirm_dialog(
            "revoke-key",
            "Revoke key?",
            "This cannot be undone.",
            "Revoke",
            "danger",
        ));
        assert!(html.contains("<dialog"), "{html}");
        assert!(html.contains(r#"id="revoke-key""#), "{html}");
        assert!(html.contains("method=\"post\""), "{html}");
    }
}
