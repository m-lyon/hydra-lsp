use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::import_resolver::{ImportResolver, join_module_parts};
use crate::python_analyzer::{
    ClassAttributeInfo, DefinitionInfo, FunctionSignature, PythonAnalyzer, normalize_path_for_key,
};
use ruff_db::files::FileRootKind;
use ruff_db::system::SystemPathBuf;
use tracing::debug;

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
/// `.pth` create/delete events bump the enclosing site-packages `FileRoot`
/// revision via `ruff_db::files::Files::touch_root`.
#[salsa::input]
pub struct PythonConfig {
    /// Workspace root directory path.
    #[returns(ref)]
    pub workspace_root: Option<String>,

    /// Configured Python interpreter path.
    #[returns(ref)]
    pub interpreter: Option<String>,
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

/// Interned search-path list used as a salsa cache key for module resolution.
///
/// Salsa interns by value, so calls with equal `Vec<PathBuf>` values share a
/// single allocation and use cheap identity comparison as the cache key.
/// In practice all targets within one workspace session share the same search
/// paths (derived from the workspace root and site-packages), so there is
/// typically only one live `InternedSearchPaths` value in the database.
#[salsa::interned]
pub struct InternedSearchPaths {
    pub paths: Vec<PathBuf>,
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
    let paths = PythonAnalyzer::discover_python_environment(workspace_root, interpreter.as_deref())
        .unwrap_or_default();
    for path in &paths {
        db.files()
            .try_add_root(db, path.as_path(), FileRootKind::LibrarySearchPath);
    }
    paths
}

/// Cached module-resolution search-path list for a Python environment config.
///
/// Builds the ordered search-path list that module resolution walks:
/// `[workspace_root, ".", site_packages_0, pth_paths_0…, site_packages_1, …]`.
///
/// Recomputes when `PythonConfig`'s `workspace_root` or `interpreter` fields
/// change, and — via `PythonAnalyzer::parse_pth_files`'s dependency (called within
/// PythonAnalyzer::build_search_paths) on each site-packages `FileRoot` revision —
/// when a `.pth` file is created or deleted in a site-packages directory.
#[salsa::tracked(returns(ref))]
pub fn search_paths_for_config(db: &dyn ruff_db::Db, config: PythonConfig) -> Vec<PathBuf> {
    let workspace_root = config.workspace_root(db).as_deref().map(Path::new);
    let site_packages = site_packages_paths(db, config);

    PythonAnalyzer::build_search_paths(db, workspace_root, site_packages)
}

/// Cached module path → file path resolution.
///
/// Validates the name, then probes each search root in order
/// with `ImportResolver::find_module_file`. Salsa memoises the result, so the
/// filesystem scan for a given `(module_path, search_paths)` pair is performed
/// at most once per revision. The primary beneficiaries are
/// `resolve_class_attribute_chain`, which probes O(k) progressively shorter
/// prefixes of a dotted target (k = dot-count), and
/// `ImportResolver::resolve_module_path`, which runs once per import hop. On a
/// second hover for any symbol in the same module, all those probes become
/// O(1) cache lookups.
///
/// Returns `None` when the module cannot be resolved, including when the name
/// is empty, has an empty dot-separated part, or is not a plain dotted module name.
/// Uses `lru = 1024` to bound memory across large workspaces.
#[salsa::tracked(returns(ref), lru = 1024)]
pub fn resolve_module_cached<'db>(
    db: &'db dyn ruff_db::Db,
    module_path: TargetString<'db>,
    search_paths: InternedSearchPaths<'db>,
) -> Option<PathBuf> {
    let module_path_str = module_path.value(db);
    let search_paths = search_paths.paths(db);

    // An empty part means a leading, trailing or doubled dot. Hydra cannot
    // instantiate such a target, and `join_module_parts` would skip the part
    // and resolve the name as if the extra dot were not there.
    if module_path_str.split('.').any(str::is_empty) {
        return None;
    }

    let relative_path = join_module_parts(Path::new(""), module_path_str)?;

    for search_path in search_paths {
        // `find_module_file` covers both shapes a module can take: a package
        // directory with an `__init__` file, and a plain `.py`/`.pyi` file.
        let package_path = search_path.join(&relative_path);
        if let Some(found_path) = ImportResolver::find_module_file(db, &package_path) {
            return Some(found_path);
        }
    }

    None
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

    let search_paths = search_paths_for_config(db, config);
    CachedDefinitionResult::from_result(PythonAnalyzer::extract_definition_info(
        db,
        target_str,
        search_paths,
    ))
}

