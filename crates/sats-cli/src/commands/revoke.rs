use anyhow::{Result, bail};
use sats_core::bitcoin::Network;
use sats_wallet::config::network_name;
use sats_wallet::store::Store;

use crate::ui;

pub fn run(store: &Store, network: Network, agent: &str, json: bool) -> Result<()> {
    let net_name = network_name(network);
    // Under the grant lock so revocation serializes with any in-flight
    // request creation or approval: whichever runs second sees the other.
    let revoked = {
        let _lock = store.lock_grants(net_name)?;
        store.delete_grant(net_name, agent)?
    };
    if !revoked {
        bail!("no grant for {agent:?}");
    }
    if json {
        println!("{}", serde_json::json!({ "agent": agent, "revoked": true }));
    } else {
        ui::ok(&format!("revoked  {agent}"));
    }
    Ok(())
}
