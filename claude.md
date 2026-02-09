# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Hydrust is a Language Server for [Hydra](https://hydra.cc/) configuration files. It provides IDE features (hover, go-to-definition, diagnostics, semantic tokens, signature help) for YAML files that use Hydra's `_target_` pattern to instantiate Python objects.

The project also includes a CLI tool `hydra-check` for validating Hydra configs from the command line.

## Build and Test Commands

```bash
# Build
cargo build

# Run all tests
cargo test

# Run a specific test file
cargo test --test hover
cargo test --test diagnostics

# Run with output visible
cargo test -- --nocapture

# Snapshot testing (uses insta)
cargo install cargo-insta
cargo test
cargo insta review

# Build release and copy to VS Code extension
make build-vscode
```

## Architecture

### Data Flow

1. **YAML Parsing** (`yaml_parser.rs`) - Extracts `_target_` references and parameters from Hydra configs
2. **Python Resolution** (`python_analyzer.rs`) - Resolves `_target_` strings (e.g., `torch.nn.Linear`) to Python source files using ruff's `ty_module_resolver`
3. **Import Resolution** (`import_resolver.rs`) - Follows Python imports/re-exports to find actual symbol definitions
4. **LSP Backend** (`backend.rs`) - Implements LSP protocol via `tower-lsp` crate
5. **Diagnostics** (`diagnostics.rs`) - Parameter validation with codes like `"module-not-found"`, `"symbol-not-found"`, `"unknown-parameter"`

### Key Dependencies

- Uses **ruff crates** (`ruff_python_parser`, `ruff_python_ast`, `ty_module_resolver`, `ty_python_semantic`) for Python parsing/analysis
- `tower-lsp` for LSP protocol implementation
- `insta` for snapshot testing

### Python Environment Discovery

Priority order: configured interpreter → VIRTUAL_ENV → CONDA_PREFIX → `.venv` → system Python

### Adding LSP Features

Implement methods in `HydraLspBackend` in `backend.rs` and register capabilities in the `initialize()` method.

### Python Symbol Extraction

Uses visitor pattern from `ruff_python_ast`. See `ClassExtractor` and `FunctionExtractor` in `python_analyzer.rs`.

## Test Structure

- Test fixtures in `tests/workspace/` contain YAML configs and Python modules
- Snapshots stored in `tests/snapshots/`
- Uses `TestContext` pattern for integration tests
