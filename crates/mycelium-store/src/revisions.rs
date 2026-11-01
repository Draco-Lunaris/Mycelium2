//! Concept revision history (the undo log) — spec §3.2 + §3.2a.
//!
//! `concept_revisions` is a write-ahead undo log: every concept-changing
//! op records the full markdown snapshot BEFORE the effect commits. This
//! module owns the revision ROWS only — the ConceptStore mutation boundary
//! (Task 2 of the lifecycle stream) derives ops mechanically at
//! put/patch/delete and calls [`RevisionStore::record`] there; nothing
//! else writes the table.
//!
//! Crypto posture: payloads are AEAD full-concept markdown under the
//! scope's [`IndexKeys`] index DEK (the same sensitivity class as the
//! encrypted-index doc payloads already in SQLite), with AES-GCM AAD
//! binding the purpose explicitly —
//! `rev:<scope_id>:<path_token>:<revision_no>` — so a revision blob can
//! never be replayed as a doc payload, another revision, or another
//! path's history. No new key type.

use std::sync::atomic::{AtomicU32, Ordering};

use mycelium_crypto::aead::{aead_open, aead_seal};
use mycelium_crypto::keys::{MasterKey, ServiceKey};
use mycelium_crypto::store_keys::IndexKeys;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::config::{ConfigStore, get_sealed};

/// Revision op tags (spec §3.2 enum; create/update/delete derive
/// mechanically at the ConceptStore boundary — Task 2 — while
/// restore/migration/merge/consolidate are explicit). [`RevisionStore::
/// record`] rejects any op not in this list: the undo log is a bounded
/// vocabulary, not free-form prose.
pub const REVISION_OPS: [&str; 7] = [
    "create",
    "update",
    "delete",
    "restore",
    "merge",
    "consolidate",
    "migration",
];

/// Default retention in revisions per concept (spec §3.2: 200, range
/// 25–2000).
pub const DEFAULT_REVISION_RETENTION: u32 = 200;

/// Accepted range for revision retention (spec §3.2), clamped on EVERY
/// set, get, save and load.
pub const REVISION_RETENTION_RANGE: (u32, u32) = (25, 2000);

/// Config key for the sealed lifecycle row (the one row this stream's
/// configuration uses; step 5's admin section writes it).
pub const LIFECYCLE_CONFIG_KEY: &str = "lifecycle";

/// Process-global retention, initialized to the default and clamped on
/// every set and get (a stale in-memory value below 25 can never wedge a
/// prune). Set at boot by [`sync_lifecycle_from_store`] — deliberately
/// BEFORE the packaged-skill seed and every boot sweep write concepts —
/// and live by step 5's admin section.
static REVISION_RETENTION: AtomicU32 = AtomicU32::new(DEFAULT_REVISION_RETENTION);

fn clamp_retention(v: u32) -> u32 {
    v.clamp(REVISION_RETENTION_RANGE.0, REVISION_RETENTION_RANGE.1)
}

/// Set the process-global retention. Clamp 25–2000 on EVERY set — an
/// out-of-range caller (a hand-edited row, an older build, step 5's
/// form) can never open an unbounded or empty log.
pub fn set_revision_retention(retention: u32) {
    REVISION_RETENTION.store(clamp_retention(retention), Ordering::Relaxed);
}

/// The process-global retention (clamped — get applies the range, so a
/// value that entered through any path reads as documented).
pub fn revision_retention() -> u32 {
    clamp_retention(REVISION_RETENTION.load(Ordering::Relaxed))
}

/// Default hits returned by a zero-LLM `memory_query` (spec §3.3: 8,
/// admin-tunable, range 1–64).
pub const DEFAULT_QUERY_TOP_K: u32 = 8;

/// Accepted range for `query_top_k` (spec §3.2a-7), clamped on EVERY
/// set, get, save and load. Zero is meaningful only as an EXPLICIT
/// caller request; it never lives in the row or the static.
pub const QUERY_TOP_K_RANGE: (u32, u32) = (1, 64);

/// Process-global zero-LLM query top-k, the [`REVISION_RETENTION`]
/// pattern: initialized to the default and clamped on every set and get.
/// Set at boot by [`sync_lifecycle_from_store`] and live by step 5's
/// admin section; read by the MCP hits path (spec §3.3).
static QUERY_TOP_K: AtomicU32 = AtomicU32::new(DEFAULT_QUERY_TOP_K);

