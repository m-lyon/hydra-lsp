use crate::python_analyzer::{
    ClassInfo, FunctionSignature, PythonAnalyzer, get_parsed_module, path_is_file,
};
use crate::python_cache::{InternedSearchPaths, TargetString, resolve_module_cached};
use ruff_python_ast::{self as ast, Expr, Stmt, visitor::Visitor};
use rustc_hash::FxHashSet;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

const MAX_IMPORT_DEPTH: usize = 10;

/// Append a dotted module name to a directory, one path segment per part.
/// Empty parts are skipped, so an empty module name gives back `base`.
/// Returns `None` if a part is not a plain name. Module strings can come from
/// user YAML (a `_target_` value), where `"/etc/x"` or `"../x"` would otherwise
/// escape `base` — an absolute part would even replace it outright.
pub(crate) fn join_module_parts(base: &Path, module: &str) -> Option<PathBuf> {
    let mut path = base.to_path_buf();
    for part in module.split('.') {
        if part.is_empty() {
            continue;
        }
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) => path.push(name),
            _ => return None,
        }
    }
    Some(path)
}

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
pub struct ImportResolver<'db, 'sp> {
    db: &'db dyn ruff_db::Db,
    search_paths: &'sp [PathBuf],
    visited_files: HashSet<PathBuf>,
    depth: usize,
}

impl<'db, 'sp> ImportResolver<'db, 'sp> {
    pub fn new(db: &'db dyn ruff_db::Db, search_paths: &'sp [PathBuf]) -> Self {
        Self {
            db,
            search_paths,
            visited_files: HashSet::new(),
            depth: 0,
        }
    }

    /// Try to find a module file given a base package path
    /// Returns the first existing file in priority order: __init__.pyi, __init__.py, module.pyi, module.py
    ///
    /// Existence checks go through `path_is_file` (salsa `system_path_to_file`)
    /// rather than `Path::exists`, so each probed candidate is interned as a
    /// tracked `File` and a later create/delete of that path invalidates the
    /// calling query. See [`path_is_file`].
    pub fn find_module_file(db: &dyn ruff_db::Db, package_path: &Path) -> Option<PathBuf> {
        // Check for package __init__.py (prioritize .pyi over .py)
        let init_pyi_path = package_path.join("__init__.pyi");
        if path_is_file(db, &init_pyi_path) {
            return Some(init_pyi_path);
        }

        let init_path = package_path.join("__init__.py");
        if path_is_file(db, &init_path) {
            return Some(init_path);
        }

        // Check for regular module file (prioritize .pyi over .py)
        let file_pyi_path = package_path.with_extension("pyi");
        if path_is_file(db, &file_pyi_path) {
            return Some(file_pyi_path);
        }

        let file_path = package_path.with_extension("py");
        if path_is_file(db, &file_path) {
            return Some(file_path);
        }

        None
    }

    /// Resolve a module path to a file path.
    ///
    /// Delegates to the memoised `resolve_module_cached` so that a module hit
    /// once per `follow_import` hop is not re-probed on the filesystem, and so
    /// there is a single copy of the resolution rules.
    pub fn resolve_module_path(&self, module_path: &str) -> Option<PathBuf> {
        resolve_module_cached(
            self.db,
            TargetString::new(self.db, module_path.to_string()),
            InternedSearchPaths::new(self.db, self.search_paths.to_vec()),
        )
        .clone()
    }

    /// The directory a relative import is resolved against: the importing
    /// file's own directory for level 1, one directory up for each extra level.
    fn relative_import_dir(current_file: &Path, level: u32) -> Option<&Path> {
        // level 1 = current package (.)
        // level 2 = parent package (..)
        // etc.
        let mut current_dir = current_file.parent()?;
        for _ in 1..level {
            current_dir = current_dir.parent()?;
        }
        Some(current_dir)
    }

