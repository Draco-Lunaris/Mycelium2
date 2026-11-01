//! The vector maintenance worker (memory-lifecycle spec §3.3): a small
//! tick loop that keeps `search_vectors` in step with the concepts and
//! the admin-configured embeddings model. The write side fills a
//! bounded queue from the concept-store hooks (a put enqueues the
//! scope's path token, a delete does not re-embed); this side consumes
//! it.
//!
//! One tick, in order:
//!
//! 1. Read the `embeddings` config row — absent ⇒ the machine goes
//!    Disabled and the tick ends; undecryptable ⇒ warn (per tick, no
//!    observation) and end; present ⇒ `note_configured` (posture lift)
//!    and the config is clamped before use.
//! 2. Prologue: a learned dims stamp (`EMBEDDING_DIMS != 0`) marks
//!    every row whose model or dims mismatches the config stale — one
//!    whole-corpus statement, no per-scope loops.
//! 3. A Red backend skips the rest of the tick (nothing embeds while
//!    the model is down; Yellow proceeds).
//! 4. Drain the queue, group by scope, resolve each scope's concept
//!    store, then per token: the path exists ⇒ load the concept and
//!    collect it for embedding; the path is gone ⇒ the belt
//!    (`remove_row_by_token`).
//! 5. Embedding runs in `cfg.batch`-sized batches with up to three
//!    attempts per batch; a batch that still fails drops its items with
//!    a bounded warn — nothing errors upward, and every successful
//!    concept upserts its row and advances `VECTOR_PROCESSED_TOTAL`.
//!    The scope's decoded cache invalidates once, after the drain's
//!    upserts land.
//! 6. Stale rebuild: every scope with stale rows (deterministic order)
//!    re-embeds up to [`STALE_ROWS_PER_TICK`] rows per tick from the
//!    registry's own backlog enumeration, then invalidates again. A
//!    larger backlog simply resumes next tick.
//! 7. Boot backlog (FIRST tick only, per process): every scope's
//!    `backlog_paths` re-enters the queue (writes that happened before
//!    a worker ever saw them), bounded by the queue itself — at
//!    capacity a NEW item is dropped and the remainder resumes next
//!    boot.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use mycelium_crypto::keys::ServiceKey;
use mycelium_store::vectors::{
    VECTOR_PROCESSED_TOTAL, VECTOR_QUEUE_CAPACITY, VectorStore, drain_queue, enqueue_write,
    invalidate_scope,
};
use mycelium_store::{ConceptEntry, ConceptStore, ConceptStoreError, ConfigStore, Store};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::embeddings::{EmbeddingsClient, EmbeddingsConfig};
use crate::llm_health::{
    BackendState, EMBEDDING_DIMS, EMBEDDING_HEALTH, embedding_state, short_detail,
};
use crate::queue_worker::MasterKeyRecovery;

/// Wall-clock cadence of the production tick (tests spawn with a shorter
/// interval through [`VectorWorker::spawn_with_tick`]).
pub const VECTOR_TICK_SECS: u64 = 5;

/// How many stale rows one tick's rebuild phase re-embeds (a scope with
/// a larger backlog resumes on the next tick — a big model swap neither
/// hogs nor wedges the worker).
pub const STALE_ROWS_PER_TICK: usize = 256;

/// Embeddings attempts on one batch before the batch drops with its
/// bounded warn (the items were drained — the drop never echoes back
/// into the queue, so nothing wedges).
const EMBED_ATTEMPTS: usize = 3;

// ---- worker ----

pub struct VectorWorker {
    pub store: Arc<Store>,
    pub config: Arc<ConfigStore>,
    pub recovery: Arc<MasterKeyRecovery>,
    /// The service key, needed to decrypt the sealed embeddings row.
    pub service_key: Arc<ServiceKey>,
}

impl VectorWorker {
    /// Wire up the worker's seams (the queue worker's ctor shape).
    pub fn new(
        store: Arc<Store>,
        config: Arc<ConfigStore>,
        service_key: Arc<ServiceKey>,
        recovery: Arc<MasterKeyRecovery>,
    ) -> Self {
        Self {
            store,
            config,
            recovery,
            service_key,
        }
    }

