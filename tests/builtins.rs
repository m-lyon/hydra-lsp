//! End-to-end coverage for `_target_` values that name a Python builtin.
//!
//! Hydra instantiates builtins only through the `builtins.` prefix, and the
//! signatures come from the typeshed stubs vendored into the binary rather than
//! from any file in the workspace. These tests exercise that path through the
//! server, against the real stub — see `tests/README.md` for the harness.

mod common;

use tower_lsp::lsp_types::*;

use crate::common::*;

fn extract_code(diagnostic: &Diagnostic) -> String {
    diagnostic
        .code
        .as_ref()
        .map(|c| match c {
            NumberOrString::Number(n) => n.to_string(),
            NumberOrString::String(s) => s.clone(),
        })
        .unwrap_or_else(|| "none".to_string())
}

/// Open `content` as `name` and return the published diagnostics.
async fn diagnostics_for(name: &str, content: &str) -> Vec<Diagnostic> {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;
    ctx.open_document(name, content.to_string()).await;
    ctx.recv::<PublishDiagnosticsParams>().await.diagnostics
}

/// Render diagnostics as `code: message`, for readable assertion failures.
fn summarize(diagnostics: &[Diagnostic]) -> Vec<String> {
    diagnostics
        .iter()
        .map(|d| format!("{}: {}", extract_code(d), d.message))
        .collect()
}

/// Hover markdown for the `_target_` on `line` of `content`.
async fn hover_for(name: &str, content: &str, line: u32) -> String {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;
    ctx.open_document(name, content.to_string()).await;

    let res = ctx
        .request::<request::HoverRequest>(HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line,
                    character: 13,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri(name),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        })
        .await;

    match res.expect("expected a hover response").contents {
        HoverContents::Markup(markup) => markup.value,
        other => panic!("expected markup hover content, got {other:?}"),
    }
}

#[tokio::test]
async fn test_builtin_function_resolves_without_diagnostics() {
    let content = r#"# @hydra
length:
  _target_: builtins.len
  _args_: [[1, 2, 3]]
"#;
    let diagnostics = diagnostics_for("builtin_len.yaml", content).await;
    assert!(
        diagnostics.is_empty(),
        "builtins.len with _args_ should be clean, got: {:?}",
        summarize(&diagnostics)
    );
}

#[tokio::test]
async fn test_builtin_hover_shows_signature_and_docstring() {
    let content = r#"# @hydra
length:
  _target_: builtins.len
  _args_: [[1, 2, 3]]
"#;
    let hover = hover_for("builtin_len_hover.yaml", content, 2).await;

    // The `/` marker is what tells the reader `obj` cannot be passed by name.
    assert!(
        hover.contains("def len(obj: Sized, /) -> int"),
        "hover should show the positional-only signature, got:\n{hover}"
    );
    assert!(
        hover.contains("Return the number of items in a container"),
        "hover should show the typeshed docstring, got:\n{hover}"
    );
}

/// Every builtin named in the issue's acceptance criteria. `dict`, `list`,
/// `set`, `int`, `str`, `range`, `open` and `sorted` are all either overloaded
/// or constructed through `__new__`, so any of them would produce a false
/// `unknown-argument` or `missing-argument` without that handling.
#[tokio::test]
async fn test_common_builtins_resolve_without_false_argument_errors() {
    let content = r#"# @hydra
a:
  _target_: builtins.dict
b:
  _target_: builtins.list
c:
  _target_: builtins.set
d:
  _target_: builtins.tuple
e:
  _target_: builtins.int
f:
  _target_: builtins.str
g:
  _target_: builtins.range
h:
  _target_: builtins.open
i:
  _target_: builtins.sorted
"#;
    let diagnostics = diagnostics_for("builtin_types.yaml", content).await;
    assert!(
        diagnostics.is_empty(),
        "no builtin should report a diagnostic, got: {:?}",
        summarize(&diagnostics)
    );
}

