//! `sats providers`: list, add, use, protect, and remove against local
//! Subfrost- and Esplora-shaped endpoints. Hermetic: each endpoint is a
//! loopback listener that answers the signet genesis hash, Subfrost only
//! for the right key.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use common::{HttpServer, json_stdout, sats};
use predicates::prelude::*;
use tempfile::TempDir;

const KEY: &str = "SUBFROSTKEYSECRET";
const SIGNET_GENESIS: &str = "00000008819873e925422c1ff0f99f7cc9bbb232af63a077a480a3633bee1ef6";

/// A loopback Subfrost stand-in. Returns its URL and the API-key headers it
/// received, one per request.
fn fake_subfrost() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v4/jsonrpc", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut key, mut length) = (None, 0usize);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    match name.trim().to_ascii_lowercase().as_str() {
                        "x-subfrost-api-key" => key = Some(value.trim().to_string()),
                        "content-length" => length = value.trim().parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let response = if key.as_deref() == Some(KEY) {
                let body = format!(r#"{{"jsonrpc":"2.0","id":0,"result":"{SIGNET_GENESIS}"}}"#);
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string()
            };
            log.lock().unwrap().push(key);
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (url, seen)
}

/// A loopback Esplora that serves the signet genesis block.
fn fake_esplora() -> HttpServer {
    HttpServer::start(|request| {
        assert!(request.starts_with("GET /block-height/0 "), "{request}");
        Some((200, SIGNET_GENESIS.into()))
    })
}

fn config_text(dir: &TempDir) -> String {
    std::fs::read_to_string(dir.path().join("config.toml")).unwrap_or_default()
}

fn add_subfrost(dir: &TempDir, url: &str, extra: &[&str]) {
    sats(dir)
        .args(["providers", "add", "subfrost", "--url", url])
        .args(extra)
        .write_stdin(format!("{KEY}\n"))
        .assert()
        .success();
}

fn overview(dir: &TempDir) -> serde_json::Value {
    json_stdout(sats(dir).args(["--json", "providers"]).assert().success())
}

#[test]
fn list_shows_the_defaults_and_how_to_add_subfrost() {
    let dir = TempDir::new().unwrap();
    sats(&dir)
        .arg("providers")
        .assert()
        .success()
        .stdout(predicate::str::contains("signet providers"))
        .stdout(predicate::str::contains("mempool (default)"))
        .stdout(predicate::str::contains(
            "only the 546/330-sat postage check",
        ))
        .stdout(predicate::str::contains("sats providers add subfrost"));

    let out = json_stdout(
        sats(&dir)
            .args(["--json", "providers", "list"])
            .assert()
            .success(),
    );
    assert_eq!(out["network"], "signet");
    assert_eq!(out["chain"]["provider"], "mempool");
    assert_eq!(out["chain"]["source"], "default");
    assert_eq!(out["chain"]["auth"], "none");
    assert!(out["asset_protection"].is_null());
    assert!(out["alkanes_views"].is_null());

    // No mempool.space on regtest, and no default Subfrost endpoint.
    let regtest = json_stdout(
        sats(&dir)
            .args(["--network", "regtest", "--json", "providers"])
            .assert()
            .success(),
    );
    assert_eq!(regtest["chain"]["provider"], "esplora");
    sats(&dir)
        .args(["--network", "regtest", "providers"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "sats providers add subfrost --url URL",
        ));
}

