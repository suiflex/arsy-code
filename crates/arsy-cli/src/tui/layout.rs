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
        // `-iexten` as well: with it on, the macOS driver keeps ^O as its
        // DISCARD key and swallows it, so Ctrl+O never reached ARSY.
        stty(&[
            "-echo", "-icanon", "-isig", "-iexten", "min", "1", "time", "0",
        ])?;
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

/// The terminal's size as `(columns, rows)`, or `None` when nothing answers.
///
/// The controlling terminal is asked first, because that is the window the
/// operator is looking at — including a terminal the session was attached to
/// over a link, where `COLUMNS` and `LINES` are whatever the far shell had and
/// go stale on the first resize. On Unix this is one `tcgetwinsize` ioctl: no
/// subprocess to spawn, no escape sequence to write, and nothing taken out of
/// the input stream. A terminal that cannot be opened or asked — no
/// controlling terminal at all, a pipe, a platform without the ioctl — falls
/// back to `stdin`, and to `None` when that fails too, which leaves the caller
/// to its environment and defaults.
///
/// A measured zero is reported as it was read rather than treated as failure:
/// the size is one answer, and each caller decides which of its components are
/// usable.
pub(super) fn read_terminal_dimensions() -> Option<(usize, usize)> {
    #[cfg(unix)]
    {
        if let Ok(tty) = std::fs::File::open("/dev/tty") {
            if let Ok(size) = rustix::termios::tcgetwinsize(&tty) {
                return Some((usize::from(size.ws_col), usize::from(size.ws_row)));
            }
        }
        if let Ok(size) = rustix::termios::tcgetwinsize(std::io::stdin()) {
            return Some((usize::from(size.ws_col), usize::from(size.ws_row)));
        }
        None
    }
    #[cfg(not(unix))]
    {
        // `stty size` prints rows first; the columns follow it.
        let size = stty(&["size"]).ok()?;
        let mut fields = size.split_whitespace();
        let rows = fields.next()?.parse().ok()?;
        let columns = fields.next()?.parse().ok()?;
        Some((columns, rows))
    }
}
// Palette, themes and the role table now live in `arsy-tui`: they are
// presentation, and this module is terminal lifecycle. Re-exported so every
// existing `tui::Palette` and `tui::builtin_palette` keeps resolving.
pub use arsy_tui::{builtin_palette, hex_to_sgr, Palette, DEFAULT_THEME, THEMES, THEME_ROLES};
