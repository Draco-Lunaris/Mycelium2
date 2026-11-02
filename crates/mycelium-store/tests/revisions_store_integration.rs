//! RevisionStore integration tests (memory-lifecycle spec §3.2 + §3.2a):
//! the write-ahead undo log — monotonic revision numbers, purpose-bound
//! AEAD payloads under the user master key / service key, retention
//! pruning, the revisions-browser per-path index (dead paths included),
//! scope isolation, and the sealed `lifecycle` config row that drives
//! retention. Real `Store::open` over a temp dir — the table behaves
//! exactly as production will see it, no hand-rolled schema.

use mycelium_core::concept::{Concept, Frontmatter};
use mycelium_crypto::keys::{ServiceKey, generate_master_key};
use mycelium_crypto::store_keys::IndexKeys;
use mycelium_crypto::{aead_open, aead_seal};
use mycelium_store::revisions::{
    DEFAULT_CONSOLIDATION_PATCHES, DEFAULT_DEDUP_COSINE, DEFAULT_DREAM_INTERVAL_S,
    DEFAULT_QUERY_TOP_K, DEFAULT_RECENT_WINDOW, DEFAULT_REVISION_RETENTION, DEFAULT_RRF_K,
    DEFAULT_RRF_WEIGHT, DEFAULT_STALE_DAYS, LifecycleConfig, RevisionStore, RevisionsError,
    consolidation_patches, dedup_cosine, op_summary, query_top_k, recent_window,
    revision_retention, rrf_params, set_consolidation_patches, set_dedup_cosine, set_query_top_k,
    set_recent_window, set_revision_retention, set_rrf_k, set_rrf_weight_lex, set_rrf_weight_vec,
    set_stale_days, stale_days, sync_lifecycle_from_store,
};
use mycelium_store::{ConceptStore, ConfigStore, Store, config};
use uuid::Uuid;

/// Every test in this binary takes this lock: the retention static is
/// process-global and the default harness runs a binary's tests in
/// parallel, so the retention-setting tests would race the record-heavy
/// tests into nondeterminism. Every test ends with the global at
/// `DEFAULT_REVISION_RETENTION`, making entry order irrelevant.
static RETENTION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn retention_lock() -> tokio::sync::MutexGuard<'static, ()> {
    RETENTION_LOCK.lock().await
}

/// Restores the process-global `query_top_k` on drop, so a failing
/// assertion cannot leak a set value into the next test (the store-side
/// counterpart of the MCP suite's guard; the retention tests keep their
/// literal end-of-test resets — pre-existing idiom, out of this branch's
/// churn).
struct TopKDefaults;

impl Drop for TopKDefaults {
    fn drop(&mut self) {
        set_query_top_k(DEFAULT_QUERY_TOP_K);
    }
}

/// Restores the four memory-lifecycle step-4 fusion statics on drop —
/// the same shape as [`TopKDefaults`], declared AFTER the lock so the
/// restore happens while the lock is held.
struct FusionDefaults;

impl Drop for FusionDefaults {
    fn drop(&mut self) {
        set_rrf_k(DEFAULT_RRF_K);
        set_rrf_weight_lex(DEFAULT_RRF_WEIGHT);
        set_rrf_weight_vec(DEFAULT_RRF_WEIGHT);
        set_dedup_cosine(DEFAULT_DEDUP_COSINE);
    }
}

/// Restores the three memory-lifecycle step-5 dream-signal statics on
/// drop — [`FusionDefaults`]' shape, declared AFTER the lock so the
/// restore happens while the lock is held.
struct SignalDefaults;

impl Drop for SignalDefaults {
    fn drop(&mut self) {
        set_stale_days(DEFAULT_STALE_DAYS);
        set_recent_window(DEFAULT_RECENT_WINDOW);
        set_consolidation_patches(DEFAULT_CONSOLIDATION_PATCHES);
    }
}

async fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    (dir, store)
}

/// A concept for the live-side of the `list_paths` test (registry row via
/// `ConceptStore::put`), same shape as the `concept()` helper in the
/// concept_store unit tests.
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

/// A raw-concept-markdown revision payload — frontmatter included, as
/// Task 2's ConceptStore boundary will pass it.
fn markdown(body: &str) -> Vec<u8> {
    format!("---\ntype: note\ntitle: t\n---\n{body}\n").into_bytes()
}

