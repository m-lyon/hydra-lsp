mod common;

use hydra_lsp::yaml_parser::SemanticTokenType;
use tower_lsp::lsp_types::*;

use crate::common::*;

/// Convert token type index to the SemanticTokenType enum name
fn token_type_name(index: u32) -> String {
    SemanticTokenType::from_index(index)
        .map(|t| format!("{:?}", t))
        .unwrap_or_else(|| format!("Unknown({})", index))
}

#[tokio::test]
async fn test_semantic_tokens_class_target() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
model:
  _target_: my_module.DataLoader
  batch_size: 32
  shuffle: true
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    let res = ctx
        .request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
            text_document: TextDocumentIdentifier {
                uri: ctx.doc_uri("test.yaml"),
            },
        })
        .await;

    if let Some(SemanticTokensResult::Tokens(tokens)) = res {
        assert!(!tokens.data.is_empty(), "Should have semantic tokens");

        // Convert tokens to a readable format for snapshot testing
        let token_summary: Vec<_> = tokens
            .data
            .iter()
            .map(|t| {
                format!(
                    "delta_line={}, delta_start={}, length={}, type={}, modifiers={}",
                    t.delta_line,
                    t.delta_start,
                    t.length,
                    token_type_name(t.token_type),
                    t.token_modifiers_bitset
                )
            })
            .collect();

        insta::assert_yaml_snapshot!("semantic_tokens_class_target", token_summary);
    } else {
        panic!("Expected semantic tokens but got None");
    }
}

#[tokio::test]
async fn test_semantic_tokens_function_target() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
func:
  _target_: my_module.create_model
  input_dim: 10
  output_dim: 5
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    let res = ctx
        .request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
            text_document: TextDocumentIdentifier {
                uri: ctx.doc_uri("test.yaml"),
            },
        })
        .await;

    if let Some(SemanticTokensResult::Tokens(tokens)) = res {
        assert!(!tokens.data.is_empty(), "Should have semantic tokens");

        let token_summary: Vec<_> = tokens
            .data
            .iter()
            .map(|t| {
                format!(
                    "delta_line={}, delta_start={}, length={}, type={}, modifiers={}",
                    t.delta_line,
                    t.delta_start,
                    t.length,
                    token_type_name(t.token_type),
                    t.token_modifiers_bitset
                )
            })
            .collect();

        insta::assert_yaml_snapshot!("semantic_tokens_function_target", token_summary);
    } else {
        panic!("Expected semantic tokens but got None");
    }
}

#[tokio::test]
async fn test_semantic_tokens_nested_module_path() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
model:
  _target_: my.deep.nested.module.ModelClass
  hidden_size: 256
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    let res = ctx
        .request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
            text_document: TextDocumentIdentifier {
                uri: ctx.doc_uri("test.yaml"),
            },
        })
        .await;

    if let Some(SemanticTokensResult::Tokens(tokens)) = res {
        assert!(!tokens.data.is_empty(), "Should have semantic tokens");

        // Should have multiple namespace tokens for nested module path
        let token_summary: Vec<_> = tokens
            .data
            .iter()
            .map(|t| {
                format!(
                    "delta_line={}, delta_start={}, length={}, type={}, modifiers={}",
                    t.delta_line,
                    t.delta_start,
                    t.length,
                    token_type_name(t.token_type),
                    t.token_modifiers_bitset
                )
            })
            .collect();

        insta::assert_yaml_snapshot!("semantic_tokens_nested_module", token_summary);
    } else {
        panic!("Expected semantic tokens but got None");
    }
}

#[tokio::test]
async fn test_semantic_tokens_multiple_targets() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
model:
  _target_: my_module.DataLoader
  batch_size: 32

optimizer:
  _target_: torch.optim.Adam
  lr: 0.001
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    let res = ctx
        .request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
            text_document: TextDocumentIdentifier {
                uri: ctx.doc_uri("test.yaml"),
            },
        })
        .await;

    if let Some(SemanticTokensResult::Tokens(tokens)) = res {
        assert!(!tokens.data.is_empty(), "Should have semantic tokens");

        let token_summary: Vec<_> = tokens
            .data
            .iter()
            .map(|t| {
                format!(
                    "delta_line={}, delta_start={}, length={}, type={}, modifiers={}",
                    t.delta_line,
                    t.delta_start,
                    t.length,
                    token_type_name(t.token_type),
                    t.token_modifiers_bitset
                )
            })
            .collect();

        insta::assert_yaml_snapshot!("semantic_tokens_multiple_targets", token_summary);
    } else {
        panic!("Expected semantic tokens but got None");
    }
}

