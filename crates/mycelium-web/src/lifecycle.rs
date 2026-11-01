//! Lifecycle boot sweeps (memory-lifecycle spec §3.0 + §3.2a-4):
//!
//! * `migrate_deprecated_tags` — concepts tagged `deprecated` under the
//!   legacy convention are migrated to `status: retired` at boot.
//! * `backfill_index_lengths` — every `search_docs` row whose `length`
//!   is still NULL (all data predating migration 0011) is reindexed.
//!
//! Both are idempotent and predicate-driven — the predicate is a
//! property the migrated/backfilled row no longer carries ("the
//! frontmatter carries a tag lowercasing to `deprecated`" / "`length`
//! is NULL"), so a processed row simply stops matching: no marker row,
//! no flag needed. The deprecated sweep records a `migration`-tagged
//! revision row per rewrite (a failed scope retries next boot for
//! free); the backfill is index-only and writes no revision row at
//! all (§3.2a-4).
//!
//! Scope of `migrate_deprecated_tags`: every user scope (raw
//! `SELECT id FROM users`) plus the `global:skills` service scope.
//! `global:library` is NOT swept by it — generated/book content the
//! ingest worker owns; rewriting it is not that sweep's business (the
//! backfill sweep below is index-only, so it sweeps library too).
//!
//! Posture (mirrors the queue worker's and chat resume's boot sweeps),
//! both sweeps: best-effort, never boot-blocking. Only enumerating the
//! users can hard-fail; a scope whose master key is unrecoverable is
//! logged and skipped (it retries next boot), and a per-concept failure
//! is logged while the rest of that scope completes.

use mycelium_core::concept::Concept;
use mycelium_crypto::keys::MasterKey;
use mycelium_store::Store;
use mycelium_store::concept_store::ConceptStore;
use uuid::Uuid;

use crate::state::AppState;

/// The label the sweep stamps into every rewritten concept (`source`,
/// and the write-side provenance on the ConceptStore).
const MIGRATION_LABEL: &str = "migration";

/// The boot sweeps' shared hard-failure surface (both sweeps use this
/// enum): enumerating the user scopes is the only thing that can fail
/// a sweep outright. Everything else — master-key recovery, the
/// per-scope candidate/listing queries, reindexing, reading and
/// rewriting — is logged and continued so one bad scope or concept
/// cannot stall the boot for the rest.
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
/// `source` + `confirmed_at` on the way to disk, and
/// `.with_revision_op("migration")`, which tags the revision row the
/// write-ahead hook records for the rewrite).
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

/// The deprecated-tag boot sweep (idempotent, predicate-driven — no
/// marker row): every user scope + the global:skills scope; a scope
/// whose master key fails is logged and skipped (retried next boot); a
/// per-concept failure is logged and the rest of the scope continues.
/// Returns the number of concepts migrated across all scopes — the
/// count is for observability only; the sweep never drives boot
/// decisions.
///
/// Scope enumeration + master-key recovery are SHARED with the
/// length-backfill sweep (`sweep_targets`); this sweep's write-side
/// stamps (`with_provenance` + `with_revision_op` = `migration`) are
/// attached per element HERE, at the call site — the shared fn returns
/// stores unstamped. The `global:library` shelf `sweep_targets`
/// returns is deliberately SKIPPED in this loop: this sweep rewrites
/// concepts, and the library shelf is generated/book content the
/// ingest worker owns — never rewritten by lifecycle sweeps (the
/// index-only backfill DOES sweep it).
pub async fn migrate_deprecated_tags(state: &AppState) -> Result<u32, LifecycleError> {
    let mut migrated: u32 = 0;
    for cs in sweep_targets(state, "lifecycle sweep").await? {
        let scope = cs.scope_id();
        if scope == "global:library" {
            continue; // a concept-rewriting sweep — the library shelf stays out
        }
        let cs = cs
            .with_provenance(MIGRATION_LABEL)
            .with_revision_op(MIGRATION_LABEL);
        migrated += sweep_scope(&cs, &scope).await;
    }
    Ok(migrated)
}

