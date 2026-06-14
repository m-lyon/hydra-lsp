use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::debug;

use crate::python_analyzer::{DefinitionInfo, PythonAnalyzer};
use ruff_db::files::FileRootKind;
use ruff_db::system::SystemPathBuf;

/// Salsa input for Python environment configuration.
///
/// In the LSP backend, this is created during `initialize()` alongside the
/// analysis database, using the workspace root from LSP init params (or a
/// current-directory fallback) plus any configured Python interpreter.
///
/// This input participates in the cache key for Python definition lookups, so
/// changing its values invalidates cached module-resolution results. Per-file
/// invalidation for watched Python source and existing `.pth` edits is handled
/// separately via `ruff_db::files::File::sync_path` in the LSP backend, while
/// `.pth` create/delete events update the per-directory state handles below.
#[salsa::input]
pub struct PythonConfig {
    /// Workspace root directory path.
    #[returns(ref)]
    pub workspace_root: Option<String>,

    /// Configured Python interpreter path.
    #[returns(ref)]
    pub interpreter: Option<String>,

    /// Tracked state for site-packages directories whose `.pth` members can
    /// affect editable-install resolution.
    #[returns(ref)]
    pub site_packages_pth_states: Vec<SitePackagesPthState>,
}

/// Tracked revision for the `.pth` state of one site-packages directory.
#[salsa::input]
pub struct SitePackagesPthState {
    #[returns(ref)]
    pub directory: String,
    pub revision: u64,
}

/// Interned target string for cache key deduplication.
///
/// Salsa interns these so that repeated lookups for the same `_target_`
/// value (e.g., "myapp.models.MyModel") share a single allocation
/// and use cheap identity comparison as the cache key.
#[salsa::interned]
pub struct TargetString {
    #[returns(ref)]
    pub value: String,
}

/// Successfully resolved Python definition data.
#[derive(Clone)]
pub struct CachedDefinition {
    pub definition_info: DefinitionInfo,
    pub file_path: PathBuf,
    pub module_path: String,
    pub symbol_name: String,
}

/// Cached result of `PythonAnalyzer::extract_definition_info`.
///
/// Uses Arc + pointer-based equality (same pattern as `ParsedYaml`)
/// to satisfy salsa's Update requirements without expensive deep comparison.
#[derive(Clone)]
pub struct CachedDefinitionResult {
    inner: Arc<Result<CachedDefinition, String>>,
}

impl CachedDefinitionResult {
    fn from_result(result: anyhow::Result<(DefinitionInfo, PathBuf, String, String)>) -> Self {
        Self {
            inner: Arc::new(
                result
                    .map(|(def, path, module, symbol)| CachedDefinition {
                        definition_info: def,
                        file_path: path,
                        module_path: module,
                        symbol_name: symbol,
                    })
                    .map_err(|e| e.to_string()),
            ),
        }
    }

    /// Get the cached result.
    pub fn get(&self) -> Result<&CachedDefinition, &str> {
        self.inner.as_ref().as_ref().map_err(|e| e.as_str())
    }
}

