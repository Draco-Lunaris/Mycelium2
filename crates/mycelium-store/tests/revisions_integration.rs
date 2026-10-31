//! ConceptStore write-ahead revision hooks + `ConceptStore::restore`
//! integration tests (memory-lifecycle plan Task 2, spec §3.2 + §3.2a).
//!
//! The contract under test: every concept mutation through the
//! ConceptStore boundary records its revision row BEFORE the effect
//! lands — create/update derived mechanically from the registry,
//! `delete` with the pre-state bytes, `/mutation-queue/` staging bypass,
//! explicit op tags for system rewrites (`with_revision_op`), stamped
//! provenance mirrored into the row — and `restore(path, n)` applies
//! snapshot N verbatim through the normal stamped put path as a NEW
//! `restore` revision (undeleting when the concept is gone).
//!
//! Real `Store::open` over a temp dir; the pre-state / created-markdown
//! fidelity pins read the raw FileRepo bytes directly.

use mycelium_core::concept::{Concept, Frontmatter};
use mycelium_crypto::keys::{ServiceKey, generate_master_key};
use mycelium_store::revisions::{RevisionStore, RevisionsError};
use mycelium_store::{ConceptStore, ConceptStoreError, FileRepo, Scope, Store};
use uuid::Uuid;

async fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    (dir, store)
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

/// How many revision rows this scope+path chain currently holds.
async fn revision_count(store: &Store, rs: &RevisionStore<'_>, scope_id: &str, path: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM concept_revisions WHERE scope_id = ? AND path_token = ?",
    )
    .bind(scope_id)
    .bind(rs.path_token_of(path))
    .fetch_one(store.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn put_then_reput_writes_create_then_update_rows() {
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let cs = ConceptStore::for_user(&store, user, master.clone());
    let rs = RevisionStore::for_user(store.pool(), user, &master);
    let repo = FileRepo::new(store.user_dir(user));
    let scope = Scope::User(master.clone());
    let path = "/notes/hook.md";

    // First put: the hook records the CREATE row with the created
    // markdown — byte-identical to what landed on disk.
    cs.put(&concept(path, "V1", "first era body"))
        .await
        .unwrap();
    let created = repo.read(path, &scope).await.unwrap();
    let rows = rs.list(path).await.unwrap();
    assert_eq!(rows.len(), 1, "one row after the first put");
    assert_eq!(rows[0].revision_no, 1);
    assert_eq!(rows[0].op, "create");
    assert_eq!(rows[0].summary, "concept created");
    assert_eq!(
        rs.get(path, 1).await.unwrap(),
        created,
        "create payload == the created markdown (byte fidelity)"
    );

    // Capture the pre-state BEFORE the re-put.
    let pre_state = repo.read(path, &scope).await.unwrap();
    assert_eq!(pre_state, created, "v2's pre-state IS v1's committed bytes");
    cs.put(&concept(path, "V2", "second era body yak"))
        .await
        .unwrap();

    let rows = rs.list(path).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].revision_no, 2);
    assert_eq!(rows[0].op, "update");
    assert_eq!(rows[0].summary, "concept updated");
    assert_eq!(
        rs.get(path, 2).await.unwrap(),
        pre_state,
        "update payload == the RAW pre-put bytes (no re-serialization)"
    );
    // The create row is untouched by the re-put; the live content is v2.
    assert_eq!(rs.get(path, 1).await.unwrap(), created);
    assert_eq!(
        cs.get(path).await.unwrap().frontmatter.title.as_deref(),
        Some("V2")
    );
}

#[tokio::test]
async fn delete_writes_delete_row_with_pre_state() {
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let cs = ConceptStore::for_user(&store, user, master.clone());
    let rs = RevisionStore::for_user(store.pool(), user, &master);
    let repo = FileRepo::new(store.user_dir(user));
    let scope = Scope::User(master.clone());
    let path = "/notes/doomed.md";

    cs.put(&concept(path, "Doomed", "doomed body"))
        .await
        .unwrap();
    let pre_state = repo.read(path, &scope).await.unwrap();

    cs.delete(path).await.unwrap();

    let rows = rs.list(path).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].revision_no, 2);
    assert_eq!(rows[0].op, "delete");
    assert_eq!(rows[0].summary, "concept deleted");
    assert_eq!(
        rs.get(path, 2).await.unwrap(),
        pre_state,
        "delete row payload == the raw pre-delete state"
    );
    // The delete row itself keeps nothing on disk: the concept is gone.
    assert!(cs.get(path).await.is_err());
    assert!(cs.list().await.unwrap().is_empty());

    // Re-delete contract: deleting a MISSING path → NotFound, with NO new
    // revision row and NO scope_files / registry side changes.
    let rows_before = rows.clone();
    let listing_before = cs.list().await.unwrap();
    let err = cs.delete(path).await.unwrap_err();
    assert!(
        matches!(err, ConceptStoreError::NotFound(ref p) if p == path),
        "expected NotFound, got {err}"
    );
    assert_eq!(
        rs.list(path).await.unwrap(),
        rows_before,
        "no new revision row"
    );
    assert_eq!(
        cs.list().await.unwrap(),
        listing_before,
        "no scope_files change"
    );
}

