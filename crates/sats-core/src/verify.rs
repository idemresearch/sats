//! Independent verification of what a PSBT actually does.
//!
//! This is the boundary that makes agent policy enforcing rather than
//! advisory. A caller that prepares a transaction also controls every
//! number it could report about that transaction, so the authorization
//! path must never accept a claimed amount or fee. [`derive_intent`]
//! recomputes both from the PSBT itself, using only the wallet's own
//! descriptors.
//!
//! PURE — no filesystem, network, or clock.
//!
//! # What is trusted
//!
//! An output belongs to this wallet exactly when some derivation index
//! of one of its two descriptors reproduces that output's script. The
//! PSBT's own key-origin metadata is used only as a *hint* for which
//! index to try; the answer comes from re-deriving and comparing scripts,
//! so a caller cannot make a foreign script look like change by lying
//! about its origin. Withholding the hint is possible, and fails safe: an
//! unhinted output counts as a payment, which can only raise `sats_out`.
//!
//! Input values come from each input's `witness_utxo`, which the caller
//! also supplies. That is safe for the single-sig taproot wallets sats
//! creates: the BIP-341 sighash commits to every input's amount and
//! script, so understating an input to fake a small fee yields a
//! signature that is invalid against the real UTXO. The transaction
//! cannot relay, so the attack reduces to burning the caller's own
//! reserved budget rather than evading the fee cap.

use bdk_wallet::bitcoin::bip32::{ChildNumber, DerivationPath};
use bdk_wallet::bitcoin::{Address, Network, Psbt, ScriptBuf};
use bdk_wallet::descriptor::ExtendedDescriptor;

use crate::error::VerifyError;

/// Most derivation hints considered per input or output. A single-sig
/// taproot script has exactly one; the cap bounds the work a hostile
/// PSBT can ask for, since each candidate costs a derivation.
const MAX_HINTS: usize = 8;

/// Which keychain a script derived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owned {
    /// The external (receive) keychain.
    External,
    /// The internal (change) keychain.
    Internal,
}

/// What a PSBT does, according to the wallet rather than the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedIntent {
    /// Outputs that do not derive from the internal keychain, in
    /// transaction order: `(address, sats)`.
    pub recipients: Vec<(String, u64)>,
    /// Total sats leaving the wallet, excluding the fee.
    pub sats_out: u64,
    /// Total sats returning as change.
    pub change_sat: u64,
    /// `sum(input values) - sum(output values)`.
    pub fee_sat: u64,
    /// Total value of the inputs being spent.
    pub input_sat: u64,
}

impl DerivedIntent {
    /// What this transaction draws from a budget: sats out plus fee.
    pub fn total_sat(&self) -> u64 {
        self.sats_out.saturating_add(self.fee_sat)
    }

    /// The single recipient of an ordinary send.
    ///
    /// Every transaction sats prepares pays exactly one recipient plus
    /// optional change. Anything else is refused rather than summed,
    /// so an extra output can never be folded into an amount a human
    /// already approved.
    pub fn sole_recipient(&self) -> Result<(&str, u64), VerifyError> {
        match self.recipients.as_slice() {
            [(address, amount)] => Ok((address.as_str(), *amount)),
            [] => Err(VerifyError::NoRecipient),
            many => Err(VerifyError::MultipleRecipients(many.len())),
        }
    }
}

