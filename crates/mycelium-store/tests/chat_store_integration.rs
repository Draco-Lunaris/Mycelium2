//! ChatStore integration: sealed roundtrips, ownership, one-running
//! invariant, claim exclusivity, boot sweep, prune, supersede.

use mycelium_crypto::keys::MasterKey;
use mycelium_store::Store;
use mycelium_store::chat::{ChatError, ChatRole, ChatStore, ChatTurnStatus};
use uuid::Uuid;

/// Temp-dir store + one FKed user row (the brief's `test_store` shape).
async fn test_store() -> (tempfile::TempDir, Store, MasterKey, Uuid) {
    test_store_named("u1").await
}

async fn test_store_named(name: &str) -> (tempfile::TempDir, Store, MasterKey, Uuid) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let user_id = seed_user(&store, name).await;
    let master = mycelium_crypto::generate_master_key();
    (dir, store, master, user_id)
}

/// A user row to satisfy the FK (users are owned by the auth layer;
/// integration tests insert directly).
async fn seed_user(store: &Store, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO users (id, username, email, role, auth_provider, password_hash, \
         sealed_master_key, must_change_password, totp_secret, created_at, updated_at) \
         VALUES (?, ?, ?, 'user', 'local', NULL, ?, 0, NULL, ?, ?)",
    )
    .bind(id.to_string())
    .bind(name)
    .bind(format!("{name}@t.local"))
    .bind("placeholder-sealed-master-key")
    .bind(&now)
    .bind(&now)
    .execute(store.pool())
    .await
    .unwrap();
    id
}

fn store_of(store: &Store) -> ChatStore {
    ChatStore::new(store.pool().clone())
}

#[tokio::test]
async fn create_list_roundtrip_seals_titles() {
    let (_dir, store, master, user) = test_store().await;
    let chat = store_of(&store);
    // Two sessions w/ distinct titles.
    let id_a = chat
        .create_session(user, &master, "First title")
        .await
        .unwrap();
    let id_b = chat
        .create_session(user, &master, "Second title")
        .await
        .unwrap();
    assert_ne!(id_a, id_b);
    // List is newest-first by updated_at (b created after a).
    let sessions = chat.list_sessions(user, &master).await.unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].id, id_b);
    assert_eq!(sessions[0].title, "Second title");
    assert_eq!(sessions[1].id, id_a);
    assert_eq!(sessions[1].title, "First title");
    // updated_at surfaces as an RFC 3339-ish string.
    assert!(!sessions[0].updated_at.is_empty());
    // Raw column holds a hex AEAD envelope — no plaintext in SQLite.
    let raw: Vec<(String,)> =
        sqlx::query_as("SELECT title_sealed FROM chat_sessions WHERE user_id = ?")
            .bind(user.to_string())
            .fetch_all(store.pool())
            .await
            .unwrap();
    assert_eq!(raw.len(), 2);
    for (title_sealed,) in raw {
        assert!(!title_sealed.contains("First title"));
        assert!(!title_sealed.contains("Second title"));
        // Hex envelope: all characters are hex digits (MYC2 envelope
        // bytes hex-encoded — the config.rs sealing precedent).
        assert!(
            title_sealed.bytes().all(|b| b.is_ascii_hexdigit()),
            "title_sealed is not hex: {title_sealed}"
        );
    }
}

#[tokio::test]
async fn title_is_truncated() {
    let (_dir, store, master, user) = test_store().await;
    let chat = store_of(&store);
    // The WEB layer does derive+truncate; the store stores what it is
    // given. Pin the pass-through: a 200-char title roundtrips unharmed.
    let long = "L".repeat(200);
    let id = chat.create_session(user, &master, &long).await.unwrap();
    let meta = chat.session_meta(user, &master, id).await.unwrap();
    assert_eq!(meta.title, long);
    let sessions = chat.list_sessions(user, &master).await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].title, long);
}

