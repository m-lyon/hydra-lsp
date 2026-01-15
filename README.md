# Hydra LSP (Language Server Protocol)

A Language Server Protocol implementation for [Hydra](https://hydra.cc/) configuration files, written in Rust.

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

- 🔄 **Type Validation**: Validate YAML values against Python type annotations
- 🔄 **Smart Autocomplete**: Suggest Python classes/functions and parameters

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Acknowledgments

- Built with [tower-lsp](https://github.com/ebkalderon/tower-lsp) framework
- Python analysis design based on [ruff](https://github.com/astral-sh/ruff) and [ty](https://github.com/astral-sh/ty)

## References

- [Language Server Protocol Specification](https://microsoft.github.io/language-server-protocol/)
- [Hydra Documentation](https://hydra.cc/docs/intro/)
- [Tower-LSP Documentation](https://docs.rs/tower-lsp/)
