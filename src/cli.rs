//! CLI tool for diagnosing Hydra YAML configuration files.
//!
//! This tool parses Hydra YAML files and outputs diagnostics to help debug
//! issues with `_target_` resolution and parameter validation.

use std::fs;
use std::path::{Path, PathBuf};

use clap::{Parser, ValueEnum};
use colored::Colorize;
use tower_lsp::lsp_types::DiagnosticSeverity;
use tracing::{Level, debug, error, info, warn};

use hydra_lsp::diagnostics::validate_document;
use hydra_lsp::python_analyzer::PythonAnalyzer;
use hydra_lsp::yaml_parser::YamlParser;

/// CLI tool for diagnosing Hydra YAML configuration files
#[derive(Parser, Debug)]
#[command(name = "hydra-check")]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the YAML file to check
    #[arg(required = true)]
    file: PathBuf,

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
    #[arg(short, long, value_enum, default_value = "pretty")]
    format: OutputFormat,

    /// Show detailed resolution steps for each target
    #[arg(long)]
    trace_resolution: bool,
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
}

fn main() {
    let args = Args::parse();

    // Initialize tracing with the specified verbosity
    let level: Level = args.verbosity.into();
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .with_ansi(true)
        .init();

    info!("hydra-check starting");
    debug!("Arguments: {:?}", args);

    // Run the main logic and handle errors
    match run(&args) {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            error!("Fatal error: {}", e);
            eprintln!("{}: {}", "Error".red().bold(), e);
            std::process::exit(2);
        }
    }
}

fn run(args: &Args) -> anyhow::Result<i32> {
    // Validate file exists
    if !args.file.exists() {
        anyhow::bail!("File not found: {}", args.file.display());
    }

    // Resolve absolute paths
    let file_path = args.file.canonicalize()?;
    info!("Checking file: {}", file_path.display());

    let workspace_root = if let Some(ref ws) = args.workspace {
        Some(ws.canonicalize()?)
    } else {
        // Default to parent directory of the file
        file_path.parent().map(PathBuf::from)
    };

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

    // Read file content
    let content = fs::read_to_string(&file_path)?;
    debug!("File content length: {} bytes", content.len());

    // Check if it's a Hydra file
    if !YamlParser::is_hydra_file(&content) {
        warn!("File does not appear to be a Hydra configuration file");
        println!(
            "{}: File does not contain Hydra markers (# @hydra, # @package) or _target_ keys",
            "Warning".yellow().bold()
        );
    }

    // Parse YAML and extract targets
    info!("Parsing YAML content...");
    let (targets, _line_map) = match YamlParser::parse(&content) {
        Ok(result) => result,
        Err(e) => {
            error!("Failed to parse YAML: {}", e);
            println!("{}: Failed to parse YAML: {}", "Error".red().bold(), e);
            return Ok(1);
        }
    };

    info!("Found {} _target_ definitions", targets.len());

    // If trace_resolution is enabled, show detailed info for each target
    if args.trace_resolution {
        println!("\n{}", "=== Target Resolution Trace ===".cyan().bold());
        for (i, target) in targets.iter().enumerate() {
            trace_target_resolution(
                i,
                target,
                workspace_root.as_deref(),
                python_interpreter.as_deref(),
            );
        }
        println!();
    }

    // Run diagnostics
    info!("Running diagnostics...");
    let diagnostics = validate_document(
        targets.clone(),
        workspace_root.as_deref(),
        python_interpreter.as_deref(),
    );

    // Output results
    match args.format {
        OutputFormat::Pretty => output_pretty(&file_path, &diagnostics),
        OutputFormat::Json => output_json(&file_path, &diagnostics)?,
        OutputFormat::Compact => output_compact(&file_path, &diagnostics),
    }

    // Return exit code: 0 if no errors, 1 if there are errors
    let error_count = diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .count();

    if error_count > 0 {
        info!("Found {} error(s)", error_count);
        Ok(1)
    } else {
        info!("No errors found");
        Ok(0)
    }
}

