//! Overloaded targets are narrowed to the overloads a Hydra node's arguments
//! match, the way `ty` narrows them to a call's arguments. The fixtures live in
//! `tests/workspace/simple/overloaded.py`.

mod common;

use tower_lsp::lsp_types::*;

use crate::common::*;

async fn hover_text(ctx: &mut TestContext, path: &str, content: &str) -> String {
    ctx.open_document(path, content.to_string()).await;
    let res = ctx
        .request::<request::HoverRequest>(HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 2,
                    character: 13,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri(path),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        })
        .await;
    match res.expect("expected hover").contents {
        HoverContents::Markup(markup) => markup.value,
        other => panic!("expected markup hover, got {other:?}"),
    }
}

#[tokio::test]
async fn test_hover_narrows_to_matching_overload() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let text = hover_text(
        &mut ctx,
        "text.yaml",
        "# @hydra\nf:\n  _target_: overloaded.load\n  path: a.txt\n",
    )
    .await;
    insta::assert_snapshot!("hover_overload_text", text);

    let binary = hover_text(
        &mut ctx,
        "binary.yaml",
        "# @hydra\nf:\n  _target_: overloaded.load\n  path: a.bin\n  binary: true\n",
    )
    .await;
    insta::assert_snapshot!("hover_overload_binary", binary);
}

/// Before the node's required keys are filled in nothing matches, and every
/// overload is shown rather than none.
#[tokio::test]
async fn test_hover_shows_every_overload_when_none_match() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let text = hover_text(
        &mut ctx,
        "empty.yaml",
        "# @hydra\nf:\n  _target_: overloaded.load\n",
    )
    .await;
    insta::assert_snapshot!("hover_overload_none_match", text);
}

#[tokio::test]
async fn test_hover_narrows_constructor_overloads() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let text = hover_text(
        &mut ctx,
        "reader.yaml",
        "# @hydra\nr:\n  _target_: overloaded.Reader\n  path: a.txt\n  encoding: latin-1\n",
    )
    .await;
    insta::assert_snapshot!("hover_overload_constructor", text);
}

/// Go-to-definition lands on the matching overload and on the implementation,
/// the same set `ty` offers for a call.
#[tokio::test]
async fn test_goto_definition_lands_on_matching_overload_and_implementation() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    ctx.open_document(
        "goto.yaml",
        "# @hydra\nf:\n  _target_: overloaded.load\n  path: a.bin\n  binary: true\n".to_string(),
    )
    .await;
    let res = ctx
        .request::<request::GotoDefinition>(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 2,
                    character: 13,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("goto.yaml"),
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

    let Some(GotoDefinitionResponse::Array(locations)) = res else {
        panic!("expected one location per definition, got {res:?}");
    };
    let start_lines: Vec<u32> = locations.iter().map(|l| l.range.start.line).collect();
    // The `binary` overload, from its `@overload` on line 8 (1-based), and the
    // implementation on line 10.
    assert_eq!(start_lines, vec![7, 9], "got {locations:?}");
    assert!(
        locations
            .iter()
            .all(|l| l.uri.path().ends_with("overloaded.py"))
    );
}

/// Signature help lists every overload, with the one the node's arguments
/// match made active.
#[tokio::test]
async fn test_signature_help_activates_matching_overload() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    ctx.open_document(
        "sig.yaml",
        "# @hydra\nf:\n  _target_: overloaded.load\n  path: a.bin\n  binary: true\n".to_string(),
    )
    .await;
    let res = ctx
        .request::<request::SignatureHelpRequest>(SignatureHelpParams {
            context: None,
            text_document_position_params: TextDocumentPositionParams {
                position: Position {
                    line: 4,
                    character: 4,
                },
                text_document: TextDocumentIdentifier {
                    uri: ctx.doc_uri("sig.yaml"),
                },
            },
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: None,
            },
        })
        .await;

    let sig_help = res.expect("expected signature help");
    let labels: Vec<&str> = sig_help
        .signatures
        .iter()
        .map(|s| s.label.as_str())
        .collect();
    assert_eq!(
        labels,
        vec!["load(path: str)", "load(path: str, *, binary: bool)"]
    );
    assert_eq!(sig_help.active_signature, Some(1));
    // `binary` is the second parameter of the active overload. The first
    // overload has no such parameter, so its index is out of bounds and
    // nothing is highlighted there.
    assert_eq!(sig_help.active_parameter, Some(1));
    assert_eq!(sig_help.signatures[1].active_parameter, Some(1));
    assert_eq!(sig_help.signatures[0].active_parameter, Some(1));
}
