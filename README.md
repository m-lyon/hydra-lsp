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

## Client Compatibility

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

### Adding a feature

**A settings key** — parse it in `initialize`, then add it to `CORE_SETTINGS` (or
to the `feature_toggles!` list for an on/off switch, which registers the key for
you). Update the counts in [tests/capabilities.rs](tests/capabilities.rs). In the
extension: declare it in `package.json`, send it from `startServer`, add a
`SETTING_COMPAT` entry.

**A diagnostic rule** — add it to `diagnostic_rules!` in
[src/diagnostics.rs](src/diagnostics.rs) and it is advertised automatically. In
the extension, add a `RULE_COMPAT` entry; for a *rename*, record the old code as
`previousCode` so the client can rewrite it for older servers, as
`invalid-target` → `invalid-hydra-parameter` did in v0.3.0.

**A behaviour the client must know about** — only needed when it depends on a
client capability, or the client has to branch on it. Read the client capability
in `initialize` and store the flag, add a field to `NegotiatedFeatures` and a
`(name, gate)` pair to `SUPPORTED_FEATURES`, and gate the behaviour on that same
flag so the advertised name is never a promise the session will not keep. Cover
it both ways in [tests/capabilities.rs](tests/capabilities.rs). In the extension,
add a `FEATURE_COMPAT` entry; names become `hydrust.supports.<name>` context keys.

Anything the server always does and can advertise through a standard LSP
capability field needs none of this.

### Rules

- Bump `HYDRUST_PROTOCOL_VERSION` only when something already in the block
  changes meaning: a key that starts doing something different, a repurposed
  feature name, a field that changes type. Additions never need a bump.
- Never remove or repurpose a name quietly — a client may still send it.
- Unknown settings keys stay ignored, never rejected.
- `features` is always an array, even when empty; clients test membership on it.
- Bump the crate version and add a CHANGELOG entry. The client's fallback table
  is keyed on release versions.

### Checking it

`cargo test --test capabilities` covers the block's shape. In the extension repo,
`npm run test:contract` checks the client against a real running binary, and
`npm run test:table-audit` re-verifies the fallback table against tagged sources.

## Threading

The server runs its analysis on two `rayon` pools — a latency pool for hover,
completion and semantic tokens, and a worker pool for diagnostics — alongside a
single-threaded tokio runtime that handles the protocol itself. The `numThreads`
setting is the total across all three, and defaults to a size the server picks to
fit the machine.

[docs/threading-model.md](docs/threading-model.md) is the reference for this:
where each request handler does its work, why the counts are what they are, and
which alternatives were tried and rejected. Read it before changing a thread
count, the concurrency level, or where a handler runs.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Acknowledgments

- Built with [tower-lsp](https://github.com/ebkalderon/tower-lsp) framework
- Python analysis design based on [ruff](https://github.com/astral-sh/ruff) and [ty](https://github.com/astral-sh/ty)

## References

- [Language Server Protocol Specification](https://microsoft.github.io/language-server-protocol/)
- [Hydra Documentation](https://hydra.cc/docs/intro/)
- [Tower-LSP Documentation](https://docs.rs/tower-lsp/)
