//! VectorStore integration tests (memory-lifecycle spec §3.3): AEAD f32
//! row round-trips with the exact `vec:<scope>:<token>` AAD, scope
//! isolation, the self-healing backlog enumeration, the put/delete
//! write hooks, the decoded cache's active-set filter, and the bounded
//! dedup write queue. Real `Store::open` over a temp dir — the tables
//! and hooks behave exactly as production will see them.

use mycelium_core::concept::{Concept, Frontmatter};
use mycelium_crypto::aead_open;
use mycelium_crypto::keys::{MasterKey, ServiceKey, generate_master_key};
use mycelium_crypto::store_keys::IndexKeys;
use mycelium_store::vectors::{
    VECTOR_QUEUE_CAPACITY, VectorError, VectorStore, drain_queue, enqueue_write, invalidate_scope,
    vector_queue_depth,
};
use mycelium_store::{ConceptStore, Store};
use uuid::Uuid;

/// Every test in this binary takes this lock: the write queue and the
/// decoded cache are process-global statics, and the default harness
/// runs one binary's tests in parallel (the revisions suite's
/// RETENTION_LOCK idiom). Queue-asserting tests also DRAIN at entry, so
/// any residue a sibling test enqueued cannot reach their assertions.
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn lock() -> tokio::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().await
}

async fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    (dir, store)
}

/// A user scope's (master key, ConceptStore, VectorStore) over one temp
/// store — the same master key derives both stores' IndexKeys, so the
/// tests can hand-open sealed rows with the exact production AAD.
struct Fixtures<'a> {
    _user: Uuid,
    master: MasterKey,
    cs: ConceptStore<'a>,
    vs: VectorStore<'a>,
}

fn fixtures(store: &Store) -> Fixtures<'_> {
    let master = generate_master_key();
    // ONE user id for both stores — identical scope identity, which the
    // hook tests lean on (the enqueue carries the ConceptStore's scope).
    let user = Uuid::new_v4();
    let cs = ConceptStore::for_user(store, user, master.clone());
    let vs = VectorStore::for_user(store.pool(), user, &master);
    Fixtures {
        _user: user,
        master,
        cs,
        vs,
    }
}

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

/// A concept with a lifecycle status in its frontmatter.
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

/// An 8-dim vector (the backlog test's learned dims).
fn d8(v: f32) -> Vec<f32> {
    (0..8).map(|i| v + i as f32).collect()
}

