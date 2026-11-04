//! The relay protocol between sats agent, a relay, and sats human.
//!
//! A relay is an intermediary: it serves MCP to an agent that can't reach
//! the human's device, queues what the agent files, and reports what the
//! human's device decided. It holds no wallet and no key. Everything here
//! is pure: identities, the agent's signed credential, sealed envelopes,
//! and signed device calls. Transport and storage belong to callers.
//!
//! - **Human key.** A secp256k1 key the human's device generates for relay
//!   communication only. It is never derived from, and never unlocks, the
//!   wallet seed. Its public key names the device's mailbox.
//! - **Capability.** The connection code the human pastes on the relay's
//!   sign-in page: the device-signed statement "agent A may file under
//!   grant G to mailbox H until T". It carries no grant token. The relay
//!   verifies the signature and routes, so it needs no table of pairings.
//!   The device verifies its own signature again on every filing and checks
//!   that grant G is still the agent's grant.
//! - **Results.** What the device posts back is sealed under a key derived
//!   from the capability's grant, so the relay can read a result only while
//!   serving a connection that holds that capability.
//! - **Envelope.** A filing sealed to the human key (ephemeral ECDH,
//!   ChaCha20-Poly1305), bound to its request id and mailbox, so a relay's
//!   storage holds ciphertext only.
//! - **Device calls.** The device signs each call to the relay over the
//!   method, path, time, and body; the relay accepts calls within a short
//!   window.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bdk_wallet::bitcoin::hashes::{Hash, sha256};
use bdk_wallet::bitcoin::secp256k1::{
    Keypair, Message, PublicKey, Secp256k1, SecretKey, XOnlyPublicKey, ecdh::SharedSecret, schnorr,
};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// The capability format this build writes and accepts.
pub const CAPABILITY_VERSION: u8 = 2;
/// The relayed-filing format this build writes and accepts.
pub const FILING_VERSION: u8 = 2;
/// The longest capability string accepted, bounding work on untrusted input.
pub const MAX_CAPABILITY_LEN: usize = 2048;
/// How far a device call's timestamp may be from the relay's clock.
pub const CALL_WINDOW_SECS: u64 = 300;
/// The header a device call carries: `<human pubkey hex>.<unix secs>.<sig hex>`.
pub const AUTH_HEADER: &str = "sats-human-auth";

/// The networks a capability may name. Mainnet is refused, as everywhere.
const TEST_NETWORKS: [&str; 3] = ["signet", "testnet4", "regtest"];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RelayError {
    #[error("malformed {0}")]
    Malformed(&'static str),
    #[error("bad signature")]
    BadSignature,
    #[error("unsupported version {0}")]
    Version(u8),
    #[error("network {0:?} is not a test network")]
    Network(String),
    #[error("cannot open the envelope")]
    Open,
    #[error("the operating system's random number generator failed")]
    Rng,
    #[error("the call is outside the {CALL_WINDOW_SECS}s window")]
    Stale,
}

fn tagged(tag: &str, parts: &[&[u8]]) -> [u8; 32] {
    let mut engine = sha256::Hash::engine();
    use bdk_wallet::bitcoin::hashes::HashEngine;
    engine.input(tag.as_bytes());
    for part in parts {
        engine.input(&[0]);
        engine.input(&(part.len() as u64).to_be_bytes());
        engine.input(part);
    }
    sha256::Hash::from_engine(engine).to_byte_array()
}

/// The human device's relay key. Never a wallet key.
pub struct HumanKey {
    secret: SecretKey,
}

impl HumanKey {
    pub fn generate() -> Result<HumanKey, RelayError> {
        loop {
            let mut bytes = Zeroizing::new([0u8; 32]);
            getrandom::fill(&mut bytes[..]).map_err(|_| RelayError::Rng)?;
            if let Ok(secret) = SecretKey::from_slice(&bytes[..]) {
                return Ok(HumanKey { secret });
            }
        }
    }

    pub fn from_hex(hex_secret: &str) -> Result<HumanKey, RelayError> {
        let bytes = Zeroizing::new(
            hex::decode(hex_secret.trim()).map_err(|_| RelayError::Malformed("human key"))?,
        );
        let secret =
            SecretKey::from_slice(&bytes).map_err(|_| RelayError::Malformed("human key"))?;
        Ok(HumanKey { secret })
    }

    pub fn to_hex(&self) -> Zeroizing<String> {
        Zeroizing::new(hex::encode(self.secret.secret_bytes()))
    }

    /// The compressed public key, hex: the mailbox name.
    pub fn public_hex(&self) -> String {
        hex::encode(PublicKey::from_secret_key(&Secp256k1::new(), &self.secret).serialize())
    }

    fn sign(&self, digest: [u8; 32]) -> String {
        let secp = Secp256k1::new();
        let keypair = Keypair::from_secret_key(&secp, &self.secret);
        hex::encode(
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(digest), &keypair)
                .serialize(),
        )
    }
}

