//! Encrypted per-scope inverted index in SQLite.
//!
//! Terms are HMAC-SHA256 tokens (per-scope key) so plaintext never touches
//! the database; document payloads (title + snippet) are AEAD envelopes.
//! Results are ranked by term frequency, matching the in-memory reference
//! index in mycelium-core (oracle equivalence via `searchable_text`).

use mycelium_core::search::{SearchIndex, SearchQuery, SearchResult, searchable_text, tokenize};
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
    pub async fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, IndexError> {
        if query.terms.is_empty() {
            return Ok(vec![]);
        }
        // Score = sum of tf over all query terms (AND not required —
        // matches the in-memory oracle's OR semantics). One grouped
        // query per DISTINCT term instead of one round-trip per term:
        // a 32-term query on a large library is 32 index seeks either
        // way, but the grouping happens in SQLite, not in N fetches.
        let mut distinct: Vec<String> = query.terms.iter().map(|t| t.to_lowercase()).collect();
        distinct.sort();
        distinct.dedup();
        let tokens: Vec<String> = distinct.iter().map(|t| self.keys.token(t)).collect();
        let mut scores: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for chunk in tokens.chunks(16) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT concept_path, SUM(tf) FROM search_tokens \
                 WHERE scope = ? AND token IN ({placeholders}) \
                 GROUP BY concept_path"
            );
            let mut q = sqlx::query_as::<_, (String, i64)>(&sql).bind(&self.scope_id);
            for token in chunk {
                q = q.bind(token);
            }
            for (path, tf) in q.fetch_all(&self.pool).await? {
                *scores.entry(path).or_insert(0) += tf;
            }
        }
        // Batch doc load: one query for all scored paths instead of a
        // round-trip per path (a broad term on a big library scores
        // hundreds of paths).
        let mut results = Vec::with_capacity(scores.len());
        for chunk in scores.keys().collect::<Vec<_>>().chunks(64) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT concept_path, payload FROM search_docs \
                 WHERE scope = ? AND concept_path IN ({placeholders})"
            );
            let mut q = sqlx::query_as::<_, (String, Vec<u8>)>(&sql).bind(&self.scope_id);
            for path in chunk {
                q = q.bind(path);
            }
            for (path, payload_bytes) in q.fetch_all(&self.pool).await? {
                let plain = match aead_open(&payload_bytes, path.as_bytes(), self.keys.index_dek())
                {
                    Ok(p) => p,
                    Err(_) => continue, // corrupt row: skip, don't fail search
                };
                let payload: DocPayload = match serde_json::from_slice(&plain) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let score = scores.get(&path).copied().unwrap_or(0);
                results.push(SearchResult {
                    concept_path: path,
                    title: payload.title,
                    snippet: payload.snippet,
                    score: score as f32,
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

        // Tokenize the canonical searchable text.
        let text = searchable_text(concept);
        let mut token_counts: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        for token in tokenize(&text) {
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
        sqlx::query("INSERT INTO search_docs (scope, concept_path, payload) VALUES (?, ?, ?)")
            .bind(&self.scope_id)
            .bind(&concept.source_path)
            .bind(&sealed)
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
        ];
        for c in &concepts {
            index.add_async(c).await.unwrap();
            oracle.add(c).ok();
        }
        for term in ["rust", "deploy", "test", "zebra"] {
            let encrypted = index
                .search(&SearchQuery::new(vec![term.into()]))
                .await
                .unwrap();
            let memory = oracle.search(&SearchQuery::new(vec![term.into()])).unwrap();
            let paths_e: Vec<&String> = encrypted.iter().map(|r| &r.concept_path).collect();
            let paths_m: Vec<&String> = memory.iter().map(|r| &r.concept_path).collect();
            assert_eq!(paths_e, paths_m, "term {term}: ordering mismatch");
        }
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