/// Substring check over the raw at-rest bytes ([`[u8]::contains`] is
/// element-typed; the substring test is what the assertion wants).
fn looks_like(needle: &[u8], haystack: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn record_assigns_monotonic_revision_numbers() {
    let _retention = retention_lock().await;
    set_revision_retention(DEFAULT_REVISION_RETENTION);
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let rs = RevisionStore::for_user(store.pool(), user, &master);
    let path = "/notes/rev.md";

    let n1 = rs
        .record(path, "create", "concept created", &markdown("v1"))
        .await
        .unwrap();
    let n2 = rs
        .record_as(
            path,
            "update",
            "web:dana",
            "concept updated",
            &markdown("v2"),
        )
        .await
        .unwrap();
    assert_eq!(n1, 1, "first revision of the path is no. 1");
    assert_eq!(n2, 2, "the next record assigns MAX+1");

    let entries = rs.list(path).await.unwrap();
    assert_eq!(entries.len(), 2);
    // Newest first.
    assert_eq!(entries[0].revision_no, 2);
    assert_eq!(entries[1].revision_no, 1);
    assert_eq!(entries[0].op, "update");
    assert_eq!(entries[1].op, "create");
    assert_eq!(entries[1].summary, "concept created");
    // Provenance: explicit via record_as, '' (unlabeled) via record.
    assert_eq!(entries[0].source, "web:dana");
    assert_eq!(entries[1].source, "");
    assert!(!entries[0].created_at.is_empty());
    // Meta only: the serialized entry carries op/source/summary/created_at
    // and structurally cannot carry the snapshot text — the payload is
    // reachable only through `get`.
    let json = serde_json::to_string(&entries[0]).unwrap();
    assert!(json.contains("\"op\":\"update\""));
    assert!(!json.contains("type:"));

    // (R3 rider) The empty-payload convention (§3.2a-3) pins at the raw
    // layer too: a recorded EMPTY payload lists as empty in the meta,
    // the markdown-payload rows do not. No decrypt is involved — the
    // meta derives from the stored envelope length.
    let n3 = rs
        .record(path, "restore", "restored to revision 1", &[])
        .await
        .unwrap();
    assert_eq!(n3, 3, "the empty restore row joins the chain normally");
    let empties = rs.list(path).await.unwrap();
    assert!(
        empties[0].payload_empty,
        "the recorded-empty restore row lists empty: {empties:?}"
    );
    assert!(
        !empties[1].payload_empty && !empties[2].payload_empty,
        "the markdown-payload rows list non-empty"
    );
}

#[tokio::test]
async fn get_decodes_the_raw_payload_with_aad_binding() {
    let _retention = retention_lock().await;
    set_revision_retention(DEFAULT_REVISION_RETENTION);
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let rs = RevisionStore::for_user(store.pool(), user, &master);
    let keys = IndexKeys::from_master_key(&master);
    let scope_id = format!("user:{user}");
    let path = "/notes/aad.md";
    let token = rs.path_token_of(path);
    let p1 = markdown("first body zebra");
    let p2 = markdown("second body yak");

    rs.record(path, "create", "concept created", &p1)
        .await
        .unwrap();
    let n2 = rs
        .record(path, "update", "concept updated", &p2)
        .await
        .unwrap();
    assert_eq!(n2, 2);

    assert_eq!(rs.get(path, 1).await.unwrap(), p1);
    assert_eq!(rs.get(path, 2).await.unwrap(), p2);

    // Cross-AAD tamper: take revision 2's stored payload, open it under
    // its own AAD, re-seal the same plaintext under REVISION 1's AAD and
    // write it back to the row. `get(path, 2)` opens with
    // `rev:<scope>:<token>:2` — a revision blob is only ever meaningful
    // against its own number (pinned: a revision payload replayed as a
    // different revision fails authentication).
    let rev_aad = |n: u64| format!("rev:{scope_id}:{token}:{n}");
    let (stored2,): (Vec<u8>,) = sqlx::query_as(
        "SELECT payload FROM concept_revisions
         WHERE scope_id = ? AND path_token = ? AND revision_no = ?",
    )
    .bind(&scope_id)
    .bind(&token)
    .bind(2i64)
    .fetch_one(store.pool())
    .await
    .unwrap();
    let plain = aead_open(&stored2, rev_aad(2).as_bytes(), keys.index_dek()).unwrap();
    let tampered = aead_seal(&plain, rev_aad(1).as_bytes(), keys.index_dek()).unwrap();
    sqlx::query(
        "UPDATE concept_revisions SET payload = ?
         WHERE scope_id = ? AND path_token = ? AND revision_no = ?",
    )
    .bind(&tampered)
    .bind(&scope_id)
    .bind(&token)
    .bind(2i64)
    .execute(store.pool())
    .await
    .unwrap();

    assert!(
        rs.get(path, 2).await.is_err(),
        "a payload re-sealed under revision 1's AAD must not open for revision 2"
    );
    // Revision 1 is untouched — the failure is the per-revision AAD
    // binding, not the chain or the key.
    assert_eq!(rs.get(path, 1).await.unwrap(), p1);
    // Unknown revision → typed NotFound.
    assert!(matches!(
        rs.get(path, 99).await,
        Err(RevisionsError::NotFound { .. })
    ));
}

#[tokio::test]
async fn payloads_encrypt_at_rest() {
    let _retention = retention_lock().await;
    set_revision_retention(DEFAULT_REVISION_RETENTION);
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let rs = RevisionStore::for_user(store.pool(), user, &master);
    let scope_id = format!("user:{user}");
    let path = "/notes/encrypted.md";
    let token = rs.path_token_of(path);
    let payload = markdown("classified body zebra");

    rs.record(path, "create", "concept created", &payload)
        .await
        .unwrap();

    let (stored,): (Vec<u8>,) = sqlx::query_as(
        "SELECT payload FROM concept_revisions WHERE scope_id = ? AND path_token = ?",
    )
    .bind(&scope_id)
    .bind(&token)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert!(
        !looks_like(b"type:", &stored) && !looks_like(b"classified", &stored),
        "the at-rest blob must not contain frontmatter or body text"
    );
    assert!(
        !looks_like(b"---\ntype:", &stored),
        "raw markdown must never be stored as the payload column"
    );
    // ...while `get` decodes it back exactly.
    assert_eq!(rs.get(path, 1).await.unwrap(), payload);
}

#[tokio::test]
async fn list_paths_includes_dead_paths_and_liveness() {
    let _retention = retention_lock().await;
    set_revision_retention(DEFAULT_REVISION_RETENTION);
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let cs = ConceptStore::for_user(&store, user, master.clone());
    let rs = RevisionStore::for_user(store.pool(), user, &master);

    let alive = "/notes/alive.md";
    let dead = "/notes/dead.md";

    // Task 2 wired the boundary hook: the ConceptStore's own puts and
    // deletes write the rows now. `alive` keeps a single-put chain;
    // `dead` is put and then deleted — dead, but its chain stays listed.
    cs.put(&concept(alive, "Alive", "kept body zebra"))
        .await
        .unwrap();

    cs.put(&concept(dead, "Dead", "doomed body")).await.unwrap();
    cs.delete(dead).await.unwrap();

    let paths = rs.list_paths().await.unwrap();
    assert_eq!(
        paths.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
        vec![alive, dead],
        "deterministic path order, dead paths included (§3.2a-1 exists for this)"
    );
    let alive_row = &paths[0];
    let dead_row = &paths[1];
    assert!(alive_row.live, "scope_files still has the live path");
    assert_eq!(alive_row.latest_no, 1);
    assert_eq!(alive_row.last_op, "create");
    assert!(!dead_row.live, "a deleted concept is dead but still listed");
    assert_eq!(dead_row.latest_no, 2);
    assert_eq!(dead_row.last_op, "delete");
    assert!(!alive_row.last_op_at.is_empty());
    assert!(!dead_row.last_op_at.is_empty());

    // A delete row carries the pre-delete state (hook contract §3.2a-3) —
    // restoring it re-creates the concept (pinned fully by Task 2's
    // restore tests; here: the dead path's chain stays nameable).
    assert_eq!(
        rs.get(dead, 2).await.unwrap(),
        rs.get(dead, 1).await.unwrap()
    );
}

#[tokio::test]
async fn retention_prunes_oldest_first() {
    let _retention = retention_lock().await;
    set_revision_retention(DEFAULT_REVISION_RETENTION);
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let rs = RevisionStore::for_user(store.pool(), user, &master);
    let scope_id = format!("user:{user}");
    let path = "/notes/ret.md";
    let token = rs.path_token_of(path);

    // Below retention: five records land untouched.
    for i in 1..=5 {
        rs.record(
            path,
            "update",
            "concept updated",
            &markdown(&format!("v{i}")),
        )
        .await
        .unwrap();
    }

    // Lowering the retention to 3 clamps UP to the range floor (25 —
    // the clamp contract pinned by `retention_clamps_everywhere`), so
    // the brief's original 6-row step is expressed here as a chain long
    // enough for the clamped floor to bite: prune keeps the newest
    // `retention` rows per concept (§3.2a-4: `revision_no <= MAX -
    // retention`), deleting oldest-revision-first.
    set_revision_retention(3);
    assert_eq!(revision_retention(), 25, "set(3) holds the clamped floor");
    // Fill up to the clamped floor (nothing prunes while the chain is
    // at or under it), then step one over it.
    for i in 6..=25 {
        rs.record(
            path,
            "update",
            "concept updated",
            &markdown(&format!("v{i}")),
        )
        .await
        .unwrap();
    }
    let n26 = rs
        .record(path, "update", "concept updated", &markdown("v26"))
        .await
        .unwrap();
    assert_eq!(n26, 26, "pruning does not disturb the monotonic numbering");
    let entries = rs.list(path).await.unwrap();
    let nos: Vec<u64> = entries.iter().map(|e| e.revision_no).collect();
    assert_eq!(
        nos,
        (2..=26u64).rev().collect::<Vec<_>>(),
        "the newest 25 only"
    );
    // Exactly the oldest rows fall off, one cut deeper per record.
    for i in 27..=31 {
        rs.record(
            path,
            "update",
            "concept updated",
            &markdown(&format!("v{i}")),
        )
        .await
        .unwrap();
    }
    let entries = rs.list(path).await.unwrap();
    assert_eq!(
        entries.iter().map(|e| e.revision_no).collect::<Vec<_>>(),
        (7..=31u64).rev().collect::<Vec<_>>(),
        "oldest deleted first, newest retention rows kept"
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM concept_revisions WHERE scope_id = ? AND path_token = ?",
    )
    .bind(&scope_id)
    .bind(&token)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(count, 25, "physically deleted, not just hidden");
    assert_eq!(rs.get(path, 7).await.unwrap(), markdown("v7"));
    assert!(
        matches!(rs.get(path, 1).await, Err(RevisionsError::NotFound { .. })),
        "pruned rows are gone for good"
    );

    // Leave the global at the default for the other tests in the binary.
    set_revision_retention(DEFAULT_REVISION_RETENTION);
}

#[tokio::test]
async fn retention_clamps_everywhere() {
    let _retention = retention_lock().await;
    // Below the floor → the floor.
    set_revision_retention(1);
    assert_eq!(revision_retention(), 25);
    // Above the ceiling → the ceiling.
    set_revision_retention(10_000);
    assert_eq!(revision_retention(), 2000);
    // In-range passes through untouched.
    set_revision_retention(500);
    assert_eq!(revision_retention(), 500);
    // The "never set" default is the constant this process-global is
    // initialized from (tests in this binary share the static and set
    // it back to the default on exit — the constant is the pin).
    assert_eq!(DEFAULT_REVISION_RETENTION, 200);
    set_revision_retention(DEFAULT_REVISION_RETENTION);
}

#[tokio::test]
async fn sync_lifecycle_row_reads_clamped_value() {
    let _retention = retention_lock().await;
    // Deliberately NOT the default: an absent row must RESET the global
    // to the default, not leave whatever was live before the sync (a
    // sync that no-oped on the absent row would still pass a
    // default-before/default-after assert).
    set_revision_retention(500);
    let (_dir, store) = store().await;
    let config = ConfigStore::new(store.pool().clone());
    let service_key = ServiceKey::from_bytes(&[7u8; 32]).unwrap();

    // Absent row → the defaults (spec §3.2: "default 200 when the row is
    // absent"; the llm-row pattern — missing/blank keeps defaults).
    sync_lifecycle_from_store(&config, &service_key)
        .await
        .unwrap();
    assert_eq!(
        revision_retention(),
        200,
        "absent lifecycle row resets the global to the default"
    );

    // A persisted value of 9 clamps ON LOAD: the global can never leave
    // 25..=2000, whatever an older or hand-edited row contains (clamp on
    // load mirrors the clamp on save).
    let sealed = config::seal_config(
        &service_key,
        "lifecycle",
        &LifecycleConfig {
            revision_retention: 9,
            ..Default::default()
        },
    );
    config.set("lifecycle", &sealed).await.unwrap();
    sync_lifecycle_from_store(&config, &service_key)
        .await
        .unwrap();
    assert_eq!(revision_retention(), 25, "stored value clamps on load");

    set_revision_retention(DEFAULT_REVISION_RETENTION);
}

#[tokio::test]
async fn query_top_k_clamps_everywhere() {
    let _retention = retention_lock().await;
    let _topk = TopKDefaults;
    // Below the floor → the floor: zero stays meaningful only as an
    // explicit CALLER request (spec §3.2a-7), never as a stored/static
    // value.
    set_query_top_k(0);
    assert_eq!(query_top_k(), 1);
    // Above the ceiling → the ceiling.
    set_query_top_k(100);
    assert_eq!(query_top_k(), 64);
    // In-range passes through untouched.
    set_query_top_k(2);
    assert_eq!(query_top_k(), 2);
    // The "never set" default is the constant this process-global is
    // initialized from (the retention pin's shape, mirrored); the
    // TopKDefaults guard restores the default on exit).
    assert_eq!(DEFAULT_QUERY_TOP_K, 8);
}

#[tokio::test]
async fn sync_lifecycle_row_reads_clamped_query_top_k() {
    let _retention = retention_lock().await;
    let _topk = TopKDefaults;
    // Deliberately NOT the default: an absent row must RESET the global
    // to the default, not leave whatever was live before the sync (the
    // retention sync test's reasoning, mirrored).
    set_query_top_k(16);
    let (_dir, store) = store().await;
    let config = ConfigStore::new(store.pool().clone());
    let service_key = ServiceKey::from_bytes(&[7u8; 32]).unwrap();

    // Absent row → the defaults (spec §3.3: "default 8 when the row is
    // absent"; the llm-row pattern — missing/blank keeps defaults).
    sync_lifecycle_from_store(&config, &service_key)
        .await
        .unwrap();
    assert_eq!(
        query_top_k(),
        8,
        "absent lifecycle row resets the top-k global to the default"
    );

    // Persisted values clamp ON LOAD: 0 → the floor (an unbounded query
    // can never leave the static), 200 → the ceiling (clamp on load
    // mirrors the clamp on save, as with retention).
    let sealed = config::seal_config(
        &service_key,
        "lifecycle",
        &serde_json::json!({
            "revision_retention": DEFAULT_REVISION_RETENTION,
            "query_top_k": 0
        }),
    );
    config.set("lifecycle", &sealed).await.unwrap();
    sync_lifecycle_from_store(&config, &service_key)
        .await
        .unwrap();
    assert_eq!(query_top_k(), 1, "stored zero clamps to the floor on load");

    let sealed = config::seal_config(
        &service_key,
        "lifecycle",
        &serde_json::json!({
            "revision_retention": DEFAULT_REVISION_RETENTION,
            "query_top_k": 200
        }),
    );
    config.set("lifecycle", &sealed).await.unwrap();
    sync_lifecycle_from_store(&config, &service_key)
        .await
        .unwrap();
    assert_eq!(
        query_top_k(),
        64,
        "stored 200 clamps to the ceiling on load"
    );
}

#[tokio::test]
async fn lifecycle_old_row_defaults_query_top_k() {
    let _retention = retention_lock().await;
    let _topk = TopKDefaults;
    // Deliberately NOT the default: a sync over the old row would
    // otherwise look like a no-op.
    set_query_top_k(2);
    let (_dir, store) = store().await;
    let config = ConfigStore::new(store.pool().clone());
    let service_key = ServiceKey::from_bytes(&[7u8; 32]).unwrap();

    // An older build's row — its JSON predates the field. serde's
    // `#[serde(default = ...)]` keeps it valid: the field reads as the
    // default 8, both through a direct parse and through the sync.
    let sealed = config::seal_config(
        &service_key,
        "lifecycle",
        &serde_json::json!({ "revision_retention": 200 }),
    );
    config.set("lifecycle", &sealed).await.unwrap();
    let parsed = config::get_sealed::<LifecycleConfig>(&config, &service_key, "lifecycle")
        .await
        .unwrap();
    assert_eq!(
        parsed.query_top_k, 8,
        "an old row (no query_top_k in its JSON) keeps the serde default"
    );
    sync_lifecycle_from_store(&config, &service_key)
        .await
        .unwrap();
    assert_eq!(query_top_k(), 8, "the sync applies the defaulted field");
}

/// A hand-edited row carrying only the pre-fusion fields: an older
/// build's shape, kept valid by the fusion fields' serde defaults.
fn old_row_json() -> serde_json::Value {
    serde_json::json!({ "revision_retention": 200, "query_top_k": 8 })
}

#[tokio::test]
async fn lifecycle_row_gains_the_fusion_fields() {
    let _retention = retention_lock().await;
    let _fusion = FusionDefaults;
    // Deliberately NOT the defaults: a sync over the old row would
    // otherwise look like a no-op (the top-k test's reasoning, mirrored).
    set_rrf_k(90);
    set_dedup_cosine(0.5);
    let (_dir, store) = store().await;
    let config = ConfigStore::new(store.pool().clone());
    let service_key = ServiceKey::from_bytes(&[7u8; 32]).unwrap();

    // Absent row → the defaults (the llm-row pattern: missing keeps
    // defaults, so an absent row RESETS the global, it never keeps).
    sync_lifecycle_from_store(&config, &service_key)
        .await
        .unwrap();
    let params = rrf_params();
    assert_eq!(params.rrf_k, 60.0, "absent row resets rrf_k");
    assert_eq!(params.w_lex, 1.0, "absent row resets the lexical weight");
    assert_eq!(params.w_vec, 1.0, "absent row resets the vector weight");
    assert_eq!(
        dedup_cosine(),
        0.92,
        "absent row resets the dedup threshold"
    );

    // An older build's row predates the fusion fields; serde's
    // `#[serde(default)]` keeps it valid — the fields parse as the
    // defaults, both through a direct read and through the sync.
    config
        .set(
            "lifecycle",
            &config::seal_config(&service_key, "lifecycle", &old_row_json()),
        )
        .await
        .unwrap();
    let parsed = config::get_sealed::<LifecycleConfig>(&config, &service_key, "lifecycle")
        .await
        .unwrap();
    assert_eq!(parsed.rrf_k, 60, "old row default rrf_k");
    assert_eq!(parsed.rrf_weight_lex, 1.0, "old row default lexical weight");
    assert_eq!(parsed.rrf_weight_vec, 1.0, "old row default vector weight");
    assert_eq!(parsed.dedup_cosine, 0.92, "old row default dedup threshold");

    // A stored row stamps ALL the fusion statics through the sync, u32 →
    // f32 at the seam (`rrf_params`).
    let sealed = config::seal_config(
        &service_key,
        "lifecycle",
        &serde_json::json!({
            "revision_retention": 200,
            "query_top_k": 8,
            "rrf_k": 100,
            "rrf_weight_lex": 0.5,
            "rrf_weight_vec": 0.25,
            "dedup_cosine": 0.99
        }),
    );
    config.set("lifecycle", &sealed).await.unwrap();
    sync_lifecycle_from_store(&config, &service_key)
        .await
        .unwrap();
    let params = rrf_params();
    assert_eq!(params.rrf_k, 100.0, "sync stamps the stored rrf_k");
    assert_eq!(params.w_lex, 0.5, "sync stamps the stored lexical weight");
    assert_eq!(params.w_vec, 0.25, "sync stamps the stored vector weight");
    assert_eq!(dedup_cosine(), 0.99, "sync stamps the stored threshold");

    // Clamps bind at the setters (which are also the LOAD-side clamp —
    // a hand-edited row can never widen the tunables). Floors:
    set_rrf_k(0);
    assert_eq!(rrf_params().rrf_k, 1.0, "rrf_k floor (range 1..=1024)");
    set_rrf_weight_lex(-3.0);
    assert_eq!(rrf_params().w_lex, 0.0, "weight floor");
    set_dedup_cosine(0.1);
    assert_eq!(dedup_cosine(), 0.5, "dedup floor");
    // ...and ceilings (the brief's sketch pins w 12 → 10 and cosine 2.0
    // → 1.0 explicitly).
    set_rrf_k(2000);
    assert_eq!(rrf_params().rrf_k, 1024.0, "rrf_k ceiling");
    set_rrf_weight_lex(12.0);
    assert_eq!(rrf_params().w_lex, 10.0, "weight ceiling");
    set_rrf_weight_vec(12.0);
    assert_eq!(rrf_params().w_vec, 10.0, "weight ceiling");
    set_dedup_cosine(2.0);
    assert_eq!(dedup_cosine(), 1.0, "dedup ceiling");

    // NaN guards every f32 field: the setters fold NaN to the field's
    // default (f32::clamp propagates NaN — the explicit guard is the
    // only thing standing between a NaN row and a NaN-scored composer).
    set_rrf_weight_lex(f32::NAN);
    assert_eq!(rrf_params().w_lex, DEFAULT_RRF_WEIGHT);
    set_rrf_weight_vec(f32::NAN);
    assert_eq!(rrf_params().w_vec, DEFAULT_RRF_WEIGHT);
    set_dedup_cosine(f32::NAN);
    assert_eq!(dedup_cosine(), DEFAULT_DEDUP_COSINE);

    // The struct clamp the admin save path applies carries the same
    // NaN guard (serde_json cannot encode NaN, but a constructed row
    // can reach it directly — the clamp is the save-side guard).
    let nan_row = LifecycleConfig {
        rrf_weight_lex: f32::NAN,
        rrf_weight_vec: f32::NAN,
        dedup_cosine: f32::NAN,
        ..Default::default()
    };
    let clamped = nan_row.clamp();
    assert_eq!(clamped.rrf_weight_lex, DEFAULT_RRF_WEIGHT);
    assert_eq!(clamped.rrf_weight_vec, DEFAULT_RRF_WEIGHT);
    assert_eq!(clamped.dedup_cosine, DEFAULT_DEDUP_COSINE);
}

#[test]
fn op_summary_pins_all_seven_ops_and_rejects_unknown() {
    assert_eq!(op_summary("create", None).unwrap(), "concept created");
    assert_eq!(op_summary("update", None).unwrap(), "concept updated");
    assert_eq!(op_summary("delete", None).unwrap(), "concept deleted");
    assert_eq!(
        op_summary("restore", Some(7)).unwrap(),
        "restored to revision 7"
    );
    assert_eq!(op_summary("merge", None).unwrap(), "dream merge");
    assert_eq!(
        op_summary("consolidate", None).unwrap(),
        "consolidated body history"
    );
    assert_eq!(
        op_summary("migration", None).unwrap(),
        "deprecated tag migrated to status: retired"
    );
    // `restore` without its target revision is a caller bug — loud, not
    // a mangled summary.
    assert!(op_summary("restore", None).is_err());
    // Unknown ops (and the empty op) fail loudly.
    assert!(op_summary("purge", None).is_err());
    assert!(op_summary("", None).is_err());
}

#[tokio::test]
async fn concurrent_same_path_puts_all_revise() {
    let _retention = retention_lock().await;
    set_revision_retention(DEFAULT_REVISION_RETENTION);
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let rs = RevisionStore::for_user(store.pool(), user, &master);
    let scope_id = format!("user:{user}");
    let path = "/notes/race.md";
    let token = rs.path_token_of(path);

    // Four concurrent writers on one path, interleaved at every await
    // point, three records each — the MAX+1 assign plus the UNIQUE key
    // and its retry must absorb every race; no writer's record errors
    // and no number is lost or duplicated.
    async fn writer(rs: &RevisionStore<'_>, tag: &str, path: &str) {
        for j in 1..=3 {
            rs.record(
                path,
                "update",
                "concept updated",
                &markdown(&format!("{tag} r{j}")),
            )
            .await
            .unwrap();
        }
    }
    tokio::join!(
        writer(&rs, "w1", path),
        writer(&rs, "w2", path),
        writer(&rs, "w3", path),
        writer(&rs, "w4", path)
    );

    let nos: Vec<u64> = rs
        .list(path)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.revision_no)
        .collect();
    assert_eq!(
        nos,
        (1..=12u64).rev().collect::<Vec<_>>(),
        "12 contiguous newest-first revisions, no gaps"
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM concept_revisions WHERE scope_id = ? AND path_token = ?",
    )
    .bind(&scope_id)
    .bind(&token)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(count, 12);
}

