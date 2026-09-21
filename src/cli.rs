//! The `hydrust` binary: `hydrust check` and `hydrust server`.
//!
//! `check` parses Hydra YAML files and outputs diagnostics to help debug
//! issues with `_target_` resolution and parameter validation. `server` runs
//! the same analysis as a language server (`server.rs`).

use std::fmt;
use std::fs;
use std::io::stderr;
use std::path::{Component, Path, PathBuf};
use std::process;

use anyhow::Context;
use clap::{Args, Parser, Subcommand, ValueEnum};
use colored::Colorize;
use ignore::WalkBuilder;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};
use tracing::{Level, debug, error, info, warn};

use hydrust::database::HydraDatabase;
use hydrust::diagnostics::{DiagnosticRule, validate_document};
use hydrust::python_analyzer::PythonAnalyzer;
use hydrust::python_cache::PythonConfig;
use hydrust::yaml_parser::YamlParser;

use std::collections::{HashMap, HashSet};

/// Tooling for Hydra YAML configuration files
#[derive(Parser)]
#[command(name = "hydrust")]
#[command(author, version, long_about = None)]
#[command(about = "Diagnostics and language server for Hydra YAML configuration files")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check Hydra YAML configuration files for diagnostics.
    Check(CheckCommand),

    /// Run the language server, speaking LSP over stdin/stdout.
    Server(ServerCommand),
}

/// `hydrust server` — the language server.
///
/// Takes no options of its own. `ignored` is a catch-all to prevent provided arguments
/// from erroring out.
#[derive(Args)]
struct ServerCommand {
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        hide = true,
        value_name = "IGNORED"
    )]
    ignored: Vec<String>,
}

#[derive(Args)]
struct CheckCommand {
    /// Files or directories to check. Directories are searched recursively for
    /// `.yaml` and `.yml` files, following symlinks, honouring `.gitignore` and
    /// `.ignore` files and skipping hidden files and directories.
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Working directory for resolving Python modules (defaults to current directory)
    #[arg(short, long)]
    workspace: Option<PathBuf>,

    /// Path to Python interpreter to use for module resolution
    #[arg(short, long)]
    python: Option<PathBuf>,

    /// Verbosity level for logging
    #[arg(short, long, value_enum, default_value = "info")]
    verbosity: Verbosity,

    /// Output format
    #[arg(
        short = 'f',
        long = "output-format",
        value_enum,
        default_value = "pretty"
    )]
    format: OutputFormat,

    /// Show detailed resolution steps for each target (written to stderr, so
    /// it does not corrupt machine-readable output on stdout)
    #[arg(long)]
    trace_resolution: bool,

    /// Disable a diagnostic rule (can be repeated)
    #[arg(
        long = "disable-rule",
        value_name = "RULE",
        value_parser = clap::builder::PossibleValuesParser::new(
            DiagnosticRule::all_codes().iter().copied()
        ),
    )]
    disable_rules: Vec<String>,
}

struct OptionalPath<'a>(Option<&'a PathBuf>);

impl fmt::Debug for OptionalPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(p) => write!(f, "\"{}\"", p.display()),
            None => write!(f, "None"),
        }
    }
}

impl fmt::Debug for CheckCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let paths: Vec<String> = self.paths.iter().map(|p| p.display().to_string()).collect();
        f.debug_struct("CheckCommand")
            .field("paths", &paths)
            .field("workspace", &OptionalPath(self.workspace.as_ref()))
            .field("python", &OptionalPath(self.python.as_ref()))
            .field("verbosity", &self.verbosity)
            .field("format", &self.format)
            .field("trace_resolution", &self.trace_resolution)
            .field("disable_rules", &self.disable_rules)
            .finish()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Verbosity {
    /// Only show errors
    Error,
    /// Show warnings and errors
    Warn,
    /// Show info, warnings, and errors
    Info,
    /// Show debug information
    Debug,
    /// Show all trace information
    Trace,
}

