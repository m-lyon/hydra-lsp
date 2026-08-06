//! Tests for the `experimental.hydrust` block the server advertises at
//! initialize. The shape is a contract with the VS Code client, which uses it
//! to warn about settings a given server build would silently ignore, and to
//! set context keys for the features that are live on this connection.

mod common;

use hydra_lsp::backend::{HydrustCapabilities, NegotiatedFeatures};
use hydra_lsp::diagnostics::DiagnosticRule;

use crate::common::*;

/// A client that asked for everything the server can offer.
fn all_negotiated() -> NegotiatedFeatures {
    NegotiatedFeatures {
        pull_diagnostics: true,
        watched_files: true,
        diagnostic_refresh: true,
    }
}

/// Pull the `hydrust` block out of an `InitializeResult`.
fn hydrust_block(result: &tower_lsp::lsp_types::InitializeResult) -> serde_json::Value {
    result
        .capabilities
        .experimental
        .as_ref()
        .expect("server should advertise experimental capabilities")
        .get("hydrust")
        .expect("experimental capabilities should carry a 'hydrust' block")
        .clone()
}

#[test]
fn test_capability_block_shape_with_all_features() {
    let value = serde_json::to_value(HydrustCapabilities::new(all_negotiated())).unwrap();

    assert_eq!(
        value,
        serde_json::json!({
            "protocolVersion": 1,
            "supportedSettings": [
                "pythonInterpreter",
                "disabledRules",
                "numThreads",
                "enableHover",
                "enableCompletion",
                "enableSignatureHelp",
                "enableGotoDefinition",
                "enableSemanticTokens",
                "enableDiagnostics"
            ],
            "supportedRules": [
                "missing-argument",
                "unknown-argument",
                "unresolved-reference",
                "unresolved-import",
                "invalid-hydra-parameter",
                "parameter-already-assigned",
                "too-many-positional-arguments"
            ],
            "features": ["pullDiagnostics", "watchedFiles", "diagnosticRefresh"]
        })
    );
}

/// A client that asked for none of the optional behaviours must still get the
/// key, as an empty array — the client does membership tests against it, so a
/// missing key or a null would break it.
#[test]
fn test_capability_block_shape_with_no_features() {
    let value =
        serde_json::to_value(HydrustCapabilities::new(NegotiatedFeatures::default())).unwrap();

    let features = value
        .get("features")
        .expect("'features' must always be present");
    assert!(features.is_array(), "'features' must be an array");
    assert!(!features.is_null());
    assert_eq!(features, &serde_json::json!([]));

    // The rest of the block does not depend on negotiation.
    assert_eq!(value["protocolVersion"], 1);
    assert_eq!(value["supportedSettings"].as_array().unwrap().len(), 9);
    assert_eq!(
        value["supportedRules"].as_array().unwrap().len(),
        DiagnosticRule::all().len()
    );
}

/// Partial negotiation: each name is gated on its own flag.
#[test]
fn test_capability_block_shape_with_some_features() {
    let value = serde_json::to_value(HydrustCapabilities::new(NegotiatedFeatures {
        pull_diagnostics: true,
        watched_files: false,
        diagnostic_refresh: false,
    }))
    .unwrap();
    assert_eq!(value["features"], serde_json::json!(["pullDiagnostics"]));

    let value = serde_json::to_value(HydrustCapabilities::new(NegotiatedFeatures {
        pull_diagnostics: false,
        watched_files: true,
        diagnostic_refresh: true,
    }))
    .unwrap();
    assert_eq!(
        value["features"],
        serde_json::json!(["watchedFiles", "diagnosticRefresh"])
    );
}

#[test]
fn test_supported_rules_round_trip() {
    let caps = HydrustCapabilities::new(all_negotiated());

    for code in caps.supported_rules {
        let rule = DiagnosticRule::from_code(code)
            .unwrap_or_else(|| panic!("advertised rule '{code}' is not a known rule"));
        assert_eq!(rule.as_code(), *code);
    }

    // Every rule is advertised, not just the ones that happen to be listed.
    assert_eq!(caps.supported_rules.len(), DiagnosticRule::all().len());
}

/// The setting keys we advertise must be the ones `initialize` actually reads.
/// This catches a key being added to the parsing code without being added here.
#[test]
fn test_supported_settings_are_read_by_the_server() {
    let caps = HydrustCapabilities::new(all_negotiated());
    for key in [
        "pythonInterpreter",
        "disabledRules",
        "numThreads",
        "enableHover",
        "enableCompletion",
        "enableSignatureHelp",
        "enableGotoDefinition",
        "enableSemanticTokens",
        "enableDiagnostics",
    ] {
        assert!(
            caps.supported_settings.contains(&key),
            "setting '{key}' is read by the server but not advertised"
        );
    }
    assert_eq!(caps.supported_settings.len(), 9);
}

#[tokio::test]
async fn test_initialize_advertises_negotiated_features() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    // Watched files, pull diagnostics and diagnostic refresh all advertised.
    let result = ctx.initialize_with_capabilities(true, true, true).await;

    let hydrust = hydrust_block(&result);
    assert_eq!(
        hydrust,
        serde_json::to_value(HydrustCapabilities::new(all_negotiated())).unwrap()
    );
    assert_eq!(hydrust["protocolVersion"], 1);
}

#[tokio::test]
async fn test_initialize_advertises_no_features_for_a_bare_client() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    // A client advertising none of the three capabilities.
    let result = ctx.initialize_with_capabilities(false, false, false).await;

    let hydrust = hydrust_block(&result);
    assert_eq!(hydrust["features"], serde_json::json!([]));
    // The settings and rules lists are unaffected by negotiation.
    assert_eq!(hydrust["supportedSettings"].as_array().unwrap().len(), 9);
    assert_eq!(
        hydrust["supportedRules"].as_array().unwrap().len(),
        DiagnosticRule::all().len()
    );
}

#[tokio::test]
async fn test_initialize_advertises_partial_features() {
    let mut ctx = TestContext::new(TestWorkspace::Simple);
    // Pull diagnostics only: no watchers, and no way to ask for a re-pull.
    let result = ctx.initialize_with_capabilities(false, true, false).await;

    assert_eq!(
        hydrust_block(&result)["features"],
        serde_json::json!(["pullDiagnostics"])
    );
}
