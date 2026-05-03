use dashmap::DashMap;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tower_lsp::jsonrpc::{Error, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use ruff_db::files::File;
use ruff_db::system::SystemPath;
use salsa::{Database as _, Setter};

use crate::database::HydraDatabase;
use crate::diagnostics::{self, DiagnosticRule};
use crate::document::DocumentStore;
use crate::python_analyzer::{
    DefinitionInfo, ParameterInfo, PythonAnalyzer, normalize_pth_inventory_key,
};
use crate::python_cache::{self, PthInventory, PythonConfig, TargetString};
use crate::yaml_cache::{self, DocumentInput};
use crate::yaml_parser::{
    ARGS_KEY, CONVERT_KEY, CompletionContext, ConvertMode, HydraSemanticToken, PARTIAL_KEY,
    RECURSIVE_KEY, ResolvedParameterContext, YamlParser,
};

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

/// Feature toggle settings for individual LSP capabilities.
#[derive(Debug, Clone, Copy)]
pub struct FeatureToggles {
    pub hover: bool,
    pub completion: bool,
    pub signature_help: bool,
    pub goto_definition: bool,
    pub semantic_tokens: bool,
    pub diagnostics: bool,
}

impl Default for FeatureToggles {
    fn default() -> Self {
        Self {
            hover: true,
            completion: true,
            signature_help: true,
            goto_definition: true,
            semantic_tokens: true,
            diagnostics: true,
        }
    }
}

impl FeatureToggles {
    /// Parse feature toggles from a JSON settings object.
    fn from_json(settings: &serde_json::Value) -> Self {
        fn bool_setting(settings: &serde_json::Value, key: &str) -> bool {
            settings.get(key).and_then(|v| v.as_bool()).unwrap_or(true)
        }
        Self {
            hover: bool_setting(settings, "enableHover"),
            completion: bool_setting(settings, "enableCompletion"),
            signature_help: bool_setting(settings, "enableSignatureHelp"),
            goto_definition: bool_setting(settings, "enableGotoDefinition"),
            semantic_tokens: bool_setting(settings, "enableSemanticTokens"),
            diagnostics: bool_setting(settings, "enableDiagnostics"),
        }
    }

    /// Return names of disabled features, if any.
    fn disabled_names(&self) -> Vec<&'static str> {
        [
            (!self.hover, "hover"),
            (!self.completion, "completion"),
            (!self.signature_help, "signatureHelp"),
            (!self.goto_definition, "gotoDefinition"),
            (!self.semantic_tokens, "semanticTokens"),
            (!self.diagnostics, "diagnostics"),
        ]
        .into_iter()
        .filter(|(disabled, _)| *disabled)
        .map(|(_, name)| name)
        .collect()
    }
}

/// Server-wide settings for Hydrust Server.
#[derive(Debug, Default)]
pub struct Settings {
    pub python_interpreter: Option<String>,
    pub disabled_rules: HashSet<DiagnosticRule>,
    pub features: FeatureToggles,
}

#[derive(Clone)]
struct AnalysisState {
    db: HydraDatabase,
    python_config: PythonConfig,
    pth_inventories: HashMap<String, PthInventory>,
}

impl AnalysisState {
    fn ensure_pth_inventory(&mut self, directory: &std::path::Path) -> PthInventory {
        // Normalized so the watched-event side and the discovery side agree
        // even when the path differs by symlink, case, or trailing separator.
        // See `python_analyzer::normalize_pth_inventory_key`.
        let key = normalize_pth_inventory_key(directory);
        if let Some(inventory) = self.pth_inventories.get(&key) {
            return *inventory;
        }

        let inventory = PthInventory::new(&self.db, key.clone(), 0);
        let mut inventories = self.python_config.pth_inventories(&self.db).clone();
        inventories.push(inventory);
        self.python_config
            .set_pth_inventories(&mut self.db)
            .to(inventories);
        self.pth_inventories.insert(key, inventory);
        inventory
    }
}

pub struct HydraLspBackend {
    pub client: Client,
    pub documents: Arc<DocumentStore>,
    pub settings: Arc<RwLock<Settings>>,
    /// Salsa database for incremental caching.
    /// Uses `Mutex` because salsa's Storage is `Send` but not `Sync`.
    analysis: Arc<parking_lot::Mutex<Option<AnalysisState>>>,
    /// Map from document URI to its salsa input handle.
    ///
    /// Lock order: `self.analysis` MUST be acquired before any
    /// `document_inputs` shard guard. `get_or_create_input` takes the
    /// analysis lock first and then enters the shard inside the closure
    /// to keep concurrent `did_open` calls for the same URI from creating
    /// distinct inputs. Any code path that already holds a shard guard
    /// must drop it before locking `analysis`.
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
}