/// Recompute a PSBT's payments and fee from the wallet's descriptors.
///
/// Every input must derive from this wallet: a transaction with a
/// foreign input is refused rather than signed, because its fee cannot
/// be attributed and its other inputs are not ours to spend.
pub fn derive_intent(
    psbt: &Psbt,
    external: &ExtendedDescriptor,
    internal: &ExtendedDescriptor,
    network: Network,
) -> Result<DerivedIntent, VerifyError> {
    let tx = &psbt.unsigned_tx;
    if tx.input.is_empty() {
        return Err(VerifyError::NoInputs);
    }
    if psbt.inputs.len() != tx.input.len() || psbt.outputs.len() != tx.output.len() {
        return Err(VerifyError::Malformed(
            "psbt maps do not match the unsigned transaction".into(),
        ));
    }

    let mut input_sat: u64 = 0;
    for (index, input) in psbt.inputs.iter().enumerate() {
        let txout = input
            .witness_utxo
            .as_ref()
            .ok_or(VerifyError::MissingWitnessUtxo(index))?;
        // Refuse anything we cannot prove is ours. Signing a foreign
        // input would spend someone else's coin and make the fee — and
        // therefore every cap that depends on it — meaningless.
        if owner(
            &txout.script_pubkey,
            &hints(&input.tap_key_origins, &input.bip32_derivation),
            external,
            internal,
        )
        .is_none()
        {
            return Err(VerifyError::ForeignInput(index));
        }
        input_sat = input_sat
            .checked_add(txout.value.to_sat())
            .ok_or(VerifyError::ValueOverflow)?;
    }

    let mut output_sat: u64 = 0;
    let mut sats_out: u64 = 0;
    let mut change_sat: u64 = 0;
    let mut recipients = Vec::new();
    for (index, txout) in tx.output.iter().enumerate() {
        let value = txout.value.to_sat();
        output_sat = output_sat
            .checked_add(value)
            .ok_or(VerifyError::ValueOverflow)?;

        let map = &psbt.outputs[index];
        let owned = owner(
            &txout.script_pubkey,
            &hints(&map.tap_key_origins, &map.bip32_derivation),
            external,
            internal,
        );
        // Only the internal keychain is change. An output paying one of
        // our own receive addresses stays a payment, which keeps
        // `sats_out` equal to what a human or agent asked to send.
        if owned == Some(Owned::Internal) {
            change_sat = change_sat
                .checked_add(value)
                .ok_or(VerifyError::ValueOverflow)?;
            continue;
        }
        let address = Address::from_script(&txout.script_pubkey, network)
            .map_err(|_| VerifyError::UndecodableOutput(index))?;
        recipients.push((address.to_string(), value));
        sats_out = sats_out
            .checked_add(value)
            .ok_or(VerifyError::ValueOverflow)?;
    }

    let fee_sat = input_sat
        .checked_sub(output_sat)
        .ok_or(VerifyError::OutputsExceedInputs)?;

    Ok(DerivedIntent {
        recipients,
        sats_out,
        change_sat,
        fee_sat,
        input_sat,
    })
}

/// Candidate derivation indexes suggested by a PSBT's key origins. The
/// values are untrusted: every candidate is checked by re-deriving.
fn hints<A, B>(
    tap: &std::collections::BTreeMap<
        A,
        (
            Vec<B>,
            (bdk_wallet::bitcoin::bip32::Fingerprint, DerivationPath),
        ),
    >,
    bip32: &std::collections::BTreeMap<
        bdk_wallet::bitcoin::secp256k1::PublicKey,
        (bdk_wallet::bitcoin::bip32::Fingerprint, DerivationPath),
    >,
) -> Vec<u32> {
    let paths = tap
        .values()
        .map(|(_, source)| &source.1)
        .chain(bip32.values().map(|source| &source.1));
    let mut out = Vec::new();
    for path in paths {
        if out.len() >= MAX_HINTS {
            break;
        }
        if let Some(ChildNumber::Normal { index }) = path.as_ref().last()
            && !out.contains(index)
        {
            out.push(*index);
        }
    }
    out
}

/// Which keychain, if either, produces this script at one of the hinted
/// indexes. The hint chooses what to try; the script comparison decides.
fn owner(
    script: &ScriptBuf,
    hints: &[u32],
    external: &ExtendedDescriptor,
    internal: &ExtendedDescriptor,
) -> Option<Owned> {
    for &index in hints {
        // Internal first: misattributing change as a payment is safe,
        // the reverse is not, so the change keychain gets the first look.
        if spk_at(internal, index).as_ref() == Some(script) {
            return Some(Owned::Internal);
        }
        if spk_at(external, index).as_ref() == Some(script) {
            return Some(Owned::External);
        }
    }
    None
}

