//! Preparation sequencing through the real CLI with disposable wallets.
//! A local Esplora fee endpoint counts its requests; the mock chain source
//! keeps funding deterministic and can fail each fresh sync.

mod common;

use std::fs;
use std::sync::atomic::Ordering;

use common::{ADDRESS, HttpServer, fund_wallet, init_wallet, json_stdout, sats};
use predicates::prelude::*;
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    chain: std::path::PathBuf,
    fees: HttpServer,
}

impl Fixture {
    fn new(values: &[u64], fees_available: bool) -> Self {
        let dir = TempDir::new().unwrap();
        init_wallet(&dir);
        fund_wallet(&dir, values);
        let chain = dir.path().join("chain");
        fs::create_dir(&chain).unwrap();
        let fees = HttpServer::start(move |request| {
            assert!(request.starts_with("GET /fee-estimates "), "{request}");
            Some(if fees_available {
                (200, r#"{"2":2.0}"#.into())
            } else {
                (503, "fees unavailable".into())
            })
        });
        // Mock chain data with fee estimates handed to the local Esplora.
        fs::write(chain.join("fees-via"), &fees.url).unwrap();
        fs::write(
            dir.path().join("config.toml"),
            format!(
                "network = \"signet\"\n\n[signet]\nchain = \"esplora\"\n\n[signet.esplora]\nurl = \"file://{}\"\n",
                chain.display(),
            ),
        )
        .unwrap();
        Self { dir, chain, fees }
    }

    fn fee_calls(&self) -> usize {
        self.fees.requests.load(Ordering::SeqCst)
    }

    fn dry_run(&self) -> assert_cmd::Command {
        let mut command = sats(&self.dir);
        command.args(["send", ADDRESS, "5000", "--dry-run", "--json"]);
        command
    }

    fn pending_request(&self) -> &'static str {
        sats(&self.dir)
            .args(["agent", "grant", "claude", "--budget", "50000", "--json"])
            .assert()
            .success();
        let grant: serde_json::Value = serde_json::from_slice(
            &fs::read(self.dir.path().join("signet/grants/claude.json")).unwrap(),
        )
        .unwrap();
        let id = "r-0123456789abcdef0123456789abcdef";
        let intent = sats_core::intent::SendIntent {
            network: "signet".into(),
            agent: "claude".into(),
            recipient: ADDRESS.into(),
            amount_sat: 5_000,
        };
        let requests = self.dir.path().join("signet/agent-requests/claude");
        fs::create_dir_all(&requests).unwrap();
        fs::write(
            requests.join(format!("{id}.json")),
            serde_json::json!({
                "format_version": sats_core::request::REQUEST_FORMAT_VERSION,
                "id": id,
                "network": intent.network,
                "agent": intent.agent,
                "grant_id": grant["grant_id"],
                "idempotency_key": "preparation-guidance",
                "recipient": intent.recipient,
                "amount_sat": intent.amount_sat,
                "intent_digest": intent.digest(),
                "created_at": 1000,
                "updated_at": 1000,
                "status": "pending_approval",
            })
            .to_string(),
        )
        .unwrap();
        id
    }
}

#[test]
fn empty_synced_wallet_skips_fees_and_explains_funding() {
    let fx = Fixture::new(&[], false);
    fx.dry_run()
        .assert()
        .failure()
        .stderr(predicate::str::contains("Insufficient funds"))
        .stderr(predicate::str::contains("sats receive"))
        .stderr(predicate::str::contains("estimate fee").not());
    assert_eq!(fx.fee_calls(), 0);
}

#[test]
fn dust_only_wallet_is_refused_before_fees() {
    let fx = Fixture::new(&[330, 546], false);
    fx.dry_run()
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "all 2 unspent outputs are excluded by the dust heuristic",
        ))
        .stderr(predicate::str::contains("no unspent outputs").not())
        .stderr(predicate::str::contains("estimate fee").not())
        .stderr(predicate::str::contains("--allow-dust").not());
    assert_eq!(fx.fee_calls(), 0);
}

#[test]
fn dust_is_excluded_and_the_rest_funds_the_plan() {
    let fx = Fixture::new(&[546, 10_000, 100_000], true);
    let output = json_stdout(fx.dry_run().assert().success());
    assert_eq!(output["excluded_utxos"], 1);
    assert_eq!(output["amount_sat"], 5_000);
    assert!(output["fee_sat"].as_u64().unwrap() > 0);
    assert_eq!(fx.fee_calls(), 1);
}

#[test]
fn sync_failure_is_never_reported_as_an_empty_wallet() {
    let fx = Fixture::new(&[], false);
    fs::write(fx.chain.join("sync-error"), "fresh sync failed").unwrap();
    fx.dry_run()
        .assert()
        .failure()
        .stderr(predicate::str::contains("refusing to plan on stale state"))
        .stderr(predicate::str::contains("Insufficient funds").not())
        .stderr(predicate::str::contains("sats receive").not());
    assert_eq!(fx.fee_calls(), 0);
}

#[test]
fn each_preparation_requires_fresh_sync_before_fee_calls() {
    let fx = Fixture::new(&[100_000], true);
    fx.dry_run().assert().success();
    assert_eq!(fx.fee_calls(), 1);
    fs::write(fx.chain.join("sync-error"), "subsequent sync failed").unwrap();
    fx.dry_run()
        .assert()
        .failure()
        .stderr(predicate::str::contains("refusing to plan on stale state"));
    assert_eq!(fx.fee_calls(), 1, "failed fresh sync stops downstream IO");
}

#[test]
fn fee_provider_guidance_is_valid_for_send_and_approval() {
    let fx = Fixture::new(&[100_000], false);
    let id = fx.pending_request();
    fx.dry_run()
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "check the configured fee provider",
        ))
        .stderr(predicate::str::contains("--fee-rate").not());
    let send_fee_calls = fx.fee_calls();
    assert!(send_fee_calls > 0);
    sats(&fx.dir)
        .args(["agent", "approve", id, "--yes", "--json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "check the configured fee provider",
        ))
        .stderr(predicate::str::contains("--fee-rate").not());
    // Transport-level retries may add requests for the 503 response;
    // both surfaces must have attempted fee estimation independently.
    assert!(fx.fee_calls() > send_fee_calls);
    assert!(!fx.chain.join("broadcasts.log").exists());
}

#[test]
fn explicit_fee_skips_fee_estimation() {
    let fx = Fixture::new(&[100_000], false);
    fx.dry_run().args(["--fee-rate", "2"]).assert().success();
    assert_eq!(fx.fee_calls(), 0);
}