fn clamp_query_top_k(v: u32) -> u32 {
    v.clamp(QUERY_TOP_K_RANGE.0, QUERY_TOP_K_RANGE.1)
}

/// Set the process-global query top-k. Clamp 1–64 on EVERY set — an
/// out-of-range caller (a hand-edited row, an older build, step 5's
/// form) can never make a default query return zero hits or an
/// unbounded tail.
pub fn set_query_top_k(k: u32) {
    QUERY_TOP_K.store(clamp_query_top_k(k), Ordering::Relaxed);
}

/// The process-global query top-k (clamped — get applies the range, so a
/// value that entered through any path reads as documented).
pub fn query_top_k() -> u32 {
    clamp_query_top_k(QUERY_TOP_K.load(Ordering::Relaxed))
}

/// Default fusion rank constant (memory-lifecycle spec §3.5: the
/// classic RRF k of 60), admin-tunable, range 1–1024.
pub const DEFAULT_RRF_K: u32 = 60;

/// Accepted range for the fusion rank constant, clamped on EVERY set,
/// get, save and load.
pub const RRF_K_RANGE: (u32, u32) = (1, 1024);

/// Default value for BOTH fusion weights (spec §3.5: symmetric 1.0/1.0,
/// so neither source dominates), admin-tunable per field, shared range
/// 0.0–10.0.
pub const DEFAULT_RRF_WEIGHT: f32 = 1.0;

/// Accepted range for both fusion weights (0 keeps a pool usable at 0 —
/// the composer's term vanishes rather than faulting; 10 caps a
/// dominate-everything weight).
pub const RRF_WEIGHT_RANGE: (f32, f32) = (0.0, 10.0);

/// Default candidate-dedup cosine threshold for the dream consolidation
/// (spec §3.5: 0.92 — near-duplicates only), admin-tunable.
pub const DEFAULT_DEDUP_COSINE: f32 = 0.92;

/// Accepted range for the dedup threshold: below 0.5 would merge
/// unrelated candidates, 1.0 only merges byte-equal embeddings.
pub const DEDUP_COSINE_RANGE: (f32, f32) = (0.5, 1.0);

/// Process-global fusion rank constant, the [`REVISION_RETENTION`]
/// pattern: initialized to the default, clamped on every set and get.
/// Set at boot by [`sync_lifecycle_from_store`] and live by step 5's
/// admin section; read by the hybrid composer (`rrf_params`).
static RRF_K: AtomicU32 = AtomicU32::new(DEFAULT_RRF_K);

/// The lexical pool's fusion weight, stored as `f32::to_bits()` (the
/// composer accepts f32; the atomic carries bits — the set/get seam
/// re-clamps so a stale value can never leave the range).
static RRF_WEIGHT_LEX: AtomicU32 = AtomicU32::new(DEFAULT_RRF_WEIGHT.to_bits());

/// The vector pool's fusion weight ([`RRF_WEIGHT_LEX`] pattern).
static RRF_WEIGHT_VEC: AtomicU32 = AtomicU32::new(DEFAULT_RRF_WEIGHT.to_bits());

/// The dream dedup cosine threshold ([`RRF_WEIGHT_LEX`] pattern; Task 6
/// is the reader).
static DEDUP_COSINE: AtomicU32 = AtomicU32::new(DEFAULT_DEDUP_COSINE.to_bits());

fn clamp_rrf_k(k: u32) -> u32 {
    k.clamp(RRF_K_RANGE.0, RRF_K_RANGE.1)
}

/// A fusion weight clamped to its range with the NaN guard: `f32::clamp`
/// propagates NaN, so a NaN folds to the field's default instead (a NaN
/// weight would poison every fused score).
fn clamp_fusion_weight(w: f32) -> f32 {
    if w.is_nan() {
        DEFAULT_RRF_WEIGHT
    } else {
        w.clamp(RRF_WEIGHT_RANGE.0, RRF_WEIGHT_RANGE.1)
    }
}

/// The dedup threshold's clamp, with the same NaN guard.
fn clamp_dedup_cosine(t: f32) -> f32 {
    if t.is_nan() {
        DEFAULT_DEDUP_COSINE
    } else {
        t.clamp(DEDUP_COSINE_RANGE.0, DEDUP_COSINE_RANGE.1)
    }
}

