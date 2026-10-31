//! Migration 0010 schema guards: `concept_revisions` (spec §3.2 — the
//! write-ahead undo log) and `search_vectors` (spec §3.3 — the hybrid
//! vector rows) exist with exactly the columns and constraints the
//! design spec prescribes. The later steps of the stream build their
//! writers and readers on this schema, so any drift here is a schema
//! contract break caught at the pin, not downstream.

use mycelium_store::Store;

/// Fresh temp-dir store (runs ALL migrations, including 0010),
/// mirroring `chat_schema_test.rs`.
async fn test_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    (dir, store)
}

/// `(name, type, notnull, dflt_value)` for one table's columns, in
/// declaration order (pragma's cid order).
async fn columns(store: &Store, table: &str) -> Vec<(String, String, i32, Option<String>)> {
    sqlx::query_as(&format!(
        "SELECT name, \"type\", \"notnull\", dflt_value FROM pragma_table_info('{table}')"
    ))
    .fetch_all(store.pool())
    .await
    .unwrap()
}

/// The declared UNIQUE (scope_id, path_token, revision_no) as the
/// `(ordered column names)` of its enforced index — whatever name the
/// autoindex carries.
async fn enforced_unique_columns(store: &Store, table: &str) -> Vec<(String,)> {
    let (index_name,): (String,) = sqlx::query_as(&format!(
        "SELECT name FROM pragma_index_list('{table}') WHERE origin = 'u'"
    ))
    .fetch_one(store.pool())
    .await
    .unwrap();
    sqlx::query_as(&format!(
        "SELECT name FROM pragma_index_info('{index_name}')"
    ))
    .fetch_all(store.pool())
    .await
    .unwrap()
}

/// The table-level PRIMARY KEY as `(ordered column names)` read off the
/// pragma's `pk` column (1-based composite order).
async fn primary_key_columns(store: &Store, table: &str) -> Vec<(String,)> {
    sqlx::query_as(&format!(
        "SELECT name FROM pragma_table_info('{table}') WHERE pk != 0 ORDER BY pk"
    ))
    .fetch_all(store.pool())
    .await
    .unwrap()
}

/// Spec §3.2: the column list in declaration order with their SQL
/// types and NOT NULLs; `summary` has no default (the spec prescribes
/// the summary text, never "unsettable").
#[tokio::test]
async fn concept_revisions_columns_match_spec() {
    let (_dir, store) = test_store().await;
    let cols = columns(&store, "concept_revisions").await;
    let req = |name: &str, ty: &str| {
        (
            name.to_string(),
            ty.to_string(),
            1i32,
            Option::<String>::None,
        )
    };
    assert_eq!(
        cols,
        vec![
            // `id INTEGER PRIMARY KEY` is a rowid alias: SQLite reports
            // notnull 0 (the PK makes NULL impossible) — the declared
            // shape is exactly the spec's.
            ("id".to_string(), "INTEGER".to_string(), 0, None),
            req("scope_id", "TEXT"),
            req("path_token", "TEXT"),
            req("revision_no", "INTEGER"),
            req("op", "TEXT"),
            req("source", "TEXT"),
            req("summary", "TEXT"),
            req("payload", "BLOB"),
            req("created_at", "TEXT"),
        ],
        "concept_revisions columns must match spec §3.2 exactly (order included)"
    );
}

/// Spec §3.2: the UNIQUE (scope_id, path_token, revision_no), in that
/// order.
#[tokio::test]
async fn concept_revisions_unique_key_is_scope_path_revision() {
    let (_dir, store) = test_store().await;
    let unique = enforced_unique_columns(&store, "concept_revisions").await;
    assert_eq!(
        unique,
        vec![
            ("scope_id".to_string(),),
            ("path_token".to_string(),),
            ("revision_no".to_string(),)
        ],
        "the UNIQUE key must be (scope_id, path_token, revision_no) in order"
    );
}

/// Spec §3.3: the column list with their SQL types and NOT NULLs, and
/// the PK is (scope_id, path_token) — one vector row per concept per
/// scope, upsertable in place.
#[tokio::test]
async fn search_vectors_columns_and_pk_match_spec() {
    let (_dir, store) = test_store().await;
    let cols = columns(&store, "search_vectors").await;
    let req = |name: &str, ty: &str| {
        (
            name.to_string(),
            ty.to_string(),
            1i32,
            Option::<String>::None,
        )
    };
    assert_eq!(
        cols,
        vec![
            req("scope_id", "TEXT"),
            req("path_token", "TEXT"),
            req("model", "TEXT"),
            req("dims", "SMALLINT"),
            // Spec §3.3 writes `stale INTEGER NOT NULL DEFAULT 0` — the
            // one column with a default, and it defaults to 0 (fresh).
            (
                "stale".to_string(),
                "INTEGER".to_string(),
                1i32,
                Some("0".to_string())
            ),
            req("vec", "BLOB"),
            req("updated_at", "TEXT"),
        ],
        "search_vectors columns must match spec §3.3 exactly (order included)"
    );

    assert_eq!(
        primary_key_columns(&store, "search_vectors").await,
        vec![("scope_id".to_string(),), ("path_token".to_string(),)],
        "the PRIMARY KEY must be (scope_id, path_token) in order"
    );
    // A composite PK is only a PK if it is enforced UNIQUE — the pragma
    // must show the PK key as unique.
    let unique: i32 = sqlx::query_scalar(
        "SELECT \"unique\" FROM pragma_index_list('search_vectors') WHERE origin = 'pk'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(unique, 1, "the PK index must be unique");
}
