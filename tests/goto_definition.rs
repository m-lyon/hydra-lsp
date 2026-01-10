mod common;

use tower_lsp::lsp_types::*;

use crate::common::*;

#[tokio::test]
async fn test_goto_definition_class() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = std::fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
    ctx.open_document("config.yaml", content.clone()).await;

    // Find the line with _target_: my_module.DataLoader
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

    match res {
        Some(GotoDefinitionResponse::Scalar(location)) => {
            let file_name = location.uri.path().split('/').next_back().unwrap_or("");
            insta::assert_snapshot!(
                "goto_definition_class",
                format!(
                    "File: {}\nLine: {}\nCharacter: {}",
                    file_name, location.range.start.line, location.range.start.character
                )
            );
        }
        _ => panic!("Expected scalar location response"),
    }
}

#[tokio::test]
async fn test_goto_definition_function() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
test:
  _target_: my_module.create_model
  input_dim: 10
  output_dim: 5
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    let res = ctx
        .request::<request::GotoDefinition>(GotoDefinitionParams {
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
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
        })
        .await;

    match res {
        Some(GotoDefinitionResponse::Scalar(location)) => {
            let file_name = location.uri.path().split('/').next_back().unwrap_or("");
            insta::assert_snapshot!(
                "goto_definition_function",
                format!(
                    "File: {}\nLine: {}\nCharacter: {}",
                    file_name, location.range.start.line, location.range.start.character
                )
            );
        }
        _ => panic!("Expected scalar location response"),
    }
}

#[tokio::test]
async fn test_no_definition_outside_target() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
test:
  _target_: my_module.DataLoader
  batch_size: 32
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    // Try goto definition on a parameter line (not _target_)
    let res = ctx
        .request::<request::GotoDefinition>(GotoDefinitionParams {
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
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
        })
        .await;

    assert!(
        res.is_none(),
        "Should not get definition on non-target line"
    );
}
