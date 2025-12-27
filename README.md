# Hydra LSP (Language Server Protocol)

A Language Server Protocol implementation for [Hydra](https://hydra.cc/) configuration files, written in Rust.

## Features

### Currently Implemented (v0.1.0)

- ✅ **Hydra File Detection**: Automatically detects Hydra YAML files using:
  - Comment markers (`# @hydra` or `# hydra:`)
  - Presence of `_target_` keyword
- ✅ **YAML Parsing**: Extracts `_target_` references and their parameters
- ✅ **Basic Hover Support**: Shows module and symbol information when hovering over `_target_` values
- ✅ **Diagnostics**: Basic validation of `_target_` format
- ✅ **Completion Placeholders**: Framework for target and parameter autocompletion

### Planned Features

- 🔄 **Full Python Analysis** (requires adding ruff/ty dependencies):
  - Parse Python files to extract function/class signatures
  - Resolve Python modules from `_target_` references  
  - Show actual parameter types and docstrings in hover
- 🔄 **Advanced Argument Validation**:
  - Detect unknown parameters (unless `**kwargs` present)
  - Detect missing required parameters
  - Type validation for parameter values
- 🔄 **Smart Autocomplete**:
  - Suggest Python classes/functions when completing `_target_`
  - Suggest parameters based on actual function signatures
  - Filter by current context and partial input
- 🔄 **Go to Definition**: Jump from YAML config to Python source
- 🔄 **Configuration**: Custom Python path, virtual environment support

## Architecture

```
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

## Adding Full Python Analysis

Currently, Python analysis is disabled because the `ruff` and `ty` crates are not published to crates.io. To enable full Python analysis:

1. **Add git dependencies to `Cargo.toml`**:

```toml
[dependencies]
# ... existing dependencies ...

# Python parsing and analysis
ruff_python_parser = { git = "https://github.com/astral-sh/ruff", tag = "v0.9.0" }
ruff_python_ast = { git = "https://github.com/astral-sh/ruff", tag = "v0.9.0" }
ruff_text_size = { git = "https://github.com/astral-sh/ruff", tag = "v0.9.0" }

# Python semantic analysis
ty_module_resolver = { git = "https://github.com/astral-sh/ty" }
ty_python_semantic = { git = "https://github.com/astral-sh/ty" }
ty_project = { git = "https://github.com/astral-sh/ty" }
ty = { git = "https://github.com/astral-sh/ty" }
```

2. **Uncomment Python analysis code in `src/python_analyzer.rs`**

3. **Implement the full analysis pipeline** as documented in `PYTHON_ANALYSIS_TOOLS.md`

See [PYTHON_ANALYSIS_TOOLS.md](PYTHON_ANALYSIS_TOOLS.md) for detailed information about using ruff and ty for Python analysis.

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
