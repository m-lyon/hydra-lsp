use super::cache::resolve_module_by_name;
use super::extract::{
    assigned_value, extract_class_info, extract_function_signature, parsed_python_module,
};
use super::model::{ClassInfo, FunctionSignature};
use ruff_db::files::{File, system_path_to_file};
use ruff_db::parsed::ParsedModuleRef;
use ruff_db::system::{SystemPath, SystemPathBuf};
use ruff_python_ast::{self as ast, Expr, PythonVersion, Stmt, visitor::Visitor};
use rustc_hash::FxHashSet;
use std::path::{Component, Path};
use ty_module_resolver::{
    ModuleResolveMode, ResolverEnvironment, search_paths, system_module_search_paths,
};

const MAX_IMPORT_DEPTH: usize = 10;

/// Append a dotted module name to a directory, one path segment per part.
/// Empty parts are skipped, so an empty module name gives back `base`.
/// Returns `None` if a part is not a plain name. Module strings can come from
/// user YAML (a `_target_` value), where `"/etc/x"` or `"../x"` would otherwise
/// escape `base` — an absolute part would even replace it outright.
fn join_module_parts(base: &SystemPath, module: &str) -> Option<SystemPathBuf> {
    let mut path = base.to_path_buf();
    for part in module.split('.') {
        if part.is_empty() {
            continue;
        }
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) => path.push(name.to_str()?),
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
        level: u32, // For relative imports: 0 = absolute, 1 = '.', 2 = '..', etc.
    },
    /// `import module` or `import module as alias`
    Import { module: String },
}

/// Context for import resolution operations
pub(super) struct ImportResolver<'db> {
    db: &'db dyn ty_module_resolver::Db,
    environment: ResolverEnvironment<'db>,
}

/// Where a symbol is defined: the file, and the symbol's name there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedSymbol {
    pub(super) file: File,
    pub(super) name: String,
}

/// The cycle and depth state of one [`ImportResolver::resolve_symbol`] call.
#[derive(Default)]
struct ImportWalk {
    visited_files: FxHashSet<File>,
    depth: usize,
}

/// Try to find a module file given a base package path
/// Returns the first existing file in priority order: __init__.pyi, __init__.py, module.pyi, module.py
///
/// Existence checks go through salsa's `system_path_to_file` rather than
/// `Path::exists`. It interns each probed candidate as a `File` *even when it
/// does not exist* and records a dependency on that file's existence, so a
/// later create or delete of the path, synced with `File::sync_path`,
/// invalidates the calling query.
pub(super) fn find_module_file(db: &dyn ruff_db::Db, package_path: &SystemPath) -> Option<File> {
    [
        package_path.join("__init__.pyi"),
        package_path.join("__init__.py"),
        package_path.with_extension("pyi"),
        package_path.with_extension("py"),
    ]
    .into_iter()
    .find_map(|candidate| system_path_to_file(db, candidate).ok())
}

impl<'db> ImportResolver<'db> {
    pub(super) fn new(
        db: &'db dyn ty_module_resolver::Db,
        environment: ResolverEnvironment<'db>,
    ) -> Self {
        Self { db, environment }
    }

    fn python_version(&self) -> PythonVersion {
        self.environment.python_version(self.db)
    }

    fn parsed(&self, file: File) -> ParsedModuleRef {
        parsed_python_module(self.db, file, self.python_version())
    }

    /// Resolve an absolute module name to its file, through the memoised
    /// `resolved_module_file`.
    pub(super) fn resolve_module_file(&self, module_path: &str) -> Option<File> {
        resolve_module_by_name(self.db, module_path, self.environment)
    }

    /// The directory a relative import is resolved against: the importing
    /// file's own directory for level 1, one directory up for each extra level.
    fn relative_import_dir(current_file: &SystemPath, level: u32) -> Option<&SystemPath> {
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
        current_file: File,
        module: Option<&str>,
        level: u32,
    ) -> Option<File> {
        // A vendored stub has no directory on disk to resolve against.
        let current_path = current_file.path(self.db).as_system_path()?;
        let current_dir = Self::relative_import_dir(current_path, level)?;

        // Resolve against the importing file's own directory, but only when
        // that directory is inside a search root. Otherwise the import reached
        // past the top-level package, which Python rejects, so there is nothing
        // valid left to resolve against.
        if !system_module_search_paths(self.db, self.environment)
            .any(|search_path| current_dir.starts_with(search_path))
            && !self.in_real_stdlib(current_dir)
        {
            return None;
        }

        let candidate = join_module_parts(current_dir, module.unwrap_or_default())?;
        find_module_file(self.db, &candidate)
    }

    /// Whether `dir` is inside the interpreter's stdlib, where the runtime
    /// source behind a stdlib stub lives. Only runtime mode searches it.
    fn in_real_stdlib(&self, dir: &SystemPath) -> bool {
        // The probe need not exist. ty takes only `.pyi` names as being on a
        // stdlib search path, the real one included.
        let probe = dir.join("__init__.pyi");
        search_paths(self.db, self.environment, ModuleResolveMode::Runtime)
            .filter(|search_path| search_path.is_standard_library())
            .any(|stdlib| stdlib.module_name_for_system_path(&probe).is_some())
    }

