//! Access to the typeshed stubs vendored by `ty_vendored`.
//!
//! Hydra can instantiate Python builtins, but only through the `builtins.`
//! prefix — `{"_target_": "len"}` raises `InstantiationException` while
//! `{"_target_": "builtins.len"}` works. `builtins` is a C module with no
//! Python source on disk, so the only place a signature or docstring for `len`
//! can come from is a stub file. `ty_vendored` already ships typeshed as a
//! zipped [`VendoredFileSystem`], which `HydraDatabase` hands to `ruff_db`.
//!
//! Module resolution goes through `ty_module_resolver`, which hands back the
//! vendored stub as a `File` whose path is a `FilePath::Vendored`, such as
//! `stdlib/builtins.pyi`. Reads and parses go through `ruff_db`, which reaches
//! the archive for those files instead of the OS.
//!
//! The resolver puts the stdlib stubs after the first-party roots and before
//! site-packages, as the typing spec orders them. `resolved_module_file` keeps
//! a stub-backed module only when [`is_builtins_module`] is true for it, so the
//! rest of the stdlib stays unresolved.
//!
//! Nothing on disk corresponds to a vendored file, so `goto_definition` returns
//! no location for one rather than handing the editor a URI it cannot open.

/// The `builtins` module name. Hydra requires this prefix on a builtin target.
pub(crate) const BUILTINS_MODULE: &str = "builtins";

/// Whether `name` is one the runtime module actually exposes.
///
/// The stub declares a fair amount that no `import builtins` can reach: the
/// typevars and protocol classes its own annotations are written in (`_T`,
/// `_KT`, `_SupportsRound1`, `_Opener`) and a placeholder for a type Python has
/// but does not name (`function`). Resolving those would let
/// `_target_: builtins._SupportsRound1` through with no diagnostic at all, and
/// Hydra would then fail with `AttributeError` at run time.
pub(super) fn is_runtime_builtin_name(name: &str) -> bool {
    if !name.starts_with('_') {
        return true;
    }
    name.starts_with("__") && name.ends_with("__") && name.len() > 4
}

/// Whether `module_path` is `builtins` or one of its submodules.
///
/// The resolver makes the whole stdlib reachable, but only builtins is relevant
/// for Hydra, so these are the only stdlib modules `resolved_module_file` keeps.
pub(super) fn is_builtins_module(module_path: &str) -> bool {
    module_path == BUILTINS_MODULE
        || module_path
            .strip_prefix(BUILTINS_MODULE)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Whether `module_path` is a PyPI package that the vendored stdlib stubs. ty doesn't
/// let an installed copy shadow these, but Hydra imports the installed copy, and fails
/// to import it when it is missing.
pub(super) fn is_stubbed_pypi_package(module_path: &str) -> bool {
    module_path == "typing_extensions"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_internal_names_are_not_runtime_builtins() {
        assert!(is_runtime_builtin_name("len"));
        assert!(is_runtime_builtin_name("dict"));
        assert!(
            is_runtime_builtin_name("__import__"),
            "dunders are real builtins"
        );
        assert!(is_runtime_builtin_name("__build_class__"));

        assert!(!is_runtime_builtin_name("_T"));
        assert!(!is_runtime_builtin_name("_SupportsRound1"));
        assert!(!is_runtime_builtin_name("_Opener"));
        assert!(!is_runtime_builtin_name("__"), "not a dunder name");
    }

    #[test]
    fn only_builtins_resolves_in_the_vendored_root() {
        assert!(is_builtins_module("builtins"));
        assert!(is_builtins_module("builtins.submodule"));
        assert!(!is_builtins_module("datetime"));
        assert!(!is_builtins_module("os.path"));
        assert!(!is_builtins_module("builtinsish"));
    }
}
