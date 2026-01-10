mod common;

use tower_lsp::lsp_types::*;

use crate::common::*;

#[tokio::test]
async fn test_hover_on_target() {
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

    if let Some(hover) = res {
        match hover.contents {
            HoverContents::Markup(markup) => {
                insta::assert_snapshot!("hover_on_dataloader", markup.value);
            }
            _ => {
                panic!("Expected Markup hover content but got something else");
            }
        }
    } else {
        panic!("Expected hover response but got None");
    }
}

#[tokio::test]
async fn test_hover_on_function_target() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
test:
  _target_: my_module.create_model
  input_dim: 10
  output_dim: 5
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    // Position should be on line 2 where _target_ is (0-indexed)
    let res = ctx
        .request::<request::HoverRequest>(HoverParams {
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

    if let Some(hover) = res {
        match hover.contents {
            HoverContents::Markup(markup) => {
                insta::assert_snapshot!("hover_on_function", markup.value);
            }
            _ => {
                panic!("Expected Markup hover content but got something else");
            }
        }
    } else {
        panic!("Expected hover response but got None");
    }
}

#[tokio::test]
async fn test_no_hover_outside_target() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
test:
  _target_: my_module.DataLoader
  batch_size: 32
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    // Try hovering on a parameter line (not _target_)
    let res = ctx
        .request::<request::HoverRequest>(HoverParams {
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

    assert!(res.is_none(), "Should not get hover on non-target line");
}
