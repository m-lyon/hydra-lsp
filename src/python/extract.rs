//! Reading definitions out of one Python file: salsa-tracked reads and parses,
//! and the AST visitors that pull out a function, class, method or class
//! attribute by name.

use super::model::{
    ClassAttributeInfo, ClassInfo, FunctionSignature, MethodInfo, ParameterInfo, ParameterKind,
};
use super::typeshed::{is_vendored_path, to_vendored_path};
use anyhow::{Context, Result};
use ruff_db::PythonFile;
use ruff_db::files::{File, system_path_to_file, vendored_path_to_file};
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::{SourceText, source_text};
use ruff_db::system::SystemPath;
use ruff_python_ast::{self as ast, Expr, PythonVersion, Stmt, visitor::Visitor};
use ruff_source_file::{LineIndex, PositionEncoding};
use ruff_text_size::TextRange;
use std::path::{Path, PathBuf};

/// Intern `path` as a salsa `File`, routing sentinel paths to the vendored
/// typeshed archive and everything else to the host filesystem.
fn path_to_file(db: &dyn ruff_db::Db, path: &Path) -> Option<File> {
    if is_vendored_path(path) {
        return vendored_path_to_file(db, to_vendored_path(path)?).ok();
    }
    system_path_to_file(db, SystemPath::from_std_path(path)?).ok()
}

/// Tracked existence probe for a regular file.
///
/// Returns `true` when `path` is an existing regular file. Unlike
/// `Path::exists`, the check goes through salsa's `system_path_to_file`, which
/// interns `path` as a `File` *even when it does not exist* and records a
/// dependency on that file's existence status. When the file is later created
/// or deleted and the backend calls `File::sync_path`, every query that probed
/// the path is invalidated on the next request.
///
/// Vendored typeshed paths are probed against the immutable in-binary archive
/// instead.
///
/// Falls back to an untracked `Path::exists()` only for non-UTF-8 paths, which
/// cannot be represented as a `SystemPath`.
pub(super) fn path_is_file(db: &dyn ruff_db::Db, path: &Path) -> bool {
    if is_vendored_path(path) {
        return path_to_file(db, path).is_some();
    }
    match SystemPath::from_std_path(path) {
        Some(sys_path) => system_path_to_file(db, sys_path).is_ok(),
        None => path.exists(),
    }
}

/// Read a Python source file through ruff_db's salsa-tracked `source_text`.
///
/// File reads go through `source_text` (not `std::fs`) so that the lookup
/// participates in salsa's dependency graph — when the file's revision is
/// bumped (via `File::sync_path`), every memo that called this function for
/// that path is invalidated.
///
/// The returned `SourceText` is `Arc`-backed; callers should bind it to a
/// local and call `.as_str()` to get a borrowed `&str`.
pub(super) fn read_source(db: &dyn ruff_db::Db, path: &Path) -> Result<SourceText> {
    let file = path_to_file(db, path)
        .with_context(|| format!("could not resolve file: {}", path.display()))?;
    let source = source_text(db, file);
    if let Some(err) = source.read_error() {
        anyhow::bail!("failed to read {}: {}", path.display(), err);
    }
    Ok(source)
}

/// Parse a Python source file through ruff_db's salsa-tracked `parsed_module`.
///
/// Returns the salsa-cached `ParsedModuleRef` (LRU 200). Callers that also need
/// the raw source text should call `read_source` separately — both are cache hits
/// for the same revision.
pub(super) fn get_parsed_module(db: &dyn ruff_db::Db, path: &Path) -> Result<ParsedModuleRef> {
    let file = path_to_file(db, path)
        .with_context(|| format!("could not resolve file: {}", path.display()))?;
    // The interpreter's version is not plumbed through yet, so every file is
    // parsed as the latest dialect. It only affects which syntax the parser
    // reports as unsupported, which nothing here reads.
    let file = PythonFile::new(db, file, PythonVersion::latest_ty());
    Ok(parsed_module(db, file).load(db))
}

