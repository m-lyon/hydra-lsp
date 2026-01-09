use serde_yaml::Value;
use std::collections::{HashMap, VecDeque};
use tower_lsp::lsp_types::Position;

pub const TARGET_KEY: &str = "_target_";

/// Represents a parameter in a YAML configuration with position information
/// Can either be a simple value or a nested target
#[derive(Debug, Clone)]
pub struct ParameterValue {
    pub kind: ParameterKind,
    pub line: u32,
    pub key: String,
}

/// The kind of parameter value - either a simple value or a nested target
#[derive(Debug, Clone)]
pub enum ParameterKind {
    Value(Value),
    NestedTargetIndex(usize),
}

impl ParameterValue {
    fn new_value(key: String, value: Value) -> Self {
        Self {
            kind: ParameterKind::Value(value),
            line: 0,
            key,
        }
    }

    fn new_nested(key: String, index: usize) -> Self {
        Self {
            kind: ParameterKind::NestedTargetIndex(index),
            line: 0,
            key,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TargetInfo {
    pub value: String,
    pub parameters: Vec<ParameterValue>,
    pub line: u32,
    pub key_start: u32,
    pub value_start: u32,
}

impl TargetInfo {
    fn new(value: String, parameters: Vec<ParameterValue>) -> Self {
        Self {
            value,
            parameters,
            line: 0,
            key_start: 0,
            value_start: 0,
        }
    }

    /// Get the end position of the target value
    pub fn value_end(&self) -> u32 {
        self.value_start + self.value.len() as u32
    }
}

#[derive(Debug)]
pub struct YamlParser;

impl YamlParser {
    /// Parse YAML content and extract all _target_ references with their parameters
    /// Returns a vector of TargetInfo and a line-to-index lookup map
    pub fn parse(
        content: &str,
    ) -> Result<(Vec<TargetInfo>, HashMap<u32, usize>), serde_yaml::Error> {
        // Changed return type
        let value: Value = serde_yaml::from_str(content)?;
        let mut targets: VecDeque<TargetInfo> = VecDeque::new();
        Self::extract_targets(&value, &mut targets);

        // Find positions for all targets
        let targets = Self::find_positions(content, targets);

        // Build line-to-index lookup map
        let mut line_map = HashMap::new();
        for (idx, target) in targets.iter().enumerate() {
            line_map.insert(target.line, idx);
        }

        Ok((targets, line_map))
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
        Self::find_target_with_colon(content).is_some()
    }

    /// Find "_target_" (optionally surrounded by quotes) followed by optional whitespace and ":"
    /// Returns the position of the opening quote or "_target_" if found, and the offset to _target_
    fn find_target_with_colon(text: &str) -> Option<(usize, usize)> {
        let mut pos = 0;
        while let Some(target_pos) = text[pos..].find(TARGET_KEY) {
            let absolute_pos = pos + target_pos;
            let after_target = absolute_pos + TARGET_KEY.len();

            if after_target >= text.len() {
                return None;
            }

            // Check if there's a quote before _target_
            let mut quote_offset = 0;
            let mut key_start = absolute_pos;
            if absolute_pos > 0 {
                let before_char = text.chars().nth(absolute_pos - 1);
                if before_char == Some('"') || before_char == Some('\'') {
                    quote_offset = 1;
                    key_start = absolute_pos - 1;
                }
            }

            // Check for optional whitespace followed by colon
            let remaining = &text[after_target..];
            let mut chars = remaining.chars().peekable();
            let mut found_colon = false;
            let mut found_closing_quote = false;

            // If we found an opening quote, look for closing quote first
            if quote_offset > 0 {
                let opening_quote = text.chars().nth(absolute_pos - 1).unwrap();
                if let Some(&ch) = chars.peek() {
                    if ch == opening_quote {
                        found_closing_quote = true;
                        chars.next(); // consume the quote
                    }
                }
            }

            // Now look for optional whitespace and colon
            for ch in chars.by_ref() {
                if ch == ':' {
                    found_colon = true;
                    break;
                } else if !ch.is_whitespace() {
                    break;
                }
            }

            // Valid if we found a colon AND (no opening quote OR found matching closing quote)
            let is_valid = found_colon && (quote_offset == 0 || found_closing_quote);
            if is_valid {
                return Some((key_start, quote_offset));
            }

            pos = after_target;
        }
        None
    }

    /// Find the target info at a specific position
    pub fn find_target_at_position(
        content: &str,
        position: Position,
    ) -> Result<Option<TargetInfo>, serde_yaml::Error> {
        let (targets, line_map) = Self::parse(content)?;
        if let Some(line_index) = line_map.get(&position.line) {
            let target = &targets[*line_index];
            // Check if the column is within the function definition
            if position.character > target.value_start && position.character < target.value_end() {
                return Ok(Some(target.clone()));
            }
        }
        Ok(None)
    }

    /// Recursively extract all `_target_` references from YAML value and build tree structure
    fn extract_targets(value: &Value, targets: &mut VecDeque<TargetInfo>) {
        match value {
            Value::Mapping(map) => {
                // Check if this mapping has a _target_ key
                if let Some(Value::String(target_str)) = map.get(TARGET_KEY) {
                    // Create and push the target immediately to preserve order
                    let target_index = targets.len();
                    targets.push_back(TargetInfo::new(target_str.clone(), Vec::new()));

                    // Extract parameters, checking for nested targets
                    let parameters = Self::extract_parameters(map, targets);

                    // Update the target with the collected parameters
                    targets[target_index].parameters = parameters;
                } else {
                    // If no _target_ found, recursively process nested mappings
                    for (_key, val) in map {
                        Self::extract_targets(val, targets);
                    }
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

    /// Extract parameters from a mapping that contains a `_target_` key
    fn extract_parameters(
        map: &serde_yaml::Mapping,
        targets: &mut VecDeque<TargetInfo>,
    ) -> Vec<ParameterValue> {
        let mut parameters = Vec::new();

        for (key, val) in map {
            if let Value::String(key_str) = key {
                // The _target_ key itself is not a parameter, but is the target identifier
                if key_str != TARGET_KEY {
                    // Check if this parameter value is a nested target
                    if let Value::Mapping(nested_map) = val {
                        if nested_map.get(TARGET_KEY).is_some() {
                            // This is a nested target - extract it recursively
                            let nested_index = targets.len();
                            Self::extract_targets(val, targets);
                            parameters
                                .push(ParameterValue::new_nested(key_str.clone(), nested_index));
                            continue; // Skip the regular value insertion below
                        }
                    }
                    // Simple value (string, number, mapping without _target_, etc.)
                    parameters.push(ParameterValue::new_value(key_str.clone(), val.clone()));
                }
            }
        }

        parameters
    }

    /// Find the actual line and column positions of `_target_` occurrences in the text
    fn find_positions(content: &str, targets: VecDeque<TargetInfo>) -> Vec<TargetInfo> {
        let mut targets = targets;
        let mut positioned_targets = Vec::new();
        for (line_num, line) in content.lines().enumerate() {
            // Look for _target_ followed by optional whitespace and colon
            if let Some((col, quote_offset)) = Self::find_target_with_colon(line) {
                // remove the first entry from targets
                let mut target = targets.pop_front().unwrap();
                target.line = line_num as u32;
                target.key_start = col as u32;

                // Find the colon position after potential whitespace (and closing quote if present)
                let after_target = col + quote_offset + TARGET_KEY.len();
                if let Some(colon_offset) = line[after_target..].find(':') {
                    let after_colon = after_target + colon_offset + 1;
                    // find the value start position (first non-whitespace after colon)
                    if let Some(value_offset) =
                        line[after_colon..].find(|c: char| !c.is_whitespace())
                    {
                        let potential_value_start = after_colon + value_offset;
                        // Check if the value starts with a quote
                        let value_char = line.chars().nth(potential_value_start);
                        if value_char == Some('"') || value_char == Some('\'') {
                            // Skip the opening quote
                            target.value_start = (potential_value_start + 1) as u32;
                        } else {
                            target.value_start = potential_value_start as u32;
                        }
                    }
                }

                // Find parameter positions in subsequent lines
                Self::find_parameter_positions(content, line_num + 1, &mut target);
                positioned_targets.push(target);
            }
        }

        positioned_targets
    }

    /// Find positions for parameters associated with a `_target_`
    fn find_parameter_positions(content: &str, start_line: usize, target_info: &mut TargetInfo) {
        let lines: Vec<&str> = content.lines().collect();
        if start_line >= lines.len() {
            return;
        }

        let mut remaining_params = std::mem::take(&mut target_info.parameters);

        // Look through subsequent lines for parameters at the same or deeper indentation
        for (idx, line) in lines.iter().enumerate().skip(start_line) {
            if remaining_params.is_empty() {
                return;
            }

            let line_indent = line.find(|c: char| !c.is_whitespace()).unwrap_or(0);

            if line_indent == target_info.key_start as usize {
                // Same indentation as target so we're looking at a paremeter line
                let mut param = remaining_params.remove(0);
                param.line = idx as u32;
                target_info.parameters.push(param);
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
        let char_pos = (position.character as usize).min(line.len());
        let prefix = &line[..char_pos];

        // Check if we're completing a _target_ value
        if let Some((target_pos, quote_offset)) = Self::find_target_with_colon(prefix) {
            // Find the colon position after potential whitespace
            let after_target = target_pos + quote_offset + TARGET_KEY.len();
            if let Some(colon_offset) = prefix[after_target..].find(':') {
                let value_start = after_target + colon_offset + 1;
                let partial = prefix[value_start..].trim();
                return Ok(CompletionContext::TargetValue {
                    partial: partial.to_string(),
                });
            }
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
            if let Some((target_pos, quote_offset)) = Self::find_target_with_colon(line) {
                if line_indent == current_indent {
                    // Find the colon and extract the value
                    let after_target = target_pos + quote_offset + TARGET_KEY.len();
                    if let Some(colon_offset) = line[after_target..].find(':') {
                        let after_colon = after_target + colon_offset + 1;
                        let value = line[after_colon..].trim();
                        return Ok(Some(value.trim_matches('"').trim_matches('\'')));
                    }
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
        let (targets, line_map) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(line_map.len(), 1);
        let target = targets.first().unwrap();
        assert_eq!(target.value, "myproject.Model");
        assert_eq!(target.parameters.len(), 2);
        assert_eq!(target.line, 2);
        assert_eq!(*line_map.get(&2).unwrap(), 0);
        assert_eq!(target.key_start, 2);
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
        let (targets, line_map) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 3, "Should have 3 targets total");
        assert_eq!(line_map.len(), 3, "Line map should have 3 entries");

        let model = targets.first().unwrap();
        assert_eq!(model.parameters.len(), 2);
        assert_eq!(model.line, 2);
        assert_eq!(*line_map.get(&2).unwrap(), 0);

        let encoder = targets.get(1).unwrap();
        assert_eq!(encoder.parameters.len(), 1);
        assert_eq!(encoder.line, 4);
        assert_eq!(*line_map.get(&4).unwrap(), 1);

        let decoder = targets.get(2).unwrap();
        assert_eq!(decoder.parameters.len(), 1);
        assert_eq!(decoder.line, 7);
        assert_eq!(*line_map.get(&7).unwrap(), 2);
    }

    #[test]
    fn test_find_target_at_position_positive() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
  num_layers: 12
"#;
        let position = Position::new(2, 15);
        let target_info = YamlParser::find_target_at_position(content, position)
            .unwrap()
            .unwrap();
        assert_eq!(target_info.value, "myproject.Model");
        assert_eq!(target_info.line, 2);
        assert_eq!(target_info.key_start, 2);
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
        let position = Position::new(2, 11); // Column before _target_ value
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
        let position = Position::new(2, 27); // Column after _target_
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

    #[test]
    fn test_duplicate_target_values_same_order() {
        // When parameter keys are alphabetically ordered the same as text order
        let content = r#"
config:
  a_model:
    _target_: myproject.Model
    size: 128
  b_model:
    _target_: myproject.Model
    size: 256
"#;
        let (targets, line_map) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 2, "Should have 2 targets");
        assert_eq!(line_map.len(), 2, "Line map should have 2 entries");

        // First occurrence (line 3)
        let first_model = targets.first().unwrap();
        assert_eq!(first_model.value, "myproject.Model");
        assert_eq!(first_model.line, 3);
        assert_eq!(*line_map.get(&3).unwrap(), 0);
        assert_eq!(first_model.key_start, 4);
        assert_eq!(first_model.parameters.len(), 1);

        // Check the size value
        if let ParameterKind::Value(val) = &first_model.parameters.first().unwrap().kind {
            if let Value::Number(num) = val {
                assert_eq!(num.as_i64(), Some(128));
            } else {
                panic!("Expected Number value");
            }
        } else {
            panic!("Expected Value parameter");
        }

        // Second occurrence (line 6)
        let second_model = targets.get(1).unwrap();
        assert_eq!(second_model.value, "myproject.Model");
        assert_eq!(second_model.line, 6);
        assert_eq!(*line_map.get(&6).unwrap(), 1);
        assert_eq!(second_model.key_start, 4);
        assert_eq!(second_model.parameters.len(), 1);

        // Check the size value
        if let ParameterKind::Value(val) = &second_model.parameters.first().unwrap().kind {
            if let Value::Number(num) = val {
                assert_eq!(num.as_i64(), Some(256));
            } else {
                panic!("Expected Number value");
            }
        } else {
            panic!("Expected Value parameter");
        }
    }

    #[test]
    fn test_duplicate_target_values_reverse_order() {
        // When parameter keys are alphabetically opposite to text order
        let content = r#"
config:
  z_model:
    _target_: myproject.Model
    size: 128
  a_model:
    _target_: myproject.Model
    size: 256
"#;
        let (targets, line_map) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 2, "Should have 2 targets");
        assert_eq!(line_map.len(), 2, "Line map should have 2 entries");
        assert_eq!(*line_map.get(&3).unwrap(), 0);
        assert_eq!(*line_map.get(&6).unwrap(), 1);

        let target_at_line_3 = targets.first().unwrap();
        let target_at_line_6 = targets.get(1).unwrap();

        // Verify both targets are correct
        if let ParameterKind::Value(Value::Number(num)) =
            &target_at_line_3.parameters.first().unwrap().kind
        {
            assert_eq!(
                num.as_i64(),
                Some(128),
                "Line 3's target should have size: 128"
            );
        }

        if let ParameterKind::Value(Value::Number(num)) =
            &target_at_line_6.parameters.first().unwrap().kind
        {
            assert_eq!(
                num.as_i64(),
                Some(256),
                "Line 6's target should have size: 256"
            );
        }
    }

    #[test]
    fn test_target_with_whitespace_before_colon() {
        // Test that we can handle whitespace between _target_ and :
        let content = r#"
model:
  _target_   : myproject.Model
  hidden_size: 256
another:
  _target_	: another.Model
  param: value
"#;
        let (targets, line_map) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 2, "Should have 2 targets");
        assert_eq!(line_map.len(), 2, "Line map should have 2 entries");

        // First target with spaces before colon
        let first = targets.first().unwrap();
        assert_eq!(first.value, "myproject.Model");
        assert_eq!(first.line, 2);
        assert_eq!(first.key_start, 2);
        assert_eq!(first.parameters.len(), 1);

        // Second target with tab before colon
        let second = targets.get(1).unwrap();
        assert_eq!(second.value, "another.Model");
        assert_eq!(second.line, 5);
        assert_eq!(second.key_start, 2);
        assert_eq!(second.parameters.len(), 1);
    }

    #[test]
    fn test_target_with_double_quotes() {
        // Test that we can handle "_target_": syntax
        let content = r#"
model:
  "_target_": myproject.Model
  hidden_size: 256
"#;
        let (targets, line_map) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 1, "Should have 1 target");
        assert_eq!(line_map.len(), 1, "Line map should have 1 entry");

        let target = targets.first().unwrap();
        assert_eq!(target.value, "myproject.Model");
        assert_eq!(target.line, 2);
        assert_eq!(target.key_start, 2); // Position of opening quote
        assert_eq!(target.parameters.len(), 1);
    }

    #[test]
    fn test_target_with_single_quotes() {
        // Test that we can handle '_target_': syntax
        let content = r#"
model:
  '_target_': myproject.Model
  hidden_size: 256
"#;
        let (targets, line_map) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 1, "Should have 1 target");
        assert_eq!(line_map.len(), 1, "Line map should have 1 entry");

        let target = targets.first().unwrap();
        assert_eq!(target.value, "myproject.Model");
        assert_eq!(target.line, 2);
        assert_eq!(target.key_start, 2); // Position of opening quote
        assert_eq!(target.parameters.len(), 1);
    }

    #[test]
    fn test_target_with_quotes_and_whitespace() {
        // Test that we can handle "_target_" : syntax (quotes + whitespace)
        let content = r#"
model:
  "_target_"  : myproject.Model
  hidden_size: 256
another:
  '_target_'	: another.Model
  param: value
"#;
        let (targets, line_map) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 2, "Should have 2 targets");
        assert_eq!(line_map.len(), 2, "Line map should have 2 entries");

        let first = targets.first().unwrap();
        assert_eq!(first.value, "myproject.Model");
        assert_eq!(first.line, 2);
        assert_eq!(first.key_start, 2);

        let second = targets.get(1).unwrap();
        assert_eq!(second.value, "another.Model");
        assert_eq!(second.line, 5);
        assert_eq!(second.key_start, 2);
    }

    #[test]
    fn test_is_hydra_file_with_quoted_target() {
        let content = "model:\n  \"_target_\": my.Model\n  param: 123";
        assert!(YamlParser::is_hydra_file(content));

        let content2 = "model:\n  '_target_': my.Model\n  param: 123";
        assert!(YamlParser::is_hydra_file(content2));
    }

    #[test]
    fn test_find_target_at_position_with_quotes() {
        let content = r#"
model:
  "_target_": myproject.Model
  hidden_size: 256
"#;
        let position = Position::new(2, 20); // In the value part
        let target_info = YamlParser::find_target_at_position(content, position)
            .unwrap()
            .unwrap();
        assert_eq!(target_info.value, "myproject.Model");
        assert_eq!(target_info.line, 2);
        assert_eq!(target_info.key_start, 2); // Position of opening quote
    }

    #[test]
    fn test_is_hydra_file_with_whitespace_in_target() {
        let content = "model:\n  _target_  : my.Model\n  param: 123";
        assert!(YamlParser::is_hydra_file(content));
    }

    #[test]
    fn test_find_target_at_position_with_whitespace() {
        let content = r#"
model:
  _target_   : myproject.Model
  hidden_size: 256
"#;
        let position = Position::new(2, 20); // In the value part
        let target_info = YamlParser::find_target_at_position(content, position)
            .unwrap()
            .unwrap();
        assert_eq!(target_info.value, "myproject.Model");
        assert_eq!(target_info.line, 2);
        assert_eq!(target_info.key_start, 2);
    }

    #[test]
    fn test_invalid_quote_opening_only() {
        // Test that YAML parser rejects "_target_: (opening quote but no closing quote)
        let content = r#"
model:
  "_target_: myproject.Model
  hidden_size: 256
"#;
        // serde_yaml should fail to parse this as it's invalid YAML
        let result = YamlParser::parse(content);
        assert!(
            result.is_err(),
            "Should fail to parse YAML with unclosed quote"
        );
    }

    #[test]
    fn test_invalid_quote_closing_only() {
        // Test that we reject _target_": (closing quote but no opening quote)
        let content = r#"
model:
  _target_": myproject.Model
  hidden_size: 256
"#;
        let (targets, _) = YamlParser::parse(content).unwrap();
        // Should not find the target because there's an unexpected quote
        assert_eq!(
            targets.len(),
            0,
            "Should not find target with invalid closing quote"
        );
    }

    #[test]
    fn test_mismatched_quotes() {
        // Test that YAML parser rejects "_target_': (mismatched quotes)
        let content = r#"
model:
  "_target_': myproject.Model
  hidden_size: 256
"#;
        // serde_yaml should fail to parse this as it's invalid YAML
        let result = YamlParser::parse(content);
        assert!(
            result.is_err(),
            "Should fail to parse YAML with mismatched quotes"
        );
    }

    #[test]
    fn test_is_hydra_file_with_invalid_quotes() {
        // Opening quote only - this is invalid YAML but we should still not detect it as valid _target_
        let content1 = "model:\n  \"_target_: my.Model\n  param: 123";
        assert!(
            !YamlParser::is_hydra_file(content1),
            "Should not detect opening quote only"
        );

        // Closing quote only - this should not match our pattern
        let content2 = "model:\n  _target_\": my.Model\n  param: 123";
        assert!(
            !YamlParser::is_hydra_file(content2),
            "Should not detect closing quote only"
        );
    }

    #[test]
    fn test_target_value_with_double_quotes() {
        // Test that we handle quoted values: _target_: "myproject.Model"
        let content = r#"
model:
  _target_: "myproject.Model"
  hidden_size: 256
"#;
        let (targets, _) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 1);

        let target = targets.first().unwrap();
        // serde_yaml strips the quotes from the value
        assert_eq!(target.value, "myproject.Model");
        assert_eq!(target.line, 2);
        // value_start should point to the first character of the actual value (after the quote)
        assert_eq!(target.value_start, 13); // Position after opening quote
    }

    #[test]
    fn test_target_value_with_single_quotes() {
        // Test that we handle quoted values: _target_: 'myproject.Model'
        let content = r#"
model:
  _target_: 'myproject.Model'
  hidden_size: 256
"#;
        let (targets, _) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 1);

        let target = targets.first().unwrap();
        // serde_yaml strips the quotes from the value
        assert_eq!(target.value, "myproject.Model");
        assert_eq!(target.line, 2);
        // value_start should point to the first character of the actual value (after the quote)
        assert_eq!(target.value_start, 13); // Position after opening quote
    }

    #[test]
    fn test_quoted_key_and_quoted_value() {
        // Test both key and value quoted: "_target_": "myproject.Model"
        let content = r#"
model:
  "_target_": "myproject.Model"
  hidden_size: 256
"#;
        let (targets, _) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 1);

        let target = targets.first().unwrap();
        assert_eq!(target.value, "myproject.Model");
        assert_eq!(target.line, 2);
        assert_eq!(target.key_start, 2); // Position of opening quote of key
                                         // value_start should point to after the opening quote of the value
        assert_eq!(target.value_start, 15); // Position after opening quote of value
    }

    #[test]
    fn test_find_target_at_position_with_quoted_value() {
        let content = r#"
model:
  _target_: "myproject.Model"
  hidden_size: 256
"#;
        // Position in the middle of the value (inside quotes)
        let position = Position::new(2, 20);
        let target_info = YamlParser::find_target_at_position(content, position)
            .unwrap()
            .unwrap();
        assert_eq!(target_info.value, "myproject.Model");
        assert_eq!(target_info.line, 2);
    }
}
