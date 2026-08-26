//! Subfrost driver: one aggregate JSON-RPC endpoint multiplexing
//! esplora-style chain queries with ord and alkanes indexer queries
//! (sandshrew-compatible namespacing).
//!
//! The URL may carry an API key in its path
//! (`https://mainnet.subfrost.io/v4/<KEY>/jsonrpc`), so it is a secret:
//! every error and log line uses the redacted origin-only form.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use bdk_esplora::esplora_client::api::{BlockInfo, OutputStatus, Tx, TxStatus, Vin};
use bdk_wallet::KeychainKind;
use bdk_wallet::chain::spk_client::{
    FullScanRequest, FullScanResponse, SpkWithExpectedTxids, SyncRequest, SyncResponse,
};
use bdk_wallet::chain::{BlockId, CheckPoint, ConfirmationBlockTime, TxUpdate};
use sats_core::bitcoin::hashes::{Hash, sha256};
use sats_core::bitcoin::{
    Amount, BlockHash, Network, OutPoint, Script, Transaction, TxOut, Txid, consensus, constants,
};
use serde::de::DeserializeOwned;

use super::error::{ProviderError, redact_url};

/// Consecutive unused script pubkeys before a full scan stops — matches the
/// esplora driver.
const STOP_GAP: usize = 20;

/// The wire dialect: sandshrew-compatible namespaced methods, where
/// `esplora_*` mirrors the Esplora REST paths.
///
/// TODO(subfrost-dialect): confirm every method name and result shape
/// against a live endpoint (the docs sites are unreachable from this
/// development environment). The dialect is deliberately confined to this
/// table and the `parse_*` helpers below so corrections stay local.
mod dialect {
    pub const FEE_ESTIMATES: &str = "esplora_fee-estimates";
    pub const BROADCAST: &str = "esplora_broadcast";
    pub const ORD_OUTPUT: &str = "ord_output";
    pub const ALKANES_BY_OUTPOINT: &str = "alkanes_protorunesbyoutpoint";
    /// Contract bytecode by alkane id. Params: one `{block, tx}` object
    /// with decimal-string values (u128s exceed JSON number range).
    /// Result: hex string.
    pub const ALKANES_GET_BYTECODE: &str = "alkanes_getbytecode";
    /// Simulate a contract call. Params: one
    /// `{target: {block, tx}, inputs: [...]}` object, decimal strings.
    /// Result: opaque JSON, displayed rather than trusted.
    pub const ALKANES_SIMULATE: &str = "alkanes_simulate";
    /// `GET /blocks` — recent block summaries. Params: `[]`.
    pub const BLOCKS: &str = "esplora_blocks";
    /// `GET /block-height/:height` — block hash at height. Params: `[height]`.
    pub const BLOCK_HEIGHT: &str = "esplora_block-height";
    /// `GET /scripthash/:hash/txs` — confirmed+mempool txs. Params: `[hash]`.
    pub const SCRIPTHASH_TXS: &str = "esplora_scripthash::txs";
    /// `GET /scripthash/:hash/txs/chain/:last_seen` — paging. Params: `[hash, last_seen]`.
    pub const SCRIPTHASH_TXS_CHAIN: &str = "esplora_scripthash::txs:chain";
    /// `GET /tx/:txid` — tx with status, or null. Params: `[txid]`.
    pub const TX: &str = "esplora_tx";
    /// `GET /tx/:txid/outspend/:vout` — spend status. Params: `[txid, vout]`.
    pub const TX_OUTSPEND: &str = "esplora_tx::outspend";
}

#[derive(Clone)]
pub struct SubfrostClient {
    url: String,
    display_url: String,
}

/// Manual Debug: the path key must never reach a log line.
impl std::fmt::Debug for SubfrostClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubfrostClient")
            .field("url", &self.display_url)
            .finish_non_exhaustive()
    }
}

impl SubfrostClient {
    pub fn new(url: String) -> Self {
        let display_url = redact_url(&url);
        SubfrostClient { url, display_url }
    }

    pub fn display_url(&self) -> &str {
        &self.display_url
    }