#[tokio::test]
async fn service_scopes_isolate_revisions() {
    let _retention = retention_lock().await;
    set_revision_retention(DEFAULT_REVISION_RETENTION);
    let (_dir, store) = store().await;
    let service_key = ServiceKey::from_bytes(&[5u8; 32]).unwrap();
    let skills = RevisionStore::for_service(store.pool(), &service_key, "skills");
    let library = RevisionStore::for_service(store.pool(), &service_key, "library");

    let path = "/shared/pkg.md";
    // Same key ⇒ the same path token — isolation rides entirely on
    // scope_id (exactly like the registry/index namespaces).
    assert_eq!(skills.path_token_of(path), library.path_token_of(path));

    let s1 = skills
        .record(path, "create", "concept created", &markdown("skills body"))
        .await
        .unwrap();
    let s2 = skills
        .record(path, "update", "concept updated", &markdown("skills v2"))
        .await
        .unwrap();
    let l1 = library
        .record(path, "create", "concept created", &markdown("library body"))
        .await
        .unwrap();
    assert_eq!(s1, 1);
    assert_eq!(s2, 2);
    assert_eq!(l1, 1, "the library chain numbers independently");

    assert_eq!(skills.get(path, 1).await.unwrap(), markdown("skills body"));
    assert_eq!(
        library.get(path, 1).await.unwrap(),
        markdown("library body")
    );
    assert!(matches!(
        library.get(path, 2).await,
        Err(RevisionsError::NotFound { .. })
    ));

    // The rows are scope-tagged in SQLite: one path token, two scope_ids.
    let scopes: Vec<(String,)> =
        sqlx::query_as("SELECT DISTINCT scope_id FROM concept_revisions ORDER BY scope_id")
            .fetch_all(store.pool())
            .await
            .unwrap();
    assert_eq!(
        scopes,
        vec![
            ("global:library".to_string(),),
            ("global:skills".to_string(),)
        ]
    );

    // Each instance's browser index sees only its own scope.
    assert_eq!(skills.list_paths().await.unwrap().len(), 1);
    assert_eq!(library.list_paths().await.unwrap().len(), 1);
}

