//! Lifecycle boot sweep (memory-lifecycle spec §3.0, Task 6): concepts
//! tagged `deprecated` under the legacy convention are migrated to
//! `status: retired` at boot. Idempotent and predicate-driven — the
//! predicate is "the frontmatter carries a tag lowercasing to
//! `deprecated`", so a migrated concept simply stops matching: no
//! marker row, no flag, no revision (a failed scope retries next boot
//! for free).
//!
//! Scope: every user scope (raw `SELECT id FROM users`) plus the
//! `global:skills` service scope. `global:library` is NOT swept —
//! generated/book content the ingest worker owns; rewriting it is not
//! this sweep's business.
//!
//! Posture (mirrors the queue worker's and chat resume's boot sweeps):
//! best-effort, never boot-blocking. Only enumerating the users can
//! hard-fail; a scope whose master key is unrecoverable is logged and
//! skipped (it retries next boot), and a per-concept failure is logged
//! while the rest of that scope completes.

use mycelium_core::concept::Concept;
use mycelium_crypto::keys::MasterKey;
use mycelium_store::concept_store::ConceptStore;
use uuid::Uuid;

use crate::state::AppState;

/// The label the sweep stamps into every rewritten concept (`source`,
/// and the write-side provenance on the ConceptStore).
const MIGRATION_LABEL: &str = "migration";

/// The sweep's only hard-failure surface: enumerating the user scopes.
/// Everything else — master-key recovery, listing, reading, writing —
/// is logged and continued so one bad scope or concept cannot stall
/// the boot for the rest.
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error("user enumeration query failed: {0}")]
    UserEnumeration(#[from] sqlx::Error),
}

/// Rewrite one concept if it carries a `deprecated` tag
/// (case-insensitive, trimmed, exact token — never a substring match):
/// remove the tag, set `status = Some("retired")`,
/// `source = Some("migration")`, `confirmed_at = now`, and return the
/// rewritten concept. Returns None when the concept is untouched —
/// live concepts are never rewritten by this fn, which is what makes
/// the boot sweep idempotent without any marker.
///
/// Pure — no I/O; the sweep wires the result into `ConceptStore::put`
/// (built with `.with_provenance("migration")`, which re-stamps
/// `source` + `confirmed_at` on the way to disk).
pub fn rewrite_if_deprecated(concept: Concept) -> Option<Concept> {
    if !concept
        .frontmatter
        .tags
        .iter()
        .any(|t| is_deprecated_tag(t))
    {
        return None;
    }
    let mut c = concept;
    // Remove every matching tag, keep the rest in order; unrelated
    // (non-deprecated) tags are never deduped or normalized here.
    c.frontmatter.tags.retain(|t| !is_deprecated_tag(t));
    c.frontmatter.status = Some("retired".into());
    c.frontmatter.source = Some(MIGRATION_LABEL.into());
    c.frontmatter.confirmed_at = Some(chrono::Utc::now().to_rfc3339());
    Some(c)
}

/// The tag predicate: trimmed tag equality after `to_lowercase`
/// (matching the case-insensitive reserved-name precedent) — so
/// `"deprecated"`, `"Deprecated"`, `"deprecated "` match;
/// `"deprecated-x"`, `"deprecation"` do not.
fn is_deprecated_tag(tag: &str) -> bool {
    tag.trim().to_lowercase() == "deprecated"
}

