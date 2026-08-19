//! Sealed blobs: authenticated encryption for secrets at rest.
//!
//! Two modes share one versioned format, distinguished by `kdf`:
//! - `"argon2id"` — password-based (the master seed file)
//! - `"none"` — a caller-provided random 32-byte key (grant-wrapped seeds)
//!
//! Cipher is XChaCha20-Poly1305; the caller's AAD binds a blob to its
//! purpose so a grant blob can never be replayed as a seed file.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::SealError;

const VERSION: u32 = 1;
const KDF_ARGON2ID: &str = "argon2id";
const KDF_NONE: &str = "none";

/// Argon2id parameters: 64 MiB, 3 passes, 1 lane.
const M_KIB: u32 = 65536;
const T_COST: u32 = 3;
const P_COST: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SealedBlob {
    pub v: u32,
    pub kdf: String,
    #[serde(default)]
    pub m_kib: u32,
    #[serde(default)]
    pub t: u32,
    #[serde(default)]
    pub p: u32,
    pub salt: String,
    pub nonce: String,
    pub ct: String,
}

/// Seal `plaintext` under a password (argon2id-derived key).
pub fn seal(plaintext: &[u8], password: &[u8], aad: &[u8]) -> Result<SealedBlob, SealError> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|_| SealError::Rng)?;
    let key = derive_key(password, &salt, M_KIB, T_COST, P_COST)?;
    let (nonce, ct) = encrypt(&key, plaintext, aad)?;
    Ok(SealedBlob {
        v: VERSION,
        kdf: KDF_ARGON2ID.into(),
        m_kib: M_KIB,
        t: T_COST,
        p: P_COST,
        salt: B64.encode(salt),
        nonce: B64.encode(nonce),
        ct: B64.encode(ct),
    })
}

/// Seal `plaintext` under a caller-provided random 32-byte key (no KDF).
pub fn seal_with_key(plaintext: &[u8], key: &[u8; 32], aad: &[u8]) -> Result<SealedBlob, SealError> {
    let (nonce, ct) = encrypt(key, plaintext, aad)?;
    Ok(SealedBlob {
        v: VERSION,
        kdf: KDF_NONE.into(),
        m_kib: 0,
        t: 0,
        p: 0,
        salt: String::new(),
        nonce: B64.encode(nonce),
        ct: B64.encode(ct),
    })
}

/// Open a password-sealed blob.
pub fn open(blob: &SealedBlob, password: &[u8], aad: &[u8]) -> Result<Zeroizing<Vec<u8>>, SealError> {
    check_version(blob)?;
    if blob.kdf != KDF_ARGON2ID {
        return Err(SealError::UnsupportedKdf(blob.kdf.clone()));
    }
    let salt = b64_field(&blob.salt, "salt")?;
    let key = derive_key(password, &salt, blob.m_kib, blob.t, blob.p)?;
    decrypt(&key, blob, aad)
}

/// Open a key-sealed blob.
pub fn open_with_key(blob: &SealedBlob, key: &[u8; 32], aad: &[u8]) -> Result<Zeroizing<Vec<u8>>, SealError> {
    check_version(blob)?;
    if blob.kdf != KDF_NONE {
        return Err(SealError::UnsupportedKdf(blob.kdf.clone()));
    }
    decrypt(key, blob, aad)
}

/// A fresh random 32-byte key, base64-encoded (grant keys).
pub fn generate_key_b64() -> Result<String, SealError> {
    let mut key = Zeroizing::new([0u8; 32]);
    getrandom::fill(key.as_mut()).map_err(|_| SealError::Rng)?;
    Ok(B64.encode(key.as_slice()))
}

/// Decode a base64 key produced by [`generate_key_b64`].
pub fn decode_key_b64(s: &str) -> Result<Zeroizing<[u8; 32]>, SealError> {
    let bytes = b64_field(s, "key")?;
    let arr: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| SealError::Malformed("key must be 32 bytes".into()))?;
    Ok(Zeroizing::new(arr))
}

fn check_version(blob: &SealedBlob) -> Result<(), SealError> {
    if blob.v != VERSION {
        return Err(SealError::UnsupportedVersion(blob.v));
    }
    Ok(())
}

