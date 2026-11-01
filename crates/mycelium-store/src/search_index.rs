//! Encrypted per-scope inverted index in SQLite.
//!
//! Terms are HMAC-SHA256 tokens (per-scope key) so plaintext never touches
//! the database; document payloads (title + snippet) are AEAD envelopes.
//! Results are ranked by BM25 through the shared `mycelium_core::search`
//! scorer, matching the in-memory reference index in mycelium-core (oracle
//! equivalence via `searchable_text`): `df` is the scoped posting-list size
//! per token, `N` is the scope's row count (`COUNT(*)`) and `avgdl` its mean
//! length over `search_docs.length` (§3.2a-11), and each doc's length is
//! that column, captured at `add`.

use mycelium_core::search::{
    NON_ACTIVE_STATUSES, SearchIndex, SearchQuery, SearchResult, StatusFilter, bm25, bm25_score,
    searchable_text, tokenize,
};
use mycelium_crypto::aead::{aead_open, aead_seal};
use mycelium_crypto::keys::{MasterKey, ServiceKey};
use mycelium_crypto::store_keys::IndexKeys;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("crypto error: {0}")]
    Crypto(#[from] mycelium_crypto::CryptoError),
    #[error("corrupt index payload for {path}: {reason}")]
    Corrupt { path: String, reason: String },
}

/// The encrypted index for one scope, backed by the shared SQLite pool.
pub struct EncryptedIndex {
    pool: SqlitePool,
    scope_id: String,
    keys: IndexKeys,
}

#[derive(Serialize, Deserialize)]
struct DocPayload {
    title: String,
    snippet: String,
}

impl EncryptedIndex {
    /// Open the index for a specific user's private bundle.
    ///
    /// The scope id is `user:<user_id>` so users sharing the database never
    /// collide in the token/doc tables.
    pub fn for_user(pool: SqlitePool, user_id: &str, master_key: &MasterKey) -> Self {
        Self {
            pool,
            scope_id: format!("user:{user_id}"),
            keys: IndexKeys::from_master_key(master_key),
        }
    }

    /// Open the index for a global (service-key) scope.
    ///
    /// `namespace` separates the two service-key scopes so their tokens
    /// and docs never collide: `"skills"` (global skills shelf) and
    /// `"library"` (book catalogs). The scope id is `global:<namespace>`.
    pub fn for_service(pool: SqlitePool, service_key: &ServiceKey, namespace: &str) -> Self {
        Self {
            pool,
            scope_id: format!("global:{namespace}"),
            keys: IndexKeys::from_service_key(service_key),
        }
    }

