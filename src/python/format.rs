//! Rendering signatures as text

use super::model::{
    ClassInfo, DefaultStyle, FunctionSignature, MethodInfo, ParameterInfo, ParameterKind,
};

/// Where the markers that say *how* a parameter may be passed belong in
/// `params`, as `(after_positional_only, before_keyword_only)` indexes.
fn parameter_markers(params: &[&ParameterInfo]) -> (Option<usize>, Option<usize>) {
    let after_positional_only = params
        .iter()
        .rposition(|p| p.kind == ParameterKind::PositionalOnly);
    let before_keyword_only = params
        .iter()
        .position(|p| p.kind == ParameterKind::KeywordOnly)
        .filter(|_| {
            !params
                .iter()
                .any(|p| p.kind == ParameterKind::VarPositional)
        });
    (after_positional_only, before_keyword_only)
}

/// Render a parameter list with the `/` and `*` markers in place.
pub fn render_params(
    params: &[&ParameterInfo],
    format_param: impl Fn(&ParameterInfo) -> String,
) -> Vec<String> {
    let (after_positional_only, before_keyword_only) = parameter_markers(params);

    let mut result = Vec::with_capacity(params.len() + 2);
    for (index, param) in params.iter().enumerate() {
        if Some(index) == before_keyword_only {
            result.push("*".to_string());
        }
        result.push(format_param(param));
        if Some(index) == after_positional_only {
            result.push("/".to_string());
        }
    }
    result
}

/// Render a parameter list for hover, with the `/` and `*` markers in place.
fn format_parameters(params: &[ParameterInfo]) -> Vec<String> {
    let refs: Vec<&ParameterInfo> = params.iter().collect();
    render_params(&refs, |p| p.label(DefaultStyle::Value))
}

/// Format a function signature for display (e.g., in hover)
pub(crate) fn format_function(sig: &FunctionSignature) -> String {
    let mut result = String::new();
    result.push_str("```python\n");
    result.push_str(&format_callable(sig, None, None));
    result.push_str("\n```");

    result
}

/// Format a class for display (e.g., in hover)
/// Shows class definition with base classes, class docstring,
/// then __init__ method definition with its docstring
pub(crate) fn format_class(class: &ClassInfo) -> String {
    let mut result = String::new();
    result.push_str("```python\n");

    result.push_str(&format_definition(
        "class",
        &class.name,
        &class.base_classes,
        false,
        None,
        class.docstring.as_ref(),
        None,
    ));

    // Add the constructor (`__init__`, or `__new__` when that is all the
    // class declares) if present
    if let Some(init_sig) = &class.init_signature {
        result.push_str("\n\n");
        result.push_str(&format_callable(init_sig, None, Some(4)));
    }

    result.push_str("\n```");

    result
}

/// Format a method for display (e.g., in hover)
/// Shows the method signature with @classmethod or @staticmethod decorator if applicable
pub(crate) fn format_method(method: &MethodInfo) -> String {
    let decorator = if method.is_classmethod {
        Some("classmethod")
    } else if method.is_staticmethod {
        Some("staticmethod")
    } else {
        None
    };

    let mut result = String::new();
    result.push_str("```python\n");
    result.push_str(&format_callable(&method.signature, decorator, None));
    result.push_str("\n```");

    result
}

/// Render a callable's `def`, or one `@overload`-decorated `def` per entry
/// in `sig.overloads` when it has them.
fn format_callable(
    sig: &FunctionSignature,
    decorator: Option<&str>,
    indent: Option<usize>,
) -> String {
    let indent_str = " ".repeat(indent.unwrap_or(0));
    let is_overload_set = !sig.overloads.is_empty();
    let shown: Vec<&FunctionSignature> = if is_overload_set {
        sig.overloads.iter().collect()
    } else {
        vec![sig]
    };
    let docstring = shown
        .iter()
        .find_map(|s| s.docstring.as_ref())
        .or(sig.docstring.as_ref());

    let defs: Vec<String> = shown
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut def = String::new();
            if let Some(decorator) = decorator {
                def.push_str(&format!("{indent_str}@{decorator}\n"));
            }
            if is_overload_set {
                def.push_str(&format!("{indent_str}@overload\n"));
            }
            def.push_str(&format_definition(
                "def",
                &s.name,
                &format_parameters(&s.parameters),
                true,
                s.return_type.as_ref(),
                docstring.filter(|_| i + 1 == shown.len()),
                indent,
            ));
            def
        })
        .collect();
    defs.join("\n")
}

