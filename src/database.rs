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

impl HydraDatabase {
    /// Log memory usage statistics for all salsa ingredients.
    ///
    /// Reports struct counts, field sizes, and query memo counts.
    /// Useful for tuning LRU cache sizes and diagnosing memory issues.
    pub fn log_memory_usage(&self) {
        let db: &dyn salsa::Database = self;
        let info = db.memory_usage();

        for s in &info.structs {
            if s.count() > 0 {
                tracing::info!(
                    name = s.debug_name(),
                    count = s.count(),
                    fields_bytes = s.size_of_fields(),
                    metadata_bytes = s.size_of_metadata(),
                    "salsa struct"
                );
            }
        }

        for (name, q) in &info.queries {
            if q.count() > 0 {
                tracing::info!(
                    query = *name,
                    memos = q.count(),
                    fields_bytes = q.size_of_fields(),
                    metadata_bytes = q.size_of_metadata(),
                    "salsa query"
                );
            }
        }
    }
}

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

    #[test]
    fn test_hydra_database_creation() {
        use ruff_db::Db;
        let db = super::HydraDatabase::new(ruff_db::system::SystemPath::new("/tmp"));
        // Verify the database is functional
        let _files = db.files();
    }

    #[test]
    fn test_hydra_database_memory_usage() {
        let db = super::HydraDatabase::new(ruff_db::system::SystemPath::new("/tmp"));
        // Should not panic — verifies salsa_unstable feature works
        db.log_memory_usage();
    }

    #[test]
    fn test_hydra_database_memory_usage_after_queries() {
        use crate::yaml_cache::{DocumentInput, is_hydra_file, parsed_yaml};

        let db = TestDb::new();
        let input = DocumentInput::new(
            &db,
            "# @hydra\nmodel:\n  _target_: my.Mod\n".to_string(),
            1,
        );
        // Run some queries to populate the memo table
        let _ = is_hydra_file(&db, input);
        let _ = parsed_yaml(&db, input);

        // memory_usage should reflect the cached queries
        let db_ref: &dyn salsa::Database = &db;
        let info = db_ref.memory_usage();
        let total_memos: usize = info.queries.values().map(|q| q.count()).sum();
        assert!(total_memos > 0, "should have cached query memos");
    }

    #[test]
    fn test_testdb_new() {
        let db = TestDb::new();
        // Verify basic salsa operations work
        let input = crate::yaml_cache::DocumentInput::new(&db, "test".to_string(), 0);
        assert_eq!(input.text(&db), "test");
    }

    #[test]
    fn test_hydra_database_clone() {
        use crate::yaml_cache::{DocumentInput, is_hydra_file};

        let db = super::HydraDatabase::new(ruff_db::system::SystemPath::new("/tmp"));
        let input = DocumentInput::new(&db, "# @hydra\n_target_: a.B\n".to_string(), 1);
        let _ = is_hydra_file(&db, input);

        // Clone should work (used for snapshot-based concurrency)
        let db2 = db.clone();
        // Snapshot should see the same cached result
        let result = is_hydra_file(&db2, input);
        assert!(result);
    }
}
