//! CLI tool for diagnosing Hydra YAML configuration files.
//!
//! This tool parses Hydra YAML files and outputs diagnostics to help debug
//! issues with `_target_` resolution and parameter validation.

use std::fmt;
use std::fs;
use std::io::stderr;
use std::path::{Path, PathBuf};
use std::process;

use clap::{Args, Parser, Subcommand, ValueEnum};
use colored::Colorize;
use ignore::WalkBuilder;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity};
use tracing::{Level, debug, error, info, warn};

use hydra_lsp::database::HydraDatabase;
use hydra_lsp::diagnostics::{DiagnosticRule, validate_document};
use hydra_lsp::python_analyzer::PythonAnalyzer;
use hydra_lsp::python_cache::PythonConfig;
use hydra_lsp::yaml_parser::YamlParser;

use std::collections::HashSet;

/// CLI tool for diagnosing Hydra YAML configuration files
#[derive(Parser)]
#[command(name = "hydrust")]
#[command(author, version, long_about = None)]
#[command(about = "Check Hydra YAML configuration files for diagnostics")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check Hydra YAML configuration files for diagnostics.
    Check(CheckCommand),
}

#[derive(Args)]
struct CheckCommand {
    /// Files or directories to check. Directories are searched recursively for
    /// `.yaml` and `.yml` files, honouring `.gitignore`.
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

    /// Show detailed resolution steps for each target
    #[arg(long)]
    trace_resolution: bool,

    /// Disable a diagnostic rule (can be repeated). For valid rules, see
    /// `diagnostics::DiagnosticRule::all()`
    #[arg(long = "disable-rule", value_name = "RULE")]
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
    let Command::Check(args) = &cli.command;

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
    if targets.is_empty() {
        anyhow::bail!("No YAML files found in the given path(s)");
    }
    info!("Checking {} file(s)", targets.len());

    let workspace_root = resolve_workspace_root(args, &targets)?;
    if let Some(ref ws) = workspace_root {
        info!("Workspace root: {}", ws.display());
    }

    let python_interpreter = args
        .python
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    if let Some(ref py) = python_interpreter {
        info!("Python interpreter: {}", py);
    }

    let disabled_rules = parse_disabled_rules(&args.disable_rules);

    // One salsa db + PythonConfig shared across every file, so module
    // resolution done for the first file is reused by the rest.
    let db_root = workspace_root
        .as_deref()
        .and_then(|p| p.to_str())
        .unwrap_or(".");
    let db = HydraDatabase::new(ruff_db::system::SystemPath::new(db_root));
    let python_config = PythonConfig::new(
        &db,
        workspace_root
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        python_interpreter.clone(),
    );

    let mut reports = Vec::with_capacity(targets.len());
    for target in &targets {
        if let Some(report) = check_target(target, args, &db, python_config, &disabled_rules) {
            reports.push(report);
        }
    }

    match args.format {
        OutputFormat::Pretty => output_pretty(&reports),
        OutputFormat::Json => output_json(&reports)?,
        OutputFormat::Compact => output_compact(&reports),
        OutputFormat::Github => output_github(&reports),
    }

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

/// Expand the command-line paths into the set of files to check.
///
/// Files are taken as given; directories are walked recursively for `.yaml` and
/// `.yml` files, respecting `.gitignore`. Duplicates are dropped, so overlapping
/// arguments (`config.yaml conf/`) check each file once.
fn collect_targets(paths: &[PathBuf]) -> anyhow::Result<Vec<CheckTarget>> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    let cwd = std::env::current_dir()
        .and_then(|dir| dir.canonicalize())
        .ok();

    for path in paths {
        if !path.exists() {
            anyhow::bail!("Path not found: {}", path.display());
        }

        if path.is_file() {
            let canonical = path.canonicalize()?;
            if seen.insert(canonical.clone()) {
                targets.push(CheckTarget {
                    display: display_path(&canonical, cwd.as_deref()),
                    path: canonical,
                    explicit: true,
                });
            }
            continue;
        }

        // `require_git(false)` so that `.gitignore` is honoured whether or not
        // the tree happens to be a git checkout; otherwise which files get
        // checked would depend on the presence of `.git`. Sorted so that output
        // is reproducible across runs and platforms.
        let walk = WalkBuilder::new(path)
            .require_git(false)
            .sort_by_file_path(|a, b| a.cmp(b))
            .build();
        for entry in walk {
            let entry = entry?;
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            if !is_yaml_file(entry.path()) {
                continue;
            }
            let canonical = entry.path().canonicalize()?;
            if seen.insert(canonical.clone()) {
                targets.push(CheckTarget {
                    display: display_path(&canonical, cwd.as_deref()),
                    path: canonical,
                    explicit: false,
                });
            }
        }
    }

    Ok(targets)
}

