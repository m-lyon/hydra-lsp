//! Reading definitions out of one Python file: salsa-tracked reads and parses,
//! and the AST visitors that pull out a function, class, method or class
//! attribute by name.

use super::model::{
    ClassAttributeInfo, ClassInfo, FunctionSignature, MethodInfo, ParameterInfo, ParameterKind,
    SourcePosition, SourceRange,
};
use ruff_db::PythonFile;
use ruff_db::files::File;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::{SourceText, source_text};
use ruff_python_ast::{self as ast, Expr, PythonVersion, Stmt, visitor::Visitor};
use ruff_source_file::{LineIndex, PositionEncoding};
use ruff_text_size::TextRange;

/// Read a Python source file through ruff_db's salsa-tracked `source_text`.
///
/// File reads go through `source_text` (not `std::fs`) so that the lookup
/// participates in salsa's dependency graph — when the file's revision is
/// bumped (via `File::sync_path`), every memo that called this function for
/// that file is invalidated.
///
/// The returned `SourceText` is `Arc`-backed; callers should bind it to a
/// local and call `.as_str()` to get a borrowed `&str`. `None` when the file
/// does not exist or cannot be read.
pub(super) fn read_source(db: &dyn ruff_db::Db, file: File) -> Option<SourceText> {
    let source = source_text(db, file);
    source.read_error().is_none().then_some(source)
}

/// Run `find` over the parsed `file` and its source text, through the
/// salsa-tracked reads.
fn find_in_module<T>(
    db: &dyn ruff_db::Db,
    file: File,
    python_version: PythonVersion,
    find: impl FnOnce(&[Stmt], &str) -> Option<T>,
) -> Option<T> {
    let source = read_source(db, file)?;
    let parsed = parsed_python_module(db, file, python_version);
    find(parsed.suite(), source.as_str())
}

/// Parse a Python source file through ruff_db's salsa-tracked `parsed_module`.
///
/// Returns the salsa-cached `ParsedModuleRef` (LRU 200). Callers that also need
/// the raw source text should call `read_source` separately — both are cache hits
/// for the same revision.
pub(super) fn parsed_python_module(
    db: &dyn ruff_db::Db,
    file: File,
    python_version: PythonVersion,
) -> ParsedModuleRef {
    parsed_module(db, PythonFile::new(db, file, python_version)).load(db)
}

/// Extract function signature from source code via salsa-cached parsing.
pub(super) fn extract_function_signature(
    db: &dyn ruff_db::Db,
    file: File,
    function_name: &str,
    python_version: PythonVersion,
) -> Option<FunctionSignature> {
    find_in_module(db, file, python_version, |suite, source| {
        let mut visitor = FunctionExtractor {
            target_name: function_name,
            result: None,
            source,
        };
        visitor.visit_body(suite);
        visitor.result
    })
}

/// Whether `name` is bound at module scope in `file`.
pub(super) fn module_defines_top_level(
    db: &dyn ruff_db::Db,
    file: File,
    name: &str,
    python_version: PythonVersion,
) -> bool {
    fn body_binds(body: &[Stmt], name: &str) -> bool {
        body.iter().any(|stmt| match stmt {
            Stmt::FunctionDef(func_def) => {
                func_def.name.as_str() == name
                    && !has_type_check_only_decorator(&func_def.decorator_list)
            }
            Stmt::ClassDef(class_def) => {
                class_def.name.as_str() == name
                    && !has_type_check_only_decorator(&class_def.decorator_list)
            }
            Stmt::Assign(_) | Stmt::AnnAssign(_) => assigned_value(stmt, name).is_some(),
            Stmt::If(if_stmt) => {
                body_binds(&if_stmt.body, name)
                    || if_stmt
                        .elif_else_clauses
                        .iter()
                        .any(|clause| body_binds(&clause.body, name))
            }
            _ => false,
        })
    }

    body_binds(parsed_python_module(db, file, python_version).suite(), name)
}

/// Extract class information from source code via salsa-cached parsing.
pub(super) fn extract_class_info(
    db: &dyn ruff_db::Db,
    file: File,
    class_name: &str,
    python_version: PythonVersion,
) -> Option<ClassInfo> {
    find_in_module(db, file, python_version, |suite, source| {
        let mut visitor = ClassExtractor {
            target_name: class_name,
            result: None,
            source,
        };
        visitor.visit_body(suite);
        visitor.result
    })
}

