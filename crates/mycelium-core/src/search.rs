//! Search index abstractions and an in-memory reference implementation.

use std::collections::HashMap;

use crate::concept::Concept;

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub terms: Vec<String>,
    /// Hint for callers that support cross-scope search: also include
    /// global-scope results. NOTE: single-scope indexes (like the encrypted
    /// SQLite index) ignore this — cross-scope search is performed by the
    /// caller querying both the user index and the global index and merging.
    pub include_global: bool,
}

impl SearchQuery {
    pub fn new(terms: Vec<String>) -> Self {
        Self {
            terms,
            include_global: false,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResult {
    pub concept_path: String,
    pub title: String,
    pub snippet: String,
    pub score: f32,
}

pub trait SearchIndex {
    type Error;

    fn add(&mut self, concept: &Concept) -> Result<(), Self::Error>;
    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, Self::Error>;
}

/// Tokenize: lowercase, split on non-alphanumeric, min length 2. No stemming.
pub fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_string())
        .collect()
}

/// The canonical text indexed for a concept: title + description + tags +
/// body. Both the in-memory reference index and the encrypted SQLite index
/// must use this so their results agree (oracle equivalence).
pub fn searchable_text(concept: &Concept) -> String {
    let mut text = String::new();
    text.push_str(
        &concept
            .frontmatter
            .title
            .clone()
            .unwrap_or_else(|| concept.source_path.clone()),
    );
    if let Some(d) = &concept.frontmatter.description {
        text.push(' ');
        text.push_str(d);
    }
    for tag in &concept.frontmatter.tags {
        text.push(' ');
        text.push_str(tag);
    }
    text.push(' ');
    text.push_str(&concept.body);
    text
}

/// In-memory reference index: tokenizes title + description + tags + body,
/// scores by term frequency. Serves as the test oracle for the Phase 3
/// encrypted SQLite index.
#[derive(Debug, Default)]
pub struct InMemoryIndex {
    /// term -> (concept_path -> tf)
    postings: HashMap<String, HashMap<String, u32>>,
    docs: HashMap<String, (String, String)>, // path -> (title, body)
}

impl InMemoryIndex {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SearchIndex for InMemoryIndex {
    type Error = std::convert::Infallible;

    fn add(&mut self, concept: &Concept) -> Result<(), Self::Error> {
        let title = concept
            .frontmatter
            .title
            .clone()
            .unwrap_or_else(|| concept.source_path.clone());
        let text = searchable_text(concept);

        for token in tokenize(&text) {
            self.postings
                .entry(token)
                .or_default()
                .entry(concept.source_path.clone())
                .and_modify(|tf| *tf += 1)
                .or_insert(1);
        }
        self.docs
            .insert(concept.source_path.clone(), (title, concept.body.clone()));
        Ok(())
    }

    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, Self::Error> {
        let mut scores: HashMap<&str, f32> = HashMap::new();
        for term in &query.terms {
            let token = term.to_lowercase();
            if let Some(docs) = self.postings.get(&token) {
                for (path, tf) in docs {
                    *scores.entry(path.as_str()).or_insert(0.0) += *tf as f32;
                }
            }
        }
        let mut results: Vec<SearchResult> = scores
            .into_iter()
            .map(|(path, score)| {
                let (title, body) = &self.docs[path];
                let snippet = body
                    .chars()
                    .take(200)
                    .collect::<String>()
                    .trim()
                    .to_string();
                SearchResult {
                    concept_path: path.to_string(),
                    title: title.clone(),
                    snippet,
                    score,
                }
            })
            .collect();
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.concept_path.cmp(&b.concept_path))
        });
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concept::Frontmatter;

    fn concept(path: &str, title: &str, body: &str) -> Concept {
        Concept::new(
            Frontmatter {
                concept_type: "Note".into(),
                title: Some(title.into()),
                description: None,
                resource: None,
                tags: vec![],
                timestamp: None,
            },
            body.into(),
            path.into(),
        )
    }

    #[test]
    fn tokenize_basics() {
        assert_eq!(
            tokenize("Hello, World! foo a1"),
            vec!["hello", "world", "foo", "a1"]
        );
    }

    #[test]
    fn search_finds_by_title_token() {
        let mut idx = InMemoryIndex::new();
        idx.add(&concept("/a.md", "Deploy Rust Service", "how to deploy"))
            .ok();
        idx.add(&concept("/b.md", "Cooking Pasta", "boil water"))
            .ok();
        let results = idx
            .search(&SearchQuery::new(vec!["deploy".into()]))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].concept_path, "/a.md");
    }

    #[test]
    fn search_ranks_by_tf() {
        let mut idx = InMemoryIndex::new();
        idx.add(&concept("/a.md", "One", "rust rust rust")).ok();
        idx.add(&concept("/b.md", "Two", "rust once")).ok();
        let results = idx.search(&SearchQuery::new(vec!["rust".into()])).unwrap();
        assert_eq!(results[0].concept_path, "/a.md");
        assert_eq!(results[1].concept_path, "/b.md");
    }

    #[test]
    fn empty_query_returns_empty() {
        let mut idx = InMemoryIndex::new();
        idx.add(&concept("/a.md", "One", "body")).ok();
        let results = idx.search(&SearchQuery::new(vec![])).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn skill_concept_searchable() {
        let mut idx = InMemoryIndex::new();
        let skill = Concept::new(
            Frontmatter {
                concept_type: "Skill".into(),
                title: Some("Deploy Rust Service".into()),
                description: Some("Skill for deploying rust services".into()),
                resource: None,
                tags: vec!["skill".into(), "rust".into()],
                timestamp: None,
            },
            "# Instructions\n\nRun cargo build --release.".into(),
            "/skills/deploy-rust-service.md".into(),
        );
        idx.add(&skill).ok();
        let results = idx.search(&SearchQuery::new(vec!["rust".into()])).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].concept_path, "/skills/deploy-rust-service.md");
    }
}
