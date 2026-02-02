use serde_yaml::Value;
use std::collections::{HashMap, VecDeque};
use tower_lsp::lsp_types::Position;

/// Represents a semantic token in the document (internal representation)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydraSemanticToken {
    pub line: u32,
    pub start_char: u32,
    pub length: u32,
    pub token_type: SemanticTokenType,
}

/// Token types for semantic highlighting
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticTokenType {
    Namespace, // Module path parts (e.g., "myproject" in "myproject.models")
    Class,     // Class name in _target_
    Function,  // Function name in _target_
    Parameter, // Parameter key names
    Property,  // YAML property keys
    String,    // String values
    Number,    // Numeric values
}

impl SemanticTokenType {
    /// Convert to LSP token type index (based on order in legend)
    pub fn to_index(self) -> u32 {
        match self {
            SemanticTokenType::Namespace => 0,
            SemanticTokenType::Class => 1,
            SemanticTokenType::Function => 2,
            SemanticTokenType::Parameter => 3,
            SemanticTokenType::Property => 4,
            SemanticTokenType::String => 6,
            SemanticTokenType::Number => 7,
        }
    }

    /// Convert from LSP token type index to SemanticTokenType
    pub fn from_index(index: u32) -> Option<Self> {
        match index {
            0 => Some(SemanticTokenType::Namespace),
            1 => Some(SemanticTokenType::Class),
            2 => Some(SemanticTokenType::Function),
            3 => Some(SemanticTokenType::Parameter),
            4 => Some(SemanticTokenType::Property),
            6 => Some(SemanticTokenType::String),
            7 => Some(SemanticTokenType::Number),
            _ => None,
        }
    }
}

impl HydraSemanticToken {
    /// Convert a list of semantic tokens to LSP SemanticToken format (delta-encoded)
    /// LSP uses relative positioning: [deltaLine, deltaStartChar, length, tokenType, tokenModifiers]
    pub fn to_lsp_tokens(
        tokens: &[HydraSemanticToken],
    ) -> Vec<tower_lsp::lsp_types::SemanticToken> {
        let mut lsp_tokens = Vec::new();
        let mut prev_line = 0;
        let mut prev_start = 0;

        for token in tokens {
            let delta_line = token.line - prev_line;
            let delta_start = if delta_line == 0 {
                token.start_char - prev_start
            } else {
                token.start_char
            };

            lsp_tokens.push(tower_lsp::lsp_types::SemanticToken {
                delta_line,
                delta_start,
                length: token.length,
                token_type: token.token_type.to_index(),
                token_modifiers_bitset: 0, // No modifiers for now
            });

            prev_line = token.line;
            prev_start = token.start_char;
        }

        lsp_tokens
    }
}

pub const TARGET_KEY: &str = "_target_";
pub const PARTIAL_KEY: &str = "_partial_";

/// Represents a parameter in a YAML configuration with position information
/// Can either be a simple value or a nested target
#[derive(Debug, Clone)]
pub struct Parameter {
    pub value: Value,
    pub line: u32,
    pub key: String,
}

