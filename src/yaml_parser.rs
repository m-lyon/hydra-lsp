use serde_yaml::Value;
use std::collections::HashMap;
use tower_lsp::lsp_types::Position;

pub struct TargetInfo {
    pub value: String,
    pub parameters: HashMap<String, Value>,
    pub line: u32,
    pub col: u32,
}

impl TargetInfo {
    fn new(value: String, parameters: HashMap<String, Value>) -> Self {
        Self {
            value,
            parameters,
            line: 0,
            col: 0,
        }
    }

    fn with_all(value: String, parameters: HashMap<String, Value>, line: u32, col: u32) -> Self {
        Self {
            value,
            parameters,
            line,
            col,
        }
    }
}

#[derive(Debug)]
pub struct YamlParser;

impl YamlParser {
    /// Parse YAML content and extract all _target_ references with their parameters
    pub fn parse(content: &str) -> Result<HashMap<u32, TargetInfo>, serde_yaml::Error> {
        let value: Value = serde_yaml::from_str(content)?;
        let mut targets: Vec<TargetInfo> = Vec::new();
        Self::extract_targets(&value, &mut targets);
        Self::find_positions(content, &mut targets);

        // Convert Vec to HashMap keyed by line number
        let mut target_map = HashMap::new();
        for target in targets {
            target_map.insert(target.line, target);
        }

        Ok(target_map)
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

    /// Check if content contains `_target_` keyword
    fn has_target_keyword(content: &str) -> bool {
        content.contains("_target_")
    }

    /// Find the target info at a specific position
    pub fn find_target_at_position(
        content: &str,
        position: Position,
    ) -> Result<Option<TargetInfo>, serde_yaml::Error> {
        let mut target_map = Self::parse(content)?;

        // Direct HashMap lookup by line number
        match target_map.remove(&position.line) {
            Some(target_info) => {
                // Check if the column is within the _target_ key
                if position.character >= target_info.col
                    && position.character <= target_info.col + "_target_:".len() as u32
                {
                    Ok(Some(target_info))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    /// Recursively extract all `_target_` references from YAML value
    fn extract_targets(value: &Value, targets: &mut Vec<TargetInfo>) {
        match value {
            Value::Mapping(map) => {
                // Check if this mapping has a _target_ key
                if let Some(Value::String(target_str)) = map.get("_target_") {
                    // Extract parameters (all keys except _target_)
                    let mut parameters = HashMap::new();
                    for (key, val) in map {
                        if let Value::String(key_str) = key {
                            if key_str != "_target_" {
                                parameters.insert(key_str.clone(), val.clone());
                            }
                        }
                    }

                    targets.push(TargetInfo::new(target_str.clone(), parameters));
                }

                // Recursively process nested mappings
                for (_key, val) in map {
                    Self::extract_targets(val, targets);
                }
            }
            Value::Sequence(seq) => {
                // Recursively process sequences
                for item in seq {
                    Self::extract_targets(item, targets);
                }
            }
            _ => {}
        }
    }

    /// Find the actual line and column positions of `_target_` occurrences in the text
    fn find_positions(content: &str, targets: &mut [TargetInfo]) {
        let mut target_idx = 0;

        for (line_num, line) in content.lines().enumerate() {
            if target_idx >= targets.len() {
                break;
            }

            // Look for _target_: in this line
            if let Some(col) = line.find("_target_:") {
                // Found a _target_, assign position to the next unassigned target
                targets[target_idx].line = line_num as u32;
                targets[target_idx].col = col as u32;
                target_idx += 1;
            }
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
        if let Some(target_pos) = prefix.find("_target_:") {
            let value_start = target_pos + "_target_:".len();
            let partial = prefix[value_start..].trim();
            return Ok(CompletionContext::TargetValue {
                partial: partial.to_string(),
            });
        }

        // Check if we're completing a parameter key
        // Look for _target_ in previous lines to get context
        if let Ok(Some(target_value)) = Self::find_target_in_scope(content, position) {
            // We're in a scope with a _target_, so we might be completing parameters
            let trimmed = prefix.trim();
            if !trimmed.is_empty() {
                if trimmed.contains(':') {
                    // Likely completing a parameter value
                    let parts: Vec<&str> = trimmed.splitn(2, ':').collect();
                    let param_key = parts[0].trim();
                    let partial_value = parts[1].trim();
                    return Ok(CompletionContext::ParameterValue {
                        target: target_value.to_string(),
                        parameter: param_key.to_string(),
                        partial: partial_value.to_string(),
                    });
                } else {
                    // Completing a parameter key
                    return Ok(CompletionContext::ParameterKey {
                        target: target_value.to_string(),
                        partial: trimmed.to_string(),
                    });
                }
            }
        }

        Ok(CompletionContext::Unknown)
    }

    /// Find the `_target_` value in the current scope (same indentation level)
    fn find_target_in_scope(
        content: &str,
        position: Position,
    ) -> Result<Option<&str>, serde_yaml::Error> {
        let lines: Vec<&str> = content.lines().collect();
        if position.line as usize >= lines.len() {
            return Ok(None);
        }

        // Get current indentation level
        let current_line = lines[position.line as usize];
        let current_indent = current_line.len() - current_line.trim_start().len();

        // Search backwards for _target_ at same indentation
        for i in (0..=position.line as usize).rev() {
            let line = lines[i];
            let line_indent = line.len() - line.trim_start().len();

            // If we hit a line with less indentation, we've left the scope
            if line_indent < current_indent && !line.trim().is_empty() {
                break;
            }

            // Check if this line has _target_
            if let Some(value_start) = line.find("_target_:") {
                if line_indent == current_indent {
                    let value = line[value_start + "_target_:".len()..].trim();
                    return Ok(Some(value.trim_matches('"').trim_matches('\'')));
                }
            }
        }

        Ok(None)
    }
}

/// Represents the context for code completion in a YAML file. The context can be
/// either completing a target value, a parameter key for a specific target, or unknown.
#[derive(Debug)]
pub enum CompletionContext {
    TargetValue {
        partial: String,
    },
    ParameterKey {
        target: String,
        partial: String,
    },
    ParameterValue {
        target: String,
        parameter: String,
        partial: String,
    },
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_hydra_file_with_comment() {
        let content = "# @hydra\nmodel:\n  value: my.Model";
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
        let target_map = YamlParser::parse(content).unwrap();
        assert_eq!(target_map.len(), 1);
        let target = target_map.get(&2).unwrap();
        assert_eq!(target.value, "myproject.Model");
        assert_eq!(target.parameters.len(), 2);
        assert_eq!(target.line, 2);
        assert_eq!(target.col, 2);
    }

    #[test]
    fn test_parse_nested_config() {
        let content = r#"
model:
  _target_: myproject.Model
  encoder:
    _target_: myproject.Encoder
    layers: 6
  decoder:
    _target_: myproject.Decoder
    layers: 6
"#;
        let target_map = YamlParser::parse(content).unwrap();
        assert_eq!(target_map.len(), 3);

        let target1 = target_map.get(&2).unwrap();
        assert_eq!(target1.value, "myproject.Model");
        assert_eq!(target1.parameters.len(), 2);
        assert_eq!(target1.line, 2);
        assert_eq!(target1.col, 2);

        let target2 = target_map.get(&4).unwrap();
        assert_eq!(target2.value, "myproject.Encoder");
        assert_eq!(target2.parameters.len(), 1);
        assert_eq!(target2.line, 4);
        assert_eq!(target2.col, 4);

        let target3 = target_map.get(&7).unwrap();
        assert_eq!(target3.value, "myproject.Decoder");
        assert_eq!(target3.parameters.len(), 1);
        assert_eq!(target3.line, 7);
        assert_eq!(target3.col, 4);
    }

    #[test]
    fn test_find_target_at_position_positive() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
  num_layers: 12
"#;
        let position = Position::new(2, 5);
        let target_info = YamlParser::find_target_at_position(content, position)
            .unwrap()
            .unwrap();
        assert_eq!(target_info.value, "myproject.Model");
        assert_eq!(target_info.line, 2);
        assert_eq!(target_info.col, 2);
    }

    #[test]
    fn test_find_target_at_position_negative_line() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
  num_layers: 12
"#;
        let position = Position::new(1, 2); // Line without _target_
        let target_info = YamlParser::find_target_at_position(content, position).unwrap();
        assert!(target_info.is_none());
    }

    #[test]
    fn test_find_target_at_position_negative_col_before() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
  num_layers: 12
"#;
        let position = Position::new(2, 1); // Column before _target_
        let target_info = YamlParser::find_target_at_position(content, position).unwrap();
        assert!(target_info.is_none());
    }

    #[test]
    fn test_find_target_at_position_negative_col_after() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
  num_layers: 12
"#;
        let position = Position::new(2, 12); // Column after _target_
        let target_info = YamlParser::find_target_at_position(content, position).unwrap();
        assert!(target_info.is_none());
    }

    #[test]
    fn test_get_completion_context_target_value() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
"#;
        let position = Position::new(2, 15); // After _target_:
        let context = YamlParser::get_completion_context(content, position).unwrap();
        match context {
            CompletionContext::TargetValue { partial } => {
                assert_eq!(partial, "myp");
            }
            _ => panic!("Expected TargetValue context"),
        }
    }

    #[test]
    fn test_get_completion_context_parameter_key() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
"#;
        let position = Position::new(3, 6); // On hidden_size key
        let context = YamlParser::get_completion_context(content, position).unwrap();
        match context {
            CompletionContext::ParameterKey { target, partial } => {
                assert_eq!(target, "myproject.Model");
                assert_eq!(partial, "hidd");
            }
            _ => panic!("Expected ParameterKey context"),
        }
    }
    #[test]
    fn test_get_completion_context_parameter_value() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
"#;
        let position = Position::new(3, 17);
        let context = YamlParser::get_completion_context(content, position).unwrap();
        match context {
            CompletionContext::ParameterValue {
                target,
                parameter,
                partial,
            } => {
                assert_eq!(target, "myproject.Model");
                assert_eq!(parameter, "hidden_size");
                assert_eq!(partial, "25");
            }
            _ => panic!("Expected ParameterValue context"),
        }
    }
}
