use crate::python_analyzer::{ClassExtractor, ClassInfo, FunctionExtractor, FunctionSignature};
use ruff_python_ast::{self as ast, Expr, Stmt, visitor::Visitor};
use ruff_python_parser::parse_module;
use rustc_hash::FxHashSet;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_IMPORT_DEPTH: usize = 10;

/// Information about an import statement
#[derive(Debug, Clone)]
enum ImportInfo {
    /// `from module import name` or `from module import name as alias`
    FromImport {
        module: String,
        name: String,
        _alias: Option<String>,
        level: u32, // For relative imports: 0 = absolute, 1 = '.', 2 = '..', etc.
    },
    /// `from module import *`
    StarImport { module: String, level: u32 },
    /// `import module` or `import module as alias`
    Import {
        module: String,
        _alias: Option<String>,
    },
}

/// Context for import resolution operations
pub struct ImportResolver {
    search_paths: Vec<PathBuf>,
    visited_files: HashSet<PathBuf>,
    depth: usize,
}

impl ImportResolver {
    pub fn new(search_paths: Vec<PathBuf>) -> Self {
        Self {
            search_paths,
            visited_files: HashSet::new(),
            depth: 0,
        }
    }

    /// Try to find a module file given a base package path
    /// Returns the first existing file in priority order: __init__.pyi, __init__.py, module.pyi, module.py
    pub fn find_module_file(package_path: &Path) -> Option<PathBuf> {
        // Check for package __init__.py (prioritize .pyi over .py)
        let init_pyi_path = package_path.join("__init__.pyi");
        if init_pyi_path.exists() {
            return Some(init_pyi_path);
        }

        let init_path = package_path.join("__init__.py");
        if init_path.exists() {
            return Some(init_path);
        }

        // Check for regular module file (prioritize .pyi over .py)
        let file_pyi_path = package_path.with_extension("pyi");
        if file_pyi_path.exists() {
            return Some(file_pyi_path);
        }

        let file_path = package_path.with_extension("py");
        if file_path.exists() {
            return Some(file_path);
        }

        None
    }

    /// Resolve a module path to a file path
    fn resolve_module_path(&self, module_path: &str) -> Option<PathBuf> {
        let module_parts: Vec<&str> = module_path.split('.').collect();

        for search_path in &self.search_paths {
            if !search_path.exists() {
                continue;
            }

            // Try as a package with __init__.py
            let mut package_path = search_path.clone();
            for part in &module_parts {
                package_path.push(part);
            }

            if let Some(found_path) = Self::find_module_file(&package_path) {
                return Some(found_path);
            }
        }
        None
    }

    /// Resolve a relative import to an absolute module path
    fn resolve_relative_import(
        &self,
        current_file: &Path,
        module: Option<&str>,
        level: u32,
    ) -> Option<String> {
        // Get the current package path
        let mut current_dir = current_file.parent()?;

        // Navigate up based on level (each level is one "..")
        // level 1 = current package (.)
        // level 2 = parent package (..)
        // etc.
        for _ in 1..level {
            current_dir = current_dir.parent()?;
        }

        // Find which search path this is under
        // Sort search paths by length descending to match more specific paths first
        // (e.g., site-packages should match before workspace root)
        let mut sorted_paths = self.search_paths.clone();
        sorted_paths.sort_by(|a, b| b.as_os_str().len().cmp(&a.as_os_str().len()));

        let mut package_path: Option<String> = None;
        for search_path in &sorted_paths {
            if current_dir.starts_with(search_path) {
                let relative = current_dir.strip_prefix(search_path).ok()?;
                let parts: Vec<&str> = relative.iter().filter_map(|p| p.to_str()).collect();
                package_path = Some(parts.join("."));
                break;
            }
        }

        let base = package_path.unwrap_or_default();
        if let Some(module) = module {
            if base.is_empty() {
                Some(module.to_string())
            } else {
                Some(format!("{}.{}", base, module))
            }
        } else if base.is_empty() {
            None
        } else {
            Some(base)
        }
    }

    /// Extract __all__ from a module
    fn extract_dunder_all(&self, file_path: &Path) -> Option<FxHashSet<String>> {
        let source = fs::read_to_string(file_path).ok()?;
        let parsed = parse_module(&source).ok()?;

        let mut dunder_all_finder = DunderAllFinder::default();
        dunder_all_finder.visit_body(parsed.suite());
        dunder_all_finder.names
    }

