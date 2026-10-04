//! Shared helpers for the CLI integration tests. Fully offline and
//! deterministic: every test gets its own SATS_DIR, uses SATS_PASSWORD
//! instead of a prompt, and talks to the file-driven mock provider.
#![allow(dead_code)]

use assert_cmd::Command;
use bdk_wallet::bitcoin::hashes::Hash;
use bdk_wallet::bitcoin::{Amount, BlockHash, Network, OutPoint};
use bdk_wallet::chain::{BlockId, ConfirmationBlockTime};
use bdk_wallet::test_utils::{insert_checkpoint, receive_output};
use predicates::prelude::*;
use tempfile::TempDir;

pub const PASSWORD: &str = "integration-test-pw";

/// A valid signet taproot address for send targets.
pub const ADDRESS: &str = "tb1pvlnw9n2zuefmxzwmuz0763uajw8nmaattkhd8002g3ekejjspxtshu2q9n";

pub fn sats(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("sats").unwrap();
    cmd.env("SATS_DIR", dir.path())
        .env("SATS_PASSWORD", PASSWORD)
        .env("NO_COLOR", "1");
    cmd
}

pub fn init_wallet(dir: &TempDir) {
    sats(dir)
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("wallet created"));
}

/// A deterministic taproot address that belongs to no test wallet, for
/// send targets beyond [`ADDRESS`]. Built from a fixed key, so it is
/// valid on any requested network.
pub fn foreign_address(network: Network) -> String {
    use bdk_wallet::bitcoin::secp256k1::{Secp256k1, SecretKey};
    let secp = Secp256k1::new();
    let key = SecretKey::from_slice(&[7u8; 32]).unwrap();
    let (internal, _) = key.public_key(&secp).x_only_public_key();
    bdk_wallet::bitcoin::Address::p2tr(&secp, internal, None, network).to_string()
}

/// Point the config at the hermetic mock chain provider (no network).
/// Returns the mock data directory controlling its behavior.
pub fn write_mock_provider(dir: &TempDir) -> std::path::PathBuf {
    let mockdata = dir.path().join("mockdata");
    std::fs::create_dir_all(&mockdata).unwrap();
    // The mock stands in for both the chain source and Subfrost (Alkanes
    // views).
    let config = format!(
        "network = \"signet\"\n\n[signet]\nchain = \"esplora\"\nsubfrost_url = \"file://{0}\"\n\n[signet.esplora]\nurl = \"file://{0}\"\n",
        mockdata.display()
    );
    std::fs::write(dir.path().join("config.toml"), config).unwrap();
    mockdata
}

/// Seed the CLI's persisted signet wallet with confirmed UTXOs. The mock
/// provider's sync is a no-op, so these funds survive CLI syncs.
pub fn fund_wallet(dir: &TempDir, values_sat: &[u64]) -> Vec<OutPoint> {
    let db = dir.path().join("signet/wallet.sqlite");
    let mut conn = rusqlite::Connection::open(&db).unwrap();
    let mut wallet = bdk_wallet::Wallet::load()
        .check_network(Network::Signet)
        .load_wallet(&mut conn)
        .unwrap()
        .expect("run init_wallet first");
    let block_900 = BlockId {
        height: 900,
        hash: BlockHash::all_zeros(),
    };
    insert_checkpoint(&mut wallet, block_900);
    insert_checkpoint(
        &mut wallet,
        BlockId {
            height: 1_000,
            hash: BlockHash::all_zeros(),
        },
    );
    let outpoints = values_sat
        .iter()
        .map(|v| {
            receive_output(
                &mut wallet,
                Amount::from_sat(*v),
                ConfirmationBlockTime {
                    block_id: block_900,
                    confirmation_time: 100,
                },
            )
        })
        .collect();
    wallet.persist(&mut conn).unwrap();
    outpoints
}

pub fn json_stdout(assert: assert_cmd::assert::Assert) -> serde_json::Value {
    serde_json::from_slice(&assert.get_output().stdout).expect("json output")
}

/// Local HTTP fixture. A None response holds the connection open until drop,
/// allowing tests to exercise the production timeout without sleeping servers.
pub struct HttpServer {
    pub url: String,
    pub requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HttpServer {
    pub fn start(handler: impl Fn(&str) -> Option<(u16, String)> + Send + 'static) -> Self {
        Self::start_with_body(move |request, _body| handler(request))
    }

    /// [`Self::start`] for a handler that also reads the request body.
    pub fn start_with_body(
        handler: impl Fn(&str, &str) -> Option<(u16, String)> + Send + 'static,
    ) -> Self {
        use std::io::{BufRead, Read, Write};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&requests);
        let (stop, stopped) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut held = Vec::new();
            loop {
                if stopped.try_recv() != Err(std::sync::mpsc::TryRecvError::Empty) {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // BSD/macOS accepted sockets inherit the listener's
                        // non-blocking flag; reads below must block.
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                            .unwrap();
                        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        let request = line.clone();
                        let mut length = 0;
                        loop {
                            line.clear();
                            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                                break;
                            }
                            if let Some(value) =
                                line.to_ascii_lowercase().strip_prefix("content-length:")
                            {
                                length = value.trim().parse::<usize>().unwrap();
                            }
                        }
                        let mut body = vec![0; length];
                        reader.read_exact(&mut body).unwrap();
                        count.fetch_add(1, Ordering::SeqCst);
                        match handler(request.trim(), &String::from_utf8_lossy(&body)) {
                            Some((status, body)) => {
                                let response = format!(
                                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                );
                                let _ = stream.write_all(response.as_bytes());
                            }
                            None => held.push(stream),
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(e) => panic!("HTTP fixture accept: {e}"),
                }
            }
        });
        Self {
            url,
            requests,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.stop.take();
        let _ = self.thread.take().unwrap().join();
    }
}

/// What every chain command reports for [`write_unresolvable_providers`].
pub const UNRESOLVABLE: &str = "no Esplora URL is configured";

/// A configuration that loads but cannot be served: chain data is set to
/// Esplora with no URL. Subfrost (Alkanes views) points at a live
/// listener, kept so tests can also assert no provider was called.
pub fn write_unresolvable_providers(dir: &TempDir) -> HttpServer {
    let server = HttpServer::start(|_| Some((400, "unexpected provider call".into())));
    let config = format!(
        "network = \"signet\"\n\n[signet]\nchain = \"esplora\"\nsubfrost_url = {:?}\n",
        server.url
    );
    std::fs::write(dir.path().join("config.toml"), config).unwrap();
    server
}

pub fn assert_no_provider_calls(server: &HttpServer) {
    assert_eq!(server.requests.load(std::sync::atomic::Ordering::SeqCst), 0);
}
