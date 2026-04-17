use std::sync::Arc;

use crate::yaml_parser::{ParsedContent, YamlParseError, YamlParser};

/// A salsa input representing a document's source text.
///
/// Each open document in the editor gets one of these. When the editor
/// sends `didChange`, we update the text field, which automatically
/// invalidates all dependent cached queries (like `parsed_yaml`).
#[salsa::input]
pub struct DocumentInput {
    /// The full text of the document.
    #[returns(ref)]
    pub text: String,

    /// The document version from the LSP protocol.
    pub version: i32,
}

/// Cached result of parsing a YAML file.
///
/// Wraps the parse result in an `Arc` for cheap cloning and uses
/// pointer-based equality (similar to ruff_db's `ParsedModule`) so
/// that salsa's tracked functions compile without requiring deep
/// structural equality on `ParsedContent`.
#[derive(Clone)]
pub struct ParsedYaml {
    inner: Arc<Result<ParsedContent, String>>,
}

impl ParsedYaml {
    fn new(result: Result<ParsedContent, YamlParseError>) -> Self {
        Self {
            inner: Arc::new(result.map_err(|e| e.to_string())),
        }
    }

    /// Get the parse result.
    pub fn result(&self) -> Result<&ParsedContent, &str> {
        self.inner.as_ref().as_ref().map_err(|e| e.as_str())
    }

    /// Returns true if parsing succeeded.
    pub fn is_ok(&self) -> bool {
        self.inner.is_ok()
    }
}

impl PartialEq for ParsedYaml {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for ParsedYaml {}

/// Cached check for whether a file is a Hydra configuration file.
///
/// Depends on `DocumentInput::text`, so it automatically invalidates
/// when the document content changes.
#[salsa::tracked]
pub fn is_hydra_file(db: &dyn salsa::Database, input: DocumentInput) -> bool {
    let text = input.text(db);
    YamlParser::is_hydra_file(text)
}

/// Cached YAML parsing for a document.
///
/// Returns the full `ParsedContent` (hydra objects, target line maps, etc.)
/// cached by Salsa. Only re-parses when the document text changes.
///
/// Uses `no_eq` to always propagate changes to dependents (avoids
/// expensive structural comparison of parsed YAML).
/// Uses `lru=100` to bound memory usage for large workspaces.
#[salsa::tracked(no_eq, lru = 100)]
pub fn parsed_yaml(db: &dyn salsa::Database, input: DocumentInput) -> ParsedYaml {
    let text = input.text(db);
    ParsedYaml::new(YamlParser::parse(text))
}
