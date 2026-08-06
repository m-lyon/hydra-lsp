use dashmap::DashMap;
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tower_lsp::jsonrpc::{Error, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use ruff_db::Db;
use ruff_db::files::{File, Files};
use ruff_db::system::SystemPath;
use salsa::Setter;

use crate::database::HydraDatabase;
use crate::diagnostics::{self, DiagnosticRule};
use crate::python_analyzer::{DefinitionInfo, ParameterInfo, PythonAnalyzer};
use crate::python_cache::{self, PythonConfig, TargetString};
use crate::yaml_cache::{self, DocumentInput, ParsedYaml};
use crate::yaml_parser::{
    ARGS_KEY, CONVERT_KEY, CompletionContext, ConvertMode, HydraSemanticToken, PARTIAL_KEY,
    RECURSIVE_KEY, ResolvedParameterContext, YamlParser,
};

/// Glob applied to every watched root — the workspace folders (via a plain
/// string pattern) and each out-of-workspace Python search path (via relative
/// patterns; see `initialized`). Single source of truth for on-disk change
/// notifications; the extension filter in `did_change_watched_files` must stay
/// aligned with the extensions here.
const WATCHED_PY_GLOB: &str = "**/*.{py,pyi,pth}";

/// Format a parameter as a string for signature labels (e.g., "*args", "name: str")
fn format_param_label(p: &ParameterInfo) -> String {
    let mut s = String::new();
    if p.is_variadic {
        s.push('*');
    } else if p.is_variadic_keyword {
        s.push_str("**");
    }
    s.push_str(&p.name);
    if let Some(type_ann) = &p.type_annotation {
        s.push_str(&format!(": {}", type_ann));
    }
    s
}

/// Convert a ParameterInfo to LSP ParameterInformation
fn to_parameter_information(p: &ParameterInfo) -> ParameterInformation {
    ParameterInformation {
        label: ParameterLabel::Simple(format_param_label(p)),
        documentation: p.default_value.as_ref().map(|dv| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("Default: `{}`", dv),
            })
        }),
    }
}

/// Build signature label and parameter information from a list of parameters.
/// Returns the label string, the LSP parameter info list, and the filtered
/// `ParameterInfo` references (needed for active-parameter resolution).
fn build_signature_params<'a>(
    params: &'a [ParameterInfo],
    filter_param: Option<&str>,
) -> (String, Vec<ParameterInformation>, Vec<&'a ParameterInfo>) {
    let filtered: Vec<_> = params
        .iter()
        .filter(|p| filter_param.is_none_or(|f| p.name != f))
        .collect();
    let param_strs: Vec<String> = filtered.iter().map(|p| format_param_label(p)).collect();
    let param_infos: Vec<ParameterInformation> = filtered
        .iter()
        .map(|p| to_parameter_information(p))
        .collect();
    (param_strs.join(", "), param_infos, filtered)
}

/// Declare the per-capability feature toggles once, as
/// `field => "settingKey", "logName"` triples.
///
/// One invocation generates everything that has to agree about a toggle: the
/// [`FeatureToggles`] field, its default, the settings key it is parsed from,
/// the name used when logging it as disabled, and the key list that goes out in
/// [`HydrustCapabilities::supported_settings`]. Adding a toggle is a one-line
/// edit here, so none of those can drift apart.
macro_rules! feature_toggles {
    ($($field:ident => $key:literal, $name:literal),+ $(,)?) => {
        /// Feature toggle settings for individual LSP capabilities.
        #[derive(Debug, Clone, Copy)]
        pub struct FeatureToggles {
            $(pub $field: bool,)+
        }

        impl Default for FeatureToggles {
            fn default() -> Self {
                Self { $($field: true,)+ }
            }
        }

        impl FeatureToggles {
            /// The `initializationOptions.settings` keys these toggles read.
            /// Part of [`SUPPORTED_SETTINGS`].
            pub const SETTING_KEYS: &'static [&'static str] = &[$($key,)+];

            /// Parse feature toggles from a JSON settings object. A key that is
            /// missing or not a boolean leaves the feature on.
            fn from_json(settings: &serde_json::Value) -> Self {
                Self {
                    $($field: settings.get($key).and_then(|v| v.as_bool()).unwrap_or(true),)+
                }
            }

            /// Return names of disabled features, if any.
            fn disabled_names(&self) -> Vec<&'static str> {
                let mut names = Vec::new();
                $(if !self.$field {
                    names.push($name);
                })+
                names
            }
        }
    };
}

feature_toggles! {
    hover => "enableHover", "hover",
    completion => "enableCompletion", "completion",
    signature_help => "enableSignatureHelp", "signatureHelp",
    goto_definition => "enableGotoDefinition", "gotoDefinition",
    semantic_tokens => "enableSemanticTokens", "semanticTokens",
    diagnostics => "enableDiagnostics", "diagnostics",
}

/// Server-wide settings for Hydrust Server.
#[derive(Debug, Default)]
pub struct Settings {
    pub python_interpreter: Option<String>,
    pub disabled_rules: HashSet<DiagnosticRule>,
    pub features: FeatureToggles,
}

/// Version of the `experimental.hydrust` block described by
/// [`HydrustCapabilities`]. A client compares it against the highest version it
/// knows how to read.
///
/// Bump this only when something already in the block changes meaning in a way
/// that would mislead an older client: a setting key that starts doing
/// something different, a renamed or repurposed feature name, a field that
/// changes type. Purely additive changes do NOT need a bump, because clients are
/// expected to ignore names they do not recognise.
const HYDRUST_PROTOCOL_VERSION: u32 = 1;

/// The non-toggle keys the server reads out of
/// `initializationOptions.settings`. This list must stay in step with the
/// parsing in `initialize`; the feature toggles alongside it come from
/// [`FeatureToggles::SETTING_KEYS`], which is generated with the toggles
/// themselves.
const CORE_SETTINGS: &[&str] = &["pythonInterpreter", "disabledRules", "numThreads"];

/// Every key the server actually reads out of `initializationOptions.settings`.
/// Anything else a client sends is silently ignored.
static SUPPORTED_SETTINGS: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| {
        CORE_SETTINGS
            .iter()
            .chain(FeatureToggles::SETTING_KEYS)
            .copied()
            .collect()
    });

/// Which optional behaviours the server will actually use this session. Each
/// one needs the client to have asked for it in its `initialize` capabilities,
/// so these are read from the flags captured at the top of `initialize`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NegotiatedFeatures {
    /// The client issues `textDocument/diagnostic`, so we answer pull requests
    /// rather than pushing diagnostics at it.
    pub pull_diagnostics: bool,
    /// The client allows watchers to be registered at runtime, so we set up our
    /// own `workspace/didChangeWatchedFiles` watchers.
    pub watched_files: bool,
    /// The client accepts `workspace/diagnostic/refresh`, so we can ask it to
    /// re-pull after a watched Python file changes.
    pub diagnostic_refresh: bool,
}

/// A feature name paired with the flag that decides whether it is on.
type FeatureGate = (&'static str, fn(&NegotiatedFeatures) -> bool);

/// The coarse feature names, each paired with the flag that decides whether it
/// is active. Keeping the name and its gate together means a name can only be
/// advertised when the matching behaviour really is switched on.
///
/// Every name here is only sent to a client that advertised the capability it
/// depends on:
///
/// - `pullDiagnostics` — we answer `textDocument/diagnostic`, returning
///   unchanged reports via result IDs. Absent when the client did not advertise
///   pull support, in which case it gets diagnostics pushed to it instead.
/// - `watchedFiles` — we register our own file watchers on `initialized`, so
///   the client does not need to configure any. Absent when the client did not
///   advertise dynamic registration for `workspace/didChangeWatchedFiles`.
/// - `diagnosticRefresh` — we send `workspace/diagnostic/refresh` after a
///   watched Python file changes. Absent when the client did not advertise
///   refresh support, in which case it would need to re-pull on its own.
const SUPPORTED_FEATURES: &[FeatureGate] = &[
    ("pullDiagnostics", |f| f.pull_diagnostics),
    ("watchedFiles", |f| f.watched_files),
    ("diagnosticRefresh", |f| f.diagnostic_refresh),
];

/// What this build of the server understands, sent back from `initialize` as
/// `capabilities.experimental.hydrust`.
///
/// This lets a client that may be driving any released server version discover
/// the surface directly.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HydrustCapabilities {
    /// See [`HYDRUST_PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Setting keys under `initializationOptions.settings` that do something.
    pub supported_settings: &'static [&'static str],
    /// Rule codes accepted in the `disabledRules` setting, and emitted as
    /// diagnostic codes.
    pub supported_rules: &'static [&'static str],
    /// The features that are switched on for this session, after matching what
    /// the server can do against what the client asked for. A name being absent
    /// means the behaviour will not happen on this connection, not that the
    /// server is too old to do it. Empty when the client asked for none of
    /// them. See [`SUPPORTED_FEATURES`].
    pub features: Vec<&'static str>,
}

impl HydrustCapabilities {
    /// Build the block for a session, keeping only the features the client and
    /// server agreed on.
    pub fn new(negotiated: NegotiatedFeatures) -> Self {
        Self {
            protocol_version: HYDRUST_PROTOCOL_VERSION,
            supported_settings: SUPPORTED_SETTINGS.as_slice(),
            supported_rules: DiagnosticRule::all_codes(),
            features: SUPPORTED_FEATURES
                .iter()
                .filter(|(_, is_active)| is_active(&negotiated))
                .map(|(name, _)| *name)
                .collect(),
        }
    }

    /// Build the value for `ServerCapabilities::experimental`, i.e. this block
    /// wrapped in its `hydrust` key. Returns `None` only if serialization
    /// fails, which it cannot for these plain fields.
    pub fn to_experimental(&self) -> Option<serde_json::Value> {
        #[derive(serde::Serialize)]
        struct Experimental<'a> {
            hydrust: &'a HydrustCapabilities,
        }
        serde_json::to_value(Experimental { hydrust: self }).ok()
    }
}

/// Initialized analysis state owned by `HydraLspBackend`.
///
/// `db` lives behind a `Mutex` rather than an `RwLock` because `HydraDatabase`
/// (via `salsa::Storage`) is `Send` but not `Sync` — salsa's storage uses
/// interior mutability that is not safe to share by `&T` across threads.
/// The Mutex is taken only briefly: read handlers acquire it long enough to
/// clone a snapshot (an Arc-share of cached data, microseconds), and writers
/// acquire it long enough to mutate a salsa input. The expensive work runs
/// on independent snapshots outside the lock — see `Session::snapshot`.
///
struct Session {
    db: parking_lot::Mutex<HydraDatabase>,
    python_config: PythonConfig,
}

