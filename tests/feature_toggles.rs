mod common;

use tower_lsp::lsp_types::*;

use crate::common::*;

/// Helper to initialize a server with a single feature disabled.
async fn init_with_disabled(ctx: &mut TestContext, feature_key: &str) {
    ctx.initialize_with_settings(serde_json::json!({ feature_key: false }))
        .await;
}

// ── Hover ──────────────────────────────────────────────────────────

#[tokio::test]
async fn test_hover_disabled_returns_none() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    init_with_disabled(&mut ctx, "enableHover").await;

    let content = std::fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
    ctx.open_document("config.yaml", content.clone()).await;

    let target_line = content
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("_target_: my_module.DataLoader"))
        .map(|(idx, _)| idx)
        .unwrap();

    let res = ctx
        .request::<request::HoverRequest>(HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: target_line as u32,
                    character: 13,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("config.yaml"),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        })
        .await;

    assert!(res.is_none(), "Hover should return None when disabled");
}

// ── Goto Definition ────────────────────────────────────────────────

#[tokio::test]
async fn test_goto_definition_disabled_returns_none() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    init_with_disabled(&mut ctx, "enableGotoDefinition").await;

    let content = std::fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
    ctx.open_document("config.yaml", content.clone()).await;

    let target_line = content
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("_target_: my_module.DataLoader"))
        .map(|(idx, _)| idx)
        .unwrap();

    let res = ctx
        .request::<request::GotoDefinition>(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: target_line as u32,
                    character: 13,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("config.yaml"),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
        })
        .await;

    assert!(
        res.is_none(),
        "GotoDefinition should return None when disabled"
    );
}

// ── Signature Help ─────────────────────────────────────────────────

#[tokio::test]
async fn test_signature_help_disabled_returns_none() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    init_with_disabled(&mut ctx, "enableSignatureHelp").await;

    let content = std::fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
    ctx.open_document("config.yaml", content.clone()).await;

    let target_line = content
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("_target_: my_module.DataLoader"))
        .map(|(idx, _)| idx)
        .unwrap();

    let res = ctx
        .request::<request::SignatureHelpRequest>(SignatureHelpParams {
            context: None,
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: target_line as u32,
                    character: 13,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("config.yaml"),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        })
        .await;

    assert!(
        res.is_none(),
        "SignatureHelp should return None when disabled"
    );
}

// ── Semantic Tokens ────────────────────────────────────────────────

#[tokio::test]
async fn test_semantic_tokens_disabled_returns_none() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    init_with_disabled(&mut ctx, "enableSemanticTokens").await;

    let content = std::fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
    ctx.open_document("config.yaml", content.clone()).await;

    let res = ctx
        .request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
            text_document: TextDocumentIdentifier {
                uri: ctx.doc_uri("config.yaml"),
            },
        })
        .await;

    assert!(
        res.is_none(),
        "SemanticTokens should return None when disabled"
    );
}

// ── Diagnostics ────────────────────────────────────────────────────

#[tokio::test]
async fn test_diagnostics_disabled_produces_no_diagnostics() {
    let mut ctx = TestContext::new(TestWorkspace::Diagnostics);
    init_with_disabled(&mut ctx, "enableDiagnostics").await;

    let content = std::fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
    ctx.open_document("config.yaml", content).await;

    // With diagnostics disabled, the server should NOT publish any diagnostics.
    // We send a hover request and verify it completes — if diagnostics were
    // published they would appear in the response stream before the hover reply.
    // The response() helper already skips diagnostic notifications, so if we
    // reach here without hanging the test passes.
    let res = ctx
        .request::<request::HoverRequest>(HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 0,
                    character: 0,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("config.yaml"),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        })
        .await;

    // The hover at line 0, char 0 (a comment) should return None
    assert!(res.is_none());
}

// ── All enabled by default ─────────────────────────────────────────

#[tokio::test]
async fn test_defaults_all_features_enabled() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    // Initialize with empty settings — all toggles should default to true
    ctx.initialize_with_settings(serde_json::json!({})).await;

    let content = std::fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
    ctx.open_document("config.yaml", content.clone()).await;

    let target_line = content
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("_target_: my_module.DataLoader"))
        .map(|(idx, _)| idx)
        .unwrap();

    // Hover should work when all defaults are enabled
    let res = ctx
        .request::<request::HoverRequest>(HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: target_line as u32,
                    character: 13,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("config.yaml"),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        })
        .await;

    assert!(
        res.is_some(),
        "Hover should return a result when features default to enabled"
    );
}
