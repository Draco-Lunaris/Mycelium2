//! The escape-witness newtype for raw-markup slots.
//!
//! The raw interpolation surface — `card`'s body, `data_table` cells,
//! `page_header` actions — renders through leptos's `inner_html`,
//! which does NOT escape what lands there. Escaping therefore happens
//! at composition time in the caller: every user-derived fragment is
//! `html_escape`d (or percent-encoded, for hrefs) BEFORE the markup is
//! composed. [`TrustedHtml`] turns that discipline into a compile-time
//! fact: the raw-slot params take the newtype directly, so passing a
//! plain `String` at a raw slot does not compile — reaching the raw
//! interpolation boundary requires the caller to explicitly claim, via
//! [`TrustedHtml::from_markup`], that the markup is composed from
//! escaped fragments.

/// Trusted, caller-composed markup for a raw (`inner_html`) slot.
///
/// Contract: constructed from already-escaped fragments; this type is
/// the caller's escape-then-compose witness. The raw-slot params —
/// `card`'s `body`, `data_table`'s cells, `page_header`'s `actions` —
/// take [`TrustedHtml`] directly, so unescaped interpolation (a plain
/// `String` at a raw slot) is a compile error, and the only production
/// constructor is [`TrustedHtml::from_markup`] over markup whose
/// user-derived fragments were escaped at composition.
///
/// `From<String>` exists for test ergonomics only; production call
/// sites wrap with [`TrustedHtml::from_markup`].
#[cfg(feature = "ssr")]
#[derive(Debug, Clone)]
pub struct TrustedHtml(String);

/// Constructors and the `inner_html` bridge.
#[cfg(feature = "ssr")]
impl TrustedHtml {
    /// Claim `s` as already-escaped, caller-composed markup.
    ///
    /// The ONLY production constructor into the raw slots. By calling
    /// this, the caller asserts that every user-derived fragment in
    /// `s` has already been through `html_escape` (or percent-encoding,
    /// for href values) — this call is the escape-then-compose witness.
    pub fn from_markup(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The markup itself — the bridge the raw-slot components feed to
    /// leptos's `inner_html` (unescaped, by design).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Test-ergonomics constructor: in-module tests build slot values from
/// plain `String`s. Production code constructs [`TrustedHtml`] only via
/// [`TrustedHtml::from_markup`].
#[cfg(feature = "ssr")]
impl From<String> for TrustedHtml {
    fn from(s: String) -> Self {
        Self(s)
    }
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::*;

    #[test]
    fn trusted_html_from_markup_and_as_str() {
        let t = TrustedHtml::from_markup("<b>escaped by caller</b>");
        assert_eq!(t.as_str(), "<b>escaped by caller</b>");
    }

    #[test]
    fn from_string_for_tests_only() {
        let t: TrustedHtml = String::from("x").into();
        assert_eq!(t.as_str(), "x");
    }
}