    /// Scrub the full URL (which may carry a path key) out of transport
    /// error text before it can reach a user-visible string.
    fn scrub(&self, text: &str) -> String {
        text.replace(&self.url, &self.display_url)
    }

    fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T, String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": method,
            "params": params,
        });
        let response = minreq::post(&self.url)
            .with_header("Content-Type", "application/json")
            .with_json(&body)
            .map_err(|e| self.scrub(&e.to_string()))?
            .send()
            .map_err(|e| self.scrub(&e.to_string()))?;
        if !(200..300).contains(&response.status_code) {
            return Err(format!("http {}", response.status_code));
        }
        let text = response.as_str().map_err(|e| self.scrub(&e.to_string()))?;
        parse_jsonrpc(text)
    }

    pub fn fee_estimates(&self) -> Result<HashMap<u16, f64>, ProviderError> {
        self.call(dialect::FEE_ESTIMATES, serde_json::json!([]))
            .map_err(|message| ProviderError::Fees {
                url: self.display_url.clone(),
                message,
            })
    }

    pub fn broadcast(&self, tx: &Transaction) -> Result<Txid, ProviderError> {
        let hex = hex::encode(consensus::encode::serialize(tx));
        let txid = tx.compute_txid();
        let echoed: String = self
            .call(dialect::BROADCAST, serde_json::json!([hex]))
            .map_err(|message| ProviderError::Broadcast {
                url: self.display_url.clone(),
                message,
            })?;
        if echoed.trim() != txid.to_string() {
            return Err(ProviderError::Broadcast {
                url: self.display_url.clone(),
                message: format!("endpoint echoed unexpected txid {:?}", echoed.trim()),
            });
        }
        Ok(txid)
    }

    fn guard_err(&self, name: &'static str, message: String) -> ProviderError {
        ProviderError::Guard {
            name: name.to_string(),
            url: self.display_url.clone(),
            message,
        }
    }

    /// Which of these outpoints carry ord assets (inscriptions or runes)?
    pub fn ord_protected(&self, outpoints: &[OutPoint]) -> Result<Vec<OutPoint>, ProviderError> {
        let mut protected = Vec::new();
        for outpoint in outpoints {
            let value: serde_json::Value = self
                .call(
                    dialect::ORD_OUTPUT,
                    serde_json::json!([outpoint.to_string()]),
                )
                .map_err(|m| self.guard_err("ord", m))?;
            if parse_ord_output(&value) {
                protected.push(*outpoint);
            }
        }
        Ok(protected)
    }

    fn view_err(&self, message: String) -> ProviderError {
        ProviderError::View {
            url: self.display_url.clone(),
            message,
        }
    }

    /// Contract bytecode for an alkane id, decoded from the endpoint's
    /// hex result.
    pub fn alkanes_bytecode(&self, block: u128, tx: u128) -> Result<Vec<u8>, ProviderError> {
        let value: serde_json::Value = self
            .call(
                dialect::ALKANES_GET_BYTECODE,
                serde_json::json!([{ "block": block.to_string(), "tx": tx.to_string() }]),
            )
            .map_err(|m| self.view_err(m))?;
        parse_bytecode_result(&value).map_err(|m| self.view_err(m))
    }

    /// Simulate a contract call; the result is returned verbatim.
    pub fn alkanes_simulate(
        &self,
        block: u128,
        tx: u128,
        inputs: &[u128],
    ) -> Result<serde_json::Value, ProviderError> {
        let inputs: Vec<String> = inputs.iter().map(u128::to_string).collect();
        self.call(
            dialect::ALKANES_SIMULATE,
            serde_json::json!([{
                "target": { "block": block.to_string(), "tx": tx.to_string() },
                "inputs": inputs,
            }]),
        )
        .map_err(|m| self.view_err(m))
    }

    /// Which of these outpoints carry alkanes balances?
    pub fn alkanes_protected(
        &self,
        outpoints: &[OutPoint],
    ) -> Result<Vec<OutPoint>, ProviderError> {
        let mut protected = Vec::new();
        for outpoint in outpoints {
            let value: serde_json::Value = self
                .call(
                    dialect::ALKANES_BY_OUTPOINT,
                    serde_json::json!([{ "txid": outpoint.txid.to_string(), "vout": outpoint.vout }]),
                )
                .map_err(|m| self.guard_err("alkanes", m))?;
            if parse_alkanes_outpoint(&value) {
                protected.push(*outpoint);
            }
        }
        Ok(protected)
    }
}

