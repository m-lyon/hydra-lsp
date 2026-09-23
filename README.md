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

## Installation

You can install `hydrust` through PyPI:

```bash
uv tool install hydrust   # or: pixi global install hydrust, pip install hydrust
uvx hydrust check conf/   # run once without installing
```

## Usage

`hydrust` provides the `check` subcommand for one-time CLI and CI invocations, as well as an LSP for hydra diagnostics over stdin/stdout:

```bash
hydrust check conf/   # diagnose configs on the command line
hydrust server        # LSP
```

### `hydrust check`

```bash
# Check a single file
hydrust check config.yaml

# Check several files, or a whole directory tree
hydrust check config.yaml overrides.yaml
hydrust check conf/

# Specify workspace root for local module resolution
hydrust check config.yaml -w /path/to/project

# Specify Python interpreter for site-packages resolution
hydrust check config.yaml -p /path/to/venv/bin/python

# Enable detailed resolution tracing for debugging
hydrust check config.yaml --trace-resolution

# Change verbosity level (error, warn, info, debug, trace)
hydrust check config.yaml -v debug

# Output in different formats (pretty, json, compact, github)
hydrust check config.yaml -f json
```

Directories are searched recursively for `.yaml` and `.yml` files. The walk honours `.gitignore` and `.ignore` files within the directory being walked, and skips hidden files and directories such as `.github/`. Symlinks are followed.

#### Options

| Option | Description |
|--------|-------------|
| `-w, --workspace <PATH>` | Working directory for resolving Python modules |
| `-p, --python <PATH>` | Path to Python interpreter for module resolution |
| `-v, --verbosity <LEVEL>` | Logging verbosity: error, warn, info, debug, trace |
| `-f, --output-format <FORMAT>` | Output format: pretty (default), json, compact, github |
| `--trace-resolution` | Show detailed resolution steps for each target (written to stderr) |
| `--disable-rule <RULE>` | Disable a diagnostic rule; may be repeated |

When `--workspace` is omitted, `hydrust` resolves Python modules against the current directory.

#### Continuous integration

`--output-format github` emits GitHub Actions workflow commands, so
diagnostics appear as inline annotations on the pull request. A complete
workflow, run with `uvx` so nothing needs installing beyond uv itself:

```yaml
name: Hydra configs

on: [push, pull_request]

jobs:
  hydrust:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
      - uses: astral-sh/setup-uv@v10
      - run: uvx hydrust@0.5.0 check --output-format github .
```

`_target_` resolution needs the Python packages your configs point at. If they
are not in the checked-out tree, install the project first and point `hydrust`
at that interpreter with `--python`:

```yaml
      - run: uv sync
      - run: uvx hydrust@0.5.0 check --output-format github --python .venv/bin/python conf/
```

#### Exit Codes

- `0`: No errors found
- `1`: One or more errors found
- `2`: Fatal error (path not found, invalid arguments, etc.)

Finding nothing to check is not an error: whether no YAML files matched at all
or none of the ones found are Hydra configs, `hydrust check` warns on stderr and
exits `0`.

## `hydrust server`

### Client Compatibility

A client can be pointed at any released server binary, and an old server quietly
ignores settings it was never taught to read. So the server describes itself in
`capabilities.experimental.hydrust` at `initialize` (`HydrustCapabilities::new`
in [src/backend.rs](src/backend.rs)):

- `protocolVersion` — the version of this block's shape.
- `supportedSettings` — keys read from `initializationOptions.settings`.
- `supportedRules` — codes accepted in `disabledRules`.
- `features` — optional behaviours switched on *for this session*, after matching
  what the server can do against what the client asked for.

A client that sees the block uses it instead of any built-in table; servers
before v0.4.0 send no block, so clients fall back to a version table keyed on
`serverInfo.version` or `--version`. The reference client is the VS Code
extension ([hydra-lsp-vscode](https://github.com/m-lyon/hydra-lsp-vscode)), in
`src/common/compatTable.ts`.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Acknowledgments

- Built with [tower-lsp](https://github.com/ebkalderon/tower-lsp) framework
- Python analysis design based on [ruff](https://github.com/astral-sh/ruff) and [ty](https://github.com/astral-sh/ty)

## References

- [Language Server Protocol Specification](https://microsoft.github.io/language-server-protocol/)
- [Hydra Documentation](https://hydra.cc/docs/intro/)
- [Tower-LSP Documentation](https://docs.rs/tower-lsp/)
