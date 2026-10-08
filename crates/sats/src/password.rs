use std::io::IsTerminal;

use anyhow::{Result, bail};
use zeroize::Zeroizing;

const MIN_LEN: usize = 8;

/// Test seam: debug builds also take secrets without a terminal (the
/// password from `SATS_PASSWORD`, a restore phrase from a pipe) so the
/// integration tests can drive the binary. A release build takes the
/// password and mnemonic only from a terminal: every program the user
/// starts, agents included, inherits the environment, and with
/// `agent approve --yes` an environment password would sign with no human
/// at the keyboard.
pub(crate) const NON_INTERACTIVE_SECRETS: bool = cfg!(debug_assertions);

/// Resolve the wallet password from an interactive prompt (or, in debug
/// builds only, `SATS_PASSWORD`). `confirm` re-prompts and enforces the
/// minimum length (init).
pub fn get(confirm: bool) -> Result<Zeroizing<String>> {
    let env = std::env::var("SATS_PASSWORD").ok().map(Zeroizing::new);
    if let Some(pw) = from_env(env, NON_INTERACTIVE_SECRETS)? {
        if confirm {
            check_strength(&pw)?;
        }
        return Ok(pw);
    }
    if !std::io::stdin().is_terminal() {
        if NON_INTERACTIVE_SECRETS {
            bail!("no password: set SATS_PASSWORD or run interactively");
        }
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

fn from_env(value: Option<Zeroizing<String>>, honored: bool) -> Result<Option<Zeroizing<String>>> {
    match value {
        Some(_) if !honored => bail!(
            "SATS_PASSWORD is not supported: every program you start, agents \
             included, can read it. Unset it (unset SATS_PASSWORD) and enter \
             the password when sats asks"
        ),
        value => Ok(value),
    }
}

fn check_strength(pw: &str) -> Result<()> {
    if pw.len() < MIN_LEN {
        bail!("password must be at least {MIN_LEN} characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(pw: &str) -> Option<Zeroizing<String>> {
        Some(Zeroizing::new(pw.to_string()))
    }

    #[test]
    fn release_builds_refuse_the_environment_password() {
        let err = from_env(set("correct horse"), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("SATS_PASSWORD is not supported"), "{err}");
        assert!(err.contains("unset SATS_PASSWORD"), "{err}");
        assert!(
            !err.contains("correct horse"),
            "the refusal must not echo the secret: {err}"
        );
    }

    #[test]
    fn debug_builds_take_the_environment_password() {
        let pw = from_env(set("correct horse"), true).unwrap().unwrap();
        assert_eq!(pw.as_str(), "correct horse");
    }

    #[test]
    fn an_unset_variable_falls_through_to_the_prompt() {
        assert!(from_env(None, false).unwrap().is_none());
        assert!(from_env(None, true).unwrap().is_none());
    }

    #[test]
    fn this_test_build_honors_the_seam() {
        assert!(
            NON_INTERACTIVE_SECRETS,
            "integration tests drive the binary with SATS_PASSWORD; run them in a debug build"
        );
    }
}
