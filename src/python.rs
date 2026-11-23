pub(crate) mod cache;
pub(crate) mod call_check;
mod env;
mod extract;
pub(crate) mod format;
mod import_resolver;
pub(crate) mod model;
mod resolve;
mod stub_mapping;
pub(crate) mod typeshed;
mod value_type;

/// Helpers shared by the unit tests of the modules above.
#[cfg(test)]
pub(crate) mod test_support {
    use super::model::{FunctionSignature, ParameterInfo, ParameterKind, SourceRange};
    use crate::database::HydraDatabase;
    use ruff_db::files::{File, system_path_to_file};
    use ruff_db::system::SystemPathBuf;
    use std::path::{Path, PathBuf};

    /// The `tests/workspace/<name>` fixture directory.
    pub(crate) fn fixture_dir(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("workspace")
            .join(name)
    }

    /// The `tests/workspace/simple` fixture directory.
    pub(crate) fn get_simple_test_dir() -> PathBuf {
        fixture_dir("simple")
    }

    /// Write `contents` to `path`, creating the parent directories first.
    pub(crate) fn write_file(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().expect("path has a parent")).expect("create dirs");
        std::fs::write(path, contents).expect("write file");
    }

    /// Write a minimal virtual environment at `venv`: an interpreter file, a
    /// site-packages directory and a `pyvenv.cfg` naming `version`, such as
    /// `3.9.18`. Returns the site-packages directory.
    pub(crate) fn write_venv(venv: &Path, version: &str) -> PathBuf {
        let minor = version.split('.').take(2).collect::<Vec<_>>().join(".");
        let (exe, site_packages) = if cfg!(target_os = "windows") {
            (
                venv.join(r"Scripts\python.exe"),
                venv.join(r"Lib\site-packages"),
            )
        } else {
            (
                venv.join("bin/python"),
                venv.join(format!("lib/python{minor}/site-packages")),
            )
        };
        write_file(&exe, "");
        std::fs::create_dir_all(&site_packages).expect("create site-packages");
        write_file(
            &venv.join("pyvenv.cfg"),
            &format!("home = {}\nversion = {version}\n", venv.display()),
        );
        site_packages
    }

    /// `path` as a `SystemPathBuf`.
    pub(crate) fn system_path(path: &Path) -> SystemPathBuf {
        SystemPathBuf::from_path_buf(path.to_path_buf()).expect("UTF-8 path")
    }

    /// The `File` for `path`, which must be an existing file.
    pub(crate) fn system_file(db: &dyn ruff_db::Db, path: &Path) -> File {
        system_path_to_file(db, system_path(path)).expect("file exists")
    }

    /// The host path of `file`, which must be on disk.
    pub(crate) fn disk_path(db: &dyn ruff_db::Db, file: File) -> PathBuf {
        let path = file.path(db).as_system_path().expect("a file on disk");
        path.as_std_path().to_path_buf()
    }

    /// `path` canonicalized the way ty does it. On Windows this drops the
    /// `\\?\` prefix that `std::fs::canonicalize` adds.
    pub(crate) fn canonicalize(path: &Path) -> PathBuf {
        let canonical = std::fs::canonicalize(path).expect("canonicalize path");
        system_path(&canonical)
            .simplified()
            .as_std_path()
            .to_path_buf()
    }

    /// The root of the filesystem the tests run on: `/`, or the current drive's
    /// root on Windows. `OsSystem` needs an absolute current directory.
    pub(crate) fn filesystem_root() -> SystemPathBuf {
        let cwd = std::env::current_dir().expect("current directory");
        system_path(cwd.ancestors().last().expect("an absolute path has a root"))
    }

    /// Build a fresh `HydraDatabase` for tests. Each call is a clean db so
    /// tests don't share salsa caches.
    pub(crate) fn test_db() -> HydraDatabase {
        HydraDatabase::new(&filesystem_root())
    }

    /// A parameter with no annotation or default.
    pub(crate) fn sig_param(name: &str, kind: ParameterKind) -> ParameterInfo {
        ParameterInfo {
            name: name.to_string(),
            kind,
            type_annotation: None,
            default_value: None,
        }
    }

    /// A plain signature for `name`, with no return type, docstring or overloads.
    pub(crate) fn sig_for(name: &str, parameters: Vec<ParameterInfo>) -> FunctionSignature {
        FunctionSignature {
            name: name.to_string(),
            parameters,
            return_type: None,
            docstring: None,
            is_overloaded: false,
            overloads: Vec::new(),
            range: SourceRange::default(),
        }
    }

    /// `extract_definition_info` with `roots` as the first-party search roots.
    pub(crate) fn extract_definition_info(
        db: &dyn ty_module_resolver::Db,
        target: &str,
        roots: &[PathBuf],
    ) -> Result<super::cache::ResolvedDefinition, super::model::ResolveError> {
        super::resolve::extract_definition_info(
            db,
            target,
            super::cache::resolver_environment_for_roots(db, roots),
        )
    }
}