fn trace_target_resolution(
    index: usize,
    target: &hydra_lsp::yaml_parser::TargetInfo,
    workspace_root: Option<&Path>,
    python_interpreter: Option<&str>,
) {
    println!(
        "\n{} [{}] {} (line {})",
        "Target".blue().bold(),
        index + 1,
        target.value.yellow(),
        target.line + 1
    );

    // Try to extract definition info
    match PythonAnalyzer::extract_definition_info(&target.value, workspace_root, python_interpreter)
    {
        Ok((def_info, file_path, module_path, symbol_name)) => {
            println!("  {} {}", "Module:".dimmed(), module_path);
            println!("  {} {}", "Symbol:".dimmed(), symbol_name);
            println!("  {} {}", "Definition found:".green(), file_path.display());

            match def_info {
                hydra_lsp::python_analyzer::DefinitionInfo::Function(sig) => {
                    println!("  {} Function", "Type:".dimmed());
                    println!(
                        "  {} {}",
                        "Signature:".dimmed(),
                        format_signature_brief(&sig)
                    );
                }
                hydra_lsp::python_analyzer::DefinitionInfo::Class(class_info) => {
                    println!("  {} Class", "Type:".dimmed());
                    if let Some(ref init_sig) = class_info.init_signature {
                        println!(
                            "  {} {}",
                            "__init__:".dimmed(),
                            format_signature_brief(init_sig)
                        );
                    } else {
                        println!("  {} (no __init__ found)", "__init__:".dimmed());
                    }
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
    if !target.parameters.is_empty() {
        println!(
            "  {} {} parameters",
            "Parameters:".dimmed(),
            target.parameters.len()
        );
        for param in &target.parameters {
            println!("    - {} (line {})", param.key.cyan(), param.line + 1);
        }
    }
}

fn format_signature_brief(sig: &hydra_lsp::python_analyzer::FunctionSignature) -> String {
    let params: Vec<String> = sig
        .parameters
        .iter()
        .filter(|p| p.name != "self")
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

fn output_pretty(file_path: &Path, diagnostics: &[tower_lsp::lsp_types::Diagnostic]) {
    if diagnostics.is_empty() {
        println!(
            "\n{} {} - no issues found",
            "✓".green().bold(),
            file_path.display()
        );
        return;
    }

    println!(
        "\n{} {}",
        "Diagnostics for".bold(),
        file_path.display().to_string().underline()
    );
    println!("{}", "─".repeat(60));

    for diag in diagnostics {
        let severity_str = match diag.severity {
            Some(DiagnosticSeverity::ERROR) => "ERROR".red().bold(),
            Some(DiagnosticSeverity::WARNING) => "WARNING".yellow().bold(),
            Some(DiagnosticSeverity::INFORMATION) => "INFO".blue().bold(),
            Some(DiagnosticSeverity::HINT) => "HINT".dimmed().bold(),
            _ => "UNKNOWN".dimmed().bold(),
        };

        let code_str = match &diag.code {
            Some(tower_lsp::lsp_types::NumberOrString::String(s)) => format!("[{}]", s),
            Some(tower_lsp::lsp_types::NumberOrString::Number(n)) => format!("[{}]", n),
            None => String::new(),
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

    // Summary
    let error_count = diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .count();
    let warning_count = diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::WARNING))
        .count();
    let other_count = diagnostics.len() - error_count - warning_count;

    println!("\n{}", "─".repeat(60));
    print!("{}", "Summary: ".bold());
    if error_count > 0 {
        print!("{} error(s)", error_count.to_string().red().bold());
    }
    if warning_count > 0 {
        if error_count > 0 {
            print!(", ");
        }
        print!("{} warning(s)", warning_count.to_string().yellow().bold());
    }
    if other_count > 0 {
        if error_count > 0 || warning_count > 0 {
            print!(", ");
        }
        print!("{} other(s)", other_count.to_string().blue());
    }
    if error_count == 0 && warning_count == 0 && other_count == 0 {
        print!("{}", "No issues".green());
    }
    println!();
}

fn output_json(
    file_path: &Path,
    diagnostics: &[tower_lsp::lsp_types::Diagnostic],
) -> anyhow::Result<()> {
    let output = serde_json::json!({
        "file": file_path.display().to_string(),
        "diagnostics": diagnostics.iter().map(|d| {
            serde_json::json!({
                "severity": match d.severity {
                    Some(DiagnosticSeverity::ERROR) => "error",
                    Some(DiagnosticSeverity::WARNING) => "warning",
                    Some(DiagnosticSeverity::INFORMATION) => "information",
                    Some(DiagnosticSeverity::HINT) => "hint",
                    _ => "unknown",
                },
                "code": match &d.code {
                    Some(tower_lsp::lsp_types::NumberOrString::String(s)) => s.clone(),
                    Some(tower_lsp::lsp_types::NumberOrString::Number(n)) => n.to_string(),
                    None => String::new(),
                },
                "line": d.range.start.line + 1,
                "column": d.range.start.character + 1,
                "end_line": d.range.end.line + 1,
                "end_column": d.range.end.character + 1,
                "message": d.message.clone(),
            })
        }).collect::<Vec<_>>(),
        "summary": {
            "total": diagnostics.len(),
            "errors": diagnostics.iter().filter(|d| d.severity == Some(DiagnosticSeverity::ERROR)).count(),
            "warnings": diagnostics.iter().filter(|d| d.severity == Some(DiagnosticSeverity::WARNING)).count(),
        }
    });

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn output_compact(file_path: &Path, diagnostics: &[tower_lsp::lsp_types::Diagnostic]) {
    if diagnostics.is_empty() {
        println!("{}:0:0: OK - no issues found", file_path.display());
        return;
    }

    for diag in diagnostics {
        let severity = match diag.severity {
            Some(DiagnosticSeverity::ERROR) => "error",
            Some(DiagnosticSeverity::WARNING) => "warning",
            Some(DiagnosticSeverity::INFORMATION) => "info",
            Some(DiagnosticSeverity::HINT) => "hint",
            _ => "unknown",
        };

        let code = match &diag.code {
            Some(tower_lsp::lsp_types::NumberOrString::String(s)) => s.clone(),
            Some(tower_lsp::lsp_types::NumberOrString::Number(n)) => n.to_string(),
            None => String::new(),
        };

        println!(
            "{}:{}:{}: {}: [{}] {}",
            file_path.display(),
            diag.range.start.line + 1,
            diag.range.start.character + 1,
            severity,
            code,
            diag.message.replace('\n', " ")
        );
    }
}
