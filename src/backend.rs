use parking_lot::RwLock;
use std::sync::Arc;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::diagnostics;
use crate::document::DocumentStore;
use crate::python_analyzer::{DefinitionInfo, ParameterInfo, PythonAnalyzer};
use crate::yaml_parser::{CompletionContext, HydraSemanticToken, YamlParser};

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
        documentation: p
            .default_value
            .as_ref()
            .map(|dv| Documentation::String(format!("Default: {}", dv))),
    }
}

/// Build signature label and parameter information from a list of parameters
fn build_signature_params(
    params: &[ParameterInfo],
    filter_param: &str,
) -> (String, Vec<ParameterInformation>) {
    let filtered: Vec<_> = params.iter().filter(|p| p.name != filter_param).collect();
    let param_strs: Vec<String> = filtered.iter().map(|p| format_param_label(p)).collect();
    let param_infos: Vec<ParameterInformation> = filtered
        .iter()
        .map(|p| to_parameter_information(p))
        .collect();
    (param_strs.join(", "), param_infos)
}

#[derive(Debug)]
pub struct HydraLspBackend {
    pub client: Client,
    pub documents: Arc<DocumentStore>,
    pub python_interpreter: Arc<RwLock<Option<String>>>,
}

impl HydraLspBackend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Arc::new(DocumentStore::new()),
            python_interpreter: Arc::new(RwLock::new(None)),
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
                    "Hydra LSP server initializing with options: {:?}",
                    params.initialization_options
                ),
            )
            .await;

        // Parse initialization options to extract Python interpreter
        if let Some(init_options) = params.initialization_options
            && let Some(settings) = init_options.get("settings")
            && let Some(python_interpreter) = settings.get("pythonInterpreter")
            && let Some(interpreter_path) = python_interpreter.as_str()
        {
            *self.python_interpreter.write() = Some(interpreter_path.to_string());
            self.client
                .log_message(
                    MessageType::INFO,
                    format!("Python interpreter configured: {}", interpreter_path),
                )
                .await;
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
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
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
            .log_message(MessageType::INFO, "Hydra LSP server initialized")
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

        // Publish diagnostics if this is a Hydra file
        if YamlParser::is_hydra_file(&text) {
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

            // Re-publish diagnostics if this is a Hydra file
            if YamlParser::is_hydra_file(&change.text) {
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

        self.client
            .log_message(MessageType::INFO, format!("Document closed: {}", uri))
            .await;
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
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

        self.client
            .log_message(
                MessageType::LOG,
                format!("Found target at position: {:?}", target_info),
            )
            .await;

        // Try to get the workspace root from the URI
        let workspace_root = uri
            .to_file_path()
            .ok()
            .and_then(|path| path.parent().map(|p| p.to_path_buf()));

        // Get the python interpreter path
        let python_interpreter = self.python_interpreter.read().clone();

        // Try to extract Python definition information
        match PythonAnalyzer::extract_definition_info(
            &target_info.value,
            workspace_root.as_deref(),
            python_interpreter.as_deref(),
        ) {
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
                        line: target_info.line,
                        character: target_info.value_start,
                    },
                    end: Position {
                        line: target_info.line,
                        character: target_info.value_end(),
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
        }
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
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
                // TODO: Resolve target and parameter type to provide value completions
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!(
                            "Parameter value completion requested for target: {}, parameter: {}, partial: {}",
                            target, parameter, partial
                        ),
                    )
                    .await;

                // For demonstration, return some placeholder value completions
                // Ok(Some(CompletionResponse::Array(vec![
                //     CompletionItem {
                //         label: "true".to_string(),
                //         kind: Some(CompletionItemKind::VALUE),
                //         detail: Some("Boolean value".to_string()),
                //         ..Default::default()
                //     },
                //     CompletionItem {
                //         label: "false".to_string(),
                //         kind: Some(CompletionItemKind::VALUE),
                //         detail: Some("Boolean value".to_string()),
                //         ..Default::default()
                //     },
                // ])))
                Ok(None) // Placeholder: no completions yet
            }
            crate::yaml_parser::CompletionContext::Unknown => Ok(None),
        }
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
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

        // Find _target_ at or near cursor position to get context
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

        // Try to get the workspace root from the URI
        let workspace_root = uri
            .to_file_path()
            .ok()
            .and_then(|path| path.parent().map(|p| p.to_path_buf()));

        // Get the python interpreter path
        let python_interpreter = self.python_interpreter.read().clone();

        // Try to extract Python definition information
        match PythonAnalyzer::extract_definition_info(
            &target_info.value,
            workspace_root.as_deref(),
            python_interpreter.as_deref(),
        ) {
            Ok((definition_info, _file_path, _module_path, _symbol_name)) => {
                let (signature_label, parameters, doc_string) = match definition_info {
                    DefinitionInfo::Function(sig) => {
                        let (params_str, params) = build_signature_params(&sig.parameters, "");
                        let label = format!("{}({})", sig.name, params_str);
                        (label, params, sig.docstring.clone())
                    }
                    DefinitionInfo::Class(class_info) => {
                        if let Some(init_sig) = &class_info.init_signature {
                            let (params_str, params) =
                                build_signature_params(&init_sig.parameters, "self");
                            let label = format!("{}({})", class_info.name, params_str);
                            (label, params, class_info.docstring.clone())
                        } else {
                            let label = format!("{}()", class_info.name);
                            (label, vec![], class_info.docstring.clone())
                        }
                    }
                    DefinitionInfo::Method(method_info) => {
                        let sig = &method_info.signature;
                        // Filter out 'self' and 'cls' for classmethods
                        let filter_param = if method_info.is_classmethod {
                            "cls"
                        } else if method_info.is_staticmethod {
                            "" // No parameter to filter for staticmethods
                        } else {
                            "self"
                        };

                        let (params_str, params) =
                            build_signature_params(&sig.parameters, filter_param);
                        let label =
                            format!("{}.{}({})", method_info.class_name, sig.name, params_str);
                        (label, params, sig.docstring.clone())
                    }
                };

                Ok(Some(SignatureHelp {
                    signatures: vec![SignatureInformation {
                        label: signature_label,
                        documentation: doc_string.map(|ds| {
                            Documentation::MarkupContent(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: ds,
                            })
                        }),
                        parameters: if parameters.is_empty() {
                            None
                        } else {
                            Some(parameters)
                        },
                        active_parameter: None,
                    }],
                    active_signature: Some(0),
                    active_parameter: None,
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
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
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

        // Try to get the workspace root from the URI
        let workspace_root = uri
            .to_file_path()
            .ok()
            .and_then(|path| path.parent().map(|p| p.to_path_buf()));

        // Get the python interpreter path
        let python_interpreter = self.python_interpreter.read().clone();

        // Extract definition info to get the line number
        let (file_path, start_line, start_col, end_line, end_col) =
            match PythonAnalyzer::extract_definition_info(
                &target_info.value,
                workspace_root.as_deref(),
                python_interpreter.as_deref(),
            ) {
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

        Ok(Some(GotoDefinitionResponse::Scalar(Location {
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
        })))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
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
    /// Publish diagnostics for a document
    async fn publish_diagnostics_for_document(&self, uri: &Url, content: &str) {
        let workspace_root = uri
            .to_file_path()
            .ok()
            .and_then(|path| path.parent().map(|p| p.to_path_buf()));

        // Get the python interpreter path
        let python_interpreter = self.python_interpreter.read().clone();

        match YamlParser::parse(content) {
            Ok((targets, _line_map)) => {
                let diagnostics = diagnostics::validate_document(
                    targets,
                    workspace_root.as_deref(),
                    python_interpreter.as_deref(),
                );
                self.client
                    .publish_diagnostics(uri.clone(), diagnostics, None)
                    .await;
            }
            Err(e) => {
                // Publish YAML syntax error as diagnostic
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