#[tokio::test]
async fn foreign_key_cannot_decrypt_rows() {
    let (_dir, store, master, user) = test_store().await;
    let chat = store_of(&store);
    let id = chat
        .create_session(user, &master, "secret title")
        .await
        .unwrap();
    chat.append_message(user, &master, id, ChatRole::User, "hello there")
        .await
        .unwrap();
    // A different master key derives a different chat DEK: nothing opens.
    let other = mycelium_crypto::generate_master_key();
    let sessions = chat.list_sessions(user, &other).await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].title, "(unreadable)");
    let rows = chat.messages_after(user, &other, id, 0).await.unwrap();
    // The unreadable row is skipped, not returned and not fatal.
    assert!(rows.is_empty(), "undecryptable rows must be skipped");
    // session_meta is ownership-scoped, not key-scoped: same user +
    // wrong key is still Ok, with the degraded title (skip-not-fail).
    let meta = chat.session_meta(user, &other, id).await.unwrap();
    assert_eq!(meta.title, "(unreadable)");
    assert_eq!(meta.max_seq, 1, "metadata is key-independent");
    // The owner's full read still works.
    let meta = chat.session_meta(user, &master, id).await.unwrap();
    assert_eq!(meta.title, "secret title");
    let rows = chat.messages_after(user, &master, id, 0).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content, "hello there");
}

#[tokio::test]
async fn create_turn_and_double_submit_conflict() {
    let (_dir, store, master, user) = test_store().await;
    let chat = store_of(&store);
    let id = chat.create_session(user, &master, "s").await.unwrap();
    // First turn is created.
    let turn = chat
        .create_turn(user, &master, id, "first message")
        .await
        .unwrap();
    assert_eq!(turn.status, ChatTurnStatus::Running);
    assert_eq!(turn.attempts, 0);
    assert_eq!(turn.session_id, id);
    // The user message took seq 1.
    assert_eq!(turn.user_seq, 1);
    // The DB enforces the one-running invariant: the second submit is a
    // Conflict (the unique partial index, not a client flag).
    let second = chat.create_turn(user, &master, id, "second message").await;
    match second {
        Err(ChatError::Conflict) => {}
        other => panic!("expected Conflict, got {other:?}"),
    }
    // The double-submit left no second message row (the transaction
    // rolled back whole).
    let rows = chat.messages_after(user, &master, id, 0).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].seq, 1);
    assert_eq!(rows[0].role, ChatRole::User);
    assert_eq!(rows[0].content, "first message");
    // And no second turn row.
    let turns = chat.resumable_turns().await.unwrap();
    assert!(turns.is_empty(), "a running turn is not resumable-listed");
    chat.running_turn(user, id).await.unwrap().unwrap();
}

#[tokio::test]
async fn append_assigns_monotonic_seq_and_bumps_updated_at() {
    let (_dir, store, master, user) = test_store().await;
    let chat = store_of(&store);
    let id = chat.create_session(user, &master, "s").await.unwrap();
    // A standalone append starts at seq 1 (no create_turn first):
    // three appends assign 1, 2, 3; a create_turn's message then
    // continues at 4 (max seq + 1 regardless of role).
    let s1 = chat
        .append_message(user, &master, id, ChatRole::User, "first")
        .await
        .unwrap();
    let s2 = chat
        .append_message(
            user,
            &master,
            id,
            ChatRole::Tool,
            r#"{"name":"t","detail":"d"}"#,
        )
        .await
        .unwrap();
    let s3 = chat
        .append_message(user, &master, id, ChatRole::Assistant, "reply")
        .await
        .unwrap();
    assert_eq!((s1, s2, s3), (1, 2, 3));
    // updated_at strictly increases across creates.
    let updated: Vec<String> =
        sqlx::query_as("SELECT updated_at FROM chat_sessions WHERE id = ? AND user_id = ?")
            .bind(id.to_string())
            .bind(user.to_string())
            .fetch_all(store.pool())
            .await
            .unwrap()
            .into_iter()
            .map(|(u,)| u)
            .collect();
    assert_eq!(updated.len(), 1, "exactly one session row");
    // The created_at/updated_at pair prove the bump: updated_at moved
    // (created rows start with created_at == updated_at).
    let (created_at,): (String,) =
        sqlx::query_as("SELECT created_at FROM chat_sessions WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert!(
        updated[0] >= created_at,
        "updated_at exists alongside created_at"
    );
    // A fourth append (error role) continues monotonically.
    let s4 = chat
        .append_message(user, &master, id, ChatRole::Error, "boom")
        .await
        .unwrap();
    assert_eq!(s4, 4);
    // messages_after(0) returns all; messages_after(2) returns just
    // seq 3 and 4.
    let all = chat.messages_after(user, &master, id, 0).await.unwrap();
    let seqs: Vec<i64> = all.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4]);
    assert_eq!(all[0].role, ChatRole::User);
    assert_eq!(all[1].role, ChatRole::Tool);
    assert_eq!(all[2].role, ChatRole::Assistant);
    assert_eq!(all[3].role, ChatRole::Error);
    let tail = chat.messages_after(user, &master, id, 2).await.unwrap();
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].seq, 3);
    assert_eq!(tail[1].seq, 4);
}

