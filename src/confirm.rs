//! Pre-download confirmation prompt.
//!
//! The launcher used to start downloading the moment it had a plan, which meant a
//! double-click could begin writing gigabytes into whatever folder it happened to sit
//! in before the user had read a single line. This asks first.
//!
//! Drawn on the NORMAL screen, directly under the Install/System/Channel summary, so the
//! user confirms with that context still visible; pressing Enter is what hands over to
//! the full-screen progress view.
//!
//! Falls back to "yes" whenever there is no interactive terminal (piped, CI, redirected)
//! — there is nobody to answer, and blocking forever would be worse than proceeding.

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use ratatui::crossterm::{cursor, execute, style::Stylize};
use std::io::{IsTerminal, Write};
use std::time::Duration;

/// Which question to ask. The wording differs because "Download" and "Continue" are
/// genuinely different decisions to a user staring at a 6 GB number.
pub enum Prompt {
    /// Nothing of this install exists yet.
    Fresh { dir: String, files: usize, bytes: u64 },
    /// A partial/older install is present; only the delta will be fetched.
    Resume { dir: String, files: usize, bytes: u64, have: usize },
}

impl Prompt {
    fn question(&self) -> String {
        match self {
            Prompt::Fresh { dir, .. } => format!("Download to {dir}?"),
            Prompt::Resume { .. } => "Continue download?".to_string(),
        }
    }

    fn detail(&self) -> String {
        match self {
            Prompt::Fresh { files, bytes, .. } => {
                format!("{} files · {:.2} GB to fetch", files, *bytes as f64 / 1e9)
            }
            Prompt::Resume { files, bytes, have, dir } => format!(
                "{have} file(s) already in {dir} · {} file(s) · {:.2} GB still to fetch",
                files,
                *bytes as f64 / 1e9
            ),
        }
    }

    fn accept_label(&self) -> &'static str {
        match self {
            Prompt::Fresh { .. } => "Download",
            Prompt::Resume { .. } => "Continue",
        }
    }
}

const BLOCK_LINES: u16 = 5;

/// Ask the user. Returns true to proceed with the download.
///
/// `assume_yes` short-circuits for `--yes` / non-interactive automation.
pub fn ask(p: &Prompt, assume_yes: bool) -> bool {
    if assume_yes {
        return true;
    }
    // No TTY => nobody to answer. Proceed rather than hang.
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return true;
    }

    let mut accept = true; // affirmative pre-selected, as requested
    println!();
    println!("  {}", p.question().bold());
    println!("  {}", p.detail().dark_grey());
    draw(p, accept, false);

    if enable_raw_mode().is_err() {
        // Can't read keys reliably; don't strand the user on a prompt they can't answer.
        return true;
    }

    let result = loop {
        // Poll rather than block so a resize (or a spurious wake) can't wedge us.
        if !event::poll(Duration::from_millis(200)).unwrap_or(false) {
            continue;
        }
        let ev = match event::read() {
            Ok(e) => e,
            Err(_) => break true,
        };
        let Event::Key(k) = ev else { continue };
        // Windows reports both Press and Release; acting on both double-toggles.
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match k.code {
            KeyCode::Char('c') | KeyCode::Char('C')
                if k.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                break false
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                accept = !accept;
                draw(p, accept, true);
            }
            KeyCode::Enter | KeyCode::Char(' ') => break accept,
            KeyCode::Char('y') | KeyCode::Char('Y') => break true,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') | KeyCode::Esc => {
                break false
            }
            _ => {}
        }
    };

    let _ = disable_raw_mode();
    // Redraw once un-selected-looking so the final state matches what was chosen, then
    // move below the block so subsequent output doesn't overwrite it.
    draw(p, result, true);
    println!();
    result
}

/// Render (or re-render in place) the two option rows plus the key hint.
fn draw(p: &Prompt, accept: bool, redraw: bool) {
    let mut out = std::io::stdout();
    if redraw {
        let _ = execute!(out, cursor::MoveUp(BLOCK_LINES));
    }
    let yes = format!("[ {} ]", p.accept_label());
    let no = "[ Cancel ]".to_string();

    let rows = [(true, yes), (false, no)];
    let _ = execute!(out, Clear(ClearType::CurrentLine));
    let _ = writeln!(out);
    for (is_yes, label) in rows {
        let selected = is_yes == accept;
        let _ = execute!(out, Clear(ClearType::CurrentLine));
        if selected {
            let _ = writeln!(out, "      {} {}", "▸".green().bold(), label.clone().black().on_green().bold());
        } else {
            let _ = writeln!(out, "        {}", label.dark_grey());
        }
    }
    let _ = execute!(out, Clear(ClearType::CurrentLine));
    let _ = writeln!(out);
    let _ = execute!(out, Clear(ClearType::CurrentLine));
    let _ = writeln!(
        out,
        "  {}",
        "↑/↓ or Tab to change · Enter to confirm · Esc to quit".dark_grey()
    );
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wording_differs_between_fresh_and_resume() {
        let fresh = Prompt::Fresh { dir: "C:/Games/Quakers".into(), files: 4249, bytes: 6_290_000_000 };
        let resume = Prompt::Resume { dir: "C:/Games/Quakers".into(), files: 19, bytes: 3_630_000_000, have: 4230 };
        assert_eq!(fresh.question(), "Download to C:/Games/Quakers?");
        assert_eq!(resume.question(), "Continue download?");
        assert_eq!(fresh.accept_label(), "Download");
        assert_eq!(resume.accept_label(), "Continue");
    }

    #[test]
    fn detail_reports_gigabytes_and_existing_files() {
        let fresh = Prompt::Fresh { dir: "d".into(), files: 4249, bytes: 6_290_000_000 };
        assert!(fresh.detail().contains("4249 files"));
        assert!(fresh.detail().contains("6.29 GB"));
        let resume = Prompt::Resume { dir: "d".into(), files: 19, bytes: 3_630_000_000, have: 4230 };
        assert!(resume.detail().contains("4230 file(s) already"));
        assert!(resume.detail().contains("3.63 GB"));
    }

    #[test]
    fn assume_yes_skips_the_prompt() {
        let p = Prompt::Fresh { dir: "d".into(), files: 1, bytes: 1 };
        assert!(ask(&p, true));
    }
}
