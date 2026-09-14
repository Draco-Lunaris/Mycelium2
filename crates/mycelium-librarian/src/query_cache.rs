//! Query cost layers (v1 parity):
//!
//! 1. **Exact cache** — same question, unchanged bundle → instant cached
//!    answer. Invalidated by a bundle fingerprint (path + mtime + size of
//!    every concept file), so any write implicitly flushes the cache —
//!    no hooks into the write path needed.
//! 2. **Hot memory** — recently written concepts + recent answers, one
//!    tool-free LLM call. A confident hot answer also lands in the
//!    exact cache so identical repeats become instant.
//! 3. **Deep memory** — the full agent loop (the caller's run_query).
//!
//! The cache is process-global (module-level) so it survives the
//! per-request server instances of the stateless transports.

use std::collections::HashMap;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::agent::{self, AgentScopes};
use crate::llm::LlmClient;
use mycelium_store::{ConceptStore, ConceptStoreError};

const MAX_ENTRIES: usize = 200;
const DEFAULT_TTL_MS: u64 = 24 * 3_600_000;

/// Which layer answered a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuerySource {
    Cache,
    Hot,
    Deep,
}

#[derive(Debug, Clone)]
pub struct CachedQueryResult {
    pub answer: String,
    pub steps: u32,
    pub source: QuerySource,
}

struct CacheEntry {
    expires_at: std::time::Instant,
    answer: String,
}

static CACHE: Mutex<Option<HashMap<String, CacheEntry>>> = Mutex::new(None);

/// Content fingerprint of a bundle: path + updated_at of every concept
/// (registry rows — the encrypted files' mtimes are opaque names, the
/// registry is the source of truth). Any write moves the fingerprint,
/// which implicitly invalidates every cached answer.
async fn bundle_fingerprint(cs: &ConceptStore<'_>) -> Result<String, ConceptStoreError> {
    let entries = cs.list().await?;
    let mut hasher = Sha256::new();
    hasher.update(cs.scope_id());
    for e in entries {
        hasher.update(e.path.as_bytes());
        hasher.update(e.updated_at.as_bytes());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn cache_key(fingerprint: &str, question: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(fingerprint.as_bytes());
    hasher.update(b"\n");
    hasher.update(normalize(question).as_bytes());
    format!("{:x}", hasher.finalize())
}

fn normalize(question: &str) -> String {
    question.trim().to_lowercase().replace(['\n', '\t'], " ")
}

/// run_query with the cost layers. Falls through to the deep agent on
/// any miss; the deep answer feeds the cache.
pub async fn run_query_cached(
    client: &LlmClient,
    store: &ConceptStore<'_>,
    scopes: &AgentScopes<'_>,
    question: &str,
) -> Result<CachedQueryResult, agent::AgentError> {
    let fingerprint = match bundle_fingerprint(store).await {
        Ok(f) => f,
        Err(_) => {
            // Can't fingerprint → can't cache safely; go deep.
            let r = agent::run_query(client, store, scopes, question).await?;
            return Ok(CachedQueryResult {
                answer: r.answer,
                steps: r.steps,
                source: QuerySource::Deep,
            });
        }
    };
    let key = cache_key(&fingerprint, question);

    // Layer 1: exact cache — same question, unchanged bundle.
    {
        let mut guard = CACHE.lock().unwrap();
        let map = guard.get_or_insert_with(HashMap::new);
        if let Some(entry) = map.get(&key) {
            if entry.expires_at > std::time::Instant::now() {
                return Ok(CachedQueryResult {
                    answer: entry.answer.clone(),
                    steps: 0,
                    source: QuerySource::Cache,
                });
            }
            map.remove(&key);
        }
    }

    // Layer 2: hot memory — recently written concepts + recent answers,
    // one tool-free LLM call over a tiny context. A confident hot
    // answer also lands in the exact cache so identical repeats become
    // instant. UNKNOWN / empty → fall through to deep memory.
    if let Some(hot_answer) = crate::hot_memory::hot_lookup(client, store, question).await {
        let mut guard = CACHE.lock().unwrap();
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(
            key,
            CacheEntry {
                expires_at: std::time::Instant::now()
                    + std::time::Duration::from_millis(DEFAULT_TTL_MS),
                answer: hot_answer.clone(),
            },
        );
        return Ok(CachedQueryResult {
            answer: hot_answer,
            steps: 0,
            source: QuerySource::Hot,
        });
    }

    // Layer 3: deep memory — the full agent loop. Its answer feeds both
    // the exact cache and the hot working set.
    let result = agent::run_query(client, store, scopes, question).await?;
    crate::hot_memory::record_hot_query(&store.scope_id(), question, &result.answer);
    let mut guard = CACHE.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(
        key,
        CacheEntry {
            expires_at: std::time::Instant::now()
                + std::time::Duration::from_millis(DEFAULT_TTL_MS),
            answer: result.answer.clone(),
        },
    );
    while map.len() > MAX_ENTRIES {
        // Evict an arbitrary entry (HashMap order) — the TTL bounds
        // staleness; this is a size cap, not an LRU.
        if let Some(k) = map.keys().next().cloned() {
            map.remove(&k);
        } else {
            break;
        }
    }
    Ok(CachedQueryResult {
        answer: result.answer,
        steps: result.steps,
        source: QuerySource::Deep,
    })
}

/// Test hook: reset the module-level cache.
pub fn clear_query_cache() {
    *CACHE.lock().unwrap() = None;
}
