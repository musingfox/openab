mod acp;
mod agent;
mod auth;
mod llm;
mod tools;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "openab-agent", about = "Native Rust coding agent with ACP")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Authenticate with an LLM provider via OAuth device flow
    Auth {
        #[command(subcommand)]
        provider: AuthProvider,
    },
}

#[derive(Subcommand)]
enum AuthProvider {
    /// OpenAI Codex (ChatGPT Plus/Pro subscription)
    CodexOauth,
    /// Show stored credentials
    Status,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        None => {
            // Default: run ACP server
            let mut server = acp::AcpServer::new();
            server.run().await;
        }
        Some(Commands::Auth { provider }) => match provider {
            AuthProvider::CodexOauth => {
                if let Err(e) = auth::login_codex_device_flow().await {
                    eprintln!("❌ Authentication failed: {e}");
                    std::process::exit(1);
                }
            }
            AuthProvider::Status => {
                auth::show_status();
            }
        },
    }
}