// ---------------------------------------------------------------------------
// chain.sync: a port of bdk_esplora 0.22's blocking scan onto the
// multiplexed dialect. Modeled line-by-line on `blocking_ext.rs` (sequential
// rather than threaded — wallet-scale request counts don't need parallelism
// over a single multiplexed endpoint). Do not innovate on the algorithm.
// ---------------------------------------------------------------------------

impl SubfrostClient {
    fn sync_err(&self, message: impl Into<String>) -> ProviderError {
        ProviderError::Sync {
            url: self.display_url.clone(),
            message: message.into(),
        }
    }

    fn block_hash(&self, height: u32) -> Result<BlockHash, String> {
        self.call(dialect::BLOCK_HEIGHT, serde_json::json!([height]))
    }

    fn latest_blocks(&self) -> Result<BTreeMap<u32, BlockHash>, String> {
        let infos: Vec<BlockInfo> = self.call(dialect::BLOCKS, serde_json::json!([]))?;
        Ok(infos.into_iter().map(|b| (b.height, b.id)).collect())
    }

    fn scripthash_txs(&self, script: &Script, last_seen: Option<Txid>) -> Result<Vec<Tx>, String> {
        let script_hash = sha256::Hash::hash(script.as_bytes());
        match last_seen {
            Some(last_seen) => self.call(
                dialect::SCRIPTHASH_TXS_CHAIN,
                serde_json::json!([format!("{script_hash:x}"), last_seen.to_string()]),
            ),
            None => self.call(
                dialect::SCRIPTHASH_TXS,
                serde_json::json!([format!("{script_hash:x}")]),
            ),
        }
    }

    fn tx_info(&self, txid: &Txid) -> Result<Option<Tx>, String> {
        self.call(dialect::TX, serde_json::json!([txid.to_string()]))
    }

    fn output_status(&self, txid: &Txid, vout: u32) -> Result<Option<OutputStatus>, String> {
        self.call(
            dialect::TX_OUTSPEND,
            serde_json::json!([txid.to_string(), vout]),
        )
    }

    /// The endpoint must serve the wallet's network: compare its genesis
    /// block hash with the expected one.
    pub fn check_network(&self, network: Network) -> Result<(), ProviderError> {
        let genesis = self.block_hash(0).map_err(|m| self.sync_err(m))?;
        if genesis != constants::genesis_block(network).block_hash() {
            return Err(ProviderError::WrongNetwork {
                name: "subfrost".to_string(),
                url: self.display_url.clone(),
                expected: crate::config::network_name(network),
            });
        }
        Ok(())
    }

    pub fn full_scan(
        &self,
        request: impl Into<FullScanRequest<KeychainKind>>,
    ) -> Result<FullScanResponse<KeychainKind>, ProviderError> {
        let mut request: FullScanRequest<KeychainKind> = request.into();
        let start_time = request.start_time();

        let chain_tip = request.chain_tip();
        let latest_blocks = if chain_tip.is_some() {
            Some(self.latest_blocks().map_err(|m| self.sync_err(m))?)
        } else {
            None
        };

        let mut tx_update = TxUpdate::default();
        let mut inserted_txs = HashSet::<Txid>::new();
        let mut last_active_indices = BTreeMap::<KeychainKind, u32>::new();
        for keychain in request.keychains() {
            let spks: Vec<(u32, SpkWithExpectedTxids)> = request
                .iter_spks(keychain)
                .map(|(i, spk)| (i, spk.into()))
                .collect();
            let (update, last_active_index) =
                self.fetch_txs_with_keychain_spks(start_time, &mut inserted_txs, spks, STOP_GAP)?;
            tx_update.extend(update);
            if let Some(last_active_index) = last_active_index {
                last_active_indices.insert(keychain, last_active_index);
            }
        }

        let chain_update = match (chain_tip, latest_blocks) {
            (Some(chain_tip), Some(latest_blocks)) => {
                Some(self.chain_update(&latest_blocks, &chain_tip, &tx_update.anchors)?)
            }
            _ => None,
        };

        Ok(FullScanResponse {
            chain_update,
            tx_update,
            last_active_indices,
        })
    }