/// Extract method information from source code via salsa-cached parsing.
pub(super) fn extract_method_info(
    db: &dyn ruff_db::Db,
    file: File,
    class_name: &str,
    method_name: &str,
    python_version: PythonVersion,
) -> Option<MethodInfo> {
    find_in_module(db, file, python_version, |suite, source| {
        let mut visitor = MethodExtractor {
            class_name,
            method_name,
            result: None,
            source,
        };
        visitor.visit_body(suite);
        visitor.result
    })
}

/// Extract class attribute from source code via salsa-cached parsing.
pub(super) fn extract_class_attribute(
    db: &dyn ruff_db::Db,
    file: File,
    class_name: &str,
    attribute_name: &str,
    python_version: PythonVersion,
) -> Option<ClassAttributeInfo> {
    let parsed = parsed_python_module(db, file, python_version);

    let mut visitor = ClassAttributeExtractor {
        class_name: class_name.to_string(),
        attribute_name: attribute_name.to_string(),
        result: None,
    };

    visitor.visit_body(parsed.suite());
    visitor.result
}

/// What `stmt` assigns to the bare name `name`, if it is an assignment to it:
/// `Some(None)` for an annotation with no value, such as `name: int`.
pub(super) fn assigned_value<'a>(stmt: &'a Stmt, name: &str) -> Option<Option<&'a Expr>> {
    let is_name = |target: &Expr| matches!(target, Expr::Name(n) if n.id.as_str() == name);
    match stmt {
        Stmt::Assign(assign) if assign.targets.iter().any(is_name) => Some(Some(&assign.value)),
        Stmt::AnnAssign(ann_assign) if is_name(&ann_assign.target) => {
            Some(ann_assign.value.as_deref())
        }
        _ => None,
    }
}

/// Visitor to extract function signatures from AST
struct FunctionExtractor<'s> {
    target_name: &'s str,
    result: Option<FunctionSignature>,
    source: &'s str,
}

impl<'a> Visitor<'a> for FunctionExtractor<'_> {
    /// Matching is done per *body* rather than per statement so that the
    /// sibling declarations are in scope: a name declared more than once, or
    /// decorated with `@overload`, is overloaded, and this is only discernible through
    /// the enclosing body.
    fn visit_body(&mut self, body: &'a [Stmt]) {
        if self.result.is_some() {
            return; // Already found
        }
        if body.iter().any(|stmt| {
            matches!(stmt, Stmt::FunctionDef(func_def) if func_def.name.as_str() == self.target_name)
        }) {
            self.result = extract_declared_signature(body, self.target_name, self.source)
                .map(|(_, signature)| signature);
            return;
        }
        for stmt in body {
            if self.result.is_some() {
                return; // Already found
            }
            self.visit_stmt(stmt);
        }
    }

    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.result.is_some() {
            return; // Already found
        }

        // Matching happens in `visit_body`; this only descends.
        ast::visitor::walk_stmt(self, stmt);
    }
}

/// Visitor to extract class information from AST
struct ClassExtractor<'s> {
    target_name: &'s str,
    source: &'s str,
    result: Option<ClassInfo>,
}

impl<'a> Visitor<'a> for ClassExtractor<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.result.is_some() {
            return; // Already found
        }

        if let Stmt::ClassDef(class_def) = stmt
            && class_def.name.as_str() == self.target_name
        {
            self.result = Some(extract_class_info_from_def(class_def, self.source));
            return;
        }

        // Continue walking
        ast::visitor::walk_stmt(self, stmt);
    }
}

/// Visitor to extract a specific method from a class
struct MethodExtractor<'s> {
    class_name: &'s str,
    method_name: &'s str,
    result: Option<MethodInfo>,
    source: &'s str,
}

/// Visitor to extract class attribute assignments from a class definition
struct ClassAttributeExtractor {
    class_name: String,
    attribute_name: String,
    result: Option<ClassAttributeInfo>,
}

impl<'a> Visitor<'a> for ClassAttributeExtractor {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.result.is_some() {
            return; // Already found
        }

