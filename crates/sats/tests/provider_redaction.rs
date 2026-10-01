//! Observable command errors must share the providers' safe boundary.
mod common;

use common::{ADDRESS, HttpServer, init_wallet, sats};
use tempfile::TempDir;

#[test]
fn malformed_provider_arguments_never_echo_credentials() {
    let dir = TempDir::new().unwrap();
    for value in [
        "unknown=https://USERSECRET:PASSSECRET@host/PATHSECRET?other=QUERYSECRET",
        "https://USERSECRET:PASSSECRET@host/PATHSECRET?other=QUERYSECRET",
    ] {
        for inline in [false, true] {
            let mut command = sats(&dir);
            if inline {
                command.arg(format!("--provider={value}"));
            } else {
                command.args(["--provider", value]);
            }
            let result = command.arg("balance").assert().code(2);
            let rendered = String::from_utf8_lossy(&result.get_output().stderr);
            assert!(rendered.contains("--provider"));
            assert!(rendered.contains("redacted provider endpoint"));
            for secret in ["USERSECRET", "PASSSECRET", "PATHSECRET", "QUERYSECRET"] {
                assert!(!rendered.contains(secret), "{rendered}");
            }
        }
    }
}

#[test]
fn cli_preparation_errors_omit_endpoint_and_echoed_credentials() {
    for driver in ["esplora", "subfrost"] {
        let dir = TempDir::new().unwrap();
        init_wallet(&dir);
        let server = HttpServer::start(move |_| {
            let echo = "PATHSECRET QUERYSECRET BEARERSECRET USERSECRET";
            Some(if driver == "esplora" {
                (401, echo.into())
            } else {
                (
                    200,
                    serde_json::json!({"error": {"code": -32603, "message": echo}}).to_string(),
                )
            })
        });
        let auth = if driver == "esplora" {
            "auth = { bearer = 'BEARERSECRET' }"
        } else {
            "api_key = 'BEARERSECRET'"
        };
        std::fs::write(dir.path().join("config.toml"), format!(
            "network = 'signet'\n[providers.fixture]\ndriver = '{driver}'\nnetwork = 'signet'\nurl = '{}/arbitrary/PATHSECRET?unfamiliar=QUERYSECRET'\n{auth}\n",
            server.url
        )).unwrap();
        for json in [false, true] {
            let mut command = sats(&dir);
            if json {
                command.arg("--json");
            }
            let result = command.args(["send", ADDRESS, "1000"]).assert().failure();
            let output = result.get_output();
            let rendered = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(rendered.contains("chain sync failed"));
            assert!(rendered.contains(&server.url));
            for secret in ["PATHSECRET", "QUERYSECRET", "BEARERSECRET", "USERSECRET"] {
                assert!(!rendered.contains(secret), "{rendered}");
            }
        }
    }
}

#[test]
fn malformed_credential_configuration_does_not_echo_the_source() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("config.toml"),
        "network = 'signet'\n[providers.fixture]\ndriver = 'esplora'\nnetwork = 'signet'\nurl = 'https://USERSECRET:PASSSECRET@host/PATHSECRET?unknown=QUERYSECRET\n"
    ).unwrap();
    let result = sats(&dir).arg("balance").assert().failure();
    let rendered = String::from_utf8_lossy(&result.get_output().stderr);
    assert!(rendered.contains("invalid config"));
    for secret in ["USERSECRET", "PASSSECRET", "PATHSECRET", "QUERYSECRET"] {
        assert!(!rendered.contains(secret));
    }
}
