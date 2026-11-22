use std::sync::LazyLock;

use ruff_db::diagnostic::Diagnostic;
use ruff_db::files::{File, Files};
use ruff_db::system::System;
use ruff_db::system::{OsSystem, SystemPath};
use ruff_db::vendored::VendoredFileSystem;
use ty_python_core::ProgramFile;
use ty_python_semantic::dependency::DependencyMetadata;
use ty_python_semantic::lint::{LintRegistry, RuleSelection};
use ty_python_semantic::{AnalysisSettings, PythonVersionWithSource, default_lint_registry};

use crate::python::cache::{PythonConfig, program, python_version_with_source};

/// The core Salsa database for hydrust.
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
    python_config: Option<PythonConfig>,
}

impl HydraDatabase {
    /// Create a new database rooted at the given working directory.
    pub fn new(cwd: &SystemPath) -> Self {
        Self {
            storage: salsa::Storage::default(),
            files: Files::default(),
            system: OsSystem::new(cwd),
            vendored: vendored_typeshed(),
            python_config: None,
        }
    }

    /// Set the config ty's semantic queries run under. Salsa does not track
    /// this field, so set it once, before the first of those queries.
    pub fn set_python_config(&mut self, config: PythonConfig) {
        self.python_config = Some(config);
    }
}

/// hydrust reports none of ty's lints, so none are enabled.
static RULE_SELECTION: LazyLock<RuleSelection> = LazyLock::new(RuleSelection::default);

static ANALYSIS_SETTINGS: LazyLock<AnalysisSettings> = LazyLock::new(AnalysisSettings::default);

/// Implements ty's semantic database traits for a database with a
/// `python_config: Option<PythonConfig>` field.
///
/// hydrust asks ty for types but does not run ty's checker, so no file is
/// checked and ty collects no diagnostics.
macro_rules! impl_semantic_db {
    ($db:ty) => {
        #[salsa::db]
        impl ty_python_core::Db for $db {
            fn should_check_file(&self, _file: File) -> bool {
                false
            }
        }

        #[salsa::db]
        impl ty_python_semantic::Db for $db {
            fn check_file(&self, _file: File) -> Vec<Diagnostic> {
                Vec::new()
            }

            fn program_file(&self, file: File) -> ProgramFile<'_> {
                let config = self.python_config.expect(NO_PYTHON_CONFIG);
                program(self, config).program_file(self, file)
            }

            fn python_version_with_source(&self, _file: File) -> &PythonVersionWithSource {
                let config = self.python_config.expect(NO_PYTHON_CONFIG);
                python_version_with_source(self, config)
            }

            fn rule_selection(&self, _file: File) -> &RuleSelection {
                &RULE_SELECTION
            }

            fn lint_registry(&self) -> &LintRegistry {
                default_lint_registry()
            }

            fn analysis_settings(&self, _file: File) -> &AnalysisSettings {
                &ANALYSIS_SETTINGS
            }

            fn dependency_metadata(&self, _file: File) -> Option<&DependencyMetadata> {
                None
            }

            fn verbose(&self) -> bool {
                false
            }

            fn is_open_file(&self, _file: File) -> bool {
                false
            }

            fn dyn_clone(&self) -> Box<dyn ty_python_semantic::Db> {
                Box::new(self.clone())
            }
        }
    };
}

const NO_PYTHON_CONFIG: &str = "set_python_config must be called before ty's semantic queries";

/// The vendored typeshed stubs, shared by every database instance.
///
/// `ty_vendored` keeps the zip archive in a `LazyLock` and `VendoredFileSystem`
/// is `Arc`-backed, so cloning it per database is a refcount bump, not a copy.
/// See [`crate::python::typeshed`] for how these stubs are addressed.
fn vendored_typeshed() -> VendoredFileSystem {
    ty_vendored::file_system().clone()
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
}

#[salsa::db]
impl ty_module_resolver::Db for HydraDatabase {}

impl_semantic_db!(HydraDatabase);

#[salsa::db]
impl salsa::Database for HydraDatabase {}