        if let Stmt::ClassDef(class_def) = stmt
            && class_def.name.as_str() == self.class_name
        {
            // `attr = SomeClass` or `attr: Type = SomeClass` in the class body.
            for class_stmt in &class_def.body {
                if let Some(Some(value)) = assigned_value(class_stmt, &self.attribute_name) {
                    self.result = Some(ClassAttributeInfo {
                        name: self.attribute_name.clone(),
                        value: expr_to_string(value),
                    });
                    return;
                }
            }
        }

        // Continue walking to find nested classes
        ast::visitor::walk_stmt(self, stmt);
    }
}

impl<'a> Visitor<'a> for MethodExtractor<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.result.is_some() {
            return; // Already found
        }

        if let Stmt::ClassDef(class_def) = stmt
            && class_def.name.as_str() == self.class_name
            && let Some((declaration, signature)) =
                extract_declared_signature(&class_def.body, self.method_name, self.source)
        {
            // Decorators come from the declaration actually chosen.
            let (is_classmethod, is_staticmethod) =
                check_method_decorators(&declaration.decorator_list);

            self.result = Some(MethodInfo {
                class_name: self.class_name.to_string(),
                method_name: self.method_name.to_string(),
                signature,
                is_classmethod,
                is_staticmethod,
            });
            return;
        }

        // Continue walking to find nested classes
        ast::visitor::walk_stmt(self, stmt);
    }
}

/// The declaration of `name` in `body` that describes the callable actually
/// called, plus whether it is an overload set with no single signature.
fn resolve_declaration<'a>(
    body: &'a [Stmt],
    name: &str,
) -> Option<(&'a ast::StmtFunctionDef, bool)> {
    let declarations: Vec<&ast::StmtFunctionDef> = body
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(func_def) if func_def.name.as_str() == name => Some(func_def),
            _ => None,
        })
        .collect();

    let first = *declarations.first()?;
    if !declarations
        .iter()
        .any(|func_def| has_overload_decorator(&func_def.decorator_list))
    {
        return Some((first, false));
    }
    match declarations
        .iter()
        .find(|func_def| !has_overload_decorator(&func_def.decorator_list))
    {
        Some(implementation) => Some((implementation, false)),
        None => Some((first, true)),
    }
}

/// The declaration of `name` in `body` and its signature, resolved through
/// any `@overload` set.
fn extract_declared_signature<'a>(
    body: &'a [Stmt],
    name: &str,
    source: &str,
) -> Option<(&'a ast::StmtFunctionDef, FunctionSignature)> {
    let (func_def, is_overloaded) = resolve_declaration(body, name)?;
    let mut signature = extract_function_signature_from_def(func_def, source);
    signature.is_overloaded = is_overloaded;
    signature.overloads = overload_signatures(body, name, source);
    Some((func_def, signature))
}

/// The signature of every `@overload` declaration of `name` in `body`, in
/// source order.
fn overload_signatures(body: &[Stmt], name: &str, source: &str) -> Vec<FunctionSignature> {
    body.iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(func_def)
                if func_def.name.as_str() == name
                    && has_overload_decorator(&func_def.decorator_list) =>
            {
                Some(extract_function_signature_from_def(func_def, source))
            }
            _ => None,
        })
        .collect()
}

/// Check for `@overload`, however `typing.overload` was imported.
fn has_overload_decorator(decorators: &[ast::Decorator]) -> bool {
    has_decorator(decorators, "overload", None)
}

/// Check for `@type_check_only`, however `typing.type_check_only` was imported.
fn has_type_check_only_decorator(decorators: &[ast::Decorator]) -> bool {
    has_decorator(decorators, "type_check_only", None)
}

/// Match a decorator name, restricting the attribute form when a qualifier is given.
fn has_decorator(decorators: &[ast::Decorator], name: &str, qualifier: Option<&str>) -> bool {
    decorators.iter().any(|decorator| match &decorator.expression {
        Expr::Name(expression) => expression.id.as_str() == name,
        Expr::Attribute(attribute) => {
            attribute.attr.as_str() == name
                && qualifier.is_none_or(|qualifier| {
                    matches!(attribute.value.as_ref(), Expr::Name(module) if module.id.as_str() == qualifier)
                })
        }
        _ => false,
    })
}

/// Check if a function has @classmethod or @staticmethod decorators
fn check_method_decorators(decorators: &[ast::Decorator]) -> (bool, bool) {
    (
        has_decorator(decorators, "classmethod", Some("builtins")),
        has_decorator(decorators, "staticmethod", Some("builtins")),
    )
}

