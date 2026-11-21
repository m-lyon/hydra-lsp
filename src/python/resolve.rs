//! Resolving a `_target_` string to its definition: following re-exports,
//! class attribute chains and base classes across files.

use super::cache::{
    ResolvedDefinition, SymbolName, class_key, class_parent_attribute, inherited_class_members,
    resolve_module_by_name,
};
use super::extract::{extract_class_info, extract_function_signature, extract_method_info};
use super::import_resolver::{ImportResolver, ResolvedSymbol};
use super::model::{ClassInfo, DefinitionInfo, FunctionSignature, MethodInfo, ResolveError};
use std::path::{Path, PathBuf};
use ty_module_resolver::ResolverEnvironment;

/// Result of resolving a class attribute chain
enum ClassAttributeChainResult {
    /// Chain resolved to a method
    Method {
        file_path: PathBuf,
        class_name: String,
        method_name: String,
    },
    /// Chain resolved to a class
    Class {
        file_path: PathBuf,
        class_name: String,
    },
}

/// Split a dotted path at its last dot, into module path and symbol name.
/// Example: "myproject.models.MyClass" -> ("myproject.models", "MyClass")
fn split_target(target: &str) -> Option<(&str, &str)> {
    target.rsplit_once('.')
}

/// Resolve an attribute chain through classes.
///
/// Given a starting class and an attribute chain like ["nested", "nested_class"],
/// follows the chain by looking up each class attribute to find the final
/// class, or a method on it when the last attribute names one.
fn resolve_attribute_chain<'db>(
    db: &'db dyn ty_module_resolver::Db,
    file_path: &Path,
    starting_class: &str,
    attribute_chain: &[&str],
    environment: ResolverEnvironment<'db>,
) -> Option<ClassAttributeChainResult> {
    let mut current_file = file_path.to_path_buf();
    let mut current_class = starting_class.to_string();
    let resolver = ImportResolver::new(db, environment);
    let python_version = environment.python_version(db);

    for (i, attr) in attribute_chain.iter().enumerate() {
        let is_last = i == attribute_chain.len() - 1;

        // The last attribute may be a method of the current class.
        if is_last
            && extract_method_info(db, &current_file, &current_class, attr, python_version)
                .is_some()
        {
            return Some(ClassAttributeChainResult::Method {
                file_path: current_file,
                class_name: current_class,
                method_name: attr.to_string(),
            });
        }

        // Otherwise it must be a class attribute, possibly inherited. The
        // salsa-tracked class_parent_attribute memoises each (class, attr)
        // pair so shared parent classes are only walked once per revision.
        let class_key = class_key(db, &current_file, &current_class);
        let attr_key = SymbolName::new(db, attr.to_string());
        let cached_attr = class_parent_attribute(db, class_key, attr_key, environment);
        let value = cached_attr.get()?.info.value.as_str();

        // The value could be a simple name or a dotted path
        if let Some((module_path, class_name)) = split_target(value) {
            current_file = resolver.resolve_module_file(module_path)?;
            current_class = class_name.to_string();
        } else if extract_class_info(db, &current_file, value, python_version).is_some() {
            // A simple name defined in the same file
            current_class = value.to_string();
        } else {
            // A simple name imported into the file
            let resolved = resolver.resolve_symbol(&current_file, value)?;
            current_file = resolved.file;
            current_class = resolved.name;
        }
    }

    Some(ClassAttributeChainResult::Class {
        file_path: current_file,
        class_name: current_class,
    })
}

/// Resolve a base class expression to the file and name of the class.
///
/// This function handles:
/// - Simple names: `ParentClass` - looks up in the current file's imports
/// - Qualified names: `module.ClassName` - resolves the module path
pub(super) fn resolve_base_class<'db>(
    db: &'db dyn ty_module_resolver::Db,
    current_file: &Path,
    base_class_expr: &str,
    environment: ResolverEnvironment<'db>,
) -> Option<ResolvedSymbol> {
    let base_class_expr = base_class_name(base_class_expr);

    // Qualified name like `module.ClassName`. The cached lookup is shared
    // across the MRO walk and across sibling hover requests.
    if let Some((module_path, class_name)) = split_target(base_class_expr)
        && let Some(file) = resolve_module_by_name(db, module_path, environment)
    {
        return Some(ResolvedSymbol {
            file,
            name: class_name.to_string(),
        });
    }

    // Simple name - try to resolve through imports in the current file
    let resolver = ImportResolver::new(db, environment);
    if let Some(resolved) = resolver.resolve_symbol(current_file, base_class_expr) {
        return Some(resolved);
    }

    // Check if the class is defined in the same file
    extract_class_info(
        db,
        current_file,
        base_class_expr,
        environment.python_version(db),
    )
    .map(|_| ResolvedSymbol {
        file: current_file.to_path_buf(),
        name: base_class_expr.to_string(),
    })
}

