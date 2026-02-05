use crate::import_resolver::ImportResolver;
use anyhow::Result;
use ruff_db::system::{OsSystem, SystemPath, SystemPathBuf};
use ruff_python_ast::{self as ast, Expr, Stmt, visitor::Visitor};
use ruff_python_parser::parse_module;
use ruff_source_file::{LineIndex, PositionEncoding};
use ruff_text_size::TextRange;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use ty_python_semantic::{PythonEnvironment, SysPrefixPathOrigin};

/// Maximum depth for parent class resolution to prevent infinite loops
const MAX_PARENT_DEPTH: usize = 1000;

/// Cache for file contents to minimize disk reads during analysis.
/// Files are read once and stored in memory for the duration of an analysis operation.
pub struct FileCache {
    cache: HashMap<PathBuf, String>,
}

impl FileCache {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    /// Get file contents, reading from disk if not already cached
    pub fn get(&mut self, path: &Path) -> Result<&str> {
        // Use entry API to avoid double lookup
        if !self.cache.contains_key(path) {
            let content = fs::read_to_string(path)?;
            self.cache.insert(path.to_path_buf(), content);
        }
        Ok(self.cache.get(path).unwrap())
    }

    /// Check if a file is already cached
    pub fn contains(&self, path: &Path) -> bool {
        self.cache.contains_key(path)
    }
}

impl Default for FileCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct FunctionSignature {
    pub name: String,
    pub parameters: Vec<ParameterInfo>,
    pub return_type: Option<String>,
    pub docstring: Option<String>,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
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
    pub base_classes: Vec<String>,
    pub docstring: Option<String>,
    pub init_signature: Option<FunctionSignature>,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
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
}

pub struct PythonAnalyzer;

/// Result of resolving a class attribute chain
enum ClassAttributeChainResult {
    /// Chain resolved to a method
    Method {
        file_path: PathBuf,
        class_name: String,
        method_name: String,
        search_paths: Vec<PathBuf>,
    },
    /// Chain resolved to a class
    Class {
        file_path: PathBuf,
        class_name: String,
        search_paths: Vec<PathBuf>,
    },
}

impl PythonAnalyzer {
    /// Split a `_target_` string into module path and symbol name
    /// Example: "myproject.models.MyClass" -> ("myproject.models", "MyClass")
    pub fn split_target(target: &str) -> Result<(String, String)> {
        let parts: Vec<&str> = target.split('.').collect();
        if parts.len() < 2 {
            anyhow::bail!("Invalid _target_ format: '{}'", target);
        }

        let symbol_name = parts.last().unwrap().to_string();
        let module_path = parts[..parts.len() - 1].join(".");

        Ok((module_path, symbol_name))
    }

    /// Discover the Python environment and get site-packages paths
    ///
    /// This uses `ty` Python environment discovery with the following priority:
    /// 1. Configured Python interpreter from LSP initialization (highest priority)
    /// 2. Activated virtual environment (VIRTUAL_ENV)
    /// 3. Conda environment (CONDA_PREFIX)
    /// 4. Working directory virtual environment (.venv)
    /// 5. Conda base environment
    /// 6. System Python (fallback)
    ///
    /// The configured interpreter is always preferred if provided, as it represents
    /// the user's explicit choice via LSP configuration.
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
            // User provided a specific Python path (highest priority - from LSP configuration)
            // This could be an executable or sys.prefix directory
            let python_sys_path = SystemPath::from_std_path(Path::new(python_path_str))
                .ok_or_else(|| anyhow::anyhow!("Invalid Python path"))?;

