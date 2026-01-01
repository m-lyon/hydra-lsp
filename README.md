# Hydra LSP (Language Server Protocol)

A Language Server Protocol implementation for [Hydra](https://hydra.cc/) configuration files, written in Rust.

## Features

### Currently Implemented (v0.1.0)

- ✅ **Hydra File Detection**: Automatically detects Hydra YAML files using:
  - Comment markers (`# @hydra` or `# hydra:`)
  - Presence of `_target_` keyword
- ✅ **YAML Parsing**: Extracts `_target_` references and their parameters
- ✅ **Python Module Resolution**: Resolves Python modules using:
  - Workspace-relative paths
  - Python interpreter's `sys.path` (when configured)
  - Support for virtual environments and custom Python installations
- ✅ **Function/Class Signature Extraction**: Parses Python files to extract:
  - Function signatures with parameters, types, and defaults
  - Class information with `__init__` signatures
  - Docstrings for hover documentation
- ✅ **Hover Support**: Shows rich information when hovering over `_target_` values:
  - Function signatures with parameter details
  - Class information and docstrings
  - Type annotations
- ✅ **Signature Help**: Shows parameter information while typing function arguments
- ✅ **Go to Definition**: Jump from YAML `_target_` to Python source file
- ✅ **Diagnostics**: Parameter validation including:
  - Unknown parameters (unless `**kwargs` present)
  - Missing required parameters
  - Basic `_target_` format validation

### Planned Features

- 🔄 **Type Validation**: Validate YAML values against Python type annotations
- 🔄 **Smart Autocomplete**: Suggest Python classes/functions and parameters
- 🔄 **Semantic Tokens**: Syntax highlighting for Python references
- 🔄 **Configuration UI**: Better integration for Python interpreter selection

## Architecture

```bash
hydra-lsp/
├── src/
│   ├── main.rs              # LSP server entry point
│   ├── backend.rs           # LanguageServer implementation
│   ├── document.rs          # Document state management
│   ├── yaml_parser.rs       # YAML parsing and _target_ extraction
│   ├── python_analyzer.rs   # Python analysis (placeholder)
│   └── diagnostics.rs       # Validation and error reporting
└── Cargo.toml
```

## Building

```bash
cargo build --release
```

The binary will be at `target/release/hydra-lsp`.

## Running

The LSP server communicates over stdin/stdout:

```bash
cargo run
```

### Configuring Python Interpreter

The LSP can use a Python interpreter to resolve modules from virtual environments or custom Python installations. The interpreter path is stored in the `HydraLspBackend.python_interpreter` field.

**Default Behavior**: When no interpreter is specified, the LSP will:

1. Search for Python modules in workspace-relative paths
2. Look in standard Python paths (if Python is in PATH)

**Virtual Environment Support**: To enable module discovery from a virtual environment, the LSP client (e.g., VS Code extension) should:

1. Detect or prompt for the Python interpreter path
2. Update the `python_interpreter` configuration (via LSP initialization params or workspace/didChangeConfiguration)

The interpreter is used to query `sys.path` by running:

```python
python -c "import sys; print('\\n'.join(sys.path))"
```

This allows the LSP to discover:

- Packages installed in virtual environments
- Site-packages from custom Python installations  
- User-installed packages
- Development packages (editable installs)

**Example VS Code Extension Integration**:
```typescript
// Detect Python interpreter (use VS Code Python extension API or prompt user)
const pythonPath = await detectPythonInterpreter();

// Pass to LSP server (implementation depends on your extension design)
// Option 1: Via initialization params
// Option 2: Via workspace configuration
// Option 3: Via custom notification to update the RwLock
```

## Testing

Run the test suite:

```bash
cargo test
```

## Integration with VS Code

To use this LSP with VS Code, you'll need to create a VS Code extension. See the VS Code Extension API documentation for details on creating a language client extension.

Basic configuration in a VS Code extension:

```typescript
import { LanguageClient, ServerOptions, LanguageClientOptions } from 'vscode-languageclient/node';

const serverOptions: ServerOptions = {
    command: '/path/to/hydra-lsp',
    args: []
};

const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: 'file', language: 'yaml' }],
};

const client = new LanguageClient(
    'hydra-lsp',
    'Hydra LSP',
    serverOptions,
    clientOptions
);

client.start();
```

## Python Analysis

The LSP uses [ruff](https://github.com/astral-sh/ruff) for Python parsing and AST analysis:

```toml
[dependencies]
# Python parsing and analysis
ruff_python_parser = { git = "https://github.com/astral-sh/ruff", tag = "v0.9.0" }
ruff_python_ast = { git = "https://github.com/astral-sh/ruff", tag = "v0.9.0" }
ruff_text_size = { git = "https://github.com/astral-sh/ruff", tag = "v0.9.0" }
```

The Python analyzer (`src/python_analyzer.rs`) provides:

- **Module Resolution**: Finds Python files from module paths using workspace and `sys.path`
- **AST Parsing**: Parses Python files into abstract syntax trees
- **Signature Extraction**: Extracts function/class signatures using AST visitor pattern
- **Type Information**: Captures type annotations and default values

See [PYTHON_ANALYSIS_TOOLS.md](PYTHON_ANALYSIS_TOOLS.md) for detailed information about the Python analysis implementation.

## Development Plan

See the development plan document for the full implementation roadmap.

## Example Hydra Config

```yaml
# @hydra
# This file will be recognized as a Hydra configuration

model:
  _target_: myproject.models.transformer.TransformerModel
  hidden_size: 256
  num_layers: 12
  dropout: 0.1

optimizer:
  _target_: torch.optim.Adam
  lr: 0.001
  weight_decay: 0.0001
```

## Contributing

Contributions are welcome! Areas of focus:

1. **Python Module Resolution**: Implement `PythonAnalyzer::resolve_module()` using `ty_module_resolver`
2. **Signature Extraction**: Parse Python files and extract function/class signatures
3. **Smart Completions**: Improve completion context detection and suggestions
4. **Type Validation**: Validate YAML values against Python type annotations
5. **Performance**: Add caching for parsed Python files and resolved modules

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Acknowledgments

- Built with [tower-lsp](https://github.com/ebkalderon/tower-lsp) framework
- Python analysis design based on [ruff](https://github.com/astral-sh/ruff) and [ty](https://github.com/astral-sh/ty)
- Designed for [Hydra](https://hydra.cc/) configuration management

## References

- [Language Server Protocol Specification](https://microsoft.github.io/language-server-protocol/)
- [Hydra Documentation](https://hydra.cc/docs/intro/)
- [Tower-LSP Documentation](https://docs.rs/tower-lsp/)
