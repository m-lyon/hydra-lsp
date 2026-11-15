//! Settings, what the client can do, and the capabilities the server sends back
//! from `initialize`, including the `experimental.hydrust` block.

use std::collections::HashSet;
use tower_lsp::lsp_types::*;

use super::HydraLspBackend;
use super::pools::clamp_num_threads;
use crate::rules::DiagnosticRule;
use crate::yaml_parser::SEMANTIC_TOKEN_LEGEND;

/// Declare the per-capability feature toggles once, as
/// `field => "settingKey", "logName"` triples.
///
/// One invocation generates everything that has to agree about a toggle: the
/// [`FeatureToggles`] field, its default, the settings key it is parsed from,
/// the name used when logging it as disabled, and the key list that goes out in
/// [`HydrustCapabilities::supported_settings`]. Adding a toggle is a one-line
/// edit here, so none of those can drift apart.
macro_rules! feature_toggles {
    ($($field:ident => $key:literal, $name:literal),+ $(,)?) => {
        /// Feature toggle settings for individual LSP capabilities.
        #[derive(Debug, Clone, Copy)]
        pub struct FeatureToggles {
            $(pub $field: bool,)+
        }

        impl Default for FeatureToggles {
            fn default() -> Self {
                Self { $($field: true,)+ }
            }
        }

        impl FeatureToggles {
            /// The `initializationOptions.settings` keys these toggles read.
            /// Part of [`SUPPORTED_SETTINGS`].
            pub const SETTING_KEYS: &'static [&'static str] = &[$($key,)+];

            /// Parse feature toggles from a JSON settings object. A key that is
            /// missing or not a boolean leaves the feature on.
            fn from_json(settings: &serde_json::Value) -> Self {
                Self {
                    $($field: settings.get($key).and_then(|v| v.as_bool()).unwrap_or(true),)+
                }
            }

            /// Return names of disabled features, if any.
            fn disabled_names(&self) -> Vec<&'static str> {
                let mut names = Vec::new();
                $(if !self.$field {
                    names.push($name);
                })+
                names
            }
        }
    };
}

feature_toggles! {
    hover => "enableHover", "hover",
    completion => "enableCompletion", "completion",
    signature_help => "enableSignatureHelp", "signatureHelp",
    goto_definition => "enableGotoDefinition", "gotoDefinition",
    semantic_tokens => "enableSemanticTokens", "semanticTokens",
    diagnostics => "enableDiagnostics", "diagnostics",
}

/// Server-wide settings for Hydrust Server.
#[derive(Debug, Default)]
pub struct Settings {
    pub python_interpreter: Option<String>,
    pub disabled_rules: HashSet<DiagnosticRule>,
    pub features: FeatureToggles,
}

impl Settings {
    /// Parse the `initializationOptions.settings` object. Unknown keys, and
    /// values of the wrong type, are ignored. `numThreads` is not kept here: it
    /// only sizes the pools, once, so `HydraLspBackend::apply_settings` reads
    /// it itself.
    fn from_json(settings: &serde_json::Value) -> Self {
        let python_interpreter = settings
            .get("pythonInterpreter")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let mut disabled_rules = HashSet::new();
        if let Some(rules) = settings.get("disabledRules")
            && let Some(rules_array) = rules.as_array()
        {
            for rule_value in rules_array {
                if let Some(rule_str) = rule_value.as_str()
                    && let Some(rule) = DiagnosticRule::from_code(rule_str)
                {
                    disabled_rules.insert(rule);
                }
            }
        }

        Self {
            python_interpreter,
            disabled_rules,
            features: FeatureToggles::from_json(settings),
        }
    }
}

/// Version of the `experimental.hydrust` block described by
/// [`HydrustCapabilities`]. A client compares it against the highest version it
/// knows how to read.
///
/// Bump this only when something already in the block changes meaning in a way
/// that would mislead an older client: a setting key that starts doing
/// something different, a renamed or repurposed feature name, a field that
/// changes type. Purely additive changes do NOT need a bump, because clients are
/// expected to ignore names they do not recognise.
const HYDRUST_PROTOCOL_VERSION: u32 = 1;

/// The non-toggle keys the server reads out of
/// `initializationOptions.settings`. This list must stay in step with the
/// parsing in [`Settings::from_json`] and `apply_settings` (which reads
/// `numThreads`); the feature toggles alongside it come from
/// [`FeatureToggles::SETTING_KEYS`], which is generated with the toggles
/// themselves.
const CORE_SETTINGS: &[&str] = &["pythonInterpreter", "disabledRules", "numThreads"];

/// Every key the server actually reads out of `initializationOptions.settings`.
/// Anything else a client sends is silently ignored.
static SUPPORTED_SETTINGS: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| {
        CORE_SETTINGS
            .iter()
            .chain(FeatureToggles::SETTING_KEYS)
            .copied()
            .collect()
    });

