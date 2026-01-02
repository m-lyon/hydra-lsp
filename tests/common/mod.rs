#![allow(dead_code)]

use std::fmt::Debug;
use std::fs;
use std::io::Write;
use std::path::Path;

use fs_extra::dir::CopyOptions;
use temp_dir::TempDir;
use tokio::io::{duplex, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tower_lsp::lsp_types::notification::Notification;
use tower_lsp::lsp_types::{InitializedParams, Url, WorkspaceFolder};
use tower_lsp::{jsonrpc, lsp_types, lsp_types::request::Request, LspService, Server};

use hydra_lsp::backend::HydraLspBackend;

fn encode_message(content_type: Option<&str>, message: &str) -> String {
    let content_type = content_type
        .map(|data| format!("\r\nContent-Type: {data}"))
        .unwrap_or_default();
    format!(
        "Content-Length: {}{}\r\n\r\n{}",
        message.len(),
        content_type,
        message
    )
}

pub enum TestWorkspace {
    Simple,
    Diagnostics,
}

impl AsRef<str> for TestWorkspace {
    fn as_ref(&self) -> &str {
        match self {
            TestWorkspace::Simple => "simple",
            TestWorkspace::Diagnostics => "diagnostics",
        }
    }
}

pub struct TestContext {
    pub request_tx: DuplexStream,
    pub response_rx: BufReader<DuplexStream>,
    pub _server: tokio::task::JoinHandle<()>,
    pub request_id: i64,
    pub workspace: TempDir,
}

impl TestContext {
    pub fn new(base: TestWorkspace) -> Self {
        let (request_tx, req_server) = duplex(1024);
        let (resp_server, response_rx) = duplex(1024);
        let response_rx = BufReader::new(response_rx);

        let (service, socket) = LspService::new(HydraLspBackend::new);
        let server = tokio::spawn(Server::new(req_server, resp_server, socket).serve(service));

        // Create a temporary workspace and initialize it with our test inputs
        let workspace = TempDir::new().unwrap();
        let test_workspace_path = Path::new("tests").join("workspace").join(base.as_ref());

        if test_workspace_path.exists() {
            for item in fs::read_dir(&test_workspace_path).unwrap() {
                let item = item.unwrap();
                let path = item.path();
                eprintln!("copying {:?}", path);
                fs_extra::copy_items(&[&path], workspace.path(), &CopyOptions::new()).unwrap();
            }
        }

        Self {
            request_tx,
            response_rx,
            _server: server,
            request_id: 0,
            workspace,
        }
    }

    pub fn doc_uri(&self, path: &str) -> Url {
        Url::from_file_path(self.workspace.path().join(path)).unwrap()
    }

    pub async fn send(&mut self, request: &jsonrpc::Request) {
        let content = serde_json::to_string(request).unwrap();
        eprintln!("\nsending: {content}");
        std::io::stderr().flush().unwrap();
        self.request_tx
            .write_all(encode_message(None, &content).as_bytes())
            .await
            .unwrap();
    }

    pub async fn response<R: std::fmt::Debug + serde::de::DeserializeOwned>(&mut self) -> R {
        loop {
            // First line is the content length header
            let mut clh = String::new();
            self.response_rx.read_line(&mut clh).await.unwrap();
            if !clh.starts_with("Content-Length") {
                panic!("missing content length header");
            }
            let length = clh
                .trim_start_matches("Content-Length: ")
                .trim()
                .parse::<usize>()
                .unwrap();
            // Next line is just a blank line
            self.response_rx.read_line(&mut clh).await.unwrap();
            // Then the message, of the size given by the content length header
            let mut content = vec![0; length];
            self.response_rx.read_exact(&mut content).await.unwrap();
            let content = String::from_utf8(content).unwrap();
            eprintln!("received: {content}");
            std::io::stderr().flush().unwrap();
            // Skip notifications (log messages, diagnostics, etc.)
            if content.contains("window/logMessage")
                || content.contains("textDocument/publishDiagnostics")
                || !content.contains("\"id\"")
            {
                continue;
            }
            let response = serde_json::from_str::<jsonrpc::Response>(&content).unwrap();
            let (_id, result) = response.into_parts();
            return serde_json::from_value(result.unwrap()).unwrap();
        }
    }

    pub async fn request<R: Request>(&mut self, params: R::Params) -> R::Result
    where
        R::Result: Debug,
    {
        let request = jsonrpc::Request::build(R::METHOD)
            .id(self.request_id)
            .params(serde_json::to_value(params).unwrap())
            .finish();
        self.request_id += 1;
        self.send(&request).await;
        self.response().await
    }

    pub async fn recv<R: std::fmt::Debug + serde::de::DeserializeOwned>(&mut self) -> R {
        loop {
            // First line is the content length header
            let mut clh = String::new();
            self.response_rx.read_line(&mut clh).await.unwrap();
            if !clh.starts_with("Content-Length") {
                panic!("missing content length header");
            }
            let length = clh
                .trim_start_matches("Content-Length: ")
                .trim()
                .parse::<usize>()
                .unwrap();
            // Next line is just a blank line
            self.response_rx.read_line(&mut clh).await.unwrap();
            // Then the message, of the size given by the content length header
            let mut content = vec![0; length];
            self.response_rx.read_exact(&mut content).await.unwrap();
            let content = String::from_utf8(content).unwrap();
            eprintln!("received: {content}");
            std::io::stderr().flush().unwrap();
            // Skip log messages but process other notifications
            if content.contains("window/logMessage") {
                continue;
            }
            // Try to parse as a notification request (has method but no id for response)
            let response = serde_json::from_str::<jsonrpc::Request>(&content).unwrap();
            let (_method, _id, params) = response.into_parts();
            return serde_json::from_value(params.unwrap()).unwrap();
        }
    }

    pub async fn notify<N: Notification>(&mut self, params: N::Params) {
        let notification = jsonrpc::Request::build(N::METHOD)
            .params(serde_json::to_value(params).unwrap())
            .finish();
        self.send(&notification).await;
    }

    pub async fn initialize(&mut self) {
        // Real set of initialize params with workspace configuration
        let initialize = r#"{
            "capabilities": {
                "general": {
                    "positionEncodings": ["utf-8", "utf-32", "utf-16"]
                },
                "textDocument": {
                    "hover": {
                        "contentFormat": ["markdown"]
                    },
                    "completion": {
                        "completionItem": {
                            "snippetSupport": true,
                            "deprecatedSupport": true
                        }
                    },
                    "signatureHelp": {
                        "signatureInformation": {
                            "documentationFormat": ["markdown"],
                            "parameterInformation": {
                                "labelOffsetSupport": true
                            }
                        }
                    },
                    "definition": {},
                    "publishDiagnostics": {
                        "versionSupport": true
                    }
                },
                "workspace": {
                    "workspaceFolders": true,
                    "didChangeConfiguration": {
                        "dynamicRegistration": false
                    }
                }
            },
            "clientInfo": {
                "name": "test-client",
                "version": "0.1.0"
            },
            "processId": null,
            "rootPath": null,
            "rootUri": null,
            "workspaceFolders": null
        }"#;
        let mut initialize: <lsp_types::request::Initialize as Request>::Params =
            serde_json::from_str(initialize).unwrap();
        let workspace_url = Url::from_file_path(self.workspace.path()).unwrap();
        initialize.root_uri = Some(workspace_url.clone());
        initialize.workspace_folders = Some(vec![WorkspaceFolder {
            name: "test".to_owned(),
            uri: workspace_url.clone(),
        }]);
        self.request::<lsp_types::request::Initialize>(initialize)
            .await;
        self.notify::<lsp_types::notification::Initialized>(InitializedParams {})
            .await;
    }

    pub async fn open_document(&mut self, path: &str, content: String) {
        self.notify::<lsp_types::notification::DidOpenTextDocument>(
            lsp_types::DidOpenTextDocumentParams {
                text_document: lsp_types::TextDocumentItem {
                    uri: self.doc_uri(path),
                    language_id: "yaml".to_string(),
                    version: 0,
                    text: content,
                },
            },
        )
        .await;
    }
}
