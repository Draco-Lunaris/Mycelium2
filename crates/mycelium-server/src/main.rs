//! Mycelium2 server binary.

use clap::Parser;
use std::net::SocketAddr;

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

    // Open the store (creates layout + runs migrations).
    let store = mycelium_store::Store::open(&data_dir).await?;

    // Load the service key (env override or 0600 file).
    let service_key = mycelium_crypto::load_or_create_service_key(&data_dir)?;

    // Bootstrap the first admin (no-op when users exist).
    if let Some(admin) = mycelium_auth::bootstrap_admin(&store).await? {
        tracing::info!(username = %admin.username, "bootstrapped initial admin");
        tracing::info!(
            "initial password written to {} (0600) — change it on first login",
            data_dir.join("config/initial-admin-password").display()
        );
    }

    // Scaffold default assets on first boot.
    let assets_dir = data_dir.join("assets");
    mycelium_web::assets::scaffold_defaults(&assets_dir)?;

    // Assemble the app state.
    let login = mycelium_auth::login::LoginService::new(store.pool().clone());
    let state = mycelium_web::AppState::new(store, service_key, login, assets_dir);

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

    let https_addr: SocketAddr = args.https_addr.parse()?;
    let http_addr: SocketAddr = args.http_addr.parse()?;
    let shutdown = tokio_util::sync::CancellationToken::new();

    // Graceful shutdown on SIGTERM/SIGINT.
    let token = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutdown signal received");
        token.cancel();
    });

    mycelium_web::serve(
        state,
        &data_dir,
        https_addr,
        http_addr,
        args.tls_cert.as_deref().map(std::path::Path::new),
        args.tls_key.as_deref().map(std::path::Path::new),
        shutdown,
    )
    .await?;

    tracing::info!("server stopped");
    Ok(())
}