/// Convert a TextRange to line/column positions.
fn source_range(range: TextRange, source: &str) -> SourceRange {
    let line_index = LineIndex::from_source_text(source);

    // The server advertises no `positionEncoding`, so the LSP default of
    // UTF-16 applies to every position we send to the client. Emitting UTF-32
    // (codepoint) columns here would mis-position ranges by one per non-BMP
    // character (codepoints >= U+10000) earlier on the line.
    let position = |offset| {
        let location = line_index.source_location(offset, source, PositionEncoding::Utf16);
        SourcePosition {
            line: location.line.to_zero_indexed() as u32,
            column: location.character_offset.to_zero_indexed() as u32,
        }
    };
    SourceRange {
        start: position(range.start()),
        end: position(range.end()),
    }
}

/// Extract function signature from a function definition node
fn extract_function_signature_from_def(
    func_def: &ast::StmtFunctionDef,
    source: &str,
) -> FunctionSignature {
    let parameters = extract_parameters(&func_def.parameters);
    let return_type = func_def.returns.as_ref().map(|e| expr_to_string(e));
    let docstring = extract_docstring(&func_def.body);

    FunctionSignature {
        name: func_def.name.to_string(),
        parameters,
        return_type,
        docstring,
        is_overloaded: false,
        overloads: Vec::new(),
        range: source_range(func_def.range, source),
    }
}

/// Extract class info from a class definition node
fn extract_class_info_from_def(class_def: &ast::StmtClassDef, source: &str) -> ClassInfo {
    let docstring = extract_docstring(&class_def.body);

    // Extract base classes
    let base_classes: Vec<String> = class_def.bases().iter().map(expr_to_string).collect();

    let init_signature = extract_declared_signature(&class_def.body, "__init__", source)
        .map(|(_, signature)| signature);
    let new_signature = extract_declared_signature(&class_def.body, "__new__", source)
        .map(|(_, signature)| signature);

    ClassInfo {
        name: class_def.name.to_string(),
        base_classes,
        docstring,
        init_signature,
        new_signature,
        all_bases_resolved: true,
        range: source_range(class_def.range, source),
    }
}

/// Extract parameters from function parameters, in declaration order.
fn extract_parameters(params: &ast::Parameters) -> Vec<ParameterInfo> {
    fn info(param: &ast::Parameter, default: Option<&Expr>, kind: ParameterKind) -> ParameterInfo {
        ParameterInfo {
            name: param.name.to_string(),
            kind,
            type_annotation: param.annotation.as_deref().map(expr_to_string),
            default_value: default.map(expr_to_string),
        }
    }
    let with_default =
        |kind| move |p: &ast::ParameterWithDefault| info(&p.parameter, p.default.as_deref(), kind);

    let mut result = Vec::new();
    result.extend(
        params
            .posonlyargs
            .iter()
            .map(with_default(ParameterKind::PositionalOnly)),
    );
    result.extend(
        params
            .args
            .iter()
            .map(with_default(ParameterKind::PositionalOrKeyword)),
    );
    if let Some(vararg) = &params.vararg {
        result.push(info(vararg, None, ParameterKind::VarPositional));
    }
    result.extend(
        params
            .kwonlyargs
            .iter()
            .map(with_default(ParameterKind::KeywordOnly)),
    );
    if let Some(kwarg) = &params.kwarg {
        result.push(info(kwarg, None, ParameterKind::VarKeyword));
    }
    result
}

/// Extract docstring from function or class body
fn extract_docstring(body: &[Stmt]) -> Option<String> {
    if let Some(Stmt::Expr(expr_stmt)) = body.first()
        && let Expr::StringLiteral(string_literal) = expr_stmt.value.as_ref()
    {
        return Some(string_literal.value.to_string());
    }
    None
}

