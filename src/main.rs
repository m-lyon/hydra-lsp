use tower_lsp::{LspService, Server};

use hydra_lsp::backend::HydraLspBackend;

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

    let (service, socket) = LspService::new(HydraLspBackend::new);

    // Start the server
    tracing::info!("Starting Hydra LSP server");
    Server::new(stdin, stdout, socket).serve(service).await;
}
