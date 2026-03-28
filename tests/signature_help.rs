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

#[tokio::test]
async fn test_signature_help_positional_args_highlights_star_args() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // complex_function(pos_only, /, regular, *args, keyword_only, another_kw=None, **kwargs)
    let content = r#"# @hydra
test:
  _target_: my_module.complex_function
  _args_:
    - 10
    - 20
  keyword_only: value
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    // Cursor on first positional arg (line 4: "    - 10")
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

    let sig_help = res.expect("Expected signature help for positional arg");
    assert!(!sig_help.signatures.is_empty(), "Should have signatures");

    // Find which parameter index corresponds to *args
    let params = sig_help.signatures[0]
        .parameters
        .as_ref()
        .expect("Should have parameters");
    let args_index = params
        .iter()
        .position(|p| match &p.label {
            ParameterLabel::Simple(name) => name.starts_with('*') && !name.starts_with("**"),
            _ => false,
        })
        .expect("Should have *args parameter");

    assert_eq!(
        sig_help.active_parameter,
        Some(args_index as u32),
        "Positional arg should highlight *args parameter"
    );

    // Cursor on second positional arg (line 5: "    - 20") — should also highlight *args
    let res2 = ctx
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

    let sig_help2 = res2.expect("Expected signature help for second positional arg");
    assert_eq!(
        sig_help2.active_parameter,
        Some(args_index as u32),
        "Second positional arg should also highlight *args"
    );
}

#[tokio::test]
async fn test_signature_help_keyword_only_with_complex_function() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // complex_function(pos_only, /, regular, *args, keyword_only, another_kw=None, **kwargs)
    let content = r#"# @hydra
test:
  _target_: my_module.complex_function
  keyword_only: value
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

    let sig_help = res.expect("Expected signature help for keyword_only");
    let params = sig_help.signatures[0]
        .parameters
        .as_ref()
        .expect("Should have parameters");

    // Find the index of keyword_only
    let kw_index = params
        .iter()
        .position(|p| match &p.label {
            ParameterLabel::Simple(name) => name.starts_with("keyword_only"),
            _ => false,
        })
        .expect("Should have keyword_only parameter");

    assert_eq!(
        sig_help.active_parameter,
        Some(kw_index as u32),
        "Should highlight keyword_only parameter"
    );
}

#[tokio::test]
async fn test_signature_help_variadic_function() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // variadic_function(*args, **kwargs)
    let content = r#"# @hydra
test:
  _target_: my_module.variadic_function
  _args_:
    - hello
  some_kwarg: world
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    // Cursor on positional arg — should highlight *args
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

    let sig_help = res.expect("Expected signature help for positional arg");
    assert_eq!(
        sig_help.active_parameter,
        Some(0),
        "Positional arg should highlight *args (index 0)"
    );

    // Cursor on keyword arg — should not highlight any parameter (unknown kwarg)
    let res2 = ctx
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

    let sig_help2 = res2.expect("Expected signature help for kwarg");
    assert!(!sig_help2.signatures.is_empty());
    let param_count = sig_help2.signatures[0]
        .parameters
        .as_ref()
        .map_or(0, |p| p.len()) as u32;
    assert_eq!(
        sig_help2.active_parameter,
        Some(param_count),
        "Unknown kwarg should not highlight any parameter"
    );
}

#[tokio::test]
async fn test_signature_help_positional_args_highlights_star_args_inline() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // complex_function(pos_only, /, regular, *args, keyword_only, another_kw=None, **kwargs)
    let content = r#"# @hydra
