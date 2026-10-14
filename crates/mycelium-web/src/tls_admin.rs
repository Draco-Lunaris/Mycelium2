//! TLS admin operations: SAN configuration, cert display parsing,
//! third-party cert upload validation, CSR generation, and the
//! pending-CSR lifecycle. See docs/superpowers/specs/2026-10-14-tls-management-design.md.

pub const DEFAULT_SAN: &str = "localhost";

/// Parse the MYCELIUM2_TLS_SANS / --tls-sans value: comma-separated,
/// trimmed, empties dropped, order-stable dedup. None/empty → the
/// `localhost` default; a configured set is returned verbatim.
pub fn sans_from_env(raw: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.into_iter().flat_map(|raw| raw.split(',')) {
        let s = part.trim();
        if !s.is_empty() && !out.iter().any(|e| e == s) {
            out.push(s.to_string());
        }
    }
    if out.is_empty() {
        out.push(DEFAULT_SAN.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sans_none_is_localhost_only() {
        assert_eq!(sans_from_env(None), vec!["localhost"]);
    }

    #[test]
    fn sans_split_trim_dedup() {
        assert_eq!(
            sans_from_env(Some(
                "mycelium2.example, 192.0.2.10 ,localhost, mycelium2.example"
            )),
            vec!["mycelium2.example", "192.0.2.10", "localhost"]
        );
    }

    #[test]
    fn sans_set_taken_literally_no_forced_additions() {
        // Environment-agnostic (public-repo rule): a configured SAN set is
        // the operator's, verbatim — never force-merged with anything.
        assert_eq!(sans_from_env(Some("example.com")), vec!["example.com"]);
    }

    #[test]
    fn sans_empty_entries_dropped() {
        assert_eq!(sans_from_env(Some(" , ,a,, b,")), vec!["a", "b"]);
    }
}