/// Extract class information, following re-exports if necessary.
/// Returns the ClassInfo and the file path where it was found.
///
/// This also resolves missing properties (docstring, __init__) from parent classes
/// when they are not defined in the child class.
fn extract_class_info_with_imports<'db>(
    db: &'db dyn ty_module_resolver::Db,
    file_path: &Path,
    class_name: &str,
    environment: ResolverEnvironment<'db>,
) -> Option<(ClassInfo, PathBuf)> {
    let python_version = environment.python_version(db);
    let (mut class_info, resolved_file) =
        with_reexports(db, file_path, class_name, environment, |file, name| {
            extract_class_info(db, file, name, python_version)
        })?;

    // Resolve missing properties from parent classes via the memoised salsa query.
    // inherited_class_members walks the MRO recursively and caches results per (class, environment),
    // so shared parent classes across different child-class lookups are resolved only once.
    if class_info.docstring.is_none() || class_info.init_signature.is_none() {
        let class_key = class_key(db, &resolved_file, &class_info.name);
        let parent_docs = inherited_class_members(db, class_key, environment);
        parent_docs.fill_missing(
            &mut class_info.docstring,
            &mut class_info.init_signature,
            &mut class_info.new_signature,
        );
        class_info.all_bases_resolved = parent_docs.all_bases_resolved();
    }

    if class_info.init_signature.is_none() {
        class_info.init_signature = class_info.new_signature.take();
    }

    Some((class_info, resolved_file))
}

/// Extract function signature, following re-exports if necessary.
/// Returns the FunctionSignature and the file path where it was found.
fn extract_function_signature_with_imports<'db>(
    db: &'db dyn ty_module_resolver::Db,
    file_path: &Path,
    function_name: &str,
    environment: ResolverEnvironment<'db>,
) -> Option<(FunctionSignature, PathBuf)> {
    let python_version = environment.python_version(db);
    with_reexports(db, file_path, function_name, environment, |file, name| {
        extract_function_signature(db, file, name, python_version)
    })
}

/// Run `extract` for `name` in `file_path`, and if that finds nothing, again
/// where the file's imports say `name` really lives. Returns the result and
/// the file it came from.
fn with_reexports<'db, T>(
    db: &'db dyn ty_module_resolver::Db,
    file_path: &Path,
    name: &str,
    environment: ResolverEnvironment<'db>,
    extract: impl Fn(&Path, &str) -> Option<T>,
) -> Option<(T, PathBuf)> {
    if let Some(found) = extract(file_path, name) {
        return Some((found, file_path.to_path_buf()));
    }

    let resolved = ImportResolver::new(db, environment).resolve_symbol(file_path, name)?;
    let found = extract(&resolved.file, &resolved.name)?;
    Some((found, resolved.file))
}

/// Extract definition info (function, class or method) from a target
/// string, along with the file it was found in and the module name and
/// symbol name the target resolved to.
pub(super) fn extract_definition_info<'db>(
    db: &'db dyn ty_module_resolver::Db,
    target: &str,
    environment: ResolverEnvironment<'db>,
) -> Result<ResolvedDefinition, ResolveError> {
    let Some((module_name, symbol_name)) = split_target(target) else {
        return Err(ResolveError::InvalidFormat(target.to_string()));
    };
    let found = |definition_info, file_path, symbol_name| ResolvedDefinition {
        definition_info,
        file_path,
        module_name: module_name.to_string(),
        symbol_name,
    };

    // Track whether we found the module but not the symbol
    let mut module_found = false;

    // First try standard resolution: module.symbol where symbol is a function or class
    if let Some(file_path) = resolve_module_by_name(db, module_name, environment) {
        module_found = true;

        // Try to extract as function first (with import resolution)
        if let Some((func_sig, resolved_file)) =
            extract_function_signature_with_imports(db, &file_path, symbol_name, environment)
        {
            return Ok(found(
                DefinitionInfo::Function(func_sig),
                resolved_file,
                symbol_name.to_string(),
            ));
        }

        // Try to extract as class (with import resolution)
        if let Some((class_info, resolved_file)) =
            extract_class_info_with_imports(db, &file_path, symbol_name, environment)
        {
            return Ok(found(
                DefinitionInfo::Class(class_info),
                resolved_file,
                symbol_name.to_string(),
            ));
        }
    }

    // Try to interpret as Class.method pattern (e.g., "my_module.MyClass.from_config")
    // or nested pattern (e.g., "my_module.OuterClass.nested.nested_class.method")
    // or nested class pattern (e.g., "my_module.OuterClass.nested.nested_class")
    match resolve_class_attribute_chain(db, target, environment) {
        Some(ClassAttributeChainResult::Method {
            file_path: resolved_file,
            class_name,
            method_name,
        }) => {
            if let Some((method_info, final_file)) = extract_method_info_with_imports(
                db,
                &resolved_file,
                &class_name,
                &method_name,
                environment,
            ) {
                return Ok(found(
                    DefinitionInfo::Method(method_info),
                    final_file,
                    format!("{}.{}", class_name, method_name),
                ));
            }
        }
        Some(ClassAttributeChainResult::Class {
            file_path: resolved_file,
            class_name,
        }) => {
            if let Some((class_info, final_file)) =
                extract_class_info_with_imports(db, &resolved_file, &class_name, environment)
            {
                return Ok(found(
                    DefinitionInfo::Class(class_info),
                    final_file,
                    class_name,
                ));
            }
        }
        None => {}
    }

    // Return appropriate error based on whether module was found
    if module_found {
        Err(ResolveError::UnresolvedSymbol {
            module: module_name.to_string(),
            symbol: symbol_name.to_string(),
        })
    } else {
        Err(ResolveError::UnresolvedModule(module_name.to_string()))
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
fn resolve_class_attribute_chain<'db>(
    db: &'db dyn ty_module_resolver::Db,
    target: &str,
    environment: ResolverEnvironment<'db>,
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

    let python_version = environment.python_version(db);

    for module_end_idx in (1..parts.len() - 1).rev() {
        let module_path = parts[..module_end_idx].join(".");
        let remaining_parts = &parts[module_end_idx..];

        // Cached salsa lookup: same (module_path, environment) → cache hit on
        // second hover for any symbol in the same module.
        let Some(file_path) = resolve_module_by_name(db, &module_path, environment) else {
            continue;
        };

        // The first remaining part should be a class (or function that returns a class, but we focus on classes)
        let first_symbol = remaining_parts[0];

        // Check if this is a class
        if let Some((class_info, resolved_file)) =
            extract_class_info_with_imports(db, &file_path, first_symbol, environment)
        {
            if remaining_parts.len() == 2 {
                // Simple case: Class.method
                let method_name = remaining_parts[1];
                // Check if method exists (salsa-cached)
                if extract_method_info(
                    db,
                    &resolved_file,
                    &class_info.name,
                    method_name,
                    python_version,
                )
                .is_some()
                {
                    return Some(ClassAttributeChainResult::Method {
                        file_path: resolved_file,
                        class_name: class_info.name,
                        method_name: method_name.to_string(),
                    });
                }
            } else if remaining_parts.len() > 2 {
                // Nested case: Class.attr1.attr2...method_or_class
                let attribute_chain = &remaining_parts[1..];
                if let Some(result) = resolve_attribute_chain(
                    db,
                    &resolved_file,
                    &class_info.name,
                    attribute_chain,
                    environment,
                ) {
                    return Some(result);
                }
            }
        }
    }

    None
}

/// Extract method info, following re-exports if necessary.
/// Returns the MethodInfo and the file path where it was found.
fn extract_method_info_with_imports<'db>(
    db: &'db dyn ty_module_resolver::Db,
    file_path: &Path,
    class_name: &str,
    method_name: &str,
    environment: ResolverEnvironment<'db>,
) -> Option<(MethodInfo, PathBuf)> {
    let python_version = environment.python_version(db);
    // The class is what gets re-exported, so it is the name to follow.
    with_reexports(db, file_path, class_name, environment, |file, class| {
        extract_method_info(db, file, class, method_name, python_version)
    })
}