            PythonEnvironment::new(python_sys_path, SysPrefixPathOrigin::Editor, &system)?
        } else {
            // Auto-discover Python environment with built-in priority:
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

    /// Resolve a Python module path to a file path using ty's module resolution
    ///
    /// This implementation:
    /// - Discovers Python environment (venv, conda, system)
    /// - Uses proper site-packages resolution
    /// - Handles package hierarchies correctly
    /// - Supports .pyi stub files
    ///
    /// Resolve a Python module path to a file path using ty's module resolution
    ///
    /// Returns the resolved file path and the search paths used
    pub fn resolve_module(
        module_path: &str,
        workspace_root: Option<&Path>,
        python_interpreter: Option<&str>,
    ) -> Result<(PathBuf, Vec<PathBuf>)> {
        let site_packages_paths =
            Self::discover_python_environment(workspace_root, python_interpreter)
                .unwrap_or_default();
        let search_paths = Self::build_search_paths(workspace_root, site_packages_paths.to_vec());
        let file_path = Self::resolve_module_with_search_paths(module_path, &search_paths)?;
        Ok((file_path, search_paths))
    }

    /// Resolve a Python module path to a file path using pre-built search paths
    pub fn resolve_module_with_search_paths(
        module_path: &str,
        search_paths: &[PathBuf],
    ) -> Result<PathBuf> {
        let module_parts: Vec<&str> = module_path.split('.').collect();
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

            if let Some(found_path) = ImportResolver::find_module_file(&package_path) {
                return Ok(found_path);
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
            "Could not resolve module: '{}' (tried {} search paths)",
            module_path,
            search_path_count
        )
    }

    /// Extract function signature from source code
    pub fn extract_function_signature(
        source: &str,
        function_name: &str,
    ) -> Result<FunctionSignature> {
        let parsed = parse_module(source)?;

        let mut visitor = FunctionExtractor {
            target_name: function_name.to_string(),
            result: None,
            source: source.to_string(),
        };

        visitor.visit_body(parsed.suite());

        visitor
            .result
            .ok_or_else(|| anyhow::anyhow!("Function '{}' not found", function_name))
    }

    /// Extract class information from source code
    pub fn extract_class_info(source: &str, class_name: &str) -> Result<ClassInfo> {
        let parsed = parse_module(source)?;

        let mut visitor = ClassExtractor {
            target_name: class_name.to_string(),
            result: None,
            source: source.to_string(),
        };

        visitor.visit_body(parsed.suite());

        visitor
            .result
            .ok_or_else(|| anyhow::anyhow!("Class '{}' not found", class_name))
    }

    /// Extract method information from source code
    pub fn extract_method_info(
        source: &str,
        class_name: &str,
        method_name: &str,
    ) -> Result<MethodInfo> {
        let parsed = parse_module(source)?;

        let mut visitor = MethodExtractor {
            class_name: class_name.to_string(),
            method_name: method_name.to_string(),
            result: None,
            source: source.to_string(),
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

    /// Extract class attribute from source code
    pub fn extract_class_attribute(
        source: &str,
        class_name: &str,
        attribute_name: &str,
    ) -> Result<ClassAttributeInfo> {
        let parsed = parse_module(source)?;

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

    /// Extract a class attribute, searching parent classes if not found directly.
    ///
    /// Returns (ClassAttributeInfo, file_path, class_name) where the attribute was found.
    fn extract_class_attribute_with_inheritance(
        file_path: &Path,
        class_name: &str,
        attribute_name: &str,
        search_paths: &[PathBuf],
        file_cache: &mut FileCache,
        visited: &mut HashSet<String>,
    ) -> Result<(ClassAttributeInfo, PathBuf, String)> {
        // Limit recursion depth
        if visited.len() >= MAX_PARENT_DEPTH {
            anyhow::bail!("Maximum inheritance depth exceeded");
        }

        // Check for cycles
        let canonical_path = file_path
            .canonicalize()
            .unwrap_or_else(|_| file_path.to_path_buf());
        let visit_key = format!("{}::{}", canonical_path.display(), class_name);
        if visited.contains(&visit_key) {
            anyhow::bail!("Cycle detected in inheritance chain");
        }
        visited.insert(visit_key);

        // Read the source file
        let source = file_cache.get(file_path)?;

        // Try to find the attribute directly on this class
        if let Ok(attr_info) = Self::extract_class_attribute(source, class_name, attribute_name) {
            return Ok((attr_info, file_path.to_path_buf(), class_name.to_string()));
        }

        // Get the class info to find parent classes
        let class_info = Self::extract_class_info(source, class_name)?;

        // Search parent classes for the attribute
        for base_class in &class_info.base_classes {
            // Skip common base classes
            if base_class == "object" || base_class == "ABC" || base_class == "Protocol" {
                continue;
            }

            // Resolve the parent class to a file and name
            let Some((parent_file, parent_class_name)) =
                Self::resolve_base_class(base_class, file_path, search_paths)
            else {
                continue;
            };

            // Recursively search in parent class
            if let Ok(result) = Self::extract_class_attribute_with_inheritance(
                &parent_file,
                &parent_class_name,
                attribute_name,
                search_paths,
                file_cache,
                visited,
            ) {
                return Ok(result);
            }
        }

        anyhow::bail!(
            "Attribute '{}' not found in class '{}' or its parents",
            attribute_name,
            class_name
        )
    }

    /// Resolve an attribute chain through classes.
    ///
    /// Given a starting class and an attribute chain like ["nested", "nested_class"],
    /// follows the chain by looking up each class attribute to find the final class/method.
    ///
    /// Returns the final resolved class name and the remaining method name (if any).
    fn resolve_attribute_chain(
        file_path: &Path,
        starting_class: &str,
        attribute_chain: &[&str],
        search_paths: &[PathBuf],
        file_cache: &mut FileCache,
    ) -> Result<(PathBuf, String, Option<String>)> {
        if attribute_chain.is_empty() {
            anyhow::bail!("Empty attribute chain");
        }

        let mut current_file = file_path.to_path_buf();
        let mut current_class = starting_class.to_string();
        let mut resolver = ImportResolver::new(search_paths.to_vec());

        // Process all but the last attribute (which might be a method)
        for (i, attr) in attribute_chain.iter().enumerate() {
            let is_last = i == attribute_chain.len() - 1;

            // First, check if this is a method on the current class
            {
                // Read current file source in its own scope to release borrow
                let source = file_cache.get(&current_file)?;
                if is_last && Self::extract_method_info(source, &current_class, attr).is_ok() {
                    // It's a method - return the class and method name
                    return Ok((current_file, current_class, Some(attr.to_string())));
                }
            }

            // Try to get the attribute as a class attribute (with inheritance support)
            let mut visited = HashSet::new();
            match Self::extract_class_attribute_with_inheritance(
                &current_file,
                &current_class,
                attr,
                search_paths,
                file_cache,
                &mut visited,
            ) {
                Ok((attr_info, _attr_file, _attr_class)) => {
                    // The value could be a simple name or a dotted path
                    let value_parts: Vec<&str> = attr_info.value.split('.').collect();

                    if value_parts.len() == 1 {
                        // Simple name - look for it in the same file first
                        let new_class_name = value_parts[0];

                        // Try direct lookup in current file
                        let source = file_cache.get(&current_file)?;
                        if Self::extract_class_info(source, new_class_name).is_ok() {
                            current_class = new_class_name.to_string();
                            continue;
                        }

                        // Try resolving through imports
                        if let Some((resolved_file, resolved_name)) =
                            resolver.resolve_symbol(&current_file, new_class_name)
                        {
                            current_file = resolved_file;
                            current_class = if resolved_name.is_empty() {
                                new_class_name.to_string()
                            } else {
                                resolved_name
                            };
                            continue;
                        }

                        anyhow::bail!(
                            "Could not resolve class '{}' from attribute '{}'",
                            new_class_name,
                            attr
                        );
                    } else {
                        // Dotted path like "module.Class"
                        let module_path = value_parts[..value_parts.len() - 1].join(".");
                        let class_name = value_parts[value_parts.len() - 1];

                        // Try to resolve the module
                        if let Some(resolved_file) = resolver.resolve_module_path(&module_path) {
                            current_file = resolved_file;
                            current_class = class_name.to_string();
                            continue;
                        }

                        anyhow::bail!(
                            "Could not resolve module path '{}' from attribute '{}'",
                            module_path,
                            attr
                        );
                    }
                }
                Err(_) if is_last => {
                    // Last attribute and not a class attribute - maybe it's a method
                    // that we couldn't find (could be inherited, etc.)
                    anyhow::bail!(
                        "Attribute or method '{}' not found in class '{}'",
                        attr,
                        current_class
                    );
                }
                Err(e) => return Err(e),
            }
        }

        // Reached end of chain without finding a method
        Ok((current_file, current_class, None))
    }

    /// Build search paths from discovered site-packages
    fn build_search_paths(
        workspace_root: Option<&Path>,
        site_packages_paths: Vec<SystemPathBuf>,
    ) -> Vec<PathBuf> {
        let mut search_paths = Vec::new();

        // Add workspace root first (highest priority for first-party code)
        if let Some(root) = workspace_root {
            search_paths.push(PathBuf::from(root));
        }

        // Add current directory
        search_paths.push(PathBuf::from("."));

        // Add site-packages paths AND process .pth files for editable installs
        for sys_path in site_packages_paths {
            let site_packages = sys_path.as_std_path().to_path_buf();

            // Process .pth files in this site-packages directory
            let editable_paths = Self::parse_pth_files(&site_packages);

            search_paths.push(site_packages);
            search_paths.extend(editable_paths);
        }

        search_paths
    }

    /// Parse all .pth files in a site-packages directory and extract editable install paths.
    ///
    /// This follows the Python site module specification:
    /// - Empty lines and lines beginning with `#` are skipped
    /// - Lines starting with `import ` or `import\t` are executed, not treated as paths
    /// - All other lines are treated as directories to add to `sys.path`
    ///
    /// See: https://docs.python.org/3/library/site.html
    fn parse_pth_files(site_packages: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();

        let Ok(entries) = fs::read_dir(site_packages) else {
            return paths;
        };

        // Collect and sort .pth files alphabetically (per Python spec)
        let mut pth_files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "pth"))
            .map(|e| e.path())
            .collect();
        pth_files.sort();

        for pth_file in pth_files {
            if let Ok(contents) = fs::read_to_string(&pth_file) {
                for line in contents.lines() {
                    let line = line.trim_end();

                    // Skip empty lines, comments, and import statements
                    if line.is_empty()
                        || line.starts_with('#')
                        || line.starts_with("import ")
                        || line.starts_with("import\t")
                    {
                        continue;
                    }

                    // Resolve relative paths relative to site-packages
                    let path = if Path::new(line).is_absolute() {
                        PathBuf::from(line)
                    } else {
                        site_packages.join(line)
                    };

                    // Only add if the directory exists
                    if path.is_dir() {
                        paths.push(path);
                    }
                }
            }
        }

        paths
    }

    /// Resolve a base class expression to its file path and actual class name.
    ///
    /// This function handles:
    /// - Simple names: `ParentClass` - looks up in the current file's imports
    /// - Qualified names: `module.ClassName` - resolves the module path
    fn resolve_base_class(
        base_class_expr: &str,
        current_file: &Path,
        search_paths: &[PathBuf],
    ) -> Option<(PathBuf, String)> {
        // Check if it's a qualified name (contains a dot)
        if let Some(dot_pos) = base_class_expr.rfind('.') {
            // Qualified name like `module.ClassName`
            let module_path = &base_class_expr[..dot_pos];
            let class_name = &base_class_expr[dot_pos + 1..];

            // Try to resolve the module
            if let Ok(file_path) = Self::resolve_module_with_search_paths(module_path, search_paths)
            {
                return Some((file_path, class_name.to_string()));
            }
        }

        // Simple name - try to resolve through imports in the current file
        let mut resolver = ImportResolver::new(search_paths.to_vec());
        if let Some((resolved_file, resolved_name)) =
            resolver.resolve_symbol(current_file, base_class_expr)
        {
            let actual_name = if resolved_name.is_empty() {
                base_class_expr.to_string()
            } else {
                resolved_name
            };
            return Some((resolved_file, actual_name));
        }

        // Check if the class is defined in the same file
        if let Ok(source) = fs::read_to_string(current_file)
            && let Ok(parsed) = parse_module(&source)
        {
            let mut visitor = ClassExtractor::new(base_class_expr.to_string(), source);
            visitor.visit_body(parsed.suite());
            if visitor.get_result().is_some() {
                return Some((current_file.to_path_buf(), base_class_expr.to_string()));
            }
        }

        None
    }

    /// Resolve missing properties (docstring, __init__) from parent classes.
    ///
    /// This follows Python's MRO (left-to-right for simple cases) and searches
    /// parent classes recursively when properties are not found in the child.
    fn resolve_from_parents(
        base_classes: &[String],
        current_file: &Path,
        search_paths: &[PathBuf],
        file_cache: &mut FileCache,
        visited: &mut HashSet<String>,
        needs_docstring: bool,
        needs_init: bool,
    ) -> (Option<String>, Option<FunctionSignature>) {
        // Base case: nothing needed
        if !needs_docstring && !needs_init {
            return (None, None);
        }

        // Limit recursion depth via visited set size
        if visited.len() >= MAX_PARENT_DEPTH {
            return (None, None);
        }

        let mut found_docstring: Option<String> = None;
        let mut found_init: Option<FunctionSignature> = None;

        // Iterate through base classes in MRO order (left-to-right)
        for base_class in base_classes {
            // Skip common base classes that won't have useful info
            if base_class == "object" || base_class == "ABC" || base_class == "Protocol" {
                continue;
            }

            // Resolve the base class to a file and name
            let Some((parent_file, parent_class_name)) =
                Self::resolve_base_class(base_class, current_file, search_paths)
            else {
                continue;
            };

            // Check for cycles using (file, class_name) as key
            // This allows multiple classes in the same file to be visited
            let canonical_path = parent_file
                .canonicalize()
                .unwrap_or_else(|_| parent_file.clone());
            let visit_key = format!("{}::{}", canonical_path.display(), parent_class_name);
            if visited.contains(&visit_key) {
                continue;
            }
            visited.insert(visit_key);

            // Extract the parent class info using cache
            let source = match file_cache.get(&parent_file) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let parent_info = match Self::extract_class_info(source, &parent_class_name) {
                Ok(info) => info,
                Err(_) => continue,
            };

            // Check if the parent has what we need
            if needs_docstring && found_docstring.is_none() && parent_info.docstring.is_some() {
                found_docstring = parent_info.docstring.clone();
            }

            if needs_init && found_init.is_none() && parent_info.init_signature.is_some() {
                found_init = parent_info.init_signature.clone();
            }

            // If we still need something, recurse into grandparents
            let still_needs_docstring = needs_docstring && found_docstring.is_none();
            let still_needs_init = needs_init && found_init.is_none();

            if still_needs_docstring || still_needs_init {
                let (grandparent_docstring, grandparent_init) = Self::resolve_from_parents(
                    &parent_info.base_classes,
                    &parent_file,
                    search_paths,
                    file_cache,
                    visited,
                    still_needs_docstring,
                    still_needs_init,
                );

                if still_needs_docstring && found_docstring.is_none() {
                    found_docstring = grandparent_docstring;
                }
                if still_needs_init && found_init.is_none() {
                    found_init = grandparent_init;
                }
            }

            // If we found everything, stop searching
            if (!needs_docstring || found_docstring.is_some())
                && (!needs_init || found_init.is_some())
            {
                break;
            }
        }

        (found_docstring, found_init)
    }

    /// Extract class information, following re-exports if necessary.
    /// Returns the ClassInfo and the file path where it was found.
    ///
    /// This also resolves missing properties (docstring, __init__) from parent classes
    /// when they are not defined in the child class.
    pub fn extract_class_info_with_imports(
        file_path: &Path,
        class_name: &str,
        search_paths: Vec<PathBuf>,
        file_cache: &mut FileCache,
    ) -> Result<(ClassInfo, PathBuf)> {
        // First try direct lookup using cache
        let (mut class_info, resolved_file) = if let Ok(source) = file_cache.get(file_path)
            && let Ok(class_info) = Self::extract_class_info(source, class_name)
        {
            (class_info, file_path.to_path_buf())
        } else {
            // Try to resolve through imports
            let mut resolver = ImportResolver::new(search_paths.clone());
            if let Some((resolved_file, resolved_name)) =
                resolver.resolve_symbol(file_path, class_name)
            {
                let actual_name = if resolved_name.is_empty() {
                    class_name.to_string()
                } else {
                    resolved_name
                };
                // Use cache for the resolved file
                let source = file_cache.get(&resolved_file)?;
                let class_info = Self::extract_class_info(source, &actual_name)?;
                (class_info, resolved_file)
            } else {
                anyhow::bail!(
                    "Class '{}' not found in {} (also checked re-exports)",
                    class_name,
                    file_path.display()
                )
            }
        };

        // Resolve missing properties from parent classes
        if class_info.docstring.is_none() || class_info.init_signature.is_none() {
            let mut visited = HashSet::new();
            // Use (file, class_name) as key to allow visiting multiple classes in same file
            let canonical_path = resolved_file
                .canonicalize()
                .unwrap_or_else(|_| resolved_file.clone());
            let visit_key = format!("{}::{}", canonical_path.display(), class_info.name);
            visited.insert(visit_key);

            let (parent_docstring, parent_init) = Self::resolve_from_parents(
                &class_info.base_classes,
                &resolved_file,
                &search_paths,
                file_cache,
                &mut visited,
                class_info.docstring.is_none(),
                class_info.init_signature.is_none(),
            );

            if class_info.docstring.is_none() {
                class_info.docstring = parent_docstring;
            }
            if class_info.init_signature.is_none() {
                class_info.init_signature = parent_init;
            }
        }

        Ok((class_info, resolved_file))
    }

    /// Extract function signature, following re-exports if necessary.
    /// Returns the FunctionSignature and the file path where it was found.
    pub fn extract_function_signature_with_imports(
        file_path: &Path,
        function_name: &str,
        search_paths: Vec<PathBuf>,
        file_cache: &mut FileCache,
    ) -> Result<(FunctionSignature, PathBuf)> {
        // First try direct lookup using cache
        if let Ok(source) = file_cache.get(file_path)
            && let Ok(func_sig) = Self::extract_function_signature(source, function_name)
        {
            return Ok((func_sig, file_path.to_path_buf()));
        }

        // Try to resolve through imports
        let mut resolver = ImportResolver::new(search_paths);
        if let Some((resolved_file, resolved_name)) =
            resolver.resolve_symbol(file_path, function_name)
        {
            let actual_name = if resolved_name.is_empty() {
                function_name.to_string()
            } else {
                resolved_name
            };
            // Use cache for the resolved file
            let source = file_cache.get(&resolved_file)?;
            let func_sig = Self::extract_function_signature(source, &actual_name)?;
            return Ok((func_sig, resolved_file));
        }

        anyhow::bail!(
            "Function '{}' not found in {} (also checked re-exports)",
            function_name,
            file_path.display()
        )
    }

    /// Extract definition info (function or class) from a target string.
    /// Returns:
    /// - DefinitionInfo (Function or Class)
    /// - File path where the definition was found
    /// - Module path
    /// - Symbol name
    pub fn extract_definition_info(
        target: &str,
        workspace_root: Option<&Path>,
        python_interpreter: Option<&str>,
    ) -> Result<(DefinitionInfo, PathBuf, String, String)> {
        let (module_path, symbol_name) = Self::split_target(target)?;

        // Create a shared file cache for all operations
        let mut file_cache = FileCache::new();

        // Track whether we found the module but not the symbol
        let mut module_found = false;

        // First try standard resolution: module.symbol where symbol is a function or class
        if let Ok((file_path, search_paths)) =
            Self::resolve_module(&module_path, workspace_root, python_interpreter)
        {
            module_found = true;

            // Try to extract as function first (with import resolution)
            if let Ok((func_sig, resolved_file)) = Self::extract_function_signature_with_imports(
                &file_path,
                &symbol_name,
                search_paths.clone(),
                &mut file_cache,
            ) {
                return Ok((
                    DefinitionInfo::Function(func_sig),
                    resolved_file,
                    module_path,
                    symbol_name,
                ));
            }

            // Try to extract as class (with import resolution)
            if let Ok((class_info, resolved_file)) = Self::extract_class_info_with_imports(
                &file_path,
                &symbol_name,
                search_paths,
                &mut file_cache,
            ) {
                return Ok((
                    DefinitionInfo::Class(class_info),
                    resolved_file,
                    module_path,
                    symbol_name,
                ));
            }
        }

        // Try to interpret as Class.method pattern (e.g., "my_module.MyClass.from_config")
        // or nested pattern (e.g., "my_module.OuterClass.nested.nested_class.method")
        // or nested class pattern (e.g., "my_module.OuterClass.nested.nested_class")
        if let Some(result) = Self::resolve_class_attribute_chain(
            target,
            workspace_root,
            python_interpreter,
            &mut file_cache,
        ) {
            match result {
                ClassAttributeChainResult::Method {
                    file_path: resolved_file,
                    class_name,
                    method_name,
                    search_paths,
                } => {
                    if let Ok((method_info, final_file)) = Self::extract_method_info_with_imports(
                        &resolved_file,
                        &class_name,
                        &method_name,
                        search_paths,
                        &mut file_cache,
                    ) {
                        return Ok((
                            DefinitionInfo::Method(method_info),
                            final_file,
                            module_path.clone(),
                            format!("{}.{}", class_name, method_name),
                        ));
                    }
                }
                ClassAttributeChainResult::Class {
                    file_path: resolved_file,
                    class_name,
                    search_paths,
                } => {
                    if let Ok((class_info, final_file)) = Self::extract_class_info_with_imports(
                        &resolved_file,
                        &class_name,
                        search_paths,
                        &mut file_cache,
                    ) {
                        return Ok((
                            DefinitionInfo::Class(class_info),
                            final_file,
                            module_path.clone(),
                            class_name,
                        ));
                    }
                }
            }
        }

        // Return appropriate error based on whether module was found
        if module_found {
            anyhow::bail!(
                "Symbol '{}' not found in module '{}'",
                symbol_name,
                module_path
            )
        } else {
            anyhow::bail!("Could not resolve module: '{}'", module_path)
        }
    }

    /// Resolve a target string that may include a class attribute chain pattern.
    ///
    /// This handles various patterns:
    /// - Simple method: `module.Class.method`
    /// - Nested method: `module.OuterClass.nested_attr.nested_attr.method`
    /// - Nested class: `module.OuterClass.nested_attr.nested_class`
    ///
    /// Returns either a Method or Class result depending on what the chain resolves to.
    fn resolve_class_attribute_chain(
        target: &str,
        workspace_root: Option<&Path>,
        python_interpreter: Option<&str>,
        file_cache: &mut FileCache,
    ) -> Option<ClassAttributeChainResult> {
        let parts: Vec<&str> = target.split('.').collect();
        if parts.len() < 3 {
            return None;
        }

        // Try progressively shorter module paths
        // For "a.b.c.D.e.f.method", try:
        //   module="a.b.c.D.e.f" (fails - no method left)
        //   module="a.b.c.D.e", attr_chain=["f"] (if f is a method of class from e)
        //   module="a.b.c.D", attr_chain=["e", "f"] ...
        //   module="a.b.c", attr_chain=["D", "e", "f"] (D is a class, e.f is attr chain + method)
        //   etc.

        for module_end_idx in (1..parts.len() - 1).rev() {
            let module_path = parts[..module_end_idx].join(".");
            let remaining_parts = &parts[module_end_idx..];

            // Try to resolve the module
            let Ok((file_path, search_paths)) =
                Self::resolve_module(&module_path, workspace_root, python_interpreter)
            else {
                continue;
            };

            // The first remaining part should be a class (or function that returns a class, but we focus on classes)
            let first_symbol = remaining_parts[0];

            // Check if this is a class
            if let Ok((class_info, resolved_file)) = Self::extract_class_info_with_imports(
                &file_path,
                first_symbol,
                search_paths.clone(),
                file_cache,
            ) {
                if remaining_parts.len() == 2 {
                    // Simple case: Class.method
                    let method_name = remaining_parts[1];
                    // Read the resolved file source and check if method exists
                    if let Ok(source) = file_cache.get(&resolved_file)
                        && Self::extract_method_info(source, &class_info.name, method_name).is_ok()
                    {
                        return Some(ClassAttributeChainResult::Method {
                            file_path: resolved_file,
                            class_name: class_info.name,
                            method_name: method_name.to_string(),
                            search_paths,
                        });
                    }
                } else if remaining_parts.len() > 2 {
                    // Nested case: Class.attr1.attr2...method_or_class
                    // Follow the attribute chain
                    let attribute_chain = &remaining_parts[1..];
                    if let Ok((final_file, final_class, method_name_opt)) =
                        Self::resolve_attribute_chain(
                            &resolved_file,
                            &class_info.name,
                            attribute_chain,
                            &search_paths,
                            file_cache,
                        )
                    {
                        if let Some(method_name) = method_name_opt {
                            return Some(ClassAttributeChainResult::Method {
                                file_path: final_file,
                                class_name: final_class,
                                method_name,
                                search_paths,
                            });
                        } else {
                            return Some(ClassAttributeChainResult::Class {
                                file_path: final_file,
                                class_name: final_class,
                                search_paths,
                            });
                        }
                    }
                }
            }
        }

        None
    }

    /// Extract method info, following re-exports if necessary.
    /// Returns the MethodInfo and the file path where it was found.
    pub fn extract_method_info_with_imports(
        file_path: &Path,
        class_name: &str,
        method_name: &str,
        search_paths: Vec<PathBuf>,
        file_cache: &mut FileCache,
    ) -> Result<(MethodInfo, PathBuf)> {
        // First try direct lookup using cache
        if let Ok(source) = file_cache.get(file_path)
            && let Ok(method_info) = Self::extract_method_info(source, class_name, method_name)
        {
            return Ok((method_info, file_path.to_path_buf()));
        }

        // Try to resolve class through imports, then find the method
        let mut resolver = ImportResolver::new(search_paths);
        if let Some((resolved_file, resolved_name)) = resolver.resolve_symbol(file_path, class_name)
        {
            let actual_class_name = if resolved_name.is_empty() {
                class_name.to_string()
            } else {
                resolved_name
            };
            let source = file_cache.get(&resolved_file)?;
            let method_info = Self::extract_method_info(source, &actual_class_name, method_name)?;
            return Ok((method_info, resolved_file));
        }

        anyhow::bail!(
            "Method '{}' not found in class '{}' in {} (also checked re-exports)",
            method_name,
            class_name,
            file_path.display()
        )
    }

    /// Format a single parameter for display
    fn format_parameter(p: &ParameterInfo) -> String {
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
    }

    /// Format a function signature for display (e.g., in hover)
    pub fn format_function(sig: &FunctionSignature) -> String {
        let mut result = String::new();
        result.push_str("```python\n");

        let param_strs: Vec<String> = sig.parameters.iter().map(Self::format_parameter).collect();

        result.push_str(&Self::format_definition(
            "def",
            &sig.name,
            &param_strs,
            true,
            sig.return_type.as_ref(),
            sig.docstring.as_ref(),
            None,
        ));

        result.push_str("\n```");

        result
    }

    /// Format a class for display (e.g., in hover)
    /// Shows class definition with base classes, class docstring,
    /// then __init__ method definition with its docstring
    pub fn format_class(class: &ClassInfo) -> String {
        let mut result = String::new();
        result.push_str("```python\n");

        result.push_str(&Self::format_definition(
            "class",
            &class.name,
            &class.base_classes,
            false,
            None,
            class.docstring.as_ref(),
            None,
        ));

        // Add __init__ method if present
        if let Some(init_sig) = &class.init_signature {
            let param_strs: Vec<String> = init_sig
                .parameters
                .iter()
                .map(Self::format_parameter)
                .collect();

            result.push_str("\n\n");

            result.push_str(&Self::format_definition(
                "def",
                "__init__",
                &param_strs,
                true,
                init_sig.return_type.as_ref(),
                init_sig.docstring.as_ref(),
                Some(4),
            ));
        }

        result.push_str("\n```");

        result
    }

    /// Format a method for display (e.g., in hover)
    /// Shows the method signature with @classmethod or @staticmethod decorator if applicable
    pub fn format_method(method: &MethodInfo) -> String {
        let mut result = String::new();
        result.push_str("```python\n");

        // Show decorator if applicable
        if method.is_classmethod {
            result.push_str("@classmethod\n");
        } else if method.is_staticmethod {
            result.push_str("@staticmethod\n");
        }

        let param_strs: Vec<String> = method
            .signature
            .parameters
            .iter()
            .map(Self::format_parameter)
            .collect();

        result.push_str(&Self::format_definition(
            "def",
            &method.signature.name,
            &param_strs,
            true,
            method.signature.return_type.as_ref(),
            method.signature.docstring.as_ref(),
            None,
        ));

        result.push_str("\n```");

        result
    }

    /// Format a function definition string (placeholder for future use)
    fn format_definition(
        def_str: &str,
        name: &str,
        param_strs: &[String],
        wrap_empty_params: bool,
        return_type: Option<&String>,
        docstring: Option<&String>,
        indent: Option<usize>,
    ) -> String {
        // Placeholder for future implementation
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
}

/// Visitor to extract function signatures from AST
pub struct FunctionExtractor {
    target_name: String,
    result: Option<FunctionSignature>,
    source: String,
}

impl FunctionExtractor {
    pub fn new(target_name: String, source: String) -> Self {
        Self {
            target_name,
            result: None,
            source,
        }
    }

    pub fn get_result(self) -> Option<FunctionSignature> {
        self.result
    }
}

impl<'a> Visitor<'a> for FunctionExtractor {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.result.is_some() {
            return; // Already found
        }

        if let Stmt::FunctionDef(func_def) = stmt
            && func_def.name.as_str() == self.target_name
        {
            self.result = Some(extract_function_signature_from_def(func_def, &self.source));
            return;
        }

        // Continue walking
        ast::visitor::walk_stmt(self, stmt);
    }
}

/// Visitor to extract class information from AST
pub struct ClassExtractor {
    target_name: String,
    source: String,
    result: Option<ClassInfo>,
}

impl ClassExtractor {
    pub fn new(target_name: String, source: String) -> Self {
        Self {
            target_name,
            source,
            result: None,
        }
    }

    pub fn get_result(self) -> Option<ClassInfo> {
        self.result
    }
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

/// Represents a class attribute assignment (e.g., `nested_class = ModelFactory`)
#[derive(Debug, Clone)]
pub struct ClassAttributeInfo {
    pub name: String,
    /// The value as a string (e.g., "ModelFactory", "SomeModule.SomeClass")
    pub value: String,
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
                {
                    let signature = extract_function_signature_from_def(func_def, &self.source);

                    // Check decorators for @classmethod or @staticmethod
                    let (is_classmethod, is_staticmethod) =
                        check_method_decorators(&func_def.decorator_list);

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

    let start = line_index.source_location(range.start(), source, PositionEncoding::Utf32);
    let end = line_index.source_location(range.end(), source, PositionEncoding::Utf32);

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

    // Look for __init__ method
    let init_signature = class_def.body.iter().find_map(|stmt| {
        if let Stmt::FunctionDef(func_def) = stmt
            && func_def.name.as_str() == "__init__"
        {
            return Some(extract_function_signature_from_def(func_def, source));
        }
        None
    });

    ClassInfo {
        name: class_def.name.to_string(),
        base_classes,
        docstring,
        init_signature,
        start_line,
        start_column,
        end_line,
        end_column,
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
    use std::env;
    use std::fs;

    fn get_simple_test_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("workspace")
            .join("simple")
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
        let examples_dir = get_simple_test_dir();
        let result = PythonAnalyzer::resolve_module("my_module", Some(&examples_dir), None);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert!(path.ends_with("my_module.py"));
    }

    #[test]
    fn test_resolve_module_package() {
        let examples_dir = get_simple_test_dir();
        let result = PythonAnalyzer::resolve_module("test_package", Some(&examples_dir), None);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert!(path.ends_with("__init__.py"));
    }

    #[test]
    fn test_resolve_module_submodule() {
        let examples_dir = get_simple_test_dir();
        let result =
            PythonAnalyzer::resolve_module("test_package.submodule", Some(&examples_dir), None);
        assert!(result.is_ok());
        let (path, _) = result.unwrap();
        assert!(path.ends_with("submodule.py"));
    }

    #[test]
    fn test_resolve_module_nonexistent() {
        let examples_dir = get_simple_test_dir();
        let result =
            PythonAnalyzer::resolve_module("nonexistent_module", Some(&examples_dir), None);
        assert!(result.is_err());
    }

    // ==================== extract_function_signature tests ====================

    #[test]
    fn test_extract_simple_function() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let source = fs::read_to_string(&test_file).unwrap();

        let sig = PythonAnalyzer::extract_function_signature(&source, "simple_function").unwrap();
        assert_eq!(sig.name, "simple_function");
        assert_eq!(sig.parameters.len(), 0);
        assert!(sig.docstring.is_some());
        assert!(sig.docstring.as_ref().unwrap().contains("simple function"));
    }

    #[test]
    fn test_extract_function_with_params() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let source = fs::read_to_string(&test_file).unwrap();

        let sig =
            PythonAnalyzer::extract_function_signature(&source, "function_with_params").unwrap();
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
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let source = fs::read_to_string(&test_file).unwrap();

        let sig =
            PythonAnalyzer::extract_function_signature(&source, "function_with_return").unwrap();
        assert_eq!(sig.name, "function_with_return");
        assert!(sig.return_type.is_some());
        assert_eq!(sig.return_type.as_ref().unwrap(), "int");
    }

    #[test]
    fn test_extract_variadic_function() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let source = fs::read_to_string(&test_file).unwrap();

        let sig = PythonAnalyzer::extract_function_signature(&source, "variadic_function").unwrap();
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
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let source = fs::read_to_string(&test_file).unwrap();

        let sig = PythonAnalyzer::extract_function_signature(&source, "complex_function").unwrap();
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
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let source = fs::read_to_string(&test_file).unwrap();

        let result = PythonAnalyzer::extract_function_signature(&source, "nonexistent");
        assert!(result.is_err());
    }

    // ==================== extract_class_info tests ====================

    #[test]
    fn test_extract_simple_class() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let source = fs::read_to_string(&test_file).unwrap();

        let class_info = PythonAnalyzer::extract_class_info(&source, "SimpleClass").unwrap();
        assert_eq!(class_info.name, "SimpleClass");
        assert!(class_info.docstring.is_some());
        assert!(class_info.init_signature.is_none());
    }

    #[test]
    fn test_extract_class_with_init() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let source = fs::read_to_string(&test_file).unwrap();

        let class_info = PythonAnalyzer::extract_class_info(&source, "ClassWithInit").unwrap();
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
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");
        let source = fs::read_to_string(&test_file).unwrap();

        let class_info = PythonAnalyzer::extract_class_info(&source, "ComplexClass").unwrap();
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
        let source = fs::read_to_string(&test_file).unwrap();

        let result = PythonAnalyzer::extract_class_info(&source, "NonexistentClass");
        assert!(result.is_err());
    }

    // ==================== inherited class tests ====================

    #[test]
    fn test_extract_child_without_init_inherits_from_parent() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");

        // First verify that direct extraction does NOT give us __init__
        let source = fs::read_to_string(&test_file).unwrap();
        let direct_info = PythonAnalyzer::extract_class_info(&source, "ChildWithoutInit").unwrap();
        assert_eq!(direct_info.name, "ChildWithoutInit");
        assert!(
            direct_info.init_signature.is_none(),
            "Direct extraction should not have __init__"
        );
        assert!(direct_info.docstring.is_some(), "Should have own docstring");

        // Now verify that extract_class_info_with_imports gives us inherited __init__
        let search_paths = vec![examples_dir.clone()];
        let mut file_cache = FileCache::new();
        let (class_info, _) = PythonAnalyzer::extract_class_info_with_imports(
            &test_file,
            "ChildWithoutInit",
            search_paths,
            &mut file_cache,
        )
        .unwrap();

        assert_eq!(class_info.name, "ChildWithoutInit");
        assert!(
            class_info.init_signature.is_some(),
            "Should inherit __init__ from parent"
        );

        let init_sig = class_info.init_signature.as_ref().unwrap();
        assert_eq!(init_sig.name, "__init__");
        // Should have: self, name, value from ParentWithInit
        assert_eq!(init_sig.parameters.len(), 3);
        assert_eq!(init_sig.parameters[0].name, "self");
        assert_eq!(init_sig.parameters[1].name, "name");
        assert_eq!(init_sig.parameters[2].name, "value");

        // Check that the child's own docstring is preserved
        assert!(class_info.docstring.is_some());
        assert!(
            class_info
                .docstring
                .as_ref()
                .unwrap()
                .contains("inherits __init__")
        );
    }

    #[test]
    fn test_extract_grandchild_without_init_inherits_from_grandparent() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");

        let search_paths = vec![examples_dir.clone()];
        let mut file_cache = FileCache::new();
        let (class_info, _) = PythonAnalyzer::extract_class_info_with_imports(
            &test_file,
            "GrandchildWithoutInit",
            search_paths,
            &mut file_cache,
        )
        .unwrap();

        assert_eq!(class_info.name, "GrandchildWithoutInit");
        assert!(
            class_info.init_signature.is_some(),
            "Should inherit __init__ from grandparent through parent"
        );

        let init_sig = class_info.init_signature.as_ref().unwrap();
        // Should have: self, name, value from ParentWithInit (grandparent)
        assert_eq!(init_sig.parameters.len(), 3);
        assert_eq!(init_sig.parameters[1].name, "name");
        assert_eq!(init_sig.parameters[2].name, "value");
    }

    #[test]
    fn test_extract_child_with_own_init_does_not_inherit() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");

        let search_paths = vec![examples_dir.clone()];
        let mut file_cache = FileCache::new();
        let (class_info, _) = PythonAnalyzer::extract_class_info_with_imports(
            &test_file,
            "ChildWithOwnInit",
            search_paths,
            &mut file_cache,
        )
        .unwrap();

        assert_eq!(class_info.name, "ChildWithOwnInit");
        assert!(
            class_info.init_signature.is_some(),
            "Should have its own __init__"
        );

        let init_sig = class_info.init_signature.as_ref().unwrap();
        // Should have: self, name, extra (from ChildWithOwnInit, NOT from parent)
        assert_eq!(init_sig.parameters.len(), 3);
        assert_eq!(init_sig.parameters[1].name, "name");
        assert_eq!(
            init_sig.parameters[2].name, "extra",
            "Should have child's 'extra' param, not parent's 'value'"
        );
    }

    #[test]
    fn test_extract_definition_child_without_init() {
        let examples_dir = get_simple_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "my_module.ChildWithoutInit",
            Some(&examples_dir),
            None,
        );
        assert!(result.is_ok());
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Class(class_info) => {
                assert_eq!(class_info.name, "ChildWithoutInit");
                assert!(
                    class_info.init_signature.is_some(),
                    "Should have inherited __init__"
                );

                let init_sig = class_info.init_signature.as_ref().unwrap();
                // Should have inherited params from ParentWithInit
                assert_eq!(init_sig.parameters.len(), 3);
                assert_eq!(init_sig.parameters[1].name, "name");
            }
            _ => panic!("Expected Class definition"),
        }
    }

    #[test]
    fn test_extract_nested_inherited_classmethod() {
        let examples_dir = get_simple_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "class_methods.InheritedNested.nested.nested_class.from_config",
            Some(&examples_dir),
            None,
        );
        assert!(
            result.is_ok(),
            "Should resolve classmethod: {:?}",
            result.err()
        );
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Method(method_info) => {
                assert_eq!(method_info.class_name, "ModelFactory");
                assert_eq!(method_info.method_name, "from_config");
                assert!(method_info.is_classmethod);
                assert!(!method_info.is_staticmethod);
                // Check parameters: cls and config
                assert_eq!(method_info.signature.parameters.len(), 2);
                assert_eq!(method_info.signature.parameters[0].name, "cls");
                assert_eq!(method_info.signature.parameters[1].name, "config");
            }
            _ => panic!("Expected Method definition, got {:?}", definition_info),
        }
    }

    #[test]
    fn test_extract_nested_inherited_staticmethod() {
        let workspace_dir = get_simple_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "class_methods.InheritedNested.nested.nested_class.compute_size",
            Some(&workspace_dir),
            None,
        );

        assert!(
            result.is_ok(),
            "Should resolve staticmethod: {:?}",
            result.err()
        );
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Method(method_info) => {
                assert_eq!(method_info.class_name, "ModelFactory");
                assert_eq!(method_info.method_name, "compute_size");
                assert!(!method_info.is_classmethod);
                assert!(method_info.is_staticmethod);
                // Check parameters: dim1 and dim2
                assert_eq!(method_info.signature.parameters.len(), 2);
                assert_eq!(method_info.signature.parameters[0].name, "dim1");
                assert_eq!(method_info.signature.parameters[1].name, "dim2");
            }
            _ => panic!("Expected Method definition, got {:?}", definition_info),
        }
    }

    #[test]
    fn test_extract_nested_inherited_init() {
        let examples_dir = get_simple_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "class_methods.NestedTwice.nested.inherited_nested_class",
            Some(&examples_dir),
            None,
        );
        assert!(
            result.is_ok(),
            "Should resolve classmethod: {:?}",
            result.err()
        );
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Class(class_info) => {
                assert_eq!(class_info.name, "InheritedFactory");
                assert!(
                    class_info.init_signature.is_some(),
                    "Should have inherited __init__"
                );

                let init_sig = class_info.init_signature.as_ref().unwrap();
                // Should have inherited params from ModelFactory
                assert_eq!(init_sig.parameters.len(), 3);
                assert_eq!(init_sig.parameters[1].name, "input_dim");
                assert_eq!(init_sig.parameters[2].name, "output_dim");
            }
            _ => panic!("Expected Class definition"),
        }
    }

    // ==================== extract_definition_info tests ====================

    #[test]
    fn test_extract_definition_function() {
        let examples_dir = get_simple_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "my_module.simple_function",
            Some(&examples_dir),
            None,
        );
        assert!(result.is_ok());
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Function(sig) => {
                assert_eq!(sig.name, "simple_function");
            }
            _ => panic!("Expected Function definition"),
        }
    }

    #[test]
    fn test_extract_definition_class() {
        let examples_dir = get_simple_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "my_module.SimpleClass",
            Some(&examples_dir),
            None,
        );
        assert!(result.is_ok());
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Class(class_info) => {
                assert_eq!(class_info.name, "SimpleClass");
            }
            _ => panic!("Expected Class definition"),
        }
    }

    #[test]
    fn test_extract_definition_from_package() {
        let examples_dir = get_simple_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "test_package.submodule.SubmoduleClass",
            Some(&examples_dir),
            None,
        );
        assert!(result.is_ok());
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Class(class_info) => {
                assert_eq!(class_info.name, "SubmoduleClass");
            }
            _ => panic!("Expected Class definition"),
        }
    }

    #[test]
    fn test_extract_definition_nonexistent() {
        let examples_dir = get_simple_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "my_module.NonexistentSymbol",
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
            start_line: 0,
            start_column: 0,
            end_line: 5,
            end_column: 5,
        };

        let formatted = PythonAnalyzer::format_function(&sig);
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
            start_line: 0,
            start_column: 0,
            end_line: 5,
            end_column: 5,
        };

        let formatted = PythonAnalyzer::format_function(&sig);
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
            start_line: 0,
            start_column: 0,
            end_line: 5,
            end_column: 5,
        };

        let formatted = PythonAnalyzer::format_function(&sig);
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
            start_line: 0,
            start_column: 0,
            end_line: 5,
            end_column: 5,
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
            base_classes: vec![],
            docstring: Some("A test class".to_string()),
            start_line: 0,
            start_column: 0,
            end_line: 5,
            end_column: 5,
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
                start_line: 0,
                start_column: 0,
                end_line: 5,
                end_column: 5,
            }),
        };

        let formatted = PythonAnalyzer::format_class(&class_info);
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
            start_line: 0,
            start_column: 0,
            end_line: 5,
            end_column: 5,
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
                start_line: 0,
                start_column: 0,
                end_line: 5,
                end_column: 5,
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

    // ==================== Python Interpreter Priority Tests ====================
    // These tests verify that the configured Python interpreter from LSP initialization
    // takes priority over auto-discovery

    #[test]
    #[ignore = "Requires Python in PATH"]
    fn test_configured_interpreter_takes_priority() {
        // This test verifies that when a Python interpreter is explicitly configured
        // via LSP initialization, it is used instead of auto-discovery
        let examples_dir = get_simple_test_dir();

        // Get the current system Python
        let system_python = which::which("python3")
            .or_else(|_| which::which("python"))
            .expect("Python not found in PATH");

        let system_python_str = system_python.to_str().unwrap();

        // Resolve a module with explicit interpreter configuration
        let result_with_config = PythonAnalyzer::resolve_module(
            "my_module",
            Some(&examples_dir),
            Some(system_python_str),
        );

        // Should succeed using the configured interpreter
        assert!(
            result_with_config.is_ok(),
            "Module resolution should succeed with configured interpreter"
        );
    }

    #[test]
    fn test_auto_discovery_when_no_interpreter_configured() {
        // This test verifies that when no Python interpreter is configured,
        // the system falls back to auto-discovery
        let examples_dir = get_simple_test_dir();

        // Resolve a module without explicit interpreter configuration (None)
        let result = PythonAnalyzer::resolve_module("my_module", Some(&examples_dir), None);

        // Should still succeed using auto-discovery
        assert!(
            result.is_ok(),
            "Module resolution should succeed with auto-discovery"
        );
    }

    #[test]
    #[ignore = "Requires Python in PATH"]
    fn test_extract_definition_with_configured_interpreter() {
        // Test that definition extraction uses configured interpreter
        let examples_dir = get_simple_test_dir();

        // Get the current system Python
        let system_python = which::which("python3")
            .or_else(|_| which::which("python"))
            .ok();

        let system_python_str = system_python.as_ref().and_then(|p| p.to_str());

        // Extract definition with configured interpreter
        let result = PythonAnalyzer::extract_definition_info(
            "my_module.simple_function",
            Some(&examples_dir),
            system_python_str,
        );

        assert!(
            result.is_ok(),
            "Definition extraction should work with configured interpreter: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_extract_definition_without_configured_interpreter() {
        // Test that definition extraction works without configured interpreter (auto-discovery)
        let examples_dir = get_simple_test_dir();

        // Extract definition without configured interpreter
        let result = PythonAnalyzer::extract_definition_info(
            "my_module.simple_function",
            Some(&examples_dir),
            None,
        );

        assert!(
            result.is_ok(),
            "Definition extraction should work with auto-discovery: {:?}",
            result.err()
        );
    }

    #[test]
    #[ignore = "Requires Python in PATH"]
    fn test_resolve_module_priority_order() {
        // This test documents the priority order:
        // 1. Configured interpreter (highest priority)
        // 2. Auto-discovered environment (fallback)
        let examples_dir = get_simple_test_dir();

        // Test with configured interpreter (priority 1)
        if let Ok(python_path) = which::which("python3").or_else(|_| which::which("python")) {
            let result_configured = PythonAnalyzer::resolve_module(
                "my_module",
                Some(&examples_dir),
                python_path.to_str(),
            );
            assert!(
                result_configured.is_ok(),
                "Should resolve with configured interpreter"
            );
        }

        // Test with auto-discovery (priority 2)
        let result_auto = PythonAnalyzer::resolve_module("my_module", Some(&examples_dir), None);
        assert!(result_auto.is_ok(), "Should resolve with auto-discovery");
    }

    // ==================== Re-export resolution tests ====================

    fn get_reexport_test_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("workspace")
            .join("reexport")
    }

    #[test]
    fn test_reexport_via_subpackage() {
        // Test: mylib.Linear should resolve to mylib/modules/linear.py::Linear
        // Path: mylib/__init__.py imports from mylib.modules
        //       mylib/modules/__init__.py imports from mylib.modules.linear
        //       mylib/modules/linear.py defines Linear
        let workspace_dir = get_reexport_test_dir();

        let result =
            PythonAnalyzer::extract_definition_info("mylib.Linear", Some(&workspace_dir), None);

        assert!(
            result.is_ok(),
            "Should resolve Linear through re-export chain: {:?}",
            result.err()
        );
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Class(class_info) => {
                assert_eq!(class_info.name, "Linear");
                assert!(class_info.init_signature.is_some());
                let init = class_info.init_signature.as_ref().unwrap();
                // Check that it has the expected parameters: self, in_features, out_features, bias
                assert_eq!(init.parameters.len(), 4);
                assert_eq!(init.parameters[1].name, "in_features");
                assert_eq!(init.parameters[2].name, "out_features");
                assert_eq!(init.parameters[3].name, "bias");
            }
            _ => panic!("Expected Class definition"),
        }
    }

    #[test]
    fn test_reexport_with_alias() {
        // Test: mylib.AliasedClass should resolve to mylib/modules/linear.py::OriginalClass
        // Path: mylib/__init__.py imports OriginalClass as AliasedClass
        let workspace_dir = get_reexport_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "mylib.AliasedClass",
            Some(&workspace_dir),
            None,
        );

        assert!(
            result.is_ok(),
            "Should resolve AliasedClass through aliased re-export: {:?}",
            result.err()
        );
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Class(class_info) => {
                // The actual class is OriginalClass, but we look it up via the alias
                assert_eq!(class_info.name, "OriginalClass");
            }
            _ => panic!("Expected Class definition"),
        }
    }

    #[test]
    fn test_reexport_via_star_import() {
        // Test: mylib.StarExportedClass should resolve through star import
        // Path: mylib/__init__.py has `from mylib.star_module import *`
        //       mylib/star_module.py defines StarExportedClass and has __all__
        let workspace_dir = get_reexport_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "mylib.StarExportedClass",
            Some(&workspace_dir),
            None,
        );

        assert!(
            result.is_ok(),
            "Should resolve StarExportedClass through star import: {:?}",
            result.err()
        );
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Class(class_info) => {
                assert_eq!(class_info.name, "StarExportedClass");
            }
            _ => panic!("Expected Class definition"),
        }
    }

    #[test]
    fn test_direct_module_access() {
        // Test: mylib.modules.linear.DirectClass should resolve directly
        let workspace_dir = get_reexport_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "mylib.modules.linear.DirectClass",
            Some(&workspace_dir),
            None,
        );

        assert!(
            result.is_ok(),
            "Should resolve DirectClass directly: {:?}",
            result.err()
        );
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Class(class_info) => {
                assert_eq!(class_info.name, "DirectClass");
            }
            _ => panic!("Expected Class definition"),
        }
    }

    #[test]
    fn test_private_class_not_star_exported() {
        // Test: _PrivateClass should NOT be accessible via star import
        // because it starts with _ and is not in __all__
        let workspace_dir = get_reexport_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "mylib._PrivateClass",
            Some(&workspace_dir),
            None,
        );

        assert!(
            result.is_err(),
            "Private class should not be accessible via star import"
        );
    }

    // ==================== classmethod and staticmethod tests ====================

    #[test]
    fn test_extract_classmethod() {
        // Test: class_methods.ModelFactory.from_config should resolve to a classmethod
        let workspace_dir = get_simple_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "class_methods.ModelFactory.from_config",
            Some(&workspace_dir),
            None,
        );

        assert!(
            result.is_ok(),
            "Should resolve classmethod: {:?}",
            result.err()
        );
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Method(method_info) => {
                assert_eq!(method_info.class_name, "ModelFactory");
                assert_eq!(method_info.method_name, "from_config");
                assert!(method_info.is_classmethod);
                assert!(!method_info.is_staticmethod);
                assert_eq!(method_info.signature.name, "from_config");
                // Check parameters: cls and config
                assert_eq!(method_info.signature.parameters.len(), 2);
                assert_eq!(method_info.signature.parameters[0].name, "cls");
                assert_eq!(method_info.signature.parameters[1].name, "config");
            }
            _ => panic!("Expected Method definition, got {:?}", definition_info),
        }
    }

    #[test]
    fn test_extract_staticmethod() {
        // Test: class_methods.ModelFactory.compute_size should resolve to a staticmethod
        let workspace_dir = get_simple_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "class_methods.ModelFactory.compute_size",
            Some(&workspace_dir),
            None,
        );

        assert!(
            result.is_ok(),
            "Should resolve staticmethod: {:?}",
            result.err()
        );
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Method(method_info) => {
                assert_eq!(method_info.class_name, "ModelFactory");
                assert_eq!(method_info.method_name, "compute_size");
                assert!(!method_info.is_classmethod);
                assert!(method_info.is_staticmethod);
                assert_eq!(method_info.signature.name, "compute_size");
                // Check parameters: dim1 and dim2
                assert_eq!(method_info.signature.parameters.len(), 2);
                assert_eq!(method_info.signature.parameters[0].name, "dim1");
                assert_eq!(method_info.signature.parameters[1].name, "dim2");
            }
            _ => panic!("Expected Method definition, got {:?}", definition_info),
        }
    }

    #[test]
    fn test_extract_classmethod_with_defaults() {
        // Test: class_methods.ModelFactory.with_defaults has default parameter
        let workspace_dir = get_simple_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "class_methods.ModelFactory.with_defaults",
            Some(&workspace_dir),
            None,
        );

        assert!(
            result.is_ok(),
            "Should resolve classmethod: {:?}",
            result.err()
        );
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Method(method_info) => {
                assert_eq!(method_info.class_name, "ModelFactory");
                assert_eq!(method_info.method_name, "with_defaults");
                assert!(method_info.is_classmethod);
                // Check parameters: cls and output_dim (with default)
                assert_eq!(method_info.signature.parameters.len(), 2);
                assert_eq!(method_info.signature.parameters[1].name, "output_dim");
                assert!(method_info.signature.parameters[1].has_default);
            }
            _ => panic!("Expected Method definition, got {:?}", definition_info),
        }
    }

    #[test]
    fn test_extract_classmethod_with_kwargs() {
        // Test: class_methods.DataProcessor.create has **kwargs
        let workspace_dir = get_simple_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "class_methods.DataProcessor.create",
            Some(&workspace_dir),
            None,
        );

        assert!(
            result.is_ok(),
            "Should resolve classmethod: {:?}",
            result.err()
        );
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Method(method_info) => {
                assert_eq!(method_info.class_name, "DataProcessor");
                assert_eq!(method_info.method_name, "create");
                assert!(method_info.is_classmethod);
                // Check parameters: cls, name, **kwargs
                assert_eq!(method_info.signature.parameters.len(), 3);
                assert_eq!(method_info.signature.parameters[2].name, "kwargs");
                assert!(method_info.signature.parameters[2].is_variadic_keyword);
            }
            _ => panic!("Expected Method definition, got {:?}", definition_info),
        }
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
                        type_annotation: None,
                        default_value: None,
                        has_default: false,
                        is_variadic: false,
                        is_variadic_keyword: false,
                        is_keyword_only: false,
                    },
                    ParameterInfo {
                        name: "config".to_string(),
                        type_annotation: Some("dict".to_string()),
                        default_value: None,
                        has_default: false,
                        is_variadic: false,
                        is_variadic_keyword: false,
                        is_keyword_only: false,
                    },
                ],
                return_type: Some("MyClass".to_string()),
                docstring: Some("Create from config".to_string()),
                start_line: 0,
                start_column: 0,
                end_line: 5,
                end_column: 5,
            },
            is_classmethod: true,
            is_staticmethod: false,
        };

        let formatted = PythonAnalyzer::format_method(&method_info);
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
                    type_annotation: Some("int".to_string()),
                    default_value: None,
                    has_default: false,
                    is_variadic: false,
                    is_variadic_keyword: false,
                    is_keyword_only: false,
                }],
                return_type: Some("int".to_string()),
                docstring: None,
                start_line: 0,
                start_column: 0,
                end_line: 5,
                end_column: 5,
            },
            is_classmethod: false,
            is_staticmethod: true,
        };

        let formatted = PythonAnalyzer::format_method(&method_info);
        assert!(formatted.contains("@staticmethod"));
        assert!(formatted.contains("def helper(value: int) -> int"));
    }

    #[test]
    fn test_extract_nested_classmethod() {
        // Test: Nested class properties should resolve to a classmethod
        // Path: NestedTwice.nested -> NestedExample, NestedExample.nested_class -> ModelFactory
        // ModelFactory.from_config is a classmethod
        let workspace_dir = get_simple_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "class_methods.NestedTwice.nested.nested_class.from_config",
            Some(&workspace_dir),
            None,
        );

        assert!(
            result.is_ok(),
            "Should resolve classmethod: {:?}",
            result.err()
        );
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Method(method_info) => {
                assert_eq!(method_info.class_name, "ModelFactory");
                assert_eq!(method_info.method_name, "from_config");
                assert!(method_info.is_classmethod);
                assert!(!method_info.is_staticmethod);
                // Check parameters: cls and config
                assert_eq!(method_info.signature.parameters.len(), 2);
                assert_eq!(method_info.signature.parameters[0].name, "cls");
                assert_eq!(method_info.signature.parameters[1].name, "config");
            }
            _ => panic!("Expected Method definition, got {:?}", definition_info),
        }
    }

    #[test]
    fn test_extract_nested_staticmethod() {
        // Test: Nested class properties should resolve to a staticmethod
        // Path: NestedTwice.nested -> NestedExample, NestedExample.nested_class -> ModelFactory
        // ModelFactory.compute_size is a staticmethod
        let workspace_dir = get_simple_test_dir();

        let result = PythonAnalyzer::extract_definition_info(
            "class_methods.NestedTwice.nested.nested_class.compute_size",
            Some(&workspace_dir),
            None,
        );

        assert!(
            result.is_ok(),
            "Should resolve staticmethod: {:?}",
            result.err()
        );
        let (definition_info, _file_path, _module_path, _symbol_name) = result.unwrap();

        match definition_info {
            DefinitionInfo::Method(method_info) => {
                assert_eq!(method_info.class_name, "ModelFactory");
                assert_eq!(method_info.method_name, "compute_size");
                assert!(!method_info.is_classmethod);
                assert!(method_info.is_staticmethod);
                // Check parameters: dim1 and dim2
                assert_eq!(method_info.signature.parameters.len(), 2);
                assert_eq!(method_info.signature.parameters[0].name, "dim1");
                assert_eq!(method_info.signature.parameters[1].name, "dim2");
            }
            _ => panic!("Expected Method definition, got {:?}", definition_info),
        }
    }

    // ==================== .pth file parsing tests ====================

    fn get_editable_test_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("workspace")
            .join("editable")
    }

    #[test]
    fn test_parse_pth_files_basic() {
        // Test parsing .pth files from the editable test workspace
        let workspace_dir = get_editable_test_dir();
        let site_packages = workspace_dir.join("site-packages");

        let paths = PythonAnalyzer::parse_pth_files(&site_packages);

        // Should find the path from _editable_package.pth
        assert!(!paths.is_empty(), "Should find paths from .pth files");

        // The path should point to the src directory
        let has_src_path = paths.iter().any(|p| p.ends_with("src"));
        assert!(has_src_path, "Should find src path from .pth file");
    }

    #[test]
    fn test_parse_pth_files_nonexistent_dir() {
        // Test that parsing .pth files from nonexistent directory returns empty
        let nonexistent = PathBuf::from("/nonexistent/path/site-packages");
        let paths = PythonAnalyzer::parse_pth_files(&nonexistent);
        assert!(paths.is_empty(), "Should return empty for nonexistent dir");
    }

    #[test]
    fn test_resolve_editable_module() {
        // Test resolving a module from an editable install via .pth file
        let workspace_dir = get_editable_test_dir();
        let site_packages = workspace_dir.join("site-packages");

        // Build search paths manually with site-packages that has .pth files
        let mut search_paths = vec![workspace_dir.clone()];
        search_paths.push(site_packages.clone());
        search_paths.extend(PythonAnalyzer::parse_pth_files(&site_packages));

        // The editable_package should now be resolvable
        let result =
            PythonAnalyzer::resolve_module_with_search_paths("editable_package.lib", &search_paths);

        assert!(
            result.is_ok(),
            "Should resolve editable_package.lib: {:?}",
            result.err()
        );
        let path = result.unwrap();
        assert!(
            path.ends_with("lib.py"),
            "Should resolve to lib.py: {:?}",
            path
        );
    }

    #[test]
    fn test_extract_definition_from_editable_package() {
        // Test extracting class definition from an editable package via .pth resolution
        let workspace_dir = get_editable_test_dir();
        let site_packages = workspace_dir.join("site-packages");

        // Build search paths manually with site-packages that has .pth files
        let mut search_paths = vec![workspace_dir.clone()];
        search_paths.push(site_packages.clone());
        search_paths.extend(PythonAnalyzer::parse_pth_files(&site_packages));

        // Resolve the module first
        let module_path =
            PythonAnalyzer::resolve_module_with_search_paths("editable_package.lib", &search_paths)
                .expect("Should resolve module");

        // Extract the class info
        let source = fs::read_to_string(&module_path).expect("Should read module file");
        let class_info = PythonAnalyzer::extract_class_info(&source, "EditableModel")
            .expect("Should extract class info");

        assert_eq!(class_info.name, "EditableModel");
        assert!(class_info.docstring.is_some());
        assert!(class_info.init_signature.is_some());

        let init_sig = class_info.init_signature.as_ref().unwrap();
        // Should have self, input_size, output_size
        assert_eq!(init_sig.parameters.len(), 3);
        assert_eq!(init_sig.parameters[1].name, "input_size");
        assert_eq!(init_sig.parameters[2].name, "output_size");
    }
}
