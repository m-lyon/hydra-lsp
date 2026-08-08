//! End-to-end tests for `workspace/didChangeWatchedFiles`-driven invalidation.
//!
//! These drive the real `HydraLspBackend` over the LSP protocol against a real
//! on-disk temp workspace. Unlike the unit tests in `python_cache.rs` (which
//! call `File::sync_path` directly), these exercise the full wiring:
//!
//!   client CREATE/DELETE event
//!     → `did_change_watched_files` (extension filter, `File::sync_path`)
//!     → salsa invalidation of `resolve_module_cached`
//!     → goto-definition response
//!
//! The one thing they deliberately *cannot* cover is whether the editor's file
//! watcher actually emits the event (that is client-side, e.g. VS Code's
//! `createFileSystemWatcher` glob) — here we hand-feed the notification. What
//! they *do* prove that the unit tests can't: that the path key derived from the
//! client URI (`change.uri.to_file_path()`) matches the key the resolver
//! interned from the workspace search paths.

mod common;

use std::fs;

use tower_lsp::lsp_types::notification::DidChangeWatchedFiles;
use tower_lsp::lsp_types::request::GotoDefinition;
use tower_lsp::lsp_types::*;

use crate::common::*;

/// Build a position a few columns into a `_target_` value on the line that
/// contains `needle`.
fn target_position(content: &str, needle: &str) -> Position {
    let (line_idx, line) = content
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains(needle))
        .expect("target line present");
    // A couple of columns past the start of the value keeps the cursor inside
    // the target span. All targets here are ASCII, so byte == UTF-16 offset.
    let character = line.find(needle).unwrap() + 2;
    Position {
        line: line_idx as u32,
        character: character as u32,
    }
}

/// Request goto-definition at `position` in `doc`.
async fn goto(
    ctx: &mut TestContext,
    doc: &str,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    ctx.request::<GotoDefinition>(GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            position,
            text_document: TextDocumentIdentifier {
                uri: ctx.doc_uri(doc),
            },
        },
        work_done_progress_params: WorkDoneProgressParams {
            work_done_token: None,
        },
        partial_result_params: PartialResultParams {
            partial_result_token: None,
        },
    })
    .await
}

/// Assert a goto response resolved to a file whose path ends with `suffix`.
fn assert_resolved_to(res: &Option<GotoDefinitionResponse>, suffix: &str) {
    match res {
        Some(GotoDefinitionResponse::Scalar(loc)) => {
            let path = loc.uri.path();
            assert!(
                path.ends_with(suffix),
                "expected resolution ending in {suffix:?}, got {path:?}"
            );
        }
        other => panic!("expected a scalar goto result ending in {suffix:?}, got {other:?}"),
    }
}

/// Notify the server of a single watched-file change.
async fn notify_change(ctx: &mut TestContext, doc: &str, typ: FileChangeType) {
    ctx.notify::<DidChangeWatchedFiles>(DidChangeWatchedFilesParams {
        changes: vec![FileEvent {
            uri: ctx.doc_uri(doc),
            typ,
        }],
    })
    .await;
}

/// The server must ask the client to watch the expected globs via dynamic
/// capability registration. This is the "does the server request the right
/// watchers" half of the wiring; the tests below cover the "does the server
/// react to the events" half. Together they replace the old drift-prone
/// arrangement where the glob list lived only in the client.
#[tokio::test]
async fn test_server_registers_watched_file_globs() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize_with_watched_files_support().await;

    // The server sends `client/registerCapability` from `initialized`; read it
    // off the pipe and answer it so the server's await completes.
    let (id, params) = ctx.recv_request("client/registerCapability").await;
    ctx.reply_ok(id).await;

    let params: RegistrationParams = serde_json::from_value(params).unwrap();
    let globs: Vec<String> = params
        .registrations
        .iter()
        .filter(|r| r.method == "workspace/didChangeWatchedFiles")
        .filter_map(|r| {
            serde_json::from_value::<DidChangeWatchedFilesRegistrationOptions>(
                r.register_options.clone()?,
            )
            .ok()
        })
        .flat_map(|options| options.watchers)
        .map(|watcher| match watcher.glob_pattern {
            GlobPattern::String(glob) => glob,
            other => panic!("expected a string glob pattern, got {other:?}"),
        })
        .collect();

    assert!(
        globs.contains(&"**/*.{py,pyi,pth}".to_string()),
        "Python globs must be registered, got {globs:?}"
    );
    // YAML is intentionally NOT watched: config files are handled by
    // text-document sync (open editor buffers), and the watched-files handler
    // discards YAML events. See `WATCHED_PY_GLOB`.
    assert!(
        !globs
            .iter()
            .any(|g| g.contains("yaml") || g.contains("yml")),
        "YAML globs must not be registered (handled by document sync), got {globs:?}"
    );
}

