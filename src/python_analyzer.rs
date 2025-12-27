use anyhow::{Context, Result};
use std::path::PathBuf;

// Note: Python parsing is currently disabled because ruff crates are not published to crates.io
// To enable full Python analysis, add ruff and ty as git dependencies in Cargo.toml

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
    /// Split a _target_ string into module path and symbol name
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

    /// Resolve a Python module path to a file path
    /// This is a simplified version - full implementation would use ty_module_resolver
    pub fn resolve_module(_module_path: &str) -> Result<PathBuf> {
        // TODO: Implement proper module resolution using ty_module_resolver
        // For now, return a placeholder error
        anyhow::bail!("Module resolution not yet implemented - requires ruff/ty dependencies")
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
