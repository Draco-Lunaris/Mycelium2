//! Books/ingest-job admin row queries (Task 10, outage recovery):
//! `pending_ingest_job_rows` — the admin System page's waiting-table
//! source for ingest work. pending jobs only, with the book slug joined
//! from `books` and the requester's username from `users`, ordered.

use mycelium_store::Store;
use uuid::Uuid;

async fn store() -> (Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    (store, dir)
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

/// shelf + book rows for a job FK; returns (shelf_id, book_id).
async fn seed_book(store: &Store, slug: &str) -> (Uuid, Uuid) {
    let shelf = Uuid::new_v4();
    let book = Uuid::new_v4();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO bookshelves (id, name, created_at) VALUES (?, ?, ?)")
        .bind(shelf.to_string())
        .bind(format!("shelf-{slug}"))
        .bind(&now)
        .execute(store.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO books (id, bookshelf_id, slug, title, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(book.to_string())
    .bind(shelf.to_string())
    .bind(slug)
    .bind(format!("Book {slug}"))
    .bind(&now)
    .execute(store.pool())
    .await
    .unwrap();
    (shelf, book)
}

#[tokio::test]
async fn pending_ingest_job_rows_join_and_order() {
    let (store, _dir) = store().await;
    let alice = seed_user(&store, "alice").await;
    let bob = seed_user(&store, "bob").await;
    let (shelf_a, book_a) = seed_book(&store, "alpha-book").await;
    let (shelf_b, book_b) = seed_book(&store, "beta-book").await;

    // Oldest pending first (create_ingest_job stamps created_at now, so
    // enqueue order is creation order), then a done job that must not
    // appear.
    let job_a = store
        .create_ingest_job(shelf_a, book_a, alice)
        .await
        .unwrap();
    let job_b = store.create_ingest_job(shelf_b, book_b, bob).await.unwrap();
    store
        .update_ingest_job(job_b, mycelium_store::IngestStatus::Done, "ok")
        .await
        .unwrap();

    let rows = store.pending_ingest_job_rows().await.unwrap();
    assert_eq!(rows.len(), 1, "pending only: {rows:?}");
    assert_eq!(rows[0].id, job_a);
    assert_eq!(rows[0].slug, "alpha-book");
    assert_eq!(rows[0].username, "alice");

    // A second pending job orders by created_at after the first.
    let job_c = store.create_ingest_job(shelf_b, book_b, bob).await.unwrap();
    let rows = store.pending_ingest_job_rows().await.unwrap();
    assert_eq!(
        rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![job_a, job_c],
        "explicit ORDER BY created_at"
    );
    assert_eq!(rows[1].username, "bob");
    assert_eq!(rows[1].slug, "beta-book");
}