#[tokio::test]
async fn restore_to_revision_updates_content_and_commits_restore_row() {
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let cs = ConceptStore::for_user(&store, user, master.clone());
    let rs = RevisionStore::for_user(store.pool(), user, &master);
    let repo = FileRepo::new(store.user_dir(user));
    let scope = Scope::User(master.clone());
    let path = "/notes/restore.md";

    // Author-declared source: the unlabeled restore must KEEP it.
    let mut v1 = concept(path, "V1", "first era body");
    v1.frontmatter.source = Some("web:form".into());
    cs.put(&v1).await.unwrap(); // rev 1 create
    cs.put(&concept(path, "V2", "second era body yak"))
        .await
        .unwrap(); // rev 2 update
    let v2_disk = repo.read(path, &scope).await.unwrap();

    cs.restore(path, 1).await.unwrap();

    // Live content == v1 frontmatter+body modulo re-stamps.
    let got = cs.get(path).await.unwrap();
    assert_eq!(got.frontmatter.title.as_deref(), Some("V1"));
    assert_eq!(got.body.trim(), "first era body");
    assert_eq!(
        got.frontmatter.source.as_deref(),
        Some("web:form"),
        "unlabeled store keeps the author/snapshot source"
    );
    let confirmed = got
        .frontmatter
        .confirmed_at
        .as_deref()
        .expect("confirmed_at re-stamped unconditionally");
    let t = chrono::DateTime::parse_from_rfc3339(confirmed).unwrap();
    assert!(
        t.timestamp() >= chrono::Utc::now().timestamp() - 300,
        "confirmed_at is now-ish, got {confirmed}"
    );

    // The restore committed as a NEW revision: op restore, summary
    // "restored to revision 1", payload == the pre-restore v2 bytes.
    let rows = rs.list(path).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].revision_no, 3);
    assert_eq!(rows[0].op, "restore");
    assert_eq!(rows[0].summary, "restored to revision 1");
    assert_eq!(
        rs.get(path, 3).await.unwrap(),
        v2_disk,
        "restore row payload == the pre-restore raw markdown"
    );

    // Labeled store: a web:alice restore re-stamps source per its label
    // (overriding the snapshot's own), on disk AND in the new row.
    let cs_alice =
        ConceptStore::for_user(&store, user, master.clone()).with_provenance("web:alice");
    cs_alice.restore(path, 1).await.unwrap();
    let got = cs_alice.get(path).await.unwrap();
    assert_eq!(got.frontmatter.title.as_deref(), Some("V1"));
    assert_eq!(
        got.frontmatter.source.as_deref(),
        Some("web:alice"),
        "labeled restore re-stamps source web:alice"
    );
    let rows = rs.list(path).await.unwrap();
    assert_eq!(rows[0].op, "restore");
    assert_eq!(rows[0].source, "web:alice", "the row mirrors the label");
    assert_eq!(rows[0].summary, "restored to revision 1");
}

#[tokio::test]
async fn restore_undeletes_at_original_path() {
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let cs = ConceptStore::for_user(&store, user, master.clone());
    let rs = RevisionStore::for_user(store.pool(), user, &master);
    let path = "/notes/ghost.md";

    cs.put(&concept(path, "Ghost", "ghost body")).await.unwrap(); // rev 1
    cs.delete(path).await.unwrap(); // rev 2
    assert!(
        cs.get(path).await.is_err(),
        "concept is gone before the restore"
    );

    // Restoring the DELETE revision re-creates it at the original path.
    cs.restore(path, 2).await.unwrap();

    let got = cs.get(path).await.unwrap();
    assert_eq!(got.frontmatter.title.as_deref(), Some("Ghost"));
    assert_eq!(got.body.trim(), "ghost body");
    // Registry row is back.
    assert!(cs.list().await.unwrap().iter().any(|e| e.path == path));
    // The restore row records the pre-restore state — for an undelete
    // that is NOTHING (§3.2a-3): empty payload.
    let rows = rs.list(path).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].op, "restore");
    assert_eq!(rows[0].summary, "restored to revision 2");
    assert_eq!(
        rs.get(path, 3).await.unwrap(),
        Vec::<u8>::new(),
        "undeleting restore row carries an EMPTY payload"
    );
    // (R3 rider) The same convention is visible in the list META
    // (no decrypt — derived from the stored envelope length): the
    // undelete-restore row reports an empty payload, the ordinary
    // delete/create rows do not.
    assert!(
        rows[0].payload_empty,
        "the undelete-restore row lists as empty: {rows:?}"
    );
    assert!(
        !rows[1].payload_empty && !rows[2].payload_empty,
        "delete/create rows list as non-empty"
    );
}

