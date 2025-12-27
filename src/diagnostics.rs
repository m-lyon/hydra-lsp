use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::python_analyzer::{FunctionSignature, PythonAnalyzer};
use crate::yaml_parser::TargetInfo;

pub struct DiagnosticsEngine;

impl DiagnosticsEngine {
    /// Validate a Hydra configuration and generate diagnostics
    pub fn validate_target(target_info: &TargetInfo) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Split target to validate format
        let (module_path, symbol_name) = match PythonAnalyzer::split_target(&target_info.value) {
            Ok(parts) => parts,
            Err(_) => {
                // Invalid target format
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position {
                            line: target_info.line,
                            character: target_info.col,
                        },
                        end: Position {
                            line: target_info.line,
                            character: target_info.col + target_info.value.len() as u32,
                        },
                    },
                    severity: Some(DiagnosticSeverity::ERROR),
                    code: Some(tower_lsp::lsp_types::NumberOrString::String(
                        "invalid-target".to_string(),
                    )),
                    source: Some("hydra-lsp".to_string()),
                    message: format!("Invalid _target_ format: {}", target_info.value),
                    ..Default::default()
                });
                return diagnostics;
            }
        };

        // TODO: Try to resolve the module and get the actual definition
        // For now, we'll create a placeholder diagnostic
        let _module_diagnostic = Diagnostic {
            range: Range {
                start: Position {
                    line: target_info.line,
                    character: target_info.col,
                },
                end: Position {
                    line: target_info.line,
                    character: target_info.col + target_info.value.len() as u32,
                },
            },
            severity: Some(DiagnosticSeverity::INFORMATION),
            code: None,
            source: Some("hydra-lsp".to_string()),
            message: format!("Target: {}.{}", module_path, symbol_name),
            ..Default::default()
        };

        // Don't add the info diagnostic by default
        // diagnostics.push(module_diagnostic);

        diagnostics
    }

    /// Validate parameters against a function signature
    pub fn validate_parameters(
        target_info: &TargetInfo,
        signature: &FunctionSignature,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        // Get parameter names from YAML (excluding _target_)
        let yaml_params: std::collections::HashSet<String> =
            target_info.parameters.keys().cloned().collect();

        // Get expected parameter names from signature (excluding self)
        let expected_params: std::collections::HashSet<String> = signature
            .parameters
            .iter()
            .filter(|p| p.name != "self" && !p.is_variadic && !p.is_variadic_keyword)
            .map(|p| p.name.clone())
            .collect();

        // Check if function accepts **kwargs
        let has_kwargs = signature.parameters.iter().any(|p| p.is_variadic_keyword);

        // Check for unknown parameters
        for yaml_param in &yaml_params {
            if !expected_params.contains(yaml_param) && !has_kwargs {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position {
                            line: target_info.line + 1, // Approximate line
                            character: 0,
                        },
                        end: Position {
                            line: target_info.line + 1,
                            character: yaml_param.len() as u32,
                        },
                    },
                    severity: Some(DiagnosticSeverity::ERROR),
                    code: Some(tower_lsp::lsp_types::NumberOrString::String(
                        "unknown-parameter".to_string(),
                    )),
                    source: Some("hydra-lsp".to_string()),
                    message: format!("Unknown parameter '{}' for {}", yaml_param, signature.name),
                    ..Default::default()
                });
            }
        }

        // Check for missing required parameters
        for param in &signature.parameters {
            if !param.has_default
                && !param.is_variadic
                && !param.is_variadic_keyword
                && param.name != "self"
                && !yaml_params.contains(&param.name)
            {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position {
                            line: target_info.line,
                            character: 0,
                        },
                        end: Position {
                            line: target_info.line,
                            character: 10, // Length of "_target_:"
                        },
                    },
                    severity: Some(DiagnosticSeverity::ERROR),
                    code: Some(tower_lsp::lsp_types::NumberOrString::String(
                        "missing-parameter".to_string(),
                    )),
                    source: Some("hydra-lsp".to_string()),
                    message: format!(
                        "Missing required parameter '{}' for {}",
                        param.name, signature.name
                    ),
                    ..Default::default()
                });
            }
        }

        // If **kwargs present, give a warning instead of error for unknown params
        if has_kwargs && !yaml_params.is_subset(&expected_params) {
            let unknown: Vec<_> = yaml_params.difference(&expected_params).collect();
            if !unknown.is_empty() {
                diagnostics.retain(|d| {
                    !matches!(&d.code, Some(tower_lsp::lsp_types::NumberOrString::String(code)) if code == "unknown-parameter")
                });

                for param in unknown {
                    diagnostics.push(Diagnostic {
                        range: Range {
                            start: Position {
                                line: target_info.line + 1,
                                character: 0,
                            },
                            end: Position {
                                line: target_info.line + 1,
                                character: param.len() as u32,
                            },
                        },
                        severity: Some(DiagnosticSeverity::HINT),
                        code: None,
                        source: Some("hydra-lsp".to_string()),
                        message: format!("Parameter '{}' will be passed via **kwargs", param),
                        ..Default::default()
                    });
                }
            }
        }

        diagnostics
    }

    /// Validate all targets in a document
    pub fn validate_document(targets: Vec<TargetInfo>) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        for target in targets {
            let target_diagnostics = Self::validate_target(&target);
            diagnostics.extend(target_diagnostics);

            // TODO: If we successfully resolve the target, validate parameters
            // For now, this is a placeholder for when full Python analysis is implemented
        }

        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::python_analyzer::ParameterInfo;

    #[test]
    fn test_validate_missing_required_param() {
        let target_info = TargetInfo {
            value: "my.Class".to_string(),
            parameters: std::collections::HashMap::new(),
            line: 0,
            col: 0,
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

        let diagnostics = DiagnosticsEngine::validate_parameters(&target_info, &signature);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0]
            .message
            .contains("Missing required parameter"));
    }

    #[test]
    fn test_validate_unknown_param_without_kwargs() {
        let mut params = std::collections::HashMap::new();
        params.insert("unknown_param".to_string(), serde_yaml::Value::Null);

        let target_info = TargetInfo {
            value: "my.Class".to_string(),
            parameters: params,
            line: 0,
            col: 0,
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

        let diagnostics = DiagnosticsEngine::validate_parameters(&target_info, &signature);
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("Unknown parameter"));
    }

    #[test]
    fn test_validate_unknown_param_with_kwargs() {
        let mut params = std::collections::HashMap::new();
        params.insert("any_param".to_string(), serde_yaml::Value::Null);

        let target_info = TargetInfo {
            value: "my.Class".to_string(),
            parameters: params,
            line: 0,
            col: 0,
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

        let diagnostics = DiagnosticsEngine::validate_parameters(&target_info, &signature);
        // Should be a HINT, not ERROR
        assert!(diagnostics
            .iter()
            .any(|d| d.severity == Some(DiagnosticSeverity::HINT)));
    }
}