#[tokio::test]
async fn test_keyword_argument_for_positional_only_parameter_is_reported() {
    let content = r#"# @hydra
length:
  _target_: builtins.len
  obj: [1, 2, 3]
"#;
    let diagnostics = diagnostics_for("builtin_posonly.yaml", content).await;

    let positional_only: Vec<_> = diagnostics
        .iter()
        .filter(|d| extract_code(d) == "positional-only-parameter")
        .collect();

    assert_eq!(
        positional_only.len(),
        1,
        "expected one positional-only diagnostic, got: {:?}",
        summarize(&diagnostics)
    );
    let message = &positional_only[0].message;
    assert!(
        message.contains("'obj'") && message.contains("_args_"),
        "diagnostic should name the parameter and point at _args_, got: {message}"
    );
    // `obj` is a real parameter, so it must not also be reported as unknown.
    assert!(
        !diagnostics
            .iter()
            .any(|d| extract_code(d) == "unknown-argument"),
        "positional-only should not also be unknown, got: {:?}",
        summarize(&diagnostics)
    );
}

#[tokio::test]
async fn test_missing_positional_only_argument_points_at_args() {
    let content = r#"# @hydra
length:
  _target_: builtins.len
"#;
    let diagnostics = diagnostics_for("builtin_missing.yaml", content).await;

    let missing: Vec<_> = diagnostics
        .iter()
        .filter(|d| extract_code(d) == "missing-argument")
        .collect();

    assert_eq!(
        missing.len(),
        1,
        "expected one missing-argument diagnostic, got: {:?}",
        summarize(&diagnostics)
    );
    assert!(
        missing[0].message.contains("_args_"),
        "a positional-only parameter cannot be supplied by name, so the message \
         should point at _args_, got: {}",
        missing[0].message
    );
}

#[tokio::test]
async fn test_bare_builtin_name_suggests_the_prefixed_form() {
    let content = r#"# @hydra
length:
  _target_: len
"#;
    let diagnostics = diagnostics_for("bare_builtin.yaml", content).await;

    let invalid: Vec<_> = diagnostics
        .iter()
        .filter(|d| extract_code(d) == "invalid-hydra-parameter")
        .collect();

    assert_eq!(
        invalid.len(),
        1,
        "expected one invalid-hydra-parameter diagnostic, got: {:?}",
        summarize(&diagnostics)
    );
    assert!(
        invalid[0].message.contains("builtins.len"),
        "the message should name the form Hydra accepts, got: {}",
        invalid[0].message
    );
}

#[tokio::test]
async fn test_bare_non_builtin_name_keeps_the_generic_hint() {
    let content = r#"# @hydra
thing:
  _target_: NotABuiltin
"#;
    let diagnostics = diagnostics_for("bare_name.yaml", content).await;

    let invalid: Vec<_> = diagnostics
        .iter()
        .filter(|d| extract_code(d) == "invalid-hydra-parameter")
        .collect();

    assert_eq!(invalid.len(), 1, "got: {:?}", summarize(&diagnostics));
    assert!(
        invalid[0].message.contains("module.path.SymbolName"),
        "got: {}",
        invalid[0].message
    );
}

/// Wiring in typeshed makes the whole stdlib reachable; issue #34 deliberately
/// ships only `builtins` (see `vendored_typeshed::is_vendored_module`). This
/// pins the gate so widening it is a conscious change.
#[tokio::test]
async fn test_other_stdlib_modules_are_still_unresolved() {
    let content = r#"# @hydra
now:
  _target_: datetime.datetime
"#;
    let diagnostics = diagnostics_for("stdlib_gate.yaml", content).await;

    assert!(
        diagnostics
            .iter()
            .any(|d| extract_code(d) == "unresolved-import"),
        "only builtins is resolved from the vendored stubs, got: {:?}",
        summarize(&diagnostics)
    );
}

#[tokio::test]
async fn test_suppression_comment_still_silences_a_builtin_diagnostic() {
    let content = r#"# @hydra
length:
  _target_: builtins.len # hydrust: ignore[missing-argument]
"#;
    let diagnostics = diagnostics_for("builtin_suppressed.yaml", content).await;
    assert!(
        diagnostics.is_empty(),
        "the ignore comment should suppress the diagnostic, got: {:?}",
        summarize(&diagnostics)
    );
}

/// A vendored stub has no file on disk, so there is nowhere to jump to.
/// Go-to-definition returns nothing rather than a URI the editor cannot open.
#[tokio::test]
async fn test_goto_definition_into_a_vendored_stub_is_a_no_op() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
length:
  _target_: builtins.len
  _args_: [[1, 2, 3]]
"#;
    ctx.open_document("builtin_goto.yaml", content.to_string())
        .await;

    let res = ctx
        .request::<request::GotoDefinition>(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 2,
                    character: 13,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("builtin_goto.yaml"),
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
        "expected no location for a vendored stub, got {res:?}"
    );
}
