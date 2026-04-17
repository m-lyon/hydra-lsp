use ruff_db::files::Files;
use ruff_db::system::System;
use ruff_db::system::{OsSystem, SystemPath};
use ruff_db::vendored::VendoredFileSystem;
use ruff_python_ast::PythonVersion;

/// The core Salsa database for hydra-lsp.
///
/// Implements `ruff_db::Db` (which extends `salsa::Database`) to provide:
/// - File tracking and interning via `Files`
/// - File system access via `OsSystem`
/// - Source text caching (via ruff_db::source::source_text)
/// - Line index caching (via ruff_db::source::line_index)
///
/// This database serves as the foundation for incremental caching of
/// YAML parsing, Python analysis, and all LSP queries.
#[salsa::db]
#[derive(Clone)]
pub struct HydraDatabase {
    storage: salsa::Storage<Self>,
    files: Files,
    system: OsSystem,
    vendored: VendoredFileSystem,
}

impl HydraDatabase {
    /// Create a new database rooted at the given working directory.
    pub fn new(cwd: &SystemPath) -> Self {
        Self {
            storage: salsa::Storage::default(),
            files: Files::default(),
            system: OsSystem::new(cwd),
            vendored: VendoredFileSystem::default(),
        }
    }
}

#[salsa::db]
impl ruff_db::Db for HydraDatabase {
    fn vendored(&self) -> &VendoredFileSystem {
        &self.vendored
    }

    fn system(&self) -> &dyn System {
        &self.system
    }

    fn files(&self) -> &Files {
        &self.files
    }

    fn python_version(&self) -> PythonVersion {
        PythonVersion::latest_ty()
    }
}

#[salsa::db]
impl salsa::Database for HydraDatabase {}

#[cfg(test)]
pub mod tests {
    use ruff_db::files::Files;
    use ruff_db::system::{DbWithTestSystem, System, TestSystem};
    use ruff_db::vendored::VendoredFileSystem;
    use ruff_python_ast::PythonVersion;

    /// Test database using an in-memory filesystem.
    #[salsa::db]
    #[derive(Default, Clone)]
    pub struct TestDb {
        storage: salsa::Storage<Self>,
        files: Files,
        system: TestSystem,
        vendored: VendoredFileSystem,
    }

    impl TestDb {
        pub fn new() -> Self {
            Self::default()
        }
    }

    #[salsa::db]
    impl ruff_db::Db for TestDb {
        fn vendored(&self) -> &VendoredFileSystem {
            &self.vendored
        }

        fn system(&self) -> &dyn System {
            &self.system
        }

        fn files(&self) -> &Files {
            &self.files
        }

        fn python_version(&self) -> PythonVersion {
            PythonVersion::latest_ty()
        }
    }

    impl DbWithTestSystem for TestDb {
        fn test_system(&self) -> &TestSystem {
            &self.system
        }

        fn test_system_mut(&mut self) -> &mut TestSystem {
            &mut self.system
        }
    }

    #[salsa::db]
    impl salsa::Database for TestDb {}
}