/// A mailbox name: a compressed secp256k1 public key, hex.
pub fn parse_public(hex_public: &str) -> Result<PublicKey, RelayError> {
    if hex_public.len() != 66 {
        return Err(RelayError::Malformed("human public key"));
    }
    let bytes = hex::decode(hex_public).map_err(|_| RelayError::Malformed("human public key"))?;
    PublicKey::from_slice(&bytes).map_err(|_| RelayError::Malformed("human public key"))
}

fn verify(public: &PublicKey, digest: [u8; 32], sig: &[u8]) -> Result<(), RelayError> {
    let sig = schnorr::Signature::from_slice(sig).map_err(|_| RelayError::BadSignature)?;
    let (xonly, _): (XOnlyPublicKey, _) = public.x_only_public_key();
    Secp256k1::verification_only()
        .verify_schnorr(&sig, &Message::from_digest(digest), &xonly)
        .map_err(|_| RelayError::BadSignature)
}

/// The agent's relay credential, signed by the human key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub v: u8,
    pub network: String,
    /// The human device's public key: where filings go.
    pub human: String,
    pub agent: String,
    /// The grant instance filings bind to.
    pub grant_id: String,
    pub expires_at: u64,
}

const CAPABILITY_TAG: &str = "sats-relay/capability/v2";

impl Capability {
    /// `<base64url payload>.<base64url signature>`, URL-path safe.
    pub fn encode(&self, key: &HumanKey) -> Result<String, RelayError> {
        if self.human != key.public_hex() {
            return Err(RelayError::Malformed("capability: human key mismatch"));
        }
        let payload = serde_json::to_vec(self).map_err(|_| RelayError::Malformed("capability"))?;
        let sig =
            hex::decode(key.sign(tagged(CAPABILITY_TAG, &[&payload]))).expect("hex we just wrote");
        Ok(format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&payload),
            URL_SAFE_NO_PAD.encode(sig)
        ))
    }

    /// Decode and verify a capability against the key it names. Expiry is
    /// the caller's check, against its own clock.
    pub fn decode(encoded: &str) -> Result<Capability, RelayError> {
        if encoded.len() > MAX_CAPABILITY_LEN {
            return Err(RelayError::Malformed("capability"));
        }
        let (payload, sig) = encoded
            .split_once('.')
            .ok_or(RelayError::Malformed("capability"))?;
        let payload = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| RelayError::Malformed("capability"))?;
        let sig = URL_SAFE_NO_PAD
            .decode(sig)
            .map_err(|_| RelayError::Malformed("capability"))?;
        let capability: Capability =
            serde_json::from_slice(&payload).map_err(|_| RelayError::Malformed("capability"))?;
        if capability.v != CAPABILITY_VERSION {
            return Err(RelayError::Version(capability.v));
        }
        if !TEST_NETWORKS.contains(&capability.network.as_str()) {
            return Err(RelayError::Network(capability.network));
        }
        let human = parse_public(&capability.human)?;
        verify(&human, tagged(CAPABILITY_TAG, &[&payload]), &sig)?;
        Ok(capability)
    }
}

/// What the agent asked for, as the relay hands it to the device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RelayedOperation {
    Send { address: String, amount_sat: u64 },
    FrbtcWrap { amount_sat: u64 },
}

impl RelayedOperation {
    /// A digest of the operation, so a reused idempotency key with a
    /// different operation is a conflict at the relay too.
    pub fn digest(&self) -> String {
        let canonical = serde_json::to_vec(self).expect("serializable");
        hex::encode(tagged("sats-relay/operation/v1", &[&canonical]))
    }
}

/// One filing, sealed to the device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayedFiling {
    pub v: u8,
    pub request_id: String,
    /// The encoded capability the agent connected with. The device verifies
    /// its own signature on it before filing anything.
    pub capability: String,
    pub idempotency_key: String,
    pub operation: RelayedOperation,
}