/// The boot sweep (idempotent, predicate-driven — no marker row): every
/// user scope + the global:skills scope; a scope whose master key fails
/// is logged and skipped (retried next boot); a per-concept failure is
/// logged and the rest of the scope continues. Returns the number of
/// concepts migrated across all scopes — the count is for observability
/// only; the sweep never drives boot decisions.
pub async fn migrate_deprecated_tags(state: &AppState) -> Result<u32, LifecycleError> {
    let mut migrated: u32 = 0;

    // Service scope: the global skills shelf ONLY. `global:library` is
    // deliberately absent — generated/book content the ingest worker
    // owns, never rewritten by lifecycle sweeps.
    let skills = ConceptStore::for_service(
        &state.store,
        (*state.service_key).clone(),
        &state.store.skills_dir(),
        "skills",
    )
    .with_provenance(MIGRATION_LABEL);
    migrated += sweep_scope(&skills, "global:skills").await;

    // Every user scope, enumerated with raw SQL on the pool (the
    // queue-worker sweep's shape: the sweep works below the auth
    // layer). ORDER BY id keeps user iteration deterministic across
    // boots. A user row whose key is unrecoverable skips one scope —
    // the predicate-driven sweep simply retriggers next boot.
    let users: Vec<(String,)> = sqlx::query_as("SELECT id FROM users ORDER BY id")
        .fetch_all(state.store.pool())
        .await?;
    for (id,) in users {
        let user_id: Uuid = match id.parse() {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(user = %id, error = %e,
                    "lifecycle sweep: user row id unreadable — scope skipped");
                continue;
            }
        };
        // The exact recovery path chat_resume/queue worker use: cache →
        // service-key seal (empty cache at boot → the seal).
        let master: MasterKey = match state.master_key_for(user_id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(user_id = %user_id, error = %e,
                    "lifecycle sweep: master key unrecoverable — user scope skipped (retries next boot)");
                continue;
            }
        };
        let cs =
            ConceptStore::for_user(&state.store, user_id, master).with_provenance(MIGRATION_LABEL);
        migrated += sweep_scope(&cs, &format!("user:{user_id}")).await;
    }
    Ok(migrated)
}

/// One scope's pass: the registry's listing (already path-ascending —
/// the sweep keeps that order), each concept read, rewritten when the
/// predicate matches, put back. Any per-concept failure is logged and
/// the rest of the scope continues.
async fn sweep_scope(cs: &ConceptStore<'_>, scope: &str) -> u32 {
    let entries = match cs.list().await {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(scope = %scope, error = %e,
                "lifecycle sweep: listing failed — scope skipped this boot");
            return 0;
        }
    };
    let mut migrated: u32 = 0;
    for entry in entries {
        let path = entry.path;
        // Queue-internal staging notes are never sweep targets: the
        // mutation queue owns `/mutation-queue/` (a put there would be
        // rejected as QueuePath) and integrates those concepts through
        // its own path.
        if path.starts_with("/mutation-queue/") {
            continue;
        }
        let concept = match cs.get(&path).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(scope = %scope, path = %path, error = %e,
                    "lifecycle sweep: concept read failed — skipped");
                continue;
            }
        };
        let Some(rewritten) = rewrite_if_deprecated(concept) else {
            continue; // predicate miss — the concept is never rewritten
        };
        match cs.put(&rewritten).await {
            Ok(()) => {
                migrated += 1;
                tracing::info!(scope = %scope, path = %path,
                    "lifecycle sweep: migrated deprecated-tagged concept to status: retired");
            }
            Err(e) => {
                tracing::warn!(scope = %scope, path = %path, error = %e,
                    "lifecycle sweep: concept rewrite failed — skipped, scope continues");
            }
        }
    }
    migrated
}

#[cfg(test)]
mod tests {
    use super::*;
    use mycelium_auth::login::LoginService;
    use mycelium_core::concept::Frontmatter;
    use mycelium_store::Store;

    // ---- the four spec tests (written FIRST — TDD) ----

    #[test]
    fn rewrite_if_deprecated_migrates_and_stamps() {
        let migrated =
            rewrite_if_deprecated(concept("/notes/legacy.md", vec!["deprecated", "auth"]))
                .expect("a concept carrying the deprecated tag is migrated");
        // The tag is gone, the rest keep their order.
        assert_eq!(migrated.frontmatter.tags, vec!["auth".to_string()]);
        assert_eq!(migrated.frontmatter.status.as_deref(), Some("retired"));
        assert_eq!(migrated.frontmatter.source.as_deref(), Some("migration"));
        let confirmed = migrated
            .frontmatter
            .confirmed_at
            .clone()
            .expect("confirmed_at stamped");
        assert!(
            chrono::DateTime::parse_from_rfc3339(&confirmed).is_ok(),
            "confirmed_at must be an RFC 3339 timestamp, got: {confirmed}"
        );
        // The stamped frontmatter passes the write-time guard the put
        // path runs (`status: retired` is a valid enum value).
        assert!(
            migrated.frontmatter.validated().is_ok(),
            "migrated frontmatter must pass Frontmatter::validated"
        );
        // Content untouched.
        assert_eq!(migrated.body, "body");
        assert_eq!(migrated.source_path, "/notes/legacy.md");
    }