impl From<Verbosity> for Level {
    fn from(v: Verbosity) -> Self {
        match v {
            Verbosity::Error => Level::ERROR,
            Verbosity::Warn => Level::WARN,
            Verbosity::Info => Level::INFO,
            Verbosity::Debug => Level::DEBUG,
            Verbosity::Trace => Level::TRACE,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    /// Human-readable pretty output
    Pretty,
    /// JSON output
    Json,
    /// Compact single-line per diagnostic
    Compact,
    /// GitHub Actions workflow commands, rendered as inline annotations
    Github,
}

/// A file selected for checking, plus whether it was explicitly selected, or
/// discovered by walking a directory.
struct CheckTarget {
    /// Absolute, canonicalized path, used to read the file.
    path: PathBuf,
    /// Path as reported to the user: relative to the current directory where possible.
    display: String,
    explicit: bool,
}

/// The outcome of checking one file.
struct FileReport {
    path: String,
    diagnostics: Vec<Diagnostic>,
    /// Set when the file could not be read or parsed at all.
    failure: Option<String>,
    /// Stable code naming the kind of failure, for machine-readable output.
    failure_code: Option<&'static str>,
}

impl FileReport {
    fn error_count(&self) -> usize {
        let parse_failure = usize::from(self.failure.is_some());
        parse_failure
            + self
                .diagnostics
                .iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                .count()
    }
}

fn main() {
    let cli = Cli::parse();
    let args = match &cli.command {
        Command::Check(args) => args,
        Command::Server(server) => {
            if !server.ignored.is_empty() {
                // stderr, never stdout: stdout is the LSP transport.
                eprintln!("hydrust: ignoring unrecognised arguments; starting language server");
            }
            hydrust::server::serve();
            return;
        }
    };

    // Initialize tracing with the specified verbosity
    let level: Level = args.verbosity.into();
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(stderr)
        .with_ansi(true)
        .init();

    info!("hydrust starting");
    debug!("Arguments: {:?}", args);

    // Run the main logic and handle errors
    match run(args) {
        Ok(exit_code) => process::exit(exit_code),
        Err(e) => {
            error!("Fatal error: {}", e);
            eprintln!("{}: {}", "Error".red().bold(), e);
            process::exit(2);
        }
    }
}

fn run(args: &CheckCommand) -> anyhow::Result<i32> {
    let targets = collect_targets(&args.paths)?;
    // Resolved before the empty check so that a bad `--workspace` is a usage
    // error regardless of what happens to be on disk.
    let workspace_root = resolve_workspace_root(args)?;
    if targets.is_empty() {
        eprintln!(
            "{}: no YAML files found in the given path(s)",
            "warning".yellow().bold()
        );
        emit(args.format, &[])?;
        return Ok(0);
    }
    info!("Checking {} file(s)", targets.len());

    info!("Workspace root: {}", workspace_root.display());

    let python_interpreter = args
        .python
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    if let Some(ref py) = python_interpreter {
        info!("Python interpreter: {}", py);
    }

    let disabled_rules = parse_disabled_rules(&args.disable_rules);

    let db_root = workspace_root.to_str().unwrap_or(".");
    let db = HydraDatabase::new(ruff_db::system::SystemPath::new(db_root));
    let python_config = PythonConfig::new(
        &db,
        Some(workspace_root.to_string_lossy().to_string()),
        python_interpreter.clone(),
    );

    let mut reports = Vec::with_capacity(targets.len());
    let mut not_hydra = 0usize;
    let mut vanished = 0usize;
    for target in &targets {
        match check_target(target, args, &db, python_config, &disabled_rules) {
            CheckOutcome::Report(report) => reports.push(report),
            CheckOutcome::NotHydra => not_hydra += 1,
            CheckOutcome::Vanished => vanished += 1,
        }
    }

    if reports.is_empty() {
        if not_hydra > 0 {
            eprintln!(
                "{}: found {} YAML file(s), but none appear to be Hydra configs",
                "warning".yellow().bold(),
                not_hydra
            );
        }
        if vanished > 0 {
            eprintln!(
                "{}: {} file(s) disappeared before they could be checked",
                "warning".yellow().bold(),
                vanished
            );
        }
    }

    emit(args.format, &reports)?;

    // Return exit code: 0 if no errors, 1 if there are errors
    let error_count: usize = reports.iter().map(FileReport::error_count).sum();
    if error_count > 0 {
        info!("Found {} error(s)", error_count);
        Ok(1)
    } else {
        info!("No errors found");
        Ok(0)
    }
}

fn emit(format: OutputFormat, reports: &[FileReport]) -> anyhow::Result<()> {
    match format {
        OutputFormat::Pretty => output_pretty(reports),
        OutputFormat::Json => output_json(reports)?,
        OutputFormat::Compact => output_compact(reports),
        OutputFormat::Github => output_github(reports),
    }
    Ok(())
}

/// Expand the command-line paths into the set of files to check.
///
/// Files are taken as given; directories are walked recursively for `.yaml` and
/// `.yml` files, respecting `.gitignore`. Duplicates are dropped, so overlapping
/// arguments (`config.yaml conf/`) check each file once.
fn collect_targets(paths: &[PathBuf]) -> anyhow::Result<Vec<CheckTarget>> {
    let mut targets: Vec<CheckTarget> = Vec::new();
    let mut seen: HashMap<PathBuf, usize> = HashMap::new();
    // Both the logical and the canonical cwd are kept as display bases: an
    // absolute argument is left as the user typed it, so it only strips against
    // whichever form it was spelled with. They differ whenever the cwd is
    // reached through a symlink, and always on Windows, where `canonicalize`
    // returns the verbatim `\\?\C:\...` form.
    let cwd_bases: Vec<PathBuf> = match std::env::current_dir() {
        Ok(dir) => {
            let canonical = dir.canonicalize().ok().filter(|c| *c != dir);
            std::iter::once(dir).chain(canonical).collect()
        }
        Err(_) => Vec::new(),
    };
    let cwd = cwd_bases.first().map(PathBuf::as_path);

    for path in paths {
        if !path.exists() {
            anyhow::bail!("Path not found: {}", path.display());
        }

        if path.is_file() {
            let canonical = path
                .canonicalize()
                .with_context(|| format!("Failed to resolve path: {}", path.display()))?;
            // An explicit mention always wins, whichever order the arguments
            // arrive in: a file already picked up by a directory walk is
            // promoted rather than dropped as a duplicate.
            match seen.get(&canonical) {
                Some(&index) => targets[index].explicit = true,
                None => {
                    seen.insert(canonical.clone(), targets.len());
                    targets.push(CheckTarget {
                        display: display_name(path, &canonical, cwd, &cwd_bases),
                        path: canonical,
                        explicit: true,
                    });
                }
            }
            continue;
        }

        if !path.is_dir() {
            anyhow::bail!("Not a regular file or directory: {}", path.display());
        }

        // `require_git(false)` so that `.gitignore` is honoured whether or not
        // the tree happens to be a git checkout; otherwise which files get
        // checked would depend on the presence of `.git`. `git_global(false)`,
        // `git_exclude(false)` and `parents(false)` so that nothing that is
        // not committed inside the walk root - the developer's personal global
        // excludes, the clone-local `.git/info/exclude`, or a stray
        // `~/.gitignore` - can make a local run disagree with CI. Sorted so that output is
        // reproducible across runs and platforms.
        let walk = WalkBuilder::new(path)
            .require_git(false)
            .git_global(false)
            .git_exclude(false)
            .parents(false)
            .follow_links(true)
            .sort_by_file_path(|a, b| a.cmp(b))
            .build();
        for entry in walk {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    warn!("Skipping unreadable entry under {}: {e}", path.display());
                    continue;
                }
            };
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            if !is_yaml_file(entry.path()) {
                continue;
            }
            let canonical = match entry.path().canonicalize() {
                Ok(canonical) => canonical,
                Err(e) => {
                    warn!("Skipping {}: {e}", entry.path().display());
                    continue;
                }
            };
            if let std::collections::hash_map::Entry::Vacant(slot) = seen.entry(canonical.clone()) {
                slot.insert(targets.len());
                targets.push(CheckTarget {
                    display: display_name(entry.path(), &canonical, cwd, &cwd_bases),
                    path: canonical,
                    explicit: false,
                });
            }
        }
    }