    pub fn sync(
        &self,
        request: impl Into<SyncRequest<(KeychainKind, u32)>>,
    ) -> Result<SyncResponse, ProviderError> {
        let mut request: SyncRequest<(KeychainKind, u32)> = request.into();
        let start_time = request.start_time();

        let chain_tip = request.chain_tip();
        let latest_blocks = if chain_tip.is_some() {
            Some(self.latest_blocks().map_err(|m| self.sync_err(m))?)
        } else {
            None
        };

        let mut tx_update = TxUpdate::<ConfirmationBlockTime>::default();
        let mut inserted_txs = HashSet::<Txid>::new();
        let spks: Vec<(u32, SpkWithExpectedTxids)> = request
            .iter_spks_with_expected_txids()
            .enumerate()
            .map(|(i, spk)| (i as u32, spk))
            .collect();
        let (spk_update, _) =
            self.fetch_txs_with_keychain_spks(start_time, &mut inserted_txs, spks, usize::MAX)?;
        tx_update.extend(spk_update);
        let txids: Vec<Txid> = request.iter_txids().collect();
        tx_update.extend(self.fetch_txs_with_txids(start_time, &mut inserted_txs, txids)?);
        let outpoints: Vec<OutPoint> = request.iter_outpoints().collect();
        tx_update.extend(self.fetch_txs_with_outpoints(
            start_time,
            &mut inserted_txs,
            outpoints,
        )?);

        let chain_update = match (chain_tip, latest_blocks) {
            (Some(chain_tip), Some(latest_blocks)) => {
                Some(self.chain_update(&latest_blocks, &chain_tip, &tx_update.anchors)?)
            }
            _ => None,
        };

        Ok(SyncResponse {
            chain_update,
            tx_update,
        })
    }

    fn fetch_txs_with_keychain_spks(
        &self,
        start_time: u64,
        inserted_txs: &mut HashSet<Txid>,
        spks: Vec<(u32, SpkWithExpectedTxids)>,
        stop_gap: usize,
    ) -> Result<(TxUpdate<ConfirmationBlockTime>, Option<u32>), ProviderError> {
        let mut update = TxUpdate::<ConfirmationBlockTime>::default();
        let mut last_active_index = Option::<u32>::None;
        let mut consecutive_unused = 0usize;
        let gap_limit = stop_gap.max(1);

        for (spk_index, spk) in spks {
            let mut last_txid = None;
            let mut spk_txs = Vec::new();
            loop {
                let txs = self
                    .scripthash_txs(&spk.spk, last_txid)
                    .map_err(|m| self.sync_err(m))?;
                let tx_count = txs.len();
                last_txid = txs.last().map(|tx| tx.txid);
                spk_txs.extend(txs);
                // Esplora pages confirmed txs 25 at a time.
                if tx_count < 25 {
                    break;
                }
            }
            let got_txids: HashSet<Txid> = spk_txs.iter().map(|tx| tx.txid).collect();
            let evicted_txids = spk.expected_txids.difference(&got_txids).copied();
            update
                .evicted_ats
                .extend(evicted_txids.map(|txid| (txid, start_time)));

            if spk_txs.is_empty() {
                consecutive_unused = consecutive_unused.saturating_add(1);
            } else {
                consecutive_unused = 0;
                last_active_index = Some(spk_index);
            }
            for tx in spk_txs {
                if inserted_txs.insert(tx.txid) {
                    update.txs.push(tx.to_tx().into());
                }
                insert_anchor_or_seen_at_from_status(&mut update, start_time, tx.txid, tx.status);
                insert_prevouts(&mut update, tx.vin);
            }

            if consecutive_unused >= gap_limit {
                break;
            }
        }

        Ok((update, last_active_index))
    }

