//! mutation_queue row-op integration tests (temp data dir, real SQLite).

use mycelium_store::{QueueError, QueueStatus, QueueTool, Store};

async fn store() -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    (store, dir)
}

/// A user row to satisfy the FK (queue rows reference users; the
/// users table needs the NOT NULL columns from 0001).
async fn seed_user(store: &Store, name: &str) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO users (id, username, email, role, auth_provider, password_hash, sealed_master_key, must_change_password, totp_secret, created_at, updated_at)
         VALUES (?, ?, ?, 'user', 'local', NULL, ?, 0, NULL, ?, ?)",
    )
    .bind(id.to_string())
    .bind(name)
    .bind(format!("{name}@t"))
    .bind("placeholder-sealed-master-key")
    .bind(&now)
    .bind(&now)
    .execute(store.pool())
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn enqueue_activate_receipt_roundtrip() {
    let (store, _dir) = store().await;
    let user = seed_user(&store, "u1").await;
    // enqueue lands in staging.
    let id = store.enqueue(user, QueueTool::Add).await.unwrap();
    let row = store.queue_item(user, id).await.unwrap().unwrap();
    assert_eq!(row.status, QueueStatus::Staging);
    assert_eq!(row.attempts, 0);
    assert_eq!(row.tool, QueueTool::Add);
    // activate flips to pending (idempotent on replay).
    store.activate(id).await.unwrap();
    store.activate(id).await.unwrap();
    let row = store.queue_item(user, id).await.unwrap().unwrap();
    assert_eq!(row.status, QueueStatus::Pending);
    // claim transitions pending → running and bumps attempts.
    let claimed = store.claim_due_for_user(user).await.unwrap().unwrap();
    assert_eq!(claimed.id, id);
    assert_eq!(claimed.attempts, 1);
    // no more due items for this user until it's released.
    assert!(store.claim_due_for_user(user).await.unwrap().is_none());
    // another user is unaffected.
    let other = seed_user(&store, "u2").await;
    assert!(store.claim_due_for_user(other).await.unwrap().is_none());
}