    Ok(targets)
}

/// The name to report `path` under: its lexical form, unless dropping a `..`
/// that followed a symlink made that name a different file from the one that
/// is actually read, in which case the canonical path is used instead.
fn display_name(
    path: &Path,
    canonical: &Path,
    cwd: Option<&Path>,
    cwd_bases: &[PathBuf],
) -> String {
    let lexical = lexical_absolute(path, cwd);
    let name = if lexical.canonicalize().is_ok_and(|c| c == canonical) {
        lexical
    } else {
        canonical.to_path_buf()
    };
    display_path(&name, cwd_bases)
}

/// Make `path` absolute against `base` and drop `.`/`..` components without
/// resolving symlinks.
fn lexical_absolute(path: &Path, base: Option<&Path>) -> PathBuf {
    let joined = match base {
        Some(base) if path.is_relative() => base.join(path),
        _ => path.to_path_buf(),
    };

    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Render `path` relative to `base` when it sits underneath it, otherwise as
/// the absolute path.
///
/// Separators are always `/`, including on Windows: GitHub only attaches an
/// annotation when `file=` is a `/`-separated path relative to the repository
/// root, so a `\`-separated path is dropped without explanation. The replacement is
/// skipped where `\` is a legal character in a file name.
fn display_path(path: &Path, bases: &[PathBuf]) -> String {
    let rendered = bases
        .iter()
        .find_map(|base| path.strip_prefix(base).ok())
        .unwrap_or(path)
        .display()
        .to_string();

    if std::path::MAIN_SEPARATOR == '\\' {
        rendered.replace('\\', "/")
    } else {
        rendered
    }
}

fn is_yaml_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml"))
}

