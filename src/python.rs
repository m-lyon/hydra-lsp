//! Python analysis: from a `_target_` string to the definition it names.
//!
//! Each layer depends only on the ones above it:
//!
//! - `model` (plain data) and `typeshed` (the vendored stubs).
//! - `extract`: reads definitions out of one file, following no imports.
//! - `env`: finds the search paths.
//! - `resolve`, `import_resolver` and `cache`: follow imports and base classes
//!   across files. These three call each other, because the salsa queries in
//!   `cache` recurse through resolution.
//!
//! `format`, which renders a definition as text, needs only `model`.

pub mod cache;
pub mod env;
pub mod extract;
pub mod format;
mod import_resolver;
pub mod model;
pub mod resolve;
pub mod typeshed;
