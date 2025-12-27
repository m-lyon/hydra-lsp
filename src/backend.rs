use std::sync::Arc;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::diagnostics::DiagnosticsEngine;
use crate::document::DocumentStore;
use crate::python_analyzer::{DefinitionInfo, PythonAnalyzer};
use crate::yaml_parser::YamlParser;

#[derive(Debug)]
pub struct HydraLspBackend {
    pub client: Client,
    pub documents: Arc<DocumentStore>,
}

impl HydraLspBackend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Arc::new(DocumentStore::new()),
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for HydraLspBackend {
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult> {
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
            self.documents.update(uri.clone(), change.text.clone(), version);

            // Re-publish diagnostics if this is a Hydra file
            if YamlParser::is_hydra_file(&change.text) {
                self.publish_diagnostics_for_document(&uri, &change.text).await;
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

        // Split target into module and symbol
        let (module_path, symbol_name) = match PythonAnalyzer::split_target(&target_info.target_value) {
            Ok(parts) => parts,
            Err(e) => {
                self.client
                    .log_message(MessageType::ERROR, format!("Invalid target: {}", e))
                    .await;
                return Ok(None);
            }
        };

        // For now, create a mock response since module resolution isn't fully implemented
        // TODO: Implement full Python module resolution and analysis
        let hover_content = format!(
            "**Hydra Target**\n\nModule: `{}`\n\nSymbol: `{}`\n\n---\n\n*Note: Full Python analysis not yet implemented. This is a placeholder hover.*",
            module_path, symbol_name
        );

        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: hover_content,
            }),
            range: None,
        }))
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
                    .log_message(MessageType::ERROR, format!("Completion context error: {}", e))
                    .await;
                return Ok(None);
            }
        };

        match context {
            crate::yaml_parser::CompletionContext::TargetValue { partial } => {
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
            crate::yaml_parser::CompletionContext::ParameterKey { target, partial } => {
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
            crate::yaml_parser::CompletionContext::Unknown => Ok(None),
        }
    }
}

impl HydraLspBackend {
    /// Publish diagnostics for a document
    async fn publish_diagnostics_for_document(&self, uri: &Url, content: &str) {
        match YamlParser::parse(content) {
            Ok(targets) => {
                let diagnostics = DiagnosticsEngine::validate_document(targets);
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
