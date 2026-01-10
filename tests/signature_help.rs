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

    if let Some(sig_help) = res {
        assert!(!sig_help.signatures.is_empty(), "Should have signatures");

        let signatures: Vec<_> = sig_help
            .signatures
            .iter()
            .map(|sig| format!("Signature: {}", sig.label))
            .collect();

        insta::assert_snapshot!("signature_help_class", signatures.join("\n"));
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

    if let Some(sig_help) = res {
        assert!(!sig_help.signatures.is_empty(), "Should have signatures");

        let signature = &sig_help.signatures[0];
        insta::assert_snapshot!(
            "signature_help_function",
            format!("Signature: {}", signature.label,)
        );
    } else {
        panic!("Expected signature help but got None");
    }
}
