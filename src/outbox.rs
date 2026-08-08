//! The queue every server-to-client message goes through.
//!
//! Handlers do not talk to `tower_lsp::Client` directly. They push onto this
//! queue, which is a plain channel send that finishes immediately, and one
//! background task drains the queue and does the awaiting. Messages leave in
//! the order they were pushed.
//!
//! # Why handlers must not await the client
//!
//! `tower_lsp::Server::serve` stops sending messages to the client once its input
//! stream closes, such as when the client disconnects. `tower_lsp` can only hold one
//! message in the outgoing queue at a time, so if a handler awaits on a dead client, it
//! can block the queue. Similar story for a handler that awaits on a request to the
//! client.
//!
//! The outbox avoids this by making an internal queue that handlers push onto, and a
//! separate task that drains the queue and awaits on the client. If the client is dead, the
//! drain task will eventually notice and exit, but the handlers, and thus the server,
//! will not be blocked. This is the same pattern used in `ty_server` and `ruff_server`.

use tokio::sync::{mpsc, oneshot};
use tower_lsp::Client;
use tower_lsp::lsp_types::{Diagnostic, MessageType, Registration, Url};

/// How long [`ClientOutbox::flush`] waits before giving up.
///
/// A flush that waited forever would reintroduce the hang this type exists to
/// remove, so it is bounded. Only `shutdown` flushes, and the client sends
/// `exit` straight after, so a second is long enough to matter and short
/// enough not to delay a real exit.
const FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// One queued message, in the order it was handed over.
enum ClientCall {
    Log(MessageType, String),
    PublishDiagnostics {
        uri: Url,
        diagnostics: Vec<Diagnostic>,
        version: Option<i32>,
    },
    RegisterCapability(Vec<Registration>),
    WorkspaceDiagnosticRefresh,
    /// Replies once every message queued before it has been sent.
    Flush(oneshot::Sender<()>),
}

/// Send handle held by the backend. Every method returns immediately.
pub struct ClientOutbox {
    tx: mpsc::UnboundedSender<ClientCall>,
}

impl ClientOutbox {
    /// Start the drain task and return the handle to feed it.
    ///
    /// # Panics
    ///
    /// Panics if called outside a tokio runtime, because it spawns a task.
    /// The backend is only ever built inside `Runtime::block_on` (`main.rs`)
    /// or inside a `#[tokio::test]` (`tests/common`), so this holds.
    pub fn spawn(client: Client) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(drain(client, rx));
        Self { tx }
    }

    /// Send a `window/logMessage`.
    pub fn log(&self, typ: MessageType, message: impl std::fmt::Display) {
        self.send(ClientCall::Log(typ, message.to_string()));
    }

    /// Send a `textDocument/publishDiagnostics`.
    pub fn publish_diagnostics(
        &self,
        uri: Url,
        diagnostics: Vec<Diagnostic>,
        version: Option<i32>,
    ) {
        self.send(ClientCall::PublishDiagnostics {
            uri,
            diagnostics,
            version,
        });
    }

    /// Ask the client to register capabilities. Failures are reported by the
    /// drain task, since there is nobody here to return them to.
    pub fn register_capability(&self, registrations: Vec<Registration>) {
        self.send(ClientCall::RegisterCapability(registrations));
    }

    /// Ask the client to re-pull diagnostics for every open document.
    pub fn workspace_diagnostic_refresh(&self) {
        self.send(ClientCall::WorkspaceDiagnosticRefresh);
    }

    /// Wait for everything queued so far to reach the client, or for
    /// [`FLUSH_TIMEOUT`], whichever comes first.
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(ClientCall::Flush(tx)).is_err() {
            return;
        }
        if tokio::time::timeout(FLUSH_TIMEOUT, rx).await.is_err() {
            tracing::warn!("gave up waiting for queued client messages to be sent");
        }
    }

    fn send(&self, call: ClientCall) {
        // The only way this fails is the drain task being gone, which happens
        // on the way out. Nothing can be done about it and nothing should
        // stop for it, so say so on stderr and carry on.
        if self.tx.send(call).is_err() {
            tracing::error!("dropped a message for the client: the outbox is closed");
        }
    }
}

impl std::fmt::Debug for ClientOutbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientOutbox").finish_non_exhaustive()
    }
}

/// Send queued messages one at a time, in order, for as long as the handle
/// lives.
///
/// Awaiting here is safe in a way that awaiting in a handler is not: if the
/// client stops reading, this task is the only thing that stops.
async fn drain(client: Client, mut rx: mpsc::UnboundedReceiver<ClientCall>) {
    while let Some(call) = rx.recv().await {
        match call {
            ClientCall::Log(typ, message) => {
                client.log_message(typ, message).await;
            }
            ClientCall::PublishDiagnostics {
                uri,
                diagnostics,
                version,
            } => {
                client.publish_diagnostics(uri, diagnostics, version).await;
            }
            ClientCall::RegisterCapability(registrations) => {
                if let Err(error) = client.register_capability(registrations).await {
                    client
                        .log_message(
                            MessageType::WARNING,
                            format!("Failed to register file watchers: {error}"),
                        )
                        .await;
                }
            }
            ClientCall::WorkspaceDiagnosticRefresh => {
                if let Err(error) = client.workspace_diagnostic_refresh().await {
                    tracing::warn!(%error, "failed to request workspace diagnostic refresh");
                }
            }
            ClientCall::Flush(reply) => {
                // Everything queued ahead of this has been sent, because the
                // loop handles one message at a time in order.
                let _ = reply.send(());
            }
        }
    }
}
