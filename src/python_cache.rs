use std::path::PathBuf;
use std::sync::Arc;

use tracing::debug;

use crate::python_analyzer::{DefinitionInfo, PythonAnalyzer};

/// Salsa input for Python environment configuration.
///
/// Created once when the LSP backend starts. Updated when:
/// - Python interpreter setting changes (via initialization options)
/// - Workspace root is determined (from LSP init params or cwd)
///
/// Changes to this input invalidate all cached definition lookups,
/// triggering re-resolution of modules on the next request.
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

/// Cached extraction of Python definition info for a `_target_` string.
///
/// Wraps `PythonAnalyzer::extract_definition_info` with salsa caching.
/// The result is cached and only recomputed when:
/// - The target string changes (different `_target_` value)
/// - The Python configuration changes (interpreter or workspace root)
///
/// Uses `lru=200` to bound memory for workspaces with many distinct targets.
#[salsa::tracked(no_eq, lru = 200)]
pub fn cached_definition_info<'db>(
    db: &'db dyn salsa::Database,
    config: PythonConfig,
    target: TargetString<'db>,
) -> CachedDefinitionResult {
    let target_str = target.value(db);
    debug!(target = target_str, "cached_definition_info: executing (cache miss)");

    let workspace_root = config
        .workspace_root(db)
        .as_deref()
        .map(PathBuf::from);
    let interpreter = config.interpreter(db);

    CachedDefinitionResult::from_result(PythonAnalyzer::extract_definition_info(
        target_str,
        workspace_root.as_deref().map(std::path::Path::new),
        interpreter.as_deref(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::tests::TestDb;
    use salsa::Setter;

    fn examples_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/workspace/simple")
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
        assert_eq!(
            config.interpreter(&db).as_deref(),
            Some("/usr/bin/python3")
        );
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
        let db = TestDb::new();
        let workspace = examples_dir();
        let config = PythonConfig::new(
            &db,
            Some(workspace.to_string_lossy().to_string()),
            None,
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
        );
        let target = TargetString::new(&db, "nonexistent.Module".to_string());
        let result = cached_definition_info(&db, config, target);
        assert!(result.get().is_err());
    }

    #[test]
    fn test_cached_definition_info_cache_hit() {
        let db = TestDb::new();
        let workspace = examples_dir();
        let config = PythonConfig::new(
            &db,
            Some(workspace.to_string_lossy().to_string()),
            None,
        );
        let target = TargetString::new(&db, "my_module.DataLoader".to_string());

        let result1 = cached_definition_info(&db, config, target);
        let result2 = cached_definition_info(&db, config, target);

        // Same input, no changes — should be pointer-equal (cache hit)
        assert!(result1 == result2);
    }

    #[test]
    fn test_cached_definition_info_function_target() {
        let db = TestDb::new();
        let workspace = examples_dir();
        let config = PythonConfig::new(
            &db,
            Some(workspace.to_string_lossy().to_string()),
            None,
        );
        let target = TargetString::new(&db, "my_module.create_model".to_string());
        let result = cached_definition_info(&db, config, target);

        let def = result.get().expect("should resolve create_model");
        assert_eq!(def.symbol_name, "create_model");
        assert!(matches!(def.definition_info, DefinitionInfo::Function(_)));
    }

    #[test]
    fn test_cached_definition_info_invalidation_on_config_change() {
        let mut db = TestDb::new();
        let workspace = examples_dir();
        let config = PythonConfig::new(
            &db,
            Some(workspace.to_string_lossy().to_string()),
            None,
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
