//! Mycelium2 admin CLI.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "mycelium2-cli")]
#[command(about = "Mycelium2 administrative CLI")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create the first admin user.
    BootstrapAdmin {
        #[arg(
            long,
            env = "MYCELIUM2_DATA_DIR",
            default_value = "/opt/mycelium2/data"
        )]
        data_dir: String,
    },
    /// Run database migrations.
    Migrate {
        #[arg(
            long,
            env = "MYCELIUM2_DATA_DIR",
            default_value = "/opt/mycelium2/data"
        )]
        data_dir: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _args = Args::parse();
    // TODO: implement CLI commands
    Ok(())
}
