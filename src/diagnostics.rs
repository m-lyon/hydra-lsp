use crate::python_analyzer::{DefinitionInfo, FunctionSignature, ParameterInfo};
use crate::python_cache::{PythonConfig, TargetString, cached_definition_info};
use crate::yaml_parser::{
    ARGS_KEY, CONVERT_KEY, ConvertMode, HydraObject, PARTIAL_KEY, Parameter, ParsedContent,
    RECURSIVE_KEY,
};
use std::collections::HashSet;
use std::fmt;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

/// Build the `DiagnosticRule` enum and everything that maps a variant to or
/// from its string code from one list of pairs, so the variants, the codes,
/// and the "all rules" list can never fall out of step with each other.
macro_rules! diagnostic_rules {
    ($($variant:ident => $code:literal),+ $(,)?) => {
        /// Diagnostic rule codes for Hydrust diagnostics.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum DiagnosticRule {
            $($variant,)+
        }

        impl DiagnosticRule {
            /// Return the string code for this rule.
            pub fn as_code(&self) -> &'static str {
                match self {
                    $(DiagnosticRule::$variant => $code,)+
                }
            }

            /// Parse a rule from its string code.
            pub fn from_code(code: &str) -> Option<Self> {
                match code {
                    $($code => Some(DiagnosticRule::$variant),)+
                    _ => None,
                }
            }

            /// Return all diagnostic rules.
            pub fn all() -> &'static [DiagnosticRule] {
                &[$(DiagnosticRule::$variant,)+]
            }

            /// Return the string code of every rule, in the same order as
            /// [`DiagnosticRule::all`].
            pub fn all_codes() -> &'static [&'static str] {
                &[$($code,)+]
            }
        }
    };
}

