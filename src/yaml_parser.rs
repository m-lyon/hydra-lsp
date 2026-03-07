use hashlink::LinkedHashMap;
use saphyr::{LoadableYamlNode, MarkedYamlOwned};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use tower_lsp::lsp_types::Position;

use crate::diagnostics::DiagnosticRule;

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

#[derive(Debug, Clone)]
pub enum YamlValue {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    Sequence(Vec<YamlValue>),
    Mapping(LinkedHashMap<String, YamlValue>),
}

impl YamlValue {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            YamlValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn is_mapping(&self) -> bool {
        matches!(self, YamlValue::Mapping(_))
    }
}

/// Error type for YAML parsing
#[derive(Debug)]
pub enum YamlParseError {
    ScanError(saphyr::ScanError),
    MiscError(String),
}

impl fmt::Display for YamlParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            YamlParseError::ScanError(e) => write!(f, "{}", e),
            YamlParseError::MiscError(msg) => write!(f, "{}", msg),
        }
    }
}

impl Error for YamlParseError {}

impl From<saphyr::ScanError> for YamlParseError {
    fn from(err: saphyr::ScanError) -> Self {
        YamlParseError::ScanError(err)
    }
}

/// Represents a parameter in a YAML configuration with position information
/// Can either be a simple value or a nested target
#[derive(Debug, Clone)]
pub struct Parameter {
    pub value: YamlValue,
    pub line: u32,
    pub key: String,
    /// Diagnostic rules suppressed by an inline comment on this parameter's line.
    pub suppressed_rules: HashSet<DiagnosticRule>,
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
    /// Diagnostic rules suppressed by an inline comment on the `_target_` line.
    pub suppressed_rules: HashSet<DiagnosticRule>,
    /// Whether the `_partial_` parameter is set to true for this target
    pub is_partial: bool,
}

impl TargetInfo {
    /// Get the end position of the target value
    pub fn value_end(&self) -> u32 {
        self.value_start + self.value.len() as u32
    }
}

pub struct ParsedContent {
    /// All targets found in the document, in the order they appear
    pub targets: Vec<TargetInfo>,
    /// Mapping from line number to index in the targets vector for quick lookup
    pub line_map: HashMap<u32, usize>,
    /// File-wide diagnostic suppressions from header comments
    pub file_suppressions: HashSet<DiagnosticRule>,
}

/// Convert a saphyr MarkedYamlOwned node to YamlValue
fn node_to_yaml_value(node: &MarkedYamlOwned) -> YamlValue {
    let data = &node.data;
    if data.is_null() {
        YamlValue::Null
    } else if let Some(b) = data.as_bool() {
        YamlValue::Bool(b)
    } else if let Some(i) = data.as_integer() {
        YamlValue::Integer(i)
    } else if let Some(f) = data.as_floating_point() {
        YamlValue::Float(f)
    } else if let Some(s) = data.as_str() {
        YamlValue::String(s.to_string())
    } else if let Some(seq) = data.as_sequence() {
        YamlValue::Sequence(seq.iter().map(node_to_yaml_value).collect())
    } else if let Some(map) = data.as_mapping() {
        let entries = map
            .iter()
            .filter_map(|(k, v)| {
                let key_str = k.data.as_str()?.to_string();
                Some((key_str, node_to_yaml_value(v)))
            })
            .collect();
        YamlValue::Mapping(entries)
    } else {
        YamlValue::Null
    }
}

#[derive(Debug)]
pub struct YamlParser;