    /// Extract import information for a specific symbol from a module
    fn find_import_for_symbol(&self, file_path: &Path, symbol_name: &str) -> Option<ImportInfo> {
        let source = fs::read_to_string(file_path).ok()?;
        let parsed = parse_module(&source).ok()?;

        let mut finder = ImportFinder {
            target_symbol: symbol_name.to_string(),
            result: None,
        };
        finder.visit_body(parsed.suite());
        finder.result
    }

    /// Extract all star imports from a module
    fn find_star_imports(&self, file_path: &Path) -> Vec<ImportInfo> {
        let source = match fs::read_to_string(file_path) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let parsed = match parse_module(&source) {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };

        let mut finder = StarImportFinder::default();
        finder.visit_body(parsed.suite());
        finder.star_imports
    }

    /// Check if a class is directly defined in the file
    fn find_class_direct(&self, file_path: &Path, class_name: &str) -> Option<ClassInfo> {
        let source = fs::read_to_string(file_path).ok()?;
        let parsed = parse_module(&source).ok()?;

        let mut visitor = ClassExtractor::new(class_name.to_string(), source);
        visitor.visit_body(parsed.suite());
        visitor.get_result()
    }

    /// Check if a function is directly defined in the file
    fn find_function_direct(
        &self,
        file_path: &Path,
        function_name: &str,
    ) -> Option<FunctionSignature> {
        let source = fs::read_to_string(file_path).ok()?;
        let parsed = parse_module(&source).ok()?;

        let mut visitor = FunctionExtractor::new(function_name.to_string(), source);
        visitor.visit_body(parsed.suite());
        visitor.get_result()
    }

    /// Resolve a symbol by following import chains
    /// Returns (file_path, original_name) where the symbol is actually defined
    pub fn resolve_symbol(
        &mut self,
        starting_file: &Path,
        symbol_name: &str,
    ) -> Option<(PathBuf, String)> {
        // Check for cycles and depth limit
        if self.depth >= MAX_IMPORT_DEPTH {
            return None;
        }

        let canonical_path = starting_file
            .canonicalize()
            .unwrap_or_else(|_| starting_file.to_path_buf());
        if self.visited_files.contains(&canonical_path) {
            return None;
        }
        self.visited_files.insert(canonical_path.clone());
        self.depth += 1;

        // First, check if the symbol is directly defined in this file
        if self.find_class_direct(starting_file, symbol_name).is_some()
            || self
                .find_function_direct(starting_file, symbol_name)
                .is_some()
        {
            self.depth -= 1;
            return Some((starting_file.to_path_buf(), symbol_name.to_string()));
        }

        // Look for an explicit import of this symbol
        if let Some(ref import_info) = self.find_import_for_symbol(starting_file, symbol_name) {
            let result = self.follow_import(starting_file, import_info);
            self.depth -= 1;
            return result;
        }

        // Check star imports to see if the symbol is re-exported from another module
        let star_imports = self.find_star_imports(starting_file);

        for star_import in star_imports {
            if let ImportInfo::StarImport { ref module, level } = star_import {
                let resolved_module = if level > 0 {
                    self.resolve_relative_import(starting_file, Some(module), level)
                } else {
                    Some(module.clone())
                };

                if let Some(module_path) = resolved_module {
                    if let Some(module_file) = self.resolve_module_path(&module_path) {
                        // Check if this symbol is exported from the star-imported module
                        let star_dunder_all = self.extract_dunder_all(&module_file);

                        // If __all__ is defined, check if symbol is in it
                        // If not defined, check if symbol doesn't start with _
                        let is_exported = if let Some(ref all_names) = star_dunder_all {
                            all_names.contains(&symbol_name.to_string())
                        } else {
                            !symbol_name.starts_with('_')
                        };

                        if is_exported {
                            if let Some(result) = self.resolve_symbol(&module_file, symbol_name) {
                                self.depth -= 1;
                                return Some(result);
                            }
                        }
                    }
                }
            }
        }

        self.depth -= 1;
        None
    }

