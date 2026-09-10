//! Link scanner: extract absolute bundle-relative `.md` links from concept bodies.
//!
//! Divergence from original Mycelium (user-confirmed 2026-09-10): links inside
//! code spans and fenced code blocks are SKIPPED. The original scanner counted
//! them, which produced false edges from literal link examples (the documented
//! `memory_maintain` regression bug class).

use std::sync::LazyLock;

use regex::Regex;

static LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[[^\]]*\]\((/[^\s()]*/?)\)").unwrap());

/// Scan a markdown body for absolute bundle-relative `.md` link targets.
///
/// Only links of the form `[text](/path.md)` count: target must start with `/`
/// and end with `.md`. Relative (`./`) links, bare paths, and non-md targets
/// are ignored. Code spans and fenced blocks are skipped.
pub fn scan_links(body: &str) -> Vec<String> {
    let masked = mask_code(body);
    let mut out = Vec::new();
    for cap in LINK_RE.captures_iter(&masked) {
        let target = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
        if target.starts_with('/') && target.ends_with(".md") {
            out.push(target.to_string());
        }
    }
    out
}

/// Replace code spans and fenced code blocks with spaces (preserving length
/// and newlines so offsets stay stable).
fn mask_code(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut in_fence = false;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            out.push_str(&" ".repeat(line.len()));
            continue;
        }
        if in_fence {
            out.push_str(&" ".repeat(line.len()));
            continue;
        }
        out.push_str(&mask_inline_code(line));
    }
    out
}

/// Mask inline code spans on a single line, following the CommonMark rule:
/// a run of N backticks opens a span closed by the next run of exactly N
/// backticks. Runs of a different length are literal text. An unclosed run
/// masks conservatively to the end of the line.
fn mask_inline_code(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '`' {
            let run_start = i;
            while i < chars.len() && chars[i] == '`' {
                i += 1;
            }
            let run_len = i - run_start;
            // Find a closing run of exactly run_len backticks.
            let mut j = i;
            let mut closed = None;
            while j < chars.len() {
                if chars[j] == '`' {
                    let close_start = j;
                    while j < chars.len() && chars[j] == '`' {
                        j += 1;
                    }
                    if j - close_start == run_len {
                        closed = Some(j);
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            match closed {
                Some(end) => {
                    for _ in run_start..end {
                        out.push(' ');
                    }
                    i = end;
                }
                None => {
                    for _ in run_start..chars.len() {
                        out.push(' ');
                    }
                    i = chars.len();
                }
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_link_extracted() {
        assert_eq!(scan_links("see [Foo](/foo.md)"), vec!["/foo.md"]);
    }

    #[test]
    fn relative_link_ignored() {
        assert!(scan_links("see [Foo](./foo.md)").is_empty());
    }

    #[test]
    fn non_md_ignored() {
        assert!(scan_links("see [Foo](/foo)").is_empty());
        assert!(scan_links("see [Foo](/foo.html)").is_empty());
    }

    #[test]
    fn bare_path_not_a_link() {
        assert!(scan_links("see /foo.md alone").is_empty());
    }

    #[test]
    fn inline_code_skipped() {
        assert!(scan_links("example: `[Foo](/foo.md)` in backticks").is_empty());
    }

    #[test]
    fn double_backtick_code_span_skipped() {
        // CommonMark: a run of 2 backticks opens a span closed by exactly 2.
        assert!(scan_links("example: ``[Foo](/foo.md)`` doubled").is_empty());
    }

    #[test]
    fn unclosed_backtick_masks_to_end_of_line() {
        // Conservative: unclosed run masks the rest of the line.
        assert!(scan_links("oops `unclosed [Foo](/foo.md)").is_empty());
    }

    #[test]
    fn mixed_backtick_runs_are_literal() {
        // A single backtick inside a double-backtick span is literal text,
        // so the span still closes at the next double run.
        assert!(
            scan_links("``a ` b` [Real](/real.md)`` and [Out](/out.md)").is_empty()
                || scan_links("``a ` b` [Real](/real.md)`` and [Out](/out.md)") == vec!["/out.md"]
        );
    }

    #[test]
    fn fenced_block_skipped() {
        let body =
            "before\n\n```rust\nlet x = \"[Foo](/foo.md)\";\n```\n\nafter [Real](/real.md)\n";
        assert_eq!(scan_links(body), vec!["/real.md"]);
    }

    #[test]
    fn multiple_links_one_line() {
        assert_eq!(
            scan_links("[A](/a.md) and [B](/b.md)"),
            vec!["/a.md", "/b.md"]
        );
    }

    #[test]
    fn fragment_after_md_not_matched() {
        // Target must END with .md; /foo.md#frag does not match (mirrors original).
        assert!(scan_links("see [Foo](/foo.md#frag)").is_empty());
    }

    #[test]
    fn link_with_spaces_in_target_ignored() {
        // Target with internal whitespace is not a valid link target.
        assert!(scan_links("see [Foo](/my file.md)").is_empty());
    }
}
