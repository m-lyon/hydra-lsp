mod common;

use tower_lsp::lsp_types::*;

use crate::common::*;

#[tokio::test]
async fn test_signature_help_class() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
test:
  _target_: my_module.DataLoader
  batch_size: 32
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    let res = ctx
        .request::<request::SignatureHelpRequest>(SignatureHelpParams {
            context: None,
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 3,
                    character: 5,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("test.yaml"),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        })
        .await;

    if let Some(sig_help) = res {
        assert!(!sig_help.signatures.is_empty(), "Should have signatures");

        let signatures: Vec<_> = sig_help
            .signatures
            .iter()
            .map(|sig| format!("Signature: {}", sig.label))
            .collect();

        insta::assert_snapshot!("signature_help_class", signatures.join("\n"));

        // active_parameter should point to batch_size (index 0)
        assert_eq!(sig_help.active_parameter, Some(0));
    } else {
        panic!("Expected signature help but got None");
    }
}

#[tokio::test]
async fn test_signature_help_function() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
test:
  _target_: my_module.create_model
  input_dim: 10
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    let res = ctx
        .request::<request::SignatureHelpRequest>(SignatureHelpParams {
            context: None,
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 3,
                    character: 5,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("test.yaml"),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        })
        .await;

    if let Some(sig_help) = res {
        assert!(!sig_help.signatures.is_empty(), "Should have signatures");

        let signature = &sig_help.signatures[0];
        insta::assert_snapshot!(
            "signature_help_function",
            format!("Signature: {}", signature.label,)
        );

        // active_parameter should point to input_dim (index 0)
        assert_eq!(sig_help.active_parameter, Some(0));
    } else {
        panic!("Expected signature help but got None");
    }
}

#[tokio::test]
async fn test_signature_help_not_on_target_line() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
test:
  _target_: my_module.DataLoader
  batch_size: 32
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    // Cursor on the _target_ line itself
    let res = ctx
        .request::<request::SignatureHelpRequest>(SignatureHelpParams {
            context: None,
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 2,
                    character: 13,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("test.yaml"),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        })
        .await;

    assert!(
        res.is_none(),
        "Signature help should not trigger on _target_ line"
    );
}

#[tokio::test]
async fn test_signature_help_active_parameter() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
test:
  _target_: my_module.create_model
  input_dim: 10
  output_dim: 20
  hidden_dim: 30
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    // Check input_dim (index 0)
    let res = ctx
        .request::<request::SignatureHelpRequest>(SignatureHelpParams {
            context: None,
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 3,
                    character: 5,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("test.yaml"),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        })
        .await;
    assert_eq!(res.unwrap().active_parameter, Some(0));

    // Check output_dim (index 1)
    let res = ctx
        .request::<request::SignatureHelpRequest>(SignatureHelpParams {
            context: None,
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 4,
                    character: 5,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("test.yaml"),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        })
        .await;
    assert_eq!(res.unwrap().active_parameter, Some(1));

    // Check hidden_dim (index 2)
    let res = ctx
        .request::<request::SignatureHelpRequest>(SignatureHelpParams {
            context: None,
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 5,
                    character: 5,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("test.yaml"),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        })
        .await;
    assert_eq!(res.unwrap().active_parameter, Some(2));
}

#[tokio::test]
async fn test_signature_help_unknown_parameter() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
test:
  _target_: my_module.create_model
  nonexistent_param: 10
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    let res = ctx
        .request::<request::SignatureHelpRequest>(SignatureHelpParams {
            context: None,
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 3,
                    character: 5,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("test.yaml"),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        })
        .await;

    if let Some(sig_help) = res {
        assert!(
            !sig_help.signatures.is_empty(),
            "Should still return signature"
        );
        let param_count = sig_help.signatures[0]
            .parameters
            .as_ref()
            .map_or(0, |p| p.len()) as u32;
        assert_eq!(
            sig_help.active_parameter,
            Some(param_count),
            "active_parameter should be out-of-bounds so no param is highlighted"
        );
    } else {
        panic!("Expected signature help but got None");
    }
}
