//! `sats providers`: listing, login against a local Subfrost-shaped
//! endpoint, and logout. Hermetic: the endpoint is a loopback listener that
//! answers JSON-RPC with the signet genesis hash only for the right key.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use common::{json_stdout, sats};
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

fn config_text(dir: &TempDir) -> String {
    std::fs::read_to_string(dir.path().join("config.toml")).unwrap_or_default()
}

#[test]
fn list_shows_the_default_and_how_to_add_subfrost() {
    let dir = TempDir::new().unwrap();
    sats(&dir)
        .arg("providers")
        .assert()
        .success()
        .stdout(predicate::str::contains("signet providers"))
        .stdout(predicate::str::contains("esplora(default)"))
        .stdout(predicate::str::contains("asset guards: none"))
        .stdout(predicate::str::contains("sats providers login subfrost"));

    let out = json_stdout(
        sats(&dir)
            .args(["--json", "providers", "list"])
            .assert()
            .success(),
    );
    assert_eq!(out["network"], "signet");
    let rows = out["providers"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["source"], "default");
    assert_eq!(rows[0]["auth"], "none");
    assert_eq!(
        rows[0]["serves"],
        serde_json::json!(["chain.sync", "chain.fees", "chain.broadcast"])
    );

    // No default Subfrost endpoint on regtest: the hint asks for a URL.
    sats(&dir)
        .args(["--network", "regtest", "providers"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "sats providers login subfrost --url URL",
        ));
}

#[test]
fn subfrost_login_checks_the_key_and_stores_it_privately() {
    let dir = TempDir::new().unwrap();
    let (url, seen) = fake_subfrost();

    // A rejected key writes nothing.
    sats(&dir)
        .args(["providers", "login", "subfrost", "--url", &url])
        .write_stdin("wrong-key\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("subfrost was not saved"))
        .stderr(predicate::str::contains("wrong-key").not());
    assert!(!dir.path().join("config.toml").exists());

    let assert = sats(&dir)
        .args(["providers", "login", "subfrost", "--url", &url])
        .write_stdin(format!("{KEY}\n"))
        .assert()
        .success()
        .stdout(predicate::str::contains("logged in to subfrost  signet"))
        .stdout(predicate::str::contains("asset guards are off"));
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
    assert!(config.contains("[providers.subfrost]"));
    assert!(config.contains(&format!("api_key = \"{KEY}\"")));
    assert!(!config.contains("capabilities"), "guards stay opt-in");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join("config.toml")), 0o600);
        assert_eq!(mode(dir.path()), 0o700);
    }

    let out = json_stdout(sats(&dir).args(["--json", "providers"]).assert().success());
    let rows = out["providers"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "Subfrost replaces the default for chain data"
    );
    assert_eq!(rows[0]["name"], "subfrost");
    assert_eq!(rows[0]["auth"], "api_key");
    assert!(!out.to_string().contains(KEY));
    assert!(
        !out.to_string().contains("/v4/jsonrpc"),
        "only the origin is shown"
    );
}