/// Set the process-global fusion rank constant. Clamp 1–1024 on EVERY
/// set — an out-of-range caller can never drive every rank to the same
/// score or overflow the composer's arithmetic.
pub fn set_rrf_k(k: u32) {
    RRF_K.store(clamp_rrf_k(k), Ordering::Relaxed);
}

/// Set the lexical pool's fusion weight (clamped + NaN-guarded).
pub fn set_rrf_weight_lex(w: f32) {
    RRF_WEIGHT_LEX.store(clamp_fusion_weight(w).to_bits(), Ordering::Relaxed);
}

/// Set the vector pool's fusion weight (clamped + NaN-guarded).
pub fn set_rrf_weight_vec(w: f32) {
    RRF_WEIGHT_VEC.store(clamp_fusion_weight(w).to_bits(), Ordering::Relaxed);
}

/// Set the dream dedup cosine threshold (clamped + NaN-guarded).
pub fn set_dedup_cosine(t: f32) {
    DEDUP_COSINE.store(clamp_dedup_cosine(t).to_bits(), Ordering::Relaxed);
}

/// The fusion weights as the composer's [`RrfParams`] — the u32 rank
/// constant widens to f32 (lossless within the clamped range) and the
/// weights come back off their bit statics. The hybrid composer calls
/// this per request so step 5's admin retune applies with no restart.
#[must_use]
pub fn rrf_params() -> mycelium_core::search::RrfParams {
    mycelium_core::search::RrfParams {
        rrf_k: RRF_K.load(Ordering::Relaxed) as f32,
        w_lex: f32::from_bits(RRF_WEIGHT_LEX.load(Ordering::Relaxed)),
        w_vec: f32::from_bits(RRF_WEIGHT_VEC.load(Ordering::Relaxed)),
    }
}

/// The process-global dedup cosine threshold (clamped — Task 6's
/// dream-consolidation reader).
#[must_use]
pub fn dedup_cosine() -> f32 {
    f32::from_bits(DEDUP_COSINE.load(Ordering::Relaxed))
}

/// Deterministic per-op summaries (byte-safe bounded, the queue
/// receipts' `short_detail` discipline): create→"concept created",
/// update→"concept updated", delete→"concept deleted", restore→
/// "restored to revision {n}", migration→"deprecated tag migrated to
/// status: retired", merge→"dream merge", consolidate→"consolidated body
/// history". Unknown op → Err (and restore without its target revision
/// number is a caller bug — loud, not a mangled summary).
pub fn op_summary(op: &str, n: Option<u64>) -> Result<String, RevisionsError> {
    match op {
        "create" => Ok("concept created".to_string()),
        "update" => Ok("concept updated".to_string()),
        "delete" => Ok("concept deleted".to_string()),
        "restore" => n
            .map(|k| format!("restored to revision {k}"))
            .ok_or_else(|| {
                RevisionsError::InvalidOp("restore requires the target revision number".to_string())
            }),
        "merge" => Ok("dream merge".to_string()),
        "consolidate" => Ok("consolidated body history".to_string()),
        "migration" => Ok("deprecated tag migrated to status: retired".to_string()),
        other => Err(RevisionsError::InvalidOp(other.to_string())),
    }
}

/// Sealed `lifecycle` config row (key [`LIFECYCLE_CONFIG_KEY`]), the
/// llm-row pattern: a blank/missing row keeps the defaults, and the
/// values clamp on BOTH save and load, so a row written by an older
/// (or hand-edited) build can never widen the log or change the query
/// defaults. Step 2 carries ONE field; later steps extend the struct —
/// `serde(default)` keeps old rows valid through that growth (step 3's
/// `query_top_k` rides exactly that promise).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct LifecycleConfig {
    #[serde(default = "default_retention")]
    pub revision_retention: u32,
    /// Hits a zero-LLM `memory_query` returns when the caller sends no
    /// `limit` (spec §3.3); 1–64, clamped on save and load.
    #[serde(default = "default_query_top_k")]
    pub query_top_k: u32,
    /// The hybrid composer's fusion rank constant (spec §3.5: 60);
    /// 1–1024, clamped on save and load.
    #[serde(default = "default_rrf_k")]
    pub rrf_k: u32,
    /// The hybrid composer's lexical pool weight (spec §3.5: 1.0);
    /// 0.0–10.0, clamped on save and load, NaN resets the default.
    #[serde(default = "default_rrf_weight")]
    pub rrf_weight_lex: f32,
    /// The hybrid composer's vector pool weight (spec §3.5: 1.0);
    /// 0.0–10.0, clamped on save and load, NaN resets the default.
    #[serde(default = "default_rrf_weight")]
    pub rrf_weight_vec: f32,
    /// The dream dedup's cosine threshold (spec §3.5: 0.92); 0.5–1.0,
    /// clamped on save and load, NaN resets the default.
    #[serde(default = "default_dedup_cosine")]
    pub dedup_cosine: f32,
}

