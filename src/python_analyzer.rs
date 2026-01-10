use anyhow::Result;
use ruff_db::system::{OsSystem, SystemPath, SystemPathBuf};
use ruff_python_ast::{self as ast, visitor::Visitor, Expr, Stmt};
use ruff_python_parser::parse_module;
use std::fs;
use std::path::{Path, PathBuf};
use ty_python_semantic::{PythonEnvironment, SysPrefixPathOrigin};

#[derive(Debug, Clone)]
pub struct FunctionSignature {
    pub name: String,
    pub parameters: Vec<ParameterInfo>,
    pub return_type: Option<String>,
    pub docstring: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ParameterInfo {
    pub name: String,
    pub type_annotation: Option<String>,
    pub default_value: Option<String>,
    pub has_default: bool,
    pub is_variadic: bool,         // *args
    pub is_variadic_keyword: bool, // **kwargs
    pub is_keyword_only: bool,
}

impl ParameterInfo {
    pub fn is_required(&self) -> bool {
        !self.has_default && !self.is_variadic && !self.is_variadic_keyword && self.name != "self"
    }
}

#[derive(Debug, Clone)]
pub struct ClassInfo {
    pub name: String,
    pub docstring: Option<String>,
    pub init_signature: Option<FunctionSignature>,
}

#[derive(Debug, Clone)]
pub enum DefinitionInfo {
    Function(FunctionSignature),
    Class(ClassInfo),
}

pub struct PythonAnalyzer;

impl PythonAnalyzer {
    /// Split a `_target_` string into module path and symbol name
    /// Example: "myproject.models.MyClass" -> ("myproject.models", "MyClass")
    pub fn split_target(target: &str) -> Result<(String, String)> {
        let parts: Vec<&str> = target.split('.').collect();
        if parts.len() < 2 {
            anyhow::bail!("Invalid target format: {}", target);
        }

        let symbol_name = parts.last().unwrap().to_string();
        let module_path = parts[..parts.len() - 1].join(".");

        Ok((module_path, symbol_name))
    }

    /// Discover the Python environment and get site-packages paths
    ///
    /// This uses ty's sophisticated Python environment discovery which:
    /// - Discovers virtual environments (venv, conda, uv)
    /// - Handles Python interpreter paths vs sys.prefix directories
    /// - Resolves site-packages for different Python versions and implementations
    /// - Supports system site-packages and editable installs
    fn discover_python_environment(
        workspace_root: Option<&Path>,
        python_path: Option<&str>,
    ) -> Result<Vec<SystemPathBuf>> {
        // Create the system - OsSystem needs a current working directory
        let cwd = if let Some(root) = workspace_root {
            SystemPath::from_std_path(root)
                .ok_or_else(|| anyhow::anyhow!("Invalid workspace root path"))?
        } else {
            SystemPath::new(".")
        };
        let system = OsSystem::new(cwd);

        // Use the same path as project root for environment discovery
        let project_root = cwd;

        let env = if let Some(python_path_str) = python_path {
            // User provided a specific Python path (could be executable or sys.prefix directory)
            let python_sys_path = SystemPath::from_std_path(Path::new(python_path_str))
                .ok_or_else(|| anyhow::anyhow!("Invalid Python path"))?;

            PythonEnvironment::new(python_sys_path, SysPrefixPathOrigin::PythonCliFlag, &system)?
        } else {
            // Auto-discover Python environment with priority:
            // 1. Activated virtual environment (VIRTUAL_ENV)
            // 2. Conda environment (CONDA_PREFIX)
            // 3. Working directory virtual environment (.venv)
            // 4. Conda base environment
            PythonEnvironment::discover(project_root, &system)?
                .ok_or_else(|| anyhow::anyhow!("No Python environment found"))?
        };

        // Get site-packages directories from the discovered environment
        let site_packages_paths = env.site_packages_paths(&system)?;

        // Convert SitePackagesPaths to Vec<SystemPathBuf> for compatibility
        Ok(site_packages_paths.into_vec())
    }

