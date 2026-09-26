# Changelog

## [0.6.0]

- Added support for Python builtins as a `_target_`, resolved from the typeshed stubs vendored by `ty_vendored` rather than from disk (fixes #34). `builtins.len`, `builtins.dict`, `builtins.open` and the rest now hover and validate; `builtins` is a C module, so a stub is the only place its signatures and docstrings exist.
- Scoped stub resolution to `builtins`. Wiring in typeshed makes the whole stdlib reachable, but enabling it is deliberately left to a follow-up — see `vendored_typeshed::is_vendored_module`.
- Fixed generic base classes never resolving: `class Child(Base[T])` was looked up as a class literally named `Base[T]`, so an `__init__` inherited through one was invisible.
- Fixed a re-exported base class never resolving: a qualified base resolves only its module, so `class Thing(pkg.Widget)` looked for `Widget` in `pkg/__init__.py` and missed the `from .impl import Widget` that put the class body one hop further on. A base whose body genuinely cannot be read now marks the MRO incomplete rather than reporting it as fully walked.
- Added handling for `@overload`: an overloaded symbol is now recorded as such and treated as accepting any arguments, instead of validating against whichever declaration came first. Typeshed gives `open` eight overloads and `dict.__init__` another eight. In a `.py` source the undecorated implementation that closes the set is used instead, since that is the signature Hydra calls.
- Added a `__new__` fallback when a class declares no `__init__`. `int`, `str`, `float`, `bool`, `tuple` and `range` are all shaped that way, and every argument to them was previously reported as unknown.
- Added `is_positional_only` to parameters, along with a new `positional-only-parameter` diagnostic. Hover now renders the `/` marker (`def len(obj: Sized, /) -> int`), and a positional-only parameter passed by name — or missing entirely — is reported with a message that points at `_args_`.
- A keyword key no longer satisfies a positional-only parameter when the callee also takes `**kwargs`: `f(a=1)` on `def f(a, /, **kw)` puts the value in `kw` and still raises "missing 1 required positional argument".
- Left argument validation off when a class's constructor came from `__new__` while part of its MRO could not be resolved: the real `__init__` may be in the ancestor that is missing. Hover still shows what was found.
- Rejected the names the typeshed stub declares but the runtime `builtins` module does not have — its typevars and protocol classes (`_T`, `_SupportsRound1`) and `@type_check_only` placeholders such as `function`. They used to resolve with no diagnostic at all, green-lighting a config that fails with `AttributeError`.
- Fixed parameter diagnostic ranges over-extending on a non-ASCII key: columns are UTF-16 code units, and the end was derived from the key's byte length.
- Diagnostics anchored to a parameter's line now point at that parameter's own column rather than the `_target_` key's; the two only coincide in block-style YAML.
- An inline `# hydrust: ignore[...]` on a parameter's own line now silences the diagnostics that point at it (`positional-only-parameter`, `unknown-argument`, `parameter-already-assigned`), not just a comment on the file header or the `_target_` line.
- Improved the bare-name `_target_` error: `len` now reports that it is a builtin and names `builtins.len`, the form Hydra actually accepts.
- Go-to-definition on a target that resolves into a vendored stub is a no-op rather than an error, since there is no file on disk to open. Hover and diagnostics are unaffected.
- Signature help now carries the same `/` and `*` markers as hover, and says when it is showing the first of several overloads.
- Hover now renders the bare `*` that opens a run of keyword-only parameters, alongside the `/` — `def sorted(iterable, /, *, key=None, reverse=False)`.
- An inline `_args_` flow sequence is now recognised from the value written after the colon rather than inferred from where its entries sit, so `_args_: # see [docs]` above a block sequence is no longer read as a flow sequence starting inside the comment. An anchor or a tag before the bracket (`_args_: &defaults [1, 2]`) is stepped over, so such a sequence still gets signature help.
- Fixed a panic when `_args_` contained a nested list or mapping (`_args_: [[1, 2, 3]]`). saphyr gives collection nodes no position, which underflowed the line conversion. Such a node now borrows its position from its first positioned descendant, so a block-style `- [1, 2]` keeps its own line and still gets signature help.

## [0.5.1]

- Consolidate the release pipeline to compile each target once.
- The Windows zip now has a top-level `hydrust-x86_64-pc-windows-msvc/` directory, as the tarballs do.
- PyPI is published from the same workflow run as the GitHub Release.

## [0.5.0]

### Breaking

- **One binary.** The `hydra-lsp` and `hydra-check` binaries are gone. Everything is now `hydrust`, with two subcommands: `hydrust check` and `hydrust server`. An editor that used to launch `hydra-lsp` with no arguments must launch `hydrust server`; the VSCode extension does this from v0.1.7 onwards.
- **The crate is renamed `hydra-lsp` → `hydrust`**, so the release archives are now `hydrust-<target>.<ext>` and contain a `hydrust` executable. A VSCode extension older than v0.1.7 does not recognise the new asset name and silently stays on v0.4.2. `use hydra_lsp::` becomes `use hydrust::` for anything depending on the library.
- **Removed `server` cargo feature**, along with `--no-default-features`.
- `serverInfo.name` in the `initialize` response is now `"hydrust"`.
- **`hydrust check --disable-rule` rejects an unknown rule** as a usage error (exit 2) instead of warning and continuing.
- **`hydrust check` without `--workspace` resolves Python modules against the current directory**, even for a single file. `hydra-check` previously used the file's own directory.

### Other changes

- **Published to PyPI as `hydrust`.** e.g. `pip install hydrust`, `uv tool install hydrust`.
- `hydrust check` now accepts multiple files and directories. Directories are walked recursively for `.yaml` and `.yml` files, honouring `.gitignore`; discovered files that carry no Hydra markers are skipped silently.
- Added `--output-format github`, emitting GitHub Actions workflow commands so diagnostics render as inline annotations.
- Renamed `--format` to `--output-format`.
- JSON output is now a single document covering every checked file, rather than one document per file.
- The JSON `summary` now counts diagnostics only in `total`, reports `other`, and counts files that could not be read or parsed under a separate `failed_files` field.
- Reported paths always use `/` separators, so `--output-format github` annotations attach on Windows runners.
- Directory walks no longer apply the user's global git excludes (`core.excludesFile`), so a local run and a CI run check the same files.
- An unreadable directory or a file that disappears mid-walk is now logged and skipped, rather than aborting the whole run.
- Finding no YAML files to check now warns on stderr and exits 0, rather than being a fatal error (exit 2).
- The JSON `end_column` is now an inclusive 1-based column, matching `endColumn` in the github format; it was previously exclusive, so single-line values are one lower than before. A multi-line range that ends at the start of a line is reported as ending on the previous line, with `end_column` null (and `endColumn` omitted).
- The workspace root falls back to the canonicalized current directory, so it matches the paths reported for each file.

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
