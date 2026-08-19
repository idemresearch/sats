use std::io::IsTerminal;

use anyhow::{Result, bail};
use zeroize::Zeroizing;

const MIN_LEN: usize = 8;

/// Resolve the wallet password: `SATS_PASSWORD` env, else an interactive
/// prompt. `confirm` re-prompts and enforces the minimum length (init).
pub fn get(confirm: bool) -> Result<Zeroizing<String>> {
    if let Ok(pw) = std::env::var("SATS_PASSWORD") {
        let pw = Zeroizing::new(pw);
        if confirm {
            check_strength(&pw)?;
        }
        return Ok(pw);
    }
    if !std::io::stdin().is_terminal() {
        bail!("no password: set SATS_PASSWORD or run interactively");
    }
    let pw = Zeroizing::new(rpassword::prompt_password("password: ")?);
    if confirm {
        check_strength(&pw)?;
        let again = Zeroizing::new(rpassword::prompt_password("confirm:  ")?);
        if *pw != *again {
            bail!("passwords do not match");
        }
    }
    Ok(pw)
}

fn check_strength(pw: &str) -> Result<()> {
    if pw.len() < MIN_LEN {
        bail!("password must be at least {MIN_LEN} characters");
    }
    Ok(())
}
