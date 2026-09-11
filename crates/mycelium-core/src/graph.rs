//! OKF graph builder: nodes, edges, broken links, orphans, health metrics.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::bundle::Bundle;
use crate::concept::Concept;
use crate::links::scan_links;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Node {
    pub id: String,
    pub title: String,
    pub concept_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Edge {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BrokenLink {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphHealth {
    pub concept_count: usize,
    pub edge_count: usize,
    pub broken_link_count: usize,
    pub orphan_count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Graph {
    pub nodes: HashMap<String, Node>,
    /// Deduplicated, sorted edges.
    pub edges: Vec<Edge>,
    /// Deduplicated, sorted broken links.
    pub broken_links: Vec<BrokenLink>,
    /// Sorted orphan node ids (0 in + 0 out edges).
    pub orphans: Vec<String>,
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn health(&self) -> GraphHealth {
        GraphHealth {
            concept_count: self.nodes.len(),
            edge_count: self.edges.len(),
            broken_link_count: self.broken_links.len(),
            orphan_count: self.orphans.len(),
        }
    }
}

/// Build the link graph from a parsed bundle.
///
/// - One node per concept (id = canonical path).
/// - Edge per scanned link whose target exists in the bundle (deduplicated).
/// - BrokenLink per scanned link whose target is missing.
/// - Orphan = node with no in or out edges.
/// - Output ordering is deterministic (sorted).
pub fn build_graph(bundle: &Bundle) -> Graph {
    let mut graph = Graph::new();

    for concept in &bundle.concepts {
        graph.nodes.insert(
            concept.source_path.clone(),
            Node {
                id: concept.source_path.clone(),
                title: concept
                    .frontmatter
                    .title
                    .clone()
                    .unwrap_or_else(|| concept.source_path.clone()),
                concept_type: concept.frontmatter.concept_type.clone(),
            },
        );
    }

    let known: HashSet<&str> = bundle
        .concepts
        .iter()
        .map(|c| c.source_path.as_str())
        .collect();

    let mut edges: HashSet<(String, String)> = HashSet::new();
    let mut broken: HashSet<(String, String)> = HashSet::new();
    let mut linked: HashSet<String> = HashSet::new();

    for concept in &bundle.concepts {
        for target in scan_links(&concept.body) {
            if known.contains(target.as_str()) {
                edges.insert((concept.source_path.clone(), target.clone()));
            } else {
                broken.insert((concept.source_path.clone(), target.clone()));
            }
            linked.insert(concept.source_path.clone());
            linked.insert(target);
        }
    }

    graph.edges = edges
        .into_iter()
        .map(|(from, to)| Edge { from, to })
        .collect();
    graph.edges.sort();

    graph.broken_links = broken
        .into_iter()
        .map(|(from, to)| BrokenLink { from, to })
        .collect();
    graph.broken_links.sort();

    graph.orphans = graph
        .nodes
        .keys()
        .filter(|id| !linked.contains(*id))
        .cloned()
        .collect();
    graph.orphans.sort();

    graph
}

/// Convenience: build a graph from a slice of concepts (for tests).
pub fn build_graph_from_concepts(concepts: &[Concept]) -> Graph {
    let bundle = Bundle {
        root: std::path::PathBuf::new(),
        concepts: concepts.to_vec(),
        shelf_info: None,
        reserved_files_seen: vec![],
        naming_warnings: vec![],
    };
    build_graph(&bundle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concept::Frontmatter;

    fn concept(path: &str, body: &str) -> Concept {
        Concept::new(
            Frontmatter {
                concept_type: "Note".into(),
                title: Some(path.to_string()),
                ..Default::default()
            },
            body.to_string(),
            path.into(),
        )
    }

    #[test]
    fn edges_and_broken_links() {
        let concepts = vec![
            concept("/a.md", "links to [B](/b.md) and [Missing](/missing.md)"),
            concept("/b.md", "links back to [A](/a.md)"),
            concept("/orphan.md", "no links at all"),
        ];
        let g = build_graph_from_concepts(&concepts);
        let health = g.health();
        assert_eq!(health.concept_count, 3);
        assert_eq!(health.edge_count, 2); // a->b, b->a
        assert_eq!(health.broken_link_count, 1); // a->missing
        assert_eq!(health.orphan_count, 1); // orphan.md
        assert_eq!(g.orphans, vec!["/orphan.md"]);
    }

    #[test]
    fn duplicate_links_deduplicated() {
        let concepts = vec![
            concept("/a.md", "[B](/b.md) [B again](/b.md) [B third](/b.md)"),
            concept("/b.md", ""),
        ];
        let g = build_graph_from_concepts(&concepts);
        assert_eq!(g.edges.len(), 1);
    }

    #[test]
    fn self_link_counts_as_edge_not_orphan_breaker() {
        let concepts = vec![concept("/a.md", "[me](/a.md)")];
        let g = build_graph_from_concepts(&concepts);
        assert_eq!(g.edges.len(), 1);
        assert!(g.orphans.is_empty());
    }

    #[test]
    fn deterministic_output() {
        let concepts = vec![
            concept("/a.md", "[B](/b.md) [C](/c.md)"),
            concept("/b.md", "[A](/a.md)"),
            concept("/c.md", ""),
        ];
        let g1 = build_graph_from_concepts(&concepts);
        let g2 = build_graph_from_concepts(&concepts);
        assert_eq!(g1.edges, g2.edges);
        assert_eq!(g1.orphans, g2.orphans);
        assert_eq!(g1.broken_links, g2.broken_links);
    }
}