#[tokio::test]
async fn lifecycle_config_new_fields_survive_old_rows() {
    let _retention = retention_lock().await;
    let _signal = SignalDefaults;
    // Deliberately NOT the defaults: a sync over the old row would
    // otherwise look like a no-op (the top-k test's reasoning, mirrored).
    set_stale_days(7);
    set_recent_window(1);
    set_consolidation_patches(2);
    let (_dir, store) = store().await;
    let config = ConfigStore::new(store.pool().clone());
    let service_key = ServiceKey::from_bytes(&[7u8; 32]).unwrap();

    // The constants the defaults ride on (the top-k pin's shape).
    assert_eq!(DEFAULT_STALE_DAYS, 90);
    assert_eq!(DEFAULT_RECENT_WINDOW, 14);
    assert_eq!(DEFAULT_CONSOLIDATION_PATCHES, 8);
    assert_eq!(DEFAULT_DREAM_INTERVAL_S, 86_400);

    // Absent row → the defaults, and the sync stamps all three signal
    // statics (an absent row RESETS the globals — it never keeps, the
    // llm-row pattern).
    sync_lifecycle_from_store(&config, &service_key)
        .await
        .unwrap();
    assert_eq!(stale_days(), 90, "absent row resets stale_days");
    assert_eq!(recent_window(), 14, "absent row resets recent_window");
    assert_eq!(
        consolidation_patches(),
        8,
        "absent row resets consolidation_patches"
    );

    // An older build's row — the step 2/4 shape, all six fields. serde's
    // `#[serde(default = ...)]` keeps it valid: the five step-5 fields
    // parse as their defaults, both through a direct read and through
    // the sync.
    config
        .set(
            "lifecycle",
            &config::seal_config(
                &service_key,
                "lifecycle",
                &serde_json::json!({
                    "revision_retention": 200,
                    "query_top_k": 8,
                    "rrf_k": 60,
                    "rrf_weight_lex": 1.0,
                    "rrf_weight_vec": 1.0,
                    "dedup_cosine": 0.92
                }),
            ),
        )
        .await
        .unwrap();
    let parsed = config::get_sealed::<LifecycleConfig>(&config, &service_key, "lifecycle")
        .await
        .unwrap();
    assert_eq!(parsed.stale_days, 90, "old row default stale_days");
    assert_eq!(parsed.recent_window, 14, "old row default recent_window");
    assert_eq!(
        parsed.consolidation_patches, 8,
        "old row default consolidation_patches"
    );
    assert_eq!(
        parsed.dream_interval_s, 86_400,
        "old row default dream_interval_s"
    );
    assert!(parsed.dream_enabled, "old row default dream_enabled: true");
    sync_lifecycle_from_store(&config, &service_key)
        .await
        .unwrap();
    assert_eq!(
        stale_days(),
        90,
        "the sync applies the defaulted stale_days"
    );
    assert_eq!(
        recent_window(),
        14,
        "the sync applies the defaulted recent_window"
    );
    assert_eq!(
        consolidation_patches(),
        8,
        "the sync applies the defaulted consolidation_patches"
    );
}

