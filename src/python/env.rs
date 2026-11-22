use anyhow::Result;
use ruff_db::system::{OsSystem, SystemPath, SystemPathBuf};
use ty_python_semantic::{PythonEnvironment, PythonVersionWithSource, SysPrefixPathOrigin};

/// What discovery found out about the interpreter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct DiscoveredEnvironment {
    pub(super) site_packages: Vec<SystemPathBuf>,
    /// The latest version ty knows, with a `Default` source, when neither
    /// `pyvenv.cfg` nor the site-packages layout names it.
    pub(super) python_version: PythonVersionWithSource,
}

/// Discover the Python environment, its site-packages paths and its version.
///
/// This uses `ty` Python environment discovery with the following priority:
/// 1. Configured Python interpreter from LSP initialization (highest priority)
/// 2. Activated virtual environment (VIRTUAL_ENV)
/// 3. Active child Conda or Pixi environment (CONDA_PREFIX)
/// 4. Working directory virtual environment (.venv)
/// 5. Active base Conda environment
/// 6. A Python interpreter on PATH
///
/// The configured interpreter is always preferred if provided, as it represents
/// the user's explicit choice via LSP configuration.
pub(super) fn discover_python_environment(
    workspace_root: &SystemPath,
    interpreter: Option<&SystemPath>,
) -> Result<DiscoveredEnvironment> {
    let system = OsSystem::new(workspace_root);

    let env = if let Some(interpreter) = interpreter {
        // User provided a specific Python path (highest priority - from LSP configuration)
        // This could be an executable or sys.prefix directory
        PythonEnvironment::new(interpreter, SysPrefixPathOrigin::Editor, &system)?
    } else {
        // Auto-discover Python environment with built-in priority:
        // 1. Activated virtual environment (VIRTUAL_ENV)
        // 2. Active child Conda or Pixi environment (CONDA_PREFIX)
        // 3. Working directory virtual environment (.venv)
        // 4. Active base Conda environment
        // 5. A Python interpreter on PATH
        PythonEnvironment::discover(Some(workspace_root), &system)?
            .ok_or_else(|| anyhow::anyhow!("no Python environment found"))?
    };

    let site_packages_paths = env.site_packages_paths(&system)?;
    // The same order ty uses: the venv's own metadata, then the layout.
    let python_version = env
        .python_version_from_metadata()
        .cloned()
        .or_else(|| site_packages_paths.python_version_from_layout())
        .unwrap_or_default();

    Ok(DiscoveredEnvironment {
        site_packages: site_packages_paths.into_vec(),
        python_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::HydraDatabase;
    use crate::python::cache::{
        PythonConfig, resolve_module_by_name, resolver_environment, resolver_environment_from_parts,
    };
    use crate::python::extract::extract_class_info;
    use ruff_python_ast::PythonVersion;
    use std::path::PathBuf;
    use ty_module_resolver::ResolverEnvironment;
    use ty_python_semantic::PythonVersionSource;

    use crate::python::test_support::{
        canonicalize, extract_definition_info, get_simple_test_dir, system_path, test_db,
    };

    // ==================== Python Interpreter Priority Tests ====================
    // These tests verify that the configured Python interpreter from LSP initialization
    // takes priority over auto-discovery

    /// Resolve `module` with `examples_dir` as the workspace and `interpreter`
    /// configured, the way the server builds its environment.
    fn resolve_with_interpreter(interpreter: Option<&str>, module: &str) -> Option<PathBuf> {
        let examples_dir = get_simple_test_dir();
        let db = test_db();
        let config = PythonConfig::new(
            &db,
            system_path(&examples_dir),
            interpreter.map(SystemPathBuf::from),
        );
        resolve_module_by_name(&db, module, resolver_environment(&db, config))
    }

    fn system_python() -> PathBuf {
        which::which("python3")
            .or_else(|_| which::which("python"))
            .expect("Python not found in PATH")
    }

    #[test]
    #[ignore = "Requires Python in PATH"]
    fn test_configured_interpreter_takes_priority() {
        let python = system_python();
        assert!(
            resolve_with_interpreter(python.to_str(), "my_module").is_some(),
            "Module resolution should succeed with configured interpreter"
        );
    }

    #[test]
    fn test_auto_discovery_when_no_interpreter_configured() {
        assert!(
            resolve_with_interpreter(None, "my_module").is_some(),
            "Module resolution should succeed with auto-discovery"
        );
    }

    #[test]
    #[ignore = "Requires Python in PATH"]
    fn test_configured_interpreter_reports_its_version() {
        let python = system_python();
        let cwd = system_path(&std::env::current_dir().unwrap());
        let discovered = discover_python_environment(&cwd, Some(&system_path(&python)))
            .expect("discover the interpreter");
        assert!(
            discovered.python_version.source != PythonVersionSource::Default,
            "a real interpreter's version should be discoverable, got {discovered:?}"
        );
    }

    #[test]
    fn test_extract_definition_without_configured_interpreter() {
        // Test that definition extraction works without configured interpreter (auto-discovery)
        let examples_dir = get_simple_test_dir();

        // Extract definition without configured interpreter
        let result = extract_definition_info(
            &test_db(),
            "my_module.simple_function",
            &[examples_dir.clone(), PathBuf::from(".")],
        );

        assert!(
            result.is_ok(),
            "Definition extraction should work with auto-discovery: {:?}",
            result.err()
        );
    }

    // ==================== editable installs ====================

    fn get_editable_test_dir() -> PathBuf {
        crate::python::test_support::fixture_dir("editable")
    }

    /// The editable fixture's workspace and site-packages, as a resolver
    /// environment.
    fn editable_environment(db: &HydraDatabase) -> ResolverEnvironment<'_> {
        let workspace_dir = get_editable_test_dir();
        let site_packages = workspace_dir.join("site-packages");
        resolver_environment_from_parts(
            db,
            vec![SystemPathBuf::from_path_buf(workspace_dir).unwrap()],
            vec![SystemPathBuf::from_path_buf(site_packages).unwrap()],
            PythonVersion::latest_ty(),
        )
    }

    #[test]
    fn test_pth_file_lists_the_editable_root() {
        let db = test_db();
        let environment = editable_environment(&db);

        let editables: Vec<_> = ty_module_resolver::editable_search_paths(&db, environment)
            .map(|path| path.as_std_path().to_path_buf())
            .collect();

        assert_eq!(
            editables,
            vec![canonicalize(&get_editable_test_dir().join("src"))],
        );
    }

    #[test]
    fn test_missing_site_packages_has_no_editables() {
        let db = test_db();
        let environment = resolver_environment_from_parts(
            &db,
            Vec::new(),
            vec![SystemPathBuf::from("/nonexistent/path/site-packages")],
            PythonVersion::latest_ty(),
        );
        assert_eq!(
            ty_module_resolver::editable_search_paths(&db, environment).count(),
            0
        );
    }

    #[test]
    fn test_resolve_editable_module() {
        let db = test_db();
        let result = resolve_module_by_name(&db, "editable_package.lib", editable_environment(&db));

        let path = result.expect("Should resolve editable_package.lib");
        assert!(
            path.ends_with("lib.py"),
            "Should resolve to lib.py: {:?}",
            path
        );
    }

    #[test]
    fn test_extract_definition_from_editable_package() {
        let db = test_db();
        let module_path =
            resolve_module_by_name(&db, "editable_package.lib", editable_environment(&db))
                .expect("Should resolve module");

        // Extract the class info (salsa-cached)
        let class_info = extract_class_info(
            &db,
            &module_path,
            "EditableModel",
            PythonVersion::latest_ty(),
        )
        .expect("Should extract class info");

        assert_eq!(class_info.name, "EditableModel");
        assert!(class_info.docstring.is_some());
        assert!(class_info.init_signature.is_some());

        let init_sig = class_info.init_signature.as_ref().unwrap();
        // Should have self, input_size, output_size
        assert_eq!(init_sig.parameters.len(), 3);
        assert_eq!(init_sig.parameters[1].name, "input_size");
        assert_eq!(init_sig.parameters[2].name, "output_size");
    }
}
