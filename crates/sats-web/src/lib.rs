//! sats-web — the sats wallet engine running in the browser.
//!
//! Compiles `sats-core` to WebAssembly behind a small JSON API for the
//! website playground. Only the chain is simulated: an in-memory faucet
//! mints fake confirmed outputs and "broadcast" confirms into the next
//! simulated block. Descriptors, planning, conservative UTXO exclusion,
//! signing, seed sealing, and grant authorization are the same `sats-core`
//! code the native CLI and MCP server run.
//!
//! The API is stringly typed on purpose: methods return JSON documents the
//! terminal UI renders, and errors are plain messages. Nothing here is a
//! compatibility surface; the native CLI and MCP schemas remain the stable
//! contracts.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use bdk_wallet::bitcoin::absolute::LockTime;
use bdk_wallet::bitcoin::hashes::Hash;
use bdk_wallet::bitcoin::transaction::Version;
use bdk_wallet::bitcoin::{
    Address, Amount, BlockHash, FeeRate, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxOut, Txid, Witness,
};
use bdk_wallet::chain::{BlockId, ConfirmationBlockTime, TxUpdate};
use bdk_wallet::{KeychainKind, Update, Wallet};
use bip39::Mnemonic;
use sats_core::authz::{GRANT_FORMAT_VERSION, Grant, SpendRequest};
use sats_core::fmt::format_sats;
use sats_core::plan::{PreparedSpend, TransactionRecord};
use sats_core::signer::{LocalSigner, Signer};
use sats_core::{amount, engine, seed, token};
use serde_json::json;
use wasm_bindgen::prelude::*;

const NETWORK: Network = Network::Signet;
const NETWORK_NAME: &str = "signet";

/// The whole playground state: a real watch-only wallet plus the simulated
/// chain tip and grant/plan/history stores that the native shell would keep
/// on disk.
struct Sim {
    mnemonic: Mnemonic,
    wallet: Wallet,
    height: u32,
    grants: HashMap<String, Grant>,
    prepared: HashMap<String, PreparedSpend>,
    history: Vec<TransactionRecord>,
}

fn random_bytes32() -> Result<[u8; 32], String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| "no entropy source available".to_string())?;
    Ok(bytes)
}

fn apply(wallet: &mut Wallet, update: Update) -> Result<(), String> {
    wallet
        .apply_update(update)
        .map_err(|e| format!("simulated chain update failed: {e}"))
}

/// Extend the simulated chain with one block (clock-free equivalent of
/// bdk's test-utils `insert_checkpoint`, which reads the system time and
/// would trap on wasm).
fn apply_checkpoint(wallet: &mut Wallet, block: BlockId) -> Result<(), String> {
    let cp = wallet.latest_checkpoint().insert(block);
    apply(
        wallet,
        Update {
            chain: Some(cp),
            ..Default::default()
        },
    )
}

fn apply_tx(wallet: &mut Wallet, tx: Transaction, seen_at: u64) -> Result<(), String> {
    let txid = tx.compute_txid();
    let mut tx_update = TxUpdate::default();
    tx_update.txs = vec![Arc::new(tx)];
    tx_update.seen_ats = [(txid, seen_at)].into();
    apply(
        wallet,
        Update {
            tx_update,
            ..Default::default()
        },
    )
}

fn apply_anchor(
    wallet: &mut Wallet,
    txid: Txid,
    anchor: ConfirmationBlockTime,
) -> Result<(), String> {
    let mut tx_update = TxUpdate::default();
    tx_update.anchors = [(anchor, txid)].into();
    apply(
        wallet,
        Update {
            tx_update,
            ..Default::default()
        },
    )
}

fn parse_recipient(s: &str) -> Result<Address, String> {
    Address::from_str(s)
        .map_err(|_| format!("invalid address {s:?}"))?
        .require_network(NETWORK)
        .map_err(|_| format!("address {s:?} is not a {NETWORK_NAME} address"))
}

/// Same rule as the native CLI (`sats agent grant`).
fn validate_agent_name(agent: &str) -> Result<(), String> {
    let ok = !agent.is_empty()
        && agent.len() <= 32
        && agent
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err("agent name must be 1-32 chars of a-z, 0-9, - or _".to_string())
    }
}

