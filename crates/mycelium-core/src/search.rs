//! Search index abstractions and an in-memory reference implementation.

use std::collections::{HashMap, HashSet};

use crate::concept::Concept;

/// Which lifecycle statuses a search includes. Constructing a
/// [`SearchQuery`] without saying otherwise means active-only: concepts
/// marked `superseded` or `retired` disappear from default retrieval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusFilter {
    #[default]
    ActiveOnly,
    All,
}

/// Status values that hide a concept from default retrieval.
pub const NON_ACTIVE_STATUSES: [&str; 2] = ["superseded", "retired"];

/// True iff a concept with this frontmatter `status` value participates
/// in ActiveOnly retrieval. None, unknown values and "active" are active.
pub fn is_active_status(status: Option<&str>) -> bool {
    match status {
        None => true,
        Some(s) => !NON_ACTIVE_STATUSES.contains(&s),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SearchMode {
    /// Fuse lexical results with vector results when the embeddings
    /// backend is configured and healthy; degrade to lexical-only
    /// otherwise (never an error).
    #[default]
    Auto,
    /// Lexical pools only — the pre-vector behavior, byte-for-byte.
    Lexical,
    /// Vector pools only, with NO lexical fallback: a deliberately
    /// honest diagnostic (an empty pool stays empty).
    VectorOnly,
}

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub terms: Vec<String>,
    /// Hint for callers that support cross-scope search: also include
    /// global-scope results. NOTE: single-scope indexes (like the encrypted
    /// SQLite index) ignore this — cross-scope search is performed by the
    /// caller querying both the user index and the global index and merging.
    pub include_global: bool,
    /// Lifecycle filter: [`StatusFilter::ActiveOnly`] (the default) skips
    /// docs whose status is in [`NON_ACTIVE_STATUSES`].
    pub status_filter: StatusFilter,
    /// Which sources the search COMPOSER combines (the hybrid composer
    /// in mycelium-librarian / mycelium-web). **The lexical engines
    /// IGNORE this field**: [`SearchIndex::search`] only ever scores
    /// lexically — the mode selects which pools the composer builds
    /// around the engine's results, not how those pools are scored.
    pub mode: SearchMode,
}

impl SearchQuery {
    pub fn new(terms: Vec<String>) -> Self {
        Self {
            mode: SearchMode::default(),
            terms,
            include_global: false,
            status_filter: StatusFilter::default(),
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
///
/// Body content is cut at the first line whose `trim_end()` is exactly
/// `# History`: everything from that heading on is lifecycle history and is
/// never indexed.
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
    // Line scan: keep every line before the first exact `# History` heading
    // (trim_end comparison so trailing whitespace still matches); a body that
    // starts with the marker contributes no body content.
    let body_len: usize = concept
        .body
        .split_inclusive('\n')
        .take_while(|line| line.trim_end() != "# History")
        .map(str::len)
        .sum();
    text.push_str(&concept.body[..body_len]);
    text
}

/// Query-fusion primitives (memory-lifecycle spec §3.3): the weights
/// the hybrid composer uses and the two functions both consumer sites
/// (mycelium-librarian's `cross_scope_hits` and mycelium-web's
/// `search_all_scopes`) share.
///
/// Reciprocal Rank Fusion: every pool is a DISTINCT source of truth, so
/// only each pool's RANK (position, 1-based) is consumed — the fused
/// score of a candidate is the sum of its per-pool terms, and because
/// ranks (not raw scores) drive the arithmetic, fused scores are
/// comparable ACROSS scopes. That cross-scope comparability is the
/// reason the composer fuses per scope and merges afterwards.
///
/// The hybrid composer's formula constants. Spec-set defaults (k 60 —
/// the classic rank-softening constant — and equal weights); the
/// `lifecycle` config row retunes all three at runtime via
/// `mycelium_store::revisions::rrf_params()`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RrfParams {
    /// The rank-softening constant: larger values flatten rank
    /// differences (position 1 scores 1/(k+1), position 50 scores
    /// 1/(k+50)).
    pub rrf_k: f32,
    /// The lexical pool's weight.
    pub w_lex: f32,
    /// The vector pool's weight.
    pub w_vec: f32,
}

impl Default for RrfParams {
    fn default() -> Self {
        RrfParams {
            rrf_k: 60.0,
            w_lex: 1.0,
            w_vec: 1.0,
        }
    }
}

/// Cosine similarity of two vectors: `a·b / (|a|·|b|)`. Callers filter
/// dimensions (and model) first, so the pools only ever compare
/// same-model rows — mismatched or empty lengths return 0.0, as does a
/// zero-norm guard (a zero vector is similarity-0 to everything, not a
/// NaN, and never the 1.0 self-similarity trap).
#[must_use]
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    debug_assert_eq!(
        a.len(),
        b.len(),
        "the vector composer filters dimensions before calling cosine"
    );
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 || denom.is_nan() {
        return 0.0;
    }
    dot / denom
}

/// Fuse two already-ranked pools (rank 1 = best, in position order)
/// into one ranked list of `(path, fused_score)`: each candidate's
/// fused score is `w/(k + rank)` summed over the pools it appears in,
/// an absent pool contributing that term as 0 (see the module-level
/// fusion docs). Output sorted by fused score descending, ties broken
/// by path ascending.
#[must_use]
pub fn rrf_fuse(
    lex_pool: &[(String, f32)],
    vec_pool: &[(String, f32)],
    params: &RrfParams,
) -> Vec<(String, f32)> {
    // Accumulate by path; a duplicate path inside ONE pool would sum
    // twice, but pools are distinct-path by construction (ranked search
    // results / one vector row per path).
    let mut scores: std::collections::BTreeMap<String, f32> = std::collections::BTreeMap::new();
    for (rank, (path, _)) in lex_pool.iter().enumerate() {
        *scores.entry(path.clone()).or_insert(0.0) +=
            params.w_lex / (params.rrf_k + rank as f32 + 1.0);
    }
    for (rank, (path, _)) in vec_pool.iter().enumerate() {
        *scores.entry(path.clone()).or_insert(0.0) +=
            params.w_vec / (params.rrf_k + rank as f32 + 1.0);
    }
    let mut out: Vec<(String, f32)> = scores.into_iter().collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

/// Shared BM25 parameters and scorer.
///
/// Both indexes (the in-memory oracle below and the encrypted SQLite index
/// in `mycelium-store`) score through [`bm25_score`] with the same inputs
/// so their result ordering agrees (oracle equivalence).
pub mod bm25 {
    /// Scoring parameters. `k1` controls term-frequency saturation, `b`
    /// controls how strongly document length is normalized.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Params {
        pub k1: f32,
        pub b: f32,
    }

    impl Default for Params {
        fn default() -> Self {
            Self { k1: 1.2, b: 0.75 }
        }
    }
}

/// BM25 contribution of one (term, document) pair.
///
/// Argument order is fixed — `n_docs`, `df`, `tf`, `doc_len`, `avgdl` — so
/// callers cannot transpose the count-like arguments. Semantics:
///
/// - idf uses the non-negative Lucene form
///   `ln(1 + (n_docs − df + 0.5) / (df + 0.5))` (§3.2a-1). Plain Robertson
///   idf turns negative once `df > n_docs / 2` and, because query scores
///   are a sum over terms, a corpus-common term would then *subtract* from
///   every multi-term hit.
/// - `df == 0` contributes `0.0` (callers normally skip such terms).
/// - `avgdl == 0.0` leaves only the tf-saturation term: the length ratio is
///   treated as `1.0` rather than dividing by zero (§3.2a-5).
///
/// Callers must dedup query terms before summing over them (§3.2a-2), or a
/// repeated term would double its weight. `df` must not exceed `n_docs` (a
/// posting list is a subset of the corpus); both indexes get that from
/// counting the same doc set they score against.
pub fn bm25_score(
    params: &bm25::Params,
    n_docs: usize,
    df: usize,
    tf: u32,
    doc_len: u32,
    avgdl: f32,
) -> f32 {
    if df == 0 {
        return 0.0;
    }
    let n_docs = n_docs as f32;
    let df = df as f32;
    let idf = ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
    let length_ratio = if avgdl == 0.0 {
        1.0
    } else {
        doc_len as f32 / avgdl
    };
    let tf = tf as f32;
    let tf_term =
        tf * (params.k1 + 1.0) / (tf + params.k1 * (1.0 - params.b + params.b * length_ratio));
    idf * tf_term
}

/// One indexed document: the fields a search result needs plus the token
/// count BM25 normalizes against.
#[derive(Debug)]
struct DocMeta {
    title: String,
    body: String,
    status: Option<String>,
    /// Token count of the document's [`searchable_text`] (repeats
    /// included, i.e. equal to the sum of tf over its distinct tokens).
    /// Tracked at `add` so scoring never re-tokenizes.
    length: u32,
}

/// In-memory reference index: tokenizes title + description + tags + body
/// and scores with [`bm25_score`]. Serves as the test oracle for the Phase
/// 3 encrypted SQLite index, so it must agree with it on the three things
/// that decide ordering: deduped query terms, the BM25 formula, and the
/// pre-scoring lifecycle skip.
#[derive(Debug, Default)]
pub struct InMemoryIndex {
    /// term -> (concept_path -> tf)
    postings: HashMap<String, HashMap<String, u32>>,
    docs: HashMap<String, DocMeta>, // path -> metadata
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
        let tokens = tokenize(&text);

        // A re-add REPLACES the path's postings (§3.2a-9): strip it from
        // every posting list first, then drop lists left empty. Accumulating
        // instead would keep superseded content searchable, resurrect terms
        // the new version dropped, and inflate df — which BM25 makes
        // load-bearing. The store index behaves the same way (its `add` is
        // DELETE-then-INSERT).
        for posting in self.postings.values_mut() {
            posting.remove(&concept.source_path);
        }
        self.postings.retain(|_, posting| !posting.is_empty());

        for token in tokens.iter() {
            *self
                .postings
                .entry(token.clone())
                .or_default()
                .entry(concept.source_path.clone())
                .or_insert(0) += 1;
        }
        self.docs.insert(
            concept.source_path.clone(),
            DocMeta {
                title,
                body: concept.body.clone(),
                status: concept.frontmatter.status.clone(),
                length: tokens.len() as u32,
            },
        );
        Ok(())
    }

    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, Self::Error> {
        // Corpus statistics cover every doc regardless of lifecycle status:
        // the filter is a per-result skip, not an index cut.
        let n_docs = self.docs.len();
        let avgdl = if n_docs == 0 {
            0.0
        } else {
            let total_len: u64 = self.docs.values().map(|doc| doc.length as u64).sum();
            total_len as f32 / n_docs as f32
        };
        let params = bm25::Params::default();

        let mut scores: HashMap<&str, f32> = HashMap::new();
        let mut seen: HashSet<String> = HashSet::new();
        for term in &query.terms {
            // Query terms are deduped before scoring (§3.2a-2): under BM25
            // a repeated term must not double its weight, and the store
            // index dedups the same way. Dedup on the lowercased token —
            // the same form the posting lookup uses.
            let token = term.to_lowercase();
            if !seen.insert(token.clone()) {
                continue;
            }
            if let Some(docs) = self.postings.get(&token) {
                let df = docs.len();
                for (path, tf) in docs {
                    // Lifecycle filter is a skip BEFORE scoring: scoring,
                    // ordering and snippets are untouched by it.
                    if query.status_filter == StatusFilter::ActiveOnly {
                        let status = &self.docs[path].status;
                        if !is_active_status(status.as_deref()) {
                            continue;
                        }
                    }
                    let doc_len = self.docs[path].length;
                    *scores.entry(path.as_str()).or_insert(0.0) +=
                        bm25_score(&params, n_docs, df, *tf, doc_len, avgdl);
                }
            }
        }
        let mut results: Vec<SearchResult> = scores
            .into_iter()
            .map(|(path, score)| {
                let doc = &self.docs[path];
                let snippet = doc
                    .body
                    .chars()
                    .take(200)
                    .collect::<String>()
                    .trim()
                    .to_string();
                SearchResult {
                    concept_path: path.to_string(),
                    title: doc.title.clone(),
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
                ..Default::default()
            },
            body.into(),
            path.into(),
        )
    }

    fn concept_with_status(path: &str, status: Option<&str>) -> Concept {
        let mut c = concept(path, "Rust Fact", "rust details");
        c.frontmatter.status = status.map(str::to_string);
        c
    }

    fn paths(results: &[SearchResult]) -> Vec<String> {
        results.iter().map(|r| r.concept_path.clone()).collect()
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
    fn history_section_not_indexed() {
        let c = Concept::parse(
            "/x.md",
            "---\ntype: Note\n---\ncurrent body\n\n# History\nobsolete zebracorn facts\n",
        )
        .unwrap();
        let t = searchable_text(&c);
        assert!(t.contains("current body"));
        assert!(!t.contains("zebracorn"));
    }

    #[test]
    fn searchable_text_body_starting_with_history_is_empty() {
        let c = Concept::parse("/x.md", "---\ntype: Note\n---\n# History\nzebracorn\n").unwrap();
        assert!(!searchable_text(&c).contains("zebracorn"));
    }

    #[test]
    fn history_marker_requires_exact_line() {
        for heading in ["## History", "#History", "# History extra", "# history"] {
            let c = Concept::parse(
                "/x.md",
                &format!("---\ntype: Note\n---\nbody\n\n{heading}\nzebracorn facts\n"),
            )
            .unwrap();
            assert!(
                searchable_text(&c).contains("zebracorn"),
                "{heading} must NOT split"
            );
        }
    }

    #[test]
    fn skill_concept_searchable() {
        let mut idx = InMemoryIndex::new();
        let skill = Concept::new(
            Frontmatter {
                concept_type: "Skill".into(),
                title: Some("Deploy Rust Service".into()),
                description: Some("Skill for deploying rust services".into()),
                tags: vec!["skill".into(), "rust".into()],
                ..Default::default()
            },
            "# Instructions\n\nRun cargo build --release.".into(),
            "/skills/deploy-rust-service.md".into(),
        );
        idx.add(&skill).ok();
        let results = idx.search(&SearchQuery::new(vec!["rust".into()])).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].concept_path, "/skills/deploy-rust-service.md");
    }

    #[test]
    fn retired_concept_hidden_by_default() {
        let mut idx = InMemoryIndex::new();
        idx.add(&concept_with_status("/retired.md", Some("retired")))
            .unwrap();
        idx.add(&concept_with_status("/active.md", Some("active")))
            .unwrap();
        let hits = idx.search(&SearchQuery::new(vec!["rust".into()])).unwrap();
        assert_eq!(paths(&hits), ["/active.md"]);
    }

    #[test]
    fn all_filter_includes_retired() {
        let mut idx = InMemoryIndex::new();
        idx.add(&concept_with_status("/retired.md", Some("retired")))
            .unwrap();
        idx.add(&concept_with_status("/active.md", Some("active")))
            .unwrap();
        let mut query = SearchQuery::new(vec!["rust".into()]);
        query.status_filter = StatusFilter::All;
        let hits = idx.search(&query).unwrap();
        assert_eq!(paths(&hits), ["/active.md", "/retired.md"]);
    }

    #[test]
    fn unknown_status_value_kept_active() {
        let mut idx = InMemoryIndex::new();
        idx.add(&concept_with_status("/archive.md", Some("archive")))
            .unwrap();
        idx.add(&concept_with_status("/retired.md", Some("retired")))
            .unwrap();
        let hits = idx.search(&SearchQuery::new(vec!["rust".into()])).unwrap();
        assert_eq!(paths(&hits), ["/archive.md"]);
    }

    #[test]
    fn is_active_status_matches_leniency() {
        assert!(is_active_status(None));
        assert!(is_active_status(Some("active")));
        assert!(is_active_status(Some("archive"))); // unknown ⇒ active
        assert!(!is_active_status(Some("superseded")));
        assert!(!is_active_status(Some("retired")));
    }

    #[test]
    fn bm25_score_exact_value_pins() {
        let p = bm25::Params::default();
        assert!((p.k1 - 1.2).abs() < 1e-9 && (p.b - 0.75).abs() < 1e-9);

        // (a) n 4, df 2, tf 3, doc_len 4, avgdl 4.0 — right side computed
        // as an independent expression: idf ln(1 + 2.5/2.5) = ln(2) times
        // the tf term 3·2.2 / (3 + 1.2·(1 − 0.75 + 0.75·4/4)) = 6.6 / 4.2.
        let got = bm25_score(&p, 4, 2, 3, 4, 4.0);
        let want = ((4.0f32 - 2.0 + 0.5) / (2.0 + 0.5)).ln_1p() * 6.6 / 4.2;
        assert!((got - want).abs() < 1e-4, "(a) {got} vs {want}");

        // (b) df 0 contributes nothing.
        assert_eq!(bm25_score(&p, 4, 0, 3, 4, 4.0), 0.0);

        // (c) avgdl 0.0 ⇒ the length ratio is treated as 1.0 (pure
        // tf-saturation, no NaN): score = idf · tf·(k1+1)/(tf+k1).
        // At the pinned n 1, df 1 the Lucene-form idf is ln(1 + 0.5/1.5).
        let got = bm25_score(&p, 1, 1, 1, 5, 0.0);
        let want = ((1.0f32 - 1.0 + 0.5) / (1.0 + 0.5)).ln_1p() * (2.2 / (1.0 + 1.2));
        assert!((got - want).abs() < 1e-4, "(c) {got} vs {want}");
        // doc_len never leaks through when avgdl is 0: at df 1 of n 2 the
        // idf is ln(2), so the whole score is the pinned pure-tf form
        // ln(2.0) · (2.2 / (1.0 + 1.2)) — identical for any doc_len.
        let got = bm25_score(&p, 2, 1, 1, 5, 0.0);
        let want = 2.0f32.ln() * (2.2 / (1.0 + 1.2));
        assert!((got - want).abs() < 1e-4, "(c') {got} vs {want}");
        let got4 = bm25_score(&p, 2, 1, 1, 4, 0.0);
        assert!((got - got4).abs() < 1e-9, "doc_len leaked: {got} vs {got4}");
    }

    /// Long/short corpus for the length-normalization tests: `/a.md` =
    /// title "Alpha Note" (tokens `alpha`, `note`) plus body `rust rust
    /// rust` and the 197 filler tokens `n001..n197` = 202 tokens; `/b.md`
    /// = title "Beta Note" plus body `rust` = 3 tokens. Both match `rust`;
    /// corpus avgdl is (202 + 3) / 2 = 102.5.
    fn long_short_index() -> InMemoryIndex {
        let fillers: String = (1..=197).map(|i| format!(" n{i:03}")).collect();
        let mut idx = InMemoryIndex::new();
        idx.add(&concept(
            "/a.md",
            "Alpha Note",
            &format!("rust rust rust{fillers}"),
        ))
        .unwrap();
        idx.add(&concept("/b.md", "Beta Note", "rust")).unwrap();
        idx
    }

    #[test]
    fn bm25_length_normalization_long_doc_loses() {
        // Acceptance #3's in-memory analog: on equal term matches, the
        // short focused concept must outrank the long enriched one.
        let idx = long_short_index();
        let hits = idx.search(&SearchQuery::new(vec!["rust".into()])).unwrap();
        assert_eq!(paths(&hits), ["/b.md", "/a.md"]);

        // Scores computed independently from the BM25 formula: corpus
        // lengths 202 and 3 → avgdl 102.5; df 2 of n 2 →
        // idf = ln(1 + 0.5/2.5) = ln(1.2); k1 1.2, b 0.75.
        let idf = 1.2f32.ln();
        let want_a = idf * 6.6 / (3.0 + 1.2 * (1.0 - 0.75 + 0.75 * 202.0 / 102.5));
        let want_b = idf * 2.2 / (1.0 + 1.2 * (1.0 - 0.75 + 0.75 * 3.0 / 102.5));
        assert!(
            (hits[0].score - want_b).abs() < 1e-6,
            "short doc: {} vs {want_b}",
            hits[0].score
        );
        assert!(
            (hits[1].score - want_a).abs() < 1e-6,
            "long doc: {} vs {want_a}",
            hits[1].score
        );
        // Pinned magnitudes: the index's exact f32 scores for this corpus
        // (the plan's literals 0.23712/0.30243 were approximations, ~5e-5
        // off the exact 202/102.5-corpus values).
        assert!(
            (hits[1].score - 0.23717058).abs() < 1e-6,
            "long doc: {}",
            hits[1].score
        );
        assert!(
            (hits[0].score - 0.30241647).abs() < 1e-6,
            "short doc: {}",
            hits[0].score
        );
    }

    #[test]
    fn duplicate_query_terms_do_not_double_count() {
        let idx = long_short_index();
        let once = idx.search(&SearchQuery::new(vec!["rust".into()])).unwrap();
        let twice = idx
            .search(&SearchQuery::new(vec!["rust".into(), "rust".into()]))
            .unwrap();
        assert_eq!(paths(&twice), paths(&once));
        for (dup, single) in twice.iter().zip(&once) {
            assert!(
                (dup.score - single.score).abs() < 1e-6,
                "{}: {} vs {}",
                dup.concept_path,
                dup.score,
                single.score
            );
        }
    }

    #[test]
    fn readd_replaces_postings_in_memory_oracle() {
        let mut idx = InMemoryIndex::new();
        idx.add(&concept("/v.md", "V1", "alpha zebra")).unwrap();
        idx.add(&concept("/v.md", "V2", "gamma delta")).unwrap();

        // v1's postings are stripped, not accumulated: no resurrection of
        // dropped terms.
        assert!(
            idx.search(&SearchQuery::new(vec!["alpha".into()]))
                .unwrap()
                .is_empty(),
            "superseded content must not stay searchable"
        );
        assert!(
            idx.search(&SearchQuery::new(vec!["zebra".into()]))
                .unwrap()
                .is_empty(),
            "superseded content must not stay searchable"
        );
        assert_eq!(
            paths(&idx.search(&SearchQuery::new(vec!["gamma".into()])).unwrap()),
            ["/v.md"]
        );

        // df stays sane: "zebra" matches only /c3.md (df 1). With the old
        // accumulating add the stale /v.md posting still matched "zebra",
        // so the resurrected doc would fail the paths assertion below
        // before any score is compared.
        idx.add(&concept("/c3.md", "C3", "zebra shared")).unwrap();
        let hits = idx.search(&SearchQuery::new(vec!["zebra".into()])).unwrap();
        assert_eq!(paths(&hits), ["/c3.md"]);
        // Both docs tokenize to 3 tokens → avgdl 3, ratio 1; df 1 of n 2
        // → idf ln(1 + 1.5/1.5) = ln(2); tf term 1·2.2/(1 + 1.2). Had the
        // stale posting been scored instead: the docs map is path-keyed, so
        // n stays 2 while df would rise to 2, idf ln(1 + 0.5/2.5) = ln(1.2)
        // ≈ 0.18 — visibly below ln(2) ≈ 0.69.
        let want = 2.0f32.ln() * (2.2 / (1.0 + 1.2));
        assert!(
            (hits[0].score - want).abs() < 1e-4,
            "{} vs {want}",
            hits[0].score
        );
    }

    #[test]
    fn lexical_engines_ignore_mode() {
        // `SearchQuery.mode` selects which sources a COMPOSER combines —
        // the score engines never read it: an index searched under
        // VectorOnly still returns lexical matches (the composer builds
        // the pools; the index only ever scores lexically).
        let mut idx = InMemoryIndex::new();
        idx.add(&concept("/a.md", "Deploy Rust", "how to deploy"))
            .ok();
        let mut q = SearchQuery::new(vec!["rust".into()]);
        q.mode = SearchMode::VectorOnly;
        assert_eq!(paths(&idx.search(&q).unwrap()), ["/a.md"]);
        q.mode = SearchMode::Lexical;
        assert_eq!(paths(&idx.search(&q).unwrap()), ["/a.md"]);
    }

    #[test]
    fn rrf_fuse_math_and_ties() {
        let params = RrfParams::default();
        // The default constants (spec-set: k 60, equal weights).
        assert_eq!(params.rrf_k, 60.0);
        assert_eq!(params.w_lex, 1.0);
        assert_eq!(params.w_vec, 1.0);

        // Hand-computed main case: lex [A,B] (A rank 1, B rank 2), vec
        // [B,C] (B rank 1, C rank 2). Ranks are 1-based positions in the
        // already-ranked pools: B = 1/(60+2) + 1/(60+1), A = 1/(60+1),
        // C = 1/(60+2) → order B, A, C.
        let lex = vec![("/a.md".to_string(), 30.0), ("/b.md".to_string(), 20.0)];
        let vectors = vec![("/b.md".to_string(), 0.9), ("/c.md".to_string(), 0.8)];
        let out = rrf_fuse(&lex, &vectors, &params);
        // A nested fn (not a closure) so the return's lifetime ties to
        // the argument properly.
        fn paths_of(o: &[(String, f32)]) -> Vec<&str> {
            o.iter().map(|(p, _)| p.as_str()).collect()
        }
        fn score_of(o: &[(String, f32)], p: &str) -> f32 {
            o.iter()
                .find(|(q, _)| q == p)
                .unwrap_or_else(|| panic!("{p} missing from {o:?}"))
                .1
        }
        assert_eq!(paths_of(&out), vec!["/b.md", "/a.md", "/c.md"]);
        let want_b = 1.0 / (60.0 + 2.0) + 1.0 / (60.0 + 1.0);
        let want_a = 1.0 / (60.0 + 1.0);
        let want_c = 1.0 / (60.0 + 2.0);
        assert!(
            (score_of(&out, "/b.md") - want_b).abs() < 1e-6,
            "B: {} vs {want_b}",
            score_of(&out, "/b.md")
        );
        assert!(
            (score_of(&out, "/a.md") - want_a).abs() < 1e-6,
            "A: {} vs {want_a}",
            score_of(&out, "/a.md")
        );
        assert!(
            (score_of(&out, "/c.md") - want_c).abs() < 1e-6,
            "C: {} vs {want_c}",
            score_of(&out, "/c.md")
        );

        // Tie + pool ORDER (not path order) drives ranks: /b.md is at
        // lex rank 1 (its score leads), tying /z.md at vec rank 1 —
        // both fuse to 1/(60+1) and the path-ascending tiebreak orders
        // them, with the rank-2 lexical candidate last.
        let lex = vec![("/b.md".to_string(), 9.0), ("/a.md".to_string(), 2.0)];
        let vectors = vec![("/z.md".to_string(), 0.5)];
        let out = rrf_fuse(&lex, &vectors, &params);
        assert_eq!(paths_of(&out), vec!["/b.md", "/z.md", "/a.md"]);
        assert_eq!(
            score_of(&out, "/b.md"),
            score_of(&out, "/z.md"),
            "same rank in opposite pools fuses to the same score"
        );
        assert!((score_of(&out, "/b.md") - 1.0 / 61.0).abs() < 1e-6);
        assert!((score_of(&out, "/a.md") - 1.0 / 62.0).abs() < 1e-6);

        // An absent pool contributes that term as 0 — from either side.
        let out = rrf_fuse(&[("/a.md".to_string(), 1.0)], &[], &params);
        assert_eq!(paths_of(&out), vec!["/a.md"]);
        assert!((out[0].1 - 1.0 / 61.0).abs() < 1e-6);
        let out = rrf_fuse(&[], &[("/a.md".to_string(), 1.0)], &params);
        assert_eq!(paths_of(&out), vec!["/a.md"]);
        assert!((out[0].1 - 1.0 / 61.0).abs() < 1e-6);

        // Both pools empty → empty output.
        assert!(rrf_fuse(&[], &[], &params).is_empty());

        // A vector-only pool ranks by its own positions: 1/61, 1/62,
        // 1/63 — order preserved.
        let vectors = vec![
            ("/b.md".to_string(), 0.9),
            ("/c.md".to_string(), 0.8),
            ("/d.md".to_string(), 0.7),
        ];
        let out = rrf_fuse(&[], &vectors, &params);
        assert_eq!(paths_of(&out), vec!["/b.md", "/c.md", "/d.md"]);
        assert!((out[0].1 - 1.0 / 61.0).abs() < 1e-6);
        assert!((out[1].1 - 1.0 / 62.0).abs() < 1e-6);
        assert!((out[2].1 - 1.0 / 63.0).abs() < 1e-6);

        // The weights scale their own term — a shrunk vector weight
        // flips the default ordering (under equal weights /b.md's fused
        // score leads; with w_vec 0.01 the lexical rank alone wins).
        let params = RrfParams {
            rrf_k: 60.0,
            w_lex: 1.0,
            w_vec: 0.01,
        };
        let lex = vec![("/a.md".to_string(), 1.0), ("/b.md".to_string(), 0.5)];
        let vectors = vec![("/b.md".to_string(), 0.9)];
        let out = rrf_fuse(&lex, &vectors, &params);
        assert_eq!(paths_of(&out), vec!["/a.md", "/b.md"]);
        let want_a = 1.0 / 61.0;
        let want_b = 1.0 / 62.0 + 0.01 / 61.0;
        assert!(
            (score_of(&out, "/a.md") - want_a).abs() < 1e-6,
            "A: {} vs {want_a}",
            score_of(&out, "/a.md")
        );
        assert!(
            (score_of(&out, "/b.md") - want_b).abs() < 1e-6,
            "B: {} vs {want_b}",
            score_of(&out, "/b.md")
        );
        // ...and w_lex scales the lexical term (5.0, no vector pool:
        // 5/61 and 5/62).
        let params = RrfParams {
            rrf_k: 60.0,
            w_lex: 5.0,
            w_vec: 1.0,
        };
        let lex = vec![("/a.md".to_string(), 1.0), ("/b.md".to_string(), 0.5)];
        let out = rrf_fuse(&lex, &[], &params);
        assert!(
            (score_of(&out, "/a.md") - 5.0 / 61.0).abs() < 1e-6,
            "w_lex scales: {}",
            score_of(&out, "/a.md")
        );
        assert!(
            (score_of(&out, "/b.md") - 5.0 / 62.0).abs() < 1e-6,
            "w_lex scales: {}",
            score_of(&out, "/b.md")
        );
    }

    #[test]
    fn cosine_basics() {
        // Same direction (any positive scale) → 1.
        assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!((cosine(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]) - 1.0).abs() < 1e-6);
        // Orthogonal → 0; opposite → −1.
        assert!(cosine(&[1.0, 0.0], &[0.0, 5.0]).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[-3.0, 0.0]) + 1.0).abs() < 1e-6);
        // The zero-norm guard returns 0.0 (not the 1.0 self-similarity
        // trap, not NaN).
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine(&[1.0, 1.0], &[0.0, 0.0]), 0.0);
        // 0-length inputs → 0.0.
        assert_eq!(cosine(&[], &[]), 0.0);
        // The hybrid acceptance test's pair, by value: the near-parallel
        // query vector against the doc vector ≈ 0.898.
        let c = cosine(&[0.9, 0.44, 0.0], &[1.0, 0.0, 0.0]);
        assert!(
            (c - 0.9 / (0.9f32 * 0.9 + 0.44 * 0.44).sqrt()).abs() < 1e-6,
            "{c}"
        );
    }
}
