use dashmap::DashMap;
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use salsa::{Database as _, Setter};

use crate::database::HydraDatabase;
use crate::diagnostics::{self, DiagnosticRule};
use crate::document::DocumentStore;
use crate::python_analyzer::{DefinitionInfo, ParameterInfo, PythonAnalyzer};
use crate::python_cache::{self, PythonConfig, TargetString};
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

pub struct HydraLspBackend {
    pub client: Client,
    pub documents: Arc<DocumentStore>,
    pub settings: Arc<RwLock<Settings>>,
    /// Salsa database for incremental caching.
    /// Uses `Mutex` because salsa's Storage is `Send` but not `Sync`.
    db: Arc<parking_lot::Mutex<HydraDatabase>>,
    /// Map from document URI to its salsa input handle.
    document_inputs: DashMap<Url, DocumentInput>,
    /// Salsa input for Python environment config (workspace root, interpreter).
    /// Updated when LSP settings change; invalidates all cached definition lookups.
    python_config: PythonConfig,
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
        // Use the current directory as the database root.
        let cwd_string = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "/".to_string());
        let cwd = ruff_db::system::SystemPath::new(&cwd_string);
        let db = HydraDatabase::new(cwd);
        let python_config = PythonConfig::new(&db, Some(cwd_string), None);
        Self {
            client,
            documents: Arc::new(DocumentStore::new()),
            settings: Arc::new(RwLock::new(Settings::default())),
            db: Arc::new(parking_lot::Mutex::new(db)),
            document_inputs: DashMap::new(),
            python_config,
        }
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
        let db = self.db.lock().clone();
        let python_config = self.python_config;

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
    /// Holds the DashMap shard's write guard for `uri` across the salsa
    /// update so that concurrent callers for the same URI cannot both
    /// observe a vacant entry and create distinct inputs (which would
    /// orphan one in salsa storage and let stale text leak through).
    /// Lock order is shard-guard → `self.db`, matching every other code
    /// path in this module.
    fn get_or_create_input(&self, uri: &Url, text: &str, version: i32) -> DocumentInput {
        match self.document_inputs.entry(uri.clone()) {
            dashmap::Entry::Occupied(occ) => {
                let input = *occ.get();
                let mut db = self.db.lock();
                input.set_text(&mut *db).to(text.to_string());
                input.set_version(&mut *db).to(version);
                input
            }
            dashmap::Entry::Vacant(vac) => {
                let db = self.db.lock();
                let input = DocumentInput::new(&*db, text.to_string(), version);
                vac.insert(input);
                input
            }
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for HydraLspBackend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
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

            // Update the salsa PythonConfig so cached lookups invalidate
            if interpreter_path.is_some() {
                let mut db = self.db.lock();
                self.python_config
                    .set_interpreter(&mut *db)
                    .to(interpreter_path.clone());
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

        // Capture workspace root from init params for Python environment discovery
        if let Some(root_uri) = params.root_uri
            && let Ok(root_path) = root_uri.to_file_path()
        {
            let mut db = self.db.lock();
            self.python_config
                .set_workspace_root(&mut *db)
                .to(Some(root_path.to_string_lossy().to_string()));
        }

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

        // Create a salsa input for this document
        let input = self.get_or_create_input(&uri, &text, version);

        // Publish diagnostics if this is a Hydra file (using cached check)
        let is_hydra = {
            let db = self.db.lock();
            yaml_cache::is_hydra_file(&*db, input)
        };
        if is_hydra && self.settings.read().features.diagnostics {
            self.publish_diagnostics_for_document(&uri, &text).await;
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

            // Update the salsa input (invalidates cached queries)
            let input = self.get_or_create_input(&uri, &change.text, version);

            // Re-publish diagnostics if this is a Hydra file (using cached check)
            let is_hydra = {
                let db = self.db.lock();
                yaml_cache::is_hydra_file(&*db, input)
            };
            if is_hydra && self.settings.read().features.diagnostics {
                self.publish_diagnostics_for_document(&uri, &change.text)
                    .await;
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

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.remove(&uri);
        // Keep the URI -> DocumentInput entry so a subsequent did_open reuses
        // the same salsa input. Salsa inputs cannot be removed from storage,
        // so dropping the mapping here would leak a fresh input on every
        // close/open cycle. Bounding by unique URIs (rather than open count)
        // is the best we can do until salsa supports input GC.

        // Enforce LRU limits after removing a document — evicts stale
        // cache entries and keeps memory bounded.
        {
            let mut db = self.db.lock();
            db.trigger_lru_eviction();
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
    /// Uses the cached `parsed_yaml` query when a `DocumentInput` exists
    /// for this URI. Falls back to direct parsing otherwise.
    async fn publish_diagnostics_for_document(&self, uri: &Url, content: &str) {
        // Get parsed content from cache or direct parse, and snapshot the db
        // for use by validate_document inside spawn_blocking.
        let (parse_result, db_snapshot) = if let Some(input_ref) = self.document_inputs.get(uri) {
            let input = *input_ref;
            drop(input_ref);

            // Get the cached parse result and extract data before releasing the lock.
            // The lock must be released before any `.await` point.
            let db = self.db.lock();
            let cached = yaml_cache::parsed_yaml(&*db, input);
            let result = match cached.result() {
                Ok(parsed_content) => Ok(parsed_content.clone()),
                Err(e) => Err(e.to_string()),
            };
            let snapshot = db.clone();
            (result, snapshot)
        } else {
            let result = YamlParser::parse(content).map_err(|e| e.to_string());
            let snapshot = self.db.lock().clone();
            (result, snapshot)
        };

        let python_config = self.python_config;

        // Handle the parse result
        match parse_result {
            Ok(mut parsed_content) => {
                // Clone disabled_rules (other settings live in salsa via python_config)
                let disabled_rules = self.settings.read().disabled_rules.clone();
                parsed_content.file_suppressions.extend(&disabled_rules);

                // Move expensive validation (Python analysis per target) to blocking thread.
                // The snapshot lets the cached_definition_info salsa query share results
                // with hover/signature_help/goto on the main db.
                let join_result = tokio::task::spawn_blocking(move || {
                    diagnostics::validate_document(
                        parsed_content,
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
            }
            Err(e) => {
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
}
