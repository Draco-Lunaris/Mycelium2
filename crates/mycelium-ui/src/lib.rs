//! Shared UI components for the web frontend: Leptos 0.8 views,
//! primitive-only props, compiled both for SSR (mycelium-web handlers)
//! and wasm32 hydration (the hydrate bundle).
//!
//! Server build (default): cargo build
//! Hydrate build: cargo build --no-default-features --features hydrate \
//!   --target wasm32-unknown-unknown

#[cfg(feature = "ssr")]
pub mod render {
    use leptos::prelude::*;

    /// Render any view to a String (the SSR bridge every page uses).
    pub fn render(view: impl IntoView) -> String {
        view.into_view().to_html()
    }
}

mod layout;
mod primitives;
#[cfg(feature = "ssr")]
pub mod shell;

#[cfg(feature = "ssr")]
pub use layout::{
    breadcrumb, card, data_table, form_actions, nav_group, nav_item, stat_tile, tab_bar,
};
#[cfg(feature = "ssr")]
pub use primitives::{banner, button, button_class, chip, confirm_dialog, empty_state, field};
#[cfg(feature = "ssr")]
pub use shell::ASSETS_VERSION;

/// Hydration entry: wasm-bindgen calls this on module start; it walks
/// whatever <leptos-island> roots the page carries. Islands themselves
/// are #[island] components added in PR 2 — the entry ships now so the
/// hydrate build (Task 9's bundle) has its start function.
#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
fn start() {
    leptos::mount::hydrate_islands();
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    #[test]
    fn render_button_class_variants() {
        assert_eq!(super::button_class("primary"), "btn btn--primary");
        assert_eq!(super::button_class("secondary"), "btn btn--secondary");
        assert_eq!(super::button_class("danger"), "btn btn--danger");
        assert_eq!(super::button_class("ghost"), "btn btn--ghost");
        assert_eq!(super::button_class("anything-else"), "btn btn--primary");
    }
}
