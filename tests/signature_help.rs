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

    // _args_[0] should highlight the first positional param (pos_only, index 0)
    assert_eq!(
        sig_help.active_parameter,
        Some(0),
        "First positional arg should highlight pos_only (index 0)"
    );

    // Cursor on second positional arg (line 5: "    - 20") — should highlight regular (index 1)
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
        Some(1),
        "Second positional arg should highlight regular (index 1)"
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

    // Cursor on keyword arg — should highlight **kwargs since it's an unknown kwarg
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
    // Unknown kwarg should highlight **kwargs (index 1)
    let kwargs_index = sig_help2.signatures[0]
        .parameters
        .as_ref()
        .unwrap()
        .iter()
        .position(|p| match &p.label {
            ParameterLabel::Simple(name) => name.starts_with("**"),
            _ => false,
        })
        .expect("Should have **kwargs parameter");
    assert_eq!(
        sig_help2.active_parameter,
        Some(kwargs_index as u32),
        "Unknown kwarg should highlight **kwargs"
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

    // Inline _args_ line maps to Positional(0), which should highlight
    // the first positional param (pos_only at index 0, since regular params
    // come before *args).
    // Note: for inline _args_ the _args_ key line maps to Positional(0).
    assert_eq!(
        sig_help.active_parameter,
        Some(0),
        "Inline positional args should highlight first positional parameter"
    );
}

#[tokio::test]
async fn test_signature_help_inline_args_cursor_per_item() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // complex_function(pos_only, /, regular, *args, keyword_only, another_kw=None, **kwargs)
    // Signature params (after filtering): pos_only, regular, *args, keyword_only, another_kw, **kwargs
    //                                     idx 0     idx 1    idx 2
    let content = r#"# @hydra
test:
  _target_: my_module.complex_function
  _args_: [10, 20, 30]
  keyword_only: value
"#;
    //  line 3: "  _args_: [10, 20, 30]"
    //  columns: 0123456789012345678901
    //                      ^10 ^20 ^30
    ctx.open_document("test.yaml", content.to_string()).await;

    // Helper to request signature help at a given column on line 3
    async fn sig_help_at_col(ctx: &mut TestContext, col: u32) -> Option<SignatureHelp> {
        ctx.request::<request::SignatureHelpRequest>(SignatureHelpParams {
            context: None,
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 3,
                    character: col,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("test.yaml"),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        })
        .await
    }

    // Cursor before any item (on the _args_ key or '[') → first positional (pos_only, idx 0)
    let sh = sig_help_at_col(&mut ctx, 5).await.expect("should trigger");
    assert_eq!(sh.active_parameter, Some(0), "before items → pos_only (idx 0)");

    // Cursor on first item '10' (col 11) → pos_only (idx 0)
    let sh = sig_help_at_col(&mut ctx, 11).await.expect("should trigger");
    assert_eq!(sh.active_parameter, Some(0), "on first item → pos_only (idx 0)");

    // Cursor after first comma (col 14, on ' 20') → regular (idx 1)
    let sh = sig_help_at_col(&mut ctx, 15).await.expect("should trigger");
    assert_eq!(sh.active_parameter, Some(1), "on second item → regular (idx 1)");

    // Cursor on third item '30' (col 19) → *args (idx 2), since there are only 2 regular params
    let sh = sig_help_at_col(&mut ctx, 19).await.expect("should trigger");
    assert_eq!(sh.active_parameter, Some(2), "on third item → *args (idx 2)");
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

    // _args_[0] with dash trigger should highlight the first positional param (pos_only, index 0)
    assert_eq!(
        sig_help.active_parameter,
        Some(0),
        "Dash-triggered signature help should highlight first positional parameter"
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
    // YAML key 'args' should NOT match '*args'. Since variadic_function has
    // **kwargs, the unknown keyword should highlight **kwargs.
    let kwargs_index = sig_help.signatures[0]
        .parameters
        .as_ref()
        .unwrap()
        .iter()
        .position(|p| match &p.label {
            ParameterLabel::Simple(name) => name.starts_with("**"),
            _ => false,
        })
        .expect("Should have **kwargs parameter");
    assert_eq!(
        sig_help.active_parameter,
        Some(kwargs_index as u32),
        "YAML key 'args' should NOT match '*args' — should highlight **kwargs instead"
    );
}

#[tokio::test]
async fn test_signature_help_no_trigger_on_args_colon() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // Document state when user has just typed "_args_:" (no bracket yet)
    let content = r#"# @hydra
test:
  _target_: my_module.complex_function
  _args_:
"#;
    ctx.open_document("test.yaml", content.to_string()).await;

    // Simulate the ":" trigger on the _args_ key line
    let res = ctx
        .request::<request::SignatureHelpRequest>(SignatureHelpParams {
            context: Some(SignatureHelpContext {
                trigger_kind: SignatureHelpTriggerKind::TRIGGER_CHARACTER,
                trigger_character: Some(":".to_string()),
                is_retrigger: false,
                active_signature_help: None,
            }),
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 3,
                    character: 9,
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
        "Signature help should NOT trigger on ':' for bare _args_: (no bracket)"
    );
}

#[tokio::test]
async fn test_signature_help_inline_args_comma_advances_highlight() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // Simulate the state after the user types a comma: _args_: [1,]
    // (editor auto-closes the bracket)
    let content = r#"# @hydra
test:
  _target_: my_module.complex_function
  _args_: [1,]
"#;
    //  line 3: "  _args_: [1,]"
    //  columns: 01234567890123
    ctx.open_document("test.yaml", content.to_string()).await;

    // Cursor right after the comma (col 13), before the closing ']'
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

    let sig_help = res.expect("Signature help should trigger on ',' in inline _args_");
    // One comma seen → positional index 1 → should highlight "regular" (the second
    // positional param of complex_function)
    assert_eq!(
        sig_help.active_parameter,
        Some(1),
        "After comma, highlight should advance to the next positional parameter"
    );
}
