# Changelog

## [0.5.0]

### Breaking

- **One binary.** The `hydra-lsp` and `hydra-check` binaries are gone. Everything is now `hydrust`, with two subcommands: `hydrust check config.yaml` and `hydrust server`. An editor that used to launch `hydra-lsp` with no arguments must launch `hydrust server`; the VS Code extension does this from v0.1.7 onwards, unconditionally and without checking the server version first.
- **The crate is renamed `hydra-lsp` → `hydrust`**, so the release archives are now `hydrust-<target>.<ext>` and contain a `hydrust` executable. The GitHub repository keeps its name. A VS Code extension older than v0.1.7 does not recognise the new asset name, so it silently stays on v0.4.2 and keeps working rather than failing — update the extension to pick up this release. `use hydra_lsp::` becomes `use hydrust::` for anything depending on the library.
- **The `server` feature is gone**, along with `--no-default-features`. It gated no code and saved no dependencies, and the single binary must always be able to serve: the extension defaults to finding `hydrust` on `PATH` and launching it as the language server, with no fallback if it cannot.
- `serverInfo.name` in the `initialize` response is now `"hydrust"`. Display only — nothing should key off it, and `capabilities.experimental.hydrust` is unchanged (`protocolVersion` is deliberately not bumped, since no field in the block changed meaning). Diagnostic `source` and the pull-diagnostics `identifier` still read `hydra-lsp`.
- If you need to stay on the old shape, pin `hydrust.serverVersion` to `0.4.2` in the extension settings.

### Other changes

- `hydrust server` ignores arguments it does not recognise, as the old `hydra-lsp` binary did — an editor may append its own transport flag such as `--stdio`, and refusing to start is worse than ignoring it. The note about them goes to stderr; stdout carries only LSP traffic.
- `hydrust check` now accepts multiple files and directories. Directories are walked recursively for `.yaml` and `.yml` files, honouring `.gitignore`; discovered files that carry no Hydra markers are skipped silently
- Added `--output-format github`, emitting GitHub Actions workflow commands so diagnostics render as inline annotations.
- Renamed `--format` to `--output-format`.
- JSON output is now a single document covering every checked file, rather than one document per file
- The JSON `summary` now counts diagnostics only in `total`, reports `other`, and counts files that could not be read or parsed under a separate `failed_files` field
- `--disable-rule` now lists the valid rules in `--help` and rejects an unknown rule as a usage error, instead of warning and carrying on
- Reported paths always use `/` separators, so `--output-format github` annotations attach on Windows runners
- Directory walks no longer apply the user's global git excludes (`core.excludesFile`), so a local run and a CI run check the same files
- An unreadable directory or a file that disappears mid-walk is now logged and skipped, rather than aborting the whole run
- The workspace root falls back to the canonicalized current directory, so it matches the paths reported for each file

## [0.4.2]

