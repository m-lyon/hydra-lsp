//! The diagnostic rules

use std::fmt;

/// Build the `DiagnosticRule` enum and everything that maps a variant to or
/// from its string code from one list of pairs, so the variants, the codes,
/// and the "all rules" list can never fall out of step with each other.
macro_rules! diagnostic_rules {
    ($($variant:ident => $code:literal),+ $(,)?) => {
        /// Diagnostic rule codes for Hydrust diagnostics.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum DiagnosticRule {
            $($variant,)+
        }

        impl DiagnosticRule {
            /// Return the string code for this rule.
            pub fn as_code(&self) -> &'static str {
                match self {
                    $(DiagnosticRule::$variant => $code,)+
                }
            }

            /// Parse a rule from its string code.
            pub fn from_code(code: &str) -> Option<Self> {
                match code {
                    $($code => Some(DiagnosticRule::$variant),)+
                    _ => None,
                }
            }

            /// Return all diagnostic rules.
            pub fn all() -> &'static [DiagnosticRule] {
                &[$(DiagnosticRule::$variant,)+]
            }

            /// Return the string code of every rule, in the same order as
            /// [`DiagnosticRule::all`].
            pub fn all_codes() -> &'static [&'static str] {
                &[$($code,)+]
            }
        }
    };
}

diagnostic_rules! {
    MissingArgument => "missing-argument",
    UnknownArgument => "unknown-argument",
    UnresolvedReference => "unresolved-reference",
    UnresolvedImport => "unresolved-import",
    InvalidHydraParameter => "invalid-hydra-parameter",
    ParameterAlreadyAssigned => "parameter-already-assigned",
    TooManyPositionalArguments => "too-many-positional-arguments",
    PositionalOnlyParameter => "positional-only-parameter",
}

impl fmt::Display for DiagnosticRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_code())
    }
}