impl Sim {
    fn create(now: u64) -> Result<(Sim, String), String> {
        let mnemonic = seed::generate_mnemonic(12).map_err(|e| e.to_string())?;
        let (ext, int) = seed::public_descriptors(&mnemonic, NETWORK).map_err(|e| e.to_string())?;
        let descriptor = ext.clone();
        let wallet = Wallet::create(ext, int)
            .network(NETWORK)
            .create_wallet_no_persist()
            .map_err(|e| e.to_string())?;
        let mut sim = Sim {
            mnemonic,
            wallet,
            height: 0,
            grants: HashMap::new(),
            prepared: HashMap::new(),
            history: Vec::new(),
        };
        // A little simulated history so the tip looks like a live chain.
        sim.mine_block()?;
        let out = json!({
            "network": NETWORK_NAME,
            "mnemonic": sim.mnemonic.to_string(),
            "descriptor": descriptor,
            "height": sim.height,
            "created_at": now,
        })
        .to_string();
        Ok((sim, out))
    }

    fn mine_block(&mut self) -> Result<BlockId, String> {
        let block = BlockId {
            height: self.height + 1,
            hash: BlockHash::from_byte_array(random_bytes32()?),
        };
        apply_checkpoint(&mut self.wallet, block)?;
        self.height = block.height;
        Ok(block)
    }

    /// Mine `tx` into the next simulated block.
    fn confirm_tx(&mut self, tx: Transaction, now: u64) -> Result<u32, String> {
        let txid = tx.compute_txid();
        apply_tx(&mut self.wallet, tx, now)?;
        let block = self.mine_block()?;
        apply_anchor(
            &mut self.wallet,
            txid,
            ConfirmationBlockTime {
                block_id: block,
                confirmation_time: now,
            },
        )?;
        Ok(block.height)
    }

    fn receive(&mut self) -> Result<String, String> {
        let info = self.wallet.reveal_next_address(KeychainKind::External);
        Ok(json!({
            "address": info.address.to_string(),
            "index": info.index,
        })
        .to_string())
    }

    fn balance(&self) -> Result<String, String> {
        let b = self.wallet.balance();
        let pending = b.trusted_pending + b.untrusted_pending + b.immature;
        Ok(json!({
            "confirmed_sat": b.confirmed.to_sat(),
            "pending_sat": pending.to_sat(),
            "total_sat": b.total().to_sat(),
            "height": self.height,
        })
        .to_string())
    }