/// Format a function definition string
fn format_definition(
    def_str: &str,
    name: &str,
    param_strs: &[String],
    wrap_empty_params: bool,
    return_type: Option<&String>,
    docstring: Option<&String>,
    indent: Option<usize>,
) -> String {
    let mut result = String::new();
    let indent_str = " ".repeat(indent.unwrap_or(0));
    let single_line_params = if param_strs.is_empty() && !wrap_empty_params {
        String::new()
    } else {
        format!("({})", param_strs.join(", "))
    };
    let single_line = format!(
        "{}{} {}{}{}:",
        indent_str,
        def_str,
        name,
        single_line_params,
        return_type
            .map(|rt| format!(" -> {}", rt))
            .unwrap_or_default()
    );
    if single_line.len() < 100 || (param_strs.is_empty() && return_type.is_none()) {
        // Single line format (without colon - added later with docstring)
        result.push_str(&single_line[..single_line.len() - 1]);
    } else {
        // Multi-line format with parameters on separate lines
        result.push_str(&format!("{}{} {}(", indent_str, def_str, name));
        for (i, param_str) in param_strs.iter().enumerate() {
            result.push_str(&format!("\n{}    ", indent_str));
            result.push_str(param_str);
            if i < param_strs.len() - 1 {
                result.push(',');
            }
        }
        result.push_str(&format!("\n{})", indent_str));
        if let Some(ret_type) = &return_type {
            result.push_str(&format!(" -> {}", ret_type));
        }
    }

    if let Some(docstring) = docstring {
        result.push_str(&format!(":\n{}    \"\"\"{}\"\"\"", indent_str, docstring));
    } else {
        result.push(':');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::python::model::SourceRange;

    // ==================== format_signature tests ====================

    #[test]
    fn test_format_simple_signature() {
        let sig = FunctionSignature {
            name: "test_func".to_string(),
            parameters: vec![],
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
            range: SourceRange::default(),
        };

        let formatted = format_function(&sig);
        assert!(formatted.contains("def test_func()"));
        assert!(formatted.starts_with("```python"));
        assert!(formatted.contains("```"));
    }

    #[test]
    fn test_format_signature_with_params() {
        let sig = FunctionSignature {
            name: "test_func".to_string(),
            parameters: vec![
                ParameterInfo {
                    name: "x".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: Some("int".to_string()),
                    default_value: None,
                },
                ParameterInfo {
                    name: "y".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: Some("str".to_string()),
                    default_value: Some("'hello'".to_string()),
                },
            ],
            return_type: Some("bool".to_string()),
            docstring: Some("Test docstring".to_string()),
            is_overloaded: false,
            overloads: Vec::new(),
            range: SourceRange::default(),
        };

        let formatted = format_function(&sig);
        assert!(formatted.contains("def test_func(x: int, y: str = 'hello') -> bool"));
        assert!(formatted.contains("Test docstring"));
    }

    #[test]
    fn test_format_signature_with_variadic() {
        let sig = FunctionSignature {
            name: "test_func".to_string(),
            parameters: vec![
                ParameterInfo {
                    name: "args".to_string(),
                    kind: ParameterKind::VarPositional,
                    type_annotation: None,
                    default_value: None,
                },
                ParameterInfo {
                    name: "kwargs".to_string(),
                    kind: ParameterKind::VarKeyword,
                    type_annotation: None,
                    default_value: None,
                },
            ],
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
            range: SourceRange::default(),
        };

        let formatted = format_function(&sig);
        assert!(formatted.contains("*args"));
        assert!(formatted.contains("**kwargs"));
    }

    // ==================== format_class tests ====================

    #[test]
    fn test_format_simple_class() {
        let class_info = ClassInfo {
            name: "TestClass".to_string(),
            base_classes: vec![],
            docstring: Some("A test class".to_string()),
            init_signature: None,
            new_signature: None,
            all_bases_resolved: true,
            range: SourceRange::default(),
        };

        let formatted = format_class(&class_info);
        assert!(formatted.contains("class TestClass"));
        assert!(formatted.contains("A test class"));
        assert!(formatted.starts_with("```python"));
    }

    #[test]
    fn test_format_class_with_init() {
        let class_info = ClassInfo {
            name: "TestClass".to_string(),
            base_classes: vec![],
            docstring: Some("A test class".to_string()),
            range: SourceRange::default(),
            init_signature: Some(FunctionSignature {
                name: "__init__".to_string(),
                parameters: vec![
                    ParameterInfo {
                        name: "self".to_string(),
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
                range: SourceRange::default(),
            }),
            new_signature: None,
            all_bases_resolved: true,
        };

        let formatted = format_class(&class_info);
        assert!(formatted.contains("class TestClass:"));
        assert!(
            formatted.contains("def __init__(self, value: int):"),
            "Expected __init__ method in formatted output, got: \"{}\"",
            formatted
        );
        assert!(formatted.contains("A test class"));
    }

    #[test]
    fn test_format_class_with_defaults() {
        let class_info = ClassInfo {
            name: "TestClass".to_string(),
            base_classes: vec![],
            docstring: None,
            range: SourceRange::default(),
            init_signature: Some(FunctionSignature {
                name: "__init__".to_string(),
                parameters: vec![
                    ParameterInfo {
                        name: "self".to_string(),
                        kind: ParameterKind::PositionalOrKeyword,
                        type_annotation: None,
                        default_value: None,
                    },
                    ParameterInfo {
                        name: "name".to_string(),
                        kind: ParameterKind::PositionalOrKeyword,
                        type_annotation: Some("str".to_string()),
                        default_value: Some("'default'".to_string()),
                    },
                ],
                return_type: None,
                docstring: None,
                is_overloaded: false,
                overloads: Vec::new(),
                range: SourceRange::default(),
            }),
            new_signature: None,
            all_bases_resolved: true,
        };

        let formatted = format_class(&class_info);
        assert!(formatted.contains("name: str = 'default'"));
    }

    #[test]
    fn test_format_classmethod() {
        let method_info = MethodInfo {
            class_name: "MyClass".to_string(),
            method_name: "from_config".to_string(),
            signature: FunctionSignature {
                name: "from_config".to_string(),
                parameters: vec![
                    ParameterInfo {
                        name: "cls".to_string(),
                        kind: ParameterKind::PositionalOrKeyword,
                        type_annotation: None,
                        default_value: None,
                    },
                    ParameterInfo {
                        name: "config".to_string(),
                        kind: ParameterKind::PositionalOrKeyword,
                        type_annotation: Some("dict".to_string()),
                        default_value: None,
                    },
                ],
                return_type: Some("MyClass".to_string()),
                docstring: Some("Create from config".to_string()),
                is_overloaded: false,
                overloads: Vec::new(),
                range: SourceRange::default(),
            },
            is_classmethod: true,
            is_staticmethod: false,
        };

        let formatted = format_method(&method_info);
        assert!(formatted.contains("@classmethod"));
        assert!(formatted.contains("def from_config(cls, config: dict) -> MyClass"));
        assert!(formatted.contains("Create from config"));
    }

    #[test]
    fn test_format_staticmethod() {
        let method_info = MethodInfo {
            class_name: "MyClass".to_string(),
            method_name: "helper".to_string(),
            signature: FunctionSignature {
                name: "helper".to_string(),
                parameters: vec![ParameterInfo {
                    name: "value".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: Some("int".to_string()),
                    default_value: None,
                }],
                return_type: Some("int".to_string()),
                docstring: None,
                is_overloaded: false,
                overloads: Vec::new(),
                range: SourceRange::default(),
            },
            is_classmethod: false,
            is_staticmethod: true,
        };

        let formatted = format_method(&method_info);
        assert!(formatted.contains("@staticmethod"));
        assert!(formatted.contains("def helper(value: int) -> int"));
    }
}