    /// Follow an import to find where a symbol is defined
    fn follow_import(
        &mut self,
        current_file: &Path,
        import: &ImportInfo,
    ) -> Option<(PathBuf, String)> {
        match import {
            ImportInfo::FromImport {
                module,
                name,
                _alias: _,
                level,
            } => {
                let resolved_module = if *level > 0 {
                    self.resolve_relative_import(current_file, Some(module), *level)
                } else {
                    Some(module.clone())
                };

                let module_path = resolved_module?;
                let module_file = self.resolve_module_path(&module_path)?;

                // Recursively resolve the symbol in the imported module
                self.resolve_symbol(&module_file, name)
            }
            ImportInfo::StarImport { module, level } => {
                // This shouldn't be called directly for star imports
                let resolved_module = if *level > 0 {
                    self.resolve_relative_import(current_file, Some(module), *level)
                } else {
                    Some(module.clone())
                };

                let module_path = resolved_module?;
                let module_file = self.resolve_module_path(&module_path)?;
                Some((module_file, String::new()))
            }
            ImportInfo::Import { module, .. } => {
                let module_file = self.resolve_module_path(module)?;
                Some((module_file, String::new()))
            }
        }
    }
}

/// Visitor to find __all__ definition
#[derive(Default)]
struct DunderAllFinder {
    names: Option<FxHashSet<String>>,
}

impl<'a> Visitor<'a> for DunderAllFinder {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.names.is_some() {
            return; // Already found
        }

        if let Stmt::Assign(assign) = stmt {
            for target in &assign.targets {
                if let Expr::Name(name) = target {
                    if name.id.as_str() == "__all__" {
                        if let Some(names) = extract_list_of_strings(&assign.value) {
                            self.names = Some(names);
                            return;
                        }
                    }
                }
            }
        }

        ast::visitor::walk_stmt(self, stmt);
    }
}

/// Visitor to find a specific import for a symbol
struct ImportFinder {
    target_symbol: String,
    result: Option<ImportInfo>,
}

impl<'a> Visitor<'a> for ImportFinder {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.result.is_some() {
            return; // Already found
        }

        match stmt {
            Stmt::ImportFrom(import_from) => {
                let module = import_from
                    .module
                    .as_ref()
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
                let level = import_from.level;

                for alias in &import_from.names {
                    let name = alias.name.as_str();
                    let asname = alias.asname.as_ref().map(|a| a.as_str().to_string());

                    // Check if this alias matches our target (either by name or alias)
                    let local_name = asname.as_deref().unwrap_or(name);
                    if local_name == self.target_symbol {
                        self.result = Some(ImportInfo::FromImport {
                            module: module.clone(),
                            name: name.to_string(),
                            _alias: asname,
                            level,
                        });
                        return;
                    }
                }
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    let module = alias.name.as_str();
                    let asname = alias.asname.as_ref().map(|a| a.as_str().to_string());
                    let local_name = asname.as_deref().unwrap_or(module);

                    if local_name == self.target_symbol {
                        self.result = Some(ImportInfo::Import {
                            module: module.to_string(),
                            _alias: asname,
                        });
                        return;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Visitor to find all star imports
#[derive(Default)]
struct StarImportFinder {
    star_imports: Vec<ImportInfo>,
}

impl<'a> Visitor<'a> for StarImportFinder {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if let Stmt::ImportFrom(import_from) = stmt {
            for alias in &import_from.names {
                if alias.name.as_str() == "*" {
                    let module = import_from
                        .module
                        .as_ref()
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();

                    self.star_imports.push(ImportInfo::StarImport {
                        module,
                        level: import_from.level,
                    });
                }
            }
        }
    }
}

/// Extract a list of strings from an expression (for __all__)
fn extract_list_of_strings(expr: &Expr) -> Option<FxHashSet<String>> {
    let elements = match expr {
        Expr::List(list) => &list.elts,
        Expr::Tuple(tuple) => &tuple.elts,
        Expr::Set(set) => &set.elts,
        _ => return None,
    };

    let mut names = FxHashSet::default();
    for element in elements {
        if let Expr::StringLiteral(string_lit) = element {
            names.insert(string_lit.value.to_string());
        } else {
            // If any element is not a string literal, we can't reliably parse __all__
            return None;
        }
    }
    Some(names)
}