#[tokio::test]
async fn stale_recent_consolidation_statics_reclamp_and_nan_guard() {
    let _retention = retention_lock().await;
    let _signal = SignalDefaults;
    // u32 fields carry ONLY the range clamp, on set AND get — the f32
    // NaN guard has no u32 analogue (a u32 can never be NaN), so there
    // is no NaN pin for these. (The get-side re-clamp of a value that
    // entered the atomic without a setter is pinned in-module, the
    // fusion statics' raw-bits shape.)
    set_stale_days(0);
    assert_eq!(stale_days(), 7, "stale_days floor (range 7..=3650)");
    set_stale_days(4000);
    assert_eq!(stale_days(), 3650, "stale_days ceiling");
    set_stale_days(90);
    assert_eq!(stale_days(), 90, "in-range stale_days passes through");

    set_recent_window(0);
    assert_eq!(recent_window(), 1, "recent_window floor (range 1..=365)");
    set_recent_window(400);
    assert_eq!(recent_window(), 365, "recent_window ceiling");
    set_recent_window(14);
    assert_eq!(recent_window(), 14, "in-range recent_window passes through");

    set_consolidation_patches(0);
    assert_eq!(consolidation_patches(), 2, "patches floor (range 2..=100)");
    set_consolidation_patches(150);
    assert_eq!(consolidation_patches(), 100, "patches ceiling");
    set_consolidation_patches(8);
    assert_eq!(
        consolidation_patches(),
        8,
        "in-range patches passes through"
    );

    // The load seam too: a hand-edited row's out-of-range values clamp
    // on load (the setters ARE the load-side clamp via the sync).
    let (_dir, store) = store().await;
    let config = ConfigStore::new(store.pool().clone());
    let service_key = ServiceKey::from_bytes(&[7u8; 32]).unwrap();
    let sealed = config::seal_config(
        &service_key,
        "lifecycle",
        &serde_json::json!({
            "revision_retention": 200,
            "query_top_k": 8,
            "rrf_k": 60,
            "rrf_weight_lex": 1.0,
            "rrf_weight_vec": 1.0,
            "dedup_cosine": 0.92,
            "stale_days": 3,
            "recent_window": 999,
            "consolidation_patches": 1000
        }),
    );
    config.set("lifecycle", &sealed).await.unwrap();
    sync_lifecycle_from_store(&config, &service_key)
        .await
        .unwrap();
    assert_eq!(stale_days(), 7, "stored 3 clamps to the floor on load");
    assert_eq!(
        recent_window(),
        365,
        "stored 999 clamps to the ceiling on load"
    );
    assert_eq!(
        consolidation_patches(),
        100,
        "stored 1000 clamps to the ceiling on load"
    );

    // The struct clamp the admin save path applies carries the same
    // ranges — and the dream interval's (the bool has no range: it
    // copies through):
    let wild = LifecycleConfig {
        stale_days: 1,
        recent_window: 999_999,
        consolidation_patches: 0,
        dream_interval_s: 10,
        ..Default::default()
    };
    let clamped = wild.clamp();
    assert_eq!(clamped.stale_days, 7, "struct clamp: stale floor");
    assert_eq!(clamped.recent_window, 365, "struct clamp: window ceiling");
    assert_eq!(
        clamped.consolidation_patches, 2,
        "struct clamp: patches floor"
    );
    assert_eq!(
        clamped.dream_interval_s, 3_600,
        "struct clamp: dream interval floor (1 h)"
    );
    assert!(clamped.dream_enabled, "the bool has no range — it copies");
    let high = LifecycleConfig {
        dream_interval_s: 10_000_000,
        ..Default::default()
    };
    assert_eq!(
        high.clamp().dream_interval_s,
        2_592_000,
        "struct clamp: dream interval ceiling (30 days)"
    );
}

