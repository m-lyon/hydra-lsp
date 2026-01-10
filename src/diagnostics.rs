use crate::python_analyzer::{DefinitionInfo, FunctionSignature, PythonAnalyzer};
use crate::yaml_parser::TargetInfo;
use std::collections::HashSet;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

fn create_diagnostic(
    line: u32,
    start_char: u32,
    end_char: u32,
    severity: DiagnosticSeverity,
    code: Option<&str>,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line,
                character: start_char,
            },
            end: Position {
                line,
                character: end_char,
            },
        },
        severity: Some(severity),
        code: code.map(|c| tower_lsp::lsp_types::NumberOrString::String(c.to_string())),
        source: Some("hydra-lsp".to_string()),
        message,
        ..Default::default()
    }
}

/// Validate a Hydra configuration and generate diagnostics
fn validate_target(
    target_info: &TargetInfo,
    workspace_root: Option<&std::path::Path>,
    python_interpreter: Option<&str>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Split target to validate format
    let (module_path, symbol_name) = match PythonAnalyzer::split_target(&target_info.value) {
        Ok(parts) => parts,
        Err(_) => {
            // Invalid target format
            diagnostics.push(create_diagnostic(
                target_info.line,
                target_info.value_start,
                target_info.value_end(),
                DiagnosticSeverity::ERROR,
                Some("invalid-target"),
                format!(
                    "Invalid _target_ format: '{}'. Expected format: 'module.path.SymbolName'",
                    target_info.value
                ),
            ));
            return diagnostics;
        }
    };

    // Try to resolve the module to check if it exists
    match PythonAnalyzer::resolve_module(&module_path, workspace_root, python_interpreter) {
        Ok(file_path) => {
            // Module resolved successfully, now try to find the symbol
            let symbol_found =
                match PythonAnalyzer::extract_function_signature(&file_path, &symbol_name) {
                    Ok(_) => true,
                    Err(_) => {
                        // Not a function, try as a class
                        PythonAnalyzer::extract_class_info(&file_path, &symbol_name).is_ok()
                    }
                };

            if !symbol_found {
                // Module exists but symbol not found
                diagnostics.push(create_diagnostic(
                    target_info.line,
                    target_info.value_start,
                    target_info.value_end(),
                    DiagnosticSeverity::ERROR,
                    Some("symbol-not-found"),
                    format!(
                        "Symbol '{}' not found in module '{}'",
                        symbol_name, module_path
                    ),
                ));
            }
        }
        Err(err) => {
            // Module could not be resolved
            diagnostics.push(create_diagnostic(
                target_info.line,
                target_info.value_start,
                target_info.value_end(),
                DiagnosticSeverity::ERROR,
                Some("module-not-found"),
                format!("Cannot resolve module '{}': {}", module_path, err),
            ));
        }
    }

    diagnostics
}

/// Validate parameters against a function signature
fn validate_parameters(target_info: &TargetInfo, signature: &FunctionSignature) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Get parameter names from YAML (excluding _target_)
    let param_names: HashSet<String> = target_info
        .parameters
        .iter()
        .map(|param| param.key.clone())
        .collect();

    // Get expected parameter names from signature (excluding self)
    let expected_params: HashSet<String> = signature
        .parameters
        .iter()
        .filter(|p| p.name != "self" && !p.is_variadic && !p.is_variadic_keyword)
        .map(|p| p.name.clone())
        .collect();

    // Check if function accepts **kwargs
    let has_kwargs = signature.parameters.iter().any(|p| p.is_variadic_keyword);

    // Check for unknown parameters
    for param in &target_info.parameters {
        if !expected_params.contains(&param.key) && !has_kwargs {
            diagnostics.push(create_diagnostic(
                param.line,
                target_info.key_start,
                param.key.len() as u32 + target_info.key_start,
                DiagnosticSeverity::ERROR,
                Some("unknown-parameter"),
                format!("Unknown parameter '{}' for '{}'", param.key, signature.name),
            ));
        }
    }

    // Check for missing required parameters
    for param in &signature.parameters {
        if param.is_required() && !param_names.contains(&param.name) {
            diagnostics.push(create_diagnostic(
                target_info.line,
                target_info.value_start,
                target_info.value_end(),
                DiagnosticSeverity::ERROR,
                Some("missing-parameter"),
                format!(
                    "Missing required parameter '{}' for '{}'",
                    param.name, signature.name
                ),
            ));
        }
    }

    // If **kwargs present, give a warning instead of error for unknown params
    if has_kwargs && !param_names.is_subset(&expected_params) {
        let unknown: Vec<_> = param_names.difference(&expected_params).collect();
        if !unknown.is_empty() {
            diagnostics.retain(|d| {
                !matches!(&d.code, Some(tower_lsp::lsp_types::NumberOrString::String(code)) if code == "unknown-parameter")
            });

            for param_name in unknown {
                if let Some(param_value) =
                    target_info.parameters.iter().find(|p| p.key == *param_name)
                {
                    diagnostics.push(create_diagnostic(
                        param_value.line,
                        target_info.key_start,
                        target_info.key_start + param_value.key.len() as u32,
                        DiagnosticSeverity::HINT,
                        None,
                        format!("Parameter '{}' will be passed via **kwargs", param_name),
                    ));
                }
            }
        }
    }

    diagnostics
}