test:
  _target_: my_module.complex_function
  _args_: [10, 20]
  keyword_only: value
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    // Cursor on the inline _args_ line (line 3: "  _args_: [10, 20]")
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

    let sig_help = res.expect("Expected signature help for inline positional args");
    assert!(!sig_help.signatures.is_empty(), "Should have signatures");

    // Find which parameter index corresponds to *args
    let params = sig_help.signatures[0]
        .parameters
        .as_ref()
        .expect("Should have parameters");
    let args_index = params
        .iter()
        .position(|p| match &p.label {
            ParameterLabel::Simple(name) => name.starts_with('*') && !name.starts_with("**"),
            _ => false,
        })
        .expect("Should have *args parameter");

    assert_eq!(
        sig_help.active_parameter,
        Some(args_index as u32),
        "Inline positional args should highlight *args parameter"
    );
}

#[tokio::test]
async fn test_signature_help_args_trigger_dash() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
test:
  _target_: my_module.complex_function
  _args_:
    - 10
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    // Simulate signature help triggered by "-" on the args item line
    let res = ctx
        .request::<request::SignatureHelpRequest>(SignatureHelpParams {
            context: Some(SignatureHelpContext {
                trigger_kind: SignatureHelpTriggerKind::TRIGGER_CHARACTER,
                trigger_character: Some("-".to_string()),
                is_retrigger: false,
                active_signature_help: None,
            }),
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 4,
                    character: 6,
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

    let sig_help = res.expect("Signature help should trigger on '-' for _args_ items");
    assert!(!sig_help.signatures.is_empty(), "Should have signatures");

    // Should highlight *args parameter
    let params = sig_help.signatures[0]
        .parameters
        .as_ref()
        .expect("Should have parameters");
    let args_index = params
        .iter()
        .position(|p| match &p.label {
            ParameterLabel::Simple(name) => name.starts_with('*') && !name.starts_with("**"),
            _ => false,
        })
        .expect("Should have *args parameter");
    assert_eq!(
        sig_help.active_parameter,
        Some(args_index as u32),
        "Dash-triggered signature help should highlight *args"
    );
}

#[tokio::test]
async fn test_signature_help_args_trigger_bracket() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
test:
  _target_: my_module.complex_function
  _args_: [10, 20]
  keyword_only: value
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    // Simulate signature help triggered by "[" on the inline args line
    let res = ctx
        .request::<request::SignatureHelpRequest>(SignatureHelpParams {
            context: Some(SignatureHelpContext {
                trigger_kind: SignatureHelpTriggerKind::TRIGGER_CHARACTER,
                trigger_character: Some("[".to_string()),
                is_retrigger: false,
                active_signature_help: None,
            }),
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 3,
                    character: 11,
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

    let sig_help = res.expect("Signature help should trigger on '[' for inline _args_");
    assert!(!sig_help.signatures.is_empty(), "Should have signatures");
}

#[tokio::test]
async fn test_signature_help_args_trigger_comma() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
test:
  _target_: my_module.complex_function
  _args_: [10, 20]
  keyword_only: value
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    // Simulate signature help triggered by "," between inline args
    let res = ctx
        .request::<request::SignatureHelpRequest>(SignatureHelpParams {
            context: Some(SignatureHelpContext {
                trigger_kind: SignatureHelpTriggerKind::TRIGGER_CHARACTER,
                trigger_character: Some(",".to_string()),
                is_retrigger: false,
                active_signature_help: None,
            }),
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 3,
                    character: 14,
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

    let sig_help = res.expect("Signature help should trigger on ',' for inline _args_");
    assert!(!sig_help.signatures.is_empty(), "Should have signatures");
}

#[tokio::test]
async fn test_signature_help_keyword_args_should_not_match_star_args() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // variadic_function(*args, **kwargs)
    // A YAML key "args" should NOT match "*args" — it's an unknown keyword parameter
    let content = r#"# @hydra
test:
  _target_: my_module.variadic_function
  args: some_value
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

    let sig_help = res.expect("Should still return signature help");
    let param_count = sig_help.signatures[0]
        .parameters
        .as_ref()
        .map_or(0, |p| p.len()) as u32;
    assert_eq!(
        sig_help.active_parameter,
        Some(param_count),
        "YAML key 'args' should NOT match '*args' — active_parameter should be out-of-bounds"
    );
}