    /// The playground-only faucet: mint a fake confirmed output to the
    /// wallet's next unused address.
    fn faucet(&mut self, amount_str: &str, now: u64) -> Result<String, String> {
        let amount_sat = amount::parse(amount_str)?;
        if amount_sat == 0 {
            return Err("faucet amount must be greater than 0".to_string());
        }
        let address = self
            .wallet
            .next_unused_address(KeychainKind::External)
            .address;
        let tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: Txid::from_byte_array(random_bytes32()?),
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(amount_sat),
                script_pubkey: address.script_pubkey(),
            }],
        };
        let txid = tx.compute_txid();
        let height = self.confirm_tx(tx, now)?;
        Ok(json!({
            "txid": txid.to_string(),
            "amount_sat": amount_sat,
            "height": height,
        })
        .to_string())
    }

    /// Shared preparation path: parse, exclude dust suspects, plan.
    fn plan(
        &mut self,
        recipient: &str,
        amount_str: &str,
        fee_rate: u32,
        now: u64,
    ) -> Result<PreparedSpend, String> {
        let recipient = parse_recipient(recipient)?;
        let amount_sat = amount::parse(amount_str)?;
        if fee_rate == 0 {
            return Err("fee rate must be at least 1 sat/vB".to_string());
        }
        let unspendable = engine::dust_suspects(
            self.wallet
                .list_unspent()
                .map(|u| (u.outpoint, u.txout.value)),
        );
        engine::build_plan(
            &mut self.wallet,
            &recipient,
            Amount::from_sat(amount_sat),
            FeeRate::from_sat_per_vb_u32(fee_rate),
            &unspendable,
            NETWORK_NAME,
            now,
        )
        .map_err(|e| e.to_string())
    }

    fn prepare(
        &mut self,
        recipient: &str,
        amount_str: &str,
        fee_rate: u32,
        now: u64,
    ) -> Result<String, String> {
        let plan = self.plan(recipient, amount_str, fee_rate, now)?;
        let out = json!({
            "id": plan.id,
            "recipient": plan.recipient,
            "amount_sat": plan.amount_sat,
            "fee_sat": plan.fee_sat,
            "total_sat": plan.total_sat(),
            "excluded_utxos": plan.excluded_utxos,
        })
        .to_string();
        self.prepared.insert(plan.id.clone(), plan);
        Ok(out)
    }

    /// Sign a prepared spend and "broadcast" it into the simulated chain.
    fn sign_and_broadcast(
        &mut self,
        plan: PreparedSpend,
        now: u64,
    ) -> Result<TransactionRecord, String> {
        let mut psbt = plan.psbt().clone();
        let mut signer = LocalSigner::new(self.mnemonic.clone(), NETWORK);
        let finalized = signer.sign(&mut psbt).map_err(|e| e.to_string())?;
        if !finalized {
            return Err("signing did not finalize the transaction".to_string());
        }
        let mut record = plan
            .into_transaction(psbt, None)
            .map_err(|e| e.to_string())?;
        let tx = record.tx().map_err(|e| e.to_string())?;
        record.mark_broadcast();
        self.confirm_tx(tx, now)?;
        self.history.push(record.clone());
        Ok(record)
    }

    fn confirm(&mut self, id: &str, now: u64) -> Result<String, String> {
        let plan = self
            .prepared
            .remove(id)
            .ok_or_else(|| format!("no prepared spend {id:?}"))?;
        let record = self.sign_and_broadcast(plan, now)?;
        Ok(json!({
            "txid": record.txid,
            "amount_sat": record.amount_sat,
            "fee_sat": record.fee_sat,
            "total_sat": record.total_sat(),
            "height": self.height,
        })
        .to_string())
    }

    fn cancel(&mut self, id: &str) -> Result<String, String> {
        match self.prepared.remove(id) {
            Some(_) => Ok(json!({ "cancelled": id }).to_string()),
            None => Err(format!("no prepared spend {id:?}")),
        }
    }

    fn grant(
        &mut self,
        agent: &str,
        budget_str: &str,
        max_tx_str: Option<String>,
        max_fee_str: Option<String>,
        lifetime_secs: u64,
        now: u64,
    ) -> Result<String, String> {
        validate_agent_name(agent)?;
        let budget_sat = amount::parse(budget_str)?;
        if budget_sat == 0 {
            return Err("budget must be greater than 0".to_string());
        }
        if lifetime_secs == 0 {
            return Err("--for must be a positive duration".to_string());
        }
        let max_tx_sat = max_tx_str.as_deref().map(amount::parse).transpose()?;
        let max_fee_sat = max_fee_str.as_deref().map(amount::parse).transpose()?;

        // Same shape as the native grant: a capability token, never key
        // material. The playground has no daemon to hold a seed behind a
        // process boundary — it is one page — so the token is only
        // demonstrating the model, not enforcing it.
        let issued = token::generate().map_err(|e| e.to_string())?;

        let replaced = self.grants.contains_key(agent);
        let grant = Grant {
            format_version: GRANT_FORMAT_VERSION,
            agent: agent.to_string(),
            network: NETWORK_NAME.to_string(),
            budget_sat,
            spent_sat: 0,
            max_tx_sat,
            max_fee_sat,
            created_at: now,
            expires_at: now.saturating_add(lifetime_secs),
            tx_count: 0,
            token_id: issued.token_id.clone(),
            token_hash: issued.token_hash.clone(),
        };
        let out = json!({
            "agent": grant.agent,
            "token_id": grant.token_id,
            "budget_sat": grant.budget_sat,
            "max_tx_sat": grant.max_tx_sat,
            "max_fee_sat": grant.max_fee_sat,
            "expires_at": grant.expires_at,
            "replaced": replaced,
        })
        .to_string();
        self.grants.insert(agent.to_string(), grant);
        Ok(out)
    }

    fn grants(&self, now: u64) -> Result<String, String> {
        let mut list: Vec<_> = self.grants.values().collect();
        list.sort_by(|a, b| a.agent.cmp(&b.agent));
        let rows: Vec<_> = list
            .into_iter()
            .map(|g| {
                json!({
                    "agent": g.agent,
                    "budget_sat": g.budget_sat,
                    "spent_sat": g.spent_sat,
                    "remaining_sat": g.remaining_sat(),
                    "max_tx_sat": g.max_tx_sat,
                    "max_fee_sat": g.max_fee_sat,
                    "tx_count": g.tx_count,
                    "expires_at": g.expires_at,
                    "expired": g.is_expired(now),
                })
            })
            .collect();
        Ok(json!(rows).to_string())
    }

    fn revoke(&mut self, agent: &str) -> Result<String, String> {
        match self.grants.remove(agent) {
            Some(_) => Ok(json!({ "revoked": agent }).to_string()),
            None => Err(format!("no grant named {agent:?}")),
        }
    }

    /// The agent send loop the MCP server runs: re-read the grant, plan,
    /// reserve budget before signing, refund only if signing failed.
    /// Denials are successful results, not errors.
    fn agent_send(
        &mut self,
        agent: &str,
        recipient: &str,
        amount_str: &str,
        fee_rate: u32,
        now: u64,
    ) -> Result<String, String> {
        if !self.grants.contains_key(agent) {
            return Err(format!(
                "no grant named {agent:?} — create one with `sats agent grant`"
            ));
        }
        let plan = self.plan(recipient, amount_str, fee_rate, now)?;
        let req = SpendRequest {
            amount_sat: plan.amount_sat,
            fee_sat: plan.fee_sat,
        };
        let grant = self.grants.get_mut(agent).expect("checked above");
        if let Err(reason) = grant.reserve(&req, now) {
            // The CLI's detail lines are column-aligned; joined onto one
            // line the padding is noise, so collapse runs of spaces.
            let detail = reason
                .human()
                .lines()
                .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect::<Vec<_>>()
                .join("; ");
            let message = format!("human authorization required: {detail}");
            return Ok(json!({
                "status": "denied",
                "reason": reason.code(),
                "message": message,
                "amount_sat": req.amount_sat,
                "fee_sat": req.fee_sat,
            })
            .to_string());
        }
        match self.sign_and_broadcast(plan, now) {
            Ok(record) => {
                let grant = self.grants.get(agent).expect("still present");
                Ok(json!({
                    "status": "sent",
                    "txid": record.txid,
                    "amount_sat": record.amount_sat,
                    "fee_sat": record.fee_sat,
                    "total_sat": record.total_sat(),
                    "grant_remaining_sat": grant.remaining_sat(),
                    "grant_tx_count": grant.tx_count,
                })
                .to_string())
            }
            Err(e) => {
                // Signing failed, so no signature exists: refund.
                if let Some(grant) = self.grants.get_mut(agent) {
                    grant.refund(&req);
                }
                Err(e)
            }
        }
    }

    fn history(&self) -> Result<String, String> {
        let rows: Vec<_> = self
            .history
            .iter()
            .rev()
            .map(|r| {
                json!({
                    "txid": r.txid,
                    "recipient": r.recipient,
                    "amount_sat": r.amount_sat,
                    "fee_sat": r.fee_sat,
                    "status": format!("{:?}", r.status).to_lowercase(),
                    "created_at": r.created_at,
                })
            })
            .collect();
        Ok(json!(rows).to_string())
    }
}

