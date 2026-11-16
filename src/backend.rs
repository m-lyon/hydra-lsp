use dashmap::DashMap;
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::OnceLock;
use tower_lsp::Client;
use tower_lsp::lsp_types::Url;

use crate::outbox::ClientOutbox;
use crate::yaml_cache::DocumentInput;

mod capabilities;
mod diagnostics;
mod handlers;
mod pools;
mod presentation;
mod session;

pub use capabilities::{HydrustCapabilities, NegotiatedFeatures};
pub(crate) use pools::MAX_CONCURRENT_REQUESTS;

use capabilities::{ClientFeatures, Settings};
use session::Session;

pub struct HydraLspBackend {
    /// Everything the server sends to the client goes through here.
    ///
    /// There is deliberately no `Client` on this struct. Awaiting the client
    /// from a handler can wedge the whole server once the client's pipe
    /// closes — see the [`outbox`](crate::outbox) module docs — so the only
    /// `Client` in the process lives in the outbox's drain task.
    outbox: ClientOutbox,
    settings: Arc<RwLock<Settings>>,
    /// Initialized lazily on `initialize`. `None` means a notification
    /// arrived before `initialize` (LSP protocol violation) — handlers log
    /// a warning and ignore the event. The outer `RwLock` only guards the
    /// `Option`; once set, concurrent reads share the inner `Session`'s
    /// db lock so unrelated operations do not all have to proceed sequentially
    /// through a single backend-wide mutex.
    session: Arc<parking_lot::RwLock<Option<Session>>>,
    /// Map from document URI to its salsa input handle.
    ///
    /// Lock order: code that mutates a `DocumentInput` enters the
    /// `document_inputs` shard guard FIRST, then takes `Session::db.lock()`.
    /// `get_or_create_input` and `did_close` both follow this order.
    /// `get_or_create_input` additionally relies on the entry guard to make
    /// the lookup-or-insert atomic, which keeps concurrent `did_open` calls
    /// for the same URI from creating distinct inputs.
    ///
    /// The corollary for readers: never take a shard guard while holding the
    /// db lock. `DashMap::iter`/`get` hand out guards, so copy the
    /// (`Copy`) `DocumentInput` out and let the guard drop before calling
    /// `Session::db.lock()`. Running a salsa query inside an `iter().filter`
    /// closure holds the shard guard for the duration and inverts the order.
    ///
    /// This map retains one entry per unique URI ever opened — salsa
    /// exposes no API to remove an `#[salsa::input]` from storage
    /// (verified against salsa v0.26.2). On `did_close` we soft-close
    /// the input via `DocumentInput::close`, which clears the source
    /// `String` (the dominant per-document cost); the input slot itself
    /// remains so a subsequent `did_open` for the same URI reuses it.
    /// Per-URI overhead is therefore O(slot header), independent of file
    /// size. Per-query LRUs (`lru = 512`) bound cached computation memory.
    document_inputs: DashMap<Url, DocumentInput>,
    /// Rayon pool for requests on the interactive path: hover,
    /// signature_help, goto_definition and semantic_tokens.
    ///
    /// Separate from the worker pool so a diagnostics run cannot occupy every
    /// thread and leave a hover waiting. Note the isolation is over threads,
    /// not over work: both pools reach `cached_definition_info`, and when two
    /// requests want the same key salsa blocks one until the other finishes,
    /// whichever pools they came from.
    ///
    /// Defaults to 2 threads — see [`pool_sizes`](pools::pool_sizes) for why.
    /// Built exactly once, in `initialize` (`build_pools`), where the user's
    /// `numThreads` setting is available.
    latency_pool: OnceLock<rayon::ThreadPool>,
    /// Rayon pool for background work (diagnostics validation, future
    /// workspace-wide analysis).
    ///
    /// Defaults to 3 threads — see [`pool_sizes`](pools::pool_sizes). Built
    /// alongside `latency_pool`, and overridable through `numThreads`.
    worker_pool: OnceLock<rayon::ThreadPool>,
    /// What the client advertised in its `initialize` capabilities. Set exactly
    /// once, in `initialize`, like the pools, and read through
    /// [`Self::client_features`] and [`Self::negotiated`], which report
    /// everything off until then.
    client_features: OnceLock<ClientFeatures>,
}

impl std::fmt::Debug for HydraLspBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HydraLspBackend")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl HydraLspBackend {
    /// # Panics
    ///
    /// Panics if called outside a tokio runtime: the outbox spawns a task.
    pub fn new(client: Client) -> Self {
        Self {
            outbox: ClientOutbox::spawn(client),
            settings: Arc::new(RwLock::new(Settings::default())),
            session: Arc::new(parking_lot::RwLock::new(None)),
            document_inputs: DashMap::new(),
            latency_pool: OnceLock::new(),
            worker_pool: OnceLock::new(),
            client_features: OnceLock::new(),
        }
    }

    fn client_features(&self) -> ClientFeatures {
        self.client_features.get().copied().unwrap_or_default()
    }

    /// The optional behaviours on for this session: the same flags the
    /// `experimental.hydrust` block advertised.
    ///
    /// - `pull_diagnostics`: answer `textDocument/diagnostic` and stop pushing
    ///   diagnostics proactively; without it, push via `publish_diagnostics`.
    /// - `watched_files`: register our own file watchers on `initialized`, so
    ///   the client does not need to configure any.
    /// - `diagnostic_refresh`: nudge the client to re-pull with
    ///   `workspace/diagnostic/refresh` instead of re-publishing each open
    ///   document ourselves.
    fn negotiated(&self) -> NegotiatedFeatures {
        self.client_features().negotiated
    }
}
