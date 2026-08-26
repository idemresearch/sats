//! Agent capability tokens.
//!
//! A token is the bearer credential an agent presents to the signing
//! daemon. It is **not** key material: it names a policy the daemon
//! enforces, and it opens nothing on its own. Compromising a token costs
//! the grant's remaining budget until its expiry, never the seed.
//!
//! Only the SHA-256 of a token is persisted, so reading a grant file
//! yields neither a seed nor a usable token. The secret is shown to the
//! human exactly once, at grant creation.

use bdk_wallet::bitcoin::hashes::{Hash, sha256};
use zeroize::Zeroizing;

use crate::error::TokenError;

/// Bytes of entropy in a token secret.
const TOKEN_BYTES: usize = 32;

/// Hex characters of the token hash used as the public identifier. A
/// preimage of the hash, not of the secret, so it is safe to log.
const TOKEN_ID_LEN: usize = 12;

/// A freshly minted token: the secret to hand the human once, plus the
/// public identifier and hash to persist beside the grant.
pub struct NewToken {
    /// The bearer secret. Never persisted by sats.
    pub secret: Zeroizing<String>,
    pub token_id: String,
    pub token_hash: String,
}

/// Mint a token from OS entropy.
pub fn generate() -> Result<NewToken, TokenError> {
    let mut bytes = Zeroizing::new([0u8; TOKEN_BYTES]);
    getrandom::fill(&mut bytes[..]).map_err(|_| TokenError::Rng)?;
    let secret = Zeroizing::new(hex::encode(&bytes[..]));
    let token_hash = hash(&secret)?;
    let token_id = token_hash[..TOKEN_ID_LEN].to_string();
    Ok(NewToken {
        secret,
        token_id,
        token_hash,
    })
}

/// Hash a presented token secret. Rejects anything that is not exactly a
/// token-shaped hex string, so a malformed credential never reaches the
/// comparison.
pub fn hash(token: &str) -> Result<String, TokenError> {
    let bytes = hex::decode(token).map_err(|_| TokenError::Malformed)?;
    if bytes.len() != TOKEN_BYTES {
        return Err(TokenError::Malformed);
    }
    Ok(sha256::Hash::hash(&bytes).to_string())
}

/// The public identifier a token would carry, for logs and errors.
pub fn id_of(token: &str) -> Result<String, TokenError> {
    Ok(hash(token)?[..TOKEN_ID_LEN].to_string())
}

/// Whether a presented token matches a stored hash.
///
/// The comparison is constant time in the contents. It is not constant
/// time in the length, which is fixed for a hex SHA-256 and therefore
/// carries no secret.
pub fn verify(token: &str, expected_hash: &str) -> bool {
    match hash(token) {
        Ok(actual) => ct_eq(actual.as_bytes(), expected_hash.as_bytes()),
        Err(_) => false,
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_verify_and_differ() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_ne!(*a.secret, *b.secret, "tokens must not repeat");
        assert!(verify(&a.secret, &a.token_hash));
        assert!(!verify(&a.secret, &b.token_hash));
        assert!(!verify(&b.secret, &a.token_hash));
    }

    #[test]
    fn the_secret_is_not_recoverable_from_what_is_stored() {
        let token = generate().unwrap();
        assert_eq!(token.secret.len(), TOKEN_BYTES * 2);
        assert_eq!(token.token_hash.len(), 64);
        assert_eq!(token.token_id.len(), TOKEN_ID_LEN);
        assert!(token.token_hash.starts_with(&token.token_id));
        assert!(
            !token.token_hash.contains(&*token.secret),
            "stored hash must not contain the secret"
        );
        assert_eq!(id_of(&token.secret).unwrap(), token.token_id);
    }

    #[test]
    fn malformed_tokens_are_refused_not_compared() {
        let token = generate().unwrap();
        for bad in ["", "zz", &"a".repeat(63), &"a".repeat(65), "not-hex-at-all"] {
            assert!(hash(bad).is_err(), "{bad:?} must not hash");
            assert!(!verify(bad, &token.token_hash));
        }
        // Right length, wrong value: shaped like a token, still refused.
        assert!(!verify(&"0".repeat(64), &token.token_hash));
    }

    #[test]
    fn ct_eq_matches_plain_equality() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(ct_eq(b"", b""));
    }
}