impl HydraDatabase {
    /// Log memory usage statistics for all salsa ingredients.
    ///
    /// Reports struct counts, field sizes, and query memo counts.
    /// Useful for tuning LRU cache sizes and diagnosing memory issues.
    pub fn log_memory_usage(&mut self) {
        let db: &mut dyn salsa::Database = self;
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
    use super::*;
    use ruff_db::system::{DbWithTestSystem, TestSystem};

    /// Test database using an in-memory filesystem.
    #[salsa::db]
    #[derive(Clone)]
    pub struct TestDb {
        storage: salsa::Storage<Self>,
        files: Files,
        system: TestSystem,
        vendored: VendoredFileSystem,
        python_config: Option<PythonConfig>,
    }

    impl Default for TestDb {
        /// Mirrors `HydraDatabase`: the vendored typeshed must be present or
        /// every `builtins.*` target would resolve differently under test than
        /// in the server.
        fn default() -> Self {
            Self {
                storage: salsa::Storage::default(),
                files: Files::default(),
                system: TestSystem::default(),
                vendored: vendored_typeshed(),
                python_config: None,
            }
        }
    }

    impl TestDb {
        pub fn new() -> Self {
            Self::default()
        }

        /// See [`HydraDatabase::set_python_config`].
        pub fn set_python_config(&mut self, config: PythonConfig) {
            self.python_config = Some(config);
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
    impl ty_module_resolver::Db for TestDb {}

    impl_semantic_db!(TestDb);

    #[salsa::db]
    impl salsa::Database for TestDb {}

    /// The types ty infers for the values of `file`'s top-level assignments.
    fn assigned_types(db: &dyn ty_python_semantic::Db, file: File) -> Vec<String> {
        use ty_python_semantic::{HasType, ProgramEnvironment, SemanticModel};

        let program_file = db.program_file(file);
        let module = ruff_db::parsed::parsed_module(db, program_file.python_file(db)).load(db);
        let model = SemanticModel::new(db, program_file);
        let environment = ProgramEnvironment::from_program(model.program());
        module
            .suite()
            .iter()
            .filter_map(|statement| statement.as_assign_stmt())
            .map(|assignment| {
                let ty = assignment.value.inferred_type(&model).expect("a type");
                ty.display(db, &environment).to_string()
            })
            .collect()
    }

    #[test]
    fn test_ty_infers_types_across_workspace_and_stdlib() {
        use crate::python::test_support::{canonicalize, system_path, test_db, write_file};

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let workspace = canonicalize(tmp.path());
        write_file(&workspace.join("pkg/models.py"), "class Model: ...\n");
        let main = workspace.join("main.py");
        write_file(
            &main,
            "import os\nfrom pkg.models import Model\nmodel = Model()\nsep = os.sep\n",
        );

        let mut db = test_db();
        let config = PythonConfig::frozen(
            &db,
            system_path(&workspace),
            Some("/nonexistent/python".into()),
        );
        db.set_python_config(config);

        let file = ruff_db::files::system_path_to_file(&db, system_path(&main)).unwrap();
        assert_eq!(assigned_types(&db, file), ["Model", "LiteralString"]);
    }

    #[test]
    fn test_testdb_infers_types() {
        use ruff_db::system::DbWithWritableSystem;

        let mut db = TestDb::new();
        db.write_file("/workspace/main.py", "value = len(\"ab\")\n")
            .expect("write file");
        let config =
            PythonConfig::frozen(&db, "/workspace".into(), Some("/nonexistent/python".into()));
        db.set_python_config(config);

        let file = ruff_db::files::system_path_to_file(&db, "/workspace/main.py").unwrap();
        assert_eq!(assigned_types(&db, file), ["Literal[2]"]);
    }

    #[test]
    fn test_hydra_database_creation() {
        use ruff_db::Db;
        let db = crate::python::test_support::test_db();
        // Verify the database is functional
        let _files = db.files();
    }

    #[test]
    fn test_hydra_database_memory_usage() {
        let mut db = crate::python::test_support::test_db();
        // Should not panic — verifies salsa_unstable feature works
        db.log_memory_usage();
    }

    #[test]
    fn test_hydra_database_memory_usage_after_queries() {
        use crate::yaml_cache::{DocumentInput, is_hydra_file, parsed_yaml};

        let mut db = TestDb::new();
        let input =
            DocumentInput::new(&db, "# @hydra\nmodel:\n  _target_: my.Mod\n".to_string(), 1);
        // Run some queries to populate the memo table
        let _ = is_hydra_file(&db, input);
        let _ = parsed_yaml(&db, input);

        // memory_usage should reflect the cached queries
        let db_ref: &mut dyn salsa::Database = &mut db;
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

        let db = crate::python::test_support::test_db();
        let input = DocumentInput::new(&db, "# @hydra\n_target_: a.B\n".to_string(), 1);
        let _ = is_hydra_file(&db, input);

        // Clone should work (used for snapshot-based concurrency)
        let db2 = db.clone();
        // Snapshot should see the same cached result
        let result = is_hydra_file(&db2, input);
        assert!(result);
    }
}
