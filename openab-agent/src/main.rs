mod acp;
mod agent;
mod llm;
mod tools;

use acp::AcpServer;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let mut server = AcpServer::new();
    server.run().await;
}