impl Session {
    /// Cheap, read-only snapshot of the database for concurrent worker use.
    ///
    /// Holds the db lock only long enough to clone the salsa storage (an
    /// Arc-share of cached data — `HydraDatabase: Clone` via
    /// `salsa::Storage`). The returned `SessionSnapshot` is `Send` and
    /// survives independently of the write side; if a writer modifies an
    /// input after the snapshot is taken, salsa cancels in-flight reads on
    /// the snapshot at the next query boundary.
    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            db: self.db.lock().clone(),
            python_config: self.python_config,
        }
    }

    /// Bump the enclosing site-packages `FileRoot` revision for each changed
    /// directory so `parse_pth_files` re-scans it — but only when the directory
    /// is actually a registered library root (i.e. one of the resolver's
    /// site-packages search paths).
    ///
    /// Returns the number of directories actually bumped.
    fn bump_site_packages_pth_roots(&self, directories: &HashSet<std::path::PathBuf>) -> usize {
        if directories.is_empty() {
            return 0;
        }
        let mut db = self.db.lock();
        let mut bumped = 0usize;
        for directory in directories {
            let Some(sys_path) = SystemPath::from_std_path(directory) else {
                continue;
            };
            // Only registered roots have a revision to bump; `root()` returns
            // `None` for directories that aren't library search paths, so this
            // naturally gates on site-packages membership.
            if db.files().root(&*db, sys_path).is_some() {
                Files::touch_root(&mut *db, sys_path);
                bumped += 1;
            }
        }
        bumped
    }
}

/// Cheap, send-safe view of the analysis state. Produced by
/// `Session::snapshot`; consumed by handlers that hand the database to
/// `tokio::task::spawn_blocking`.
#[derive(Clone)]
pub struct SessionSnapshot {
    pub db: HydraDatabase,
    pub python_config: PythonConfig,
}

pub struct HydraLspBackend {
    pub client: Client,
    pub settings: Arc<RwLock<Settings>>,
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
    /// Rayon pool dedicated to latency-sensitive operations (hover,
    /// signature_help, goto_definition). Isolated from the worker pool so that
    /// a long diagnostics run never queues behind a hover. Built exactly once, in
    /// `initialize`, where the user's `numThreads` setting is available.
    latency_pool: OnceLock<rayon::ThreadPool>,
    /// Rayon pool for background work (diagnostics validation, future
    /// workspace-wide analysis). Sized to available_parallelism − 2, clamped to
    /// at least 1, unless overridden by `numThreads`.
    worker_pool: OnceLock<rayon::ThreadPool>,
    /// Whether the client advertised `workspace/didChangeWatchedFiles`
    /// dynamic registration in its `initialize` capabilities. Captured in
    /// `initialize` and read in `initialized` to decide whether to register the
    /// watchers at all.
    watched_files_dynamic: AtomicBool,
    /// Whether the client advertised relative-pattern support for watched
    /// files. Required to watch out-of-workspace Python search paths.
    watched_files_relative_patterns: AtomicBool,
    /// Whether the client advertised support for pull diagnostics
    /// (`textDocument/diagnostic`, LSP 3.17). Captured in `initialize`. When
    /// true we answer pull requests and skip proactively pushing diagnostics;
    /// when false we fall back to pushing via `publish_diagnostics`.
    supports_pull_diagnostics: AtomicBool,
    /// Whether the client advertised `workspace/diagnostic/refresh` support.
    /// When true, a watched-file change nudges the client to re-pull instead of
    /// the server re-publishing each open document.
    supports_diagnostic_refresh: AtomicBool,
}

/// Outcome of a closure run on a rayon pool via [`spawn_on_pool`].
///
/// Two failure modes are kept distinct because callers treat them differently:
/// - `Cancelled` is the *expected* consequence of a concurrent write: salsa
///   throws `salsa::Cancelled` (via `panic::resume_unwind`) to abandon a query
///   whose revision was superseded. Callers should silently drop the result;
///   the superseding edit will recompute it.
/// - `Panicked` is a genuine bug worth logging.
#[derive(Debug)]
pub(crate) enum PoolOutcome<R> {
    Completed(R),
    /// Salsa cancelled the query (a concurrent write bumped the revision).
    Cancelled,
    /// The closure panicked for a non-cancellation reason. Carries a
    /// best-effort message extracted from the panic payload.
    Panicked(String),
}

/// Best-effort human-readable message from a panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Bridge a rayon thread-pool task to an async tokio context.
///
/// Spawns `f` on `pool` and returns a receiver whose `.await` resolves when
/// the task finishes. Unlike `tokio::task::spawn_blocking`, the closure runs
/// on a caller-chosen rayon pool, which lets us separate latency-sensitive
/// work (hover, goto_definition) from background work (diagnostics).
///
/// `f` is run inside `catch_unwind` and its result is delivered as a
/// [`PoolOutcome`], distinguishing normal completion, salsa cancellation, and a
/// genuine panic. Without it a salsa `Cancelled` unwind would propagate into rayon and
/// abort the process, since these pools register no `panic_handler`.
///
/// The borrow of `pool` ends immediately after `pool.spawn` returns — before
/// any `.await` suspension — so callers do not need to hold a reference across
/// yield points.
fn spawn_on_pool<F, R>(
    pool: &OnceLock<rayon::ThreadPool>,
    f: F,
) -> tokio::sync::oneshot::Receiver<PoolOutcome<R>>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let Some(pool) = pool.get() else {
        // Sender dropped on return; receiver resolves to RecvError.
        return rx;
    };
    pool.spawn(move || {
        let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
            Ok(r) => PoolOutcome::Completed(r),
            Err(payload) => {
                if payload.downcast_ref::<salsa::Cancelled>().is_some() {
                    PoolOutcome::Cancelled
                } else {
                    PoolOutcome::Panicked(panic_message(&*payload))
                }
            }
        };
        let _ = tx.send(outcome);
    });
    rx
}

/// What to do with the result of the diagnostics validation task.
#[derive(Debug, PartialEq, Eq)]
enum DiagAction {
    /// Publish these diagnostics to the client.
    Publish(Vec<Diagnostic>),
    /// Do nothing — the round was superseded (cancelled) or the receiver was
    /// dropped. Leaves any previously published diagnostics intact.
    Skip,
    /// The task panicked; log the message and publish nothing.
    LogPanic(String),
}

/// Map a diagnostics task outcome to a [`DiagAction`]. Pure so it can be tested
/// without a live `Client` or rayon pool.
fn classify_diag_outcome(
    outcome: std::result::Result<
        PoolOutcome<Vec<Diagnostic>>,
        tokio::sync::oneshot::error::RecvError,
    >,
) -> DiagAction {
    match outcome {
        Ok(PoolOutcome::Completed(diagnostics)) => DiagAction::Publish(diagnostics),
        Ok(PoolOutcome::Cancelled) | Err(_) => DiagAction::Skip,
        Ok(PoolOutcome::Panicked(msg)) => DiagAction::LogPanic(msg),
    }
}