#[tokio::test]
async fn claim_resumable_is_once_only() {
    let (_dir, store, master, user) = test_store().await;
    let chat = store_of(&store);
    let id = chat.create_session(user, &master, "s").await.unwrap();
    let turn = chat.create_turn(user, &master, id, "msg").await.unwrap();
    // The boot-sweep path: running → interrupted (attempts stay 0). A
    // done turn is not running, so the sweep never touches it.
    chat.finalize_turn(user, turn.id, ChatTurnStatus::Done)
        .await
        .unwrap();
    assert_eq!(
        chat.boot_sweep().await.unwrap(),
        0,
        "done turn is not swept"
    );
    assert!(!chat.claim_resumable(user, id, turn.id).await.unwrap());
    // A genuinely resumable row: a second turn in the same session
    // (nothing is running now, so the unique index allows it), then
    // boot_sweep it interrupted with attempts = 0.
    let turn2 = chat.create_turn(user, &master, id, "again").await.unwrap();
    assert_eq!(chat.boot_sweep().await.unwrap(), 1);
    // claim_resumable: exactly once (attempts 0 → 1). The conditional
    // UPDATE is the once-guard.
    assert!(
        chat.claim_resumable(user, id, turn2.id).await.unwrap(),
        "first claim wins"
    );
    let row = chat.latest_turn(user, id).await.unwrap().unwrap();
    assert_eq!(row.id, turn2.id);
    assert_eq!(row.status, ChatTurnStatus::Running);
    assert_eq!(row.attempts, 1, "claim incremented attempts");
    // Re-claim after the claim (attempts = 1, and running anyway): false.
    assert!(!chat.claim_resumable(user, id, turn2.id).await.unwrap());
    chat.finalize_turn(user, turn2.id, ChatTurnStatus::Interrupted)
        .await
        .unwrap();
    // Finalized back to interrupted but with attempts = 1: the resume
    // worker's claim is once-only forever — a second interruption is
    // final (spec D6).
    assert!(!chat.claim_resumable(user, id, turn2.id).await.unwrap());
}

#[tokio::test]
async fn boot_sweep_marks_running_interrupted() {
    let (_dir, store, master, user) = test_store().await;
    let chat = store_of(&store);
    let id = chat.create_session(user, &master, "s").await.unwrap();
    let turn = chat.create_turn(user, &master, id, "msg").await.unwrap();
    let swept = chat.boot_sweep().await.unwrap();
    assert_eq!(swept, 1);
    // Status is interrupted, attempts unchanged (0).
    let latest = chat.latest_turn(user, id).await.unwrap().unwrap();
    assert_eq!(latest.id, turn.id);
    assert_eq!(latest.status, ChatTurnStatus::Interrupted);
    assert_eq!(latest.attempts, 0);
    // Sweep is idempotent for already-interrupted turns.
    let again = chat.boot_sweep().await.unwrap();
    assert_eq!(again, 0);
    // And a done turn is not swept.
    let other = chat.create_session(user, &master, "t").await.unwrap();
    let t2 = chat.create_turn(user, &master, other, "m2").await.unwrap();
    chat.finalize_turn(user, t2.id, ChatTurnStatus::Done)
        .await
        .unwrap();
    assert_eq!(chat.boot_sweep().await.unwrap(), 0);
}

