//! Preparation sequencing through the real CLI with disposable wallets.
//! Local HTTP fixtures count guard and fee requests; the existing mock
//! sync provider keeps funding deterministic and can fail each fresh sync.

mod common;

use std::fs;
use std::sync::atomic::Ordering;

use common::{ADDRESS, HttpServer, fund_wallet, init_wallet, json_stdout, sats};
use predicates::prelude::*;
use sats_core::bitcoin::OutPoint;
use tempfile::TempDir;

enum GuardAnswer {
    Clear,
    Protected,
    Unavailable,
}

struct Fixture {
    dir: TempDir,
    chain: std::path::PathBuf,
    outpoints: Vec<OutPoint>,
    guard: HttpServer,
    fees: HttpServer,
}

impl Fixture {
    fn new(values: &[u64], guard_answer: GuardAnswer, fees_available: bool) -> Self {
        let dir = TempDir::new().unwrap();
        init_wallet(&dir);
        let outpoints = fund_wallet(&dir, values);
        let chain = dir.path().join("chain");
        fs::create_dir(&chain).unwrap();
        let guard = HttpServer::start(move |request| {
            assert!(request.starts_with("POST / "), "{request}");
            Some(match guard_answer {
                GuardAnswer::Unavailable => (503, "guard unavailable".into()),
                GuardAnswer::Clear | GuardAnswer::Protected => (
                    200,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 0,
                        "result": {
                            "inscriptions": if matches!(guard_answer, GuardAnswer::Protected) {
                                vec!["protected-asset"]
                            } else {
                                vec![]
                            },
                            "runes": []
                        }
                    })
                    .to_string(),
                ),
            })
        });
        let fees = HttpServer::start(move |request| {
            assert!(request.starts_with("GET /fee-estimates "), "{request}");
            Some(if fees_available {
                (200, r#"{"2":2.0}"#.into())
            } else {
                (503, "fees unavailable".into())
            })
        });
        fs::write(
            dir.path().join("config.toml"),
            format!(
                "network = \"signet\"\n\
                 [providers.chain]\n\
                 driver = \"mock\"\nnetwork = \"signet\"\n\
                 url = \"file://{}\"\ncapabilities = [\"chain.sync\", \"chain.broadcast\"]\n\
                 [providers.guard]\n\
                 driver = \"subfrost\"\nnetwork = \"signet\"\n\
                 url = {:?}\ncapabilities = [\"guard.ord\"]\n\
                 [providers.fees]\n\
                 driver = \"esplora\"\nnetwork = \"signet\"\n\
                 url = {:?}\ncapabilities = [\"chain.fees\"]\n",
                chain.display(),
                guard.url,
                fees.url,
            ),
        )
        .unwrap();
        Self {
            dir,
            chain,
            outpoints,
            guard,
            fees,
        }
    }

    fn counts(&self) -> (usize, usize) {
        (
            self.guard.requests.load(Ordering::SeqCst),
            self.fees.requests.load(Ordering::SeqCst),
        )
    }

    fn dry_run(&self) -> assert_cmd::Command {
        let mut command = sats(&self.dir);
        command.args(["send", ADDRESS, "5000", "--dry-run", "--json"]);
        command
    }

    /// A second configured guard lets the union contain distinct and
    /// overlapping outpoints without changing the shared provider fixture.
    fn add_native_guard(&self, protected: Option<&[OutPoint]>) {
        let native = self.dir.path().join("native-guard");
        fs::create_dir(&native).unwrap();
        if let Some(protected) = protected {
            fs::write(
                native.join("guard.json"),
                serde_json::json!({
                    "protected": protected.iter().map(ToString::to_string).collect::<Vec<_>>()
                })
                .to_string(),
            )
            .unwrap();
        }
        let config_path = self.dir.path().join("config.toml");
        let mut config = fs::read_to_string(&config_path).unwrap();
        config.push_str(&format!(
            "\n[providers.native_guard]\n\
             driver = \"mock\"\nnetwork = \"signet\"\n\
             url = \"file://{}\"\ncapabilities = [\"guard.native\"]\n",
            native.display(),
        ));
        fs::write(config_path, config).unwrap();
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
fn empty_synced_wallet_skips_guards_and_fees_and_explains_funding() {
    let fx = Fixture::new(&[], GuardAnswer::Unavailable, false);
    // A missing native answer errors even for an empty outpoint list if
    // called, unlike the HTTP guard's per-outpoint loop.
    fx.add_native_guard(None);
    fx.dry_run()
        .assert()
        .failure()
        .stderr(predicate::str::contains("Insufficient funds"))
        .stderr(predicate::str::contains("sats receive"))
        .stderr(predicate::str::contains("asset check").not())
        .stderr(predicate::str::contains("estimate fee").not());
    assert_eq!(fx.counts(), (0, 0));
}

#[test]
fn fully_guarded_wallet_preserves_protected_funds_and_skips_fees() {
    let fx = Fixture::new(&[100_000], GuardAnswer::Protected, false);
    fx.dry_run()
        .assert()
        .failure()
        .stderr(predicate::str::contains("protected"))
        .stderr(predicate::str::contains("unprotected outputs"))
        .stderr(predicate::str::contains("no unspent outputs").not())
        .stderr(predicate::str::contains("estimate fee").not())
        .stderr(predicate::str::contains("--no-guards").not());
    assert_eq!(fx.counts(), (1, 0));
}

#[test]
fn dust_and_guard_union_can_exclude_every_candidate_before_fees() {
    let fx = Fixture::new(&[546, 10_000], GuardAnswer::Clear, false);
    fx.add_native_guard(Some(&fx.outpoints[1..]));
    fx.dry_run()
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "all 2 unspent outputs are protected",
        ))
        .stderr(predicate::str::contains("estimate fee").not())
        .stderr(predicate::str::contains("--allow-dust").not());
    assert_eq!(fx.counts(), (2, 0));
}

