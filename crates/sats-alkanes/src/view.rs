//! Requests for the indexer's view functions, called through
//! `metashrew_view(name, "0x" + hex(request), "latest")`.
//!
//! A view request is a protobuf message (alkanes-rs
//! `crates/alkanes-support/proto/alkanes.proto`): `uint128 { uint64 lo = 1;
//! uint64 hi = 2; }`, `AlkaneId { uint128 block = 1; uint128 tx = 2; }`,
//! and `BytecodeRequest { AlkaneId id = 1; }`. Proto3 omits zero scalars,
//! but every message field is written, even when empty: the indexer
//! unwraps them.
//!
//! Balances come from `protorunesbyoutpoint`
//! (`crates/protorune-support/proto/protorune.proto`):
//! `OutpointWithProtocol { bytes txid = 1; uint32 vout = 2; uint128
//! protocol = 3; }` in, `OutpointResponse { BalanceSheet balances = 1;
//! Outpoint outpoint = 2; Output output = 3; … }` out. The indexer
//! answers an outpoint it doesn't know with an *empty* response rather
//! than an error, so a caller must check the echoed outpoint and output
//! before reading "no balances" as "no assets".

use sats_core::bitcoin::hashes::Hash;
use sats_core::bitcoin::{OutPoint, Txid};

use crate::id::AlkaneId;
use crate::protostone::ALKANES_PROTOCOL_TAG;
use crate::varint;

/// Wire type 0: a varint scalar.
pub(crate) fn put_varint(field: u32, value: u64, out: &mut Vec<u8>) {
    if value != 0 {
        varint::encode_to_vec(u128::from(field << 3), out);
        varint::encode_to_vec(u128::from(value), out);
    }
}

/// Wire type 2: a length-delimited nested message.
pub(crate) fn put_message(field: u32, body: &[u8], out: &mut Vec<u8>) {
    varint::encode_to_vec(u128::from(field << 3 | 2), out);
    varint::encode_to_vec(body.len() as u128, out);
    out.extend_from_slice(body);
}

fn uint128(value: u128) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(1, value as u64, &mut out);
    put_varint(2, (value >> 64) as u64, &mut out);
    out
}

fn alkane_id(block: u128, tx: u128) -> Vec<u8> {
    let mut out = Vec::new();
    put_message(1, &uint128(block), &mut out);
    put_message(2, &uint128(tx), &mut out);
    out
}

/// The `getbytecode` view's request for one alkane id.
pub fn bytecode_request(block: u128, tx: u128) -> Vec<u8> {
    let mut out = Vec::new();
    put_message(1, &alkane_id(block, tx), &mut out);
    out
}

/// The `protorunesbyoutpoint` view's request for one outpoint's alkanes.
pub fn outpoint_request(outpoint: &OutPoint) -> Vec<u8> {
    let mut out = Vec::new();
    // The indexer reads the txid in its internal byte order.
    put_message(1, outpoint.txid.as_byte_array(), &mut out);
    put_varint(2, u64::from(outpoint.vout), &mut out);
    put_message(3, &uint128(ALKANES_PROTOCOL_TAG), &mut out);
    out
}

/// One asset held by an outpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetBalance {
    pub id: AlkaneId,
    /// In the asset's base units.
    pub amount: u128,
    /// Display labels from the indexer; never identity.
    pub name: String,
    pub symbol: String,
}

/// What the indexer says about one outpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutpointAssets {
    /// The outpoint the indexer echoed; `None` when it doesn't know it.
    pub outpoint: Option<OutPoint>,
    /// The output value the indexer recorded; `None` when absent.
    pub value_sat: Option<u64>,
    pub balances: Vec<AssetBalance>,
}

/// Decode an `OutpointResponse`. Unknown fields are skipped, as protobuf
/// requires; malformed framing is an error.
pub fn parse_outpoint_response(bytes: &[u8]) -> Result<OutpointAssets, String> {
    let mut result = OutpointAssets::default();
    for field in Fields::new(bytes) {
        match field? {
            (1, Value::Bytes(sheet)) => {
                for entry in Fields::new(sheet) {
                    if let (1, Value::Bytes(item)) = entry? {
                        result.balances.push(parse_balance_item(item)?);
                    }
                }
            }
            (2, Value::Bytes(outpoint)) => result.outpoint = Some(parse_outpoint(outpoint)?),
            (3, Value::Bytes(output)) => {
                let mut value = 0;
                for f in Fields::new(output) {
                    if let (2, Value::Varint(v)) = f? {
                        value = u64::try_from(v).map_err(|_| "output value overflows")?;
                    }
                }
                result.value_sat = Some(value);
            }
            _ => {}
        }
    }
    Ok(result)
}