impl Parameter {
    fn new_value(key: String, value: Value) -> Self {
        Self {
            line: 0,
            key,
            value,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TargetInfo {
    /// The target value (class/function path)
    pub value: String,
    /// Parameters for the target
    pub parameters: Vec<Parameter>,
    /// The line number where the `_target_` key is located
    pub line: u32,
    /// The start position of the `_target_` key in the line
    pub key_start: u32,
    /// The start position of the target value in the line
    pub value_start: u32,
}

impl TargetInfo {
    fn new(value: String, parameters: Vec<Parameter>) -> Self {
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

    /// Check if `_partial_` is set to true for this target
    pub fn is_partial(&self) -> bool {
        self.parameters
            .iter()
            .find(|p| p.key == PARTIAL_KEY)
            .is_some_and(|p| p.value.as_bool() == Some(true))
    }
}

#[derive(Debug)]
pub struct YamlParser;

impl YamlParser {
    /// Parse YAML content and extract all `_target_` references with their parameters
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

    /// Check for Hydra comment markers (# @hydra, # @package, # hydra:)
    fn has_hydra_comment(content: &str) -> bool {
        content
            .lines()
            .take(10) // Check first 10 lines
            .any(|line| {
                let trimmed = line.trim();
                trimmed.starts_with("# @hydra")
                    || trimmed.starts_with("# @package")
                    || trimmed.starts_with("# hydra:")
            })
    }

    /// Check if content contains `_target_` keyword
    fn has_target_keyword(content: &str) -> bool {
        content
            .lines()
            .any(|line| Self::find_valid_target_key(line).is_some())
    }

    /// Find `_target_` (optionally surrounded by quotes) followed by optional whitespace and ":"
    /// in a single line. Returns the position of the opening quote or `_target_` if found,
    /// and the offset to `_target_`. Valid only if preceded by whitespace or "- " (single dash
    /// with space for YAML lists)
    fn find_valid_target_key(line: &str) -> Option<(usize, usize)> {
        // Find _target_ in the line
        let target_pos = line.find(TARGET_KEY)?;

        // Check if there's a quote immediately before _target_
        let (key_start, quote_offset) = if target_pos > 0 {
            let prev_char = line.chars().nth(target_pos - 1)?;
            if prev_char == '"' || prev_char == '\'' {
                (target_pos - 1, 1)
            } else {
                (target_pos, 0)
            }
        } else {
            (target_pos, 0)
        };

        // Validate prefix (everything before the quote or _target_)
        let prefix = &line[..key_start];

        // Check if prefix matches: optional whitespace, optionally "- " followed by whitespace
        let is_valid_prefix = if let Some(dash_pos) = prefix.rfind('-') {
            // Has a dash - validate: whitespace + "-" + " " + whitespace
            let before_dash = &prefix[..dash_pos];
            let after_dash = &prefix[dash_pos + 1..];

            before_dash.chars().all(|c| c.is_whitespace())
                && after_dash.starts_with(' ')
                && after_dash[1..].chars().all(|c| c.is_whitespace())
        } else {
            // No dash - just whitespace
            prefix.chars().all(|c| c.is_whitespace())
        };

        if !is_valid_prefix {
            return None;
        }

        // Check what comes after _target_
        let after_target = &line[target_pos + TARGET_KEY.len()..];

        // If we have a quote, we need the matching closing quote
        if quote_offset > 0 {
            let quote_char = line.chars().nth(target_pos - 1)?;
            let mut chars = after_target.chars();

            // First char must be the closing quote
            if chars.next() != Some(quote_char) {
                return None;
            }

            // Then optional whitespace followed by colon
            for ch in chars {
                if ch == ':' {
                    return Some((key_start, quote_offset));
                } else if !ch.is_whitespace() {
                    return None;
                }
            }
            None
        } else {
            // No quotes - just optional whitespace then colon
            for ch in after_target.chars() {
                if ch == ':' {
                    return Some((key_start, quote_offset));
                } else if !ch.is_whitespace() {
                    return None;
                }
            }
            None
        }
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
    ) -> Vec<Parameter> {
        let mut parameters = Vec::new();

        for (key, val) in map {
            if let Value::String(key_str) = key {
                // The _target_ key itself is not a parameter, but is the target identifier
                if key_str != TARGET_KEY {
                    // Recursively check for nested targets in parameter values
                    Self::extract_targets(val, targets);

                    // Simple value (string, number, mapping without _target_, etc.)
                    parameters.push(Parameter::new_value(key_str.clone(), val.clone()));
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
            if targets.is_empty() {
                return positioned_targets;
            }
            // Look for _target_ followed by optional whitespace and colon
            if let Some((col, quote_offset)) = Self::find_valid_target_key(line) {
                // Find the colon position after potential whitespace (and closing quote if present)
                let after_target = col + quote_offset + TARGET_KEY.len();
                let colon_offset = match line[after_target..].find(':') {
                    Some(offset) => offset,
                    None => continue, // No colon found, skip this line
                };

                let after_colon = after_target + colon_offset + 1;
                // find the value start position (first non-whitespace after colon)
                let value_info = line[after_colon..].find(|c: char| !c.is_whitespace());

                // Check if there's a value and it's not a comment
                if value_info.is_none() {
                    // Empty value, skip this line
                    continue;
                }

                let value_offset = value_info.unwrap();
                let potential_value_start = after_colon + value_offset;
                let value_char = line.chars().nth(potential_value_start);

                // If the value is a comment, skip this line
                if value_char == Some('#') {
                    continue;
                }

                // Now we know this is a valid _target_ with a value, consume a target from the queue
                let mut target = targets.pop_front().unwrap();
                target.line = line_num as u32;
                target.key_start = col as u32;

                // Set the value start position
                if value_char == Some('"') || value_char == Some('\'') {
                    // Skip the opening quote
                    target.value_start = (potential_value_start + 1) as u32;
                } else {
                    target.value_start = potential_value_start as u32;
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
        if let Some((target_pos, quote_offset)) = Self::find_valid_target_key(prefix) {
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
            if let Some((target_pos, quote_offset)) = Self::find_valid_target_key(line)
                && line_indent == current_indent
            {
                // Find the colon and extract the value
                let after_target = target_pos + quote_offset + TARGET_KEY.len();
                if let Some(colon_offset) = line[after_target..].find(':') {
                    let after_colon = after_target + colon_offset + 1;
                    let value = line[after_colon..].trim();
                    return Ok(Some(value.trim_matches('"').trim_matches('\'')));
                }
            }
        }

        Ok(None)
    }

    /// Extract semantic tokens from the document for syntax highlighting
    /// Returns tokens sorted by position (line, then character)
    pub fn extract_semantic_tokens(content: &str) -> Vec<HydraSemanticToken> {
        let mut tokens = Vec::new();

        // Parse the YAML to get targets and their parameters
        let Ok((targets, _)) = Self::parse(content) else {
            return tokens;
        };

        // Generate tokens for each target
        for target in targets {
            // Tokenize the _target_ value
            Self::tokenize_target_value(&target, &mut tokens);

            // Tokenize parameter keys
            Self::tokenize_parameters(&target, content, &mut tokens);
        }

        // Sort tokens by position (line, then start character)
        tokens.sort_by_key(|t| (t.line, t.start_char));

        tokens
    }

    /// Tokenize a _target_ value, splitting it into namespace and class/function parts
    fn tokenize_target_value(target: &TargetInfo, tokens: &mut Vec<HydraSemanticToken>) {
        let value = &target.value;
        let line = target.line;
        let start = target.value_start;

        // Split by dots to identify module path vs class/function name
        if let Some(last_dot) = value.rfind('.') {
            // Everything before the last dot is the module path
            let module_path = &value[..last_dot];
            let symbol_name = &value[last_dot + 1..];

            // Tokenize module path (could have multiple dots)
            let mut current_pos = start;
            for (idx, part) in module_path.split('.').enumerate() {
                if !part.is_empty() {
                    tokens.push(HydraSemanticToken {
                        line,
                        start_char: current_pos,
                        length: part.len() as u32,
                        token_type: SemanticTokenType::Namespace,
                    });
                    current_pos += part.len() as u32;
                }
                // Skip the dot (unless it's the last segment)
                if idx < module_path.split('.').count() - 1 {
                    current_pos += 1;
                }
            }
            // Skip the last dot
            current_pos += 1;

            // Tokenize the class/function name
            // Heuristic: CamelCase = Class, snake_case = Function
            let token_type = if symbol_name
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false)
            {
                SemanticTokenType::Class
            } else {
                SemanticTokenType::Function
            };

            tokens.push(HydraSemanticToken {
                line,
                start_char: current_pos,
                length: symbol_name.len() as u32,
                token_type,
            });
        } else {
            // No dots, treat the whole thing as a class/function name
            let token_type = if value
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false)
            {
                SemanticTokenType::Class
            } else {
                SemanticTokenType::Function
            };

            tokens.push(HydraSemanticToken {
                line,
                start_char: start,
                length: value.len() as u32,
                token_type,
            });
        }
    }

    /// Tokenize parameter keys and values
    fn tokenize_parameters(
        target: &TargetInfo,
        content: &str,
        tokens: &mut Vec<HydraSemanticToken>,
    ) {
        let lines: Vec<&str> = content.lines().collect();

        for param in &target.parameters {
            let param_line = param.line as usize;
            if param_line >= lines.len() {
                continue;
            }

            let line = lines[param_line];
            let param_key = &param.key;

            // Find the parameter key in the line
            if let Some(key_pos) = line.find(param_key) {
                // Token for parameter key
                tokens.push(HydraSemanticToken {
                    line: param_line as u32,
                    start_char: key_pos as u32,
                    length: param_key.len() as u32,
                    token_type: SemanticTokenType::Parameter,
                });

                // Try to tokenize the value after the colon
                if let Some(colon_pos) = line[key_pos + param_key.len()..].find(':') {
                    let value_start = key_pos + param_key.len() + colon_pos + 1;
                    let value_part = &line[value_start..];

                    // Skip whitespace to find the actual value
                    if let Some(val_offset) = value_part.find(|c: char| !c.is_whitespace()) {
                        let val_start = value_start + val_offset;
                        Self::tokenize_value(
                            &param.value,
                            line,
                            val_start,
                            param_line as u32,
                            false,
                            tokens,
                        );
                    }
                }
            }
        }
    }

    /// Tokenize a single YAML value at the given position.
    /// Returns the length of the tokenized value (for position tracking in sequences).
    /// `in_array` controls delimiter detection: arrays use `,`/`]`, top-level uses `#`/whitespace.
    fn tokenize_value(
        value: &Value,
        line: &str,
        pos: usize,
        line_num: u32,
        in_array: bool,
        tokens: &mut Vec<HydraSemanticToken>,
    ) -> usize {
        if pos >= line.len() {
            return 0;
        }

        let remaining = &line[pos..];

        match value {
            Value::Number(_) => {
                let num_len = remaining
                    .find(|c: char| {
                        !c.is_numeric() && c != '.' && c != '-' && c != 'e' && c != 'E' && c != '+'
                    })
                    .unwrap_or(remaining.len());
                if num_len > 0 {
                    tokens.push(HydraSemanticToken {
                        line: line_num,
                        start_char: pos as u32,
                        length: num_len as u32,
                        token_type: SemanticTokenType::Number,
                    });
                }
                num_len
            }
            Value::String(_) => {
                if remaining.starts_with('"') || remaining.starts_with('\'') {
                    // Quoted string - find closing quote
                    let quote = remaining.chars().next().unwrap();
                    if let Some(end_pos) = remaining[1..].find(quote) {
                        let str_len = end_pos + 2; // Include both quotes
                        tokens.push(HydraSemanticToken {
                            line: line_num,
                            start_char: pos as u32,
                            length: str_len as u32,
                            token_type: SemanticTokenType::String,
                        });
                        str_len
                    } else {
                        remaining.len()
                    }
                } else {
                    // Unquoted string - delimiter depends on context
                    let str_len = if in_array {
                        let len = remaining.find([',', ']']).unwrap_or(remaining.len());
                        remaining[..len].trim_end().len()
                    } else {
                        remaining
                            .find('#')
                            .unwrap_or(remaining.len())
                            .min(remaining.trim_end().len())
                    };
                    if str_len > 0 {
                        tokens.push(HydraSemanticToken {
                            line: line_num,
                            start_char: pos as u32,
                            length: str_len as u32,
                            token_type: SemanticTokenType::String,
                        });
                    }
                    str_len
                }
            }
            Value::Bool(_) => {
                let bool_len = remaining
                    .find(|c: char| c.is_whitespace() || c == '#' || c == ',' || c == ']')
                    .unwrap_or(remaining.len());
                if bool_len > 0 {
                    tokens.push(HydraSemanticToken {
                        line: line_num,
                        start_char: pos as u32,
                        length: bool_len as u32,
                        token_type: SemanticTokenType::Property,
                    });
                }
                bool_len
            }
            Value::Sequence(seq) => {
                // Tokenize array elements individually
                Self::tokenize_sequence_values(seq, line, pos, line_num, tokens)
            }
            _ => {
                // Other value types (null, etc.) - treat as property
                let val_len = remaining
                    .find(|c: char| c.is_whitespace() || c == '#' || c == ',' || c == ']')
                    .unwrap_or(remaining.len());
                if val_len > 0 {
                    tokens.push(HydraSemanticToken {
                        line: line_num,
                        start_char: pos as u32,
                        length: val_len as u32,
                        token_type: SemanticTokenType::Property,
                    });
                }
                val_len
            }
        }
    }

    /// Tokenize values inside a YAML sequence (array) on a single line.
    /// Handles inline arrays like [0.9, 0.999] or ["hello", "world"].
    /// Returns the total length consumed including brackets.
    fn tokenize_sequence_values(
        seq: &[Value],
        line: &str,
        start_pos: usize,
        line_num: u32,
        tokens: &mut Vec<HydraSemanticToken>,
    ) -> usize {
        // Only handle inline arrays (single line)
        let remaining = &line[start_pos..];
        if !remaining.starts_with('[') {
            return 0;
        }

        let mut pos = start_pos + 1; // Skip opening bracket

        for value in seq {
            // Skip whitespace and commas
            while pos < line.len() {
                let c = line[pos..].chars().next().unwrap_or(' ');
                if c.is_whitespace() || c == ',' {
                    pos += 1;
                } else {
                    break;
                }
            }

            if pos >= line.len() {
                break;
            }

            // Tokenize the value and advance position
            let consumed = Self::tokenize_value(value, line, pos, line_num, true, tokens);
            pos += consumed;
        }

        // Find closing bracket to return total length
        if let Some(close_pos) = line[start_pos..].find(']') {
            close_pos + 1
        } else {
            pos - start_pos
        }
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

        if let Value::Number(num) = &first_model.parameters.first().unwrap().value {
            assert_eq!(num.as_i64(), Some(128));
        } else {
            panic!("Expected Number value");
        }

        // Second occurrence (line 6)
        let second_model = targets.get(1).unwrap();
        assert_eq!(second_model.value, "myproject.Model");
        assert_eq!(second_model.line, 6);
        assert_eq!(*line_map.get(&6).unwrap(), 1);
        assert_eq!(second_model.key_start, 4);
        assert_eq!(second_model.parameters.len(), 1);

        // Check the size value
        if let Value::Number(num) = &second_model.parameters.first().unwrap().value {
            assert_eq!(num.as_i64(), Some(256));
        } else {
            panic!("Expected Number value");
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
        if let Value::Number(num) = &target_at_line_3.parameters.first().unwrap().value {
            assert_eq!(
                num.as_i64(),
                Some(128),
                "Line 3's target should have size: 128"
            );
        }

        if let Value::Number(num) = &target_at_line_6.parameters.first().unwrap().value {
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
    fn test_target_with_comments() {
        // Test that we can handle a commented out _target_
        let content = r#"
training:
  trainer:
    logger:
      # - _target_: package.loggers.Logger
      - _target_: package.debug.Logger
        project_name: my_project
"#;
        let (targets, line_map) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 1, "Should have 1 target");
        assert_eq!(line_map.len(), 1, "Line map should have 1 entry");

        let target = targets.first().unwrap();
        assert_eq!(target.value, "package.debug.Logger");
        assert_eq!(target.line, 5);
        assert_eq!(target.key_start, 8);
    }

    #[test]
    fn test_empty_target_value() {
        // Test that we can handle _target_: with empty value
        let content = r#"
model:
  _target_: 
  hidden_size: 256
"#;
        let (targets, _) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 0, "Should have 0 targets");
    }

    #[test]
    fn test_empty_target_value_with_one_valid() {
        // Test that we can handle _target_: with empty value among valid targets
        let content = r#"
model:
  _target_: 
  hidden_size: 256
another:
  _target_: myproject.Model
  param: value
"#;
        let (targets, _) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 1, "Should have 1 target");
        let target = targets.first().unwrap();
        assert_eq!(target.value, "myproject.Model");
        assert_eq!(target.line, 5);
        assert_eq!(target.key_start, 2);
    }

    #[test]
    fn test_commented_out_target_value() {
        // Test that we can handle _target_: with empty value
        let content = r#"
model:
  _target_: # comment
  hidden_size: 256
"#;
        let (targets, _) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 0, "Should have 0 targets");
    }

    #[test]
    fn test_commented_out_target_value_with_one_valid() {
        // Test that we can handle _target_: with empty value among valid targets
        let content = r#"
model:
  _target_: # comment
  hidden_size: 256
another:
  _target_: myproject.Model
  param: value
"#;
        let (targets, _) = YamlParser::parse(content).unwrap();
        assert_eq!(targets.len(), 1, "Should have 1 target");
        let target = targets.first().unwrap();
        assert_eq!(target.value, "myproject.Model");
        assert_eq!(target.line, 5);
        assert_eq!(target.key_start, 2);
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

    #[test]
    fn test_semantic_tokens_simple_target() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
"#;
        let tokens = YamlParser::extract_semantic_tokens(content);

        // Should have tokens for: "myproject" (namespace), "Model" (class),
        // "hidden_size" (parameter), "256" (number)
        assert!(tokens.len() >= 4, "Should have at least 4 tokens");

        // Check module namespace token
        let namespace_token = tokens
            .iter()
            .find(|t| t.line == 2 && t.token_type == SemanticTokenType::Namespace)
            .expect("Should have namespace token");
        assert_eq!(namespace_token.length, "myproject".len() as u32);

        // Check class name token
        let class_token = tokens
            .iter()
            .find(|t| t.line == 2 && t.token_type == SemanticTokenType::Class)
            .expect("Should have class token");
        assert_eq!(class_token.length, "Model".len() as u32);

        // Check parameter key token
        let param_token = tokens
            .iter()
            .find(|t| t.line == 3 && t.token_type == SemanticTokenType::Parameter)
            .expect("Should have parameter token");
        assert_eq!(param_token.length, "hidden_size".len() as u32);

        // Check number value token
        let number_token = tokens
            .iter()
            .find(|t| t.line == 3 && t.token_type == SemanticTokenType::Number)
            .expect("Should have number token");
        assert_eq!(number_token.length, "256".len() as u32);
    }

    #[test]
    fn test_semantic_tokens_nested_module() {
        let content = r#"
model:
  _target_: my.project.models.Transformer
  layers: 12
"#;
        let tokens = YamlParser::extract_semantic_tokens(content);

        // Should have 3 namespace tokens (my, project, models) and 1 class token (Transformer)
        let namespace_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| t.line == 2 && t.token_type == SemanticTokenType::Namespace)
            .collect();
        assert_eq!(namespace_tokens.len(), 3, "Should have 3 namespace tokens");

        let class_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| t.line == 2 && t.token_type == SemanticTokenType::Class)
            .collect();
        assert_eq!(class_tokens.len(), 1, "Should have 1 class token");
    }

    #[test]
    fn test_semantic_tokens_function_target() {
        let content = r#"
func:
  _target_: my.module.create_model
  config: test
"#;
        let tokens = YamlParser::extract_semantic_tokens(content);

        // Function name (lowercase) should be tokenized as Function
        let function_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| t.line == 2 && t.token_type == SemanticTokenType::Function)
            .collect();
        assert_eq!(function_tokens.len(), 1, "Should have 1 function token");
        assert_eq!(function_tokens[0].length, "create_model".len() as u32);
    }

    #[test]
    fn test_semantic_tokens_lsp_encoding() {
        let content = r#"
model:
  _target_: myproject.Model
  size: 100
"#;
        let tokens = YamlParser::extract_semantic_tokens(content);
        let lsp_tokens = HydraSemanticToken::to_lsp_tokens(&tokens);

        // Should have tokens in LSP format
        assert!(!lsp_tokens.is_empty(), "Should have LSP tokens");

        // First token should have delta_line relative to start (0)
        assert_eq!(
            lsp_tokens[0].delta_line, 2,
            "First token should be on line 2"
        );

        // Tokens on same line should have delta_line = 0
        if lsp_tokens.len() > 1 {
            // Find two consecutive tokens on the same line
            for i in 1..lsp_tokens.len() {
                let prev_line = lsp_tokens[..i].iter().fold(0, |acc, t| acc + t.delta_line);
                let curr_line = lsp_tokens[..=i].iter().fold(0, |acc, t| acc + t.delta_line);

                if prev_line == curr_line {
                    assert_eq!(
                        lsp_tokens[i].delta_line, 0,
                        "Token on same line should have delta_line = 0"
                    );
                    break;
                }
            }
        }
    }

    #[test]
    fn test_semantic_tokens_string_values() {
        let content = r#"
model:
  _target_: myproject.Model
  name: "test_model"
  path: '/tmp/model'
"#;
        let tokens = YamlParser::extract_semantic_tokens(content);

        // Should have string tokens for both quoted strings
        let string_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| t.token_type == SemanticTokenType::String)
            .collect();
        assert!(
            string_tokens.len() >= 2,
            "Should have at least 2 string tokens"
        );
    }

    #[test]
    fn test_find_valid_target_with_whitespace_prefix() {
        // Valid: whitespace before _target_
        let yaml = "  _target_: some.module.Class";
        assert!(YamlParser::is_hydra_file(yaml));

        let yaml = "    _target_: another.Class";
        assert!(YamlParser::is_hydra_file(yaml));

        let yaml = "\t_target_: tab.Class";
        assert!(YamlParser::is_hydra_file(yaml));
    }

    #[test]
    fn test_find_valid_target_with_list_item() {
        // Valid: dash with space before _target_
        let yaml = "- _target_: some.module.Class";
        assert!(YamlParser::is_hydra_file(yaml));

        let yaml = "  - _target_: another.Class";
        assert!(YamlParser::is_hydra_file(yaml));

        let yaml = "    - _target_: nested.Class";
        assert!(YamlParser::is_hydra_file(yaml));
    }

    #[test]
    fn test_find_valid_target_with_quotes() {
        // Valid: quoted _target_ with whitespace prefix
        let yaml = "  \"_target_\": some.module.Class";
        assert!(YamlParser::is_hydra_file(yaml));

        let yaml = "  '_target_': another.Class";
        assert!(YamlParser::is_hydra_file(yaml));

        // Valid: quoted _target_ with list item
        let yaml = "- \"_target_\": some.module.Class";
        assert!(YamlParser::is_hydra_file(yaml));

        let yaml = "  - '_target_': another.Class";
        assert!(YamlParser::is_hydra_file(yaml));
    }

    #[test]
    fn test_invalid_target_with_invalid_prefix() {
        // Invalid: non-whitespace before _target_
        let yaml = "key_target_: some.module.Class";
        assert!(!YamlParser::is_hydra_file(yaml));

        let yaml = "my_target_: another.Class";
        assert!(!YamlParser::is_hydra_file(yaml));

        let yaml = "some_prefix_target_: test.Class";
        assert!(!YamlParser::is_hydra_file(yaml));
    }

    #[test]
    fn test_invalid_target_with_double_dash() {
        // Invalid: double dash (not valid YAML list marker)
        let yaml = "-- _target_: some.module.Class";
        assert!(!YamlParser::is_hydra_file(yaml));

        let yaml = "  -- _target_: another.Class";
        assert!(!YamlParser::is_hydra_file(yaml));
    }

    #[test]
    fn test_invalid_target_with_dash_no_space() {
        // Invalid: dash without space after it
        let yaml = "-_target_: some.module.Class";
        assert!(!YamlParser::is_hydra_file(yaml));

        let yaml = "  -_target_: another.Class";
        assert!(!YamlParser::is_hydra_file(yaml));
    }

    #[test]
    fn test_invalid_target_with_dash_multiple_spaces() {
        // Note: multiple spaces after dash should still be valid (just whitespace)
        let yaml = "-  _target_: some.module.Class";
        assert!(YamlParser::is_hydra_file(yaml));

        let yaml = "  -   _target_: another.Class";
        assert!(YamlParser::is_hydra_file(yaml));
    }

    #[test]
    fn test_valid_target_commented_out() {
        // Invalid: commented out _target_
        let yaml = "  # _target_: some.module.Class";
        assert!(!YamlParser::is_hydra_file(yaml));

        let yaml = "# - _target_: another.Class";
        assert!(!YamlParser::is_hydra_file(yaml));
    }

    #[test]
    fn test_multiline_yaml_with_valid_targets() {
        let yaml = r#"
config:
  _target_: some.module.Class
  param1: value1

items:
  - _target_: first.Item
    value: 1
  - _target_: second.Item
    value: 2
"#;
        assert!(YamlParser::is_hydra_file(yaml));

        // Parse and verify all targets are found
        let result = YamlParser::parse(yaml);
        assert!(result.is_ok());
        let (targets, _) = result.unwrap();
        assert_eq!(targets.len(), 3); // config._target_, items[0]._target_, items[1]._target_
    }

    #[test]
    fn test_multiline_yaml_with_invalid_targets() {
        let yaml = r#"
config:
  prefix_target_: some.module.Class
  param1: value1

items:
  key_target_: not.a.valid.Target
"#;
        assert!(!YamlParser::is_hydra_file(yaml));
    }

    #[test]
    fn test_mixed_valid_and_invalid_targets() {
        // This has one valid target and one invalid (as part of a key name)
        let yaml = r#"
valid:
  _target_: some.module.Class
  
invalid_target_key: this is not a target
"#;
        // Should detect as hydra file because of the valid _target_
        assert!(YamlParser::is_hydra_file(yaml));

        let result = YamlParser::parse(yaml);
        assert!(result.is_ok());
        let (targets, _) = result.unwrap();
        // Should only find the one valid target
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].value, "some.module.Class");
    }