impl std::fmt::Debug for HydraLspBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HydraLspBackend")
            .field("documents", &self.documents)
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl HydraLspBackend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Arc::new(DocumentStore::new()),
            settings: Arc::new(RwLock::new(Settings::default())),
            analysis: Arc::new(parking_lot::Mutex::new(None)),
            document_inputs: DashMap::new(),
        }
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

    fn initialize_analysis_state(
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
        let python_config = PythonConfig::new(&db, Some(workspace_root), interpreter, vec![]);
        *self.analysis.lock() = Some(AnalysisState {
            db,
            python_config,
            pth_inventories: HashMap::new(),
        });
        Ok(())
    }

    fn snapshot_analysis(&self) -> Option<AnalysisState> {
        self.analysis.lock().as_ref().cloned()
    }

    /// Run `f` with a borrow of the initialized analysis state, returning
    /// `None` (and logging a warning that names `context`) if the state has
    /// not yet been built by `initialize`. The LSP protocol requires the
    /// client to send `initialize` first, so reaching `None` means a
    /// misbehaving client — notification handlers should ignore the event
    /// rather than panic.
    fn with_analysis<T>(
        &self,
        context: &'static str,
        f: impl FnOnce(&AnalysisState) -> T,
    ) -> Option<T> {
        let analysis = self.analysis.lock();
        if analysis.is_none() {
            // Drop the guard before logging so a slow tracing subscriber
            // (file write, stderr lock contention) does not stall every
            // other handler that needs the analysis mutex.
            drop(analysis);
            tracing::warn!(context, "analysis state not initialized; ignoring");
            return None;
        }
        Some(f(analysis.as_ref().expect("checked Some above")))
    }

    fn with_analysis_mut<T>(
        &self,
        context: &'static str,
        f: impl FnOnce(&mut AnalysisState) -> T,
    ) -> Option<T> {
        let mut analysis = self.analysis.lock();
        if analysis.is_none() {
            drop(analysis);
            tracing::warn!(context, "analysis state not initialized; ignoring");
            return None;
        }
        Some(f(analysis.as_mut().expect("checked Some above")))
    }

    /// Run Python definition lookup on a blocking thread using a database snapshot.
    ///
    /// Clones the database (cheap — salsa shares cached data via Arc) and moves
    /// the clone to `tokio::task::spawn_blocking` so that expensive Python
    /// analysis (module resolution, file parsing) doesn't block the async runtime.
    ///
    /// On cache hits the blocking thread returns almost immediately; on misses
    /// it performs the full analysis without holding any locks.
    async fn spawn_definition_lookup(
        &self,
        target_value: String,
    ) -> anyhow::Result<(DefinitionInfo, std::path::PathBuf, String, String)> {
        let Some(analysis) = self.snapshot_analysis() else {
            anyhow::bail!("analysis state not initialized")
        };
        let AnalysisState {
            db, python_config, ..
        } = analysis;

        tokio::task::spawn_blocking(move || {
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
        .map_err(|e| anyhow::anyhow!("Task failed: {}", e))?
    }

    /// Get or create a `DocumentInput` for a given URI.
    ///
    /// Holds the analysis lock across the DashMap entry so that concurrent
    /// callers for the same URI cannot both observe a vacant entry and
    /// create distinct inputs (which would orphan one in salsa storage and
    /// let stale text leak through). Lock order is `self.analysis` → shard
    /// guard.
    ///
    /// Returns `None` when analysis state is not yet initialized; callers
    /// (notification handlers) should treat that as a no-op.
    fn get_or_create_input(&self, uri: &Url, text: &str, version: i32) -> Option<DocumentInput> {
        self.with_analysis_mut("opening documents", |analysis| {
            match self.document_inputs.entry(uri.clone()) {
                dashmap::Entry::Occupied(occ) => {
                    let input = *occ.get();
                    input.set_text(&mut analysis.db).to(text.to_string());
                    input.set_version(&mut analysis.db).to(version);
                    input
                }
                dashmap::Entry::Vacant(vac) => {
                    let input = DocumentInput::new(&analysis.db, text.to_string(), version);
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

        let interpreter_path = self.settings.read().python_interpreter.clone();
        self.initialize_analysis_state(workspace_root, interpreter_path)?;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
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
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "hydra-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
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

        self.documents.insert(uri.clone(), text.clone(), version);

        // Create a salsa input for this document. Returns None (and logs)
        // when called before initialize.
        let Some(input) = self.get_or_create_input(&uri, &text, version) else {
            return;
        };

        // Publish diagnostics if this is a Hydra file (using cached check)
        let is_hydra = self
            .with_analysis("opening documents", |analysis| {
                yaml_cache::is_hydra_file(&analysis.db, input)
            })
            .unwrap_or(false);
        if is_hydra && self.settings.read().features.diagnostics {
            self.publish_diagnostics_for_document(&uri).await;
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
            self.documents
                .update(uri.clone(), change.text.clone(), version);

            // Update the salsa input (invalidates cached queries). Returns
            // None (and logs) when called before initialize.
            let Some(input) = self.get_or_create_input(&uri, &change.text, version) else {
                return;
            };

            // Re-publish diagnostics if this is a Hydra file (using cached check)
            let is_hydra = self
                .with_analysis("changing documents", |analysis| {
                    yaml_cache::is_hydra_file(&analysis.db, input)
                })
                .unwrap_or(false);
            if is_hydra && self.settings.read().features.diagnostics {
                self.publish_diagnostics_for_document(&uri).await;
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
        // and watched `.pth` files all participate in analysis. Syncing
        // existing files bumps the per-file revision inside ruff_db so any
        // query that read them through `source_text` is invalidated on the
        // next request. For `.pth` create/delete events we also bump the
        // tracked inventory for that site-packages directory so the directory
        // scan is recomputed.
        if params.changes.is_empty() {
            return;
        }
        let mut pth_inventory_dirs = HashSet::new();
        let tracked_paths: Vec<std::path::PathBuf> = params
            .changes
            .iter()
            .filter_map(|change| {
                let path = change.uri.to_file_path().ok()?;
                match path.extension().and_then(|ext| ext.to_str()) {
                    Some("py") | Some("pyi") => Some(path),
                    Some("pth") => {
                        if matches!(
                            change.typ,
                            FileChangeType::CREATED | FileChangeType::DELETED
                        ) && let Some(parent) = path.parent()
                        {
                            pth_inventory_dirs.insert(parent.to_path_buf());
                        }
                        Some(path)
                    }
                    _ => None,
                }
            })
            .collect();
        if tracked_paths.is_empty() && pth_inventory_dirs.is_empty() {
            tracing::debug!(
                changed = params.changes.len(),
                "watched files changed; no python analysis inputs to sync"
            );
            return;
        }
        let synced = self
            .with_analysis_mut("watching files", |analysis| {
                let mut synced = 0usize;
                for std_path in &tracked_paths {
                    let Some(sys_path) = SystemPath::from_std_path(std_path) else {
                        continue;
                    };
                    File::sync_path(&mut analysis.db, sys_path);
                    synced += 1;
                }
                for directory in &pth_inventory_dirs {
                    let inventory = analysis.ensure_pth_inventory(directory);
                    let next_revision = inventory.revision(&analysis.db) + 1;
                    inventory.set_revision(&mut analysis.db).to(next_revision);
                }
                synced
            })
            .unwrap_or(0);
        tracing::debug!(
            changed = params.changes.len(),
            synced,
            pth_inventory_dirs = pth_inventory_dirs.len(),
            "watched files changed; synced python analysis inputs"
        );
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.remove(&uri);

        // Soft-close the salsa input: clear the source text so salsa drops
        // the per-document `String` while the input slot is retained for
        // reuse on a subsequent `did_open` for the same URI. Salsa exposes
        // no input-deletion API, so this is the minimum-footprint
        // equivalent (matches `ruff_db::files::VirtualFile::close`).
        //
        // Lock order: take `analysis` before the `document_inputs` shard
        // guard, per the comment on `document_inputs`. The shard is
        // entered inside the closure.
        //
        // Then enforce LRU limits — evicts stale cache entries and keeps
        // memory bounded. Skips entirely if analysis is not yet
        // initialized (notification arrived before initialize).
        self.with_analysis_mut("closing documents", |analysis| {
            if let Some(input) = self.document_inputs.get(&uri).map(|e| *e) {
                input.close(&mut analysis.db);
            }
            analysis.db.trigger_lru_eviction();
        });

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

        // Get document content
        let document = match self.documents.get(&uri) {
            Some(doc) => doc,
            None => return Ok(None),
        };

        // Check if this is a Hydra file
        if !YamlParser::is_hydra_file(&document.content) {
            return Ok(None);
        }

        // Find _target_ at cursor position
        let hydra_object = match YamlParser::find_target_at_position(&document.content, position) {
            Ok(Some(info)) => info,
            Ok(None) => return Ok(None),
            Err(e) => {
                self.client
                    .log_message(MessageType::ERROR, format!("YAML parse error: {}", e))
                    .await;
                return Ok(None);
            }
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

        // Get document content
        let document = match self.documents.get(&uri) {
            Some(doc) => doc,
            None => return Ok(None),
        };

        // Check if this is a Hydra file
        if !YamlParser::is_hydra_file(&document.content) {
            return Ok(None);
        }

        // Get completion context
        let context = match YamlParser::get_completion_context(&document.content, position) {
            Ok(ctx) => ctx,
            Err(e) => {
                self.client
                    .log_message(
                        MessageType::ERROR,
                        format!("Completion context error: {}", e),
                    )
                    .await;
                return Ok(None);
            }
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

        // Get document content
        let document = match self.documents.get(&uri) {
            Some(doc) => doc,
            None => return Ok(None),
        };

        // Check if this is a Hydra file
        if !YamlParser::is_hydra_file(&document.content) {
            return Ok(None);
        }

        // Find target info for the parameter line at cursor position
        let (target_value, param_context, keyword_keys) =
            match YamlParser::find_target_for_parameter_line(&document.content, position) {
                Ok(Some(result)) => result,
                Ok(None) => return Ok(None),
                Err(e) => {
                    self.client
                        .log_message(MessageType::ERROR, format!("YAML parse error: {}", e))
                        .await;
                    return Ok(None);
                }
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

        // Get document content
        let document = match self.documents.get(&uri) {
            Some(doc) => doc,
            None => return Ok(None),
        };

        // Check if this is a Hydra file
        if !YamlParser::is_hydra_file(&document.content) {
            return Ok(None);
        }

        // Find _target_ at cursor position
        let target_info = match YamlParser::find_target_at_position(&document.content, position) {
            Ok(Some(info)) => info,
            Ok(None) => return Ok(None),
            Err(e) => {
                self.client
                    .log_message(MessageType::ERROR, format!("YAML parse error: {}", e))
                    .await;
                return Ok(None);
            }
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

        // Get document content
        let document = match self.documents.get(&uri) {
            Some(doc) => doc,
            None => return Ok(None),
        };

        // Check if this is a Hydra file
        if !YamlParser::is_hydra_file(&document.content) {
            return Ok(None);
        }

        self.client
            .log_message(MessageType::INFO, "Generating semantic tokens".to_string())
            .await;

        // Extract semantic tokens from the YAML content
        let tokens = YamlParser::extract_semantic_tokens(&document.content);

        // Convert to LSP format
        let data = HydraSemanticToken::to_lsp_tokens(&tokens);

        self.client
            .log_message(
                MessageType::INFO,
                format!("Generated {} semantic tokens", tokens.len()),
            )
            .await;

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }
}

impl HydraLspBackend {
    /// Publish diagnostics for a document.
    ///
    /// Uses the cached `parsed_yaml` query for the document's `DocumentInput`.
    /// Callers (`did_open`, `did_change`) always create the input first, so
    /// the absence of an entry is a programmer error rather than a runtime
    /// concern — we skip publishing rather than silently parsing uncached.
    async fn publish_diagnostics_for_document(&self, uri: &Url) {
        // Get parsed content from cache and snapshot the db for use by
        // validate_document inside spawn_blocking.
        let Some(input_ref) = self.document_inputs.get(uri) else {
            debug_assert!(
                false,
                "publish_diagnostics_for_document called for {} with no DocumentInput; \
                 did_open/did_change should always create one first",
                uri
            );
            tracing::warn!(%uri, "no DocumentInput for diagnostics; skipping");
            return;
        };
        let input = *input_ref;
        drop(input_ref);

        let Some((parsed_yaml, db_snapshot, python_config)) =
            self.with_analysis("publishing diagnostics", |analysis| {
                // Clone the cheap Arc-wrapped ParsedYaml; the deep
                // ParsedContent stays inside the salsa cache.
                let cached = yaml_cache::parsed_yaml(&analysis.db, input);
                let snapshot = analysis.db.clone();
                (cached, snapshot, analysis.python_config)
            })
        else {
            return;
        };

        // Handle the parse result
        if parsed_yaml.is_ok() {
            // Clone disabled_rules (other settings live in salsa via python_config)
            let disabled_rules = self.settings.read().disabled_rules.clone();

            // Move expensive validation (Python analysis per target) to blocking thread.
            // The snapshot lets the cached_definition_info salsa query share results
            // with hover/signature_help/goto on the main db. The ParsedYaml clone
            // moves a single Arc, not the full HydraObject vec / line index.
            let join_result = tokio::task::spawn_blocking(move || {
                let parsed_content = match parsed_yaml.result() {
                    Ok(content) => content,
                    Err(_) => return Vec::new(),
                };
                diagnostics::validate_document(
                    parsed_content,
                    &disabled_rules,
                    &db_snapshot,
                    python_config,
                )
            })
            .await;
            let diagnostics = match join_result {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!(error = %e, "validate_document task failed");
                    Vec::new()
                }
            };

            self.client
                .publish_diagnostics(uri.clone(), diagnostics, None)
                .await;
        } else {
            let e = parsed_yaml.result().err().unwrap_or("").to_string();
            let diagnostic = Diagnostic {
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
                message: format!("YAML syntax error: {}", e),
                ..Default::default()
            };

            self.client
                .publish_diagnostics(uri.clone(), vec![diagnostic], None)
                .await;
        }
    }
}