/// The lifecycle sweeps' shared scope enumeration: the two service
/// shelves (`global:skills` from the skills dir, `global:library` from
/// the library dir) and every user scope, in that order — skills
/// first, library second, then the users in id order (raw SQL on the
/// pool, the queue-worker sweep's below-auth-layer shape; ORDER BY id
/// keeps user iteration deterministic across boots).
///
/// The user enumeration is the sweeps' one hard-failure surface (this
/// fn's `?`); an unreadable user id or an unrecoverable master key
/// logs one warn and is dropped from the returned set (a failed scope
/// simply retriggers next boot). Each sweep's `label` is prefixed into
/// the message so the shared branches' log lines stay attributable.
///
/// The stores are returned UNSTAMPED (no provenance, no revision op);
/// each sweep attaches what its writes need at its own call site —
/// the deprecated sweep tags its rewrites per element, while the
/// index-only backfill attaches nothing (§3.2a-4: no concept writes
/// exist to stamp).
async fn sweep_targets<'a>(
    state: &'a AppState,
    label: &str,
) -> Result<Vec<ConceptStore<'a>>, LifecycleError> {
    let service_key = (*state.service_key).clone();
    let mut stores = vec![
        ConceptStore::for_service(
            &state.store,
            service_key.clone(),
            &state.store.skills_dir(),
            "skills",
        ),
        ConceptStore::for_service(
            &state.store,
            service_key,
            &state.store.library_dir(),
            "library",
        ),
    ];

    let users: Vec<(String,)> = sqlx::query_as("SELECT id FROM users ORDER BY id")
        .fetch_all(state.store.pool())
        .await?;
    for (id,) in users {
        let user_id: Uuid = match id.parse() {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(user = %id, error = %e,
                    "{label}: user row id unreadable — scope skipped");
                continue;
            }
        };
        // The exact recovery path chat_resume/queue worker use: cache →
        // service-key seal (empty cache at boot → the seal).
        let master: MasterKey = match state.master_key_for(user_id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(user_id = %user_id, error = %e,
                    "{label}: master key unrecoverable — user scope skipped (retries next boot)");
                continue;
            }
        };
        stores.push(ConceptStore::for_user(&state.store, user_id, master));
    }
    Ok(stores)
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

/// The index-length backfill sweep (memory-lifecycle spec §3.2a-4):
/// every `search_docs` row whose `length` is still NULL — all data
/// predating migration 0011 — is reindexed via
/// [`ConceptStore::reindex`], which backfills `length` (the BM25 token
/// count) AND re-applies the `# History` token split to rows indexed
/// by the pre-split code. Index-only by construction (reindex writes
/// no concept file, registry row or revision row), so the deprecated
/// sweep's `global:library` exclusion does not apply — library index
/// rows are swept too — and NO provenance/revision-op builder is
/// attached (`with_provenance`/`with_revision_op` stamp concept
/// writes; there are none here, §3.2a-4).
///
/// Scope iteration is the SHARED `sweep_targets` enumeration (the deprecated
/// sweep's too): both service shelves + every user scope — raw
/// `SELECT id FROM users ORDER BY id`, master-key recovery per user,
/// an unreadable id or unrecoverable key logging one warn and skipping
/// that scope (it retries next boot). Per scope the candidates come
/// from raw pool SQL on the SAME scope string the sweep logs
/// (`cs.scope_id()` — built the same way the encrypted index builds
/// its scope id, so the labels can't drift apart).
///
/// The `/mutation-queue/` prefix is EXCLUDED from the candidates
/// (Task-2 review ruling's controller note): the queue's staging notes
/// ARE index residents (put_batch_internal → add_async reaches
/// search_docs) but are NOT concepts — `reindex` refuses the prefix
/// anyway, and a pre-0011 staging row self-heals: the drain's rewrite
/// re-stamps `length` post-0011, and rows that never drain are deleted
/// by the queue's own lifecycle (§3.2a-4's backfill must not touch
/// non-concept payloads). Residual per-row faults still log + continue
/// as the net.
///
/// Idempotent and predicate-driven — the predicate is `length IS
/// NULL`, so a backfilled row simply stops matching; no marker row or
/// flag needed. Returns the number of rows backfilled across all
/// scopes — observability only; the sweep never drives boot decisions.
pub async fn backfill_index_lengths(state: &AppState) -> Result<u32, LifecycleError> {
    let mut backfilled: u32 = 0;
    // The shared enumeration returns both service shelves (unlike the
    // deprecated-tag sweep's loop, whose library element is SKIPPED —
    // an exclusion that exists only because it rewrites concepts;
    // `reindex` is index-only, so BOTH library namespaces are looped
    // here) and every user scope. NOTHING is attached per element:
    // index-only, no concept writes to stamp (§3.2a-4).
    for cs in sweep_targets(state, "length backfill sweep").await? {
        backfilled += backfill_scope(&state.store, &cs).await;
    }
    Ok(backfilled)
}