/// The wasm-facing handle. Every method returns a JSON string on success
/// and throws a plain message on failure; `now` parameters are unix
/// seconds supplied by the page, keeping the engine clock-free.
#[wasm_bindgen]
#[derive(Default)]
pub struct Playground {
    sim: Option<Sim>,
}

fn js<T>(result: Result<T, String>) -> Result<T, JsError> {
    result.map_err(|e| JsError::new(&e))
}

#[wasm_bindgen]
impl Playground {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Playground {
        Playground::default()
    }

    /// Crate version, so the page can show what it is running.
    pub fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    pub fn has_wallet(&self) -> bool {
        self.sim.is_some()
    }

    pub fn init(&mut self, now: f64) -> Result<String, JsError> {
        let (sim, out) = js(Sim::create(now as u64))?;
        self.sim = Some(sim);
        Ok(out)
    }

    pub fn reset(&mut self) {
        self.sim = None;
    }

    pub fn format_sats(&self, sats: f64) -> String {
        format_sats(sats as u64)
    }

    pub fn receive(&mut self) -> Result<String, JsError> {
        js(self.sim_mut()?.receive())
    }

    pub fn balance(&self) -> Result<String, JsError> {
        js(self.sim_ref()?.balance())
    }

    pub fn faucet(&mut self, amount: &str, now: f64) -> Result<String, JsError> {
        js(self.sim_mut()?.faucet(amount, now as u64))
    }