    /// The file an import of `module` at `level` names, as seen from `from`:
    /// relative to `from` when `level > 0`, else through the search paths.
    fn module_file(&self, from: File, module: &str, level: u32) -> Option<File> {
        if level > 0 {
            self.resolve_relative_module_file(from, Some(module), level)
        } else {
            self.resolve_module_file(module)
        }
    }

    /// Extract __all__ from a module
    fn extract_dunder_all(&self, file: File) -> Option<FxHashSet<String>> {
        let parsed = self.parsed(file);
        let mut dunder_all_finder = DunderAllFinder::default();
        dunder_all_finder.visit_body(parsed.suite());
        dunder_all_finder.names
    }

    /// Extract import information for a specific symbol from a module
    fn find_import_for_symbol(&self, file: File, symbol_name: &str) -> Option<ImportInfo> {
        find_import(self.parsed(file).suite(), symbol_name)
    }

    /// Whether a module defines `__getattr__` at module level (PEP 562).
    /// Packages use this to lazily resolve attributes that were not bound at
    /// import time, which is how a name can be a valid runtime export
    /// without being assigned anywhere in the module body.
    fn has_module_level_getattr(&self, file: File) -> bool {
        self.parsed(file).suite().iter().any(|stmt| {
            matches!(stmt, Stmt::FunctionDef(func_def) if func_def.name.as_str() == "__getattr__")
        })
    }

    /// Find an import for `symbol_name` declared inside a module-level
    /// `if TYPE_CHECKING:` (or `if typing.TYPE_CHECKING:`) block.
    fn find_type_checking_import_for_symbol(
        &self,
        file: File,
        symbol_name: &str,
    ) -> Option<ImportInfo> {
        let parsed = self.parsed(file);
        let mut typing_aliases: Option<FxHashSet<String>> = None;
        for stmt in parsed.suite() {
            let Stmt::If(if_stmt) = stmt else { continue };
            let is_guard = match if_stmt.test.as_ref() {
                // The bare name form is deliberately loose and accepts any
                // name spelled `TYPE_CHECKING`, without checking where it came
                // from, since that spelling carries typing semantics by
                // convention.
                Expr::Name(name) => name.id.as_str() == "TYPE_CHECKING",
                // Only the attribute form needs the alias set, and collecting
                // it walks the module, so fill it on first need.
                Expr::Attribute(attr) => {
                    let aliases = typing_aliases
                        .get_or_insert_with(|| collect_typing_aliases(parsed.suite()));
                    is_typing_type_checking_attribute(attr, aliases)
                }
                _ => false,
            };
            if !is_guard {
                continue;
            }
            if let Some(import) = find_import(&if_stmt.body, symbol_name) {
                return Some(import);
            }
        }
        None
    }

    /// Extract all star imports from a module, as `(module, level)` pairs
    fn find_star_imports(&self, file: File) -> Vec<(String, u32)> {
        let parsed = self.parsed(file);
        let mut finder = StarImportFinder::default();
        finder.visit_body(parsed.suite());
        finder.star_imports
    }

    /// Check if a class is directly defined in the file
    fn find_class_direct(&self, file: File, class_name: &str) -> Option<ClassInfo> {
        extract_class_info(self.db, file, class_name, self.python_version())
    }

    /// Check if a function is directly defined in the file
    fn find_function_direct(&self, file: File, function_name: &str) -> Option<FunctionSignature> {
        extract_function_signature(self.db, file, function_name, self.python_version())
    }

    /// Resolve a symbol by following import chains.
    ///
    /// Returns the file the symbol is defined in and its name there. When the
    /// chain ends on a module rather than a symbol (`import pkg.mod as Name`),
    /// the name returned is `symbol_name` itself, so callers look it up in
    /// that module's file.
    pub(super) fn resolve_symbol(
        &self,
        starting_file: File,
        symbol_name: &str,
    ) -> Option<ResolvedSymbol> {
        let (file, name) = self.resolve(&mut ImportWalk::default(), starting_file, symbol_name)?;
        Some(ResolvedSymbol {
            file,
            name: name.unwrap_or_else(|| symbol_name.to_string()),
        })
    }

    /// One hop of [`Self::resolve_symbol`], with the depth limit and cycle
    /// check around it. The name is `None` when the chain ends on a module.
    ///
    /// A file is entered at most once per walk, even on a different branch
    /// of the search.
    fn resolve(
        &self,
        walk: &mut ImportWalk,
        starting_file: File,
        symbol_name: &str,
    ) -> Option<(File, Option<String>)> {
        // Check for cycles and depth limit
        if walk.depth >= MAX_IMPORT_DEPTH || !walk.visited_files.insert(starting_file) {
            return None;
        }

        walk.depth += 1;
        let result = self.resolve_in_file(walk, starting_file, symbol_name);
        walk.depth -= 1;
        result
    }