#[tokio::test]
async fn upsert_roundtrips_and_replaces() {
    let _guard = lock().await;
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    let (_dir, store) = store().await;
    let f = fixtures(&store);
    let keys = IndexKeys::from_master_key(&f.master);
    let aad = format!("vec:{}:{}", f.vs.scope_id(), keys.token("/a.md"));

    // dims must equal the vector length exactly, and a mismatch writes
    // nothing.
    assert!(
        matches!(
            f.vs.upsert("/a.md", "m-a", 4, &[1.0f32, 2.0, 3.0, 4.0, 5.0])
                .await,
            Err(VectorError::DimMismatch)
        ),
        "a 5-float vector declared 4-dim is DimMismatch"
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM search_vectors WHERE scope_id = ?")
        .bind(f.vs.scope_id())
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(rows, 0, "the failed upsert wrote nothing");

    let first = [0.5f32, -1.25, 2.75e-3, 1.0e8, -3.0e-8];
    let up1 = f.vs.upsert("/a.md", "m-a", 5, &first).await.unwrap();
    assert!(up1.created, "the first write of a path is an INSERT");
    // The at-rest bytes are the LE f32 chunks under the exact AAD; a
    // different AAD must not open them (purpose + scope + row bound).
    let sealed: Vec<u8> =
        sqlx::query_scalar("SELECT vec FROM search_vectors WHERE scope_id = ? AND path_token = ?")
            .bind(f.vs.scope_id())
            .bind(keys.token("/a.md"))
            .fetch_one(store.pool())
            .await
            .unwrap();
    let plain = aead_open(&sealed, aad.as_bytes(), keys.index_dek()).unwrap();
    assert_eq!(
        plain,
        first
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect::<Vec<u8>>(),
        "storage format is little-endian f32, AAD `vec:<scope>:<token>`"
    );
    assert_ne!(sealed, plain, "at-rest bytes are sealed, never plaintext");
    assert!(
        aead_open(&sealed, b"vec:wrong:scope", keys.index_dek()).is_err(),
        "a wrong AAD must not open the row"
    );

    // A second upsert REPLACES (latest vec wins, one row per path).
    let second = [9.0f32, 8.0, 7.0, 6.0, 5.0];
    let up2 = f.vs.upsert("/a.md", "m-a", 5, &second).await.unwrap();
    assert!(!up2.created, "an existing row is an UPDATE");
    let rows = f.vs.load_rows().await.unwrap();
    assert_eq!(rows.len(), 1, "the PK keeps one row per (scope, path)");
    assert_eq!(
        rows[0].vec_f32,
        second.to_vec(),
        "non-trivial f32 roundtrip"
    );
    assert_eq!(rows[0].model, "m-a");
    assert_eq!(rows[0].dims, 5);
    assert!(!rows[0].stale);
    assert!(!rows[0].updated_at.is_empty());

    // upsert-then-mark: the marker sets, the next completed upsert
    // CLEARS it (that is how a rebuild finishes).
    let marked = f.vs.mark_scope_stale().await.unwrap();
    assert_eq!(marked, 1);
    assert!(
        f.vs.load_rows().await.unwrap()[0].stale,
        "mark_scope_stale sets stale=1"
    );
    let up3 = f.vs.upsert("/a.md", "m-a", 5, &second).await.unwrap();
    assert!(!up3.created);
    assert!(
        !f.vs.load_rows().await.unwrap()[0].stale,
        "a completed upsert clears the stale flag"
    );
}

#[tokio::test]
async fn rows_are_scoped_and_invisible_cross_scope() {
    let _guard = lock().await;
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    let (_dir, store) = store().await;
    let master = generate_master_key();
    let service = ServiceKey::from_bytes(&[9u8; 32]).unwrap();
    let user_store = VectorStore::for_user(store.pool(), Uuid::new_v4(), &master);
    let service_store = VectorStore::for_service(store.pool(), &service, "skills");
    let user_vec = [1.0f32, 2.0, 3.0];
    let service_vec = [7.0f32, 8.0, 9.0];
    user_store
        .upsert("/shared.md", "m", 3, &user_vec)
        .await
        .unwrap();
    service_store
        .upsert("/shared.md", "m", 3, &service_vec)
        .await
        .unwrap();

    let seen_user = user_store.load_rows().await.unwrap();
    assert_eq!(seen_user.len(), 1, "the user scope sees its own row");
    assert_eq!(seen_user[0].vec_f32, user_vec.to_vec());
    assert_eq!(
        seen_user[0].path_token,
        user_store.path_token_of("/shared.md")
    );
    let seen_service = service_store.load_rows().await.unwrap();
    assert_eq!(seen_service.len(), 1, "the service scope sees its own row");
    assert_eq!(seen_service[0].vec_f32, service_vec.to_vec());
    assert_ne!(
        user_store.scope_id(),
        service_store.scope_id(),
        "user:<uuid> vs global:<namespace>"
    );
}

#[tokio::test]
async fn backlog_finds_missing_stale_and_model_changed_rows() {
    let _guard = lock().await;
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    let (_dir, store) = store().await;
    let f = fixtures(&store);
    // Search rows come from real puts (the enumeration keys on them).
    for path in ["/a.md", "/b.md", "/c.md"] {
        f.cs.put(&concept(path, "t", "fact body")).await.unwrap();
    }
    f.vs.upsert("/a.md", "m-a", 8, &d8(1.0)).await.unwrap();

    // Under a different model: A's row is stale-by-model, B/C missing.
    assert_eq!(
        f.vs.backlog_paths("m-b", 8).await.unwrap(),
        vec!["/a.md", "/b.md", "/c.md"],
        "model change or missing row = work"
    );
    // A fresh under its own model: only the unembedded paths remain.
    assert_eq!(
        f.vs.backlog_paths("m-a", 8).await.unwrap(),
        vec!["/b.md", "/c.md"]
    );
    // learned_dims == 0 ignores the dims condition entirely.
    assert_eq!(
        f.vs.backlog_paths("m-a", 0).await.unwrap(),
        vec!["/b.md", "/c.md"]
    );
    // A different learned dims puts A's row of the old dimensionality
    // back in the backlog.
    assert_eq!(
        f.vs.backlog_paths("m-a", 16).await.unwrap(),
        vec!["/a.md", "/b.md", "/c.md"]
    );
    // The stale marker puts A back even under its own fresh model.
    f.vs.mark_scope_stale().await.unwrap();
    assert_eq!(
        f.vs.backlog_paths("m-a", 8).await.unwrap(),
        vec!["/a.md", "/b.md", "/c.md"]
    );
    // Real puts enqueued embed work — leave no residue for the queue-
    // asserting siblings (the lock makes the order irrelevant).
    assert_eq!(drain_queue(VECTOR_QUEUE_CAPACITY).len(), 3);
}

#[tokio::test]
async fn deleted_concepts_loose_their_vector_rows() {
    let _guard = lock().await;
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    let (_dir, store) = store().await;
    let f = fixtures(&store);
    let token = f.vs.path_token_of("/a.md");
    let scope = f.vs.scope_id().to_string();

    f.cs.put(&concept("/a.md", "A", "alpha")).await.unwrap();
    // The put hook enqueued the write — exactly one item, the pair
    // (the start-drain guarantees nothing else is in the queue).
    assert_eq!(vector_queue_depth(), 1, "the put hook enqueued the write");
    assert_eq!(
        drain_queue(VECTOR_QUEUE_CAPACITY),
        vec![(scope.clone(), token.clone())]
    );

    f.vs.upsert("/a.md", "m-a", 3, &[1.0f32, 0.0, 0.0])
        .await
        .unwrap();
    let built = f.vs.cache_scope().await.unwrap();
    assert_eq!(built.rows.len(), 1);
    assert_eq!(built.rows[0].path_token, token, "the active row is cached");

    f.cs.delete("/a.md").await.unwrap();
    // Registry + re-delete behavior is untouched by the hook: the entry
    // is gone from the listing, the revision row exists (the delete
    // recorded before the effects), and a re-delete is still NotFound.
    assert!(
        f.cs.list().await.unwrap().is_empty(),
        "registry row gone as before"
    );
    assert!(
        matches!(
            f.cs.delete("/a.md").await,
            Err(mycelium_store::ConceptStoreError::NotFound(_))
        ),
        "re-delete still NotFound"
    );
    assert_eq!(
        vector_queue_depth(),
        0,
        "delete must NOT enqueue (nothing to embed)"
    );
    assert!(
        f.vs.load_rows().await.unwrap().is_empty(),
        "row removed directly"
    );
    // remove_row is IDEMPOTENT (review pin): the delete hook runs it on
    // an already-gone row and Ok is the correct answer (no NotFound arm
    // to handle).
    f.vs.remove_row("/a.md").await.unwrap();
    // The delete also dropped the decoded cache: a fresh build has no
    // entry for the token (a hook failure would return the pre-delete
    // snapshot above unchanged).
    let built = f.vs.cache_scope().await.unwrap();
    assert!(built.rows.is_empty(), "never resurrect a deleted row");
}

#[tokio::test]
async fn decoded_cache_excludes_non_active_concepts() {
    let _guard = lock().await;
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    let (_dir, store) = store().await;
    let f = fixtures(&store);

    f.cs.put(&concept("/a.md", "A", "alpha")).await.unwrap();
    let _queued = drain_queue(VECTOR_QUEUE_CAPACITY);
    f.cs.put(&concept_with_status(
        "/b.md",
        "B",
        "beta",
        Some("superseded"),
    ))
    .await
    .unwrap();
    let _queued = drain_queue(VECTOR_QUEUE_CAPACITY);
    f.vs.upsert("/a.md", "m-x", 2, &[1.0f32, 0.0])
        .await
        .unwrap();
    f.vs.upsert("/b.md", "m-x", 2, &[0.0f32, 1.0])
        .await
        .unwrap();

    let a_token = f.vs.path_token_of("/a.md");
    let b_token = f.vs.path_token_of("/b.md");
    let built = f.vs.cache_scope().await.unwrap();
    assert_eq!(built.rows.len(), 1, "only the active concept is cached");
    assert!(built.rows.iter().any(|r| r.path_token == a_token));
    assert!(
        built.rows.iter().all(|r| r.path_token != b_token),
        "a superseded concept's vector row never re-enters the cache"
    );
    // top_cosine shows the same posture via the plaintext registry: b's
    // row is already cache-excluded (and its cosine with the query
    // would be 0.0 anyway — the >0 rule cuts it too).
    let hits = f.vs.top_cosine(&[1.0f32, 0.0], "m-x", 10).await.unwrap();
    assert_eq!(hits, vec![("/a.md".to_string(), 1.0f32)]);

    // Invalidate-on-write: the third concept's put drops the cache, so
    // the next build grows to include it (a failed hook would return
    // the one-row snapshot above unchanged).
    f.cs.put(&concept("/c.md", "C", "gamma")).await.unwrap();
    let _queued = drain_queue(VECTOR_QUEUE_CAPACITY);
    f.vs.upsert("/c.md", "m-x", 2, &[1.0f32, 1.0])
        .await
        .unwrap();
    let rebuilt = f.vs.cache_scope().await.unwrap();
    assert_eq!(rebuilt.rows.len(), 2, "the put hook invalidated the scope");
    assert!(
        rebuilt
            .rows
            .iter()
            .any(|r| r.path_token == f.vs.path_token_of("/c.md"))
    );

    // Ranked: cosine desc (a 1.0 > c 1/sqrt2), mapped to registry paths.
    let hits = f.vs.top_cosine(&[1.0f32, 0.0], "m-x", 10).await.unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].0, "/a.md");
    assert_eq!(hits[0].1, 1.0);
    assert_eq!(hits[1].0, "/c.md");
    assert!(
        (hits[1].1 - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6,
        "cosine is the standard dot/(|a||b|): {}",
        hits[1].1
    );
    // top truncation: only the best row survives (the f32 sqrt of
    // 2 is one ulp off exact — assert with an epsilon).
    let hits = f.vs.top_cosine(&[1.0f32, 1.0], "m-x", 1).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, "/c.md");
    assert!((hits[0].1 - 1.0).abs() < 1e-6);

    // A row whose REGISTRY path vanished (search-side kill) is a dead
    // row: it may still be cached, but mapping to nothing drops it from
    // the RESULTS — never a result that cannot be read.
    sqlx::query("DELETE FROM scope_files WHERE scope = ? AND path = ?")
        .bind(f.vs.scope_id())
        .bind("/c.md")
        .execute(store.pool())
        .await
        .unwrap();
    let cached = f.vs.cache_scope().await.unwrap();
    assert_eq!(cached.rows.len(), 2, "the cached snapshot still holds both");
    let hits = f.vs.top_cosine(&[1.0f32, 1.0], "m-x", 10).await.unwrap();
    assert_eq!(hits.len(), 1, "dead row dropped");
    assert_eq!(hits[0].0, "/a.md");
    assert!(
        (hits[0].1 - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6,
        "a's cosine under this query is 1/sqrt2: {}",
        hits[0].1
    );

    // (review ruling) Dead rows drop BEFORE the truncate: query the
    // axis where the (dead) c row ranks clearly above the live a row
    // (0.9/0.9055 vs 0.1/0.9055) and take top=1 — the dead row still
    // gives up its slot, so a LIVE entry fills the top-1 (take-then-map
    // arithmetic would return an empty list here).
    let hits = f.vs.top_cosine(&[0.1f32, 0.9], "m-x", 1).await.unwrap();
    assert_eq!(hits.len(), 1, "the dead row does not eat a live slot");
    assert_eq!(hits[0].0, "/a.md");

    // The free invalidate_scope the hooks also call drops the decoded
    // cache without any put: hand-remove a.md's SEARCH row after the
    // cache holds it — only a post-invalidate rebuild (a fresh
    // active-set fetch) can reflect the removal.
    sqlx::query("DELETE FROM search_docs WHERE scope = ? AND concept_path = ?")
        .bind(f.vs.scope_id())
        .bind("/a.md")
        .execute(store.pool())
        .await
        .unwrap();
    invalidate_scope(f.vs.scope_id());
    let rehit = f.vs.cache_scope().await.unwrap();
    assert!(
        rehit.rows.iter().all(|r| r.path_token != a_token),
        "the post-invalidate rebuild reflects the removed active row"
    );
    assert!(
        rehit
            .rows
            .iter()
            .any(|r| r.path_token == f.vs.path_token_of("/c.md")),
        "c.md is still an active search row, so it re-enters the cache"
    );

    // (review pin 3) The stale skip applies at the cache level too:
    // stale-mark every row and the rebuilt snapshot keeps none of them.
    let stale_count = f.vs.mark_scope_stale().await.unwrap();
    assert_eq!(
        stale_count, 3,
        "all three rows (a, b, c) were living and now stale"
    );
    invalidate_scope(f.vs.scope_id());
    let stale_view = f.vs.cache_scope().await.unwrap();
    assert!(
        stale_view.rows.is_empty(),
        "stale rows never re-enter the cache"
    );
}