fn spk_at(descriptor: &ExtendedDescriptor, index: u32) -> Option<ScriptBuf> {
    descriptor
        .at_derivation_index(index)
        .ok()
        .map(|derived| derived.script_pubkey())
}

#[cfg(test)]
mod tests {
    use bdk_wallet::bitcoin::hashes::Hash;
    use bdk_wallet::bitcoin::{Amount, BlockHash, FeeRate, TxOut};
    use bdk_wallet::chain::{BlockId, ConfirmationBlockTime};
    use bdk_wallet::test_utils::{insert_checkpoint, receive_output};
    use bdk_wallet::{KeychainKind, Wallet};

    use super::*;
    use crate::engine::build_plan;
    use crate::seed;

    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const OTHER: &str = "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo wrong";

    fn funded_wallet(mnemonic: &str) -> Wallet {
        let mnemonic = seed::parse_mnemonic(mnemonic).unwrap();
        let (ext, int) = seed::public_descriptors(&mnemonic, Network::Signet).unwrap();
        let mut wallet = Wallet::create(ext, int)
            .network(Network::Signet)
            .create_wallet_no_persist()
            .unwrap();
        let block = BlockId {
            height: 1000,
            hash: BlockHash::all_zeros(),
        };
        insert_checkpoint(&mut wallet, block);
        insert_checkpoint(
            &mut wallet,
            BlockId {
                height: 2000,
                hash: BlockHash::all_zeros(),
            },
        );
        receive_output(
            &mut wallet,
            Amount::from_sat(200_000),
            ConfirmationBlockTime {
                block_id: block,
                confirmation_time: 100,
            },
        );
        wallet
    }

    fn descriptors(wallet: &Wallet) -> (ExtendedDescriptor, ExtendedDescriptor) {
        (
            wallet.public_descriptor(KeychainKind::External).clone(),
            wallet.public_descriptor(KeychainKind::Internal).clone(),
        )
    }

    /// A recipient the wallet does not own, from an unrelated seed.
    fn foreign_address() -> Address {
        let mnemonic = seed::parse_mnemonic(OTHER).unwrap();
        let (ext, int) = seed::public_descriptors(&mnemonic, Network::Signet).unwrap();
        let mut other = Wallet::create(ext, int)
            .network(Network::Signet)
            .create_wallet_no_persist()
            .unwrap();
        other.reveal_next_address(KeychainKind::External).address
    }

    fn plan(wallet: &mut Wallet, to: &Address, sats: u64) -> Psbt {
        build_plan(
            wallet,
            to,
            Amount::from_sat(sats),
            FeeRate::from_sat_per_vb_u32(2),
            &[],
            "signet",
            1_700_000_000,
        )
        .unwrap()
        .psbt()
        .clone()
    }

    /// The baseline: an honest PSBT derives to exactly what was asked
    /// for, with change recognized and the fee recomputed.
    #[test]
    fn honest_send_derives_its_own_amount_and_fee() {
        let mut wallet = funded_wallet(MNEMONIC);
        let (ext, int) = descriptors(&wallet);
        let to = foreign_address();
        let psbt = plan(&mut wallet, &to, 25_000);

        let derived = derive_intent(&psbt, &ext, &int, Network::Signet).unwrap();
        let (address, amount) = derived.sole_recipient().unwrap();
        assert_eq!(address, to.to_string());
        assert_eq!(amount, 25_000);
        assert_eq!(derived.sats_out, 25_000);
        assert!(derived.change_sat > 0, "expected change back");
        assert!(derived.fee_sat > 0);
        assert_eq!(
            derived.input_sat,
            derived.sats_out + derived.change_sat + derived.fee_sat
        );
        assert_eq!(derived.total_sat(), 25_000 + derived.fee_sat);
    }

