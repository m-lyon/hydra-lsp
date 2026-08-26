# Hydrust - Copilot Instructions

## Project Overview

This is a **Language Server Protocol (LSP) implementation** for [Hydra](https://hydra.cc/) configuration files. It provides IDE features (hover, go-to-definition, diagnostics, semantic tokens) for YAML files that use Hydra's `_target_` pattern to instantiate Python objects.

**Monorepo structure:**

- `hydra-lsp/` - Rust LSP server and CLI tool (`hydrust`)
- `hydra-lsp-vscode/` - TypeScript VS Code extension that wraps the server

## Architecture

### Data Flow

1. **YAML Parsing** ([yaml_parser.rs](../src/yaml_parser.rs)) - Extracts `_target_` references and parameters from Hydra configs
2. **Python Resolution** ([python_analyzer.rs](../src/python_analyzer.rs)) - Resolves `_target_` strings (e.g., `torch.nn.Linear`) to Python source files using ruff's `ty_module_resolver`
3. **Import Resolution** ([import_resolver.rs](../src/import_resolver.rs)) - Follows Python imports/re-exports to find actual symbol definitions
4. **LSP Backend** ([backend.rs](../src/backend.rs)) - Implements LSP protocol via `tower-lsp` crate

### Key Design Decisions

- Uses **ruff crates** (`ruff_python_parser`, `ruff_python_ast`, `ty_module_resolver`, `ty_python_semantic`) for Python parsing/analysis - see `github-packages/ruff/` for vendored reference
- Python environment discovery follows priority: configured interpreter → VIRTUAL_ENV → CONDA_PREFIX → `.venv` → system Python
- Full document sync (not incremental) - see `TextDocumentSyncKind::FULL` in backend.rs

## Development Workflow

### Building & Testing

```bash
# Build and run tests
cargo build
cargo test

# Run specific test file
cargo test --test hover
cargo test --test diagnostics

# Build release and copy to VS Code extension
make build-vscode
```

### Snapshot Testing

Tests use **insta** for snapshot testing. Snapshots live in `tests/snapshots/`.

```bash
# After test changes, review snapshots interactively
cargo install cargo-insta
cargo test
cargo insta review
```

### Test Workspaces

Test fixtures in `tests/workspace/` contain YAML configs and Python modules:

- `simple/` - Basic `_target_` resolution
- `diagnostics/` - Error cases (missing params, unknown targets)
- `nested/` - Nested configurations
- `reexport/` - Python re-export patterns

### VS Code Extension Development

```bash
cd hydra-lsp-vscode
npm install
npm run watch  # Compile TypeScript in watch mode
# Press F5 in VS Code to launch Extension Development Host
```

## Code Patterns

### Adding LSP Features

Implement methods in `HydraLspBackend` in [backend.rs](../src/backend.rs):

```rust
#[tower_lsp::async_trait]
impl LanguageServer for HydraLspBackend {
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> { ... }
}
```

Register capabilities in `initialize()` method.

### Python Symbol Extraction

Use visitor pattern from `ruff_python_ast`. See `ClassExtractor` and `FunctionExtractor` in [python_analyzer.rs](../src/python_analyzer.rs#L413):

```rust
impl<'a> Visitor<'a> for ClassExtractor<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) { ... }
}
```

### Diagnostics

Create diagnostics in [diagnostics.rs](../src/diagnostics.rs) with codes like `"module-not-found"`, `"symbol-not-found"`, `"unknown-parameter"`.