    /// Resolve a relative import (`from .module import name`) to the file it names.
    fn resolve_relative_module_file(
        &self,
        current_file: &Path,
        module: Option<&str>,
        level: u32,
    ) -> Option<PathBuf> {
        let current_dir = Self::relative_import_dir(current_file, level)?;

        // Resolve against the importing file's own directory, but only when
        // that directory is inside a search root. Otherwise the import reached
        // past the top-level package, which Python rejects, so there is nothing
        // valid left to resolve against.
        if !self
            .search_paths
            .iter()
            .any(|search_path| current_dir.starts_with(search_path))
        {
            return None;
        }

        let candidate = join_module_parts(current_dir, module.unwrap_or_default())?;
        Self::find_module_file(self.db, &candidate)
    }

    /// Extract __all__ from a module
    fn extract_dunder_all(&mut self, file_path: &Path) -> Option<FxHashSet<String>> {
        let parsed = get_parsed_module(self.db, file_path).ok()?;
        let mut dunder_all_finder = DunderAllFinder::default();
        dunder_all_finder.visit_body(parsed.suite());
        dunder_all_finder.names
    }

    /// Extract import information for a specific symbol from a module
    fn find_import_for_symbol(
        &mut self,
        file_path: &Path,
        symbol_name: &str,
    ) -> Option<ImportInfo> {
        let parsed = get_parsed_module(self.db, file_path).ok()?;
        let mut finder = ImportFinder {
            target_symbol: symbol_name.to_string(),
            result: None,
        };
        finder.visit_body(parsed.suite());
        finder.result
    }

    /// Extract all star imports from a module
    fn find_star_imports(&mut self, file_path: &Path) -> Vec<ImportInfo> {
        let Ok(parsed) = get_parsed_module(self.db, file_path) else {
            return Vec::new();
        };
        let mut finder = StarImportFinder::default();
        finder.visit_body(parsed.suite());
        finder.star_imports
    }

    /// Check if a class is directly defined in the file
    fn find_class_direct(&mut self, file_path: &Path, class_name: &str) -> Option<ClassInfo> {
        PythonAnalyzer::extract_class_info(self.db, file_path, class_name).ok()
    }

