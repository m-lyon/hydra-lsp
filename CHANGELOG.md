# Changelog

## [0.4.0]

- Routed Python source reads through `ruff_db::source_text` so resolved Python definitions participate in salsa's dependency graph
- Replaced the `PythonConfig::cache_revision` global counter with per-file `ruff_db::files::File::sync_path` invalidation in `did_change_watched_files`, so editing one Python file no longer evicts every cached resolution
- Removed the `FileCache` wrapper from `PythonAnalyzer` and `ImportResolver` in favour of a single `read_source` helper backed by salsa
- Added per-directory `PthInventory` salsa input so `.pth` create/delete events invalidate editable-install resolution without flushing every cache entry

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
