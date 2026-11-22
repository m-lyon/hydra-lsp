//! The `LanguageServer` implementation: one method per LSP request or
//! notification, delegating to the rest of the backend.

use std::time::Instant;
use tower_lsp::LanguageServer;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use ruff_db::files::{File, FilePath};
use ruff_db::system::SystemPath;

use super::HydraLspBackend;
use super::capabilities::{ClientFeatures, server_capabilities};
use super::diagnostics::{
    DiagAction, build_diagnostic_report, classify_diag_outcome, empty_full_report,
    server_cancelled_error, validate_input,
};
use super::pools::{PoolOutcome, spawn_on_pool};
use super::presentation::{definition_ranges, signature_information};
use super::session::SessionSnapshot;
use crate::database::HydraDatabase;
use crate::diagnostics;
use crate::python::cache::{self as python_cache, ResolvedDefinition, TargetString};
use crate::python::format::{format_class, format_function, format_method};
use crate::python::model::{DefinitionInfo, FunctionSignature, ResolveError};
use crate::python::typeshed::cached_vendored_path;
use crate::yaml_cache::{self, DocumentInput};
use crate::yaml_parser::{
    ARGS_KEY, CONVERT_KEY, CompletionContext, ConvertMode, HydraSemanticToken, PARTIAL_KEY,
    RECURSIVE_KEY, YamlParser,
};

/// Glob applied to every watched root — the workspace folders (via a plain
/// string pattern) and each out-of-workspace Python search path (via relative
/// patterns; see `initialized`). Single source of truth for on-disk change
/// notifications; the extension filter in `did_change_watched_files` must stay
/// aligned with the extensions here.
const WATCHED_PY_GLOB: &str = "**/*.{py,pyi,pth}";

