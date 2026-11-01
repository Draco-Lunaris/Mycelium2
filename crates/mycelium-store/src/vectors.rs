//! Vector store: the hybrid-retrieval row table, the in-process decoded
//! cache, and the concept-write hook queue (memory-lifecycle spec §3.3).
//!
//! Crypto posture: each row's f32 payload (little-endian, chunk per
//! element) is one AEAD envelope under the scope's [`IndexKeys`] index
//! DEK — the same key layer as the encrypted search index — with the
//! AES-GCM AAD binding the purpose explicitly: `vec:<scope_id>:<path_token>`
//! (migration 0010's column comment) — so a vector blob can never be
//! replayed as a doc payload, a revision payload, or another path's
//! row. No new key type.
//!
//! Scope model: one row set per scope (`user:<uuid>` / `global:skills`
//! / `global:library`, the [`crate::revisions::RevisionStore`]
//! identity), keyed by the same HMAC path tokens the search index uses
//! — plaintext paths never enter `search_vectors` (note the differing
//! scope column names: `search_docs.scope` vs `search_vectors.scope_id`
//! — both hold the same scope-id strings).
//!
//! Freshness: a completed `upsert` clears the row's `stale` marker (how
//! a rebuild finishes) and the ConceptStore write hooks enqueue
//! (scope_id, path_token) work into the bounded, dedup, in-process
//! queue the embed worker drains — the ConceptStore boundary owns the
//! hooks (a deleted concept has nothing to embed, so `delete` removes
//! its row directly), this module owns the rows, the decoded cache and
//! the queue, and the worker owns the stale corpus-invalidations. The
//! decoded cache is invalidated by concept writes only; a vector-only
//! change (a worker upsert) leaves the snapshot until the scope's next
//! concept write or an explicit [`invalidate_scope`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use mycelium_core::search::is_active_status;
use mycelium_crypto::aead::{aead_open, aead_seal};
use mycelium_crypto::keys::{MasterKey, ServiceKey};
use mycelium_crypto::store_keys::IndexKeys;
use sqlx::SqlitePool;
use uuid::Uuid;

/// Bound for unprocessed vector-write requests. When full, NEW items
/// are dropped at the BACK (the drain or a later write catches up; the
/// next write re-enqueues).
pub static VECTOR_QUEUE_CAPACITY: usize = 512;

/// Total vector rows the embed worker has processed (inc'd by Task 4's
/// worker, not by `upsert` itself — one counter, one writer).
pub static VECTOR_PROCESSED_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Set once per process after the first decode-mismatch data-integrity
/// notice at the cache layer — the same row would re-log on every
/// rebuild otherwise.
static DECODE_MISMATCH_WARNED: AtomicBool = AtomicBool::new(false);

/// The decoded vector cache: one entry per scope id, dropped by
/// [`invalidate_scope`] and rebuilt lazily by [`VectorStore::cache_scope`].
static SCOPE_VECTORS: Mutex<Option<HashMap<String, Arc<ScopeVectors>>>> = Mutex::new(None);

/// The write-hook queue: (scope_id, path_token) pairs pending embed
/// work. Enqueue-only via [`enqueue_write`], FIFO via [`drain_queue`].
static VECTOR_QUEUE: Mutex<VecDeque<(String, String)>> = Mutex::new(VecDeque::new());