/// Pick the root used for Python module resolution: `--workspace` if given,
/// otherwise the current directory.
fn resolve_workspace_root(args: &CheckCommand) -> anyhow::Result<PathBuf> {
    if let Some(ref ws) = args.workspace {
        return ws
            .canonicalize()
            .with_context(|| format!("Workspace not found: {}", ws.display()));
    }

    Ok(std::env::current_dir()?.canonicalize()?)
}

/// Turn the `--disable-rule` codes into rules.
fn parse_disabled_rules(rules: &[String]) -> HashSet<DiagnosticRule> {
    rules
        .iter()
        .map(|code| DiagnosticRule::from_code(code).expect("validated by clap"))
        .collect()
}

/// What checking a single file produced.
enum CheckOutcome {
    Report(FileReport),
    /// Discovered by walking a directory, and not a Hydra config.
    NotHydra,
    /// Discovered by walking a directory, and gone by the time it was read.
    Vanished,
}

/// Check a single file.
fn check_target(
    target: &CheckTarget,
    args: &CheckCommand,
    db: &HydraDatabase,
    python_config: PythonConfig,
    disabled_rules: &HashSet<DiagnosticRule>,
) -> CheckOutcome {
    let file_path = &target.path;
    debug!("Checking file: {}", target.display);

    let read_failure = |e: &dyn std::fmt::Display| {
        error!("Failed to read {}: {}", target.display, e);
        CheckOutcome::Report(FileReport {
            path: target.display.clone(),
            diagnostics: Vec::new(),
            failure: Some(format!("Failed to read file: {e}")),
            failure_code: Some("read-error"),
        })
    };

    let bytes = match fs::read(file_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            if !target.explicit && e.kind() == std::io::ErrorKind::NotFound {
                warn!("Skipping {}: {e}", target.display);
                return CheckOutcome::Vanished;
            }
            return read_failure(&e);
        }
    };
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(e) => {
            if !target.explicit
                && !YamlParser::is_hydra_file(&String::from_utf8_lossy(e.as_bytes()))
            {
                debug!("Skipping non-Hydra file: {}", target.display);
                return CheckOutcome::NotHydra;
            }
            return read_failure(&e);
        }
    };
    debug!("File content length: {} bytes", content.len());

    if !YamlParser::is_hydra_file(&content) {
        if !target.explicit {
            debug!("Skipping non-Hydra file: {}", target.display);
            return CheckOutcome::NotHydra;
        }
        warn!("File does not appear to be a Hydra configuration file");
        eprintln!(
            "{}: {} does not contain Hydra markers (# @hydra, # @package) or _target_ keys",
            "Warning".yellow().bold(),
            target.display
        );
    }

    debug!("Parsing YAML content...");
    let parsed_content = match YamlParser::parse(&content) {
        Ok(result) => result,
        Err(e) => {
            error!("Failed to parse YAML: {}", e);
            return CheckOutcome::Report(FileReport {
                path: target.display.clone(),
                diagnostics: Vec::new(),
                failure: Some(format!("Failed to parse YAML: {e}")),
                failure_code: Some("parse-error"),
            });
        }
    };

    debug!(
        "Found {} _target_ definitions",
        parsed_content.hydra_objects.len()
    );

    // If trace_resolution is enabled, show detailed info for each target
    if args.trace_resolution {
        eprintln!(
            "\n{} {}",
            "=== Target Resolution Trace ===".cyan().bold(),
            target.display
        );
        for (i, hydra_object) in parsed_content.hydra_objects.iter().enumerate() {
            trace_target_resolution(i, hydra_object, db, python_config);
        }
        eprintln!();
    }

    debug!("Running diagnostics...");
    let diagnostics = validate_document(&parsed_content, disabled_rules, db, python_config);

    CheckOutcome::Report(FileReport {
        path: target.display.clone(),
        diagnostics,
        failure: None,
        failure_code: None,
    })
}

