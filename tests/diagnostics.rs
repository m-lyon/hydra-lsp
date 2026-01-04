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

#[tokio::test]
async fn test_diagnostics_multiple_errors() {
    let mut ctx = TestContext::new(TestWorkspace::Diagnostics);
    ctx.initialize().await;

    let content = std::fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
    ctx.open_document("config.yaml", content).await;

    // Receive diagnostics
    let dp = ctx.recv::<PublishDiagnosticsParams>().await;

    assert_eq!(dp.uri, ctx.doc_uri("config.yaml"));
    let diagnostics = dp.diagnostics;

    // Should have diagnostics for missing parameters
    assert!(!diagnostics.is_empty(), "Should have diagnostics");

    // Serialize diagnostics for snapshot testing
    let diagnostic_summary: Vec<_> = diagnostics
        .iter()
        .map(|d| {
            format!(
                "Line {}, Col {}-{}: {} (severity: {:?}, code: '{}')",
                d.range.start.line,
                d.range.start.character,
                d.range.end.character,
                d.message,
                d.severity.unwrap(),
                extract_code(d)
            )
        })
        .collect();

    insta::assert_yaml_snapshot!("diagnostics_multiple_errors", diagnostic_summary);
}

#[tokio::test]
async fn test_diagnostics_unknown_param() {
    let mut ctx = TestContext::new(TestWorkspace::Diagnostics);
    ctx.initialize().await;

    let content = std::fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
    ctx.open_document("config.yaml", content).await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Check for unknown parameter diagnostic
    let unknown_param_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("unknown_param") || d.message.contains("Unknown parameter"));

    assert!(
        unknown_param_diag.is_some(),
        "Should have diagnostic for unknown parameter"
    );

    if let Some(diag) = unknown_param_diag {
        insta::assert_snapshot!(
            "diagnostic_unknown_param",
            format!(
                "Message: {}\nSeverity: {:?}\nCode: '{}'",
                diag.message,
                diag.severity.unwrap(),
                extract_code(diag)
            )
        );
    }
}

#[tokio::test]
async fn test_no_diagnostics_valid_config() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
model:
  _target_: my_module.DataLoader
  batch_size: 32
  shuffle: true
"#;
    ctx.open_document("valid.yaml", content.to_string()).await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Filter out any non-error diagnostics
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect();

    let summary = serde_json::json!({
        "error_count": errors.len(),
        "total_diagnostics": diagnostics.len()
    });

    insta::assert_yaml_snapshot!("no_errors_valid_config", summary);
}

#[tokio::test]
async fn test_diagnostics_module_not_found() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
model:
  _target_: nonexistent.module.ClassName
  param1: value
"#;
    ctx.open_document("invalid_module.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Should have module not found error
    let module_not_found = diagnostics
        .iter()
        .find(|d| d.message.contains("Cannot resolve module"));

    assert!(
        module_not_found.is_some(),
        "Should have module not found diagnostic"
    );

    if let Some(diag) = module_not_found {
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diag.code,
            Some(NumberOrString::String("module-not-found".to_string()))
        );
        insta::assert_snapshot!(
            "diagnostic_module_not_found",
            format!("Message: {}\nCode: '{}'", diag.message, extract_code(diag))
        );
    }
}

#[tokio::test]
async fn test_diagnostics_symbol_not_found() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
model:
  _target_: my_module.NonExistentClass
  param1: value
"#;
    ctx.open_document("invalid_symbol.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Should have symbol not found error
    let symbol_not_found = diagnostics
        .iter()
        .find(|d| d.message.contains("not found in module"));

    assert!(
        symbol_not_found.is_some(),
        "Should have symbol not found diagnostic"
    );

    if let Some(diag) = symbol_not_found {
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diag.code,
            Some(NumberOrString::String("symbol-not-found".to_string()))
        );
        insta::assert_snapshot!(
            "diagnostic_symbol_not_found",
            format!("Message: {}\nCode: '{}'", diag.message, extract_code(diag))
        );
    }
}

#[tokio::test]
async fn test_diagnostics_invalid_target_format() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"# @hydra
model:
  _target_: InvalidTarget
  param1: value
"#;
    ctx.open_document("invalid_format.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Should have invalid format error
    let invalid_format = diagnostics
        .iter()
        .find(|d| d.message.contains("Invalid _target_ format"));

    assert!(
        invalid_format.is_some(),
        "Should have invalid format diagnostic"
    );

    if let Some(diag) = invalid_format {
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diag.code,
            Some(NumberOrString::String("invalid-target".to_string()))
        );
        insta::assert_snapshot!(
            "diagnostic_invalid_format",
            format!("Message: {}\nCode: '{}'", diag.message, extract_code(diag))
        );
    }
}

#[tokio::test]
async fn test_diagnostics_with_kwargs() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // Create a Python module with a function that accepts **kwargs
    let py_content = r#"
def flexible_function(required_param: str, **kwargs):
    """Function that accepts any additional keyword arguments."""
    pass
"#;
    std::fs::write(ctx.workspace.path().join("kwargs_module.py"), py_content).unwrap();

    let yaml_content = r#"# @hydra
model:
  _target_: kwargs_module.flexible_function
  required_param: "value"
  extra_param1: 123
  extra_param2: "another"
"#;
    ctx.open_document("kwargs_config.yaml", yaml_content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Should have HINT diagnostics for extra params, not errors
    let kwargs_hints: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("**kwargs"))
        .collect();

    if !kwargs_hints.is_empty() {
        assert!(
            kwargs_hints
                .iter()
                .all(|d| d.severity == Some(DiagnosticSeverity::HINT)),
            "Extra params with **kwargs should be hints, not errors"
        );
    }

    // Should not have errors for unknown parameters
    let unknown_param_errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| {
            d.message.contains("Unknown parameter") && d.severity == Some(DiagnosticSeverity::ERROR)
        })
        .collect();

    assert!(
        unknown_param_errors.is_empty(),
        "Should not have unknown parameter errors when **kwargs is present"
    );
}

// ==================== Nested Target Tests ====================

#[tokio::test]
async fn test_nested_diagnostics_all_valid() {
    let mut ctx = TestContext::new(TestWorkspace::Nested);
    ctx.initialize().await;

    let content = std::fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
    ctx.open_document("config.yaml", content).await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Filter diagnostics for model_one (should have no errors)
    // model_one is on lines 5-15 approximately
    let model_one_errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| {
            d.range.start.line >= 5
                && d.range.start.line <= 15
                && d.severity == Some(DiagnosticSeverity::ERROR)
        })
        .collect();

    assert!(
        model_one_errors.is_empty(),
        "model_one should have no errors. Found: {:?}",
        model_one_errors
    );
}

#[tokio::test]
async fn test_nested_diagnostics_all_errors() {
    let mut ctx = TestContext::new(TestWorkspace::Nested);
    ctx.initialize().await;

    let content = std::fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
    ctx.open_document("config.yaml", content).await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Get all errors
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect();

    // Should have multiple errors across the nested configs
    assert!(
        errors.len() >= 4,
        "Should have at least 4 errors (missing d_value, missing d_value + b_value twice, unknown x_value). Found: {}",
        errors.len()
    );

    // Create comprehensive summary
    let summary: Vec<_> = errors
        .iter()
        .map(|d| {
            serde_json::json!({
                "line": d.range.start.line,
                "start_char": d.range.start.character,
                "end_char": d.range.end.character,
                "message": d.message,
                "severity": format!("{:?}", d.severity.unwrap()),
                "code": extract_code(d)
            })
        })
        .collect();

    insta::assert_yaml_snapshot!("nested_all_errors", summary);
}