/// The class named by a base-class expression, with any generic subscript
/// removed: `Sequence[_T_co]` is `Sequence` and `typing.Generic[T]` is
/// `typing.Generic`.
pub(super) fn base_class_name(base_class_expr: &str) -> &str {
    base_class_expr
        .split('[')
        .next()
        .unwrap_or(base_class_expr)
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::python::cache::resolver_environment_for_roots;
    use crate::python::extract::{module_defines_top_level, path_is_file, read_source};
    use crate::python::format::{format_class, format_function, format_method};
    use crate::python::model::ParameterKind;
    use ruff_python_ast::PythonVersion;

    use crate::python::test_support::{extract_definition_info, get_simple_test_dir, test_db};

    /// `extract_class_info_with_imports` with `roots` as the search roots.
    fn extract_class_info_with_imports(
        db: &dyn ty_module_resolver::Db,
        file_path: &Path,
        class_name: &str,
        roots: &[PathBuf],
    ) -> Option<(ClassInfo, PathBuf)> {
        super::extract_class_info_with_imports(
            db,
            file_path,
            class_name,
            resolver_environment_for_roots(db, roots),
        )
    }

    #[test]
    fn test_extract_definition_info_error_kinds() {
        let examples_dir = get_simple_test_dir();
        let search_paths = [examples_dir, PathBuf::from(".")];
        let resolve = |target: &str| {
            extract_definition_info(&test_db(), target, &search_paths)
                .err()
                .expect("target should not resolve")
        };

        assert_eq!(
            resolve("InvalidTarget"),
            ResolveError::InvalidFormat("InvalidTarget".to_string())
        );
        assert_eq!(
            resolve("no_such_module_xyz.Thing"),
            ResolveError::UnresolvedModule("no_such_module_xyz".to_string())
        );
        assert_eq!(
            resolve("my_module.NoSuchSymbolXyz"),
            ResolveError::UnresolvedSymbol {
                module: "my_module".to_string(),
                symbol: "NoSuchSymbolXyz".to_string(),
            }
        );
    }

    // ==================== split_target tests ====================

    #[test]
    fn test_split_target() {
        assert_eq!(
            split_target("myproject.models.MyClass"),
            Some(("myproject.models", "MyClass"))
        );
    }

    #[test]
    fn test_split_target_short() {
        assert_eq!(split_target("module.Class"), Some(("module", "Class")));
    }

    #[test]
    fn test_split_target_invalid() {
        assert_eq!(split_target("InvalidTarget"), None);
    }

    #[test]
    fn test_split_target_deeply_nested() {
        assert_eq!(
            split_target("a.b.c.d.e.FinalClass"),
            Some(("a.b.c.d.e", "FinalClass"))
        );
    }

    // ==================== inherited class tests ====================

    #[test]
    fn test_extract_child_without_init_inherits_from_parent() {
        let examples_dir = get_simple_test_dir();
        let test_file = examples_dir.join("my_module.py");

        // First verify that direct extraction does NOT give us __init__
        let direct_info = extract_class_info(
            &test_db(),
            &test_file,
            "ChildWithoutInit",
            PythonVersion::latest_ty(),
        )
        .unwrap();
        assert_eq!(direct_info.name, "ChildWithoutInit");
        assert!(
            direct_info.init_signature.is_none(),
            "Direct extraction should not have __init__"
        );
        assert!(direct_info.docstring.is_some(), "Should have own docstring");

        // Now verify that extract_class_info_with_imports gives us inherited __init__
        let search_paths = vec![examples_dir.clone()];
        let (class_info, _) = extract_class_info_with_imports(
            &test_db(),
            &test_file,
            "ChildWithoutInit",
            &search_paths,
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
        let (class_info, _) = extract_class_info_with_imports(
            &test_db(),
            &test_file,
            "GrandchildWithoutInit",
            &search_paths,
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
        let (class_info, _) = extract_class_info_with_imports(
            &test_db(),
            &test_file,
            "ChildWithOwnInit",
            &search_paths,
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

    /// Resolve `target` against `dir`, panicking if it does not resolve.
    fn resolve_in(dir: &Path, target: &str) -> DefinitionInfo {
        extract_definition_info(&test_db(), target, &[dir.to_path_buf(), PathBuf::from(".")])
            .unwrap_or_else(|e| panic!("{target} should resolve: {e:?}"))
            .definition_info
    }

    fn resolve_class(dir: &Path, target: &str) -> ClassInfo {
        match resolve_in(dir, target) {
            DefinitionInfo::Class(class_info) => class_info,
            other => panic!("expected a class, got {other:?}"),
        }
    }

    /// Resolve `target`, a method, against the `simple` fixture.
    fn resolve_method(target: &str) -> MethodInfo {
        match resolve_in(&get_simple_test_dir(), target) {
            DefinitionInfo::Method(method_info) => method_info,
            other => panic!("expected a method, got {other:?}"),
        }
    }

    fn param_names(sig: &FunctionSignature) -> Vec<&str> {
        sig.parameters.iter().map(|p| p.name.as_str()).collect()
    }

    /// The method's class and method name, and its classmethod and
    /// staticmethod flags.
    fn method_shape(method: &MethodInfo) -> (&str, &str, bool, bool) {
        (
            method.class_name.as_str(),
            method.method_name.as_str(),
            method.is_classmethod,
            method.is_staticmethod,
        )
    }

    /// Write `source` to `name` in a new temporary directory.
    fn temp_module(name: &str, source: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, source).unwrap();
        (dir, path)
    }

    #[test]
    fn test_extract_definition_child_without_init() {
        let class_info = resolve_class(&get_simple_test_dir(), "my_module.ChildWithoutInit");
        assert_eq!(class_info.name, "ChildWithoutInit");
        // Inherited from ParentWithInit.
        let init_sig = class_info
            .init_signature
            .expect("Should have inherited __init__");
        assert_eq!(init_sig.parameters.len(), 3);
        assert_eq!(init_sig.parameters[1].name, "name");
    }

    #[test]
    fn test_extract_nested_inherited_classmethod() {
        let method =
            resolve_method("class_methods.InheritedNested.nested.nested_class.from_config");
        assert_eq!(
            method_shape(&method),
            ("ModelFactory", "from_config", true, false)
        );
        assert_eq!(param_names(&method.signature), ["cls", "config"]);
    }

    #[test]
    fn test_extract_nested_inherited_staticmethod() {
        let method =
            resolve_method("class_methods.InheritedNested.nested.nested_class.compute_size");
        assert_eq!(
            method_shape(&method),
            ("ModelFactory", "compute_size", false, true)
        );
        assert_eq!(param_names(&method.signature), ["dim1", "dim2"]);
    }

    #[test]
    fn test_extract_nested_inherited_init() {
        let class_info = resolve_class(
            &get_simple_test_dir(),
            "class_methods.NestedTwice.nested.inherited_nested_class",
        );
        assert_eq!(class_info.name, "InheritedFactory");
        // Inherited from ModelFactory.
        let init_sig = class_info
            .init_signature
            .expect("Should have inherited __init__");
        assert_eq!(param_names(&init_sig), ["self", "input_dim", "output_dim"]);
    }

    // ==================== extract_definition_info tests ====================

    #[test]
    fn test_extract_definition_function() {
        match resolve_in(&get_simple_test_dir(), "my_module.simple_function") {
            DefinitionInfo::Function(sig) => assert_eq!(sig.name, "simple_function"),
            other => panic!("expected a function, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_definition_class() {
        let class_info = resolve_class(&get_simple_test_dir(), "my_module.SimpleClass");
        assert_eq!(class_info.name, "SimpleClass");
    }

    #[test]
    fn test_extract_definition_from_package() {
        let class_info = resolve_class(
            &get_simple_test_dir(),
            "test_package.submodule.SubmoduleClass",
        );
        assert_eq!(class_info.name, "SubmoduleClass");
    }

    #[test]
    fn test_extract_definition_nonexistent() {
        let result = extract_definition_info(
            &test_db(),
            "my_module.NonexistentSymbol",
            &[get_simple_test_dir(), PathBuf::from(".")],
        );
        assert!(result.is_err());
    }

    // ==================== Re-export resolution tests ====================

    fn get_reexport_test_dir() -> PathBuf {
        crate::python::test_support::fixture_dir("reexport")
    }

    /// `mylib/__init__.py` imports from `mylib.modules`, whose `__init__.py`
    /// imports from `mylib.modules.linear`, which defines `Linear`.
    #[test]
    fn test_reexport_via_subpackage() {
        let class_info = resolve_class(&get_reexport_test_dir(), "mylib.Linear");
        assert_eq!(class_info.name, "Linear");
        let init = class_info.init_signature.expect("Linear has an __init__");
        assert_eq!(
            param_names(&init),
            ["self", "in_features", "out_features", "bias"]
        );
    }

    /// `mylib/__init__.py` imports `OriginalClass as AliasedClass`.
    #[test]
    fn test_reexport_with_alias() {
        let class_info = resolve_class(&get_reexport_test_dir(), "mylib.AliasedClass");
        assert_eq!(class_info.name, "OriginalClass");
    }

    /// `mylib/__init__.py` has `from mylib.star_module import *`, and
    /// `star_module.py` lists `StarExportedClass` in `__all__`.
    #[test]
    fn test_reexport_via_star_import() {
        let class_info = resolve_class(&get_reexport_test_dir(), "mylib.StarExportedClass");
        assert_eq!(class_info.name, "StarExportedClass");
    }

    #[test]
    fn test_direct_module_access() {
        let class_info =
            resolve_class(&get_reexport_test_dir(), "mylib.modules.linear.DirectClass");
        assert_eq!(class_info.name, "DirectClass");
    }

    /// `_PrivateClass` starts with `_` and is not in `__all__`, so a star
    /// import does not export it.
    #[test]
    fn test_private_class_not_star_exported() {
        let result = extract_definition_info(
            &test_db(),
            "mylib._PrivateClass",
            &[get_reexport_test_dir(), PathBuf::from(".")],
        );
        assert!(result.is_err());
    }

    // ==================== classmethod and staticmethod tests ====================

    #[test]
    fn test_extract_classmethod() {
        let method = resolve_method("class_methods.ModelFactory.from_config");
        assert_eq!(
            method_shape(&method),
            ("ModelFactory", "from_config", true, false)
        );
        assert_eq!(method.signature.name, "from_config");
        assert_eq!(param_names(&method.signature), ["cls", "config"]);
    }

    #[test]
    fn test_extract_staticmethod() {
        let method = resolve_method("class_methods.ModelFactory.compute_size");
        assert_eq!(
            method_shape(&method),
            ("ModelFactory", "compute_size", false, true)
        );
        assert_eq!(method.signature.name, "compute_size");
        assert_eq!(param_names(&method.signature), ["dim1", "dim2"]);
    }

    #[test]
    fn test_extract_classmethod_with_defaults() {
        let method = resolve_method("class_methods.ModelFactory.with_defaults");
        assert_eq!(
            method_shape(&method),
            ("ModelFactory", "with_defaults", true, false)
        );
        assert_eq!(param_names(&method.signature), ["cls", "output_dim"]);
        assert!(method.signature.parameters[1].has_default());
    }

    #[test]
    fn test_extract_classmethod_with_kwargs() {
        let method = resolve_method("class_methods.DataProcessor.create");
        assert_eq!(
            method_shape(&method),
            ("DataProcessor", "create", true, false)
        );
        assert_eq!(method.signature.parameters.len(), 3);
        assert_eq!(method.signature.parameters[2].name, "kwargs");
        assert_eq!(
            method.signature.parameters[2].kind,
            ParameterKind::VarKeyword
        );
    }

    /// `NestedTwice.nested` is `NestedExample`, whose `nested_class` is
    /// `ModelFactory`.
    #[test]
    fn test_extract_nested_classmethod() {
        let method = resolve_method("class_methods.NestedTwice.nested.nested_class.from_config");
        assert_eq!(
            method_shape(&method),
            ("ModelFactory", "from_config", true, false)
        );
        assert_eq!(param_names(&method.signature), ["cls", "config"]);
    }

    #[test]
    fn test_extract_nested_staticmethod() {
        let method = resolve_method("class_methods.NestedTwice.nested.nested_class.compute_size");
        assert_eq!(
            method_shape(&method),
            ("ModelFactory", "compute_size", false, true)
        );
        assert_eq!(param_names(&method.signature), ["dim1", "dim2"]);
    }

    // ==================== vendored typeshed tests ====================
    //
    // These run against the real `builtins.pyi` shipped by `ty_vendored`
    // rather than a fixture: the constructs that motivated this handling
    // (`@overload`, `__new__`-only classes, positional-only parameters) are
    // exactly the ones a hand-written fixture would get subtly wrong.

    use crate::python::typeshed::stdlib_search_root;

    fn builtins_stub() -> PathBuf {
        stdlib_search_root().join("builtins.pyi")
    }

    #[test]
    fn test_vendored_builtins_stub_is_readable() {
        let db = test_db();
        assert!(path_is_file(&db, &builtins_stub()));
        let source = read_source(&db, &builtins_stub()).unwrap();
        assert!(source.as_str().contains("def len("));
    }

    #[test]
    fn test_builtin_len_parameter_is_positional_only() {
        let db = test_db();
        let sig =
            extract_function_signature(&db, &builtins_stub(), "len", PythonVersion::latest_ty())
                .unwrap();

        assert!(!sig.is_overloaded);
        assert_eq!(sig.parameters.len(), 1);
        assert_eq!(sig.parameters[0].name, "obj");
        assert_eq!(sig.parameters[0].kind, ParameterKind::PositionalOnly);
        assert!(sig.docstring.is_some());
        assert!(
            format_function(&sig).contains("def len(obj: Sized, /) -> int"),
            "formatted signature should carry the `/` marker"
        );
    }

    #[test]
    fn test_builtin_open_is_overloaded() {
        let db = test_db();
        let sig =
            extract_function_signature(&db, &builtins_stub(), "open", PythonVersion::latest_ty())
                .unwrap();

        assert!(
            sig.is_overloaded,
            "typeshed declares `open` eight times; validating against the first \
             declaration alone would report the wrong arity"
        );
        assert!(format_function(&sig).contains("@overload"));
    }

    #[test]
    fn test_builtin_int_uses_new_as_its_constructor() {
        let db = test_db();
        // The `__new__` fallback is applied by `extract_class_info_with_imports`,
        // once the MRO has had its chance to supply an `__init__`.
        let (class_info, _) =
            extract_class_info_with_imports(&db, &builtins_stub(), "int", &[]).unwrap();

        let init = class_info
            .init_signature
            .as_ref()
            .expect("int declares no __init__, so __new__ must stand in");
        assert_eq!(init.name, "__new__");
        assert!(init.is_overloaded);
        assert_eq!(
            DefinitionInfo::Class(class_info.clone()).implicit_param(),
            Some("cls"),
            "the implicit first parameter of __new__ is cls, not self"
        );
        assert!(format_class(&class_info).contains("def __new__("));
    }

    /// `__new__` must not shadow an `__init__` inherited from a base class:
    /// Python looks the two up independently, and it is `__init__` that
    /// receives the constructor arguments in the common
    /// `def __new__(cls, *args, **kwargs)` shape.
    #[test]
    fn test_inherited_init_wins_over_own_new() {
        let db = test_db();
        let source = r#"
class Base:
    def __init__(self, a, b):
        """Base init."""

class Child(Base):
    def __new__(cls, *args, **kwargs):
        return super().__new__(cls)
"#;
        let (dir, path) = temp_module("inherit.py", source);
        let search_paths = vec![dir.path().to_path_buf()];

        let (class_info, _) =
            extract_class_info_with_imports(&db, &path, "Child", &search_paths).unwrap();

        let init = class_info.init_signature.as_ref().unwrap();
        assert_eq!(init.name, "__init__");
        let names: Vec<_> = init.parameters.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["self", "a", "b"]);
    }

    #[test]
    fn test_builtin_dict_init_is_overloaded() {
        let db = test_db();
        let class_info =
            extract_class_info(&db, &builtins_stub(), "dict", PythonVersion::latest_ty()).unwrap();

        let init = class_info
            .init_signature
            .as_ref()
            .expect("dict declares __init__");
        assert_eq!(init.name, "__init__");
        assert!(init.is_overloaded);
    }

    #[test]
    fn test_builtin_tuple_new_is_not_flagged_overloaded() {
        let db = test_db();
        let (class_info, _) =
            extract_class_info_with_imports(&db, &builtins_stub(), "tuple", &[]).unwrap();

        let init = class_info.init_signature.as_ref().unwrap();
        assert_eq!(init.name, "__new__");
        assert!(
            !init.is_overloaded,
            "a single undecorated declaration is not an overload set"
        );
    }

    /// A stub declares only the `@overload` members — there is no
    /// implementation to fall back on, so no single signature describes the
    /// call and the symbol is reported as overloaded.
    #[test]
    fn test_overload_without_implementation_is_overloaded() {
        let db = test_db();
        let source = concat!(
            "from typing import overload\n",
            "\n",
            "@overload\n",
            "def render(value: int) -> str: ...\n",
            "@overload\n",
            "def render(value: str) -> str: ...\n",
        );
        let (_dir, path) = temp_module("stub.pyi", source);

        let render =
            extract_function_signature(&db, &path, "render", PythonVersion::latest_ty()).unwrap();
        assert!(render.is_overloaded);
    }

    /// A generic base is written with its type arguments, which no resolver can
    /// look up verbatim. Stripping them is what lets an `__init__` inherited
    /// through `class Child(Base[T])` be found at all.
    #[test]
    fn test_generic_base_class_is_resolved() {
        let db = test_db();
        let source = concat!(
            "from typing import Generic, TypeVar\n",
            "\n",
            "T = TypeVar(\"T\")\n",
            "\n",
            "class Base(Generic[T]):\n",
            "    def __init__(self, a, b):\n",
            "        pass\n",
            "\n",
            "class Child(Base[int]):\n",
            "    pass\n",
        );
        let (dir, path) = temp_module("generic.py", source);
        let search_paths = vec![dir.path().to_path_buf()];

        let (class_info, _) =
            extract_class_info_with_imports(&db, &path, "Child", &search_paths).unwrap();

        let init = class_info
            .init_signature
            .as_ref()
            .expect("the generic base's __init__ should be inherited");
        let names: Vec<_> = init.parameters.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["self", "a", "b"]);
        assert!(
            class_info.all_bases_resolved,
            "a resolvable generic base is not an unreadable MRO"
        );
    }

    /// `*args` already opens the keyword-only run, so no bare `*` is written
    /// after it — `def f(*args, *, key=1)` is not valid Python.
    #[test]
    fn test_star_marker_is_omitted_after_varargs() {
        let db = test_db();
        let source = concat!(
            "def with_varargs(a, *args, key=1, **kw):\n",
            "    pass\n",
            "\n",
            "def without_varargs(a, *, key=1):\n",
            "    pass\n",
        );
        let (_dir, path) = temp_module("markers.py", source);

        let with_varargs =
            extract_function_signature(&db, &path, "with_varargs", PythonVersion::latest_ty())
                .unwrap();
        let rendered = format_function(&with_varargs);
        assert!(
            rendered.contains("with_varargs(a, *args, key = 1, **kw)"),
            "got:\n{rendered}"
        );

        let without_varargs =
            extract_function_signature(&db, &path, "without_varargs", PythonVersion::latest_ty())
                .unwrap();
        let rendered = format_function(&without_varargs);
        assert!(
            rendered.contains("without_varargs(a, *, key = 1)"),
            "got:\n{rendered}"
        );
    }

    /// `__new__` may only stand in for a missing `__init__` when the whole MRO
    /// was readable. An unresolvable base could be hiding the real `__init__`,
    /// and validating arguments against `__new__` would then be wrong.
    #[test]
    fn test_new_fallback_is_uncertain_when_a_base_is_unresolvable() {
        let db = test_db();
        let source = r#"
from not_installed import Widget

class Thing(Widget):
    def __new__(cls, *args, **kwargs):
        return super().__new__(cls)
"#;
        let (dir, path) = temp_module("uncertain.py", source);
        let search_paths = vec![dir.path().to_path_buf()];

        let (class_info, _) =
            extract_class_info_with_imports(&db, &path, "Thing", &search_paths).unwrap();

        assert_eq!(class_info.init_signature.as_ref().unwrap().name, "__new__");
        assert!(!class_info.all_bases_resolved);
        assert!(class_info.constructor_is_uncertain());
    }

    /// The same shape with every base readable: the fallback is trustworthy and
    /// arguments are validated against it.
    #[test]
    fn test_new_fallback_is_certain_when_the_mro_resolves() {
        let db = test_db();
        let source = r#"
class Widget:
    """A widget."""

class Thing(Widget):
    def __new__(cls, size: int):
        return super().__new__(cls)
"#;
        let (dir, path) = temp_module("certain.py", source);
        let search_paths = vec![dir.path().to_path_buf()];

        let (class_info, _) =
            extract_class_info_with_imports(&db, &path, "Thing", &search_paths).unwrap();

        assert_eq!(class_info.init_signature.as_ref().unwrap().name, "__new__");
        assert!(class_info.all_bases_resolved);
        assert!(!class_info.constructor_is_uncertain());
    }

    /// A qualified base only resolves its *module*, so `pkg.Widget` lands on
    /// `pkg/__init__.py` even when the class body lives in `pkg/impl.py`. The
    /// re-export has to be followed or the inherited `__init__` is invisible.
    #[test]
    fn test_reexported_qualified_base_class_is_resolved() {
        let db = test_db();
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("__init__.py"), "from .impl import Widget\n").unwrap();
        std::fs::write(
            pkg.join("impl.py"),
            "class Widget:\n    def __init__(self, size):\n        pass\n",
        )
        .unwrap();
        let path = dir.path().join("main.py");
        std::fs::write(
            &path,
            "import pkg\n\nclass Thing(pkg.Widget):\n    def __new__(cls):\n        return super().__new__(cls)\n",
        )
        .unwrap();
        let search_paths = vec![dir.path().to_path_buf()];

        let (class_info, _) =
            extract_class_info_with_imports(&db, &path, "Thing", &search_paths).unwrap();

        let init = class_info
            .init_signature
            .as_ref()
            .expect("the re-exported base's __init__ should be inherited");
        assert_eq!(init.name, "__init__", "got {init:?}");
        let names: Vec<_> = init.parameters.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["self", "size"]);
        assert!(!class_info.constructor_is_uncertain());
    }

    /// A base whose module resolves but whose class body is never read leaves
    /// the MRO incomplete: the real `__init__` may be up there. Reporting the
    /// hierarchy as resolved would green-light validating against `__new__`.
    #[test]
    fn test_unreadable_base_class_body_leaves_the_mro_uncertain() {
        let db = test_db();
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir(&pkg).unwrap();
        std::fs::write(pkg.join("__init__.py"), "\n").unwrap();
        let path = dir.path().join("main.py");
        std::fs::write(
            &path,
            "import pkg\n\nclass Thing(pkg.Widget):\n    def __new__(cls, size):\n        return super().__new__(cls)\n",
        )
        .unwrap();
        let search_paths = vec![dir.path().to_path_buf()];

        let (class_info, _) =
            extract_class_info_with_imports(&db, &path, "Thing", &search_paths).unwrap();

        assert_eq!(class_info.init_signature.as_ref().unwrap().name, "__new__");
        assert!(!class_info.all_bases_resolved);
        assert!(class_info.constructor_is_uncertain());
    }

    /// Two declarations of one name are not an overload set. A property and its
    /// setter are the common shape, and treating them as overloaded would
    /// silently switch off argument validation for the target.
    #[test]
    fn test_property_setter_pair_is_not_an_overload() {
        let db = test_db();
        let source = r#"
class Widget:
    @property
    def value(self) -> int:
        """The value."""
        return self._value

    @value.setter
    def value(self, new_value: int) -> None:
        self._value = new_value
"#;
        let (_dir, path) = temp_module("prop.py", source);

        let method =
            extract_method_info(&db, &path, "Widget", "value", PythonVersion::latest_ty()).unwrap();
        assert!(!method.signature.is_overloaded);
        assert!(!format_method(&method).contains("@overload"));
    }

    #[test]
    fn test_module_defines_top_level_ignores_methods() {
        let db = test_db();
        assert!(module_defines_top_level(
            &db,
            &builtins_stub(),
            "len",
            PythonVersion::latest_ty()
        ));
        assert!(module_defines_top_level(
            &db,
            &builtins_stub(),
            "dict",
            PythonVersion::latest_ty()
        ));
        assert!(
            !module_defines_top_level(&db, &builtins_stub(), "count", PythonVersion::latest_ty()),
            "`count` is a method of `list`, not a builtin"
        );
        assert!(
            module_defines_top_level(&db, &builtins_stub(), "aiter", PythonVersion::latest_ty()),
            "typeshed guards `aiter` behind `if sys.version_info >= (3, 10):`"
        );
    }

    #[test]
    fn test_overload_detection_on_first_party_source() {
        let db = test_db();
        let source = r#"
from typing import overload

@overload
def render(value: int) -> str: ...
@overload
def render(value: str) -> str: ...
def render(value):
    """Render a value."""
    return str(value)

def plain(value, /, other, *, flag=False):
    """Not overloaded."""
"#;
        let (_dir, path) = temp_module("mod.py", source);

        // A `.py` overload set ends with the implementation, whose signature is
        // the one Hydra will call — so there is something definite to validate
        // against and the symbol is not reported as overloaded.
        let render =
            extract_function_signature(&db, &path, "render", PythonVersion::latest_ty()).unwrap();
        assert!(!render.is_overloaded);
        assert_eq!(render.docstring.as_deref(), Some("Render a value."));
        assert!(render.parameters[0].type_annotation.is_none());

        let plain =
            extract_function_signature(&db, &path, "plain", PythonVersion::latest_ty()).unwrap();
        assert!(!plain.is_overloaded);
        let kinds: Vec<_> = plain
            .parameters
            .iter()
            .map(|p| (p.name.as_str(), p.kind))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("value", ParameterKind::PositionalOnly),
                ("other", ParameterKind::PositionalOrKeyword),
                ("flag", ParameterKind::KeywordOnly)
            ]
        );
        assert!(
            format_function(&plain).contains("plain(value, /, other, *, flag = False)"),
            "both the positional-only `/` and the keyword-only `*` must be shown"
        );
    }

    /// Each hop of an attribute chain starts a fresh import walk. Here the
    /// second hop passes through `pkg/b.py` and `pkg/__init__.py` again, which
    /// the first hop already visited.
    #[test]
    fn test_attribute_chain_revisits_files_of_an_earlier_hop() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("__init__.py"),
            "from .b import Mid\nfrom .c import Inner\n",
        )
        .unwrap();
        std::fs::write(
            pkg.join("a.py"),
            "from pkg import Mid\n\nclass Outer:\n    mid = Mid\n",
        )
        .unwrap();
        std::fs::write(
            pkg.join("b.py"),
            "from pkg import Inner\n\nclass Mid:\n    inner = Inner\n",
        )
        .unwrap();
        std::fs::write(
            pkg.join("c.py"),
            "class Inner:\n    \"\"\"Inner doc.\"\"\"\n",
        )
        .unwrap();

        let resolved = extract_definition_info(
            &test_db(),
            "pkg.a.Outer.mid.inner",
            &[dir.path().to_path_buf()],
        )
        .expect("the chain should resolve");
        match resolved.definition_info {
            DefinitionInfo::Class(class_info) => assert_eq!(class_info.name, "Inner"),
            other => panic!("expected a class, got {other:?}"),
        }
        assert_eq!(resolved.file_path, pkg.join("c.py"));
    }
}