    /// Remove a concept's postings and doc (call before re-adding).
    pub async fn remove(&self, concept_path: &str) -> Result<(), IndexError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM search_tokens WHERE scope = ? AND concept_path = ?")
            .bind(&self.scope_id)
            .bind(concept_path)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM search_docs WHERE scope = ? AND concept_path = ?")
            .bind(&self.scope_id)
            .bind(concept_path)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Query the index; returns ranked results (score desc, path asc).
    ///
    /// A doc row missing for a scored path (e.g. a concurrent `remove`
    /// between the token scan and the doc load) is skipped, not an error —
    /// search stays correct under concurrent mutation.
    ///
    /// [`StatusFilter::ActiveOnly`] (the default) cuts the non-active
    /// statuses (`NON_ACTIVE_STATUSES`) in SQL, before scoring, on the
    /// plaintext `lifecycle_status` column — no payload decrypt is needed
    /// to apply it.
    pub async fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, IndexError> {
        if query.terms.is_empty() {
            return Ok(vec![]);
        }
        // Query terms are lowercased and deduped before scoring (§3.2a-2):
        // under BM25 a repeated term must not double its weight, and the
        // in-memory oracle dedups the same way. One grouped query per chunk
        // of DISTINCT terms (grouping happens in SQLite, not in N fetches);
        // score is OR-style sum over terms (AND is not required — matches the
        // oracle).
        let mut distinct: Vec<String> = query.terms.iter().map(|t| t.to_lowercase()).collect();
        distinct.sort();
        distinct.dedup();
        let tokens: Vec<String> = distinct.iter().map(|t| self.keys.token(t)).collect();
        let active_only = query.status_filter == StatusFilter::ActiveOnly;

        // Corpus statistics (§3.3, §3.2a-11): `N = COUNT(*)`, `avgdl =
        // COALESCE(SUM(length), 0) / N` over the WHOLE scope. N counts EVERY
        // scoped row — a NULL-length row still owns postings, so it must be
        // in N or `df` can exceed N and drive idf negative (§3.2a-11). This
        // is exactly the oracle's `docs.len()` (it counts every doc, active
        // or not, in both N and avgdl). NULL lengths add 0 to the sum but
        // count in N; `avgdl == 0` is still guarded below (no NaN).
        let (n_docs, sum_len): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(length), 0) FROM search_docs WHERE scope = ?",
        )
        .bind(&self.scope_id)
        .fetch_one(&self.pool)
        .await?;
        let n_docs_u = n_docs.max(0) as usize;
        let avgdl = if n_docs > 0 {
            sum_len as f32 / n_docs as f32
        } else {
            0.0
        };
        let params = bm25::Params::default();

        // One grouped token pass per chunk of distinct terms. It selects
        // `(token, concept_path, tf)` over ALL docs — deliberately NOT
        // lifecycle-filtered — so per-token `df` is the SCOPED POSTING-LIST
        // size (§3.3), identical to the oracle's `df = docs.len()`. Each
        // `search_tokens` row is one distinct (token, path) (its PK), so one
        // `df += 1` per row counts the posting list; `tf_by_path` keeps each
        // (path, token) tf for scoring. The lifecycle cut rides on the doc
        // load below, not here — which is exactly what keeps `df` correct
        // under ActiveOnly (a hidden doc still contributes to df, only not to
        // the results).
        let mut df: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut tf_by_path: std::collections::HashMap<
            String,
            std::collections::HashMap<String, u32>,
        > = std::collections::HashMap::new();
        for chunk in tokens.chunks(16) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT token, concept_path, tf FROM search_tokens \
                 WHERE scope = ? AND token IN ({placeholders})"
            );
            let mut q = sqlx::query_as::<_, (String, String, i64)>(&sql).bind(&self.scope_id);
            for token in chunk {
                q = q.bind(token);
            }
            for (token, path, tf) in q.fetch_all(&self.pool).await? {
                *df.entry(token.clone()).or_insert(0) += 1;
                *tf_by_path
                    .entry(path)
                    .or_default()
                    .entry(token)
                    .or_insert(0) += tf as u32;
            }
        }

        // Batch doc load for scored paths (one round-trip per chunk of 64),
        // now carrying `length` (COALESCE 0 for the transient NULL case —
        // §3.2a-11: over-boosted but still returned). Under ActiveOnly the
        // lifecycle cut is a SQL predicate on the plaintext status column —
        // hidden docs are absent from the RESULTS, not from df/avgdl above.
        // A path whose doc row vanished mid-scan is simply not returned (skip,
        // not error). Keys are sorted so the `IN (…)` batch sequence is
        // deterministic run-to-run (not HashMap-order), per the determinism
        // convention.
        let mut results = Vec::with_capacity(tf_by_path.len());
        let mut scored_paths: Vec<&String> = tf_by_path.keys().collect();
        scored_paths.sort();
        for chunk in scored_paths.chunks(64) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = if active_only {
                let status_holders = vec!["?"; NON_ACTIVE_STATUSES.len()].join(",");
                format!(
                    "SELECT concept_path, payload, COALESCE(length, 0) FROM search_docs \
                     WHERE scope = ? AND concept_path IN ({placeholders}) \
                       AND (lifecycle_status IS NULL \
                            OR lifecycle_status NOT IN ({status_holders}))"
                )
            } else {
                format!(
                    "SELECT concept_path, payload, COALESCE(length, 0) FROM search_docs \
                     WHERE scope = ? AND concept_path IN ({placeholders})"
                )
            };
            let mut q = sqlx::query_as::<_, (String, Vec<u8>, i64)>(&sql).bind(&self.scope_id);
            for path in chunk {
                q = q.bind(path);
            }
            if active_only {
                for status in NON_ACTIVE_STATUSES {
                    q = q.bind(status);
                }
            }
            for (path, payload_bytes, doc_len) in q.fetch_all(&self.pool).await? {
                let plain = match aead_open(&payload_bytes, path.as_bytes(), self.keys.index_dek())
                {
                    Ok(p) => p,
                    Err(_) => continue, // corrupt row: skip, don't fail search
                };
                let payload: DocPayload = match serde_json::from_slice(&plain) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                // score = Σ over the DISTINCT terms of the term's BM25
                // contribution to THIS doc, summed in the sorted `tokens`
                // order (deterministic across re-runs). A term this doc lacks
                // (tf 0) or the corpus has never seen (df 0) contributes 0 —
                // matching the oracle's per-doc sum. `n` is clamped to `df`
                // (§3.2a-11): the callee documents `df ≤ n_docs`, and `search`
                // reads N and the postings as separate autocommit queries, so
                // a concurrent `add_async` commit between them can transiently
                // raise a token's df above the N already captured — the clamp
                // keeps idf non-negative for that transient (NULL-length rows
                // no longer trip it, since N is now COUNT(*)).
                let path_tfs = tf_by_path.get(&path);
                let mut score = 0.0f32;
                for token in &tokens {
                    let term_df = df.get(token).copied().unwrap_or(0);
                    if term_df == 0 {
                        continue;
                    }
                    let tf = path_tfs.and_then(|m| m.get(token)).copied().unwrap_or(0);
                    if tf == 0 {
                        continue;
                    }
                    let term_n = n_docs_u.max(term_df);
                    score += bm25_score(&params, term_n, term_df, tf, doc_len as u32, avgdl);
                }
                results.push(SearchResult {
                    concept_path: path,
                    title: payload.title,
                    snippet: payload.snippet,
                    score,
                });
            }
        }
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.concept_path.cmp(&b.concept_path))
        });
        Ok(results)
    }
}

