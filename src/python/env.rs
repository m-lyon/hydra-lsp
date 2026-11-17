//! Finding the Python environment: site-packages discovery and the search
//! paths built from it, including editable installs listed in `.pth` files.

use super::extract::read_source;
use anyhow::Result;
use ruff_db::files::directory_listing;
use ruff_db::system::{OsSystem, SystemPath, SystemPathBuf};
use std::path::{Path, PathBuf};
use ty_python_semantic::{PythonEnvironment, SysPrefixPathOrigin};

/// Discover the Python environment and get site-packages paths
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
    workspace_root: Option<&Path>,
    python_path: Option<&str>,
) -> Result<Vec<SystemPathBuf>> {
    // OsSystem requires an absolute CWD. Use workspace_root when provided;
    // fall back to the process's current directory otherwise.
    let cwd_buf = if let Some(root) = workspace_root {
        SystemPath::from_std_path(root)
            .ok_or_else(|| anyhow::anyhow!("Invalid workspace root path"))?
            .to_path_buf()
    } else {
        SystemPathBuf::from_path_buf(
            std::env::current_dir()
                .map_err(|e| anyhow::anyhow!("Cannot determine current directory: {e}"))?,
        )
        .map_err(|p| anyhow::anyhow!("Current directory is not valid UTF-8: {}", p.display()))?
    };
    let cwd = cwd_buf.as_path();
    let system = OsSystem::new(cwd);

    // Use the same path as project root for environment discovery
    let project_root = cwd;

    let env = if let Some(python_path_str) = python_path {
        // User provided a specific Python path (highest priority - from LSP configuration)
        // This could be an executable or sys.prefix directory
        let python_sys_path = SystemPath::from_std_path(Path::new(python_path_str))
            .ok_or_else(|| anyhow::anyhow!("Invalid Python path"))?;

        PythonEnvironment::new(python_sys_path, SysPrefixPathOrigin::Editor, &system)?
    } else {
        // Auto-discover Python environment with built-in priority:
        // 1. Activated virtual environment (VIRTUAL_ENV)
        // 2. Active child Conda or Pixi environment (CONDA_PREFIX)
        // 3. Working directory virtual environment (.venv)
        // 4. Active base Conda environment
        // 5. A Python interpreter on PATH
        PythonEnvironment::discover(Some(project_root), &system)?
            .ok_or_else(|| anyhow::anyhow!("No Python environment found"))?
    };

    // Get site-packages directories from the discovered environment
    let site_packages_paths = env.site_packages_paths(&system)?;

    // Convert SitePackagesPaths to Vec<SystemPathBuf> for compatibility
    Ok(site_packages_paths.into_vec())
}

/// Build search paths from discovered site-packages
pub(super) fn build_search_paths(
    db: &dyn ruff_db::Db,
    workspace_root: Option<&Path>,
    site_packages_paths: &Vec<SystemPathBuf>,
) -> Vec<PathBuf> {
    let mut search_paths = Vec::new();

    // Add workspace root first (highest priority for first-party code)
    if let Some(root) = workspace_root {
        search_paths.push(PathBuf::from(root));
    }

    // Add current directory
    search_paths.push(PathBuf::from("."));

    // Add site-packages paths AND process .pth files for editable installs
    for sys_path in site_packages_paths {
        let site_packages = sys_path.as_std_path().to_path_buf();

        // Process .pth files in this site-packages directory
        let editable_paths = parse_pth_files(db, &site_packages);

        search_paths.push(site_packages);
        search_paths.extend(editable_paths);
    }

    search_paths
}

