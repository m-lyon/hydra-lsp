use parking_lot::RwLock;
use std::sync::Arc;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::diagnostics;
use crate::document::DocumentStore;
use crate::python_analyzer::{DefinitionInfo, PythonAnalyzer};
use crate::yaml_parser::{CompletionContext, YamlParser};

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
                diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                    DiagnosticOptions {
                        identifier: Some("hydra-lsp".to_string()),
                        inter_file_dependencies: false,
                        workspace_diagnostics: false,
                        ..Default::default()
                    },
                )),
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

        // Split target into module and symbol
        let (module_path, symbol_name) = match PythonAnalyzer::split_target(&target_info.value) {
            Ok(parts) => parts,
            Err(e) => {
                self.client
                    .log_message(MessageType::ERROR, format!("Invalid target: {}", e))
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
            Ok(definition_info) => {
                let hover_content = match definition_info {
                    DefinitionInfo::Function(sig) => PythonAnalyzer::format_signature(&sig),
                    DefinitionInfo::Class(class_info) => PythonAnalyzer::format_class(&class_info),
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
                // If Python analysis fails, show a basic hover with error info
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("Python analysis failed: {}", e),
                    )
                    .await;

                let hover_content = format!(
                    "**Hydra Target**\n\nModule: `{}`\n\nSymbol: `{}`\n\n---\n\n*Could not analyze Python definition: {}*",
                    module_path, symbol_name, e
                );
                Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: hover_content,
                    }),
                    range: None,
                }))
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
                // For now, return placeholder completions
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!("Target completion requested for: {}", partial),
                    )
                    .await;

                Ok(Some(CompletionResponse::Array(vec![
                    CompletionItem {
                        label: "example.module.Class".to_string(),
                        kind: Some(CompletionItemKind::CLASS),
                        detail: Some("Example class (placeholder)".to_string()),
                        ..Default::default()
                    },
                    CompletionItem {
                        label: "example.module.function".to_string(),
                        kind: Some(CompletionItemKind::FUNCTION),
                        detail: Some("Example function (placeholder)".to_string()),
                        ..Default::default()
                    },
                ])))
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
                Ok(Some(CompletionResponse::Array(vec![
                    CompletionItem {
                        label: "param1".to_string(),
                        kind: Some(CompletionItemKind::PROPERTY),
                        detail: Some("int - Example parameter".to_string()),
                        documentation: Some(Documentation::String(
                            "A placeholder parameter".to_string(),
                        )),
                        ..Default::default()
                    },
                    CompletionItem {
                        label: "param2".to_string(),
                        kind: Some(CompletionItemKind::PROPERTY),
                        detail: Some("str - Example parameter".to_string()),
                        ..Default::default()
                    },
                ])))
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
                Ok(Some(CompletionResponse::Array(vec![
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
                ])))
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

        // Split target into module and symbol
        let (_module_path, _symbol_name) = match PythonAnalyzer::split_target(&target_info.value) {
            Ok(parts) => parts,
            Err(e) => {
                self.client
                    .log_message(MessageType::ERROR, format!("Invalid target: {}", e))
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
            Ok(definition_info) => {
                let (signature_label, parameters, doc_string) = match definition_info {
                    crate::python_analyzer::DefinitionInfo::Function(sig) => {
                        let param_strs: Vec<String> = sig
                            .parameters
                            .iter()
                            .map(|p| {
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
                            })
                            .collect();

                        let label = format!("{}({})", sig.name, param_strs.join(", "));

                        let params = sig
                            .parameters
                            .iter()
                            .map(|p| {
                                let mut label = String::new();
                                if p.is_variadic {
                                    label.push('*');
                                } else if p.is_variadic_keyword {
                                    label.push_str("**");
                                }
                                label.push_str(&p.name);
                                if let Some(type_ann) = &p.type_annotation {
                                    label.push_str(&format!(": {}", type_ann));
                                }

                                ParameterInformation {
                                    label: ParameterLabel::Simple(label),
                                    documentation: p.default_value.as_ref().map(|dv| {
                                        Documentation::String(format!("Default: {}", dv))
                                    }),
                                }
                            })
                            .collect();

                        (label, params, sig.docstring.clone())
                    }
                    crate::python_analyzer::DefinitionInfo::Class(class_info) => {
                        if let Some(init_sig) = &class_info.init_signature {
                            let param_strs: Vec<String> = init_sig
                                .parameters
                                .iter()
                                .filter(|p| p.name != "self")
                                .map(|p| {
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
                                })
                                .collect();

                            let label = format!("{}({})", class_info.name, param_strs.join(", "));

                            let params = init_sig
                                .parameters
                                .iter()
                                .filter(|p| p.name != "self")
                                .map(|p| {
                                    let mut label = String::new();
                                    if p.is_variadic {
                                        label.push('*');
                                    } else if p.is_variadic_keyword {
                                        label.push_str("**");
                                    }
                                    label.push_str(&p.name);
                                    if let Some(type_ann) = &p.type_annotation {
                                        label.push_str(&format!(": {}", type_ann));
                                    }

                                    ParameterInformation {
                                        label: ParameterLabel::Simple(label),
                                        documentation: p.default_value.as_ref().map(|dv| {
                                            Documentation::String(format!("Default: {}", dv))
                                        }),
                                    }
                                })
                                .collect();

                            (label, params, class_info.docstring.clone())
                        } else {
                            let label = format!("{}()", class_info.name);
                            (label, vec![], class_info.docstring.clone())
                        }
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
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("Python analysis failed for signature help: {}", e),
                    )
                    .await;
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

        // Split target into module and symbol
        let (module_path, _symbol_name) = match PythonAnalyzer::split_target(&target_info.value) {
            Ok(parts) => parts,
            Err(e) => {
                self.client
                    .log_message(MessageType::ERROR, format!("Invalid target: {}", e))
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

        // Try to resolve the module to a file
        let file_path = match PythonAnalyzer::resolve_module(
            &module_path,
            workspace_root.as_deref(),
            python_interpreter.as_deref(),
        ) {
            Ok(path) => path,
            Err(e) => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("Could not resolve module {}: {}", module_path, e),
                    )
                    .await;
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

        // For now, just navigate to the file (line 0)
        // TODO: Find the exact line number of the definition
        Ok(Some(GotoDefinitionResponse::Scalar(Location {
            uri: target_uri,
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

        // TODO: Implement semantic token generation
        // This would involve:
        // 1. Parse YAML to find all _target_ values
        // 2. Identify parameter keys associated with targets
        // 3. Generate semantic tokens for:
        //    - Module paths (NAMESPACE)
        //    - Class/function names (CLASS/FUNCTION)
        //    - Parameter names (PARAMETER/PROPERTY)
        //    - Values (STRING/NUMBER/etc)
        // 4. Return tokens in the LSP delta format

        self.client
            .log_message(
                MessageType::INFO,
                "Semantic tokens requested (not yet fully implemented)".to_string(),
            )
            .await;

        // Placeholder: return empty token list
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: vec![],
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
            Ok(target_map) => {
                let diagnostics = diagnostics::validate_document(
                    target_map,
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