    #[test]
    fn sweep_matches_case_insensitive_tag() {
        // Mixed-case "Deprecated" matches (trimmed, lowercased).
        let migrated = rewrite_if_deprecated(concept("/x.md", vec!["Deprecated"]))
            .expect("matching is case-insensitive");
        assert!(migrated.frontmatter.tags.is_empty());
        assert_eq!(migrated.frontmatter.status.as_deref(), Some("retired"));
        // Near-miss tags are not the predicate: exact match only, never
        // a substring or prefix match.
        assert!(
            rewrite_if_deprecated(concept("/x.md", vec!["deprecation", "deprecated-x"])).is_none(),
            "near-miss tags must not match"
        );
        // Every occurrence goes: a concept carrying the tag twice (in
        // both cases) rewires to NO tags and still migrates — the
        // rewrite removes all of them, not just the first.
        let both = rewrite_if_deprecated(concept("/x.md", vec!["deprecated", "Deprecated"]))
            .expect("every occurrence of the tag matches");
        assert!(both.frontmatter.tags.is_empty());
        assert_eq!(both.frontmatter.status.as_deref(), Some("retired"));
    }

    #[test]
    fn untouched_concept_passes_through() {
        // No deprecated tag → None: live concepts are never rewritten
        // by this fn (and an empty-tag concept likewise).
        assert!(rewrite_if_deprecated(concept("/notes/live.md", vec!["auth", "rust"])).is_none());
        assert!(rewrite_if_deprecated(concept("/notes/empty-tags.md", vec![])).is_none());
    }

    #[tokio::test]
    async fn sweep_is_idempotent() {
        let (state, _dir, user, master) = boot_state().await;

        // Seed: the brief's user-scope pair (one deprecated-tagged + one
        // clean), a mixed-case tag in the skills shelf, and a
        // deprecated-tagged concept in the LIBRARY scope — the skip is
        // pinned by observing that concept unchanged after every pass.
        let user_cs = ConceptStore::for_user(&state.store, user, master.clone());
        user_cs
            .put(&concept("/notes/legacy.md", vec!["deprecated", "auth"]))
            .await
            .unwrap();
        user_cs
            .put(&concept("/notes/current.md", vec!["rust"]))
            .await
            .unwrap();
        let skills_cs = ConceptStore::for_service(
            &state.store,
            (*state.service_key).clone(),
            &state.store.skills_dir(),
            "skills",
        );
        skills_cs
            .put(&concept("/skills/old-skill.md", vec!["Deprecated"]))
            .await
            .unwrap();
        let library_cs = ConceptStore::for_service(
            &state.store,
            (*state.service_key).clone(),
            &state.store.library_dir(),
            "library",
        );
        library_cs
            .put(&concept("/books/old-book/book.md", vec!["deprecated"]))
            .await
            .unwrap();

        // Pre-sweep baselines: the clean and library concepts must be
        // byte-stable across every pass (the sweep writes nothing for
        // them — a put would re-stamp confirmed_at and change bytes).
        let clean_before = markdown(&user_cs, "/notes/current.md").await;
        let library_before = markdown(&library_cs, "/books/old-book/book.md").await;

        // Run 1: the user's legacy concept + the skills skill migrate.
        let first = migrate_deprecated_tags(&state).await.unwrap();
        assert_eq!(
            first, 2,
            "one user concept + one skills concept migrate; clean + library don't"
        );

        // The migrated concepts carry the stamps (and nothing else).
        let legacy = user_cs.get("/notes/legacy.md").await.unwrap();
        // The deprecated tag is gone, "auth" keeps its position.
        assert_eq!(legacy.frontmatter.tags, vec!["auth".to_string()]);
        assert_eq!(legacy.frontmatter.status.as_deref(), Some("retired"));
        assert_eq!(legacy.frontmatter.source.as_deref(), Some("migration"));
        assert!(legacy.frontmatter.confirmed_at.is_some());
        let legacy_after_first = legacy.to_markdown().unwrap();
        let skill_after_first = markdown(&skills_cs, "/skills/old-skill.md").await;
        let skill = skills_cs.get("/skills/old-skill.md").await.unwrap();
        assert_eq!(skill.frontmatter.status.as_deref(), Some("retired"));
        assert_eq!(skill.frontmatter.source.as_deref(), Some("migration"));

        // The clean concept's frontmatter fields were never touched.
        let clean = user_cs.get("/notes/current.md").await.unwrap();
        assert_eq!(clean.frontmatter.status, None);
        assert_eq!(clean.frontmatter.source, None);
        assert_eq!(clean.frontmatter.tags, vec!["rust".to_string()]);

        // Run 2: the predicate can no longer match — nothing writes.
        let second = migrate_deprecated_tags(&state).await.unwrap();
        assert_eq!(second, 0, "second pass migrates nothing (idempotent)");

        // ...and the migrated concepts' markdown is byte-identical to
        // after run 1 (a put would re-stamp confirmed_at, so equality
        // also proves no write happened).
        assert_eq!(
            markdown(&user_cs, "/notes/legacy.md").await,
            legacy_after_first,
            "run 2 must leave the migrated concept byte-identical"
        );
        assert_eq!(
            markdown(&skills_cs, "/skills/old-skill.md").await,
            skill_after_first,
            "run 2 must leave the migrated skills concept byte-identical"
        );

        // Untouched concepts stay untouched across both passes.
        assert_eq!(markdown(&user_cs, "/notes/current.md").await, clean_before);
        // Library scope skipped by both passes: the deprecated tag
        // survives, status stays unset, bytes unchanged.
        assert_eq!(
            markdown(&library_cs, "/books/old-book/book.md").await,
            library_before,
            "library scope must be skipped by the sweep"
        );
        let book = library_cs.get("/books/old-book/book.md").await.unwrap();
        assert!(book.frontmatter.tags.iter().any(|t| is_deprecated_tag(t)));
        assert_eq!(book.frontmatter.status, None);
    }