#[tokio::test]
async fn consecutive_update_count_counts_trailing_updates_only() {
    let _retention = retention_lock().await;
    set_revision_retention(DEFAULT_REVISION_RETENTION);
    let _signal = SignalDefaults;
    set_consolidation_patches(DEFAULT_CONSOLIDATION_PATCHES);
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let cs = ConceptStore::for_user(&store, user, master.clone());
    let rs = RevisionStore::for_user(store.pool(), user, &master);
    let path = "/notes/chain.md";
    let token = rs.path_token_of(path);

    // create → update×3 → restore → update×2: the `restore` row breaks
    // the chain, so the count is the two updates AFTER it (trailing
    // consecutive update revisions only — the scan never looks past a
    // non-update op).
    cs.put(&concept(path, "Chain", "v1")).await.unwrap();
    for i in 2..=4 {
        cs.put(&concept(path, "Chain", &format!("v{i}")))
            .await
            .unwrap();
    }
    cs.restore(path, 1).await.unwrap();
    cs.put(&concept(path, "Chain", "v5")).await.unwrap();
    cs.put(&concept(path, "Chain", "v6")).await.unwrap();
    assert_eq!(
        rs.consecutive_update_count(&token).await.unwrap(),
        2,
        "the restore row breaks the chain — only the two trailing updates count"
    );

    // A `consolidate` row resets the count the same way — the reset
    // falls out of the tail break (no counter state exists anywhere):
    // after the dreamed rewrite, the tail is updates-after-consolidate
    // and the count reads only those.
    let cons = ConceptStore::for_user(&store, user, master.clone()).with_revision_op("consolidate");
    cons.put(&concept(path, "Chain", "consolidated body"))
        .await
        .unwrap();
    assert_eq!(
        rs.consecutive_update_count(&token).await.unwrap(),
        0,
        "the consolidate row resets the count to 0"
    );
    cs.put(&concept(path, "Chain", "v7")).await.unwrap();
    cs.put(&concept(path, "Chain", "v8")).await.unwrap();
    assert_eq!(
        rs.consecutive_update_count(&token).await.unwrap(),
        2,
        "updates after the consolidate count from zero again"
    );

    // Boundary (Review Focus 5): a fresh chain — one create, then
    // consolidation_patches()+1 updates — counts exactly patches+1, the
    // value the signal layer's strict `>` comparison fires on
    // (count == consolidation_patches() → no signal; patches+1 →
    // signal). The scan window is patches+1 rows, so the count cannot
    // exceed it.
    let patches = consolidation_patches();
    assert_eq!(patches, 8);
    let boundary = "/notes/boundary.md";
    cs.put(&concept(boundary, "Boundary", "v1")).await.unwrap();
    for i in 0..(patches as usize + 1) {
        cs.put(&concept(boundary, "Boundary", &format!("v{}", i + 2)))
            .await
            .unwrap();
    }
    assert_eq!(
        rs.consecutive_update_count(&rs.path_token_of(boundary))
            .await
            .unwrap(),
        patches + 1,
        "patches+1 trailing updates read exactly patches+1"
    );
}

#[tokio::test]
async fn consecutive_update_count_empty_chain_is_zero() {
    let _retention = retention_lock().await;
    set_revision_retention(DEFAULT_REVISION_RETENTION);
    let _signal = SignalDefaults;
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let rs = RevisionStore::for_user(store.pool(), user, &master);

    // No rows at all (never-written path): the signal layer's baseline.
    let phantom = rs.path_token_of("/notes/never-written.md");
    assert_eq!(rs.consecutive_update_count(&phantom).await.unwrap(), 0);

    // A lone non-update row (create only) also reads zero — the tail
    // break lands on the first scanned row.
    let path = "/notes/only-create.md";
    let cs = ConceptStore::for_user(&store, user, master.clone());
    cs.put(&concept(path, "Only", "created once"))
        .await
        .unwrap();
    assert_eq!(
        rs.consecutive_update_count(&rs.path_token_of(path))
            .await
            .unwrap(),
        0,
        "a lone create breaks on the first scanned row"
    );
}
