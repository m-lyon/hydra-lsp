//! Tests for the language server binary's command line handling.
//!
//! The VS Code client runs `--version` on a downloaded binary before launching
//! it, and then launches it with no arguments to speak LSP over stdio. Both
//! paths are checked here, including that the stdio path never writes anything
//! to stdout other than protocol traffic.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SERVER: &str = env!("CARGO_BIN_EXE_hydra-lsp");

/// Return the first whitespace-separated token that looks like `X.Y.Z`, which
/// is what the client scans stdout for.
fn first_semver_token(text: &str) -> Option<&str> {
    text.split_whitespace().find(|token| {
        let parts: Vec<&str> = token.split('.').collect();
        parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    })
}

#[test]
fn test_version_flag_prints_name_and_version() {
    for flag in ["--version", "-V"] {
        let output = Command::new(SERVER).arg(flag).output().unwrap();
        assert!(output.status.success(), "{flag} should exit 0");

        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            stdout.trim(),
            format!("hydra-lsp {}", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            first_semver_token(&stdout),
            Some(env!("CARGO_PKG_VERSION")),
            "the client must be able to pick the version out of stdout"
        );
    }
}

#[test]
fn test_help_flag_prints_usage() {
    for flag in ["--help", "-h"] {
        let output = Command::new(SERVER).arg(flag).output().unwrap();
        assert!(output.status.success(), "{flag} should exit 0");

        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("Usage: hydra-lsp"), "got: {stdout}");
    }
}

/// How long the server gets to exit after stdin closes.
///
/// Generous: this is a deadlock detector, not a performance budget. Closing
/// stdin mid-request used to wedge the server for good, so waiting forever
/// here would hang CI rather than fail it.
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Send one `initialize` request over stdin and return everything the server
/// wrote to stdout before stdin closed.
///
/// Dropping stdin is the whole point of the exercise: the server has to finish
/// the request it is holding, write the response, and exit. Anything it wanted
/// to send the client on the way out has nowhere to go, and must not stop it
/// from leaving.
fn run_lsp_exchange(args: &[&str]) -> String {
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{},"processId":null}}"#;
    let message = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);

    let mut child = Command::new(SERVER)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(message.as_bytes())
        .unwrap();

    let deadline = Instant::now() + EXIT_TIMEOUT;
    loop {
        match child.try_wait().unwrap() {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!(
                    "server did not exit within {EXIT_TIMEOUT:?} of stdin closing; \
                     it is most likely waiting on a client that stopped reading"
                );
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }

    let output = child.wait_with_output().unwrap();
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn test_no_arguments_speaks_lsp_on_stdout() {
    let stdout = run_lsp_exchange(&[]);

    // Nothing may precede the first LSP header: stdout is the transport.
    assert!(
        stdout.starts_with("Content-Length: "),
        "stdout must contain only LSP traffic, got: {stdout:?}"
    );
    assert!(stdout.contains(r#""id":1"#), "no response to initialize");
}

#[test]
fn test_unknown_arguments_still_start_the_server() {
    let stdout = run_lsp_exchange(&["--stdio"]);

    assert!(
        stdout.starts_with("Content-Length: "),
        "an unknown flag must not stop the server starting, got: {stdout:?}"
    );
}