    /// Check if a function is directly defined in the file
    fn find_function_direct(
        &mut self,
        file_path: &Path,
        function_name: &str,
    ) -> Option<FunctionSignature> {
        PythonAnalyzer::extract_function_signature(self.db, file_path, function_name).ok()
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
                let module_file = if level > 0 {
                    self.resolve_relative_module_file(starting_file, Some(module), level)
                } else {
                    self.resolve_module_path(module)
                };

                if let Some(module_file) = module_file {
                    // Check if this symbol is exported from the star-imported module
                    let star_dunder_all = self.extract_dunder_all(&module_file);

                    // If __all__ is defined, check if symbol is in it
                    // If not defined, check if symbol doesn't start with _
                    let is_exported = if let Some(ref all_names) = star_dunder_all {
                        all_names.contains(&symbol_name.to_string())
                    } else {
                        !symbol_name.starts_with('_')
                    };

                    if is_exported
                        && let Some(result) = self.resolve_symbol(&module_file, symbol_name)
                    {
                        self.depth -= 1;
                        return Some(result);
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
                let module_file = if *level > 0 {
                    self.resolve_relative_module_file(current_file, Some(module), *level)?
                } else {
                    self.resolve_module_path(module)?
                };

                // Recursively resolve the symbol in the imported module
                self.resolve_symbol(&module_file, name)
            }
            ImportInfo::StarImport { module, level } => {
                // This shouldn't be called directly for star imports
                let module_file = if *level > 0 {
                    self.resolve_relative_module_file(current_file, Some(module), *level)?
                } else {
                    self.resolve_module_path(module)?
                };
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
                if let Expr::Name(name) = target
                    && name.id.as_str() == "__all__"
                    && let Some(names) = extract_list_of_strings(&assign.value)
                {
                    self.names = Some(names);
                    return;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::HydraDatabase;
    use crate::python_analyzer::DefinitionInfo;
    use ruff_db::system::SystemPath;
    use std::fs;
    use tempfile::TempDir;

    /// Write `contents` to `path`, creating the parent directories first.
    fn write_file(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("path has a parent")).expect("create dirs");
        fs::write(path, contents).expect("write file");
    }

    /// Build a workspace whose interpreter search root lives inside the
    /// workspace itself:
    ///
    /// ```text
    /// workspace/.venv/lib/python3.12/site-packages/example/nested/
    /// ```
    ///
    /// `nested/__init__.py` re-exports `ExportedClass` with the given relative
    /// import statement, so resolving it has to pick the site-packages root
    /// (not the workspace root) when working out what `.implementation` means.
    fn overlapping_roots_fixture(reexport: &str) -> (TempDir, PathBuf, PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let site_packages = workspace
            .join(".venv")
            .join("lib")
            .join("python3.12")
            .join("site-packages");
        let nested = site_packages.join("example").join("nested");

        write_file(&nested.join("__init__.py"), reexport);
        write_file(
            &nested.join("implementation.py"),
            "class ExportedClass:\n    pass\n",
        );

        (temp, workspace, site_packages)
    }

    /// Resolve `symbol_path` with the workspace root ahead of the nested
    /// site-packages root, which is the order production builds them in.
    fn resolve_in_overlapping_roots(
        workspace: &Path,
        site_packages: &Path,
        symbol_path: &str,
    ) -> (DefinitionInfo, PathBuf, String) {
        let db = HydraDatabase::new(SystemPath::new("/"));
        let search_paths = vec![workspace.to_path_buf(), site_packages.to_path_buf()];

        let (definition_info, file_path, _module_path, symbol_name) =
            PythonAnalyzer::extract_definition_info(&db, symbol_path, &search_paths).unwrap_or_else(
                |_| panic!("{symbol_path} should resolve when site-packages is nested inside the workspace"),
            );

        (definition_info, file_path, symbol_name)
    }

    fn assert_exported_class(definition_info: &DefinitionInfo, file_path: &Path) {
        assert!(
            file_path.ends_with("implementation.py"),
            "expected the defining module, got {}",
            file_path.display()
        );
        match definition_info {
            DefinitionInfo::Class(class_info) => assert_eq!(class_info.name, "ExportedClass"),
            other => panic!("expected a class definition, got {other:?}"),
        }
    }

    #[test]
    fn test_relative_reexport_with_nested_search_roots() {
        let (_temp, workspace, site_packages) =
            overlapping_roots_fixture("from .implementation import ExportedClass\n");

        let (definition_info, file_path, symbol_name) = resolve_in_overlapping_roots(
            &workspace,
            &site_packages,
            "example.nested.ExportedClass",
        );

        assert_eq!(symbol_name, "ExportedClass");
        assert_exported_class(&definition_info, &file_path);
    }

    #[test]
    fn test_relative_star_reexport_with_nested_search_roots() {
        let (_temp, workspace, site_packages) =
            overlapping_roots_fixture("from .implementation import *\n");

        let (definition_info, file_path, symbol_name) = resolve_in_overlapping_roots(
            &workspace,
            &site_packages,
            "example.nested.ExportedClass",
        );

        assert_eq!(symbol_name, "ExportedClass");
        assert_exported_class(&definition_info, &file_path);
    }

    /// A file that only the workspace root contains still has to resolve
    /// against it once the nested site-packages root is preferred elsewhere.
    #[test]
    fn test_relative_reexport_under_workspace_root_only() {
        let (_temp, workspace, site_packages) =
            overlapping_roots_fixture("from .implementation import ExportedClass\n");

        let first_party = workspace.join("firstparty");
        write_file(
            &first_party.join("__init__.py"),
            "from .implementation import ExportedClass\n",
        );
        write_file(
            &first_party.join("implementation.py"),
            "class ExportedClass:\n    pass\n",
        );

        let (definition_info, file_path, symbol_name) =
            resolve_in_overlapping_roots(&workspace, &site_packages, "firstparty.ExportedClass");

        assert_eq!(symbol_name, "ExportedClass");
        assert!(
            file_path.starts_with(&first_party),
            "expected the workspace copy, got {}",
            file_path.display()
        );
        assert_exported_class(&definition_info, &file_path);
    }

    /// A relative import names a sibling of the importing file, so it must not
    /// be answered by a same-named module sitting under an earlier search root.
    #[test]
    fn test_relative_import_prefers_sibling_over_shadowing_root() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("repo");
        let package = repo.join("src").join("pkg");

        write_file(
            &package.join("__init__.py"),
            "from .implementation import ExportedClass\n",
        );
        write_file(
            &package.join("implementation.py"),
            "class ExportedClass:\n    pass\n",
        );

        // A decoy `pkg.implementation` directly under the repo root: reachable
        // first if the relative import round-trips through a module name. It is
        // a namespace package so that `pkg` itself still resolves to src/pkg.
        write_file(
            &repo.join("pkg").join("implementation.py"),
            "class ExportedClass:\n    pass\n",
        );

        let db = HydraDatabase::new(SystemPath::new("/"));
        let search_paths = vec![repo.clone(), repo.join("src")];

        let (definition_info, file_path, _module_path, symbol_name) =
            PythonAnalyzer::extract_definition_info(&db, "pkg.ExportedClass", &search_paths)
                .expect("ExportedClass should resolve through the sibling module");

        assert_eq!(symbol_name, "ExportedClass");
        assert!(
            file_path.starts_with(&package),
            "expected the sibling module under src/pkg, got {}",
            file_path.display()
        );
        assert_exported_class(&definition_info, &file_path);
    }

    /// The mirror of the test above: when the named sibling does not exist, the
    /// import must fail rather than fall through to the shadowing root.
    #[test]
    fn test_relative_import_does_not_fall_back_to_shadowing_root() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("repo");

        write_file(
            &repo.join("src").join("pkg").join("__init__.py"),
            "from .missing import ExportedClass\n",
        );
        // `pkg.missing` exists under the earlier root, but it is not a sibling
        // of the importing file, so it must not answer `.missing`.
        write_file(
            &repo.join("pkg").join("missing.py"),
            "class ExportedClass:\n    pass\n",
        );

        let db = HydraDatabase::new(SystemPath::new("/"));
        let search_paths = vec![repo.clone(), repo.join("src")];

        let result =
            PythonAnalyzer::extract_definition_info(&db, "pkg.ExportedClass", &search_paths);

        assert!(
            result.is_err(),
            "expected no resolution, got {:?}",
            result.map(|(_, file_path, _, _)| file_path)
        );
    }

    /// A relative import is resolved from the importing file's own directory,
    /// so a same-named module under an earlier search root never answers it —
    /// whether or not the sibling itself exists.
    #[test]
    fn test_resolve_relative_module_file_walks_from_the_importing_file() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("repo");
        let package = repo.join("src").join("pkg");

        write_file(&package.join("__init__.py"), "");
        write_file(&package.join("implementation.py"), "");
        write_file(&repo.join("pkg").join("implementation.py"), "");
        write_file(&repo.join("pkg").join("missing.py"), "");

        let db = HydraDatabase::new(SystemPath::new("/"));
        let search_paths = vec![repo.clone(), repo.join("src")];
        let resolver = ImportResolver::new(&db, &search_paths);
        let importer = package.join("__init__.py");

        assert_eq!(
            resolver.resolve_relative_module_file(&importer, Some("implementation"), 1),
            Some(package.join("implementation.py")),
        );
        assert_eq!(
            resolver.resolve_relative_module_file(&importer, Some("missing"), 1),
            None,
        );
    }