    pub fn prepare(
        &mut self,
        recipient: &str,
        amount: &str,
        fee_rate: u32,
        now: f64,
    ) -> Result<String, JsError> {
        js(self
            .sim_mut()?
            .prepare(recipient, amount, fee_rate, now as u64))
    }

    pub fn confirm(&mut self, id: &str, now: f64) -> Result<String, JsError> {
        js(self.sim_mut()?.confirm(id, now as u64))
    }

    pub fn cancel(&mut self, id: &str) -> Result<String, JsError> {
        js(self.sim_mut()?.cancel(id))
    }

    pub fn grant(
        &mut self,
        agent: &str,
        budget: &str,
        max_tx: Option<String>,
        max_fee: Option<String>,
        lifetime_secs: f64,
        now: f64,
    ) -> Result<String, JsError> {
        js(self.sim_mut()?.grant(
            agent,
            budget,
            max_tx,
            max_fee,
            lifetime_secs as u64,
            now as u64,
        ))
    }

    pub fn grants(&self, now: f64) -> Result<String, JsError> {
        js(self.sim_ref()?.grants(now as u64))
    }

    pub fn revoke(&mut self, agent: &str) -> Result<String, JsError> {
        js(self.sim_mut()?.revoke(agent))
    }

    pub fn agent_send(
        &mut self,
        agent: &str,
        recipient: &str,
        amount: &str,
        fee_rate: u32,
        now: f64,
    ) -> Result<String, JsError> {
        js(self
            .sim_mut()?
            .agent_send(agent, recipient, amount, fee_rate, now as u64))
    }

    pub fn history(&self) -> Result<String, JsError> {
        js(self.sim_ref()?.history())
    }
}

impl Playground {
    fn sim_mut(&mut self) -> Result<&mut Sim, JsError> {
        js(self
            .sim
            .as_mut()
            .ok_or_else(|| "no wallet — run `sats init` first".to_string()))
    }