#[test]
fn add_subfrost_checks_the_key_and_stores_it_privately() {
    let dir = TempDir::new().unwrap();
    let (url, seen) = fake_subfrost();

    // A rejected key writes nothing.
    sats(&dir)
        .args(["providers", "add", "subfrost", "--url", &url])
        .write_stdin("wrong-key\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("nothing was saved"))
        .stderr(predicate::str::contains("wrong-key").not());
    assert!(!dir.path().join("config.toml").exists());

    let assert = sats(&dir)
        .args(["providers", "add", "subfrost", "--url", &url])
        .write_stdin(format!("{KEY}\n"))
        .assert()
        .success()
        .stdout(predicate::str::contains("added subfrost  signet"))
        .stdout(predicate::str::contains("sats providers protect on"));
    let output = assert.get_output();
    for stream in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(stream).contains(KEY));
    }
    assert_eq!(
        seen.lock().unwrap().last().unwrap().as_deref(),
        Some(KEY),
        "the key travels only in Subfrost's header"
    );

    let config = config_text(&dir);
    assert!(config.contains(&format!("api_key = \"{KEY}\"")));
    assert!(config.contains("chain = \"subfrost\""));
    assert!(
        !config.contains("protect_assets"),
        "protection stays opt-in"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join("config.toml")), 0o600);
        assert_eq!(mode(dir.path()), 0o700);
    }

    let out = overview(&dir);
    assert_eq!(out["chain"]["provider"], "subfrost");
    assert_eq!(out["chain"]["auth"], "api_key");
    assert_eq!(out["alkanes_views"]["provider"], "subfrost");
    assert!(out["asset_protection"].is_null());
    let rendered = out.to_string();
    assert!(!rendered.contains(KEY));
    assert!(
        !rendered.contains("/v4/jsonrpc"),
        "only the origin is shown"
    );
}