    /// `from ..module import X` climbs one package per extra dot, and an empty
    /// module name (`from . import x`) names the package directory itself.
    #[test]
    fn test_resolve_relative_module_file_handles_levels_and_empty_module() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("repo");
        let package = repo.join("pkg");
        let sub_package = package.join("sub");

        write_file(&package.join("__init__.py"), "");
        write_file(&package.join("implementation.py"), "");
        write_file(&sub_package.join("__init__.py"), "");

        let db = HydraDatabase::new(SystemPath::new("/"));
        let search_paths = vec![repo.clone()];
        let resolver = ImportResolver::new(&db, &search_paths);
        let importer = sub_package.join("__init__.py");

        assert_eq!(
            resolver.resolve_relative_module_file(&importer, Some("implementation"), 2),
            Some(package.join("implementation.py")),
        );
        assert_eq!(
            resolver.resolve_relative_module_file(&importer, Some(""), 1),
            Some(sub_package.join("__init__.py")),
        );
        assert_eq!(
            resolver.resolve_relative_module_file(&importer, Some(""), 2),
            Some(package.join("__init__.py")),
        );
    }

    /// A relative import with more dots than there are packages walks out of
    /// the project; it must not match whatever happens to sit out there.
    #[test]
    fn test_relative_import_does_not_escape_the_search_roots() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("repo");

        write_file(&repo.join("pkg").join("__init__.py"), "");
        // A sibling of the repo, outside every search root.
        write_file(
            &temp.path().join("outside.py"),
            "class ExportedClass:\n    pass\n",
        );
        // A module inside the root: it must not answer the escaping import
        // either, since the import is invalid rather than absolute.
        write_file(&repo.join("inside.py"), "class ExportedClass:\n    pass\n");

        let db = HydraDatabase::new(SystemPath::new("/"));
        let search_paths = vec![repo.clone()];
        let resolver = ImportResolver::new(&db, &search_paths);
        let importer = repo.join("pkg").join("__init__.py");

        // `from ...outside import ExportedClass` reaches above the root.
        assert_eq!(
            resolver.resolve_relative_module_file(&importer, Some("outside"), 3),
            None,
        );
        assert_eq!(
            resolver.resolve_relative_module_file(&importer, Some("inside"), 3),
            None,
        );
    }

    /// `join_module_parts` is tested directly because at the resolver level a
    /// rejected part usually names a file that does not exist anyway, so the
    /// assertion would hold with or without the guard.
    #[test]
    fn test_join_module_parts_rejects_parts_that_are_not_plain_names() {
        let base = Path::new("/root");

        assert_eq!(
            join_module_parts(base, "a.b"),
            Some(base.join("a").join("b"))
        );
        assert_eq!(join_module_parts(base, ""), Some(base.to_path_buf()));
        // Empty parts are skipped, so a bare `..` contributes nothing.
        assert_eq!(join_module_parts(base, ".."), Some(base.to_path_buf()));

        // An absolute part would replace `base` outright.
        assert_eq!(join_module_parts(base, "/etc/passwd"), None);
        // A part carrying its own separators would descend past one segment.
        assert_eq!(join_module_parts(base, "pkg/sub"), None);
        // `../outside` splits to a leading `/outside`, which is absolute.
        assert_eq!(join_module_parts(base, "../outside"), None);
        assert_eq!(join_module_parts(base, "a./b"), None);
    }

    /// The same guard at the resolver level: an absolute module name must not
    /// resolve even when the file it points at exists.
    #[test]
    fn test_resolve_module_path_rejects_an_absolute_module_name() {
        // A dot in the temp path would be split into parts and mangle the
        // absolute name, quietly making this test prove nothing.
        let temp = tempfile::Builder::new()
            .prefix("hydra")
            .tempdir()
            .expect("tempdir");
        assert!(
            !temp.path().to_str().expect("utf-8 temp path").contains('.'),
            "fixture needs a dot-free temp path, got {}",
            temp.path().display()
        );

        let repo = temp.path().join("repo");
        write_file(&repo.join("pkg").join("__init__.py"), "");
        write_file(&repo.join("pkg").join("inside.py"), "");
        let outside = temp.path().join("outside");
        write_file(&outside.with_extension("py"), "");

        let db = HydraDatabase::new(SystemPath::new("/"));
        let search_paths = vec![repo.clone()];
        let resolver = ImportResolver::new(&db, &search_paths);

        // Positive control: a guard that rejected everything would fail here.
        assert_eq!(
            resolver.resolve_module_path("pkg.inside"),
            Some(repo.join("pkg").join("inside.py")),
        );

        assert_eq!(
            resolver.resolve_module_path(outside.to_str().expect("utf-8 path")),
            None,
            "an absolute name must not replace the search root",
        );
        assert_eq!(
            resolver.resolve_relative_module_file(
                &repo.join("pkg").join("__init__.py"),
                Some(outside.to_str().expect("utf-8 path")),
                1,
            ),
            None,
        );
    }
}