#[tokio::test]
async fn queue_is_bounded_and_deduped() {
    let _guard = lock().await;
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);

    // Exact duplicates collapse to ONE entry (skipped anywhere in the
    // queue, not just at the tail).
    for _ in 0..600 {
        enqueue_write("s", "t");
    }
    assert_eq!(vector_queue_depth(), 1, "600 repeated pushes = one entry");
    enqueue_write("s", "x");
    enqueue_write("s", "y");
    enqueue_write("s", "t");
    assert_eq!(
        vector_queue_depth(),
        3,
        "a repeat anywhere is still skipped"
    );

    // The cap: 600 DIFFERENT tokens push until 512; the overflow drops
    // the NEW items (back-drop) — 3 + 509 reach the cap.
    for i in 0..600 {
        enqueue_write("s", &format!("tok-{i}"));
    }
    assert_eq!(vector_queue_depth(), VECTOR_QUEUE_CAPACITY);

    // FIFO: the oldest entries first, the dropped era never returns.
    let drained = drain_queue(VECTOR_QUEUE_CAPACITY);
    assert_eq!(drained.len(), VECTOR_QUEUE_CAPACITY);
    assert_eq!(drained[0], ("s".to_string(), "t".to_string()));
    assert_eq!(drained[1], ("s".to_string(), "x".to_string()));
    assert_eq!(drained[2], ("s".to_string(), "y".to_string()));
    assert_eq!(drained[3], ("s".to_string(), "tok-0".to_string()));
    assert_eq!(
        vector_queue_depth(),
        0,
        "a full-capacity drain empties the queue"
    );

    // Re-enqueue of a drained token works (no one-way dedup memory).
    enqueue_write("s", "t");
    assert_eq!(vector_queue_depth(), 1);
    // Partial drain is FIFO and bounded.
    enqueue_write("s", "p1");
    enqueue_write("s", "p2");
    enqueue_write("s", "p3");
    assert_eq!(
        drain_queue(2),
        vec![
            ("s".to_string(), "t".to_string()),
            ("s".to_string(), "p1".to_string())
        ]
    );
    assert_eq!(drain_queue(VECTOR_QUEUE_CAPACITY).len(), 2);
    assert_eq!(vector_queue_depth(), 0);
}

