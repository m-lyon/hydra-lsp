use crate::python_analyzer::{DefinitionInfo, FunctionSignature, PythonAnalyzer};
use crate::yaml_parser::{ParsedContent, TargetInfo};
use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

/// Diagnostic rule codes for Hydra LSP diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticRule {
    MissingArgument,
    UnknownArgument,
    UnresolvedReference,
    UnresolvedImport,
    InvalidTarget,
}

impl DiagnosticRule {
    /// Return the string code for this rule.
    pub fn as_code(&self) -> &'static str {
        match self {
            DiagnosticRule::MissingArgument => "missing-argument",
            DiagnosticRule::UnknownArgument => "unknown-argument",
            DiagnosticRule::UnresolvedReference => "unresolved-reference",
            DiagnosticRule::UnresolvedImport => "unresolved-import",
            DiagnosticRule::InvalidTarget => "invalid-target",
        }
    }

    /// Parse a rule from its string code.
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "missing-argument" => Some(DiagnosticRule::MissingArgument),
            "unknown-argument" => Some(DiagnosticRule::UnknownArgument),
            "unresolved-reference" => Some(DiagnosticRule::UnresolvedReference),
            "unresolved-import" => Some(DiagnosticRule::UnresolvedImport),
            "invalid-target" => Some(DiagnosticRule::InvalidTarget),
            _ => None,
        }
    }

    /// Return all diagnostic rules.
    pub fn all() -> &'static [DiagnosticRule] {
        &[
            DiagnosticRule::MissingArgument,
            DiagnosticRule::UnknownArgument,
            DiagnosticRule::UnresolvedReference,
            DiagnosticRule::UnresolvedImport,
            DiagnosticRule::InvalidTarget,
        ]
    }
}

impl fmt::Display for DiagnosticRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_code())
    }
}