#[derive(Debug, thiserror::Error)]
pub enum VectorError {
    #[error("vector store: {0}")]
    Db(#[from] sqlx::Error),
    #[error("vector store: crypto error: {0}")]
    Crypto(#[from] mycelium_crypto::CryptoError),
    #[error("vector store: dimension mismatch")]
    DimMismatch,
    /// A decoded envelope that does not decompose into exactly `dims`
    /// little-endian f32 chunks — storage corruption, not a query
    /// condition (load_rows errors; the cache layer skips it, warned
    /// once per process).
    #[error("vector store: data integrity: {0}")]
    DataIntegrity(String),
}

/// The result of one row write: `created` distinguishes the first-time
/// INSERT from an existing-row UPDATE (a logging hint — a concurrent
/// first-write relabels it, never changes the end state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorUpsert {
    pub created: bool,
}

/// One decoded row (`load_rows` — the strict path: any corrupt envelope
/// is an ERROR; silent row-dropping there would hide key problems).
#[derive(Debug, Clone)]
pub struct VectorRow {
    pub path_token: String,
    pub model: String,
    pub dims: u32,
    pub stale: bool,
    pub vec_f32: Vec<f32>,
    pub updated_at: String,
}

/// One decoded row inside the scope cache (the query path's filtering
/// material; stale rows and non-active concepts never appear here).
#[derive(Debug, Clone)]
pub struct CacheRow {
    pub path_token: String,
    pub model: String,
    pub dims: u32,
    pub vec_f32: Vec<f32>,
}

/// A decoded scope snapshot: every active, fresh vector row, decrypted
/// once and served per query until the scope's next concept write.
#[derive(Debug, Clone)]
pub struct ScopeVectors {
    /// The model string of the cache's first decoded row ("" when there
    /// are none) — an informational fingerprint; the query filters
    /// per-row by its own model/dims.
    pub model: String,
    /// When the snapshot was decoded (cache-age observability).
    pub built_at: std::time::Instant,
    pub rows: Vec<CacheRow>,
}

/// An un-decoded row straight from the table.
struct RawVectorRow {
    path_token: String,
    model: String,
    dims: i64,
    stale: bool,
    sealed: Vec<u8>,
    updated_at: String,
}

/// The vector store for ONE scope — the same per-scope identity as
/// [`crate::revisions::RevisionStore`] and
/// [`crate::search_index::EncryptedIndex`]: user scopes under the user's
/// master key, `global:skills` / `global:library` under the service key.
#[derive(Debug, Clone)]
pub struct VectorStore<'a> {
    pool: &'a SqlitePool,
    scope_id: String,
    keys: IndexKeys,
}

impl<'a> VectorStore<'a> {
    /// Open the vector store for a user's private bundle.
    pub fn for_user(pool: &'a SqlitePool, user_id: Uuid, master_key: &MasterKey) -> Self {
        Self {
            pool,
            scope_id: format!("user:{user_id}"),
            keys: IndexKeys::from_master_key(master_key),
        }
    }

    /// Open the vector store for a service-key scope — namespace is the
    /// registry/index namespace ("skills" / "library"); the shelves'
    /// rows never mix even for the same path token.
    pub fn for_service(pool: &'a SqlitePool, service_key: &ServiceKey, namespace: &str) -> Self {
        Self {
            pool,
            scope_id: format!("global:{namespace}"),
            keys: IndexKeys::from_service_key(service_key),
        }
    }

    /// The scope's registry id (`user:<uuid>` / `global:<ns>`).
    pub fn scope_id(&self) -> &str {
        &self.scope_id
    }

    /// The scope's HMAC path token for `path` — same scheme as
    /// `search_tokens` (one-way; the table's key alongside `scope_id`).
    pub fn path_token_of(&self, path: &str) -> String {
        self.keys.token(path)
    }

    /// The purpose-bound AAD for one row (0010's column comment).
    fn aad(&self, token: &str) -> String {
        format!("vec:{}:{}", self.scope_id, token)
    }