#[tokio::test]
async fn stale_era_restore_reversible() {
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let cs = ConceptStore::for_user(&store, user, master.clone());
    let rs = RevisionStore::for_user(store.pool(), user, &master);
    let path = "/notes/eras.md";

    // Era 1: create → update, then the concept is deleted.
    cs.put(&concept(path, "Era One", "era one body"))
        .await
        .unwrap(); // 1
    cs.put(&concept(path, "Era One Edit", "era one edited"))
        .await
        .unwrap(); // 2
    cs.delete(path).await.unwrap(); // 3
    // Era 2: fresh create → update on the same path (chain continues).
    cs.put(&concept(path, "Era Two", "era two body zebra"))
        .await
        .unwrap(); // 4
    cs.put(&concept(path, "Era Two Edit", "era two edited"))
        .await
        .unwrap(); // 5

    // Roll all the way back to era 1's create row.
    cs.restore(path, 1).await.unwrap();

    let got = cs.get(path).await.unwrap();
    assert_eq!(got.frontmatter.title.as_deref(), Some("Era One"));
    assert_eq!(got.body.trim(), "era one body");

    // The chain is intact; the newest row is the restore.
    let rows = rs.list(path).await.unwrap();
    assert_eq!(
        rows.iter().map(|r| r.revision_no).collect::<Vec<_>>(),
        (1..=6u64).rev().collect::<Vec<_>>(),
        "the chain continues across delete/re-create eras"
    );
    assert_eq!(
        rows.iter().map(|r| r.op.as_str()).collect::<Vec<_>>(),
        ["restore", "update", "create", "delete", "update", "create"],
        "era ops preserved"
    );
    assert_eq!(rows[0].summary, "restored to revision 1");
}

#[tokio::test]
async fn staging_note_bypass_writes_no_revision() {
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let cs = ConceptStore::for_user(&store, user, master.clone());
    let rs = RevisionStore::for_user(store.pool(), user, &master);
    let note_path = "/mutation-queue/abc-123.md";
    let note = concept(note_path, "Staging Note", "queue-internal scaffolding");
    let real = concept("/notes/real.md", "Real", "real body");

    // The queue-internal bypass put: the staging note writes NO revision,
    // while a real concept in the SAME batch gets its row (selective).
    cs.put_batch_internal(&[note, real]).await.unwrap();
    assert_eq!(
        revision_count(&store, &rs, &format!("user:{user}"), note_path).await,
        0,
        "staging bypass: no revision rows for the note's token"
    );
    let real_rows = rs.list("/notes/real.md").await.unwrap();
    assert_eq!(real_rows.len(), 1, "the sibling real concept still records");
    assert_eq!(real_rows[0].op, "create");

    // The staging-note delete bypasses too (the queue's
    // delete_staging_note call site is exactly this shape).
    cs.delete_internal(note_path).await.unwrap();
    assert_eq!(
        revision_count(&store, &rs, &format!("user:{user}"), note_path).await,
        0,
        "staging delete bypass: still no revision rows"
    );
}

#[tokio::test]
async fn with_revision_op_tags_migration() {
    // The deprecated-tag sweep's exact construction (service skills
    // scope, provenance "migration") — with_revision_op tags every
    // revision the put writes, overriding the mechanical derive.
    let (dir, store) = store().await;
    let service = ServiceKey::from_bytes(&[8u8; 32]).unwrap();
    let cs = ConceptStore::for_service(
        &store,
        service.clone(),
        &dir.path().join("skills"),
        "skills",
    )
    .with_provenance("migration")
    .with_revision_op("migration");
    let rs = RevisionStore::for_service(store.pool(), &service, "skills");

    // First write on a fresh path: the tag wins — the sweep's rewrite of
    // an EXISTING deprecated concept must never read as "create".
    cs.put(&concept("/deploy.md", "Deploy Skill", "deploy body"))
        .await
        .unwrap();
    let rows = rs.list("/deploy.md").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].op, "migration");
    assert_eq!(rows[0].source, "migration");
    assert_eq!(
        rows[0].summary,
        "deprecated tag migrated to status: retired"
    );

    // The tag is stable across a re-put (never flips to update).
    cs.put(&concept("/deploy.md", "Deploy Skill v2", "deploy body v2"))
        .await
        .unwrap();
    let rows = rs.list("/deploy.md").await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].op, "migration");
    // The concept itself updated normally.
    assert_eq!(
        cs.get("/deploy.md")
            .await
            .unwrap()
            .frontmatter
            .title
            .as_deref(),
        Some("Deploy Skill v2")
    );
}

