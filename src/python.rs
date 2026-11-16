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
    use std::path::PathBuf;

    /// The `tests/workspace/simple` fixture directory.
    pub(crate) fn get_simple_test_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("workspace")
            .join("simple")
    }

    /// Build a fresh `HydraDatabase` for tests. Each call is a clean db so
    /// tests don't share salsa caches.
    pub(crate) fn test_db() -> HydraDatabase {
        HydraDatabase::new(SystemPath::new("/"))
    }
}