    /// Spawn the tick loop on `shutdown` at the production cadence.
    pub fn spawn(self: &Arc<Self>, shutdown: CancellationToken) -> JoinHandle<()> {
        self.spawn_with_tick(shutdown, Duration::from_secs(VECTOR_TICK_SECS))
    }

    /// The test-visible variant: an explicit tick interval (the
    /// integration tests shorten the cadence; production callers use
    /// [`VectorWorker::spawn`]).
    pub fn spawn_with_tick(
        self: &Arc<Self>,
        shutdown: CancellationToken,
        tick: Duration,
    ) -> JoinHandle<()> {
        let me = Arc::clone(self);
        tokio::spawn(async move { me.run(shutdown, tick).await })
    }

    /// The tick loop: immediate first tick, `MissedTickBehavior::Delay`
    /// (a wedged tick never backlog-compresses), clean stop on
    /// cancellation.
    async fn run(self: Arc<Self>, shutdown: CancellationToken, tick: Duration) {
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The boot backlog fires on the FIRST executed tick only —
        // taken BEFORE each tick so a Red-skipped first tick consumes
        // the flag (an era-3 backlog must not queue while the model is
        // down; the next boot re-backlogs whatever was lost).
        let mut boot_backlog = true;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let backlog = std::mem::take(&mut boot_backlog);
                    self.tick(backlog).await;
                }
                () = shutdown.cancelled() => {
                    tracing::info!("vector worker task stopped");
                    return;
                }
            }
        }
    }

    /// One tick: the seven phases in turn (module doc).
    async fn tick(&self, boot_backlog: bool) {
        let Some(cfg) = self.read_config().await else {
            return;
        };

        // Prologue: the whole-corpus stale mark rides the learned dims.
        let learned = EMBEDDING_DIMS.load(Ordering::Relaxed);
        if learned != 0 {
            match VectorStore::mark_corpus_stale(self.store.pool(), &cfg.model, learned).await {
                Ok(n) if n > 0 => tracing::info!(
                    model = %cfg.model,
                    dims = learned,
                    stale = n,
                    "embedding config mismatch: stale rows marked for rebuild"
                ),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "corpus stale-mark failed — rebuild may lag one tick");
                }
            }
        }

        // A Red backend parks the queue: nothing was embedded, nothing
        // is re-tickable until a probe recovers the machine.
        if embedding_state() == BackendState::Red {
            return;
        }

        let client = EmbeddingsClient::new(&cfg);
        self.drain(&client, &cfg).await;
        self.rebuild_stale_scopes(&client, &cfg).await;
        if boot_backlog {
            self.enqueue_boot_backlog(&cfg.model, learned).await;
        }
    }

    /// Step 1: the config row's three postures. The machine notes ride
    /// here, not in the callers — the tick's other phases assume the
    /// machine reflects the read once this returns `Some`.
    async fn read_config(&self) -> Option<EmbeddingsConfig> {
        match self.config.raw_get("embeddings").await {
            // A DB error leaves the machine alone (the row may exist;
            // the boot sync's posture is the truth until it reads).
            Err(e) => {
                tracing::warn!(error = %e, "embeddings config lookup failed — tick skipped");
                None
            }
            Ok(None) => {
                if EMBEDDING_HEALTH.state() != BackendState::Disabled {
                    EMBEDDING_HEALTH.set_disabled();
                }
                None
            }
            Ok(Some(_raw)) => {
                // Present-but-undecryptable is a config problem, not
                // backend weather — warn per tick, feed no observation.
                let Some(cfg) = mycelium_store::config::get_sealed::<EmbeddingsConfig>(
                    &self.config,
                    &self.service_key,
                    "embeddings",
                )
                .await
                else {
                    tracing::warn!("embeddings config row is undecryptable — tick skipped");
                    return None;
                };
                EMBEDDING_HEALTH.note_configured();
                Some(cfg.clamp())
            }
        }
    }

    /// Steps 4–5: one drain pass, grouped by scope.
    async fn drain(&self, client: &EmbeddingsClient, cfg: &EmbeddingsConfig) {
        // The queue's own capacity: a full drain per tick (at capacity
        // the queue dropped its NEWEST item, so the oldest survive).
        let items = drain_queue(VECTOR_QUEUE_CAPACITY);
        if items.is_empty() {
            return;
        }
        let mut by_scope: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (scope, token) in items {
            by_scope.entry(scope).or_default().push(token);
        }
        for (scope_id, tokens) in by_scope {
            let Some(cs) = self.resolve_scope(&scope_id).await else {
                tracing::warn!(
                    scope = %scope_id,
                    count = tokens.len(),
                    "drained vector items for an unreadable scope dropped"
                );
                continue;
            };
            self.drain_scope(client, cfg, &cs, &scope_id, tokens).await;
        }
    }

    /// One scope's drained tokens: registry lookup → embed → upsert.
    async fn drain_scope(
        &self,
        client: &EmbeddingsClient,
        cfg: &EmbeddingsConfig,
        cs: &ConceptStore<'_>,
        scope_id: &str,
        tokens: Vec<String>,
    ) {
        let vs = cs.vectors();
        let entries: Vec<ConceptEntry> = match cs.list().await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(scope = %scope_id, error = %e,
                    "vector drain: listing failed — scope's items dropped");
                return;
            }
        };
        // The registry scan maps each token to its path once (the token
        // is the one-way identity the queue carries).
        let mut token_to_path: HashMap<String, String> = HashMap::with_capacity(entries.len());
        for entry in &entries {
            let token = vs.path_token_of(&entry.path);
            token_to_path.insert(token, entry.path.clone());
        }

        let mut pairs: Vec<(String, String)> = Vec::with_capacity(tokens.len());
        for token in &tokens {
            let Some(path) = token_to_path.get(token) else {
                self.belt(vs, token).await;
                continue;
            };
            match cs.get(path).await {
                Ok(concept) => {
                    let text = mycelium_core::search::searchable_text(&concept);
                    pairs.push((path.clone(), text));
                }
                Err(ConceptStoreError::NotFound(_)) => {
                    // The registry said present, the file is gone — the
                    // belt covers the residue (a row keyed by the same
                    // token must not outlive its concept).
                    self.belt(vs, token).await;
                }
                Err(e) => {
                    tracing::warn!(scope = %scope_id, path = %path, error = %e,
                        "vector drain: concept unloadable — item dropped");
                }
            }
        }

        let wrote = self
            .embed_and_upsert(client, cfg, vs, scope_id, pairs)
            .await;
        if wrote {
            // The controller's contract: the scope's decoded cache
            // invalidates ONCE per tick-side write phase (the drain's
            // upserts; the rebuild invalidates again).
            invalidate_scope(vs.scope_id());
        }
    }

    /// The belt: mark the row of a token whose concept is gone for
    /// removal (Ok on a miss — the row may already be gone).
    async fn belt(&self, vs: &VectorStore<'_>, token: &str) {
        if let Err(e) = vs.remove_row_by_token(token).await {
            tracing::warn!(
                scope = vs.scope_id(),
                token = %token,
                error = %e,
                "vector belt: row removal failed"
            );
        }
    }

    /// Embed `pairs` in `cfg.batch`-sized batches and upsert one row per
    /// concept. Per batch: up to [`EMBED_ATTEMPTS`] backend attempts
    /// (the passive hooks ride every attempt), then the batch's items
    /// are dropped with a bounded warn. A Red verdict re-checked before
    /// each attempt abandons the REST of this scope's work in one warn
    /// (a dead backend must pause the tick, not hammer it). Returns
    /// whether any upsert landed (the caller invalidates once).
    async fn embed_and_upsert(
        &self,
        client: &EmbeddingsClient,
        cfg: &EmbeddingsConfig,
        vs: &VectorStore<'_>,
        scope_id: &str,
        pairs: Vec<(String, String)>,
    ) -> bool {
        if pairs.is_empty() {
            return false;
        }
        let batch = usize::try_from(cfg.batch).unwrap_or(pairs.len());
        let batch = batch.max(1);
        let total = pairs.len();
        let mut wrote = false;
        let mut idx = 0usize;
        while idx < total {
            if embedding_state() == BackendState::Red {
                tracing::warn!(
                    scope = %scope_id,
                    remaining = total - idx,
                    "embedding paused — backend red: this drain's remaining items dropped"
                );
                break;
            }
            let chunk = &pairs[idx..(idx + batch).min(total)];
            idx += chunk.len();
            let texts: Vec<String> = chunk.iter().map(|(_, text)| text.clone()).collect();
            let mut vecs = None;
            let mut last_err = None;
            for attempt in 0..EMBED_ATTEMPTS {
                if attempt > 0 && embedding_state() == BackendState::Red {
                    break;
                }
                match client.embed_batch(&texts).await {
                    Ok(v) => {
                        vecs = Some(v);
                        break;
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            let Some(ordered) = vecs else {
                let err = last_err
                    .map(|e| short_detail(&e))
                    .unwrap_or_else(|| "backend red".to_string());
                tracing::warn!(
                    scope = %scope_id,
                    count = texts.len(),
                    reason = err,
                    "embedding batch dropped after retries"
                );
                continue;
            };

            for (slot, (path, _)) in chunk.iter().enumerate() {
                let Some(vec) = ordered.get(slot) else {
                    tracing::warn!(scope = %scope_id, path = %path, slot,
                        "embedding batch returned too few vectors — slot skipped (unreachable per ordered_vectors' contract, which rejects a short batch as malformed)");
                    continue;
                };
                match vs.upsert(path, &cfg.model, vec.len() as u32, vec).await {
                    Ok(_) => {
                        VECTOR_PROCESSED_TOTAL.fetch_add(1, Ordering::Relaxed);
                        wrote = true;
                    }
                    Err(e) => {
                        tracing::warn!(scope = %scope_id, path = %path, error = %e,
                            "vector upsert failed — item's row skipped (re-embeds on the next edit or boot backlog)");
                    }
                }
            }
        }
        wrote
    }

    /// Step 6: every scope with stale rows (deterministic order), each
    /// re-embedding at most [`STALE_ROWS_PER_TICK`] rows this tick from
    /// the registry's own backlog enumeration, then invalidating again.
    async fn rebuild_stale_scopes(&self, client: &EmbeddingsClient, cfg: &EmbeddingsConfig) {
        let scopes: Vec<(String,)> = match sqlx::query_as(
            "SELECT DISTINCT scope_id FROM search_vectors WHERE stale = 1 ORDER BY scope_id",
        )
        .fetch_all(self.store.pool())
        .await
        {
            Ok(scopes) => scopes,
            Err(e) => {
                tracing::warn!(error = %e, "stale scope lookup failed — rebuild skips this tick");
                return;
            }
        };
        for (scope_id,) in scopes {
            let Some(cs) = self.resolve_scope(&scope_id).await else {
                tracing::warn!(scope = %scope_id, "stale scope unresolvable — rebuild skipped");
                continue;
            };
            let vs = cs.vectors();
            let learned = EMBEDDING_DIMS.load(Ordering::Relaxed);
            let mut paths = match vs.backlog_paths(&cfg.model, learned).await {
                Ok(paths) => paths,
                Err(e) => {
                    tracing::warn!(scope = %scope_id, error = %e,
                        "stale rebuild: enumeration failed — scope skipped this tick");
                    continue;
                }
            };
            paths.truncate(STALE_ROWS_PER_TICK);
            let mut pairs: Vec<(String, String)> = Vec::with_capacity(paths.len());
            for path in paths {
                match cs.get(&path).await {
                    Ok(concept) => {
                        let text = mycelium_core::search::searchable_text(&concept);
                        pairs.push((path, text));
                    }
                    Err(e) => {
                        // A registry path whose concept is gone — the
                        // same belt as the drain: the row must not sit
                        // stale forever (it would re-enumerate every
                        // tick).
                        if matches!(e, ConceptStoreError::NotFound(_)) {
                            let token = vs.path_token_of(&path);
                            self.belt(vs, &token).await;
                        } else {
                            tracing::warn!(scope = %scope_id, path = %path, error = %e,
                                "stale rebuild: concept unloadable — path skipped");
                        }
                    }
                }
            }
            let wrote = self
                .embed_and_upsert(client, cfg, vs, &scope_id, pairs)
                .await;
            // The controller's AGAIN rule: after the stale-rebuild's
            // upserts, the scope's cache invalidates once more.
            if wrote {
                invalidate_scope(vs.scope_id());
            }
        }
    }

    /// Step 7 (first tick only): every scope's paths lacking their row
    /// re-enter the queue — writes that happened before a worker ran.
    /// Overflow drops NEW items (the queue's own rule); whatever was
    /// dropped resumes on the next boot.
    async fn enqueue_boot_backlog(&self, model: &str, learned: u32) {
        // The two service scopes (sweep-targets order: skills, library).
        for ns in ["skills", "library"] {
            let vs = VectorStore::for_service(self.store.pool(), &self.service_key, ns);
            self.enqueue_scope_backlog(&vs, model, learned).await;
        }
        // Users, below the auth layer: the sweep-targets enumeration.
        let users: Vec<(String,)> = match sqlx::query_as("SELECT id FROM users ORDER BY id")
            .fetch_all(self.store.pool())
            .await
        {
            Ok(users) => users,
            Err(e) => {
                tracing::warn!(error = %e, "boot backlog: user enumeration failed");
                vec![]
            }
        };
        for (id,) in users {
            let Ok(user_id) = Uuid::parse_str(&id) else {
                tracing::warn!(user = %id, "boot backlog: user row id unreadable — scope skipped");
                continue;
            };
            let master = match (self.recovery)(user_id).await {
                Ok(master) => master,
                Err(e) => {
                    tracing::warn!(user_id = %user_id, error = %e,
                        "boot backlog: master key unrecoverable — user scope skipped (backlog resumes at the next boot; queue writes retry next tick)");
                    continue;
                }
            };
            let vs = VectorStore::for_user(self.store.pool(), user_id, &master);
            self.enqueue_scope_backlog(&vs, model, learned).await;
        }
    }

    /// One scope's backlog enumeration → queue writes (deduped by the
    /// queue; capacity drops new items).
    async fn enqueue_scope_backlog(&self, vs: &VectorStore<'_>, model: &str, learned: u32) {
        let paths = match vs.backlog_paths(model, learned).await {
            Ok(paths) => paths,
            Err(e) => {
                tracing::warn!(scope = vs.scope_id(), error = %e,
                    "boot backlog: enumeration failed — scope skipped");
                return;
            }
        };
        let mut enqueued = 0usize;
        for path in paths {
            let token = vs.path_token_of(&path);
            enqueue_write(vs.scope_id(), &token);
            enqueued += 1;
        }
        if enqueued > 0 {
            tracing::info!(
                scope = vs.scope_id(),
                enqueued = enqueued,
                "boot backlog: missing vector rows enqueued"
            );
        }
    }

    /// Resolve a scope id to its concept store (registry + files +
    /// vectors accessor all ride the same keys).
    async fn resolve_scope(&self, scope_id: &str) -> Option<ConceptStore<'_>> {
        if let Some(user_id) = scope_id.strip_prefix("user:") {
            let Ok(user_id) = Uuid::parse_str(user_id) else {
                tracing::warn!(scope = %scope_id, "unparseable user scope id — scope skipped");
                return None;
            };
            match (self.recovery)(user_id).await {
                Ok(master) => Some(ConceptStore::for_user(self.store.as_ref(), user_id, master)),
                Err(e) => {
                    tracing::warn!(scope = %scope_id, error = %e,
                        "user master key unrecoverable — vector scope skipped");
                    None
                }
            }
        } else if let Some(ns) = scope_id.strip_prefix("global:") {
            match ns {
                "library" => Some(ConceptStore::for_service(
                    self.store.as_ref(),
                    (*self.service_key).clone(),
                    &self.store.library_dir(),
                    "library",
                )),
                "skills" => Some(ConceptStore::for_service(
                    self.store.as_ref(),
                    (*self.service_key).clone(),
                    &self.store.skills_dir(),
                    "skills",
                )),
                other => {
                    tracing::warn!(scope = %scope_id, namespace = other,
                        "unknown global namespace — vector scope skipped");
                    None
                }
            }
        } else {
            tracing::warn!(scope = %scope_id, "unknown scope id — vector scope skipped");
            None
        }
    }
}