#[test]
fn dust_only_wallet_still_checks_configured_guards_but_skips_fees() {
    let fx = Fixture::new(&[330, 546], GuardAnswer::Clear, false);
    fx.dry_run()
        .assert()
        .failure()
        .stderr(predicate::str::contains("protected"))
        .stderr(predicate::str::contains("no unspent outputs").not());
    assert_eq!(fx.counts(), (2, 0));
}

#[test]
fn remaining_candidates_receive_all_checks_and_union_exclusions() {
    let fx = Fixture::new(&[546, 10_000, 100_000], GuardAnswer::Clear, true);
    // One dust output is repeated by the guard; the other guarded output
    // has an ordinary value. The only remaining candidate funds the plan.
    fx.add_native_guard(Some(&fx.outpoints[..2]));
    let output = json_stdout(fx.dry_run().assert().success());
    assert_eq!(output["excluded_utxos"], 2);
    assert_eq!(output["amount_sat"], 5_000);
    assert!(output["fee_sat"].as_u64().unwrap() > 0);
    assert_eq!(fx.counts(), (3, 1));
}

#[test]
fn guard_failure_is_fail_closed_before_fees() {
    let fx = Fixture::new(&[100_000], GuardAnswer::Unavailable, true);
    fx.dry_run()
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "refusing to plan without the asset check",
        ));
    assert_eq!(fx.counts(), (1, 0));
}

#[test]
fn sync_failure_is_never_reported_as_an_empty_wallet() {
    let fx = Fixture::new(&[], GuardAnswer::Unavailable, false);
    fs::write(fx.chain.join("sync-error"), "fresh sync failed").unwrap();
    fx.dry_run()
        .assert()
        .failure()
        .stderr(predicate::str::contains("refusing to plan on stale state"))
        .stderr(predicate::str::contains("Insufficient funds").not())
        .stderr(predicate::str::contains("sats receive").not());
    assert_eq!(fx.counts(), (0, 0));
}

#[test]
fn each_preparation_requires_fresh_sync_before_guard_and_fee_calls() {
    let fx = Fixture::new(&[100_000], GuardAnswer::Clear, true);
    fx.dry_run().assert().success();
    assert_eq!(fx.counts(), (1, 1));
    fs::write(fx.chain.join("sync-error"), "subsequent sync failed").unwrap();
    fx.dry_run()
        .assert()
        .failure()
        .stderr(predicate::str::contains("refusing to plan on stale state"));
    assert_eq!(fx.counts(), (1, 1), "failed fresh sync stops downstream IO");
}

#[test]
fn fee_provider_guidance_is_valid_for_send_and_approval() {
    let fx = Fixture::new(&[100_000], GuardAnswer::Clear, false);
    let id = fx.pending_request();
    fx.dry_run()
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "check the configured fee provider",
        ))
        .stderr(predicate::str::contains("--fee-rate").not());
    let send_fee_calls = fx.counts().1;
    assert!(send_fee_calls > 0);
    sats(&fx.dir)
        .args(["agent", "approve", id, "--yes", "--json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "check the configured fee provider",
        ))
        .stderr(predicate::str::contains("--fee-rate").not());
    assert_eq!(fx.counts().0, 2);
    // Transport-level retries may add requests for the 503 response;
    // both surfaces must have attempted fee estimation independently.
    assert!(fx.counts().1 > send_fee_calls);
    assert!(!fx.chain.join("broadcasts.log").exists());
}

#[test]
fn explicit_fee_still_checks_guards_and_skips_fee_estimation() {
    let fx = Fixture::new(&[100_000], GuardAnswer::Clear, false);
    fx.dry_run().args(["--fee-rate", "2"]).assert().success();
    assert_eq!(fx.counts(), (1, 0));
}

#[test]
fn explicit_guard_bypass_with_funds_still_estimates_fees() {
    let fx = Fixture::new(&[100_000], GuardAnswer::Unavailable, true);
    fx.dry_run().arg("--no-guards").assert().success();
    assert_eq!(fx.counts(), (0, 1));
}