/// Render `path` relative to `base` when it sits underneath it, otherwise as
/// the absolute path.
fn display_path(path: &Path, base: Option<&Path>) -> String {
    base.and_then(|base| path.strip_prefix(base).ok())
        .unwrap_or(path)
        .display()
        .to_string()
}

fn is_yaml_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml"))
}

/// Pick the root used for Python module resolution.
///
/// An explicit `--workspace` always wins. Otherwise a single file argument
/// resolves against its own directory, which keeps the common
/// `hydrust check config.yaml` case working without configuration; anything
/// broader resolves against the current directory, since there is no one
/// parent directory that is right for every file.
fn resolve_workspace_root(
    args: &CheckCommand,
    targets: &[CheckTarget],
) -> anyhow::Result<Option<PathBuf>> {
    if let Some(ref ws) = args.workspace {
        return Ok(Some(ws.canonicalize()?));
    }

    if let [single] = targets
        && single.explicit
    {
        return Ok(single.path.parent().map(PathBuf::from));
    }

    Ok(Some(std::env::current_dir()?))
}

fn parse_disabled_rules(rules: &[String]) -> HashSet<DiagnosticRule> {
    let mut disabled_rules = HashSet::new();
    for rule_str in rules {
        match DiagnosticRule::from_code(rule_str) {
            Some(rule) => {
                disabled_rules.insert(rule);
            }
            None => {
                warn!("Unknown diagnostic rule: '{}', ignoring", rule_str);
                eprintln!(
                    "{}: Unknown diagnostic rule '{}'. Valid rules: {}",
                    "Warning".yellow().bold(),
                    rule_str,
                    DiagnosticRule::all()
                        .iter()
                        .map(|r| r.as_code())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }
    disabled_rules
}

/// Check a single file. Returns `None` when the file was discovered by walking
/// a directory and turns out not to be a Hydra config.
fn check_target(
    target: &CheckTarget,
    args: &CheckCommand,
    db: &HydraDatabase,
    python_config: PythonConfig,
    disabled_rules: &HashSet<DiagnosticRule>,
) -> Option<FileReport> {
    let file_path = &target.path;
    info!("Checking file: {}", target.display);

    let content = match fs::read_to_string(file_path) {
        Ok(content) => content,
        Err(e) => {
            error!("Failed to read {}: {}", target.display, e);
            return Some(FileReport {
                path: target.display.clone(),
                diagnostics: Vec::new(),
                failure: Some(format!("Failed to read file: {e}")),
            });
        }
    };
    debug!("File content length: {} bytes", content.len());

    if !YamlParser::is_hydra_file(&content) {
        if !target.explicit {
            debug!("Skipping non-Hydra file: {}", target.display);
            return None;
        }
        warn!("File does not appear to be a Hydra configuration file");
        eprintln!(
            "{}: {} does not contain Hydra markers (# @hydra, # @package) or _target_ keys",
            "Warning".yellow().bold(),
            target.display
        );
    }

    info!("Parsing YAML content...");
    let parsed_content = match YamlParser::parse(&content) {
        Ok(result) => result,
        Err(e) => {
            error!("Failed to parse YAML: {}", e);
            return Some(FileReport {
                path: target.display.clone(),
                diagnostics: Vec::new(),
                failure: Some(format!("Failed to parse YAML: {e}")),
            });
        }
    };

    info!(
        "Found {} _target_ definitions",
        parsed_content.hydra_objects.len()
    );

    // If trace_resolution is enabled, show detailed info for each target
    if args.trace_resolution {
        println!(
            "\n{} {}",
            "=== Target Resolution Trace ===".cyan().bold(),
            target.display
        );
        for (i, hydra_object) in parsed_content.hydra_objects.iter().enumerate() {
            trace_target_resolution(i, hydra_object, db, python_config);
        }
        println!();
    }

    info!("Running diagnostics...");
    let diagnostics = validate_document(&parsed_content, disabled_rules, db, python_config);

    Some(FileReport {
        path: target.display.clone(),
        diagnostics,
        failure: None,
    })
}

fn trace_target_resolution(
    index: usize,
    hydra_object: &hydra_lsp::yaml_parser::HydraObject,
    db: &HydraDatabase,
    python_config: PythonConfig,
) {
    println!(
        "\n{} [{}] {} (line {})",
        "Target".blue().bold(),
        index + 1,
        hydra_object.target.value.yellow(),
        hydra_object.target.line + 1
    );

    let search_paths = hydra_lsp::python_cache::search_paths_for_config(db, python_config);
    match PythonAnalyzer::extract_definition_info(db, &hydra_object.target.value, search_paths) {
        Ok((def_info, file_path, module_path, symbol_name)) => {
            println!("  {} {}", "Module:".dimmed(), module_path);
            println!("  {} {}", "Symbol:".dimmed(), symbol_name);
            println!("  {} {}", "Definition found:".green(), file_path.display());

            let implicit_param = def_info.implicit_param();
            match &def_info {
                hydra_lsp::python_analyzer::DefinitionInfo::Function(sig) => {
                    println!("  {} Function", "Type:".dimmed());
                    println!(
                        "  {} {}",
                        "Signature:".dimmed(),
                        format_signature_brief(sig, implicit_param)
                    );
                }
                hydra_lsp::python_analyzer::DefinitionInfo::Class(class_info) => {
                    println!("  {} Class", "Type:".dimmed());
                    if let Some(ref init_sig) = class_info.init_signature {
                        println!(
                            "  {} {}",
                            "__init__:".dimmed(),
                            format_signature_brief(init_sig, implicit_param)
                        );
                    } else {
                        println!("  {} (no __init__ found)", "__init__:".dimmed());
                    }
                }
                hydra_lsp::python_analyzer::DefinitionInfo::Method(method_info) => {
                    let method_type = if method_info.is_classmethod {
                        "classmethod"
                    } else if method_info.is_staticmethod {
                        "staticmethod"
                    } else {
                        "method"
                    };
                    println!(
                        "  {} {} ({})",
                        "Type:".dimmed(),
                        method_type,
                        method_info.class_name
                    );
                    println!(
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
                println!("  {} {}", "Error:".red(), error_msg)
            } else {
                println!("  {} {}", "Warning:".yellow(), error_msg);
            }
        }
    }

    // Show parameters
    if !hydra_object.parameters.is_empty() {
        println!(
            "  {} {} parameters",
            "Parameters:".dimmed(),
            hydra_object.parameters.len()
        );
        for param in &hydra_object.parameters {
            match param {
                hydra_lsp::yaml_parser::Parameter::Keyword { key, line, .. } => {
                    println!("    - {} (line {})", key.cyan(), line + 1);
                }
                hydra_lsp::yaml_parser::Parameter::Positional { line, .. } => {
                    println!("    - {} (line {})", "<positional>".cyan(), line + 1);
                }
            }
        }
    }
}

fn format_signature_brief(
    sig: &hydra_lsp::python_analyzer::FunctionSignature,
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
    errors: usize,
    warnings: usize,
    other: usize,
}

impl Totals {
    fn of(reports: &[FileReport]) -> Self {
        let mut totals = Totals {
            files: reports.len(),
            errors: 0,
            warnings: 0,
            other: 0,
        };
        for report in reports {
            if report.failure.is_some() {
                totals.errors += 1;
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
        self.errors == 0 && self.warnings == 0 && self.other == 0
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
    let totals = Totals::of(reports);

    println!("\n{}", "─".repeat(60));
    print!("{}", "Summary: ".bold());
    if totals.errors > 0 {
        print!("{} error(s)", totals.errors.to_string().red().bold());
    }
    if totals.warnings > 0 {
        if totals.errors > 0 {
            print!(", ");
        }
        print!("{} warning(s)", totals.warnings.to_string().yellow().bold());
    }
    if totals.other > 0 {
        if totals.errors > 0 || totals.warnings > 0 {
            print!(", ");
        }
        print!("{} other(s)", totals.other.to_string().blue());
    }
    if totals.is_clean() {
        print!("{}", "No issues".green());
    }
    println!(" across {} file(s)", totals.files);
}

fn output_json(reports: &[FileReport]) -> anyhow::Result<()> {
    let totals = Totals::of(reports);
    let output = serde_json::json!({
        "files": reports.iter().map(|report| {
            serde_json::json!({
                "file": report.path.to_string(),
                "error": report.failure,
                "diagnostics": report.diagnostics.iter().map(|d| {
                    serde_json::json!({
                        "severity": severity_label(d),
                        "code": diagnostic_code(d),
                        "line": d.range.start.line + 1,
                        "column": d.range.start.character + 1,
                        "end_line": d.range.end.line + 1,
                        "end_column": d.range.end.character + 1,
                        "message": d.message.clone(),
                    })
                }).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
        "summary": {
            "files": totals.files,
            "total": totals.errors + totals.warnings + totals.other,
            "errors": totals.errors,
            "warnings": totals.warnings,
        }
    });

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn output_compact(reports: &[FileReport]) {
    for report in reports {
        if let Some(ref failure) = report.failure {
            println!(
                "{}:1:1: error: [] {}",
                report.path,
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

    let totals = Totals::of(reports);
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

            println!(
                "::{} file={},line={},col={},endLine={},endColumn={},title={}::{}",
                level,
                file,
                diag.range.start.line + 1,
                diag.range.start.character + 1,
                diag.range.end.line + 1,
                diag.range.end.character + 1,
                title,
                escape_workflow_data(&diag.message)
            );
        }
    }
}