    /// Look for `symbol_name` in `starting_file`: defined there, imported
    /// explicitly, exported lazily, or re-exported through a star import.
    fn resolve_in_file(
        &self,
        walk: &mut ImportWalk,
        starting_file: File,
        symbol_name: &str,
    ) -> Option<(File, Option<String>)> {
        // First, check if the symbol is directly defined in this file
        if self.find_class_direct(starting_file, symbol_name).is_some()
            || self
                .find_function_direct(starting_file, symbol_name)
                .is_some()
        {
            return Some((starting_file, Some(symbol_name.to_string())));
        }

        // Look for an explicit import of this symbol
        if let Some(ref import_info) = self.find_import_for_symbol(starting_file, symbol_name) {
            return self.follow_import(walk, starting_file, import_info);
        }

        // Lazy-export fallback: a package can expose a name through a statically
        // declared `if TYPE_CHECKING:` import backed by a module-level `__getattr__`.
        // Require the name to also be listed in `__all__` before following that import,
        // so a type-only import that the module does not actually re-export at runtime
        // still reports unresolved instead of silently matching.
        if self.has_module_level_getattr(starting_file)
            && let Some(dunder_all) = self.extract_dunder_all(starting_file)
            && dunder_all.contains(symbol_name)
            && let Some(import_info) =
                self.find_type_checking_import_for_symbol(starting_file, symbol_name)
            && let Some(result) = self.follow_import(walk, starting_file, &import_info)
        {
            return Some(result);
        }

        // Check star imports to see if the symbol is re-exported from another module
        for (module, level) in self.find_star_imports(starting_file) {
            let Some(module_file) = self.module_file(starting_file, &module, level) else {
                continue;
            };

            // Check if this symbol is exported from the star-imported module.
            // If __all__ is defined, check if symbol is in it;
            // if not defined, check if symbol doesn't start with _
            let is_exported = match self.extract_dunder_all(module_file) {
                Some(all_names) => all_names.contains(symbol_name),
                None => !symbol_name.starts_with('_'),
            };

            if is_exported && let Some(result) = self.resolve(walk, module_file, symbol_name) {
                return Some(result);
            }
        }

        None
    }

    /// Follow an import to find where a symbol is defined
    fn follow_import(
        &self,
        walk: &mut ImportWalk,
        current_file: File,
        import: &ImportInfo,
    ) -> Option<(File, Option<String>)> {
        match import {
            ImportInfo::FromImport {
                module,
                name,
                level,
            } => {
                let module_file = self.module_file(current_file, module, *level)?;

                // Recursively resolve the symbol in the imported module
                self.resolve(walk, module_file, name)
            }
            ImportInfo::Import { module } => {
                let module_file = self.resolve_module_file(module)?;
                Some((module_file, None))
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

        if let Stmt::Assign(_) = stmt
            && let Some(Some(value)) = assigned_value(stmt, "__all__")
            && let Some(names) = extract_list_of_strings(value)
        {
            self.names = Some(names);
            return;
        }

        ast::visitor::walk_stmt(self, stmt);
    }
}

/// The import in `body` that binds `symbol_name`.
fn find_import(body: &[Stmt], symbol_name: &str) -> Option<ImportInfo> {
    let mut finder = ImportFinder {
        target_symbol: symbol_name,
        result: None,
    };
    finder.visit_body(body);
    finder.result
}

/// The name an import alias binds: the `as` name, else the imported name.
fn local_name(alias: &ast::Alias) -> &str {
    alias.asname.as_ref().unwrap_or(&alias.name).as_str()
}

/// The module of `from <module> import ...`, empty for `from . import ...`.
fn from_module(import_from: &ast::StmtImportFrom) -> String {
    import_from
        .module
        .as_ref()
        .map(|m| m.as_str().to_string())
        .unwrap_or_default()
}

/// Visitor to find a specific import for a symbol
struct ImportFinder<'s> {
    target_symbol: &'s str,
    result: Option<ImportInfo>,
}

impl<'a> Visitor<'a> for ImportFinder<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.result.is_some() {
            return; // Already found
        }