#[tokio::test]
async fn reindex_never_enqueues_but_restore_re_enqueues() {
    let _guard = lock().await;
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    let (_dir, store) = store().await;
    let f = fixtures(&store);
    let token = f.vs.path_token_of("/a.md");
    let scope = f.vs.scope_id().to_string();

    f.cs.put(&concept("/a.md", "A", "alpha")).await.unwrap();
    assert_eq!(
        drain_queue(VECTOR_QUEUE_CAPACITY),
        vec![(scope.clone(), token.clone())]
    );

    // reindex is index-only: re-scored docs, no new vector work.
    f.cs.reindex("/a.md").await.unwrap();
    assert_eq!(vector_queue_depth(), 0, "reindex must not enqueue");
    // The concept is still indexable afterward (pre-existing behavior).
    f.cs.delete("/a.md").await.unwrap();
    assert_eq!(vector_queue_depth(), 0, "delete must not enqueue");

    // Restore routes through the put path, so the hook fires
    // automatically — the concept comes back AND the embed work is
    // queued again (revision 1 = the create snapshot).
    f.cs.restore("/a.md", 1).await.unwrap();
    assert!(
        f.cs.list().await.unwrap().iter().any(|e| e.path == "/a.md"),
        "restore re-creates the concept (pre-existing behavior holds)"
    );
    assert_eq!(
        vector_queue_depth(),
        1,
        "restore's put enqueued via the put path"
    );
    assert_eq!(drain_queue(VECTOR_QUEUE_CAPACITY), vec![(scope, token)]);
}