fn trace_target_resolution(
    index: usize,
    hydra_object: &hydrust::yaml_parser::HydraObject,
    db: &HydraDatabase,
    python_config: PythonConfig,
) {
    eprintln!(
        "\n{} [{}] {} (line {})",
        "Target".blue().bold(),
        index + 1,
        hydra_object.target.value.yellow(),
        hydra_object.target.line + 1
    );

    let search_paths = hydrust::python_cache::search_paths_for_config(db, python_config);
    match PythonAnalyzer::extract_definition_info(db, &hydra_object.target.value, search_paths) {
        Ok((def_info, file_path, module_path, symbol_name)) => {
            eprintln!("  {} {}", "Module:".dimmed(), module_path);
            eprintln!("  {} {}", "Symbol:".dimmed(), symbol_name);
            eprintln!("  {} {}", "Definition found:".green(), file_path.display());

            let implicit_param = def_info.implicit_param();
            match &def_info {
                hydrust::python_analyzer::DefinitionInfo::Function(sig) => {
                    eprintln!("  {} Function", "Type:".dimmed());
                    eprintln!(
                        "  {} {}",
                        "Signature:".dimmed(),
                        format_signature_brief(sig, implicit_param)
                    );
                }
                hydrust::python_analyzer::DefinitionInfo::Class(class_info) => {
                    eprintln!("  {} Class", "Type:".dimmed());
                    if let Some(ref init_sig) = class_info.init_signature {
                        eprintln!(
                            "  {} {}",
                            "__init__:".dimmed(),
                            format_signature_brief(init_sig, implicit_param)
                        );
                    } else {
                        eprintln!("  {} (no __init__ found)", "__init__:".dimmed());
                    }
                }
                hydrust::python_analyzer::DefinitionInfo::Method(method_info) => {
                    let method_type = if method_info.is_classmethod {
                        "classmethod"
                    } else if method_info.is_staticmethod {
                        "staticmethod"
                    } else {
                        "method"
                    };
                    eprintln!(
                        "  {} {} ({})",
                        "Type:".dimmed(),
                        method_type,
                        method_info.class_name
                    );
                    eprintln!(
                        "  {} {}",
                        "Signature:".dimmed(),
                        format_signature_brief(&method_info.signature, implicit_param)
                    );
                }
            }
        }
        Err(e) => {
            let error_msg = e.to_string();
            if error_msg.starts_with("Invalid _target_ format:")
                || error_msg.starts_with("Could not resolve module:")
            {
                eprintln!("  {} {}", "Error:".red(), error_msg)
            } else {
                eprintln!("  {} {}", "Warning:".yellow(), error_msg);
            }
        }
    }

    // Show parameters
    if !hydra_object.parameters.is_empty() {
        eprintln!(
            "  {} {} parameters",
            "Parameters:".dimmed(),
            hydra_object.parameters.len()
        );
        for param in &hydra_object.parameters {
            match param {
                hydrust::yaml_parser::Parameter::Keyword { key, line, .. } => {
                    eprintln!("    - {} (line {})", key.cyan(), line + 1);
                }
                hydrust::yaml_parser::Parameter::Positional { line, .. } => {
                    eprintln!("    - {} (line {})", "<positional>".cyan(), line + 1);
                }
            }
        }
    }
}