impl std::fmt::Debug for HydraLspBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HydraLspBackend")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl HydraLspBackend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            settings: Arc::new(RwLock::new(Settings::default())),
            session: Arc::new(parking_lot::RwLock::new(None)),
            document_inputs: DashMap::new(),
            latency_pool: OnceLock::new(),
            worker_pool: OnceLock::new(),
            watched_files_dynamic: AtomicBool::new(false),
            watched_files_relative_patterns: AtomicBool::new(false),
            supports_pull_diagnostics: AtomicBool::new(false),
            supports_diagnostic_refresh: AtomicBool::new(false),
        }
    }

    /// Whether the client supports pull diagnostics (`textDocument/diagnostic`).
    /// When true we answer pull requests and stop pushing diagnostics proactively.
    fn supports_pull(&self) -> bool {
        self.supports_pull_diagnostics.load(Ordering::Relaxed)
    }

    /// Whether the client supports dynamic registration for
    /// `workspace/didChangeWatchedFiles`. When true we register our own file watchers
    /// on `initialized`, so the client does not need to configure any.
    fn supports_dynamic_watched_files(&self) -> bool {
        self.watched_files_dynamic.load(Ordering::Relaxed)
    }

    /// Whether the client supports `workspace/diagnostic/refresh`, letting us nudge
    /// it to re-pull instead of re-publishing each open document ourselves.
    fn supports_diag_refresh(&self) -> bool {
        self.supports_diagnostic_refresh.load(Ordering::Relaxed)
    }

    fn fallback_workspace_root() -> Result<String> {
        std::env::current_dir()
            .map_err(|error| Error {
                code: Error::internal_error().code,
                message: format!(
                    "failed to determine workspace root from current directory: {}",
                    error
                )
                .into(),
                data: None,
            })?
            .into_os_string()
            .into_string()
            .map_err(|path| Error {
                code: Error::internal_error().code,
                message: format!(
                    "failed to determine workspace root from current directory: non-utf8 path {}",
                    std::path::PathBuf::from(path).display()
                )
                .into(),
                data: None,
            })
    }

    fn initialize_session(
        &self,
        workspace_root: Option<String>,
        interpreter: Option<String>,
    ) -> Result<()> {
        let workspace_root = match workspace_root {
            Some(workspace_root) => workspace_root,
            None => Self::fallback_workspace_root()?,
        };
        let cwd = SystemPath::new(&workspace_root);
        let db = HydraDatabase::new(cwd);
        let python_config = PythonConfig::new(&db, Some(workspace_root), interpreter);
        *self.session.write() = Some(Session {
            db: parking_lot::Mutex::new(db),
            python_config,
        });
        Ok(())
    }

    /// Take a cheap snapshot of the analysis state for use on a worker thread.
    ///
    /// Returns `None` (and logs) when the session has not yet been built by
    /// `initialize`. Holds the session read lock and the inner db lock only
    /// long enough to clone the salsa storage; the returned snapshot is
    /// `Send` and can be passed to `tokio::task::spawn_blocking` without
    /// forcing other handlers to wait and run sequentially.
    fn snapshot(&self) -> Option<SessionSnapshot> {
        self.with_session("snapshot", Session::snapshot)
    }

    /// Run `f` with a borrow of the initialized session, returning `None`
    /// (and logging a warning that names `context`) when the session has
    /// not yet been built by `initialize`.
    ///
    /// The outer `RwLock` guard is held for the duration of `f`, but `f`
    /// only sees `&Session` — the inner db lock is acquired by `f` itself, so
    /// concurrent callers with different access patterns do not serialize on the
    /// outer lock.
    fn with_session<T>(&self, context: &'static str, f: impl FnOnce(&Session) -> T) -> Option<T> {
        let session = self.session.read();
        if session.is_none() {
            // Drop the guard before logging so a slow tracing subscriber
            // (file write, stderr lock contention) does not stall every
            // other handler that needs the session.
            drop(session);
            tracing::warn!(context, "session not initialized; ignoring");
            return None;
        }
        Some(f(session.as_ref().expect("checked Some above")))
    }

    /// Python search-path roots that live outside the workspace folder(s).
    ///
    /// These are the site-packages and `.pth`-target directories the resolver
    /// reads from but that a workspace-relative watcher glob would miss (see
    /// `initialized`). Shares the exact `search_paths_for_config` list the
    /// resolver uses, so each watched root equals the path the resolver interns
    /// under. Nested paths are collapsed so a parent `**` watcher isn't
    /// duplicated by one of its children.
    fn out_of_workspace_search_roots(&self) -> Vec<std::path::PathBuf> {
        self.with_session("computing watched search paths", |s| {
            let db = s.db.lock();
            let workspace = s
                .python_config
                .workspace_root(&*db)
                .as_deref()
                .map(std::path::PathBuf::from);
            let roots: Vec<ruff_db::system::SystemPathBuf> =
                python_cache::search_paths_for_config(&*db, s.python_config)
                    .iter()
                    // Drop the relative "." entry and anything under the
                    // workspace (already covered by the workspace-relative glob).
                    .filter(|path| path.is_absolute())
                    .filter(|path| workspace.as_ref().is_none_or(|w| !path.starts_with(w)))
                    .filter_map(|path| {
                        ruff_db::system::SystemPathBuf::from_path_buf(path.clone()).ok()
                    })
                    .collect();
            ruff_db::system::deduplicate_nested_paths(roots)
                .map(|path| path.as_std_path().to_path_buf())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
    }

    /// Run Python definition lookup on a blocking thread using a database snapshot.
    ///
    /// Takes a `SessionSnapshot` (cheap — salsa shares cached data via Arc) and
    /// moves it to `tokio::task::spawn_blocking` so that expensive Python
    /// analysis (module resolution, file parsing) doesn't block the async runtime.
    ///
    /// On cache hits the blocking thread returns almost immediately; on misses
    /// it performs the full analysis without holding any locks.
    async fn spawn_definition_lookup(
        &self,
        target_value: String,
    ) -> anyhow::Result<(DefinitionInfo, std::path::PathBuf, String, String)> {
        let Some(snapshot) = self.snapshot() else {
            anyhow::bail!("session not initialized")
        };
        let SessionSnapshot { db, python_config } = snapshot;

        spawn_on_pool(&self.latency_pool, move || {
            let start = Instant::now();
            let target = TargetString::new(&db, target_value);
            let cached = python_cache::cached_definition_info(&db, python_config, target);
            let result = match cached.get() {
                Ok(def) => Ok((
                    def.definition_info.clone(),
                    def.file_path.clone(),
                    def.module_path.clone(),
                    def.symbol_name.clone(),
                )),
                Err(e) => Err(anyhow::anyhow!("{}", e)),
            };
            tracing::debug!(
                elapsed_us = start.elapsed().as_micros() as u64,
                target = target.value(&db),
                ok = result.is_ok(),
                "definition lookup"
            );
            result
        })
        .await
        .map_or_else(
            // Sender dropped without sending (e.g. pool shutdown); treat like
            // cancellation — the request is stale.
            |_| anyhow::bail!("definition lookup superseded by a newer edit"),
            |outcome| match outcome {
                PoolOutcome::Completed(result) => result,
                PoolOutcome::Cancelled => {
                    anyhow::bail!("definition lookup superseded by a newer edit")
                }
                PoolOutcome::Panicked(msg) => {
                    tracing::error!(%msg, "definition lookup panicked");
                    anyhow::bail!("definition lookup panicked: {msg}")
                }
            },
        )
    }

    /// Look up the cached `parsed_yaml` result for a URI, but only if the
    /// document is a Hydra file.
    ///
    /// Checks the cheap `is_hydra_file` salsa query first; if the document
    /// is not a Hydra file we skip the YAML parse entirely. Returns `None` when:
    /// - The URI has no `DocumentInput` (notification not yet seen).
    /// - The session is not initialized.
    /// - The document is not a Hydra file.
    ///
    /// On the warm path for a Hydra file, both salsa calls are O(1) cache
    /// hits. Used by every per-keystroke handler so a hover/etc. on an
    /// already-parsed document does no YAML re-parsing.
    fn cached_parsed_yaml(&self, uri: &Url, context: &'static str) -> Option<ParsedYaml> {
        let input = *self.document_inputs.get(uri)?;
        self.with_session(context, |s| {
            let db = s.db.lock();
            if yaml_cache::is_hydra_file(&*db, input) {
                Some(yaml_cache::parsed_yaml(&*db, input))
            } else {
                None
            }
        })
        .flatten()
    }

    /// Get or create a `DocumentInput` for a given URI.
    ///
    /// Holds the dashmap entry guard across the salsa write so that concurrent
    /// callers for the same URI cannot both observe a vacant entry and create
    /// distinct inputs (which would orphan one in salsa storage and let stale
    /// text leak through). Lock order: dashmap shard → `Session::db.lock()`.
    ///
    /// Returns `None` when the session is not yet initialized; callers
    /// (notification handlers) should treat that as a no-op.
    fn get_or_create_input(&self, uri: &Url, text: &str, version: i32) -> Option<DocumentInput> {
        self.with_session("opening documents", |s| {
            match self.document_inputs.entry(uri.clone()) {
                dashmap::Entry::Occupied(occ) => {
                    let input = *occ.get();
                    let mut db = s.db.lock();
                    input.set_text(&mut *db).to(text.to_string());
                    input.set_version(&mut *db).to(version);
                    input
                }
                dashmap::Entry::Vacant(vac) => {
                    let db = s.db.lock();
                    let input = DocumentInput::new(&*db, text.to_string(), version);
                    vac.insert(input);
                    input
                }
            }
        })
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for HydraLspBackend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let workspace_root = params
            .root_uri
            .as_ref()
            .and_then(|root_uri| root_uri.to_file_path().ok())
            .map(|path| path.to_string_lossy().to_string());

        let watched_files_caps = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.did_change_watched_files.as_ref());
        self.watched_files_dynamic.store(
            watched_files_caps
                .and_then(|d| d.dynamic_registration)
                .unwrap_or(false),
            Ordering::Relaxed,
        );
        self.watched_files_relative_patterns.store(
            watched_files_caps
                .and_then(|d| d.relative_pattern_support)
                .unwrap_or(false),
            Ordering::Relaxed,
        );

        // Pull diagnostics: does the client issue `textDocument/diagnostic`?
        self.supports_pull_diagnostics.store(
            params
                .capabilities
                .text_document
                .as_ref()
                .and_then(|t| t.diagnostic.as_ref())
                .is_some(),
            Ordering::Relaxed,
        );
        // Refresh: can we nudge the client to re-pull with `workspace/diagnostic/refresh`?
        self.supports_diagnostic_refresh.store(
            params
                .capabilities
                .workspace
                .as_ref()
                .and_then(|w| w.diagnostic.as_ref())
                .and_then(|d| d.refresh_support)
                .unwrap_or(false),
            Ordering::Relaxed,
        );

        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Hydrust Server initializing with options: {:?}",
                    params.initialization_options
                ),
            )
            .await;

        // Parse initialization options
        let mut num_threads: Option<usize> = None;
        if let Some(init_options) = params.initialization_options
            && let Some(settings) = init_options.get("settings")
        {
            // Extract values without holding the lock across awaits
            let interpreter_path = settings
                .get("pythonInterpreter")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let mut parsed_rules = HashSet::new();
            if let Some(disabled_rules) = settings.get("disabledRules")
                && let Some(rules_array) = disabled_rules.as_array()
            {
                for rule_value in rules_array {
                    if let Some(rule_str) = rule_value.as_str()
                        && let Some(rule) = DiagnosticRule::from_code(rule_str)
                    {
                        parsed_rules.insert(rule);
                    }
                }
            }

            // Parse feature toggle settings
            let toggles = FeatureToggles::from_json(settings);

            // Parse optional thread-count override (total threads across both pools).
            // Clamp to sane max threads range: at least 1
            let max_threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .saturating_mul(8);
            num_threads = settings
                .get("numThreads")
                .and_then(|v| v.as_u64())
                .map(|n| (n as usize).clamp(1, max_threads));

            // Write settings under the lock (no awaits)
            {
                let mut s = self.settings.write();
                if interpreter_path.is_some() {
                    s.python_interpreter = interpreter_path.clone();
                }
                s.disabled_rules = parsed_rules;
                s.features = toggles;
            }

            // Log after releasing the lock
            if let Some(ref path) = interpreter_path {
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!("Python interpreter configured: {}", path),
                    )
                    .await;
            }

            if !self.settings.read().disabled_rules.is_empty() {
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!("Disabled rules: {:?}", self.settings.read().disabled_rules),
                    )
                    .await;
            }

            // Log disabled features
            let disabled_features = toggles.disabled_names();
            if !disabled_features.is_empty() {
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!("Disabled features: {:?}", disabled_features),
                    )
                    .await;
            }
        }

        // Build the two rayon pools exactly once, when `numThreads` is known.
        // `numThreads` is the total across both pools. Every branch keeps both
        // counts >= 1 (rayon treats num_threads(0) as "auto-detect", which would
        // silently over-allocate).
        let (latency, worker) = match num_threads {
            Some(1) | Some(2) => (1, 1),
            Some(n) => (2, n - 2),
            None => {
                let worker = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
                    .saturating_sub(2)
                    .max(1);
                (2, worker)
            }
        };
        match (
            rayon::ThreadPoolBuilder::new()
                .num_threads(latency)
                .thread_name(|i| format!("hydra-latency-{i}"))
                .build(),
            rayon::ThreadPoolBuilder::new()
                .num_threads(worker)
                .thread_name(|i| format!("hydra-worker-{i}"))
                .build(),
        ) {
            (Ok(latency_pool), Ok(worker_pool)) => {
                // `set` cannot fail: `initialize` is the sole writer, once.
                let _ = self.latency_pool.set(latency_pool);
                let _ = self.worker_pool.set(worker_pool);
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!(
                            "Thread pools configured: {} latency + {} worker ({} total)",
                            latency,
                            worker,
                            latency + worker
                        ),
                    )
                    .await;
            }
            _ => {
                let msg = format!(
                    "Failed to build thread pools ({latency} latency + {worker} worker); \
                     the OS refused thread creation"
                );
                self.client.log_message(MessageType::ERROR, &msg).await;
                let mut err = Error::internal_error();
                err.message = msg.into();
                return Err(err);
            }
        }

        let interpreter_path = self.settings.read().python_interpreter.clone();
        self.initialize_session(workspace_root, interpreter_path)?;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                // `position_encoding` is intentionally left unset (it falls under
                // `..Default::default()` below). Per the LSP spec, an unset
                // encoding means the default: UTF-16. Every position we emit and
                // consume is therefore counted in UTF-16 code units (see
                // `cp_to_utf16_col` / `utf16_col_to_byte_offset` in yaml_parser
                // and the `PositionEncoding::Utf16` conversion in python_analyzer).
                // The only realistic client is the UTF-16-native VSCode extension,
                // so explicit negotiation buys nothing; revisit only if a client
                // that prefers UTF-8 is added (which would also require making the
                // internals byte-native to be worthwhile).
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string(), "_".to_string()]),
                    resolve_provider: Some(false),
                    ..Default::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec![
                        ":".to_string(),
                        "-".to_string(),
                        "[".to_string(),
                        ",".to_string(),
                    ]),
                    retrigger_characters: None,
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                definition_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: vec![
                                    SemanticTokenType::NAMESPACE,
                                    SemanticTokenType::CLASS,
                                    SemanticTokenType::FUNCTION,
                                    SemanticTokenType::PARAMETER,
                                    SemanticTokenType::PROPERTY,
                                    SemanticTokenType::VARIABLE,
                                    SemanticTokenType::STRING,
                                    SemanticTokenType::NUMBER,
                                ],
                                token_modifiers: vec![
                                    SemanticTokenModifier::DECLARATION,
                                    SemanticTokenModifier::DEFINITION,
                                ],
                            },
                            range: Some(false),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            ..Default::default()
                        },
                    ),
                ),
                diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                    DiagnosticOptions {
                        identifier: Some("hydra-lsp".to_string()),
                        // Hydra targets resolve into watched Python files, so a
                        // change in one file can affect another's diagnostics.
                        inter_file_dependencies: true,
                        // Document/open-files pull only; no whole-workspace pull.
                        workspace_diagnostics: false,
                        work_done_progress_options: Default::default(),
                    },
                )),
                // Tell the client which settings and rules this build
                // understands, so it can warn about anything it sends that
                // would be silently ignored, plus the features that are
                // actually switched on for this session.
                experimental: HydrustCapabilities::new(NegotiatedFeatures {
                    pull_diagnostics: self.supports_pull(),
                    watched_files: self.supports_dynamic_watched_files(),
                    diagnostic_refresh: self.supports_diag_refresh(),
                })
                .to_experimental(),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "hydra-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        // Register file watchers dynamically so the server owns the single
        // source of truth for which paths trigger `did_change_watched_files`
        // (see `WATCHED_PY_GLOB`). The client creates the watchers on its side
        // in response and forwards matching events back to us.
        if self.supports_dynamic_watched_files() {
            // Workspace folders: the client matches this string glob against
            // them, covering first-party sources and any in-workspace `.venv`.
            let mut watchers = vec![FileSystemWatcher {
                glob_pattern: GlobPattern::String(WATCHED_PY_GLOB.to_string()),
                // `None` = watch for create, change, and delete.
                kind: None,
            }];

            // Out-of-workspace Python search paths (external site-packages,
            // editable `.pth` targets) live outside the workspace folders, so the
            // string glob above never matches them. Watch each as a relative
            // pattern based at its directory.
            //
            // Symlink model (matches ty / PyCharm: we register the exact
            // roots the resolver interns under — site-packages roots are already
            // canonical via ty's environment discovery — and rely on the OS
            // reporting events under that same path. We keep no real-path to symlink
            // reverse map, and macOS FSEvents does not fire under symlinked
            // directories; both are accepted limitations. Do NOT re-canonicalize
            // roots here, or watched paths would diverge from interned keys.
            if self.watched_files_relative_patterns.load(Ordering::Relaxed) {
                for root in self.out_of_workspace_search_roots() {
                    if let Ok(base_uri) = Url::from_directory_path(&root) {
                        watchers.push(FileSystemWatcher {
                            glob_pattern: GlobPattern::Relative(RelativePattern {
                                base_uri: OneOf::Right(base_uri),
                                pattern: WATCHED_PY_GLOB.to_string(),
                            }),
                            kind: None,
                        });
                    }
                }
            } else {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        "Client lacks relative-pattern watcher support; on-disk changes to \
                         out-of-workspace site-packages/.pth dirs will not invalidate caches",
                    )
                    .await;
            }

            let registration = Registration {
                id: "hydra-watched-files".to_string(),
                method: "workspace/didChangeWatchedFiles".to_string(),
                register_options: Some(
                    serde_json::to_value(DidChangeWatchedFilesRegistrationOptions { watchers })
                        .expect("watcher registration options serialize"),
                ),
            };
            if let Err(error) = self.client.register_capability(vec![registration]).await {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("Failed to register file watchers: {error}"),
                    )
                    .await;
            }
        } else {
            self.client
                .log_message(
                    MessageType::WARNING,
                    "Client lacks workspace/didChangeWatchedFiles dynamic registration; \
                     on-disk Python/.pth changes will not invalidate caches",
                )
                .await;
        }

        self.client
            .log_message(MessageType::INFO, "Hydrust Server initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        let version = params.text_document.version;

        // Create a salsa input for this document. Returns None (and logs)
        // when called before initialize.
        let Some(input) = self.get_or_create_input(&uri, &text, version) else {
            return;
        };

        // Publish diagnostics if this is a Hydra file (using cached check). When it is
        // not a Hydra file we clear any diagnostics previously published.
        let is_hydra = self
            .with_session("opening documents", |s| {
                let db = s.db.lock();
                yaml_cache::is_hydra_file(&*db, input)
            })
            .unwrap_or(false);
        if self.settings.read().features.diagnostics {
            if is_hydra {
                self.publish_diagnostics_if_needed(&uri).await;
            } else {
                self.clear_diagnostics_if_needed(&uri).await;
            }
        }

        self.client
            .log_message(MessageType::INFO, format!("Document opened: {}", uri))
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;

        // Full sync: take the first change which is the entire document
        if let Some(change) = params.content_changes.into_iter().next() {
            // Update the salsa input (invalidates cached queries). Returns
            // None (and logs) when called before initialize.
            let Some(input) = self.get_or_create_input(&uri, &change.text, version) else {
                return;
            };

            // Re-publish diagnostics if this is a Hydra file (using cached
            // check). When it is not a Hydra file we clear any diagnostics
            // previously published.
            let is_hydra = self
                .with_session("changing documents", |s| {
                    let db = s.db.lock();
                    yaml_cache::is_hydra_file(&*db, input)
                })
                .unwrap_or(false);
            if self.settings.read().features.diagnostics {
                if is_hydra {
                    self.publish_diagnostics_if_needed(&uri).await;
                } else {
                    self.clear_diagnostics_if_needed(&uri).await;
                }
            }

            self.client
                .log_message(MessageType::INFO, format!("Document changed: {}", uri))
                .await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.client
            .log_message(
                MessageType::INFO,
                format!("Document saved: {}", params.text_document.uri),
            )
            .await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        // The client tells us when files in the workspace change on disk
        // (typically Python sources outside the editor). For each changed
        // path we call `File::sync_path`, which bumps the per-file revision
        // inside ruff_db so that any salsa query reading that file's
        // `source_text` is invalidated on the next request.
        //
        // We filter to Python analysis inputs before locking: `.py`, `.pyi`,
        // and watched `.pth` files all participate in analysis. These extensions
        // must stay aligned with `WATCHED_PY_GLOB`.
        //
        // Syncing existing files bumps the per-file revision inside ruff_db so
        // any query that read them through `source_text` is invalidated on the
        // next request. For `.pth` create/delete events we also bump the
        // enclosing site-packages `FileRoot` revision so the directory scan in
        // `parse_pth_files` is recomputed.
        if params.changes.is_empty() {
            return;
        }
        let mut pth_root_dirs = HashSet::new();
        let tracked_paths: Vec<std::path::PathBuf> = params
            .changes
            .iter()
            .filter_map(|change| {
                let path = change.uri.to_file_path().ok()?;
                match path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(str::to_ascii_lowercase)
                    .as_deref()
                {
                    Some("py") | Some("pyi") => Some(path),
                    Some("pth") => {
                        if matches!(
                            change.typ,
                            FileChangeType::CREATED | FileChangeType::DELETED
                        ) && let Some(parent) = path.parent()
                        {
                            pth_root_dirs.insert(parent.to_path_buf());
                        }
                        Some(path)
                    }
                    _ => None,
                }
            })
            .collect();
        if tracked_paths.is_empty() && pth_root_dirs.is_empty() {
            tracing::debug!(
                changed = params.changes.len(),
                "watched files changed; no python analysis inputs to sync"
            );
            return;
        }
        let (synced, bumped_roots) = self
            .with_session("watching files", |s| {
                let mut synced = 0usize;
                {
                    let mut db = s.db.lock();
                    for std_path in &tracked_paths {
                        let Some(sys_path) = SystemPath::from_std_path(std_path) else {
                            continue;
                        };
                        File::sync_path(&mut *db, sys_path);
                        synced += 1;
                    }
                }
                let bumped_roots = s.bump_site_packages_pth_roots(&pth_root_dirs);
                (synced, bumped_roots)
            })
            .unwrap_or((0, 0));
        tracing::debug!(
            changed = params.changes.len(),
            synced,
            pth_root_candidate_dirs = pth_root_dirs.len(),
            pth_root_bumped_dirs = bumped_roots,
            "watched files changed; synced python analysis inputs"
        );

        // The sync above invalidated Python-dependent salsa queries but did not
        // refresh diagnostics, now we refresh diagnostics, unless none to refresh.
        if !self.settings.read().features.diagnostics || (synced == 0 && bumped_roots == 0) {
            return;
        }

        // Pull clients with refresh support: a single `workspace/diagnostic/refresh`
        // nudges the client to re-pull every open document — no per-doc work here.
        if self.supports_pull() && self.supports_diag_refresh() {
            if let Err(error) = self.client.workspace_diagnostic_refresh().await {
                tracing::warn!(%error, "failed to request workspace diagnostic refresh");
            }
            return;
        }

        // A pull client without `workspace/diagnostic/refresh` has no
        // server-driven refresh channel: we cannot nudge it to re-pull, and
        // pushing would duplicate diagnostics into a second client-side
        // collection. Diagnostics derived from the changed Python file stay
        // stale until the client re-pulls on its own (next edit, open, or
        // focus change).
        if self.supports_pull() {
            tracing::warn!(
                "python inputs changed but client supports pull diagnostics without \
                 workspace/diagnostic/refresh; diagnostics may be stale until the \
                 client re-pulls"
            );
            return;
        }

        // Otherwise fall back to refreshing each open Hydra doc.
        //
        // Snapshot the map first so every shard guard is released before we
        // take the db lock. Filtering inside `iter()` would run
        // `is_hydra_file` — which needs the db lock — while a shard read
        // guard is held, i.e. db → shard, the reverse of the order
        // `get_or_create_input` uses (see the `document_inputs` field docs).
        let candidates: Vec<(Url, DocumentInput)> = self
            .document_inputs
            .iter()
            .map(|entry| (entry.key().clone(), *entry.value()))
            .collect();
        let hydra_uris: Vec<Url> = self
            .with_session("refreshing diagnostics", |s| {
                let db = s.db.lock();
                candidates
                    .into_iter()
                    // Soft-closed docs have empty text, and `is_hydra_file`
                    // returns false on empty text, so they self-exclude.
                    .filter(|(_, input)| yaml_cache::is_hydra_file(&*db, *input))
                    .map(|(uri, _)| uri)
                    .collect()
            })
            .unwrap_or_default();
        for uri in hydra_uris {
            self.publish_diagnostics_if_needed(&uri).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;

        // Soft-close the salsa input: clear the source text so salsa drops
        // the per-document `String` while the input slot is retained for
        // reuse on a subsequent `did_open` for the same URI. Salsa exposes
        // no input-deletion API, so this is the minimum-footprint
        // equivalent (matches `ruff_db::files::VirtualFile::close`).
        let input = self.document_inputs.get(&uri).map(|entry| *entry);
        if let Some(input) = input {
            self.with_session("closing documents", |s| {
                let mut db = s.db.lock();
                input.close(&mut *db);
            });
        }

        // Clear any diagnostics we published while the document was open, so
        // they do not linger in the client after the document is closed.
        if self.settings.read().features.diagnostics {
            self.clear_diagnostics_if_needed(&uri).await;
        }

        self.client
            .log_message(MessageType::INFO, format!("Document closed: {}", uri))
            .await;
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let start = Instant::now();
        if !self.settings.read().features.hover {
            return Ok(None);
        }
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        // Cached salsa lookups: skip if not a Hydra file. Both calls are O(1)
        // hashmap probes on the warm path.
        let Some(parsed) = self.cached_parsed_yaml(&uri, "hover") else {
            return Ok(None);
        };
        let Ok(content) = parsed.result() else {
            return Ok(None);
        };
        let Some(hydra_object) = content.target_at_position(position) else {
            return Ok(None);
        };

        self.client
            .log_message(
                MessageType::LOG,
                format!("Found target at position: {:?}", hydra_object),
            )
            .await;

        // Extract Python definition info on a blocking thread (cached + non-blocking)
        let extract_result = self
            .spawn_definition_lookup(hydra_object.target.value.clone())
            .await;

        let result = match extract_result {
            Ok((definition_info, _file_path, _module_path, _symbol_name)) => {
                let hover_content = match definition_info {
                    DefinitionInfo::Function(sig) => PythonAnalyzer::format_function(&sig),
                    DefinitionInfo::Class(class_info) => PythonAnalyzer::format_class(&class_info),
                    DefinitionInfo::Method(method_info) => {
                        PythonAnalyzer::format_method(&method_info)
                    }
                };
                let range = Range {
                    start: Position {
                        line: hydra_object.target.line,
                        character: hydra_object.target.value_start,
                    },
                    end: Position {
                        line: hydra_object.target.line,
                        character: hydra_object.target_value_end(),
                    },
                };

                Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: hover_content,
                    }),
                    range: Some(range),
                }))
            }
            Err(e) => {
                // If Python analysis fails, don't show any hover, but log a warning
                let err_msg = e.to_string();
                if err_msg.starts_with("Invalid _target_ format:") {
                    self.client.log_message(MessageType::ERROR, err_msg).await;
                } else {
                    self.client.log_message(MessageType::WARNING, err_msg).await;
                }
                Ok(None)
            }
        };
        tracing::debug!(elapsed_ms = start.elapsed().as_millis() as u64, "hover");
        result
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        if !self.settings.read().features.completion {
            return Ok(None);
        }
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let Some(input) = self.document_inputs.get(&uri).map(|e| *e) else {
            return Ok(None);
        };

        // Resolve completion context.  On the common path (valid YAML, cursor on a
        // recognised line) this is two salsa hits + two O(1) hashmap lookups with
        // no full-document String clone.  Only the partial-syntax fallback (cursor
        // on a new or mid-edit line where the parse maps have no entry) clones the
        // full text and runs the backward line scan.
        //
        // This runs inline under `s.db.lock()`, which is the right call while the
        // work stays this cheap: it holds the db lock only briefly and skips the
        // pool-dispatch overhead. If real completions land (module/class/parameter
        // resolution, reading Python) and the computation becomes expensive,
        // prefer the `snapshot()` + `spawn_on_pool` pattern used by `hover` /
        // `goto_definition`: it releases the lock so it no longer blocks the write
        // path, and moves the CPU work off the tokio worker thread. Completion is
        // pull-based, so the resulting salsa cancellation on a concurrent edit is
        // free — the client re-requests.
        let Some(context) = self
            .with_session("completion", |s| {
                let db = s.db.lock();
                if !yaml_cache::is_hydra_file(&*db, input) {
                    return None;
                }
                let parsed = yaml_cache::parsed_yaml(&*db, input);

                // Try the cached parse maps first.
                if let Ok(content) = parsed.result() {
                    let text = input.text(&*db);
                    let line_text = text.lines().nth(position.line as usize).unwrap_or("");
                    if let Some(ctx) = content.completion_context_at(position, line_text) {
                        return Some(ctx);
                    }
                }

                // Fallback: full raw-text scan for partial-syntax lines (e.g. the
                // document is temporarily unparseable mid-edit, or the cursor is
                // on a new line not yet in the parse maps).
                let text = input.text(&*db);
                YamlParser::get_completion_context(text, position).ok()
            })
            .flatten()
        else {
            return Ok(None);
        };

        match context {
            CompletionContext::TargetValue { partial } => {
                // TODO: Implement module/class completion
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!("Target completion requested for: {}", partial),
                    )
                    .await;

                // Ok(Some(CompletionResponse::Array(vec![
                //     CompletionItem {
                //         label: "example.module.Class".to_string(),
                //         kind: Some(CompletionItemKind::CLASS),
                //         detail: Some("Example class (placeholder)".to_string()),
                //         ..Default::default()
                //     },
                //     CompletionItem {
                //         label: "example.module.function".to_string(),
                //         kind: Some(CompletionItemKind::FUNCTION),
                //         detail: Some("Example function (placeholder)".to_string()),
                //         ..Default::default()
                //     },
                // ])))
                Ok(None) // Placeholder: no completions yet
            }
            CompletionContext::ParameterKey { target, partial } => {
                // TODO: Resolve target and get parameter completions
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!(
                            "Parameter completion requested for target: {}, partial: {}",
                            target, partial
                        ),
                    )
                    .await;

                // For demonstration, return some placeholder parameters
                // Ok(Some(CompletionResponse::Array(vec![
                //     CompletionItem {
                //         label: "param1".to_string(),
                //         kind: Some(CompletionItemKind::PROPERTY),
                //         detail: Some("int - Example parameter".to_string()),
                //         documentation: Some(Documentation::String(
                //             "A placeholder parameter".to_string(),
                //         )),
                //         ..Default::default()
                //     },
                //     CompletionItem {
                //         label: "param2".to_string(),
                //         kind: Some(CompletionItemKind::PROPERTY),
                //         detail: Some("str - Example parameter".to_string()),
                //         ..Default::default()
                //     },
                // ])))
                Ok(None) // Placeholder: no completions yet
            }
            CompletionContext::ParameterValue {
                target,
                parameter,
                partial,
            } => {
                // Provide completions for Hydra keyword values
                let param_str = parameter.as_str();
                if param_str == PARTIAL_KEY || param_str == RECURSIVE_KEY {
                    return Ok(Some(CompletionResponse::Array(vec![
                        CompletionItem {
                            label: "true".to_string(),
                            kind: Some(CompletionItemKind::VALUE),
                            detail: Some("Boolean value".to_string()),
                            ..Default::default()
                        },
                        CompletionItem {
                            label: "false".to_string(),
                            kind: Some(CompletionItemKind::VALUE),
                            detail: Some("Boolean value".to_string()),
                            ..Default::default()
                        },
                    ])));
                }
                if param_str == CONVERT_KEY {
                    let items = ConvertMode::variants()
                        .iter()
                        .map(|v| CompletionItem {
                            label: v.to_string(),
                            kind: Some(CompletionItemKind::ENUM_MEMBER),
                            detail: Some(format!("{} convert mode", v)),
                            ..Default::default()
                        })
                        .collect();
                    return Ok(Some(CompletionResponse::Array(items)));
                }
                if param_str == ARGS_KEY {
                    return Ok(None);
                }

                self.client
                    .log_message(
                        MessageType::INFO,
                        format!(
                            "Parameter value completion requested for target: {}, parameter: {}, partial: {}",
                            target, parameter, partial
                        ),
                    )
                    .await;

                Ok(None) // Placeholder: no completions yet
            }
            crate::yaml_parser::CompletionContext::Unknown => Ok(None),
        }
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let start = Instant::now();
        if !self.settings.read().features.signature_help {
            return Ok(None);
        }
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(parsed) = self.cached_parsed_yaml(&uri, "signature_help") else {
            return Ok(None);
        };
        let Ok(content) = parsed.result() else {
            return Ok(None);
        };
        let Some((target_value, param_context, keyword_keys)) =
            content.target_for_parameter_line(position)
        else {
            return Ok(None);
        };

        // Extract Python definition info on a blocking thread (cached + non-blocking)
        let extract_result = self.spawn_definition_lookup(target_value.clone()).await;

        let result = match extract_result {
            Ok((definition_info, _file_path, _module_path, _symbol_name)) => {
                let implicit_param = definition_info.implicit_param();
                let (signature_label, parameters, param_infos) = match &definition_info {
                    DefinitionInfo::Function(sig) => {
                        let (params_str, params, infos) =
                            build_signature_params(&sig.parameters, implicit_param);
                        let label = format!("{}({})", sig.name, params_str);
                        (label, params, infos)
                    }
                    DefinitionInfo::Class(class_info) => {
                        if let Some(init_sig) = &class_info.init_signature {
                            let (params_str, params, infos) =
                                build_signature_params(&init_sig.parameters, implicit_param);
                            let label = format!("{}({})", class_info.name, params_str);
                            (label, params, infos)
                        } else {
                            let label = format!("{}()", class_info.name);
                            (label, vec![], vec![])
                        }
                    }
                    DefinitionInfo::Method(method_info) => {
                        let sig = &method_info.signature;
                        let (params_str, params, infos) =
                            build_signature_params(&sig.parameters, implicit_param);
                        let label =
                            format!("{}.{}({})", method_info.class_name, sig.name, params_str);
                        (label, params, infos)
                    }
                };

                // Use an out-of-bounds index when the YAML key doesn't match any
                // parameter, so the client doesn't default to highlighting index 0.
                let active_parameter = Some(match &param_context {
                    ResolvedParameterContext::Keyword(key) => {
                        // Try exact match first, then fall back to **kwargs
                        parameters
                            .iter()
                            .position(|p| match &p.label {
                                ParameterLabel::Simple(name) => {
                                    let param_name = name.split(':').next().unwrap_or(name).trim();
                                    param_name == key.as_str()
                                }
                                ParameterLabel::LabelOffsets(_) => false,
                            })
                            .or_else(|| {
                                // Unknown keyword: highlight **kwargs if present
                                param_infos.iter().position(|p| p.is_variadic_keyword)
                            })
                            .unwrap_or(parameters.len()) as u32
                    }
                    ResolvedParameterContext::Positional(index, num_args_in_yaml) => {
                        // _args_ entries map to positional parameters in order.
                        // Count regular (non-keyword-only, non-variadic) params
                        // that can be passed positionally.
                        let positional_count = param_infos
                            .iter()
                            .filter(|p| {
                                !p.is_variadic && !p.is_variadic_keyword && !p.is_keyword_only
                            })
                            .count();
                        let idx = *index as usize;
                        let pos = if idx < positional_count {
                            // Map to the idx-th positional parameter
                            let mut seen = 0usize;
                            param_infos
                                .iter()
                                .position(|p| {
                                    if !p.is_variadic
                                        && !p.is_variadic_keyword
                                        && !p.is_keyword_only
                                    {
                                        if seen == idx {
                                            return true;
                                        }
                                        seen += 1;
                                    }
                                    false
                                })
                                .unwrap_or(parameters.len())
                        } else {
                            // Overflow into *args if present
                            param_infos
                                .iter()
                                .position(|p| p.is_variadic)
                                .unwrap_or(parameters.len())
                        };
                        // Don't highlight any parameter when the _args_ list
                        // is empty — there are no actual arguments being passed.
                        // Also suppress when the positional parameter is already
                        // specified as a keyword argument.
                        (if *num_args_in_yaml == 0
                            || (pos < param_infos.len()
                                && keyword_keys.contains(&param_infos[pos].name))
                        {
                            parameters.len()
                        } else {
                            pos
                        }) as u32
                    }
                });

                Ok(Some(SignatureHelp {
                    signatures: vec![SignatureInformation {
                        label: signature_label,
                        documentation: None,
                        parameters: if parameters.is_empty() {
                            None
                        } else {
                            Some(parameters)
                        },
                        active_parameter: None,
                    }],
                    active_signature: Some(0),
                    active_parameter,
                }))
            }
            Err(e) => {
                let err_msg = e.to_string();
                if err_msg.starts_with("Invalid _target_ format:") {
                    self.client.log_message(MessageType::ERROR, err_msg).await;
                } else {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("Python analysis failed for signature help: {}", e),
                        )
                        .await;
                }
                Ok(None)
            }
        };
        tracing::debug!(
            elapsed_ms = start.elapsed().as_millis() as u64,
            "signature_help"
        );
        result
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let start = Instant::now();
        if !self.settings.read().features.goto_definition {
            return Ok(None);
        }
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(parsed) = self.cached_parsed_yaml(&uri, "goto_definition") else {
            return Ok(None);
        };
        let Ok(content) = parsed.result() else {
            return Ok(None);
        };
        let Some(target_info) = content.target_at_position(position).cloned() else {
            return Ok(None);
        };

        // Extract definition info on a blocking thread (cached + non-blocking)
        let extract_result = self
            .spawn_definition_lookup(target_info.target.value.clone())
            .await;
        let (file_path, start_line, start_col, end_line, end_col) = match extract_result {
            Ok((definition_info, file_path, _module_path, _symbol_name)) => {
                let (start_line, start_col, end_line, end_col) = definition_info.position();
                (file_path, start_line, start_col, end_line, end_col)
            }
            Err(e) => {
                let error_msg = e.to_string();
                if error_msg.starts_with("Invalid _target_ format:") {
                    self.client.log_message(MessageType::ERROR, error_msg).await;
                } else {
                    self.client
                        .log_message(MessageType::WARNING, error_msg)
                        .await;
                }
                return Ok(None);
            }
        };

        // Convert file path to URI
        let target_uri = match Url::from_file_path(&file_path) {
            Ok(uri) => uri,
            Err(_) => {
                self.client
                    .log_message(
                        MessageType::ERROR,
                        format!("Could not convert path to URI: {}", file_path.display()),
                    )
                    .await;
                return Ok(None);
            }
        };

        let result = Ok(Some(GotoDefinitionResponse::Scalar(Location {
            uri: target_uri,
            range: Range {
                start: Position {
                    line: start_line,
                    character: start_col,
                },
                end: Position {
                    line: end_line,
                    character: end_col,
                },
            },
        })));
        tracing::debug!(
            elapsed_ms = start.elapsed().as_millis() as u64,
            "goto_definition"
        );
        result
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        if !self.settings.read().features.semantic_tokens {
            return Ok(None);
        }
        let uri = params.text_document.uri;
        let Some(input) = self.document_inputs.get(&uri).map(|e| *e) else {
            return Ok(None);
        };

        self.client
            .log_message(MessageType::INFO, "Generating semantic tokens".to_string())
            .await;

        // Look up the salsa-cached token list. The `semantic_tokens` query
        // depends on `parsed_yaml`, so it is automatically invalidated when
        // the document changes and served from the cache on every other call.
        // The LSP format conversion happens inside the closure so we only
        // clone the small, already-converted Vec out of the lock.
        let Some(data) = self
            .with_session("semantic_tokens", |s| {
                let db = s.db.lock();
                if !yaml_cache::is_hydra_file(&*db, input) {
                    return None;
                }
                let tokens = yaml_cache::semantic_tokens(&*db, input);
                Some(HydraSemanticToken::to_lsp_tokens(tokens))
            })
            .flatten()
        else {
            return Ok(None);
        };

        self.client
            .log_message(
                MessageType::INFO,
                format!("Generated {} semantic tokens", data.len()),
            )
            .await;

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    /// Pull diagnostics: answer a `textDocument/diagnostic` request.
    ///
    /// Runs the same parse + validate closure the push path uses, but on a
    /// detached snapshot on the worker pool so latency-sensitive requests keep
    /// their thread capacity. When a concurrent write bumps the salsa revision
    /// mid-compute the round is cancelled; we surface that as an LSP
    /// `ServerCancelled` error with `retrigger_request: true` so the client
    /// re-pulls — demand-driven retry, no server-side attempt counter.
    async fn diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> Result<DocumentDiagnosticReportResult> {
        let uri = params.text_document.uri;

        // Diagnostics disabled, unknown document, or no session: report an empty
        // full report rather than an error (the client shows nothing).
        if !self.settings.read().features.diagnostics {
            return Ok(empty_full_report());
        }
        let Some(input_ref) = self.document_inputs.get(&uri) else {
            return Ok(empty_full_report());
        };
        let input = *input_ref;
        drop(input_ref);

        let Some(SessionSnapshot {
            db: db_snapshot,
            python_config,
        }) = self.snapshot()
        else {
            return Ok(empty_full_report());
        };
        let disabled_rules = self.settings.read().disabled_rules.clone();

        // Same closure as the push path: parse + validate on the worker pool,
        // inside the pool's catch_unwind so a `salsa::Cancelled` unwind is caught
        // rather than aborting the process.
        let join_result = spawn_on_pool(&self.worker_pool, move || {
            if !yaml_cache::is_hydra_file(&db_snapshot, input) {
                return Vec::new();
            }

            let parsed_yaml = yaml_cache::parsed_yaml(&db_snapshot, input);
            match parsed_yaml.result() {
                Ok(content) => diagnostics::validate_document(
                    content,
                    &disabled_rules,
                    &db_snapshot,
                    python_config,
                ),
                Err(e) => vec![yaml_syntax_error_diagnostic(e)],
            }
        })
        .await;

        match classify_diag_outcome(join_result) {
            DiagAction::Publish(diagnostics) => Ok(build_diagnostic_report(
                diagnostics,
                params.previous_result_id.as_deref(),
            )),
            // A newer revision superseded this round. Tell the client to re-pull
            // instead of retrying server-side.
            DiagAction::Skip => Err(server_cancelled_error()),
            DiagAction::LogPanic(msg) => {
                tracing::error!(%msg, "validate_document task panicked (pull)");
                Ok(empty_full_report())
            }
        }
    }
}