#[test]
fn asset_protection_is_explicit_and_checked() {
    let dir = TempDir::new().unwrap();
    sats(&dir)
        .args(["providers", "protect", "on"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("needs Subfrost"));
    assert!(!dir.path().join("config.toml").exists());

    let (url, _) = fake_subfrost();
    add_subfrost(&dir, &url, &[]);
    sats(&dir)
        .args(["providers", "protect", "on"])
        .assert()
        .success()
        .stdout(predicate::str::contains("asset protection on  signet"))
        .stdout(predicate::str::contains("--no-guards"));
    assert_eq!(overview(&dir)["asset_protection"]["provider"], "subfrost");

    sats(&dir)
        .args(["providers", "protect", "off"])
        .assert()
        .success()
        .stdout(predicate::str::contains("asset protection off"));
    assert!(overview(&dir)["asset_protection"].is_null());

    // Re-adding keeps the saved key on empty input; --protect opts in.
    let out = json_stdout(
        sats(&dir)
            .args([
                "--json",
                "providers",
                "add",
                "subfrost",
                "--url",
                &url,
                "--protect",
            ])
            .write_stdin("\n")
            .assert()
            .success(),
    );
    assert_eq!(out["asset_protection"]["provider"], "subfrost");
    assert_eq!(out["chain"]["auth"], "api_key");
    assert_eq!(config_text(&dir).matches("api_key").count(), 1);
}

#[test]
fn use_switches_the_one_chain_source() {
    let dir = TempDir::new().unwrap();
    sats(&dir)
        .args(["providers", "use", "esplora"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no Esplora URL"));
    sats(&dir)
        .args(["providers", "use", "subfrost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Subfrost isn't set up"));
    assert!(!dir.path().join("config.toml").exists());

    let (subfrost, _) = fake_subfrost();
    let esplora = fake_esplora();
    add_subfrost(&dir, &subfrost, &[]);
    sats(&dir)
        .args(["providers", "add", "esplora", "--url", &esplora.url])
        .write_stdin("\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("added esplora  signet"));
    let out = overview(&dir);
    assert_eq!(out["chain"]["provider"], "esplora");
    assert_eq!(
        out["alkanes_views"]["provider"], "subfrost",
        "Subfrost still serves views while another source serves chain data"
    );

    sats(&dir)
        .args(["providers", "use", "subfrost"])
        .assert()
        .success()
        .stdout(predicate::str::contains("signet chain data: subfrost"));
    assert_eq!(overview(&dir)["chain"]["provider"], "subfrost");
    sats(&dir)
        .args(["providers", "use", "esplora"])
        .assert()
        .success();
    assert_eq!(overview(&dir)["chain"]["provider"], "esplora");
}

#[test]
fn an_esplora_token_never_follows_a_new_url() {
    let dir = TempDir::new().unwrap();
    let first = fake_esplora();
    let second = fake_esplora();
    sats(&dir)
        .args(["providers", "add", "esplora", "--url", &first.url])
        .write_stdin("TOKENSECRET\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("bearer token"))
        .stdout(predicate::str::contains("TOKENSECRET").not());
    assert_eq!(overview(&dir)["chain"]["auth"], "bearer");

    // Same server, empty input: the token stays.
    sats(&dir)
        .args(["providers", "add", "esplora", "--url", &first.url])
        .write_stdin("\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("updated esplora"));
    assert!(config_text(&dir).contains("TOKENSECRET"));

    // A different server, empty input: no token.
    sats(&dir)
        .args(["providers", "add", "esplora", "--url", &second.url])
        .write_stdin("\n")
        .assert()
        .success();
    assert!(!config_text(&dir).contains("TOKENSECRET"));
    assert_eq!(overview(&dir)["chain"]["auth"], "none");
}

#[test]
fn add_refuses_what_it_cannot_use() {
    let dir = TempDir::new().unwrap();
    sats(&dir)
        .args(["providers", "add", "subfrost"])
        .write_stdin("\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no Subfrost API key"));
    sats(&dir)
        .args(["--network", "regtest", "providers", "add", "subfrost"])
        .write_stdin("key\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no default endpoint for regtest"));
    sats(&dir)
        .args(["providers", "add", "esplora"])
        .write_stdin("\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("pass --url"));
    sats(&dir)
        .args([
            "providers",
            "add",
            "esplora",
            "--url",
            "http://127.0.0.1:1",
            "--protect",
        ])
        .write_stdin("\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("--protect is a Subfrost option"));
    assert!(!dir.path().join("config.toml").exists());
}

#[test]
fn removing_subfrost_never_silently_drops_protection() {
    let dir = TempDir::new().unwrap();
    let (url, _) = fake_subfrost();
    add_subfrost(&dir, &url, &["--protect"]);

    sats(&dir)
        .args(["providers", "remove", "subfrost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "asset protection uses Subfrost on signet",
        ))
        .stderr(predicate::str::contains("protect off"));
    assert!(config_text(&dir).contains(KEY));

    sats(&dir)
        .args(["providers", "protect", "off"])
        .assert()
        .success();
    sats(&dir)
        .args(["providers", "remove", "subfrost"])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed subfrost and its key"))
        .stdout(predicate::str::contains(
            "signet chain data is back to the default: mempool",
        ));
    let config = config_text(&dir);
    assert!(!config.contains(KEY));
    assert!(!config.contains("subfrost"));
    let out = overview(&dir);
    assert_eq!(out["chain"]["source"], "default");
    assert!(out["alkanes_views"].is_null());

    sats(&dir)
        .args(["providers", "remove", "subfrost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Subfrost isn't set up"));
}

#[test]
fn removing_esplora_reverts_its_network_to_the_default() {
    let dir = TempDir::new().unwrap();
    let esplora = fake_esplora();
    sats(&dir)
        .args(["providers", "add", "esplora", "--url", &esplora.url])
        .write_stdin("\n")
        .assert()
        .success();
    sats(&dir)
        .args(["providers", "remove", "esplora"])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed esplora"));
    assert_eq!(overview(&dir)["chain"]["provider"], "mempool");
    sats(&dir)
        .args(["providers", "remove", "esplora"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "no Esplora server is set up for signet",
        ));
}

#[test]
fn list_diagnoses_a_setup_that_cannot_be_served() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "network = \"signet\"\n\n[subfrost]\napi_key = \"APIKEYSECRET\"\n\n[signet]\nchain = \"esplora\"\nprotect_assets = true\nsubfrost_url = \"http://b.example/PATHSECRET\"\n",
    )
    .unwrap();
    sats(&dir)
        .arg("providers")
        .assert()
        .failure()
        .stdout(predicate::str::contains("as configured"))
        .stdout(predicate::str::contains("key saved"))
        .stdout(predicate::str::contains("http://b.example"))
        .stdout(predicate::str::contains("PATHSECRET").not())
        .stdout(predicate::str::contains("APIKEYSECRET").not())
        .stderr(predicate::str::contains(common::UNRESOLVABLE));
}

#[test]
fn the_previous_provider_format_is_refused() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "network = \"signet\"\n\n[providers.subfrost]\ndriver = \"subfrost\"\nnetwork = \"signet\"\nurl = \"https://signet.subfrost.io/v4/jsonrpc\"\n",
    )
    .unwrap();
    sats(&dir)
        .arg("providers")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unsupported"))
        .stderr(predicate::str::contains("sats providers add"));
}
