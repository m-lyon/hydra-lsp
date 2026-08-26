# Changelog

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
