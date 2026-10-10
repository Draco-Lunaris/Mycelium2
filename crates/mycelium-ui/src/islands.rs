//! Hydration islands — leptos 0.8 `#[island]` components, the first of
//! PR 2: the chat composer and the API-key copy button.
//!
//! Mechanism: SSR renders an island's markup inside a
//! `<leptos-island data-component data-props>` root (the island macro
//! serializes the props into the root and emits a per-island wasm
//! export named after the component). On pages that load the hydrate
//! bundle, `/assets/islands.js` (the vendored traversal script,
//! mycelium-web `assets.rs::ISLANDS_JS`) initializes the wasm — whose
//! `#[wasm_bindgen(start)]` entry runs `hydrate_islands()` — then walks
//! the roots and calls each export, attaching the `on:` handlers below
//! (SSR renders event attributes as nothing; the single `view!` is the
//! source for both sides). Without the bundle the SSR markup IS the
//! degradation: the composer is an inert form, the key value stays
//! selectable text.
//!
//! Task-7 split contract: the composer island owns the composer's
//! EVENT wiring only (submit binding, Enter-to-send, auto-grow); the
//! transport — the POST `/api/v1/chat/stream` + the SSE pump — and the
//! log's DOM rendering stay in chat.js, exposed here as
//! `window.myceliumChatSend(message)`. The streaming shape
//! (tool-progress events, pending state) is what the chat_stream
//! integration suite pins server-side and what the UI must keep.
//!
//! Prop note: island props must be owned (`String`) — the macro
//! (de)serializes them through the `data-props` JSON attribute, and
//! serde cannot materialize `&'static str` from page-lifetime data.
//! The `#[cfg(feature = "ssr")]` wrappers keep the crate's plain-fn
//! call surface (`chat_composer(placeholder: &'static str)`) that the
//! pages and the tests use.

use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

/// Hand a composed message to chat.js's transport helper
/// (`window.myceliumChatSend`): the Task-7 split keeps the SSE fetch +
/// pump + log DOM in chat.js and only the event wiring here. Absent
/// helper (chat.js failed to load) → the send is a no-op.
fn chat_send(msg: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let target = JsValue::from(window);
    let helper = js_sys::Reflect::get(&target, &JsValue::from_str("myceliumChatSend"));
    if let Ok(f) = helper {
        if f.is_function() {
            let send = f.unchecked_into::<js_sys::Function>();
            let _ = send.call1(&JsValue::NULL, &JsValue::from_str(msg));
        }
    }
}

/// Auto-grow the composer textarea to its content, capped at 160px
/// (the cap the old chat.js wiring used; the CSS max-height still
/// applies). Runtime CSSOM writes are not CSP-governed the way markup
/// `style=` attributes are — the shipped markup carries no style.
fn auto_grow() {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(el) = doc.get_element_by_id("chat-input") else {
        return;
    };
    let input = el.unchecked_into::<web_sys::HtmlTextAreaElement>();
    // Explicit inherent call: the leptos prelude (tachys ElementExt)
    // brings a `style` trait method that would otherwise shadow the
    // web-sys accessor at method-resolution step zero.
    let style = web_sys::HtmlElement::style(&input);
    let _ = style.set_property("height", "auto");
    let capped = input.scroll_height().min(160);
    let _ = style.set_property("height", &format!("{capped}px"));
}

/// The deprecated-but-universal copy path: select the value element's
/// contents, then `execCommand("copy")` — used only when the async
/// clipboard API is missing or rejects.
fn legacy_copy(value_id: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(doc) = window.document() else {
        return;
    };
    let Some(el) = doc.get_element_by_id(value_id) else {
        return;
    };
    if let Ok(Some(selection)) = window.get_selection() {
        if let Ok(range) = doc.create_range() {
            let _ = range.select_node_contents(&el);
            let _ = selection.remove_all_ranges();
            let _ = selection.add_range(&range);
        }
    }
    let html_doc = doc.clone().unchecked_into::<web_sys::HtmlDocument>();
    let _ = html_doc.exec_command("copy");
}

/// Copy the element named by `value_id`: `navigator.clipboard.write_text`
/// when the API is present, falling back to [`legacy_copy`] when it is
/// missing or its promise rejects.
fn copy_now(value_id: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(doc) = window.document() else {
        return;
    };
    let Some(el) = doc.get_element_by_id(value_id) else {
        return;
    };
    let Some(text) = el.text_content() else {
        return;
    };
    let navigator = JsValue::from(window.navigator());
    let clipboard = js_sys::Reflect::get(&navigator, &JsValue::from_str("clipboard"))
        .ok()
        .filter(|c| !c.is_undefined() && !c.is_null());
    match clipboard {
        Some(c) => {
            let promise = c.unchecked_into::<web_sys::Clipboard>().write_text(&text);
            let id = value_id.to_string();
            wasm_bindgen_futures::spawn_local(async move {
                if JsFuture::from(promise).await.is_err() {
                    legacy_copy(&id);
                }
            });
        }
        None => legacy_copy(value_id),
    }
}