fn parse_outpoint(bytes: &[u8]) -> Result<OutPoint, String> {
    let mut txid = None;
    let mut vout = 0u32;
    for field in Fields::new(bytes) {
        match field? {
            (1, Value::Bytes(raw)) => {
                let array: [u8; 32] = raw.try_into().map_err(|_| "txid is not 32 bytes")?;
                txid = Some(Txid::from_byte_array(array));
            }
            (2, Value::Varint(v)) => vout = u32::try_from(v).map_err(|_| "vout overflows")?,
            _ => {}
        }
    }
    Ok(OutPoint {
        txid: txid.ok_or("outpoint without a txid")?,
        vout,
    })
}

fn parse_balance_item(bytes: &[u8]) -> Result<AssetBalance, String> {
    let mut id = None;
    let mut amount = 0u128;
    let mut name = String::new();
    let mut symbol = String::new();
    for field in Fields::new(bytes) {
        match field? {
            (1, Value::Bytes(rune)) => {
                for f in Fields::new(rune) {
                    match f? {
                        (1, Value::Bytes(rune_id)) => {
                            let (mut block, mut tx) = (0, 0);
                            for g in Fields::new(rune_id) {
                                match g? {
                                    (1, Value::Bytes(v)) => block = parse_uint128(v)?,
                                    (2, Value::Bytes(v)) => tx = parse_uint128(v)?,
                                    _ => {}
                                }
                            }
                            id = Some(AlkaneId { block, tx });
                        }
                        (2, Value::Bytes(v)) => name = String::from_utf8_lossy(v).into_owned(),
                        (5, Value::Bytes(v)) => symbol = String::from_utf8_lossy(v).into_owned(),
                        _ => {}
                    }
                }
            }
            (2, Value::Bytes(v)) => amount = parse_uint128(v)?,
            _ => {}
        }
    }
    Ok(AssetBalance {
        id: id.ok_or("balance entry without an asset id")?,
        amount,
        name,
        symbol,
    })
}

fn parse_uint128(bytes: &[u8]) -> Result<u128, String> {
    let (mut lo, mut hi) = (0u128, 0u128);
    for field in Fields::new(bytes) {
        match field? {
            (1, Value::Varint(v)) => lo = v,
            (2, Value::Varint(v)) => hi = v,
            _ => {}
        }
    }
    if lo > u128::from(u64::MAX) || hi > u128::from(u64::MAX) {
        return Err("uint128 half overflows u64".into());
    }
    Ok(lo | hi << 64)
}

pub(crate) enum Value<'a> {
    Varint(u128),
    Bytes(&'a [u8]),
    Fixed,
}

/// A minimal protobuf field reader: `(field number, value)` pairs.
pub(crate) struct Fields<'a> {
    rest: &'a [u8],
    failed: bool,
}

