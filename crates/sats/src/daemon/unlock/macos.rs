//! A fixed AppleScript opens a Standard Additions masked-input dialog in
//! its own process. Its stdout is a private pipe to satsd, NEVER MCP stdout.
//! No shell, external script path, password argument, or environment hook.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use super::{UnlockCode, UnlockResult, prompt_unavailable};

const SCRIPT: &str = include_str!("macos.applescript");
const DEADLINE: Duration = Duration::from_secs(120);

struct Helper(Child);

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(message: &str) -> Command {
    let mut command = Command::new("/usr/bin/osascript");
    command
        .args(["-e", SCRIPT, "--", message])
        .env_clear()
        .env("LC_CTYPE", "UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

pub(super) fn prompt(
    message: &str,
    consent: &mut dyn FnMut() -> bool,
) -> Result<Zeroizing<String>, UnlockResult> {
    run(&mut command(message), consent, DEADLINE)
}

fn run(
    command: &mut Command,
    consent: &mut dyn FnMut() -> bool,
    timeout: Duration,
) -> Result<Zeroizing<String>, UnlockResult> {
    let mut helper = Helper(command.spawn().map_err(|_| prompt_unavailable())?);
    let started = Instant::now();
    loop {
        if !consent() {
            return Err(UnlockResult::cancelled());
        }
        if started.elapsed() >= timeout {
            return Err(UnlockResult::error(
                UnlockCode::UnlockTimeout,
                "Unlock dialog timed out. Do not reopen it unless the human asks.",
            ));
        }
        if let Some(status) = helper.0.try_wait().map_err(|_| prompt_unavailable())? {
            if !status.success() {
                return Err(prompt_unavailable());
            }
            // The fixed script bounds its answer below pipe capacity.
            // Bound and zeroize our copy too; never include it in an error.
            let mut output = Zeroizing::new(String::new());
            helper
                .0
                .stdout
                .take()
                .ok_or_else(prompt_unavailable)?
                .take(8192)
                .read_to_string(&mut output)
                .map_err(|_| prompt_unavailable())?;
            return decode(&output);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn decode(output: &str) -> Result<Zeroizing<String>, UnlockResult> {
    // osascript appends one newline. Do not trim the user's password.
    let output = output.strip_suffix('\n').ok_or_else(prompt_unavailable)?;
    if let Some(password) = output.strip_prefix("password:")
        && password.chars().count() <= 1024
    {
        return Ok(Zeroizing::new(password.into()));
    }
    match output {
        "cancelled" => Err(UnlockResult::cancelled()),
        "timed_out" => Err(UnlockResult::error(
            UnlockCode::UnlockTimeout,
            "Unlock dialog timed out. Do not reopen it unless the human asks.",
        )),
        _ => Err(prompt_unavailable()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::unlock::UnlockStatus;

    #[test]
    fn helper_arguments_are_data_and_environment_is_explicit() {
        let message = "wallet: \"quotes\"\n$(touch /tmp/no) `whoami` \\ end";
        let command = command(message);
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args.last().unwrap(), &message);
        assert_eq!(args[1], SCRIPT);
        assert_eq!(command.get_program(), "/usr/bin/osascript");
        assert_eq!(command.get_envs().count(), 1);
        assert!(SCRIPT.contains("with hidden answer"));
        assert!(!SCRIPT.contains("do shell script"));
    }

    #[test]
    fn helper_reply_is_bounded_and_password_is_not_trimmed_or_echoed() {
        assert_eq!(
            &*decode("password:  whitespace\n\n").unwrap(),
            "  whitespace\n"
        );
        assert_eq!(
            decode("cancelled\n").unwrap_err().status,
            UnlockStatus::Cancelled
        );
        assert_eq!(
            decode("timed_out\n").unwrap_err().error_code,
            Some(UnlockCode::UnlockTimeout)
        );
        for bad in [
            "SECRET ERROR".to_string(),
            format!("password:{}\n", "x".repeat(1025)),
            "malformed\n".into(),
        ] {
            let result = decode(&bad).unwrap_err();
            assert_eq!(result.error_code, Some(UnlockCode::PromptUnavailable));
            assert!(!result.message.contains(&bad));
        }
    }

    #[test]
    fn helper_lifetime_is_bounded_and_cancellable() {
        let test_command = || {
            let mut command = Command::new("/bin/sleep");
            command
                .arg("60")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            command
        };
        let start = Instant::now();
        let timeout = run(&mut test_command(), &mut || true, Duration::from_millis(5)).unwrap_err();
        assert_eq!(timeout.error_code, Some(UnlockCode::UnlockTimeout));
        let cancelled =
            run(&mut test_command(), &mut || false, Duration::from_secs(120)).unwrap_err();
        assert_eq!(cancelled.status, UnlockStatus::Cancelled);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
