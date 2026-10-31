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
    DEFAULT_REVISION_RETENTION, LifecycleConfig, RevisionStore, RevisionsError, op_summary,
    revision_retention, set_revision_retention, sync_lifecycle_from_store,
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

    // Through the real ConceptStore boundary, so scope_files carries the
    // `live` side of the index. (ConceptStore does not record revisions
    // itself yet — that wiring is Task 2; the test records explicitly.)
    cs.put(&concept(alive, "Alive", "kept body zebra"))
        .await
        .unwrap();
    rs.record(alive, "create", "concept created", &markdown("v1 alive"))
        .await
        .unwrap();

    cs.put(&concept(dead, "Dead", "doomed body")).await.unwrap();
    rs.record(dead, "create", "concept created", &markdown("v1 dead"))
        .await
        .unwrap();
    rs.record_as(
        dead,
        "update",
        "web:dana",
        "concept updated",
        &markdown("v2 dead"),
    )
    .await
    .unwrap();
    // Then the concept is deleted: the registry row (and file + index
    // rows) vanish, but the revision chain must stay listed.
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
    assert_eq!(dead_row.last_op, "update");
    assert!(!alive_row.last_op_at.is_empty());
    assert!(!dead_row.last_op_at.is_empty());
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
    set_revision_retention(DEFAULT_REVISION_RETENTION);
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
        "absent lifecycle row keeps the default"
    );

    // A persisted value of 9 clamps ON LOAD: the global can never leave
    // 25..=2000, whatever an older or hand-edited row contains (clamp on
    // load mirrors the clamp on save).
    let sealed = config::seal_config(
        &service_key,
        "lifecycle",
        &LifecycleConfig {
            revision_retention: 9,
        },
    );
    config.set("lifecycle", &sealed).await.unwrap();
    sync_lifecycle_from_store(&config, &service_key)
        .await
        .unwrap();
    assert_eq!(revision_retention(), 25, "stored value clamps on load");

    set_revision_retention(DEFAULT_REVISION_RETENTION);
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