fn default_retention() -> u32 {
    DEFAULT_REVISION_RETENTION
}

fn default_query_top_k() -> u32 {
    DEFAULT_QUERY_TOP_K
}

fn default_rrf_k() -> u32 {
    DEFAULT_RRF_K
}

fn default_rrf_weight() -> f32 {
    DEFAULT_RRF_WEIGHT
}

fn default_dedup_cosine() -> f32 {
    DEFAULT_DEDUP_COSINE
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            revision_retention: DEFAULT_REVISION_RETENTION,
            query_top_k: DEFAULT_QUERY_TOP_K,
            rrf_k: DEFAULT_RRF_K,
            rrf_weight_lex: DEFAULT_RRF_WEIGHT,
            rrf_weight_vec: DEFAULT_RRF_WEIGHT,
            dedup_cosine: DEFAULT_DEDUP_COSINE,
        }
    }
}

impl LifecycleConfig {
    /// A copy with every field forced into its accepted range — the
    /// SAVE-side clamp (step 5's admin surface applies it before sealing
    /// the row); the load side clamps through [`set_revision_retention`],
    /// [`set_query_top_k`] and the fusion setters. The NaN guard folds a
    /// NaN weight/threshold to its field's default (`f32::clamp` would
    /// propagate it).
    #[must_use]
    pub fn clamp(&self) -> LifecycleConfig {
        LifecycleConfig {
            revision_retention: clamp_retention(self.revision_retention),
            query_top_k: clamp_query_top_k(self.query_top_k),
            rrf_k: clamp_rrf_k(self.rrf_k),
            rrf_weight_lex: clamp_fusion_weight(self.rrf_weight_lex),
            rrf_weight_vec: clamp_fusion_weight(self.rrf_weight_vec),
            dedup_cosine: clamp_dedup_cosine(self.dedup_cosine),
        }
    }
}

/// Read the sealed `lifecycle` row and push it onto the process-global
/// retention and query top-k. Boot-order contract: `main` syncs before
/// `seed_packaged_skills` and before every boot sweep — the seed and the
/// sweeps write concepts, so retention must be live before them (the
/// same reason `sync_llm_gate_from_store` precedes the workers).
///
/// Missing or undecryptable row simply means the defaults (`get_sealed`
/// is intentionally lenient — the llm-row pattern); a value outside its
/// range clamps through [`set_revision_retention`] or
/// [`set_query_top_k`] on the way in.
pub async fn sync_lifecycle_from_store(
    config: &ConfigStore,
    service_key: &ServiceKey,
) -> Result<(), RevisionsError> {
    let cfg: LifecycleConfig = get_sealed(config, service_key, LIFECYCLE_CONFIG_KEY)
        .await
        .unwrap_or_default();
    set_revision_retention(cfg.revision_retention);
    set_query_top_k(cfg.query_top_k);
    set_rrf_k(cfg.rrf_k);
    set_rrf_weight_lex(cfg.rrf_weight_lex);
    set_rrf_weight_vec(cfg.rrf_weight_vec);
    set_dedup_cosine(cfg.dedup_cosine);
    Ok(())
}

/// One revision of a concept — metadata only. The snapshot's bytes are
/// reachable exclusively through [`RevisionStore::get`]; no listing
/// structure carries (or could decode) payload text.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RevisionEntry {
    pub revision_no: u64,
    pub op: String,
    pub source: String,
    pub summary: String,
    pub created_at: String,
    /// The payload convention's emptiness (§3.2a-3), derived WITHOUT
    /// decrypting: the column stores the AEAD envelope, whose
    /// empty-plaintext length is the envelope format's fixed overhead
    /// (`mycelium_crypto::envelope::EMPTY_ENVELOPE_LEN`) and every
    /// non-empty payload is exactly one byte longer per byte of
    /// plaintext — so a stored length at or under the overhead decodes
    /// to nothing restorable. NULL and corrupt-short rows count as
    /// empty (fail-safe in the no-restore direction). The page layer
    /// gates restore links and the confirm interstitial on this flag.
    #[serde(default)]
    pub payload_empty: bool,
}

