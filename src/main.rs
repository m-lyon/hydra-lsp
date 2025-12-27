mod backend;
mod diagnostics;
mod document;
mod python_analyzer;
mod yaml_parser;

use tower_lsp::{LspService, Server};

use backend::HydraLspBackend;

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .init();

    // Create the LSP service
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| HydraLspBackend::new(client));

    // Start the server
    tracing::info!("Starting Hydra LSP server");
    Server::new(stdin, stdout, socket).serve(service).await;
}