    // ---- the harness ----

    /// Boot-shaped harness: a tempdir store + one user whose master
    /// key lives ONLY as the service-key seal row — the exact row the
    /// sweep's `master_key_for` recovery path reads (the cache is
    /// empty at boot, exactly like production). Raw-SQL seed, mirroring
    /// mycelium-mcp/src/lib.rs's MasterKeyCache test. Returns
    /// `(state, dir, user_id, master)`; the caller seeds concepts
    /// through `&state.store` after construction.
    async fn boot_state() -> (AppState, tempfile::TempDir, Uuid, MasterKey) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let service_key =
            mycelium_crypto::load_or_create_service_key_with(dir.path(), None).unwrap();
        let master = mycelium_crypto::generate_master_key();
        let user = uuid::Uuid::new_v4();
        let sealed = mycelium_crypto::aead::aead_seal(
            master.as_bytes(),
            b"mycelium2/seal/service/v1",
            &mycelium_crypto::keys::service_seal_dek(&service_key),
        )
        .unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, email, role, auth_provider,
             password_hash, sealed_master_key, master_key_service_sealed,
             created_at, updated_at)
             VALUES (?, 'lifecycle-sweep-u', 'lifecycle-sweep-u@localhost.local',
             'user', 'local', '', '', ?, ?, ?)",
        )
        .bind(user.to_string())
        .bind(hex::encode(sealed))
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(store.pool())
        .await
        .unwrap();
        let login = LoginService::new(store.pool().clone());
        let state = AppState::new(
            store,
            service_key,
            login,
            dir.path().join("assets"),
            dir.path().to_path_buf(),
            None,
        );
        (state, dir, user, master)
    }

    /// A minimal concept with the given tags (title-free — the path
    /// serves as one; body fixed, the sweep never touches bodies).
    fn concept(path: &str, tags: Vec<&str>) -> Concept {
        Concept::new(
            Frontmatter {
                concept_type: "Note".into(),
                tags: tags.into_iter().map(String::from).collect(),
                ..Default::default()
            },
            "body".into(),
            path.into(),
        )
    }

    /// Read a concept back and re-serialize — the round-tripped
    /// markdown the byte-identical checks compare.
    async fn markdown(cs: &ConceptStore<'_>, path: &str) -> String {
        cs.get(path).await.unwrap().to_markdown().unwrap()
    }
}