    /// The attack the boundary exists to stop: a caller appends a second
    /// output. It must raise `sats_out`, not slip through.
    #[test]
    fn an_appended_output_is_counted_not_ignored() {
        let mut wallet = funded_wallet(MNEMONIC);
        let (ext, int) = descriptors(&wallet);
        let to = foreign_address();
        let mut psbt = plan(&mut wallet, &to, 25_000);
        let honest = derive_intent(&psbt, &ext, &int, Network::Signet).unwrap();

        // Steal 10_000 sats out of the change output into a new one.
        let change_index = psbt
            .unsigned_tx
            .output
            .iter()
            .position(|o| o.value.to_sat() == honest.change_sat)
            .expect("change output");
        psbt.unsigned_tx.output[change_index].value -= Amount::from_sat(10_000);
        psbt.unsigned_tx.output.push(TxOut {
            value: Amount::from_sat(10_000),
            script_pubkey: foreign_address().script_pubkey(),
        });
        psbt.outputs.push(Default::default());

        let tampered = derive_intent(&psbt, &ext, &int, Network::Signet).unwrap();
        assert_eq!(tampered.sats_out, 35_000, "both payments must be counted");
        assert_eq!(tampered.recipients.len(), 2);
        assert_eq!(tampered.fee_sat, honest.fee_sat, "fee is unchanged");
        // And an ordinary send refuses to proceed on an ambiguous shape.
        assert!(matches!(
            tampered.sole_recipient(),
            Err(VerifyError::MultipleRecipients(2))
        ));
    }

    /// Change must be recognized through the descriptor, not the label:
    /// stripping the PSBT's origin hint may only make an output look
    /// *more* like a payment.
    #[test]
    fn stripping_the_change_hint_fails_safe() {
        let mut wallet = funded_wallet(MNEMONIC);
        let (ext, int) = descriptors(&wallet);
        let psbt_original = plan(&mut wallet, &foreign_address(), 25_000);
        let honest = derive_intent(&psbt_original, &ext, &int, Network::Signet).unwrap();

        let mut psbt = psbt_original.clone();
        for out in &mut psbt.outputs {
            out.tap_key_origins.clear();
            out.bip32_derivation.clear();
        }
        let stripped = derive_intent(&psbt, &ext, &int, Network::Signet).unwrap();
        assert_eq!(stripped.change_sat, 0);
        assert_eq!(
            stripped.sats_out,
            honest.sats_out + honest.change_sat,
            "unhinted change counts against the budget, never for it"
        );
        assert!(stripped.sats_out > honest.sats_out);
    }

    /// A forged origin hint cannot promote a foreign script to change:
    /// the index is only a suggestion, the derivation is the proof.
    #[test]
    fn a_forged_change_hint_is_not_believed() {
        let mut wallet = funded_wallet(MNEMONIC);
        let (ext, int) = descriptors(&wallet);
        let to = foreign_address();
        let mut psbt = plan(&mut wallet, &to, 25_000);

        // Copy the real change output's origin metadata onto the payment.
        let honest = derive_intent(&psbt, &ext, &int, Network::Signet).unwrap();
        let change_index = psbt
            .unsigned_tx
            .output
            .iter()
            .position(|o| o.value.to_sat() == honest.change_sat)
            .expect("change output");
        let payment_index = 1 - change_index;
        psbt.outputs[payment_index] = psbt.outputs[change_index].clone();

        let forged = derive_intent(&psbt, &ext, &int, Network::Signet).unwrap();
        assert_eq!(
            forged.sats_out, 25_000,
            "the payment is still a payment despite the borrowed hint"
        );
        assert_eq!(forged.change_sat, honest.change_sat);
    }