#[tokio::test]
async fn test_semantic_tokens_with_string_values() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
config:
  _target_: my_module.DataLoader
  name: "test_model"
  path: '/tmp/model'
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    let res = ctx
        .request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
            text_document: TextDocumentIdentifier {
                uri: ctx.doc_uri("test.yaml"),
            },
        })
        .await;

    if let Some(SemanticTokensResult::Tokens(tokens)) = res {
        assert!(!tokens.data.is_empty(), "Should have semantic tokens");

        let token_summary: Vec<_> = tokens
            .data
            .iter()
            .map(|t| {
                format!(
                    "delta_line={}, delta_start={}, length={}, type={}, modifiers={}",
                    t.delta_line,
                    t.delta_start,
                    t.length,
                    token_type_name(t.token_type),
                    t.token_modifiers_bitset
                )
            })
            .collect();

        insta::assert_yaml_snapshot!("semantic_tokens_string_values", token_summary);
    } else {
        panic!("Expected semantic tokens but got None");
    }
}

#[tokio::test]
async fn test_semantic_tokens_workspace_config() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // Use the actual config.yaml from the workspace
    let content = std::fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
    ctx.open_document("config.yaml", content).await;

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

    if let Some(SemanticTokensResult::Tokens(tokens)) = res {
        assert!(!tokens.data.is_empty(), "Should have semantic tokens");

        let token_summary: Vec<_> = tokens
            .data
            .iter()
            .map(|t| {
                format!(
                    "delta_line={}, delta_start={}, length={}, type={}, modifiers={}",
                    t.delta_line,
                    t.delta_start,
                    t.length,
                    token_type_name(t.token_type),
                    t.token_modifiers_bitset
                )
            })
            .collect();

        insta::assert_yaml_snapshot!("semantic_tokens_workspace_config", token_summary);
    } else {
        panic!("Expected semantic tokens but got None");
    }
}

#[tokio::test]
async fn test_no_semantic_tokens_non_hydra_file() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // Regular YAML without Hydra markers
    let content = r#"
# Regular YAML file without Hydra markers
key: value
nested:
  another_key: another_value
list:
  - item1
  - item2
"#;
    ctx.open_document("regular.yaml", content.to_string()).await;

    let res = ctx
        .request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
            text_document: TextDocumentIdentifier {
                uri: ctx.doc_uri("regular.yaml"),
            },
        })
        .await;

    assert!(
        res.is_none(),
        "Should not get semantic tokens for non-Hydra file"
    );
}

#[tokio::test]
async fn test_semantic_tokens_nested_target() {
    let mut ctx = TestContext::new(TestWorkspace::Nested);
    ctx.initialize().await;

    let content = r#"# @hydra
outer:
  _target_: my_module.OuterClass
  name: "outer"
  inner:
    _target_: my_module.InnerClass
    value: 42
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    let res = ctx
        .request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
            text_document: TextDocumentIdentifier {
                uri: ctx.doc_uri("test.yaml"),
            },
        })
        .await;

    if let Some(SemanticTokensResult::Tokens(tokens)) = res {
        assert!(!tokens.data.is_empty(), "Should have semantic tokens");

        let token_summary: Vec<_> = tokens
            .data
            .iter()
            .map(|t| {
                format!(
                    "delta_line={}, delta_start={}, length={}, type={}, modifiers={}",
                    t.delta_line,
                    t.delta_start,
                    t.length,
                    token_type_name(t.token_type),
                    t.token_modifiers_bitset
                )
            })
            .collect();

        insta::assert_yaml_snapshot!("semantic_tokens_nested_target", token_summary);
    } else {
        panic!("Expected semantic tokens but got None");
    }
}

#[tokio::test]
async fn test_semantic_tokens_list_of_targets() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
items:
  - _target_: my_module.DataLoader
    batch_size: 16
  - _target_: my_module.create_model
    input_dim: 10
    beep: ["boop", "bap"]
    bool: [true, false]
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    let res = ctx
        .request::<request::SemanticTokensFullRequest>(SemanticTokensParams {
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
            text_document: TextDocumentIdentifier {
                uri: ctx.doc_uri("test.yaml"),
            },
        })
        .await;

    if let Some(SemanticTokensResult::Tokens(tokens)) = res {
        assert!(!tokens.data.is_empty(), "Should have semantic tokens");

        let token_summary: Vec<_> = tokens
            .data
            .iter()
            .map(|t| {
                format!(
                    "delta_line={}, delta_start={}, length={}, type={}, modifiers={}",
                    t.delta_line,
                    t.delta_start,
                    t.length,
                    token_type_name(t.token_type),
                    t.token_modifiers_bitset
                )
            })
            .collect();

        insta::assert_yaml_snapshot!("semantic_tokens_list_of_targets", token_summary);
    } else {
        panic!("Expected semantic tokens but got None");
    }
}
