//! The network policy: sats runs only on test networks.
//!
//! Software signing on the agent's host is confined to networks whose
//! coins have no value. Every path that opens a wallet, resolves a chain
//! source, or constructs a local signer checks this first, so a caller
//! that skips the CLI is refused the same way.

use bdk_wallet::bitcoin::Network;
use thiserror::Error;

/// Mainnet (or legacy testnet3) asked of a build that refuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error(
    "mainnet is not supported in this release of sats — use signet (the default), testnet4, or \
     regtest"
)]
pub struct MainnetRefused;

/// Allow signet, testnet4, and regtest; refuse everything else.
pub fn ensure_test_network(network: Network) -> Result<(), MainnetRefused> {
    match network {
        Network::Signet | Network::Testnet4 | Network::Regtest => Ok(()),
        _ => Err(MainnetRefused),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_test_networks_are_allowed() {
        for network in [Network::Signet, Network::Testnet4, Network::Regtest] {
            assert_eq!(ensure_test_network(network), Ok(()));
        }
        assert_eq!(ensure_test_network(Network::Bitcoin), Err(MainnetRefused));
        assert_eq!(ensure_test_network(Network::Testnet), Err(MainnetRefused));
    }

    #[test]
    fn the_refusal_names_the_networks_that_work() {
        let message = MainnetRefused.to_string();
        assert!(message.contains("mainnet is not supported"), "{message}");
        assert!(message.contains("signet (the default), testnet4, or regtest"));
    }
}