/// One scope's pass of the length backfill: the scope's NULL-length
/// candidates (raw pool SQL, path-ascending), each reindexed. A
/// candidate-query failure is a scope fault (logged, scope skipped —
/// the same posture `sweep_scope` gives a listing failure); a
/// per-concept reindex fault is logged and the rest of the scope
/// continues. `cs.scope_id()` supplies the `search_docs.scope` string
/// AND the log label — one string, no drift.
///
/// The exclusion literal carries no `_`/`%` wildcard characters
/// ("mutation-queue" is hyphenated), so it is a faithful LIKE pattern
/// needing no ESCAPE clause.
async fn backfill_scope(store: &Store, cs: &ConceptStore<'_>) -> u32 {
    let scope = cs.scope_id();
    let candidates: Result<Vec<(String,)>, sqlx::Error> = sqlx::query_as(
        "SELECT concept_path FROM search_docs \
         WHERE scope = ? AND length IS NULL \
           AND concept_path NOT LIKE '/mutation-queue/%' \
         ORDER BY concept_path",
    )
    .bind(&scope)
    .fetch_all(store.pool())
    .await;
    let candidates = match candidates {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(scope = %scope, error = %e,
                "length backfill sweep: candidate query failed — scope skipped this boot");
            return 0;
        }
    };
    let mut backfilled: u32 = 0;
    for (path,) in candidates {
        match cs.reindex(&path).await {
            Ok(()) => {
                backfilled += 1;
                // Per-row visibility at DEBUG — the backfill is
                // bulk-cardinality; the aggregate lands at info via
                // main.rs's `Ok(n) if n > 0` arm.
                tracing::debug!(scope = %scope, path = %path,
                    "length backfill sweep: reindexed NULL-length index row");
            }
            Err(e) => {
                tracing::warn!(scope = %scope, path = %path, error = %e,
                    "length backfill sweep: reindex failed — skipped, scope continues");
            }
        }
    }
    backfilled
}

#[cfg(test)]
mod tests {
    use super::*;
    use mycelium_auth::login::LoginService;
    use mycelium_core::concept::Frontmatter;
    use mycelium_core::search::{searchable_text, tokenize};
    use mycelium_crypto::store_keys::IndexKeys;
    use mycelium_store::{RevisionEntry, RevisionStore, Store};

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

