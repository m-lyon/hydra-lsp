pub mod cache;
pub mod env;
pub mod extract;
pub mod format;
mod import_resolver;
pub mod model;
pub mod resolve;
pub mod typeshed;

/// Helpers shared by the unit tests of the modules above.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::database::HydraDatabase;
    use ruff_db::system::SystemPath;
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

    /// Build a fresh `HydraDatabase` for tests. Each call is a clean db so
    /// tests don't share salsa caches.
    pub(crate) fn test_db() -> HydraDatabase {
        HydraDatabase::new(SystemPath::new("/"))
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
            super::cache::environment_for_roots(db, roots),
        )
    }
}