impl YamlParser {
    /// Parse YAML content and extract all `_target_` references with their parameters
    /// Returns a vector of TargetInfo and a line-to-index lookup map
    pub fn parse(content: &str) -> Result<ParsedContent, YamlParseError> {
        let docs = MarkedYamlOwned::load_from_str(content)?;
        if content.trim().is_empty() {
            return Ok(ParsedContent {
                targets: Vec::new(),
                line_map: HashMap::new(),
                file_suppressions: HashSet::new(),
            });
        }

        if docs.len() > 1 {
            return Err(YamlParseError::MiscError(
                "Multiple YAML documents are not supported".to_string(),
            ));
        }

        let mut targets = Vec::new();
        Self::extract_targets(&docs[0], content, &mut targets);

        // Parse file-wide suppressions from header comments before any YAML content
        let file_suppressions = Self::get_filewide_suppressions(content);

        // Build line-to-index lookup map
        let mut line_map = HashMap::new();
        for (idx, target) in targets.iter().enumerate() {
            line_map.insert(target.line, idx);
        }

        // Attach suppression comments from the raw content to targets/parameters
        Self::attach_suppressions(content, &mut targets, &line_map);

        Ok(ParsedContent {
            targets,
            line_map,
            file_suppressions,
        })
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

    /// Parse a `# hydrust: ignore[...]` comment, returning the set of rules.
    fn parse_ignore_comment(text: &str) -> Option<HashSet<DiagnosticRule>> {
        let trimmed = text.trim();
        let rest = trimmed.strip_prefix("# hydrust: ignore[")?;
        let rest = rest.strip_suffix(']')?;
        let mut rules = HashSet::new();
        for part in rest.split(',') {
            let code = part.trim();
            if let Some(rule) = DiagnosticRule::from_code(code) {
                rules.insert(rule);
            }
        }
        if rules.is_empty() { None } else { Some(rules) }
    }

    /// Extract an inline ignore directive from a line that contains YAML content
    /// followed by a `# hydrust: ignore[...]` comment.
    fn extract_inline_ignore(line: &str) -> Option<HashSet<DiagnosticRule>> {
        let idx = line.find("# hydrust: ignore[")?;
        let comment_part = &line[idx..];
        Self::parse_ignore_comment(comment_part)
    }

    fn get_filewide_suppressions(content: &str) -> HashSet<DiagnosticRule> {
        let mut file_wide = HashSet::new();

        // Header = leading blanks + comment lines. Stop once we see real YAML content.
        for line in content.lines() {
            let trimmed = line.trim_start();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with('#') {
                if let Some(rules) = Self::parse_ignore_comment(trimmed) {
                    file_wide.extend(rules);
                }
                continue;
            }
            break;
        }
        file_wide
    }

    /// Attach suppression comments from raw content to the parsed targets and parameters.
    /// This is necessary because saphyr's MarkedYamlOwned does not preserve comments, so we need to
    /// map line numbers back to the original content to find any inline suppression comments.
    fn attach_suppressions(
        content: &str,
        targets: &mut [TargetInfo],
        line_map: &HashMap<u32, usize>,
    ) {
        let mut line_to_parameters: HashMap<u32, (usize, usize)> = HashMap::new();

        for &i in line_map.values() {
            for (pi, param) in targets[i].parameters.iter().enumerate() {
                line_to_parameters.insert(param.line, (i, pi));
            }
        }

        for (line_num, line) in content.lines().enumerate() {
            let line_num = line_num as u32;
            if (line_map.contains_key(&line_num) || line_to_parameters.contains_key(&line_num))
                && let Some(rules) = Self::extract_inline_ignore(line)
            {
                if let Some(&ti) = line_map.get(&line_num) {
                    targets[ti].suppressed_rules.extend(rules);
                } else if let Some(&(ti, pi)) = line_to_parameters.get(&line_num) {
                    targets[ti].parameters[pi].suppressed_rules.extend(rules);
                }
            }
        }
    }

    /// Recursively extract all `_target_` references from a marked YAML node
    fn extract_targets(node: &MarkedYamlOwned, content: &str, targets: &mut Vec<TargetInfo>) {
        if node.data.contains_mapping_key(TARGET_KEY) {
            let map = node.data.as_mapping().expect("Expected a mapping node");
            let target_entry = map
                .iter()
                .find(|(k, _)| k.data.as_str() == Some(TARGET_KEY))
                .expect("Expected _target_ key");
            let (key_node, value_node) = target_entry;

            let is_partial = if node.data.contains_mapping_key(PARTIAL_KEY) {
                map.iter()
                    .find(|(k, _)| k.data.as_str() == Some(PARTIAL_KEY))
                    .expect("Expected _partial_ key")
                    .1
                    .data
                    .as_bool()
                    .unwrap_or(false)
            } else {
                false
            };
            if let Some(target_str) = value_node.data.as_str() {
                // Saphyr lines are 1-indexed, LSP is 0-indexed
                let line = (key_node.span.start.line() - 1) as u32;
                let key_start = key_node.span.start.col() as u32;

                // For value_start: check if the value is quoted
                let value_start = Self::compute_value_start(value_node, content);

                // Create the target immediately to preserve order
                let target_index = targets.len();
                targets.push(TargetInfo {
                    value: target_str.to_string(),
                    parameters: Vec::new(),
                    line,
                    key_start,
                    value_start,
                    suppressed_rules: HashSet::new(),
                    is_partial,
                });

                // Extract parameters from all other keys in this mapping
                let parameters = Self::extract_parameters(map, content, targets);
                targets[target_index].parameters = parameters;
            }
        } else if let Some(map) = node.data.as_mapping() {
            // No _target_ found, recursively process nested mappings
            for (_key, val) in map {
                Self::extract_targets(val, content, targets);
            }
        } else if let Some(seq) = node.data.as_sequence() {
            for item in seq {
                Self::extract_targets(item, content, targets);
            }
        }
    }

    /// Compute the value_start position for a target value node.
    /// If the value is quoted, skip the opening quote character.
    fn compute_value_start(value_node: &MarkedYamlOwned, content: &str) -> u32 {
        let col = value_node.span.start.col() as u32;
        let byte_index = value_node.span.start.index();

        // Check if the character at the span start is a quote
        if byte_index < content.len() {
            let ch = content.as_bytes()[byte_index];
            if ch == b'"' || ch == b'\'' {
                return col + 1;
            }
        }
        col
    }

    /// Extract parameters from a mapping that contains a `_target_` key
    fn extract_parameters(
        map_entries: &LinkedHashMap<MarkedYamlOwned, MarkedYamlOwned>,
        content: &str,
        targets: &mut Vec<TargetInfo>,
    ) -> Vec<Parameter> {
        let mut parameters = Vec::new();

        for (key_node, val_node) in map_entries {
            if let Some(key_str) = key_node.data.as_str()
                && key_str != TARGET_KEY
                && key_str != PARTIAL_KEY
            {
                // Recursively check for nested targets
                Self::extract_targets(val_node, content, targets);

                let line = (key_node.span.start.line() - 1) as u32;
                let value = node_to_yaml_value(val_node);
                parameters.push(Parameter {
                    key: key_str.to_string(),
                    value,
                    line,
                    suppressed_rules: HashSet::new(),
                });
            }
        }

        parameters
    }

    /// Find the target info at a specific position
    pub fn find_target_at_position(
        content: &str,
        position: Position,
    ) -> Result<Option<TargetInfo>, YamlParseError> {
        let parsed_content = Self::parse(content)?;
        if let Some(&line_index) = parsed_content.line_map.get(&position.line) {
            let target = &parsed_content.targets[line_index];
            // Check if the column is within the function definition
            if position.character > target.value_start && position.character < target.value_end() {
                return Ok(Some(target.clone()));
            }
        }
        Ok(None)
    }

    /// Find the target value and parameter key for a parameter line at the given position.
    /// Returns `None` if the cursor is on a `_target_` line or an unrelated line.
    pub fn find_target_for_parameter_line(
        content: &str,
        position: Position,
    ) -> Result<Option<(String, String)>, YamlParseError> {
        let parsed_content = Self::parse(content)?;
        // If cursor is on a _target_ line, return None
        if parsed_content.line_map.contains_key(&position.line) {
            return Ok(None);
        }
        // Search targets for a parameter on this line
        for target in &parsed_content.targets {
            for param in &target.parameters {
                if param.line == position.line {
                    return Ok(Some((target.value.clone(), param.key.clone())));
                }
            }
        }
        Ok(None)
    }

    /// Get completion context at a position
    pub fn get_completion_context(
        content: &str,
        position: Position,
    ) -> Result<CompletionContext, YamlParseError> {
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
    ) -> Result<Option<&str>, YamlParseError> {
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
        let Ok(parsed_content) = Self::parse(content) else {
            return tokens;
        };

        // Generate tokens for each target
        for target in parsed_content.targets {
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
        value: &YamlValue,
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
            YamlValue::Integer(_) | YamlValue::Float(_) => {
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
            YamlValue::String(_) => {
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
            YamlValue::Bool(_) => {
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
            YamlValue::Sequence(seq) => {
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
        seq: &[YamlValue],
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
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1);
        assert_eq!(parsed_content.line_map.len(), 1);
        let target = parsed_content.targets.first().unwrap();
        assert_eq!(target.value, "myproject.Model");
        assert_eq!(target.parameters.len(), 2);
        assert_eq!(target.line, 2);
        assert_eq!(parsed_content.line_map.get(&2).unwrap(), &0);
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
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(
            parsed_content.targets.len(),
            3,
            "Should have 3 targets total"
        );
        assert_eq!(
            parsed_content.line_map.len(),
            3,
            "Line map should have 3 entries"
        );

        let model = parsed_content.targets.first().unwrap();
        assert_eq!(model.parameters.len(), 2);
        assert_eq!(model.line, 2);
        assert_eq!(parsed_content.line_map.get(&2).unwrap(), &0);

        let encoder = parsed_content.targets.get(1).unwrap();
        assert_eq!(encoder.parameters.len(), 1);
        assert_eq!(encoder.line, 4);
        assert_eq!(parsed_content.line_map.get(&4).unwrap(), &1);

        let decoder = parsed_content.targets.get(2).unwrap();
        assert_eq!(decoder.parameters.len(), 1);
        assert_eq!(decoder.line, 7);
        assert_eq!(parsed_content.line_map.get(&7).unwrap(), &2);
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
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 2, "Should have 2 targets");
        assert_eq!(
            parsed_content.line_map.len(),
            2,
            "Line map should have 2 entries"
        );

        // First occurrence (line 3)
        let first_model = parsed_content.targets.first().unwrap();
        assert_eq!(first_model.value, "myproject.Model");
        assert_eq!(first_model.line, 3);
        assert_eq!(parsed_content.line_map.get(&3).unwrap(), &0);
        assert_eq!(first_model.key_start, 4);
        assert_eq!(first_model.parameters.len(), 1);

        // Check the size value
        if let YamlValue::Integer(val) = &first_model.parameters.first().unwrap().value {
            assert_eq!(val, &128);
        } else {
            panic!("Expected Integer value");
        }

        // Second occurrence (line 6)
        let second_model = parsed_content.targets.get(1).unwrap();
        assert_eq!(second_model.value, "myproject.Model");
        assert_eq!(second_model.line, 6);
        assert_eq!(parsed_content.line_map.get(&6).unwrap(), &1);
        assert_eq!(second_model.key_start, 4);
        assert_eq!(second_model.parameters.len(), 1);

        // Check the size value
        if let YamlValue::Integer(val) = &second_model.parameters.first().unwrap().value {
            assert_eq!(val, &256);
        } else {
            panic!("Expected Integer value");
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
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 2, "Should have 2 targets");
        assert_eq!(
            parsed_content.line_map.len(),
            2,
            "Line map should have 2 entries"
        );
        assert_eq!(parsed_content.line_map.get(&3).unwrap(), &0);
        assert_eq!(parsed_content.line_map.get(&6).unwrap(), &1);

        let target_at_line_3 = parsed_content.targets.first().unwrap();
        let target_at_line_6 = parsed_content.targets.get(1).unwrap();

        // Verify both targets are correct
        if let YamlValue::Integer(val) = &target_at_line_3.parameters.first().unwrap().value {
            assert_eq!(val, &128, "Line 3's target should have size: 128");
        }

        if let YamlValue::Integer(val) = &target_at_line_6.parameters.first().unwrap().value {
            assert_eq!(val, &256, "Line 6's target should have size: 256");
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
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 2, "Should have 2 targets");
        assert_eq!(
            parsed_content.line_map.len(),
            2,
            "Line map should have 2 entries"
        );

        // First target with spaces before colon
        let first = parsed_content.targets.first().unwrap();
        assert_eq!(first.value, "myproject.Model");
        assert_eq!(first.line, 2);
        assert_eq!(first.key_start, 2);
        assert_eq!(first.parameters.len(), 1);

        // Second target with tab before colon
        let second = parsed_content.targets.get(1).unwrap();
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
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1, "Should have 1 target");
        assert_eq!(
            parsed_content.line_map.len(),
            1,
            "Line map should have 1 entry"
        );

        let target = parsed_content.targets.first().unwrap();
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
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1, "Should have 1 target");
        assert_eq!(
            parsed_content.line_map.len(),
            1,
            "Line map should have 1 entry"
        );

        let target = parsed_content.targets.first().unwrap();
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
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 2, "Should have 2 targets");
        assert_eq!(
            parsed_content.line_map.len(),
            2,
            "Line map should have 2 entries"
        );

        let first = parsed_content.targets.first().unwrap();
        assert_eq!(first.value, "myproject.Model");
        assert_eq!(first.line, 2);
        assert_eq!(first.key_start, 2);

        let second = parsed_content.targets.get(1).unwrap();
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
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1, "Should have 1 target");
        assert_eq!(
            parsed_content.line_map.len(),
            1,
            "Line map should have 1 entry"
        );

        let target = parsed_content.targets.first().unwrap();
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
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 0, "Should have 0 targets");
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
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1, "Should have 1 target");
        let target = parsed_content.targets.first().unwrap();
        assert_eq!(target.value, "myproject.Model");
        assert_eq!(target.line, 5);
        assert_eq!(target.key_start, 2);
    }

    #[test]
    fn test_commented_out_target_value() {
        // Test that we can handle _target_: with commented value
        let content = r#"
model:
  _target_: # comment
  hidden_size: 256
"#;
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 0, "Should have 0 targets");
    }

    #[test]
    fn test_commented_out_target_value_with_one_valid() {
        // Test that we can handle _target_: with commented value among valid targets
        let content = r#"
model:
  _target_: # comment
  hidden_size: 256
another:
  _target_: myproject.Model
  param: value
"#;
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1, "Should have 1 target");
        let target = parsed_content.targets.first().unwrap();
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
        let parsed_content = YamlParser::parse(content).unwrap();
        // Should not find the target because there's an unexpected quote
        assert_eq!(
            parsed_content.targets.len(),
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
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1);

        let target = parsed_content.targets.first().unwrap();
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
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1);

        let target = parsed_content.targets.first().unwrap();
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
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1);

        let target = parsed_content.targets.first().unwrap();
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
        let parsed_content = result.unwrap();
        assert_eq!(parsed_content.targets.len(), 3); // config._target_, items[0]._target_, items[1]._target_
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
        let parsed_content = result.unwrap();
        // Should only find the one valid target
        assert_eq!(parsed_content.targets.len(), 1);
        assert_eq!(parsed_content.targets[0].value, "some.module.Class");
    }

    #[test]
    fn test_edge_case_target_at_start_of_file() {
        // Valid: _target_ at very start of file (no indentation)
        let yaml = "_target_: some.module.Class";
        assert!(YamlParser::is_hydra_file(yaml));

        let result = YamlParser::parse(yaml);
        assert!(result.is_ok());
        let parsed_content = result.unwrap();
        assert_eq!(parsed_content.targets.len(), 1);
        assert_eq!(parsed_content.targets[0].value, "some.module.Class");
    }

    #[test]
    fn test_edge_case_list_at_start_of_file() {
        // Valid: list item with _target_ at start of file
        let yaml = "- _target_: some.module.Class";
        assert!(YamlParser::is_hydra_file(yaml));

        let result = YamlParser::parse(yaml);
        assert!(result.is_ok());
        let parsed_content = result.unwrap();
        assert_eq!(parsed_content.targets.len(), 1);
        assert_eq!(parsed_content.targets[0].value, "some.module.Class");
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
        let parsed_content = YamlParser::parse(content).unwrap();

        assert_eq!(
            parsed_content.targets.len(),
            3,
            "Should have 3 targets total"
        );

        // Expected order based on document line order
        assert_eq!(
            parsed_content.targets[0].value, "made.up.Module",
            "First target should be made.up.Module"
        );
        assert_eq!(
            parsed_content.targets[0].line, 3,
            "First target should be on line 3"
        );

        assert_eq!(
            parsed_content.targets[1].value, "DataLoader",
            "Second target should be DataLoader"
        );
        assert_eq!(
            parsed_content.targets[1].line, 7,
            "Second target should be on line 7"
        );

        assert_eq!(
            parsed_content.targets[2].value, "made.up.mod",
            "Third target should be made.up.mod"
        );
        assert_eq!(
            parsed_content.targets[2].line, 11,
            "Third target should be on line 11"
        );
    }

    #[test]
    fn test_params_before_target_get_correct_lines() {
        let content = r#"
my_module:
  bap: false
  # comment
  boop: true
  _target_: myproject.Model
  beep: 42
  # comment
  another: 123
  # comment
"#;
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1);
        let target = &parsed_content.targets[0];
        assert_eq!(target.line, 5);

        // Verify each parameter has the correct line
        let find_param = |key: &str| target.parameters.iter().find(|p| p.key == key).unwrap();
        assert_eq!(find_param("bap").line, 2, "bap should be on line 2");
        assert_eq!(find_param("boop").line, 4, "boop should be on line 4");
        assert_eq!(find_param("beep").line, 6, "beep should be on line 6");
        assert_eq!(find_param("another").line, 8, "another should be on line 8");
    }

    #[test]
    fn test_params_only_before_target() {
        let content = r#"
my_module:
  shuffle: true
  batch_size: 32
  _target_: myproject.Model
"#;
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1);
        let target = &parsed_content.targets[0];

        let find_param = |key: &str| target.parameters.iter().find(|p| p.key == key).unwrap();
        assert_eq!(find_param("shuffle").line, 2, "shuffle should be on line 2");
        assert_eq!(
            find_param("batch_size").line,
            3,
            "batch_size should be on line 3"
        );
    }

    // ==================== parsing _partial_ tests ====================
    #[test]
    fn test_partial_true_sets_is_partial_and_excludes_parameter() {
        let content = r#"
model:
  _target_: myproject.Model
  _partial_: true
  hidden_size: 256
"#;
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1);

        let target = &parsed_content.targets[0];
        assert_eq!(target.value, "myproject.Model");
        assert!(
            target.is_partial,
            "Expected is_partial=true when _partial_: true"
        );
        assert!(
            target.parameters.iter().all(|p| p.key != PARTIAL_KEY),
            "_partial_ should not be included in parameters"
        );

        let hidden = target
            .parameters
            .iter()
            .find(|p| p.key == "hidden_size")
            .expect("Expected hidden_size parameter");
        assert_eq!(hidden.line, 4);
        match hidden.value {
            YamlValue::Integer(v) => assert_eq!(v, 256),
            _ => panic!("Expected Integer value for hidden_size"),
        }
    }

    #[test]
    fn test_partial_before_target_still_applies() {
        let content = r#"
model:
  _partial_: true
  _target_: myproject.Model
  hidden_size: 256
"#;
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1);

        let target = &parsed_content.targets[0];
        assert_eq!(target.value, "myproject.Model");
        assert_eq!(target.line, 3, "_target_ should be on line 3");
        assert!(
            target.is_partial,
            "Expected is_partial=true when _partial_ precedes _target_"
        );
        assert!(
            target.parameters.iter().all(|p| p.key != PARTIAL_KEY),
            "_partial_ should not be included in parameters"
        );
    }

    #[test]
    fn test_partial_false_or_missing_defaults_to_false() {
        // Missing _partial_
        let content_missing = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
"#;
        let parsed_missing = YamlParser::parse(content_missing).unwrap();
        assert_eq!(parsed_missing.targets.len(), 1);
        assert!(
            !parsed_missing.targets[0].is_partial,
            "Expected is_partial=false when _partial_ is absent"
        );

        // Explicit _partial_: false
        let content_false = r#"
model:
  _target_: myproject.Model
  _partial_: false
  hidden_size: 256
"#;
        let parsed_false = YamlParser::parse(content_false).unwrap();
        assert_eq!(parsed_false.targets.len(), 1);
        assert!(
            !parsed_false.targets[0].is_partial,
            "Expected is_partial=false when _partial_: false"
        );
    }

    #[test]
    fn test_partial_non_bool_value_is_treated_as_false() {
        // Quoted "true" is a string, not a YAML bool.
        let content = r#"
model:
  _target_: myproject.Model
  _partial_: "true"
  hidden_size: 256
"#;
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1);

        let target = &parsed_content.targets[0];
        assert_eq!(target.value, "myproject.Model");
        assert!(
            !target.is_partial,
            "Expected is_partial=false when _partial_ is a string"
        );
    }

    #[test]
    fn test_partial_does_not_create_target_without_target_key() {
        let content = r#"
model:
  _partial_: true
  hidden_size: 256
"#;
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(
            parsed_content.targets.len(),
            0,
            "Should not create a target when _target_ is missing"
        );
    }

    #[test]
    fn test_partial_in_list_items() {
        let content = r#"
items:
  - _target_: a.b.C
    _partial_: true
  - _partial_: true
    _target_: a.b.D
"#;
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(
            parsed_content.targets.len(),
            2,
            "Should find two list item targets"
        );

        let first = &parsed_content.targets[0];
        assert_eq!(first.value, "a.b.C");
        assert_eq!(first.line, 2);
        assert_eq!(first.key_start, 4, "Key should start after `  - `");
        assert!(first.is_partial);

        let second = &parsed_content.targets[1];
        assert_eq!(second.value, "a.b.D");
        assert_eq!(second.line, 5);
        assert_eq!(
            second.key_start, 4,
            "Key should start after indentation for list item mapping"
        );
        assert!(second.is_partial);
    }

    #[test]
    fn test_partial_nested_targets_independent_flags() {
        let content = r#"
outer:
  _target_: pkg.Outer
  _partial_: true
  inner:
    _target_: pkg.Inner
    _partial_: false
"#;
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 2);

        let outer = &parsed_content.targets[0];
        assert_eq!(outer.value, "pkg.Outer");
        assert!(outer.is_partial);

        let inner = &parsed_content.targets[1];
        assert_eq!(inner.value, "pkg.Inner");
        assert!(!inner.is_partial);
    }

    // ==================== suppression comment tests ====================

    #[test]
    fn test_file_suppression_in_header() {
        let content = "\
# hydrust: ignore[missing-argument, unknown-argument]
# @hydra
db:
  _target_: my_module.DB
  host: localhost
";
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1);
        assert!(
            parsed_content
                .file_suppressions
                .contains(&DiagnosticRule::MissingArgument)
        );
        assert!(
            parsed_content
                .file_suppressions
                .contains(&DiagnosticRule::UnknownArgument)
        );
        assert!(parsed_content.targets[0].suppressed_rules.is_empty());
    }

    #[test]
    fn test_file_suppression_with_blanks_in_header() {
        let content = "\
# hydrust: ignore[unresolved-import]

# @hydra

db:
  _target_: my_module.DB
";
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1);
        assert!(
            parsed_content
                .file_suppressions
                .contains(&DiagnosticRule::UnresolvedImport)
        );
    }

    #[test]
    fn test_inline_suppression_on_target_line() {
        let content = "\
db:
  _target_: my_module.DB # hydrust: ignore[invalid-target]
  host: localhost
";
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1);
        assert!(
            parsed_content.targets[0]
                .suppressed_rules
                .contains(&DiagnosticRule::InvalidTarget)
        );
        assert!(parsed_content.file_suppressions.is_empty());
    }

    #[test]
    fn test_inline_suppression_on_parameter_line() {
        let content = "\
db:
  _target_: my_module.DB
  host: localhost # hydrust: ignore[unknown-argument]
";
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1);
        let param = parsed_content.targets[0]
            .parameters
            .iter()
            .find(|p| p.key == "host")
            .unwrap();
        assert!(
            param
                .suppressed_rules
                .contains(&DiagnosticRule::UnknownArgument)
        );
    }

    #[test]
    fn test_suppression_comment_after_content_not_file_wide() {
        // A suppression comment that appears after YAML content starts
        // should NOT be treated as file-wide.
        let content = "\
db:
  _target_: my_module.DB
# hydrust: ignore[missing-argument]
  host: localhost
";
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1);
        assert!(parsed_content.file_suppressions.is_empty());
    }

    #[test]
    fn test_invalid_suppression_rule_ignored() {
        let content = "\
# hydrust: ignore[fake-rule]
db:
  _target_: my_module.DB
";
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.targets.len(), 1);
        assert!(parsed_content.file_suppressions.is_empty());
    }
    #[test]
    fn test_file_suppression_multiple_header_comment_lines() {
        let content = "\
# hydrust: ignore[missing-argument]
  # hydrust: ignore[unknown-argument]
# @hydra

db:
  _target_: my_module.DB
  host: localhost
";
        let parsed_content = YamlParser::parse(content).unwrap();
        assert!(
            parsed_content
                .file_suppressions
                .contains(&DiagnosticRule::MissingArgument)
        );
        assert!(
            parsed_content
                .file_suppressions
                .contains(&DiagnosticRule::UnknownArgument)
        );
    }

    #[test]
    fn test_find_target_for_parameter_line_on_param() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
  num_layers: 12
"#;
        let position = Position::new(3, 5); // on hidden_size line
        let (target_value, param_key) =
            YamlParser::find_target_for_parameter_line(content, position)
                .unwrap()
                .unwrap();
        assert_eq!(target_value, "myproject.Model");
        assert_eq!(param_key, "hidden_size");
    }

    #[test]
    fn test_find_target_for_parameter_line_on_target() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
"#;
        let position = Position::new(2, 10); // on _target_ line
        let result = YamlParser::find_target_for_parameter_line(content, position).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_find_target_for_parameter_line_second_target() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
optimizer:
  _target_: myproject.Optimizer
  lr: 0.001
"#;
        // Cursor on lr line (parameter of second target)
        let (target_value, param_key) =
            YamlParser::find_target_for_parameter_line(content, Position::new(6, 5))
                .unwrap()
                .unwrap();
        assert_eq!(target_value, "myproject.Optimizer");
        assert_eq!(param_key, "lr");
    }

    #[test]
    fn test_find_target_for_parameter_line_unrelated() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
"#;
        let position = Position::new(1, 2); // on "model:" line
        let result = YamlParser::find_target_for_parameter_line(content, position).unwrap();
        assert!(result.is_none());
    }
}