/// Validate all targets in a document
pub fn validate_document(
    targets: Vec<TargetInfo>,
    workspace_root: Option<&std::path::Path>,
    python_interpreter: Option<&str>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for target in &targets {
        let target_diagnostics = validate_target(target, workspace_root, python_interpreter);
        diagnostics.extend(target_diagnostics);

        // Try to resolve the target and validate parameters
        if let Ok(definition_info) = PythonAnalyzer::extract_definition_info(
            &target.value,
            workspace_root,
            python_interpreter,
        ) {
            let signature = match definition_info {
                DefinitionInfo::Function(sig) => sig,
                DefinitionInfo::Class(class_info) => {
                    // For classes, use the __init__ signature if available
                    if let Some(init_sig) = class_info.init_signature {
                        init_sig
                    } else {
                        // Class with no __init__, no parameters to validate
                        continue;
                    }
                }
            };

            let parameter_diagnostics = validate_parameters(target, &signature);
            diagnostics.extend(parameter_diagnostics);
        }
        // If Python analysis fails, we've already added a basic validation diagnostic above
    }

    // Sort all diagnostics by position for consistent ordering
    diagnostics.sort_by(|a, b| {
        a.range
            .start
            .line
            .cmp(&b.range.start.line)
            .then_with(|| a.range.start.character.cmp(&b.range.start.character))
    });

    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::python_analyzer::ParameterInfo;
    use crate::yaml_parser::{ParameterKind, ParameterValue};
    use std::path::PathBuf;

    fn get_test_resources_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources")
    }

    // ==================== validate_parameters tests ====================

    #[test]
    fn test_validate_missing_required_param() {
        let target_info = TargetInfo {
            value: "my.Class".to_string(),
            parameters: Vec::new(),
            line: 0,
            key_start: 0,
            value_start: 0,
        };

        let signature = FunctionSignature {
            name: "Class".to_string(),
            parameters: vec![
                ParameterInfo {
                    name: "self".to_string(),
                    type_annotation: None,
                    default_value: None,
                    has_default: false,
                    is_variadic: false,
                    is_variadic_keyword: false,
                    is_keyword_only: false,
                },
                ParameterInfo {
                    name: "required_param".to_string(),
                    type_annotation: Some("int".to_string()),
                    default_value: None,
                    has_default: false,
                    is_variadic: false,
                    is_variadic_keyword: false,
                    is_keyword_only: false,
                },
            ],
            return_type: None,
            docstring: None,
        };

        let diagnostics = validate_parameters(&target_info, &signature);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0]
            .message
            .contains("Missing required parameter"));
        assert_eq!(
            diagnostics[0].code,
            Some(tower_lsp::lsp_types::NumberOrString::String(
                "missing-parameter".to_string()
            ))
        );
    }

    #[test]
    fn test_validate_unknown_param_without_kwargs() {
        let params = vec![ParameterValue {
            kind: ParameterKind::Value(serde_yaml::Value::Null),
            line: 1,
            key: "unknown_param".to_string(),
        }];

        let target_info = TargetInfo {
            value: "my.Class".to_string(),
            parameters: params,
            line: 0,
            key_start: 0,
            value_start: 0,
        };

        let signature = FunctionSignature {
            name: "Class".to_string(),
            parameters: vec![ParameterInfo {
                name: "self".to_string(),
                type_annotation: None,
                default_value: None,
                has_default: false,
                is_variadic: false,
                is_variadic_keyword: false,
                is_keyword_only: false,
            }],
            return_type: None,
            docstring: None,
        };

        let diagnostics = validate_parameters(&target_info, &signature);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("Unknown parameter"));
        assert_eq!(
            diagnostics[0].code,
            Some(tower_lsp::lsp_types::NumberOrString::String(
                "unknown-parameter".to_string()
            ))
        );
    }

    #[test]
    fn test_validate_unknown_param_with_kwargs() {
        let params = vec![ParameterValue {
            kind: ParameterKind::Value(serde_yaml::Value::Null),
            line: 1,
            key: "any_param".to_string(),
        }];

        let target_info = TargetInfo {
            value: "my.Class".to_string(),
            parameters: params,
            line: 0,
            key_start: 0,
            value_start: 0,
        };

        let signature = FunctionSignature {
            name: "Class".to_string(),
            parameters: vec![
                ParameterInfo {
                    name: "self".to_string(),
                    type_annotation: None,
                    default_value: None,
                    has_default: false,
                    is_variadic: false,
                    is_variadic_keyword: false,
                    is_keyword_only: false,
                },
                ParameterInfo {
                    name: "**kwargs".to_string(),
                    type_annotation: None,
                    default_value: None,
                    has_default: false,
                    is_variadic: false,
                    is_variadic_keyword: true,
                    is_keyword_only: false,
                },
            ],
            return_type: None,
            docstring: None,
        };

        let diagnostics = validate_parameters(&target_info, &signature);
        // Should be a HINT, not ERROR
        assert!(diagnostics
            .iter()
            .any(|d| d.severity == Some(DiagnosticSeverity::HINT)));
        assert!(diagnostics.iter().any(|d| d.message.contains("**kwargs")));
    }

    // ==================== validate_target tests ====================

    #[test]
    fn test_validate_target_invalid_format() {
        let target_info = TargetInfo {
            value: "InvalidTarget".to_string(), // No module path
            parameters: Vec::new(),
            line: 0,
            key_start: 10,
            value_start: 10 + "_target_:".len() as u32 + 1,
        };

        let diagnostics = validate_target(&target_info, None, None);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("Invalid _target_ format"));
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostics[0].code,
            Some(tower_lsp::lsp_types::NumberOrString::String(
                "invalid-target".to_string()
            ))
        );
    }

    #[test]
    fn test_validate_target_module_not_found() {
        let target_info = TargetInfo {
            value: "nonexistent.module.Class".to_string(),
            parameters: Vec::new(),
            line: 0,
            key_start: 10,
            value_start: 10 + "_target_:".len() as u32 + 1,
        };

        let diagnostics = validate_target(&target_info, Some(&get_test_resources_dir()), None);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("Cannot resolve module"));
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostics[0].code,
            Some(tower_lsp::lsp_types::NumberOrString::String(
                "module-not-found".to_string()
            ))
        );
    }

    #[test]
    fn test_validate_target_symbol_not_found() {
        let target_info = TargetInfo {
            value: "test_module.NonExistentClass".to_string(),
            parameters: Vec::new(),
            line: 0,
            key_start: 10,
            value_start: 10 + "_target_:".len() as u32 + 1,
        };

        let resources_dir = get_test_resources_dir();
        let diagnostics = validate_target(&target_info, Some(&resources_dir), None);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("Symbol"));
        assert!(diagnostics[0].message.contains("not found"));
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostics[0].code,
            Some(tower_lsp::lsp_types::NumberOrString::String(
                "symbol-not-found".to_string()
            ))
        );
    }

    #[test]
    fn test_validate_target_valid_class() {
        let target_info = TargetInfo {
            value: "test_module.ClassWithInit".to_string(),
            parameters: Vec::new(),
            line: 0,
            key_start: 10,
            value_start: 10 + "_target_:".len() as u32 + 1,
        };

        let resources_dir = get_test_resources_dir();
        let diagnostics = validate_target(&target_info, Some(&resources_dir), None);

        // Should not have module/symbol not found errors
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("Cannot resolve module")),
            "Should not have module not found error"
        );
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("Symbol") && d.message.contains("not found")),
            "Should not have symbol not found error"
        );
    }

    #[test]
    fn test_validate_target_valid_function() {
        let target_info = TargetInfo {
            value: "test_module.simple_function".to_string(),
            parameters: Vec::new(),
            line: 0,
            key_start: 10,
            value_start: 10 + "_target_:".len() as u32 + 1,
        };

        let resources_dir = get_test_resources_dir();
        let diagnostics = validate_target(&target_info, Some(&resources_dir), None);

        // Should not have module/symbol not found errors
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("Cannot resolve module")),
            "Should not have module not found error"
        );
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("Symbol") && d.message.contains("not found")),
            "Should not have symbol not found error"
        );
    }

    // ==================== validate_document tests ====================

    #[test]
    fn test_validate_document_multiple_targets() {
        let targets = vec![
            TargetInfo {
                value: "test_module.simple_function".to_string(),
                parameters: Vec::new(),
                line: 0,
                key_start: 10,
                value_start: 10 + "_target_:".len() as u32 + 1,
            },
            TargetInfo {
                value: "InvalidTarget".to_string(),
                parameters: Vec::new(),
                line: 2,
                key_start: 10,
                value_start: 10 + "_target_:".len() as u32 + 1,
            },
            TargetInfo {
                value: "nonexistent.Module".to_string(),
                parameters: Vec::new(),
                line: 4,
                key_start: 10,
                value_start: 10 + "_target_:".len() as u32 + 1,
            },
        ];

        let resources_dir = get_test_resources_dir();
        let diagnostics = validate_document(targets, Some(&resources_dir), None);

        // Should have at least 2 errors (invalid format and module not found)
        let errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
            .collect();
        assert!(errors.len() >= 2, "Should have at least 2 errors");

        // Diagnostics should be sorted by line number
        for i in 1..diagnostics.len() {
            assert!(
                diagnostics[i - 1].range.start.line <= diagnostics[i].range.start.line,
                "Diagnostics should be sorted by line"
            );
        }
    }

    #[test]
    fn test_validate_document_with_parameter_validation() {
        let params = vec![ParameterValue {
            kind: ParameterKind::Value(serde_yaml::Value::Number(serde_yaml::Number::from(42))),
            line: 1,
            key: "value".to_string(),
        }];
        // Missing required 'name' parameter (it has no default)
        let targets = vec![TargetInfo {
            value: "test_module.ClassWithInit".to_string(),
            parameters: params,
            line: 0,
            key_start: 10,
            value_start: 10 + "_target_:".len() as u32 + 1,
        }];

        let resources_dir = get_test_resources_dir();
        let diagnostics = validate_document(targets, Some(&resources_dir), None);

        // Should have diagnostic for missing required parameter 'name'
        let missing_param_diag = diagnostics.iter().find(|d| {
            d.message.contains("Missing required parameter") && d.message.contains("name")
        });
        assert!(
            missing_param_diag.is_some(),
            "Should have missing parameter diagnostic for 'name'. Got diagnostics: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_validate_nested_target_valid() {
        // Create a nested target parameter
        let mut nested_map = serde_yaml::Mapping::new();
        nested_map.insert(
            serde_yaml::Value::String("_target_".to_string()),
            serde_yaml::Value::String("test_module.SimpleClass".to_string()),
        );

        let params = vec![ParameterValue {
            kind: ParameterKind::Value(serde_yaml::Value::Mapping(nested_map)),
            line: 1,
            key: "value".to_string(),
        }];

        let targets = vec![TargetInfo {
            value: "test_module.function_with_params".to_string(),
            parameters: params,
            line: 0,
            key_start: 10,
            value_start: 10 + "_target_:".len() as u32 + 1,
        }];

        let resources_dir = get_test_resources_dir();
        let diagnostics = validate_document(targets, Some(&resources_dir), None);

        // Should not have errors for the nested target (it's a valid SimpleClass)
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("Cannot resolve module")),
            "Should not have module not found error"
        );
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("Symbol") && d.message.contains("not found")),
            "Should not have symbol not found error"
        );
    }
}