impl SearchIndex for EncryptedIndex {
    type Error = IndexError;

    fn add(&mut self, concept: &mycelium_core::Concept) -> Result<(), Self::Error> {
        // Synchronous trait over async internals: block on a small runtime.
        // The index is used from async contexts via the async-native
        // methods; this impl exists to satisfy the core trait.
        let fut = self.add_async(concept);
        futures_executor::block_on(fut)
    }

    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, Self::Error> {
        let fut = self.search(query);
        futures_executor::block_on(fut)
    }
}

impl EncryptedIndex {
    /// Async-native add (the trait method blocks on this).
    pub async fn add_async(&self, concept: &mycelium_core::Concept) -> Result<(), IndexError> {
        let title = concept
            .frontmatter
            .title
            .clone()
            .unwrap_or_else(|| concept.source_path.clone());
        let snippet: String = concept
            .body
            .chars()
            .take(200)
            .collect::<String>()
            .trim()
            .to_string();
        let payload = DocPayload { title, snippet };
        let payload_bytes = serde_json::to_vec(&payload).map_err(|e| IndexError::Corrupt {
            path: concept.source_path.clone(),
            reason: e.to_string(),
        })?;
        let sealed = aead_seal(
            &payload_bytes,
            concept.source_path.as_bytes(),
            self.keys.index_dek(),
        )?;

        // Tokenize the canonical searchable text. The token count (repeats
        // included, equal to Σ tf over `token_counts`) is this doc's BM25
        // length — bound on the doc row so scoring reads it without
        // re-tokenizing (spec §3.3).
        let text = searchable_text(concept);
        let tokens = tokenize(&text);
        let doc_len = tokens.len() as i64;
        let mut token_counts: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        for token in tokens {
            *token_counts.entry(token).or_insert(0) += 1;
        }

        let mut tx = self.pool.begin().await?;
        // Replace any prior postings/doc for this path.
        sqlx::query("DELETE FROM search_tokens WHERE scope = ? AND concept_path = ?")
            .bind(&self.scope_id)
            .bind(&concept.source_path)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM search_docs WHERE scope = ? AND concept_path = ?")
            .bind(&self.scope_id)
            .bind(&concept.source_path)
            .execute(&mut *tx)
            .await?;
        for (term, tf) in token_counts {
            let token = self.keys.token(&term);
            sqlx::query(
                "INSERT INTO search_tokens (scope, token, concept_path, tf) VALUES (?, ?, ?, ?)",
            )
            .bind(&self.scope_id)
            .bind(&token)
            .bind(&concept.source_path)
            .bind(tf as i64)
            .execute(&mut *tx)
            .await?;
        }
        // The doc row is rebuilt wholesale above, so the lifecycle status
        // rides along on every re-add — stored AS-WRITTEN (absent status is
        // SQL NULL, unknown values kept verbatim); leniency belongs to
        // `is_active_status` at query time, not to normalization here.
        sqlx::query(
            "INSERT INTO search_docs (scope, concept_path, payload, length, lifecycle_status) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&self.scope_id)
        .bind(&concept.source_path)
        .bind(&sealed)
        .bind(doc_len)
        .bind(concept.frontmatter.status.as_deref())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use mycelium_core::concept::{Concept, Frontmatter};
    use mycelium_core::search::is_active_status;

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

    fn concept_with_status(path: &str, title: &str, body: &str, status: Option<&str>) -> Concept {
        Concept::new(
            Frontmatter {
                concept_type: "Note".into(),
                title: Some(title.into()),
                status: status.map(str::to_string),
                ..Default::default()
            },
            body.into(),
            path.into(),
        )
    }

    /// The same query with the lifecycle filter lifted.
    fn query_all(terms: Vec<String>) -> SearchQuery {
        SearchQuery {
            terms,
            include_global: false,
            status_filter: StatusFilter::All,
        }
    }

    fn paths(results: &[SearchResult]) -> Vec<String> {
        results.iter().map(|r| r.concept_path.clone()).collect()
    }

    async fn test_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn add_and_search_round_trip() {
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        index
            .add_async(&concept(
                "/a.md",
                "Deploy Rust Service",
                "how to deploy rust",
            ))
            .await
            .unwrap();
        index
            .add_async(&concept("/b.md", "Cooking Pasta", "boil water"))
            .await
            .unwrap();
        let results = index
            .search(&SearchQuery::new(vec!["deploy".into()]))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].concept_path, "/a.md");
        assert_eq!(results[0].title, "Deploy Rust Service");
    }