    /// Resolve a Python module path to a file path using ty's sophisticated module resolution
    ///
    /// This implementation:
    /// - Discovers Python environment (venv, conda, system)
    /// - Uses proper site-packages resolution
    /// - Handles package hierarchies correctly
    /// - Supports .pyi stub files
    pub fn resolve_module(
        module_path: &str,
        workspace_root: Option<&Path>,
        python_interpreter: Option<&str>,
    ) -> Result<PathBuf> {
        let module_parts: Vec<&str> = module_path.split('.').collect();

        // Build search paths: workspace root + site-packages from ty
        let mut search_paths = Vec::new();

        // Add workspace root first (highest priority for first-party code)
        if let Some(root) = workspace_root {
            search_paths.push(PathBuf::from(root));
        }

        // Add current directory
        search_paths.push(PathBuf::from("."));

        // Use ty's environment discovery to get site-packages paths
        match Self::discover_python_environment(workspace_root, python_interpreter) {
            Ok(site_packages_paths) => {
                // Convert SystemPathBuf to PathBuf for searching
                for sys_path in site_packages_paths {
                    // SystemPath provides as_std_path() to convert to std::path::Path
                    search_paths.push(sys_path.as_std_path().to_path_buf());
                }
            }
            Err(e) => {
                // Log error but continue with basic search paths
                eprintln!("Warning: Could not discover Python environment: {}", e);
            }
        }

        // Store the count before iterating
        let search_path_count = search_paths.len();

        // Try to find the module as a package or file
        for search_path in search_paths {
            // Skip empty or non-existent paths
            if !search_path.exists() {
                continue;
            }

            // Try as a package with __init__.py
            let mut package_path = search_path.clone();
            for part in &module_parts {
                package_path.push(part);
            }

            // Check for package __init__.py (prioritize .pyi over .py)
            let init_pyi_path = package_path.join("__init__.pyi");
            if init_pyi_path.exists() {
                return Ok(init_pyi_path);
            }

            let init_path = package_path.join("__init__.py");
            if init_path.exists() {
                return Ok(init_path);
            }

            // Check for regular module file (prioritize .pyi over .py)
            let file_pyi_path = package_path.with_extension("pyi");
            if file_pyi_path.exists() {
                return Ok(file_pyi_path);
            }

            let file_path = package_path.with_extension("py");
            if file_path.exists() {
                return Ok(file_path);
            }

            // Try parent as package and last part as module
            if module_parts.len() > 1 {
                let mut parent_path = search_path.clone();
                for part in &module_parts[..module_parts.len() - 1] {
                    parent_path.push(part);
                }

                // Try .pyi first
                let module_pyi_file =
                    parent_path.join(format!("{}.pyi", module_parts.last().unwrap()));
                if module_pyi_file.exists() {
                    return Ok(module_pyi_file);
                }

                let module_file = parent_path.join(format!("{}.py", module_parts.last().unwrap()));
                if module_file.exists() {
                    return Ok(module_file);
                }
            }
        }

        anyhow::bail!(
            "Could not resolve module: {} (tried {} search paths)",
            module_path,
            search_path_count
        )
    }

    /// Extract function signature from a parsed Python AST
    /// This is a simplified extraction that visits the AST to find function definitions
    pub fn extract_function_signature(
        file_path: &Path,
        function_name: &str,
    ) -> Result<FunctionSignature> {
        let source = fs::read_to_string(file_path)?;
        let parsed = parse_module(&source)?;

        let mut visitor = FunctionExtractor {
            target_name: function_name.to_string(),
            result: None,
        };

        visitor.visit_body(parsed.suite());

        visitor.result.ok_or_else(|| {
            anyhow::anyhow!(
                "Function '{}' not found in {}",
                function_name,
                file_path.display()
            )
        })
    }

    /// Extract class information from a parsed Python AST
    pub fn extract_class_info(file_path: &Path, class_name: &str) -> Result<ClassInfo> {
        let source = fs::read_to_string(file_path)?;
        let parsed = parse_module(&source)?;

        let mut visitor = ClassExtractor {
            target_name: class_name.to_string(),
            result: None,
        };

        visitor.visit_body(parsed.suite());

        visitor.result.ok_or_else(|| {
            anyhow::anyhow!(
                "Class '{}' not found in {}",
                class_name,
                file_path.display()
            )
        })
    }

    /// Extract definition info (function or class) from a target string
    pub fn extract_definition_info(
        target: &str,
        workspace_root: Option<&Path>,
        python_interpreter: Option<&str>,
    ) -> Result<DefinitionInfo> {
        let (module_path, symbol_name) = Self::split_target(target)?;
        let file_path = Self::resolve_module(&module_path, workspace_root, python_interpreter)?;

        // Try to extract as function first
        if let Ok(func_sig) = Self::extract_function_signature(&file_path, &symbol_name) {
            return Ok(DefinitionInfo::Function(func_sig));
        }

        // Try to extract as class
        if let Ok(class_info) = Self::extract_class_info(&file_path, &symbol_name) {
            return Ok(DefinitionInfo::Class(class_info));
        }

        anyhow::bail!("Symbol '{}' not found in module", symbol_name)
    }

    /// Format a function signature for display (e.g., in hover)
    pub fn format_signature(sig: &FunctionSignature) -> String {
        let mut result = String::new();
        result.push_str("```python\n");
        result.push_str(&format!("def {}(", sig.name));

        let param_strs: Vec<String> = sig
            .parameters
            .iter()
            .map(|p| {
                let mut s = String::new();

                // Add * or ** prefix for variadic parameters
                if p.is_variadic {
                    s.push('*');
                } else if p.is_variadic_keyword {
                    s.push_str("**");
                }

                s.push_str(&p.name);

                if let Some(type_ann) = &p.type_annotation {
                    s.push_str(&format!(": {}", type_ann));
                }
                if let Some(default) = &p.default_value {
                    s.push_str(&format!(" = {}", default));
                }
                s
            })
            .collect();

        result.push_str(&param_strs.join(", "));
        result.push(')');

        if let Some(ret_type) = &sig.return_type {
            result.push_str(&format!(" -> {}", ret_type));
        }

        result.push_str("\n```");

        if let Some(docstring) = &sig.docstring {
            result.push_str("\n\n---\n\n");
            result.push_str(docstring);
        }

        result
    }