/// Cached docstring + `__init__` resolution for a class, walking its MRO.
///
/// Salsa memoises each `(class_key, search_paths)` pair, so parent classes
/// shared across different child classes are resolved at most once per revision.
/// The own class properties take priority; parent properties fill in only what
/// is missing.
#[derive(Clone, Debug)]
pub struct ClassParentDocs {
    inner: Arc<(Option<String>, Option<FunctionSignature>)>,
}

impl ClassParentDocs {
    fn new(docstring: Option<String>, init: Option<FunctionSignature>) -> Self {
        Self {
            inner: Arc::new((docstring, init)),
        }
    }

    /// Resolved docstring — from the class itself or the nearest ancestor that has one.
    pub fn docstring(&self) -> Option<&String> {
        self.inner.0.as_ref()
    }

    /// Resolved `__init__` signature — from the class itself or the nearest ancestor.
    pub fn init(&self) -> Option<&FunctionSignature> {
        self.inner.1.as_ref()
    }
}

impl PartialEq for ClassParentDocs {
    /// Value equality (deep-compares the resolved docstring and `__init__`).
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}
impl Eq for ClassParentDocs {}

/// Cached result of looking up a class attribute by walking the MRO.
///
/// `None` inner value means the attribute was not found on the class or any parent.
#[derive(Clone, Debug)]
pub struct CachedClassAttribute {
    inner: Arc<Option<(ClassAttributeInfo, PathBuf, String)>>,
}

impl CachedClassAttribute {
    fn found(attr_info: ClassAttributeInfo, file_path: PathBuf, class_name: String) -> Self {
        Self {
            inner: Arc::new(Some((attr_info, file_path, class_name))),
        }
    }

    fn not_found() -> Self {
        Self {
            inner: Arc::new(None),
        }
    }

    /// Returns a reference to `(ClassAttributeInfo, file_path, class_name)` if found.
    pub fn get(&self) -> Option<&(ClassAttributeInfo, PathBuf, String)> {
        self.inner.as_ref().as_ref()
    }
}

impl PartialEq for CachedClassAttribute {
    /// Value equality (deep-compares the resolved attribute, file path, and class).
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}
impl Eq for CachedClassAttribute {}

/// Cached docstring + `__init__` resolution for one class, including its MRO.
///
/// `class_key` encodes `"canonical_file_path::ClassName"`. The query calls
/// `extract_class_info` for the class itself and, for any missing property,
/// recursively invokes itself for each parent (MRO order, left-to-right).
/// Salsa memoises every node independently, so a grandparent shared by two
/// sibling classes is resolved only once per revision.
///
/// `cycle_result` handles circular inheritance (invalid Python) gracefully by
/// returning empty docs rather than panicking.
#[salsa::tracked(lru = 512, cycle_result = class_parent_docs_cycle)]
pub fn class_parent_docs<'db>(
    db: &'db dyn ruff_db::Db,
    class_key: TargetString<'db>,
    search_paths: InternedSearchPaths<'db>,
) -> ClassParentDocs {
    let key_str = class_key.value(db);
    let Some((file_path_str, class_name)) = key_str.split_once("::") else {
        return ClassParentDocs::new(None, None);
    };
    let file_path = Path::new(file_path_str);

    let Ok(class_info) = PythonAnalyzer::extract_class_info(db, file_path, class_name) else {
        return ClassParentDocs::new(None, None);
    };

    let mut docstring = class_info.docstring;
    let mut init = class_info.init_signature;

    if docstring.is_some() && init.is_some() {
        return ClassParentDocs::new(docstring, init);
    }

    let search_paths_vec = search_paths.paths(db);

    for base_class in &class_info.base_classes {
        if matches!(base_class.as_str(), "object" | "ABC" | "Protocol") {
            continue;
        }
        let Some((parent_file, parent_class_name)) =
            PythonAnalyzer::resolve_base_class(db, base_class, file_path, &search_paths_vec)
        else {
            continue;
        };
        // Lexical normalization only — no `fs::canonicalize` syscall inside this
        // tracked body (keeps the memo key a pure function of salsa inputs).
        // Symlink resolution already happened at root construction; see
        // `normalize_path_for_key`.
        let normalized = normalize_path_for_key(db, &parent_file);
        let parent_key = TargetString::new(
            db,
            format!("{}::{}", normalized.display(), parent_class_name),
        );
        let parent_docs = class_parent_docs(db, parent_key, search_paths);

        if docstring.is_none() {
            docstring = parent_docs.docstring().cloned();
        }
        if init.is_none() {
            init = parent_docs.init().cloned();
        }
        if docstring.is_some() && init.is_some() {
            break;
        }
    }

    ClassParentDocs::new(docstring, init)
}

fn class_parent_docs_cycle(
    _db: &dyn ruff_db::Db,
    _id: salsa::Id,
    _class_key: TargetString,
    _search_paths: InternedSearchPaths,
) -> ClassParentDocs {
    ClassParentDocs::new(None, None)
}