    /// Write one row for `path` (the embed worker's write, post-drain):
    /// a dimension mismatch fails BEFORE any write; a new (scope, token)
    /// row is an insert, an existing one is REPLACED, and either way the
    /// stale marker clears — a completed upsert is the rebuild's finish.
    pub async fn upsert(
        &self,
        path: &str,
        model: &str,
        dims: u32,
        vec_f32: &[f32],
    ) -> Result<VectorUpsert, VectorError> {
        if dims as usize != vec_f32.len() {
            return Err(VectorError::DimMismatch);
        }
        let token = self.path_token_of(path);
        let sealed = aead_seal(
            &vec_bytes(vec_f32),
            self.aad(&token).as_bytes(),
            self.keys.index_dek(),
        )?;
        let now = chrono::Utc::now().to_rfc3339();
        // INSERT-vs-UPDATE hint: SQLite's ON CONFLICT upsert reports
        // rows_affected = 1 for BOTH arms, so `changes()` cannot tell
        // them apart — a pre-read decides (the revision chain derives
        // its create/update op the same way; a concurrent first-write
        // here relabels one log line, never the end state).
        let existed: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM search_vectors
               WHERE scope_id = ? AND path_token = ?)",
        )
        .bind(&self.scope_id)
        .bind(&token)
        .fetch_one(self.pool)
        .await?;
        sqlx::query(
            "INSERT INTO search_vectors (scope_id, path_token, model, dims, stale, vec, updated_at) \
             VALUES (?, ?, ?, ?, 0, ?, ?) \
             ON CONFLICT(scope_id, path_token) DO UPDATE SET \
               model = ?, dims = ?, stale = 0, vec = ?, updated_at = ?",
        )
        .bind(&self.scope_id)
        .bind(&token)
        .bind(model)
        .bind(dims as i32)
        .bind(&sealed)
        .bind(&now)
        .bind(model)
        .bind(dims as i32)
        .bind(&sealed)
        .bind(&now)
        .execute(self.pool)
        .await?;
        Ok(VectorUpsert {
            created: existed == 0,
        })
    }

    /// Remove the path's row (the delete hook: a deleted concept has
    /// nothing to embed).
    pub async fn remove_row(&self, path: &str) -> Result<(), VectorError> {
        let token = self.path_token_of(path);
        sqlx::query("DELETE FROM search_vectors WHERE scope_id = ? AND path_token = ?")
            .bind(&self.scope_id)
            .bind(&token)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Mark every row of the scope stale (corpus-wide invalidation
    /// marker; rows return to fresh only via re-upsert).
    pub async fn mark_scope_stale(&self) -> Result<u64, VectorError> {
        let result = sqlx::query("UPDATE search_vectors SET stale = 1 WHERE scope_id = ?")
            .bind(&self.scope_id)
            .execute(self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// Decode EVERY row of the scope — STRICT: an envelope that does not
    /// open, or does not decompose into exactly `dims` f32 chunks, is an
    /// ERROR (vectors are repairable by re-embed, and silent row-dropping
    /// here would hide key/configuration problems; the query path skips
    /// instead — see [`cache_scope`]).
    pub async fn load_rows(&self) -> Result<Vec<VectorRow>, VectorError> {
        let raw = self.fetch_rows().await?;
        let mut out = Vec::with_capacity(raw.len());
        for row in raw {
            out.push(self.decode_row(&row)?);
        }
        Ok(out)
    }

    /// Every indexed concept lacking a FRESH row for `(model,
    /// learned_dims)`: the search side enumerates plaintext paths
    /// (`search_docs.scope`; a SQL join is impossible over the HMAC) and
    /// the vector side is keyed by token, so the "needs work" condition
    /// re-runs per path in Rust — no row ‖ row.stale ‖ model differs ‖
    /// dims differ (dims only when a learned dimensionality is
    /// asserted). No marker row is invented: every condition
    /// re-evaluates per boot, which is the enumeration's resumability.
    pub async fn backlog_paths(
        &self,
        model: &str,
        learned_dims: u32,
    ) -> Result<Vec<String>, VectorError> {
        let doc_paths: Vec<(String,)> = sqlx::query_as(
            "SELECT concept_path FROM search_docs WHERE scope = ? ORDER BY concept_path",
        )
        .bind(&self.scope_id)
        .fetch_all(self.pool)
        .await?;
        // token → (model, dims, stale); the PK makes the map unique.
        let mut rows: HashMap<String, (String, u32, bool)> = HashMap::new();
        for (token, row_model, dims, stale) in sqlx::query_as::<_, (String, String, i64, i64)>(
            "SELECT path_token, model, dims, stale FROM search_vectors WHERE scope_id = ?",
        )
        .bind(&self.scope_id)
        .fetch_all(self.pool)
        .await?
        {
            rows.insert(
                token,
                (row_model, u32::try_from(dims).unwrap_or(0), stale != 0),
            );
        }
        let mut out = Vec::with_capacity(doc_paths.len());
        for (path,) in doc_paths {
            let needs_work = match rows.get(&self.path_token_of(&path)) {
                // No row: the concept has never been embedded.
                None => true,
                Some((row_model, row_dims, stale)) => {
                    *stale
                        || row_model.as_str() != model
                        || (learned_dims != 0 && *row_dims != learned_dims)
                }
            };
            if needs_work {
                out.push(path);
            }
        }
        Ok(out)
    }

    /// Cosine ranking over cached-or-built rows: rows of the query's
    /// model/dims only (stale rows are absent from the cache by
    /// construction), cosine > 0 per row, sorted (cosine desc,
    /// path_token asc), top taken, then tokens mapped to plaintext
    /// paths via the plaintext registry (`scope_files.path`). A
    /// candidate whose registry path vanished is a dead row and drops —
    /// never a result that cannot be read.
    pub async fn top_cosine(
        &self,
        qvec: &[f32],
        model: &str,
        top: usize,
    ) -> Result<Vec<(String, f32)>, VectorError> {
        if top == 0 {
            return Ok(vec![]);
        }
        let snapshot = self.cache_scope().await?;
        let mut scored: Vec<(String, f32)> = snapshot
            .rows
            .iter()
            .filter(|r| r.model == model && r.dims as usize == qvec.len())
            .map(|r| (r.path_token.clone(), cosine(&r.vec_f32, qvec)))
            .filter(|(_, c)| *c > 0.0)
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(top);
        if scored.is_empty() {
            return Ok(vec![]);
        }
        // The registry is plaintext; the token mapping is computed, the
        // dead-row drop is exact.
        let paths: Vec<(String,)> = sqlx::query_as("SELECT path FROM scope_files WHERE scope = ?")
            .bind(&self.scope_id)
            .fetch_all(self.pool)
            .await?;
        let by_token: HashMap<String, String> = paths
            .into_iter()
            .map(|(path,)| (self.path_token_of(&path), path))
            .collect();
        Ok(scored
            .into_iter()
            .filter_map(|(token, c)| by_token.get(&token).map(|path| (path.clone(), c)))
            .collect())
    }

    /// The scope's decoded cache, built on first use after an
    /// invalidation. The build applies the ACTIVE-SET filter (spec §6.1
    /// corollary): a superseded/retired concept's row, or any row whose
    /// search row has vanished (deleted concept), never re-enters the
    /// cache. Skips are debug-grade (an unavailable vector row is an
    /// availability question), except decode-integrity damage, warned
    /// once per process; `load_rows` stays the strict path.
    pub async fn cache_scope(&self) -> Result<Arc<ScopeVectors>, VectorError> {
        {
            let guard = SCOPE_VECTORS.lock().unwrap();
            if let Some(map) = guard.as_ref()
                && let Some(hit) = map.get(&self.scope_id)
            {
                return Ok(Arc::clone(hit));
            }
        }
        // Active (or NULL-status) concept tokens as of this build.
        let actives: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT concept_path, lifecycle_status FROM search_docs WHERE scope = ?",
        )
        .bind(&self.scope_id)
        .fetch_all(self.pool)
        .await?;
        let active_tokens: HashSet<String> = actives
            .into_iter()
            .filter(|(_, status)| is_active_status(status.as_deref()))
            .map(|(path, _)| self.path_token_of(&path))
            .collect();

        let mut kept: Vec<CacheRow> = Vec::new();
        for row in self.fetch_rows().await? {
            if row.stale {
                tracing::debug!(
                    scope = %self.scope_id,
                    token = %row.path_token,
                    "vector cache: stale row skipped (rebuild pending)"
                );
                continue;
            }
            if !active_tokens.contains(&row.path_token) {
                tracing::debug!(
                    scope = %self.scope_id,
                    token = %row.path_token,
                    "vector cache: inactive/absent concept row skipped"
                );
                continue;
            }
            match self.decode_row(&row) {
                Ok(decoded) => kept.push(CacheRow {
                    path_token: decoded.path_token,
                    model: decoded.model,
                    dims: decoded.dims,
                    vec_f32: decoded.vec_f32,
                }),
                // (a) unopenable envelope: skip, not fatal (load_rows is
                // the loud, strict path).
                Err(VectorError::Crypto(e)) => tracing::debug!(
                    scope = %self.scope_id,
                    token = %row.path_token,
                    err = %e,
                    "vector cache: unopenable row skipped"
                ),
                // Decode-integrity damage: log ONCE per process (the
                // strict path surfaces it to its callers), then skip.
                Err(VectorError::DataIntegrity(detail)) => {
                    if !DECODE_MISMATCH_WARNED.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            scope = %self.scope_id,
                            token = %row.path_token,
                            detail = %detail,
                            "vector cache: decode-mismatch row dropped (repairable by re-embed)"
                        );
                    } else {
                        tracing::debug!(
                            scope = %self.scope_id,
                            token = %row.path_token,
                            detail = %detail,
                            "vector cache: decode-mismatch row dropped"
                        );
                    }
                }
                Err(other) => return Err(other),
            }
        }
        let snapshot = ScopeVectors {
            model: kept.first().map(|r| r.model.clone()).unwrap_or_default(),
            built_at: std::time::Instant::now(),
            rows: kept,
        };
        let arc = Arc::new(snapshot);
        SCOPE_VECTORS
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(self.scope_id.clone(), Arc::clone(&arc));
        Ok(arc)
    }

    /// Fetch the scope's raw rows (token order — deterministic for every
    /// consumer).
    async fn fetch_rows(&self) -> Result<Vec<RawVectorRow>, VectorError> {
        let rows: Vec<(String, String, i64, i64, Vec<u8>, String)> = sqlx::query_as(
            "SELECT path_token, model, dims, stale, vec, updated_at \
             FROM search_vectors WHERE scope_id = ? ORDER BY path_token",
        )
        .bind(&self.scope_id)
        .fetch_all(self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(path_token, model, dims, stale, sealed, updated_at)| RawVectorRow {
                    path_token,
                    model,
                    dims,
                    stale: stale != 0,
                    sealed,
                    updated_at,
                },
            )
            .collect())
    }

    /// Strictly decode one raw row: open under the row's exact AAD, then
    /// LE f32 chunks, then the dims check.
    fn decode_row(&self, row: &RawVectorRow) -> Result<VectorRow, VectorError> {
        let plain = aead_open(
            &row.sealed,
            self.aad(&row.path_token).as_bytes(),
            self.keys.index_dek(),
        )?;
        let dims = u32::try_from(row.dims).map_err(|_| {
            VectorError::DataIntegrity(format!("row dims {} is not a non-negative u32", row.dims))
        })?;
        if plain.len() % 4 != 0 {
            return Err(VectorError::DataIntegrity(format!(
                "row payload length {} is not a multiple of 4",
                plain.len()
            )));
        }
        let vec_f32 = bytes_to_vec(&plain);
        if vec_f32.len() != dims as usize {
            return Err(VectorError::DataIntegrity(format!(
                "row decoded to {} floats but declares {dims} dims",
                vec_f32.len()
            )));
        }
        Ok(VectorRow {
            path_token: row.path_token.clone(),
            model: row.model.clone(),
            dims,
            stale: row.stale,
            vec_f32,
            updated_at: row.updated_at.clone(),
        })
    }
}