impl HydraLspBackend {
    /// Run Python definition lookup on a blocking thread using a database snapshot,
    /// and return what `project` takes from the definition there, while the
    /// database snapshot is still available.
    ///
    /// Takes a `SessionSnapshot` (cheap — salsa shares cached data via Arc) and
    /// moves it to `tokio::task::spawn_blocking` so that expensive Python
    /// analysis (module resolution, file parsing) doesn't block the async runtime.
    ///
    /// On cache hits the blocking thread returns almost immediately; on misses
    /// it performs the full analysis without holding any locks.
    async fn spawn_definition_lookup<T: Send + 'static>(
        &self,
        target_value: String,
        project: impl FnOnce(&HydraDatabase, &ResolvedDefinition) -> T + Send + 'static,
    ) -> std::result::Result<T, LookupError> {
        let Some(snapshot) = self.snapshot() else {
            return Err(LookupError::NotInitialized);
        };
        let SessionSnapshot { db, python_config } = snapshot;

        let received = spawn_on_pool(&self.latency_pool, db, move |db| {
            let start = Instant::now();
            let target = TargetString::new(db, target_value);
            let cached = python_cache::resolved_definition(db, python_config, target);
            let result = cached
                .get()
                .map(|definition| project(db, definition))
                .map_err(|err| LookupError::Resolve(err.clone()));
            tracing::debug!(
                elapsed_us = start.elapsed().as_micros() as u64,
                target = target.value(db),
                ok = result.is_ok(),
                "definition lookup"
            );
            result
        })
        .await;
        // A dropped sender (e.g. pool shutdown) counts as cancellation: the
        // request is stale.
        match PoolOutcome::from_received(received) {
            PoolOutcome::Completed(result) => result,
            PoolOutcome::Cancelled => Err(LookupError::Superseded),
            PoolOutcome::Panicked(msg) => {
                tracing::error!(%msg, "definition lookup panicked");
                Err(LookupError::Panicked(msg))
            }
        }
    }

    /// Store `text` as the document's contents, then refresh its diagnostics.
    /// Returns `false`, having logged, when called before `initialize`.
    /// `context` names the calling handler in that log.
    fn update_document(&self, uri: &Url, text: &str, version: i32, context: &'static str) -> bool {
        let Some(input) = self.get_or_create_input(uri, text, version, context) else {
            return false;
        };
        self.refresh_after_edit(uri, input, context);
        true
    }

    async fn hover_response(&self, params: HoverParams) -> Result<Option<Hover>> {
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

        self.outbox.log(
            MessageType::LOG,
            format!("Found target at position: {:?}", hydra_object),
        );

        // Extract Python definition info on a blocking thread (cached + non-blocking)
        let extract_result = self
            .spawn_definition_lookup(hydra_object.target.value.clone(), |_, def| def.clone())
            .await;

        match extract_result {
            Ok(ResolvedDefinition {
                mut definition_info,
                ..
            }) => {
                diagnostics::narrow_overloads(&mut definition_info, hydra_object);
                let hover_content = match definition_info {
                    DefinitionInfo::Function(sig) => format_function(&sig),
                    DefinitionInfo::Class(class_info) => format_class(&class_info),
                    DefinitionInfo::Method(method_info) => format_method(&method_info),
                };
                let range = hydra_object.target_range();

                Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: hover_content,
                    }),
                    range: Some(range),
                }))
            }
            Err(e) => {
                // If Python analysis fails, don't show any hover, but log it
                self.log_lookup_failure(&e, None);
                Ok(None)
            }
        }
    }

    async fn signature_help_response(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
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
        let Some((target_value, param_context)) = content.target_for_parameter_line(position)
        else {
            return Ok(None);
        };
        let Some(hydra_object) = content.hydra_object_for_parameter_line(position) else {
            return Ok(None);
        };

        // Extract Python definition info on a blocking thread (cached + non-blocking)
        let extract_result = self
            .spawn_definition_lookup(target_value.clone(), |_, def| def.clone())
            .await;

        match extract_result {
            Ok(ResolvedDefinition {
                definition_info, ..
            }) => {
                let implicit_param = definition_info.implicit_param();
                let label_prefix = match &definition_info {
                    DefinitionInfo::Function(sig) => sig.name.clone(),
                    DefinitionInfo::Class(class_info) => class_info.name.clone(),
                    DefinitionInfo::Method(method_info) => {
                        format!("{}.{}", method_info.class_name, method_info.signature.name)
                    }
                };
                let call_signature = definition_info.call_signature();

                // An overloaded target lists every overload, with the first one
                // the YAML node's arguments match made active. Otherwise there is just
                // the one signature, or a bare `Name()` for a class with no constructor
                // at all.
                let (shown, active_signature): (Vec<Option<&FunctionSignature>>, usize) =
                    match call_signature {
                        Some(sig) if !sig.overloads.is_empty() => (
                            sig.overloads.iter().map(Some).collect(),
                            diagnostics::matching_overloads(hydra_object, sig, implicit_param)
                                .first()
                                .copied()
                                .unwrap_or(0),
                        ),
                        other => (vec![other], 0),
                    };

                let signatures: Vec<SignatureInformation> = shown
                    .into_iter()
                    .map(|sig| {
                        signature_information(&label_prefix, sig, implicit_param, &param_context)
                    })
                    .collect();
                let active_parameter = signatures[active_signature].active_parameter;

                Ok(Some(SignatureHelp {
                    signatures,
                    active_signature: Some(active_signature as u32),
                    active_parameter,
                }))
            }
            Err(e) => {
                self.log_lookup_failure(&e, Some("Python analysis failed for signature help"));
                Ok(None)
            }
        }
    }

    async fn goto_definition_response(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
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

        let narrow_against = target_info.clone();
        let extract_result = self
            .spawn_definition_lookup(target_info.target.value.clone(), move |db, def| {
                if let Some(source) = &def.source {
                    let path = source
                        .file
                        .path(db)
                        .as_system_path()
                        .map(|p| p.to_path_buf());
                    return (path, source.ranges.clone());
                }
                let mut definition_info = def.definition_info.clone();
                diagnostics::narrow_overloads(&mut definition_info, &narrow_against);
                let path = match def.file.path(db) {
                    FilePath::System(path) => Some(path.to_path_buf()),
                    FilePath::Vendored(path) => cached_vendored_path(db, path),
                    FilePath::SystemVirtual(_) => None,
                };
                (path, definition_ranges(&definition_info))
            })
            .await;
        let (file_path, ranges) = match extract_result {
            Ok((Some(file_path), ranges)) => (file_path, ranges),
            Ok((None, _)) => {
                tracing::debug!("goto_definition: the definition has no file to open");
                return Ok(None);
            }
            Err(e) => {
                self.log_lookup_failure(&e, None);
                return Ok(None);
            }
        };

        // Convert file path to URI
        let target_uri = match Url::from_file_path(file_path.as_std_path()) {
            Ok(uri) => uri,
            Err(_) => {
                self.outbox.log(
                    MessageType::ERROR,
                    format!("Could not convert path to URI: {file_path}"),
                );
                return Ok(None);
            }
        };

        let mut locations: Vec<Location> = ranges
            .into_iter()
            .map(|range| Location {
                uri: target_uri.clone(),
                range: range.into(),
            })
            .collect();
        Ok(Some(if locations.len() == 1 {
            GotoDefinitionResponse::Scalar(locations.remove(0))
        } else {
            GotoDefinitionResponse::Array(locations)
        }))
    }

    /// Log a failed `spawn_definition_lookup` for hover, signature help or
    /// go-to-definition, which then return no result.
    fn log_lookup_failure(&self, err: &LookupError, context: Option<&str>) {
        match (err, context) {
            (LookupError::Resolve(ResolveError::InvalidFormat(_)), _) => {
                self.outbox.log(MessageType::ERROR, err)
            }
            (_, Some(context)) => self
                .outbox
                .log(MessageType::WARNING, format!("{context}: {err}")),
            (_, None) => self.outbox.log(MessageType::WARNING, err),
        }
    }
}

