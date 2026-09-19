//! Mycelium2 admin CLI.
//!
//! Offline administrative operations against a data directory:
//! `migrate` (run SQL migrations), `backup` (tar the data directory),
//! and `verify` (integrity check of the store layout). The admin
//! account is created only through the web /setup page.

use clap::{Parser, Subcommand};
use std::path::Path;

#[derive(Parser, Debug)]
#[command(name = "mycelium2-cli")]
#[command(about = "Mycelium2 administrative CLI")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run database migrations (idempotent; the server also does this
    /// on startup).
    Migrate {
        #[arg(
            long,
            env = "MYCELIUM2_DATA_DIR",
            default_value = "/opt/mycelium2/data"
        )]
        data_dir: String,
    },
    /// Write a full backup of the data directory as a .tar.gz.
    /// For a guaranteed-consistent snapshot, stop the server first.
    Backup {
        #[arg(
            long,
            env = "MYCELIUM2_DATA_DIR",
            default_value = "/opt/mycelium2/data"
        )]
        data_dir: String,
        /// Output file (default: <data_dir>/backup-<timestamp>.tar.gz).
        #[arg(long)]
        out: Option<String>,
    },
    /// Verify the data-directory layout and database integrity.
    Verify {
        #[arg(
            long,
            env = "MYCELIUM2_DATA_DIR",
            default_value = "/opt/mycelium2/data"
        )]
        data_dir: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Migrate { data_dir } => {
            // Store::open runs migrations as part of opening.
            let _store = mycelium_store::Store::open(Path::new(&data_dir)).await?;
            println!("migrations complete");
        }
        Command::Backup { data_dir, out } => {
            // Default output goes OUTSIDE the data dir: the archive
            // walks the whole data directory, so a default inside it
            // would recursively embed previous backups (unbounded
            // growth). Sibling of the data dir instead.
            let out = out.unwrap_or_else(|| {
                let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                let dir = Path::new(&data_dir);
                match dir.parent() {
                    Some(parent) => format!("{}/backup-{}.tar.gz", parent.display(), ts),
                    None => format!("backup-{ts}.tar.gz"),
                }
            });
            let bytes = mycelium_store::backup::backup_tar_gz(Path::new(&data_dir))?;
            // The archive contains the service key + sealed master
            // keys — write it 0600, never world-readable.
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&out)?;
            std::io::Write::write_all(&mut file, &bytes)?;
            println!("backup written to {out}");
        }
        Command::Verify { data_dir } => {
            let store = mycelium_store::Store::open(Path::new(&data_dir)).await?;
            let ok = verify_store(&store).await?;
            if ok {
                println!("store verified: layout + database OK");
            } else {
                anyhow::bail!("store verification failed");
            }
        }
    }
    Ok(())
}

/// Integrity check: required layout + SQLite `PRAGMA integrity_check`.
async fn verify_store(store: &mycelium_store::Store) -> anyhow::Result<bool> {
    let data_dir = store.data_dir();
    for sub in ["config", "db", "users", "library", "skills", "assets"] {
        if !data_dir.join(sub).is_dir() {
            eprintln!("missing directory: {}", data_dir.join(sub).display());
            return Ok(false);
        }
    }
    let row: (String,) = sqlx::query_as("PRAGMA integrity_check")
        .fetch_one(store.pool())
        .await?;
    if row.0 != "ok" {
        eprintln!("sqlite integrity_check: {}", row.0);
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// The admin account is created only through the web /setup page —
    /// no CLI bootstrap subcommand exists.
    #[test]
    fn no_bootstrap_subcommand() {
        let cmd = Args::command();
        assert!(cmd.find_subcommand("bootstrap-admin").is_none());
        assert!(cmd.find_subcommand("migrate").is_some());
        assert!(cmd.find_subcommand("backup").is_some());
        assert!(cmd.find_subcommand("verify").is_some());
    }
}
