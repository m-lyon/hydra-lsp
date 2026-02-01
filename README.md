# Hydrust

A Language Server for [Hydra](https://hydra.cc/) configuration files, written in Rust.

## Features

### Currently Implemented

- ✅ **YAML Parsing**: Extracts `_target_` references and their parameters
- ✅ **Hover Support**: Shows rich information when hovering over `_target_` values:
  - Function signatures with parameter details
  - Class information and docstrings
  - Type annotations
- ✅ **Go to Definition**: Jump from YAML `_target_` to Python source file
- ✅ **Diagnostics**: Parameter validation including:
  - Unknown parameters (unless `**kwargs` present)
  - Missing required parameters
  - Basic `_target_` format validation
- ✅ **Semantic Tokens**: Rich syntax highlighting for Hydra configurations:
  - Module path components (namespace tokens)
  - Class and function names
  - Parameter keys (parameter tokens)
  - Values (string, number, and property tokens)
- ✅ **Signature Help**: Shows parameter information while typing function arguments

### Planned Features

For a list of planned features and enhancements, see the [issues](https://github.com/m-lyon/hydra-lsp/issues) page.

## CLI Tool: hydra-check

In addition to the language server, this project provides a standalone CLI tool for diagnosing Hydra YAML configuration files. This is useful for:

- Debugging why a `_target_` is not being resolved
- CI/CD pipeline validation
- Quick command-line checks without an IDE

### Usage

```bash
# Basic usage
hydra-check config.yaml

# Specify workspace root for local module resolution
hydra-check config.yaml -w /path/to/project

# Specify Python interpreter for site-packages resolution
hydra-check config.yaml -p /path/to/venv/bin/python

# Enable detailed resolution tracing for debugging
hydra-check config.yaml --trace-resolution

# Change verbosity level (error, warn, info, debug, trace)
hydra-check config.yaml -v debug

# Output in different formats (pretty, json, compact)
hydra-check config.yaml -f json
```

### Options

| Option | Description |
|--------|-------------|
| `-w, --workspace <PATH>` | Working directory for resolving Python modules |
| `-p, --python <PATH>` | Path to Python interpreter for module resolution |
| `-v, --verbosity <LEVEL>` | Logging verbosity: error, warn, info, debug, trace |
| `-f, --format <FORMAT>` | Output format: pretty (default), json, compact |
| `--trace-resolution` | Show detailed resolution steps for each target |

### Exit Codes

- `0`: No errors found
- `1`: One or more errors found
- `2`: Fatal error (file not found, parse error, etc.)

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Acknowledgments

- Built with [tower-lsp](https://github.com/ebkalderon/tower-lsp) framework
- Python analysis design based on [ruff](https://github.com/astral-sh/ruff) and [ty](https://github.com/astral-sh/ty)

## References

- [Language Server Protocol Specification](https://microsoft.github.io/language-server-protocol/)
- [Hydra Documentation](https://hydra.cc/docs/intro/)
- [Tower-LSP Documentation](https://docs.rs/tower-lsp/)
