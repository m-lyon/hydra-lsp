use std::sync::Arc;

use tracing::debug;

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
    debug!("is_hydra_file: executing (cache miss)");
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
/// Uses `lru=512` to bound memory usage for large workspaces while
/// comfortably exceeding the number of YAML files an editor typically
/// has open at once.
#[salsa::tracked(no_eq, lru = 512)]
pub fn parsed_yaml(db: &dyn salsa::Database, input: DocumentInput) -> ParsedYaml {
    debug!("parsed_yaml: executing (cache miss)");
    let text = input.text(db);
    ParsedYaml::new(YamlParser::parse(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::tests::TestDb;
    use salsa::Setter;

    fn hydra_yaml() -> &'static str {
        "# @hydra\nmodel:\n  _target_: my.Module\n  param: 42\n"
    }

    fn non_hydra_yaml() -> &'static str {
        "server:\n  host: localhost\n  port: 8080\n"
    }

    #[test]
    fn test_document_input_fields() {
        let db = TestDb::new();
        let input = DocumentInput::new(&db, "hello".to_string(), 1);
        assert_eq!(input.text(&db), "hello");
        assert_eq!(input.version(&db), 1);
    }

    #[test]
    fn test_document_input_update() {
        let mut db = TestDb::new();
        let input = DocumentInput::new(&db, "v1".to_string(), 1);
        assert_eq!(input.text(&db), "v1");

        input.set_text(&mut db).to("v2".to_string());
        input.set_version(&mut db).to(2);
        assert_eq!(input.text(&db), "v2");
        assert_eq!(input.version(&db), 2);
    }

    #[test]
    fn test_is_hydra_file_true() {
        let db = TestDb::new();
        let input = DocumentInput::new(&db, hydra_yaml().to_string(), 1);
        assert!(is_hydra_file(&db, input));
    }

    #[test]
    fn test_is_hydra_file_false() {
        let db = TestDb::new();
        let input = DocumentInput::new(&db, non_hydra_yaml().to_string(), 1);
        assert!(!is_hydra_file(&db, input));
    }

    #[test]
    fn test_is_hydra_file_invalidation() {
        let mut db = TestDb::new();
        let input = DocumentInput::new(&db, non_hydra_yaml().to_string(), 1);
        assert!(!is_hydra_file(&db, input));

        // Change content to a Hydra file — cached result should invalidate
        input.set_text(&mut db).to(hydra_yaml().to_string());
        input.set_version(&mut db).to(2);
        assert!(is_hydra_file(&db, input));
    }

    #[test]
    fn test_parsed_yaml_success() {
        let db = TestDb::new();
        let input = DocumentInput::new(&db, hydra_yaml().to_string(), 1);
        let parsed = parsed_yaml(&db, input);
        assert!(parsed.is_ok());

        let content = parsed.result().unwrap();
        assert!(!content.hydra_objects.is_empty());
        assert_eq!(content.hydra_objects[0].target.value, "my.Module");
    }

    #[test]
    fn test_parsed_yaml_invalid() {
        let db = TestDb::new();
        let input = DocumentInput::new(&db, ":\n  - :\n  bad yaml [[[".to_string(), 1);
        let parsed = parsed_yaml(&db, input);
        assert!(!parsed.is_ok());
        assert!(parsed.result().is_err());
    }

    #[test]
    fn test_parsed_yaml_non_hydra() {
        let db = TestDb::new();
        let input = DocumentInput::new(&db, non_hydra_yaml().to_string(), 1);
        let parsed = parsed_yaml(&db, input);
        // Non-hydra YAML still parses successfully — just no targets
        assert!(parsed.is_ok());
        let content = parsed.result().unwrap();
        assert!(content.hydra_objects.is_empty());
    }

    #[test]
    fn test_parsed_yaml_invalidation() {
        let mut db = TestDb::new();
        let input = DocumentInput::new(&db, hydra_yaml().to_string(), 1);

        let parsed1 = parsed_yaml(&db, input);
        assert_eq!(parsed1.result().unwrap().hydra_objects[0].target.value, "my.Module");

        // Update text — should reparse
        input
            .set_text(&mut db)
            .to("# @hydra\nother:\n  _target_: other.Class\n".to_string());
        input.set_version(&mut db).to(2);

        let parsed2 = parsed_yaml(&db, input);
        assert_eq!(
            parsed2.result().unwrap().hydra_objects[0].target.value,
            "other.Class"
        );
    }

    #[test]
    fn test_parsed_yaml_cache_returns_same_arc() {
        let db = TestDb::new();
        let input = DocumentInput::new(&db, hydra_yaml().to_string(), 1);

        let parsed1 = parsed_yaml(&db, input);
        let parsed2 = parsed_yaml(&db, input);
        // Same input, no changes — should be pointer-equal (same Arc)
        assert!(parsed1 == parsed2);
    }

    #[test]
    fn test_parsed_yaml_multiple_targets() {
        let db = TestDb::new();
        let yaml = r#"# @hydra
model:
  _target_: my.Model
  size: 10
optimizer:
  _target_: torch.optim.Adam
  lr: 0.001
"#;
        let input = DocumentInput::new(&db, yaml.to_string(), 1);
        let parsed = parsed_yaml(&db, input);
        let content = parsed.result().unwrap();
        assert_eq!(content.hydra_objects.len(), 2);
    }
}
