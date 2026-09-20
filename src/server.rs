//! The language server: LSP over stdin/stdout.
//!
//! Reached as `hydrust server`. Normally launched by an editor, which may
//! append transport flags of its own; see `ServerCommand` in `cli.rs` for why
//! those are ignored rather than rejected.

use tower_lsp::{LspService, Server};

use crate::backend::{HydraLspBackend, MAX_CONCURRENT_REQUESTS};

/// Initialise tracing for the language server.
///
/// Separate from `hydrust check`'s init (`cli.rs`, ANSI on, level from
/// `--verbosity`) because only one subscriber can win per process, so each
/// subcommand installs its own. Here the writer is stderr and ANSI is off:
/// stdout is the LSP transport, and an editor's log pane is not a terminal.
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
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
/// The server spawns exactly one task of its own, the client outbox
/// (`outbox.rs`). It only moves already-built messages out to the client, so
/// it takes no lock and does no analysis, and on a current-thread runtime it
/// costs no OS thread.
///
/// Therefore anything that blocks without awaiting — notably taking `Session::db`, a
/// `parking_lot::Mutex` — stalls stdin and every other in-flight request until it
/// returns. Extra runtime threads would not have helped with that, since the stdin
/// loop shares the blocked task.
pub fn serve() {
    init_tracing();

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .thread_name("hydra-tokio")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            // stderr, never stdout: stdout is the LSP transport.
            eprintln!("hydrust: failed to start the async runtime: {error}");
            std::process::exit(1);
        }
    };

    runtime.block_on(async {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();

        let (service, socket) = LspService::new(HydraLspBackend::new);

        Server::new(stdin, stdout, socket)
            .concurrency_level(MAX_CONCURRENT_REQUESTS)
            .serve(service)
            .await;
    });
}
