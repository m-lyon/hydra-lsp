use anyhow::Result;
use ruff_python_ast::{self as ast, visitor::Visitor, Expr, Stmt};
use ruff_python_parser::parse_module;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

    /// Get Python sys.path from the interpreter
    fn get_python_sys_path(python_interpreter: Option<&str>) -> Result<Vec<PathBuf>> {
        let python_cmd = python_interpreter.unwrap_or("python3");
        
        let output = Command::new(python_cmd)
            .arg("-c")
            .arg("import sys; print('\\n'.join(sys.path))")
            .output();
        
        match output {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let paths = stdout
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(PathBuf::from)
                    .collect();
                Ok(paths)
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("Python interpreter failed: {}", stderr)
            }
            Err(e) => {
                anyhow::bail!("Failed to run Python interpreter '{}': {}", python_cmd, e)
            }
        }
    }

    /// Resolve a Python module path to a file path using Python interpreter's sys.path
    /// If python_interpreter is None, uses "python3" by default
    pub fn resolve_module(
        module_path: &str,
        workspace_root: Option<&Path>,
        python_interpreter: Option<&str>,
    ) -> Result<PathBuf> {
        let module_parts: Vec<&str> = module_path.split('.').collect();
        
        // Build search paths: workspace root + Python sys.path
        let mut search_paths = Vec::new();
        
        // Add workspace root first (highest priority)
        if let Some(root) = workspace_root {
            search_paths.push(root.to_path_buf());
        }
        
        // Add current directory
        search_paths.push(PathBuf::from("."));
        
        // Try to get Python sys.path from interpreter
        match Self::get_python_sys_path(python_interpreter) {
            Ok(sys_paths) => {
                search_paths.extend(sys_paths);
            }
            Err(e) => {
                // Log error but continue with basic search paths
                eprintln!("Warning: Could not get Python sys.path: {}", e);
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

            // Check for package __init__.py
            let init_path = package_path.join("__init__.py");
            if init_path.exists() {
                return Ok(init_path);
            }

            // Check for regular module file
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
                let module_file = parent_path.join(format!("{}.py", module_parts.last().unwrap()));
                if module_file.exists() {
                    return Ok(module_file);
                }
            }
        }

        anyhow::bail!("Could not resolve module: {} (tried {} search paths)", module_path, search_path_count)
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

        anyhow::bail!("Could not find definition for '{}'", symbol_name)
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
    if let Some(first_stmt) = body.first() {
        if let Stmt::Expr(expr_stmt) = first_stmt {
            if let Expr::StringLiteral(string_literal) = expr_stmt.value.as_ref() {
                return Some(string_literal.value.to_string());
            }
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
        Expr::NumberLiteral(n) => format!("{:?}", n.value),
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
}