impl Capability {
    /// Verify that `encoded` is a live capability signed by `key` itself:
    /// the device accepts filings only under connection codes it minted.
    pub fn verify_own(
        encoded: &str,
        key: &HumanKey,
        now_unix: u64,
    ) -> Result<Capability, RelayError> {
        let capability = Capability::decode(encoded)?;
        if capability.human != key.public_hex() {
            return Err(RelayError::BadSignature);
        }
        if now_unix >= capability.expires_at {
            return Err(RelayError::Stale);
        }
        Ok(capability)
    }
}

const RESULT_TAG: &str = "sats-relay/result/v1";

/// The key a device's results for one grant are sealed under. Both sides
/// derive it from facts in the capability; the relay's store holds none
/// of them in the clear.
pub fn result_key(network: &str, human: &str, agent: &str, grant_id: &str) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(tagged(
        RESULT_TAG,
        &[
            network.as_bytes(),
            human.as_bytes(),
            agent.as_bytes(),
            grant_id.as_bytes(),
        ],
    ))
}

impl Capability {
    pub fn result_key(&self) -> Zeroizing<[u8; 32]> {
        result_key(&self.network, &self.human, &self.agent, &self.grant_id)
    }
}

fn result_aad(request_id: &str) -> Vec<u8> {
    format!("{RESULT_TAG}\0{request_id}").into_bytes()
}

/// Seal a result view: `nonce (12) || ciphertext`, bound to its request id.
pub fn seal_result(key: &[u8; 32], request_id: &str, view: &[u8]) -> Result<Vec<u8>, RelayError> {
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).map_err(|_| RelayError::Rng)?;
    let ciphertext = ChaCha20Poly1305::new(key.into())
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: view,
                aad: &result_aad(request_id),
            },
        )
        .map_err(|_| RelayError::Open)?;
    let mut sealed = nonce.to_vec();
    sealed.extend_from_slice(&ciphertext);
    Ok(sealed)
}

pub fn open_result(key: &[u8; 32], request_id: &str, sealed: &[u8]) -> Result<Vec<u8>, RelayError> {
    if sealed.len() < 12 + 16 {
        return Err(RelayError::Open);
    }
    ChaCha20Poly1305::new(key.into())
        .decrypt(
            Nonce::from_slice(&sealed[..12]),
            Payload {
                msg: &sealed[12..],
                aad: &result_aad(request_id),
            },
        )
        .map_err(|_| RelayError::Open)
}

const SEAL_TAG: &str = "sats-relay/seal/v1";

fn seal_key(
    shared: &SharedSecret,
    ephemeral: &PublicKey,
    recipient: &PublicKey,
) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(tagged(
        SEAL_TAG,
        &[
            &shared.secret_bytes(),
            &ephemeral.serialize(),
            &recipient.serialize(),
        ],
    ))
}

/// Seal `plaintext` to `recipient`: `ephemeral pubkey (33) || nonce (12) || ciphertext`.
pub fn seal(recipient: &PublicKey, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, RelayError> {
    let ephemeral = HumanKey::generate()?;
    let ephemeral_public = PublicKey::from_secret_key(&Secp256k1::new(), &ephemeral.secret);
    let key = seal_key(
        &SharedSecret::new(recipient, &ephemeral.secret),
        &ephemeral_public,
        recipient,
    );
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).map_err(|_| RelayError::Rng)?;
    let ciphertext = ChaCha20Poly1305::new((&*key).into())
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| RelayError::Open)?;
    let mut sealed = Vec::with_capacity(33 + 12 + ciphertext.len());
    sealed.extend_from_slice(&ephemeral_public.serialize());
    sealed.extend_from_slice(&nonce);
    sealed.extend_from_slice(&ciphertext);
    Ok(sealed)
}

