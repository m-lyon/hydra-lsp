//! Regression tests for the UTF-16 / byte / codepoint encoding fixes.
//!
//! Background: three unit systems collide in this server:
//!   * saphyr reports `col()`/`index()` in **codepoints**,
//!   * Rust `str` indexing is in **bytes**,
//!   * the LSP default `positionEncoding` is **UTF-16 code units**.
//!
//! These diverge for any non-ASCII char (bytes) and for astral chars
//! (codepoints vs UTF-16).

use hydra_lsp::yaml_parser::{CompletionContext, Parameter, YamlParser};
use tower_lsp::lsp_types::Position;

/// FM1 (Group C) — cursor column vs byte slicing on a `_target_` value line.
///
/// The cursor position arrives as a UTF-16 column, but the line is sliced by
/// byte offset. When a multibyte character precedes the cursor those units
/// diverge, and an encoding-unaware slice can land inside a character and panic.
/// Here the cursor sits just after `café` (UTF-16 column 16), a byte offset that
/// falls in the middle of `é`. Completion must return the typed text intact.
#[test]
fn fm1_completion_at_multibyte_target_value() {
    let content = "# @hydra\nm:\n  _target_: café";
    let parsed = YamlParser::parse(content).expect("parse should succeed");
    let line_text = "  _target_: café";

    let ctx = parsed.completion_context_at(
        Position {
            line: 2,
            character: 16,
        },
        line_text,
    );

    match ctx {
        Some(CompletionContext::TargetValue { partial }) => {
            assert_eq!(partial, "café", "partial should be the full typed value");
        }
        other => panic!("expected TargetValue, got {other:?}"),
    }
}

/// FM2 (Group B) — value-end columns must count UTF-16 units, not bytes.
///
/// A `_target_` value's end column is its start plus the value's length, and
/// that length must be measured in UTF-16 units. A byte-length measurement
/// overshoots for any non-ASCII value ("café" is 5 bytes but 4 UTF-16 units).
#[test]
fn fm2_target_value_end_counts_utf16_not_bytes() {
    let content = "# @hydra\nm:\n  _target_: café";
    let parsed = YamlParser::parse(content).expect("parse should succeed");
    let obj = &parsed.hydra_objects[0];

    assert_eq!(obj.target.value, "café");
    // The "  _target_: " prefix is pure ASCII, so value_start is identical in
    // every encoding — this is not the discriminating assertion.
    assert_eq!(obj.target.value_start, 12);
    // The discriminator: end = value_start (12) + UTF-16 len (4) = 16,
    // NOT value_start + byte len (5) = 17.
    assert_eq!(
        obj.target_value_end(),
        16,
        "value end must count UTF-16 units, not bytes"
    );
}

/// FM3 (Group A) — columns must be reported in UTF-16, not codepoints.
///
/// An astral character is a single codepoint but two UTF-16 units, so a
/// parameter value containing one must still report a UTF-16 end column. Here
/// `🎯z` spans 2 codepoints but 3 UTF-16 units, giving an end column of 10.
#[test]
fn fm3_param_value_end_counts_utf16_not_codepoints() {
    let content = "# @hydra\nm:\n  _target_: x.Y\n  tag: 🎯z";
    let parsed = YamlParser::parse(content).expect("parse should succeed");
    let obj = &parsed.hydra_objects[0];

    let tag = obj
        .parameters
        .iter()
        .find(|p| p.key() == Some("tag"))
        .expect("tag parameter should be parsed");

    match tag {
        Parameter::Keyword {
            value_start,
            value_end,
            ..
        } => {
            // ASCII prefix => encoding-agnostic.
            assert_eq!(*value_start, 7);
            // Discriminator: 🎯 counts as 2 UTF-16 units, so end = 10 not 9.
            assert_eq!(
                *value_end, 10,
                "value end must count 🎯 as 2 UTF-16 units, not 1 codepoint"
            );
        }
        _ => panic!("tag should be a keyword parameter"),
    }
}

/// FM4 (Group D) — locating inline `_args_` must keep document offsets
/// encoding-consistent.
///
/// Reporting the column of the `[` in an inline `_args_` sequence must stay
/// correct even when earlier lines hold multibyte or astral characters. Mixing
/// a codepoint-based document offset with byte indexing skews the bracket column
/// and can slice mid-character. Here an astral char (`🎯`) sits on an earlier
/// line, yet the bracket must still resolve to UTF-16 column 10.
#[test]
fn fm4_inline_args_with_earlier_astral_char() {
    let content = "# @hydra\nm:\n  _target_: 🎯\n  _args_: [a, b]";
    let parsed = YamlParser::parse(content).expect("parse should succeed");
    let obj = &parsed.hydra_objects[0];

    let args = obj.args.as_ref().expect("_args_ should be present");
    let inline = args
        .value
        .as_ref()
        .expect("_args_ should be an inline flow sequence");

    // `  _args_: ` is 10 cols; the bracket sits at UTF-16 column 10.
    assert_eq!(
        inline.bracket_col, 10,
        "bracket column must be a UTF-16 (line-local) offset"
    );
    assert_eq!(inline.text_after_bracket, "a, b]");
}
