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
pub const ARGS_KEY: &str = "_args_";
pub const RECURSIVE_KEY: &str = "_recursive_";
pub const CONVERT_KEY: &str = "_convert_";
pub const HYDRA_KEYWORDS: &[&str] = &[
    TARGET_KEY,
    PARTIAL_KEY,
    ARGS_KEY,
    RECURSIVE_KEY,
    CONVERT_KEY,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertMode {
    None,
    Partial,
    All,
    Object,
}

impl fmt::Display for ConvertMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConvertMode::None => write!(f, "none"),
            ConvertMode::Partial => write!(f, "partial"),
            ConvertMode::All => write!(f, "all"),
            ConvertMode::Object => write!(f, "object"),
        }
    }
}

impl ConvertMode {
    pub fn variants() -> &'static [&'static str] {
        &["none", "partial", "all", "object"]
    }
}

impl std::str::FromStr for ConvertMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(ConvertMode::None),
            "partial" => Ok(ConvertMode::Partial),
            "all" => Ok(ConvertMode::All),
            "object" => Ok(ConvertMode::Object),
            _ => Err(()),
        }
    }
}

/// A YAML value paired with its source position, used for sequence elements.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionedValue {
    pub value: YamlValue,
    pub line: u32,
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum YamlValue {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    Sequence(Vec<PositionedValue>),
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

/// Represents a parameter in a YAML configuration with position information.
/// Can be either a keyword argument (with a key) or a positional argument (from `_args_`).
#[derive(Debug, Clone)]
pub enum Parameter {
    Keyword {
        key: String,
        value: YamlValue,
        line: u32,
        key_start: u32,
        value_start: u32,
        value_end: u32,
        suppressed_rules: HashSet<DiagnosticRule>,
    },
    Positional {
        value: YamlValue,
        line: u32,
        value_start: u32,
        value_end: u32,
        suppressed_rules: HashSet<DiagnosticRule>,
    },
}

impl Parameter {
    pub fn line(&self) -> u32 {
        match self {
            Parameter::Keyword { line, .. } | Parameter::Positional { line, .. } => *line,
        }
    }

    pub fn key(&self) -> Option<&str> {
        match self {
            Parameter::Keyword { key, .. } => Some(key),
            Parameter::Positional { .. } => None,
        }
    }

    pub fn value(&self) -> &YamlValue {
        match self {
            Parameter::Keyword { value, .. } | Parameter::Positional { value, .. } => value,
        }
    }

    pub fn suppressed_rules(&self) -> &HashSet<DiagnosticRule> {
        match self {
            Parameter::Keyword {
                suppressed_rules, ..
            }
            | Parameter::Positional {
                suppressed_rules, ..
            } => suppressed_rules,
        }
    }

    pub fn suppressed_rules_mut(&mut self) -> &mut HashSet<DiagnosticRule> {
        match self {
            Parameter::Keyword {
                suppressed_rules, ..
            }
            | Parameter::Positional {
                suppressed_rules, ..
            } => suppressed_rules,
        }
    }
}

/// Trait for accessing position/span fields of hydra keyword parameters,
/// regardless of the value type `T`.
trait HydraKeywordSpan {
    fn line(&self) -> u32;
    fn invalid(&self) -> bool;
    fn key_start(&self) -> u32;
    fn value_start(&self) -> u32;
    fn value_end(&self) -> u32;
}

/// A Hydra keyword parameter with its parsed value and position information.
#[derive(Debug, Clone, PartialEq)]
pub struct HydraParameter<T> {
    pub value: T,
    pub line: u32,
    pub invalid: bool,
    pub key_start: u32,
    pub value_start: u32,
    pub value_end: u32,
}

impl<T> HydraKeywordSpan for HydraParameter<T> {
    fn line(&self) -> u32 {
        self.line
    }
    fn invalid(&self) -> bool {
        self.invalid
    }
    fn key_start(&self) -> u32 {
        self.key_start
    }
    fn value_start(&self) -> u32 {
        self.value_start
    }
    fn value_end(&self) -> u32 {
        self.value_end
    }
}

#[derive(Debug, Clone)]
pub struct HydraObject {
    /// The `_target_` keyword
    pub target: HydraParameter<String>,
    /// Parameters for the target (keyword arguments + positional from `_args_`)
    pub parameters: Vec<Parameter>,
    /// Diagnostic rules suppressed by an inline comment on the `_target_` line.
    pub suppressed_rules: HashSet<DiagnosticRule>,
    /// The `_partial_` keyword (if present)
    pub partial: Option<HydraParameter<bool>>,
    /// The `_recursive_` keyword (if present)
    pub recursive: Option<HydraParameter<bool>>,
    /// The `_convert_` keyword (if present)
    pub convert: Option<HydraParameter<ConvertMode>>,
    /// The `_args_` keyword (if present).
    /// The value is `Some(InlineArgsText)` for inline flow sequences,
    /// `None` for block sequences or invalid values.
    pub args: Option<HydraParameter<Option<InlineArgsText>>>,
}

impl HydraObject {
    /// Get the end position of the target value
    pub fn target_value_end(&self) -> u32 {
        self.target.value_start + self.target.value.len() as u32
    }

    /// Check if this HydraObject is marked as partial
    pub fn is_partial(&self) -> bool {
        self.partial.as_ref().is_some_and(|p| !p.invalid && p.value)
    }
}

#[derive(Clone)]
pub struct ParsedContent {
    /// All hydra objects found in the document, in the order they appear
    pub hydra_objects: Vec<HydraObject>,
    /// Mapping from line number to index in the hydra_objects vector for quick lookup
    pub target_line_map: HashMap<u32, usize>,
    /// Mapping from parameter line number to (hydra_objects index, parameter context)
    pub param_line_map: HashMap<u32, (usize, ParameterContext)>,
    /// File-wide diagnostic suppressions from header comments
    pub file_suppressions: HashSet<DiagnosticRule>,
}

impl ParsedContent {
    /// Look up the [`HydraObject`] whose `_target_` value contains the cursor
    /// position. Returns `None` when the cursor is on a non-target line or on
    /// the column outside the value range. O(1) on the precomputed
    /// `target_line_map`.
    pub fn target_at_position(&self, position: Position) -> Option<&HydraObject> {
        let line_index = *self.target_line_map.get(&position.line)?;
        let hydra_object = &self.hydra_objects[line_index];
        if position.character > hydra_object.target.value_start
            && position.character < hydra_object.target_value_end()
        {
            Some(hydra_object)
        } else {
            None
        }
    }

    /// Look up the target value and parameter context for a parameter line at
    /// the given position. Returns `None` when the cursor is on a `_target_`
    /// line or an unrelated line. The third element of the tuple is the list
    /// of keyword parameter names already specified in the same Hydra object,
    /// which callers use to detect conflicts between positional (`_args_`) and
    /// keyword assignments. O(1) on the precomputed `param_line_map`.
    pub fn target_for_parameter_line(
        &self,
        position: Position,
    ) -> Option<(String, ResolvedParameterContext, Vec<String>)> {
        if self.target_line_map.contains_key(&position.line) {
            return None;
        }
        let (idx, context) = self.param_line_map.get(&position.line)?;
        let hydra_object = &self.hydra_objects[*idx];
        let (resolved, keyword_keys) = match context {
            ParameterContext::Keyword(key) => {
                (ResolvedParameterContext::Keyword(key.clone()), vec![])
            }
            ParameterContext::Positional(index) => {
                let (num_args, kw_keys) = YamlParser::count_param_kinds(&hydra_object.parameters);
                (
                    ResolvedParameterContext::Positional(*index, num_args),
                    kw_keys,
                )
            }
            ParameterContext::InlinePositional {
                bracket_col,
                text_after_bracket,
            } => {
                let cursor_chars =
                    (position.character as usize).saturating_sub(*bracket_col as usize + 1);
                let end: usize = text_after_bracket
                    .char_indices()
                    .nth(cursor_chars)
                    .map_or(text_after_bracket.len(), |(i, _)| i);
                let index = YamlParser::count_flow_commas(text_after_bracket, 0, end);
                let (num_args, kw_keys) = YamlParser::count_param_kinds(&hydra_object.parameters);
                (
                    ResolvedParameterContext::Positional(index, num_args),
                    kw_keys,
                )
            }
        };
        Some((hydra_object.target.value.clone(), resolved, keyword_keys))
    }

    /// Determine completion context using the precomputed parse maps.
    ///
    /// Uses `target_line_map` and `param_line_map` (O(1) lookups) instead of
    /// scanning the full document backwards. Returns `None` when the cursor
    /// line is not present in either map — i.e. the cursor is on a structural
    /// YAML line (parent mapping key, blank line, comment) that carries no
    /// hydra semantics. Callers should fall back to
    /// `YamlParser::get_completion_context` in that case.
    ///
    /// `line_text` is only the current line (not the full document), truncated
    /// at `position.character` by the caller or sliced here. Providing only
    /// the single line avoids the full-document `String` clone on the hot path.
    pub fn completion_context_at(
        &self,
        position: Position,
        line_text: &str,
    ) -> Option<CompletionContext> {
        let cursor_col = (position.character as usize).min(line_text.len());

        // Cursor is on a `_target_` value line.
        if let Some(&idx) = self.target_line_map.get(&position.line) {
            let target = &self.hydra_objects[idx].target;
            let value_start = (target.value_start as usize).min(cursor_col);
            let partial = line_text[value_start..cursor_col].trim().to_string();
            return Some(CompletionContext::TargetValue { partial });
        }

        // Cursor is on a parameter line belonging to a hydra object.
        if let Some((idx, context)) = self.param_line_map.get(&position.line) {
            let hydra_object = &self.hydra_objects[*idx];
            let prefix = &line_text[..cursor_col];
            return match context {
                ParameterContext::Keyword(key) => {
                    if let Some(colon_pos) = prefix.find(':') {
                        let partial = prefix[colon_pos + 1..].trim().to_string();
                        Some(CompletionContext::ParameterValue {
                            target: hydra_object.target.value.clone(),
                            parameter: key.clone(),
                            partial,
                        })
                    } else {
                        let partial = prefix.trim().to_string();
                        Some(CompletionContext::ParameterKey {
                            target: hydra_object.target.value.clone(),
                            partial,
                        })
                    }
                }
                ParameterContext::Positional(_) | ParameterContext::InlinePositional { .. } => {
                    Some(CompletionContext::Unknown)
                }
            };
        }

        // Line not present in the parse maps
        None
    }

    /// Generate semantic tokens for syntax highlighting from the already
    /// parsed content. Returns tokens sorted by `(line, start_char)`.
    pub fn extract_semantic_tokens(&self) -> Vec<HydraSemanticToken> {
        let mut tokens = Vec::new();
        for hydra_obj in &self.hydra_objects {
            YamlParser::tokenize_hydra_keywords(hydra_obj, &mut tokens);
            YamlParser::tokenize_parameters(hydra_obj, &mut tokens);
        }
        tokens.sort_by_key(|t| (t.line, t.start_char));
        tokens
    }
}

/// Convert a saphyr `MarkedYamlOwned` node to `YamlValue`
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
        YamlValue::Sequence(
            seq.iter()
                .map(|item| PositionedValue {
                    value: node_to_yaml_value(item),
                    line: (item.span.start.line() - 1) as u32,
                    start: item.span.start.col() as u32,
                    end: item.span.end.col() as u32,
                })
                .collect(),
        )
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
    /// Returns a vector of `TargetInfo` and a line-to-index lookup map
    pub fn parse(content: &str) -> Result<ParsedContent, YamlParseError> {
        let docs = MarkedYamlOwned::load_from_str(content)?;
        if content.trim().is_empty() {
            return Ok(ParsedContent {
                hydra_objects: Vec::new(),
                target_line_map: HashMap::new(),
                param_line_map: HashMap::new(),
                file_suppressions: HashSet::new(),
            });
        }

        if docs.len() > 1 {
            return Err(YamlParseError::MiscError(
                "Multiple YAML documents are not supported".to_string(),
            ));
        }

        let mut hydra_objects = Vec::new();
        Self::extract_hydra_objects(&docs[0], content, &mut hydra_objects);

        // Parse file-wide suppressions from header comments before any YAML content
        let file_suppressions = Self::get_filewide_suppressions(content);

        // Build line-to-index and param-line-to-index lookup maps
        let mut target_line_map = HashMap::new();
        let mut param_line_map = HashMap::new();
        for (target_idx, hydra_obj) in hydra_objects.iter().enumerate() {
            target_line_map.insert(hydra_obj.target.line, target_idx);
            Self::build_param_line_map(hydra_obj, target_idx, &mut param_line_map);
        }

        // Attach suppression comments from the raw content to targets/parameters
        Self::attach_suppressions(content, &mut hydra_objects, &target_line_map);

        Ok(ParsedContent {
            hydra_objects,
            target_line_map,
            param_line_map,
            file_suppressions,
        })
    }

    /// Check if a YAML file is a Hydra configuration file
    pub fn is_hydra_file(content: &str) -> bool {
        Self::has_hydra_comment(content) || Self::has_target_keyword(content)
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
        hydra_objects: &mut [HydraObject],
        line_map: &HashMap<u32, usize>,
    ) {
        let mut line_to_parameters: HashMap<u32, (usize, usize)> = HashMap::new();

        for &i in line_map.values() {
            for (pi, param) in hydra_objects[i].parameters.iter().enumerate() {
                line_to_parameters.insert(param.line(), (i, pi));
            }
        }

        for (line_num, line) in content.lines().enumerate() {
            let line_num = line_num as u32;
            if (line_map.contains_key(&line_num) || line_to_parameters.contains_key(&line_num))
                && let Some(rules) = Self::extract_inline_ignore(line)
            {
                if let Some(&ti) = line_map.get(&line_num) {
                    hydra_objects[ti].suppressed_rules.extend(rules);
                } else if let Some(&(ti, pi)) = line_to_parameters.get(&line_num) {
                    hydra_objects[ti].parameters[pi]
                        .suppressed_rules_mut()
                        .extend(rules);
                }
            }
        }
    }

    /// Extract a boolean Hydra keyword from a mapping, returning `Some(HydraParameter)` if present.
    fn extract_bool_keyword(
        map: &LinkedHashMap<MarkedYamlOwned, MarkedYamlOwned>,
        keyword: &str,
    ) -> Option<HydraParameter<bool>> {
        let (key_n, val_node) = map.iter().find(|(k, _)| k.data.as_str() == Some(keyword))?;
        let line = (key_n.span.start.line() - 1) as u32;
        let key_start = key_n.span.start.col() as u32;
        let value_start = val_node.span.start.col() as u32;
        let value_end = val_node.span.end.col() as u32;
        match val_node.data.as_bool() {
            Some(b) => Some(HydraParameter {
                value: b,
                line,
                invalid: false,
                key_start,
                value_start,
                value_end,
            }),
            None => Some(HydraParameter {
                value: false,
                line,
                invalid: true,
                key_start,
                value_start,
                value_end,
            }),
        }
    }

    /// Extract the `_convert_` Hydra keyword from a mapping.
    fn extract_convert_keyword(
        map: &LinkedHashMap<MarkedYamlOwned, MarkedYamlOwned>,
    ) -> Option<HydraParameter<ConvertMode>> {
        let (key_n, val_node) = map
            .iter()
            .find(|(k, _)| k.data.as_str() == Some(CONVERT_KEY))?;
        let line = (key_n.span.start.line() - 1) as u32;
        let key_start = key_n.span.start.col() as u32;
        let value_start = val_node.span.start.col() as u32;
        let value_end = val_node.span.end.col() as u32;
        match val_node
            .data
            .as_str()
            .and_then(|s| s.parse::<ConvertMode>().ok())
        {
            Some(mode) => Some(HydraParameter {
                value: mode,
                line,
                invalid: false,
                key_start,
                value_start,
                value_end,
            }),
            None => Some(HydraParameter {
                value: ConvertMode::None,
                line,
                invalid: true,
                key_start,
                value_start,
                value_end,
            }),
        }
    }

    /// Populate `param_line_map` entries for a single `HydraObject`.
    ///
    /// Keyword parameters get a `Keyword` context. Block-style positional
    /// parameters (each on its own line) get `Positional(index)`. Inline
    /// `_args_` sequences get an `InlinePositional` context using the
    /// [`InlineArgsText`] captured during parsing.
    fn build_param_line_map(
        hydra_obj: &HydraObject,
        target_idx: usize,
        param_line_map: &mut HashMap<u32, (usize, ParameterContext)>,
    ) {
        let args_hp = hydra_obj.args.as_ref();
        let args_line = args_hp.map(|hp| hp.line);
        let mut param_index = 0u32;

        for param in &hydra_obj.parameters {
            match param {
                Parameter::Keyword { key, .. } => {
                    param_line_map.insert(
                        param.line(),
                        (target_idx, ParameterContext::Keyword(key.clone())),
                    );
                }
                Parameter::Positional { line, .. } => {
                    if Some(*line) != args_line {
                        // Block-style positional: each on its own line
                        param_line_map.insert(
                            *line,
                            (target_idx, ParameterContext::Positional(param_index)),
                        );
                    }
                    param_index += 1;
                }
            }
        }

        // Map the _args_ key line for inline sequences only.
        // The InlineArgsText is only present for valid flow sequences.
        if let Some(hp) = args_hp
            && let Some(inline) = &hp.value
        {
            param_line_map.insert(
                hp.line,
                (
                    target_idx,
                    ParameterContext::InlinePositional {
                        bracket_col: inline.bracket_col,
                        text_after_bracket: inline.text_after_bracket.clone(),
                    },
                ),
            );
        }
    }

    /// Extract the `_args_` Hydra keyword from a mapping.
    /// Returns the HydraParameter and any positional parameters parsed from the list.
    /// For inline flow sequences (`[a, b]`), the HydraParameter value carries
    /// an [`InlineArgsText`] with the bracket column and trailing text.
    fn extract_args_keyword(
        map: &LinkedHashMap<MarkedYamlOwned, MarkedYamlOwned>,
        content: &str,
    ) -> Option<(HydraParameter<Option<InlineArgsText>>, Vec<Parameter>)> {
        let (key_n, val_node) = map
            .iter()
            .find(|(k, _)| k.data.as_str() == Some(ARGS_KEY))?;
        let line = (key_n.span.start.line() - 1) as u32;
        let key_start = key_n.span.start.col() as u32;
        let value_start = val_node.span.start.col() as u32;
        let value_end = val_node.span.end.col() as u32;
        if let Some(seq) = val_node.data.as_sequence() {
            let positional_params: Vec<Parameter> = seq
                .iter()
                .map(|item| {
                    let arg_line = (item.span.start.line() - 1) as u32;
                    let arg_value_start = item.span.start.col() as u32;
                    let arg_value_end = item.span.end.col() as u32;
                    Parameter::Positional {
                        value: node_to_yaml_value(item),
                        line: arg_line,
                        value_start: arg_value_start,
                        value_end: arg_value_end,
                        suppressed_rules: HashSet::new(),
                    }
                })
                .collect();

            // Detect inline flow sequence: items share the key line, or the
            // sequence is empty and valid. For inline sequences, find '['
            // on the line and capture the text after it.
            let is_inline =
                positional_params.is_empty() || positional_params.iter().all(|p| p.line() == line);
            let inline_info = if is_inline {
                let key_byte_idx = key_n.span.start.index();
                content[key_byte_idx..].find('[').map(|i| {
                    let abs = key_byte_idx + i;
                    let chars_before_bracket = content[key_byte_idx..abs].chars().count() as u32;
                    InlineArgsText {
                        bracket_col: key_start + chars_before_bracket,
                        text_after_bracket: content[abs + 1..]
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_string(),
                    }
                })
            } else {
                None
            };

            Some((
                HydraParameter {
                    value: inline_info,
                    line,
                    invalid: false,
                    key_start,
                    value_start,
                    value_end,
                },
                positional_params,
            ))
        } else {
            Some((
                HydraParameter {
                    value: None,
                    line,
                    invalid: true,
                    key_start,
                    value_start,
                    value_end,
                },
                Vec::new(),
            ))
        }
    }

    /// Recursively extract all `_target_` references and their parameters
    /// from a marked YAML node
    fn extract_hydra_objects(
        node: &MarkedYamlOwned,
        content: &str,
        hydra_objects: &mut Vec<HydraObject>,
    ) {
        if node.data.contains_mapping_key(TARGET_KEY) {
            let map = node.data.as_mapping().expect("Expected a mapping node");
            let target_entry = map
                .iter()
                .find(|(k, _)| k.data.as_str() == Some(TARGET_KEY))
                .expect("Expected _target_ key");
            let (key_node, value_node) = target_entry;

            let partial = Self::extract_bool_keyword(map, PARTIAL_KEY);
            let recursive = Self::extract_bool_keyword(map, RECURSIVE_KEY);
            let convert = Self::extract_convert_keyword(map);
            let args_result = Self::extract_args_keyword(map, content);

            if let Some(target_str) = value_node.data.as_str() {
                // Saphyr lines are 1-indexed, LSP is 0-indexed
                let line = (key_node.span.start.line() - 1) as u32;
                let key_start = key_node.span.start.col() as u32;

                // For value_start: check if the value is quoted
                let value_start = Self::compute_value_start(value_node, content);
                let value_end = value_start + target_str.len() as u32;

                // Create the target immediately to preserve order
                let obj_index = hydra_objects.len();
                let (args, positional_params) = match args_result {
                    Some((hp, params)) => (Some(hp), params),
                    None => (None, Vec::new()),
                };

                hydra_objects.push(HydraObject {
                    target: HydraParameter {
                        value: target_str.to_string(),
                        line,
                        invalid: false,
                        key_start,
                        value_start,
                        value_end,
                    },
                    parameters: Vec::new(),
                    suppressed_rules: HashSet::new(),
                    partial,
                    recursive,
                    convert,
                    args,
                });

                // Extract keyword parameters from all other keys in this mapping
                let mut parameters = Self::extract_parameters(map, content, hydra_objects);
                // Add positional parameters from _args_
                parameters.extend(positional_params);
                hydra_objects[obj_index].parameters = parameters;
            }
        } else if let Some(map) = node.data.as_mapping() {
            // No _target_ found, recursively process nested mappings
            for (_key, val) in map {
                Self::extract_hydra_objects(val, content, hydra_objects);
            }
        } else if let Some(seq) = node.data.as_sequence() {
            for item in seq {
                Self::extract_hydra_objects(item, content, hydra_objects);
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
        hydra_objects: &mut Vec<HydraObject>,
    ) -> Vec<Parameter> {
        let mut parameters = Vec::new();

        for (key_node, val_node) in map_entries {
            if let Some(key_str) = key_node.data.as_str()
                && !HYDRA_KEYWORDS.contains(&key_str)
            {
                // Recursively check for nested targets
                Self::extract_hydra_objects(val_node, content, hydra_objects);

                let line = (key_node.span.start.line() - 1) as u32;
                let key_start = key_node.span.start.col() as u32;
                let value_start = val_node.span.start.col() as u32;
                let value_end = val_node.span.end.col() as u32;
                let value = node_to_yaml_value(val_node);
                parameters.push(Parameter::Keyword {
                    key: key_str.to_string(),
                    value,
                    line,
                    key_start,
                    value_start,
                    value_end,
                    suppressed_rules: HashSet::new(),
                });
            }
        }

        parameters
    }

    /// Count positional args and collect keyword keys from a parameter list.
    fn count_param_kinds(parameters: &[Parameter]) -> (u32, Vec<String>) {
        let mut num_args = 0u32;
        let mut kw_keys = Vec::new();
        for p in parameters {
            match p {
                Parameter::Positional { .. } => num_args += 1,
                Parameter::Keyword { key, .. } => kw_keys.push(key.clone()),
            }
        }
        (num_args, kw_keys)
    }

    /// Count top-level commas in a slice of a YAML flow sequence.
    ///
    /// Tracks `[]` / `{}` nesting depth and quoted strings so commas inside
    /// nested structures or string literals are not counted.
    fn count_flow_commas(line: &str, start: usize, end: usize) -> u32 {
        let mut depth = 0u32;
        let mut count = 0u32;
        let mut in_quote: Option<char> = None;
        let mut escaped = false;
        for ch in line[start..end].chars() {
            if let Some(q) = in_quote {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == q {
                    in_quote = None;
                }
                continue;
            }
            match ch {
                '"' | '\'' => in_quote = Some(ch),
                '[' | '{' => depth += 1,
                ']' | '}' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => count += 1,
                _ => {}
            }
        }
        count
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

    /// Tokenize a _target_ value, splitting it into namespace and class/function parts
    fn tokenize_target_value(
        value: &str,
        line: u32,
        start: u32,
        tokens: &mut Vec<HydraSemanticToken>,
    ) {
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

    /// Tokenize parameter keys and values using stored position data
    fn tokenize_parameters(hydra_object: &HydraObject, tokens: &mut Vec<HydraSemanticToken>) {
        for param in &hydra_object.parameters {
            let (param_key, param_value, param_line, key_start, value_start, value_end) =
                match param {
                    Parameter::Keyword {
                        key,
                        value,
                        line,
                        key_start,
                        value_start,
                        value_end,
                        ..
                    } => (key, value, *line, *key_start, *value_start, *value_end),
                    Parameter::Positional { .. } => continue,
                };

            // Token for parameter key
            tokens.push(HydraSemanticToken {
                line: param_line,
                start_char: key_start,
                length: param_key.len() as u32,
                token_type: SemanticTokenType::Parameter,
            });

            // Token(s) for parameter value
            if let YamlValue::Sequence(elements) = param_value {
                for elem in elements {
                    let length = elem.end.saturating_sub(elem.start);
                    if length == 0 {
                        continue;
                    }
                    let token_type = match &elem.value {
                        YamlValue::Integer(_) | YamlValue::Float(_) => SemanticTokenType::Number,
                        YamlValue::String(_) => SemanticTokenType::String,
                        YamlValue::Bool(_) => SemanticTokenType::Property,
                        _ => SemanticTokenType::Property,
                    };
                    tokens.push(HydraSemanticToken {
                        line: elem.line,
                        start_char: elem.start,
                        length,
                        token_type,
                    });
                }
            } else {
                let length = value_end.saturating_sub(value_start);
                if length == 0 {
                    continue;
                }
                let token_type = match param_value {
                    YamlValue::Integer(_) | YamlValue::Float(_) => SemanticTokenType::Number,
                    YamlValue::String(_) => SemanticTokenType::String,
                    YamlValue::Bool(_) => SemanticTokenType::Property,
                    _ => SemanticTokenType::Property,
                };
                tokens.push(HydraSemanticToken {
                    line: param_line,
                    start_char: value_start,
                    length,
                    token_type,
                });
            }
        }
    }

    /// Tokenize all hydra keyword keys and their values, including `_target_`
    fn tokenize_hydra_keywords(hydra_object: &HydraObject, tokens: &mut Vec<HydraSemanticToken>) {
        // Tokenize _target_ key and value
        let target = &hydra_object.target;
        tokens.push(HydraSemanticToken {
            line: target.line,
            start_char: target.key_start,
            length: TARGET_KEY.len() as u32,
            token_type: SemanticTokenType::Property,
        });
        Self::tokenize_target_value(&target.value, target.line, target.value_start, tokens);

        if let Some(ref hp) = hydra_object.partial {
            Self::emit_keyword_tokens(PARTIAL_KEY, hp, Some(SemanticTokenType::Property), tokens);
        }
        if let Some(ref hp) = hydra_object.recursive {
            Self::emit_keyword_tokens(RECURSIVE_KEY, hp, Some(SemanticTokenType::Property), tokens);
        }
        if let Some(ref hp) = hydra_object.convert {
            Self::emit_keyword_tokens(CONVERT_KEY, hp, Some(SemanticTokenType::String), tokens);
        }
        if let Some(ref hp) = hydra_object.args {
            // _args_ values are lists, individual elements handled as positional parameters
            Self::emit_keyword_tokens(ARGS_KEY, hp, None, tokens);
        }
    }

    /// Emit key and optional value tokens for a hydra keyword.
    /// Uses `HydraKeywordSpan` to access position fields without requiring a specific generic type.
    fn emit_keyword_tokens(
        keyword: &str,
        hp: &dyn HydraKeywordSpan,
        value_token_type: Option<SemanticTokenType>,
        tokens: &mut Vec<HydraSemanticToken>,
    ) {
        // Key token
        tokens.push(HydraSemanticToken {
            line: hp.line(),
            start_char: hp.key_start(),
            length: keyword.len() as u32,
            token_type: SemanticTokenType::Property,
        });
        // Value token (if applicable and valid)
        if let Some(tt) = value_token_type
            && !hp.invalid()
        {
            let length = hp.value_end().saturating_sub(hp.value_start());
            if length > 0 {
                tokens.push(HydraSemanticToken {
                    line: hp.line(),
                    start_char: hp.value_start(),
                    length,
                    token_type: tt,
                });
            }
        }
    }
}

/// Pre-extracted text from an inline `_args_: [...]` flow sequence.
/// Captured during parsing so that signature-help resolution can count
/// commas without re-reading source content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineArgsText {
    pub bracket_col: u32,
    pub text_after_bracket: String,
}

/// Identifies which parameter the cursor is on, for signature help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParameterContext {
    /// A keyword argument with the given key name.
    Keyword(String),
    /// A positional argument at the given index within `_args_` (block style).
    Positional(u32),
    /// An inline `_args_` sequence (e.g. `_args_: [a, b, c]`).
    /// Stores the column of the opening `[` and the text that follows it
    /// so the cursor column can be resolved to a positional index by
    /// counting commas, without re-reading the source content.
    InlinePositional {
        bracket_col: u32,
        /// Text after the `[` up to end-of-line (e.g. `"a, b, c]"`).
        text_after_bracket: String,
    },
}

/// Resolved parameter context returned by [`ParsedContent::target_for_parameter_line`].
///
/// Unlike [`ParameterContext`] this has no `InlinePositional` variant — inline
/// sequences are resolved to a concrete `Positional` index using the cursor column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedParameterContext {
    Keyword(String),
    /// Positional parameter context from `_args_`.
    /// Fields: (positional_index, num_args_in_yaml)
    /// `num_args_in_yaml` is the number of items actually present in the `_args_`
    /// list, used to distinguish an empty list from a populated one.
    Positional(u32, u32),
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
        assert_eq!(parsed_content.hydra_objects.len(), 1);
        assert_eq!(parsed_content.target_line_map.len(), 1);
        let hydra_object = parsed_content.hydra_objects.first().unwrap();
        assert_eq!(hydra_object.target.value, "myproject.Model");
        assert_eq!(hydra_object.parameters.len(), 2);
        assert_eq!(hydra_object.target.line, 2);
        assert_eq!(parsed_content.target_line_map.get(&2).unwrap(), &0);
        assert_eq!(hydra_object.target.key_start, 2);
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
            parsed_content.hydra_objects.len(),
            3,
            "Should have 3 targets total"
        );
        assert_eq!(
            parsed_content.target_line_map.len(),
            3,
            "Line map should have 3 entries"
        );

        let model = parsed_content.hydra_objects.first().unwrap();
        assert_eq!(model.parameters.len(), 2);
        assert_eq!(model.target.line, 2);
        assert_eq!(parsed_content.target_line_map.get(&2).unwrap(), &0);

        let encoder = parsed_content.hydra_objects.get(1).unwrap();
        assert_eq!(encoder.parameters.len(), 1);
        assert_eq!(encoder.target.line, 4);
        assert_eq!(parsed_content.target_line_map.get(&4).unwrap(), &1);

        let decoder = parsed_content.hydra_objects.get(2).unwrap();
        assert_eq!(decoder.parameters.len(), 1);
        assert_eq!(decoder.target.line, 7);
        assert_eq!(parsed_content.target_line_map.get(&7).unwrap(), &2);
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
        let parsed = YamlParser::parse(content).unwrap();
        let hydra_object = parsed.target_at_position(position).unwrap();
        assert_eq!(hydra_object.target.value, "myproject.Model");
        assert_eq!(hydra_object.target.line, 2);
        assert_eq!(hydra_object.target.key_start, 2);
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
        let parsed = YamlParser::parse(content).unwrap();
        assert!(parsed.target_at_position(position).is_none());
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
        let parsed = YamlParser::parse(content).unwrap();
        assert!(parsed.target_at_position(position).is_none());
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
        let parsed = YamlParser::parse(content).unwrap();
        assert!(parsed.target_at_position(position).is_none());
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
        assert_eq!(
            parsed_content.hydra_objects.len(),
            2,
            "Should have 2 targets"
        );
        assert_eq!(
            parsed_content.target_line_map.len(),
            2,
            "Line map should have 2 entries"
        );

        // First occurrence (line 3)
        let first_model = parsed_content.hydra_objects.first().unwrap();
        assert_eq!(first_model.target.value, "myproject.Model");
        assert_eq!(first_model.target.line, 3);
        assert_eq!(parsed_content.target_line_map.get(&3).unwrap(), &0);
        assert_eq!(first_model.target.key_start, 4);
        assert_eq!(first_model.parameters.len(), 1);

        // Check the size value
        if let YamlValue::Integer(val) = &first_model.parameters.first().unwrap().value() {
            assert_eq!(*val, 128);
        } else {
            panic!("Expected Integer value");
        }

        // Second occurrence (line 6)
        let second_model = parsed_content.hydra_objects.get(1).unwrap();
        assert_eq!(second_model.target.value, "myproject.Model");
        assert_eq!(second_model.target.line, 6);
        assert_eq!(parsed_content.target_line_map.get(&6).unwrap(), &1);
        assert_eq!(second_model.target.key_start, 4);
        assert_eq!(second_model.parameters.len(), 1);

        // Check the size value
        if let YamlValue::Integer(val) = &second_model.parameters.first().unwrap().value() {
            assert_eq!(*val, 256);
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
        assert_eq!(
            parsed_content.hydra_objects.len(),
            2,
            "Should have 2 targets"
        );
        assert_eq!(
            parsed_content.target_line_map.len(),
            2,
            "Line map should have 2 entries"
        );
        assert_eq!(parsed_content.target_line_map.get(&3).unwrap(), &0);
        assert_eq!(parsed_content.target_line_map.get(&6).unwrap(), &1);

        let target_at_line_3 = parsed_content.hydra_objects.first().unwrap();
        let target_at_line_6 = parsed_content.hydra_objects.get(1).unwrap();

        // Verify both targets are correct
        if let YamlValue::Integer(val) = &target_at_line_3.parameters.first().unwrap().value() {
            assert_eq!(val, &128, "Line 3's target should have size: 128");
        }

        if let YamlValue::Integer(val) = &target_at_line_6.parameters.first().unwrap().value() {
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
        assert_eq!(
            parsed_content.hydra_objects.len(),
            2,
            "Should have 2 targets"
        );
        assert_eq!(
            parsed_content.target_line_map.len(),
            2,
            "Line map should have 2 entries"
        );

        // First target with spaces before colon
        let first = parsed_content.hydra_objects.first().unwrap();
        assert_eq!(first.target.value, "myproject.Model");
        assert_eq!(first.target.line, 2);
        assert_eq!(first.target.key_start, 2);
        assert_eq!(first.parameters.len(), 1);

        // Second target with tab before colon
        let second = parsed_content.hydra_objects.get(1).unwrap();
        assert_eq!(second.target.value, "another.Model");
        assert_eq!(second.target.line, 5);
        assert_eq!(second.target.key_start, 2);
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
        assert_eq!(
            parsed_content.hydra_objects.len(),
            1,
            "Should have 1 target"
        );
        assert_eq!(
            parsed_content.target_line_map.len(),
            1,
            "Line map should have 1 entry"
        );

        let hydra_object = parsed_content.hydra_objects.first().unwrap();
        assert_eq!(hydra_object.target.value, "myproject.Model");
        assert_eq!(hydra_object.target.line, 2);
        assert_eq!(hydra_object.target.key_start, 2); // Position of opening quote
        assert_eq!(hydra_object.parameters.len(), 1);
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
        assert_eq!(
            parsed_content.hydra_objects.len(),
            1,
            "Should have 1 target"
        );
        assert_eq!(
            parsed_content.target_line_map.len(),
            1,
            "Line map should have 1 entry"
        );

        let hydra_object = parsed_content.hydra_objects.first().unwrap();
        assert_eq!(hydra_object.target.value, "myproject.Model");
        assert_eq!(hydra_object.target.line, 2);
        assert_eq!(hydra_object.target.key_start, 2); // Position of opening quote
        assert_eq!(hydra_object.parameters.len(), 1);
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
        assert_eq!(
            parsed_content.hydra_objects.len(),
            2,
            "Should have 2 targets"
        );
        assert_eq!(
            parsed_content.target_line_map.len(),
            2,
            "Line map should have 2 entries"
        );

        let first = parsed_content.hydra_objects.first().unwrap();
        assert_eq!(first.target.value, "myproject.Model");
        assert_eq!(first.target.line, 2);
        assert_eq!(first.target.key_start, 2);

        let second = parsed_content.hydra_objects.get(1).unwrap();
        assert_eq!(second.target.value, "another.Model");
        assert_eq!(second.target.line, 5);
        assert_eq!(second.target.key_start, 2);
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
        assert_eq!(
            parsed_content.hydra_objects.len(),
            1,
            "Should have 1 target"
        );
        assert_eq!(
            parsed_content.target_line_map.len(),
            1,
            "Line map should have 1 entry"
        );

        let hydra_object = parsed_content.hydra_objects.first().unwrap();
        assert_eq!(hydra_object.target.value, "package.debug.Logger");
        assert_eq!(hydra_object.target.line, 5);
        assert_eq!(hydra_object.target.key_start, 8);
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
        assert_eq!(
            parsed_content.hydra_objects.len(),
            0,
            "Should have 0 targets"
        );
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
        assert_eq!(
            parsed_content.hydra_objects.len(),
            1,
            "Should have 1 target"
        );
        let hydra_object = parsed_content.hydra_objects.first().unwrap();
        assert_eq!(hydra_object.target.value, "myproject.Model");
        assert_eq!(hydra_object.target.line, 5);
        assert_eq!(hydra_object.target.key_start, 2);
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
        assert_eq!(
            parsed_content.hydra_objects.len(),
            0,
            "Should have 0 targets"
        );
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
        assert_eq!(
            parsed_content.hydra_objects.len(),
            1,
            "Should have 1 target"
        );
        let hydra_object = parsed_content.hydra_objects.first().unwrap();
        assert_eq!(hydra_object.target.value, "myproject.Model");
        assert_eq!(hydra_object.target.line, 5);
        assert_eq!(hydra_object.target.key_start, 2);
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
        let parsed = YamlParser::parse(content).unwrap();
        let hydra_object = parsed.target_at_position(position).unwrap();
        assert_eq!(hydra_object.target.value, "myproject.Model");
        assert_eq!(hydra_object.target.line, 2);
        assert_eq!(hydra_object.target.key_start, 2); // Position of opening quote
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
        let parsed = YamlParser::parse(content).unwrap();
        let hydra_object = parsed.target_at_position(position).unwrap();
        assert_eq!(hydra_object.target.value, "myproject.Model");
        assert_eq!(hydra_object.target.line, 2);
        assert_eq!(hydra_object.target.key_start, 2);
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
            parsed_content.hydra_objects.len(),
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
        assert_eq!(parsed_content.hydra_objects.len(), 1);

        let hydra_object = parsed_content.hydra_objects.first().unwrap();
        assert_eq!(hydra_object.target.value, "myproject.Model");
        assert_eq!(hydra_object.target.line, 2);
        // value_start should point to the first character of the actual value (after the quote)
        assert_eq!(hydra_object.target.value_start, 13); // Position after opening quote
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
        assert_eq!(parsed_content.hydra_objects.len(), 1);

        let hydra_object = parsed_content.hydra_objects.first().unwrap();
        assert_eq!(hydra_object.target.value, "myproject.Model");
        assert_eq!(hydra_object.target.line, 2);
        // value_start should point to the first character of the actual value (after the quote)
        assert_eq!(hydra_object.target.value_start, 13); // Position after opening quote
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
        assert_eq!(parsed_content.hydra_objects.len(), 1);

        let hydra_object = parsed_content.hydra_objects.first().unwrap();
        assert_eq!(hydra_object.target.value, "myproject.Model");
        assert_eq!(hydra_object.target.line, 2);
        assert_eq!(hydra_object.target.key_start, 2); // Position of opening quote of key
        // value_start should point to after the opening quote of the value
        assert_eq!(hydra_object.target.value_start, 15); // Position after opening quote of value
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
        let parsed = YamlParser::parse(content).unwrap();
        let hydra_object = parsed.target_at_position(position).unwrap();
        assert_eq!(hydra_object.target.value, "myproject.Model");
        assert_eq!(hydra_object.target.line, 2);
    }

    #[test]
    fn test_semantic_tokens_simple_target() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
"#;
        let tokens = YamlParser::parse(content)
            .unwrap()
            .extract_semantic_tokens();

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
        let tokens = YamlParser::parse(content)
            .unwrap()
            .extract_semantic_tokens();

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
        let tokens = YamlParser::parse(content)
            .unwrap()
            .extract_semantic_tokens();

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
        let tokens = YamlParser::parse(content)
            .unwrap()
            .extract_semantic_tokens();
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
        let tokens = YamlParser::parse(content)
            .unwrap()
            .extract_semantic_tokens();

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
        assert_eq!(parsed_content.hydra_objects.len(), 3); // config._target_, items[0]._target_, items[1]._target_
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
        assert_eq!(parsed_content.hydra_objects.len(), 1);
        assert_eq!(
            parsed_content.hydra_objects[0].target.value,
            "some.module.Class"
        );
    }

    #[test]
    fn test_edge_case_target_at_start_of_file() {
        // Valid: _target_ at very start of file (no indentation)
        let yaml = "_target_: some.module.Class";
        assert!(YamlParser::is_hydra_file(yaml));

        let result = YamlParser::parse(yaml);
        assert!(result.is_ok());
        let parsed_content = result.unwrap();
        assert_eq!(parsed_content.hydra_objects.len(), 1);
        assert_eq!(
            parsed_content.hydra_objects[0].target.value,
            "some.module.Class"
        );
    }

    #[test]
    fn test_edge_case_list_at_start_of_file() {
        // Valid: list item with _target_ at start of file
        let yaml = "- _target_: some.module.Class";
        assert!(YamlParser::is_hydra_file(yaml));

        let result = YamlParser::parse(yaml);
        assert!(result.is_ok());
        let parsed_content = result.unwrap();
        assert_eq!(parsed_content.hydra_objects.len(), 1);
        assert_eq!(
            parsed_content.hydra_objects[0].target.value,
            "some.module.Class"
        );
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
            parsed_content.hydra_objects.len(),
            3,
            "Should have 3 targets total"
        );

        // Expected order based on document line order
        assert_eq!(
            parsed_content.hydra_objects[0].target.value, "made.up.Module",
            "First target should be made.up.Module"
        );
        assert_eq!(
            parsed_content.hydra_objects[0].target.line, 3,
            "First target should be on line 3"
        );

        assert_eq!(
            parsed_content.hydra_objects[1].target.value, "DataLoader",
            "Second target should be DataLoader"
        );
        assert_eq!(
            parsed_content.hydra_objects[1].target.line, 7,
            "Second target should be on line 7"
        );

        assert_eq!(
            parsed_content.hydra_objects[2].target.value, "made.up.mod",
            "Third target should be made.up.mod"
        );
        assert_eq!(
            parsed_content.hydra_objects[2].target.line, 11,
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
        assert_eq!(parsed_content.hydra_objects.len(), 1);
        let hydra_object = &parsed_content.hydra_objects[0];
        assert_eq!(hydra_object.target.line, 5);

        // Verify each parameter has the correct line
        let find_param = |key: &str| {
            hydra_object
                .parameters
                .iter()
                .find(|p| matches!(p, Parameter::Keyword { key: k, .. } if k == key))
                .unwrap()
        };
        assert_eq!(find_param("bap").line(), 2, "bap should be on line 2");
        assert_eq!(find_param("boop").line(), 4, "boop should be on line 4");
        assert_eq!(find_param("beep").line(), 6, "beep should be on line 6");
        assert_eq!(
            find_param("another").line(),
            8,
            "another should be on line 8"
        );
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
        assert_eq!(parsed_content.hydra_objects.len(), 1);
        let hydra_object = &parsed_content.hydra_objects[0];

        let find_param = |key: &str| {
            hydra_object
                .parameters
                .iter()
                .find(|p| matches!(p, Parameter::Keyword { key: k, .. } if k == key))
                .unwrap()
        };
        assert_eq!(
            find_param("shuffle").line(),
            2,
            "shuffle should be on line 2"
        );
        assert_eq!(
            find_param("batch_size").line(),
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
        assert_eq!(parsed_content.hydra_objects.len(), 1);

        let hydra_object = &parsed_content.hydra_objects[0];
        assert_eq!(hydra_object.target.value, "myproject.Model");
        assert!(
            hydra_object
                .partial
                .as_ref()
                .is_some_and(|p| !p.invalid && p.value),
            "Expected partial=true when _partial_: true"
        );
        assert!(
            hydra_object
                .parameters
                .iter()
                .all(|p| p.key() != Some(PARTIAL_KEY)),
            "_partial_ should not be included in parameters"
        );

        let hidden = hydra_object
            .parameters
            .iter()
            .find(|p| p.key() == Some("hidden_size"))
            .expect("Expected hidden_size parameter");
        assert_eq!(hidden.line(), 4);
        match hidden.value() {
            YamlValue::Integer(v) => assert_eq!(*v, 256),
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
        assert_eq!(parsed_content.hydra_objects.len(), 1);

        let hydra_object = &parsed_content.hydra_objects[0];
        assert_eq!(hydra_object.target.value, "myproject.Model");
        assert_eq!(hydra_object.target.line, 3, "_target_ should be on line 3");
        assert!(
            hydra_object
                .partial
                .as_ref()
                .is_some_and(|p| !p.invalid && p.value),
            "Expected partial=true when _partial_ precedes _target_"
        );
        assert!(
            hydra_object
                .parameters
                .iter()
                .all(|p| p.key() != Some(PARTIAL_KEY)),
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
        assert_eq!(parsed_missing.hydra_objects.len(), 1);
        assert!(
            parsed_missing.hydra_objects[0].partial.is_none(),
            "Expected partial=None when _partial_ is absent"
        );

        // Explicit _partial_: false
        let content_false = r#"
model:
  _target_: myproject.Model
  _partial_: false
  hidden_size: 256
"#;
        let parsed_false = YamlParser::parse(content_false).unwrap();
        assert_eq!(parsed_false.hydra_objects.len(), 1);
        assert!(
            parsed_false.hydra_objects[0]
                .partial
                .as_ref()
                .is_some_and(|p| !p.value),
            "Expected partial.value=false when _partial_: false"
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
        assert_eq!(parsed_content.hydra_objects.len(), 1);

        let hydra_object = &parsed_content.hydra_objects[0];
        assert_eq!(hydra_object.target.value, "myproject.Model");
        assert!(
            hydra_object.partial.as_ref().is_some_and(|p| p.invalid),
            "Expected partial.invalid=true when _partial_ is a string"
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
            parsed_content.hydra_objects.len(),
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
            parsed_content.hydra_objects.len(),
            2,
            "Should find two list item targets"
        );

        let first = &parsed_content.hydra_objects[0];
        assert_eq!(first.target.value, "a.b.C");
        assert_eq!(first.target.line, 2);
        assert_eq!(first.target.key_start, 4, "Key should start after `  - `");
        assert!(
            first
                .partial
                .as_ref()
                .is_some_and(|p| !p.invalid && p.value)
        );

        let second = &parsed_content.hydra_objects[1];
        assert_eq!(second.target.value, "a.b.D");
        assert_eq!(second.target.line, 5);
        assert_eq!(
            second.target.key_start, 4,
            "Key should start after indentation for list item mapping"
        );
        assert!(
            second
                .partial
                .as_ref()
                .is_some_and(|p| !p.invalid && p.value)
        );
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
        assert_eq!(parsed_content.hydra_objects.len(), 2);

        let outer = &parsed_content.hydra_objects[0];
        assert_eq!(outer.target.value, "pkg.Outer");
        assert!(
            outer
                .partial
                .as_ref()
                .is_some_and(|p| !p.invalid && p.value)
        );

        let inner = &parsed_content.hydra_objects[1];
        assert_eq!(inner.target.value, "pkg.Inner");
        assert!(inner.partial.as_ref().is_some_and(|p| !p.value));
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
        assert_eq!(parsed_content.hydra_objects.len(), 1);
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
        assert!(parsed_content.hydra_objects[0].suppressed_rules.is_empty());
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
        assert_eq!(parsed_content.hydra_objects.len(), 1);
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
  _target_: my_module.DB # hydrust: ignore[invalid-hydra-parameter]
  host: localhost
";
        let parsed_content = YamlParser::parse(content).unwrap();
        assert_eq!(parsed_content.hydra_objects.len(), 1);
        assert!(
            parsed_content.hydra_objects[0]
                .suppressed_rules
                .contains(&DiagnosticRule::InvalidHydraParameter)
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
        assert_eq!(parsed_content.hydra_objects.len(), 1);
        let param = parsed_content.hydra_objects[0]
            .parameters
            .iter()
            .find(|p| p.key() == Some("host"))
            .unwrap();
        assert!(
            param
                .suppressed_rules()
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
        assert_eq!(parsed_content.hydra_objects.len(), 1);
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
        assert_eq!(parsed_content.hydra_objects.len(), 1);
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
        let parsed = YamlParser::parse(content).unwrap();
        let (target_value, param_ctx, _) = parsed.target_for_parameter_line(position).unwrap();
        assert_eq!(target_value, "myproject.Model");
        assert_eq!(
            param_ctx,
            ResolvedParameterContext::Keyword("hidden_size".to_string())
        );
    }

    #[test]
    fn test_find_target_for_parameter_line_on_target() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
"#;
        let position = Position::new(2, 10); // on _target_ line
        let parsed = YamlParser::parse(content).unwrap();
        assert!(parsed.target_for_parameter_line(position).is_none());
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
        let parsed = YamlParser::parse(content).unwrap();
        let (target_value, param_ctx, _) = parsed
            .target_for_parameter_line(Position::new(6, 5))
            .unwrap();
        assert_eq!(target_value, "myproject.Optimizer");
        assert_eq!(
            param_ctx,
            ResolvedParameterContext::Keyword("lr".to_string())
        );
    }

    #[test]
    fn test_find_target_for_parameter_line_unrelated() {
        let content = r#"
model:
  _target_: myproject.Model
  hidden_size: 256
"#;
        let position = Position::new(1, 2); // on "model:" line
        let parsed = YamlParser::parse(content).unwrap();
        assert!(parsed.target_for_parameter_line(position).is_none());
    }

    #[test]
    fn test_find_target_for_parameter_line_positional_arg() {
        let content = r#"
model:
  _target_: myproject.Model
  _args_:
    - 10
    - 20
    - 30
  param: value
"#;
        let parsed = YamlParser::parse(content).unwrap();
        // Cursor on first positional arg (line 4: "    - 10")
        let (target_value, param_ctx, _) = parsed
            .target_for_parameter_line(Position::new(4, 5))
            .unwrap();
        assert_eq!(target_value, "myproject.Model");
        assert_eq!(param_ctx, ResolvedParameterContext::Positional(0, 3));

        // Cursor on second positional arg (line 5: "    - 20")
        let (_, param_ctx, _) = parsed
            .target_for_parameter_line(Position::new(5, 5))
            .unwrap();
        assert_eq!(param_ctx, ResolvedParameterContext::Positional(1, 3));

        // Cursor on third positional arg (line 6: "    - 30")
        let (_, param_ctx, _) = parsed
            .target_for_parameter_line(Position::new(6, 5))
            .unwrap();
        assert_eq!(param_ctx, ResolvedParameterContext::Positional(2, 3));

        // Cursor on keyword param (line 7: "  param: value")
        let (_, param_ctx, _) = parsed
            .target_for_parameter_line(Position::new(7, 5))
            .unwrap();
        assert_eq!(
            param_ctx,
            ResolvedParameterContext::Keyword("param".to_string())
        );
    }

    // ==================== Hydra Keyword Tests ====================

    #[test]
    fn test_recursive_true_parsed() {
        let content = r#"
model:
  _target_: myproject.Model
  _recursive_: true
  param: 1
"#;
        let parsed = YamlParser::parse(content).unwrap();
        assert_eq!(parsed.hydra_objects.len(), 1);
        let hydra_object = &parsed.hydra_objects[0];
        let rec = hydra_object
            .recursive
            .as_ref()
            .expect("recursive should be present");
        assert!(rec.value);
        assert!(!rec.invalid);
        // _recursive_ should not appear as a parameter
        assert!(
            hydra_object
                .parameters
                .iter()
                .all(|p| p.key() != Some("_recursive_"))
        );
        assert_eq!(hydra_object.parameters.len(), 1);
    }

    #[test]
    fn test_recursive_false_parsed() {
        let content = r#"
model:
  _target_: myproject.Model
  _recursive_: false
"#;
        let parsed = YamlParser::parse(content).unwrap();
        let hydra_object = &parsed.hydra_objects[0];
        let rec = hydra_object
            .recursive
            .as_ref()
            .expect("recursive should be present");
        assert!(!rec.value);
        assert!(!rec.invalid);
    }

    #[test]
    fn test_recursive_invalid_value() {
        let content = r#"
model:
  _target_: myproject.Model
  _recursive_: "yes"
"#;
        let parsed = YamlParser::parse(content).unwrap();
        let hydra_object = &parsed.hydra_objects[0];
        let rec = hydra_object
            .recursive
            .as_ref()
            .expect("recursive should be present");
        assert!(rec.invalid);
    }

    #[test]
    fn test_convert_valid_values() {
        for mode_str in ConvertMode::variants() {
            let content = format!(
                "\nmodel:\n  _target_: myproject.Model\n  _convert_: {}\n",
                mode_str
            );
            let parsed = YamlParser::parse(&content).unwrap();
            let hydra_object = &parsed.hydra_objects[0];
            let conv = hydra_object
                .convert
                .as_ref()
                .unwrap_or_else(|| panic!("Should parse _convert_: {}", mode_str));
            assert!(!conv.invalid);
        }
    }

    #[test]
    fn test_convert_invalid_value() {
        let content = r#"
model:
  _target_: myproject.Model
  _convert_: invalid
"#;
        let parsed = YamlParser::parse(content).unwrap();
        let hydra_object = &parsed.hydra_objects[0];
        let conv = hydra_object
            .convert
            .as_ref()
            .expect("convert should be present");
        assert!(conv.invalid);
    }

    #[test]
    fn test_args_valid_list() {
        let content = r#"
model:
  _target_: myproject.Model
  _args_: [1, 2, 3]
"#;
        let parsed = YamlParser::parse(content).unwrap();
        let hydra_object = &parsed.hydra_objects[0];
        let args = hydra_object.args.as_ref().expect("args should be present");
        assert!(!args.invalid);
        // Inline flow sequence should have InlineArgsText
        let inline = args
            .value
            .as_ref()
            .expect("should have InlineArgsText for flow sequence");
        assert_eq!(inline.text_after_bracket, "1, 2, 3]");
        // _args_ should not appear as a parameter key
        assert!(
            hydra_object
                .parameters
                .iter()
                .all(|p| p.key() != Some("_args_"))
        );
    }

    #[test]
    fn test_args_invalid_not_list() {
        let content = r#"
model:
  _target_: myproject.Model
  _args_: "not a list"
"#;
        let parsed = YamlParser::parse(content).unwrap();
        let hydra_object = &parsed.hydra_objects[0];
        let args = hydra_object.args.as_ref().expect("args should be present");
        assert!(args.invalid);
    }

    #[test]
    fn test_all_hydra_keywords_excluded_from_parameters() {
        let content = r#"
model:
  _target_: myproject.Model
  _partial_: true
  _recursive_: false
  _convert_: all
  _args_: [1]
  real_param: 42
"#;
        let parsed = YamlParser::parse(content).unwrap();
        let hydra_object = &parsed.hydra_objects[0];
        // 1 positional arg from _args_ + 1 real_param
        assert_eq!(hydra_object.parameters.len(), 2);
        assert!(
            hydra_object
                .parameters
                .iter()
                .any(|p| matches!(p, Parameter::Keyword { key, .. } if key == "real_param"))
        );
        assert!(
            hydra_object
                .parameters
                .iter()
                .any(|p| matches!(p, Parameter::Positional { .. }))
        );
        // No hydra keyword keys in parameters
        for kw in HYDRA_KEYWORDS {
            assert!(hydra_object.parameters.iter().all(|p| p.key() != Some(*kw)));
        }
    }

    #[test]
    fn test_hydra_keywords_not_present_defaults() {
        let content = r#"
model:
  _target_: myproject.Model
  param: 1
"#;
        let parsed = YamlParser::parse(content).unwrap();
        let hydra_object = &parsed.hydra_objects[0];
        assert!(hydra_object.recursive.is_none());
        assert!(hydra_object.convert.is_none());
        assert!(hydra_object.args.is_none());
    }

    #[test]
    fn test_completion_context_recursive_value() {
        let content = r#"
model:
  _target_: myproject.Model
  _recursive_: t
"#;
        let position = Position::new(3, 16);
        let context = YamlParser::get_completion_context(content, position).unwrap();
        match context {
            CompletionContext::ParameterValue {
                parameter, partial, ..
            } => {
                assert_eq!(parameter, "_recursive_");
                assert_eq!(partial, "t");
            }
            _ => panic!("Expected ParameterValue context"),
        }
    }

    #[test]
    fn test_completion_context_convert_value() {
        let content = r#"
model:
  _target_: myproject.Model
  _convert_: par
"#;
        let position = Position::new(3, 17);
        let context = YamlParser::get_completion_context(content, position).unwrap();
        match context {
            CompletionContext::ParameterValue {
                parameter, partial, ..
            } => {
                assert_eq!(parameter, "_convert_");
                assert_eq!(partial, "par");
            }
            _ => panic!("Expected ParameterValue context"),
        }
    }

    #[test]
    fn test_count_flow_commas_ignores_commas_in_quotes() {
        // "a,b" contains a comma but it should not be counted
        let line = r#""a,b", "c", d"#;
        assert_eq!(YamlParser::count_flow_commas(line, 0, line.len()), 2);
    }

    #[test]
    fn test_count_flow_commas_single_quotes() {
        let line = "'a,b', c";
        assert_eq!(YamlParser::count_flow_commas(line, 0, line.len()), 1);
    }

    #[test]
    fn test_count_flow_commas_nested_brackets_with_quotes() {
        let line = r#"["a,b", [1,2]], c"#;
        assert_eq!(YamlParser::count_flow_commas(line, 0, line.len()), 1);
    }

    #[test]
    fn test_count_flow_commas_escaped_quotes() {
        // Escaped quote should not end the quoted state
        let line = r#""a\"b,c", d"#;
        assert_eq!(YamlParser::count_flow_commas(line, 0, line.len()), 1);
    }

    #[test]
    fn test_find_target_for_parameter_line_empty_args() {
        let content = r#"
model:
  _target_: myproject.Model
  _args_: []
  param: value
"#;
        let parsed = YamlParser::parse(content).unwrap();
        // Cursor on _args_ line (line 3), inside the empty brackets
        let (target, param_ctx, _) = parsed
            .target_for_parameter_line(Position::new(3, 11))
            .unwrap();
        assert_eq!(target, "myproject.Model");
        assert_eq!(param_ctx, ResolvedParameterContext::Positional(0, 0));

        // Cursor on keyword param (line 4: "  param: value")
        let (_, param_ctx, _) = parsed
            .target_for_parameter_line(Position::new(4, 5))
            .unwrap();
        assert_eq!(
            param_ctx,
            ResolvedParameterContext::Keyword("param".to_string())
        );
    }

    // ==================== completion_context_at tests ====================

    #[test]
    fn test_completion_context_at_target_value_partial() {
        // Cursor mid-way through the target value on a fully-parsed line.
        let content = "\nmodel:\n  _target_: myproject.Model\n  hidden_size: 256\n";
        // Line 2: "  _target_: myproject.Model"
        //          0123456789012345678
        // value_start for "myproject.Model" is 12 (after "_target_: ").
        let parsed = YamlParser::parse(content).unwrap();
        // cursor at col 15 → "myp" typed so far
        let ctx = parsed
            .completion_context_at(Position::new(2, 15), "  _target_: myproject.Model")
            .unwrap();
        match ctx {
            CompletionContext::TargetValue { partial } => assert_eq!(partial, "myp"),
            _ => panic!("expected TargetValue"),
        }
    }

    #[test]
    fn test_completion_context_at_target_value_full() {
        // Cursor past the end of the value — partial is the full value.
        let content = "\nmodel:\n  _target_: myproject.Model\n  hidden_size: 256\n";
        let parsed = YamlParser::parse(content).unwrap();
        let line = "  _target_: myproject.Model";
        let ctx = parsed
            .completion_context_at(Position::new(2, line.len() as u32), line)
            .unwrap();
        match ctx {
            CompletionContext::TargetValue { partial } => {
                assert_eq!(partial, "myproject.Model")
            }
            _ => panic!("expected TargetValue"),
        }
    }

    #[test]
    fn test_completion_context_at_parameter_key() {
        // Cursor on a parameter line, before the colon → ParameterKey.
        let content = "\nmodel:\n  _target_: myproject.Model\n  hidden_size: 256\n";
        let parsed = YamlParser::parse(content).unwrap();
        // Line 3: "  hidden_size: 256", cursor at col 6 → "hidd"
        let ctx = parsed
            .completion_context_at(Position::new(3, 6), "  hidden_size: 256")
            .unwrap();
        match ctx {
            CompletionContext::ParameterKey { target, partial } => {
                assert_eq!(target, "myproject.Model");
                assert_eq!(partial, "hidd");
            }
            _ => panic!("expected ParameterKey, got {:?}", ctx),
        }
    }

    #[test]
    fn test_completion_context_at_parameter_value() {
        // Cursor after the colon → ParameterValue.
        let content = "\nmodel:\n  _target_: myproject.Model\n  hidden_size: 256\n";
        let parsed = YamlParser::parse(content).unwrap();
        // Line 3: "  hidden_size: 256", cursor at col 17 → "25"
        let ctx = parsed
            .completion_context_at(Position::new(3, 17), "  hidden_size: 256")
            .unwrap();
        match ctx {
            CompletionContext::ParameterValue {
                target,
                parameter,
                partial,
            } => {
                assert_eq!(target, "myproject.Model");
                assert_eq!(parameter, "hidden_size");
                assert_eq!(partial, "25");
            }
            _ => panic!("expected ParameterValue, got {:?}", ctx),
        }
    }

    #[test]
    fn test_completion_context_at_unrecognized_line_returns_none() {
        // Cursor on a line that is not in target_line_map or param_line_map
        // (e.g. a parent mapping key line).
        let content = "\nmodel:\n  _target_: myproject.Model\n  hidden_size: 256\n";
        let parsed = YamlParser::parse(content).unwrap();
        // Line 1: "model:" — not a target or parameter line.
        let ctx = parsed.completion_context_at(Position::new(1, 3), "model:");
        assert!(ctx.is_none(), "expected None for non-target/param line");
    }

    #[test]
    fn test_completion_context_at_positional_args_unknown() {
        // Cursor on an _args_ positional line → Unknown (no key completion).
        let content = "\nmodel:\n  _target_: myproject.Model\n  _args_:\n    - val\n";
        let parsed = YamlParser::parse(content).unwrap();
        // Line 4: "    - val" — a Positional parameter line.
        let ctx = parsed
            .completion_context_at(Position::new(4, 5), "    - val")
            .unwrap();
        assert!(
            matches!(ctx, CompletionContext::Unknown),
            "expected Unknown for positional args"
        );
    }
}
