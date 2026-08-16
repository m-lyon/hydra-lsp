//! Tests for the `hydrust check` command line surface.
//!
//! These cover the parts a CI pipeline depends on: which files get checked when
//! a directory is given, the exit code, and the shape of the `github` output
//! format that GitHub turns into inline annotations.

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

const HYDRUST: &str = env!("CARGO_BIN_EXE_hydrust");

/// A config whose `_target_` cannot resolve, so it always produces an error.
const BROKEN_CONFIG: &str = "model:\n  _target_: no_such_module.Thing\n  size: 4\n";

/// Valid YAML with no Hydra markers at all.
const PLAIN_YAML: &str = "name: not-a-hydra-config\nvalues:\n  - 1\n  - 2\n";

struct CheckOutput {
    code: i32,
    stdout: String,
}

/// Run `hydrust check` from inside `dir`, so that reported paths are relative
/// to it in the same way they are relative to the repository root under CI.
fn check_in(dir: &Path, args: &[&str]) -> CheckOutput {
    let output = Command::new(HYDRUST)
        .arg("check")
        .args(args)
        // Quiet: the tracing output goes to stderr and would only add noise.
        .args(["--verbosity", "error"])
        .current_dir(dir)
        .output()
        .unwrap();

    CheckOutput {
        code: output.status.code().unwrap(),
        stdout: String::from_utf8(output.stdout).unwrap(),
    }
}

#[test]
fn test_check_is_a_subcommand() {
    let output = Command::new(HYDRUST).arg("--help").output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(stdout.contains("Usage: hydrust <COMMAND>"), "got: {stdout}");
    assert!(stdout.contains("check"), "got: {stdout}");
}

#[test]
fn test_missing_path_is_a_fatal_error() {
    let dir = TempDir::new().unwrap();
    let result = check_in(dir.path(), &["does-not-exist.yaml"]);

    assert_eq!(result.code, 2, "a missing path is fatal, not a diagnostic");
}

#[test]
fn test_directory_with_no_yaml_is_a_fatal_error() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("notes.txt"), "nothing to check").unwrap();

    let result = check_in(dir.path(), &["."]);

    assert_eq!(result.code, 2);
}

#[test]
fn test_diagnostics_exit_one() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("config.yaml"), BROKEN_CONFIG).unwrap();

    let result = check_in(dir.path(), &["config.yaml", "--output-format", "compact"]);

    assert_eq!(result.code, 1);
    assert!(
        result.stdout.contains("config.yaml:"),
        "got: {}",
        result.stdout
    );
}

#[test]
fn test_format_is_not_accepted() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("config.yaml"), BROKEN_CONFIG).unwrap();

    // `--output-format` is the only spelling. Nothing is published yet, so there
    // is no `--format` alias to keep compatible.
    let result = check_in(dir.path(), &["config.yaml", "--format", "compact"]);

    assert_eq!(result.code, 2, "got: {}", result.stdout);
}

#[test]
fn test_directory_walk_finds_nested_configs() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("conf/model")).unwrap();
    fs::write(dir.path().join("conf/top.yaml"), BROKEN_CONFIG).unwrap();
    fs::write(dir.path().join("conf/model/deep.yml"), BROKEN_CONFIG).unwrap();

    let result = check_in(dir.path(), &["conf", "--output-format", "compact"]);

    assert_eq!(result.code, 1);
    assert!(result.stdout.contains("top.yaml"), "got: {}", result.stdout);
    assert!(result.stdout.contains("deep.yml"), "got: {}", result.stdout);
}

#[test]
fn test_directory_walk_skips_non_hydra_yaml() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("plain.yaml"), PLAIN_YAML).unwrap();
    fs::write(dir.path().join("config.yaml"), BROKEN_CONFIG).unwrap();

    let result = check_in(dir.path(), &[".", "--output-format", "compact"]);

    assert!(
        !result.stdout.contains("plain.yaml"),
        "a discovered file without Hydra markers should be skipped silently, got: {}",
        result.stdout
    );
    assert!(
        result.stdout.contains("config.yaml"),
        "got: {}",
        result.stdout
    );
}

#[test]
fn test_directory_walk_honours_gitignore() {
    let dir = TempDir::new().unwrap();
    // Deliberately not a git checkout: `.gitignore` should apply either way.
    fs::write(dir.path().join(".gitignore"), "vendor/\n").unwrap();
    fs::create_dir_all(dir.path().join("vendor")).unwrap();
    fs::write(dir.path().join("vendor/skipped.yaml"), BROKEN_CONFIG).unwrap();
    fs::write(dir.path().join("config.yaml"), BROKEN_CONFIG).unwrap();

    let result = check_in(dir.path(), &[".", "--output-format", "compact"]);

    assert!(
        !result.stdout.contains("skipped.yaml"),
        "got: {}",
        result.stdout
    );
    assert!(
        result.stdout.contains("config.yaml"),
        "got: {}",
        result.stdout
    );
}

#[test]
fn test_overlapping_paths_check_each_file_once() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("conf")).unwrap();
    fs::write(dir.path().join("conf/config.yaml"), BROKEN_CONFIG).unwrap();

    let result = check_in(
        dir.path(),
        &["conf/config.yaml", "conf", "--output-format", "github"],
    );

    let annotations = result
        .stdout
        .lines()
        .filter(|line| line.starts_with("::"))
        .count();
    let unique: std::collections::HashSet<&str> = result
        .stdout
        .lines()
        .filter(|line| line.starts_with("::"))
        .collect();

    assert_eq!(
        annotations,
        unique.len(),
        "the same file was checked twice, got: {}",
        result.stdout
    );
}

#[test]
fn test_github_format_emits_relative_annotations() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("conf")).unwrap();
    fs::write(dir.path().join("conf/config.yaml"), BROKEN_CONFIG).unwrap();

    let result = check_in(dir.path(), &["conf", "--output-format", "github"]);

    assert_eq!(result.code, 1);
    let first = result
        .stdout
        .lines()
        .find(|line| line.starts_with("::error"))
        .unwrap_or_else(|| panic!("no annotation in: {}", result.stdout));

    // GitHub only attaches an annotation when the path is relative to the
    // repository root, so an absolute path here is a silent failure in CI.
    assert!(
        first.contains("file=conf/config.yaml,"),
        "expected a repository-relative path, got: {first}"
    );
    assert!(first.contains("line="), "got: {first}");
    assert!(first.contains("title=hydrust("), "got: {first}");
}

#[test]
fn test_json_output_is_a_single_document_for_many_files() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.yaml"), BROKEN_CONFIG).unwrap();
    fs::write(dir.path().join("b.yaml"), BROKEN_CONFIG).unwrap();

    let result = check_in(dir.path(), &[".", "--output-format", "json"]);

    let parsed: serde_json::Value = serde_json::from_str(&result.stdout)
        .unwrap_or_else(|e| panic!("output was not one JSON document ({e}): {}", result.stdout));

    let files = parsed["files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "got: {parsed}");
    assert_eq!(parsed["summary"]["files"], 2);
}

#[test]
fn test_clean_run_exits_zero() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("config.yaml"), BROKEN_CONFIG).unwrap();

    // The only diagnostic these configs produce is the unresolved import, so
    // disabling it leaves a clean run.
    let result = check_in(
        dir.path(),
        &[
            ".",
            "--disable-rule",
            "unresolved-import",
            "--output-format",
            "compact",
        ],
    );

    assert_eq!(result.code, 0, "got: {}", result.stdout);
    assert!(
        result.stdout.contains("OK - no issues found"),
        "got: {}",
        result.stdout
    );
}