fn derive_key(
    password: &[u8],
    salt: &[u8],
    m_kib: u32,
    t: u32,
    p: u32,
) -> Result<Zeroizing<[u8; 32]>, SealError> {
    let params = argon2::Params::new(m_kib, t, p, Some(32)).map_err(|_| SealError::Kdf)?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(password, salt, key.as_mut())
        .map_err(|_| SealError::Kdf)?;
    Ok(key)
}

fn encrypt(key: &[u8; 32], plaintext: &[u8], aad: &[u8]) -> Result<([u8; 24], Vec<u8>), SealError> {
    let mut nonce = [0u8; 24];
    getrandom::fill(&mut nonce).map_err(|_| SealError::Rng)?;
    let cipher = XChaCha20Poly1305::new(key.into());
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plaintext, aad })
        .map_err(|_| SealError::Encrypt)?;
    Ok((nonce, ct))
}

fn decrypt(key: &[u8; 32], blob: &SealedBlob, aad: &[u8]) -> Result<Zeroizing<Vec<u8>>, SealError> {
    let nonce = b64_field(&blob.nonce, "nonce")?;
    let ct = b64_field(&blob.ct, "ct")?;
    let cipher = XChaCha20Poly1305::new(key.into());
    let pt = cipher
        .decrypt(
            XNonce::from_slice(&nonce),
            Payload { msg: &ct, aad },
        )
        .map_err(|_| SealError::Decrypt)?;
    Ok(Zeroizing::new(pt))
}

fn b64_field(s: &str, name: &str) -> Result<Vec<u8>, SealError> {
    B64.decode(s)
        .map_err(|_| SealError::Malformed(format!("bad base64 in {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AAD: &[u8] = b"sats-test-v1";

    #[test]
    fn password_round_trip() {
        let blob = seal(b"secret words here", b"hunter22", AAD).unwrap();
        assert_eq!(blob.kdf, "argon2id");
        let pt = open(&blob, b"hunter22", AAD).unwrap();
        assert_eq!(pt.as_slice(), b"secret words here");
    }

    #[test]
    fn wrong_password_fails() {
        let blob = seal(b"secret", b"correct", AAD).unwrap();
        assert!(matches!(open(&blob, b"wrong", AAD), Err(SealError::Decrypt)));
    }

    #[test]
    fn wrong_aad_fails() {
        let blob = seal(b"secret", b"pw", AAD).unwrap();
        assert!(matches!(open(&blob, b"pw", b"other-purpose"), Err(SealError::Decrypt)));
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let mut blob = seal(b"secret", b"pw", AAD).unwrap();
        let mut ct = B64.decode(&blob.ct).unwrap();
        ct[0] ^= 0x01;
        blob.ct = B64.encode(ct);
        assert!(matches!(open(&blob, b"pw", AAD), Err(SealError::Decrypt)));
    }

    #[test]
    fn key_round_trip() {
        let key_b64 = generate_key_b64().unwrap();
        let key = decode_key_b64(&key_b64).unwrap();
        let blob = seal_with_key(b"wrapped seed", &key, AAD).unwrap();
        assert_eq!(blob.kdf, "none");
        let pt = open_with_key(&blob, &key, AAD).unwrap();
        assert_eq!(pt.as_slice(), b"wrapped seed");
    }

    #[test]
    fn mode_confusion_rejected() {
        let key = decode_key_b64(&generate_key_b64().unwrap()).unwrap();
        let blob = seal_with_key(b"x", &key, AAD).unwrap();
        assert!(matches!(open(&blob, b"pw", AAD), Err(SealError::UnsupportedKdf(_))));

        let blob2 = seal(b"x", b"pw", AAD).unwrap();
        assert!(matches!(
            open_with_key(&blob2, &key, AAD),
            Err(SealError::UnsupportedKdf(_))
        ));
    }

    #[test]
    fn unknown_version_rejected() {
        let mut blob = seal(b"x", b"pw", AAD).unwrap();
        blob.v = 2;
        assert!(matches!(open(&blob, b"pw", AAD), Err(SealError::UnsupportedVersion(2))));
    }

    #[test]
    fn serde_round_trip() {
        let blob = seal(b"x", b"pw", AAD).unwrap();
        let json = serde_json::to_string(&blob).unwrap();
        let back: SealedBlob = serde_json::from_str(&json).unwrap();
        let pt = open(&back, b"pw", AAD).unwrap();
        assert_eq!(pt.as_slice(), b"x");
    }
}