/// Cached class-attribute lookup, walking the MRO when not found directly.
///
/// `class_key` encodes `"canonical_file_path::ClassName"`;
/// `attribute_key` is the bare attribute name (interned for deduplication).
/// Each `(class_key, attribute_key, search_paths)` triple is memoised, so
/// shared parent classes are looked up at most once per revision.
///
/// `cycle_result` handles circular inheritance gracefully.
#[salsa::tracked(lru = 512, cycle_result = class_parent_attribute_cycle)]
pub fn class_parent_attribute<'db>(
    db: &'db dyn ruff_db::Db,
    class_key: TargetString<'db>,
    attribute_key: TargetString<'db>,
    search_paths: InternedSearchPaths<'db>,
) -> CachedClassAttribute {
    let key_str = class_key.value(db);
    let Some((file_path_str, class_name)) = key_str.split_once("::") else {
        return CachedClassAttribute::not_found();
    };
    let file_path = Path::new(file_path_str);
    let attribute_name = attribute_key.value(db);

    if let Ok(attr_info) =
        PythonAnalyzer::extract_class_attribute(db, file_path, class_name, attribute_name)
    {
        return CachedClassAttribute::found(
            attr_info,
            file_path.to_path_buf(),
            class_name.to_string(),
        );
    }

    let Ok(class_info) = PythonAnalyzer::extract_class_info(db, file_path, class_name) else {
        return CachedClassAttribute::not_found();
    };

    let search_paths_vec = search_paths.paths(db);

    for base_class in &class_info.base_classes {
        if matches!(base_class.as_str(), "object" | "ABC" | "Protocol") {
            continue;
        }
        let Some((parent_file, parent_class_name)) =
            PythonAnalyzer::resolve_base_class(db, base_class, file_path, &search_paths_vec)
        else {
            continue;
        };
        // Lexical normalization only — no `fs::canonicalize` syscall inside this
        // tracked body (keeps the memo key a pure function of salsa inputs).
        // Symlink resolution already happened at root construction; see
        // `normalize_path_for_key`.
        let normalized = normalize_path_for_key(db, &parent_file);
        let parent_key = TargetString::new(
            db,
            format!("{}::{}", normalized.display(), parent_class_name),
        );
        let result = class_parent_attribute(db, parent_key, attribute_key, search_paths);
        if result.get().is_some() {
            return result;
        }
    }

    CachedClassAttribute::not_found()
}

