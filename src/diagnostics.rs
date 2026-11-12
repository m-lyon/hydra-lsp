use crate::python_analyzer::{
    DefinitionInfo, FunctionSignature, ParameterInfo, ParameterKind, ResolveError,
};
use crate::python_cache::{PythonConfig, TargetString, cached_definition_info, is_builtin_symbol};
use crate::vendored_typeshed::BUILTINS_MODULE;
use crate::yaml_parser::{
    ARGS_KEY, CONVERT_KEY, ConvertMode, HydraObject, PARTIAL_KEY, Parameter, ParsedContent,
    RECURSIVE_KEY, utf16_len,
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
    PositionalOnlyParameter => "positional-only-parameter",
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
        Err(err) => {
            let rule = err.rule();
            let msg = match err {
                ResolveError::InvalidFormat(_) => {
                    format!("{}. {}", err, target_format_hint(db, target))
                }
                ResolveError::UnresolvedModule(_) | ResolveError::Other(_) => err.to_string(),
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

impl ResolveError {
    /// The diagnostic rule a failed `_target_` lookup is reported under.
    ///
    /// Lives here rather than beside `ResolveError` so `python_analyzer` does
    /// not depend on `diagnostics`.
    pub fn rule(&self) -> DiagnosticRule {
        match self {
            ResolveError::InvalidFormat(_) => DiagnosticRule::InvalidHydraParameter,
            ResolveError::UnresolvedModule(_) => DiagnosticRule::UnresolvedImport,
            ResolveError::Other(_) => DiagnosticRule::UnresolvedReference,
        }
    }
}

fn target_format_hint(db: &dyn ruff_db::Db, target: TargetString<'_>) -> String {
    let name = target.value(db);
    if is_builtin_symbol(db, target) {
        format!("'{name}' is a Python builtin; use '{BUILTINS_MODULE}.{name}'")
    } else {
        "Expected format: 'module.path.SymbolName'".to_string()
    }
}

struct KeywordArg<'a> {
    param: &'a Parameter,
    key: &'a str,
    line: u32,
    key_start: u32,
}

struct CallCheck<'a> {
    hydra_obj: &'a HydraObject,
    display_name: &'a str,
    file_suppressions: &'a HashSet<DiagnosticRule>,
    params: Vec<&'a ParameterInfo>,
    has_var_positional: bool,
    has_var_keyword: bool,
    keywords: Vec<KeywordArg<'a>>,
    num_positional_args: usize,
}

impl<'a> CallCheck<'a> {
    fn new(
        hydra_obj: &'a HydraObject,
        signature: &'a FunctionSignature,
        display_name: &'a str,
        implicit_param: Option<&str>,
        file_suppressions: &'a HashSet<DiagnosticRule>,
    ) -> Self {
        let has_kind = |kind| signature.parameters.iter().any(|p| p.kind == kind);
        let keywords = hydra_obj
            .parameters
            .iter()
            .filter_map(|param| match param {
                Parameter::Keyword {
                    key,
                    line,
                    key_start,
                    ..
                } => Some(KeywordArg {
                    param,
                    key,
                    line: *line,
                    key_start: *key_start,
                }),
                Parameter::Positional { .. } => None,
            })
            .collect();
        CallCheck {
            hydra_obj,
            display_name,
            file_suppressions,
            params: signature
                .parameters
                .iter()
                .filter(|p| Some(p.name.as_str()) != implicit_param)
                .collect(),
            has_var_positional: has_kind(ParameterKind::VarPositional),
            has_var_keyword: has_kind(ParameterKind::VarKeyword),
            keywords,
            num_positional_args: hydra_obj
                .parameters
                .iter()
                .filter(|p| matches!(p, Parameter::Positional { .. }))
                .count(),
        }
    }

    /// Whether `rule` is off for the whole file or this node.
    fn rule_suppressed(&self, rule: DiagnosticRule) -> bool {
        self.file_suppressions.contains(&rule) || self.hydra_obj.suppressed_rules.contains(&rule)
    }

    /// Whether `rule` is off for `arg`, which may also suppress it itself.
    fn suppressed_at(&self, rule: DiagnosticRule, arg: &KeywordArg) -> bool {
        self.rule_suppressed(rule) || arg.param.suppressed_rules().contains(&rule)
    }

    /// A diagnostic on `arg`'s key.
    fn at_key(
        &self,
        arg: &KeywordArg,
        severity: DiagnosticSeverity,
        rule: Option<DiagnosticRule>,
        message: String,
    ) -> Diagnostic {
        create_diagnostic(
            arg.line,
            arg.key_start,
            arg.key_start + utf16_len(arg.key),
            severity,
            rule,
            message,
        )
    }

    /// The named parameter `name`, if the signature has one a keyword can
    /// match (`*args` and `**kwargs` are not matched by name).
    fn named(&self, name: &str) -> Option<&'a ParameterInfo> {
        self.params
            .iter()
            .copied()
            .find(|p| p.name == name && !p.is_variadic())
    }

    /// The parameters `_args_` entries can fill, in order.
    fn positional_params(&self) -> impl Iterator<Item = &'a ParameterInfo> + '_ {
        self.params
            .iter()
            .copied()
            .filter(|p| p.binds_positionally())
    }

    /// The parameters the `_args_` entries actually fill.
    fn positionally_covered(&self) -> impl Iterator<Item = &'a ParameterInfo> + '_ {
        self.positional_params().take(self.num_positional_args)
    }

    fn is_positionally_covered(&self, name: &str) -> bool {
        self.positionally_covered().any(|p| p.name == name)
    }

    fn has_keyword(&self, name: &str) -> bool {
        self.keywords.iter().any(|arg| arg.key == name)
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

    // An overloaded symbol is only represented here by its first declaration,
    // so parameter validation is not correct.
    if signature.is_overloaded {
        return diagnostics;
    }

    let check = CallCheck::new(
        hydra_obj,
        signature,
        display_name,
        implicit_param,
        file_suppressions,
    );
    check_positional_only_keywords(&check, &mut diagnostics);
    check_unknown_keywords(&check, &mut diagnostics);
    check_missing_arguments(&check, &mut diagnostics);
    check_already_assigned(&check, &mut diagnostics);
    check_too_many_positional(&check, &mut diagnostics);
    diagnostics
}

fn check_positional_only_keywords(check: &CallCheck, diagnostics: &mut Vec<Diagnostic>) {
    if check.has_var_keyword {
        return;
    }
    let rule = DiagnosticRule::PositionalOnlyParameter;
    for arg in &check.keywords {
        let positional_only = check
            .named(arg.key)
            .is_some_and(|p| p.kind == ParameterKind::PositionalOnly);
        if positional_only && !check.suppressed_at(rule, arg) {
            diagnostics.push(check.at_key(
                arg,
                DiagnosticSeverity::ERROR,
                Some(rule),
                format!(
                    "Parameter '{}' of '{}' is positional-only; pass it via {}",
                    arg.key, check.display_name, ARGS_KEY
                ),
            ));
        }
    }
}

fn check_unknown_keywords(check: &CallCheck, diagnostics: &mut Vec<Diagnostic>) {
    let rule = DiagnosticRule::UnknownArgument;
    let mut hinted = HashSet::new();
    for arg in &check.keywords {
        if check.named(arg.key).is_some() {
            continue;
        }
        if !check.has_var_keyword {
            if !check.suppressed_at(rule, arg) {
                diagnostics.push(check.at_key(
                    arg,
                    DiagnosticSeverity::ERROR,
                    Some(rule),
                    format!(
                        "Unknown parameter '{}' for '{}'",
                        arg.key, check.display_name
                    ),
                ));
            }
        } else if hinted.insert(arg.key) && !check.suppressed_at(rule, arg) {
            diagnostics.push(check.at_key(
                arg,
                DiagnosticSeverity::HINT,
                None,
                format!("Parameter '{}' will be passed via **kwargs", arg.key),
            ));
        }
    }
}

fn check_missing_arguments(check: &CallCheck, diagnostics: &mut Vec<Diagnostic>) {
    let rule = DiagnosticRule::MissingArgument;
    if check.hydra_obj.is_partial() || check.rule_suppressed(rule) {
        return;
    }
    for param in &check.params {
        let positional_only = param.kind == ParameterKind::PositionalOnly;
        let accounted_for_by_keyword =
            check.has_keyword(&param.name) && !(positional_only && check.has_var_keyword);

        if param.is_required()
            && !accounted_for_by_keyword
            && !check.is_positionally_covered(&param.name)
        {
            let message = if positional_only {
                format!(
                    "Missing required positional-only parameter '{}' for '{}'; pass it via {}",
                    param.name, check.display_name, ARGS_KEY
                )
            } else {
                format!(
                    "Missing required parameter '{}' for '{}'",
                    param.name, check.display_name
                )
            };
            let hydra_obj = check.hydra_obj;
            diagnostics.push(create_diagnostic(
                hydra_obj.target.line,
                hydra_obj.target.value_start,
                hydra_obj.target_value_end(),
                DiagnosticSeverity::ERROR,
                Some(rule),
                message,
            ));
        }
    }
}

fn check_already_assigned(check: &CallCheck, diagnostics: &mut Vec<Diagnostic>) {
    let rule = DiagnosticRule::ParameterAlreadyAssigned;
    let mut seen = HashSet::new();
    for arg in &check.keywords {
        if !seen.insert(arg.key) {
            continue;
        }
        let assigned_twice = check
            .positionally_covered()
            .any(|p| p.name == arg.key && p.kind != ParameterKind::PositionalOnly);
        if assigned_twice && !check.suppressed_at(rule, arg) {
            diagnostics.push(check.at_key(
                arg,
                DiagnosticSeverity::ERROR,
                Some(rule),
                format!(
                    "'{}' got multiple values for argument '{}' (already provided positionally via _args_)",
                    check.display_name, arg.key
                ),
            ));
        }
    }
}

fn check_too_many_positional(check: &CallCheck, diagnostics: &mut Vec<Diagnostic>) {
    let rule = DiagnosticRule::TooManyPositionalArguments;
    let capacity = check.positional_params().count();
    if check.num_positional_args > capacity
        && !check.has_var_positional
        && !check.rule_suppressed(rule)
        && let Some(ref hp) = check.hydra_obj.args
    {
        diagnostics.push(create_diagnostic(
            hp.line,
            hp.key_start,
            hp.key_start + ARGS_KEY.len() as u32,
            DiagnosticSeverity::ERROR,
            Some(rule),
            format!(
                "'{}' takes {} positional argument(s) but {} were given via _args_",
                check.display_name, capacity, check.num_positional_args
            ),
        ));
    }
}

/// Whether `signature` accepts the arguments `hydra_obj` passes: its keys,
/// its `_args_` count and, unless it is `_partial_`, every required parameter.
fn accepts_arguments(
    hydra_obj: &HydraObject,
    signature: &FunctionSignature,
    implicit_param: Option<&str>,
) -> bool {
    validate_parameters(hydra_obj, signature, "", implicit_param, &HashSet::new())
        .iter()
        .all(|d| d.severity != Some(DiagnosticSeverity::ERROR))
}

/// The indices into `signature.overloads` of the overloads `hydra_obj`'s
/// arguments match, in source order.
pub fn matching_overloads(
    hydra_obj: &HydraObject,
    signature: &FunctionSignature,
    implicit_param: Option<&str>,
) -> Vec<usize> {
    signature
        .overloads
        .iter()
        .enumerate()
        .filter(|(_, overload)| accepts_arguments(hydra_obj, overload, implicit_param))
        .map(|(i, _)| i)
        .collect()
}

/// Narrow the overloads of `definition`'s call signature to those matching
/// `hydra_obj`.
pub fn narrow_overloads(definition: &mut DefinitionInfo, hydra_obj: &HydraObject) {
    let implicit_param = definition.implicit_param().map(str::to_owned);
    let Some(signature) = definition.call_signature_mut() else {
        return;
    };
    let matching = matching_overloads(hydra_obj, signature, implicit_param.as_deref());
    if matching.is_empty() {
        return;
    }
    let mut index = 0;
    signature.overloads.retain(|_| {
        let keep = matching.contains(&index);
        index += 1;
        keep
    });
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

        // Try to resolve the target and validate parameters. `None` here means
        // there is nothing sound to validate against.
        let callable = definition_info.as_ref().and_then(|definition_info| {
            // A class with no constructor has no parameters to check.
            let signature = definition_info.call_signature()?;
            let display_name = match definition_info {
                DefinitionInfo::Class(class_info) if class_info.constructor_is_uncertain() => {
                    return None;
                }
                DefinitionInfo::Class(class_info) => {
                    format!("{}.{}", class_info.name, signature.name)
                }
                DefinitionInfo::Function(_) | DefinitionInfo::Method(_) => signature.name.clone(),
            };
            Some((signature, display_name, definition_info.implicit_param()))
        });

        if let Some((signature, display_name, implicit_param)) = callable {
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

    // Sort all diagnostics by position for consistent ordering. The sort is
    // stable, so diagnostics at the same position keep their emission order.
    diagnostics.sort_by_key(|d| (d.range.start.line, d.range.start.character));

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

    // ==================== ResolveError::rule tests ====================

    #[test]
    fn test_resolve_error_rule_invalid_format() {
        assert_eq!(
            ResolveError::InvalidFormat("Foo".to_string()).rule(),
            DiagnosticRule::InvalidHydraParameter
        );
    }

    #[test]
    fn test_resolve_error_rule_unresolved_module() {
        assert_eq!(
            ResolveError::UnresolvedModule("no_such".to_string()).rule(),
            DiagnosticRule::UnresolvedImport
        );
    }

    #[test]
    fn test_resolve_error_rule_other() {
        assert_eq!(
            ResolveError::Other("Symbol 'X' not found in module 'm'".to_string()).rule(),
            DiagnosticRule::UnresolvedReference
        );
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
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: None,
                    default_value: None,
                },
                ParameterInfo {
                    name: "required_param".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: Some("int".to_string()),
                    default_value: None,
                },
            ],
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
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
                kind: ParameterKind::PositionalOrKeyword,
                type_annotation: None,
                default_value: None,
            }],
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
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
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: None,
                    default_value: None,
                },
                ParameterInfo {
                    name: "**kwargs".to_string(),
                    kind: ParameterKind::VarKeyword,
                    type_annotation: None,
                    default_value: None,
                },
            ],
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
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
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: None,
                    default_value: None,
                },
                ParameterInfo {
                    name: "config_path".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: Some("str".to_string()),
                    default_value: None,
                },
            ],
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
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
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: None,
                    default_value: None,
                },
                ParameterInfo {
                    name: "config_path".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: Some("str".to_string()),
                    default_value: None,
                },
            ],
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
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
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: None,
                    default_value: None,
                },
                ParameterInfo {
                    name: "value".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: Some("int".to_string()),
                    default_value: None,
                },
            ],
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
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
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: None,
                    default_value: None,
                },
                ParameterInfo {
                    name: "config_path".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: Some("str".to_string()),
                    default_value: None,
                },
            ],
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
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
                kind: ParameterKind::PositionalOrKeyword,
                type_annotation: Some("int".to_string()),
                default_value: None,
            }],
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
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

    /// The behaviour `constructor_is_uncertain` exists for: when `__new__` only
    /// stood in because part of the MRO is unreadable, arguments are not
    /// validated against it — the real `__init__` may be in the ancestor that is
    /// missing.
    #[test]
    fn test_uncertain_constructor_produces_no_argument_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mod.py"),
            concat!(
                "from not_installed import Widget\n",
                "\n",
                "class Thing(Widget):\n",
                "    def __new__(cls):\n",
                "        pass\n",
            ),
        )
        .unwrap();

        let vs = 10 + "_target_:".len() as u32 + 1;
        let parsed_content: ParsedContent = ParsedContent {
            hydra_objects: vec![build_hydra_object(
                "mod.Thing",
                vec![make_param("size", YamlValue::Integer(1), 1)],
                0,
                10,
                vs,
                false,
            )],
            target_line_map: HashMap::new(),
            param_line_map: HashMap::new(),
            file_suppressions: HashSet::new(),
        };

        let (db, config) = test_env(Some(dir.path()));
        let diagnostics = validate_document(&parsed_content, &HashSet::new(), &db, config);
        assert!(
            diagnostics.is_empty(),
            "`__new__(cls)` may not be the constructor Hydra calls, got: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    /// The same shape with a readable MRO: `__new__` is the constructor, and an
    /// argument it does not declare is reported.
    #[test]
    fn test_certain_constructor_is_validated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mod.py"),
            concat!(
                "class Widget:\n",
                "    pass\n",
                "\n",
                "class Thing(Widget):\n",
                "    def __new__(cls):\n",
                "        pass\n",
            ),
        )
        .unwrap();

        let vs = 10 + "_target_:".len() as u32 + 1;
        let parsed_content: ParsedContent = ParsedContent {
            hydra_objects: vec![build_hydra_object(
                "mod.Thing",
                vec![make_param("size", YamlValue::Integer(1), 1)],
                0,
                10,
                vs,
                false,
            )],
            target_line_map: HashMap::new(),
            param_line_map: HashMap::new(),
            file_suppressions: HashSet::new(),
        };

        let (db, config) = test_env(Some(dir.path()));
        let diagnostics = validate_document(&parsed_content, &HashSet::new(), &db, config);
        assert_eq!(
            codes(&diagnostics),
            vec![DiagnosticRule::UnknownArgument.as_code().to_string()],
        );
    }

    /// A target with no signature to validate against must not take the rest
    /// of the checks down with it: `_convert_` is still wrong regardless of
    /// what `SimpleClass`'s constructor looks like.
    #[test]
    fn test_hydra_keywords_validated_for_a_class_with_no_constructor() {
        let vs = 10 + "_target_:".len() as u32 + 1;
        let mut target = build_hydra_object("my_module.SimpleClass", Vec::new(), 0, 10, vs, false);
        target.convert = Some(HydraParameter {
            value: ConvertMode::None,
            line: 1,
            invalid: true,
            key_start: 2,
            value_start: 12,
            value_end: 20,
        });
        let parsed_content: ParsedContent = ParsedContent {
            hydra_objects: vec![target],
            target_line_map: HashMap::new(),
            param_line_map: HashMap::new(),
            file_suppressions: HashSet::new(),
        };

        let resources_dir = get_simple_test_dir();
        let (db, config) = test_env(Some(&resources_dir));
        let diagnostics = validate_document(&parsed_content, &HashSet::new(), &db, config);

        assert!(
            diagnostics.iter().any(|d| d.message.contains(CONVERT_KEY)),
            "got: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
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
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: None,
                    default_value: None,
                },
                ParameterInfo {
                    name: "required_param".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: Some("int".to_string()),
                    default_value: None,
                },
            ],
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
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
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: None,
                    default_value: None,
                },
                ParameterInfo {
                    name: "required_param".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: Some("int".to_string()),
                    default_value: None,
                },
            ],
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
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
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: None,
                    default_value: None,
                },
                ParameterInfo {
                    name: "required_param".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: Some("int".to_string()),
                    default_value: None,
                },
                ParameterInfo {
                    name: "valid_param".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: Some("str".to_string()),
                    default_value: Some("\"default\"".to_string()),
                },
            ],
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
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

    // ==================== positional-only tests ====================

    /// Codes of the diagnostics produced, for readable assertions.
    fn codes(diagnostics: &[Diagnostic]) -> Vec<String> {
        diagnostics
            .iter()
            .map(|d| match &d.code {
                Some(tower_lsp::lsp_types::NumberOrString::String(c)) => c.clone(),
                _ => "none".to_string(),
            })
            .collect()
    }

    /// Build a parameter with no default for a synthetic signature.
    fn sig_param(name: &str, kind: ParameterKind) -> ParameterInfo {
        ParameterInfo {
            name: name.to_string(),
            kind,
            type_annotation: None,
            default_value: None,
        }
    }

    fn sig_for(name: &str, parameters: Vec<ParameterInfo>) -> FunctionSignature {
        FunctionSignature {
            name: name.to_string(),
            parameters,
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 1,
        }
    }

    #[test]
    fn test_positional_only_passed_by_name_is_reported() {
        let hydra_obj = build_hydra_object(
            "mod.f",
            vec![make_param("a", YamlValue::Integer(1), 1)],
            0,
            0,
            0,
            false,
        );
        let signature = sig_for("f", vec![sig_param("a", ParameterKind::PositionalOnly)]);

        let diagnostics = validate_parameters(&hydra_obj, &signature, "f", None, &HashSet::new());

        // The one report names the real problem and points at `_args_`; the
        // parameter is not also reported as missing.
        assert_eq!(
            codes(&diagnostics),
            vec![
                DiagnosticRule::PositionalOnlyParameter
                    .as_code()
                    .to_string()
            ],
        );
    }

    /// `def f(a, /, **kw)` accepts the *key* `a=1` — it lands in `kw` — so
    /// there is no positional-only report. The positional slot is still empty
    /// though, which `f(a=1)` raises a `TypeError` for, so it is reported as a
    /// missing argument that `_args_` has to supply.
    #[test]
    fn test_required_positional_only_with_kwargs_is_still_missing() {
        let hydra_obj = build_hydra_object(
            "mod.f",
            vec![make_param("a", YamlValue::Integer(1), 1)],
            0,
            0,
            0,
            false,
        );
        let signature = sig_for(
            "f",
            vec![
                sig_param("a", ParameterKind::PositionalOnly),
                sig_param("kw", ParameterKind::VarKeyword),
            ],
        );

        let diagnostics = validate_parameters(&hydra_obj, &signature, "f", None, &HashSet::new());

        assert_eq!(
            codes(&diagnostics),
            vec![DiagnosticRule::MissingArgument.as_code().to_string()],
        );
        assert!(diagnostics[0].message.contains(ARGS_KEY));
    }

    /// The same shape with a default: nothing is missing and the key is legal,
    /// so there is nothing at all to report.
    #[test]
    fn test_defaulted_positional_only_with_kwargs_is_clean() {
        let hydra_obj = build_hydra_object(
            "mod.f",
            vec![make_param("a", YamlValue::Integer(1), 1)],
            0,
            0,
            0,
            false,
        );
        let mut a = sig_param("a", ParameterKind::PositionalOnly);
        a.default_value = Some("0".to_string());
        let signature = sig_for("f", vec![a, sig_param("kw", ParameterKind::VarKeyword)]);

        let diagnostics = validate_parameters(&hydra_obj, &signature, "f", None, &HashSet::new());
        assert!(diagnostics.is_empty(), "got: {:?}", codes(&diagnostics));
    }

    /// Columns are UTF-16 code units, so the end of a key is its UTF-16 length,
    /// not its byte length.
    #[test]
    fn test_parameter_diagnostic_range_is_utf16() {
        let hydra_obj = build_hydra_object(
            "mod.f",
            vec![make_param("café", YamlValue::Integer(1), 1)],
            1,
            2,
            13,
            false,
        );
        let signature = sig_for(
            "f",
            vec![sig_param("a", ParameterKind::PositionalOrKeyword)],
        );

        let diagnostics = validate_parameters(&hydra_obj, &signature, "f", None, &HashSet::new());
        let unknown = &diagnostics[0];
        assert_eq!(
            unknown.range.end.character - unknown.range.start.character,
            4,
            "`café` is four UTF-16 units, five bytes"
        );
    }

    /// A diagnostic anchored to a parameter's line points at that parameter's
    /// column, not the `_target_` key's — the two only coincide in block style.
    #[test]
    fn test_parameter_diagnostics_point_at_the_parameter() {
        let mut param = make_param("nope", YamlValue::Integer(1), 1);
        if let Parameter::Keyword { key_start, .. } = &mut param {
            *key_start = 34;
        }
        let hydra_obj = build_hydra_object("mod.f", vec![param], 1, 9, 20, false);
        let signature = sig_for(
            "f",
            vec![sig_param("a", ParameterKind::PositionalOrKeyword)],
        );

        let diagnostics = validate_parameters(&hydra_obj, &signature, "f", None, &HashSet::new());

        let unknown = diagnostics
            .iter()
            .find(|d| {
                matches!(&d.code, Some(tower_lsp::lsp_types::NumberOrString::String(c))
                    if c == DiagnosticRule::UnknownArgument.as_code())
            })
            .expect("expected an unknown-argument diagnostic");
        assert_eq!(unknown.range.start.character, 34);
        assert_eq!(unknown.range.end.character, 34 + "nope".len() as u32);
    }

    /// `def f(a, /, **kw)` with `_args_: [1]` and `a: 2` binds `a` positionally
    /// and puts `{'a': 2}` in `kw` — two different bindings, not a conflict.
    #[test]
    fn test_positional_only_with_kwargs_is_not_already_assigned() {
        let mut hydra_obj = build_hydra_object(
            "mod.f",
            vec![
                make_param("a", YamlValue::Integer(2), 1),
                Parameter::Positional {
                    value: YamlValue::Integer(1),
                    line: 2,
                    value_start: 0,
                    value_end: 0,
                    suppressed_rules: HashSet::new(),
                },
            ],
            0,
            0,
            0,
            false,
        );
        hydra_obj.args = Some(HydraParameter {
            value: None,
            line: 2,
            invalid: false,
            key_start: 0,
            value_start: 0,
            value_end: 0,
        });
        let signature = sig_for(
            "f",
            vec![
                sig_param("a", ParameterKind::PositionalOnly),
                sig_param("kw", ParameterKind::VarKeyword),
            ],
        );

        let diagnostics = validate_parameters(&hydra_obj, &signature, "f", None, &HashSet::new());
        assert!(
            !diagnostics.iter().any(|d| matches!(
                &d.code,
                Some(tower_lsp::lsp_types::NumberOrString::String(c))
                    if c == DiagnosticRule::ParameterAlreadyAssigned.as_code()
            )),
            "got: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    /// An overloaded symbol is only represented by its first declaration, so
    /// nothing about the arguments can be checked against it.
    #[test]
    fn test_overloaded_signature_skips_argument_validation() {
        let hydra_obj = build_hydra_object(
            "mod.f",
            vec![make_param("not_a_param", YamlValue::Integer(1), 1)],
            0,
            0,
            0,
            false,
        );
        let mut signature = sig_for(
            "f",
            vec![sig_param("a", ParameterKind::PositionalOrKeyword)],
        );
        signature.is_overloaded = true;

        let diagnostics = validate_parameters(&hydra_obj, &signature, "f", None, &HashSet::new());
        assert!(diagnostics.is_empty(), "got: {diagnostics:?}");
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