fn create_diagnostic(
    line: u32,
    start_char: u32,
    end_char: u32,
    severity: DiagnosticSeverity,
    code: Option<DiagnosticRule>,
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

/// Validate a Hydra `_target_` entry. Returns diagnostics and optionally the resolved
/// DefinitionInfo.
fn validate_target(
    target_info: &TargetInfo,
    workspace_root: Option<&Path>,
    python_interpreter: Option<&str>,
    file_suppressions: &HashSet<DiagnosticRule>,
) -> (Vec<Diagnostic>, Option<DefinitionInfo>) {
    let mut diagnostics = Vec::new();

    match PythonAnalyzer::extract_definition_info(
        &target_info.value,
        workspace_root,
        python_interpreter,
    ) {
        Ok((definition_info, _file_path, _module_path, _symbol_name)) => {
            (diagnostics, Some(definition_info))
        }
        Err(err) => {
            let error_msg = err.to_string();
            let (rule, msg) = if error_msg.starts_with("Could not resolve module:") {
                (DiagnosticRule::UnresolvedImport, error_msg)
            } else if error_msg.starts_with("Invalid _target_ format:") {
                (
                    DiagnosticRule::InvalidTarget,
                    format!("{}. Expected format: 'module.path.SymbolName'", error_msg),
                )
            } else {
                (DiagnosticRule::UnresolvedReference, error_msg)
            };
            if !file_suppressions.contains(&rule) && !target_info.suppressed_rules.contains(&rule) {
                diagnostics.push(create_diagnostic(
                    target_info.line,
                    target_info.value_start,
                    target_info.value_end(),
                    DiagnosticSeverity::ERROR,
                    Some(rule),
                    msg,
                ));
            }
            (diagnostics, None)
        }
    }
}

/// Validate parameters against a function signature
fn validate_parameters(
    target_info: &TargetInfo,
    signature: &FunctionSignature,
    file_suppressions: &HashSet<DiagnosticRule>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Get parameter names from YAML
    let param_names: HashSet<String> = target_info
        .parameters
        .iter()
        .map(|param| param.key.clone())
        .collect();

    // Get expected parameter names from signature (excluding self)
    let expected_params: HashSet<String> = signature
        .parameters
        .iter()
        .filter(|p| p.name != "self" && p.name != "cls" && !p.is_variadic && !p.is_variadic_keyword)
        .map(|p| p.name.clone())
        .collect();

    // Check if function accepts **kwargs
    let has_kwargs = signature.parameters.iter().any(|p| p.is_variadic_keyword);

    // Check for unknown parameters
    for param in &target_info.parameters {
        if !expected_params.contains(&param.key)
            && !has_kwargs
            && !file_suppressions.contains(&DiagnosticRule::UnknownArgument)
            && !target_info
                .suppressed_rules
                .contains(&DiagnosticRule::UnknownArgument)
        {
            diagnostics.push(create_diagnostic(
                param.line,
                target_info.key_start,
                param.key.len() as u32 + target_info.key_start,
                DiagnosticSeverity::ERROR,
                Some(DiagnosticRule::UnknownArgument),
                format!("Unknown parameter '{}' for '{}'", param.key, signature.name),
            ));
        }
    }

    // Check for missing required parameters (skip if is _partial_)
    if !target_info.is_partial {
        for param in &signature.parameters {
            if param.is_required()
                && !param_names.contains(&param.name)
                && !file_suppressions.contains(&DiagnosticRule::MissingArgument)
                && !target_info
                    .suppressed_rules
                    .contains(&DiagnosticRule::MissingArgument)
            {
                diagnostics.push(create_diagnostic(
                    target_info.line,
                    target_info.value_start,
                    target_info.value_end(),
                    DiagnosticSeverity::ERROR,
                    Some(DiagnosticRule::MissingArgument),
                    format!(
                        "Missing required parameter '{}' for '{}'",
                        param.name, signature.name
                    ),
                ));
            }
        }
    }

    // If **kwargs present, give a warning instead of error for unknown params
    if has_kwargs && !param_names.is_subset(&expected_params) {
        let unknown: Vec<_> = param_names.difference(&expected_params).collect();
        if !unknown.is_empty() {
            diagnostics.retain(|d| {
                !matches!(&d.code, Some(tower_lsp::lsp_types::NumberOrString::String(code)) if code == DiagnosticRule::UnknownArgument.as_code())
            });

            for param_name in unknown {
                if let Some(param_value) =
                    target_info.parameters.iter().find(|p| p.key == *param_name)
                    && !file_suppressions.contains(&DiagnosticRule::UnknownArgument)
                    && !target_info
                        .suppressed_rules
                        .contains(&DiagnosticRule::UnknownArgument)
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
    parsed_content: ParsedContent,
    workspace_root: Option<&Path>,
    python_interpreter: Option<&str>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for target in &parsed_content.targets {
        let (target_diagnostics, definition_info) = validate_target(
            target,
            workspace_root,
            python_interpreter,
            &parsed_content.file_suppressions,
        );
        diagnostics.extend(target_diagnostics);

        // Try to resolve the target and validate parameters
        if let Some(definition_info) = definition_info {
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
                DefinitionInfo::Method(method_info) => method_info.signature,
            };

            let parameter_diagnostics =
                validate_parameters(target, &signature, &parsed_content.file_suppressions);
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
    use crate::yaml_parser::{Parameter, YamlValue};
    use hashlink::LinkedHashMap;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn get_simple_test_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("workspace")
            .join("simple")
    }

    /// Helper to create a TargetInfo with default suppression fields.
    fn make_target(
        value: &str,
        parameters: Vec<Parameter>,
        line: u32,
        key_start: u32,
        value_start: u32,
        is_partial: bool,
    ) -> TargetInfo {
        TargetInfo {
            value: value.to_string(),
            parameters,
            line,
            key_start,
            value_start,
            suppressed_rules: HashSet::new(),
            is_partial,
        }
    }

    /// Helper to create a Parameter with default suppression fields.
    fn make_param(key: &str, value: YamlValue, line: u32) -> Parameter {
        Parameter {
            key: key.to_string(),
            value,
            line,
            suppressed_rules: HashSet::new(),
        }
    }

    // ==================== validate_parameters tests ====================

    #[test]
    fn test_validate_missing_required_param() {
        let target_info = make_target("my.Class", Vec::new(), 0, 0, 0, false);

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
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 1,
        };

        let diagnostics = validate_parameters(&target_info, &signature, &HashSet::new());
        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0]
                .message
                .contains("Missing required parameter")
        );
        assert_eq!(
            diagnostics[0].code,
            Some(tower_lsp::lsp_types::NumberOrString::String(
                "missing-argument".to_string()
            ))
        );
    }

    #[test]
    fn test_validate_unknown_param_without_kwargs() {
        let params = vec![make_param("unknown_param", YamlValue::Null, 1)];
        let target_info = make_target("my.Class", params, 0, 0, 0, false);

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
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 1,
        };

        let diagnostics = validate_parameters(&target_info, &signature, &HashSet::new());
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("Unknown parameter"));
        assert_eq!(
            diagnostics[0].code,
            Some(tower_lsp::lsp_types::NumberOrString::String(
                "unknown-argument".to_string()
            ))
        );
    }

    #[test]
    fn test_validate_unknown_param_with_kwargs() {
        let params = vec![make_param("any_param", YamlValue::Null, 1)];
        let target_info = make_target("my.Class", params, 0, 0, 0, false);

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
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 1,
        };

        let diagnostics = validate_parameters(&target_info, &signature, &HashSet::new());
        // Should be a HINT, not ERROR
        assert!(
            diagnostics
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::HINT))
        );
        assert!(diagnostics.iter().any(|d| d.message.contains("**kwargs")));
    }

    #[test]
    fn test_classmethod_cls_not_required() {
        // cls should not be reported as a missing required parameter for classmethods
        let target_info = make_target("my.Class.from_config", Vec::new(), 0, 0, 0, false);

        let signature = FunctionSignature {
            name: "from_config".to_string(),
            parameters: vec![
                ParameterInfo {
                    name: "cls".to_string(),
                    type_annotation: None,
                    default_value: None,
                    has_default: false,
                    is_variadic: false,
                    is_variadic_keyword: false,
                    is_keyword_only: false,
                },
                ParameterInfo {
                    name: "config_path".to_string(),
                    type_annotation: Some("str".to_string()),
                    default_value: None,
                    has_default: false,
                    is_variadic: false,
                    is_variadic_keyword: false,
                    is_keyword_only: false,
                },
            ],
            return_type: None,
            docstring: None,
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 1,
        };

        let diagnostics = validate_parameters(&target_info, &signature, &HashSet::new());
        // Should only report missing 'config_path', not 'cls'
        assert_eq!(
            diagnostics.len(),
            1,
            "Expected 1 diagnostic, got: {:?}",
            diagnostics
        );
        assert!(
            diagnostics[0].message.contains("config_path"),
            "Should report missing 'config_path', not 'cls': {}",
            diagnostics[0].message
        );
        assert!(
            !diagnostics[0].message.contains("cls"),
            "Should not mention 'cls' as missing: {}",
            diagnostics[0].message
        );
    }

    #[test]
    fn test_classmethod_cls_not_unknown_param() {
        // cls should not appear in expected params, so providing it should be flagged as unknown
        // But more importantly, not providing it should not be an error
        let params = vec![make_param(
            "config_path",
            YamlValue::String("path/to/config".to_string()),
            2,
        )];
        let target_info = make_target("my.Class.from_config", params, 0, 0, 0, false);

        let signature = FunctionSignature {
            name: "from_config".to_string(),
            parameters: vec![
                ParameterInfo {
                    name: "cls".to_string(),
                    type_annotation: None,
                    default_value: None,
                    has_default: false,
                    is_variadic: false,
                    is_variadic_keyword: false,
                    is_keyword_only: false,
                },
                ParameterInfo {
                    name: "config_path".to_string(),
                    type_annotation: Some("str".to_string()),
                    default_value: None,
                    has_default: false,
                    is_variadic: false,
                    is_variadic_keyword: false,
                    is_keyword_only: false,
                },
            ],
            return_type: None,
            docstring: None,
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 1,
        };

        let diagnostics = validate_parameters(&target_info, &signature, &HashSet::new());
        assert!(
            diagnostics.is_empty(),
            "Should have no diagnostics when all required params are provided, but got: {:?}",
            diagnostics
        );
    }

    // ==================== validate_target tests ====================

    #[test]
    fn test_validate_target_invalid_format() {
        let target_info = make_target(
            "InvalidTarget",
            Vec::new(),
            0,
            10,
            10 + "_target_:".len() as u32 + 1,
            false,
        );

        let (diagnostics, _definition_info) =
            validate_target(&target_info, None, None, &HashSet::new());
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
        let target_info = make_target(
            "nonexistent.module.Class",
            Vec::new(),
            0,
            10,
            10 + "_target_:".len() as u32 + 1,
            false,
        );

        let (diagnostics, _definition_info) = validate_target(
            &target_info,
            Some(&get_simple_test_dir()),
            None,
            &HashSet::new(),
        );
        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0].message.contains("Could not resolve module"),
            "Got message: {}",
            diagnostics[0].message
        );
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostics[0].code,
            Some(tower_lsp::lsp_types::NumberOrString::String(
                "unresolved-import".to_string()
            ))
        );
    }

    #[test]
    fn test_validate_target_symbol_not_found() {
        let target_info = make_target(
            "my_module.NonExistentClass",
            Vec::new(),
            0,
            10,
            10 + "_target_:".len() as u32 + 1,
            false,
        );

        let resources_dir = get_simple_test_dir();
        let (diagnostics, _definition_info) =
            validate_target(&target_info, Some(&resources_dir), None, &HashSet::new());
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("Symbol"));
        assert!(diagnostics[0].message.contains("not found"));
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostics[0].code,
            Some(tower_lsp::lsp_types::NumberOrString::String(
                "unresolved-reference".to_string()
            ))
        );
    }

    #[test]
    fn test_validate_target_valid_class() {
        let target_info = make_target(
            "my_module.ClassWithInit",
            Vec::new(),
            0,
            10,
            10 + "_target_:".len() as u32 + 1,
            false,
        );

        let resources_dir = get_simple_test_dir();
        let (diagnostics, _definition_info) =
            validate_target(&target_info, Some(&resources_dir), None, &HashSet::new());

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
        let target_info = make_target(
            "my_module.simple_function",
            Vec::new(),
            0,
            10,
            10 + "_target_:".len() as u32 + 1,
            false,
        );

        let resources_dir = get_simple_test_dir();
        let (diagnostics, _definition_info) =
            validate_target(&target_info, Some(&resources_dir), None, &HashSet::new());

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
        let vs = 10 + "_target_:".len() as u32 + 1;
        let targets = vec![
            make_target("my_module.simple_function", Vec::new(), 0, 10, vs, false),
            make_target("InvalidTarget", Vec::new(), 2, 10, vs, false),
            make_target("nonexistent.Module", Vec::new(), 4, 10, vs, false),
        ];
        let parsed_content: ParsedContent = ParsedContent {
            targets,
            line_map: HashMap::new(),
            file_suppressions: HashSet::new(),
        };

        let resources_dir = get_simple_test_dir();
        let diagnostics = validate_document(parsed_content, Some(&resources_dir), None);

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
        let params = vec![make_param("value", YamlValue::Integer(42), 1)];
        let vs = 10 + "_target_:".len() as u32 + 1;
        let targets = vec![make_target(
            "my_module.ClassWithInit",
            params,
            0,
            10,
            vs,
            false,
        )];
        let parsed_content: ParsedContent = ParsedContent {
            targets,
            line_map: HashMap::new(),
            file_suppressions: HashSet::new(),
        };

        let resources_dir = get_simple_test_dir();
        let diagnostics = validate_document(parsed_content, Some(&resources_dir), None);

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
        let mut mapping = LinkedHashMap::new();
        mapping.insert(
            "_target_".to_string(),
            YamlValue::String("test_module.SimpleClass".to_string()),
        );
        let params = vec![make_param("value", YamlValue::Mapping(mapping), 1)];
        let vs = 10 + "_target_:".len() as u32 + 1;
        let targets = vec![make_target(
            "my_module.function_with_params",
            params,
            0,
            10,
            vs,
            false,
        )];

        let resources_dir = get_simple_test_dir();
        let parsed_content: ParsedContent = ParsedContent {
            targets,
            line_map: HashMap::new(),
            file_suppressions: HashSet::new(),
        };

        let diagnostics = validate_document(parsed_content, Some(&resources_dir), None);

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

    // ==================== _partial_ support tests ====================

    #[test]
    fn test_partial_true_skips_missing_required_params() {
        let target_info = make_target("my.Class", Vec::new(), 0, 0, 0, true);

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
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 1,
        };

        let diagnostics = validate_parameters(&target_info, &signature, &HashSet::new());
        assert!(
            diagnostics.is_empty(),
            "Should have no diagnostics when _partial_: true, but got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_partial_false_reports_missing_required_params() {
        let target_info = make_target("my.Class", Vec::new(), 0, 0, 0, false);

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
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 1,
        };

        let diagnostics = validate_parameters(&target_info, &signature, &HashSet::new());
        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0]
                .message
                .contains("Missing required parameter")
        );
    }

    #[test]
    fn test_partial_with_other_params() {
        let params = vec![
            make_param("valid_param", YamlValue::String("value".to_string()), 2),
            make_param("unknown_param", YamlValue::String("value".to_string()), 3),
        ];
        let target_info = make_target("my.Class", params, 0, 0, 0, true);

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
                ParameterInfo {
                    name: "valid_param".to_string(),
                    type_annotation: Some("str".to_string()),
                    default_value: None,
                    has_default: true,
                    is_variadic: false,
                    is_variadic_keyword: false,
                    is_keyword_only: false,
                },
            ],
            return_type: None,
            docstring: None,
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 1,
        };

        let diagnostics = validate_parameters(&target_info, &signature, &HashSet::new());
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("unknown_param"));
        assert!(diagnostics[0].message.contains("Unknown parameter"));
    }

    // ==================== DiagnosticRule tests ====================

    #[test]
    fn test_diagnostic_rule_round_trip() {
        for rule in DiagnosticRule::all() {
            let code = rule.as_code();
            let parsed = DiagnosticRule::from_code(code);
            assert_eq!(parsed, Some(*rule), "Round-trip failed for {:?}", rule);
        }
    }

    #[test]
    fn test_diagnostic_rule_from_unknown_code() {
        assert_eq!(DiagnosticRule::from_code("nonexistent-rule"), None);
    }

    // ==================== validate_document with disabled rules ====================

    #[test]
    fn test_validate_document_with_file_suppression() {
        let vs = 10 + "_target_:".len() as u32 + 1;
        let target = make_target("InvalidTarget", Vec::new(), 2, 10, vs, false);
        let mut file_suppressions = HashSet::new();
        file_suppressions.insert(DiagnosticRule::InvalidTarget);
        let targets = vec![target];
        let parsed_content: ParsedContent = ParsedContent {
            targets,
            line_map: HashMap::new(),
            file_suppressions,
        };

        let diagnostics = validate_document(parsed_content, None, None);

        assert!(
            diagnostics.is_empty(),
            "Should have no diagnostics when rule is file-suppressed, but got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_validate_document_with_inline_suppression_on_target() {
        let vs = 10 + "_target_:".len() as u32 + 1;
        let mut target = make_target("InvalidTarget", Vec::new(), 2, 10, vs, false);
        target
            .suppressed_rules
            .insert(DiagnosticRule::InvalidTarget);
        let targets = vec![target];
        let parsed_content: ParsedContent = ParsedContent {
            targets,
            line_map: HashMap::new(),
            file_suppressions: HashSet::new(),
        };

        let diagnostics = validate_document(parsed_content, None, None);

        assert!(
            diagnostics.is_empty(),
            "Should have no diagnostics when rule is inline-suppressed on target, but got: {:?}",
            diagnostics
        );
    }
}