#[tokio::test]
async fn revision_source_mirrors_stamped_concept() {
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let rs = RevisionStore::for_user(store.pool(), user, &master);

    // Labeled store: the system label wins over a spoofed author source.
    let labeled = ConceptStore::for_user(&store, user, master.clone()).with_provenance("mcp:test");
    let mut spoofed = concept("/a.md", "A", "body a");
    spoofed.frontmatter.source = Some("llm-spoofed".into());
    labeled.put(&spoofed).await.unwrap();
    assert_eq!(
        rs.list("/a.md").await.unwrap()[0].source,
        "mcp:test",
        "the stamped label, not the author's spoofed value"
    );

    // Unlabeled store: no system actor — the author's own source value
    // survives on disk and is what the row carries.
    let unlabeled = ConceptStore::for_user(&store, user, master.clone());
    let mut authored = concept("/b.md", "B", "body b");
    authored.frontmatter.source = Some("web:form".into());
    unlabeled.put(&authored).await.unwrap();
    assert_eq!(
        rs.list("/b.md").await.unwrap()[0].source,
        "web:form",
        "no label ⇒ the row mirrors the concept's source"
    );

    // An EMPTY label is the legacy-unknown-provenance convention: the row
    // records '' even though the concept's own frontmatter keeps its
    // author value (the queue's real call site passes the row's source).
    let empty_label = ConceptStore::for_user(&store, user, master.clone()).with_provenance("");
    let mut legacy = concept("/c.md", "C", "body c");
    legacy.frontmatter.source = Some("another-spoof".into());
    empty_label.put(&legacy).await.unwrap();
    assert_eq!(
        rs.list("/c.md").await.unwrap()[0].source,
        "",
        "empty label ⇒ unknown provenance in the row"
    );
}

#[tokio::test]
async fn lifecycle_validation_failure_writes_no_revision() {
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let cs = ConceptStore::for_user(&store, user, master.clone());
    let rs = RevisionStore::for_user(store.pool(), user, &master);

    // Guard-before-revision: the stamping block's validated() runs before
    // any revision insert, so a bad-status concept rejects with ZERO
    // revision rows (asserted by behavior — the row count stays 0).
    let mut bad = concept("/lifecycle/bad.md", "Bad", "bad body");
    bad.frontmatter.status = Some("archive".into()); // not active|superseded|retired
    let err = cs.put(&bad).await.unwrap_err();
    assert!(
        matches!(
            err,
            ConceptStoreError::Parse(mycelium_core::ConceptError::InvalidLifecycleStatus { .. })
        ),
        "expected the lifecycle guard, got {err}"
    );
    assert_eq!(
        revision_count(&store, &rs, &format!("user:{user}"), "/lifecycle/bad.md").await,
        0,
        "validated() rejects BEFORE any revision row"
    );
    assert!(
        cs.list().await.unwrap().is_empty(),
        "no side changes either"
    );
}

#[tokio::test]
async fn revision_failure_leaves_no_effect() {
    // Review Focus #5: a revision insert that FAILS aborts the put
    // BEFORE any effect (no half-applied state, no revision-less
    // change). An op tag outside REVISION_OPS is the natural injection.
    let (_dir, store) = store().await;
    let user = Uuid::new_v4();
    let master = generate_master_key();
    let cs = ConceptStore::for_user(&store, user, master.clone()).with_revision_op("bogus-op");
    let rs = RevisionStore::for_user(store.pool(), user, &master);

    let err = cs
        .put(&concept("/notes/nope.md", "Nope", "nope body"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ConceptStoreError::Revisions(RevisionsError::InvalidOp(_))
        ),
        "expected the record's InvalidOp failure, got {err}"
    );
    assert_eq!(
        revision_count(&store, &rs, &format!("user:{user}"), "/notes/nope.md").await,
        0
    );
    assert!(
        cs.list().await.unwrap().is_empty(),
        "no registry row landed"
    );
    assert!(cs.get("/notes/nope.md").await.is_err(), "no file landed");
}
