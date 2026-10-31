//! Mycelium2 server binary.

use clap::Parser;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "mycelium2")]
#[command(about = "Mycelium2 knowledge base server")]
struct Args {
    /// Path to the data directory.
    #[arg(
        long,
        env = "MYCELIUM2_DATA_DIR",
        default_value = "/opt/mycelium2/data"
    )]
    data_dir: String,

    /// HTTPS listen address.
    #[arg(long, env = "MYCELIUM2_HTTPS_ADDR", default_value = "0.0.0.0:443")]
    https_addr: String,

    /// HTTP redirect listen address.
    #[arg(long, env = "MYCELIUM2_HTTP_ADDR", default_value = "0.0.0.0:80")]
    http_addr: String,

    /// Admin-supplied TLS cert (PEM). Auto self-signed when absent.
    #[arg(long, env = "MYCELIUM2_TLS_CERT")]
    tls_cert: Option<String>,

    /// Admin-supplied TLS key (PEM). Auto self-signed when absent.
    #[arg(long, env = "MYCELIUM2_TLS_KEY")]
    tls_key: Option<String>,

    /// SANs (comma-separated) for the generated self-signed cert.
    /// Unset → `localhost` only (the default). A set list is taken
    /// verbatim — no forced additions. Regenerates the managed pair
    /// when the set changes.
    #[arg(long, env = "MYCELIUM2_TLS_SANS")]
    tls_sans: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();

    // Structured JSON logs (DESIGN decision).
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let data_dir = std::path::PathBuf::from(&args.data_dir);
    tracing::info!(data_dir = %args.data_dir, "starting Mycelium2");

    let tls_sans = mycelium_web::tls_admin::sans_from_env(args.tls_sans.as_deref());
    tracing::info!(sans = ?tls_sans, "TLS SAN configuration");

    // Open the store (creates layout + runs migrations).
    let store = mycelium_store::Store::open(&data_dir).await?;

    // Load the service key (env override or 0600 file).
    let service_key = mycelium_crypto::load_or_create_service_key(&data_dir)?;

    // First-run guidance: no users exist yet — the admin account is
    // created through the web /setup page (no bootstrap files are ever
    // written).
    let users = mycelium_auth::UserStore::new(store.pool().clone());
    match users.count().await {
        Ok(0) => tracing::info!(
            "no users yet — open https://<host>/setup in a browser to create the admin account"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "user count check failed"),
    }

    // Scaffold default assets on first boot.
    let assets_dir = data_dir.join("assets");
    mycelium_web::assets::scaffold_defaults(&assets_dir)?;

    // Revision retention before ANY concept-writing step (memory-lifecycle
    // spec §3.2a-4): the packaged-skill seed and every boot sweep below
    // write concept revisions, so the stored `lifecycle` row must be live
    // before them. Same boot-order discipline as the LLM gate sync below:
    // the gate precedes the sweeps, retention precedes the seed.
    mycelium_store::revisions::sync_lifecycle_from_store(
        &mycelium_store::ConfigStore::new(store.pool().clone()),
        &service_key,
    )
    .await?;

    // Seed the packaged skills (pdf- and ebook-to-markdown)
    // into the global skills shelf — first boot writes them; later boots
    // refresh on a version bump (mirrors the assets scaffold).
    mycelium_web::packaged_skills::seed_packaged_skills(&store, &service_key, &store.skills_dir())
        .await?;

    // Assemble the app state.
    let login = mycelium_auth::login::LoginService::new(store.pool().clone());
    // `tls: None` here — serve() loads the pair and stashes the live
    // handle into the state it is given.
    let state = mycelium_web::AppState::new(
        store,
        service_key,
        login,
        assets_dir,
        data_dir.clone(),
        None,
    );

    // LLM admission gate first (LLM concurrency spec §3.2): every boot
    // sweep and worker below admits on the process-global gate — the
    // mutation-queue drain pass runs inline right here, and both
    // long-lived workers tick immediately on spawn. Syncing the stored
    // `max_parallel` before any of them is what keeps that boot burst
    // inside the operator's capacity instead of the built-in default.
    mycelium_web::server::sync_llm_gate_from_store(&state).await;

    // Librarian boot sweep: fail jobs interrupted by a previous
    // shutdown, requeue pending ones.
    match state.librarian.recover_on_boot().await {
        Ok(requeued) if !requeued.is_empty() => {
            tracing::info!(count = requeued.len(), "requeued pending ingest jobs");
            // Run them now (best-effort; failures are recorded per job).
            if let Err(e) = state.librarian.run_pending().await {
                tracing::error!(error = %e, "ingest requeue run failed");
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "librarian boot sweep failed"),
    }

    // Mutation-queue boot sweep: reconcile interrupted runs + staging
    // rows, then one drain pass over whatever survived (spec §6).
    match state.queue_worker.recover_on_boot().await {
        Ok(()) => {}
        Err(e) => tracing::warn!(error = %e, "queue boot sweep failed"),
    }
    if let Err(e) = state.queue_worker.run_pending().await {
        tracing::error!(error = %e, "initial queue drain failed");
    }

    // Lifecycle boot sweep (memory-lifecycle spec §3.0): legacy
    // `deprecated` tags → `status: retired`, across every user scope +
    // the global skills shelf (library is never swept). Idempotent,
    // predicate-driven — a scope whose key fails this boot simply
    // retriggers next boot.
    match mycelium_web::lifecycle::migrate_deprecated_tags(&state).await {
        Ok(n) if n > 0 => {
            tracing::info!(
                count = n,
                "migrated deprecated-tagged concepts to status: retired"
            )
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "lifecycle boot sweep failed"),
    }

    let https_addr: SocketAddr = args.https_addr.parse()?;
    let http_addr: SocketAddr = args.http_addr.parse()?;
    let shutdown = tokio_util::sync::CancellationToken::new();

    // Graceful shutdown on SIGTERM (docker stop) and SIGINT (ctrl-c).
    let token = shutdown.clone();
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut sig) => {
                    sig.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();
        tokio::select! {
            _ = ctrl_c => {}
            _ = terminate => {}
        }
        tracing::info!("shutdown signal received");
        token.cancel();
    });

    // Drain worker: long task, cancelled by the same shutdown token.
    let worker = state.queue_worker.clone();
    worker.spawn(shutdown.clone());

    // Ingest sweep (outage-recovery spec §3.5): re-runs pending ingest
    // jobs on a signal and a 60 s tick, so a failed boot pass above is
    // not terminal for the queue. The boot block stays for immediacy.
    state.librarian.clone().spawn(shutdown.clone());

    // Chat boot sweep + resume worker (chat session history spec §3.4):
    // running → interrupted; the worker auto-resumes each once.
    if let Err(e) = state.chat_sessions.boot_sweep().await {
        tracing::warn!(error = %e, "chat boot sweep failed");
    }
    let resume = Arc::new(mycelium_web::chat_resume::ChatResumeWorker::new(Arc::new(
        state.clone(),
    )));
    resume.spawn(shutdown.clone());

    // Backend health (outage recovery spec §3.1): seed Disabled until
    // the probe's first tick resolves it — the static machine starts
    // "configured, fresh", which would otherwise read Green to a
    // request landing before the task's first pass. The probe loop's
    // immediate first tick lifts the pin (config row present) and
    // probes, or re-pins it (row absent).
    mycelium_librarian::llm_health::LLM_HEALTH.set_disabled();
    mycelium_web::server::spawn_backend_probe(
        state.clone(),
        shutdown.clone(),
        std::time::Duration::from_secs(mycelium_librarian::llm_health::PROBE_INTERVAL_SECS),
    )
    .await;

    mycelium_web::serve(
        state,
        &data_dir,
        https_addr,
        http_addr,
        args.tls_cert.as_deref().map(std::path::Path::new),
        args.tls_key.as_deref().map(std::path::Path::new),
        tls_sans,
        shutdown,
    )
    .await?;

    tracing::info!("server stopped");
    Ok(())
}