    #[test]
    fn test_edge_case_target_at_start_of_file() {
        // Valid: _target_ at very start of file (no indentation)
        let yaml = "_target_: some.module.Class";
        assert!(YamlParser::is_hydra_file(yaml));

        let result = YamlParser::parse(yaml);
        assert!(result.is_ok());
        let (targets, _) = result.unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].value, "some.module.Class");
    }

    #[test]
    fn test_edge_case_list_at_start_of_file() {
        // Valid: list item with _target_ at start of file
        let yaml = "- _target_: some.module.Class";
        assert!(YamlParser::is_hydra_file(yaml));

        let result = YamlParser::parse(yaml);
        assert!(result.is_ok());
        let (targets, _) = result.unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].value, "some.module.Class");
    }

    #[test]
    fn test_parse_nested_sibling_targets() {
        let content = r#"
training:
  lightning_module:
    _target_: made.up.Module

    metrics:
      accuracy:
        _target_: DataLoader
        batch_size: 2

    partial_optimizer:
      _target_: made.up.mod
"#;
        let (targets, _line_map) = YamlParser::parse(content).unwrap();

        assert_eq!(targets.len(), 3, "Should have 3 targets total");

        // Expected order based on document line order
        assert_eq!(
            targets[0].value, "made.up.Module",
            "First target should be made.up.Module"
        );
        assert_eq!(targets[0].line, 3, "First target should be on line 3");

        assert_eq!(
            targets[1].value, "DataLoader",
            "Second target should be DataLoader"
        );
        assert_eq!(targets[1].line, 7, "Second target should be on line 7");

        assert_eq!(
            targets[2].value, "made.up.mod",
            "Third target should be made.up.mod"
        );
        assert_eq!(targets[2].line, 11, "Third target should be on line 11");
    }
}