    fn fetch_txs_with_txids(
        &self,
        start_time: u64,
        inserted_txs: &mut HashSet<Txid>,
        txids: impl IntoIterator<Item = Txid>,
    ) -> Result<TxUpdate<ConfirmationBlockTime>, ProviderError> {
        let mut update = TxUpdate::<ConfirmationBlockTime>::default();
        for txid in txids {
            if inserted_txs.contains(&txid) {
                continue;
            }
            let Some(tx_info) = self.tx_info(&txid).map_err(|m| self.sync_err(m))? else {
                continue;
            };
            if inserted_txs.insert(txid) {
                update.txs.push(tx_info.to_tx().into());
            }
            insert_anchor_or_seen_at_from_status(&mut update, start_time, txid, tx_info.status);
            insert_prevouts(&mut update, tx_info.vin);
        }
        Ok(update)
    }

    fn fetch_txs_with_outpoints(
        &self,
        start_time: u64,
        inserted_txs: &mut HashSet<Txid>,
        outpoints: Vec<OutPoint>,
    ) -> Result<TxUpdate<ConfirmationBlockTime>, ProviderError> {
        let mut update = TxUpdate::<ConfirmationBlockTime>::default();

        // Make sure txs exist in the graph and statuses are updated.
        update.extend(self.fetch_txs_with_txids(
            start_time,
            inserted_txs,
            outpoints.iter().map(|op| op.txid),
        )?);

        // Then the spend-status of each outpoint.
        let mut missing_txs = Vec::<Txid>::new();
        for op in outpoints {
            let Some(op_status) = self
                .output_status(&op.txid, op.vout)
                .map_err(|m| self.sync_err(m))?
            else {
                continue;
            };
            let Some(spend_txid) = op_status.txid else {
                continue;
            };
            if !inserted_txs.contains(&spend_txid) {
                missing_txs.push(spend_txid);
            }
            if let Some(spend_status) = op_status.status {
                insert_anchor_or_seen_at_from_status(
                    &mut update,
                    start_time,
                    spend_txid,
                    spend_status,
                );
            }
        }

        update.extend(self.fetch_txs_with_txids(start_time, inserted_txs, missing_txs)?);
        Ok(update)
    }

    /// Fetch a block hash without surpassing `latest_blocks` (the local tip
    /// signals last-synced-up-to-height; a later hash could skip blocks).
    fn fetch_block(
        &self,
        latest_blocks: &BTreeMap<u32, BlockHash>,
        height: u32,
    ) -> Result<Option<BlockHash>, ProviderError> {
        if let Some(&hash) = latest_blocks.get(&height) {
            return Ok(Some(hash));
        }
        match latest_blocks.keys().last().copied() {
            None => return Ok(None),
            Some(tip_height) if height > tip_height => return Ok(None),
            Some(_) => {}
        }
        Ok(Some(self.block_hash(height).map_err(|m| self.sync_err(m))?))
    }

    /// Build the chain update: find the point of agreement with the local
    /// checkpoint chain, then anchor and latest blocks on top.
    fn chain_update(
        &self,
        latest_blocks: &BTreeMap<u32, BlockHash>,
        local_tip: &CheckPoint,
        anchors: &BTreeSet<(ConfirmationBlockTime, Txid)>,
    ) -> Result<CheckPoint, ProviderError> {
        let mut point_of_agreement = None;
        let mut conflicts = vec![];
        for local_cp in local_tip.iter() {
            let remote_hash = match self.fetch_block(latest_blocks, local_cp.height())? {
                Some(hash) => hash,
                None => continue,
            };
            if remote_hash == local_cp.hash() {
                point_of_agreement = Some(local_cp);
                break;
            }
            conflicts.push(BlockId {
                height: local_cp.height(),
                hash: remote_hash,
            });
        }

        let mut tip = point_of_agreement
            .ok_or_else(|| self.sync_err("no point of agreement with the local chain"))?;
        // The conflict heights come from remote block data: a hostile or
        // broken endpoint must produce a sync error, never a panic.
        tip = tip
            .extend(conflicts.into_iter().rev())
            .map_err(|_| self.sync_err("provider served checkpoint conflicts out of order"))?;

        for (anchor, _) in anchors {
            let height = anchor.block_id.height;
            if tip.get(height).is_none() {
                let hash = match self.fetch_block(latest_blocks, height)? {
                    Some(hash) => hash,
                    None => continue,
                };
                tip = tip.insert(BlockId { height, hash });
            }
        }

        for (&height, &hash) in latest_blocks.iter() {
            tip = tip.insert(BlockId { height, hash });
        }

        Ok(tip)
    }
}

