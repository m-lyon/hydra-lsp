//! Signature-help and go-to-definition results built from a resolved Python
//! definition.

use tower_lsp::lsp_types::*;

use crate::python::format::render_params;
use crate::python::model::{
    DefaultStyle, DefinitionInfo, FunctionSignature, ParameterInfo, ParameterKind, SourcePosition,
    SourceRange,
};
use crate::yaml_parser::ResolvedParameterContext;

/// Convert a ParameterInfo to LSP ParameterInformation
fn to_parameter_information(p: &ParameterInfo) -> ParameterInformation {
    ParameterInformation {
        label: ParameterLabel::Simple(p.label(DefaultStyle::Omit)),
        documentation: p.default_value.as_ref().map(|dv| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("Default: `{}`", dv),
            })
        }),
    }
}

/// Build signature label and parameter information from a list of parameters.
/// Returns the label string, the LSP parameter info list, and the filtered
/// `ParameterInfo` references (needed for active-parameter resolution).
fn build_signature_params<'a>(
    params: &'a [ParameterInfo],
    filter_param: Option<&str>,
) -> (String, Vec<ParameterInformation>, Vec<&'a ParameterInfo>) {
    let filtered: Vec<_> = params
        .iter()
        .filter(|p| filter_param.is_none_or(|f| p.name != f))
        .collect();
    let param_strs = render_params(&filtered, |p| p.label(DefaultStyle::Omit));

    let param_infos: Vec<ParameterInformation> = filtered
        .iter()
        .map(|p| to_parameter_information(p))
        .collect();
    (param_strs.join(", "), param_infos, filtered)
}

impl From<SourceRange> for Range {
    fn from(range: SourceRange) -> Self {
        let position = |p: SourcePosition| Position::new(p.line, p.column);
        Range::new(position(range.start), position(range.end))
    }
}

/// Where go-to-definition lands for a target, as ranges in one file.
///
/// An overloaded function or method lands on each of its (already narrowed)
/// overloads.
pub(super) fn definition_ranges(definition_info: &DefinitionInfo) -> Vec<SourceRange> {
    let signature = match definition_info {
        DefinitionInfo::Function(sig) => sig,
        DefinitionInfo::Method(method_info) => &method_info.signature,
        DefinitionInfo::Class(_) => return vec![definition_info.range()],
    };
    if signature.overloads.is_empty() {
        return vec![definition_info.range()];
    }
    let mut ranges: Vec<_> = signature.overloads.iter().map(|sig| sig.range).collect();
    if !signature.is_overloaded {
        ranges.push(signature.range);
    }
    ranges
}

/// Returns the index in `params` of the parameter that the cursor is on, or
/// `params.len()` when no parameter should be highlighted.
fn active_parameter(params: &[&ParameterInfo], param_context: &ResolvedParameterContext) -> u32 {
    let position_of = |kind| params.iter().position(|p| p.kind == kind);
    let none = params.len();
    let active = match param_context {
        ResolvedParameterContext::Keyword(key) => params
            .iter()
            .position(|p| p.name == *key && !p.is_variadic())
            .or_else(|| position_of(ParameterKind::VarKeyword))
            .unwrap_or(none),
        ResolvedParameterContext::Positional { num_args: 0, .. } => none,
        ResolvedParameterContext::Positional {
            index,
            keyword_keys,
            ..
        } => {
            let pos = params
                .iter()
                .enumerate()
                .filter(|(_, p)| p.binds_positionally())
                .nth(*index as usize)
                .map(|(i, _)| i)
                .or_else(|| position_of(ParameterKind::VarPositional));
            match pos {
                Some(i) if !keyword_keys.contains(&params[i].name) => i,
                _ => none,
            }
        }
    };
    active as u32
}

