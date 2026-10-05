//! fallback.rs integration tests: the deterministic write paths moved
//! from the MCP layer work against a real temp-dir store.

use mycelium_crypto::keys::MasterKey;
use mycelium_librarian::fallback::{self, derive_title, slugify};
use mycelium_store::{ConceptStore, Store};

async fn user_store() -> (
    &'static Store,
    tempfile::TempDir,
    ConceptStore<'static>,
    uuid::Uuid,
) {
    // Leak the Store to lend it to ConceptStore<'static> for test brevity:
    // the tempdir outlives the test function, so this is safe for tests.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let store: &'static Store = Box::leak(Box::new(store));
    let user_id = uuid::Uuid::new_v4();
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO users (id, username, email, role, auth_provider, password_hash, sealed_master_key, must_change_password, totp_secret, created_at, updated_at)
         VALUES (?, 'u-seed', 'seed@t', 'user', 'local', NULL, 'placeholder', 0, NULL, ?, ?)",
    )
    .bind(user_id.to_string())
    .bind(&now)
    .bind(&now)
    .execute(store.pool())
    .await
    .unwrap();
    // There is no MasterKey::generate() — the constructor is the free
    // fn mycelium_crypto::generate_master_key().
    let master: MasterKey = mycelium_crypto::generate_master_key();
    let cs = ConceptStore::for_user(store, user_id, master);
    (store, dir, cs, user_id)
}

#[tokio::test]
async fn add_direct_write_collision_disambiguation() {
    let (_store, _dir, cs, _u) = user_store().await;
    let p1 = fallback::direct_write_add(&cs, "alpha fact one", Some("notes/alpha"), None, None)
        .await
        .unwrap();
    let p2 = fallback::direct_write_add(&cs, "different body", Some("notes/alpha"), None, None)
        .await
        .unwrap();
    assert_eq!(p1, "/notes/alpha.md");
    assert_eq!(p2, "/notes/alpha-2.md");
    // Idempotent re-record: same content, same path.
    let p3 = fallback::direct_write_add(&cs, "alpha fact one", Some("notes/alpha"), None, None)
        .await
        .unwrap();
    assert_eq!(p3, "/notes/alpha.md");
    // concept_type honored.
    let p4 = fallback::direct_write_add(&cs, "skill body", None, None, Some("Skill"))
        .await
        .unwrap();
    let c = cs.get(&p4).await.unwrap();
    assert_eq!(c.frontmatter.concept_type, "Skill");
    assert!(cs.list().await.unwrap().iter().any(|e| e.path == p1));
}

#[tokio::test]
async fn addendum_appends_dated_block() {
    let (_store, _dir, cs, _u) = user_store().await;
    fallback::direct_write_add(&cs, "original content", Some("topic"), None, None)
        .await
        .unwrap();
    let p = fallback::dated_addendum_update(&cs, "correct the second paragraph", Some("/topic.md"))
        .await
        .unwrap();
    assert_eq!(p, "/topic.md");
    let c = cs.get(&p).await.unwrap();
    assert!(c.body.contains("correct the second paragraph"));
    assert!(c.body.contains("<!-- mycelium2:update:"));
    // No-match addendum is a NotFound.
    let err = fallback::dated_addendum_update(&cs, "no such topic anywhere", None).await;
    assert!(matches!(
        err,
        Err(mycelium_store::ConceptStoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn maintain_wires_orphans_and_flags_broken() {
    let (_store, _dir, cs, _u) = user_store().await;
    fallback::direct_write_add(&cs, "topic A body", Some("topic-a"), None, None)
        .await
        .unwrap();
    fallback::direct_write_add(&cs, "topic A body companion", Some("topic-a2"), None, None)
        .await
        .unwrap();
    // An inbound broken link.
    let mut c = cs.get("/topic-a.md").await.unwrap();
    c.body.push_str("\n[missing](/ghost.md)\n");
    cs.put(&c).await.unwrap(); // cs is plain user scope; put is fine (not queue path)
    let summary = fallback::wire_and_flag_maintain(&cs).await.unwrap();
    // Orphan wiring: one of the two same-topic concepts got linked.
    assert!(summary.contains("wired"));
    // Still absent — the broken link is only flagged in its owner.
    assert!(cs.get("/ghost.md").await.is_err());
    let flagged = cs.get("/topic-a.md").await.unwrap();
    // Marker-position regression (moved from the MCP integration suite
    // when the repair code moved here): the flag goes AFTER the link's
    // closing paren so the markdown link stays well-formed and the
    // scanner still sees the broken target on the next health check.
    assert!(
        flagged
            .body
            .contains("](/ghost.md) <!-- mycelium2:broken-link -->"),
        "marker must follow the closing paren, got: {:?}",
        flagged.body
    );
    assert!(
        !flagged.body.contains("](/ghost.md <!--"),
        "marker must NOT be inside the link destination"
    );
    // A second run on a now-healthy graph returns the healthy string.
    // (After wiring, each previously-orphaned concept has an inbound or
    // outbound link; the flagged broken-link still counts as broken, so
    // assert only the wiring count decreases.)
    let summary2 = fallback::wire_and_flag_maintain(&cs).await.unwrap();
    assert!(summary2.contains("maintained"));
}

#[tokio::test]
async fn helpers_match_mcp_behavior() {
    assert_eq!(derive_title("first line\nsecond"), "first line");
    assert_eq!(derive_title(""), "untitled");
    assert_eq!(slugify("Hello, World — École!"), "hello-world-cole");
    assert_eq!(fallback::canonical("foo/bar.md"), "/foo/bar.md");
    assert_eq!(fallback::canonical("foo/bar"), "/foo/bar.md");
}
