mod common;

use std::fs;
use std::time::Duration;

use tower_lsp::lsp_types::notification::DidChangeWatchedFiles;
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

    let content = fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
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

    let content = fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
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
        .find(|d| d.message.contains("Could not resolve module"));

    assert!(
        module_not_found.is_some(),
        "Should have module not found diagnostic"
    );

    if let Some(diag) = module_not_found {
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diag.code,
            Some(NumberOrString::String("unresolved-import".to_string()))
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
            Some(NumberOrString::String("unresolved-reference".to_string()))
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
            Some(NumberOrString::String(
                "invalid-hydra-parameter".to_string()
            ))
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
    fs::write(ctx.workspace.path().join("kwargs_module.py"), py_content).unwrap();

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

    let content = fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
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

    let content = fs::read_to_string(ctx.workspace.path().join("config.yaml")).unwrap();
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
#[tokio::test]
async fn test_two_missing_targets() {
    let mut ctx = TestContext::new(TestWorkspace::Diagnostics);
    ctx.initialize().await;

    let content = r#"
training:
  lightning_module:
    _target_: made.up.Module

    metrics:
      accuracy:
        _target_: my_module.DataLoader
        batch_size: 2

    partial_optimizer:
      _target_: made.up.mod
"#;
    ctx.open_document("two_missing.yaml", content.to_string())
        .await;
    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    let diagnostics: Vec<_> = diagnostics.iter().collect();

    assert_eq!(
        diagnostics.len(),
        2,
        "Should have two missing module errors"
    );

    let summary: Vec<_> = diagnostics
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
    insta::assert_yaml_snapshot!("two_missing_targets", summary);
}

// ==================== _partial_ Integration Tests ====================

#[tokio::test]
async fn test_partial_true_suppresses_missing_params() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // my_module.DataLoader requires batch_size (required) and shuffle (optional)
    // With _partial_: true, missing batch_size should not be an error
    let content = r#"
model:
  _target_: my_module.DataLoader
  _partial_: true
  shuffle: true
"#;
    ctx.open_document("partial_config.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Filter errors
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect();

    // Should have no errors - missing batch_size is suppressed by _partial_: true
    assert!(
        errors.is_empty(),
        "Should have no errors with _partial_: true, but got: {:?}",
        errors
    );

    // _partial_ itself should not be flagged as unknown parameter
    assert!(
        !diagnostics.iter().any(|d| d.message.contains("_partial_")),
        "_partial_ should not be flagged as unknown parameter"
    );
}

#[tokio::test]
async fn test_partial_false_still_reports_missing_params() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // With _partial_: false, missing required params should still be reported
    let content = r#"
model:
  _target_: my_module.DataLoader
  _partial_: false
  shuffle: true
"#;
    ctx.open_document("partial_false_config.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Should have error for missing batch_size
    let missing_param_error = diagnostics.iter().find(|d| {
        d.message.contains("Missing required parameter")
            && d.message.contains("batch_size")
            && d.severity == Some(DiagnosticSeverity::ERROR)
    });

    assert!(
        missing_param_error.is_some(),
        "Should have missing parameter error for batch_size when _partial_: false"
    );
}

#[tokio::test]
async fn test_partial_not_reported_as_unknown_param() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // Even without other params, _partial_ should not be flagged as unknown
    let content = r#"
model:
  _target_: my_module.DataLoader
  _partial_: true
  batch_size: 32
"#;
    ctx.open_document("partial_valid.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Should have no unknown parameter error for _partial_
    let unknown_param_errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| {
            d.message.contains("Unknown parameter") && d.severity == Some(DiagnosticSeverity::ERROR)
        })
        .collect();

    assert!(
        unknown_param_errors.is_empty(),
        "Should have no unknown parameter errors, but got: {:?}",
        unknown_param_errors
    );
}

// ==================== Inherited Init Tests ====================

#[tokio::test]
async fn test_diagnostics_inherited_init_valid() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // Valid config using child class with inherited __init__ from parent
    let content = r#"# @hydra
test:
  _target_: my_module.ChildWithoutInit
  name: "test_name"
  value: 42
"#;
    ctx.open_document("inherited_valid.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Should have no errors - all params from inherited __init__ are valid
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect();

    assert!(
        errors.is_empty(),
        "Should have no errors for valid inherited __init__ usage. Found: {:?}",
        errors
    );
}

#[tokio::test]
async fn test_diagnostics_inherited_init_missing_required() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // Missing required 'name' parameter from inherited __init__
    let content = r#"# @hydra