/// Lexically normalize a path for use in a salsa memo key, without touching the
/// filesystem.
///
/// Collapses `.`/`..` and makes the path absolute against the database's current
/// directory via `SystemPath::absolute`. This mirrors `ty`, which canonicalizes
/// search-path *roots* at construction but keeps everything lexical downstream
/// (module resolution + `File` keys).
///
/// Falls back to the original path for non-UTF8 paths that cannot be represented
/// as a `SystemPath`.
pub(super) fn normalize_path_for_key(db: &dyn ruff_db::Db, path: &Path) -> PathBuf {
    // Vendored sentinel paths are already canonical.
    if is_vendored_path(path) {
        return path.to_path_buf();
    }
    match SystemPath::from_std_path(path) {
        Some(sys_path) => {
            SystemPath::absolute(sys_path, db.system().current_directory()).into_std_path_buf()
        }
        None => path.to_path_buf(),
    }
}

/// Extract function signature from source code via salsa-cached parsing.
pub fn extract_function_signature(
    db: &dyn ruff_db::Db,
    path: &Path,
    function_name: &str,
) -> Result<FunctionSignature> {
    let source = read_source(db, path)?;
    let parsed = get_parsed_module(db, path)?;

    let mut visitor = FunctionExtractor {
        target_name: function_name.to_string(),
        result: None,
        source: source.as_str().to_string(),
    };
    visitor.visit_body(parsed.suite());
    visitor
        .result
        .ok_or_else(|| anyhow::anyhow!("Function '{}' not found", function_name))
}

/// Whether `name` is bound at module scope in the module at `path`.
pub(super) fn module_defines_top_level(db: &dyn ruff_db::Db, path: &Path, name: &str) -> bool {
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
            Stmt::Assign(assign) => assign
                .targets
                .iter()
                .any(|target| matches!(target, Expr::Name(n) if n.id.as_str() == name)),
            Stmt::AnnAssign(ann_assign) => {
                matches!(ann_assign.target.as_ref(), Expr::Name(n) if n.id.as_str() == name)
            }
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

    let Ok(parsed) = get_parsed_module(db, path) else {
        return false;
    };
    body_binds(parsed.suite(), name)
}

/// Extract class information from source code via salsa-cached parsing.
pub fn extract_class_info(
    db: &dyn ruff_db::Db,
    path: &Path,
    class_name: &str,
) -> Result<ClassInfo> {
    let source = read_source(db, path)?;
    let parsed = get_parsed_module(db, path)?;

    let mut visitor = ClassExtractor {
        target_name: class_name.to_string(),
        result: None,
        source: source.as_str().to_string(),
    };
    visitor.visit_body(parsed.suite());
    visitor
        .result
        .ok_or_else(|| anyhow::anyhow!("Class '{}' not found", class_name))
}

/// Extract method information from source code via salsa-cached parsing.
pub(super) fn extract_method_info(
    db: &dyn ruff_db::Db,
    path: &Path,
    class_name: &str,
    method_name: &str,
) -> Result<MethodInfo> {
    let source = read_source(db, path)?;
    let parsed = get_parsed_module(db, path)?;

    let mut visitor = MethodExtractor {
        class_name: class_name.to_string(),
        method_name: method_name.to_string(),
        result: None,
        source: source.as_str().to_string(),
    };
    visitor.visit_body(parsed.suite());
    visitor.result.ok_or_else(|| {
        anyhow::anyhow!(
            "Method '{}' not found in class '{}'",
            method_name,
            class_name
        )
    })
}

/// Extract class attribute from source code via salsa-cached parsing.
pub(super) fn extract_class_attribute(
    db: &dyn ruff_db::Db,
    path: &Path,
    class_name: &str,
    attribute_name: &str,
) -> Result<ClassAttributeInfo> {
    let parsed = get_parsed_module(db, path)?;

    let mut visitor = ClassAttributeExtractor {
        class_name: class_name.to_string(),
        attribute_name: attribute_name.to_string(),
        result: None,
    };

    visitor.visit_body(parsed.suite());

    visitor.result.ok_or_else(|| {
        anyhow::anyhow!(
            "Attribute '{}' not found in class '{}'",
            attribute_name,
            class_name
        )
    })
}

/// Visitor to extract function signatures from AST
struct FunctionExtractor {
    target_name: String,
    result: Option<FunctionSignature>,
    source: String,
}

