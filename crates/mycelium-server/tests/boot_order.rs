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