/// Build one signature-help entry: show `label_prefix(...)` with the active parameter
/// under the cursor, or just `label_prefix()` when there is no callable signature
/// (for example, a class with no constructor).
///
/// If the cursor's key matches no parameter, the active index is set past the
/// end so the client leaves no parameter highlighted by default.
pub(super) fn signature_information(
    label_prefix: &str,
    sig: Option<&FunctionSignature>,
    implicit_param: Option<&str>,
    param_context: &ResolvedParameterContext,
) -> SignatureInformation {
    let (params_str, parameters, param_infos) = match sig {
        Some(sig) => build_signature_params(&sig.parameters, implicit_param),
        None => (String::new(), vec![], vec![]),
    };
    let active_parameter = active_parameter(&param_infos, param_context);

    SignatureInformation {
        label: format!("{label_prefix}({params_str})"),
        documentation: None,
        parameters: if parameters.is_empty() {
            None
        } else {
            Some(parameters)
        },
        active_parameter: Some(active_parameter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== active_parameter tests ====================

    fn param(name: &str, kind: ParameterKind) -> ParameterInfo {
        ParameterInfo {
            name: name.to_string(),
            kind,
            type_annotation: None,
            default_value: None,
        }
    }

    /// `def f(a, /, b, *args, c, **kwargs)`
    fn full_signature() -> Vec<ParameterInfo> {
        vec![
            param("a", ParameterKind::PositionalOnly),
            param("b", ParameterKind::PositionalOrKeyword),
            param("args", ParameterKind::VarPositional),
            param("c", ParameterKind::KeywordOnly),
            param("kwargs", ParameterKind::VarKeyword),
        ]
    }

    fn active(params: &[ParameterInfo], ctx: ResolvedParameterContext) -> u32 {
        let refs: Vec<&ParameterInfo> = params.iter().collect();
        active_parameter(&refs, &ctx)
    }

    fn keyword(key: &str) -> ResolvedParameterContext {
        ResolvedParameterContext::Keyword(key.to_string())
    }

    fn positional(index: u32, num_args: u32, keyword_keys: &[&str]) -> ResolvedParameterContext {
        ResolvedParameterContext::Positional {
            index,
            num_args,
            keyword_keys: keyword_keys.iter().map(|k| k.to_string()).collect(),
        }
    }

    #[test]
    fn test_active_parameter_keyword_match() {
        let params = full_signature();
        assert_eq!(active(&params, keyword("b")), 1);
        assert_eq!(active(&params, keyword("c")), 3);
    }

    #[test]
    fn test_active_parameter_keyword_falls_back_to_kwargs() {
        let params = full_signature();
        assert_eq!(active(&params, keyword("unknown")), 4);
        // A key naming a variadic parameter is not that parameter.
        assert_eq!(active(&params, keyword("args")), 4);
    }

    #[test]
    fn test_active_parameter_keyword_no_match() {
        let params = vec![param("a", ParameterKind::PositionalOrKeyword)];
        assert_eq!(active(&params, keyword("unknown")), 1);
        assert_eq!(active(&[], keyword("unknown")), 0);
    }

    #[test]
    fn test_active_parameter_positional_index() {
        let params = full_signature();
        // Positional-only and positional-or-keyword both bind in order.
        assert_eq!(active(&params, positional(0, 3, &[])), 0);
        assert_eq!(active(&params, positional(1, 3, &[])), 1);
    }

    #[test]
    fn test_active_parameter_positional_overflow_into_args() {
        let params = full_signature();
        // Past the last positional parameter, `*args` takes the rest; the
        // keyword-only `c` is never filled positionally.
        assert_eq!(active(&params, positional(2, 3, &[])), 2);
        assert_eq!(active(&params, positional(5, 6, &[])), 2);
    }

    #[test]
    fn test_active_parameter_positional_stops_at_keyword_only() {
        // `def f(a, *, c)`: no `*args`, so a second `_args_` entry fills nothing.
        let params = vec![
            param("a", ParameterKind::PositionalOrKeyword),
            param("c", ParameterKind::KeywordOnly),
        ];
        assert_eq!(active(&params, positional(1, 2, &[])), 2);
    }

    #[test]
    fn test_active_parameter_positional_suppressed() {
        let params = full_signature();
        // An empty `_args_` list passes nothing.
        assert_eq!(active(&params, positional(0, 0, &[])), 5);
        // A parameter also given by keyword is not highlighted.
        assert_eq!(active(&params, positional(1, 2, &["b"])), 5);
    }
}
