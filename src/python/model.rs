//! What the Python analysis produces: signatures, classes, methods, and why a
//! `_target_` could not be resolved.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionSignature {
    pub name: String,
    pub parameters: Vec<ParameterInfo>,
    pub return_type: Option<String>,
    pub docstring: Option<String>,
    /// Set when the symbol is `@overload`ed - `parameters` then describes only the
    /// first declaration, so it is not a sound basis for argument diagnostics.
    pub is_overloaded: bool,
    /// Every `@overload` declaration, in source order; empty if not overloaded.
    pub overloads: Vec<FunctionSignature>,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterKind {
    /// Declared before `/`.
    PositionalOnly,
    /// An ordinary parameter, passable either way.
    PositionalOrKeyword,
    /// `*args`.
    VarPositional,
    /// Declared after `*` or `*args`.
    KeywordOnly,
    /// `**kwargs`.
    VarKeyword,
}

impl ParameterKind {
    /// `*args` or `**kwargs`.
    pub fn is_variadic(self) -> bool {
        matches!(
            self,
            ParameterKind::VarPositional | ParameterKind::VarKeyword
        )
    }

    /// A single named parameter that a positional argument can fill:
    /// positional-only or positional-or-keyword. `*args` is excluded; it only
    /// takes the overflow.
    pub fn binds_positionally(self) -> bool {
        matches!(
            self,
            ParameterKind::PositionalOnly | ParameterKind::PositionalOrKeyword
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterInfo {
    pub name: String,
    pub kind: ParameterKind,
    pub type_annotation: Option<String>,
    pub default_value: Option<String>,
}

/// How [`ParameterInfo::label`] shows a parameter's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultStyle {
    /// No default, as in signature-help labels (`x: int`).
    Omit,
    /// The default's source text, as in hover (`x: int = 3`).
    Value,
    /// A placeholder, as in the CLI's brief signatures (`x: int = ...`).
    Ellipsis,
}

impl ParameterInfo {
    pub fn has_default(&self) -> bool {
        self.default_value.is_some()
    }

    pub fn is_required(&self) -> bool {
        !self.has_default() && !self.is_variadic()
    }

    /// See [`ParameterKind::is_variadic`].
    pub fn is_variadic(&self) -> bool {
        self.kind.is_variadic()
    }

    /// See [`ParameterKind::binds_positionally`].
    pub fn binds_positionally(&self) -> bool {
        self.kind.binds_positionally()
    }

    /// The parameter as it reads in a signature: the `*`/`**` prefix, the
    /// name, the annotation, then the default as `defaults` asks.
    pub fn label(&self, defaults: DefaultStyle) -> String {
        let mut s = String::new();
        match self.kind {
            ParameterKind::VarPositional => s.push('*'),
            ParameterKind::VarKeyword => s.push_str("**"),
            _ => {}
        }
        s.push_str(&self.name);
        if let Some(type_ann) = &self.type_annotation {
            s.push_str(": ");
            s.push_str(type_ann);
        }
        match defaults {
            DefaultStyle::Omit => {}
            DefaultStyle::Value => {
                if let Some(default) = &self.default_value {
                    s.push_str(" = ");
                    s.push_str(default);
                }
            }
            DefaultStyle::Ellipsis => {
                if self.has_default() {
                    s.push_str(" = ...");
                }
            }
        }
        s
    }
}

#[derive(Debug, Clone)]
pub struct ClassInfo {
    pub name: String,
    pub base_classes: Vec<String>,
    pub docstring: Option<String>,
    /// The signature Hydra calls when instantiating the class.
    pub init_signature: Option<FunctionSignature>,
    /// The class's own `__new__`, when it declares one.
    pub new_signature: Option<FunctionSignature>,
    pub unresolved_base_classes: bool,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

impl ClassInfo {
    /// Whether `init_signature` might not be the signature Hydra actually calls.
    ///
    /// True only when `__new__` stood in for a missing `__init__` *and* part of
    /// the MRO could not be resolved.
    pub fn constructor_is_uncertain(&self) -> bool {
        self.unresolved_base_classes
            && self
                .init_signature
                .as_ref()
                .is_some_and(|sig| sig.name == "__new__")
    }
}

/// Represents a method within a class
#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub class_name: String,
    pub method_name: String,
    pub signature: FunctionSignature,
    pub is_classmethod: bool,
    pub is_staticmethod: bool,
}

#[derive(Debug, Clone)]
pub enum DefinitionInfo {
    Function(FunctionSignature),
    Class(ClassInfo),
    Method(MethodInfo),
}

/// Why a `_target_` string could not be resolved to a Python definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// The target has no `.` separating a module from a symbol. Holds the
    /// target as written.
    InvalidFormat(String),
    /// No search path contains the target's module. Holds the module path.
    UnresolvedModule(String),
    /// Anything else, e.g. the module resolved but the symbol was not found.
    /// Holds the full message.
    Other(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidFormat(target) => write!(f, "Invalid _target_ format: '{target}'"),
            Self::UnresolvedModule(module) => write!(f, "Could not resolve module: '{module}'"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ResolveError {}

impl DefinitionInfo {
    /// Get the source location (start_line, start_col, end_line, end_col)
    pub fn position(&self) -> (u32, u32, u32, u32) {
        match self {
            DefinitionInfo::Function(sig) => (
                sig.start_line,
                sig.start_column,
                sig.end_line,
                sig.end_column,
            ),
            DefinitionInfo::Class(info) => (
                info.start_line,
                info.start_column,
                info.end_line,
                info.end_column,
            ),
            DefinitionInfo::Method(info) => (
                info.signature.start_line,
                info.signature.start_column,
                info.signature.end_line,
                info.signature.end_column,
            ),
        }
    }

    /// Return the name of the implicit first parameter (e.g. `self` or `cls`) that
    /// should be excluded from user-facing parameter lists, or `None` when no
    /// implicit parameter exists.
    ///
    /// Rather than hard-coding `"self"` and `"cls"`, this inspects the method
    /// kind and returns the *actual* name of the first parameter:
    ///
    /// * Regular instance methods / `__init__` → first parameter (conventionally `self`)
    /// * `@classmethod` → first parameter (conventionally `cls`)
    /// * `@staticmethod` / plain functions → `None`
    pub fn implicit_param(&self) -> Option<&str> {
        match self {
            DefinitionInfo::Function(_) => None,
            DefinitionInfo::Class(class_info) => class_info
                .init_signature
                .as_ref()
                .and_then(|sig| sig.parameters.first())
                .map(|p| p.name.as_str()),
            DefinitionInfo::Method(method_info) => {
                if method_info.is_staticmethod {
                    None
                } else {
                    // Both instance methods and classmethods have an implicit first param
                    method_info
                        .signature
                        .parameters
                        .first()
                        .map(|p| p.name.as_str())
                }
            }
        }
    }

    /// The signature Hydra calls for this target: the function or method
    /// itself, or a class's constructor.
    pub fn call_signature(&self) -> Option<&FunctionSignature> {
        match self {
            DefinitionInfo::Function(sig) => Some(sig),
            DefinitionInfo::Class(class_info) => class_info.init_signature.as_ref(),
            DefinitionInfo::Method(method_info) => Some(&method_info.signature),
        }
    }

    /// Mutable access to [`DefinitionInfo::call_signature`].
    pub fn call_signature_mut(&mut self) -> Option<&mut FunctionSignature> {
        match self {
            DefinitionInfo::Function(sig) => Some(sig),
            DefinitionInfo::Class(class_info) => class_info.init_signature.as_mut(),
            DefinitionInfo::Method(method_info) => Some(&mut method_info.signature),
        }
    }
}

/// Represents a class attribute assignment (e.g., `nested_class = ModelFactory`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassAttributeInfo {
    pub name: String,
    /// The value as a string (e.g., "ModelFactory", "SomeModule.SomeClass")
    pub value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== ParameterInfo::label tests ====================

    fn label_param(name: &str) -> ParameterInfo {
        ParameterInfo {
            name: name.to_string(),
            kind: ParameterKind::PositionalOrKeyword,
            type_annotation: None,
            default_value: None,
        }
    }

    #[test]
    fn test_label_plain_and_annotated() {
        let plain = label_param("x");
        let annotated = ParameterInfo {
            type_annotation: Some("int".to_string()),
            ..label_param("x")
        };
        for style in [
            DefaultStyle::Omit,
            DefaultStyle::Value,
            DefaultStyle::Ellipsis,
        ] {
            assert_eq!(plain.label(style), "x");
            assert_eq!(annotated.label(style), "x: int");
        }
    }

    #[test]
    fn test_label_default_styles() {
        let p = ParameterInfo {
            type_annotation: Some("int".to_string()),
            default_value: Some("3".to_string()),
            ..label_param("x")
        };
        assert_eq!(p.label(DefaultStyle::Omit), "x: int");
        assert_eq!(p.label(DefaultStyle::Value), "x: int = 3");
        assert_eq!(p.label(DefaultStyle::Ellipsis), "x: int = ...");
    }

    #[test]
    fn test_label_variadic_prefixes() {
        let args = ParameterInfo {
            kind: ParameterKind::VarPositional,
            ..label_param("args")
        };
        let kwargs = ParameterInfo {
            kind: ParameterKind::VarKeyword,
            type_annotation: Some("Any".to_string()),
            ..label_param("kwargs")
        };
        for style in [
            DefaultStyle::Omit,
            DefaultStyle::Value,
            DefaultStyle::Ellipsis,
        ] {
            assert_eq!(args.label(style), "*args");
            assert_eq!(kwargs.label(style), "**kwargs: Any");
        }
    }

    #[test]
    fn test_label_keyword_and_positional_only_have_no_marker() {
        // The `/` and `*` separators are `render_params`' job, not the label's.
        let kw = ParameterInfo {
            kind: ParameterKind::KeywordOnly,
            ..label_param("k")
        };
        let pos = ParameterInfo {
            kind: ParameterKind::PositionalOnly,
            ..label_param("p")
        };
        assert_eq!(kw.label(DefaultStyle::Value), "k");
        assert_eq!(pos.label(DefaultStyle::Value), "p");
    }

    // ==================== ResolveError tests ====================

    // Diagnostic messages, CLI output and snapshots depend on this text, so
    // each variant is checked against the format string it replaced.

    #[test]
    fn test_resolve_error_display_invalid_format() {
        let target = "InvalidTarget";
        assert_eq!(
            ResolveError::InvalidFormat(target.to_string()).to_string(),
            format!("Invalid _target_ format: '{}'", target)
        );
    }

    #[test]
    fn test_resolve_error_display_unresolved_module() {
        let module = "no_such.module";
        assert_eq!(
            ResolveError::UnresolvedModule(module.to_string()).to_string(),
            format!("Could not resolve module: '{}'", module)
        );
    }

    #[test]
    fn test_resolve_error_display_other() {
        let message = format!(
            "Symbol '{}' not found in module '{}'",
            "Missing", "my_module"
        );
        assert_eq!(ResolveError::Other(message.clone()).to_string(), message);
    }

    // ==================== implicit_param tests ====================

    #[test]
    fn test_implicit_param_function() {
        let def = DefinitionInfo::Function(FunctionSignature {
            name: "my_func".to_string(),
            parameters: vec![ParameterInfo {
                name: "x".to_string(),
                kind: ParameterKind::PositionalOrKeyword,
                type_annotation: None,
                default_value: None,
            }],
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
            start_line: 0,
            start_column: 0,
            end_line: 0,
            end_column: 0,
        });
        assert_eq!(def.implicit_param(), None);
    }

    #[test]
    fn test_implicit_param_class_init() {
        let def = DefinitionInfo::Class(ClassInfo {
            name: "MyClass".to_string(),
            base_classes: vec![],
            docstring: None,
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
                        type_annotation: None,
                        default_value: None,
                    },
                ],
                return_type: None,
                docstring: None,
                is_overloaded: false,
                overloads: Vec::new(),
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 0,
            }),
            new_signature: None,
            unresolved_base_classes: false,
            start_line: 0,
            start_column: 0,
            end_line: 0,
            end_column: 0,
        });
        assert_eq!(def.implicit_param(), Some("self"));
    }

    #[test]
    fn test_implicit_param_class_init_non_conventional() {
        let def = DefinitionInfo::Class(ClassInfo {
            name: "MyClass".to_string(),
            base_classes: vec![],
            docstring: None,
            init_signature: Some(FunctionSignature {
                name: "__init__".to_string(),
                parameters: vec![ParameterInfo {
                    name: "this".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: None,
                    default_value: None,
                }],
                return_type: None,
                docstring: None,
                is_overloaded: false,
                overloads: Vec::new(),
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 0,
            }),
            new_signature: None,
            unresolved_base_classes: false,
            start_line: 0,
            start_column: 0,
            end_line: 0,
            end_column: 0,
        });
        assert_eq!(def.implicit_param(), Some("this"));
    }

    #[test]
    fn test_implicit_param_class_no_init() {
        let def = DefinitionInfo::Class(ClassInfo {
            name: "MyClass".to_string(),
            base_classes: vec![],
            docstring: None,
            init_signature: None,
            new_signature: None,
            unresolved_base_classes: false,
            start_line: 0,
            start_column: 0,
            end_line: 0,
            end_column: 0,
        });
        assert_eq!(def.implicit_param(), None);
    }

    #[test]
    fn test_implicit_param_instance_method() {
        let def = DefinitionInfo::Method(MethodInfo {
            class_name: "MyClass".to_string(),
            method_name: "do_thing".to_string(),
            signature: FunctionSignature {
                name: "do_thing".to_string(),
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
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 0,
            },
            is_classmethod: false,
            is_staticmethod: false,
        });
        assert_eq!(def.implicit_param(), Some("self"));
    }

    #[test]
    fn test_implicit_param_classmethod() {
        let def = DefinitionInfo::Method(MethodInfo {
            class_name: "MyClass".to_string(),
            method_name: "from_config".to_string(),
            signature: FunctionSignature {
                name: "from_config".to_string(),
                parameters: vec![ParameterInfo {
                    name: "cls".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: None,
                    default_value: None,
                }],
                return_type: None,
                docstring: None,
                is_overloaded: false,
                overloads: Vec::new(),
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 0,
            },
            is_classmethod: true,
            is_staticmethod: false,
        });
        assert_eq!(def.implicit_param(), Some("cls"));
    }

    #[test]
    fn test_implicit_param_classmethod_non_conventional() {
        let def = DefinitionInfo::Method(MethodInfo {
            class_name: "MyClass".to_string(),
            method_name: "from_config".to_string(),
            signature: FunctionSignature {
                name: "from_config".to_string(),
                parameters: vec![ParameterInfo {
                    name: "klass".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: None,
                    default_value: None,
                }],
                return_type: None,
                docstring: None,
                is_overloaded: false,
                overloads: Vec::new(),
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 0,
            },
            is_classmethod: true,
            is_staticmethod: false,
        });
        assert_eq!(def.implicit_param(), Some("klass"));
    }

    #[test]
    fn test_implicit_param_staticmethod() {
        let def = DefinitionInfo::Method(MethodInfo {
            class_name: "MyClass".to_string(),
            method_name: "helper".to_string(),
            signature: FunctionSignature {
                name: "helper".to_string(),
                parameters: vec![ParameterInfo {
                    name: "value".to_string(),
                    kind: ParameterKind::PositionalOrKeyword,
                    type_annotation: None,
                    default_value: None,
                }],
                return_type: None,
                docstring: None,
                is_overloaded: false,
                overloads: Vec::new(),
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 0,
            },
            is_classmethod: false,
            is_staticmethod: true,
        });
        assert_eq!(def.implicit_param(), None);
    }
}