    #[tokio::test]
    async fn tokens_are_opaque_in_db() {
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        index
            .add_async(&concept(
                "/a.md",
                "Secret Project",
                "classified keyword zebra",
            ))
            .await
            .unwrap();
        // Raw DB dump must not contain plaintext terms or titles.
        let dump: String = sqlx::query_scalar("SELECT group_concat(token) FROM search_tokens")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert!(!dump.contains("zebra"));
        assert!(!dump.contains("secret"));
        let docs: Vec<Vec<u8>> = sqlx::query_scalar("SELECT payload FROM search_docs")
            .fetch_all(store.pool())
            .await
            .unwrap();
        let combined = docs.concat();
        let docs_str = String::from_utf8_lossy(&combined);
        assert!(!docs_str.contains("Secret Project"));
    }

    #[tokio::test]
    async fn matches_in_memory_oracle() {
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        let mut oracle = mycelium_core::search::InMemoryIndex::new();

        let concepts = vec![
            concept("/a.md", "Rust Deploy", "rust rust rust deploy"),
            concept("/b.md", "Rust Test", "rust once test"),
            concept("/c.md", "Other", "unrelated words here"),
            concept_with_status(
                "/d.md",
                "Rust Retired",
                "rust deploy retired",
                Some("superseded"),
            ),
        ];
        for c in &concepts {
            index.add_async(c).await.unwrap();
            oracle.add(c).ok();
        }
        // Now that BOTH indexes score with BM25, ordering alone is no longer
        // an equivalence test (two scorers can agree on order while
        // disagreeing on magnitude). Assert BOTH: identical orderings AND
        // per-path score agreement (|Δ| ≤ 1e-6) for every returned path.
        //
        // The superseded `/d.md` shares "rust" and "deploy" with active docs,
        // so this corpus pins the df rule too: `df` is the scoped posting-list
        // count (includes the hidden doc), while the lifecycle filter is a
        // per-result skip — never an index cut — exactly as the oracle does.
        //
        // Multi-term queries exercise the sum-over-terms path; the duplicate
        // cases pin the dedup (§3.2a-2 — a repeated term must not double).
        let queries: Vec<Vec<&str>> = vec![
            vec!["rust"],
            vec!["deploy"],
            vec!["test"],
            vec!["zebra"],
            vec!["rust", "deploy"],
            vec!["rust", "rust"],
            vec!["rust", "deploy", "rust"],
        ];
        for filter in [StatusFilter::ActiveOnly, StatusFilter::All] {
            for terms in &queries {
                let query = SearchQuery {
                    terms: terms.iter().map(|t| (*t).to_string()).collect(),
                    include_global: false,
                    status_filter: filter,
                };
                let encrypted = index.search(&query).await.unwrap();
                let memory = oracle.search(&query).unwrap();
                assert_eq!(
                    paths(&encrypted),
                    paths(&memory),
                    "terms {terms:?} filter {filter:?}: ordering mismatch"
                );
                assert_eq!(
                    encrypted.len(),
                    memory.len(),
                    "terms {terms:?} filter {filter:?}: length mismatch"
                );
                for (enc, mem) in encrypted.iter().zip(&memory) {
                    assert_eq!(
                        enc.concept_path, mem.concept_path,
                        "terms {terms:?} filter {filter:?}: path pairing mismatch"
                    );
                    assert!(
                        (enc.score - mem.score).abs() <= 1e-6,
                        "terms {terms:?} filter {filter:?} path {}: \
                         store score {} vs oracle {} (|Δ| > 1e-6)",
                        enc.concept_path,
                        enc.score,
                        mem.score
                    );
                }
            }
        }
        // The superseded doc is reachable only with the filter lifted.
        assert!(
            index
                .search(&SearchQuery::new(vec!["retired".into()]))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            paths(
                &index
                    .search(&query_all(vec!["retired".into()]))
                    .await
                    .unwrap()
            ),
            vec!["/d.md".to_string()]
        );
    }