fn format_signature_brief(
    sig: &hydrust::python_analyzer::FunctionSignature,
    implicit_param: Option<&str>,
) -> String {
    let params: Vec<String> = sig
        .parameters
        .iter()
        .filter(|p| Some(p.name.as_str()) != implicit_param)
        .map(|p| {
            let mut s = p.name.clone();
            if let Some(ref ty) = p.type_annotation {
                s.push_str(&format!(": {}", ty));
            }
            if p.has_default {
                s.push_str(" = ...");
            }
            s
        })
        .collect();
    format!("({})", params.join(", "))
}

fn severity_label(diagnostic: &Diagnostic) -> &'static str {
    match diagnostic.severity {
        Some(DiagnosticSeverity::ERROR) => "error",
        Some(DiagnosticSeverity::WARNING) => "warning",
        Some(DiagnosticSeverity::INFORMATION) => "info",
        Some(DiagnosticSeverity::HINT) => "hint",
        _ => "unknown",
    }
}

/// `severity_label` spells `INFORMATION` as `info` for the compact output; the
/// JSON field has always been `information` and consumers match on it.
fn json_severity_label(diagnostic: &Diagnostic) -> &'static str {
    match severity_label(diagnostic) {
        "info" => "information",
        other => other,
    }
}

fn diagnostic_code(diagnostic: &Diagnostic) -> String {
    match &diagnostic.code {
        Some(tower_lsp::lsp_types::NumberOrString::String(s)) => s.clone(),
        Some(tower_lsp::lsp_types::NumberOrString::Number(n)) => n.to_string(),
        None => String::new(),
    }
}

/// Totals across every checked file, used by the summary lines.
struct Totals {
    files: usize,
    failures: usize,
    errors: usize,
    warnings: usize,
    other: usize,
}

impl Totals {
    fn from_reports(reports: &[FileReport]) -> Self {
        let mut totals = Totals {
            files: reports.len(),
            failures: 0,
            errors: 0,
            warnings: 0,
            other: 0,
        };
        for report in reports {
            if report.failure.is_some() {
                totals.failures += 1;
            }
            for diag in &report.diagnostics {
                match diag.severity {
                    Some(DiagnosticSeverity::ERROR) => totals.errors += 1,
                    Some(DiagnosticSeverity::WARNING) => totals.warnings += 1,
                    _ => totals.other += 1,
                }
            }
        }
        totals
    }