/// Why `spawn_definition_lookup` returned no definition.
#[derive(Debug)]
enum LookupError {
    /// The target itself did not resolve.
    Resolve(ResolveError),
    /// `initialize` has not built the session yet.
    NotInitialized,
    /// A newer edit cancelled the lookup.
    Superseded,
    Panicked(String),
}

impl std::fmt::Display for LookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resolve(err) => err.fmt(f),
            Self::NotInitialized => f.write_str("session not initialized"),
            Self::Superseded => f.write_str("definition lookup superseded by a newer edit"),
            Self::Panicked(msg) => write!(f, "definition lookup panicked: {msg}"),
        }
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

        let client = ClientFeatures::from_capabilities(&params.capabilities);
        // `set` cannot fail: `initialize` is the sole writer, once.
        let _ = self.client_features.set(client);

        self.outbox.log(
            MessageType::INFO,
            format!(
                "Hydrust Server initializing with options: {:?}",
                params.initialization_options
            ),
        );

        let num_threads = self.apply_settings(params.initialization_options.as_ref());
        self.build_pools(num_threads)?;

        let interpreter_path = self.settings.read().python_interpreter.clone();
        self.initialize_session(workspace_root, interpreter_path)?;

        Ok(InitializeResult {
            capabilities: server_capabilities(&client),
            server_info: Some(ServerInfo {
                name: "hydrust".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        // Register file watchers dynamically so the server owns the single
        // source of truth for which paths trigger `did_change_watched_files`
        // (see `WATCHED_PY_GLOB`). The client creates the watchers on its side
        // in response and forwards matching events back to us.
        if self.negotiated().watched_files {
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
            if self.client_features().relative_patterns {
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
                self.outbox.log(
                    MessageType::WARNING,
                    "Client lacks relative-pattern watcher support; on-disk changes to \
                         out-of-workspace site-packages/.pth dirs will not invalidate caches",
                );
            }

            let registration = Registration {
                id: "hydra-watched-files".to_string(),
                method: "workspace/didChangeWatchedFiles".to_string(),
                register_options: Some(
                    serde_json::to_value(DidChangeWatchedFilesRegistrationOptions { watchers })
                        .expect("watcher registration options serialize"),
                ),
            };
            self.outbox.register_capability(vec![registration]);
        } else {
            self.outbox.log(
                MessageType::WARNING,
                "Client lacks workspace/didChangeWatchedFiles dynamic registration; \
                     on-disk Python/.pth changes will not invalidate caches",
            );
        }

        self.outbox
            .log(MessageType::INFO, "Hydrust Server initialized");
    }

    async fn shutdown(&self) -> Result<()> {
        // The client is still reading at this point, so give whatever is queued
        // a chance to arrive before `exit` tears the runtime down. Bounded, so a
        // client that has stopped reading cannot hold up the exit.
        self.outbox.flush().await;
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        let version = params.text_document.version;

        if self.update_document(&uri, &text, version, "did_open") {
            self.outbox
                .log(MessageType::INFO, format!("Document opened: {}", uri));
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;

        // Full sync: take the first change which is the entire document
        if let Some(change) = params.content_changes.into_iter().next()
            && self.update_document(&uri, &change.text, version, "did_change")
        {
            self.outbox
                .log(MessageType::INFO, format!("Document changed: {}", uri));
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.outbox.log(
            MessageType::INFO,
            format!("Document saved: {}", params.text_document.uri),
        );
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        // The client tells us when files in the workspace change on disk
        // (typically Python sources outside the editor). For each changed
        // path we call `File::sync_path`, which bumps the per-file revision
        // inside ruff_db so that any salsa query reading that file's
        // `source_text` is invalidated on the next request. A created or
        // deleted file also bumps its ancestor directories, whose listings
        // module resolution and the `.pth` scan read.
        //
        // We filter to Python analysis inputs before locking: `.py`, `.pyi`,
        // and watched `.pth` files all participate in analysis. These extensions
        // must stay aligned with `WATCHED_PY_GLOB`.
        if params.changes.is_empty() {
            return;
        }
        let tracked_paths: Vec<(std::path::PathBuf, FileChangeType)> = params
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
                    Some("py") | Some("pyi") | Some("pth") => Some((path, change.typ)),
                    _ => None,
                }
            })
            .collect();
        if tracked_paths.is_empty() {
            tracing::debug!(
                changed = params.changes.len(),
                "watched files changed; no python analysis inputs to sync"
            );
            return;
        }
        let synced = self
            .with_session("did_change_watched_files", |s| {
                let mut synced = 0usize;
                let mut db = s.db.lock();
                // The reported path is synced as is. ty canonicalizes
                // site-packages roots, so a `.pth` change reported through a
                // symlink (e.g. a symlinked `.venv`) does not invalidate the
                // editable scan. Known gap, left unfixed as niche.
                for (std_path, typ) in &tracked_paths {
                    let Some(sys_path) = SystemPath::from_std_path(std_path) else {
                        continue;
                    };
                    sync_watched_path(&mut *db, sys_path, *typ != FileChangeType::CHANGED);
                    synced += 1;
                }
                synced
            })
            .unwrap_or(0);
        tracing::debug!(
            changed = params.changes.len(),
            synced,
            "watched files changed; synced python analysis inputs"
        );

        // The sync above invalidated Python-dependent salsa queries but did not
        // refresh diagnostics, now we refresh diagnostics, unless none to refresh.
        if !self.settings.read().features.diagnostics || synced == 0 {
            return;
        }

        // Pull clients with refresh support: a single `workspace/diagnostic/refresh`
        // nudges the client to re-pull every open document — no per-doc work here.
        let negotiated = self.negotiated();
        if negotiated.pull_diagnostics && negotiated.diagnostic_refresh {
            self.outbox.workspace_diagnostic_refresh();
            return;
        }

        // A pull client without `workspace/diagnostic/refresh` has no
        // server-driven refresh channel: we cannot nudge it to re-pull, and
        // pushing would duplicate diagnostics into a second client-side
        // collection. Diagnostics derived from the changed Python file stay
        // stale until the client re-pulls on its own (next edit, open, or
        // focus change).
        if negotiated.pull_diagnostics {
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
            .with_session("did_change_watched_files", |s| {
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
            self.publish_diagnostics_if_needed(&uri);
            // Nothing in the loop body awaits any more, and `compute_diagnostics`
            // holds the db lock on the runtime thread, so without this the whole
            // refresh runs before stdin is read again.
            tokio::task::yield_now().await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;

        // Soft-close the salsa input: clear the source text so salsa drops
        // the per-document `String` while the input slot is retained for
        // reuse on a subsequent `did_open` for the same URI. Salsa exposes
        // no input-deletion API, so this is the minimum-footprint
        // equivalent (matches `ruff_db::files::VirtualFile::close`).
        if let Some(input) = self.document_input(&uri) {
            self.with_session("did_close", |s| {
                let mut db = s.db.lock();
                input.close(&mut *db);
            });
        }

        // Clear any diagnostics we published while the document was open, so
        // they do not linger in the client after the document is closed.
        if self.settings.read().features.diagnostics {
            self.clear_diagnostics_if_needed(&uri);
        }

        self.outbox
            .log(MessageType::INFO, format!("Document closed: {}", uri));
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        timed("hover", self.hover_response(params)).await
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        if !self.settings.read().features.completion {
            return Ok(None);
        }
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let Some(input) = self.document_input(&uri) else {
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
                tracing::debug!(%partial, "target completion requested");

                Ok(None) // Placeholder: no completions yet
            }
            CompletionContext::ParameterKey { target, partial } => {
                // TODO: Resolve target and get parameter completions
                tracing::debug!(%target, %partial, "parameter completion requested");

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

                tracing::debug!(%target, %parameter, %partial, "parameter value completion requested");

                Ok(None) // Placeholder: no completions yet
            }
            crate::yaml_parser::CompletionContext::Unknown => Ok(None),
        }
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        timed("signature_help", self.signature_help_response(params)).await
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        timed("goto_definition", self.goto_definition_response(params)).await
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        if !self.settings.read().features.semantic_tokens {
            return Ok(None);
        }
        let uri = params.text_document.uri;
        let Some(input) = self.document_input(&uri) else {
            return Ok(None);
        };

        self.outbox
            .log(MessageType::INFO, "Generating semantic tokens".to_string());

        let Some(SessionSnapshot { db, .. }) = self.snapshot() else {
            return Ok(None);
        };

        // Runs on a snapshot rather than inline under `s.db.lock()`. The
        // `semantic_tokens` query depends on `parsed_yaml`, so the first
        // request after an edit needs a full YAML parse — holding the db lock
        // across that would block the next `did_change` write, which would then
        // immediately render the parse result stale.
        let outcome = spawn_on_pool(&self.latency_pool, db, move |db| {
            if !yaml_cache::is_hydra_file(db, input) {
                return None;
            }
            let tokens = yaml_cache::semantic_tokens(db, input);
            Some(HydraSemanticToken::to_lsp_tokens(tokens))
        })
        .await;

        let data = match PoolOutcome::from_received(outcome) {
            PoolOutcome::Completed(Some(data)) => data,
            // Not a Hydra file.
            PoolOutcome::Completed(None) => return Ok(None),
            // A concurrent edit bumped the revision out from under the query,
            // or (the same situation reached a different way) the pool is not
            // built yet, so the sender was dropped. Both want a re-request.
            PoolOutcome::Cancelled => return Err(content_modified_error()),
            PoolOutcome::Panicked(msg) => {
                tracing::error!(%msg, "semantic_tokens task panicked");
                return Ok(None);
            }
        };

        self.outbox.log(
            MessageType::INFO,
            format!("Generated {} semantic tokens", data.len()),
        );

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
        let Some(input) = self.document_input(&uri) else {
            return Ok(empty_full_report());
        };

        let Some(SessionSnapshot {
            db: db_snapshot,
            python_config,
        }) = self.snapshot()
        else {
            return Ok(empty_full_report());
        };
        let disabled_rules = self.settings.read().disabled_rules.clone();

        // The push path's parse + validate, but on the worker pool, inside the
        // pool's catch_unwind so a `salsa::Cancelled` unwind is caught rather
        // than aborting the process.
        let join_result = spawn_on_pool(&self.worker_pool, db_snapshot, move |db| {
            if !yaml_cache::is_hydra_file(db, input) {
                return Vec::new();
            }
            validate_input(db, input, &disabled_rules, python_config)
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

/// Await `work`, then log at debug level how long it took under `name`.
async fn timed<T>(name: &'static str, work: impl std::future::Future<Output = T>) -> T {
    let start = Instant::now();
    let result = work.await;
    tracing::debug!(elapsed_ms = start.elapsed().as_millis() as u64, "{name}");
    result
}

fn sync_watched_path(db: &mut dyn ruff_db::Db, path: &SystemPath, created_or_deleted: bool) {
    File::sync_path(db, path);
    if created_or_deleted {
        let absolute = SystemPath::absolute(path, db.system().current_directory());
        for ancestor in absolute.ancestors().skip(1) {
            File::sync_path_only(db, ancestor);
        }
    }
}

/// LSP `ContentModified`: the revision this request was answered against has
/// been superseded by an edit.
///
/// Distinct from [`server_cancelled_error`], which is diagnostics-specific — it
/// carries `DiagnosticServerCancellationData` and only `textDocument/diagnostic`
/// knows how to read it. For every other pull-based request `ContentModified` is
/// the spec's answer, and clients re-request against the new revision.
fn content_modified_error() -> tower_lsp::jsonrpc::Error {
    tower_lsp::jsonrpc::Error {
        code: tower_lsp::jsonrpc::ErrorCode::ServerError(-32801), // LSP ContentModified
        message: "document changed while the request was in flight".into(),
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_modified_error_uses_lsp_code() {
        let err = content_modified_error();
        assert!(matches!(
            err.code,
            tower_lsp::jsonrpc::ErrorCode::ServerError(-32801)
        ));
        // Unlike `server_cancelled_error`, this one is generic: no
        // diagnostics-specific payload rides along.
        assert!(err.data.is_none());
    }
}