impl<'a> Visitor<'a> for FunctionExtractor {
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
            self.result = extract_declared_signature(body, &self.target_name, &self.source);
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
struct ClassExtractor {
    target_name: String,
    source: String,
    result: Option<ClassInfo>,
}

impl<'a> Visitor<'a> for ClassExtractor {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.result.is_some() {
            return; // Already found
        }

        if let Stmt::ClassDef(class_def) = stmt
            && class_def.name.as_str() == self.target_name
        {
            self.result = Some(extract_class_info_from_def(class_def, &self.source));
            return;
        }

        // Continue walking
        ast::visitor::walk_stmt(self, stmt);
    }
}

/// Visitor to extract a specific method from a class
struct MethodExtractor {
    class_name: String,
    method_name: String,
    result: Option<MethodInfo>,
    source: String,
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
            // Look for attribute assignments within the class body
            for class_stmt in &class_def.body {
                // Handle simple assignments: `attr = SomeClass`
                if let Stmt::Assign(assign) = class_stmt {
                    for target in &assign.targets {
                        if let Expr::Name(name) = target
                            && name.id.as_str() == self.attribute_name
                        {
                            self.result = Some(ClassAttributeInfo {
                                name: self.attribute_name.clone(),
                                value: expr_to_string(&assign.value),
                            });
                            return;
                        }
                    }
                }
                // Handle annotated assignments: `attr: Type = SomeClass`
                if let Stmt::AnnAssign(ann_assign) = class_stmt
                    && let Expr::Name(name) = ann_assign.target.as_ref()
                    && name.id.as_str() == self.attribute_name
                    && let Some(value) = &ann_assign.value
                {
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

impl<'a> Visitor<'a> for MethodExtractor {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.result.is_some() {
            return; // Already found
        }

        if let Stmt::ClassDef(class_def) = stmt
            && class_def.name.as_str() == self.class_name
        {
            // Look for the method within the class body
            for class_stmt in &class_def.body {
                if let Stmt::FunctionDef(func_def) = class_stmt
                    && func_def.name.as_str() == self.method_name
                    && let Some((declaration, is_overloaded)) =
                        resolve_declaration(&class_def.body, &self.method_name)
                {
                    let mut signature =
                        extract_function_signature_from_def(declaration, &self.source);
                    signature.is_overloaded = is_overloaded;
                    signature.overloads =
                        overload_signatures(&class_def.body, &self.method_name, &self.source);

                    // Check decorators for @classmethod or @staticmethod, on the
                    // declaration actually chosen rather than on this statement.
                    let (is_classmethod, is_staticmethod) =
                        check_method_decorators(&declaration.decorator_list);

                    self.result = Some(MethodInfo {
                        class_name: self.class_name.clone(),
                        method_name: self.method_name.clone(),
                        signature,
                        is_classmethod,
                        is_staticmethod,
                    });
                    return;
                }
            }
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

/// The signature of `name` as declared in `body`, resolved through any
/// `@overload` set.
fn extract_declared_signature(
    body: &[Stmt],
    name: &str,
    source: &str,
) -> Option<FunctionSignature> {
    let (func_def, is_overloaded) = resolve_declaration(body, name)?;
    let mut signature = extract_function_signature_from_def(func_def, source);
    signature.is_overloaded = is_overloaded;
    signature.overloads = overload_signatures(body, name, source);
    Some(signature)
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
    has_decorator(decorators, "overload")
}

/// Check for `@type_check_only`, however `typing.type_check_only` was imported.
fn has_type_check_only_decorator(decorators: &[ast::Decorator]) -> bool {
    has_decorator(decorators, "type_check_only")
}

/// Whether any decorator names `name`, qualified or not.
fn has_decorator(decorators: &[ast::Decorator], name: &str) -> bool {
    decorators.iter().any(|decorator| {
        let decorator = expr_to_string(&decorator.expression);
        decorator == name || decorator.ends_with(&format!(".{name}"))
    })
}

/// Check if a function has @classmethod or @staticmethod decorators
fn check_method_decorators(decorators: &[ast::Decorator]) -> (bool, bool) {
    let mut is_classmethod = false;
    let mut is_staticmethod = false;

    for decorator in decorators {
        if let Expr::Name(name) = &decorator.expression {
            match name.id.as_str() {
                "classmethod" => is_classmethod = true,
                "staticmethod" => is_staticmethod = true,
                _ => {}
            }
        }
    }

    (is_classmethod, is_staticmethod)
}

/// Convert a TextRange to line/column position
/// Returns (start_line, start_column, end_line, end_column)
fn get_position_info(range: TextRange, source: &str) -> (u32, u32, u32, u32) {
    let line_index = LineIndex::from_source_text(source);

    // The server advertises no `positionEncoding`, so the LSP default of
    // UTF-16 applies to every position we send to the client. Emitting UTF-32
    // (codepoint) columns here would mis-position ranges by one per non-BMP
    // character (codepoints >= U+10000) earlier on the line.
    let start = line_index.source_location(range.start(), source, PositionEncoding::Utf16);
    let end = line_index.source_location(range.end(), source, PositionEncoding::Utf16);

    (
        start.line.to_zero_indexed() as u32,
        start.character_offset.to_zero_indexed() as u32,
        end.line.to_zero_indexed() as u32,
        end.character_offset.to_zero_indexed() as u32,
    )
}

/// Extract function signature from a function definition node
fn extract_function_signature_from_def(
    func_def: &ast::StmtFunctionDef,
    source: &str,
) -> FunctionSignature {
    let parameters = extract_parameters(&func_def.parameters);
    let return_type = func_def.returns.as_ref().map(|e| expr_to_string(e));
    let docstring = extract_docstring(&func_def.body);
    let (start_line, start_column, end_line, end_column) =
        get_position_info(func_def.range, source);

    FunctionSignature {
        name: func_def.name.to_string(),
        parameters,
        return_type,
        docstring,
        is_overloaded: false,
        overloads: Vec::new(),
        start_line,
        start_column,
        end_line,
        end_column,
    }
}

/// Extract class info from a class definition node
fn extract_class_info_from_def(class_def: &ast::StmtClassDef, source: &str) -> ClassInfo {
    let docstring = extract_docstring(&class_def.body);
    let (start_line, start_column, end_line, end_column) =
        get_position_info(class_def.range, source);

    // Extract base classes
    let base_classes: Vec<String> = class_def.bases().iter().map(expr_to_string).collect();

    let init_signature = extract_declared_signature(&class_def.body, "__init__", source);
    let new_signature = extract_declared_signature(&class_def.body, "__new__", source);

    ClassInfo {
        name: class_def.name.to_string(),
        base_classes,
        docstring,
        init_signature,
        new_signature,
        unresolved_base_classes: false,
        start_line,
        start_column,
        end_line,
        end_column,
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

    use crate::python::test_support::{get_simple_test_dir, test_db};

    // ==================== extract_function_signature tests ====================

    #[test]
    fn test_extract_simple_function() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let db = test_db();

        let sig = extract_function_signature(&db, &test_file, "simple_function").unwrap();
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

        let sig = extract_function_signature(&db, &test_file, "function_with_params").unwrap();
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

        let sig = extract_function_signature(&db, &test_file, "function_with_return").unwrap();
        assert_eq!(sig.name, "function_with_return");
        assert!(sig.return_type.is_some());
        assert_eq!(sig.return_type.as_ref().unwrap(), "int");
    }

    #[test]
    fn test_extract_variadic_function() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let db = test_db();

        let sig = extract_function_signature(&db, &test_file, "variadic_function").unwrap();
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

        let sig = extract_function_signature(&db, &test_file, "complex_function").unwrap();
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

        let result = extract_function_signature(&db, &test_file, "nonexistent");
        assert!(result.is_err());
    }

    // ==================== extract_class_info tests ====================

    #[test]
    fn test_extract_simple_class() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let db = test_db();

        let class_info = extract_class_info(&db, &test_file, "SimpleClass").unwrap();
        assert_eq!(class_info.name, "SimpleClass");
        assert!(class_info.docstring.is_some());
        assert!(class_info.init_signature.is_none());
    }

    #[test]
    fn test_extract_class_with_init() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let db = test_db();

        let class_info = extract_class_info(&db, &test_file, "ClassWithInit").unwrap();
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

        let class_info = extract_class_info(&db, &test_file, "ComplexClass").unwrap();
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

        let result = extract_class_info(&db, &test_file, "NonexistentClass");
        assert!(result.is_err());
    }
}
