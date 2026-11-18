//! Resolving a `_target_` string to its definition: following re-exports,
//! class attribute chains and base classes across files.

use super::cache::{
    ResolvedDefinition, TargetString, class_key, class_parent_attribute, class_parent_docs,
    resolve_module_cached,
};
use super::extract::{extract_class_info, extract_function_signature, extract_method_info};
use super::import_resolver::ImportResolver;
use super::model::{ClassInfo, DefinitionInfo, FunctionSignature, MethodInfo, ResolveError};
use anyhow::Result;
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

/// Split a `_target_` string into module path and symbol name
/// Example: "myproject.models.MyClass" -> ("myproject.models", "MyClass")
fn split_target(target: &str) -> Result<(String, String)> {
    let parts: Vec<&str> = target.split('.').collect();
    if parts.len() < 2 {
        anyhow::bail!("Invalid _target_ format: '{}'", target);
    }

    let symbol_name = parts.last().unwrap().to_string();
    let module_path = parts[..parts.len() - 1].join(".");

    Ok((module_path, symbol_name))
}

/// Resolve an attribute chain through classes.
///
/// Given a starting class and an attribute chain like ["nested", "nested_class"],
/// follows the chain by looking up each class attribute to find the final class/method.
///
/// Returns the final resolved class name and the remaining method name (if any).
fn resolve_attribute_chain<'db>(
    db: &'db dyn ty_module_resolver::Db,
    file_path: &Path,
    starting_class: &str,
    attribute_chain: &[&str],
    environment: ResolverEnvironment<'db>,
) -> Result<(PathBuf, String, Option<String>)> {
    if attribute_chain.is_empty() {
        anyhow::bail!("Empty attribute chain");
    }

    let mut current_file = file_path.to_path_buf();
    let mut current_class = starting_class.to_string();
    let mut resolver = ImportResolver::new(db, environment);
    let python_version = environment.python_version(db);

    // Process all but the last attribute (which might be a method)
    for (i, attr) in attribute_chain.iter().enumerate() {
        let is_last = i == attribute_chain.len() - 1;

        // First, check if this is a method on the current class
        if is_last
            && extract_method_info(db, &current_file, &current_class, attr, python_version).is_ok()
        {
            return Ok((current_file, current_class, Some(attr.to_string())));
        }

        // Try to get the attribute as a class attribute (with inheritance support).
        // The salsa-tracked class_parent_attribute memoises each (class, attr) pair so
        // shared parent classes are only walked once per revision.
        let class_key = class_key(db, &current_file, &current_class);
        let attr_key = TargetString::new(db, attr.to_string());
        let cached_attr = class_parent_attribute(db, class_key, attr_key, environment);
        match cached_attr.get() {
            Some((attr_info, _attr_file, _attr_class)) => {
                // The value could be a simple name or a dotted path
                let value_parts: Vec<&str> = attr_info.value.split('.').collect();

                if value_parts.len() == 1 {
                    // Simple name - look for it in the same file first
                    let new_class_name = value_parts[0];

                    // Try direct lookup in current file
                    if extract_class_info(db, &current_file, new_class_name, python_version).is_ok()
                    {
                        current_class = new_class_name.to_string();
                        continue;
                    }

                    // Try resolving through imports
                    if let Some((resolved_file, resolved_name)) =
                        resolver.resolve_symbol(&current_file, new_class_name)
                    {
                        current_file = resolved_file;
                        current_class = resolved_name;
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
            None if is_last => {
                anyhow::bail!(
                    "Attribute or method '{}' not found in class '{}'",
                    attr,
                    current_class
                );
            }
            None => {
                anyhow::bail!(
                    "Attribute '{}' not found in class '{}' or its parents",
                    attr,
                    current_class
                );
            }
        }
    }

    // Reached end of chain without finding a method
    Ok((current_file, current_class, None))
}

/// Resolve a base class expression to its file path and actual class name.
///
/// This function handles:
/// - Simple names: `ParentClass` - looks up in the current file's imports
/// - Qualified names: `module.ClassName` - resolves the module path
pub(super) fn resolve_base_class<'db>(
    db: &'db dyn ty_module_resolver::Db,
    base_class_expr: &str,
    current_file: &Path,
    environment: ResolverEnvironment<'db>,
) -> Option<(PathBuf, String)> {
    let base_class_expr = base_class_name(base_class_expr);

    // Check if it's a qualified name (contains a dot)
    if let Some(dot_pos) = base_class_expr.rfind('.') {
        // Qualified name like `module.ClassName`
        let module_path = &base_class_expr[..dot_pos];
        let class_name = &base_class_expr[dot_pos + 1..];

        // Cached lookup: base-class resolution for the same parent module is
        // shared across the MRO walk and across sibling hover requests.
        let module_path_id = TargetString::new(db, module_path.to_string());
        if let Some(file_path) = resolve_module_cached(db, module_path_id, environment).clone() {
            return Some((file_path, class_name.to_string()));
        }
    }

    // Simple name - try to resolve through imports in the current file
    let mut resolver = ImportResolver::new(db, environment);
    if let Some(resolved) = resolver.resolve_symbol(current_file, base_class_expr) {
        return Some(resolved);
    }

    // Check if the class is defined in the same file
    if extract_class_info(
        db,
        current_file,
        base_class_expr,
        environment.python_version(db),
    )
    .is_ok()
    {
        return Some((current_file.to_path_buf(), base_class_expr.to_string()));
    }

    None
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
) -> Result<(ClassInfo, PathBuf)> {
    let python_version = environment.python_version(db);
    let (mut class_info, resolved_file) = with_reexports(
        db,
        file_path,
        class_name,
        environment,
        |file, name| extract_class_info(db, file, name, python_version),
        || {
            anyhow::anyhow!(
                "Class '{}' not found in {} (also checked re-exports)",
                class_name,
                file_path.display()
            )
        },
    )?;

    // Resolve missing properties from parent classes via the memoised salsa query.
    // class_parent_docs walks the MRO recursively and caches results per (class, environment),
    // so shared parent classes across different child-class lookups are resolved only once.
    if class_info.docstring.is_none() || class_info.init_signature.is_none() {
        let class_key = class_key(db, &resolved_file, &class_info.name);
        let parent_docs = class_parent_docs(db, class_key, environment);

        if class_info.docstring.is_none() {
            class_info.docstring = parent_docs.docstring().cloned();
        }
        if class_info.init_signature.is_none() {
            class_info.init_signature = parent_docs.init().cloned();
        }
        if class_info.new_signature.is_none() {
            class_info.new_signature = parent_docs.new_signature().cloned();
        }
        class_info.unresolved_base_classes = !parent_docs.all_bases_resolved();
    }

    if class_info.init_signature.is_none() {
        class_info.init_signature = class_info.new_signature.take();
    }

    Ok((class_info, resolved_file))
}

/// Extract function signature, following re-exports if necessary.
/// Returns the FunctionSignature and the file path where it was found.
fn extract_function_signature_with_imports<'db>(
    db: &'db dyn ty_module_resolver::Db,
    file_path: &Path,
    function_name: &str,
    environment: ResolverEnvironment<'db>,
) -> Result<(FunctionSignature, PathBuf)> {
    let python_version = environment.python_version(db);
    with_reexports(
        db,
        file_path,
        function_name,
        environment,
        |file, name| extract_function_signature(db, file, name, python_version),
        || {
            anyhow::anyhow!(
                "Function '{}' not found in {} (also checked re-exports)",
                function_name,
                file_path.display()
            )
        },
    )
}

/// Run `extract` for `name` in `file_path`, and if that fails, again where
/// the file's imports say `name` really lives. Returns the result and the
/// file it came from.
///
/// The in-file error is dropped. When the import chain resolves, the
/// retry's error is returned as is; when it does not, `not_found` is.
fn with_reexports<'db, T>(
    db: &'db dyn ty_module_resolver::Db,
    file_path: &Path,
    name: &str,
    environment: ResolverEnvironment<'db>,
    extract: impl Fn(&Path, &str) -> Result<T>,
    not_found: impl FnOnce() -> anyhow::Error,
) -> Result<(T, PathBuf)> {
    if let Ok(found) = extract(file_path, name) {
        return Ok((found, file_path.to_path_buf()));
    }

    let mut resolver = ImportResolver::new(db, environment);
    let Some((resolved_file, resolved_name)) = resolver.resolve_symbol(file_path, name) else {
        return Err(not_found());
    };
    let found = extract(&resolved_file, &resolved_name)?;
    Ok((found, resolved_file))
}