/// Anchor confirmed txs to their block; mark the rest seen-at now.
fn insert_anchor_or_seen_at_from_status(
    update: &mut TxUpdate<ConfirmationBlockTime>,
    start_time: u64,
    txid: Txid,
    status: TxStatus,
) {
    if let TxStatus {
        block_height: Some(height),
        block_hash: Some(hash),
        block_time: Some(time),
        ..
    } = status
    {
        let anchor = ConfirmationBlockTime {
            block_id: BlockId { height, hash },
            confirmation_time: time,
        };
        update.anchors.insert((anchor, txid));
    } else {
        update.seen_ats.insert((txid, start_time));
    }
}

/// Record the previous outputs esplora-style Vins carry, so the wallet can
/// compute fees for foreign inputs.
fn insert_prevouts(
    update: &mut TxUpdate<ConfirmationBlockTime>,
    inputs: impl IntoIterator<Item = Vin>,
) {
    let prevouts = inputs
        .into_iter()
        .filter_map(|vin| Some((vin.txid, vin.vout, vin.prevout?)));
    for (prev_txid, prev_vout, prev_txout) in prevouts {
        update.txouts.insert(
            OutPoint::new(prev_txid, prev_vout),
            TxOut {
                script_pubkey: prev_txout.scriptpubkey,
                value: Amount::from_sat(prev_txout.value),
            },
        );
    }
}

/// Unwrap a JSON-RPC 2.0 envelope. A JSON-RPC error is an error string;
/// a missing result is too.
fn parse_jsonrpc<T: DeserializeOwned>(body: &str) -> Result<T, String> {
    let envelope: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("invalid json-rpc response: {e}"))?;
    if let Some(error) = envelope.get("error").filter(|e| !e.is_null()) {
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(format!("rpc error {code}: {message}"));
    }
    let result = envelope
        .get("result")
        .ok_or_else(|| "json-rpc response has no result".to_string())?;
    serde_json::from_value(result.clone()).map_err(|e| format!("unexpected result shape: {e}"))
}

/// An ord `output` result marks the outpoint protected iff it lists any
/// inscriptions or runes. Opaque to sats: no decoding, only presence.
fn parse_ord_output(value: &serde_json::Value) -> bool {
    let has_inscriptions = value
        .get("inscriptions")
        .and_then(|i| i.as_array())
        .is_some_and(|a| !a.is_empty());
    let has_runes = match value.get("runes") {
        Some(serde_json::Value::Array(a)) => !a.is_empty(),
        Some(serde_json::Value::Object(o)) => !o.is_empty(),
        _ => false,
    };
    has_inscriptions || has_runes
}

/// An alkanes by-outpoint result marks the outpoint protected iff it
/// reports any balance entries.
fn parse_alkanes_outpoint(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => match o.get("balances") {
            Some(serde_json::Value::Array(a)) => !a.is_empty(),
            Some(serde_json::Value::Object(inner)) => !inner.is_empty(),
            Some(serde_json::Value::Null) | None => !o.is_empty(),
            Some(_) => true,
        },
        _ => false,
    }
}