/// Convert an expression to a string representation
fn expr_to_string(expr: &Expr) -> String {
    match expr {
        Expr::Name(name) => name.id.to_string(),
        Expr::Attribute(attr) => {
            format!("{}.{}", expr_to_string(&attr.value), attr.attr)
        }
        Expr::Subscript(subscript) => {
            format!(
                "{}[{}]",
                expr_to_string(&subscript.value),
                expr_to_string(&subscript.slice)
            )
        }
        Expr::Tuple(tuple) => {
            let elements: Vec<String> = tuple.elts.iter().map(expr_to_string).collect();
            format!("({})", elements.join(", "))
        }
        Expr::List(list) => {
            let elements: Vec<String> = list.elts.iter().map(expr_to_string).collect();
            format!("[{}]", elements.join(", "))
        }
        Expr::StringLiteral(s) => format!("'{}'", s.value),
        Expr::NumberLiteral(n) => match &n.value {
            ast::Number::Int(i) => i.to_string(),
            ast::Number::Float(f) => f.to_string(),
            ast::Number::Complex { real, imag } => format!("{}+{}j", real, imag),
        },
        Expr::BooleanLiteral(b) => {
            if b.value {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Expr::NoneLiteral(_) => "None".to_string(),
        Expr::BinOp(binop) => {
            format!(
                "{} | {}",
                expr_to_string(&binop.left),
                expr_to_string(&binop.right)
            )
        }
        _ => "...".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::python::model::DefinitionInfo;
    use crate::python::test_support::{
        extract_definition_info, get_simple_test_dir, system_file, test_db, write_file,
    };
    use std::path::PathBuf;

    // ==================== extract_function_signature tests ====================

    #[test]
    fn test_extract_simple_function() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let db = test_db();

        let sig = extract_function_signature(
            &db,
            system_file(&db, &test_file),
            "simple_function",
            PythonVersion::latest_ty(),
        )
        .unwrap();
        assert_eq!(sig.name, "simple_function");
        assert_eq!(sig.parameters.len(), 0);
        assert!(sig.docstring.is_some());
        assert!(sig.docstring.as_ref().unwrap().contains("simple function"));
    }

    #[test]
    fn test_extract_function_with_params() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let db = test_db();

        let sig = extract_function_signature(
            &db,
            system_file(&db, &test_file),
            "function_with_params",
            PythonVersion::latest_ty(),
        )
        .unwrap();
        assert_eq!(sig.name, "function_with_params");
        assert_eq!(sig.parameters.len(), 3);

        // Check first parameter (no type annotation)
        assert_eq!(sig.parameters[0].name, "arg1");
        assert!(sig.parameters[0].type_annotation.is_none());
        assert!(!sig.parameters[0].has_default());

        // Check second parameter (with type)
        assert_eq!(sig.parameters[1].name, "arg2");
        assert_eq!(sig.parameters[1].type_annotation.as_ref().unwrap(), "int");
        assert!(!sig.parameters[1].has_default());

        // Check third parameter (with type and default)
        assert_eq!(sig.parameters[2].name, "arg3");
        assert_eq!(sig.parameters[2].type_annotation.as_ref().unwrap(), "str");
        assert!(sig.parameters[2].has_default());
        assert_eq!(
            sig.parameters[2].default_value.as_ref().unwrap(),
            "'default'"
        );
    }

    #[test]
    fn test_extract_function_with_return() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let db = test_db();

        let sig = extract_function_signature(
            &db,
            system_file(&db, &test_file),
            "function_with_return",
            PythonVersion::latest_ty(),
        )
        .unwrap();
        assert_eq!(sig.name, "function_with_return");
        assert!(sig.return_type.is_some());
        assert_eq!(sig.return_type.as_ref().unwrap(), "int");
    }

    #[test]
    fn test_extract_variadic_function() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let db = test_db();

        let sig = extract_function_signature(
            &db,
            system_file(&db, &test_file),
            "variadic_function",
            PythonVersion::latest_ty(),
        )
        .unwrap();
        assert_eq!(sig.name, "variadic_function");
        assert_eq!(sig.parameters.len(), 2);

        // Check *args
        assert_eq!(sig.parameters[0].name, "args");
        assert_eq!(sig.parameters[0].kind, ParameterKind::VarPositional);

        // Check **kwargs
        assert_eq!(sig.parameters[1].name, "kwargs");
        assert_eq!(sig.parameters[1].kind, ParameterKind::VarKeyword);
    }

    #[test]
    fn test_extract_complex_function() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let db = test_db();

        let sig = extract_function_signature(
            &db,
            system_file(&db, &test_file),
            "complex_function",
            PythonVersion::latest_ty(),
        )
        .unwrap();
        assert_eq!(sig.name, "complex_function");

        // Should have: pos_only, regular, *args, keyword_only, another_kw, **kwargs
        assert_eq!(sig.parameters.len(), 6);

        // Check keyword-only parameter
        let kw_only = sig
            .parameters
            .iter()
            .find(|p| p.name == "keyword_only")
            .unwrap();
        assert_eq!(kw_only.kind, ParameterKind::KeywordOnly);

        // Check return type
        assert!(sig.return_type.is_some());
    }

    #[test]
    fn test_extract_nonexistent_function() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let db = test_db();

        let result = extract_function_signature(
            &db,
            system_file(&db, &test_file),
            "nonexistent",
            PythonVersion::latest_ty(),
        );
        assert!(result.is_none());
    }

    // ==================== extract_class_info tests ====================

    #[test]
    fn test_extract_simple_class() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let db = test_db();

        let class_info = extract_class_info(
            &db,
            system_file(&db, &test_file),
            "SimpleClass",
            PythonVersion::latest_ty(),
        )
        .unwrap();
        assert_eq!(class_info.name, "SimpleClass");
        assert!(class_info.docstring.is_some());
        assert!(class_info.init_signature.is_none());
    }

    #[test]
    fn test_extract_class_with_init() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let db = test_db();

        let class_info = extract_class_info(
            &db,
            system_file(&db, &test_file),
            "ClassWithInit",
            PythonVersion::latest_ty(),
        )
        .unwrap();
        assert_eq!(class_info.name, "ClassWithInit");
        assert!(class_info.docstring.is_some());
        assert!(class_info.init_signature.is_some());

        let init_sig = class_info.init_signature.as_ref().unwrap();
        assert_eq!(init_sig.name, "__init__");
        assert_eq!(init_sig.parameters.len(), 3); // self, name, value

        // Check self parameter
        assert_eq!(init_sig.parameters[0].name, "self");

        // Check name parameter
        assert_eq!(init_sig.parameters[1].name, "name");
        assert_eq!(
            init_sig.parameters[1].type_annotation.as_ref().unwrap(),
            "str"
        );

        // Check value parameter with default
        assert_eq!(init_sig.parameters[2].name, "value");
        assert_eq!(
            init_sig.parameters[2].type_annotation.as_ref().unwrap(),
            "int"
        );
        assert!(init_sig.parameters[2].has_default());
    }

    #[test]
    fn test_extract_complex_class() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let db = test_db();

        let class_info = extract_class_info(
            &db,
            system_file(&db, &test_file),
            "ComplexClass",
            PythonVersion::latest_ty(),
        )
        .unwrap();
        assert_eq!(class_info.name, "ComplexClass");
        assert!(class_info.init_signature.is_some());

        let init_sig = class_info.init_signature.as_ref().unwrap();
        // Should have: self, *args, **kwargs
        assert_eq!(init_sig.parameters.len(), 3);
    }

    #[test]
    fn test_extract_nonexistent_class() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let db = test_db();

        let result = extract_class_info(
            &db,
            system_file(&db, &test_file),
            "NonexistentClass",
            PythonVersion::latest_ty(),
        );
        assert!(result.is_none());
    }

    // ==================== extract_method_info tests ====================

    #[test]
    fn test_extract_qualified_method_decorators() {
        let path = get_simple_test_dir().join("class_methods.py");
        let db = test_db();

        let kinds = ["make", "from_value", "custom"].map(|name| {
            let method = extract_method_info(
                &db,
                system_file(&db, &path),
                "QualifiedFactory",
                name,
                PythonVersion::latest_ty(),
            )
            .unwrap();
            (method.is_classmethod, method.is_staticmethod)
        });
        assert_eq!(kinds, [(false, true), (true, false), (false, false)]);
    }

    // ==================== source range tests ====================

    /// Write `source` to `name` in a new temporary directory.
    fn write_module(name: &str, source: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(name);
        write_file(&path, source);
        (dir, path)
    }

    fn function_range(source: &str, name: &str) -> SourceRange {
        let (_dir, path) = write_module("module.py", source);
        let db = test_db();
        extract_function_signature(
            &db,
            system_file(&db, &path),
            name,
            PythonVersion::latest_ty(),
        )
        .unwrap()
        .range
    }

    fn position(line: u32, column: u32) -> SourcePosition {
        SourcePosition { line, column }
    }

    #[test]
    fn test_function_line_number_conversion() {
        let source = r#"# This is line 0
# This is line 1

def my_function(x: int, y: int) -> int:
    """A test function."""
    return x + y

class MyClass:
    """A test class."""
    
    def __init__(self, value: int):
        """Initialize the class."""
        self.value = value
"#;
        let range = function_range(source, "my_function");
        assert_eq!(range.start, position(3, 0));
        assert!(range.end.line >= range.start.line);
    }

    #[test]
    fn test_class_line_number_conversion() {
        let source = r#"# This is line 0
# This is line 1
# This is line 2
# This is line 3
# This is line 4

class MyClass:
    """A test class on line 6."""

    def __init__(self, value: int):
        """Initialize the class."""
        self.value = value

    def method(self):
        pass
"#;
        let (_dir, path) = write_module("module.py", source);
        let db = test_db();
        let class_info = extract_class_info(
            &db,
            system_file(&db, &path),
            "MyClass",
            PythonVersion::latest_ty(),
        )
        .unwrap();

        assert_eq!(class_info.range.start, position(6, 0));
        let init_sig = class_info.init_signature.expect("__init__ should be found");
        assert_eq!(init_sig.range.start.line, 9);
    }

    #[test]
    fn test_indented_function() {
        let source = r#"class Container:
    def nested_function(self, arg: str) -> str:
        """A nested function."""
        return arg.upper()
"#;
        assert_eq!(
            function_range(source, "nested_function").start,
            position(1, 4)
        );
    }

    #[test]
    fn test_multiline_function() {
        let source = r#"def multiline_function(
    param1: int,
    param2: str,
    param3: bool
) -> tuple[int, str, bool]:
    """A function with multiple lines."""
    return (param1, param2, param3)