/// A `_target_` pointing at a module that does not exist yet must resolve once
/// the module file is created on disk and a CREATE event arrives — no config
/// touch required. This is the exact gap the raw `Path::exists()` probes left
/// open.
#[tokio::test]
async fn test_watched_create_resolves_new_module() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let yaml = "# @hydra\nnode:\n  _target_: brand_new_module.NewClass\n";
    fs::write(ctx.workspace.path().join("new_config.yaml"), yaml).unwrap();
    ctx.open_document("new_config.yaml", yaml.to_string()).await;

    let pos = target_position(yaml, "brand_new_module.NewClass");

    // Before the module exists, the target must not resolve.
    let before = goto(&mut ctx, "new_config.yaml", pos).await;
    assert!(
        before.is_none(),
        "target should not resolve before the module is created, got {before:?}"
    );

    // Create the module on disk and notify the server, as the client would.
    fs::write(
        ctx.workspace.path().join("brand_new_module.py"),
        "class NewClass:\n    \"\"\"A brand new class.\"\"\"\n    pass\n",
    )
    .unwrap();
    notify_change(&mut ctx, "brand_new_module.py", FileChangeType::CREATED).await;

    // The create must now be observed without any config change.
    let after = goto(&mut ctx, "new_config.yaml", pos).await;
    assert_resolved_to(&after, "brand_new_module.py");
}

/// Same as above, but the new module lives in a newly-created subdirectory —
/// verifies the path-key matching holds for deep paths.
#[tokio::test]
async fn test_watched_create_resolves_nested_module() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let yaml = "# @hydra\nnode:\n  _target_: pkg.sub.new_mod.NestedClass\n";
    fs::write(ctx.workspace.path().join("nested_config.yaml"), yaml).unwrap();
    ctx.open_document("nested_config.yaml", yaml.to_string())
        .await;

    let pos = target_position(yaml, "pkg.sub.new_mod.NestedClass");

    let before = goto(&mut ctx, "nested_config.yaml", pos).await;
    assert!(
        before.is_none(),
        "nested target should not resolve before creation, got {before:?}"
    );

    // Create the nested package path on disk, then notify for the leaf file.
    fs::create_dir_all(ctx.workspace.path().join("pkg").join("sub")).unwrap();
    fs::write(
        ctx.workspace
            .path()
            .join("pkg")
            .join("sub")
            .join("new_mod.py"),
        "class NestedClass:\n    pass\n",
    )
    .unwrap();
    notify_change(&mut ctx, "pkg/sub/new_mod.py", FileChangeType::CREATED).await;

    let after = goto(&mut ctx, "nested_config.yaml", pos).await;
    assert_resolved_to(&after, "new_mod.py");
}

/// A target that resolves must stop resolving once its module file is deleted
/// and a DELETE event arrives.
///
/// Note this is a weaker guard than the create tests: even without the
/// `resolve_module_cached` tracking fix, delete self-heals downstream — the
/// definition extraction reads the module through salsa `source_text`, which
/// `File::sync_path` invalidates, so extraction fails and goto returns nothing
/// regardless. The test still locks in the correct end-to-end behavior.
#[tokio::test]
async fn test_watched_delete_unresolves_module() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // `my_module.py` is copied into the Simple workspace, so this resolves
    // immediately.
    let yaml = "# @hydra\nnode:\n  _target_: my_module.DataLoader\n";
    fs::write(ctx.workspace.path().join("del_config.yaml"), yaml).unwrap();
    ctx.open_document("del_config.yaml", yaml.to_string()).await;

    let pos = target_position(yaml, "my_module.DataLoader");

    let before = goto(&mut ctx, "del_config.yaml", pos).await;
    assert_resolved_to(&before, "my_module.py");

    // Delete the module on disk and notify the server.
    fs::remove_file(ctx.workspace.path().join("my_module.py")).unwrap();
    notify_change(&mut ctx, "my_module.py", FileChangeType::DELETED).await;

    let after = goto(&mut ctx, "del_config.yaml", pos).await;
    assert!(
        after.is_none(),
        "target should stop resolving after the module is deleted, got {after:?}"
    );
}