/// One path's revision chain within a scope — the revisions-browser
/// page's row type. Dead paths are included (the plaintext `path`
/// column carries the display name long after the registry row is
/// gone); `live` is whether a `scope_files` row exists right now.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RevisionPathEntry {
    pub path: String,
    pub live: bool,
    pub latest_no: u64,
    pub last_op: String,
    pub last_op_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RevisionsError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("crypto error: {0}")]
    Crypto(#[from] mycelium_crypto::CryptoError),
    #[error("revision {n} not found for {path}")]
    NotFound { path: String, n: u64 },
    #[error("invalid revision op: {0}")]
    InvalidOp(String),
    #[error("concept_revisions row integrity: {detail}")]
    Corrupted { detail: String },
}

/// The revision store for ONE scope — the same per-scope identity as
/// [`crate::search_index::EncryptedIndex`]: user scopes under the user's
/// master key, `global:skills` / `global:library` under the service key,
/// keyed by HMAC path tokens so plaintext paths never needed the
/// revision table before §3.2a-1 (and still never carry search terms).
#[derive(Debug, Clone)]
pub struct RevisionStore<'a> {
    pool: &'a SqlitePool,
    scope_id: String,
    keys: IndexKeys,
}

/// Rows past retention sit at or below `MAX - retention`: keep the
/// newest `retention` rows per concept. Monotonic contiguous numbering
/// makes the cutoff exact; pruned rows are irrecoverable.
fn retention_cutoff(max: i64, retention: u32) -> i64 {
    max - i64::from(retention)
}

impl<'a> RevisionStore<'a> {
    /// Open the revision store for a user's private bundle.
    pub fn for_user(pool: &'a SqlitePool, user_id: Uuid, master_key: &MasterKey) -> Self {
        Self {
            pool,
            scope_id: format!("user:{user_id}"),
            keys: IndexKeys::from_master_key(master_key),
        }
    }

    /// Open the revision store for a service-key scope — namespace is
    /// the registry/index namespace ("skills" / "library"); the two
    /// shelves' chains never mix even for the same path token.
    pub fn for_service(pool: &'a SqlitePool, service_key: &ServiceKey, namespace: &str) -> Self {
        Self {
            pool,
            scope_id: format!("global:{namespace}"),
            keys: IndexKeys::from_service_key(service_key),
        }
    }

    /// The scope's HMAC path token for `path` — same scheme as
    /// `search_tokens` (one-way; the table's key alongside `scope_id`).
    pub fn path_token_of(&self, path: &str) -> String {
        self.keys.token(path)
    }

    /// The purpose-bound AAD for one revision row.
    fn aad(&self, path_token: &str, revision_no: u64) -> String {
        format!("rev:{}:{}:{}", self.scope_id, path_token, revision_no)
    }

    /// Write-ahead insert with unlabeled provenance (the '' convention
    /// — an explicit label goes through [`record_as`]).
    pub async fn record(
        &self,
        path: &str,
        op: &str,
        summary: &str,
        payload: &[u8],
    ) -> Result<u64, RevisionsError> {
        self.record_internal(path, op, "", summary, payload).await
    }

    /// Write-ahead insert with the caller's provenance label (the
    /// ConceptStore boundary stamps its `with_provenance` source, e.g.
    /// `web:<username>` for restore — spec §3.1).
    pub async fn record_as(
        &self,
        path: &str,
        op: &str,
        source: &str,
        summary: &str,
        payload: &[u8],
    ) -> Result<u64, RevisionsError> {
        self.record_internal(path, op, source, summary, payload)
            .await
    }