    fn is_clean(&self) -> bool {
        self.failures == 0 && self.errors == 0 && self.warnings == 0 && self.other == 0
    }
}

fn output_pretty(reports: &[FileReport]) {
    for report in reports {
        if let Some(ref failure) = report.failure {
            println!("\n{} {}", "Diagnostics for".bold(), report.path.underline());
            println!("{}", "─".repeat(60));
            println!("\n  {} {}", "ERROR".red().bold(), failure);
            continue;
        }

        if report.diagnostics.is_empty() {
            println!("\n{} {} - no issues found", "✓".green().bold(), report.path);
            continue;
        }

        println!("\n{} {}", "Diagnostics for".bold(), report.path.underline());
        println!("{}", "─".repeat(60));

        for diag in &report.diagnostics {
            let severity_str = match diag.severity {
                Some(DiagnosticSeverity::ERROR) => "ERROR".red().bold(),
                Some(DiagnosticSeverity::WARNING) => "WARNING".yellow().bold(),
                Some(DiagnosticSeverity::INFORMATION) => "INFO".blue().bold(),
                Some(DiagnosticSeverity::HINT) => "HINT".dimmed().bold(),
                _ => "UNKNOWN".dimmed().bold(),
            };

            let code = diagnostic_code(diag);
            let code_str = if code.is_empty() {
                String::new()
            } else {
                format!("[{}]", code)
            };

            println!(
                "\n  {} {} at line {}:{}",
                severity_str,
                code_str.dimmed(),
                diag.range.start.line + 1,
                diag.range.start.character + 1
            );
            println!("  {}", diag.message);
        }
    }

    // Summary
    let totals = Totals::from_reports(reports);

    let mut parts = Vec::new();
    if totals.errors > 0 {
        parts.push(format!(
            "{} error(s)",
            totals.errors.to_string().red().bold()
        ));
    }
    if totals.warnings > 0 {
        parts.push(format!(
            "{} warning(s)",
            totals.warnings.to_string().yellow().bold()
        ));
    }
    if totals.other > 0 {
        parts.push(format!("{} other(s)", totals.other.to_string().blue()));
    }
    if totals.failures > 0 {
        parts.push(format!(
            "{} file(s) that could not be checked",
            totals.failures.to_string().red().bold()
        ));
    }
    if totals.is_clean() {
        parts.push(format!("{}", "No issues".green()));
    }

    println!("\n{}", "─".repeat(60));
    println!(
        "{}{} across {} file(s)",
        "Summary: ".bold(),
        parts.join(", "),
        totals.files
    );
}

/// Inclusive 1-based end line and column: the LSP end is exclusive 0-based, so
/// the last covered column is `end.character`. A single-line range is widened
/// to cover at least its start column; columns on different lines are not
/// comparable. A multi-line range ending at column 0 of a line really ends at
/// the end of the previous line, whose length is not known here, so the column
/// is omitted.
fn inclusive_end(diag: &Diagnostic) -> (u32, Option<u32>) {
    let range = diag.range;
    if range.start.line == range.end.line {
        (
            range.end.line + 1,
            Some(range.end.character.max(range.start.character + 1)),
        )
    } else if range.end.character == 0 {
        (range.end.line, None)
    } else {
        (range.end.line + 1, Some(range.end.character))
    }
}