/// Drop a scope's decoded cache — the ConceptStore write hooks call
/// this; the next [`VectorStore::cache_scope`] rebuilds from the rows.
pub fn invalidate_scope(scope_id: &str) {
    let mut guard = SCOPE_VECTORS.lock().unwrap();
    if let Some(map) = guard.as_mut() {
        map.remove(scope_id);
    }
}

/// Enqueue one write — the ConceptStore put hook's entry point. Exact
/// duplicates are skipped wherever they sit in the queue; at capacity
/// the NEW item is dropped (the next write re-enqueues).
pub fn enqueue_write(scope_id: &str, token: &str) {
    let item = (scope_id.to_string(), token.to_string());
    let mut queue = VECTOR_QUEUE.lock().unwrap();
    if queue.iter().any(|e| *e == item) || queue.len() >= VECTOR_QUEUE_CAPACITY {
        return;
    }
    queue.push_back(item);
}

/// FIFO drain of up to `max` items, removing the drained ones.
pub fn drain_queue(max: usize) -> Vec<(String, String)> {
    let mut queue = VECTOR_QUEUE.lock().unwrap();
    let take = max.min(queue.len());
    queue.drain(..take).collect()
}

/// The queue's current depth (health/metrics surfaces).
pub fn vector_queue_depth() -> usize {
    VECTOR_QUEUE.lock().unwrap().len()
}

/// The vector's storage format: plain little-endian f32 chunks, one per
/// element (sealed before it ever reaches SQLite).
fn vec_bytes(vec_f32: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vec_f32.len() * 4);
    for f in vec_f32 {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// The inverse of [`vec_bytes`]. (as_chunks rather than chunks_exact +
/// indexing: the 4-byte pattern is a `[u8; 4]` under LE f32 decode.)
fn bytes_to_vec(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

/// The private cosine — dot over |a|·|b| (the same math core's scorer
/// uses; Task 5 unifies consumers on the core fn). Non-finite results
/// (zero vectors: NaN) are cut by the query path's cosine > 0 rule.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..a.len().min(b.len()) {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    dot / (na.sqrt() * nb.sqrt())
}
