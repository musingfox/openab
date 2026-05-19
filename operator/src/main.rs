mod manifest;
mod apply;
mod get;
mod delete;
mod discord;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "oabctl", about = "OAB agent provisioner for ECS")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create or update OAB services from manifest files
    Apply {
        /// Path to manifest file or directory
        #[arg(short, long)]
        file: String,
        /// Auto-register Discord bots (requires DISCORD_DEVELOPER_TOKEN env var or SSM)
        #[arg(long, default_value_t = false)]
        auto_register: bool,
    },
    /// List OAB services and their status
    Get {
        /// Resource type
        resource: String,
        /// Optional resource name
        name: Option<String>,
        /// ECS cluster name
        #[arg(long, default_value = "default")]
        cluster: String,
    },
    /// Delete an OAB service
    Delete {
        /// Resource type
        resource: String,
        /// Resource name
        name: String,
        /// ECS cluster name
        #[arg(long, default_value = "default")]
        cluster: String,
        /// Namespace
        #[arg(long, default_value = "prod")]
        namespace: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;

    match cli.command {
        Commands::Apply { file, auto_register } => apply::run(&config, &file, auto_register).await,
        Commands::Get { resource, name, cluster } => get::run(&config, &resource, name.as_deref(), &cluster).await,
        Commands::Delete { resource, name, cluster, namespace } => {
            delete::run(&config, &resource, &name, &cluster, &namespace).await
        }
    }
}