fn output_json(reports: &[FileReport]) -> anyhow::Result<()> {
    let totals = Totals::from_reports(reports);
    let output = serde_json::json!({
        "files": reports.iter().map(|report| {
            serde_json::json!({
                "file": report.path.to_string(),
                "error": report.failure,
                "error_code": report.failure_code,
                "diagnostics": report.diagnostics.iter().map(|d| {
                    let (end_line, end_column) = inclusive_end(d);
                    serde_json::json!({
                        "severity": json_severity_label(d),
                        "code": diagnostic_code(d),
                        "line": d.range.start.line + 1,
                        "column": d.range.start.character + 1,
                        "end_line": end_line,
                        "end_column": end_column,
                        "message": d.message.clone(),
                    })
                }).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
        "summary": {
            "files": totals.files,
            "failed_files": totals.failures,
            "total": totals.errors + totals.warnings + totals.other,
            "errors": totals.errors,
            "warnings": totals.warnings,
            "other": totals.other,
        }
    });

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn output_compact(reports: &[FileReport]) {
    for report in reports {
        if let Some(ref failure) = report.failure {
            println!(
                "{}:1:1: error: [{}] {}",
                report.path,
                report.failure_code.unwrap_or("check-error"),
                failure.replace('\n', " ")
            );
            continue;
        }

        for diag in &report.diagnostics {
            println!(
                "{}:{}:{}: {}: [{}] {}",
                report.path,
                diag.range.start.line + 1,
                diag.range.start.character + 1,
                severity_label(diag),
                diagnostic_code(diag),
                diag.message.replace('\n', " ")
            );
        }
    }

    let totals = Totals::from_reports(reports);
    if totals.is_clean() {
        println!("OK - no issues found across {} file(s)", totals.files);
    }
}

/// Escape a value for the body of a GitHub Actions workflow command.
///
/// See <https://docs.github.com/actions/reference/workflow-commands-for-github-actions>.
fn escape_workflow_data(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Escape a value used as a workflow command property, which additionally may
/// not contain the `:` and `,` separators.
fn escape_workflow_property(value: &str) -> String {
    escape_workflow_data(value)
        .replace(':', "%3A")
        .replace(',', "%2C")
}

fn output_github(reports: &[FileReport]) {
    // GitHub silently drops an annotation whose `file=` is not relative to the
    // repository root, so say so once rather than exiting 1 with nothing shown.
    if reports.iter().any(|r| Path::new(&r.path).is_absolute()) {
        eprintln!(
            "{}: some paths are outside the current directory; GitHub will not \
             attach their annotations. Run hydrust from the repository root.",
            "warning".yellow().bold()
        );
    }

    for report in reports {
        let file = escape_workflow_property(&report.path);

        if let Some(ref failure) = report.failure {
            println!(
                "::error file={},line=1,col=1,title=hydrust::{}",
                file,
                escape_workflow_data(failure)
            );
            continue;
        }

        for diag in &report.diagnostics {
            // GitHub only renders error, warning and notice.
            let level = match diag.severity {
                Some(DiagnosticSeverity::ERROR) => "error",
                Some(DiagnosticSeverity::WARNING) => "warning",
                _ => "notice",
            };

            let code = diagnostic_code(diag);
            let title = if code.is_empty() {
                "hydrust".to_string()
            } else {
                format!("hydrust({})", escape_workflow_property(&code))
            };

            let (end_line, end_column) = inclusive_end(diag);
            let end_column = end_column
                .map(|column| format!(",endColumn={column}"))
                .unwrap_or_default();
            println!(
                "::{} file={},line={},col={},endLine={}{},title={}::{}",
                level,
                file,
                diag.range.start.line + 1,
                diag.range.start.character + 1,
                end_line,
                end_column,
                title,
                escape_workflow_data(&diag.message)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::{Position, Range};

    fn diag(start: (u32, u32), end: (u32, u32)) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(start.0, start.1), Position::new(end.0, end.1)),
            ..Default::default()
        }
    }

    #[test]
    fn inclusive_end_single_line() {
        assert_eq!(inclusive_end(&diag((0, 4), (0, 9))), (1, Some(9)));
        assert_eq!(inclusive_end(&diag((0, 4), (0, 4))), (1, Some(5)));
    }

    #[test]
    fn inclusive_end_multi_line_ignores_start_column() {
        assert_eq!(inclusive_end(&diag((0, 10), (2, 3))), (3, Some(3)));
    }

    #[test]
    fn inclusive_end_multi_line_at_column_zero_is_previous_line() {
        assert_eq!(inclusive_end(&diag((0, 10), (2, 0))), (2, None));
    }
}