        match stmt {
            Stmt::ImportFrom(import_from) => {
                if let Some(alias) = import_from
                    .names
                    .iter()
                    .find(|alias| local_name(alias) == self.target_symbol)
                {
                    self.result = Some(ImportInfo::FromImport {
                        module: from_module(import_from),
                        name: alias.name.to_string(),
                        level: import_from.level,
                    });
                }
            }
            Stmt::Import(import) => {
                if let Some(alias) = import
                    .names
                    .iter()
                    .find(|alias| local_name(alias) == self.target_symbol)
                {
                    self.result = Some(ImportInfo::Import {
                        module: alias.name.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
}

/// Visitor to find all star imports
#[derive(Default)]
struct StarImportFinder {
    /// `(module, level)` for each `from module import *`
    star_imports: Vec<(String, u32)>,
}

impl<'a> Visitor<'a> for StarImportFinder {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if let Stmt::ImportFrom(import_from) = stmt {
            for alias in &import_from.names {
                if alias.name.as_str() == "*" {
                    self.star_imports
                        .push((from_module(import_from), import_from.level));
                }
            }
        }
    }
}

/// Visitor to find the names `typing` is bound to by `import` statements.
#[derive(Default)]
struct TypingAliasFinder {
    aliases: FxHashSet<String>,
}

impl<'a> Visitor<'a> for TypingAliasFinder {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            Stmt::Import(import) => {
                for alias in &import.names {
                    let module = alias.name.as_str();
                    if module == "typing" || module == "typing_extensions" {
                        self.aliases.insert(local_name(alias).to_string());
                    }
                }
            }
            _ => ast::visitor::walk_stmt(self, stmt),
        }
    }
}

/// Names the `typing` module is bound to by the module's `import` statements,
/// e.g. including `import typing as t`. `typing_extensions`  is included also.
fn collect_typing_aliases(suite: &[Stmt]) -> FxHashSet<String> {
    let mut finder = TypingAliasFinder::default();
    finder.visit_body(suite);
    finder.aliases
}

/// Whether an attribute expression is `<typing alias>.TYPE_CHECKING`, one of
/// the two forms `typing.TYPE_CHECKING` is conventionally guarded with. The
/// other form, the bare name, is tested by the caller.
///
/// The base must be a name the module actually bound to `typing`, so an
/// unrelated `settings.TYPE_CHECKING` flag is not mistaken for a typing guard.
fn is_typing_type_checking_attribute(
    attr: &ast::ExprAttribute,
    typing_aliases: &FxHashSet<String>,
) -> bool {
    attr.attr.as_str() == "TYPE_CHECKING"
        && matches!(attr.value.as_ref(), Expr::Name(base) if typing_aliases.contains(base.id.as_str()))
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
    use crate::python::cache::{ResolvedDefinition, resolver_environment_for_roots};
    use crate::python::model::DefinitionInfo;
    use crate::python::test_support::{
        disk_path, extract_definition_info, system_file, test_db, write_file,
    };
    use std::path::PathBuf;
    use tempfile::TempDir;

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
        let db = test_db();
        let search_paths = vec![workspace.to_path_buf(), site_packages.to_path_buf()];

        let def = extract_definition_info(&db, symbol_path, &search_paths).unwrap_or_else(|_| {
            panic!("{symbol_path} should resolve when site-packages is nested inside the workspace")
        });

        (
            def.definition_info,
            disk_path(&db, def.file),
            def.symbol_name,
        )
    }

    /// `resolve_relative_module_file` for an import in the file at `importer`.
    fn resolve_relative(
        resolver: &ImportResolver,
        importer: &Path,
        module: &str,
        level: u32,
    ) -> Option<PathBuf> {
        let importer = system_file(resolver.db, importer);
        resolver
            .resolve_relative_module_file(importer, Some(module), level)
            .map(|file| disk_path(resolver.db, file))
    }

    fn resolver_for<'db>(
        db: &'db dyn ty_module_resolver::Db,
        roots: &[PathBuf],
    ) -> ImportResolver<'db> {
        ImportResolver::new(db, resolver_environment_for_roots(db, roots))
    }

    /// Assert the definition is `ExportedClass`, found in a file named `file_name`.
    fn assert_exported_class(definition_info: &DefinitionInfo, file_path: &Path, file_name: &str) {
        assert!(
            file_path.ends_with(file_name),
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
        assert_exported_class(&definition_info, &file_path, "implementation.py");
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
        assert_exported_class(&definition_info, &file_path, "implementation.py");
    }

    // ==================== lazy `TYPE_CHECKING` export fallback ====================
    //
    // Packages sometimes only expose a name through a statically declared
    // `if TYPE_CHECKING:` import backed by a module-level `__getattr__`
    // (PEP 562), so type checkers see the import but nothing imports it at
    // runtime. The tests below build such a package and resolve it directly
    // through `ImportResolver::resolve_symbol`.

    /// Build a package `example_pkg` whose `__init__.py` is `package_init`
    /// and whose `_implementation.py` defines `class_name`.
    fn lazy_export_fixture(package_init: &str, class_name: &str) -> (TempDir, PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("root");
        let package = root.join("example_pkg");

        write_file(&package.join("__init__.py"), package_init);
        write_file(
            &package.join("_implementation.py"),
            &format!("class {class_name}:\n    pass\n"),
        );

        (temp, root)
    }

    /// Resolve `symbol_name` starting from `example_pkg/__init__.py` under `root`.
    fn resolve_lazy_export(root: &Path, symbol_name: &str) -> Option<(PathBuf, String)> {
        let db = test_db();
        let search_paths = vec![root.to_path_buf()];
        let resolver = resolver_for(&db, &search_paths);
        let init_file = system_file(&db, &root.join("example_pkg").join("__init__.py"));
        resolver
            .resolve_symbol(init_file, symbol_name)
            .map(|resolved| (disk_path(&db, resolved.file), resolved.name))
    }

    #[test]
    fn test_chain_ending_on_a_module_returns_the_requested_name() {
        // `X` -> `shim.Y` -> `import example_pkg._implementation as Y`: the
        // chain ends on a module, so the name is the one first asked for.
        let (_temp, root) = lazy_export_fixture("from .shim import Y as X\n", "Unused");
        write_file(
            &root.join("example_pkg").join("shim.py"),
            "import example_pkg._implementation as Y\n",
        );

        let (file_path, symbol_name) =
            resolve_lazy_export(&root, "X").expect("the module import should resolve");

        assert!(
            file_path.ends_with("_implementation.py"),
            "got {file_path:?}"
        );
        assert_eq!(symbol_name, "X");
    }

    #[test]
    fn test_lazy_export_via_type_checking_import_and_getattr() {
        let (_temp, root) = lazy_export_fixture(
            "from importlib import import_module\n\
             from typing import TYPE_CHECKING\n\
             \n\
             if TYPE_CHECKING:\n    \
                 from ._implementation import ExportedClass\n\
             \n\
             __all__ = [\"ExportedClass\"]\n\
             \n\
             \n\
             def __getattr__(name):\n    \
                 if name not in __all__:\n        \
                     raise AttributeError(name)\n    \
                 module = import_module(\"example_pkg._implementation\")\n    \
                 return getattr(module, name)\n",
            "ExportedClass",
        );

        let (file_path, symbol_name) =
            resolve_lazy_export(&root, "ExportedClass").expect("lazy export should resolve");

        assert!(file_path.ends_with("_implementation.py"));
        assert_eq!(symbol_name, "ExportedClass");
    }

    #[test]
    fn test_lazy_export_with_qualified_type_checking_guard() {
        let (_temp, root) = lazy_export_fixture(
            "import typing\n\
             from importlib import import_module\n\
             \n\
             if typing.TYPE_CHECKING:\n    \
                 from ._implementation import ExportedClass\n\
             \n\
             __all__ = [\"ExportedClass\"]\n\
             \n\
             \n\
             def __getattr__(name):\n    \
                 if name not in __all__:\n        \
                     raise AttributeError(name)\n    \
                 return getattr(import_module(\"example_pkg._implementation\"), name)\n",
            "ExportedClass",
        );

        let (file_path, symbol_name) = resolve_lazy_export(&root, "ExportedClass")
            .expect("`typing.TYPE_CHECKING` guard should resolve");

        assert!(file_path.ends_with("_implementation.py"));
        assert_eq!(symbol_name, "ExportedClass");
    }

    #[test]
    fn test_lazy_export_resolves_aliased_type_checking_import() {
        let (_temp, root) = lazy_export_fixture(
            "from importlib import import_module\n\
             from typing import TYPE_CHECKING\n\
             \n\
             if TYPE_CHECKING:\n    \
                 from ._implementation import InternalName as ExportedClass\n\
             \n\
             __all__ = [\"ExportedClass\"]\n\
             \n\
             \n\
             def __getattr__(name):\n    \
                 if name not in __all__:\n        \
                     raise AttributeError(name)\n    \
                 return getattr(import_module(\"example_pkg._implementation\"), \"InternalName\")\n",
            "InternalName",
        );

        let (file_path, symbol_name) = resolve_lazy_export(&root, "ExportedClass")
            .expect("aliased import should resolve by its public alias");

        assert!(file_path.ends_with("_implementation.py"));
        assert_eq!(symbol_name, "InternalName");
    }

    /// Without a module-level `__getattr__`, the `TYPE_CHECKING` import is
    /// exactly what it looks like: type-only. Nothing serves the name at
    /// runtime, so it must stay unresolved rather than false-negative.
    #[test]
    fn test_type_checking_import_without_getattr_remains_unresolved() {
        let (_temp, root) = lazy_export_fixture(
            "from typing import TYPE_CHECKING\n\
             \n\
             if TYPE_CHECKING:\n    \
                 from ._implementation import ExportedClass\n\
             \n\
             __all__ = [\"ExportedClass\"]\n",
            "ExportedClass",
        );

        assert_eq!(resolve_lazy_export(&root, "ExportedClass"), None);
    }

    /// The fallback only looks at the direct body of a module-level `if`
    /// whose test is `TYPE_CHECKING`. An import guarded by an unrelated
    /// condition, or nested inside a function or class, must not be mistaken
    /// for a package export even when `__all__` and `__getattr__` line up.
    #[test]
    fn test_type_checking_fallback_ignores_non_module_level_imports() {
        let (_temp, root) = lazy_export_fixture(
            "from importlib import import_module\n\
             \n\
             if some_runtime_flag:\n    \
                 from ._implementation import ExportedClass\n\
             \n\
             def _lazy():\n    \
                 from ._implementation import ExportedClass\n\
             \n\
             class _Wrapper:\n    \
                 from typing import TYPE_CHECKING\n    \
                 if TYPE_CHECKING:\n        \
                     from ._implementation import ExportedClass\n\
             \n\
             __all__ = [\"ExportedClass\"]\n\
             \n\
             \n\
             def __getattr__(name):\n    \
                 if name not in __all__:\n        \
                     raise AttributeError(name)\n    \
                 return getattr(import_module(\"example_pkg._implementation\"), name)\n",
            "ExportedClass",
        );

        assert_eq!(resolve_lazy_export(&root, "ExportedClass"), None);
    }

    /// `__all__` is the guard that proves the module means to re-export the
    /// name. With a module-level `__getattr__` and a `TYPE_CHECKING` import
    /// present but the name absent from `__all__`, the fallback must decline.
    #[test]
    fn test_type_checking_fallback_requires_name_in_dunder_all() {
        let (_temp, root) = lazy_export_fixture(
            "from importlib import import_module\n\
             from typing import TYPE_CHECKING\n\
             \n\
             if TYPE_CHECKING:\n    \
                 from ._implementation import ExportedClass\n\
             \n\
             __all__ = [\"SomethingElse\"]\n\
             \n\
             \n\
             def __getattr__(name):\n    \
                 if name not in __all__:\n        \
                     raise AttributeError(name)\n    \
                 return getattr(import_module(\"example_pkg._implementation\"), name)\n",
            "ExportedClass",
        );

        assert_eq!(resolve_lazy_export(&root, "ExportedClass"), None);
    }

    /// A dynamically built `__all__` cannot be read statically, so the
    /// membership guard cannot be satisfied and the fallback must decline.
    #[test]
    fn test_type_checking_fallback_requires_static_dunder_all() {
        let (_temp, root) = lazy_export_fixture(
            "from importlib import import_module\n\
             from typing import TYPE_CHECKING\n\
             \n\
             if TYPE_CHECKING:\n    \
                 from ._implementation import ExportedClass\n\
             \n\
             __all__ = [\"Exported\" + \"Class\"]\n\
             \n\
             \n\
             def __getattr__(name):\n    \
                 return getattr(import_module(\"example_pkg._implementation\"), name)\n",
            "ExportedClass",
        );

        assert_eq!(resolve_lazy_export(&root, "ExportedClass"), None);
    }

    /// The attribute form of the guard must name the `typing` module. An
    /// unrelated object that happens to carry a `TYPE_CHECKING` attribute is
    /// a runtime flag, not a typing guard.
    #[test]
    fn test_type_checking_fallback_ignores_non_typing_attribute_guard() {
        let (_temp, root) = lazy_export_fixture(
            "from importlib import import_module\n\
             import settings\n\
             \n\
             if settings.TYPE_CHECKING:\n    \
                 from ._implementation import ExportedClass\n\
             \n\
             __all__ = [\"ExportedClass\"]\n\
             \n\
             \n\
             def __getattr__(name):\n    \
                 return getattr(import_module(\"example_pkg._implementation\"), name)\n",
            "ExportedClass",
        );

        assert_eq!(resolve_lazy_export(&root, "ExportedClass"), None);
    }

    /// `import typing as t` is still a typing guard, so the aliased attribute
    /// form must resolve.
    #[test]
    fn test_lazy_export_with_aliased_typing_module_guard() {
        let (_temp, root) = lazy_export_fixture(
            "import typing as t\n\
             from importlib import import_module\n\
             \n\
             if t.TYPE_CHECKING:\n    \
                 from ._implementation import ExportedClass\n\
             \n\
             __all__ = [\"ExportedClass\"]\n\
             \n\
             \n\
             def __getattr__(name):\n    \
                 return getattr(import_module(\"example_pkg._implementation\"), name)\n",
            "ExportedClass",
        );

        let (file_path, symbol_name) = resolve_lazy_export(&root, "ExportedClass")
            .expect("an aliased `typing` guard should resolve");

        assert!(file_path.ends_with("_implementation.py"));
        assert_eq!(symbol_name, "ExportedClass");
    }

    /// A `TYPE_CHECKING` import that cannot be followed must not short-circuit
    /// the remaining resolution paths. Here the name is a genuine re-export
    /// that the star import serves, so the fallback has to fall through
    /// instead of returning `None`.
    #[test]
    fn test_unfollowable_type_checking_import_falls_through_to_star_import() {
        let (_temp, root) = lazy_export_fixture(
            "from importlib import import_module\n\
             from typing import TYPE_CHECKING\n\
             from ._implementation import *\n\
             \n\
             if TYPE_CHECKING:\n    \
                 from ._missing import ExportedClass\n\
             \n\
             __all__ = [\"ExportedClass\"]\n\
             \n\
             \n\
             def __getattr__(name):\n    \
                 return getattr(import_module(\"example_pkg._implementation\"), name)\n",
            "ExportedClass",
        );

        let (file_path, symbol_name) = resolve_lazy_export(&root, "ExportedClass")
            .expect("an unfollowable `TYPE_CHECKING` import should fall through");

        assert!(file_path.ends_with("_implementation.py"));
        assert_eq!(symbol_name, "ExportedClass");
    }

    /// `typing` may be bound by a version-conditional import. The guard is
    /// still a typing guard, so the alias scan must look past the top-level
    /// statement list.
    #[test]
    fn test_lazy_export_with_version_conditional_typing_import() {
        let (_temp, root) = lazy_export_fixture(
            "import sys\n\
             from importlib import import_module\n\
             \n\
             if sys.version_info >= (3, 11):\n    \
                 import typing\n\
             else:\n    \
                 import typing_extensions as typing\n\
             \n\
             if typing.TYPE_CHECKING:\n    \
                 from ._implementation import ExportedClass\n\
             \n\
             __all__ = [\"ExportedClass\"]\n\
             \n\
             \n\
             def __getattr__(name):\n    \
                 return getattr(import_module(\"example_pkg._implementation\"), name)\n",
            "ExportedClass",
        );

        let (file_path, symbol_name) = resolve_lazy_export(&root, "ExportedClass")
            .expect("a conditionally imported `typing` should still guard the export");

        assert!(file_path.ends_with("_implementation.py"));
        assert_eq!(symbol_name, "ExportedClass");
    }

    /// `typing_extensions` re-exports `TYPE_CHECKING`, so it licenses a guard
    /// on its own. The module is bound under its own name, so the accepted
    /// module name and the accepted base name cannot stand in for each other.
    #[test]
    fn test_lazy_export_with_typing_extensions_only_guard() {
        let (_temp, root) = lazy_export_fixture(
            "import typing_extensions\n\
             from importlib import import_module\n\
             \n\
             if typing_extensions.TYPE_CHECKING:\n    \
                 from ._implementation import ExportedClass\n\
             \n\
             __all__ = [\"ExportedClass\"]\n\
             \n\
             \n\
             def __getattr__(name):\n    \
                 return getattr(import_module(\"example_pkg._implementation\"), name)\n",
            "ExportedClass",
        );

        let (file_path, symbol_name) = resolve_lazy_export(&root, "ExportedClass")
            .expect("`typing_extensions` alone should license the guard");

        assert!(file_path.ends_with("_implementation.py"));
        assert_eq!(symbol_name, "ExportedClass");
    }

    /// A `typing` name bound inside a function is local to it and cannot be
    /// the base of a module-level guard, so it must not license one.
    #[test]
    fn test_type_checking_fallback_ignores_function_local_typing_import() {
        let (_temp, root) = lazy_export_fixture(
            "from importlib import import_module\n\
             \n\
             def _uses_typing():\n    \
                 import typing as settings\n\
             \n\
             if settings.TYPE_CHECKING:\n    \
                 from ._implementation import ExportedClass\n\
             \n\
             __all__ = [\"ExportedClass\"]\n\
             \n\
             \n\
             def __getattr__(name):\n    \
                 return getattr(import_module(\"example_pkg._implementation\"), name)\n",
            "ExportedClass",
        );

        assert_eq!(resolve_lazy_export(&root, "ExportedClass"), None);
    }

    /// End-to-end: the same lazy-export pattern resolves through
    /// `extract_definition_info`, the entry point diagnostics,
    /// hover, and go-to-definition all share.
    #[test]
    fn test_extract_definition_info_resolves_lazy_export() {
        let (_temp, root) = lazy_export_fixture(
            "from importlib import import_module\n\
             from typing import TYPE_CHECKING\n\
             \n\
             if TYPE_CHECKING:\n    \
                 from ._implementation import ExportedClass\n\
             \n\
             __all__ = [\"ExportedClass\"]\n\
             \n\
             \n\
             def __getattr__(name):\n    \
                 if name not in __all__:\n        \
                     raise AttributeError(name)\n    \
                 module = import_module(\"example_pkg._implementation\")\n    \
                 return getattr(module, name)\n",
            "ExportedClass",
        );

        let db = test_db();
        let search_paths = vec![root.clone()];

        let ResolvedDefinition {
            definition_info,
            file,
            symbol_name,
            ..
        } = extract_definition_info(&db, "example_pkg.ExportedClass", &search_paths)
            .expect("the lazy export should resolve end-to-end");
        let file_path = disk_path(&db, file);

        assert_eq!(symbol_name, "ExportedClass");
        assert_exported_class(&definition_info, &file_path, "_implementation.py");
    }

    // ==================== PEP 810 `lazy` imports ====================
    //
    // `lazy import x` and `lazy from x import y` bind the same names as their
    // eager forms and only defer loading, so re-exports through them resolve
    // like ordinary imports. They are unrelated to the `__getattr__` lazy
    // exports above.

    /// Whether `example_pkg/__init__.py` under `root` parses to at least one
    /// import with `is_lazy` set, so the tests below can't pass by accident
    /// if the parser stops recognising the syntax.
    fn package_init_has_lazy_import(root: &Path) -> bool {
        let db = test_db();
        let init_file = system_file(&db, &root.join("example_pkg").join("__init__.py"));
        let parsed = parsed_python_module(&db, init_file, PythonVersion::latest_ty());
        parsed.suite().iter().any(|stmt| match stmt {
            Stmt::Import(import) => import.is_lazy,
            Stmt::ImportFrom(import_from) => import_from.is_lazy,
            _ => false,
        })
    }

    #[test]
    fn test_lazy_from_import_reexport_resolves() {
        let (_temp, root) = lazy_export_fixture(
            "lazy from ._implementation import ExportedClass\n",
            "ExportedClass",
        );
        assert!(package_init_has_lazy_import(&root));

        let (file_path, symbol_name) =
            resolve_lazy_export(&root, "ExportedClass").expect("lazy re-export should resolve");

        assert!(
            file_path.ends_with("_implementation.py"),
            "got {file_path:?}"
        );
        assert_eq!(symbol_name, "ExportedClass");
    }

    #[test]
    fn test_aliased_lazy_from_import_reexport_resolves() {
        let (_temp, root) = lazy_export_fixture(
            "lazy from ._implementation import _Hidden as ExportedClass\n",
            "_Hidden",
        );
        assert!(package_init_has_lazy_import(&root));

        let (file_path, symbol_name) =
            resolve_lazy_export(&root, "ExportedClass").expect("aliased lazy re-export");

        assert!(
            file_path.ends_with("_implementation.py"),
            "got {file_path:?}"
        );
        assert_eq!(symbol_name, "_Hidden");
    }

    #[test]
    fn test_lazy_module_import_resolves_to_the_module() {
        let (_temp, root) = lazy_export_fixture(
            "lazy import example_pkg._implementation as impl\n",
            "Unused",
        );
        assert!(package_init_has_lazy_import(&root));

        let (file_path, symbol_name) =
            resolve_lazy_export(&root, "impl").expect("lazy module import should resolve");

        assert!(
            file_path.ends_with("_implementation.py"),
            "got {file_path:?}"
        );
        assert_eq!(symbol_name, "impl");
    }

    /// End-to-end through `extract_definition_info`, with the lazy import
    /// in the middle of a two-hop chain.
    #[test]
    fn test_extract_definition_info_follows_lazy_reexport_chain() {
        let (_temp, root) =
            lazy_export_fixture("lazy from .shim import ExportedClass\n", "ExportedClass");
        write_file(
            &root.join("example_pkg").join("shim.py"),
            "lazy from ._implementation import ExportedClass\n",
        );
        assert!(package_init_has_lazy_import(&root));

        let db = test_db();
        let search_paths = vec![root.clone()];

        let ResolvedDefinition {
            definition_info,
            file,
            symbol_name,
            ..
        } = extract_definition_info(&db, "example_pkg.ExportedClass", &search_paths)
            .expect("the lazy re-export chain should resolve end-to-end");
        let file_path = disk_path(&db, file);

        assert_eq!(symbol_name, "ExportedClass");
        assert_exported_class(&definition_info, &file_path, "_implementation.py");
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
        assert_exported_class(&definition_info, &file_path, "implementation.py");
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

        let db = test_db();
        let search_paths = vec![repo.clone(), repo.join("src")];

        let ResolvedDefinition {
            definition_info,
            file,
            symbol_name,
            ..
        } = extract_definition_info(&db, "pkg.ExportedClass", &search_paths)
            .expect("ExportedClass should resolve through the sibling module");
        let file_path = disk_path(&db, file);

        assert_eq!(symbol_name, "ExportedClass");
        assert!(
            file_path.starts_with(&package),
            "expected the sibling module under src/pkg, got {}",
            file_path.display()
        );
        assert_exported_class(&definition_info, &file_path, "implementation.py");
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

        let db = test_db();
        let search_paths = vec![repo.clone(), repo.join("src")];

        let result = extract_definition_info(&db, "pkg.ExportedClass", &search_paths);

        assert!(
            result.is_err(),
            "expected no resolution, got {:?}",
            result.map(|def| disk_path(&db, def.file))
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

        let db = test_db();
        let search_paths = vec![repo.clone(), repo.join("src")];
        let resolver = resolver_for(&db, &search_paths);
        let importer = package.join("__init__.py");

        assert_eq!(
            resolve_relative(&resolver, &importer, "implementation", 1),
            Some(package.join("implementation.py")),
        );
        assert_eq!(resolve_relative(&resolver, &importer, "missing", 1), None,);
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

        let db = test_db();
        let search_paths = vec![repo.clone()];
        let resolver = resolver_for(&db, &search_paths);
        let importer = sub_package.join("__init__.py");

        assert_eq!(
            resolve_relative(&resolver, &importer, "implementation", 2),
            Some(package.join("implementation.py")),
        );
        assert_eq!(
            resolve_relative(&resolver, &importer, "", 1),
            Some(sub_package.join("__init__.py")),
        );
        assert_eq!(
            resolve_relative(&resolver, &importer, "", 2),
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

        let db = test_db();
        let search_paths = vec![repo.clone()];
        let resolver = resolver_for(&db, &search_paths);
        let importer = repo.join("pkg").join("__init__.py");

        // `from ...outside import ExportedClass` reaches above the root.
        assert_eq!(resolve_relative(&resolver, &importer, "outside", 3), None,);
        assert_eq!(resolve_relative(&resolver, &importer, "inside", 3), None,);
    }

    /// `join_module_parts` is tested directly because at the resolver level a
    /// rejected part usually names a file that does not exist anyway, so the
    /// assertion would hold with or without the guard.
    #[test]
    fn test_join_module_parts_rejects_parts_that_are_not_plain_names() {
        let base = SystemPath::new("/root");

        assert_eq!(
            join_module_parts(base, "a.b"),
            Some(base.join("a").join("b"))
        );
        // Landing back on `base` only means something for a relative import
        // (`from . import x` passes ""). `resolved_module_file` rejects these
        // rather than resolving a malformed `_target_` to a search root.
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

    /// `resolve_module_file` delegates to `resolved_module_file`, whose escape
    /// guard is covered in `cache`. This only checks the delegation is
    /// wired up, plus the same guard on the relative path, which resolves
    /// without going through that query.
    #[test]
    fn test_resolve_module_path_delegates_to_the_cached_query() {
        use crate::database::tests::TestDb;
        use ruff_db::system::DbWithWritableSystem;

        let mut db = TestDb::new();
        db.write_file("/root/pkg/__init__.py", "")
            .expect("write pkg init");
        db.write_file("/root/pkg/inside.py", "")
            .expect("write inside");
        db.write_file("/outside.py", "").expect("write outside");

        let search_paths = vec![PathBuf::from("/root")];
        let resolver = resolver_for(&db, &search_paths);

        // Positive control: a guard that rejected everything would fail here.
        assert_eq!(
            resolver
                .resolve_module_file("pkg.inside")
                .map(|file| disk_path(&db, file)),
            Some(PathBuf::from("/root/pkg/inside.py")),
        );

        // `/outside.py` exists, so both of these fail without the guard.
        assert_eq!(
            resolver.resolve_module_file("/outside"),
            None,
            "an absolute name must not replace the search root",
        );
        assert_eq!(
            resolve_relative(&resolver, Path::new("/root/pkg/__init__.py"), "/outside", 1),
            None,
        );
    }
}
