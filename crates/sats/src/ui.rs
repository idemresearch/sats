//! Output styling: quiet, aligned, minimal color.

use std::io::{IsTerminal, Write};

use owo_colors::OwoColorize;
use sats_core::fmt::format_sats;

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
    let key_w = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (key, value) in rows {
        println!("{key:<key_w$}  {value}");
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
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "cannot confirm: stdin is not a terminal (use --yes, or give an agent a budget: sats agent grant)"
        );
    }
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{prompt} {hint} ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(match line.trim().to_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    })
}