fn class_parent_attribute_cycle(
    _db: &dyn ruff_db::Db,
    _id: salsa::Id,
    _class_key: TargetString,
    _attribute_key: TargetString,
    _search_paths: InternedSearchPaths,
) -> CachedClassAttribute {
    CachedClassAttribute::not_found()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::HydraDatabase;
    use crate::database::tests::TestDb;
    use ruff_db::Db;
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
        PythonAnalyzer::parse_pth_files(db, Path::new(site_packages.path(db)))
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
        let config = PythonConfig::new(&db, None, None);
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
        let config = PythonConfig::new(&db, Some(examples.to_string_lossy().to_string()), None);
        let result1 = site_packages_paths(&db, config);
        let result2 = site_packages_paths(&db, config);
        // Pointer equality: salsa returns the same Arc on a cache hit.
        assert!(std::ptr::eq(result1 as *const _, result2 as *const _));
    }

    #[test]
    fn test_site_packages_paths_invalidation_on_interpreter_change() {
        let mut db = real_fs_db();
        let examples = examples_dir();
        let config = PythonConfig::new(&db, Some(examples.to_string_lossy().to_string()), None);
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
    fn test_search_paths_for_config_contains_workspace_root() {
        let db = real_fs_db();
        let examples = examples_dir();
        let config = PythonConfig::new(&db, Some(examples.to_string_lossy().to_string()), None);
        let paths = search_paths_for_config(&db, config);
        assert!(
            paths.iter().any(|p| p == &examples),
            "workspace root should appear first in search paths"
        );
        assert!(
            paths.iter().position(|p| p == &examples) == Some(0),
            "workspace root should be at index 0"
        );
    }

    #[test]
    fn test_search_paths_for_config_cache_hit() {
        let db = real_fs_db();
        let config = PythonConfig::new(&db, None, None);
        let r1 = search_paths_for_config(&db, config);
        let r2 = search_paths_for_config(&db, config);
        assert!(std::ptr::eq(r1 as *const _, r2 as *const _));
    }

    #[test]
    fn test_python_config_fields() {
        let db = TestDb::new();
        let config = PythonConfig::new(&db, Some("/workspace".to_string()), None);
        assert_eq!(config.workspace_root(&db).as_deref(), Some("/workspace"));
        assert_eq!(config.interpreter(&db).as_deref(), None);
    }

    #[test]
    fn test_python_config_update() {
        let mut db = TestDb::new();
        let config = PythonConfig::new(&db, None, None);
        assert!(config.interpreter(&db).is_none());

        config
            .set_interpreter(&mut db)
            .to(Some("/usr/bin/python3".to_string()));
        assert_eq!(config.interpreter(&db).as_deref(), Some("/usr/bin/python3"));
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
        let config = PythonConfig::new(&db, Some(workspace.to_string_lossy().to_string()), None);
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
        let config = PythonConfig::new(&db, Some(workspace.to_string_lossy().to_string()), None);
        let target = TargetString::new(&db, "nonexistent.Module".to_string());
        let result = cached_definition_info(&db, config, target);
        assert!(result.get().is_err());
    }

    #[test]
    fn test_cached_definition_info_cache_hit() {
        let db = real_fs_db();
        let workspace = examples_dir();
        let config = PythonConfig::new(&db, Some(workspace.to_string_lossy().to_string()), None);
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
        let config = PythonConfig::new(&db, Some(workspace.to_string_lossy().to_string()), None);
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
        let config = PythonConfig::new(&db, Some(workspace.to_string_lossy().to_string()), None);

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
        let config = PythonConfig::new(&db, Some(workspace.to_string_lossy().to_string()), None);
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
    fn test_site_packages_pth_root_invalidation_on_file_create() {
        use ruff_db::files::Files;
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let site_packages = tmp.path().join("site-packages");
        let editable_src = tmp.path().join("src");
        fs::create_dir_all(&site_packages).expect("create site-packages dir");
        fs::create_dir_all(&editable_src).expect("create editable src dir");

        let mut db = real_fs_db();
        let site_packages_sys = SystemPath::from_std_path(&site_packages).unwrap();

        // Register the site-packages directory as a `FileRoot`, exactly as
        // `site_packages_paths` does in production. `parse_pth_files` then reads
        // this root's revision, so bumping it invalidates the directory scan.
        db.files()
            .try_add_root(&db, site_packages_sys, FileRootKind::LibrarySearchPath);

        let site_packages_str = site_packages.to_string_lossy().to_string();
        let result1 = {
            let site_packages_key = SitePackagesPath::new(&db, site_packages_str.clone());
            tracked_pth_paths(&db, site_packages_key).clone()
        };
        assert!(result1.is_empty());

        // Create a `.pth` file and bump the enclosing root, exactly as
        // `did_change_watched_files` does on a `.pth` CREATE event.
        let pth_file = site_packages.join("editable_package.pth");
        fs::write(&pth_file, "../src\n").expect("write pth");
        Files::touch_root(&mut db, site_packages_sys);

        let result2 = {
            let site_packages_key = SitePackagesPath::new(&db, site_packages_str);
            tracked_pth_paths(&db, site_packages_key).clone()
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

    // ---- Backdating regression tests for the recursive MRO queries ----
    //
    // `class_parent_docs` / `class_parent_attribute` are recursive, so each query
    // is its own memoised dependent. Their result types must compare by *value*
    // or salsa can never backdate, and any edit to a file in an MRO chain re-runs the
    // entire chain even when the resolved result is identical. These tests lock in
    // value equality.

    /// Minimal `FunctionSignature` with `param_count` positional parameters.
    fn test_sig(name: &str, param_count: usize) -> FunctionSignature {
        FunctionSignature {
            name: name.to_string(),
            parameters: (0..param_count)
                .map(|i| crate::python_analyzer::ParameterInfo {
                    name: format!("p{i}"),
                    type_annotation: None,
                    default_value: None,
                    has_default: false,
                    is_variadic: false,
                    is_variadic_keyword: false,
                    is_keyword_only: false,
                })
                .collect(),
            return_type: None,
            docstring: None,
            start_line: 1,
            start_column: 1,
            end_line: 2,
            end_column: 1,
        }
    }

    #[test]
    fn test_class_parent_docs_value_equality() {
        // Two independently-allocated results with identical contents must be
        // equal so salsa can backdate. Under the old `Arc::ptr_eq` impl these
        // were always `!=`.
        let a = ClassParentDocs::new(Some("doc".to_string()), Some(test_sig("__init__", 1)));
        let b = ClassParentDocs::new(Some("doc".to_string()), Some(test_sig("__init__", 1)));
        assert_eq!(a, b, "equal contents must compare equal (value equality)");

        // Differing contents must compare unequal — guards against false
        // negatives (failing to invalidate on a real change).
        let diff_doc =
            ClassParentDocs::new(Some("other".to_string()), Some(test_sig("__init__", 1)));
        assert_ne!(a, diff_doc, "different docstring must compare unequal");
        let diff_init =
            ClassParentDocs::new(Some("doc".to_string()), Some(test_sig("__init__", 2)));
        assert_ne!(
            a, diff_init,
            "different __init__ signature must compare unequal"
        );
    }

    #[test]
    fn test_cached_class_attribute_value_equality() {
        let attr = |v: &str| ClassAttributeInfo {
            name: "factory".to_string(),
            value: v.to_string(),
        };
        let a = CachedClassAttribute::found(attr("Foo"), PathBuf::from("/p.py"), "C".to_string());
        let b = CachedClassAttribute::found(attr("Foo"), PathBuf::from("/p.py"), "C".to_string());
        assert_eq!(a, b, "equal contents must compare equal (value equality)");

        // Each component participates in equality (no false negatives).
        let diff_value =
            CachedClassAttribute::found(attr("Bar"), PathBuf::from("/p.py"), "C".to_string());
        assert_ne!(
            a, diff_value,
            "different attribute value must compare unequal"
        );
        let diff_path =
            CachedClassAttribute::found(attr("Foo"), PathBuf::from("/q.py"), "C".to_string());
        assert_ne!(a, diff_path, "different file path must compare unequal");

        let n1 = CachedClassAttribute::not_found();
        let n2 = CachedClassAttribute::not_found();
        assert_eq!(n1, n2, "two not-found results must compare equal");
        assert_ne!(a, n1, "found vs not-found must compare unequal");
    }

    /// Write a 3-file `Child -> Parent -> GrandParent` workspace and return the
    /// file paths. `gp_body` is the body of the GrandParent class file.
    fn write_mro_workspace(workspace: &Path, gp_body: &str) -> (PathBuf, PathBuf, PathBuf) {
        use filetime::{FileTime, set_file_mtime};
        use std::fs;

        let gp = workspace.join("gp.py");
        fs::write(&gp, gp_body).expect("write gp");
        let parent = workspace.join("parent.py");
        fs::write(
            &parent,
            "from gp import GrandParent\n\n\nclass Parent(GrandParent):\n    pass\n",
        )
        .expect("write parent");
        let child = workspace.join("child.py");
        fs::write(
            &child,
            "from parent import Parent\n\n\nclass Child(Parent):\n    pass\n",
        )
        .expect("write child");
        for f in [&gp, &parent, &child] {
            set_file_mtime(f, FileTime::from_unix_time(1_000_000, 0)).expect("set mtime");
        }
        (gp, parent, child)
    }

    #[test]
    fn test_class_parent_docs_backdates_on_irrelevant_ancestor_edit() {
        use filetime::{FileTime, set_file_mtime};
        use ruff_db::files::File;
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path();
        let gp_v1 = "class GrandParent:\n    \"\"\"gp docstring\"\"\"\n    def __init__(self, a):\n        pass\n";
        let (gp, _parent, child) = write_mro_workspace(workspace, gp_v1);

        let mut db = real_fs_db();

        let r1 = {
            let sp = InternedSearchPaths::new(&db, vec![workspace.to_path_buf()]);
            // The LSP keys the MRO walk off the lexically-normalized path (the
            // form the client reports on watch events); no symlink resolution.
            let child_lexical = normalize_path_for_key(&db, &child);
            let child_key = TargetString::new(&db, format!("{}::Child", child_lexical.display()));
            class_parent_docs(&db, child_key, sp)
        };
        // Prove the MRO walk really resolved through to GrandParent — otherwise
        // a trivially-empty result could make this test pass for the wrong reason.
        assert_eq!(
            r1.docstring().map(String::as_str),
            Some("gp docstring"),
            "Child must inherit GrandParent's docstring via the MRO"
        );
        assert_eq!(
            r1.init().map(|s| s.parameters.len()),
            Some(2),
            "Child must inherit GrandParent's __init__(self, a) — 2 params incl. self"
        );

        // Append an unrelated function AFTER the class so GrandParent's own
        // docstring and __init__ spans are byte-for-byte unchanged.
        fs::write(&gp, format!("{gp_v1}\n\ndef unrelated():\n    pass\n")).expect("write gp v2");
        set_file_mtime(&gp, FileTime::from_unix_time(2_000_000, 0)).expect("bump gp mtime");
        // The MRO queries open GrandParent via its lexically-normalized path, so
        // the sync must target that same (lexical) path — exactly as the LSP
        // client reports it on a `did_change_watched_files` event.
        let gp_lexical = normalize_path_for_key(&db, &gp);
        File::sync_path(&mut db, SystemPath::from_std_path(&gp_lexical).unwrap());

        let r2 = {
            let sp = InternedSearchPaths::new(&db, vec![workspace.to_path_buf()]);
            // The LSP keys the MRO walk off the lexically-normalized path (the
            // form the client reports on watch events); no symlink resolution.
            let child_lexical = normalize_path_for_key(&db, &child);
            let child_key = TargetString::new(&db, format!("{}::Child", child_lexical.display()));
            class_parent_docs(&db, child_key, sp)
        };

        // Backdating: GrandParent re-executed but produced a value-equal result,
        // so its `changed_at` did not advance and the Parent/Child memos were
        // never re-run — the Child result is the *same* cached Arc.
        assert!(
            Arc::ptr_eq(&r1.inner, &r2.inner),
            "an irrelevant ancestor edit must backdate; the Child result must \
             not be recomputed (this fails under Arc::ptr_eq equality)"
        );
    }

    #[test]
    fn test_class_parent_docs_invalidates_on_relevant_ancestor_edit() {
        use filetime::{FileTime, set_file_mtime};
        use ruff_db::files::File;
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path();
        let (gp, _parent, child) = write_mro_workspace(
            workspace,
            "class GrandParent:\n    \"\"\"gp docstring\"\"\"\n    def __init__(self, a):\n        pass\n",
        );

        let mut db = real_fs_db();
        let r1 = {
            let sp = InternedSearchPaths::new(&db, vec![workspace.to_path_buf()]);
            // The LSP keys the MRO walk off the lexically-normalized path (the
            // form the client reports on watch events); no symlink resolution.
            let child_lexical = normalize_path_for_key(&db, &child);
            let child_key = TargetString::new(&db, format!("{}::Child", child_lexical.display()));
            class_parent_docs(&db, child_key, sp)
        };
        assert_eq!(r1.init().map(|s| s.parameters.len()), Some(2));

        // Genuinely change GrandParent's __init__ signature: the inherited
        // result MUST update (no false negative).
        fs::write(
            &gp,
            "class GrandParent:\n    \"\"\"gp docstring\"\"\"\n    def __init__(self, a, b):\n        pass\n",
        )
        .expect("write gp v2");
        set_file_mtime(&gp, FileTime::from_unix_time(2_000_000, 0)).expect("bump gp mtime");
        // The MRO queries open GrandParent via its lexically-normalized path, so
        // the sync must target that same (lexical) path — exactly as the LSP
        // client reports it on a `did_change_watched_files` event.
        let gp_lexical = normalize_path_for_key(&db, &gp);
        File::sync_path(&mut db, SystemPath::from_std_path(&gp_lexical).unwrap());

        let r2 = {
            let sp = InternedSearchPaths::new(&db, vec![workspace.to_path_buf()]);
            // The LSP keys the MRO walk off the lexically-normalized path (the
            // form the client reports on watch events); no symlink resolution.
            let child_lexical = normalize_path_for_key(&db, &child);
            let child_key = TargetString::new(&db, format!("{}::Child", child_lexical.display()));
            class_parent_docs(&db, child_key, sp)
        };
        assert_eq!(
            r2.init().map(|s| s.parameters.len()),
            Some(3),
            "a real __init__ change must propagate through the MRO"
        );
        assert!(r1 != r2, "result must differ after a meaningful change");
    }

    #[test]
    fn test_class_parent_attribute_backdates_on_irrelevant_ancestor_edit() {
        use filetime::{FileTime, set_file_mtime};
        use ruff_db::files::File;
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path();
        let gp_v1 = "class GrandParent:\n    factory = SomeFactory\n";
        let (gp, _parent, child) = write_mro_workspace(workspace, gp_v1);

        let mut db = real_fs_db();
        let r1 = {
            let sp = InternedSearchPaths::new(&db, vec![workspace.to_path_buf()]);
            // The LSP keys the MRO walk off the lexically-normalized path (the
            // form the client reports on watch events); no symlink resolution.
            let child_lexical = normalize_path_for_key(&db, &child);
            let child_key = TargetString::new(&db, format!("{}::Child", child_lexical.display()));
            let attr_key = TargetString::new(&db, "factory".to_string());
            class_parent_attribute(&db, child_key, attr_key, sp)
        };
        assert_eq!(
            r1.get().map(|(a, _, _)| a.value.as_str()),
            Some("SomeFactory"),
            "Child must inherit GrandParent's `factory` attribute via the MRO"
        );

        // Append unrelated content; the `factory` assignment is unchanged.
        fs::write(&gp, format!("{gp_v1}\n\ndef unrelated():\n    pass\n")).expect("write gp v2");
        set_file_mtime(&gp, FileTime::from_unix_time(2_000_000, 0)).expect("bump gp mtime");
        // The MRO queries open GrandParent via its lexically-normalized path, so
        // the sync must target that same (lexical) path — exactly as the LSP
        // client reports it on a `did_change_watched_files` event.
        let gp_lexical = normalize_path_for_key(&db, &gp);
        File::sync_path(&mut db, SystemPath::from_std_path(&gp_lexical).unwrap());

        let r2 = {
            let sp = InternedSearchPaths::new(&db, vec![workspace.to_path_buf()]);
            // The LSP keys the MRO walk off the lexically-normalized path (the
            // form the client reports on watch events); no symlink resolution.
            let child_lexical = normalize_path_for_key(&db, &child);
            let child_key = TargetString::new(&db, format!("{}::Child", child_lexical.display()));
            let attr_key = TargetString::new(&db, "factory".to_string());
            class_parent_attribute(&db, child_key, attr_key, sp)
        };
        assert!(
            Arc::ptr_eq(&r1.inner, &r2.inner),
            "an irrelevant ancestor edit must backdate the attribute lookup \
             (this fails under Arc::ptr_eq equality)"
        );
    }

    #[test]
    fn test_class_parent_attribute_invalidates_on_relevant_ancestor_edit() {
        use filetime::{FileTime, set_file_mtime};
        use ruff_db::files::File;
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path();
        let (gp, _parent, child) =
            write_mro_workspace(workspace, "class GrandParent:\n    factory = SomeFactory\n");

        let mut db = real_fs_db();
        let r1 = {
            let sp = InternedSearchPaths::new(&db, vec![workspace.to_path_buf()]);
            // The LSP keys the MRO walk off the lexically-normalized path (the
            // form the client reports on watch events); no symlink resolution.
            let child_lexical = normalize_path_for_key(&db, &child);
            let child_key = TargetString::new(&db, format!("{}::Child", child_lexical.display()));
            let attr_key = TargetString::new(&db, "factory".to_string());
            class_parent_attribute(&db, child_key, attr_key, sp)
        };
        assert_eq!(
            r1.get().map(|(a, _, _)| a.value.as_str()),
            Some("SomeFactory")
        );

        // Change the attribute's value: the inherited result MUST update.
        fs::write(&gp, "class GrandParent:\n    factory = OtherFactory\n").expect("write gp v2");
        set_file_mtime(&gp, FileTime::from_unix_time(2_000_000, 0)).expect("bump gp mtime");
        // The MRO queries open GrandParent via its lexically-normalized path, so
        // the sync must target that same (lexical) path — exactly as the LSP
        // client reports it on a `did_change_watched_files` event.
        let gp_lexical = normalize_path_for_key(&db, &gp);
        File::sync_path(&mut db, SystemPath::from_std_path(&gp_lexical).unwrap());

        let r2 = {
            let sp = InternedSearchPaths::new(&db, vec![workspace.to_path_buf()]);
            // The LSP keys the MRO walk off the lexically-normalized path (the
            // form the client reports on watch events); no symlink resolution.
            let child_lexical = normalize_path_for_key(&db, &child);
            let child_key = TargetString::new(&db, format!("{}::Child", child_lexical.display()));
            let attr_key = TargetString::new(&db, "factory".to_string());
            class_parent_attribute(&db, child_key, attr_key, sp)
        };
        assert_eq!(
            r2.get().map(|(a, _, _)| a.value.as_str()),
            Some("OtherFactory"),
            "a real attribute change must propagate through the MRO"
        );
        assert!(r1 != r2, "result must differ after a meaningful change");
    }

    /// When the workspace is reached through a symlinked path (the path the LSP client
    /// reports), a real ancestor edit must still invalidate the MRO memo.
    #[cfg(unix)]
    #[test]
    fn test_class_parent_docs_invalidates_via_symlinked_workspace() {
        use filetime::{FileTime, set_file_mtime};
        use ruff_db::files::File;
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let real_ws = tmp.path().join("real");
        fs::create_dir_all(&real_ws).expect("create real workspace");
        let (gp, _parent, _child) = write_mro_workspace(
            &real_ws,
            "class GrandParent:\n    \"\"\"gp docstring\"\"\"\n    def __init__(self, a):\n        pass\n",
        );

        // Access the same tree through a symlink — this is the lexical path the
        // editor/client would report, distinct from the canonical `real/` path.
        let link_ws = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_ws, &link_ws).expect("create workspace symlink");
        // Sanity: the symlink path really is non-canonical.
        assert_ne!(
            link_ws,
            link_ws.canonicalize().expect("canonicalize link"),
            "symlink path must differ from its canonical target for this test to be meaningful"
        );

        let mut db = real_fs_db();
        let child_via_link = link_ws.join("child.py");
        let query = |db: &HydraDatabase| {
            let sp = InternedSearchPaths::new(db, vec![link_ws.clone()]);
            let child_lexical = normalize_path_for_key(db, &child_via_link);
            let child_key = TargetString::new(db, format!("{}::Child", child_lexical.display()));
            class_parent_docs(db, child_key, sp)
        };

        let r1 = query(&db);
        assert_eq!(
            r1.init().map(|s| s.parameters.len()),
            Some(2),
            "Child must inherit GrandParent's __init__(self, a) via the symlinked workspace"
        );

        // Genuinely change GrandParent's __init__, then sync the *lexical*
        // symlink path (as the client reports on a watch event).
        fs::write(
            &gp,
            "class GrandParent:\n    \"\"\"gp docstring\"\"\"\n    def __init__(self, a, b):\n        pass\n",
        )
        .expect("write gp v2");
        set_file_mtime(&gp, FileTime::from_unix_time(2_000_000, 0)).expect("bump gp mtime");
        let gp_via_link = normalize_path_for_key(&db, &link_ws.join("gp.py"));
        File::sync_path(&mut db, SystemPath::from_std_path(&gp_via_link).unwrap());

        let r2 = query(&db);
        assert_eq!(
            r2.init().map(|s| s.parameters.len()),
            Some(3),
            "a real __init__ change must invalidate the MRO memo even when the \
             workspace is reached through a symlink"
        );
    }

    #[test]
    fn test_resolve_module_invalidation_on_file_create() {
        use ruff_db::files::File;
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path();

        let mut db = real_fs_db();
        let search_paths = vec![workspace.to_path_buf()];

        // First lookup: the module file does not exist yet.
        let result1 = {
            let mid = TargetString::new(&db, "new_module".to_string());
            let spid = InternedSearchPaths::new(&db, search_paths.clone());
            resolve_module_cached(&db, mid, spid).clone()
        };
        assert!(
            result1.is_none(),
            "module should not resolve before creation"
        );

        // Create the module file on disk and notify salsa, as the LSP does on a
        // did_change_watched_files CREATE event.
        // Sync the same (lexical, un-canonicalized) path the resolver probes
        // and that the LSP passes to `File::sync_path`.
        let py_file = workspace.join("new_module.py");
        fs::write(&py_file, "class NewClass:\n    pass\n").expect("write module");
        File::sync_path(&mut db, SystemPath::from_std_path(&py_file).unwrap());

        // Second lookup: the create must now be observed.
        let result2 = {
            let mid = TargetString::new(&db, "new_module".to_string());
            let spid = InternedSearchPaths::new(&db, search_paths);
            resolve_module_cached(&db, mid, spid).clone()
        };
        assert!(
            result2
                .as_ref()
                .is_some_and(|p| p.ends_with("new_module.py")),
            "module must resolve after creation + sync_path, got {result2:?}"
        );
    }

    /// The module name reaching this query is the `_target_` string a user
    /// typed into their YAML, so segments such as `..` or an absolute path must
    /// not make it probe files outside the search roots.
    #[test]
    fn test_resolve_module_cached_cannot_escape_the_search_roots() {
        use ruff_db::system::DbWithWritableSystem;

        // Fixed in-memory paths: a real temp directory can carry a dot in its
        // prefix, which would split the absolute name below into parts and
        // quietly make this test prove nothing.
        let mut db = TestDb::new();
        db.write_file("/root/__init__.py", "class Thing:\n    pass\n")
            .expect("write root init");
        db.write_file("/root/inside.py", "class Thing:\n    pass\n")
            .expect("write inside");
        db.write_file("/outside.py", "class Thing:\n    pass\n")
            .expect("write outside");
        db.write_file("/root/pkg/sub.py", "class Thing:\n    pass\n")
            .expect("write sub");

        let search_paths = vec![PathBuf::from("/root")];
        let resolve = |module: &str| {
            let mid = TargetString::new(&db, module.to_string());
            let spid = InternedSearchPaths::new(&db, search_paths.clone());
            resolve_module_cached(&db, mid, spid).clone()
        };

        // Positive control: a guard that rejected everything would fail here.
        assert_eq!(
            resolve("inside"),
            Some(PathBuf::from("/root/inside.py")),
            "a plain module name must still resolve"
        );

        // `/outside.py` exists, so this fails without the guard.
        assert_eq!(
            resolve("/outside"),
            None,
            "an absolute name must not replace the search root"
        );

        // `_target_: ".Thing"` splits to an empty module name, and
        // `/root/__init__.py` exists, so this fails without the dot-only guard.
        assert_eq!(
            resolve(""),
            None,
            "an empty name must not resolve to the search root itself"
        );
        assert_eq!(resolve("."), None, "nor must a dot-only name");

        // A leading or doubled dot leaves an empty part, which must not be
        // skipped: `/root/inside.py` exists, so both resolve without the guard.
        assert_eq!(
            resolve("..inside"),
            None,
            "a leading-dot name must not resolve as if the dots were absent"
        );
        assert_eq!(
            resolve("pkg..sub"),
            None,
            "nor must a name with a doubled dot"
        );
    }
}
