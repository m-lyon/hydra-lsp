//! Scalars as OmegaConf loads them.
//!
//! saphyr follows YAML 1.2, but OmegaConf loads configs with PyYAML's `SafeLoader`, which
//! follows YAML 1.1, plus a resolver of its own that reads `1e5` as a float and with
//! timestamps dropped (`omegaconf/_yaml.py`). The regular expressions below are copied from
//! PyYAML 5.4 (`yaml/resolver.py`) and OmegaConf 2.4.

use std::sync::LazyLock;

use regex::Regex;
use saphyr::ScalarStyle;

/// A scalar as it is written in the source: its text, with any quotes and escapes already
/// resolved, and how it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scalar {
    pub text: String,
    pub style: ScalarStyle,
}

/// The Python value OmegaConf loads for a scalar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConfigScalar<'a> {
    Null,
    Bool(bool),
    /// An `int`, or `None` when it does not fit in an `i64`.
    Int(Option<i64>),
    Float,
    Str(&'a str),
}

impl Scalar {
    /// The value OmegaConf loads for this scalar. Only a plain scalar is resolved; a quoted
    /// or block scalar is always a string.
    pub fn config_value(&self) -> ConfigScalar<'_> {
        let text = self.text.as_str();
        if self.style != ScalarStyle::Plain {
            return ConfigScalar::Str(text);
        }
        match text {
            "" | "~" | "null" | "Null" | "NULL" => return ConfigScalar::Null,
            "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON" => {
                return ConfigScalar::Bool(true);
            }
            "no" | "No" | "NO" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF" => {
                return ConfigScalar::Bool(false);
            }
            _ => {}
        }
        if PYYAML_FLOAT.is_match(text) {
            ConfigScalar::Float
        } else if INT.is_match(text) {
            ConfigScalar::Int(int_value(text))
        } else if OMEGACONF_FLOAT.is_match(text) {
            ConfigScalar::Float
        } else {
            ConfigScalar::Str(text)
        }
    }
}

// PyYAML tries its resolvers in the order they were added (bool, float, int, null), and
// OmegaConf's float resolver after them.

static PYYAML_FLOAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)^(?:[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?
        |\.[0-9_]+(?:[eE][-+][0-9]+)?
        |[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*
        |[-+]?\.(?:inf|Inf|INF)
        |\.(?:nan|NaN|NAN))$",
    )
    .unwrap()
});

static INT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)^(?:[-+]?0b[0-1_]+
        |[-+]?0[0-7_]+
        |[-+]?(?:0|[1-9][0-9_]*)
        |[-+]?0x[0-9a-fA-F_]+
        |[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+)$",
    )
    .unwrap()
});

static OMEGACONF_FLOAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)^(?:[-+]?[0-9]+(?:_[0-9]+)*\.[0-9_]*(?:[eE][-+]?[0-9]+)?
        |[-+]?[0-9]+(?:_[0-9]+)*(?:[eE][-+]?[0-9]+)
        |\.[0-9]+(?:_[0-9]+)*(?:[eE][-+][0-9]+)?
        |[-+]?[0-9]+(?:_[0-9]+)*(?::[0-5]?[0-9])+\.[0-9_]*
        |[-+]?\.(?:inf|Inf|INF)
        |\.(?:nan|NaN|NAN))$",
    )
    .unwrap()
});

/// The value of a YAML 1.1 int, as PyYAML's `construct_yaml_int` computes it.
fn int_value(text: &str) -> Option<i64> {
    let text = text.replace('_', "");
    let (sign, digits) = match text.as_bytes().first()? {
        b'-' => (-1, &text[1..]),
        b'+' => (1, &text[1..]),
        _ => (1, text.as_str()),
    };
    let magnitude = if digits == "0" {
        0
    } else if let Some(binary) = digits.strip_prefix("0b") {
        i64::from_str_radix(binary, 2).ok()?
    } else if let Some(hex) = digits.strip_prefix("0x") {
        i64::from_str_radix(hex, 16).ok()?
    } else if digits.starts_with('0') {
        i64::from_str_radix(digits, 8).ok()?
    } else if digits.contains(':') {
        digits.split(':').try_fold(0i64, |value, part| {
            value.checked_mul(60)?.checked_add(part.parse().ok()?)
        })?
    } else {
        digits.parse().ok()?
    };
    magnitude.checked_mul(sign)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each row was checked against OmegaConf 2.4's loader on PyYAML 5.4.
    #[test]
    fn plain_scalars_follow_omegaconf() {
        use ConfigScalar::*;
        for (text, expected) in [
            ("", Null),
            ("~", Null),
            ("null", Null),
            ("Null", Null),
            ("yes", Bool(true)),
            ("No", Bool(false)),
            ("ON", Bool(true)),
            ("off", Bool(false)),
            ("true", Bool(true)),
            ("False", Bool(false)),
            ("y", Str("y")),
            ("n", Str("n")),
            ("017", Int(Some(15))),
            ("0017", Int(Some(15))),
            ("-017", Int(Some(-15))),
            ("0o17", Str("0o17")),
            ("08", Str("08")),
            ("1_000", Int(Some(1000))),
            ("1__0", Int(Some(10))),
            ("1_", Int(Some(1))),
            ("_1", Str("_1")),
            ("0b101", Int(Some(5))),
            ("0x1F", Int(Some(31))),
            ("-0x1f", Int(Some(-31))),
            ("1:30", Int(Some(90))),
            ("190:20:30", Int(Some(685230))),
            ("1:60", Str("1:60")),
            ("+1", Int(Some(1))),
            ("-0", Int(Some(0))),
            ("99999999999999999999", Int(None)),
            ("1e5", Float),
            ("1E-3", Float),
            ("12e+05", Float),
            ("1_000e3", Float),
            ("1e", Str("1e")),
            ("1.5", Float),
            ("1.", Float),
            ("0.", Float),
            ("09.5", Float),
            ("3.14_15", Float),
            ("1.5e3", Float),
            (".5", Float),
            (".5e+3", Float),
            (".5e3", Str(".5e3")),
            ("-.5", Str("-.5")),
            ("190:20:30.15", Float),
            (".inf", Float),
            ("-.Inf", Float),
            ("+.inf", Float),
            (".NaN", Float),
            ("NaN", Str("NaN")),
            ("2001-12-14", Str("2001-12-14")),
            ("0b", Str("0b")),
            ("abc", Str("abc")),
        ] {
            let scalar = Scalar {
                text: text.to_string(),
                style: ScalarStyle::Plain,
            };
            assert_eq!(scalar.config_value(), expected, "{text:?}");
        }
    }

    #[test]
    fn quoted_scalars_are_strings() {
        for style in [
            ScalarStyle::SingleQuoted,
            ScalarStyle::DoubleQuoted,
            ScalarStyle::Literal,
            ScalarStyle::Folded,
        ] {
            let scalar = Scalar {
                text: "yes".to_string(),
                style,
            };
            assert_eq!(scalar.config_value(), ConfigScalar::Str("yes"));
        }
    }
}