/// A bytecode result is a hex string, with or without a 0x prefix. An
/// empty result means nothing is deployed at that id.
fn parse_bytecode_result(value: &serde_json::Value) -> Result<Vec<u8>, String> {
    let hex_str = value
        .as_str()
        .ok_or_else(|| "expected a hex string result".to_string())?;
    let stripped = hex_str
        .trim()
        .strip_prefix("0x")
        .or_else(|| hex_str.trim().strip_prefix("0X"))
        .unwrap_or(hex_str.trim());
    if stripped.is_empty() {
        return Err("no bytecode at this alkane id".to_string());
    }
    hex::decode(stripped).map_err(|e| format!("invalid bytecode hex: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonrpc_result_unwraps() {
        let ok: HashMap<u16, f64> =
            parse_jsonrpc(r#"{"jsonrpc":"2.0","id":0,"result":{"2":3.5}}"#).unwrap();
        assert_eq!(ok.get(&2), Some(&3.5));
    }

    #[test]
    fn jsonrpc_error_is_reported() {
        let err = parse_jsonrpc::<serde_json::Value>(
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"method not found"}}"#,
        )
        .unwrap_err();
        assert!(err.contains("-32601"));
        assert!(err.contains("method not found"));
    }

    #[test]
    fn jsonrpc_null_error_is_not_an_error() {
        let ok: String =
            parse_jsonrpc(r#"{"jsonrpc":"2.0","id":0,"error":null,"result":"abc"}"#).unwrap();
        assert_eq!(ok, "abc");
    }

    #[test]
    fn ord_output_protection() {
        let empty: serde_json::Value =
            serde_json::from_str(r#"{"inscriptions":[],"runes":{}}"#).unwrap();
        assert!(!parse_ord_output(&empty));
        let inscribed: serde_json::Value =
            serde_json::from_str(r#"{"inscriptions":["abc123i0"],"runes":{}}"#).unwrap();
        assert!(parse_ord_output(&inscribed));
        let runic: serde_json::Value = serde_json::from_str(
            r#"{"inscriptions":[],"runes":{"UNCOMMON•GOODS":{"amount":420}}}"#,
        )
        .unwrap();
        assert!(parse_ord_output(&runic));
        let bare: serde_json::Value = serde_json::from_str("{}").unwrap();
        assert!(!parse_ord_output(&bare));
    }

    #[test]
    fn alkanes_outpoint_protection() {
        assert!(!parse_alkanes_outpoint(&serde_json::Value::Null));
        let empty: serde_json::Value = serde_json::from_str("[]").unwrap();
        assert!(!parse_alkanes_outpoint(&empty));
        let some: serde_json::Value =
            serde_json::from_str(r#"[{"token":{"id":"2:0"},"value":"1000"}]"#).unwrap();
        assert!(parse_alkanes_outpoint(&some));
        let object: serde_json::Value =
            serde_json::from_str(r#"{"balances":[{"id":"2:0"}]}"#).unwrap();
        assert!(parse_alkanes_outpoint(&object));
        let object_empty: serde_json::Value = serde_json::from_str(r#"{"balances":[]}"#).unwrap();
        assert!(!parse_alkanes_outpoint(&object_empty));
    }

    #[test]
    fn secrets_never_appear_in_errors() {
        let client = SubfrostClient::new("https://mainnet.subfrost.io/v4/SECRETKEY/jsonrpc".into());
        assert_eq!(client.display_url(), "https://mainnet.subfrost.io");
        assert!(!format!("{client:?}").contains("SECRETKEY"));
        let scrubbed = client
            .scrub("error connecting to https://mainnet.subfrost.io/v4/SECRETKEY/jsonrpc: refused");
        assert!(!scrubbed.contains("SECRETKEY"));
        let guard_err = client.guard_err(
            "ord",
            client.scrub("boom at https://mainnet.subfrost.io/v4/SECRETKEY/jsonrpc"),
        );
        assert!(!guard_err.to_string().contains("SECRETKEY"));
        let view_err = client
            .view_err(client.scrub("boom at https://mainnet.subfrost.io/v4/SECRETKEY/jsonrpc"));
        assert!(!view_err.to_string().contains("SECRETKEY"));
    }

    #[test]
    fn bytecode_results_decode_or_fail_typed() {
        let with_prefix = serde_json::json!("0x0061736d");
        assert_eq!(parse_bytecode_result(&with_prefix).unwrap(), b"\0asm");
        let bare = serde_json::json!("0061736d");
        assert_eq!(parse_bytecode_result(&bare).unwrap(), b"\0asm");
        assert!(parse_bytecode_result(&serde_json::json!("")).is_err());
        assert!(parse_bytecode_result(&serde_json::json!("0x")).is_err());
        assert!(parse_bytecode_result(&serde_json::json!(null)).is_err());
        assert!(parse_bytecode_result(&serde_json::json!("zz")).is_err());
    }
}