impl HydraLspBackend {
    /// Clear any diagnostics previously published for `uri`.
    async fn clear_diagnostics(&self, uri: &Url) {
        self.client
            .publish_diagnostics(uri.clone(), Vec::new(), None)
            .await;
    }

    /// Clear diagnostics only when the client relies on push.
    ///
    /// Pull clients own their diagnostic state (they stop requesting a closed
    /// document), so there is nothing for the server to clear.
    async fn clear_diagnostics_if_needed(&self, uri: &Url) {
        if self.supports_pull() {
            return;
        }
        self.clear_diagnostics(uri).await;
    }

    /// Publish diagnostics only when the client relies on push.
    ///
    /// Pull clients fetch diagnostics via `textDocument/diagnostic`; pushing to
    /// them would be redundant, so this is a no-op for them.
    async fn publish_diagnostics_if_needed(&self, uri: &Url) {
        if self.supports_pull() {
            return;
        }
        self.publish_diagnostics_for_document(uri).await;
    }

    /// Compute the diagnostics for `uri` from the current database state.
    ///
    /// Returns `None` when there is no session or no `DocumentInput` for the
    /// URI. The parse and validation run while holding the db lock so no
    /// concurrent write can bump the salsa revision mid-compute — the same
    /// synchronous model ty uses for its push handler, which is why no
    /// cancellation (and therefore no retry) is possible here.
    fn compute_diagnostics(&self, uri: &Url) -> Option<Vec<Diagnostic>> {
        let input = *self.document_inputs.get(uri)?;
        let disabled_rules = self.settings.read().disabled_rules.clone();
        self.with_session("compute diagnostics", |s| {
            // Hold the lock across the whole compute: with no interleaved
            // `set_text`, the revision can't move under us, so `parsed_yaml`
            // and `validate_document` never observe a `salsa::Cancelled`.
            let db = s.db.lock();
            let parsed_yaml = yaml_cache::parsed_yaml(&*db, input);
            match parsed_yaml.result() {
                Ok(content) => {
                    diagnostics::validate_document(content, &disabled_rules, &*db, s.python_config)
                }
                Err(e) => vec![yaml_syntax_error_diagnostic(e)],
            }
        })
    }

