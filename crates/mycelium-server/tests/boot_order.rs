//! Standing mechanical guard on the binary's boot ORDER (LLM
//! concurrency spec §3.2).
//!
//! The mutation-queue boot drain pass runs inline in `main`, and both
//! long-lived workers (the drain worker and the chat resume worker)
//! admit try-class the moment they are spawned — a `tokio::time::
//! interval`'s first tick fires immediately. So the gate must be synced
//! from the stored `"llm"` row BEFORE any of them, or that boot burst
//! runs against `DEFAULT_CAPACITY` instead of the operator's
//! `max_parallel`.
//!
//! The memory-lifecycle spec §3.2a-4 adds a second guard on the same
//! principle: revision retention must be synced from the stored
//! `lifecycle` row BEFORE the packaged-skill seed, because the seed
//! writes concepts (and revisions) — retention must be live first.
//!
//! Task 4 of the same spec adds a third: the embeddings posture is
//! seeded (Disabled — an absent `embeddings` row must not boot as a
//! live Green) and synced from the stored row BEFORE the vector worker
//! and the embeddings probe spawn, both of which observe the machine on
//! their first immediate tick.
//!
//! This is asserted over `src/main.rs`'s source rather than by timing:
//! the ordering is a property of the boot sequence, and a runtime probe
//! of it would be a race.

/// `main.rs`, read from the crate root at test time.
fn main_rs() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The byte offset of `needle`'s single occurrence in `src`.
fn only_at(src: &str, needle: &str) -> usize {
    let first = src
        .find(needle)
        .unwrap_or_else(|| panic!("main.rs no longer contains `{needle}`"));
    assert!(
        src[first + needle.len()..].find(needle).is_none(),
        "`{needle}` appears more than once in main.rs — the order guard needs one site"
    );
    first
}

#[test]
fn llm_gate_is_synced_before_the_boot_sweeps_and_workers() {
    let src = main_rs();
    let sync = only_at(&src, "sync_llm_gate_from_store(&state)");
    // The inline boot drain pass (try-class admission).
    let drain_pass = only_at(&src, "state.queue_worker.run_pending()");
    // The long-lived drain worker and the chat resume worker: both tick
    // immediately once spawned.
    let drain_worker = only_at(&src, "worker.spawn(shutdown.clone())");
    let resume_worker = only_at(&src, "resume.spawn(shutdown.clone())");
    // The listener itself (serve syncs again, idempotently).
    let serve = only_at(&src, "mycelium_web::serve(");

    for (what, at) in [
        ("the inline queue drain pass", drain_pass),
        ("the drain worker spawn", drain_worker),
        ("the chat resume worker spawn", resume_worker),
        ("serve()", serve),
    ] {
        assert!(
            sync < at,
            "the LLM gate sync must come before {what} in main.rs (sync at {sync}, {what} at {at})"
        );
    }
}

#[test]
fn lifecycle_retention_is_synced_before_the_packaged_skill_seed() {
    let src = main_rs();
    // Retention sync (memory-lifecycle spec §3.2a-4).
    let sync = only_at(&src, "sync_lifecycle_from_store(");
    // The seed writes concepts (and revisions) — retention must be live.
    let seed = only_at(&src, "seed_packaged_skills(");

    assert!(
        sync < seed,
        "the lifecycle sync must come before the packaged-skill seed in main.rs \
         (sync at {sync}, seed at {seed})"
    );
}

/// The length-backfill sweep (memory-lifecycle spec §3.2a-4, Task 5)
/// sits in the same lifecycle-sweep block as the deprecated-tag sweep:
/// after the retention sync (the uniform boot discipline — every
/// post-sync boot step sees live config) and after the deprecated-tag
/// sweep (main.rs orders the two lifecycle sweeps back-to-back; the
/// backfill is index-only, so it neither writes revisions nor needs to
/// precede anything, but it must not drift ahead of its block).
#[test]
fn backfill_sweep_follows_retention_sync_and_the_deprecated_tag_sweep() {
    let src = main_rs();
    let sync = only_at(&src, "sync_lifecycle_from_store(");
    // The deprecated-tag sweep (the first lifecycle sweep).
    let deprecated = only_at(&src, "migrate_deprecated_tags(&state)");
    // The length backfill sweep (the second lifecycle sweep).
    let backfill = only_at(&src, "backfill_index_lengths(&state)");

    assert!(
        sync < backfill,
        "the backfill sweep must come after the lifecycle retention sync in main.rs \
         (sync at {sync}, backfill at {backfill})"
    );
    assert!(
        deprecated < backfill,
        "the backfill sweep must come after the deprecated-tag sweep in main.rs \
         (deprecated at {deprecated}, backfill at {backfill})"
    );
}

/// The embeddings posture is seeded (Disabled) and synced from the
/// stored `embeddings` row BEFORE the vector worker and the embeddings
/// probe spawn (both observe the machine on their first immediate
/// tick); the vector worker's spawn sits with the other long-lived
/// workers, and the embeddings probe spawn directly follows the LLM
/// probe's.
#[test]
fn embedding_health_is_synced_before_the_vector_workers() {
    let src = main_rs();
    // The seed + the sync.
    let seed = only_at(&src, "EMBEDDING_HEALTH.set_disabled()");
    let sync = only_at(&src, "sync_embedding_from_store(&state)");
    // The long-lived vector worker's spawn.
    let worker = only_at(&src, "state.vector_worker.clone().spawn(shutdown.clone())");
    // The embeddings probe spawn.
    let probe = only_at(&src, "spawn_embedding_probe(");
    let serve = only_at(&src, "mycelium_web::serve(");

    for (what, at) in [
        ("the vector worker spawn", worker),
        ("the embeddings probe spawn", probe),
        ("serve()", serve),
    ] {
        assert!(
            seed < at,
            "the embeddings seed must come before {what} in main.rs \
             (seed at {seed}, {what} at {at})"
        );
        assert!(
            sync < at,
            "the embeddings sync must come before {what} in main.rs \
             (sync at {sync}, {what} at {at})"
        );
    }
    assert!(
        worker < probe,
        "the vector worker's spawn must precede the probe task's in main.rs \
         (worker at {worker}, probe at {probe})"
    );
    assert!(
        probe < serve,
        "the embeddings probe must spawn before serve() in main.rs \
         (probe at {probe}, serve at {serve})"
    );
    assert!(
        worker < serve,
        "the vector worker must spawn before serve() in main.rs \
         (worker at {worker}, serve at {serve})"
    );
}
