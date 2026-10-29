//! Migration 0009 schema guards: `chat_turns` gains the `waiting_backend`
//! status, `park_state` provenance, and `last_error` metadata via a
//! CHECK-widen table rebuild — and the rebuild leaves everything that was
//! there before (FK cascade, the one-running invariant, the resumable
//! index) intact.

use mycelium_store::Store;
use uuid::Uuid;

/// Fresh temp-dir store (runs ALL migrations, including the 0009 rebuild)
/// + one FKed user row, mirroring `chat_store_integration.rs`.
async fn test_store() -> (tempfile::TempDir, Store, Uuid) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let user_id = seed_user(&store, "u1").await;
    (dir, store, user_id)
}

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

/// A raw chat_sessions row (title sealed placeholder — the schema test
/// doesn't go through ChatStore). Returns the session id.
async fn seed_session(store: &Store, user: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO chat_sessions (id, user_id, title_sealed, created_at, updated_at) \
         VALUES (?, ?, 'sealed-title-placeholder', ?, ?)",
    )
    .bind(id.to_string())
    .bind(user.to_string())
    .bind(&now)
    .bind(&now)
    .execute(store.pool())
    .await
    .unwrap();
    id
}

/// Raw INSERT of a turn with an explicit status; returns the execute result
/// so callers can assert CHECK acceptance/rejection.
async fn insert_turn(
    store: &Store,
    session: Uuid,
    status: &str,
    id: Uuid,
) -> Result<u64, sqlx::Error> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO chat_turns (id, session_id, user_seq, status, created_at, updated_at) \
         VALUES (?, ?, 1, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(session.to_string())
    .bind(status)
    .bind(&now)
    .bind(&now)
    .execute(store.pool())
    .await
    .map(|r| r.rows_affected())
}

/// `(name, notnull, dflt_value)` for every chat_turns column.
async fn columns(store: &Store) -> Vec<(String, i32, Option<String>)> {
    sqlx::query_as("SELECT name, \"notnull\", dflt_value FROM pragma_table_info('chat_turns')")
        .fetch_all(store.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn table_info_has_park_state_and_last_error() {
    let (_dir, store, _user) = test_store().await;
    let cols = columns(&store).await;
    let park = cols
        .iter()
        .find(|(name, _, _)| name == "park_state")
        .expect("park_state column exists after migration 0009");
    assert_eq!(park.1, 1, "park_state is NOT NULL");
    assert_eq!(
        park.2.as_deref(),
        Some("'interrupted'"),
        "park_state defaults to 'interrupted'"
    );
    let err = cols
        .iter()
        .find(|(name, _, _)| name == "last_error")
        .expect("last_error column exists after migration 0009");
    assert_eq!(err.1, 0, "last_error is nullable");
}

#[tokio::test]
async fn waiting_backend_insert_succeeds_and_columns_default() {
    let (_dir, store, user) = test_store().await;
    let session = seed_session(&store, user).await;
    let id = Uuid::new_v4();
    insert_turn(&store, session, "waiting_backend", id)
        .await
        .expect("waiting_backend passes the widened CHECK");
    // A bare INSERT omits the new columns: park_state defaults to
    // 'interrupted', last_error stays NULL.
    let (park_state, last_error): (String, Option<String>) =
        sqlx::query_as("SELECT park_state, last_error FROM chat_turns WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(park_state, "interrupted");
    assert_eq!(last_error, None);
}

#[tokio::test]
async fn bogus_status_still_rejected_by_check() {
    let (_dir, store, user) = test_store().await;
    let session = seed_session(&store, user).await;
    let res = insert_turn(&store, session, "sideways", Uuid::new_v4()).await;
    assert!(
        res.is_err(),
        "the rebuild's CHECK must still reject statuses outside the six"
    );
}

#[tokio::test]
async fn one_running_invariant_survives_rebuild() {
    let (_dir, store, user) = test_store().await;
    let session = seed_session(&store, user).await;
    insert_turn(&store, session, "running", Uuid::new_v4())
        .await
        .expect("first running turn");
    let second = insert_turn(&store, session, "running", Uuid::new_v4()).await;
    assert!(
        second.is_err(),
        "uq_chat_turns_running must still allow one running turn per session"
    );
    // A second waiting_backend turn in the same session is fine (the
    // waiting index is NOT unique).
    insert_turn(&store, session, "waiting_backend", Uuid::new_v4())
        .await
        .expect("waiting_backend turns are not unique-constrained");
}

#[tokio::test]
async fn indexes_recreated_after_rebuild() {
    let (_dir, store, _user) = test_store().await;
    let names: Vec<(String, String)> = sqlx::query_as(
        "SELECT name, COALESCE(sql, '') FROM sqlite_master \
         WHERE type = 'index' AND tbl_name = 'chat_turns'",
    )
    .fetch_all(store.pool())
    .await
    .unwrap();
    let by_name = |n: &str| names.iter().find(|(name, _)| name == n);
    let running = by_name("uq_chat_turns_running").expect("one-running index recreated");
    assert!(running.1.contains("UNIQUE"));
    assert!(running.1.contains("'running'"));
    let resumable = by_name("idx_chat_turns_resumable").expect("resumable index recreated");
    assert!(resumable.1.contains("'interrupted'"));
    let waiting = by_name("idx_chat_turns_waiting").expect("waiting index created");
    assert!(
        waiting.1.contains("'waiting_backend'"),
        "idx_chat_turns_waiting is a partial index on waiting_backend"
    );
}

#[tokio::test]
async fn session_delete_cascades_to_turns() {
    let (_dir, store, user) = test_store().await;
    let session = seed_session(&store, user).await;
    insert_turn(&store, session, "waiting_backend", Uuid::new_v4())
        .await
        .unwrap();
    insert_turn(&store, session, "done", Uuid::new_v4())
        .await
        .unwrap();
    let deleted = sqlx::query("DELETE FROM chat_sessions WHERE id = ?")
        .bind(session.to_string())
        .execute(store.pool())
        .await
        .unwrap();
    assert_eq!(deleted.rows_affected(), 1);
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_turns WHERE session_id = ?")
        .bind(session.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(remaining, 0, "FK cascade must follow rebuilt rows");
}
