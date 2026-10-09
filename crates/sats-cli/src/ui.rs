//! Output styling: quiet, aligned, minimal color.

use std::io::{BufRead, IsTerminal, Write};
use std::sync::{Arc, Mutex};

use owo_colors::OwoColorize;
use sats_core::fmt::format_sats;
use sats_wallet::provider::Progress;

pub fn use_color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

pub fn ok(msg: &str) {
    if use_color() {
        println!("{} {msg}", "✓".green());
    } else {
        println!("✓ {msg}");
    }
}

pub fn warn(msg: &str) {
    if use_color() {
        println!("{} {msg}", "!".yellow());
    } else {
        println!("! {msg}");
    }
}

pub fn dim(msg: &str) {
    if use_color() {
        println!("{}", msg.dimmed());
    } else {
        println!("{msg}");
    }
}

/// Aligned sat-amount rows:
/// ```text
/// Send   25,000 sats
/// Fee       412 sats
/// Total  25,412 sats
/// ```
pub fn sat_rows(rows: &[(&str, u64)]) {
    let key_w = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let formatted: Vec<String> = rows.iter().map(|(_, v)| format_sats(*v)).collect();
    let val_w = formatted.iter().map(|v| v.len()).max().unwrap_or(0);
    for ((key, _), value) in rows.iter().zip(&formatted) {
        println!("{key:<key_w$}  {value:>val_w$} sat");
    }
}

/// Aligned key-value rows with pre-formatted values (mixed content).
pub fn kv_rows(rows: &[(&str, String)]) {
    kv_rows_to(&mut std::io::stdout(), rows);
}

/// The same rows on stderr: the human review channel when stdout is a
/// machine-readable stream.
pub fn kv_rows_stderr(rows: &[(&str, String)]) {
    kv_rows_to(&mut std::io::stderr(), rows);
}

fn kv_rows_to(out: &mut dyn Write, rows: &[(&str, String)]) {
    let key_w = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (key, value) in rows {
        let _ = writeln!(out, "{key:<key_w$}  {value}");
    }
}

/// Compact duration: "18h", "3d 2h", "45m".
pub fn human_duration(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let minutes = (secs % 3_600) / 60;
    if days > 0 {
        if hours > 0 {
            format!("{days}d {hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        }
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{secs}s")
    }
}

/// Print the workflow's warnings on stderr. Only sats' own records: a
/// dependency's log output could carry endpoint details, and was never
/// shown before.
pub fn install_warnings() {
    static WARNINGS: Warnings = Warnings;
    if log::set_logger(&WARNINGS).is_ok() {
        log::set_max_level(log::LevelFilter::Warn);
    }
}

struct Warnings;

impl log::Log for Warnings {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        // The target starts with the emitting crate: `sats` (this binary)
        // or a `sats_*` library.
        let krate = metadata.target().split("::").next().unwrap_or_default();
        metadata.level() <= log::Level::Warn && (krate == "sats" || krate.starts_with("sats_"))
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("⚠ {}", record.args());
        }
    }

    fn flush(&self) {}
}

/// The "syncing…" status line, for chain services.
pub fn sync_progress() -> Arc<dyn Progress> {
    Arc::new(SyncStatus(Mutex::new(None)))
}

struct SyncStatus(Mutex<Option<StatusLine>>);

impl Progress for SyncStatus {
    fn sync_started(&self) {
        if let Ok(mut line) = self.0.lock() {
            *line = Some(StatusLine::start("syncing…"));
        }
    }

    fn sync_finished(&self) {
        if let Some(line) = self.0.lock().ok().and_then(|mut line| line.take()) {
            line.finish();
        }
    }
}

/// A transient stderr status line, erased when the work finishes.
pub struct StatusLine {
    active: bool,
    len: usize,
}

impl StatusLine {
    pub fn start(msg: &str) -> StatusLine {
        let active = std::io::stderr().is_terminal();
        if active {
            eprint!("{msg}");
            let _ = std::io::stderr().flush();
        }
        StatusLine {
            active,
            len: msg.chars().count(),
        }
    }

    pub fn finish(self) {
        if self.active {
            eprint!("\r{}\r", " ".repeat(self.len));
            let _ = std::io::stderr().flush();
        }
    }
}

/// A bare `[Y/n]` confirmation. Returns the default on empty input; any
/// non-tty stdin refuses (agents must go through grants, not prompts).
pub fn confirm(prompt: &str, default_yes: bool) -> anyhow::Result<bool> {
    confirm_via(prompt, default_yes, false)
}

/// Confirm with the prompt on stderr, keeping stdout for machine output.
pub fn confirm_stderr(prompt: &str, default_yes: bool) -> anyhow::Result<bool> {
    confirm_via(prompt, default_yes, true)
}

fn confirm_via(prompt: &str, default_yes: bool, stderr: bool) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "cannot confirm: stdin is not a terminal (use --yes, or give an agent a budget: sats agent grant)"
        );
    }
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    if stderr {
        eprint!("{prompt} {hint} ");
        std::io::stderr().flush()?;
    } else {
        print!("{prompt} {hint} ");
        std::io::stdout().flush()?;
    }
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(match line.trim().to_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    })
}

/// A menu selects a snapshot for review, never permission to execute it.
#[derive(Debug, PartialEq, Eq)]
pub enum ReviewChoice {
    Select(usize),
    Refresh,
    Wait,
    Cancel,
}

/// Keep selection prompts on the human channel, including in JSON mode.
pub fn review_choice(
    input: &mut impl BufRead,
    out: &mut impl Write,
    count: usize,
) -> anyhow::Result<ReviewChoice> {
    loop {
        if count > 0 {
            write!(out, "Review number (1-{count}), ")?;
        }
        write!(out, "Enter/r refresh, w wait 1s, q cancel: ")?;
        out.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(ReviewChoice::Cancel);
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" | "r" => return Ok(ReviewChoice::Refresh),
            "w" => return Ok(ReviewChoice::Wait),
            "q" => return Ok(ReviewChoice::Cancel),
            number => {
                if let Ok(number) = number.parse::<usize>()
                    && (1..=count).contains(&number)
                {
                    return Ok(ReviewChoice::Select(number - 1));
                }
                writeln!(out, "Choose a displayed number, r, w, or q.")?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;

    #[test]
    fn only_sats_warnings_are_printed() {
        let shown = |target: &str, level| {
            Warnings.enabled(&log::Metadata::builder().target(target).level(level).build())
        };
        assert!(shown("sats::store", log::Level::Warn));
        assert!(shown("sats_wallet::request", log::Level::Error));
        assert!(!shown("sats::store", log::Level::Info));
        assert!(!shown("rustls::client", log::Level::Warn));
        assert!(!shown("satsuma", log::Level::Warn));
    }

    #[test]
    fn review_requires_a_displayed_number_and_never_defaults_to_one() {
        let mut output = Vec::new();
        assert_eq!(
            review_choice(&mut &b"\n"[..], &mut output, 1).unwrap(),
            ReviewChoice::Refresh
        );
        assert_eq!(
            review_choice(&mut &b"0\n2\n1\n"[..], &mut output, 1).unwrap(),
            ReviewChoice::Select(0)
        );
        assert_eq!(
            review_choice(&mut &b"1\nq\n"[..], &mut output, 0).unwrap(),
            ReviewChoice::Cancel
        );
        assert_eq!(
            review_choice(&mut &b""[..], &mut output, 1).unwrap(),
            ReviewChoice::Cancel
        );
        assert_eq!(
            review_choice(&mut &b"w\n"[..], &mut output, 0).unwrap(),
            ReviewChoice::Wait
        );
    }
}