/// Which optional behaviours the server will actually use this session. Each
/// one needs the client to have asked for it in its `initialize` capabilities,
/// so these are read by `ClientFeatures::from_capabilities`. The handlers
/// gate on the same value the `experimental.hydrust` block is built from.
#[derive(Debug, Clone, Copy, Default)]
pub struct NegotiatedFeatures {
    /// The client issues `textDocument/diagnostic`, so we answer pull requests
    /// rather than pushing diagnostics at it.
    pub pull_diagnostics: bool,
    /// The client allows watchers to be registered at runtime, so we set up our
    /// own `workspace/didChangeWatchedFiles` watchers.
    pub watched_files: bool,
    /// The client accepts `workspace/diagnostic/refresh`, so we can ask it to
    /// re-pull after a watched Python file changes.
    pub diagnostic_refresh: bool,
}

/// What the client said it can do, read once from its `initialize`
/// capabilities and kept for the whole session.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ClientFeatures {
    /// The optional behaviours advertised in `experimental.hydrust`, and the
    /// flags the handlers gate those behaviours on.
    pub(crate) negotiated: NegotiatedFeatures,
    /// The client accepts relative patterns for watched files. Required to watch
    /// out-of-workspace Python search paths. Not advertised: it only refines
    /// which watchers `watched_files` registers.
    pub(crate) relative_patterns: bool,
}

impl ClientFeatures {
    /// Read the client capabilities the server branches on.
    pub(crate) fn from_capabilities(capabilities: &ClientCapabilities) -> Self {
        let watched_files_caps = capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.did_change_watched_files.as_ref());
        Self {
            negotiated: NegotiatedFeatures {
                // Pull diagnostics: does the client issue `textDocument/diagnostic`?
                pull_diagnostics: capabilities
                    .text_document
                    .as_ref()
                    .and_then(|t| t.diagnostic.as_ref())
                    .is_some(),
                watched_files: watched_files_caps
                    .and_then(|d| d.dynamic_registration)
                    .unwrap_or(false),
                // Refresh: can we nudge the client to re-pull with
                // `workspace/diagnostic/refresh`?
                diagnostic_refresh: capabilities
                    .workspace
                    .as_ref()
                    .and_then(|w| w.diagnostic.as_ref())
                    .and_then(|d| d.refresh_support)
                    .unwrap_or(false),
            },
            relative_patterns: watched_files_caps
                .and_then(|d| d.relative_pattern_support)
                .unwrap_or(false),
        }
    }
}

/// A feature name paired with the flag that decides whether it is on.
type FeatureGate = (&'static str, fn(&NegotiatedFeatures) -> bool);

/// The coarse feature names, each paired with the flag that decides whether it
/// is active. Keeping the name and its gate together means a name can only be
/// advertised when the matching behaviour really is switched on.
///
/// Every name here is only sent to a client that advertised the capability it
/// depends on:
///
/// - `pullDiagnostics` — we answer `textDocument/diagnostic`, returning
///   unchanged reports via result IDs. Absent when the client did not advertise
///   pull support, in which case it gets diagnostics pushed to it instead.
/// - `watchedFiles` — we register our own file watchers on `initialized`, so
///   the client does not need to configure any. Absent when the client did not
///   advertise dynamic registration for `workspace/didChangeWatchedFiles`.
/// - `diagnosticRefresh` — we send `workspace/diagnostic/refresh` after a
///   watched Python file changes. Absent when the client did not advertise
///   refresh support, in which case it would need to re-pull on its own.
const SUPPORTED_FEATURES: &[FeatureGate] = &[
    ("pullDiagnostics", |f| f.pull_diagnostics),
    ("watchedFiles", |f| f.watched_files),
    ("diagnosticRefresh", |f| f.diagnostic_refresh),
];

/// What this build of the server understands, sent back from `initialize` as
/// `capabilities.experimental.hydrust`.
///
/// This lets a client that may be driving any released server version discover
/// the surface directly.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HydrustCapabilities {
    /// See `HYDRUST_PROTOCOL_VERSION`.
    pub protocol_version: u32,
    /// Setting keys under `initializationOptions.settings` that do something.
    pub supported_settings: &'static [&'static str],
    /// Rule codes accepted in the `disabledRules` setting, and emitted as
    /// diagnostic codes.
    pub supported_rules: &'static [&'static str],
    /// The features that are switched on for this session, after matching what
    /// the server can do against what the client asked for. A name being absent
    /// means the behaviour will not happen on this connection, not that the
    /// server is too old to do it. Empty when the client asked for none of
    /// them. See `SUPPORTED_FEATURES`.
    pub features: Vec<&'static str>,
}