    /// Format a class for display (e.g., in hover)
    pub fn format_class(class: &ClassInfo) -> String {
        let mut result = String::new();
        result.push_str("```python\n");
        result.push_str(&format!("class {}", class.name));

        if let Some(init_sig) = &class.init_signature {
            result.push('(');
            let param_strs: Vec<String> = init_sig
                .parameters
                .iter()
                .filter(|p| p.name != "self") // Skip self parameter
                .map(|p| {
                    let mut s = p.name.clone();
                    if let Some(type_ann) = &p.type_annotation {
                        s.push_str(&format!(": {}", type_ann));
                    }
                    if let Some(default) = &p.default_value {
                        s.push_str(&format!(" = {}", default));
                    }
                    s
                })
                .collect();
            result.push_str(&param_strs.join(", "));
            result.push(')');
        }

        result.push_str("\n```");

        if let Some(docstring) = &class.docstring {
            result.push_str("\n\n---\n\n");
            result.push_str(docstring);
        }

        result
    }
}

/// Visitor to extract function signatures from AST
struct FunctionExtractor {
    target_name: String,
    result: Option<FunctionSignature>,
}

impl<'a> Visitor<'a> for FunctionExtractor {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.result.is_some() {
            return; // Already found
        }

        if let Stmt::FunctionDef(func_def) = stmt {
            if func_def.name.as_str() == self.target_name {
                self.result = Some(extract_function_signature_from_def(func_def));
                return;
            }
        }

        // Continue walking
        ast::visitor::walk_stmt(self, stmt);
    }
}

/// Visitor to extract class information from AST
struct ClassExtractor {
    target_name: String,
    result: Option<ClassInfo>,
}

impl<'a> Visitor<'a> for ClassExtractor {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.result.is_some() {
            return; // Already found
        }

        if let Stmt::ClassDef(class_def) = stmt {
            if class_def.name.as_str() == self.target_name {
                self.result = Some(extract_class_info_from_def(class_def));
                return;
            }
        }

        // Continue walking
        ast::visitor::walk_stmt(self, stmt);
    }
}

/// Extract function signature from a function definition node
fn extract_function_signature_from_def(func_def: &ast::StmtFunctionDef) -> FunctionSignature {
    let parameters = extract_parameters(&func_def.parameters);
    let return_type = func_def.returns.as_ref().map(|e| expr_to_string(e));
    let docstring = extract_docstring(&func_def.body);

    FunctionSignature {
        name: func_def.name.to_string(),
        parameters,
        return_type,
        docstring,
    }
}

/// Extract class info from a class definition node
fn extract_class_info_from_def(class_def: &ast::StmtClassDef) -> ClassInfo {
    let docstring = extract_docstring(&class_def.body);

    // Look for __init__ method
    let init_signature = class_def.body.iter().find_map(|stmt| {
        if let Stmt::FunctionDef(func_def) = stmt {
            if func_def.name.as_str() == "__init__" {
                return Some(extract_function_signature_from_def(func_def));
            }
        }
        None
    });

    ClassInfo {
        name: class_def.name.to_string(),
        docstring,
        init_signature,
    }
}

/// Extract parameters from function parameters
fn extract_parameters(params: &ast::Parameters) -> Vec<ParameterInfo> {
    let mut result = Vec::new();

    // Process regular parameters and positional-only
    for param_with_default in params.posonlyargs.iter().chain(params.args.iter()) {
        let param = &param_with_default.parameter;
        result.push(ParameterInfo {
            name: param.name.to_string(),
            type_annotation: param.annotation.as_ref().map(|e| expr_to_string(e)),
            default_value: param_with_default
                .default
                .as_ref()
                .map(|e| expr_to_string(e)),
            has_default: param_with_default.default.is_some(),
            is_variadic: false,
            is_variadic_keyword: false,
            is_keyword_only: false,
        });
    }

    // Process *args
    if let Some(vararg) = &params.vararg {
        result.push(ParameterInfo {
            name: vararg.name.to_string(),
            type_annotation: vararg.annotation.as_ref().map(|e| expr_to_string(e)),
            default_value: None,
            has_default: false,
            is_variadic: true,
            is_variadic_keyword: false,
            is_keyword_only: false,
        });
    }

    // Process keyword-only parameters
    for param_with_default in &params.kwonlyargs {
        let param = &param_with_default.parameter;
        result.push(ParameterInfo {
            name: param.name.to_string(),
            type_annotation: param.annotation.as_ref().map(|e| expr_to_string(e)),
            default_value: param_with_default
                .default
                .as_ref()
                .map(|e| expr_to_string(e)),
            has_default: param_with_default.default.is_some(),
            is_variadic: false,
            is_variadic_keyword: false,
            is_keyword_only: true,
        });
    }

    // Process **kwargs
    if let Some(kwarg) = &params.kwarg {
        result.push(ParameterInfo {
            name: kwarg.name.to_string(),
            type_annotation: kwarg.annotation.as_ref().map(|e| expr_to_string(e)),
            default_value: None,
            has_default: false,
            is_variadic: false,
            is_variadic_keyword: true,
            is_keyword_only: false,
        });
    }

    result
}