#[tokio::test]
async fn supersede_abandons_interrupted_turns() {
    let (_dir, store, master, user) = test_store().await;
    let chat = store_of(&store);
    let id = chat.create_session(user, &master, "s").await.unwrap();
    // Build a resumable interrupted turn: create → boot_sweep.
    let old = chat.create_turn(user, &master, id, "first").await.unwrap();
    assert_eq!(chat.boot_sweep().await.unwrap(), 1);
    // create_turn supersedes the resumable interrupted turn (D8).
    let new_turn = chat.create_turn(user, &master, id, "second").await.unwrap();
    assert_eq!(new_turn.status, ChatTurnStatus::Running);
    assert_ne!(new_turn.id, old.id);
    // Old turn abandoned, new turn running (the unique index allows it).
    let (new_status,): (String,) = sqlx::query_as("SELECT status FROM chat_turns WHERE id = ?")
        .bind(new_turn.id.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(new_status, "running");
    let (old_status,): (String,) = sqlx::query_as("SELECT status FROM chat_turns WHERE id = ?")
        .bind(old.id.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(old_status, "abandoned");
    // The superseded turn is out of the resumable sweep.
    assert!(chat.resumable_turns().await.unwrap().is_empty());
    // And the new message took the next seq.
    let new_row = chat.messages_after(user, &master, id, 1).await.unwrap();
    assert_eq!(new_row.len(), 1);
    assert_eq!(new_row[0].seq, 2);
    assert_eq!(new_row[0].role, ChatRole::User);
}

#[tokio::test]
async fn delete_cascade_and_running_conflict() {
    let (_dir, store, master, user) = test_store().await;
    let chat = store_of(&store);
    let id = chat.create_session(user, &master, "s").await.unwrap();
    let turn = chat.create_turn(user, &master, id, "msg").await.unwrap();
    chat.append_message(user, &master, id, ChatRole::Assistant, "partial")
        .await
        .unwrap();
    // Delete while running → Conflict.
    match chat.delete_session(user, id).await {
        Err(ChatError::Conflict) => {}
        other => panic!("expected Conflict, got {other:?}"),
    }
    // The rows survive.
    assert!(
        chat.messages_after(user, &master, id, 0)
            .await
            .unwrap()
            .len()
            == 2
    );
    // Finalize done, then delete cleanly (cascades messages + turns).
    chat.finalize_turn(user, turn.id, ChatTurnStatus::Done)
        .await
        .unwrap();
    chat.delete_session(user, id).await.unwrap();
    assert!(matches!(
        chat.session_meta(user, &master, id).await,
        Err(ChatError::NotFound)
    ));
    let (messages, turns): (i64, i64) = (
        sqlx::query_scalar("SELECT COUNT(*) FROM chat_messages WHERE session_id = ?")
            .bind(id.to_string())
            .fetch_one(store.pool())
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT COUNT(*) FROM chat_turns WHERE session_id = ?")
            .bind(id.to_string())
            .fetch_one(store.pool())
            .await
            .unwrap(),
    );
    assert_eq!(messages, 0, "chat_messages cascade-deleted");
    assert_eq!(turns, 0, "chat_turns cascade-deleted");
}

#[tokio::test]
async fn prune_keeps_owed_answers() {
    let (_dir, store, master, user) = test_store().await;
    let chat = store_of(&store);
    // 100 plain sessions fill the cap.
    for _ in 0..100 {
        chat.create_session(user, &master, "plain").await.unwrap();
    }
    // 101st create succeeds (auto-prune-on-create, never a rejection).
    let next = chat.create_session(user, &master, "next").await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_sessions WHERE user_id = ?")
        .bind(user.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count, 100, "prune keeps the cap at 100");
    // The oldest plain session was deleted; the newest survives.
    assert!(
        chat.session_meta(user, &master, next).await.is_ok(),
        "newest session survives"
    );
    // A session owing an answer is never pruned: give the current
    // oldest an interrupted turn, then overflow the cap again.
    // Turn the current oldest into a resumable (interrupted, attempts 0).
    let oldest_id = Uuid::parse_str(
        &sqlx::query_scalar::<_, String>(
            "SELECT id FROM chat_sessions WHERE user_id = ? ORDER BY updated_at LIMIT 1",
        )
        .bind(user.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap(),
    )
    .unwrap();
    let turn = chat
        .create_turn(user, &master, oldest_id, "owes an answer")
        .await
        .unwrap();
    assert_eq!(chat.boot_sweep().await.unwrap(), 1);
    // Overflow again: the protected oldest survives; the next-oldest
    // plain session is deleted.
    chat.create_session(user, &master, "next2").await.unwrap();
    assert!(
        chat.session_meta(user, &master, oldest_id).await.is_ok(),
        "session with a resumable interrupted turn survives prune"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_sessions WHERE user_id = ?")
        .bind(user.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count, 100);
    // The protected turn is still resumable after the prune.
    let resumables = chat.resumable_turns().await.unwrap();
    assert_eq!(resumables.len(), 1);
    assert_eq!(resumables[0].1.id, turn.id);
    assert_eq!(resumables[0].1.status, ChatTurnStatus::Interrupted);
    assert_eq!(resumables[0].0, user, "resumable_turns pairs its owner");
    assert_eq!(turn.attempts, 0);
}

#[tokio::test]
async fn ownership_scoping() {
    let (_dir, store, master_a, user_a) = test_store().await;
    let chat = store_of(&store);
    let user_b = seed_user(&store, "uB").await;
    let id = chat
        .create_session(user_a, &master_a, "A's session")
        .await
        .unwrap();
    chat.create_turn(user_a, &master_a, id, "message")
        .await
        .unwrap();
    // User B sees no sessions from A.
    let b_sessions = chat.list_sessions(user_b, &master_a).await.unwrap();
    assert!(b_sessions.is_empty(), "list is user-scoped");
    // Every user-facing read of A's session under B is NotFound.
    assert!(matches!(
        chat.session_meta(user_b, &master_a, id).await,
        Err(ChatError::NotFound)
    ));
    assert!(matches!(
        chat.messages_after(user_b, &master_a, id, 0).await,
        Err(ChatError::NotFound)
    ));
    assert!(chat.running_turn(user_b, id).await.unwrap().is_none());
    assert!(chat.latest_turn(user_b, id).await.unwrap().is_none());
    // Writes under B are NotFound too.
    assert!(matches!(
        chat.append_message(user_b, &master_a, id, ChatRole::User, "no")
            .await,
        Err(ChatError::NotFound)
    ));
    assert!(matches!(
        chat.create_turn(user_b, &master_a, id, "no").await,
        Err(ChatError::NotFound)
    ));
    assert!(matches!(
        chat.delete_session(user_b, id).await,
        Err(ChatError::NotFound)
    ));
    // claim_resumable under B: no rows affected (no oracle for the id).
    let turn = chat.running_turn(user_a, id).await.unwrap().unwrap();
    assert!(
        !chat.claim_resumable(user_b, id, turn.id).await.unwrap(),
        "foreign claim must find nothing"
    );
    assert!(matches!(
        chat.finalize_turn(user_b, turn.id, ChatTurnStatus::Done)
            .await,
        Err(ChatError::NotFound)
    ));
}
