use std::sync::Arc;

use salsa::Setter;
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

impl DocumentInput {
    /// Soft-close the document: drop the source text so salsa releases the
    /// per-document `String` (the dominant cost) while keeping the input slot
    /// alive for reuse on a future `did_open` for the same URI.
    ///
    /// Salsa exposes no API to remove an `#[salsa::input]` from storage
    /// (verified against salsa v0.26.2). This method matches the soft-close
    /// idiom used by `ruff_db::files::VirtualFile::close`, which sets a
    /// status field rather than deleting the input.
    ///
    /// Reopening the same URI replays `set_text`/`set_version` on the same
    /// input — that's the existing `did_change` code path, so dependent
    /// queries (`is_hydra_file`, `parsed_yaml`) recompute against the new
    /// text correctly.
    pub fn close(self, db: &mut dyn salsa::Database) {
        self.set_text(db).to(String::new());
        // The version field isn't read by any tracked query, so this bump is
        // purely defensive — it ensures any future code that does watch
        // `version` observes the close. `wrapping_add` is safe; the next
        // `did_open` overwrites this with the LSP-provided version anyway.
        let next = self.version(db).wrapping_add(1);
        self.set_version(db).to(next);
    }
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
/// when the document content changes. Bounded with `lru=512`.
#[salsa::tracked(lru = 512)]
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
        assert_eq!(
            parsed1.result().unwrap().hydra_objects[0].target.value,
            "my.Module"
        );

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
    fn test_close_drops_text_and_bumps_version() {
        let mut db = TestDb::new();
        let input = DocumentInput::new(&db, hydra_yaml().to_string(), 7);

        // Warm cache so we can verify invalidation downstream.
        assert!(is_hydra_file(&db, input));

        input.close(&mut db);

        assert_eq!(input.text(&db), "");
        assert_ne!(input.version(&db), 7);

        // After close, the same input observed against empty text — no
        // hydra markers, no targets — so `is_hydra_file` flips to false.
        assert!(!is_hydra_file(&db, input));
    }

    #[test]
    fn test_close_then_reopen_reuses_input_and_recomputes() {
        let mut db = TestDb::new();
        let input = DocumentInput::new(&db, hydra_yaml().to_string(), 1);
        assert!(is_hydra_file(&db, input));
        let original_id = input;

        input.close(&mut db);
        assert!(!is_hydra_file(&db, input));

        // Simulate did_open on the same URI: the backend looks up the
        // existing input via `document_inputs.get(...)` and re-applies
        // set_text/set_version. The DocumentInput id is unchanged.
        input
            .set_text(&mut db)
            .to("# @hydra\nother:\n  _target_: c.D\n".to_string());
        input.set_version(&mut db).to(2);

        assert!(input == original_id, "did_open should reuse the same input");
        assert!(is_hydra_file(&db, input));
        let parsed = parsed_yaml(&db, input);
        assert_eq!(
            parsed.result().unwrap().hydra_objects[0].target.value,
            "c.D"
        );
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