#[tokio::test]
async fn claim_skips_items_not_yet_due() {
    let (store, _dir) = store().await;
    let user = seed_user(&store, "u1").await;
    let id = store.enqueue(user, QueueTool::Update).await.unwrap();
    store.activate(id).await.unwrap();
    // Simulate a retry timer in the future.
    let later = (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339();
    store.mark_failed(id, later, "transient").await.unwrap();
    assert!(store.claim_due_for_user(user).await.unwrap().is_none());
    // Expire the timer and claim again.
    let past = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
    store.mark_failed(id, past, "transient").await.unwrap();
    assert!(store.claim_due_for_user(user).await.unwrap().is_some());
}

#[tokio::test]
async fn done_dead_terminal_and_health() {
    let (store, _dir) = store().await;
    let user = seed_user(&store, "u1").await;
    let done_id = store.enqueue(user, QueueTool::Add).await.unwrap();
    store.activate(done_id).await.unwrap();
    store.claim_due_for_user(user).await.unwrap().unwrap();
    store
        .mark_done(done_id, &["/rust/async.md".to_string()], "integrated")
        .await
        .unwrap();
    let row = store.queue_item(user, done_id).await.unwrap().unwrap();
    assert_eq!(row.status, QueueStatus::Done);
    assert_eq!(
        serde_json::from_str::<Vec<String>>(row.final_paths.as_deref().unwrap()).unwrap(),
        vec!["/rust/async.md"]
    );
    // A dead item also leaves pending/running counts untouched.
    let dead_id = store.enqueue(user, QueueTool::Maintain).await.unwrap();
    store.activate(dead_id).await.unwrap();
    store.claim_due_for_user(user).await.unwrap().unwrap();
    store.mark_dead(dead_id, "fallback failed").await.unwrap();
    assert_eq!(
        store
            .queue_item(user, dead_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        QueueStatus::Dead
    );
    let health = store.queue_health_counts().await.unwrap();
    assert_eq!(health.dead_count, 1);
    assert_eq!(health.depth, 0); // done + dead are terminal
}

#[tokio::test]
async fn queue_health_counts_pending_by_tool() {
    let (store, _dir) = store().await;
    let user = seed_user(&store, "u1").await;
    let a = store.enqueue(user, QueueTool::Add).await.unwrap();
    let b = store.enqueue(user, QueueTool::Add).await.unwrap();
    let c = store.enqueue(user, QueueTool::Update).await.unwrap();
    for id in [a, b, c] {
        store.activate(id).await.unwrap();
    }
    let health = store.queue_health_counts().await.unwrap();
    assert_eq!(health.depth, 3);
    assert_eq!(
        health
            .per_tool
            .iter()
            .copied()
            .max_by_key(|t| t.1)
            .unwrap()
            .0,
        QueueTool::Add
    );
    assert!(health.oldest_pending_age_seconds.unwrap() >= 0);
}

#[tokio::test]
async fn enqueue_capped_rejects_at_capacity() {
    let (store, _dir) = store().await;
    let user = seed_user(&store, "u1").await;
    let a = store.enqueue_capped(user, QueueTool::Add, 2).await.unwrap();
    store.activate(a).await.unwrap();
    let b = store.enqueue_capped(user, QueueTool::Add, 2).await.unwrap();
    store.activate(b).await.unwrap();
    assert!(matches!(
        store.enqueue_capped(user, QueueTool::Add, 2).await,
        Err(QueueError::Capacity)
    ));
    // Terminal rows do not count against the cap.
    store.claim_due_for_user(user).await.unwrap().unwrap();
    store.mark_done(a, &[], "ok").await.unwrap();
    let c = store.enqueue_capped(user, QueueTool::Add, 2).await.unwrap();
    assert_ne!(c, uuid::Uuid::nil());
}

#[tokio::test]
async fn staging_and_running_rows_for_sweep() {
    let (store, _dir) = store().await;
    let user = seed_user(&store, "u1").await;
    let staged = store.enqueue(user, QueueTool::Add).await.unwrap();
    let mid = store.enqueue(user, QueueTool::Update).await.unwrap();
    store.activate(mid).await.unwrap();
    store.claim_due_for_user(user).await.unwrap().unwrap();
    assert_eq!(store.staging_rows().await.unwrap().len(), 1);
    assert_eq!(store.running_rows().await.unwrap().len(), 1);
    assert_eq!(store.staging_rows().await.unwrap()[0].id, staged);
}

#[tokio::test]
async fn due_users_listed_oldest_first() {
    let (store, _dir) = store().await;
    let u1 = seed_user(&store, "u1").await;
    let u2 = seed_user(&store, "u2").await;
    let a = store.enqueue(u1, QueueTool::Add).await.unwrap();
    store.activate(a).await.unwrap();
    // u2's row is enqueued AFTER u1's: wait a tick so created_at differs
    // (RFC 3339 strings compare lexicographically — same second ties are
    // fine for the test because ordering is by created_at and the
    // expectation only needs u1 NOT to be missing).
    let b = store.enqueue(u2, QueueTool::Add).await.unwrap();
    store.activate(b).await.unwrap();
    let due = store.queue_users_with_due().await.unwrap();
    assert_eq!(due, vec![u1, u2]);
}

/// Two concurrent claim/activate racers over many items: every row
/// ends pending (or claimed), none stuck in staging — the crash-window
/// invariant for concurrent enqueues finishing the flip.
#[tokio::test]
async fn concurrent_activate_all_reach_pending() {
    let (store, _dir) = store().await;
    let user = seed_user(&store, "u1").await;
    let mut items: Vec<uuid::Uuid> = Vec::with_capacity(8);
    for _ in 0..8 {
        items.push(store.enqueue(user, QueueTool::Add).await.unwrap());
    }
    // Racer bodies share the &Store behind awaits in this single test
    // task; tokio::join! interleaves at await points, which is the
    // interleaving the invariant cares about.
    tokio::join!(
        async {
            for id in &items {
                store.activate(*id).await.unwrap();
            }
        },
        async {
            for id in &items {
                store.activate(*id).await.unwrap();
            }
        }
    );
    // Final state: every row is pending.
    let health = store.queue_health_counts().await.unwrap();
    assert_eq!(health.depth, items.len());
    for id in &items {
        assert_eq!(
            store.queue_item(user, *id).await.unwrap().unwrap().status,
            QueueStatus::Pending
        );
    }
}

mod payload {
    use mycelium_store::QueueTool;
    use mycelium_store::file_repo::FileRepo;
    use mycelium_store::mutation_queue::{self, MutationPayload};

    #[tokio::test]
    async fn payload_roundtrip_and_not_found() {
        let (store, _dir) = super::store().await;
        let user = super::seed_user(&store, "u1").await;
        let master = mycelium_crypto::generate_master_key();
        let repo = FileRepo::new(store.user_dir(user));
        let id = uuid::Uuid::new_v4();
        let p = MutationPayload {
            tool: QueueTool::Add,
            args_json: r#"{"content":"x"}"#.to_string(),
            content: "x".to_string(),
        };
        mutation_queue::write_payload(&repo, &master, id, &p)
            .await
            .unwrap();
        let back = mutation_queue::read_payload(&repo, &master, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(back.content, "x");
        assert_eq!(back.tool, QueueTool::Add);
        // A different master key derives a different opaque stored
        // filename (the HMAC is keyed by the master key), so the blob is
        // simply not found → None — no existence leak, and no
        // decryption error to distinguish a wrong key from a missing
        // blob.
        let other = mycelium_crypto::generate_master_key();
        assert!(
            mutation_queue::read_payload(&repo, &other, id)
                .await
                .unwrap()
                .is_none()
        );
        // Same key, unknown id → absent blob → None.
        assert!(
            mutation_queue::read_payload(&repo, &master, uuid::Uuid::new_v4())
                .await
                .unwrap()
                .is_none()
        );
        // Stored name is opaque + flat: list() never shows canonical paths.
        let names = repo.list().await.unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].len(), 64);
    }
}

mod reservation {
    use mycelium_core::concept::{Concept, Frontmatter};
    use mycelium_store::ConceptStore;

    #[tokio::test]
    async fn external_put_rejected_internal_allowed() {
        let (store, _dir) = super::store().await;
        let user = super::seed_user(&store, "u1").await;
        let master = mycelium_crypto::generate_master_key();
        let cs = ConceptStore::for_user(&store, user, master);
        // External caller: reserved prefix rejected.
        let foreign = Concept::new(
            Frontmatter {
                concept_type: "Note".into(),
                title: Some("x".into()),
                ..Default::default()
            },
            "hi".to_string(),
            "/mutation-queue/12345678-1234-1234-1234-123456789abc".to_string(),
        );
        assert!(matches!(
            cs.put(&foreign).await,
            Err(mycelium_store::ConceptStoreError::QueuePath(_))
        ));
        // Other paths are unaffected.
        let ok = Concept::new(
            Frontmatter {
                concept_type: "Note".into(),
                title: Some("y".into()),
                ..Default::default()
            },
            "hi".to_string(),
            "/notes/y.md".to_string(),
        );
        cs.put(&ok).await.unwrap();
        // Internal caller: the exact same path writes through.
        cs.put_batch_internal(std::slice::from_ref(&foreign))
            .await
            .unwrap();
        let read_back = cs.get(foreign.source_path.as_str()).await.unwrap();
        assert_eq!(read_back.body, "hi");
        // External delete also rejected; internal delete works.
        assert!(matches!(
            cs.delete(foreign.source_path.as_str()).await,
            Err(mycelium_store::ConceptStoreError::QueuePath(_))
        ));
        cs.delete_internal(foreign.source_path.as_str())
            .await
            .unwrap();
        assert!(cs.get(foreign.source_path.as_str()).await.is_err());
    }
}