/// The chat composer island: the form + textarea + Send button with
/// the ids/classes chat.js's surviving code and the page CSS depend on
/// (`chat-form`, `chat-input-bar`, `chat-input`, `button[type=submit]`).
/// Event wiring attaches on hydrate: submit → preventDefault → hand the
/// trimmed message to chat.js's `window.myceliumChatSend`; Enter sends
/// and Shift+Enter inserts a newline (via `requestSubmit`, so the
/// `required` validation gate still applies); the textarea auto-grows.
#[island]
pub fn ChatComposer(#[prop(into)] placeholder: String) -> impl IntoView {
    view! {
        <form id="chat-form" class="chat-input-bar" on:submit=move |ev| {
            ev.prevent_default();
            let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
                return;
            };
            let Some(input) = doc
                .get_element_by_id("chat-input")
                .map(|el| el.unchecked_into::<web_sys::HtmlTextAreaElement>())
            else {
                return;
            };
            let msg = input.value().trim().to_string();
            if !msg.is_empty() {
                chat_send(&msg);
                input.set_value("");
            }
        }>
            <textarea
                id="chat-input"
                rows="1"
                placeholder=placeholder
                required
                on:keydown=move |ev| {
                    if ev.key() == "Enter" && !ev.shift_key() {
                        ev.prevent_default();
                        if let Some(form) = ev
                            .target()
                            .and_then(|t| t.unchecked_into::<web_sys::HtmlTextAreaElement>().form())
                        {
                            let _ = form.request_submit();
                        }
                    }
                }
                on:input=move |_| auto_grow()
            ></textarea>
            <button type="submit">"Send"</button>
        </form>
    }
}

/// The API-keys copy button island: a quiet control beside a
/// shown-once secret. On hydrate, clicking copies the value element
/// named by `data-copy-target` (the sibling `<code>`) via
/// `navigator.clipboard`, falling back to the legacy
/// `execCommand("copy")` selection path when the API is missing or
/// rejects. The SSR markup is the no-JS degradation: the value stays
/// selectable text.
#[island]
pub fn CopyButton(#[prop(into)] value_id: String) -> impl IntoView {
    view! {
        <button class="copy-button" data-copy-target=value_id.clone() on:click=move |_| copy_now(&value_id)>
            "Copy"
        </button>
    }
}

/// SSR-facing wrapper — the brief's interface:
/// `chat_composer(placeholder: &'static str) -> impl IntoView`.
#[cfg(feature = "ssr")]
pub fn chat_composer(placeholder: &'static str) -> impl IntoView {
    view! { <ChatComposer placeholder=placeholder/> }
}

/// SSR-facing wrapper — the brief's interface:
/// `copy_button(value_id: &'static str) -> impl IntoView`.
#[cfg(feature = "ssr")]
pub fn copy_button(value_id: &'static str) -> impl IntoView {
    view! { <CopyButton value_id=value_id/> }
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::*;
    use crate::render::render;

    #[test]
    fn chat_composer_renders_ssrsend_control() {
        let html = render(chat_composer("Ask the librarian"));
        assert!(html.contains("chat-input-bar"), "input bar class: {html}");
        assert!(html.contains("Ask the librarian"), "{html}");
        assert!(
            html.contains(r#"<form id="chat-form" class="chat-input-bar">"#),
            "the ids/classes chat.js + the page CSS depend on: {html}"
        );
        assert!(html.contains(r#"<textarea id="chat-input""#), "{html}");
        assert!(html.contains("required"), "{html}");
        assert!(html.contains(r#"<button type="submit">"#), "{html}");
        assert!(!html.contains("style="), "{html}");
    }

    #[test]
    fn copy_button_renders_with_selectable_fallback() {
        let html = render(copy_button("key-value"));
        assert!(html.contains("copy-button"), "{html}");
        assert!(
            html.contains(r#"data-copy-target="key-value""#),
            "target id rides on the button: {html}"
        );
        assert!(!html.contains("style="), "{html}");
    }

    #[test]
    fn islands_marked_for_hydration() {
        let chat = render(chat_composer("x"));
        assert!(
            chat.contains("leptos-island"),
            "island root element: {chat}"
        );
        assert!(chat.contains("data-component="), "{chat}");
        let copy = render(copy_button("y"));
        assert!(
            copy.contains("leptos-island"),
            "island root element: {copy}"
        );
        assert!(copy.contains("data-component="), "{copy}");
    }
}