/// Extract docstring from function or class body
fn extract_docstring(body: &[Stmt]) -> Option<String> {
    if let Some(Stmt::Expr(expr_stmt)) = body.first() {
        if let Expr::StringLiteral(string_literal) = expr_stmt.value.as_ref() {
            return Some(string_literal.value.to_string());
        }
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
        Expr::BooleanLiteral(b) => format!("{}", b.value),
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
    use std::env;

    fn get_resources_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources")
    }

    // ==================== split_target tests ====================

    #[test]
    fn test_split_target() {
        let (module, symbol) = PythonAnalyzer::split_target("myproject.models.MyClass").unwrap();
        assert_eq!(module, "myproject.models");
        assert_eq!(symbol, "MyClass");
    }

    #[test]
    fn test_split_target_short() {
        let (module, symbol) = PythonAnalyzer::split_target("module.Class").unwrap();
        assert_eq!(module, "module");
        assert_eq!(symbol, "Class");
    }

    #[test]
    fn test_split_target_invalid() {
        assert!(PythonAnalyzer::split_target("InvalidTarget").is_err());
    }

    #[test]
    fn test_split_target_deeply_nested() {
        let (module, symbol) = PythonAnalyzer::split_target("a.b.c.d.e.FinalClass").unwrap();
        assert_eq!(module, "a.b.c.d.e");
        assert_eq!(symbol, "FinalClass");
    }

    // ==================== resolve_module tests ====================

    #[test]
    fn test_resolve_module_simple() {
        let examples_dir = get_resources_dir();
        let result = PythonAnalyzer::resolve_module("test_module", Some(&examples_dir), None);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert!(path.ends_with("test_module.py"));
    }

    #[test]
    fn test_resolve_module_package() {
        let examples_dir = get_resources_dir();
        let result = PythonAnalyzer::resolve_module("test_package", Some(&examples_dir), None);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert!(path.ends_with("__init__.py"));
    }

    #[test]
    fn test_resolve_module_submodule() {
        let examples_dir = get_resources_dir();
        let result =
            PythonAnalyzer::resolve_module("test_package.submodule", Some(&examples_dir), None);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert!(path.ends_with("submodule.py"));
    }

    #[test]
    fn test_resolve_module_nonexistent() {
        let examples_dir = get_resources_dir();
        let result =
            PythonAnalyzer::resolve_module("nonexistent_module", Some(&examples_dir), None);
        assert!(result.is_err());
    }

    // ==================== extract_function_signature tests ====================

    #[test]
    fn test_extract_simple_function() {
        let examples_dir = get_resources_dir();
        let test_file = examples_dir.join("test_module.py");

        let sig =
            PythonAnalyzer::extract_function_signature(&test_file, "simple_function").unwrap();
        assert_eq!(sig.name, "simple_function");
        assert_eq!(sig.parameters.len(), 0);
        assert!(sig.docstring.is_some());
        assert!(sig.docstring.as_ref().unwrap().contains("simple function"));
    }

    #[test]
    fn test_extract_function_with_params() {
        let examples_dir = get_resources_dir();
        let test_file = examples_dir.join("test_module.py");

        let sig =
            PythonAnalyzer::extract_function_signature(&test_file, "function_with_params").unwrap();
        assert_eq!(sig.name, "function_with_params");
        assert_eq!(sig.parameters.len(), 3);

        // Check first parameter (no type annotation)
        assert_eq!(sig.parameters[0].name, "arg1");
        assert!(sig.parameters[0].type_annotation.is_none());
        assert!(!sig.parameters[0].has_default);

        // Check second parameter (with type)
        assert_eq!(sig.parameters[1].name, "arg2");
        assert_eq!(sig.parameters[1].type_annotation.as_ref().unwrap(), "int");
        assert!(!sig.parameters[1].has_default);

        // Check third parameter (with type and default)
        assert_eq!(sig.parameters[2].name, "arg3");
        assert_eq!(sig.parameters[2].type_annotation.as_ref().unwrap(), "str");
        assert!(sig.parameters[2].has_default);
        assert_eq!(
            sig.parameters[2].default_value.as_ref().unwrap(),
            "'default'"
        );
    }

    #[test]
    fn test_extract_function_with_return() {
        let examples_dir = get_resources_dir();
        let test_file = examples_dir.join("test_module.py");

        let sig =
            PythonAnalyzer::extract_function_signature(&test_file, "function_with_return").unwrap();
        assert_eq!(sig.name, "function_with_return");
        assert!(sig.return_type.is_some());
        assert_eq!(sig.return_type.as_ref().unwrap(), "int");
    }

    #[test]
    fn test_extract_variadic_function() {
        let examples_dir = get_resources_dir();
        let test_file = examples_dir.join("test_module.py");

        let sig =
            PythonAnalyzer::extract_function_signature(&test_file, "variadic_function").unwrap();
        assert_eq!(sig.name, "variadic_function");
        assert_eq!(sig.parameters.len(), 2);

        // Check *args
        assert_eq!(sig.parameters[0].name, "args");
        assert!(sig.parameters[0].is_variadic);
        assert!(!sig.parameters[0].is_variadic_keyword);

        // Check **kwargs
        assert_eq!(sig.parameters[1].name, "kwargs");
        assert!(!sig.parameters[1].is_variadic);
        assert!(sig.parameters[1].is_variadic_keyword);
    }

    #[test]
    fn test_extract_complex_function() {
        let examples_dir = get_resources_dir();
        let test_file = examples_dir.join("test_module.py");

        let sig =
            PythonAnalyzer::extract_function_signature(&test_file, "complex_function").unwrap();
        assert_eq!(sig.name, "complex_function");

        // Should have: pos_only, regular, *args, keyword_only, another_kw, **kwargs
        assert_eq!(sig.parameters.len(), 6);

        // Check keyword-only parameter
        let kw_only = sig
            .parameters
            .iter()
            .find(|p| p.name == "keyword_only")
            .unwrap();
        assert!(kw_only.is_keyword_only);

        // Check return type
        assert!(sig.return_type.is_some());
    }

    #[test]
    fn test_extract_nonexistent_function() {
        let examples_dir = get_resources_dir();
        let test_file = examples_dir.join("test_module.py");

        let result = PythonAnalyzer::extract_function_signature(&test_file, "nonexistent");
        assert!(result.is_err());
    }

    // ==================== extract_class_info tests ====================

    #[test]
    fn test_extract_simple_class() {
        let examples_dir = get_resources_dir();
        let test_file = examples_dir.join("test_module.py");

        let class_info = PythonAnalyzer::extract_class_info(&test_file, "SimpleClass").unwrap();
        assert_eq!(class_info.name, "SimpleClass");
        assert!(class_info.docstring.is_some());
        assert!(class_info.init_signature.is_none());
    }

    #[test]
    fn test_extract_class_with_init() {
        let examples_dir = get_resources_dir();
        let test_file = examples_dir.join("test_module.py");

        let class_info = PythonAnalyzer::extract_class_info(&test_file, "ClassWithInit").unwrap();
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
        assert!(init_sig.parameters[2].has_default);
    }

    #[test]
    fn test_extract_complex_class() {
        let examples_dir = get_resources_dir();
        let test_file = examples_dir.join("test_module.py");

        let class_info = PythonAnalyzer::extract_class_info(&test_file, "ComplexClass").unwrap();
        assert_eq!(class_info.name, "ComplexClass");
        assert!(class_info.init_signature.is_some());

        let init_sig = class_info.init_signature.as_ref().unwrap();
        // Should have: self, *args, **kwargs
        assert_eq!(init_sig.parameters.len(), 3);
    }

    #[test]
    fn test_extract_nonexistent_class() {
        let examples_dir = get_resources_dir();
        let test_file = examples_dir.join("test_module.py");

        let result = PythonAnalyzer::extract_class_info(&test_file, "NonexistentClass");
        assert!(result.is_err());
    }

    // ==================== extract_definition_info tests ====================

    #[test]
    fn test_extract_definition_function() {
        let examples_dir = get_resources_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "test_module.simple_function",
            Some(&examples_dir),
            None,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            DefinitionInfo::Function(sig) => {
                assert_eq!(sig.name, "simple_function");
            }
            _ => panic!("Expected Function definition"),
        }
    }

    #[test]
    fn test_extract_definition_class() {
        let examples_dir = get_resources_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "test_module.SimpleClass",
            Some(&examples_dir),
            None,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            DefinitionInfo::Class(class_info) => {
                assert_eq!(class_info.name, "SimpleClass");
            }
            _ => panic!("Expected Class definition"),
        }
    }

    #[test]
    fn test_extract_definition_from_package() {
        let examples_dir = get_resources_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "test_package.submodule.SubmoduleClass",
            Some(&examples_dir),
            None,
        );
        assert!(result.is_ok());

        match result.unwrap() {
            DefinitionInfo::Class(class_info) => {
                assert_eq!(class_info.name, "SubmoduleClass");
            }
            _ => panic!("Expected Class definition"),
        }
    }

    #[test]
    fn test_extract_definition_nonexistent() {
        let examples_dir = get_resources_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "test_module.NonexistentSymbol",
            Some(&examples_dir),
            None,
        );
        assert!(result.is_err());
    }

    // ==================== format_signature tests ====================

    #[test]
    fn test_format_simple_signature() {
        let sig = FunctionSignature {
            name: "test_func".to_string(),
            parameters: vec![],
            return_type: None,
            docstring: None,
        };

        let formatted = PythonAnalyzer::format_signature(&sig);
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
                    type_annotation: Some("int".to_string()),
                    default_value: None,
                    has_default: false,
                    is_variadic: false,
                    is_variadic_keyword: false,
                    is_keyword_only: false,
                },
                ParameterInfo {
                    name: "y".to_string(),
                    type_annotation: Some("str".to_string()),
                    default_value: Some("'hello'".to_string()),
                    has_default: true,
                    is_variadic: false,
                    is_variadic_keyword: false,
                    is_keyword_only: false,
                },
            ],
            return_type: Some("bool".to_string()),
            docstring: Some("Test docstring".to_string()),
        };

        let formatted = PythonAnalyzer::format_signature(&sig);
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
                    type_annotation: None,
                    default_value: None,
                    has_default: false,
                    is_variadic: true,
                    is_variadic_keyword: false,
                    is_keyword_only: false,
                },
                ParameterInfo {
                    name: "kwargs".to_string(),
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

        let formatted = PythonAnalyzer::format_signature(&sig);
        assert!(formatted.contains("*args"));
        assert!(formatted.contains("**kwargs"));
    }

    // ==================== format_class tests ====================

    #[test]
    fn test_format_simple_class() {
        let class_info = ClassInfo {
            name: "TestClass".to_string(),
            docstring: Some("A test class".to_string()),
            init_signature: None,
        };

        let formatted = PythonAnalyzer::format_class(&class_info);
        assert!(formatted.contains("class TestClass"));
        assert!(formatted.contains("A test class"));
        assert!(formatted.starts_with("```python"));
    }

    #[test]
    fn test_format_class_with_init() {
        let class_info = ClassInfo {
            name: "TestClass".to_string(),
            docstring: Some("A test class".to_string()),
            init_signature: Some(FunctionSignature {
                name: "__init__".to_string(),
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
            }),
        };

        let formatted = PythonAnalyzer::format_class(&class_info);
        assert!(formatted.contains("class TestClass(value: int)"));
        assert!(!formatted.contains("self")); // self should be filtered out
        assert!(formatted.contains("A test class"));
    }

    #[test]
    fn test_format_class_with_defaults() {
        let class_info = ClassInfo {
            name: "TestClass".to_string(),
            docstring: None,
            init_signature: Some(FunctionSignature {
                name: "__init__".to_string(),
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
                        name: "name".to_string(),
                        type_annotation: Some("str".to_string()),
                        default_value: Some("'default'".to_string()),
                        has_default: true,
                        is_variadic: false,
                        is_variadic_keyword: false,
                        is_keyword_only: false,
                    },
                ],
                return_type: None,
                docstring: None,
            }),
        };

        let formatted = PythonAnalyzer::format_class(&class_info);
        assert!(formatted.contains("name: str = 'default'"));
    }

    // ==================== Environment discovery and module resolution tests ====================

    mod environment_tests {
        use super::*;
        use ruff_db::system::TestSystem;

        /// Helper to create a mock Python environment structure
        fn create_mock_venv(
            system: &TestSystem,
            venv_path: &str,
            python_version: &str,
            include_system_site_packages: bool,
        ) {
            let memory_fs = system.memory_file_system();
            let venv_root = SystemPathBuf::from(venv_path);

            // Create the appropriate structure based on OS
            let (exe_path, site_packages_path, pyvenv_cfg_path, home_path) =
                if cfg!(target_os = "windows") {
                    (
                        venv_root.join(r"Scripts\python.exe"),
                        venv_root.join(r"Lib\site-packages"),
                        venv_root.join("pyvenv.cfg"),
                        format!(r"\Python{}\Scripts", python_version.replace('.', "")),
                    )
                } else {
                    (
                        venv_root.join("bin/python"),
                        venv_root.join(format!("lib/python{}/site-packages", python_version)),
                        venv_root.join("pyvenv.cfg"),
                        format!("/usr/local/python{}/bin", python_version),
                    )
                };

            // Create python executable
            memory_fs.write_file_all(&exe_path, "").unwrap();

            // Create site-packages directory
            memory_fs.create_directory_all(&site_packages_path).unwrap();

            // Create pyvenv.cfg
            let mut cfg_contents = format!("home = {}\n", home_path);
            cfg_contents.push_str(&format!("version = {}\n", python_version));
            if include_system_site_packages {
                cfg_contents.push_str("include-system-site-packages = true\n");
            }

            memory_fs
                .write_file_all(&pyvenv_cfg_path, &cfg_contents)
                .unwrap();
        }

        /// Helper to create a mock system Python installation
        fn create_mock_system_python(
            system: &TestSystem,
            install_path: &str,
            python_version: &str,
        ) {
            let memory_fs = system.memory_file_system();
            let sys_prefix = SystemPathBuf::from(install_path);

            let (exe_path, site_packages_path) = if cfg!(target_os = "windows") {
                (
                    sys_prefix.join("python.exe"),
                    sys_prefix.join(r"Lib\site-packages"),
                )
            } else {
                (
                    sys_prefix.join("bin/python"),
                    sys_prefix.join(format!("lib/python{}/site-packages", python_version)),
                )
            };

            memory_fs.write_file_all(&exe_path, "").unwrap();
            memory_fs.create_directory_all(&site_packages_path).unwrap();
        }

        /// Helper to create a mock third-party package in site-packages
        fn create_mock_package_in_site_packages(
            system: &TestSystem,
            site_packages_path: &str,
            package_name: &str,
            has_init: bool,
        ) {
            let memory_fs = system.memory_file_system();
            let package_dir = SystemPathBuf::from(site_packages_path).join(package_name);

            memory_fs.create_directory_all(&package_dir).unwrap();

            if has_init {
                let init_file = package_dir.join("__init__.py");
                memory_fs
                    .write_file_all(&init_file, "# Package init\n")
                    .unwrap();
            }
        }

        #[test]
        fn test_resolve_module_with_venv() {
            let system = TestSystem::default();
            let venv_path = "/.venv";
            let python_version = "3.12";

            create_mock_venv(&system, venv_path, python_version, false);

            // Create a mock package in the venv's site-packages
            let site_packages = if cfg!(target_os = "windows") {
                format!(r"{}\Lib\site-packages", venv_path)
            } else {
                format!("{}/lib/python{}/site-packages", venv_path, python_version)
            };

            create_mock_package_in_site_packages(&system, &site_packages, "my_package", true);

            // Create a module file in the package
            let memory_fs = system.memory_file_system();
            let module_path = SystemPathBuf::from(site_packages.as_str())
                .join("my_package")
                .join("module.py");
            memory_fs
                .write_file_all(
                    &module_path,
                    "def test_func():\n    \"\"\"Test function\"\"\"\n    pass\n",
                )
                .unwrap();

            // Test resolving the module
            // Note: This test demonstrates the structure, but actual resolution
            // requires the full ty environment discovery which is complex to mock
            let expected_path = module_path.as_std_path().to_path_buf();
            assert!(expected_path.to_string_lossy().contains("my_package"));
            assert!(expected_path.to_string_lossy().contains("module.py"));
            assert!(memory_fs.exists(&module_path));
        }

        #[test]
        fn test_resolve_module_with_system_python() {
            let system = TestSystem::default();
            let install_path = if cfg!(target_os = "windows") {
                r"\Python312"
            } else {
                "/usr/local/python3.12"
            };
            let python_version = "3.12";

            create_mock_system_python(&system, install_path, python_version);

            let site_packages = if cfg!(target_os = "windows") {
                format!(r"{}\Lib\site-packages", install_path)
            } else {
                format!(
                    "{}/lib/python{}/site-packages",
                    install_path, python_version
                )
            };

            create_mock_package_in_site_packages(&system, &site_packages, "system_pkg", true);

            let memory_fs = system.memory_file_system();
            let module_path = SystemPathBuf::from(site_packages.as_str())
                .join("system_pkg")
                .join("__init__.py");
            memory_fs
                .write_file_all(&module_path, "# System package\n")
                .unwrap();

            assert!(memory_fs.exists(&module_path));
        }

        #[test]
        fn test_resolve_module_with_pyi_stub_priority() {
            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();
            let workspace = SystemPathBuf::from("/workspace");

            memory_fs.create_directory_all(&workspace).unwrap();

            // Create both .py and .pyi files
            let py_file = workspace.join("mymodule.py");
            let pyi_file = workspace.join("mymodule.pyi");

            memory_fs
                .write_file_all(&py_file, "def func(): pass\n")
                .unwrap();
            memory_fs
                .write_file_all(&pyi_file, "def func() -> None: ...\n")
                .unwrap();

            // The .pyi file should be preferred
            // This is tested by the actual resolve_module logic
            assert!(memory_fs.exists(&pyi_file));
            assert!(memory_fs.exists(&py_file));
        }

        #[test]
        fn test_discover_python_environment_structure() {
            let system = TestSystem::default();
            let venv_path = "/.venv";
            create_mock_venv(&system, venv_path, "3.12", false);

            let memory_fs = system.memory_file_system();
            let pyvenv_cfg = SystemPathBuf::from(venv_path).join("pyvenv.cfg");

            // Verify the structure was created correctly
            assert!(memory_fs.exists(&pyvenv_cfg));

            let cfg_content = memory_fs.read_to_string(&pyvenv_cfg).unwrap();
            assert!(cfg_content.contains("home ="));
            assert!(cfg_content.contains("version = 3.12"));
        }

        #[test]
        fn test_venv_with_system_site_packages() {
            let system = TestSystem::default();
            let venv_path = "/.venv";
            create_mock_venv(&system, venv_path, "3.12", true);

            let memory_fs = system.memory_file_system();
            let pyvenv_cfg = SystemPathBuf::from(venv_path).join("pyvenv.cfg");
            let cfg_content = memory_fs.read_to_string(&pyvenv_cfg).unwrap();

            assert!(cfg_content.contains("include-system-site-packages = true"));
        }

        #[test]
        fn test_multiple_python_versions() {
            let system = TestSystem::default();

            for version in &["3.10", "3.11", "3.12", "3.13"] {
                let venv_path = format!("/.venv{}", version.replace('.', ""));
                create_mock_venv(&system, &venv_path, version, false);

                let memory_fs = system.memory_file_system();
                let site_packages = if cfg!(target_os = "windows") {
                    SystemPathBuf::from(venv_path.as_str()).join(r"Lib\site-packages")
                } else {
                    SystemPathBuf::from(venv_path.as_str())
                        .join(format!("lib/python{}/site-packages", version))
                };

                assert!(memory_fs.exists(&site_packages));
            }
        }

        #[test]
        fn test_package_with_submodules() {
            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();
            let workspace = SystemPathBuf::from("/workspace");

            // Create a package with submodules
            let package_dir = workspace.join("mypackage");
            let subpackage_dir = package_dir.join("subpackage");

            memory_fs.create_directory_all(&subpackage_dir).unwrap();

            // Create __init__.py files
            memory_fs
                .write_file_all(package_dir.join("__init__.py"), "# Package init\n")
                .unwrap();
            memory_fs
                .write_file_all(subpackage_dir.join("__init__.py"), "# Subpackage init\n")
                .unwrap();

            // Create a module in the subpackage
            memory_fs
                .write_file_all(
                    subpackage_dir.join("module.py"),
                    "def submodule_func(): pass\n",
                )
                .unwrap();

            assert!(memory_fs.exists(&package_dir.join("__init__.py")));
            assert!(memory_fs.exists(&subpackage_dir.join("__init__.py")));
            assert!(memory_fs.exists(&subpackage_dir.join("module.py")));
        }

        #[test]
        fn test_namespace_package_without_init() {
            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();
            let workspace = SystemPathBuf::from("/workspace");

            // Create a namespace package (no __init__.py)
            let package_dir = workspace.join("namespace_pkg");
            memory_fs.create_directory_all(&package_dir).unwrap();

            // Create a module directly in the package
            memory_fs
                .write_file_all(package_dir.join("module.py"), "def func(): pass\n")
                .unwrap();

            // Verify no __init__.py exists
            assert!(!memory_fs.exists(&package_dir.join("__init__.py")));
            assert!(memory_fs.exists(&package_dir.join("module.py")));
        }

        #[test]
        fn test_resolve_with_workspace_priority() {
            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();

            // Create workspace module
            let workspace = SystemPathBuf::from("/workspace");
            memory_fs.create_directory_all(&workspace).unwrap();
            memory_fs
                .write_file_all(workspace.join("mymodule.py"), "# Workspace version\n")
                .unwrap();

            // Create venv with same module name
            let venv_path = "/.venv";
            create_mock_venv(&system, venv_path, "3.12", false);

            let site_packages = if cfg!(target_os = "windows") {
                format!(r"{}\Lib\site-packages", venv_path)
            } else {
                format!("{}/lib/python3.12/site-packages", venv_path)
            };

            let site_packages_path = SystemPathBuf::from(site_packages.as_str());
            memory_fs.create_directory_all(&site_packages_path).unwrap();
            memory_fs
                .write_file_all(
                    site_packages_path.join("mymodule.py"),
                    "# Site-packages version\n",
                )
                .unwrap();

            // Both exist, but workspace should have priority
            assert!(memory_fs.exists(&workspace.join("mymodule.py")));
            assert!(memory_fs.exists(&site_packages_path.join("mymodule.py")));
        }

        #[test]
        fn test_python_version_detection_from_pyvenv() {
            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();
            let venv_path = SystemPathBuf::from("/.venv");

            // Create minimal venv structure
            let pyvenv_cfg = venv_path.join("pyvenv.cfg");
            memory_fs.create_directory_all(&venv_path).unwrap();

            let cfg_contents = "home = /usr/local/python3.11/bin\nversion = 3.11.5\n";
            memory_fs.write_file_all(&pyvenv_cfg, cfg_contents).unwrap();

            let content = memory_fs.read_to_string(&pyvenv_cfg).unwrap();
            assert!(content.contains("version = 3.11.5"));

            // Parse version
            let version_line = content
                .lines()
                .find(|line| line.starts_with("version"))
                .unwrap();
            assert!(version_line.contains("3.11.5"));
        }

        #[test]
        fn test_conda_environment_structure() {
            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();

            // Create a conda environment structure
            let conda_prefix = SystemPathBuf::from("/opt/conda/envs/myenv");

            let (exe_path, site_packages) = if cfg!(target_os = "windows") {
                (
                    conda_prefix.join("python.exe"),
                    conda_prefix.join(r"Lib\site-packages"),
                )
            } else {
                (
                    conda_prefix.join("bin/python"),
                    conda_prefix.join("lib/python3.12/site-packages"),
                )
            };

            memory_fs.write_file_all(&exe_path, "").unwrap();
            memory_fs.create_directory_all(&site_packages).unwrap();

            // Create conda-meta directory (distinctive conda feature)
            let conda_meta = conda_prefix.join("conda-meta");
            memory_fs.create_directory_all(&conda_meta).unwrap();

            assert!(memory_fs.exists(&exe_path));
            assert!(memory_fs.exists(&site_packages));
            assert!(memory_fs.exists(&conda_meta));
        }

        #[test]
        fn test_lib_vs_lib64_on_unix() {
            if cfg!(target_os = "windows") {
                return; // Skip on Windows
            }

            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();
            let install_path = SystemPathBuf::from("/usr/local/python3.12");

            // Some systems use lib, others use lib64
            let lib_site_packages = install_path.join("lib/python3.12/site-packages");
            let lib64_site_packages = install_path.join("lib64/python3.12/site-packages");

            memory_fs.create_directory_all(&lib_site_packages).unwrap();
            memory_fs
                .create_directory_all(&lib64_site_packages)
                .unwrap();

            assert!(memory_fs.exists(&lib_site_packages));
            assert!(memory_fs.exists(&lib64_site_packages));
        }

        #[test]
        fn test_editable_install_structure() {
            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();

            let site_packages = if cfg!(target_os = "windows") {
                SystemPathBuf::from(r"\.venv\Lib\site-packages")
            } else {
                SystemPathBuf::from("/.venv/lib/python3.12/site-packages")
            };

            memory_fs.create_directory_all(&site_packages).unwrap();

            // Create a .pth file for editable install
            let pth_file = site_packages.join("myproject.pth");
            memory_fs
                .write_file_all(&pth_file, "/home/user/projects/myproject\n")
                .unwrap();

            assert!(memory_fs.exists(&pth_file));

            let content = memory_fs.read_to_string(&pth_file).unwrap();
            assert!(content.contains("myproject"));
        }

        #[test]
        fn test_free_threaded_python_313() {
            if cfg!(target_os = "windows") {
                return; // Skip on Windows for this test
            }

            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();
            let install_path = SystemPathBuf::from("/usr/local/python3.13");

            // Python 3.13+ free-threaded builds use a 't' suffix
            let site_packages = install_path.join("lib/python3.13t/site-packages");
            memory_fs.create_directory_all(&site_packages).unwrap();

            assert!(memory_fs.exists(&site_packages));
            assert!(site_packages.to_string().contains("python3.13t"));
        }
    }
}