    /// Acceptance #3: on equal term matches, length normalization stops a
    /// long enriched concept from outranking a short focused one. `/a.md` is
    /// long (title + 3× "rust" + 197 fillers), `/b.md` short (title + "rust").
    /// The pre-BM25 tf-sum scorer ranked the long doc first (tf 3 > 1); BM25
    /// must rank the short doc first — in BOTH indexes.
    #[tokio::test]
    async fn bm25_short_focused_beats_long_enriched_both_indexes() {
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        let mut oracle = mycelium_core::search::InMemoryIndex::new();

        let fillers: String = (1..=197).map(|i| format!(" n{i:03}")).collect();
        let long = concept("/a.md", "Alpha Note", &format!("rust rust rust{fillers}"));
        let short = concept("/b.md", "Beta Note", "rust");
        for c in [&long, &short] {
            index.add_async(c).await.unwrap();
            oracle.add(c).ok();
        }

        let query = SearchQuery::new(vec!["rust".into()]);
        let store_paths = paths(&index.search(&query).await.unwrap());
        let memory_paths = paths(&oracle.search(&query).unwrap());
        assert_eq!(
            store_paths,
            vec!["/b.md".to_string(), "/a.md".to_string()],
            "store: short focused doc must outrank the long enriched one"
        );
        assert_eq!(
            memory_paths,
            vec!["/b.md".to_string(), "/a.md".to_string()],
            "oracle: agrees with the store"
        );
    }