- Fixed lazy package exports declared under `if TYPE_CHECKING:` (or `if typing.TYPE_CHECKING:`) not resolving when combined with a module-level `__getattr__` and `__all__` (fixes #43)

## [0.4.1]

- Fixed relative re-exports failing to resolve when a package sits under more than one search root, such as a virtual environment stored inside the workspace. The most specific containing root is now used to convert a relative import to an absolute module name.

## [0.4.0]

- Added an incremental cache built on salsa (`HydraDatabase`), so YAML parses, Python target resolutions, and diagnostics are reused between requests instead of being recomputed on every keystroke
- Cached YAML parsing per document version through the `DocumentInput`/`ParsedYaml` salsa inputs, replacing the previous `DocumentStore`
- Cached Python definition lookups in `python_cache`, keyed on the target string and the resolved search paths
- Routed Python source reads through `ruff_db::source_text` so resolved Python definitions participate in salsa's dependency graph
- Removed the `FileCache` wrapper from `PythonAnalyzer` and `ImportResolver` in favour of a single `read_source` helper backed by salsa
- Moved analysis off the async runtime onto two `rayon` pools (a latency pool for interactive requests, a worker pool for diagnostics), sized by the new `numThreads` initialization option
- Answered `textDocument/semanticTokens/full` from a database snapshot on the latency pool instead of inline under the database lock
- Sized the thread pools to a fixed 2 latency + 3 worker by default, rather than scaling the worker pool with the CPU count
- Fixed the server hanging instead of exiting when the client's pipe closes while a handler is talking to it
- Raised `tower-lsp`'s concurrency level from its default of 4 to 8
- Clamped `numThreads` to 11 rather than to the CPU count. Beyond that the threads are unreachable rather than merely oversubscribed.
- Registered `workspace/didChangeWatchedFiles` dynamically for `**/*.{py,pyi,pth}`, covering the workspace folders and — where the client supports relative patterns — each out-of-workspace site-packages root
- Replaced the `PythonConfig::cache_revision` global counter with per-file `ruff_db::files::File::sync_path` invalidation in `did_change_watched_files`, so editing one Python file no longer evicts every cached resolution
- Added per-directory `PthInventory` salsa input so `.pth` create/delete events invalidate editable-install resolution without flushing every cache entry
- Added pull diagnostics (`textDocument/diagnostic`), returning unchanged reports via result IDs when nothing has changed, and falling back to push publishing for clients without pull support
- Sent `workspace/diagnostic/refresh` after watched Python files change, so open configs pick up edits made outside the editor
- Cleared diagnostics when a document stops being a Hydra file or is closed, instead of leaving stale entries in the client
- Returned `ServerCancelled` with `retrigger_request` when a pull-diagnostic round is superseded by a newer edit
- Fixed server panics caused by salsa cancellation escaping into the rayon pools
- Fixed UTF-16 position handling for non-ASCII content, so positions match the encoding advertised to the client
- Fixed `.pth` parsing to follow Python's lexical rules, and to only cache `.pth` files on the search path
- Added `--version` and `--help` flags to the server binary, so a client can identify a binary before launching it; any other invocation still starts the stdio language server
- Added a self-describing `capabilities.experimental.hydrust` block to the `initialize` response, listing the protocol version, the settings keys and diagnostic rules this build understands, and the coarse features actually switched on for the session after negotiating against the client's capabilities

## [0.3.0]

- Added class name for `__init__` diagnostics
- Implemented `signature_help` for parameters
- Added support for `_args_`, `_convert_`, and `_recursive_`
- Renamed `invalid-target` diagnostic to `invalid-hydra-parameter` and included other checks within that rule

## [0.2.0]

- Refactored `yaml` parsing from `serde_yaml` to `saphyr`
- Added suppression comments
- Fixed issue where `missing-argument` diagnostic appeared for `cls` in class methods
- Fixed issue whereby non-conventionally named first arguments would not be filtered out in instance and class methods

## [0.1.5]

- Added support for `_partial_`
- Fixed issue of finding parent class docstring and signature when not overridden by child class
- Fixed module resolution for `classmethod`s and `staticmethod`s
- Refactored `PythonAnalyzer` to cache file reads
- Fixed parameter placement issue when parameter's were above `_target_`

## [0.1.4]

- Fixed issue with `goto_definition` filepath for re-exported modules
- Improved docstring rendering on hover

## [0.1.3]

- Fixed issue with `python` reimports not being found by `PythonAnalyzer`
- Created `hydra-check` CLI tool
- Removed dummy `CompletionResponse`s
- Fixed issue in `YamlParser` whereby parameters weren't being fully recursively searched
- Fixed issue with incorrect semantic token handling for sequences
- Fixed issue of `.pth` files not being used for python import resolution

## [0.1.2]

- Refactor of `YamlParser::find_valid_target_key` to simplify implementation

## [0.1.1]

- Fixes to `YamlParser` when encountering commented out `_target_` key or values

## [0.1.0]

- Initial release