        // The revision chains (Task 6 wiring): the sweep's rewrite puts
        // go through stores built `.with_revision_op("migration")`, so
        // each migrated concept gains a `migration` row ON TOP of its
        // seed `create` row — never the mechanically derived `update`
        // (`list` is newest-first).
        let user_rs = RevisionStore::for_user(state.store.pool(), user, &master);
        let skills_rs =
            RevisionStore::for_service(state.store.pool(), &state.service_key, "skills");
        let legacy_revs = user_rs.list("/notes/legacy.md").await.unwrap();
        assert_eq!(
            ops(&legacy_revs),
            vec!["migration", "create"],
            "the sweep's rewrite lands a migration row above the seed create"
        );
        assert_eq!(
            legacy_revs[0].summary, "deprecated tag migrated to status: retired",
            "the migration row carries its op summary"
        );
        assert_eq!(
            ops(&skills_rs.list("/skills/old-skill.md").await.unwrap()),
            vec!["migration", "create"],
            "the skills scope's rewrite is tagged too"
        );
        // A concept the sweep never writes keeps its seed chain only.
        assert_eq!(
            ops(&user_rs.list("/notes/current.md").await.unwrap()),
            vec!["create"],
            "an untouched concept gains no revision row"
        );

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
        // ...and run 2 writes NO revision row either — the predicate
        // miss means no put, and no put means no undo-log entry.
        assert_eq!(
            user_rs.list("/notes/legacy.md").await.unwrap().len(),
            2,
            "the second sweep adds no revision row"
        );
        assert_eq!(
            skills_rs.list("/skills/old-skill.md").await.unwrap().len(),
            2,
            "the second sweep adds no revision row (skills scope)"
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

    // ---- the backfill sweep's three spec tests (written FIRST — TDD) ----

    #[tokio::test]
    async fn backfill_index_lengths_refills_and_retokens() {
        let (state, _dir, user, master) = boot_state().await;

        // Seed: a user concept + a skills concept, both with a
        // `# History` body — the §3.2a-4 retokenization reapplication
        // needs that exact shape (the history text must NOT be
        // searchable after the backfill reindex).
        let body = "current body facts\n\n# History\nzebracorn obsolete\n";
        let user_cs = ConceptStore::for_user(&state.store, user, master.clone());
        let user_c = history_concept("/notes/legacy.md", body);
        user_cs.put(&user_c).await.unwrap();
        let skills_cs = ConceptStore::for_service(
            &state.store,
            (*state.service_key).clone(),
            &state.store.skills_dir(),
            "skills",
        );
        let skills_c = history_concept("/skills/legacy-skill.md", body);
        skills_cs.put(&skills_c).await.unwrap();
        let skills_keys = IndexKeys::from_service_key(&state.service_key);

        // Simulate the pre-0011 index rows: NULL lengths AND — to make
        // the retokenization observable at all — the stray `# History`
        // posting a pre-split row carried (a post-step-1 put does not
        // index history text; the concept_store.rs reindex test's
        // legacy simulation).
        {
            let pool = state.store.pool();
            let user_scope = user_cs.scope_id();
            let skills_scope = skills_cs.scope_id();
            sqlx::query(
                "UPDATE search_docs SET length = NULL WHERE concept_path = ? OR concept_path = ?",
            )
            .bind("/notes/legacy.md")
            .bind("/skills/legacy-skill.md")
            .execute(pool)
            .await
            .unwrap();
            for (scope, path, token) in [
                (
                    &user_scope,
                    "/notes/legacy.md",
                    IndexKeys::from_master_key(&master).token("zebracorn"),
                ),
                (
                    &skills_scope,
                    "/skills/legacy-skill.md",
                    skills_keys.token("zebracorn"),
                ),
            ] {
                sqlx::query(
                    "INSERT INTO search_tokens (scope, token, concept_path, tf) VALUES (?, ?, ?, 1)",
                )
                .bind(scope)
                .bind(token)
                .bind(path)
                .execute(pool)
                .await
                .unwrap();
            }
        }

        // Pre-sweep assertions for BOTH seeded scopes (user + skills):
        // each legacy row is NULL-length AND matched by the planted
        // history token — proving the pre-state, so the post-sweep
        // "no longer matches" is non-vacuous.
        let user_scope = user_cs.scope_id();
        let skills_scope = skills_cs.scope_id();
        let pool = state.store.pool();
        assert!(
            stored_length(pool, &user_scope, "/notes/legacy.md")
                .await
                .is_none(),
            "user legacy row starts with NULL length"
        );
        assert!(
            stored_length(pool, &skills_scope, "/skills/legacy-skill.md")
                .await
                .is_none(),
            "skills legacy row starts with NULL length"
        );
        let pre_user = search_paths(&user_cs, "zebracorn").await;
        assert_eq!(
            pre_user,
            vec!["/notes/legacy.md".to_string()],
            "pre-backfill, the stray history token matches on the user scope"
        );
        let pre_skills = search_paths(&skills_cs, "zebracorn").await;
        assert_eq!(
            pre_skills,
            vec!["/skills/legacy-skill.md".to_string()],
            "pre-backfill, the stray history token matches on the skills scope"
        );

        // The sweep.
        let count = backfill_index_lengths(&state).await.unwrap();
        assert_eq!(
            count, 2,
            "both seeded legacy rows are backfilled, one per scope"
        );

        // Lengths restored to the SAME computation the index stamps at
        // add (never hand-waved).
        let want_user = tokenize(&searchable_text(&user_c)).len() as i64;
        assert!(want_user > 0, "the live text tokenizes to > 0 tokens");
        assert_eq!(
            stored_length(pool, &user_scope, "/notes/legacy.md").await,
            Some(want_user),
            "user legacy row's length is backfilled"
        );
        let want_skills = tokenize(&searchable_text(&skills_c)).len() as i64;
        assert_eq!(
            stored_length(pool, &skills_scope, "/skills/legacy-skill.md").await,
            Some(want_skills),
            "skills legacy row's length is backfilled"
        );

        // The `# History` split is re-applied on BOTH scopes: the stray
        // zebracorn postings are gone; the live text still matches.
        assert!(
            search_paths(&user_cs, "zebracorn").await.is_empty(),
            "the user scope's history token must stop matching"
        );
        assert!(
            search_paths(&skills_cs, "zebracorn").await.is_empty(),
            "the skills scope's history token must stop matching"
        );
        assert_eq!(
            search_paths(&user_cs, "current").await,
            vec!["/notes/legacy.md".to_string()],
            "the live text is still searchable (reindex re-added it)"
        );
    }

    #[tokio::test]
    async fn backfill_is_idempotent() {
        let (state, _dir, user, master) = boot_state().await;

        // One legacy user row (NULL length + stray history posting).
        let user_cs = ConceptStore::for_user(&state.store, user, master.clone());
        let c = history_concept(
            "/notes/legacy.md",
            "current body facts\n\n# History\nzebracorn",
        );
        user_cs.put(&c).await.unwrap();
        let scope = user_cs.scope_id();
        let pool = state.store.pool();
        sqlx::query("UPDATE search_docs SET length = NULL WHERE concept_path = ?")
            .bind("/notes/legacy.md")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO search_tokens (scope, token, concept_path, tf) VALUES (?, ?, ?, 1)",
        )
        .bind(&scope)
        .bind(IndexKeys::from_master_key(&master).token("zebracorn"))
        .bind("/notes/legacy.md")
        .execute(pool)
        .await
        .unwrap();

        // Run 1: backfills the row (count 1) and the predicate empties
        // DB-wide.
        let first = backfill_index_lengths(&state).await.unwrap();
        assert_eq!(first, 1, "the seeded legacy row is backfilled");
        let nulls_after_first: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM search_docs WHERE length IS NULL")
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(
            nulls_after_first, 0,
            "the predicate is empty after the first run"
        );
        let length_after_first = stored_length(pool, &scope, "/notes/legacy.md").await;
        assert!(
            length_after_first.is_some(),
            "the row's length is backfilled by the first run"
        );

        // Run 2: nothing matches the predicate — zero backfilled, and
        // the lengths are stable (byte-for-byte the same column value;
        // a second re-add would ALSO rewrite the same value, but the
        // zero count is what proves the predicate miss, not the value).
        let second = backfill_index_lengths(&state).await.unwrap();
        assert_eq!(second, 0, "second pass backfills nothing (idempotent)");
        let nulls_after_second: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM search_docs WHERE length IS NULL")
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(
            nulls_after_second, 0,
            "the predicate stays empty after the second run"
        );
        assert_eq!(
            stored_length(pool, &scope, "/notes/legacy.md").await,
            length_after_first,
            "the backfilled length is stable across runs"
        );
        // ...and the stray history token stays gone (nothing re-added
        // it — the second run never even reads the concept).
        let stray: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM search_tokens WHERE scope = ? AND token = ? AND concept_path = ?",
        )
        .bind(&scope)
        .bind(IndexKeys::from_master_key(&master).token("zebracorn"))
        .bind("/notes/legacy.md")
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(stray, 0, "the planted history token stays gone after run 2");
    }

    #[tokio::test]
    async fn backfill_covers_library_scope() {
        let (state, _dir, _user, _master) = boot_state().await;

        // Seed a library-scope concept the same way (the deprecated-tag
        // sweep EXCLUDES this scope — but reindex is index-only, so the
        // backfill must NOT: §3.2a-4 sweeps library index rows too).
        let body = "current body facts\n\n# History\nzebracorn obsolete\n";
        let library_cs = ConceptStore::for_service(
            &state.store,
            (*state.service_key).clone(),
            &state.store.library_dir(),
            "library",
        );
        let c = history_concept("/books/old-book/book.md", body);
        library_cs.put(&c).await.unwrap();
        let scope = library_cs.scope_id();
        let pool = state.store.pool();
        sqlx::query("UPDATE search_docs SET length = NULL WHERE concept_path = ?")
            .bind("/books/old-book/book.md")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO search_tokens (scope, token, concept_path, tf) VALUES (?, ?, ?, 1)",
        )
        .bind(&scope)
        .bind(IndexKeys::from_service_key(&state.service_key).token("zebracorn"))
        .bind("/books/old-book/book.md")
        .execute(pool)
        .await
        .unwrap();

        // Pre-sweep: NULL-length AND matched by the planted history
        // token.
        assert!(
            stored_length(pool, &scope, "/books/old-book/book.md")
                .await
                .is_none(),
            "library legacy row starts with NULL length"
        );
        assert_eq!(
            search_paths(&library_cs, "zebracorn").await,
            vec!["/books/old-book/book.md".to_string()],
            "pre-backfill, the stray history token matches on the library scope"
        );

        // The sweep: the library row is reindexed (count 1) — proving
        // the library inclusion.
        let count = backfill_index_lengths(&state).await.unwrap();
        assert_eq!(count, 1, "the library scope's legacy row is backfilled");

        // Its NULL length refilled; the history token gone.
        let want = tokenize(&searchable_text(&c)).len() as i64;
        assert_eq!(
            stored_length(pool, &scope, "/books/old-book/book.md").await,
            Some(want),
            "library legacy row's length is backfilled"
        );
        assert!(
            search_paths(&library_cs, "zebracorn").await.is_empty(),
            "the library scope's history token must stop matching"
        );
    }

    #[tokio::test]
    async fn backfill_continues_past_a_concept_read_fault() {
        let (state, _dir, user, master) = boot_state().await;

        // Seed one GOOD user row (a real concept, put normally) ...
        let user_cs = ConceptStore::for_user(&state.store, user, master.clone());
        let c = history_concept("/notes/good.md", "good body facts\n\n# History\nzebracorn");
        user_cs.put(&c).await.unwrap();
        let scope = user_cs.scope_id();
        let pool = state.store.pool();
        sqlx::query("UPDATE search_docs SET length = NULL WHERE concept_path = ?")
            .bind("/notes/good.md")
            .execute(pool)
            .await
            .unwrap();
        // ...next to a DEAD row: an index resident whose concept file
        // never lands (the per-concept fault shape — reindex's `get`
        // fails NotFound). The payload blob is filler; the sweep must
        // not touch the row (reindex faults before any payload work).
        sqlx::query(
            "INSERT INTO search_docs (scope, concept_path, payload, length, lifecycle_status) \
             VALUES (?, '/notes/dead.md', x'', NULL, NULL)",
        )
        .bind(&scope)
        .execute(pool)
        .await
        .unwrap();

        // Pre-sweep: both candidates present, both NULL.
        assert_eq!(
            stored_length(pool, &scope, "/notes/good.md").await,
            None,
            "the good row is a candidate (NULL length)"
        );
        assert_eq!(
            stored_length(pool, &scope, "/notes/dead.md").await,
            None,
            "the dead row is a candidate (NULL length)"
        );

        // The sweep returns Ok: the dead row is a per-concept fault
        // (logged, the pass continues); the good row still backfills.
        let count = backfill_index_lengths(&state).await.unwrap();
        assert_eq!(
            count, 1,
            "the count is the GOOD rows only — a fault is not a count"
        );
        assert_eq!(
            stored_length(pool, &scope, "/notes/good.md").await,
            Some(tokenize(&searchable_text(&c)).len() as i64),
            "the good row's length is backfilled"
        );
        assert_eq!(
            stored_length(pool, &scope, "/notes/dead.md").await,
            None,
            "the dead row stays NULL — the fault reindexed nothing"
        );
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

    /// The op tags of a revision list, in list order (newest first).
    fn ops(revs: &[RevisionEntry]) -> Vec<&str> {
        revs.iter().map(|r| r.op.as_str()).collect()
    }

    /// A concept whose body carries a `# History` section — the exact
    /// shape the backfill's retokenization reapplication needs (the
    /// history text must drop out of the index on re-add).
    fn history_concept(path: &str, body: &str) -> Concept {
        Concept::new(
            Frontmatter {
                concept_type: "Note".into(),
                ..Default::default()
            },
            body.into(),
            path.into(),
        )
    }

    /// A scope row's `search_docs.length` (None = NULL).
    async fn stored_length(pool: &sqlx::SqlitePool, scope: &str, path: &str) -> Option<i64> {
        sqlx::query_scalar("SELECT length FROM search_docs WHERE scope = ? AND concept_path = ?")
            .bind(scope)
            .bind(path)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Search one scope for a single term; the hit paths, list order.
    async fn search_paths(cs: &ConceptStore<'_>, term: &str) -> Vec<String> {
        cs.search(&mycelium_core::search::SearchQuery::new(vec![
            term.to_string(),
        ]))
        .await
        .unwrap()
        .into_iter()
        .map(|hit| hit.concept_path)
        .collect()
    }
}