impl<'a> Fields<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Fields {
            rest: bytes,
            failed: false,
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.rest.len() < n {
            return Err("truncated protobuf".into());
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    fn varint(&mut self) -> Result<u128, String> {
        let (value, used) = varint::decode(self.rest).map_err(String::from)?;
        self.rest = &self.rest[used..];
        Ok(value)
    }

    fn field(&mut self) -> Result<(u128, Value<'a>), String> {
        let key = self.varint()?;
        let value = match key & 7 {
            0 => Value::Varint(self.varint()?),
            1 => {
                self.take(8)?;
                Value::Fixed
            }
            2 => {
                let len = usize::try_from(self.varint()?).map_err(|_| "length overflows")?;
                Value::Bytes(self.take(len)?)
            }
            5 => {
                self.take(4)?;
                Value::Fixed
            }
            other => return Err(format!("unsupported protobuf wire type {other}")),
        };
        Ok((key >> 3, value))
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u128, Value<'a>), String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.rest.is_empty() {
            return None;
        }
        let field = self.field();
        self.failed = field.is_err();
        Some(field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frozen against a live signet indexer: this request returned the
    /// bytecode of 2:0.
    #[test]
    fn bytecode_request_matches_the_indexer() {
        assert_eq!(hex::encode(bytecode_request(2, 0)), "0a060a0208021200");
    }

    #[test]
    fn multi_byte_ids_and_the_high_half_are_encoded() {
        // 32 → 08 20; 300 → 08 ac 02 (two varint bytes).
        assert_eq!(
            hex::encode(bytecode_request(32, 300)),
            "0a090a020820120308ac02"
        );
        // 2^64 + 1 → lo 1 (08 01), hi 1 (10 01); tx 0 stays present, empty.
        assert_eq!(
            hex::encode(bytecode_request(u128::from(u64::MAX) + 2, 0)),
            "0a080a04080110011200"
        );
    }

    fn txid(byte: u8) -> Txid {
        Txid::from_byte_array([byte; 32])
    }

    /// An `OutpointResponse` as the indexer would encode it.
    fn response(outpoint: &OutPoint, value: u64, entries: &[(u128, u128, u128, &str)]) -> Vec<u8> {
        let mut sheet = Vec::new();
        for (block, tx, amount, name) in entries {
            let mut rune_id = Vec::new();
            put_message(1, &uint128(*block), &mut rune_id);
            put_message(2, &uint128(*tx), &mut rune_id);
            let mut rune = Vec::new();
            put_message(1, &rune_id, &mut rune);
            put_message(2, name.as_bytes(), &mut rune);
            put_varint(3, 8, &mut rune);
            put_message(5, b"SYM", &mut rune);
            let mut item = Vec::new();
            put_message(1, &rune, &mut item);
            put_message(2, &uint128(*amount), &mut item);
            put_message(1, &item, &mut sheet);
        }
        let mut op = Vec::new();
        put_message(1, outpoint.txid.as_byte_array(), &mut op);
        put_varint(2, u64::from(outpoint.vout), &mut op);
        let mut output = Vec::new();
        put_message(1, &[0x51, 0x20], &mut output);
        put_varint(2, value, &mut output);
        let mut out = Vec::new();
        put_message(1, &sheet, &mut out);
        put_message(2, &op, &mut out);
        put_message(3, &output, &mut out);
        put_varint(4, 900, &mut out);
        out
    }

    #[test]
    fn outpoint_request_encodes_txid_vout_and_the_alkanes_tag() {
        let outpoint = OutPoint {
            txid: txid(0xab),
            vout: 3,
        };
        let mut expected = String::from("0a20");
        expected.push_str(&"ab".repeat(32));
        expected.push_str("1003"); // vout 3
        expected.push_str("1a020801"); // protocol { lo: 1 }
        assert_eq!(hex::encode(outpoint_request(&outpoint)), expected);
    }

    #[test]
    fn response_with_balances_parses() {
        let outpoint = OutPoint {
            txid: txid(7),
            vout: 1,
        };
        let big = u128::from(u64::MAX) + 5;
        let bytes = response(
            &outpoint,
            546,
            &[(32, 0, 150_000, "frBTC"), (2, 9, big, "")],
        );
        let parsed = parse_outpoint_response(&bytes).unwrap();
        assert_eq!(parsed.outpoint, Some(outpoint));
        assert_eq!(parsed.value_sat, Some(546));
        assert_eq!(parsed.balances.len(), 2);
        assert_eq!(parsed.balances[0].id, AlkaneId { block: 32, tx: 0 });
        assert_eq!(parsed.balances[0].amount, 150_000);
        assert_eq!(parsed.balances[0].name, "frBTC");
        assert_eq!(parsed.balances[0].symbol, "SYM");
        assert_eq!(parsed.balances[1].amount, big);
    }

    #[test]
    fn an_unknown_outpoint_is_an_empty_response() {
        // The indexer's default response: nothing echoed, nothing held.
        let parsed = parse_outpoint_response(&[]).unwrap();
        assert_eq!(parsed.outpoint, None);
        assert_eq!(parsed.value_sat, None);
        assert!(parsed.balances.is_empty());
    }

    #[test]
    fn truncated_responses_are_errors() {
        let outpoint = OutPoint {
            txid: txid(7),
            vout: 1,
        };
        let bytes = response(&outpoint, 546, &[(32, 0, 1, "x")]);
        // Cut inside the output message (the last 3 bytes are a whole field).
        assert!(parse_outpoint_response(&bytes[..bytes.len() - 5]).is_err());
        assert!(parse_outpoint_response(&[0x0a, 0x05, 0x01]).is_err());
    }
}