/// Extract definition info (function, class or method) from a target
/// string, along with the file it was found in and the module path and
/// symbol name the target resolved to.
pub fn extract_definition_info<'db>(
    db: &'db dyn ty_module_resolver::Db,
    target: &str,
    environment: ResolverEnvironment<'db>,
) -> std::result::Result<ResolvedDefinition, ResolveError> {
    let Ok((module_path, symbol_name)) = split_target(target) else {
        return Err(ResolveError::InvalidFormat(target.to_string()));
    };

    // Track whether we found the module but not the symbol
    let mut module_found = false;

    // First try standard resolution: module.symbol where symbol is a function or class
    let module_path_id = TargetString::new(db, module_path.clone());
    if let Some(file_path) = resolve_module_cached(db, module_path_id, environment).clone() {
        module_found = true;

        // Try to extract as function first (with import resolution)
        if let Ok((func_sig, resolved_file)) =
            extract_function_signature_with_imports(db, &file_path, &symbol_name, environment)
        {
            return Ok(ResolvedDefinition {
                definition_info: DefinitionInfo::Function(func_sig),
                file_path: resolved_file,
                module_path,
                symbol_name,
            });
        }

        // Try to extract as class (with import resolution)
        if let Ok((class_info, resolved_file)) =
            extract_class_info_with_imports(db, &file_path, &symbol_name, environment)
        {
            return Ok(ResolvedDefinition {
                definition_info: DefinitionInfo::Class(class_info),
                file_path: resolved_file,
                module_path,
                symbol_name,
            });
        }
    }

    // Try to interpret as Class.method pattern (e.g., "my_module.MyClass.from_config")
    // or nested pattern (e.g., "my_module.OuterClass.nested.nested_class.method")
    // or nested class pattern (e.g., "my_module.OuterClass.nested.nested_class")
    if let Some(result) = resolve_class_attribute_chain(db, target, environment) {
        match result {
            ClassAttributeChainResult::Method {
                file_path: resolved_file,
                class_name,
                method_name,
            } => {
                if let Ok((method_info, final_file)) = extract_method_info_with_imports(
                    db,
                    &resolved_file,
                    &class_name,
                    &method_name,
                    environment,
                ) {
                    return Ok(ResolvedDefinition {
                        definition_info: DefinitionInfo::Method(method_info),
                        file_path: final_file,
                        module_path: module_path.clone(),
                        symbol_name: format!("{}.{}", class_name, method_name),
                    });
                }
            }
            ClassAttributeChainResult::Class {
                file_path: resolved_file,
                class_name,
            } => {
                if let Ok((class_info, final_file)) =
                    extract_class_info_with_imports(db, &resolved_file, &class_name, environment)
                {
                    return Ok(ResolvedDefinition {
                        definition_info: DefinitionInfo::Class(class_info),
                        file_path: final_file,
                        module_path: module_path.clone(),
                        symbol_name: class_name,
                    });
                }
            }
        }
    }

    // Return appropriate error based on whether module was found
    if module_found {
        Err(ResolveError::Other(format!(
            "Symbol '{}' not found in module '{}'",
            symbol_name, module_path
        )))
    } else {
        Err(ResolveError::UnresolvedModule(module_path))
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
        let module_path_id = TargetString::new(db, module_path);
        let Some(file_path) = resolve_module_cached(db, module_path_id, environment).clone() else {
            continue;
        };

        // The first remaining part should be a class (or function that returns a class, but we focus on classes)
        let first_symbol = remaining_parts[0];

        // Check if this is a class
        if let Ok((class_info, resolved_file)) =
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
                .is_ok()
                {
                    return Some(ClassAttributeChainResult::Method {
                        file_path: resolved_file,
                        class_name: class_info.name,
                        method_name: method_name.to_string(),
                    });
                }
            } else if remaining_parts.len() > 2 {
                // Nested case: Class.attr1.attr2...method_or_class
                // Follow the attribute chain
                let attribute_chain = &remaining_parts[1..];
                if let Ok((final_file, final_class, method_name_opt)) = resolve_attribute_chain(
                    db,
                    &resolved_file,
                    &class_info.name,
                    attribute_chain,
                    environment,
                ) {
                    if let Some(method_name) = method_name_opt {
                        return Some(ClassAttributeChainResult::Method {
                            file_path: final_file,
                            class_name: final_class,
                            method_name,
                        });
                    } else {
                        return Some(ClassAttributeChainResult::Class {
                            file_path: final_file,
                            class_name: final_class,
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
fn extract_method_info_with_imports<'db>(
    db: &'db dyn ty_module_resolver::Db,
    file_path: &Path,
    class_name: &str,
    method_name: &str,
    environment: ResolverEnvironment<'db>,
) -> Result<(MethodInfo, PathBuf)> {
    let python_version = environment.python_version(db);
    // The class is what gets re-exported, so it is the name to follow.
    with_reexports(
        db,
        file_path,
        class_name,
        environment,
        |file, class| extract_method_info(db, file, class, method_name, python_version),
        || {
            anyhow::anyhow!(
                "Method '{}' not found in class '{}' in {} (also checked re-exports)",
                method_name,
                class_name,
                file_path.display()
            )
        },
    )
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
    use crate::python::cache::environment_for_roots;
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
    ) -> Result<(ClassInfo, PathBuf)> {
        super::extract_class_info_with_imports(
            db,
            file_path,
            class_name,
            environment_for_roots(db, roots),
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
            ResolveError::Other(
                "Symbol 'NoSuchSymbolXyz' not found in module 'my_module'".to_string()
            )
        );
    }

    // ==================== split_target tests ====================

    #[test]
    fn test_split_target() {
        let (module, symbol) = split_target("myproject.models.MyClass").unwrap();
        assert_eq!(module, "myproject.models");
        assert_eq!(symbol, "MyClass");
    }

    #[test]
    fn test_split_target_short() {
        let (module, symbol) = split_target("module.Class").unwrap();
        assert_eq!(module, "module");
        assert_eq!(symbol, "Class");
    }

    #[test]
    fn test_split_target_invalid() {
        assert!(split_target("InvalidTarget").is_err());
    }

    #[test]
    fn test_split_target_deeply_nested() {
        let (module, symbol) = split_target("a.b.c.d.e.FinalClass").unwrap();
        assert_eq!(module, "a.b.c.d.e");
        assert_eq!(symbol, "FinalClass");
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

    #[test]
    fn test_extract_definition_child_without_init() {
        let examples_dir = get_simple_test_dir();

        let result = extract_definition_info(
            &test_db(),
            "my_module.ChildWithoutInit",
            &[examples_dir.clone(), PathBuf::from(".")],
        );
        assert!(result.is_ok());
        let definition_info = result.unwrap().definition_info;

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

        let result = extract_definition_info(
            &test_db(),
            "class_methods.InheritedNested.nested.nested_class.from_config",
            &[examples_dir.clone(), PathBuf::from(".")],
        );
        assert!(
            result.is_ok(),
            "Should resolve classmethod: {:?}",
            result.err()
        );
        let definition_info = result.unwrap().definition_info;

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

        let result = extract_definition_info(
            &test_db(),
            "class_methods.InheritedNested.nested.nested_class.compute_size",
            &[workspace_dir.clone(), PathBuf::from(".")],
        );

        assert!(
            result.is_ok(),
            "Should resolve staticmethod: {:?}",
            result.err()
        );
        let definition_info = result.unwrap().definition_info;

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

        let result = extract_definition_info(
            &test_db(),
            "class_methods.NestedTwice.nested.inherited_nested_class",
            &[examples_dir.clone(), PathBuf::from(".")],
        );
        assert!(
            result.is_ok(),
            "Should resolve classmethod: {:?}",
            result.err()
        );
        let definition_info = result.unwrap().definition_info;

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

        let result = extract_definition_info(
            &test_db(),
            "my_module.simple_function",
            &[examples_dir.clone(), PathBuf::from(".")],
        );
        assert!(result.is_ok());
        let definition_info = result.unwrap().definition_info;

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

        let result = extract_definition_info(
            &test_db(),
            "my_module.SimpleClass",
            &[examples_dir.clone(), PathBuf::from(".")],
        );
        assert!(result.is_ok());
        let definition_info = result.unwrap().definition_info;

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

        let result = extract_definition_info(
            &test_db(),
            "test_package.submodule.SubmoduleClass",
            &[examples_dir.clone(), PathBuf::from(".")],
        );
        assert!(result.is_ok());
        let definition_info = result.unwrap().definition_info;

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

        let result = extract_definition_info(
            &test_db(),
            "my_module.NonexistentSymbol",
            &[examples_dir.clone(), PathBuf::from(".")],
        );
        assert!(result.is_err());
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

        let result = extract_definition_info(
            &test_db(),
            "mylib.Linear",
            &[workspace_dir.clone(), PathBuf::from(".")],
        );

        assert!(
            result.is_ok(),
            "Should resolve Linear through re-export chain: {:?}",
            result.err()
        );
        let definition_info = result.unwrap().definition_info;

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

        let result = extract_definition_info(
            &test_db(),
            "mylib.AliasedClass",
            &[workspace_dir.clone(), PathBuf::from(".")],
        );

        assert!(
            result.is_ok(),
            "Should resolve AliasedClass through aliased re-export: {:?}",
            result.err()
        );
        let definition_info = result.unwrap().definition_info;

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

        let result = extract_definition_info(
            &test_db(),
            "mylib.StarExportedClass",
            &[workspace_dir.clone(), PathBuf::from(".")],
        );

        assert!(
            result.is_ok(),
            "Should resolve StarExportedClass through star import: {:?}",
            result.err()
        );
        let definition_info = result.unwrap().definition_info;

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

        let result = extract_definition_info(
            &test_db(),
            "mylib.modules.linear.DirectClass",
            &[workspace_dir.clone(), PathBuf::from(".")],
        );

        assert!(
            result.is_ok(),
            "Should resolve DirectClass directly: {:?}",
            result.err()
        );
        let definition_info = result.unwrap().definition_info;

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

        let result = extract_definition_info(
            &test_db(),
            "mylib._PrivateClass",
            &[workspace_dir.clone(), PathBuf::from(".")],
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

        let result = extract_definition_info(
            &test_db(),
            "class_methods.ModelFactory.from_config",
            &[workspace_dir.clone(), PathBuf::from(".")],
        );

        assert!(
            result.is_ok(),
            "Should resolve classmethod: {:?}",
            result.err()
        );
        let definition_info = result.unwrap().definition_info;

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

        let result = extract_definition_info(
            &test_db(),
            "class_methods.ModelFactory.compute_size",
            &[workspace_dir.clone(), PathBuf::from(".")],
        );

        assert!(
            result.is_ok(),
            "Should resolve staticmethod: {:?}",
            result.err()
        );
        let definition_info = result.unwrap().definition_info;

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

        let result = extract_definition_info(
            &test_db(),
            "class_methods.ModelFactory.with_defaults",
            &[workspace_dir.clone(), PathBuf::from(".")],
        );

        assert!(
            result.is_ok(),
            "Should resolve classmethod: {:?}",
            result.err()
        );
        let definition_info = result.unwrap().definition_info;

        match definition_info {
            DefinitionInfo::Method(method_info) => {
                assert_eq!(method_info.class_name, "ModelFactory");
                assert_eq!(method_info.method_name, "with_defaults");
                assert!(method_info.is_classmethod);
                // Check parameters: cls and output_dim (with default)
                assert_eq!(method_info.signature.parameters.len(), 2);
                assert_eq!(method_info.signature.parameters[1].name, "output_dim");
                assert!(method_info.signature.parameters[1].has_default());
            }
            _ => panic!("Expected Method definition, got {:?}", definition_info),
        }
    }

    #[test]
    fn test_extract_classmethod_with_kwargs() {
        // Test: class_methods.DataProcessor.create has **kwargs
        let workspace_dir = get_simple_test_dir();

        let result = extract_definition_info(
            &test_db(),
            "class_methods.DataProcessor.create",
            &[workspace_dir.clone(), PathBuf::from(".")],
        );

        assert!(
            result.is_ok(),
            "Should resolve classmethod: {:?}",
            result.err()
        );
        let definition_info = result.unwrap().definition_info;

        match definition_info {
            DefinitionInfo::Method(method_info) => {
                assert_eq!(method_info.class_name, "DataProcessor");
                assert_eq!(method_info.method_name, "create");
                assert!(method_info.is_classmethod);
                // Check parameters: cls, name, **kwargs
                assert_eq!(method_info.signature.parameters.len(), 3);
                assert_eq!(method_info.signature.parameters[2].name, "kwargs");
                assert_eq!(
                    method_info.signature.parameters[2].kind,
                    ParameterKind::VarKeyword
                );
            }
            _ => panic!("Expected Method definition, got {:?}", definition_info),
        }
    }

    #[test]
    fn test_extract_nested_classmethod() {
        // Test: Nested class properties should resolve to a classmethod
        // Path: NestedTwice.nested -> NestedExample, NestedExample.nested_class -> ModelFactory
        // ModelFactory.from_config is a classmethod
        let workspace_dir = get_simple_test_dir();

        let result = extract_definition_info(
            &test_db(),
            "class_methods.NestedTwice.nested.nested_class.from_config",
            &[workspace_dir.clone(), PathBuf::from(".")],
        );

        assert!(
            result.is_ok(),
            "Should resolve classmethod: {:?}",
            result.err()
        );
        let definition_info = result.unwrap().definition_info;

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

        let result = extract_definition_info(
            &test_db(),
            "class_methods.NestedTwice.nested.nested_class.compute_size",
            &[workspace_dir.clone(), PathBuf::from(".")],
        );

        assert!(
            result.is_ok(),
            "Should resolve staticmethod: {:?}",
            result.err()
        );
        let definition_info = result.unwrap().definition_info;

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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inherit.py");
        std::fs::write(&path, source).unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stub.pyi");
        std::fs::write(&path, source).unwrap();

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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generic.py");
        std::fs::write(&path, source).unwrap();
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
            !class_info.unresolved_base_classes,
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("markers.py");
        std::fs::write(&path, source).unwrap();

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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uncertain.py");
        std::fs::write(&path, source).unwrap();
        let search_paths = vec![dir.path().to_path_buf()];

        let (class_info, _) =
            extract_class_info_with_imports(&db, &path, "Thing", &search_paths).unwrap();

        assert_eq!(class_info.init_signature.as_ref().unwrap().name, "__new__");
        assert!(class_info.unresolved_base_classes);
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("certain.py");
        std::fs::write(&path, source).unwrap();
        let search_paths = vec![dir.path().to_path_buf()];

        let (class_info, _) =
            extract_class_info_with_imports(&db, &path, "Thing", &search_paths).unwrap();

        assert_eq!(class_info.init_signature.as_ref().unwrap().name, "__new__");
        assert!(!class_info.unresolved_base_classes);
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
        assert!(class_info.unresolved_base_classes);
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prop.py");
        std::fs::write(&path, source).unwrap();

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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mod.py");
        std::fs::write(&path, source).unwrap();

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
}