impl PartialEq for CachedDefinitionResult {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for CachedDefinitionResult {}

/// Cached site-packages discovery for a Python environment.
///
/// Wraps `PythonAnalyzer::discover_python_environment` with salsa caching.
/// The result is cached and only recomputed when `PythonConfig`'s `workspace_root`
/// or `interpreter` fields change — both are salsa inputs, so invalidation is
/// automatic.
///
/// Also registers each discovered directory as a `LibrarySearchPath` file root
/// so library files get `Durability::HIGH`: project-file edits don't invalidate
/// library-derived memos.
#[salsa::tracked(returns(ref))]
pub fn site_packages_paths(db: &dyn ruff_db::Db, config: PythonConfig) -> Vec<SystemPathBuf> {
    let workspace_root = config.workspace_root(db).as_deref().map(Path::new);
    let interpreter = config.interpreter(db);
    let paths =
        PythonAnalyzer::discover_python_environment(workspace_root, interpreter.as_deref())
            .unwrap_or_default();
    for path in &paths {
        db.files()
            .try_add_root(db, path.as_path(), FileRootKind::LibrarySearchPath);
    }
    paths
}

/// Cached extraction of Python definition info for a `_target_` string.
///
/// Wraps `PythonAnalyzer::extract_definition_info` with salsa caching.
/// The result is cached and only recomputed when:
/// - The target string changes (different `_target_` value)
/// - The Python configuration changes (interpreter, workspace root, or
///   site-packages `.pth` state handles)
///
/// Uses `lru=1024` to bound memory for workspaces with many distinct targets.
/// Hydra projects often reference many `_target_` strings (e.g. one per config),
/// so the bound is set well above typical workspace sizes to avoid thrashing.
#[salsa::tracked(no_eq, lru = 1024)]
pub fn cached_definition_info<'db>(
    db: &'db dyn ruff_db::Db,
    config: PythonConfig,
    target: TargetString<'db>,
) -> CachedDefinitionResult {
    let target_str = target.value(db);
    debug!(
        target = target_str,
        "cached_definition_info: executing (cache miss)"
    );

    CachedDefinitionResult::from_result(PythonAnalyzer::extract_definition_info_for_config(
        db, target_str, config,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::HydraDatabase;
    use crate::database::tests::TestDb;
    use ruff_db::system::SystemPath;
    use salsa::Setter;
    use std::path::Path;

    #[salsa::interned]
    struct SitePackagesPath {
        #[returns(ref)]
        path: String,
    }

    #[salsa::tracked(returns(ref))]
    fn tracked_pth_paths<'db>(
        db: &'db dyn ruff_db::Db,
        site_packages: SitePackagesPath<'db>,
    ) -> Vec<PathBuf> {
        PythonAnalyzer::parse_pth_files(db, Path::new(site_packages.path(db)), None)
    }

    #[salsa::tracked(returns(ref))]
    fn tracked_pth_paths_with_site_packages_state<'db>(
        db: &'db dyn ruff_db::Db,
        config: PythonConfig,
        site_packages: SitePackagesPath<'db>,
    ) -> Vec<PathBuf> {
        let site_packages_pth_states = config.site_packages_pth_states(db);
        PythonAnalyzer::parse_pth_files(
            db,
            Path::new(site_packages.path(db)),
            Some(site_packages_pth_states.as_slice()),
        )
    }

    fn examples_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/workspace/simple")
    }

    /// Build a HydraDatabase for tests that read real on-disk Python files.
    /// `TestDb` uses an in-memory `TestSystem` and can't see workspace files.
    fn real_fs_db() -> HydraDatabase {
        HydraDatabase::new(SystemPath::new("/"))
    }

    #[test]
    fn test_site_packages_paths_no_config() {
        let db = real_fs_db();
        let config = PythonConfig::new(&db, None, None, vec![]);
        // Should not panic; returns whatever the current-dir discovery finds (possibly empty).
        let paths = site_packages_paths(&db, config);
        // Result is a Vec<SystemPathBuf>; we can't assert a specific value since it
        // depends on the active Python environment, but the call must succeed.
        let _ = paths.len();
    }

    #[test]
    fn test_site_packages_paths_cache_hit() {
        let db = real_fs_db();
        let examples = examples_dir();
        let config = PythonConfig::new(
            &db,
            Some(examples.to_string_lossy().to_string()),
            None,
            vec![],
        );
        let result1 = site_packages_paths(&db, config);
        let result2 = site_packages_paths(&db, config);
        // Pointer equality: salsa returns the same Arc on a cache hit.
        assert!(std::ptr::eq(result1 as *const _, result2 as *const _));
    }

    #[test]
    fn test_site_packages_paths_invalidation_on_interpreter_change() {
        let mut db = real_fs_db();
        let examples = examples_dir();
        let config = PythonConfig::new(
            &db,
            Some(examples.to_string_lossy().to_string()),
            None,
            vec![],
        );
        let result1 = site_packages_paths(&db, config).clone();

        // Changing the interpreter invalidates the memo.
        config
            .set_interpreter(&mut db)
            .to(Some("/nonexistent/python".to_string()));
        let result2 = site_packages_paths(&db, config).clone();

        // Results may differ (nonexistent interpreter → empty) — the important
        // thing is that the query ran again rather than returning the cached value.
        // We can't assert pointer inequality after a clone, but we can assert the
        // new result is empty (nonexistent interpreter → discovery fails → empty).
        assert!(result2.is_empty());
        // Original result is unaffected (already cloned).
        drop(result1);
    }

    #[test]
    fn test_python_config_fields() {
        let db = TestDb::new();
        let config = PythonConfig::new(&db, Some("/workspace".to_string()), None, vec![]);
        assert_eq!(config.workspace_root(&db).as_deref(), Some("/workspace"));
        assert_eq!(config.interpreter(&db).as_deref(), None);
        assert!(config.site_packages_pth_states(&db).is_empty());
    }

    #[test]
    fn test_python_config_update() {
        let mut db = TestDb::new();
        let config = PythonConfig::new(&db, None, None, vec![]);
        assert!(config.interpreter(&db).is_none());

        config
            .set_interpreter(&mut db)
            .to(Some("/usr/bin/python3".to_string()));
        assert_eq!(config.interpreter(&db).as_deref(), Some("/usr/bin/python3"));

        let site_packages_pth_state =
            SitePackagesPthState::new(&db, "/workspace/site-packages".to_string(), 0);
        config
            .set_site_packages_pth_states(&mut db)
            .to(vec![site_packages_pth_state]);
        assert!(config.site_packages_pth_states(&db) == &[site_packages_pth_state]);
    }

    #[test]
    fn test_target_string_interning() {
        let db = TestDb::new();
        let t1 = TargetString::new(&db, "my.Module".to_string());
        let t2 = TargetString::new(&db, "my.Module".to_string());
        let t3 = TargetString::new(&db, "other.Module".to_string());

        // Same string → same interned ID
        assert!(t1 == t2);
        // Different string → different ID
        assert!(t1 != t3);
        // Value round-trips
        assert_eq!(t1.value(&db), "my.Module");
    }

    #[test]
    fn test_cached_definition_info_valid_target() {
        let db = real_fs_db();
        let workspace = examples_dir();
        let config = PythonConfig::new(
            &db,
            Some(workspace.to_string_lossy().to_string()),
            None,
            vec![],
        );
        let target = TargetString::new(&db, "my_module.DataLoader".to_string());
        let result = cached_definition_info(&db, config, target);

        let def = result.get().expect("should resolve DataLoader");
        assert_eq!(def.symbol_name, "DataLoader");
        assert!(matches!(def.definition_info, DefinitionInfo::Class(_)));
    }

    #[test]
    fn test_cached_definition_info_invalid_target() {
        let db = TestDb::new();
        let workspace = examples_dir();
        let config = PythonConfig::new(
            &db,
            Some(workspace.to_string_lossy().to_string()),
            None,
            vec![],
        );
        let target = TargetString::new(&db, "nonexistent.Module".to_string());
        let result = cached_definition_info(&db, config, target);
        assert!(result.get().is_err());
    }

    #[test]
    fn test_cached_definition_info_cache_hit() {
        let db = real_fs_db();
        let workspace = examples_dir();
        let config = PythonConfig::new(
            &db,
            Some(workspace.to_string_lossy().to_string()),
            None,
            vec![],
        );
        let target = TargetString::new(&db, "my_module.DataLoader".to_string());

        let result1 = cached_definition_info(&db, config, target);
        let result2 = cached_definition_info(&db, config, target);

        // Same input, no changes — should be pointer-equal (cache hit)
        assert!(result1 == result2);
    }

    #[test]
    fn test_cached_definition_info_function_target() {
        let db = real_fs_db();
        let workspace = examples_dir();
        let config = PythonConfig::new(
            &db,
            Some(workspace.to_string_lossy().to_string()),
            None,
            vec![],
        );
        let target = TargetString::new(&db, "my_module.create_model".to_string());
        let result = cached_definition_info(&db, config, target);

        let def = result.get().expect("should resolve create_model");
        assert_eq!(def.symbol_name, "create_model");
        assert!(matches!(def.definition_info, DefinitionInfo::Function(_)));
    }

    #[test]
    fn test_cached_definition_info_invalidation_on_config_change() {
        let mut db = real_fs_db();
        let workspace = examples_dir();
        let config = PythonConfig::new(
            &db,
            Some(workspace.to_string_lossy().to_string()),
            None,
            vec![],
        );

        let result1 = {
            let target = TargetString::new(&db, "my_module.DataLoader".to_string());
            cached_definition_info(&db, config, target)
        };
        assert!(result1.get().is_ok());

        // Change workspace root — invalidates all cached lookups
        config
            .set_workspace_root(&mut db)
            .to(Some("/nonexistent/path".to_string()));

        let result2 = {
            let target = TargetString::new(&db, "my_module.DataLoader".to_string());
            cached_definition_info(&db, config, target)
        };
        // Result pointer should differ (recomputed, not from cache)
        assert!(result1 != result2);
    }

    #[test]
    fn test_cached_definition_info_invalidation_on_file_sync() {
        use filetime::{FileTime, set_file_mtime};
        use ruff_db::files::File;
        use std::fs;
        use tempfile::TempDir;

        // Build a temporary workspace so we can actually modify the file
        // backing the resolved target. `File::sync_path` is intentionally a
        // no-op when on-disk metadata has not changed (matches the LSP path:
        // the file watcher only fires for real changes).
        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path();
        let py_file = workspace.join("watched_module.py");
        fs::write(
            &py_file,
            "class WatchedClass:\n    \"\"\"v1\"\"\"\n    pass\n",
        )
        .expect("write v1");
        // Pin the initial mtime so the post-write bump below is unambiguous,
        // even on filesystems with second-resolution mtimes (network mounts,
        // older HFS+, ext3) where relying on the wall clock is flaky.
        set_file_mtime(&py_file, FileTime::from_unix_time(1_000_000, 0))
            .expect("set initial mtime");

        let mut db = real_fs_db();
        let config = PythonConfig::new(
            &db,
            Some(workspace.to_string_lossy().to_string()),
            None,
            vec![],
        );
        let result1 = {
            let target = TargetString::new(&db, "watched_module.WatchedClass".to_string());
            cached_definition_info(&db, config, target)
        };
        assert!(result1.get().is_ok(), "should resolve WatchedClass");

        // Modify the file on disk and force the mtime to a known-newer value
        // so `sync_path` definitely sees a change. This models what the LSP
        // does when did_change_watched_files notifies us of a real change.
        fs::write(
            &py_file,
            "class WatchedClass:\n    \"\"\"v2\"\"\"\n    pass\n",
        )
        .expect("write v2");
        set_file_mtime(&py_file, FileTime::from_unix_time(2_000_000, 0))
            .expect("set updated mtime");

        let sys_path = SystemPath::from_std_path(&py_file).unwrap();
        File::sync_path(&mut db, sys_path);

        let result2 = {
            let target = TargetString::new(&db, "watched_module.WatchedClass".to_string());
            cached_definition_info(&db, config, target)
        };
        assert!(
            result1 != result2,
            "memo should be invalidated after sync_path picks up the disk change"
        );
    }

    #[test]
    fn test_cached_definition_info_invalidation_on_pth_file_sync() {
        use filetime::{FileTime, set_file_mtime};
        use ruff_db::files::File;
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let site_packages = tmp.path().join("site-packages");
        let editable_src = tmp.path().join("src");
        fs::create_dir_all(&site_packages).expect("create site-packages dir");
        fs::create_dir_all(&editable_src).expect("create editable src dir");

        let pth_file = site_packages.join("editable_package.pth");
        fs::write(&pth_file, "../src\n").expect("write initial pth");
        set_file_mtime(&pth_file, FileTime::from_unix_time(1_000_000, 0))
            .expect("set initial pth mtime");

        let mut db = real_fs_db();
        let site_packages_str = site_packages.to_string_lossy().to_string();
        let result1 = {
            let site_packages_key = SitePackagesPath::new(&db, site_packages_str.clone());
            tracked_pth_paths(&db, site_packages_key).clone()
        };
        assert_eq!(result1.len(), 1);
        assert_eq!(
            std::fs::canonicalize(&result1[0]).expect("canonicalize returned editable path"),
            std::fs::canonicalize(&editable_src).expect("canonicalize expected editable path"),
        );

        fs::write(&pth_file, "../missing\n").expect("write updated pth");
        set_file_mtime(&pth_file, FileTime::from_unix_time(2_000_000, 0))
            .expect("set updated pth mtime");

        let sys_path = SystemPath::from_std_path(&pth_file).unwrap();
        File::sync_path(&mut db, sys_path);

        let result2 = {
            let site_packages_key = SitePackagesPath::new(&db, site_packages_str);
            tracked_pth_paths(&db, site_packages_key).clone()
        };
        assert!(result2.is_empty());
    }

    #[test]
    fn test_site_packages_pth_state_invalidation_on_file_create() {
        use crate::python_analyzer::normalize_site_packages_pth_state_key;
        use filetime::{FileTime, set_file_mtime};
        use ruff_db::files::File;
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let site_packages = tmp.path().join("site-packages");
        let editable_src = tmp.path().join("src");
        fs::create_dir_all(&site_packages).expect("create site-packages dir");
        fs::create_dir_all(&editable_src).expect("create editable src dir");

        let mut db = real_fs_db();
        // Inventory key must go through the same normalization that
        // `parse_pth_files` applies to its lookup side; otherwise the lookup
        // misses and the salsa dep on `inventory.revision` is never registered.
        let inventory_key = normalize_site_packages_pth_state_key(&site_packages);
        let site_packages_pth_state = SitePackagesPthState::new(&db, inventory_key, 0);
        let config = PythonConfig::new(&db, None, None, vec![site_packages_pth_state]);
        let site_packages_str = site_packages.to_string_lossy().to_string();
        let result1 = {
            let site_packages_key = SitePackagesPath::new(&db, site_packages_str.clone());
            tracked_pth_paths_with_site_packages_state(&db, config, site_packages_key).clone()
        };
        assert!(result1.is_empty());

        let pth_file = site_packages.join("editable_package.pth");
        fs::write(&pth_file, "../src\n").expect("write pth");
        set_file_mtime(&pth_file, FileTime::from_unix_time(1_000_000, 0)).expect("set pth mtime");

        let sys_path = SystemPath::from_std_path(&pth_file).unwrap();
        File::sync_path(&mut db, sys_path);
        site_packages_pth_state.set_revision(&mut db).to(1);

        let result2 = {
            let site_packages_key = SitePackagesPath::new(&db, site_packages_str);
            tracked_pth_paths_with_site_packages_state(&db, config, site_packages_key).clone()
        };
        assert_eq!(result2.len(), 1);
        assert_eq!(
            std::fs::canonicalize(&result2[0]).expect("canonicalize returned editable path"),
            std::fs::canonicalize(&editable_src).expect("canonicalize expected editable path"),
        );
    }

    #[test]
    fn test_cached_definition_result_equality() {
        // Two separately-created results should not be pointer-equal
        let r1 = CachedDefinitionResult::from_result(Err(anyhow::anyhow!("err")));
        let r2 = CachedDefinitionResult::from_result(Err(anyhow::anyhow!("err")));
        assert!(r1 != r2);

        // Cloned result should be pointer-equal
        let r3 = r1.clone();
        assert!(r1 == r3);
    }
}