    /// Publish diagnostics for a document (push fallback for non-pull clients).
    ///
    /// Computes synchronously under the db lock (see [`compute_diagnostics`])
    /// and publishes the result. Because the compute can't be cancelled, there
    /// is no retry loop: a single pass always reflects the newest revision.
    ///
    /// [`compute_diagnostics`]: Self::compute_diagnostics
    async fn publish_diagnostics_for_document(&self, uri: &Url) {
        // Callers (`did_open`, `did_change`) always create the input first, so a
        // missing entry is a programmer error rather than a runtime concern.
        if !self.document_inputs.contains_key(uri) {
            debug_assert!(
                false,
                "publish_diagnostics_for_document called for {} with no DocumentInput; \
                 did_open/did_change should always create one first",
                uri
            );
            tracing::warn!(%uri, "no DocumentInput for diagnostics; skipping");
            return;
        }

        if let Some(diagnostics) = self.compute_diagnostics(uri) {
            self.client
                .publish_diagnostics(uri.clone(), diagnostics, None)
                .await;
        }
    }
}

/// Build the single diagnostic shown for a YAML document that failed to parse.
fn yaml_syntax_error_diagnostic(msg: &str) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 0,
            },
        },
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(tower_lsp::lsp_types::NumberOrString::String(
            "yaml-syntax-error".to_string(),
        )),
        source: Some("hydra-lsp".to_string()),
        message: format!("YAML syntax error: {}", msg),
        ..Default::default()
    }
}

