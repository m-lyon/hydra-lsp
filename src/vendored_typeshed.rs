//! Access to the typeshed stubs vendored by `ty_vendored`.
//!
//! Hydra can instantiate Python builtins, but only through the `builtins.`
//! prefix — `{"_target_": "len"}` raises `InstantiationException` while
//! `{"_target_": "builtins.len"}` works. `builtins` is a C module with no
//! Python source on disk, so the only place a signature or docstring for `len`
//! can come from is a stub file. `ty_vendored` already ships typeshed as a
//! zipped [`VendoredFileSystem`], which `HydraDatabase` hands to `ruff_db`.
//!
//! # Sentinel paths
//!
//! Module resolution throughout the crate speaks `std::path::Path`, and the
//! vendored stubs are not on disk. Rather than thread a `FilePath` enum through
//! every resolver signature, vendored files are addressed by a sentinel path
//! rooted at [`VENDORED_ROOT`]: `<typeshed>/stdlib/builtins.pyi`. Such a path
//! is built and probed exactly like a real one — `ImportResolver`'s
//! `find_module_file` walks the same `.pyi`-before-`.py` candidates against
//! [`stdlib_search_root`] — and only the two functions that actually touch a
//! file, `path_is_file` and `path_to_file` in `python_analyzer`, branch on
//! [`is_vendored_path`] to reach the `VendoredFileSystem` instead of the OS.
//!
//! The root is *not* a member of the search-path list built by
//! `build_search_paths`. `resolve_module_cached` consults it as a last resort,
//! after every real root has missed, and only for a module [`is_vendored_module`]
//! admits — so a workspace or site-packages `builtins` still shadows the stub,
//! and the sentinel never leaks into consumers of the search paths such as the
//! file watcher.
//!
//! `<typeshed>` is a relative single-component path containing characters that
//! are illegal in Windows filenames, so it cannot collide with a real search
//! root. Nothing on disk corresponds to it, so `Backend::goto_definition`
//! checks [`is_vendored_path`] and returns no location rather than handing the
//! editor a URI it cannot open.

use ruff_db::vendored::{VendoredPath, VendoredPathBuf};
use std::path::{Component, Path, PathBuf};

/// First component of every sentinel path that addresses a vendored stub.
pub const VENDORED_ROOT: &str = "<typeshed>";

/// The `builtins` module name. Hydra requires this prefix on a builtin target.
pub const BUILTINS_MODULE: &str = "builtins";

/// The root that vendored stdlib stubs are resolved against.
///
/// Typeshed's stdlib stubs live under `stdlib/` inside the archive, so the root
/// is `<typeshed>/stdlib` and `builtins` resolves to
/// `<typeshed>/stdlib/builtins.pyi`. `resolve_module_cached` probes it after
/// every real search root has missed.
pub fn stdlib_search_root() -> PathBuf {
    Path::new(VENDORED_ROOT).join("stdlib")
}

/// Whether `path` addresses a file inside the vendored typeshed archive.
pub fn is_vendored_path(path: &Path) -> bool {
    path.components().next() == Some(Component::Normal(VENDORED_ROOT.as_ref()))
}

/// Convert a sentinel path to the archive-relative path the
/// [`VendoredFileSystem`](ruff_db::vendored::VendoredFileSystem) understands.
///
/// Returns `None` when `path` is not a sentinel path or is not valid UTF-8.
pub fn to_vendored_path(path: &Path) -> Option<VendoredPathBuf> {
    let relative = path.strip_prefix(VENDORED_ROOT).ok()?;
    // The archive always uses `/` as its separator, regardless of host OS.
    let mut vendored = String::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return None;
        };
        if !vendored.is_empty() {
            vendored.push('/');
        }
        vendored.push_str(part.to_str()?);
    }
    if vendored.is_empty() {
        return None;
    }
    Some(VendoredPath::new(&vendored).to_path_buf())
}

/// Whether module resolution may fall through to the vendored search root for
/// `module_path`.
///
/// Wiring in typeshed makes the whole stdlib reachable, but issue #34 is scoped
/// to builtins: enabling every stdlib module at once widens the blast radius of
/// stub-shaped constructs (`@overload`, `__new__`, positional-only parameters,
/// typeshed's `VERSIONS` gating) well beyond what its acceptance criteria
/// cover. Resolution is therefore gated to `builtins` and its submodules, and
/// broadening the gate is a one-line change once the rest of the stdlib has
/// been evaluated on its own.
pub fn is_vendored_module(module_path: &str) -> bool {
    module_path == BUILTINS_MODULE
        || module_path
            .strip_prefix(BUILTINS_MODULE)
            .is_some_and(|rest| rest.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdlib_root_is_a_sentinel_path() {
        let root = stdlib_search_root();
        assert!(is_vendored_path(&root));
        assert!(!root.is_absolute());
    }

    #[test]
    fn builtins_stub_maps_into_the_archive() {
        let stub = stdlib_search_root().join("builtins.pyi");
        assert_eq!(
            to_vendored_path(&stub).unwrap().as_str(),
            "stdlib/builtins.pyi"
        );
    }

    #[test]
    fn non_sentinel_paths_are_not_vendored() {
        assert!(!is_vendored_path(Path::new("/usr/lib/python3.12/os.py")));
        assert!(!is_vendored_path(Path::new("src/typeshed/os.pyi")));
        assert!(to_vendored_path(Path::new("/tmp/builtins.pyi")).is_none());
    }

    #[test]
    fn traversal_components_are_rejected() {
        assert!(to_vendored_path(Path::new("<typeshed>/../../etc/passwd")).is_none());
        assert!(to_vendored_path(Path::new(VENDORED_ROOT)).is_none());
    }

    #[test]
    fn only_builtins_resolves_in_the_vendored_root() {
        assert!(is_vendored_module("builtins"));
        assert!(is_vendored_module("builtins.submodule"));
        assert!(!is_vendored_module("datetime"));
        assert!(!is_vendored_module("os.path"));
        assert!(!is_vendored_module("builtinsish"));
    }
}
