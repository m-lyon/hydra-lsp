//! Language server binary for Hydra YAML configuration files.
//!
//! Normally launched by an editor with no arguments, in which case it talks
//! LSP over stdin/stdout. `--version` and `--help` are handled up front so a
//! client can identify a downloaded binary before running it.

use clap::Parser;
use clap::error::ErrorKind;
use tower_lsp::{LspService, Server};

use hydra_lsp::backend::{HydraLspBackend, MAX_CONCURRENT_REQUESTS};

/// Language Server for Hydra configuration files
#[derive(Parser)]
#[command(name = "hydra-lsp")]
#[command(author, version, about, long_about = None)]
#[command(
    after_help = "Run without arguments to speak the Language Server Protocol over stdin/stdout."
)]
struct Args {}

/// Print help or version and exit if that is what was asked for.
///
/// Anything else — no arguments at all, or a flag we do not know — falls
/// through to the LSP loop. Unknown flags are deliberately not fatal: an editor
/// may append its own transport flag, and refusing to start would be worse than
/// ignoring it. Nothing is written to stdout on that path, because stdout is
/// the LSP transport and a stray byte would corrupt the protocol.
fn handle_args() {
    // Nothing to parse in the common case, so skip clap entirely.
    if std::env::args_os().len() <= 1 {
        return;
    }
    match Args::try_parse() {
        Ok(Args {}) => {}
        Err(err) => match err.kind() {
            // clap writes these two to stdout, which is what a client reading
            // `--version` expects.
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                let _ = err.print();
                std::process::exit(0);
            }
            _ => eprintln!("hydra-lsp: ignoring unrecognised arguments; starting language server"),
        },
    }
}

fn main() {
    // Deal with arguments before the async runtime starts, so `--version`
    // costs nothing more than a process start.
    handle_args();
    serve();
}

/// Run the LSP loop to completion.
///
/// Not `async`: `#[tokio::main]` only ever expanded to "build a runtime, call
/// `block_on`", so this was already a synchronous call from `main`.
///
/// A single-threaded runtime, because a multi-threaded one would have nothing
/// to give the extra threads. `tower_lsp::Server::serve` deliberately avoids
/// `tokio::spawn` — it aims to be executor agnostic — and instead `join!`s
/// reading stdin, writing stdout, and a `buffer_unordered` over the in-flight
/// handlers. That is one task, so `block_on` drives the whole server on this
/// thread and handlers interleave cooperatively rather than run in parallel.
/// The expensive work goes to the rayon pools built in `initialize`, and a
/// handler awaiting a pool result yields here so the next message can be read.
///
/// hydra-lsp spawns exactly one task of its own, the client outbox
/// (`outbox.rs`). It only moves already-built messages out to the client, so
/// it takes no lock and does no analysis, and on a current-thread runtime it
/// costs no OS thread.
///
/// Therefore anything that blocks without awaiting — notably taking `Session::db`, a
/// `parking_lot::Mutex` — stalls stdin and every other in-flight request until it
/// returns. Extra runtime threads would not have helped with that, since the stdin
/// loop shares the blocked task.
fn serve() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .thread_name("hydra-tokio")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            // stderr, never stdout: stdout is the LSP transport.
            eprintln!("hydra-lsp: failed to start the async runtime: {error}");
            std::process::exit(1);
        }
    };

    runtime.block_on(async {
        // Create the LSP service
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();

        let (service, socket) = LspService::new(HydraLspBackend::new);

        // Start the server
        Server::new(stdin, stdout, socket)
            .concurrency_level(MAX_CONCURRENT_REQUESTS)
            .serve(service)
            .await;
    });
}
