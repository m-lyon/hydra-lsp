use serde_yaml::Value;
use std::collections::HashMap;
use tower_lsp::lsp_types::Position;

#[derive(Debug, Clone)]
pub struct TargetInfo {
    pub target_value: String,
    pub parameters: HashMap<String, Value>,
    pub target_line: u32,
    pub target_col: u32,
}

#[derive(Debug)]
pub struct YamlParser;

impl YamlParser {
    /// Parse YAML content and extract all _target_ references with their parameters
    pub fn parse(content: &str) -> Result<Vec<TargetInfo>, serde_yaml::Error> {
        let value: Value = serde_yaml::from_str(content)?;
        let mut targets = Vec::new();
        Self::extract_targets(&value, &mut targets, 0, 0);
        Ok(targets)
    }

    /// Check if a YAML file is a Hydra configuration file
    pub fn is_hydra_file(content: &str) -> bool {
        // Strategy 1: Check for comment markers
        if Self::has_hydra_comment(content) {
            return true;
        }

        // Strategy 2: Check for _target_ keyword
        if Self::has_target_keyword(content) {
            return true;
        }

        false
    }

    /// Check for Hydra comment markers (# @hydra or # hydra:)
    fn has_hydra_comment(content: &str) -> bool {
        content
            .lines()
            .take(10) // Check first 10 lines
            .any(|line| {
                let trimmed = line.trim();
                trimmed.starts_with("# @hydra") || trimmed.starts_with("# hydra:")
            })
    }

    /// Check if content contains _target_ keyword
    fn has_target_keyword(content: &str) -> bool {
        content.contains("_target_")
    }

    /// Find the target info at a specific position
    pub fn find_target_at_position(
        content: &str,
        position: Position,
    ) -> Result<Option<TargetInfo>, serde_yaml::Error> {
        let targets = Self::parse(content)?;

        // Find the target that contains this position
        // For now, do a simple line-based search
        for target in targets {
            if target.target_line == position.line {
                return Ok(Some(target));
            }
        }

        Ok(None)
    }

    /// Recursively extract all _target_ references from YAML value
    fn extract_targets(
        value: &Value,
        targets: &mut Vec<TargetInfo>,
        _line: u32,
        _col: u32,
    ) {
        match value {
            Value::Mapping(map) => {
                // Check if this mapping has a _target_ key
                if let Some(target_value) = map.get(&Value::String("_target_".to_string())) {
                    if let Value::String(target_str) = target_value {
                        // Extract parameters (all keys except _target_)
                        let mut parameters = HashMap::new();
                        for (key, val) in map {
                            if let Value::String(key_str) = key {
                                if key_str != "_target_" {
                                    parameters.insert(key_str.clone(), val.clone());
                                }
                            }
                        }

                        targets.push(TargetInfo {
                            target_value: target_str.clone(),
                            parameters,
                            target_line: _line, // TODO: Get actual line number
                            target_col: _col,   // TODO: Get actual column number
                        });
                    }
                }

                // Recursively process nested mappings
                for (_key, val) in map {
                    Self::extract_targets(val, targets, _line, _col);
                }
            }
            Value::Sequence(seq) => {
                // Recursively process sequences
                for item in seq {
                    Self::extract_targets(item, targets, _line, _col);
                }
            }
            _ => {}
        }
    }

    /// Get completion context at a position
    pub fn get_completion_context(
        content: &str,
        position: Position,
    ) -> Result<CompletionContext, serde_yaml::Error> {
        let lines: Vec<&str> = content.lines().collect();
        if position.line as usize >= lines.len() {
            return Ok(CompletionContext::Unknown);
        }

        let line = lines[position.line as usize];
        let prefix = &line[..position.character.min(line.len() as u32) as usize];

        // Check if we're completing a _target_ value
        if prefix.contains("_target_:") {
            let value_start = prefix.find("_target_:").unwrap() + "_target_:".len();
            let partial = prefix[value_start..].trim();
            return Ok(CompletionContext::TargetValue {
                partial: partial.to_string(),
            });
        }

        // Check if we're completing a parameter key
        // Look for _target_ in previous lines to get context
        if let Ok(Some(target_info)) = Self::find_target_in_scope(content, position) {
            // We're in a scope with a _target_, so we might be completing parameters
            let trimmed = prefix.trim();
            if !trimmed.is_empty() && !trimmed.ends_with(':') {
                return Ok(CompletionContext::ParameterKey {
                    target: target_info.target_value,
                    partial: trimmed.to_string(),
                });
            }
        }

        Ok(CompletionContext::Unknown)
    }

    /// Find the _target_ value in the current scope (same indentation level)
    fn find_target_in_scope(
        content: &str,
        position: Position,
    ) -> Result<Option<TargetInfo>, serde_yaml::Error> {
        let lines: Vec<&str> = content.lines().collect();
        if position.line as usize >= lines.len() {
            return Ok(None);
        }

        // Get current indentation level
        let current_line = lines[position.line as usize];
        let current_indent = current_line.len() - current_line.trim_start().len();

        // Search backwards for _target_ at same or less indentation
        for i in (0..=position.line as usize).rev() {
            let line = lines[i];
            let line_indent = line.len() - line.trim_start().len();

            // If we hit a line with less indentation, we've left the scope
            if line_indent < current_indent && !line.trim().is_empty() {
                break;
            }

            // Check if this line has _target_
            if line.contains("_target_:") && line_indent == current_indent {
                if let Some(value_start) = line.find("_target_:") {
                    let value = line[value_start + "_target_:".len()..].trim();
                    return Ok(Some(TargetInfo {
                        target_value: value.trim_matches('"').trim_matches('\'').to_string(),
                        parameters: HashMap::new(),
                        target_line: i as u32,
                        target_col: value_start as u32,
                    }));
                }
            }
        }

        Ok(None)
    }
}

#[derive(Debug)]
pub enum CompletionContext {
    TargetValue { partial: String },
    ParameterKey { target: String, partial: String },
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_hydra_file_with_comment() {
        let content = "# @hydra\nmodel:\n  _target_: my.Model";
        assert!(YamlParser::is_hydra_file(content));
    }

    #[test]
    fn test_is_hydra_file_with_target() {
        let content = "model:\n  _target_: my.Model\n  param: 123";
        assert!(YamlParser::is_hydra_file(content));
    }

    #[test]
    fn test_parse_simple_config() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
  num_layers: 12
"#;
        let targets = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target_value, "myproject.Model");
        assert_eq!(targets[0].parameters.len(), 2);
    }

    #[test]
    fn test_parse_nested_config() {
        let content = r#"
model:
  _target_: myproject.Model
  encoder:
    _target_: myproject.Encoder
    layers: 6
"#;
        let targets = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 2);
    }
}
