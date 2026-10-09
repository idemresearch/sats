use std::io::IsTerminal;

use anyhow::{Result, bail};
use zeroize::Zeroizing;

const MIN_LEN: usize = 8;

/// Test seam: debug builds also take secrets without a terminal (the
/// password from `SATS_PASSWORD`, a restore phrase from a pipe) so the
/// integration tests can drive the binary. Release builds compile it out
/// and read secrets only from a terminal.
pub(crate) const TEST_SEAM: bool = cfg!(debug_assertions);

/// Resolve the wallet password from an interactive prompt. `confirm`
/// re-prompts and enforces the minimum length (init).
pub fn get(confirm: bool) -> Result<Zeroizing<String>> {
    if let Some(pw) = test_password() {
        if confirm {
            check_strength(&pw)?;
        }
        return Ok(pw);
    }
    if !std::io::stdin().is_terminal() {
        bail!("no password: run sats on a terminal to enter it");
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

#[cfg(debug_assertions)]
fn test_password() -> Option<Zeroizing<String>> {
    std::env::var("SATS_PASSWORD").ok().map(Zeroizing::new)
}

#[cfg(not(debug_assertions))]
fn test_password() -> Option<Zeroizing<String>> {
    None
}

fn check_strength(pw: &str) -> Result<()> {
    if pw.len() < MIN_LEN {
        bail!("password must be at least {MIN_LEN} characters");
    }
    Ok(())
}