diagnostic_rules! {
    MissingArgument => "missing-argument",
    UnknownArgument => "unknown-argument",
    UnresolvedReference => "unresolved-reference",
    UnresolvedImport => "unresolved-import",
    InvalidHydraParameter => "invalid-hydra-parameter",
    ParameterAlreadyAssigned => "parameter-already-assigned",
    TooManyPositionalArguments => "too-many-positional-arguments",
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
///
/// Routes through `cached_definition_info`.
fn validate_target(
    hydra_obj: &HydraObject,
    db: &dyn ruff_db::Db,
    python_config: PythonConfig,
    file_suppressions: &HashSet<DiagnosticRule>,
) -> (Vec<Diagnostic>, Option<DefinitionInfo>) {
    let mut diagnostics = Vec::new();

    let target = TargetString::new(db, hydra_obj.target.value.clone());
    let cached = cached_definition_info(db, python_config, target);
    match cached.get() {
        Ok(def) => (diagnostics, Some(def.definition_info.clone())),
        Err(error_msg) => {
            let error_msg = error_msg.to_string();
            let (rule, msg) = if error_msg.starts_with("Could not resolve module:") {
                (DiagnosticRule::UnresolvedImport, error_msg)
            } else if error_msg.starts_with("Invalid _target_ format:") {
                (
                    DiagnosticRule::InvalidHydraParameter,
                    format!("{}. Expected format: 'module.path.SymbolName'", error_msg),
                )
            } else {
                (DiagnosticRule::UnresolvedReference, error_msg)
            };
            if !file_suppressions.contains(&rule) && !hydra_obj.suppressed_rules.contains(&rule) {
                diagnostics.push(create_diagnostic(
                    hydra_obj.target.line,
                    hydra_obj.target.value_start,
                    hydra_obj.target_value_end(),
                    DiagnosticSeverity::ERROR,
                    Some(rule),
                    msg,
                ));
            }
            (diagnostics, None)
        }
    }
}

/// Validate parameters against a function signature.
///
/// `implicit_param` is the name of the implicit first parameter (e.g. `self` / `cls`)
/// that should be excluded from validation. Pass `None` for plain functions and
/// static methods.
fn validate_parameters(
    hydra_obj: &HydraObject,
    signature: &FunctionSignature,
    display_name: &str,
    implicit_param: Option<&str>,
    file_suppressions: &HashSet<DiagnosticRule>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    let key_start = hydra_obj.target.key_start;

    // Get parameter names from YAML (only keyword params, not positional)
    let param_names: HashSet<String> = hydra_obj
        .parameters
        .iter()
        .filter_map(|p| match p {
            Parameter::Keyword { key, .. } => Some(key.clone()),
            Parameter::Positional { .. } => None,
        })
        .collect();

    // Get expected parameter names from signature (excluding the implicit parameter)
    let expected_params: HashSet<String> = signature
        .parameters
        .iter()
        .filter(|p| {
            Some(p.name.as_str()) != implicit_param && !p.is_variadic && !p.is_variadic_keyword
        })
        .map(|p| p.name.clone())
        .collect();

    // Determine which signature params are satisfied by positional args from _args_
    let num_positional_args = hydra_obj
        .parameters
        .iter()
        .filter(|p| matches!(p, Parameter::Positional { .. }))
        .count();

    let positional_eligible: Vec<&ParameterInfo> = signature
        .parameters
        .iter()
        .filter(|p| {
            Some(p.name.as_str()) != implicit_param
                && !p.is_variadic
                && !p.is_variadic_keyword
                && !p.is_keyword_only
        })
        .collect();

    let positionally_covered: HashSet<String> = positional_eligible
        .iter()
        .take(num_positional_args)
        .map(|p| p.name.clone())
        .collect();

    // Check if function accepts *args or **kwargs
    let has_variadic = signature.parameters.iter().any(|p| p.is_variadic);
    let has_kwargs = signature.parameters.iter().any(|p| p.is_variadic_keyword);

    // Check for unknown parameters (only keyword params)
    for param in &hydra_obj.parameters {
        if let Parameter::Keyword { key, line, .. } = param
            && !expected_params.contains(key)
            && !has_kwargs
            && !file_suppressions.contains(&DiagnosticRule::UnknownArgument)
            && !hydra_obj
                .suppressed_rules
                .contains(&DiagnosticRule::UnknownArgument)
        {
            diagnostics.push(create_diagnostic(
                *line,
                key_start,
                key.len() as u32 + key_start,
                DiagnosticSeverity::ERROR,
                Some(DiagnosticRule::UnknownArgument),
                format!("Unknown parameter '{}' for '{}'", key, display_name),
            ));
        }
    }

    if !hydra_obj.is_partial() {
        for param in &signature.parameters {
            if param.is_required()
                && Some(param.name.as_str()) != implicit_param
                && !param_names.contains(&param.name)
                && !positionally_covered.contains(&param.name)
                && !file_suppressions.contains(&DiagnosticRule::MissingArgument)
                && !hydra_obj
                    .suppressed_rules
                    .contains(&DiagnosticRule::MissingArgument)
            {
                diagnostics.push(create_diagnostic(
                    hydra_obj.target.line,
                    hydra_obj.target.value_start,
                    hydra_obj.target_value_end(),
                    DiagnosticSeverity::ERROR,
                    Some(DiagnosticRule::MissingArgument),
                    format!(
                        "Missing required parameter '{}' for '{}'",
                        param.name, display_name
                    ),
                ));
            }
        }
    }

    // Check for parameters provided both positionally via _args_ and as keyword args
    for param_name in &positionally_covered {
        if param_names.contains(param_name)
            && let Some(Parameter::Keyword { key, line, .. }) = hydra_obj
                .parameters
                .iter()
                .find(|p| matches!(p, Parameter::Keyword { key, .. } if key == param_name))
            && !file_suppressions.contains(&DiagnosticRule::ParameterAlreadyAssigned)
            && !hydra_obj
                .suppressed_rules
                .contains(&DiagnosticRule::ParameterAlreadyAssigned)
        {
            diagnostics.push(create_diagnostic(
                    *line,
                    key_start,
                    key.len() as u32 + key_start,
                    DiagnosticSeverity::ERROR,
                    Some(DiagnosticRule::ParameterAlreadyAssigned),
                    format!(
                        "'{}' got multiple values for argument '{}' (already provided positionally via _args_)",
                        display_name, param_name
                    ),
                ));
        }
    }

    // Check for too many positional arguments
    if num_positional_args > positional_eligible.len()
        && !has_variadic
        && !file_suppressions.contains(&DiagnosticRule::TooManyPositionalArguments)
        && !hydra_obj
            .suppressed_rules
            .contains(&DiagnosticRule::TooManyPositionalArguments)
        && let Some(ref hp) = hydra_obj.args
    {
        diagnostics.push(create_diagnostic(
            hp.line,
            hp.key_start,
            hp.key_start + ARGS_KEY.len() as u32,
            DiagnosticSeverity::ERROR,
            Some(DiagnosticRule::TooManyPositionalArguments),
            format!(
                "'{}' takes {} positional argument(s) but {} were given via _args_",
                display_name,
                positional_eligible.len(),
                num_positional_args
            ),
        ));
    }

    // If **kwargs present, give a warning instead of error for unknown params
    if has_kwargs && !param_names.is_subset(&expected_params) {
        let unknown: Vec<_> = param_names.difference(&expected_params).collect();
        if !unknown.is_empty() {
            diagnostics.retain(|d| {
                !matches!(&d.code, Some(tower_lsp::lsp_types::NumberOrString::String(code)) if code == DiagnosticRule::UnknownArgument.as_code())
            });

            for param_name in unknown {
                if let Some(Parameter::Keyword { key, line, .. }) = hydra_obj
                    .parameters
                    .iter()
                    .find(|p| matches!(p, Parameter::Keyword { key, .. } if key == param_name))
                    && !file_suppressions.contains(&DiagnosticRule::UnknownArgument)
                    && !hydra_obj
                        .suppressed_rules
                        .contains(&DiagnosticRule::UnknownArgument)
                {
                    diagnostics.push(create_diagnostic(
                        *line,
                        key_start,
                        key_start + key.len() as u32,
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

/// Validate Hydra keyword values (_partial_, _recursive_, _convert_, _args_).
fn validate_hydra_keywords(
    hydra_obj: &HydraObject,
    file_suppressions: &HashSet<DiagnosticRule>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    if file_suppressions.contains(&DiagnosticRule::InvalidHydraParameter)
        || hydra_obj
            .suppressed_rules
            .contains(&DiagnosticRule::InvalidHydraParameter)
    {
        return diagnostics;
    }

    let mut check_keyword =
        |keyword: &str, hp_invalid: bool, hp_line: u32, hp_key_start: u32, msg: String| {
            if hp_invalid {
                diagnostics.push(create_diagnostic(
                    hp_line,
                    hp_key_start,
                    hp_key_start + keyword.len() as u32,
                    DiagnosticSeverity::ERROR,
                    Some(DiagnosticRule::InvalidHydraParameter),
                    msg,
                ));
            }
        };

    if let Some(ref hp) = hydra_obj.partial {
        check_keyword(
            PARTIAL_KEY,
            hp.invalid,
            hp.line,
            hp.key_start,
            format!(
                "Invalid value for '{}': must be a boolean (true or false)",
                PARTIAL_KEY
            ),
        );
    }
    if let Some(ref hp) = hydra_obj.recursive {
        check_keyword(
            RECURSIVE_KEY,
            hp.invalid,
            hp.line,
            hp.key_start,
            format!(
                "Invalid value for '{}': must be a boolean (true or false)",
                RECURSIVE_KEY
            ),
        );
    }
    if let Some(ref hp) = hydra_obj.convert {
        check_keyword(
            CONVERT_KEY,
            hp.invalid,
            hp.line,
            hp.key_start,
            format!(
                "Invalid value for '{}': must be one of: {}",
                CONVERT_KEY,
                ConvertMode::variants().join(", ")
            ),
        );
    }
    if let Some(ref hp) = hydra_obj.args {
        check_keyword(
            ARGS_KEY,
            hp.invalid,
            hp.line,
            hp.key_start,
            format!("Invalid value for '{}': must be a list", ARGS_KEY),
        );
    }

    diagnostics
}

/// Validate all targets in a document.
///
/// Takes a salsa database and `PythonConfig` so target resolution shares the
/// cached_definition_info LRU with the rest of the LSP.
///
/// `extra_suppressions` is unioned with the file's own suppressions so the
/// caller can inject globally-disabled rules (e.g. from user settings) without
/// mutating the cached `ParsedContent`.
pub fn validate_document(
    parsed_content: &ParsedContent,
    extra_suppressions: &HashSet<DiagnosticRule>,
    db: &dyn ruff_db::Db,
    python_config: PythonConfig,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    let suppressions: HashSet<DiagnosticRule> = if extra_suppressions.is_empty() {
        parsed_content.file_suppressions.clone()
    } else {
        parsed_content
            .file_suppressions
            .iter()
            .copied()
            .chain(extra_suppressions.iter().copied())
            .collect()
    };

    for target in &parsed_content.hydra_objects {
        let (target_diagnostics, definition_info) =
            validate_target(target, db, python_config, &suppressions);
        diagnostics.extend(target_diagnostics);

        // Try to resolve the target and validate parameters
        if let Some(definition_info) = &definition_info {
            let implicit_param = definition_info.implicit_param();
            let (signature, display_name) = match definition_info {
                DefinitionInfo::Function(sig) => (sig, sig.name.clone()),
                DefinitionInfo::Class(class_info) => {
                    // For classes, use the __init__ signature if available
                    if let Some(init_sig) = &class_info.init_signature {
                        (init_sig, format!("{}.{}", class_info.name, init_sig.name))
                    } else {
                        // Class with no __init__, no parameters to validate
                        continue;
                    }
                }
                DefinitionInfo::Method(method_info) => {
                    (&method_info.signature, method_info.signature.name.clone())
                }
            };

            let parameter_diagnostics = validate_parameters(
                target,
                signature,
                &display_name,
                implicit_param,
                &suppressions,
            );
            diagnostics.extend(parameter_diagnostics);
        }
        // If Python analysis fails, we've already added a basic validation diagnostic above

        // Validate Hydra keyword values
        let keyword_diagnostics = validate_hydra_keywords(target, &suppressions);
        diagnostics.extend(keyword_diagnostics);
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
    use crate::database::HydraDatabase;
    use crate::python_analyzer::ParameterInfo;
    use crate::yaml_parser::{HydraParameter, Parameter, YamlValue};
    use hashlink::LinkedHashMap;
    use ruff_db::system::SystemPath;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    fn get_simple_test_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("workspace")
            .join("simple")
    }

    /// Build a fresh salsa db + PythonConfig for tests. Uses `HydraDatabase`
    /// (not `TestDb`) so analyzer file reads can hit the real workspace files.
    fn test_env(workspace_root: Option<&Path>) -> (HydraDatabase, PythonConfig) {
        let db = HydraDatabase::new(SystemPath::new("/"));
        let workspace = workspace_root.map(|p| p.to_string_lossy().to_string());
        let config = PythonConfig::new(&db, workspace, None);
        (db, config)
    }

    /// Helper to create a HydraObject with default suppression fields.
    fn build_hydra_object(
        value: &str,
        parameters: Vec<Parameter>,
        line: u32,
        key_start: u32,
        value_start: u32,
        is_partial: bool,
    ) -> HydraObject {
        HydraObject {
            target: HydraParameter {
                value: value.to_string(),
                line,
                invalid: false,
                key_start,
                value_start,
                value_end: value_start + value.len() as u32,
            },
            parameters,
            suppressed_rules: HashSet::new(),
            partial: if is_partial {
                Some(HydraParameter {
                    value: true,
                    line: 0,
                    invalid: false,
                    key_start: 0,
                    value_start: 0,
                    value_end: 0,
                })
            } else {
                None
            },
            recursive: None,
            convert: None,
            args: None,
        }
    }

    /// Helper to create a keyword Parameter with default suppression fields.
    fn make_param(key: &str, value: YamlValue, line: u32) -> Parameter {
        Parameter::Keyword {
            key: key.to_string(),
            value,
            line,
            key_start: 0,
            value_start: 0,
            value_end: 0,
            suppressed_rules: HashSet::new(),
        }
    }

    // ==================== validate_parameters tests ====================

    #[test]
    fn test_validate_missing_required_param() {
        let hydra_obj = build_hydra_object("my.Class", Vec::new(), 0, 0, 0, false);

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

        let diagnostics = validate_parameters(
            &hydra_obj,
            &signature,
            &signature.name,
            Some("self"),
            &HashSet::new(),
        );
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
        let hydra_obj = build_hydra_object("my.Class", params, 0, 0, 0, false);

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

        let diagnostics = validate_parameters(
            &hydra_obj,
            &signature,
            &signature.name,
            Some("self"),
            &HashSet::new(),
        );
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
        let hydra_obj = build_hydra_object("my.Class", params, 0, 0, 0, false);

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

        let diagnostics = validate_parameters(
            &hydra_obj,
            &signature,
            &signature.name,
            Some("self"),
            &HashSet::new(),
        );
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
        let hydra_obj = build_hydra_object("my.Class.from_config", Vec::new(), 0, 0, 0, false);

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

        let diagnostics = validate_parameters(
            &hydra_obj,
            &signature,
            &signature.name,
            Some("cls"),
            &HashSet::new(),
        );
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
        let hydra_obj = build_hydra_object("my.Class.from_config", params, 0, 0, 0, false);

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

        let diagnostics = validate_parameters(
            &hydra_obj,
            &signature,
            &signature.name,
            Some("cls"),
            &HashSet::new(),
        );
        assert!(
            diagnostics.is_empty(),
            "Should have no diagnostics when all required params are provided, but got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_non_conventional_self_name_filtered() {
        // Instance method using "this" instead of "self" should still be filtered
        let hydra_obj = build_hydra_object("my.Class", Vec::new(), 0, 0, 0, false);

        let signature = FunctionSignature {
            name: "__init__".to_string(),
            parameters: vec![
                ParameterInfo {
                    name: "this".to_string(),
                    type_annotation: None,
                    default_value: None,
                    has_default: false,
                    is_variadic: false,
                    is_variadic_keyword: false,
                    is_keyword_only: false,
                },
                ParameterInfo {
                    name: "value".to_string(),
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

        // "this" is the implicit first param, so it should be filtered
        let diagnostics = validate_parameters(
            &hydra_obj,
            &signature,
            &signature.name,
            Some("this"),
            &HashSet::new(),
        );
        assert_eq!(
            diagnostics.len(),
            1,
            "Expected 1 diagnostic for missing 'value', got: {:?}",
            diagnostics
        );
        assert!(
            diagnostics[0].message.contains("value"),
            "Should report missing 'value', not 'this': {}",
            diagnostics[0].message
        );
        assert!(
            !diagnostics[0].message.contains("this"),
            "Should not mention 'this' as missing: {}",
            diagnostics[0].message
        );
    }

    #[test]
    fn test_non_conventional_cls_name_filtered() {
        // Classmethod using "klass" instead of "cls" should still be filtered
        let params = vec![make_param(
            "config_path",
            YamlValue::String("path".to_string()),
            2,
        )];
        let hydra_obj = build_hydra_object("my.Class.from_config", params, 0, 0, 0, false);

        let signature = FunctionSignature {
            name: "from_config".to_string(),
            parameters: vec![
                ParameterInfo {
                    name: "klass".to_string(),
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

        // "klass" is the implicit first param, so it should be filtered
        let diagnostics = validate_parameters(
            &hydra_obj,
            &signature,
            &signature.name,
            Some("klass"),
            &HashSet::new(),
        );
        assert!(
            diagnostics.is_empty(),
            "Should have no diagnostics when all required params are provided, but got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_staticmethod_no_implicit_param() {
        // Static methods have no implicit parameter to filter
        let hydra_obj = build_hydra_object("my.Class.create", Vec::new(), 0, 0, 0, false);

        let signature = FunctionSignature {
            name: "create".to_string(),
            parameters: vec![ParameterInfo {
                name: "value".to_string(),
                type_annotation: Some("int".to_string()),
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

        // No implicit param for static methods
        let diagnostics = validate_parameters(
            &hydra_obj,
            &signature,
            &signature.name,
            None,
            &HashSet::new(),
        );
        assert_eq!(
            diagnostics.len(),
            1,
            "Expected 1 diagnostic for missing 'value', got: {:?}",
            diagnostics
        );
        assert!(diagnostics[0].message.contains("value"));
    }

    // ==================== validate_target tests ====================

    #[test]
    fn test_validate_target_invalid_format() {
        let hydra_obj = build_hydra_object(
            "InvalidTarget",
            Vec::new(),
            0,
            10,
            10 + "_target_:".len() as u32 + 1,
            false,
        );

        let (db, config) = test_env(None);
        let (diagnostics, _definition_info) =
            validate_target(&hydra_obj, &db, config, &HashSet::new());
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].message.contains("Invalid _target_ format"));
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diagnostics[0].code,
            Some(tower_lsp::lsp_types::NumberOrString::String(
                "invalid-hydra-parameter".to_string()
            ))
        );
    }

    #[test]
    fn test_validate_target_module_not_found() {
        let hydra_obj = build_hydra_object(
            "nonexistent.module.Class",
            Vec::new(),
            0,
            10,
            10 + "_target_:".len() as u32 + 1,
            false,
        );

        let resources_dir = get_simple_test_dir();
        let (db, config) = test_env(Some(&resources_dir));
        let (diagnostics, _definition_info) =
            validate_target(&hydra_obj, &db, config, &HashSet::new());
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
        let hydra_obj = build_hydra_object(
            "my_module.NonExistentClass",
            Vec::new(),
            0,
            10,
            10 + "_target_:".len() as u32 + 1,
            false,
        );

        let resources_dir = get_simple_test_dir();
        let (db, config) = test_env(Some(&resources_dir));
        let (diagnostics, _definition_info) =
            validate_target(&hydra_obj, &db, config, &HashSet::new());
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
        let hydra_obj = build_hydra_object(
            "my_module.ClassWithInit",
            Vec::new(),
            0,
            10,
            10 + "_target_:".len() as u32 + 1,
            false,
        );

        let resources_dir = get_simple_test_dir();
        let (db, config) = test_env(Some(&resources_dir));
        let (diagnostics, _definition_info) =
            validate_target(&hydra_obj, &db, config, &HashSet::new());

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
        let hydra_obj = build_hydra_object(
            "my_module.simple_function",
            Vec::new(),
            0,
            10,
            10 + "_target_:".len() as u32 + 1,
            false,
        );

        let resources_dir = get_simple_test_dir();
        let (db, config) = test_env(Some(&resources_dir));
        let (diagnostics, _definition_info) =
            validate_target(&hydra_obj, &db, config, &HashSet::new());

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
            build_hydra_object("my_module.simple_function", Vec::new(), 0, 10, vs, false),
            build_hydra_object("InvalidTarget", Vec::new(), 2, 10, vs, false),
            build_hydra_object("nonexistent.Module", Vec::new(), 4, 10, vs, false),
        ];
        let parsed_content: ParsedContent = ParsedContent {
            hydra_objects: targets,
            target_line_map: HashMap::new(),
            param_line_map: HashMap::new(),
            file_suppressions: HashSet::new(),
        };

        let resources_dir = get_simple_test_dir();
        let (db, config) = test_env(Some(&resources_dir));
        let diagnostics = validate_document(&parsed_content, &HashSet::new(), &db, config);

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
        let targets = vec![build_hydra_object(
            "my_module.ClassWithInit",
            params,
            0,
            10,
            vs,
            false,
        )];
        let parsed_content: ParsedContent = ParsedContent {
            hydra_objects: targets,
            target_line_map: HashMap::new(),
            param_line_map: HashMap::new(),
            file_suppressions: HashSet::new(),
        };

        let resources_dir = get_simple_test_dir();
        let (db, config) = test_env(Some(&resources_dir));
        let diagnostics = validate_document(&parsed_content, &HashSet::new(), &db, config);

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
        let targets = vec![build_hydra_object(
            "my_module.function_with_params",
            params,
            0,
            10,
            vs,
            false,
        )];

        let resources_dir = get_simple_test_dir();
        let parsed_content: ParsedContent = ParsedContent {
            hydra_objects: targets,
            target_line_map: HashMap::new(),
            param_line_map: HashMap::new(),
            file_suppressions: HashSet::new(),
        };

        let (db, config) = test_env(Some(&resources_dir));
        let diagnostics = validate_document(&parsed_content, &HashSet::new(), &db, config);

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
        let hydra_obj = build_hydra_object("my.Class", Vec::new(), 0, 0, 0, true);

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

        let diagnostics = validate_parameters(
            &hydra_obj,
            &signature,
            &signature.name,
            Some("self"),
            &HashSet::new(),
        );
        assert!(
            diagnostics.is_empty(),
            "Should have no diagnostics when _partial_: true, but got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_partial_false_reports_missing_required_params() {
        let hydra_obj = build_hydra_object("my.Class", Vec::new(), 0, 0, 0, false);

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

        let diagnostics = validate_parameters(
            &hydra_obj,
            &signature,
            &signature.name,
            Some("self"),
            &HashSet::new(),
        );
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
        let hydra_obj = build_hydra_object("my.Class", params, 0, 0, 0, true);

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

        let diagnostics = validate_parameters(
            &hydra_obj,
            &signature,
            &signature.name,
            Some("self"),
            &HashSet::new(),
        );
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
        let hydra_object = build_hydra_object("InvalidTarget", Vec::new(), 2, 10, vs, false);
        let mut file_suppressions = HashSet::new();
        file_suppressions.insert(DiagnosticRule::InvalidHydraParameter);
        let targets = vec![hydra_object];
        let parsed_content: ParsedContent = ParsedContent {
            hydra_objects: targets,
            target_line_map: HashMap::new(),
            param_line_map: HashMap::new(),
            file_suppressions,
        };

        let (db, config) = test_env(None);
        let diagnostics = validate_document(&parsed_content, &HashSet::new(), &db, config);

        assert!(
            diagnostics.is_empty(),
            "Should have no diagnostics when rule is file-suppressed, but got: {:?}",
            diagnostics
        );
    }

    #[test]
    fn test_validate_document_with_inline_suppression_on_target() {
        let vs = 10 + "_target_:".len() as u32 + 1;
        let mut hydra_object = build_hydra_object("InvalidTarget", Vec::new(), 2, 10, vs, false);
        hydra_object
            .suppressed_rules
            .insert(DiagnosticRule::InvalidHydraParameter);
        let targets = vec![hydra_object];
        let parsed_content: ParsedContent = ParsedContent {
            hydra_objects: targets,
            target_line_map: HashMap::new(),
            param_line_map: HashMap::new(),
            file_suppressions: HashSet::new(),
        };

        let (db, config) = test_env(None);
        let diagnostics = validate_document(&parsed_content, &HashSet::new(), &db, config);

        assert!(
            diagnostics.is_empty(),
            "Should have no diagnostics when rule is inline-suppressed on target, but got: {:?}",
            diagnostics
        );
    }

    // ==================== Invalid Keyword Value Tests ====================

    #[test]
    fn test_invalid_recursive_value_diagnostic() {
        let mut hydra_object = build_hydra_object("InvalidTarget", Vec::new(), 0, 0, 0, false);
        hydra_object.recursive = Some(HydraParameter {
            value: false,
            line: 1,
            invalid: true,
            key_start: 0,
            value_start: 0,
            value_end: 0,
        });
        let parsed_content = ParsedContent {
            hydra_objects: vec![hydra_object],
            target_line_map: HashMap::new(),
            param_line_map: HashMap::new(),
            file_suppressions: HashSet::new(),
        };

        let (db, config) = test_env(None);
        let diagnostics = validate_document(&parsed_content, &HashSet::new(), &db, config);
        let keyword_diags: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("_recursive_"))
            .collect();
        assert_eq!(keyword_diags.len(), 1);
        assert!(keyword_diags[0].message.contains("boolean"));
        assert_eq!(
            keyword_diags[0].code,
            Some(tower_lsp::lsp_types::NumberOrString::String(
                "invalid-hydra-parameter".to_string()
            ))
        );
    }

    #[test]
    fn test_invalid_convert_value_diagnostic() {
        let mut hydra_object = build_hydra_object("InvalidTarget", Vec::new(), 0, 0, 0, false);
        hydra_object.convert = Some(HydraParameter {
            value: crate::yaml_parser::ConvertMode::None,
            line: 1,
            invalid: true,
            key_start: 0,
            value_start: 0,
            value_end: 0,
        });
        let parsed_content = ParsedContent {
            hydra_objects: vec![hydra_object],
            target_line_map: HashMap::new(),
            param_line_map: HashMap::new(),
            file_suppressions: HashSet::new(),
        };

        let (db, config) = test_env(None);
        let diagnostics = validate_document(&parsed_content, &HashSet::new(), &db, config);
        let keyword_diags: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("_convert_"))
            .collect();
        assert_eq!(keyword_diags.len(), 1);
        assert!(
            keyword_diags[0]
                .message
                .contains("none, partial, all, object")
        );
        assert_eq!(
            keyword_diags[0].code,
            Some(tower_lsp::lsp_types::NumberOrString::String(
                "invalid-hydra-parameter".to_string()
            ))
        );
    }

    #[test]
    fn test_invalid_args_value_diagnostic() {
        let mut hydra_object = build_hydra_object("InvalidTarget", Vec::new(), 0, 0, 0, false);
        hydra_object.args = Some(HydraParameter {
            value: None,
            line: 1,
            invalid: true,
            key_start: 0,
            value_start: 0,
            value_end: 0,
        });
        let parsed_content = ParsedContent {
            hydra_objects: vec![hydra_object],
            target_line_map: HashMap::new(),
            param_line_map: HashMap::new(),
            file_suppressions: HashSet::new(),
        };

        let (db, config) = test_env(None);
        let diagnostics = validate_document(&parsed_content, &HashSet::new(), &db, config);
        let keyword_diags: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("_args_"))
            .collect();
        assert_eq!(keyword_diags.len(), 1);
        assert!(keyword_diags[0].message.contains("list"));
    }

    #[test]
    fn test_valid_keywords_no_diagnostics() {
        let mut hydra_object = build_hydra_object("InvalidTarget", Vec::new(), 0, 0, 0, false);
        hydra_object.recursive = Some(HydraParameter {
            value: true,
            line: 1,
            invalid: false,
            key_start: 0,
            value_start: 0,
            value_end: 0,
        });
        hydra_object.convert = Some(HydraParameter {
            value: crate::yaml_parser::ConvertMode::All,
            line: 2,
            invalid: false,
            key_start: 0,
            value_start: 0,
            value_end: 0,
        });
        hydra_object.args = Some(HydraParameter {
            value: None,
            line: 3,
            invalid: false,
            key_start: 0,
            value_start: 0,
            value_end: 0,
        });
        let parsed_content = ParsedContent {
            hydra_objects: vec![hydra_object],
            target_line_map: HashMap::new(),
            param_line_map: HashMap::new(),
            file_suppressions: HashSet::new(),
        };

        let (db, config) = test_env(None);
        let diagnostics = validate_document(&parsed_content, &HashSet::new(), &db, config);
        let keyword_diags: Vec<_> = diagnostics
            .iter()
            .filter(|d| {
                d.message.contains("_recursive_")
                    || d.message.contains("_convert_")
                    || d.message.contains("_args_")
            })
            .collect();
        assert!(
            keyword_diags.is_empty(),
            "Valid keywords should produce no keyword diagnostics, got: {:?}",
            keyword_diags
        );
    }
}