    fn sim_ref(&self) -> Result<&Sim, JsError> {
        js(self
            .sim
            .as_ref()
            .ok_or_else(|| "no wallet — run `sats init` first".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;

    fn value(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    fn funded_sim() -> Sim {
        let (mut sim, out) = Sim::create(NOW).unwrap();
        let init = value(&out);
        assert_eq!(init["network"], "signet");
        assert_eq!(
            init["mnemonic"]
                .as_str()
                .unwrap()
                .split_whitespace()
                .count(),
            12
        );
        sim.faucet("100k", NOW).unwrap();
        sim
    }

    fn own_address(sim: &mut Sim) -> String {
        value(&sim.receive().unwrap())["address"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn faucet_funds_confirmed_balance() {
        let sim = funded_sim();
        let balance = value(&sim.balance().unwrap());
        assert_eq!(balance["confirmed_sat"], 100_000);
        assert_eq!(balance["pending_sat"], 0);
        assert_eq!(balance["total_sat"], 100_000);
    }

    #[test]
    fn prepare_confirm_send_updates_balance_and_history() {
        let mut sim = funded_sim();
        let addr = own_address(&mut sim);
        let plan = value(&sim.prepare(&addr, "25k", 2, NOW).unwrap());
        assert_eq!(plan["amount_sat"], 25_000);
        let fee = plan["fee_sat"].as_u64().unwrap();
        assert!(fee > 0);

        let id = plan["id"].as_str().unwrap();
        let sent = value(&sim.confirm(id, NOW).unwrap());
        assert_eq!(sent["amount_sat"], 25_000);

        // Self-send: only the fee leaves the wallet.
        let balance = value(&sim.balance().unwrap());
        assert_eq!(balance["total_sat"], 100_000 - fee);

        let history = value(&sim.history().unwrap());
        assert_eq!(history.as_array().unwrap().len(), 1);
        assert_eq!(history[0]["status"], "broadcast");

        // The prepared spend is consumed.
        assert!(sim.confirm(id, NOW).is_err());
    }

    #[test]
    fn cancel_drops_prepared_spend() {
        let mut sim = funded_sim();
        let addr = own_address(&mut sim);
        let plan = value(&sim.prepare(&addr, "10k", 2, NOW).unwrap());
        let id = plan["id"].as_str().unwrap();
        sim.cancel(id).unwrap();
        assert!(sim.confirm(id, NOW).is_err());
    }

    #[test]
    fn overspend_is_a_typed_error() {
        let mut sim = funded_sim();
        let addr = own_address(&mut sim);
        assert!(sim.prepare(&addr, "1m", 2, NOW).is_err());
    }

    #[test]
    fn mainnet_address_is_rejected() {
        let mut sim = funded_sim();
        let err = sim
            .prepare(
                "bc1p5cyxnuxmeuwuvkwfem96lqzszd02n6xdcjrs20cac6yqjjwudpxqkedrcr",
                "10k",
                2,
                NOW,
            )
            .unwrap_err();
        assert!(err.contains("signet"), "{err}");
    }

    #[test]
    fn agent_send_within_grant_draws_budget_down() {
        let mut sim = funded_sim();
        let addr = own_address(&mut sim);
        sim.grant("claude", "50k", None, None, 86_400, NOW).unwrap();
        let sent = value(&sim.agent_send("claude", &addr, "10k", 2, NOW).unwrap());
        assert_eq!(sent["status"], "sent");
        let fee = sent["fee_sat"].as_u64().unwrap();
        assert_eq!(
            sent["grant_remaining_sat"].as_u64().unwrap(),
            50_000 - 10_000 - fee
        );

        let grants = value(&sim.grants(NOW).unwrap());
        assert_eq!(grants[0]["tx_count"], 1);
    }

    #[test]
    fn agent_send_over_cap_is_denied_not_error() {
        let mut sim = funded_sim();
        let addr = own_address(&mut sim);
        sim.grant("claude", "50k", Some("10k".to_string()), None, 86_400, NOW)
            .unwrap();
        let denied = value(&sim.agent_send("claude", &addr, "20k", 2, NOW).unwrap());
        assert_eq!(denied["status"], "denied");
        assert_eq!(denied["reason"], "over_max_tx");
        assert!(
            denied["message"]
                .as_str()
                .unwrap()
                .starts_with("human authorization required"),
        );
        // A denial reserves nothing.
        let grants = value(&sim.grants(NOW).unwrap());
        assert_eq!(grants[0]["spent_sat"], 0);
        assert_eq!(grants[0]["tx_count"], 0);
    }

    #[test]
    fn expired_grant_denies_and_revocation_removes_authority() {
        let mut sim = funded_sim();
        let addr = own_address(&mut sim);
        sim.grant("claude", "50k", None, None, 3_600, NOW).unwrap();
        let denied = value(
            &sim.agent_send("claude", &addr, "10k", 2, NOW + 3_600)
                .unwrap(),
        );
        assert_eq!(denied["reason"], "expired");

        sim.revoke("claude").unwrap();
        assert!(sim.agent_send("claude", &addr, "10k", 2, NOW).is_err());
        assert!(sim.revoke("claude").is_err());
    }

    #[test]
    fn dust_suspect_outputs_are_excluded_from_selection() {
        let mut sim = funded_sim();
        // A classic inscription postage output.
        sim.faucet("546", NOW).unwrap();
        let addr = own_address(&mut sim);
        let plan = value(&sim.prepare(&addr, "99k", 2, NOW).unwrap());
        assert_eq!(plan["excluded_utxos"], 1);
    }

    #[test]
    fn grant_names_are_validated() {
        let mut sim = funded_sim();
        assert!(
            sim.grant("Bad Name", "50k", None, None, 3_600, NOW)
                .is_err()
        );
        assert!(sim.grant("claude", "0", None, None, 3_600, NOW).is_err());
    }
}