    /// Paying our own receive address stays a payment: conservative, and
    /// it keeps `sats_out` equal to what the caller asked to send.
    #[test]
    fn our_own_receive_address_is_a_payment_not_change() {
        let mut wallet = funded_wallet(MNEMONIC);
        let (ext, int) = descriptors(&wallet);
        let mine = wallet.reveal_next_address(KeychainKind::External).address;
        let psbt = plan(&mut wallet, &mine, 25_000);

        let derived = derive_intent(&psbt, &ext, &int, Network::Signet).unwrap();
        assert_eq!(derived.sats_out, 25_000);
        let (address, _) = derived.sole_recipient().unwrap();
        assert_eq!(address, mine.to_string());
    }

    /// An input we cannot prove is ours is refused, not priced.
    #[test]
    fn a_foreign_input_is_refused() {
        let mut wallet = funded_wallet(MNEMONIC);
        let (ext, int) = descriptors(&wallet);
        let mut psbt = plan(&mut wallet, &foreign_address(), 25_000);

        psbt.inputs[0].tap_key_origins.clear();
        psbt.inputs[0].bip32_derivation.clear();
        assert!(matches!(
            derive_intent(&psbt, &ext, &int, Network::Signet),
            Err(VerifyError::ForeignInput(0))
        ));

        // A different wallet's descriptors reach the same verdict.
        let other = funded_wallet(OTHER);
        let (oext, oint) = descriptors(&other);
        let psbt = plan(&mut wallet, &foreign_address(), 25_000);
        assert!(matches!(
            derive_intent(&psbt, &oext, &oint, Network::Signet),
            Err(VerifyError::ForeignInput(0))
        ));
    }

    /// Fee arithmetic must fail closed on impossible inputs rather than
    /// wrapping into a small, cap-passing number.
    #[test]
    fn fee_arithmetic_fails_closed() {
        let mut wallet = funded_wallet(MNEMONIC);
        let (ext, int) = descriptors(&wallet);

        let mut psbt = plan(&mut wallet, &foreign_address(), 25_000);
        psbt.inputs[0].witness_utxo = None;
        assert!(matches!(
            derive_intent(&psbt, &ext, &int, Network::Signet),
            Err(VerifyError::MissingWitnessUtxo(0))
        ));

        // Outputs worth more than the inputs is not a negative fee.
        let mut psbt = plan(&mut wallet, &foreign_address(), 25_000);
        psbt.unsigned_tx.output[0].value = Amount::from_sat(10_000_000);
        assert!(matches!(
            derive_intent(&psbt, &ext, &int, Network::Signet),
            Err(VerifyError::OutputsExceedInputs)
        ));
    }

    /// Understating an input to fake a small fee is detectable, because
    /// the derived fee stops matching the real one.
    #[test]
    fn understated_input_value_shows_up_in_the_derived_fee() {
        let mut wallet = funded_wallet(MNEMONIC);
        let (ext, int) = descriptors(&wallet);
        let mut psbt = plan(&mut wallet, &foreign_address(), 25_000);
        let honest = derive_intent(&psbt, &ext, &int, Network::Signet).unwrap();

        let real = psbt.inputs[0].witness_utxo.clone().unwrap();
        let understate = |psbt: &mut Psbt, by: u64| {
            psbt.inputs[0].witness_utxo = Some(TxOut {
                value: real.value - Amount::from_sat(by),
                script_pubkey: real.script_pubkey.clone(),
            });
        };

        // Drive the claimed fee down to 1 sat: the derivation reports the
        // lie rather than the truth, because only the caller knows both.
        understate(&mut psbt, honest.fee_sat - 1);
        let lying = derive_intent(&psbt, &ext, &int, Network::Signet).unwrap();
        assert_eq!(lying.fee_sat, 1);
        assert_eq!(lying.sats_out, honest.sats_out);

        // Overshooting the lie is caught by the arithmetic itself.
        understate(&mut psbt, honest.fee_sat + 1);
        assert!(matches!(
            derive_intent(&psbt, &ext, &int, Network::Signet),
            Err(VerifyError::OutputsExceedInputs)
        ));
        // The signature such a PSBT produces commits to the false amount
        // (BIP-341), so it cannot spend the real UTXO. See the module docs.
    }
}