/// Open an envelope sealed to `key` with the same `aad`.
pub fn open(key: &HumanKey, aad: &[u8], sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, RelayError> {
    if sealed.len() < 33 + 12 + 16 {
        return Err(RelayError::Open);
    }
    let ephemeral = PublicKey::from_slice(&sealed[..33]).map_err(|_| RelayError::Open)?;
    let recipient = PublicKey::from_secret_key(&Secp256k1::new(), &key.secret);
    let aead_key = seal_key(
        &SharedSecret::new(&ephemeral, &key.secret),
        &ephemeral,
        &recipient,
    );
    ChaCha20Poly1305::new((&*aead_key).into())
        .decrypt(
            Nonce::from_slice(&sealed[33..45]),
            Payload {
                msg: &sealed[45..],
                aad,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| RelayError::Open)
}

/// The associated data binding an envelope to its mailbox and request.
pub fn envelope_aad(human: &str, request_id: &str) -> Vec<u8> {
    format!("sats-relay/envelope/v1\0{human}\0{request_id}").into_bytes()
}

const CALL_TAG: &str = "sats-relay/call/v1";

fn call_digest(method: &str, path: &str, unix: u64, body: &[u8]) -> [u8; 32] {
    tagged(
        CALL_TAG,
        &[
            method.as_bytes(),
            path.as_bytes(),
            &unix.to_be_bytes(),
            &sha256::Hash::hash(body).to_byte_array(),
        ],
    )
}

/// The [`AUTH_HEADER`] value for one device call.
pub fn sign_call(key: &HumanKey, method: &str, path: &str, unix: u64, body: &[u8]) -> String {
    format!(
        "{}.{unix}.{}",
        key.public_hex(),
        key.sign(call_digest(method, path, unix, body))
    )
}

/// Verify a device call; returns the calling device's public key (hex).
pub fn verify_call(
    header: &str,
    method: &str,
    path: &str,
    body: &[u8],
    now_unix: u64,
) -> Result<String, RelayError> {
    let mut parts = header.split('.');
    let (Some(human), Some(unix), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(RelayError::Malformed("auth header"));
    };
    let unix: u64 = unix
        .parse()
        .map_err(|_| RelayError::Malformed("auth header"))?;
    if now_unix.abs_diff(unix) > CALL_WINDOW_SECS {
        return Err(RelayError::Stale);
    }
    let public = parse_public(human)?;
    let sig = hex::decode(sig).map_err(|_| RelayError::BadSignature)?;
    verify(&public, call_digest(method, path, unix, body), &sig)?;
    Ok(human.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capability(key: &HumanKey) -> Capability {
        Capability {
            v: CAPABILITY_VERSION,
            network: "signet".into(),
            human: key.public_hex(),
            agent: "chatgpt".into(),
            grant_id: "g".repeat(32),
            expires_at: 1_900_000_000,
        }
    }

    #[test]
    fn a_capability_round_trips_and_any_change_breaks_it() {
        let key = HumanKey::generate().unwrap();
        let cap = capability(&key);
        let encoded = cap.encode(&key).unwrap();
        assert!(!encoded.contains('/') && !encoded.contains('+') && !encoded.contains('='));
        assert_eq!(Capability::decode(&encoded).unwrap(), cap);

        // Rewriting the payload (fx a longer expiry) invalidates it.
        let (_, sig) = encoded.split_once('.').unwrap();
        let mut widened = cap.clone();
        widened.expires_at += 1;
        let forged = format!(
            "{}.{sig}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&widened).unwrap())
        );
        assert_eq!(Capability::decode(&forged), Err(RelayError::BadSignature));

        // Another key can't mint for this mailbox.
        let other = HumanKey::generate().unwrap();
        assert!(cap.encode(&other).is_err());
        let mut claimed = capability(&other);
        claimed.human = key.public_hex();
        let payload = serde_json::to_vec(&claimed).unwrap();
        let sig = other.sign(tagged(CAPABILITY_TAG, &[&payload]));
        let forged = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&payload),
            URL_SAFE_NO_PAD.encode(hex::decode(sig).unwrap())
        );
        assert_eq!(Capability::decode(&forged), Err(RelayError::BadSignature));
    }

    #[test]
    fn a_capability_refuses_mainnet_unknown_versions_and_junk() {
        let key = HumanKey::generate().unwrap();
        let mut cap = capability(&key);
        cap.network = "mainnet".into();
        let encoded = cap.encode(&key).unwrap();
        assert!(matches!(
            Capability::decode(&encoded),
            Err(RelayError::Network(_))
        ));
        let mut cap = capability(&key);
        for version in [1, 3] {
            cap.v = version;
            let encoded = cap.encode(&key).unwrap();
            assert_eq!(
                Capability::decode(&encoded),
                Err(RelayError::Version(version))
            );
        }
        for junk in ["", ".", "abc", "a.b.c", &"a".repeat(MAX_CAPABILITY_LEN + 1)] {
            assert!(Capability::decode(junk).is_err(), "{junk}");
        }
    }

    #[test]
    fn envelopes_open_only_for_their_key_and_binding() {
        let key = HumanKey::generate().unwrap();
        let public = parse_public(&key.public_hex()).unwrap();
        let aad = envelope_aad(&key.public_hex(), "r-1");
        let sealed = seal(&public, &aad, b"wrap 10k").unwrap();
        assert_eq!(&open(&key, &aad, &sealed).unwrap()[..], b"wrap 10k");
        // Moved to another request id or mailbox: refused.
        assert_eq!(
            open(&key, &envelope_aad(&key.public_hex(), "r-2"), &sealed),
            Err(RelayError::Open)
        );
        let other = HumanKey::generate().unwrap();
        assert_eq!(open(&other, &aad, &sealed), Err(RelayError::Open));
        let mut flipped = sealed.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert_eq!(open(&key, &aad, &flipped), Err(RelayError::Open));
        assert_eq!(open(&key, &aad, &sealed[..40]), Err(RelayError::Open));
    }

    #[test]
    fn device_calls_bind_method_path_time_and_body() {
        let key = HumanKey::generate().unwrap();
        let header = sign_call(&key, "POST", "/v1/results", 1000, b"{}");
        assert_eq!(
            verify_call(&header, "POST", "/v1/results", b"{}", 1000).unwrap(),
            key.public_hex()
        );
        assert!(
            verify_call(
                &header,
                "POST",
                "/v1/results",
                b"{}",
                1000 + CALL_WINDOW_SECS
            )
            .is_ok()
        );
        assert_eq!(
            verify_call(
                &header,
                "POST",
                "/v1/results",
                b"{}",
                1001 + CALL_WINDOW_SECS
            ),
            Err(RelayError::Stale)
        );
        for (method, path, body) in [
            ("GET", "/v1/results", &b"{}"[..]),
            ("POST", "/v1/inbox", b"{}"),
            ("POST", "/v1/results", b"{ }"),
        ] {
            assert_eq!(
                verify_call(&header, method, path, body, 1000),
                Err(RelayError::BadSignature)
            );
        }
        assert!(verify_call("x.y", "GET", "/", b"", 0).is_err());
    }

    #[test]
    fn the_human_key_round_trips_through_hex() {
        let key = HumanKey::generate().unwrap();
        let again = HumanKey::from_hex(&key.to_hex()).unwrap();
        assert_eq!(again.public_hex(), key.public_hex());
        assert!(HumanKey::from_hex("00").is_err());
        assert!(parse_public(&format!("05{}", "11".repeat(32))).is_err());
    }

    #[test]
    fn operation_digests_differ_by_operation() {
        let send = RelayedOperation::Send {
            address: "tb1q".into(),
            amount_sat: 1,
        };
        let wrap = RelayedOperation::FrbtcWrap { amount_sat: 1 };
        assert_ne!(send.digest(), wrap.digest());
        assert_eq!(
            wrap.digest(),
            RelayedOperation::FrbtcWrap { amount_sat: 1 }.digest()
        );
    }

    #[test]
    fn a_device_accepts_only_its_own_live_capabilities() {
        let key = HumanKey::generate().unwrap();
        let encoded = capability(&key).encode(&key).unwrap();
        assert!(Capability::verify_own(&encoded, &key, 1_000).is_ok());
        assert_eq!(
            Capability::verify_own(&encoded, &key, 1_900_000_000),
            Err(RelayError::Stale)
        );
        let other = HumanKey::generate().unwrap();
        let foreign = capability(&other).encode(&other).unwrap();
        assert_eq!(
            Capability::verify_own(&foreign, &key, 1_000),
            Err(RelayError::BadSignature)
        );
    }

    #[test]
    fn results_open_only_with_their_grant_key_and_request_id() {
        let key = HumanKey::generate().unwrap();
        let cap = capability(&key);
        let k = cap.result_key();
        assert_eq!(
            *k,
            *result_key(&cap.network, &cap.human, &cap.agent, &cap.grant_id)
        );
        let sealed = seal_result(&k, "r-1", b"{\"status\":\"sent\"}").unwrap();
        assert_eq!(
            open_result(&k, "r-1", &sealed).unwrap(),
            b"{\"status\":\"sent\"}"
        );
        assert!(open_result(&k, "r-2", &sealed).is_err());
        let mut other = cap.clone();
        other.grant_id = "h".repeat(32);
        assert!(open_result(&other.result_key(), "r-1", &sealed).is_err());
        assert!(open_result(&k, "r-1", &sealed[..20]).is_err());
    }
}