    #[tokio::test]
    async fn search_length_column_tracks_token_count() {
        // `add_async` binds `search_docs.length` = the token count of the
        // concept's `searchable_text` (title tokens are PREPENDED, then the
        // `# History`-cut body). The expected value is computed the exact
        // same way the index computes it — never hand-waved.
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        let c = concept("/a.md", "Deploy Rust Service", "how to deploy rust");
        index.add_async(&c).await.unwrap();

        let expected_len = tokenize(&searchable_text(&c)).len() as i64;
        let stored: Option<i64> =
            sqlx::query_scalar("SELECT length FROM search_docs WHERE concept_path = ?")
                .bind("/a.md")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(stored, Some(expected_len), "length must equal token count");
        // Sanity: the token count equals Σ tf over the postings (the length
        // is the doc's total token count, repeats included).
        let sum_tf: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(tf), 0) FROM search_tokens WHERE concept_path = ?",
        )
        .bind("/a.md")
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(stored, Some(sum_tf), "length must equal Σ tf");
        // The exact expected count is pinned via the same tokenizer, so this
        // is a real check, not a tautology: title "Deploy Rust Service"
        // (deploy, rust, service) + body "how to deploy rust" (how, to,
        // deploy, rust) = 7 tokens.
        assert_eq!(expected_len, 7);
    }

    #[tokio::test]
    async fn readd_replaces_length() {
        // A re-add REPLACES the doc row (DELETE-then-INSERT), so a shorter
        // re-add lowers `length` rather than summing onto the old value.
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        index
            .add_async(&concept(
                "/a.md",
                "V1",
                "alpha beta gamma delta epsilon zeta eta",
            ))
            .await
            .unwrap();
        let long: Option<i64> =
            sqlx::query_scalar("SELECT length FROM search_docs WHERE concept_path = ?")
                .bind("/a.md")
                .fetch_one(store.pool())
                .await
                .unwrap();
        let short_c = concept("/a.md", "V2", "gamma");
        index.add_async(&short_c).await.unwrap();
        let short: Option<i64> =
            sqlx::query_scalar("SELECT length FROM search_docs WHERE concept_path = ?")
                .bind("/a.md")
                .fetch_one(store.pool())
                .await
                .unwrap();
        let want = tokenize(&searchable_text(&short_c)).len() as i64;
        assert_eq!(short, Some(want), "length must be the re-added count");
        assert!(
            short < long,
            "length must be replaced (short {short:?} < long {long:?}), not summed"
        );
    }

    #[tokio::test]
    async fn null_length_scoring_guard() {
        // §3.2a-11 (supersedes the amendment-5 wording): a scored doc with
        // `length = NULL` scores as length 0 — still returned, OVER-boosted,
        // never an error. The corpus-stat universe is EVERY scoped row, so
        // `n_docs = COUNT(*)` counts BOTH docs here (a NULL-length row still
        // owns postings, so it contributes to `df` and must be in `N` — that
        // is exactly the oracle's `docs.len()`); `avgdl = COALESCE(SUM,0)/N`
        // counts the NULL row in N but adds 0 to the sum. With /a (NULL) +
        // /b (length 3) sharing "zebra": N = 2, Σ = 3, avgdl = 3/2 = 1.5.
        // The over-boost (doc_len 0 → smallest length penalty) must RANK the
        // NULL-length doc first with non-negative scores — the old
        // `COUNT(length)` N (1 here) made df (2) exceed N and drove idf
        // negative, turning the over-boost into a demotion.
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        index
            .add_async(&concept("/a.md", "Alpha", "zebra alpha details"))
            .await
            .unwrap();
        index
            .add_async(&concept("/b.md", "Beta", "zebra beta"))
            .await
            .unwrap();
        sqlx::query("UPDATE search_docs SET length = NULL WHERE concept_path = ?")
            .bind("/a.md")
            .execute(store.pool())
            .await
            .unwrap();
        let hits = index
            .search(&SearchQuery::new(vec!["zebra".into()]))
            .await
            .unwrap();
        // Both present, over-boosted doc first: NULL-length /a.md ranks ABOVE
        // /b.md (§3.2a-11's over-boost intent, not the amendment-5 demotion).
        assert_eq!(
            paths(&hits),
            vec!["/a.md".to_string(), "/b.md".to_string()],
            "NULL-length doc must be over-boosted to first, both present: {hits:?}"
        );
        // Non-negative idf: N ≥ df, so no term's idf goes negative and drags
        // the sum below 0.
        for h in &hits {
            assert!(
                h.score >= 0.0,
                "BM25 idf must stay non-negative (df ≤ N): {} score {}",
                h.concept_path,
                h.score
            );
        }
        assert!(
            hits[0].score >= hits[1].score,
            "over-boosted NULL-length doc must outrank the length-bearing doc"
        );
    }

    #[tokio::test]
    async fn status_filter_hides_retired_in_store_index() {
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        index
            .add_async(&concept_with_status(
                "/active.md",
                "Active Note",
                "zebra fact",
                Some("active"),
            ))
            .await
            .unwrap();
        index
            .add_async(&concept_with_status(
                "/retired.md",
                "Retired Note",
                "zebra fact",
                Some("retired"),
            ))
            .await
            .unwrap();
        index
            .add_async(&concept_with_status(
                "/superseded.md",
                "Superseded Note",
                "zebra fact",
                Some("superseded"),
            ))
            .await
            .unwrap();
        // Default filter: the two non-active statuses are cut.
        let hits = index
            .search(&SearchQuery::new(vec!["zebra".into()]))
            .await
            .unwrap();
        assert_eq!(paths(&hits), vec!["/active.md".to_string()]);
        // StatusFilter::All reaches them again (equal scores → path asc).
        let hits = index
            .search(&query_all(vec!["zebra".into()]))
            .await
            .unwrap();
        assert_eq!(
            paths(&hits),
            vec![
                "/active.md".to_string(),
                "/retired.md".to_string(),
                "/superseded.md".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn explicit_active_status_stays_in_results() {
        // `status: active` is written as a non-NULL value; it must not be
        // filtered (absent ⇒ active does not mean non-null ⇒ hidden).
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        index
            .add_async(&concept_with_status(
                "/explicit.md",
                "Explicit",
                "zebra fact",
                Some("active"),
            ))
            .await
            .unwrap();
        let hits = index
            .search(&SearchQuery::new(vec!["zebra".into()]))
            .await
            .unwrap();
        assert_eq!(paths(&hits), vec!["/explicit.md".to_string()]);
    }

    #[tokio::test]
    async fn unknown_status_value_kept_active() {
        // Leniency mirrors core's `is_active_status`: only the known
        // non-active statuses hide a concept — unknown values and
        // differently-cased near-misses stay in default results.
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        index
            .add_async(&concept_with_status(
                "/archive.md",
                "Archive",
                "zebra fact",
                Some("archive"),
            ))
            .await
            .unwrap();
        index
            .add_async(&concept_with_status(
                "/cased.md",
                "Cased",
                "zebra fact",
                Some("SUPERSEDED"),
            ))
            .await
            .unwrap();
        let hits = index
            .search(&SearchQuery::new(vec!["zebra".into()]))
            .await
            .unwrap();
        assert_eq!(
            paths(&hits),
            vec!["/archive.md".to_string(), "/cased.md".to_string()]
        );
    }

    #[tokio::test]
    async fn sql_predicate_matches_core_is_active_status() {
        // Drift guard: the store's SQL predicate must agree with core's
        // `is_active_status` on every status shape (the expected set is
        // derived from that function, never hand-listed), so the SQL mirror
        // cannot silently diverge from the in-memory semantics.
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        let statuses: [Option<&str>; 6] = [
            None,
            Some("active"),
            Some("superseded"),
            Some("retired"),
            Some("archive"),
            Some("SUPERSEDED"),
        ];
        for (i, status) in statuses.iter().enumerate() {
            let path = format!("/s{i}.md");
            index
                .add_async(&concept_with_status(&path, "Note", "zebra fact", *status))
                .await
                .unwrap();
        }
        let hits = paths(
            &index
                .search(&SearchQuery::new(vec!["zebra".into()]))
                .await
                .unwrap(),
        );
        let expected: Vec<String> = statuses
            .iter()
            .enumerate()
            .filter(|(_, s)| is_active_status(**s))
            .map(|(i, _)| format!("/s{i}.md"))
            .collect();
        assert_eq!(hits, expected);
        // Lifted filter: every doc is reachable again.
        let all = paths(
            &index
                .search(&query_all(vec!["zebra".into()]))
                .await
                .unwrap(),
        );
        assert_eq!(all.len(), statuses.len());
    }

    #[tokio::test]
    async fn readd_updates_status() {
        // The doc row is rebuilt on re-add, so a status set later hides the
        // concept from default retrieval (and clearing it brings it back).
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        index
            .add_async(&concept_with_status("/a.md", "V1", "zebra fact", None))
            .await
            .unwrap();
        assert_eq!(
            paths(
                &index
                    .search(&SearchQuery::new(vec!["zebra".into()]))
                    .await
                    .unwrap()
            ),
            vec!["/a.md".to_string()]
        );
        // Absent status is stored as-written (NULL), not normalized.
        let stored: Option<String> =
            sqlx::query_scalar("SELECT lifecycle_status FROM search_docs WHERE concept_path = ?")
                .bind("/a.md")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(stored, None);
        index
            .add_async(&concept_with_status(
                "/a.md",
                "V2",
                "zebra fact",
                Some("superseded"),
            ))
            .await
            .unwrap();
        assert!(
            index
                .search(&SearchQuery::new(vec!["zebra".into()]))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            paths(
                &index
                    .search(&query_all(vec!["zebra".into()]))
                    .await
                    .unwrap()
            ),
            vec!["/a.md".to_string()]
        );
        let stored: Option<String> =
            sqlx::query_scalar("SELECT lifecycle_status FROM search_docs WHERE concept_path = ?")
                .bind("/a.md")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(stored.as_deref(), Some("superseded"));
        // Clearing the status again brings the doc back to default retrieval.
        index
            .add_async(&concept_with_status("/a.md", "V3", "zebra fact", None))
            .await
            .unwrap();
        assert_eq!(
            paths(
                &index
                    .search(&SearchQuery::new(vec!["zebra".into()]))
                    .await
                    .unwrap()
            ),
            vec!["/a.md".to_string()]
        );
    }

    #[tokio::test]
    async fn remove_drops_postings() {
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        index
            .add_async(&concept("/a.md", "Thing", "unique zebra word"))
            .await
            .unwrap();
        index.remove("/a.md").await.unwrap();
        let results = index
            .search(&SearchQuery::new(vec!["zebra".into()]))
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn readd_replaces_postings() {
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        index
            .add_async(&concept("/a.md", "V1", "alpha beta"))
            .await
            .unwrap();
        index
            .add_async(&concept("/a.md", "V2", "gamma delta"))
            .await
            .unwrap();
        // Old terms gone, new terms present, no duplicate rows.
        assert!(
            index
                .search(&SearchQuery::new(vec!["alpha".into()]))
                .await
                .unwrap()
                .is_empty()
        );
        let hits = index
            .search(&SearchQuery::new(vec!["gamma".into()]))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "V2");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM search_tokens")
            .fetch_one(store.pool())
            .await
            .unwrap();
        // Tokens: v2 (title) + gamma + delta (body).
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn service_scope_index_works() {
        let (_dir, store) = test_store().await;
        let service = mycelium_crypto::ServiceKey::from_bytes(&[3u8; 32]).unwrap();
        let index = EncryptedIndex::for_service(store.pool().clone(), &service, "skills");
        index
            .add_async(&concept(
                "/skills/deploy.md",
                "Deploy Skill",
                "deploy things",
            ))
            .await
            .unwrap();
        let results = index
            .search(&SearchQuery::new(vec!["deploy".into()]))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].concept_path, "/skills/deploy.md");
    }

    #[tokio::test]
    async fn scopes_are_isolated() {
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let service = mycelium_crypto::ServiceKey::from_bytes(&[4u8; 32]).unwrap();
        let user_index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        let global_index = EncryptedIndex::for_service(store.pool().clone(), &service, "skills");
        user_index
            .add_async(&concept("/private.md", "Private Note", "private zebra"))
            .await
            .unwrap();
        global_index
            .add_async(&concept("/global.md", "Global Note", "global zebra"))
            .await
            .unwrap();
        // User search sees only their doc.
        let user_hits = user_index
            .search(&SearchQuery::new(vec!["zebra".into()]))
            .await
            .unwrap();
        assert_eq!(user_hits.len(), 1);
        assert_eq!(user_hits[0].concept_path, "/private.md");
        // Global search sees only the global doc.
        let global_hits = global_index
            .search(&SearchQuery::new(vec!["zebra".into()]))
            .await
            .unwrap();
        assert_eq!(global_hits.len(), 1);
        assert_eq!(global_hits[0].concept_path, "/global.md");
    }

    #[tokio::test]
    async fn service_namespaces_are_isolated() {
        // The two service-key scopes (skills shelf, library catalogs)
        // share the service key but must never collide in the token/doc
        // tables — even on identical concept paths.
        let (_dir, store) = test_store().await;
        let service = mycelium_crypto::ServiceKey::from_bytes(&[8u8; 32]).unwrap();
        let skills = EncryptedIndex::for_service(store.pool().clone(), &service, "skills");
        let library = EncryptedIndex::for_service(store.pool().clone(), &service, "library");
        skills
            .add_async(&concept("/shared.md", "Skill Doc", "skill zebra"))
            .await
            .unwrap();
        library
            .add_async(&concept("/shared.md", "Library Doc", "library zebra"))
            .await
            .unwrap();
        let skill_hits = skills
            .search(&SearchQuery::new(vec!["zebra".into()]))
            .await
            .unwrap();
        assert_eq!(skill_hits.len(), 1);
        assert_eq!(skill_hits[0].title, "Skill Doc");
        let library_hits = library
            .search(&SearchQuery::new(vec!["zebra".into()]))
            .await
            .unwrap();
        assert_eq!(library_hits.len(), 1);
        assert_eq!(library_hits[0].title, "Library Doc");
    }

    #[tokio::test]
    async fn users_are_isolated_from_each_other() {
        // Regression: two users sharing the DB must never collide in the
        // token/doc tables, even on identical concept paths.
        let (_dir, store) = test_store().await;
        let master_a = mycelium_crypto::generate_master_key();
        let master_b = mycelium_crypto::generate_master_key();
        let index_a = EncryptedIndex::for_user(store.pool().clone(), "user-a", &master_a);
        let index_b = EncryptedIndex::for_user(store.pool().clone(), "user-b", &master_b);
        // Both users index the SAME path with different content.
        index_a
            .add_async(&concept("/notes/todo.md", "User A Note", "alpha zebra"))
            .await
            .unwrap();
        index_b
            .add_async(&concept("/notes/todo.md", "User B Note", "beta zebra"))
            .await
            .unwrap();
        // A's search still finds A's doc (B's add must not have replaced it).
        let hits_a = index_a
            .search(&SearchQuery::new(vec!["zebra".into()]))
            .await
            .unwrap();
        assert_eq!(hits_a.len(), 1);
        assert_eq!(hits_a[0].title, "User A Note");
        // B's search finds B's doc.
        let hits_b = index_b
            .search(&SearchQuery::new(vec!["zebra".into()]))
            .await
            .unwrap();
        assert_eq!(hits_b.len(), 1);
        assert_eq!(hits_b[0].title, "User B Note");
        // Two doc rows exist (one per user scope).
        let docs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM search_docs")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(docs, 2);
        // B removing their entry does not touch A's.
        index_b.remove("/notes/todo.md").await.unwrap();
        let hits_a_after = index_a
            .search(&SearchQuery::new(vec!["zebra".into()]))
            .await
            .unwrap();
        assert_eq!(hits_a_after.len(), 1);
    }

    #[tokio::test]
    async fn search_survives_missing_doc_row() {
        // A token row whose doc row vanished (concurrent remove) must be
        // skipped, not fail the whole search.
        let (_dir, store) = test_store().await;
        let master = mycelium_crypto::generate_master_key();
        let index = EncryptedIndex::for_user(store.pool().clone(), "user-1", &master);
        index
            .add_async(&concept("/a.md", "Alpha", "alpha zebra"))
            .await
            .unwrap();
        index
            .add_async(&concept("/b.md", "Beta", "beta zebra"))
            .await
            .unwrap();
        // Simulate the race: delete b's doc row but leave its token rows.
        sqlx::query("DELETE FROM search_docs WHERE concept_path = '/b.md'")
            .execute(store.pool())
            .await
            .unwrap();
        let hits = index
            .search(&SearchQuery::new(vec!["zebra".into()]))
            .await
            .unwrap();
        // Only a.md survives; no error.
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].concept_path, "/a.md");
    }
}