"#;
        let range = function_range(source, "multiline_function");
        assert_eq!(range.start, position(0, 0));
        assert_eq!(range.end.line, 6);
    }

    /// Long comment lines make byte offsets and line numbers differ a lot.
    #[test]
    fn test_byte_offset_not_line_number() {
        let source = r#"# Line 0: This is a comment with some content to increase byte offset
# Line 1: More comments to push the byte offset higher
# Line 2: Even more comments
# Line 3: Keep adding comments
# Line 4: One more comment line

def target_function() -> None:
    """This function is at line 6."""
    pass
"#;
        assert_eq!(function_range(source, "target_function").start.line, 6);
    }

    #[test]
    fn test_utf8_multibyte_characters() {
        let source = r#"# 中文注释
# Another comment: 日本語

def unicode_function(name: str) -> str:
    """Function with unicode: émojis 🚀 こんにちは"""
    return f"Hello, {name}!"
"#;
        assert_eq!(
            function_range(source, "unicode_function").start,
            position(3, 0)
        );
    }

    #[test]
    fn test_extract_definition_info_function_range() {
        let source = r#"# Line 0
# Line 1

def test_function(x: int) -> int:
    """Test function at line 3."""
    return x * 2
"#;
        let dir = tempfile::TempDir::new().unwrap();
        write_file(&dir.path().join("workspace/test_module.py"), source);

        let definition = extract_definition_info(
            &test_db(),
            "workspace.test_module.test_function",
            &[dir.path().to_path_buf()],
        )
        .unwrap()
        .definition_info;
        match definition {
            DefinitionInfo::Function(sig) => {
                assert_eq!(sig.name, "test_function");
                assert_eq!(sig.range.start.line, 3);
            }
            other => panic!("expected a function, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_definition_info_class_range() {
        let source = r#"# Line 0
# Line 1
# Line 2

class TestClass:
    """Test class at line 4."""
    
    def __init__(self):
        """Initialize at line 7."""
        pass
"#;
        let dir = tempfile::TempDir::new().unwrap();
        write_file(&dir.path().join("workspace/test_class_module.py"), source);

        let definition = extract_definition_info(
            &test_db(),
            "workspace.test_class_module.TestClass",
            &[dir.path().to_path_buf()],
        )
        .unwrap()
        .definition_info;
        match definition {
            DefinitionInfo::Class(class_info) => {
                assert_eq!(class_info.name, "TestClass");
                assert_eq!(class_info.range.start.line, 4);
                assert_eq!(class_info.init_signature.unwrap().range.start.line, 7);
            }
            other => panic!("expected a class, got {other:?}"),
        }
    }
}