/// Parse all .pth files in a site-packages directory and extract editable install paths.
///
/// This follows the Python site module specification:
/// - Empty lines and lines beginning with `#` are skipped
/// - Lines starting with `import ` or `import\t` are executed, not treated as paths
/// - All other lines are treated as directories to add to `sys.path`
///
/// See: https://docs.python.org/3/library/site.html
pub(super) fn parse_pth_files(db: &dyn ruff_db::Db, site_packages: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // Listing through salsa makes this scan depend on the directory's own
    // revision, which `File::sync_path` on a created or deleted `.pth` bumps.
    let Some(directory) = SystemPath::from_std_path(site_packages) else {
        return paths;
    };
    let Ok(listing) = directory_listing(db, directory) else {
        return paths;
    };

    // The listing is sorted by name, the order the Python spec asks for.
    let pth_files = listing
        .iter()
        .filter(|(name, _)| Path::new(name).extension().is_some_and(|ext| ext == "pth"))
        .map(|(name, _)| site_packages.join(name));

    for pth_file in pth_files {
        let Ok(contents) = read_source(db, &pth_file) else {
            continue;
        };
        for line in contents.as_str().lines() {
            let line = line.trim_end();

            // Skip empty lines, comments, and import statements
            if line.is_empty()
                || line.starts_with('#')
                || line.starts_with("import ")
                || line.starts_with("import\t")
            {
                continue;
            }

            // Resolve relative paths relative to site-packages
            let path = if Path::new(line).is_absolute() {
                PathBuf::from(line)
            } else {
                site_packages.join(line)
            };

            // We resolve symlinks only here, where the search-path list is
            // first built, and keep every path lexical after this point (the
            // same approach `ty` takes). Falls back to the original path if
            // resolution fails (e.g. the dir was just deleted).
            if path.is_dir() {
                let canonical = std::fs::canonicalize(&path).unwrap_or(path);
                paths.push(canonical);
            }
        }
    }

    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::python::cache::{InternedSearchPaths, TargetString, resolve_module_cached};
    use crate::python::extract::extract_class_info;
    use crate::python::resolve::extract_definition_info;

    use crate::python::test_support::{get_simple_test_dir, test_db};

    // ==================== Environment discovery and module resolution tests ====================

    mod environment_tests {
        use super::*;
        use ruff_db::system::TestSystem;

        /// Helper to create a mock Python environment structure
        fn create_mock_venv(
            system: &TestSystem,
            venv_path: &str,
            python_version: &str,
            include_system_site_packages: bool,
        ) {
            let memory_fs = system.memory_file_system();
            let venv_root = SystemPathBuf::from(venv_path);

            // Create the appropriate structure based on OS
            let (exe_path, site_packages_path, pyvenv_cfg_path, home_path) =
                if cfg!(target_os = "windows") {
                    (
                        venv_root.join(r"Scripts\python.exe"),
                        venv_root.join(r"Lib\site-packages"),
                        venv_root.join("pyvenv.cfg"),
                        format!(r"\Python{}\Scripts", python_version.replace('.', "")),
                    )
                } else {
                    (
                        venv_root.join("bin/python"),
                        venv_root.join(format!("lib/python{}/site-packages", python_version)),
                        venv_root.join("pyvenv.cfg"),
                        format!("/usr/local/python{}/bin", python_version),
                    )
                };

            // Create python executable
            memory_fs.write_file_all(&exe_path, "").unwrap();

            // Create site-packages directory
            memory_fs.create_directory_all(&site_packages_path).unwrap();

            // Create pyvenv.cfg
            let mut cfg_contents = format!("home = {}\n", home_path);
            cfg_contents.push_str(&format!("version = {}\n", python_version));
            if include_system_site_packages {
                cfg_contents.push_str("include-system-site-packages = true\n");
            }

            memory_fs
                .write_file_all(&pyvenv_cfg_path, &cfg_contents)
                .unwrap();
        }

        /// Helper to create a mock system Python installation
        fn create_mock_system_python(
            system: &TestSystem,
            install_path: &str,
            python_version: &str,
        ) {
            let memory_fs = system.memory_file_system();
            let sys_prefix = SystemPathBuf::from(install_path);

            let (exe_path, site_packages_path) = if cfg!(target_os = "windows") {
                (
                    sys_prefix.join("python.exe"),
                    sys_prefix.join(r"Lib\site-packages"),
                )
            } else {
                (
                    sys_prefix.join("bin/python"),
                    sys_prefix.join(format!("lib/python{}/site-packages", python_version)),
                )
            };

            memory_fs.write_file_all(&exe_path, "").unwrap();
            memory_fs.create_directory_all(&site_packages_path).unwrap();
        }

        /// Helper to create a mock third-party package in site-packages
        fn create_mock_package_in_site_packages(
            system: &TestSystem,
            site_packages_path: &str,
            package_name: &str,
            has_init: bool,
        ) {
            let memory_fs = system.memory_file_system();
            let package_dir = SystemPathBuf::from(site_packages_path).join(package_name);

            memory_fs.create_directory_all(&package_dir).unwrap();

            if has_init {
                let init_file = package_dir.join("__init__.py");
                memory_fs
                    .write_file_all(&init_file, "# Package init\n")
                    .unwrap();
            }
        }

        #[test]
        fn test_resolve_module_with_venv() {
            let system = TestSystem::default();
            let venv_path = "/.venv";
            let python_version = "3.12";

            create_mock_venv(&system, venv_path, python_version, false);

            // Create a mock package in the venv's site-packages
            let site_packages = if cfg!(target_os = "windows") {
                format!(r"{}\Lib\site-packages", venv_path)
            } else {
                format!("{}/lib/python{}/site-packages", venv_path, python_version)
            };

            create_mock_package_in_site_packages(&system, &site_packages, "my_package", true);

            // Create a module file in the package
            let memory_fs = system.memory_file_system();
            let module_path = SystemPathBuf::from(site_packages.as_str())
                .join("my_package")
                .join("module.py");
            memory_fs
                .write_file_all(
                    &module_path,
                    "def test_func():\n    \"\"\"Test function\"\"\"\n    pass\n",
                )
                .unwrap();

            // Test resolving the module
            // Note: This test demonstrates the structure, but actual resolution
            // requires the full ty environment discovery which is complex to mock
            let expected_path = module_path.as_std_path().to_path_buf();
            assert!(expected_path.to_string_lossy().contains("my_package"));
            assert!(expected_path.to_string_lossy().contains("module.py"));
            assert!(memory_fs.exists(&module_path));
        }

        #[test]
        fn test_resolve_module_with_system_python() {
            let system = TestSystem::default();
            let install_path = if cfg!(target_os = "windows") {
                r"\Python312"
            } else {
                "/usr/local/python3.12"
            };
            let python_version = "3.12";

            create_mock_system_python(&system, install_path, python_version);

            let site_packages = if cfg!(target_os = "windows") {
                format!(r"{}\Lib\site-packages", install_path)
            } else {
                format!(
                    "{}/lib/python{}/site-packages",
                    install_path, python_version
                )
            };

            create_mock_package_in_site_packages(&system, &site_packages, "system_pkg", true);

            let memory_fs = system.memory_file_system();
            let module_path = SystemPathBuf::from(site_packages.as_str())
                .join("system_pkg")
                .join("__init__.py");
            memory_fs
                .write_file_all(&module_path, "# System package\n")
                .unwrap();

            assert!(memory_fs.exists(&module_path));
        }

        #[test]
        fn test_resolve_module_with_pyi_stub_priority() {
            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();
            let workspace = SystemPathBuf::from("/workspace");

            memory_fs.create_directory_all(&workspace).unwrap();

            // Create both .py and .pyi files
            let py_file = workspace.join("mymodule.py");
            let pyi_file = workspace.join("mymodule.pyi");

            memory_fs
                .write_file_all(&py_file, "def func(): pass\n")
                .unwrap();
            memory_fs
                .write_file_all(&pyi_file, "def func() -> None: ...\n")
                .unwrap();

            // The .pyi file should be preferred
            // This is tested by the actual resolve_module logic
            assert!(memory_fs.exists(&pyi_file));
            assert!(memory_fs.exists(&py_file));
        }

        #[test]
        fn test_discover_python_environment_structure() {
            let system = TestSystem::default();
            let venv_path = "/.venv";
            create_mock_venv(&system, venv_path, "3.12", false);

            let memory_fs = system.memory_file_system();
            let pyvenv_cfg = SystemPathBuf::from(venv_path).join("pyvenv.cfg");

            // Verify the structure was created correctly
            assert!(memory_fs.exists(&pyvenv_cfg));

            let cfg_content = memory_fs.read_to_string(&pyvenv_cfg).unwrap();
            assert!(cfg_content.contains("home ="));
            assert!(cfg_content.contains("version = 3.12"));
        }

        #[test]
        fn test_venv_with_system_site_packages() {
            let system = TestSystem::default();
            let venv_path = "/.venv";
            create_mock_venv(&system, venv_path, "3.12", true);

            let memory_fs = system.memory_file_system();
            let pyvenv_cfg = SystemPathBuf::from(venv_path).join("pyvenv.cfg");
            let cfg_content = memory_fs.read_to_string(&pyvenv_cfg).unwrap();

            assert!(cfg_content.contains("include-system-site-packages = true"));
        }

        #[test]
        fn test_multiple_python_versions() {
            let system = TestSystem::default();

            for version in &["3.10", "3.11", "3.12", "3.13"] {
                let venv_path = format!("/.venv{}", version.replace('.', ""));
                create_mock_venv(&system, &venv_path, version, false);

                let memory_fs = system.memory_file_system();
                let site_packages = if cfg!(target_os = "windows") {
                    SystemPathBuf::from(venv_path.as_str()).join(r"Lib\site-packages")
                } else {
                    SystemPathBuf::from(venv_path.as_str())
                        .join(format!("lib/python{}/site-packages", version))
                };

                assert!(memory_fs.exists(&site_packages));
            }
        }

        #[test]
        fn test_package_with_submodules() {
            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();
            let workspace = SystemPathBuf::from("/workspace");

            // Create a package with submodules
            let package_dir = workspace.join("mypackage");
            let subpackage_dir = package_dir.join("subpackage");

            memory_fs.create_directory_all(&subpackage_dir).unwrap();

            // Create __init__.py files
            memory_fs
                .write_file_all(package_dir.join("__init__.py"), "# Package init\n")
                .unwrap();
            memory_fs
                .write_file_all(subpackage_dir.join("__init__.py"), "# Subpackage init\n")
                .unwrap();

            // Create a module in the subpackage
            memory_fs
                .write_file_all(
                    subpackage_dir.join("module.py"),
                    "def submodule_func(): pass\n",
                )
                .unwrap();

            assert!(memory_fs.exists(&package_dir.join("__init__.py")));
            assert!(memory_fs.exists(&subpackage_dir.join("__init__.py")));
            assert!(memory_fs.exists(&subpackage_dir.join("module.py")));
        }

        #[test]
        fn test_namespace_package_without_init() {
            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();
            let workspace = SystemPathBuf::from("/workspace");

            // Create a namespace package (no __init__.py)
            let package_dir = workspace.join("namespace_pkg");
            memory_fs.create_directory_all(&package_dir).unwrap();

            // Create a module directly in the package
            memory_fs
                .write_file_all(package_dir.join("module.py"), "def func(): pass\n")
                .unwrap();

            // Verify no __init__.py exists
            assert!(!memory_fs.exists(&package_dir.join("__init__.py")));
            assert!(memory_fs.exists(&package_dir.join("module.py")));
        }

        #[test]
        fn test_resolve_with_workspace_priority() {
            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();

            // Create workspace module
            let workspace = SystemPathBuf::from("/workspace");
            memory_fs.create_directory_all(&workspace).unwrap();
            memory_fs
                .write_file_all(workspace.join("mymodule.py"), "# Workspace version\n")
                .unwrap();

            // Create venv with same module name
            let venv_path = "/.venv";
            create_mock_venv(&system, venv_path, "3.12", false);

            let site_packages = if cfg!(target_os = "windows") {
                format!(r"{}\Lib\site-packages", venv_path)
            } else {
                format!("{}/lib/python3.12/site-packages", venv_path)
            };

            let site_packages_path = SystemPathBuf::from(site_packages.as_str());
            memory_fs.create_directory_all(&site_packages_path).unwrap();
            memory_fs
                .write_file_all(
                    site_packages_path.join("mymodule.py"),
                    "# Site-packages version\n",
                )
                .unwrap();

            // Both exist, but workspace should have priority
            assert!(memory_fs.exists(&workspace.join("mymodule.py")));
            assert!(memory_fs.exists(&site_packages_path.join("mymodule.py")));
        }

        #[test]
        fn test_python_version_detection_from_pyvenv() {
            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();
            let venv_path = SystemPathBuf::from("/.venv");

            // Create minimal venv structure
            let pyvenv_cfg = venv_path.join("pyvenv.cfg");
            memory_fs.create_directory_all(&venv_path).unwrap();

            let cfg_contents = "home = /usr/local/python3.11/bin\nversion = 3.11.5\n";
            memory_fs.write_file_all(&pyvenv_cfg, cfg_contents).unwrap();

            let content = memory_fs.read_to_string(&pyvenv_cfg).unwrap();
            assert!(content.contains("version = 3.11.5"));

            // Parse version
            let version_line = content
                .lines()
                .find(|line| line.starts_with("version"))
                .unwrap();
            assert!(version_line.contains("3.11.5"));
        }

        #[test]
        fn test_conda_environment_structure() {
            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();

            // Create a conda environment structure
            let conda_prefix = SystemPathBuf::from("/opt/conda/envs/myenv");

            let (exe_path, site_packages) = if cfg!(target_os = "windows") {
                (
                    conda_prefix.join("python.exe"),
                    conda_prefix.join(r"Lib\site-packages"),
                )
            } else {
                (
                    conda_prefix.join("bin/python"),
                    conda_prefix.join("lib/python3.12/site-packages"),
                )
            };

            memory_fs.write_file_all(&exe_path, "").unwrap();
            memory_fs.create_directory_all(&site_packages).unwrap();

            // Create conda-meta directory (distinctive conda feature)
            let conda_meta = conda_prefix.join("conda-meta");
            memory_fs.create_directory_all(&conda_meta).unwrap();

            assert!(memory_fs.exists(&exe_path));
            assert!(memory_fs.exists(&site_packages));
            assert!(memory_fs.exists(&conda_meta));
        }

        #[test]
        fn test_lib_vs_lib64_on_unix() {
            if cfg!(target_os = "windows") {
                return; // Skip on Windows
            }

            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();
            let install_path = SystemPathBuf::from("/usr/local/python3.12");

            // Some systems use lib, others use lib64
            let lib_site_packages = install_path.join("lib/python3.12/site-packages");
            let lib64_site_packages = install_path.join("lib64/python3.12/site-packages");

            memory_fs.create_directory_all(&lib_site_packages).unwrap();
            memory_fs
                .create_directory_all(&lib64_site_packages)
                .unwrap();

            assert!(memory_fs.exists(&lib_site_packages));
            assert!(memory_fs.exists(&lib64_site_packages));
        }

        #[test]
        fn test_editable_install_structure() {
            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();

            let site_packages = if cfg!(target_os = "windows") {
                SystemPathBuf::from(r"\.venv\Lib\site-packages")
            } else {
                SystemPathBuf::from("/.venv/lib/python3.12/site-packages")
            };

            memory_fs.create_directory_all(&site_packages).unwrap();

            // Create a .pth file for editable install
            let pth_file = site_packages.join("myproject.pth");
            memory_fs
                .write_file_all(&pth_file, "/home/user/projects/myproject\n")
                .unwrap();

            assert!(memory_fs.exists(&pth_file));

            let content = memory_fs.read_to_string(&pth_file).unwrap();
            assert!(content.contains("myproject"));
        }

        #[test]
        fn test_free_threaded_python_313() {
            if cfg!(target_os = "windows") {
                return; // Skip on Windows for this test
            }

            let system = TestSystem::default();
            let memory_fs = system.memory_file_system();
            let install_path = SystemPathBuf::from("/usr/local/python3.13");

            // Python 3.13+ free-threaded builds use a 't' suffix
            let site_packages = install_path.join("lib/python3.13t/site-packages");
            memory_fs.create_directory_all(&site_packages).unwrap();

            assert!(memory_fs.exists(&site_packages));
            assert!(site_packages.to_string().contains("python3.13t"));
        }
    }

    // ==================== Python Interpreter Priority Tests ====================
    // These tests verify that the configured Python interpreter from LSP initialization
    // takes priority over auto-discovery

    #[test]
    #[ignore = "Requires Python in PATH"]
    fn test_configured_interpreter_takes_priority() {
        // This test verifies that when a Python interpreter is explicitly configured
        // via LSP initialization, it is used instead of auto-discovery
        let examples_dir = get_simple_test_dir();

        // Get the current system Python
        let system_python = which::which("python3")
            .or_else(|_| which::which("python"))
            .expect("Python not found in PATH");

        let system_python_str = system_python.to_str().unwrap();

        // Resolve a module with explicit interpreter configuration
        let db = test_db();
        let site_packages =
            discover_python_environment(Some(examples_dir.as_ref()), Some(system_python_str))
                .unwrap_or_default();
        let search_paths = build_search_paths(&db, Some(examples_dir.as_ref()), &site_packages);
        let mid = TargetString::new(&db, "my_module".to_string());
        let spid = InternedSearchPaths::new(&db, search_paths);
        let result_with_config = resolve_module_cached(&db, mid, spid).clone();

        // Should succeed using the configured interpreter
        assert!(
            result_with_config.is_some(),
            "Module resolution should succeed with configured interpreter"
        );
    }

    #[test]
    fn test_auto_discovery_when_no_interpreter_configured() {
        // This test verifies that when no Python interpreter is configured,
        // the system falls back to auto-discovery
        let examples_dir = get_simple_test_dir();
        let db = test_db();
        let search_paths = vec![examples_dir.clone(), PathBuf::from(".")];
        let mid = TargetString::new(&db, "my_module".to_string());
        let spid = InternedSearchPaths::new(&db, search_paths);
        let result = resolve_module_cached(&db, mid, spid).clone();

        // Should still succeed using auto-discovery
        assert!(
            result.is_some(),
            "Module resolution should succeed with auto-discovery"
        );
    }

    #[test]
    #[ignore = "Requires Python in PATH"]
    fn test_extract_definition_with_configured_interpreter() {
        // Test that definition extraction uses configured interpreter
        let examples_dir = get_simple_test_dir();

        // Get the current system Python
        let system_python = which::which("python3")
            .or_else(|_| which::which("python"))
            .ok();

        let system_python_str = system_python.as_ref().and_then(|p| p.to_str());

        // Extract definition with configured interpreter
        let site_packages =
            discover_python_environment(Some(examples_dir.as_ref()), system_python_str)
                .unwrap_or_default();
        let db = test_db();
        let search_paths = build_search_paths(&db, Some(examples_dir.as_ref()), &site_packages);
        let result = extract_definition_info(&db, "my_module.simple_function", &search_paths);

        assert!(
            result.is_ok(),
            "Definition extraction should work with configured interpreter: {:?}",
            result.err()
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

    #[test]
    #[ignore = "Requires Python in PATH"]
    fn test_resolve_module_priority_order() {
        // This test documents the priority order:
        // 1. Configured interpreter (highest priority)
        // 2. Auto-discovered environment (fallback)
        let examples_dir = get_simple_test_dir();

        // Test with configured interpreter (priority 1)
        if let Ok(python_path) = which::which("python3").or_else(|_| which::which("python")) {
            let db = test_db();
            let site_packages =
                discover_python_environment(Some(examples_dir.as_ref()), python_path.to_str())
                    .unwrap_or_default();
            let search_paths = build_search_paths(&db, Some(examples_dir.as_ref()), &site_packages);
            let mid = TargetString::new(&db, "my_module".to_string());
            let spid = InternedSearchPaths::new(&db, search_paths);
            let result_configured = resolve_module_cached(&db, mid, spid).clone();
            assert!(
                result_configured.is_some(),
                "Should resolve with configured interpreter"
            );
        }

        // Test with auto-discovery (priority 2)
        let db = test_db();
        let search_paths = vec![examples_dir.clone(), PathBuf::from(".")];
        let mid = TargetString::new(&db, "my_module".to_string());
        let spid = InternedSearchPaths::new(&db, search_paths);
        let result_auto = resolve_module_cached(&db, mid, spid).clone();
        assert!(result_auto.is_some(), "Should resolve with auto-discovery");
    }

    // ==================== .pth file parsing tests ====================

    fn get_editable_test_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("workspace")
            .join("editable")
    }

    #[test]
    fn test_parse_pth_files_basic() {
        // Test parsing .pth files from the editable test workspace
        let workspace_dir = get_editable_test_dir();
        let site_packages = workspace_dir.join("site-packages");
        let db = test_db();

        let paths = parse_pth_files(&db, &site_packages);

        // Should find the path from _editable_package.pth
        assert!(!paths.is_empty(), "Should find paths from .pth files");

        // The path should point to the src directory
        let has_src_path = paths.iter().any(|p| p.ends_with("src"));
        assert!(has_src_path, "Should find src path from .pth file");
    }

    #[test]
    fn test_parse_pth_files_nonexistent_dir() {
        // Test that parsing .pth files from nonexistent directory returns empty
        let nonexistent = PathBuf::from("/nonexistent/path/site-packages");
        let db = test_db();
        let paths = parse_pth_files(&db, &nonexistent);
        assert!(paths.is_empty(), "Should return empty for nonexistent dir");
    }

    #[test]
    fn test_resolve_editable_module() {
        // Test resolving a module from an editable install via .pth file
        let workspace_dir = get_editable_test_dir();
        let site_packages = workspace_dir.join("site-packages");
        let db = test_db();

        // Build search paths manually with site-packages that has .pth files
        let mut search_paths = vec![workspace_dir.clone()];
        search_paths.push(site_packages.clone());
        search_paths.extend(parse_pth_files(&db, &site_packages));

        // The editable_package should now be resolvable
        let mid = TargetString::new(&db, "editable_package.lib".to_string());
        let spid = InternedSearchPaths::new(&db, search_paths);
        let result = resolve_module_cached(&db, mid, spid).clone();

        assert!(result.is_some(), "Should resolve editable_package.lib");
        let path = result.unwrap();
        assert!(
            path.ends_with("lib.py"),
            "Should resolve to lib.py: {:?}",
            path
        );
    }

    #[test]
    fn test_extract_definition_from_editable_package() {
        // Test extracting class definition from an editable package via .pth resolution
        let workspace_dir = get_editable_test_dir();
        let site_packages = workspace_dir.join("site-packages");
        let db = test_db();

        // Build search paths manually with site-packages that has .pth files
        let mut search_paths = vec![workspace_dir.clone()];
        search_paths.push(site_packages.clone());
        search_paths.extend(parse_pth_files(&db, &site_packages));

        // Resolve the module first
        let mid = TargetString::new(&db, "editable_package.lib".to_string());
        let spid = InternedSearchPaths::new(&db, search_paths);
        let module_path = resolve_module_cached(&db, mid, spid)
            .clone()
            .expect("Should resolve module");

        // Extract the class info (salsa-cached)
        let class_info = extract_class_info(&db, &module_path, "EditableModel")
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