test:
  _target_: my_module.ChildWithoutInit
  value: 42
"#;
    ctx.open_document("inherited_missing.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Should have an error for missing 'name' parameter
    let missing_param = diagnostics
        .iter()
        .find(|d| d.message.contains("name") && d.message.contains("Missing"));

    assert!(
        missing_param.is_some(),
        "Should have diagnostic for missing 'name' parameter from inherited __init__. Diagnostics: {:?}",
        diagnostics
    );

    if let Some(diag) = missing_param {
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        insta::assert_snapshot!(
            "diagnostic_inherited_init_missing_param",
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
async fn test_diagnostics_inherited_init_unknown_param() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // Unknown parameter for inherited __init__ (which doesn't have **kwargs)
    let content = r#"# @hydra
test:
  _target_: my_module.ChildWithoutInit
  name: "test"
  unknown_param: 123
"#;
    ctx.open_document("inherited_unknown.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Should have an error for unknown parameter
    let unknown_param = diagnostics
        .iter()
        .find(|d| d.message.contains("unknown_param") || d.message.contains("Unknown parameter"));

    assert!(
        unknown_param.is_some(),
        "Should have diagnostic for unknown parameter with inherited __init__. Diagnostics: {:?}",
        diagnostics
    );

    if let Some(diag) = unknown_param {
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        insta::assert_snapshot!(
            "diagnostic_inherited_init_unknown_param",
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
async fn test_diagnostics_grandchild_inherited_init() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // Test grandchild class that inherits __init__ from grandparent
    let content = r#"# @hydra
test:
  _target_: my_module.GrandchildWithoutInit
  name: "test_name"
  value: 42
"#;
    ctx.open_document("grandchild_valid.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Should have no errors - params from grandparent's __init__ should be valid
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect();

    assert!(
        errors.is_empty(),
        "Should have no errors for valid grandchild inherited __init__ usage. Found: {:?}",
        errors
    );
}

// ==================== Parameter Position Tests ====================

#[tokio::test]
async fn test_diagnostics_params_before_target() {
    let mut ctx = TestContext::new(TestWorkspace::Diagnostics);
    ctx.initialize().await;

    // Parameters defined before _target_ should get diagnostics on the correct lines
    let content = r#"# @hydra
my_module:
  bap: false
  # comment
  boop: true
  _target_: my_module.DataLoader
  beep: 42
  # second comment
  another: 123
"#;
    ctx.open_document("params_before.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    let unknown_params: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("Unknown parameter"))
        .collect();

    // bap, boop, beep, another are all unknown for DataLoader
    assert_eq!(
        unknown_params.len(),
        4,
        "Should have 4 unknown parameter errors, got: {:?}",
        unknown_params
    );

    // Verify each diagnostic appears on the correct line (0-indexed)
    let find_diag = |name: &str| {
        unknown_params
            .iter()
            .find(|d| d.message.contains(name))
            .unwrap_or_else(|| panic!("Missing diagnostic for '{}'", name))
    };
    assert_eq!(
        find_diag("bap").range.start.line,
        2,
        "bap should be on line 2"
    );
    assert_eq!(
        find_diag("boop").range.start.line,
        4,
        "boop should be on line 4"
    );
    assert_eq!(
        find_diag("beep").range.start.line,
        6,
        "beep should be on line 6"
    );
    assert_eq!(
        find_diag("another").range.start.line,
        8,
        "another should be on line 8"
    );
}

#[tokio::test]
async fn test_diagnostics_comment_between_target_and_params() {
    let mut ctx = TestContext::new(TestWorkspace::Diagnostics);
    ctx.initialize().await;

    // A comment between _target_ and params should not shift diagnostic positions
    let content = r#"# @hydra
my_module:
  _target_: my_module.DataLoader
  # this is a comment
  beep: 42
  shuffle: true
"#;
    ctx.open_document("comment_between.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    let unknown_params: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("Unknown parameter"))
        .collect();

    assert_eq!(
        unknown_params.len(),
        1,
        "Should have 1 unknown parameter error for 'beep', got: {:?}",
        unknown_params
    );

    // beep should be on line 4, not line 3 (where the comment is)
    assert_eq!(
        unknown_params[0].range.start.line, 4,
        "beep diagnostic should be on line 4, not on the comment line"
    );
    assert!(
        unknown_params[0].message.contains("beep"),
        "Diagnostic should be for 'beep'"
    );
}

#[tokio::test]
async fn test_diagnostics_classmethod_no_cls_error() {
    let mut ctx = TestContext::new(TestWorkspace::Diagnostics);
    ctx.initialize().await;

    // A classmethod target with all required params (except cls) should have no diagnostics
    let content = r#"# @hydra
loader:
  _target_: my_module.DataLoader.from_config
  config_path: "/path/to/config"
"#;
    ctx.open_document("classmethod.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // Should NOT have a diagnostic for missing 'cls'
    let cls_diagnostics: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("cls"))
        .collect();

    assert!(
        cls_diagnostics.is_empty(),
        "Should not report 'cls' as a missing parameter for classmethods, got: {:?}",
        cls_diagnostics
    );

    // Should have no diagnostics at all since config_path is provided
    assert!(
        diagnostics.is_empty(),
        "Should have no diagnostics for valid classmethod usage, got: {:?}",
        diagnostics
    );
}

// ==================== Hydra Keyword Integration Tests ====================

#[tokio::test]
async fn test_hydra_keywords_not_reported_as_unknown() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"
model:
  _target_: my_module.DataLoader
  _partial_: true
  _recursive_: false
  _convert_: all
  _args_: [1, 2]
  batch_size: 32
"#;
    ctx.open_document("keywords.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    // None of the hydra keywords should be reported as unknown parameters
    let unknown_keyword_diags: Vec<_> = diagnostics
        .iter()
        .filter(|d| {
            d.message.contains("Unknown parameter")
                && (d.message.contains("_partial_")
                    || d.message.contains("_recursive_")
                    || d.message.contains("_convert_")
                    || d.message.contains("_args_"))
        })
        .collect();

    assert!(
        unknown_keyword_diags.is_empty(),
        "Hydra keywords should not be reported as unknown parameters, got: {:?}",
        unknown_keyword_diags
    );
}

#[tokio::test]
async fn test_invalid_recursive_value_diagnostic() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"
model:
  _target_: my_module.DataLoader
  _recursive_: "yes"
  batch_size: 32
"#;
    ctx.open_document("invalid_recursive.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    let keyword_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("_recursive_") && d.message.contains("boolean"));

    assert!(
        keyword_diag.is_some(),
        "Should have diagnostic for invalid _recursive_ value, got: {:?}",
        diagnostics
    );

    if let Some(diag) = keyword_diag {
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diag.code,
            Some(NumberOrString::String(
                "invalid-hydra-parameter".to_string()
            ))
        );
    }
}

#[tokio::test]
async fn test_invalid_convert_value_diagnostic() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"
model:
  _target_: my_module.DataLoader
  _convert_: invalid_mode
  batch_size: 32
"#;
    ctx.open_document("invalid_convert.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    let keyword_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("_convert_") && d.message.contains("must be one of"));

    assert!(
        keyword_diag.is_some(),
        "Should have diagnostic for invalid _convert_ value, got: {:?}",
        diagnostics
    );

    if let Some(diag) = keyword_diag {
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diag.code,
            Some(NumberOrString::String(
                "invalid-hydra-parameter".to_string()
            ))
        );
    }
}

#[tokio::test]
async fn test_invalid_args_value_diagnostic() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"
model:
  _target_: my_module.DataLoader
  _args_: "not a list"
  batch_size: 32
"#;
    ctx.open_document("invalid_args.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    let keyword_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("_args_") && d.message.contains("list"));

    assert!(
        keyword_diag.is_some(),
        "Should have diagnostic for invalid _args_ value, got: {:?}",
        diagnostics
    );

    if let Some(diag) = keyword_diag {
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diag.code,
            Some(NumberOrString::String(
                "invalid-hydra-parameter".to_string()
            ))
        );
    }
}

#[tokio::test]
async fn test_valid_hydra_keywords_no_keyword_diagnostics() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"
model:
  _target_: my_module.DataLoader
  _recursive_: true
  _convert_: partial
  _args_: [1, 2, 3]
  batch_size: 32
"#;
    ctx.open_document("valid_keywords.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    let keyword_diags: Vec<_> = diagnostics
        .iter()
        .filter(|d| {
            d.code
                == Some(NumberOrString::String(
                    "invalid-hydra-parameter".to_string(),
                ))
        })
        .collect();

    assert!(
        keyword_diags.is_empty(),
        "Valid keywords should produce no keyword diagnostics, got: {:?}",
        keyword_diags
    );
}

#[tokio::test]
async fn test_args_satisfies_positional_params() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"
func:
  _target_: my_module.strict_func
  _args_: [10, 20]
"#;
    ctx.open_document("args_satisfy.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    let missing_diags: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("Missing required parameter"))
        .collect();

    assert!(
        missing_diags.is_empty(),
        "_args_ should satisfy positional parameters, got: {:?}",
        missing_diags
    );
}

#[tokio::test]
async fn test_args_partially_satisfies_positional_params() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"
func:
  _target_: my_module.strict_func
  _args_: [10]
"#;
    ctx.open_document("args_partial.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    let missing_diags: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("Missing required parameter"))
        .collect();

    assert_eq!(
        missing_diags.len(),
        1,
        "Should have exactly one missing parameter diagnostic, got: {:?}",
        missing_diags
    );
    assert!(
        missing_diags[0].message.contains("arg2"),
        "Should report arg2 as missing, got: {:?}",
        missing_diags[0].message
    );
}

#[tokio::test]
async fn test_args_multiple_values_error() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"
func:
  _target_: my_module.strict_func
  _args_: [10, 20]
  arg1: 10
"#;
    ctx.open_document("args_multiple_values.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    let already_assigned: Vec<_> = diagnostics
        .iter()
        .filter(|d| {
            d.code
                == Some(NumberOrString::String(
                    "parameter-already-assigned".to_string(),
                ))
        })
        .collect();

    assert_eq!(
        already_assigned.len(),
        1,
        "Should have one parameter-already-assigned diagnostic, got: {:?}",
        diagnostics
    );
    assert!(
        already_assigned[0].message.contains("arg1"),
        "Should mention arg1 in error, got: {:?}",
        already_assigned[0].message
    );
}

#[tokio::test]
async fn test_args_with_variadic_no_overflow_error() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"
func:
  _target_: my_module.my_func
  _args_: [10, 20, 30]
"#;
    ctx.open_document("args_variadic.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    assert!(
        diagnostics.is_empty(),
        "Should have no diagnostics when *args absorbs extra positional args, got: {:?}",
        diagnostics
    );
}

#[tokio::test]
async fn test_args_too_many_positional() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"
func:
  _target_: my_module.strict_func
  _args_: [10, 20, 30]
"#;
    ctx.open_document("args_too_many.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    let too_many: Vec<_> = diagnostics
        .iter()
        .filter(|d| {
            d.code
                == Some(NumberOrString::String(
                    "too-many-positional-arguments".to_string(),
                ))
        })
        .collect();

    assert_eq!(
        too_many.len(),
        1,
        "Should have one too-many-positional-arguments diagnostic, got: {:?}",
        diagnostics
    );
    assert!(
        too_many[0].message.contains("2 positional argument(s)"),
        "Should mention the expected count, got: {:?}",
        too_many[0].message
    );
}

#[tokio::test]
async fn test_args_with_keyword_only_not_satisfied() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"
func:
  _target_: my_module.mixed_func
  _args_: [10, 20]
"#;
    ctx.open_document("args_kw_only.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    let missing_diags: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("Missing required parameter"))
        .collect();

    assert_eq!(
        missing_diags.len(),
        1,
        "Should have one missing parameter diagnostic for keyword-only param, got: {:?}",
        diagnostics
    );
    assert!(
        missing_diags[0].message.contains("kw_only"),
        "Should report kw_only as missing, got: {:?}",
        missing_diags[0].message
    );
}

#[tokio::test]
async fn test_args_empty_list_does_not_satisfy_required() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    let content = r#"
func:
  _target_: my_module.strict_func
  _args_: []
"#;
    ctx.open_document("args_empty.yaml", content.to_string())
        .await;

    let dp = ctx.recv::<PublishDiagnosticsParams>().await;
    let diagnostics = dp.diagnostics;

    let missing_diags: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("Missing required parameter"))
        .collect();

    assert_eq!(
        missing_diags.len(),
        2,
        "Empty _args_ should not satisfy any positional parameters, got: {:?}",
        missing_diags
    );
}

// ==================== Watched-File Refresh Tests ====================

/// Editing a watched `.py` that an open Hydra config depends on must refresh
/// that config's diagnostics — even though the YAML buffer itself is never
/// touched.
///
/// This is the "stale after watched-file change" bug: `did_change_watched_files`
/// invalidates the Python-dependent salsa queries (via `File::sync_path`) but
/// never republishes, so the open Hydra doc keeps its now-wrong diagnostics
/// until the user next edits *that* file.
#[tokio::test]
async fn test_watched_py_edit_refreshes_open_hydra_doc() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize().await;

    // A config that resolves cleanly against the shipped `my_module.DataLoader`
    // (mirrors `test_no_diagnostics_valid_config`).
    let content =
        "# @hydra\nmodel:\n  _target_: my_module.DataLoader\n  batch_size: 32\n  shuffle: true\n";
    ctx.open_document("clean.yaml", content.to_string()).await;

    // Drain the `did_open` publish and confirm the starting point is error-free.
    // Consuming it first guarantees the next `recv` can only be a *new* publish.
    let opened = ctx.recv::<PublishDiagnosticsParams>().await;
    let open_errors: Vec<_> = opened
        .diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect();
    assert!(
        open_errors.is_empty(),
        "config should resolve cleanly on open, got: {open_errors:?}"
    );

    // Break the dependency on disk: overwrite `my_module.py` so the `DataLoader`
    // symbol disappears.
    fs::write(
        ctx.workspace.path().join("my_module.py"),
        "class NotDataLoader:\n    pass\n",
    )
    .unwrap();

    // Tell the server the watched Python file changed, exactly as the client's
    // file watcher would. (Mirrors `notify_change` in `tests/watched_files.rs`,
    // kept local since it is used only here.)
    async fn notify_watched_change(ctx: &mut TestContext, path: &str, typ: FileChangeType) {
        ctx.notify::<DidChangeWatchedFiles>(DidChangeWatchedFilesParams {
            changes: vec![FileEvent {
                uri: ctx.doc_uri(path),
                typ,
            }],
        })
        .await;
    }
    notify_watched_change(&mut ctx, "my_module.py", FileChangeType::CHANGED).await;

    // A fresh publish must arrive carrying the now-expected symbol error.
    // `common::recv` awaits forever, so the `timeout` is what turns the pre-fix
    // staleness into a clean failure instead of an indefinite hang.
    let refreshed = tokio::time::timeout(
        Duration::from_secs(2),
        ctx.recv::<PublishDiagnosticsParams>(),
    )
    .await
    .expect("watched .py edit must trigger a diagnostics refresh for the open Hydra doc");

    assert_eq!(refreshed.uri, ctx.doc_uri("clean.yaml"));
    let symbol_not_found = refreshed
        .diagnostics
        .iter()
        .find(|d| d.message.contains("not found in module"));
    assert!(
        symbol_not_found.is_some(),
        "refreshed diagnostics should report the missing DataLoader symbol, got: {:?}",
        refreshed.diagnostics
    );
    assert_eq!(
        symbol_not_found.unwrap().code,
        Some(NumberOrString::String("unresolved-reference".to_string())),
        "missing symbol should surface as an unresolved-reference"
    );
}

/// Pull path: a `textDocument/diagnostic` request returns a fresh report, and a
/// watched-file change on disk is reflected on the next pull.
///
/// With pull advertised the server does not push, so the report is fetched on
/// demand: clean config → empty `Full`; break the dependency + sync the watched
/// file → the next pull reports `unresolved-reference`; an identical follow-up
/// pull (echoing the `result_id`) returns `Unchanged`.
#[tokio::test]
async fn test_pull_diagnostic_reflects_watched_py_edit() {
    use tower_lsp::lsp_types::request::DocumentDiagnosticRequest;

    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize_with_pull_support().await;

    let content =
        "# @hydra\nmodel:\n  _target_: my_module.DataLoader\n  batch_size: 32\n  shuffle: true\n";
    ctx.open_document("clean.yaml", content.to_string()).await;

    // Pull the diagnostics rather than waiting for a push (there is none for a
    // pull client). A clean config yields an error-free report.
    async fn pull(
        ctx: &mut TestContext,
        uri: Url,
        previous_result_id: Option<String>,
    ) -> DocumentDiagnosticReportResult {
        ctx.request::<DocumentDiagnosticRequest>(DocumentDiagnosticParams {
            text_document: TextDocumentIdentifier { uri },
            identifier: None,
            previous_result_id,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
    }

    // Extract (items, result_id) from a `Full` report; panic on `Unchanged`.
    fn full(report: &DocumentDiagnosticReportResult) -> (&[Diagnostic], Option<String>) {
        match report {
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(full)) => (
                &full.full_document_diagnostic_report.items,
                full.full_document_diagnostic_report.result_id.clone(),
            ),
            other => panic!("expected a Full report, got: {other:?}"),
        }
    }

    let clean_uri = ctx.doc_uri("clean.yaml");
    let clean = pull(&mut ctx, clean_uri.clone(), None).await;
    let (clean_items, _) = full(&clean);
    let clean_errors: Vec<_> = clean_items
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect();
    assert!(
        clean_errors.is_empty(),
        "clean config should pull an error-free report, got: {clean_errors:?}"
    );

    // Break the dependency on disk, then tell the server the watched file changed
    // so the parse of `my_module.py` is invalidated.
    fs::write(
        ctx.workspace.path().join("my_module.py"),
        "class NotDataLoader:\n    pass\n",
    )
    .unwrap();
    ctx.notify::<DidChangeWatchedFiles>(DidChangeWatchedFilesParams {
        changes: vec![FileEvent {
            uri: ctx.doc_uri("my_module.py"),
            typ: FileChangeType::CHANGED,
        }],
    })
    .await;

    // The next pull recomputes at the newest revision and reports the missing
    // symbol.
    let broken = pull(&mut ctx, clean_uri.clone(), None).await;
    let (broken_items, broken_id) = full(&broken);
    let symbol_not_found = broken_items
        .iter()
        .find(|d| d.message.contains("not found in module"));
    assert!(
        symbol_not_found.is_some(),
        "pull after the watched edit should report the missing symbol, got: {broken_items:?}"
    );
    assert_eq!(
        symbol_not_found.unwrap().code,
        Some(NumberOrString::String("unresolved-reference".to_string())),
    );
    let broken_id = broken_id.expect("a non-empty report should carry a result_id");

    // An identical follow-up pull echoing the result_id short-circuits to Unchanged.
    let repeat = pull(&mut ctx, clean_uri.clone(), Some(broken_id.clone())).await;
    match repeat {
        DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(unchanged)) => {
            assert_eq!(
                unchanged.unchanged_document_diagnostic_report.result_id, broken_id,
                "Unchanged report should echo the same result_id"
            );
        }
        other => panic!("expected an Unchanged report on the repeat pull, got: {other:?}"),
    }
}

/// Pull path: a non-Hydra document reports nothing, even when its YAML is
/// unparseable.
///
/// The push path gates on `is_hydra_file` in `did_open` / `did_change` and
/// clears diagnostics for files it does not own. The pull handler has to make
/// the same call itself, or a broken plain YAML file would draw a spurious
/// "YAML syntax error" from hydra-lsp for pull clients only. The Hydra half of
/// this test pins the other side: the gate must not swallow syntax errors on
/// files we do own.
#[tokio::test]
async fn test_pull_diagnostic_skips_non_hydra_file() {
    use tower_lsp::lsp_types::request::DocumentDiagnosticRequest;

    let mut ctx = TestContext::new(TestWorkspace::Simple);
    ctx.initialize_with_pull_support().await;

    async fn pull(ctx: &mut TestContext, uri: Url) -> Vec<Diagnostic> {
        let report = ctx
            .request::<DocumentDiagnosticRequest>(DocumentDiagnosticParams {
                text_document: TextDocumentIdentifier { uri },
                identifier: None,
                previous_result_id: None,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await;
        match report {
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(full)) => {
                full.full_document_diagnostic_report.items
            }
            other => panic!("expected a Full report, got: {other:?}"),
        }
    }

    // Invalid YAML (the nested key is indented under a scalar value) with no
    // Hydra marker and no `_target_`, so `is_hydra_file` is false.
    let broken_yaml = "services:\n  web: image\n    ports: [8080\n";

    ctx.open_document("plain.yaml", broken_yaml.to_string())
        .await;
    let plain_uri = ctx.doc_uri("plain.yaml");
    let plain = pull(&mut ctx, plain_uri).await;
    assert!(
        plain.is_empty(),
        "a non-Hydra file should pull no diagnostics, got: {plain:?}"
    );

    // Same broken YAML, but marked as Hydra: the syntax error must still surface.
    ctx.open_document("broken.yaml", format!("# @hydra\n{broken_yaml}"))
        .await;
    let broken_uri = ctx.doc_uri("broken.yaml");
    let hydra = pull(&mut ctx, broken_uri).await;
    assert!(
        hydra.iter().any(|d| extract_code(d) == "yaml-syntax-error"),
        "a Hydra file with invalid YAML should still report the syntax error, got: {hydra:?}"
    );
}