impl HydrustCapabilities {
    /// Build the block for a session, keeping only the features the client and
    /// server agreed on.
    pub fn new(negotiated: NegotiatedFeatures) -> Self {
        Self {
            protocol_version: HYDRUST_PROTOCOL_VERSION,
            supported_settings: SUPPORTED_SETTINGS.as_slice(),
            supported_rules: DiagnosticRule::all_codes(),
            features: SUPPORTED_FEATURES
                .iter()
                .filter(|(_, is_active)| is_active(&negotiated))
                .map(|(name, _)| *name)
                .collect(),
        }
    }

    /// Build the value for `ServerCapabilities::experimental`, i.e. this block
    /// wrapped in its `hydrust` key. Returns `None` only if serialization
    /// fails, which it cannot for these plain fields.
    pub fn to_experimental(&self) -> Option<serde_json::Value> {
        #[derive(serde::Serialize)]
        struct Experimental<'a> {
            hydrust: &'a HydrustCapabilities,
        }
        serde_json::to_value(Experimental { hydrust: self }).ok()
    }
}

/// The `ServerCapabilities` sent back from `initialize`. Everything is fixed
/// except the `experimental.hydrust` block, which lists the features negotiated
/// with this client.
pub(super) fn server_capabilities(client: &ClientFeatures) -> ServerCapabilities {
    ServerCapabilities {
        // `position_encoding` is intentionally left unset (it falls under
        // `..Default::default()` below). Per the LSP spec, an unset
        // encoding means the default: UTF-16. Every position we emit and
        // consume is therefore counted in UTF-16 code units (see
        // `cp_to_utf16_col` / `utf16_col_to_byte_offset` in yaml_parser
        // and the `PositionEncoding::Utf16` conversion in python::extract).
        // The only realistic client is the UTF-16-native VSCode extension,
        // so explicit negotiation buys nothing; revisit only if a client
        // that prefers UTF-8 is added (which would also require making the
        // internals byte-native to be worthwhile).
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".to_string(), "_".to_string()]),
            resolve_provider: Some(false),
            ..Default::default()
        }),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec![
                ":".to_string(),
                "-".to_string(),
                "[".to_string(),
                ",".to_string(),
            ]),
            retrigger_characters: None,
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        definition_provider: Some(OneOf::Left(true)),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: SemanticTokensLegend {
                    token_types: SEMANTIC_TOKEN_LEGEND.to_vec(),
                    token_modifiers: vec![
                        SemanticTokenModifier::DECLARATION,
                        SemanticTokenModifier::DEFINITION,
                    ],
                },
                range: Some(false),
                full: Some(SemanticTokensFullOptions::Bool(true)),
                ..Default::default()
            },
        )),
        diagnostic_provider: Some(DiagnosticServerCapabilities::Options(DiagnosticOptions {
            identifier: Some("hydra-lsp".to_string()),
            // Hydra targets resolve into watched Python files, so a
            // change in one file can affect another's diagnostics.
            inter_file_dependencies: true,
            // Document/open-files pull only; no whole-workspace pull.
            workspace_diagnostics: false,
            work_done_progress_options: Default::default(),
        })),
        // Tell the client which settings and rules this build
        // understands, so it can warn about anything it sends that
        // would be silently ignored, plus the features that are
        // actually switched on for this session.
        experimental: HydrustCapabilities::new(client.negotiated).to_experimental(),
        ..Default::default()
    }
}

impl HydraLspBackend {
    /// Parse `initializationOptions.settings` into [`Self::settings`], log what
    /// was set, and return the `numThreads` override (`None` for the server
    /// default), already clamped to a range the server can honour.
    ///
    /// Without a `settings` object everything keeps its default.
    pub(super) fn apply_settings(&self, init_options: Option<&serde_json::Value>) -> Option<usize> {
        let settings = init_options?.get("settings")?;
        let parsed = Settings::from_json(settings);

        // Parse optional thread-count override (every thread the server
        // runs, the protocol loop's included), clamped to a range the server
        // can honour.
        let (num_threads, num_threads_warning) = settings
            .get("numThreads")
            .and_then(|v| v.as_u64())
            .map_or((None, None), clamp_num_threads);

        if let Some(ref path) = parsed.python_interpreter {
            self.outbox.log(
                MessageType::INFO,
                format!("Python interpreter configured: {}", path),
            );
        }

        if !parsed.disabled_rules.is_empty() {
            self.outbox.log(
                MessageType::INFO,
                format!("Disabled rules: {:?}", parsed.disabled_rules),
            );
        }

        let disabled_features = parsed.features.disabled_names();
        if !disabled_features.is_empty() {
            self.outbox.log(
                MessageType::INFO,
                format!("Disabled features: {:?}", disabled_features),
            );
        }

        {
            let mut s = self.settings.write();
            if parsed.python_interpreter.is_some() {
                s.python_interpreter = parsed.python_interpreter;
            }
            s.disabled_rules = parsed.disabled_rules;
            s.features = parsed.features;
        }

        if let Some(warning) = num_threads_warning {
            self.outbox.log(MessageType::WARNING, warning);
        }

        num_threads
    }
}