#[tokio::test]
async fn corrupt_row_is_loud_in_load_rows_but_skipped_in_the_cache() {
    let _guard = lock().await;
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    let (_dir, store) = store().await;
    let f = fixtures(&store);

    // An ACTIVE concept with a vector row whose blob is not a valid
    // envelope (tampered at rest).
    f.cs.put(&concept("/a.md", "A", "alpha")).await.unwrap();
    let _queued = drain_queue(VECTOR_QUEUE_CAPACITY);
    f.vs.upsert("/a.md", "m-a", 3, &[1.0f32, 0.0, 0.0])
        .await
        .unwrap();
    sqlx::query("UPDATE search_vectors SET vec = X'0000'")
        .execute(store.pool())
        .await
        .unwrap();

    let err = f.vs.load_rows().await.unwrap_err();
    assert!(
        matches!(err, VectorError::Crypto(_)),
        "load_rows errors loudly: {err:?}"
    );
    // The QUERY path's cache instead skips the row — queries stay
    // available over a corrupted row (work is re-embedable, and the
    // strict path already told the operator).
    let rows = f.vs.cache_scope().await.unwrap();
    assert!(
        rows.rows.is_empty(),
        "the corrupt row is dropped, not fatal"
    );
}

/// The build/insert race (review R1, IMPORTANT): an invalidation landing
/// while a build is fetching must never survive as the installed
/// snapshot. A build that fetched before the change either re-checks its
/// epoch at the install (same critical section as the removal contends
/// on) and rebuilds, or its stale install is chased out by the
/// invalidation's mutex-ordered entry removal — the post-race entry is
/// clean under every interleaving. Both orders pinned: observe-after-
/// invalidate (the trailing hand-invalidate) and the hostile in-flight
/// race (the flock).
#[tokio::test(flavor = "multi_thread")]
async fn cache_epoch_recheck_never_serves_the_racing_view() {
    let _guard = lock().await;
    let _cleared = drain_queue(VECTOR_QUEUE_CAPACITY);
    let (_dir, store) = store().await;
    let f = fixtures(&store);

    // Seed: two ACTIVE concepts with rows — the pre-change view holds
    // both, so the superseded row below is genuinely the bait.
    f.cs.put(&concept("/a.md", "A", "alpha")).await.unwrap();
    let _queued = drain_queue(VECTOR_QUEUE_CAPACITY);
    f.cs.put(&concept("/c.md", "C", "gamma")).await.unwrap();
    let _queued = drain_queue(VECTOR_QUEUE_CAPACITY);
    f.vs.upsert("/a.md", "m-x", 2, &[1.0f32, 0.0])
        .await
        .unwrap();
    f.vs.upsert("/c.md", "m-x", 2, &[0.0f32, 1.0])
        .await
        .unwrap();
    let a_token = f.vs.path_token_of("/a.md");
    let c_token = f.vs.path_token_of("/c.md");

    let prechange = f.vs.cache_scope().await.unwrap();
    assert!(
        prechange.rows.iter().any(|r| r.path_token == a_token),
        "the pre-change view holds a"
    );
    assert!(
        prechange.rows.iter().any(|r| r.path_token == c_token),
        "the pre-change view holds c"
    );

    // Drop the entry so the racing flock's builds all fetch fresh.
    invalidate_scope(f.vs.scope_id());

    // The hostile flock: 8 concurrent builds (all miss, all fetch) plus
    // one superseder whose put rewrites c's status and invalidates
    // while they are mid-flight. Pool contention guarantees real
    // interleaving on separate threads. A racing build may legitimately
    // RETURN a pre-change view only if its install preceded the
    // supersede — the pinned invariant is the entry at the post-join
    // read, not each racing future's immediate result.
    let mut builds = Vec::new();
    for _ in 0..8 {
        let store = store.clone();
        let user = f._user;
        let master = f.master.clone();
        builds.push(tokio::spawn(async move {
            let vs = VectorStore::for_user(store.pool(), user, &master);
            vs.cache_scope().await
        }));
    }
    let superseder = {
        let store = store.clone();
        let user = f._user;
        let master = f.master.clone();
        tokio::spawn(async move {
            let cs = ConceptStore::for_user(&store, user, master);
            cs.put(&concept_with_status(
                "/c.md",
                "C",
                "gamma",
                Some("superseded"),
            ))
            .await
        })
    };
    for build in builds {
        build.await.unwrap().unwrap();
    }
    superseder.await.unwrap().unwrap();

    // THE INVARIANT: no racing build may have left the pre-change view
    // installed — the superseded concept's row is not served.
    let post = f.vs.cache_scope().await.unwrap();
    assert!(
        post.rows.iter().any(|r| r.path_token == a_token),
        "a.md survives the race"
    );
    assert!(
        post.rows.iter().all(|r| r.path_token != c_token),
        "no pre-change view survived the supersede"
    );
    // And the read boundary holds too (defense in depth):
    let hits = f.vs.top_cosine(&[1.0f32, 0.0], "m-x", 10).await.unwrap();
    assert!(
        hits.iter().all(|(path, _)| path != "/c.md"),
        "the superseded concept never ranks"
    );

    // Order B, explicitly: invalidate-then-observe — the hand-invalidate
    // drops the entry and the rebuild sees the post-change state only.
    invalidate_scope(f.vs.scope_id());
    let again = f.vs.cache_scope().await.unwrap();
    assert!(
        again.rows.iter().all(|r| r.path_token != c_token),
        "the post-invalidate rebuild excludes the superseded row"
    );
    assert!(
        again.rows.iter().any(|r| r.path_token == a_token),
        "the post-invalidate rebuild keeps the active row"
    );

    // The supersede enqueued its embed work (the put hook); the flock's
    // builds enqueue nothing, so the depth is exactly c's pair.
    assert_eq!(vector_queue_depth(), 1);
    assert_eq!(
        drain_queue(VECTOR_QUEUE_CAPACITY),
        vec![(f.vs.scope_id().to_string(), c_token)]
    );
}