/// An empty `Full` diagnostic report — nothing to show for this document.
fn empty_full_report() -> DocumentDiagnosticReportResult {
    DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(
        RelatedFullDocumentDiagnosticReport::default(),
    ))
}

/// A stable `result_id` for a set of diagnostics, or `None` when there are none.
///
/// `lsp_types::Diagnostic` isn't `Hash`, so we hash a stable JSON serialization.
/// The client echoes this id back as `previous_result_id` on the next pull; an
/// unchanged id lets us answer `Unchanged` and skip re-sending the items.
fn diagnostics_result_id(diags: &[Diagnostic]) -> Option<String> {
    if diags.is_empty() {
        return None;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // Serialization is deterministic for a given value, so equal diagnostic
    // lists hash equally. Fall back to a length-based id if serialization ever
    // fails (it shouldn't for well-formed diagnostics).
    match serde_json::to_vec(diags) {
        Ok(bytes) => bytes.hash(&mut hasher),
        Err(_) => diags.len().hash(&mut hasher),
    }
    Some(format!("{:016x}", hasher.finish()))
}

/// Build a pull-diagnostic report, returning `Unchanged` when the freshly
/// computed `result_id` matches the client's `previous_result_id`.
fn build_diagnostic_report(
    diagnostics: Vec<Diagnostic>,
    previous_result_id: Option<&str>,
) -> DocumentDiagnosticReportResult {
    let result_id = diagnostics_result_id(&diagnostics);
    let report = match &result_id {
        Some(new_id) if Some(new_id.as_str()) == previous_result_id => {
            DocumentDiagnosticReport::Unchanged(RelatedUnchangedDocumentDiagnosticReport {
                related_documents: None,
                unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                    result_id: new_id.clone(),
                },
            })
        }
        _ => DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
            related_documents: None,
            full_document_diagnostic_report: FullDocumentDiagnosticReport {
                result_id,
                items: diagnostics,
            },
        }),
    };
    DocumentDiagnosticReportResult::Report(report)
}

