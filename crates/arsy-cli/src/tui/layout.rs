//! Terminal layout, palette, and sizing primitives.
use super::*;
/// Terminal modes, owned for as long as ARSY draws the composer.
///
/// The shell must stop echoing (ARSY paints the input line itself, so an echo
/// would double it and shift the block) and stop buffering lines (a key has to
/// arrive as it is pressed). `-isig` keeps Ctrl-C out of the signal path so it
/// arrives as a key and can interrupt the running turn instead of killing
/// ARSY. `stty` is used rather than `termios` because the workspace forbids
/// `unsafe_code`.
pub struct RawTerminal {
    saved: Option<String>,
}

impl RawTerminal {
    pub fn acquire() -> std::io::Result<Self> {
        let saved = stty(&["-g"])?;
        let terminal = Self {
            saved: Some(saved.trim().to_owned()),
        };
        stty(&["-echo", "-icanon", "-isig", "min", "1", "time", "0"])?;
        let mut stdout = std::io::stdout();
        write!(stdout, "\x1b[?2004h")?;
        stdout.flush()?;
        Ok(terminal)
    }
}

impl Drop for RawTerminal {
    /// Runs on every exit path, including unwind, so the shell is never left
    /// in raw mode.
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b[?2004l{RESET}");
        let _ = stdout.flush();
        let _ = match &self.saved {
            Some(mode) => stty(&[mode]),
            None => stty(&["sane"]),
        };
    }
}

pub(crate) fn stty(args: &[&str]) -> std::io::Result<String> {
    #[cfg(unix)]
    if let Ok(tty) = std::fs::File::open("/dev/tty") {
        if let Ok(output) = Command::new("stty").args(args).stdin(tty).output() {
            if output.status.success() {
                return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
            }
        }
    }
    let output = Command::new("stty")
        .args(args)
        .stdin(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(
            "stty could not configure the terminal",
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
// Palette, themes and the role table now live in `arsy-tui`: they are
// presentation, and this module is terminal lifecycle. Re-exported so every
// existing `tui::Palette` and `tui::builtin_palette` keeps resolving.
pub use arsy_tui::{builtin_palette, hex_to_sgr, Palette, DEFAULT_THEME, THEMES, THEME_ROLES};
