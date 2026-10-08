//! Primitive components: Button, Chip, Banner, EmptyState, Field,
//! inputs, ConfirmDialog. Components land in Task 4; the variant-class
//! helper is pinned here so both builds have a stable surface.

#[cfg(feature = "ssr")]
pub fn button_class(variant: &str) -> String {
    match variant {
        "secondary" => "btn btn--secondary".into(),
        "danger" => "btn btn--danger".into(),
        "ghost" => "btn btn--ghost".into(),
        _ => "btn btn--primary".into(),
    }
}