#[test]
fn relogin_enables_assets_and_keeps_the_stored_key() {
    let dir = TempDir::new().unwrap();
    let (url, seen) = fake_subfrost();
    sats(&dir)
        .args(["providers", "login", "subfrost", "--url", &url])
        .write_stdin(format!("{KEY}\n"))
        .assert()
        .success();

    // Empty input keeps the current key; --assets is the explicit opt-in.
    let out = json_stdout(
        sats(&dir)
            .args([
                "--json",
                "providers",
                "login",
                "subfrost",
                "--url",
                &url,
                "--assets",
            ])
            .write_stdin("\n")
            .assert()
            .success(),
    );
    assert_eq!(out["updated"], true);
    assert_eq!(out["authenticated"], true);
    assert_eq!(
        out["serves"],
        serde_json::json!([
            "chain.sync",
            "chain.fees",
            "chain.broadcast",
            "guard.ord",
            "guard.alkanes",
            "alkanes.view"
        ])
    );
    assert_eq!(seen.lock().unwrap().last().unwrap().as_deref(), Some(KEY));
    assert_eq!(config_text(&dir).matches("[providers.").count(), 1);

    // A later re-login without --assets keeps the guards it has.
    sats(&dir)
        .args(["providers", "login", "subfrost", "--url", &url])
        .write_stdin("\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("updated subfrost"))
        .stdout(predicate::str::contains("guard.ord"));
}

#[test]
fn login_refuses_what_it_cannot_use() {
    let dir = TempDir::new().unwrap();

    sats(&dir)
        .args(["providers", "login", "subfrost"])
        .write_stdin("\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no Subfrost API key"));
    sats(&dir)
        .args(["--network", "regtest", "providers", "login", "subfrost"])
        .write_stdin("key\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no default endpoint for regtest"));
    sats(&dir)
        .args(["providers", "login", "esplora"])
        .write_stdin("\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("pass --url"));
    sats(&dir)
        .args([
            "providers",
            "login",
            "esplora",
            "--url",
            "http://127.0.0.1:1",
            "--assets",
        ])
        .write_stdin("\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("--assets is a Subfrost option"));

    // A second chain provider would make every send ambiguous: refused
    // before the endpoint is contacted, and the config is left as it was.
    let existing = "network = \"signet\"\n\n[providers.chain]\ndriver = \"esplora\"\nnetwork = \"signet\"\nurl = \"http://127.0.0.1:1\"\n";
    std::fs::write(dir.path().join("config.toml"), existing).unwrap();
    sats(&dir)
        .args([
            "providers",
            "login",
            "subfrost",
            "--url",
            "http://127.0.0.1:1",
        ])
        .write_stdin("key\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("subfrost was not saved"))
        .stderr(predicate::str::contains("multiple chain.sync providers"));
    assert_eq!(config_text(&dir), existing);

    // An explicit name never repurposes another driver's entry.
    sats(&dir)
        .args([
            "providers",
            "login",
            "subfrost",
            "--name",
            "chain",
            "--url",
            "http://127.0.0.1:1",
        ])
        .write_stdin("key\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "already configured as esplora for signet",
        ));
}

#[test]
fn list_diagnoses_an_ambiguous_configuration() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "network = \"signet\"\n\n[providers.a]\ndriver = \"esplora\"\nnetwork = \"signet\"\nurl = \"http://a.example/PATHSECRET\"\n\n[providers.b]\ndriver = \"subfrost\"\nnetwork = \"signet\"\nurl = \"http://b.example\"\napi_key = \"APIKEYSECRET\"\n",
    )
    .unwrap();
    sats(&dir)
        .arg("providers")
        .assert()
        .failure()
        .stdout(predicate::str::contains("not resolvable"))
        .stdout(predicate::str::contains("http://a.example"))
        .stdout(predicate::str::contains("api key"))
        .stdout(predicate::str::contains("PATHSECRET").not())
        .stdout(predicate::str::contains("APIKEYSECRET").not())
        .stderr(predicate::str::contains("multiple chain.sync providers"));
}

#[test]
fn logout_removes_the_entry_and_its_key() {
    let dir = TempDir::new().unwrap();
    let (url, _) = fake_subfrost();
    sats(&dir)
        .args(["providers", "login", "subfrost", "--url", &url])
        .write_stdin(format!("{KEY}\n"))
        .assert()
        .success();

    sats(&dir)
        .args(["providers", "logout", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("configured: subfrost"));
    sats(&dir)
        .args(["providers", "logout", "subfrost"])
        .assert()
        .success()
        .stdout(predicate::str::contains("logged out of subfrost"));
    assert!(!config_text(&dir).contains(KEY));
    sats(&dir)
        .arg("providers")
        .assert()
        .success()
        .stdout(predicate::str::contains("esplora(default)"));
}