/// The LSP `ServerCancelled` error for a superseded pull-diagnostic round.
///
/// `retrigger_request: true` asks the client to re-pull, which replaces any
/// server-side retry loop with demand-driven retry.
fn server_cancelled_error() -> tower_lsp::jsonrpc::Error {
    tower_lsp::jsonrpc::Error {
        code: tower_lsp::jsonrpc::ErrorCode::ServerError(-32802), // LSP ServerCancelled
        message: "diagnostics superseded by a newer revision".into(),
        data: serde_json::to_value(DiagnosticServerCancellationData {
            retrigger_request: true,
        })
        .ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::tests::TestDb;
    use salsa::{Database as _, Setter};
    use std::time::Duration;

    fn single_thread_pool() -> OnceLock<rayon::ThreadPool> {
        let cell = OnceLock::new();
        let _ = cell.set(
            rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .expect("failed to build test pool"),
        );
        cell
    }

    // --- Test A: spawn_on_pool contract ---

    #[tokio::test]
    async fn spawn_on_pool_completes_normally() {
        let pool = single_thread_pool();
        let outcome = spawn_on_pool(&pool, || 42_i32)
            .await
            .expect("sender dropped");
        assert!(matches!(outcome, PoolOutcome::Completed(42)));
    }

    #[tokio::test]
    async fn spawn_on_pool_captures_panic_without_aborting() {
        // Pre-fix, a panic in the closure would reach rayon (no panic_handler)
        // and abort the whole test process. Reaching the assertion at all is
        // the regression guard; the message check confirms payload capture.
        let pool = single_thread_pool();
        let outcome = spawn_on_pool(&pool, || -> i32 { panic!("boom-42") })
            .await
            .expect("sender dropped");
        match outcome {
            PoolOutcome::Panicked(msg) => assert!(
                msg.contains("boom-42"),
                "expected panic message to contain payload, got {msg:?}"
            ),
            other => panic!("expected Panicked, got {other:?}"),
        }
    }

    // --- Test B: classify_diag_outcome (pure) ---

    #[test]
    fn classify_publishes_on_completion() {
        let diags: Vec<Diagnostic> = vec![Diagnostic::default()];
        assert_eq!(
            classify_diag_outcome(Ok(PoolOutcome::Completed(diags.clone()))),
            DiagAction::Publish(diags)
        );
    }

    #[test]
    fn classify_skips_on_cancellation() {
        assert_eq!(
            classify_diag_outcome(Ok(PoolOutcome::Cancelled)),
            DiagAction::Skip
        );
    }

    #[test]
    fn classify_skips_on_recv_error() {
        // A dropped sender is the only way to obtain a real RecvError.
        let (tx, rx) = tokio::sync::oneshot::channel::<PoolOutcome<Vec<Diagnostic>>>();
        drop(tx);
        let err = rx.blocking_recv().unwrap_err();
        assert_eq!(classify_diag_outcome(Err(err)), DiagAction::Skip);
    }

    #[test]
    fn classify_logs_on_panic() {
        assert_eq!(
            classify_diag_outcome(Ok(PoolOutcome::Panicked("kaboom".to_string()))),
            DiagAction::LogPanic("kaboom".to_string())
        );
    }

    // --- Test C: real salsa cancellation is classified as Cancelled ---

    #[tokio::test]
    async fn salsa_cancellation_is_caught_and_classified() {
        let pool = single_thread_pool();

        let mut db = TestDb::new();
        let input = DocumentInput::new(&db, "v1".to_string(), 1);
        // Snapshot shares storage with `db`; a `&mut db` write cancels queries
        // running on this handle.
        let snapshot = db.clone();

        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel::<()>();

        let rx = spawn_on_pool(&pool, move || {
            started_tx.send(()).expect("main receiver dropped");
            proceed_rx.recv().expect("main sender dropped");
            // Poll the cancellation checkpoint until the concurrent write sets
            // the flag; this unwinds with salsa::Cancelled once observed. The
            // bound + outer timeout keep a mis-choreographed race from hanging.
            for _ in 0..100_000_000_u64 {
                snapshot.unwind_if_revision_cancelled();
                std::thread::yield_now();
            }
            panic!("cancellation flag was never observed");
        });

        started_rx.recv().expect("worker never started");
        // `set_text(&mut db)` blocks in salsa's cancel_others until the snapshot
        // handle is released, so it must run on its own thread.
        let writer = std::thread::spawn(move || {
            input.set_text(&mut db).to("v2".to_string());
        });
        proceed_tx.send(()).expect("worker receiver dropped");

        let outcome = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .expect("timed out waiting for cancellation")
            .expect("sender dropped");
        assert!(
            matches!(outcome, PoolOutcome::Cancelled),
            "expected Cancelled, got {outcome:?}"
        );
        writer.join().expect("writer thread panicked");
    }

    // --- Test C2: parsed_yaml on the diagnostics route is cancel-safe ---

    /// Fix-guard (not a red-first repro): the diagnostics publisher must run its
    /// `yaml_cache::parsed_yaml` call *inside* `spawn_on_pool`, so a concurrent
    /// write that cancels it mid-flight surfaces as `PoolOutcome::Cancelled`
    /// (→ `DiagAction::Skip`) instead of an uncaught `salsa::Cancelled` panic on
    /// the task. This drives the exact parse the fix moves onto the pool:
    /// `parsed_yaml` on a cloned snapshot, cancelled by `set_text(&mut db)` on
    /// another thread. Because every salsa `fetch` begins with a cancellation
    /// check, looping `parsed_yaml` reliably observes the pending flag the
    /// blocked writer sets, exactly as the checkpoint loop in the test above.
    #[tokio::test]
    async fn parsed_yaml_cancellation_on_pool_classifies_as_skip() {
        let pool = single_thread_pool();

        let mut db = TestDb::new();
        let input = DocumentInput::new(&db, "# @hydra\nmodel:\n  _target_: a.B\n".to_string(), 1);
        // Snapshot shares storage with `db`; a `&mut db` write cancels queries
        // running on this handle. `DocumentInput` is a `Copy` salsa id, so the
        // move-closure copy below leaves `input` usable by the writer thread.
        let snapshot = db.clone();

        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel::<()>();

        let rx = spawn_on_pool(&pool, move || {
            started_tx.send(()).expect("main receiver dropped");
            proceed_rx.recv().expect("main sender dropped");
            // Re-run the diagnostics-route parse until the concurrent writer
            // sets the cancellation flag; the next `fetch` then unwinds with
            // `salsa::Cancelled`. The bound + outer timeout keep a
            // mis-choreographed race from hanging.
            for _ in 0..100_000_000_u64 {
                let _ = yaml_cache::parsed_yaml(&snapshot, input);
                std::thread::yield_now();
            }
            panic!("cancellation flag was never observed");
        });

        started_rx.recv().expect("worker never started");
        // `set_text(&mut db)` blocks in salsa's cancel_others until the snapshot
        // handle is released, so it must run on its own thread.
        let writer = std::thread::spawn(move || {
            input
                .set_text(&mut db)
                .to("# @hydra\nmodel:\n  _target_: c.D\n".to_string());
        });
        proceed_tx.send(()).expect("worker receiver dropped");

        let outcome = tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .expect("timed out waiting for cancellation")
            .expect("sender dropped");
        assert!(
            matches!(outcome, PoolOutcome::Cancelled),
            "expected Cancelled, got {outcome:?}"
        );
        // The publisher relies on this exact classification to skip (not clear)
        // diagnostics for a superseded round.
        assert_eq!(classify_diag_outcome(Ok(outcome)), DiagAction::Skip);
        writer.join().expect("writer thread panicked");
    }

    // --- Pull-diagnostic report helpers ---

    fn sample_diagnostic(message: &str) -> Diagnostic {
        Diagnostic {
            message: message.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn diagnostics_result_id_is_none_for_empty_and_stable_for_equal() {
        assert_eq!(diagnostics_result_id(&[]), None);

        let a = vec![sample_diagnostic("boom")];
        let b = vec![sample_diagnostic("boom")];
        let c = vec![sample_diagnostic("different")];
        assert_eq!(diagnostics_result_id(&a), diagnostics_result_id(&b));
        assert_ne!(diagnostics_result_id(&a), diagnostics_result_id(&c));
    }

    #[test]
    fn build_diagnostic_report_full_then_unchanged() {
        let diags = vec![sample_diagnostic("boom")];
        let id = diagnostics_result_id(&diags).expect("non-empty diags have an id");

        // No previous id → Full report carrying the items and the fresh id.
        match build_diagnostic_report(diags.clone(), None) {
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(full)) => {
                assert_eq!(full.full_document_diagnostic_report.items, diags);
                assert_eq!(
                    full.full_document_diagnostic_report.result_id,
                    Some(id.clone())
                );
            }
            other => panic!("expected Full, got {other:?}"),
        }

        // Matching previous id → Unchanged, echoing the id, dropping the items.
        match build_diagnostic_report(diags, Some(&id)) {
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(u)) => {
                assert_eq!(u.unchanged_document_diagnostic_report.result_id, id);
            }
            other => panic!("expected Unchanged, got {other:?}"),
        }
    }

    #[test]
    fn empty_diagnostics_always_report_full() {
        // An empty set has no result_id, so even a repeat pull stays Full (never
        // Unchanged), correctly clearing any prior diagnostics on the client.
        match build_diagnostic_report(Vec::new(), Some("stale-id")) {
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(full)) => {
                assert!(full.full_document_diagnostic_report.items.is_empty());
                assert_eq!(full.full_document_diagnostic_report.result_id, None);
            }
            other => panic!("expected Full, got {other:?}"),
        }
    }

    #[test]
    fn server_cancelled_error_uses_lsp_code_and_retrigger() {
        let err = server_cancelled_error();
        assert!(matches!(
            err.code,
            tower_lsp::jsonrpc::ErrorCode::ServerError(-32802)
        ));
        let data = err.data.expect("cancellation carries data");
        let cancellation: DiagnosticServerCancellationData =
            serde_json::from_value(data).expect("valid cancellation data");
        assert!(cancellation.retrigger_request);
    }

    // --- Test D: site-packages gating in bump_site_packages_pth_roots ---

    /// Build a minimal on-disk venv so `discover_python_environment` returns a
    /// real site-packages directory, and register its `FileRoot`s exactly as
    /// production does (via `python_cache::site_packages_paths`). Discovery uses
    /// a real `OsSystem` (not the db's system), so this must be an actual
    /// filesystem layout, not an in-memory one — it mirrors `python_analyzer`'s
    /// in-memory `create_mock_venv`. The returned `TempDir` must be kept alive
    /// for the venv to persist.
    fn session_with_real_venv() -> (Session, std::path::PathBuf, tempfile::TempDir) {
        use std::fs;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let venv = tmp.path().to_path_buf();
        let version = "3.11";
        let (exe, site_packages) = if cfg!(target_os = "windows") {
            (
                venv.join(r"Scripts\python.exe"),
                venv.join(r"Lib\site-packages"),
            )
        } else {
            (
                venv.join("bin/python"),
                venv.join(format!("lib/python{version}/site-packages")),
            )
        };
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, "").unwrap();
        fs::create_dir_all(&site_packages).unwrap();
        fs::write(
            venv.join("pyvenv.cfg"),
            format!("home = {}\nversion = {version}\n", venv.display()),
        )
        .unwrap();

        let db = HydraDatabase::new(SystemPath::new(venv.to_str().unwrap()));
        let python_config = PythonConfig::new(
            &db,
            Some(venv.to_string_lossy().into_owned()),
            Some(venv.to_string_lossy().into_owned()),
        );
        // Register the discovered site-packages directories as `FileRoot`s,
        // exactly as the resolver does on its first query. Without this the
        // event side has no root to bump. Discovery canonicalizes the sys.prefix
        // (e.g. macOS `/var` → `/private/var`), so the registered root path is
        // the canonical one — return that so the simulated watch events route to
        // it lexically, exactly as real events (fired under the canonical root
        // the watcher registered) do.
        python_cache::site_packages_paths(&db, python_config);
        let site_packages = std::fs::canonicalize(&site_packages).expect("canonicalize venv");
        let session = Session {
            db: parking_lot::Mutex::new(db),
            python_config,
        };
        (session, site_packages, tmp)
    }

    /// A `.pth` change in the real site-packages dir bumps its root; a change in
    /// an unrelated directory in the same batch is skipped. Unrelated `.pth`
    /// files have no registered root, so they never trigger invalidation.
    #[test]
    fn bump_tracks_only_registered_site_packages_roots() {
        let (session, site_packages, _tmp) = session_with_real_venv();
        let unrelated = site_packages.parent().unwrap().join("not-site-packages");
        std::fs::create_dir_all(&unrelated).unwrap();

        let mut dirs = HashSet::new();
        dirs.insert(site_packages);
        dirs.insert(unrelated);

        let bumped = session.bump_site_packages_pth_roots(&dirs);
        assert_eq!(bumped, 1, "only the registered site-packages root bumps");
    }

    /// A `.pth` change with no registered root bumps nothing — an unrelated
    /// `foo.pth` anywhere in the watched tree is inert.
    #[test]
    fn bump_skips_unrelated_dir_entirely() {
        let (session, site_packages, _tmp) = session_with_real_venv();
        let unrelated = site_packages.parent().unwrap().join("scratch");
        std::fs::create_dir_all(&unrelated).unwrap();

        let mut dirs = HashSet::new();
        dirs.insert(unrelated);

        assert_eq!(session.bump_site_packages_pth_roots(&dirs), 0);
    }

    /// Repeated `.pth` events for the same real dir bump the root revision each
    /// time so `parse_pth_files` re-scans.
    #[test]
    fn bump_bumps_root_revision_across_repeated_events() {
        let (session, site_packages, _tmp) = session_with_real_venv();
        let mut dirs = HashSet::new();
        dirs.insert(site_packages.clone());

        let sys_path = SystemPath::from_std_path(&site_packages).unwrap();
        let rev0 = {
            let db = session.db.lock();
            db.files().root(&*db, sys_path).unwrap().revision(&*db)
        };

        assert_eq!(session.bump_site_packages_pth_roots(&dirs), 1);
        let rev1 = {
            let db = session.db.lock();
            db.files().root(&*db, sys_path).unwrap().revision(&*db)
        };

        assert_eq!(session.bump_site_packages_pth_roots(&dirs), 1);
        let rev2 = {
            let db = session.db.lock();
            db.files().root(&*db, sys_path).unwrap().revision(&*db)
        };

        assert!(
            rev1 != rev0 && rev2 != rev1,
            "each event must bump the root revision so the directory scan is recomputed"
        );
    }

    /// Routing is lexical, so a candidate path differing from the registered
    /// root only by a trailing separator must still match (`SystemPath::absolute`
    /// strips the trailing separator before the prefix-tree lookup). Regression
    /// guard for the path symmetry the `.pth` invalidation relies on.
    #[test]
    fn bump_matches_site_packages_despite_trailing_separator() {
        let (session, site_packages, _tmp) = session_with_real_venv();
        let mut with_sep = site_packages.into_os_string();
        with_sep.push(std::path::MAIN_SEPARATOR.to_string());

        let mut dirs = HashSet::new();
        dirs.insert(std::path::PathBuf::from(with_sep));

        assert_eq!(
            session.bump_site_packages_pth_roots(&dirs),
            1,
            "a trailing-separator variant of the site-packages path must still match"
        );
    }
}