    async fn record_internal(
        &self,
        path: &str,
        op: &str,
        source: &str,
        summary: &str,
        payload: &[u8],
    ) -> Result<u64, RevisionsError> {
        if !REVISION_OPS.contains(&op) {
            return Err(RevisionsError::InvalidOp(op.to_string()));
        }
        // Concurrent same-path puts race on the MAX+1 assign; the INSERT
        // and its retention prune are one transaction, with BEGIN
        // IMMEDIATE holding SQLite's single write lock across read +
        // insert so this store's own records never double-assign. The
        // UNIQUE key plus this retry (×3) is the belt-and-braces for
        // writers that bypass the assign discipline.
        const UNIQUE_RETRIES: usize = 3;
        let mut attempt = 0usize;
        loop {
            match self.record_once(path, op, source, summary, payload).await {
                Ok(no) => return Ok(no),
                Err(RevisionsError::Db(sqlx::Error::Database(db)))
                    if db.is_unique_violation() && attempt < UNIQUE_RETRIES =>
                {
                    // Another writer committed the same revision_no
                    // first — retry re-reads MAX under a fresh write
                    // lock (attempts cap at UNIQUE_RETRIES, then the
                    // error escapes).
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn record_once(
        &self,
        path: &str,
        op: &str,
        source: &str,
        summary: &str,
        payload: &[u8],
    ) -> Result<u64, RevisionsError> {
        let token = self.path_token_of(path);
        let retention = revision_retention();
        let now = chrono::Utc::now().to_rfc3339();

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // Revision numbers are monotonic per (scope_id, path_token):
        // MAX+1 under the IMMEDIATE write lock (SQLite's single-writer
        // rule makes read + insert atomic against other writers using
        // this store).
        let max: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(revision_no), 0) FROM concept_revisions
             WHERE scope_id = ? AND path_token = ?",
        )
        .bind(&self.scope_id)
        .bind(&token)
        .fetch_one(&mut *tx)
        .await?;
        // The AAD binds the revision number BEFORE the insert: the
        // payload is meaningful only at this (scope, token, no) triple.
        let no = (max + 1) as u64;
        let sealed = aead_seal(
            payload,
            self.aad(&token, no).as_bytes(),
            self.keys.index_dek(),
        )?;

        // Payload convention (§3.2a-3): `create` rows carry the created
        // content; every other op row carries the PRE-mutation snapshot;
        // an undeleting `restore` carries an empty payload — whatever
        // the boundary passes lands verbatim, encrypted here. The
        // plaintext path rides alongside (§3.2a-1) so the browser index
        // can keep naming the path once the registry row is gone.
        sqlx::query(
            "INSERT INTO concept_revisions
               (scope_id, path_token, path, revision_no, op, source, summary, payload, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&self.scope_id)
        .bind(&token)
        .bind(path)
        .bind(no as i64)
        .bind(op)
        .bind(source)
        .bind(summary)
        .bind(&sealed)
        .bind(&now)
        .execute(&mut *tx)
        .await?;

        // Retention prune, same transaction (§3.2a-4): oldest first, per
        // concept, past rows irrecoverable. `MAX` here is the row just
        // inserted — the cutoff is exact.
        let cutoff = retention_cutoff(max + 1, retention);
        if cutoff > 0 {
            sqlx::query(
                "DELETE FROM concept_revisions
                 WHERE scope_id = ? AND path_token = ? AND revision_no <= ?",
            )
            .bind(&self.scope_id)
            .bind(&token)
            .bind(cutoff)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(no)
    }

    /// One path's chain, newest first — metadata only: revision_no, op,
    /// source, summary, created_at, and the payload-emptiness flag (the
    /// stored envelope's LENGTH compared against the envelope overhead —
    /// a length comparison, never a decrypt). Deterministic order:
    /// revision_no DESC, the contract the web revisions list renders
    /// from.
    pub async fn list(&self, path: &str) -> Result<Vec<RevisionEntry>, RevisionsError> {
        let token = self.path_token_of(path);
        let rows: Vec<(i64, String, String, String, String, Option<i64>)> = sqlx::query_as(
            "SELECT revision_no, op, source, summary, created_at, LENGTH(payload)
             FROM concept_revisions
             WHERE scope_id = ? AND path_token = ?
             ORDER BY revision_no DESC",
        )
        .bind(&self.scope_id)
        .bind(&token)
        .fetch_all(self.pool)
        .await?;
        rows.into_iter()
            .map(
                |(revision_no, op, source, summary, created_at, stored_len)| {
                    let revision_no =
                        u64::try_from(revision_no).map_err(|_| RevisionsError::Corrupted {
                            detail: format!("negative revision_no {revision_no}"),
                        })?;
                    // Emptiness is exact both ways: envelope length = the
                    // empty-envelope overhead + plaintext length (AES-GCM
                    // appends a fixed 16-byte tag). NULL reads as zero;
                    // a negative LENGTH cannot occur but fails closed.
                    let stored_len = u64::try_from(stored_len.unwrap_or(0)).map_err(|_| {
                        RevisionsError::Corrupted {
                            detail: format!("negative payload length {stored_len:?}"),
                        }
                    })?;
                    Ok(RevisionEntry {
                        revision_no,
                        op,
                        source,
                        summary,
                        created_at,
                        payload_empty: stored_len
                            <= mycelium_crypto::envelope::EMPTY_ENVELOPE_LEN as u64,
                    })
                },
            )
            .collect()
    }

    /// One revision's decoded raw markdown snapshot (frontmatter +
    /// body exactly as recorded). Fails closed: a missing row is
    /// [`RevisionsError::NotFound`], an AAD-mismatched or corrupt
    /// payload is [`RevisionsError::Crypto`] — a tampered envelope
    /// never decodes to plausible bytes.
    pub async fn get(&self, path: &str, revision_no: u64) -> Result<Vec<u8>, RevisionsError> {
        let token = self.path_token_of(path);
        let row: Option<(Vec<u8>,)> = sqlx::query_as(
            "SELECT payload FROM concept_revisions
             WHERE scope_id = ? AND path_token = ? AND revision_no = ?",
        )
        .bind(&self.scope_id)
        .bind(&token)
        .bind(revision_no as i64)
        .fetch_optional(self.pool)
        .await?;
        let (sealed,) = row.ok_or_else(|| RevisionsError::NotFound {
            path: path.to_string(),
            n: revision_no,
        })?;
        Ok(aead_open(
            &sealed,
            self.aad(&token, revision_no).as_bytes(),
            self.keys.index_dek(),
        )?)
    }

    /// Scope-level per-path index for the revisions browser: every path
    /// with revisions in this scope, live OR deleted (the plaintext
    /// `path` column carries the display name), each with its latest
    /// revision's number/op/time. Deterministic order: path asc, the
    /// registry's listing order.
    pub async fn list_paths(&self) -> Result<Vec<RevisionPathEntry>, RevisionsError> {
        let rows: Vec<(String, i64, String, String, i64)> = sqlx::query_as(
            "WITH latest AS (
                 SELECT path, path_token, MAX(revision_no) AS latest_no
                 FROM concept_revisions
                 WHERE scope_id = ?
                 GROUP BY path, path_token
             )
             SELECT l.path, l.latest_no, r.op, r.created_at,
                    EXISTS(SELECT 1 FROM scope_files sf
                            WHERE sf.scope = ? AND sf.path = l.path) AS live
             FROM latest l
             JOIN concept_revisions r
               ON r.scope_id = ? AND r.path_token = l.path_token
              AND r.revision_no = l.latest_no
             ORDER BY l.path",
        )
        .bind(&self.scope_id)
        .bind(&self.scope_id)
        .bind(&self.scope_id)
        .fetch_all(self.pool)
        .await?;
        rows.into_iter()
            .map(|(path, latest_no, last_op, last_op_at, live)| {
                let latest_no =
                    u64::try_from(latest_no).map_err(|_| RevisionsError::Corrupted {
                        detail: format!("negative revision_no {latest_no}"),
                    })?;
                Ok(RevisionPathEntry {
                    path,
                    live: live != 0,
                    latest_no,
                    last_op,
                    last_op_at,
                })
            })
            .collect()
    }

    /// Retention prune for one concept (by its path token) — exposed
    /// for Task 5's idempotence test and any out-of-band cleanup.
    /// Returns the number of rows it deleted.
    pub async fn prune(&self, path_token: &str) -> Result<u64, RevisionsError> {
        let retention = revision_retention();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let max: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(revision_no), 0) FROM concept_revisions
             WHERE scope_id = ? AND path_token = ?",
        )
        .bind(&self.scope_id)
        .bind(path_token)
        .fetch_one(&mut *tx)
        .await?;
        let mut pruned = 0u64;
        let cutoff = retention_cutoff(max, retention);
        if cutoff > 0 {
            let result = sqlx::query(
                "DELETE FROM concept_revisions
                 WHERE scope_id = ? AND path_token = ? AND revision_no <= ?",
            )
            .bind(&self.scope_id)
            .bind(path_token)
            .bind(cutoff)
            .execute(&mut *tx)
            .await?;
            pruned = result.rows_affected();
        }
        tx.commit().await?;
        Ok(pruned)
    }
}
