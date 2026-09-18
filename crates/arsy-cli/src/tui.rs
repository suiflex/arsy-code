//! Dependency-free terminal projection over canonical events.
//!
//! The visual language follows the `brainless` codex-session registry
//! (<https://brainless.swerdlow.dev>): a bordered launch card, `•` action rows
//! with a status dot and dim result, plain assistant text, and a `›` composer
//! over a warm-model / green-cwd status row.
//!
//! The public surface is a façade over focused components:
//! [`bar`] owns session chrome, [`chat`] owns input and conversation rows,
//! [`progress`] owns tool execution and provider progress, [`approval`] owns
//! decision cards, [`session`] and [`provider`] own their pickers, and
//! [`layout`] owns palette and terminal lifecycle primitives.

use arsy_kernel::{
    domain::SessionId,
    event::{EventEnvelope, EventPayload},
    policy::{ApprovalRequest, SandboxAssurance},
    provider::Effort,
};
use serde_json::Value;
use std::{
    fmt,
    io::Write,
    process::{Command, Stdio},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
mod approval;
mod bar;
mod chat;
mod keys;
mod layout;
mod mcp_dialog;
mod model;
mod progress;
mod provider;
mod session;
mod stream;
mod tool_cards;
pub use approval::*;
pub use bar::*;
pub use chat::*;
pub use keys::*;
pub(super) use layout::stty;
pub use layout::RawTerminal;
pub use layout::{builtin_palette, hex_to_sgr, Palette, DEFAULT_THEME, THEMES, THEME_ROLES};
pub use mcp_dialog::*;
pub use model::*;
pub use progress::*;
pub use provider::*;
pub use session::*;
pub use stream::*;
pub use tool_cards::*;

const MAX_TIMELINE_EVENTS: usize = 1_000;
const DEFAULT_WIDTH: usize = 80;
const DEFAULT_HEIGHT: usize = 24;
const MIN_WIDTH: usize = 20;

const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";
/// Exact-width cards must not leave a terminal's pending-wrap bit set.
pub(super) const DISABLE_AUTOWRAP: &str = "\x1b[?7l";
pub(super) const ENABLE_AUTOWRAP: &str = "\x1b[?7h";

// The renderer reads the palette through the `sgr_*` helpers below, so a theme
// swap needs no change past `activate_palette`.
//
// ponytail: a process-global, not a value threaded through every render
// function — the TUI shows one session in one theme. `activate_palette` leaks
// one `Palette` the first time each distinct palette is set, so the helpers can
// hand out `&'static str`; an unchanged palette is a no-op, so repainting the
// theme picker on every keystroke does not accumulate anything. Thread a
// `&Palette` only if a split view ever needs two themes at once.
static ACTIVE_PALETTE: std::sync::RwLock<Option<&'static Palette>> = std::sync::RwLock::new(None);

/// Make `palette` the one the renderer paints with from now on. Safe to call
/// again — on every frame, even — when `/theme` previews or changes it.
pub fn activate_palette(palette: Palette) {
    if let Ok(mut active) = ACTIVE_PALETTE.write() {
        if active.map(|current| *current == palette).unwrap_or(false) {
            return;
        }
        *active = Some(Box::leak(Box::new(palette)));
    }
}

/// Activate a built-in theme palette with optional role overrides.
pub fn set_palette(name: &str, roles: &std::collections::BTreeMap<String, String>) {
    if let Some(mut palette) = builtin_palette(name) {
        if !roles.is_empty() {
            if let Ok(overridden) = palette.clone().with_overrides(roles) {
                palette = overridden;
            }
        }
        activate_palette(palette);
    }
}

fn palette() -> &'static Palette {
    if let Some(active) = ACTIVE_PALETTE.read().ok().and_then(|active| *active) {
        return active;
    }
    static DEFAULT: std::sync::OnceLock<Palette> = std::sync::OnceLock::new();
    DEFAULT.get_or_init(|| builtin_palette(DEFAULT_THEME).expect("`dark` is built in"))
}

/// Serialise a row against the palette this session has active.
///
/// The bridge between the presentation crate, which knows what a row means,
/// and this module, which knows which theme is switched on. Every widget that
/// has moved to `arsy-tui` comes back through here, so the theme lookup stays
/// in one place rather than spreading into the crate as a global.
pub(crate) fn render_row(colour: bool, line: &arsy_tui::Line) -> String {
    line.render(palette(), colour)
}

fn sgr_assistant() -> &'static str {
    &palette().assistant
}
fn sgr_dim() -> &'static str {
    &palette().dim
}
fn sgr_accent() -> &'static str {
    &palette().accent
}
fn sgr_ok() -> &'static str {
    &palette().ok
}
fn sgr_err() -> &'static str {
    &palette().err
}
fn sgr_run() -> &'static str {
    &palette().run
}
fn sgr_model() -> &'static str {
    &palette().model
}
fn sgr_cwd() -> &'static str {
    &palette().cwd
}
fn sgr_border() -> &'static str {
    &palette().border
}
fn sgr_bullet() -> &'static str {
    &palette().bullet
}
/// Codex `user_message_bg`: white at 12% over the `#1a1a1a` terminal surface.
fn sgr_input_bg() -> &'static str {
    &palette().input_bg
}

/// `#rrggbb` to an SGR prefix — foreground, or background when `background`.
/// Take an answer to the `/theme` picker: a list number, a theme name, or an
/// empty line to keep what is set. A rejected answer reports why, like the
/// effort picker, because an accepted one is written to the user configuration.
pub fn resolve_theme_answer(line: &str, current: &str) -> Result<String, String> {
    let answer = line.trim();
    if answer.is_empty() {
        return Ok(current.to_owned());
    }
    if let Ok(number) = answer.parse::<usize>() {
        return THEMES
            .get(
                number
                    .checked_sub(1)
                    .ok_or_else(|| format!("`{answer}` is out of range; the list starts at 1"))?,
            )
            .map(|(name, _)| (*name).to_owned())
            .ok_or_else(|| format!("`{answer}` is not on the list"));
    }
    THEMES
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(answer))
        .map(|(name, _)| (*name).to_owned())
        .ok_or_else(|| {
            format!(
                "`{}` is not a theme; use {}",
                safe_text(answer),
                THEMES
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// The row the `/theme` picker opens on: the theme in force.
pub fn theme_row(current: &str) -> usize {
    THEMES
        .iter()
        .position(|(name, _)| name.eq_ignore_ascii_case(current))
        .unwrap_or(0)
}

pub fn theme_prompt(current: &str, colour: bool) -> String {
    paint(
        colour,
        sgr_dim(),
        &format!(
            "  theme [{current}] · Up/Down then Enter, a name, or 1-{}",
            THEMES.len()
        ),
    )
}
const CLEAR_EOL: &str = "\x1b[K";
#[cfg(test)]
const CARET_UP_1: &str = "\x1b[1A";
#[cfg(test)]
const CARET_UP_2: &str = "\x1b[2A";
const CLEAR_BELOW: &str = "\x1b[J";

/// Read stdin bytes on a thread, so the main loop can watch keys and provider
/// output at the same time — that is what makes a turn interruptible.
pub fn spawn_key_reader() -> std::sync::mpsc::Receiver<u8> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(4096);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut byte = [0_u8; 1];
        while std::io::Read::read(&mut stdin, &mut byte).is_ok_and(|read| read == 1) {
            if sender.send(byte[0]).is_err() {
                break;
            }
        }
    });
    receiver
}

const LABEL_WIDTH: usize = 10;

const LOGO_WIDTH: usize = 10;
const LOGO_HEIGHT: usize = 5;
/// The half-block mark gets more cells than the image, because at 10 by 5
/// it has only a hundred pixels to hold the shape and reads as noise. Six rows
/// keep the card no taller than its labels.
const BLOCK_LOGO_WIDTH: usize = 20;
const BLOCK_LOGO_HEIGHT: usize = 6;
/// Samples per side of each half-block pixel, so a pixel is lit by how much
/// of it the shape covers rather than by whether a faint edge touched it.
const BLOCK_LOGO_SAMPLES: usize = 4;
const LOGO_GAP: usize = 3;
const LOGO_SVG: &[u8] = include_bytes!("../../../assets/logo.svg");

fn logo(colour: bool) -> &'static [String] {
    static COLOUR: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    static MONOCHROME: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    if colour {
        COLOUR.get_or_init(|| render_logo(true))
    } else {
        MONOCHROME.get_or_init(|| render_logo(false))
    }
}

/// Rasterise the mark into a canvas `scale` times the `columns` by `rows`
/// cell grid it occupies.
///
/// `crop` fits the drawn shape rather than the SVG's padded view box, which
/// the half-block mark needs because it has no pixels to spend on margin.
///
/// The canvas keeps the grid's own aspect — one cell is two rows of pixels —
/// so the same geometry serves the half-block rows and the image a terminal
/// with a graphics protocol draws, and neither comes out stretched.
fn logo_pixmap(columns: usize, rows: usize, scale: u32, crop: bool) -> resvg::tiny_skia::Pixmap {
    let tree = resvg::usvg::Tree::from_data(LOGO_SVG, &resvg::usvg::Options::default())
        .expect("embedded ARSY logo must be valid SVG");
    let width = columns as u32 * scale;
    let height = (rows * 2) as u32 * scale;
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(width, height).expect("fixed logo canvas must be valid");
    let bounds = if crop {
        tree.root().abs_bounding_box()
    } else {
        tree.size()
            .to_rect(0.0, 0.0)
            .expect("logo view box must be valid")
    };
    let fit = (width as f32 / bounds.width()).min(height as f32 / bounds.height());
    let left = (width as f32 - bounds.width() * fit) / 2.0 - bounds.x() * fit;
    let top = (height as f32 - bounds.height() * fit) / 2.0 - bounds.y() * fit;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_row(fit, 0.0, 0.0, fit, left, top),
        &mut pixmap.as_mut(),
    );
    pixmap
}

/// Whether the terminal draws images through the Kitty graphics protocol.
///
/// ponytail: environment sniffing rather than the `a=q` handshake, which would
/// have to read a reply back before the key reader owns the terminal. A
/// terminal this misses draws the half-block mark, which is the old behaviour;
/// add the handshake if one worth naming turns up.
fn logo_graphics() -> bool {
    std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var("TERM").as_deref() == Ok("xterm-kitty")
        || matches!(
            std::env::var("TERM_PROGRAM").as_deref(),
            Ok("ghostty" | "WezTerm")
        )
}

/// The mark drawn into `LOGO_WIDTH` by `LOGO_HEIGHT` cells from wherever the
/// cursor stands, as Kitty graphics escapes.
///
/// The pixels travel once and every later card places the stored image, so
/// repainting the card on a resize or a mode change costs one short escape
/// rather than the whole picture again.
///
/// `C=1` leaves the cursor where it was, so the caller lays the card out in
/// text as though the mark were blank space and the image lands on top of it.
/// `q=2` silences the terminal's acknowledgement, which would otherwise reach
/// the key reader as input.
///
/// ponytail: a terminal that evicts the stored image leaves the mark blank
/// until the next run, because `q=2` also hides the error that would say so.
/// Re-transmit on a timer if that ever shows up in practice.
fn logo_graphic() -> String {
    /// Identifies the stored image. Any number does; this one is unlikely to
    /// collide with an image another program left behind.
    const IMAGE_ID: u32 = 0x4152_5359;

    static SENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let placement = format!("\x1b_Ga=p,i={IMAGE_ID},c={LOGO_WIDTH},r={LOGO_HEIGHT},C=1,q=2\x1b\\");
    if SENT.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return placement;
    }
    format!("{}{placement}", logo_transmission(IMAGE_ID))
}

/// The pixels themselves, chunked as the protocol requires.
fn logo_transmission(id: u32) -> &'static str {
    /// Pixels per cell in the rasterised canvas. Large enough that the mark is
    /// drawn from real curves rather than from the cell grid.
    const SCALE: u32 = 24;
    /// The protocol's limit on one chunk of base64 payload.
    const CHUNK: usize = 4096;

    static GRAPHIC: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    GRAPHIC.get_or_init(|| {
        let pixmap = logo_pixmap(LOGO_WIDTH, LOGO_HEIGHT, SCALE, false);
        let (width, height) = (pixmap.width(), pixmap.height());
        let mut rgba = Vec::with_capacity(pixmap.pixels().len() * 4);
        for pixel in pixmap.pixels() {
            // The protocol wants straight alpha; a pixmap holds premultiplied.
            let (red, green, blue) = logo_pixel(*pixel).unwrap_or((0, 0, 0));
            rgba.extend_from_slice(&[red, green, blue, pixel.alpha()]);
        }
        let payload = base64(&rgba);
        let mut escape = String::with_capacity(payload.len() + 256);
        let mut rest = payload.as_str();
        let mut first = true;
        while !rest.is_empty() {
            let take = rest.len().min(CHUNK);
            let (chunk, tail) = rest.split_at(take);
            let more = u8::from(!tail.is_empty());
            escape.push_str("\x1b_G");
            if first {
                escape.push_str(&format!("a=t,i={id},f=32,s={width},v={height},q=2,"));
                first = false;
            }
            escape.push_str(&format!("m={more};{chunk}\x1b\\"));
            rest = tail;
        }
        escape
    })
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let packed = group
            .iter()
            .zip([16, 8, 0])
            .fold(0u32, |packed, (byte, shift)| {
                packed | (u32::from(*byte) << shift)
            });
        let symbol = |shift: u32| char::from(ALPHABET[(packed >> shift) as usize & 0x3f]);
        // Two symbols always, then one per byte the group actually carried.
        out.push(symbol(18));
        out.push(symbol(12));
        out.push(if group.len() > 1 { symbol(6) } else { '=' });
        out.push(if group.len() > 2 { symbol(0) } else { '=' });
    }
    out
}

fn render_logo(colour: bool) -> Vec<String> {
    let samples = BLOCK_LOGO_SAMPLES;
    let pixmap = logo_pixmap(BLOCK_LOGO_WIDTH, BLOCK_LOGO_HEIGHT, samples as u32, true);
    let pixels = pixmap.pixels();
    let stride = BLOCK_LOGO_WIDTH * samples;
    // The average of one pixel's samples, lit only when the shape covers at
    // least half of it.
    let pixel = |column: usize, row: usize| {
        let sum = (row * samples..(row + 1) * samples)
            .flat_map(|y| &pixels[y * stride + column * samples..][..samples])
            .fold([0usize; 4], |[red, green, blue, alpha], sample| {
                [
                    red + usize::from(sample.red()),
                    green + usize::from(sample.green()),
                    blue + usize::from(sample.blue()),
                    alpha + usize::from(sample.alpha()),
                ]
            });
        let [red, green, blue, alpha] = sum.map(|total| (total / (samples * samples)) as u8);
        (alpha >= 128)
            .then(|| resvg::tiny_skia::PremultipliedColorU8::from_rgba(red, green, blue, alpha))
            .flatten()
            .and_then(logo_pixel)
    };
    (0..BLOCK_LOGO_HEIGHT)
        .map(|row| {
            let mut line = String::new();
            for column in 0..BLOCK_LOGO_WIDTH {
                let upper = pixel(column, row * 2);
                let lower = pixel(column, row * 2 + 1);
                line.push_str(&half_block(upper, lower, colour));
            }
            line
        })
        .collect()
}

fn logo_pixel(pixel: resvg::tiny_skia::PremultipliedColorU8) -> Option<(u8, u8, u8)> {
    let alpha = u16::from(pixel.alpha());
    (alpha >= 24).then(|| {
        let channel = |value: u8| ((u16::from(value) * 255 + alpha / 2) / alpha).min(255) as u8;
        (
            channel(pixel.red()),
            channel(pixel.green()),
            channel(pixel.blue()),
        )
    })
}

fn half_block(upper: Option<(u8, u8, u8)>, lower: Option<(u8, u8, u8)>, colour: bool) -> String {
    if !colour {
        return match (upper, lower) {
            (None, None) => " ",
            (Some(_), None) => "▀",
            (None, Some(_)) => "▄",
            (Some(_), Some(_)) => "█",
        }
        .to_owned();
    }

    match (upper, lower) {
        (None, None) => " ".to_owned(),
        (Some((r, g, b)), None) => format!("\x1b[38;2;{r};{g};{b}m▀{RESET}"),
        (None, Some((r, g, b))) => format!("\x1b[38;2;{r};{g};{b}m▄{RESET}"),
        (Some((r, g, b)), Some((br, bg, bb))) => {
            format!("\x1b[38;2;{r};{g};{b}m\x1b[48;2;{br};{bg};{bb}m▀{RESET}")
        }
    }
}

fn paint(colour: bool, code: &str, text: &str) -> String {
    let text = safe_text(text);
    if colour {
        format!("{code}{text}{RESET}")
    } else {
        text.to_owned()
    }
}

fn unwrap_shell(command: &str) -> &str {
    let Some((_, inner)) = command.split_once(" -lc ") else {
        return command;
    };
    ['"', '\'']
        .into_iter()
        .find_map(|quote| {
            inner
                .strip_prefix(quote)
                .and_then(|rest| rest.strip_suffix(quote))
        })
        .unwrap_or(inner)
}

fn first_line(text: &str) -> &str {
    text.trim().lines().next().unwrap_or_default()
}

/// The checked-out branch, read straight from `.git/HEAD`.
///
/// ponytail: a file read rather than `git rev-parse`, so the status row costs
/// no subprocess per prompt. It follows the `gitdir:` pointer a worktree or
/// submodule leaves behind, and reports a detached head as a short id. It does
/// not walk up to a parent repository: a workspace that is not itself a
/// checkout simply has no branch to show.
pub fn branch(workspace: &std::path::Path) -> Option<String> {
    let dot_git = workspace.join(".git");
    let git_dir = match std::fs::read_to_string(&dot_git) {
        Ok(pointer) => workspace.join(pointer.trim().strip_prefix("gitdir:")?.trim()),
        Err(_) => dot_git,
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let name = match head.strip_prefix("ref: refs/heads/") {
        Some(name) => name,
        // Detached: the file holds the commit id itself.
        None => head.get(..8)?,
    };
    (!name.is_empty()).then(|| safe_text(name))
}

/// `stty size` is asked first: `COLUMNS` is inherited from the shell and goes
/// stale as soon as the window is resized.
pub fn terminal_width() -> usize {
    terminal_size(1, "COLUMNS", DEFAULT_WIDTH)
}

/// Rows, read the same way and on the same schedule as the width.
pub fn terminal_rows() -> usize {
    terminal_size(0, "LINES", DEFAULT_HEIGHT)
}

fn terminal_size(field: usize, variable: &str, default: usize) -> usize {
    stty(&["size"])
        .ok()
        .and_then(|size| size.split_whitespace().nth(field)?.parse().ok())
        .or_else(|| std::env::var(variable).ok().and_then(|v| v.parse().ok()))
        .filter(|size| *size > 0)
        .unwrap_or(default)
}

/// Strip SGR escapes (`ESC [ ... m`) so padding counts printed columns only.
fn strip_sgr(text: &str) -> String {
    /// Where the scan stands. A colour ends at its `m`, but an image is an APC
    /// string — `_ ... ESC \` — whose base64 payload can hold any letter, so
    /// that one ends only at its terminator. Either can be the next thing
    /// seen, so both are tracked in one pass over the text.
    #[derive(Clone, Copy)]
    enum Scan {
        Text,
        /// An `ESC`, with the character after it still to say which kind.
        Opened,
        Colour,
        Image,
        /// An `ESC` inside an image, which closes it if a `\` follows.
        Closing,
    }

    let mut out = String::with_capacity(text.len());
    let mut scan = Scan::Text;
    for character in text.chars() {
        scan = match (scan, character) {
            (Scan::Text, '\x1b') => Scan::Opened,
            (Scan::Text, _) => {
                out.push(character);
                Scan::Text
            }
            (Scan::Opened, '_') => Scan::Image,
            // An unterminated escape drops the rest of the text, as it did
            // when this consumed the tail with an inner loop.
            (Scan::Opened | Scan::Colour, 'm') => Scan::Text,
            (Scan::Opened | Scan::Colour, _) => Scan::Colour,
            (Scan::Image | Scan::Closing, '\x1b') => Scan::Closing,
            (Scan::Closing, '\\') => Scan::Text,
            (Scan::Image | Scan::Closing, _) => Scan::Image,
        };
    }
    out
}

fn visible_len(text: &str) -> usize {
    UnicodeWidthStr::width(strip_sgr(text).as_str())
}

/// Truncate to `width` printed columns, keeping the SGR escapes that styled
/// the part that survives. A row cut by a narrow card keeps its colours.
/// Fit a path into `budget` columns by dropping leading segments: the tail is
/// what tells one checkout from another.
fn shrink_path(path: &str, budget: usize) -> String {
    if visible_len(path) <= budget {
        return path.to_owned();
    }
    let mut kept = String::new();
    for segment in path.rsplit('/').filter(|segment| !segment.is_empty()) {
        let candidate = if kept.is_empty() {
            segment.to_owned()
        } else {
            format!("{segment}/{kept}")
        };
        // Two columns are owed to the `…/` that says something was dropped.
        if visible_len(&candidate) + 2 > budget {
            break;
        }
        kept = candidate;
    }
    if kept.is_empty() {
        // Not even the last segment fits, so keep its end.
        let tail: String = path.chars().rev().take(budget.saturating_sub(1)).collect();
        return format!("…{}", tail.chars().rev().collect::<String>());
    }
    format!("…/{kept}")
}

fn fit(text: &str, width: usize) -> String {
    if visible_len(text) <= width {
        return text.to_owned();
    }
    let budget = width.saturating_sub(1);
    let mut out = String::with_capacity(text.len());
    let mut printed = 0;
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character == '\x1b' {
            out.push(character);
            for escaped in chars.by_ref() {
                out.push(escaped);
                if escaped == 'm' {
                    break;
                }
            }
            continue;
        }
        let columns = character.width().unwrap_or(0);
        if printed + columns > budget {
            break;
        }
        out.push(character);
        printed += columns;
    }
    out.push('…');
    if out.contains('\x1b') {
        out.push_str(RESET);
    }
    out
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiError {
    WrongSession,
    Gap { expected: u64, actual: u64 },
    CursorOverflow,
}

impl fmt::Display for TuiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongSession => formatter.write_str("event belongs to another session"),
            Self::Gap { expected, actual } => {
                write!(formatter, "event gap: expected {expected}, got {actual}")
            }
            Self::CursorOverflow => formatter.write_str("event cursor overflow"),
        }
    }
}

impl std::error::Error for TuiError {}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::{
        capability::{CapabilityAction, CapabilityRequirement},
        domain::{ApprovalId, CorrelationId, Principal, ResourceRef, StateVersion},
        event::{EventPayload, SchemaVersion},
        operation::OperationKind,
        policy::ApprovalRequest,
    };
    use std::time::Duration;

    #[test]
    fn theme_answers_and_overrides_resolve() {
        // Every built-in name in the picker table has a palette, and its
        // description is non-empty.
        for (name, description) in THEMES {
            assert!(builtin_palette(name).is_some(), "{name} has no palette");
            assert!(!description.is_empty());
        }
        assert!(builtin_palette("chartreuse").is_none());

        // A number, a name (any case), or an empty line to keep what is set.
        assert_eq!(resolve_theme_answer("2", "dark").unwrap(), "ocean");
        assert_eq!(resolve_theme_answer("OCEAN", "dark").unwrap(), "ocean");
        assert_eq!(resolve_theme_answer("   ", "mono").unwrap(), "mono");
        assert!(resolve_theme_answer("0", "dark").is_err());
        assert!(resolve_theme_answer("99", "dark").is_err());
        assert!(resolve_theme_answer("solarized", "dark").is_err());

        // #rrggbb (with or without the hash) becomes a truecolor prefix;
        // input_bg is a background one.
        assert_eq!(hex_to_sgr("#ff0000", false).unwrap(), "\x1b[38;2;255;0;0m");
        assert_eq!(hex_to_sgr("00ff80", true).unwrap(), "\x1b[48;2;0;255;128m");
        assert!(hex_to_sgr("#fff", false).is_err());
        assert!(hex_to_sgr("#gggggg", false).is_err());

        // Overrides replace only the named roles; an unknown role or a bad
        // colour is rejected, not ignored.
        let mut roles = std::collections::BTreeMap::new();
        roles.insert("accent".to_owned(), "#123456".to_owned());
        roles.insert("input_bg".to_owned(), "#abcdef".to_owned());
        let painted = builtin_palette("dark")
            .unwrap()
            .with_overrides(&roles)
            .unwrap();
        assert_eq!(painted.accent, "\x1b[38;2;18;52;86m");
        assert_eq!(painted.input_bg, "\x1b[48;2;171;205;239m");
        assert_eq!(
            painted.assistant,
            builtin_palette("dark").unwrap().assistant
        );

        let mut bad_role = std::collections::BTreeMap::new();
        bad_role.insert("accnt".to_owned(), "#123456".to_owned());
        assert!(builtin_palette("dark")
            .unwrap()
            .with_overrides(&bad_role)
            .is_err());

        let mut bad_hex = std::collections::BTreeMap::new();
        bad_hex.insert("accent".to_owned(), "red".to_owned());
        assert!(builtin_palette("dark")
            .unwrap()
            .with_overrides(&bad_hex)
            .is_err());
    }

    #[test]
    fn first_frame_stream_resize_no_colour_and_approval_are_complete() {
        let session = SessionId::new();
        let mut state = TuiState::new("/repo".into(), session);
        // No wall-clock assertion here: this test is about what the frame
        // contains, and a render budget measured on a shared CI runner reports
        // the runner's load, not a regression. Render cost is gated by the
        // benchmark suite in performance.yml.
        let first = state.render(80, false);
        assert!(first.contains(">_ ARSY CODE"));
        assert!(first.contains("sandbox:"));
        assert!(first.contains("none · read-only"));
        assert!(!first.contains("\x1b["));
        assert!(first.lines().all(|line| line.chars().count() == 80));
        // No model, no effort, and no checkout: the row still says what is
        // missing rather than dropping the field — and it always names the
        // approval mode, `default` included, so the mode is never something
        // the operator has to remember.
        assert_eq!(
            state.status_row(80, false, None),
            "  ✦ no model  ○ off  ⚙ manual  📁 /repo"
        );
        state.set_effort(Some(Effort::High));
        // The branch sits at the right edge, so it holds its column while the
        // fields on the left change length.
        let row = state.status_row(80, false, Some("feat/x"));
        assert!(
            row.starts_with("  ✦ no model  ● high  ⚙ manual  📁 /repo"),
            "{row:?}"
        );
        assert!(row.ends_with("⎇ feat/x"), "{row:?}");
        assert_eq!(visible_len(&row), 80, "{row:?}");
        state.set_effort(None);

        let event = EventEnvelope::new(
            session,
            1,
            Principal::System,
            None,
            CorrelationId::new(),
            SchemaVersion(1),
            "model.delta",
            EventPayload::Inline {
                data: serde_json::json!({"text": "streamed answer"}),
            },
        );
        state.apply(&event).unwrap();
        let narrow = state.render(20, false);
        assert!(narrow.contains("streamed answer"));
        assert!(narrow.lines().all(|line| line.chars().count() <= 20));

        let approval = ApprovalRequest {
            id: ApprovalId::new(),
            actor: Principal::System,
            operation: OperationKind::new("fs.write").unwrap(),
            requirement: CapabilityRequirement {
                action: CapabilityAction::FsWrite,
                resource: ResourceRef::new("file", "/repo/a").unwrap(),
            },
            operation_digest: StateVersion::from_digest([1; 32]),
            reversible: true,
            expires_at_ms: None,
            delegation_depth: 0,
            reason: "workspace rule requires consent".into(),
        };
        let prompt = TuiState::render_approval(&approval, 120);
        for label in ["Effect:", "Scope:", "Reversibility:", "Reason:"] {
            assert!(prompt.contains(label));
        }

        let wrong = EventEnvelope {
            session: SessionId::new(),
            ..event
        };
        assert_eq!(state.apply(&wrong), Err(TuiError::WrongSession));
    }

    #[test]
    fn codex_events_project_to_rows_and_never_abort_on_bad_input() {
        let row = |line: &str| render_codex_event(line, false);

        assert_eq!(
            row(r#"{"type":"turn.started"}"#),
            None,
            "transient status must not remain in scrollback"
        );
        assert_eq!(working_row(false), "  • Working…");
        assert_eq!(row(r#"{"type":"thread.started","thread_id":"t"}"#), None);
        assert_eq!(
            row(r#"{"type":"item.completed","item":{"type":"agent_message","text":"PONG\n"}}"#)
                .as_deref(),
            Some("PONG")
        );
        assert_eq!(
            row(r#"{"type":"item.completed","item":{"type":"command_execution","command":"/bin/zsh -lc \"cargo test\"","exit_code":1}}"#)
                .as_deref(),
            Some("  • Ran cargo test  → exit 1")
        );
        assert_eq!(
            row(r#"{"type":"item.completed","item":{"type":"command_execution","command":"bash -lc 'ls crates'","exit_code":0}}"#)
                .as_deref(),
            Some("  • Ran ls crates  → done")
        );
        assert_eq!(
            row(r#"{"type":"item.completed","item":{"type":"file_change","changes":[{"path":"a.rs","kind":"add"},{"path":"b.rs","kind":"update"}]}}"#)
                .as_deref(),
            Some("  • Added a.rs\n  • Edited b.rs")
        );
        assert_eq!(
            row(r#"{"type":"turn.failed","error":{"message":"x"}}"#),
            Some("  • x".into())
        );
        assert_eq!(
            row(r#"{"type":"error","message":"You've hit your usage limit."}"#).as_deref(),
            Some("  • You've hit your usage limit.")
        );
        // Codex reports some failures as an item, and provider transport
        // errors arrive as an embedded JSON body.
        assert_eq!(
            row(r#"{"type":"item.completed","item":{"type":"error","message":"Model metadata not found."}}"#)
                .as_deref(),
            Some("  • Model metadata not found.")
        );
        assert_eq!(
            row(r#"{"type":"error","message":"{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'no-such-model' model is not supported.\"}}"}"#)
                .as_deref(),
            Some("  • The 'no-such-model' model is not supported.")
        );
        // An unknown item kind still renders; malformed input renders nothing.
        assert_eq!(
            row(r#"{"type":"item.completed","item":{"type":"future_kind"}}"#).as_deref(),
            Some("  • future_kind")
        );
        for bad in ["", "not json", "{}", r#"{"type":"item.completed"}"#] {
            assert_eq!(row(bad), None, "{bad}");
        }

        // Colour output stays a single printed column count.
        let painted = working_row(true);
        assert!(painted.contains("\x1b["));
        assert_eq!(visible_len(&painted), "  • Working…".chars().count());
    }

    #[test]
    fn the_model_picker_takes_a_number_a_slug_or_the_current_default() {
        let models = [
            ModelChoice {
                provider: CODEX_PROVIDER.into(),
                slug: "gpt-5.6-sol".into(),
                name: "GPT-5.6-Sol".into(),
            },
            ModelChoice {
                provider: CODEX_PROVIDER.into(),
                slug: "gpt-5.6-luna".into(),
                name: "GPT-5.6-Luna".into(),
            },
            ModelChoice {
                provider: "hari".into(),
                slug: "mimo".into(),
                name: "on hari".into(),
            },
        ];
        let current = ModelRoute {
            provider: CODEX_PROVIDER.into(),
            model: "gpt-5.6-luna".into(),
        };
        let pick = |answer: &str| resolve_model(answer, &models, &current);

        assert_eq!(
            pick("1").unwrap(),
            ModelRoute {
                provider: CODEX_PROVIDER.into(),
                model: "gpt-5.6-sol".into(),
            }
        );
        assert_eq!(pick("").unwrap(), current, "empty keeps the current model");
        assert_eq!(
            pick("  2  ").unwrap().model,
            "gpt-5.6-luna",
            "surrounding space is ignored"
        );
        // A number answers with the row's provider, so the picker moves the
        // turn between providers as well as between models.
        assert_eq!(
            pick("3").unwrap(),
            ModelRoute {
                provider: "hari".into(),
                model: "mimo".into(),
            }
        );
        // `[provider] model` bracketed notation from the interactive picker.
        assert_eq!(
            pick("[hari] mimo").unwrap(),
            ModelRoute {
                provider: "hari".into(),
                model: "mimo".into(),
            }
        );
        // A free-text slug stays on the current provider.
        assert_eq!(pick("o3-custom").unwrap().provider, CODEX_PROVIDER);
        assert_eq!(pick("o3-custom").unwrap().model, "o3-custom");
        // Qualified provider/model names switch provider.
        assert_eq!(
            pick("openai/gpt-5.6:high").unwrap(),
            ModelRoute {
                provider: "openai".into(),
                model: "gpt-5.6:high".into(),
            }
        );

        // A rejected answer keeps the current model and says why, because an
        // accepted one is written to the user configuration and would then
        // fail every later turn in every later session.
        for (answer, expected) in [
            ("9", "choose 1-3"),
            ("0", "choose 1-3"),
            ("/model gpt-5.6-luna", "is a command"),
            ("gpt 5.6", "not a model slug"),
            ("!!", "not a model slug"),
        ] {
            let reason = pick(answer).unwrap_err();
            assert!(reason.contains(expected), "{answer}: {reason}");
        }
        assert!(validate_slug("").is_err(), "an empty slug is not a model");
        assert!(validate_slug(&"a".repeat(65)).is_err());
        // A rejection quotes the answer, so an oversized one is refused on its
        // length before any message can echo it back.
        let pasted = format!("/{}", "x ".repeat(4096));
        let reason = validate_slug(&pasted).unwrap_err();
        assert!(reason.len() < 128, "{} bytes echoed", reason.len());

        // Every provider's models are offered under their own heading, and the
        // current row is marked in place.
        let mut listing = Vec::new();
        render_model_list(&mut listing, &models, &current, false).unwrap();
        let listing = String::from_utf8(listing).unwrap();
        assert!(listing.contains("[codex]"));
        assert!(listing.contains("[hari]"));
        assert!(listing.contains("1. gpt-5.6-sol  GPT-5.6-Sol"));
        assert!(listing.contains("› 2. gpt-5.6-luna"));
        assert!(listing.contains("3. mimo  on hari"));
        assert!(model_prompt(&models, &current, false)
            .contains("model [codex/gpt-5.6-luna] · Up/Down then Enter, a name, or 1-3"));
        assert!(
            model_prompt(&[], &current, false).contains("model [codex/gpt-5.6-luna] · a slug"),
            "a configured endpoint offers no list, so it asks for a slug"
        );
    }

    #[test]
    fn a_remembered_route_names_its_provider_and_older_files_still_read() {
        let native = ModelRoute::parse("gateway/qwen3-coder");
        assert_eq!(native.provider, "gateway");
        assert_eq!(native.model, "qwen3-coder");
        assert!(!native.is_codex());
        assert_eq!(native.to_string(), "gateway/qwen3-coder");

        let legacy = ModelRoute::parse("gpt-5.6-luna");
        assert!(
            legacy.is_codex(),
            "a file written before routes named a provider meant Codex"
        );
        assert_eq!(legacy.model, "gpt-5.6-luna");
    }

    #[test]
    fn the_card_sets_the_mark_beside_its_text_and_keeps_colour_when_cut() {
        let mut state = TuiState::new(
            "/a/very/long/workspace/path/that/overflows".into(),
            SessionId::new(),
        );
        state.set_model_route(ModelRoute {
            provider: CODEX_PROVIDER.into(),
            model: "gpt-5.6-luna".into(),
        });

        let wide = state.render(92, true);
        let rows: Vec<&str> = wide.lines().collect();
        let first_mark = logo(true)
            .iter()
            .map(|row| strip_sgr(row))
            .find(|row| !row.trim().is_empty())
            .expect("rendered logo has a visible row");
        assert!(
            rows.iter().any(|row| strip_sgr(row).contains(&first_mark)),
            "card contains the rendered mark"
        );
        // Border, a blank line, then the title: whichever column is taller sets
        // the height and the other is centred against it.
        assert!(
            strip_sgr(rows[2]).contains(">_ ARSY CODE"),
            "the title leads the card"
        );
        assert_eq!(
            rows.len(),
            // model, directory, sandbox, session, the blank under the title,
            // and the title — or the taller half-block mark — inside a blank
            // line and a border each side.
            BLOCK_LOGO_HEIGHT.max(6) + 2 + 2,
            "the taller column sets the card height"
        );
        let marked = rows
            .iter()
            .position(|row| strip_sgr(row).contains(&first_mark))
            .expect("the mark is on the card");
        // Six mark rows against six label rows: the mark starts level with
        // the title, inside the blank line.
        assert_eq!(marked, 2, "the mark sits beside the labels");
        for row in &rows {
            assert_eq!(visible_len(row), 92, "every row still reaches the border");
        }

        // A cut row keeps the styling of the part that survived.
        let cut = fit(&paint(true, sgr_cwd(), "/a/very/long/path"), 8);
        assert_eq!(visible_len(&cut), 8);
        assert!(cut.starts_with(sgr_cwd()));
        assert!(cut.ends_with(RESET));
        assert!(cut.contains('…'));

        // Too narrow for both: the text wins, the mark is dropped.
        let narrow = state.render(32, true);
        assert!(!strip_sgr(&narrow).contains(&first_mark));
        assert!(strip_sgr(&narrow).contains(">_ ARSY CODE"));
    }

    #[test]
    fn an_image_mark_is_measured_as_blank_and_travels_once() {
        // The payload is base64, so the terminator has to close the escape;
        // stopping at the first `m` would leave part of it counted as text.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"ARSY"), "QVJTWQ==");

        let image = "\x1b_Ga=p,i=1,c=2,r=1,C=1,q=2;bWFtbWFt\x1b\\";
        assert_eq!(visible_len(&format!("{image}  ")), 2);
        assert_eq!(strip_sgr(&format!("{image}ok")), "ok");

        // The pixels travel with the first card and no other, so repainting
        // the card on a mode change costs one short placement.
        let first = logo_graphic();
        let second = logo_graphic();
        assert!(first.contains("a=t,i="), "the first card carries the image");
        assert!(second.starts_with("\x1b_Ga=p,i="), "{second}");
        assert!(second.len() < first.len() / 100, "{}", second.len());
    }

    #[test]
    fn the_launch_card_goes_stale_when_the_model_changes() {
        let mut state = TuiState::new("/w".into(), SessionId::new());
        assert!(state.card_is_stale(), "the card has never been drawn");
        assert!(!state.card_is_stale(), "nothing changed since");

        // Shift+Tab cycles the mode; the status row names it, so the card
        // is not reprinted for it.
        state.set_approval_mode("plan");
        assert!(!state.card_is_stale(), "the mode is not a card field");

        state.set_model_route(ModelRoute {
            provider: CODEX_PROVIDER.into(),
            model: "gpt-5.6-luna".into(),
        });
        assert!(state.card_is_stale(), "the model the card names changed");
        assert!(!state.card_is_stale());

        // The effort is a status-row field, not a card field.
        state.set_effort(Some(Effort::High));
        assert!(!state.card_is_stale());
    }

    #[test]
    fn keys_decode_utf8_escape_sequences_and_control_characters() {
        let mut keys = Keys::default();
        let feed = |keys: &mut Keys, bytes: &[u8]| -> Vec<Key> {
            bytes.iter().filter_map(|byte| keys.feed(*byte)).collect()
        };

        assert_eq!(feed(&mut keys, b"hi"), [Key::Char('h'), Key::Char('i')]);
        // A multi-byte character is held back until it is complete.
        assert_eq!(keys.feed(0xc3), None);
        assert_eq!(keys.feed(0xa9), Some(Key::Char('é')));

        assert_eq!(feed(&mut keys, b"\r"), [Key::Enter]);
        assert_eq!(feed(&mut keys, b"\x7f"), [Key::Backspace]);
        assert_eq!(feed(&mut keys, &[0x03]), [Key::Interrupt]);
        assert_eq!(feed(&mut keys, &[0x04]), [Key::Eof]);
        assert_eq!(feed(&mut keys, b"\x1b[D"), [Key::Left]);
        assert_eq!(feed(&mut keys, b"\x1b[C"), [Key::Right]);
        assert_eq!(feed(&mut keys, b"\x1b[H"), [Key::Home]);
        assert_eq!(feed(&mut keys, b"\x1b[F"), [Key::End]);
        assert_eq!(feed(&mut keys, b"\x1b[5~"), [Key::PageUp]);
        assert_eq!(feed(&mut keys, b"x"), [Key::Char('x')], "decoder recovers");

        // Escape only becomes Interrupt once nothing follows it.
        assert_eq!(keys.feed(0x1b), None);
        assert_eq!(keys.flush_escape(), Some(Key::Interrupt));
        assert_eq!(keys.flush_escape(), None, "only fires once");
    }

    #[test]
    fn paste_history_delete_and_wide_input_remain_editable() {
        let mut keys = Keys::default();
        let mut composer = Composer::default();
        for byte in b"\x1b[200~first\nsecond\x03\x1b[201~" {
            if let Some(key) = keys.feed(*byte) {
                assert_ne!(key, Key::Enter);
                assert_ne!(key, Key::Interrupt);
                composer.press(key);
            }
        }
        assert_eq!(
            composer.press(Key::Enter),
            Action::Submit("first second".into())
        );
        composer.press(Key::Char('x'));
        composer.press(Key::Up);
        assert_eq!(composer.buffer, "first second");
        composer.press(Key::Down);
        assert_eq!(composer.buffer, "x");
        composer.press(Key::Home);
        composer.press(Key::Delete);
        assert_eq!(composer.buffer, "");
        composer.restore("界界界界界界界界界界".into());
        let (text, caret) = composer.window(10);
        assert!(UnicodeWidthStr::width(text.as_str()) < 10);
        assert_eq!(caret, UnicodeWidthStr::width(text.as_str()));
        let frame = composer.render(
            20,
            false,
            "status that is much longer than the terminal window",
        );
        assert!(frame.split('\n').nth(1).unwrap().width() < 20);
        assert_eq!(
            safe_text("hello\x1b]52;c;clipboard\x07\rworld"),
            "hello]52;c;clipboardworld"
        );
        keys.feed(0xc3);
        assert_eq!(keys.feed(b'a'), Some(Key::Char('a')));
        for byte in b"\x1b[123" {
            keys.feed(*byte);
        }
        keys.flush_escape();
        assert_eq!(keys.feed(b'b'), Some(Key::Char('b')));
    }

    #[test]
    fn mcp_progress_and_failures_are_visible_and_provider_frames_are_bounded() {
        let progress = render_codex_event(r#"{"type":"item.started","item":{"type":"mcp_tool_call","server":"docs","tool":"search","status":"in_progress"}}"#, false).unwrap();
        assert!(progress.contains("docs.search"));
        assert!(progress.contains("running"));
        let failure = render_codex_event(r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"docs","tool":"search","status":"failed","error":{"message":"connection lost"}}}"#, false).unwrap();
        assert!(failure.contains("connection lost"));
        let events = provider_lines(std::io::Cursor::new(vec![b'x'; 1_048_577]));
        assert!(events
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("1 MiB"));
        let events = provider_lines(std::io::Cursor::new(b"{}\n\xff\n"));
        assert_eq!(
            events
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap(),
            "{}\n"
        );
        assert!(events
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn provider_ignoring_termination_can_be_forced_and_reaped() {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let child = Command::new("sh")
            .args([
                "-c",
                "trap '' TERM; printf 'ready\\n'; while :; do sleep 1; done",
            ])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = ProviderChild(child);
        let mut ready = String::new();
        std::io::BufReader::new(child.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        child.stop(false);
        assert!(child.0.try_wait().unwrap().is_none());
        child.stop(true);
        assert!(!child.0.wait().unwrap().success());
    }

    #[test]
    fn a_slash_opens_a_command_menu_that_arrows_select_and_enter_takes() {
        let mut composer = Composer::default();
        composer.history.push_back("an earlier task".into());
        assert!(composer.menu().is_empty(), "a task line offers no menu");

        composer.press(Key::Char('/'));
        assert_eq!(composer.menu().len(), COMMANDS.len(), "`/` offers them all");

        // Up/Down move the selection, and history stays out of the way while
        // the menu is the list in front of the reader.
        assert_eq!(composer.press(Key::Down), Action::Redraw);
        assert_eq!(composer.press(Key::Down), Action::Redraw);
        assert_eq!(composer.press(Key::Up), Action::Redraw);
        assert_eq!(composer.selected, 1);
        assert_eq!(composer.buffer, "/", "history did not replace the line");
        for _ in 0..COMMANDS.len() - 2 {
            composer.press(Key::Down);
        }
        assert_eq!(
            composer.selected,
            COMMANDS.len() - 1,
            "the selection reaches the last row"
        );

        // Both ends wrap, so neither direction is a dead end.
        composer.press(Key::Down);
        assert_eq!(composer.selected, 0, "the last row wraps to the first");
        composer.press(Key::Up);
        assert_eq!(
            composer.selected,
            COMMANDS.len() - 1,
            "the first row wraps to the last"
        );
        composer.press(Key::Up);

        // Enter takes the highlighted command; a second Enter sends it, so a
        // line that is already a command is never held back.
        let highlighted = COMMANDS[COMMANDS.len() - 2].0;
        assert_eq!(composer.press(Key::Enter), Action::Redraw);
        assert_eq!(composer.buffer, highlighted);
        assert_eq!(
            composer.press(Key::Enter),
            Action::Submit(highlighted.to_owned())
        );

        // Typing narrows the list; an argument closes it, so Enter sends the
        // whole line instead of completing the command again.
        for character in "/mo".chars() {
            composer.press(Key::Char(character));
        }
        assert_eq!(
            composer.menu(),
            vec![("/model".to_owned(), "choose the provider model".to_owned())]
        );
        for character in " x".chars() {
            composer.press(Key::Char(character));
        }
        assert!(composer.menu().is_empty(), "an argument closes the menu");
        assert_eq!(
            composer.press(Key::Enter),
            Action::Submit("/mo x".to_owned())
        );

        // The picker collects an answer, not a command, so it offers no menu.
        composer.set_picking(true);
        composer.press(Key::Char('/'));
        assert!(composer.menu().is_empty(), "the picker offers no commands");
        assert_eq!(composer.press(Key::Enter), Action::Submit("/".to_owned()));

        // `/help` and the menu are the same table, so neither can list a
        // command the other does not.
        let help = help(false);
        for (name, description) in COMMANDS {
            assert!(help.contains(name), "{name} is missing from /help");
            assert!(help.contains(description), "{name} has no description");
        }
        assert!(help.contains("Up/Down: input history"));
        // The keybinding list is where a shortcut is discovered, so a binding
        // the composer answers has to be named there.
        assert!(help.contains("Shift+Tab: step the approval mode"), "{help}");
    }

    /// A typed credential must not survive anywhere a later keystroke or a
    /// scrollback search could reach it.
    #[test]
    fn a_masked_line_is_not_painted_not_remembered_and_offers_no_menu() {
        let mut composer = Composer::default();
        composer.history.push_back("an earlier task".into());
        composer.set_masked(true);

        for character in "sk-secret".chars() {
            composer.press(Key::Char(character));
        }
        let frame = composer.render(80, false, "  status");
        assert!(!frame.contains("sk-secret"), "the secret was painted");
        assert!(!frame.contains("sk-"), "part of the secret was painted");
        assert!(frame.contains("•••••••••"), "one bullet per character");

        // A `/` in a secret is a character, not the start of a command.
        composer.press(Key::Char('/'));
        assert!(
            composer.menu().is_empty(),
            "a secret opened the command menu"
        );

        // The line still submits its real value, and leaves no copy behind.
        assert_eq!(
            composer.press(Key::Enter),
            Action::Submit("sk-secret/".to_owned())
        );
        assert_eq!(
            composer.history.len(),
            1,
            "the secret entered history: {:?}",
            composer.history
        );
        assert_eq!(
            composer.history.back().map(String::as_str),
            Some("an earlier task")
        );

        // Unmasking is what returns the line to ordinary behaviour.
        composer.set_masked(false);
        for character in "hello".chars() {
            composer.press(Key::Char(character));
        }
        assert!(composer.render(80, false, "  status").contains("hello"));
        assert_eq!(
            composer.press(Key::Enter),
            Action::Submit("hello".to_owned())
        );
        assert_eq!(composer.history.back().map(String::as_str), Some("hello"));
    }

    #[test]
    fn a_narrow_status_row_gives_up_the_path_before_the_branch() {
        let session = SessionId::new();
        let mut state = TuiState::new(
            "/Users/someone/Development/github/acme/arsy-code".into(),
            session,
        );
        state.set_model_route(ModelRoute::parse("myai/suiflex"));
        state.set_effort(Some(Effort::High));

        // Wide: everything, with the branch at the right edge.
        let wide = state.status_row(120, false, Some("feat/slash-menu"));
        assert!(wide.contains("/Users/someone/Development"), "{wide:?}");
        assert!(wide.ends_with("feat/slash-menu"), "{wide:?}");
        assert_eq!(visible_len(&wide), 120, "{wide:?}");

        // Narrower: the path loses its leading segments, the branch stays whole.
        let middle = state.status_row(72, false, Some("feat/slash-menu"));
        assert!(middle.contains("…/"), "{middle:?}");
        assert!(!middle.contains("/Users/someone"), "{middle:?}");
        assert!(middle.ends_with("feat/slash-menu"), "{middle:?}");
        assert!(visible_len(&middle) <= 72, "{middle:?}");

        // Narrower still: the path goes entirely, and then the branch — never
        // cut, always whole or absent. The approval mode outlives both, because
        // it is the field that says what will run without asking.
        let narrow = state.status_row(48, false, Some("feat/slash-menu"));
        assert!(!narrow.contains("arsy-code"), "{narrow:?}");
        assert!(narrow.contains("⚙ manual"), "{narrow:?}");
        assert!(visible_len(&narrow) <= 48, "{narrow:?}");

        let tiny = state.status_row(30, false, Some("feat/slash-menu"));
        assert!(!tiny.contains("feat/"), "{tiny:?}");
        assert!(visible_len(&tiny) <= 30, "{tiny:?}");

        // Every width in between stays inside the terminal.
        for width in 20..=120 {
            let row = state.status_row(width, false, Some("feat/slash-menu"));
            assert!(
                visible_len(&row) <= width.max(MIN_WIDTH),
                "width {width}: {row:?}"
            );
        }
    }

    #[test]
    fn shift_tab_is_decoded_and_asks_the_composer_for_the_next_approval_mode() {
        for sequence in [b"\x1b[Z".as_slice(), b"\x1b[1;2Z".as_slice()] {
            let mut keys = Keys::default();
            let decoded: Vec<Key> = sequence
                .iter()
                .filter_map(|byte| keys.feed(*byte))
                .collect();
            assert_eq!(decoded, vec![Key::CycleMode], "{sequence:?}");
        }

        // The drafted line survives because mode changes are distinct from
        // submissions.
        let mut composer = Composer::default();
        for key in "write the parser".chars().map(Key::Char) {
            composer.press(key);
        }
        assert_eq!(composer.press(Key::CycleMode), Action::CycleMode);
        assert_eq!(
            composer.press(Key::Enter),
            Action::Submit("write the parser".to_owned())
        );

        // A picker is collecting an answer, not a task: Shift+Tab is ignored
        // there rather than changing the approval mode mid-selection.
        composer.set_picking(true);
        assert_eq!(composer.press(Key::CycleMode), Action::None);
    }

    #[test]
    fn plan_mode_is_visible_in_the_status_row_not_the_card() {
        let mut state = TuiState::new("/workspace".into(), SessionId::new());
        state.set_approval_mode("plan");

        let launch = state.render(80, false);
        assert!(!launch.contains("mode:"), "{launch}");
        assert!(state.status_row(80, false, None).contains("⏸ PLAN"));
    }

    #[test]
    fn the_menu_extends_the_block_and_the_caret_still_lands_on_the_input() {
        let mut composer = Composer::default();
        composer.press(Key::Char('/'));
        let frame = composer.render(80, false, "  status");
        let rows: Vec<&str> = frame.split('\n').collect();
        assert_eq!(
            rows.len(),
            // The menu is windowed: a table longer than the window shows the
            // window, not the table.
            4 + COMMANDS.len().min(MENU_ROWS),
            "pad, input, pad, one row per visible command, status"
        );
        assert!(
            rows[3].contains(&format!("› {}", COMMANDS[0].0)),
            "{:?}",
            rows[3]
        );
        assert!(rows[4].starts_with("    "), "only one row is marked");
        assert!(
            rows.last().unwrap().contains("status"),
            "status is at the bottom"
        );
        for row in &rows {
            assert!(visible_len(row) <= 80, "{row:?}");
        }
        // Up over the bottom pad, the menu, and the status row, then across `› /`.
        assert!(
            frame.ends_with(&format!(
                "\x1b[{}A\r\x1b[3C",
                COMMANDS.len().min(MENU_ROWS) + 2
            )),
            "{frame:?}"
        );

        // A block taller than the screen would scroll, and the caret count back
        // to the input row would then land on the wrong one, so the menu takes
        // only the rows the terminal has left after pad, input, pad and status.
        composer.set_height(7);
        assert_eq!(composer.menu_window().0.len(), 3);
        assert_eq!(
            composer.render(80, false, "  status").split('\n').count(),
            7
        );
        composer.set_height(4);
        assert!(
            composer.menu_window().0.is_empty(),
            "no room leaves no menu"
        );
        let frame = composer.render(80, false, "  status");
        assert_eq!(frame.split('\n').count(), 4);
        assert!(frame.ends_with("\x1b[2A\r\x1b[3C"), "{frame:?}");
        composer.set_height(0);

        // A narrowed list shrinks the block, and the previous one is erased
        // from the row above the input whatever height it had.
        composer.press(Key::Char('q'));
        let frame = composer.render(80, false, "  status");
        assert!(frame.starts_with(&format!("{RESET}{CARET_UP_1}\r{CLEAR_BELOW}")));
        assert_eq!(
            frame.split('\n').count(),
            5,
            "pad, input, pad, /quit, status"
        );
        assert!(frame.ends_with("\x1b[3A\r\x1b[4C"), "{frame:?}");
    }

    #[test]
    fn the_composer_edits_a_line_and_repaints_a_block_of_known_height() {
        let mut composer = Composer::default();
        for key in [Key::Char('a'), Key::Char('c')] {
            assert_eq!(composer.press(key), Action::Redraw);
        }
        composer.press(Key::Left);
        composer.press(Key::Char('b'));
        assert_eq!(composer.press(Key::Enter), Action::Submit("abc".into()));

        // Ctrl-C drops a drafted line; only an empty line ends the session.
        composer.press(Key::Char('x'));
        assert_eq!(
            composer.press(Key::Eof),
            Action::None,
            "Ctrl-D must not discard a drafted line"
        );
        assert_eq!(composer.press(Key::Interrupt), Action::Redraw);
        assert_eq!(composer.press(Key::Interrupt), Action::Quit);
        assert_eq!(composer.press(Key::Eof), Action::Quit);
        // Editing past either end is a no-op, never a panic.
        assert_eq!(composer.press(Key::Backspace), Action::None);
        assert_eq!(composer.press(Key::Left), Action::None);
        assert_eq!(composer.press(Key::Right), Action::None);

        let plain = composer.render(80, false, "  status");
        assert!(!plain.starts_with('\x1b'), "the first frame erases nothing");
        assert_eq!(plain.split('\n').count(), 4, "pad, input, pad, status");
        assert!(
            plain.ends_with("\x1b[2A\r\x1b[2C"),
            "caret returns to input"
        );

        // Every later frame erases the previous block from its first row.
        let painted = composer.render(80, true, "  status");
        assert!(painted.starts_with(&format!("{RESET}{CARET_UP_1}\r{CLEAR_BELOW}")));
        assert_eq!(painted.matches(sgr_input_bg()).count(), 4);
        assert_eq!(
            composer.clear(),
            format!("{RESET}{CARET_UP_1}\r{CLEAR_BELOW}")
        );
        assert_eq!(composer.clear(), "", "nothing is drawn twice");

        // A line longer than the row scrolls instead of wrapping, because a
        // wrap would add a row the block does not account for.
        let mut long = Composer::default();
        for character in "0123456789abcdefghijklmnopqrst".chars() {
            long.press(Key::Char(character));
        }
        let frame = long.render(MIN_WIDTH, false, "");
        let input = frame.split('\n').nth(1).unwrap();
        assert!(visible_len(input) <= MIN_WIDTH, "{input:?}");
        assert!(input.ends_with('t'), "the caret end stays visible");
        assert!(!input.contains('0'), "the start scrolled away");

        // Home scrolls the other way, back to the start of the line.
        long.press(Key::Home);
        let frame = long.render(MIN_WIDTH, false, "");
        let input = frame.split('\n').nth(1).unwrap();
        assert!(visible_len(input) <= MIN_WIDTH, "{input:?}");
        assert!(input.starts_with("› 0"), "{input:?}");
        assert!(!input.contains('t'), "the far end scrolled away");
        assert!(
            frame.ends_with("\x1b[2A\r\x1b[2C"),
            "caret sits at column 0"
        );
    }

    #[test]
    fn render_turn_puts_loading_at_top_and_footer_at_bottom() {
        let mut composer = Composer::default();
        let frame = composer.render_turn(
            80,
            false,
            "  ⠋ Working… · 3s · Esc cancel",
            "  hari/mimo  effort:low  /workspace  main",
        );
        let rows: Vec<&str> = frame.split('\n').collect();
        assert_eq!(rows.len(), 5, "loading, pad, input, pad, footer");
        assert!(rows[0].contains("Working…"), "loading is at the top");
        assert!(rows[2].contains("›"), "input row is on line 3");
        assert!(rows[4].contains("hari/mimo"), "footer is at the bottom");
        assert!(
            frame.ends_with("\x1b[2A\r\x1b[2C"),
            "caret returns to line 3"
        );
        assert_eq!(
            composer.clear(),
            format!("{RESET}{CARET_UP_2}\r{CLEAR_BELOW}"),
            "clear moves up 2 lines when loading is at the top"
        );
    }

    #[test]
    fn thinking_box_renders_bordered_and_fitted_lines() {
        let box_out = thinking_box(80, false, "first thought\nsecond thought that is longer");
        let lines: Vec<&str> = box_out.lines().collect();
        assert_eq!(lines.len(), 4, "top, row 1, row 2, bottom");
        assert!(lines[0].contains("✻ Thinking"));
        assert!(lines[0].starts_with("╭──"));
        assert!(lines[0].ends_with('╮'));
        assert!(lines[1].starts_with("│ "));
        assert!(lines[1].contains("first thought"));
        assert!(lines[1].ends_with(" │"));
        assert!(lines[2].contains("second thought"));
        assert!(lines[3].starts_with('╰'));
        assert!(lines[3].ends_with('╯'));
        for line in &lines {
            assert_eq!(visible_len(line), 80, "{line:?}");
        }
    }

    #[test]
    fn shift_enter_and_multiline_composer_input() {
        let mut keys = Keys::default();
        // Alt+Enter / Option+Enter (\x1b\r)
        assert_eq!(keys.feed(0x1b), None);
        assert_eq!(keys.feed(b'\r'), Some(Key::Newline));

        // CSI u Shift+Enter (\x1b[13;2u)
        for b in b"\x1b[13;2" {
            assert_eq!(keys.feed(*b), None);
        }
        assert_eq!(keys.feed(b'u'), Some(Key::Newline));

        // xterm Shift+Enter (\x1b[27;2;13~)
        for b in b"\x1b[27;2;13" {
            assert_eq!(keys.feed(*b), None);
        }
        assert_eq!(keys.feed(b'~'), Some(Key::Newline));

        let mut composer = Composer::default();
        for ch in "first".chars() {
            composer.press(Key::Char(ch));
        }
        composer.press(Key::Newline);
        for ch in "second".chars() {
            composer.press(Key::Char(ch));
        }
        assert_eq!(composer.buffer, "first\nsecond");

        let frame = composer.render(80, false, "  status");
        let rows: Vec<&str> = frame.split('\n').collect();
        assert_eq!(rows.len(), 5, "top pad, line 1, line 2, bottom pad, status");
        assert!(rows[1].contains("› first"));
        assert!(rows[2].contains("· second"));

        let committed = composer.commit("first\nsecond", false);
        assert!(committed.contains("› You first\n· second\n"));
    }

    #[test]
    fn option_and_command_arrow_word_navigation() {
        let mut keys = Keys::default();
        // Option+Left (ESC b)
        assert_eq!(keys.feed(0x1b), None);
        assert_eq!(keys.feed(b'b'), Some(Key::WordLeft));

        // Option+Right (ESC f)
        assert_eq!(keys.feed(0x1b), None);
        assert_eq!(keys.feed(b'f'), Some(Key::WordRight));

        // Option+Backspace (ESC DEL)
        assert_eq!(keys.feed(0x1b), None);
        assert_eq!(keys.feed(0x7f), Some(Key::WordBackspace));

        // Ctrl+W
        assert_eq!(keys.feed(0x17), Some(Key::WordBackspace));

        // xterm Alt+Left (\x1b[1;3D)
        for b in b"\x1b[1;3" {
            assert_eq!(keys.feed(*b), None);
        }
        assert_eq!(keys.feed(b'D'), Some(Key::WordLeft));

        // Command+Left / Home (\x1b[1;9D)
        for b in b"\x1b[1;9" {
            assert_eq!(keys.feed(*b), None);
        }
        assert_eq!(keys.feed(b'D'), Some(Key::Home));

        let mut composer = Composer::default();
        for ch in "hello world arsy".chars() {
            composer.press(Key::Char(ch));
        }
        assert_eq!(composer.caret, 16);

        // WordLeft moves back by word
        composer.press(Key::WordLeft);
        assert_eq!(composer.caret, 12); // start of "arsy"

        composer.press(Key::WordLeft);
        assert_eq!(composer.caret, 6); // start of "world"

        // WordRight moves forward by word
        composer.press(Key::WordRight);
        assert_eq!(composer.caret, 12); // start of "arsy"

        // WordBackspace deletes word backward
        composer.press(Key::WordBackspace);
        assert_eq!(composer.buffer, "hello arsy");
        assert_eq!(composer.caret, 6);
    }

    #[test]
    fn ask_dialog_interactive_navigation_and_selection() {
        let mut dialog = AskDialogState::for_approval(
            "bash",
            "rm -rf target",
            "file deletion",
            Some("$ rm -rf target".to_owned()),
        );
        assert_eq!(dialog.selected, 0);
        assert_eq!(dialog.options.len(), 3);

        // Render output has border, title, and diff preview
        let rendered = dialog.render(80, false);
        assert!(rendered.contains("APPROVAL REQUIRED: bash"));
        assert!(rendered.contains("Summary: rm -rf target"));
        assert!(rendered.contains("Proposed Changes:"));
        assert!(rendered.contains("$ rm -rf target"));
        assert!(rendered.contains("1. Approve this call once"));

        // Down key navigates to next option
        assert_eq!(dialog.handle_key(Key::Down), None);
        assert_eq!(dialog.selected, 1);

        // Number 1 key immediately approves once
        assert_eq!(
            dialog.handle_key(Key::Char('1')),
            Some(AskDialogResult::Approve { note: None })
        );

        // Number 2 key always approves for session
        assert_eq!(
            dialog.handle_key(Key::Char('2')),
            Some(AskDialogResult::AlwaysApprove { note: None })
        );

        // Number 3 key denies
        assert_eq!(
            dialog.handle_key(Key::Char('3')),
            Some(AskDialogResult::Deny { note: None })
        );

        // 'n' opens custom note editing
        assert_eq!(dialog.handle_key(Key::Char('n')), None);
        assert!(dialog.editing_note);
        dialog.handle_key(Key::Char('a'));
        dialog.handle_key(Key::Char('b'));
        assert_eq!(dialog.custom_note, "ab");
        assert_eq!(dialog.handle_key(Key::Enter), None);
        assert!(!dialog.editing_note);
        assert_eq!(
            dialog.handle_key(Key::Char('1')),
            Some(AskDialogResult::Approve {
                note: Some("ab".to_owned())
            })
        );
    }

    #[test]
    fn plan_dialog_offers_implement_revise_and_cancel() {
        let mut dialog = AskDialogState::for_plan("1 of 1 step(s) done\n  > [~] fix the TUI");
        let rendered = dialog.render(80, false);
        assert!(rendered.contains("PLAN READY"));
        assert!(rendered.contains("Plan preview:"));
        assert!(rendered.contains("Approve and implement"));
        assert!(rendered.contains("Continue planning / revise"));
        assert!(rendered.contains("Cancel planning"));
        assert_eq!(
            dialog.handle_key(Key::Char('r')),
            Some(AskDialogResult::AlwaysApprove { note: None })
        );
        assert_eq!(
            dialog.handle_key(Key::Char('c')),
            Some(AskDialogResult::Deny { note: None })
        );
    }
    #[test]
    fn plan_dialog_scrolls_the_full_preview_and_exits_on_mode_change() {
        let preview = (1..=20)
            .map(|index| format!("step {index}: inspect the next boundary"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut dialog = AskDialogState::for_plan(preview);
        dialog.set_preview_height(3);

        let first = dialog.render(80, false);
        assert!(first.contains("step 1:"));
        assert!(!first.contains("step 20:"));
        assert!(first.contains("PgUp/PgDn scroll"));

        assert_eq!(dialog.handle_key(Key::PageDown), None);
        let middle = dialog.render(80, false);
        assert!(!middle.contains("step 1:"));
        assert!(middle.contains("step 4:"));

        assert_eq!(dialog.handle_key(Key::End), None);
        let last = dialog.render(80, false);
        assert!(last.contains("step 20:"));
        assert_eq!(
            dialog.handle_key(Key::CycleMode),
            Some(AskDialogResult::CycleMode)
        );
    }

    #[test]
    fn semantic_transcript_repaints_cards_at_the_current_terminal_width() {
        let mut transcript = Transcript::default();
        transcript.push_user("run cargo test");
        transcript.push_tool("bash", "cargo test", "ok", true, Duration::from_millis(12));
        let state = TuiState::new("/workspace".into(), SessionId::new());
        let mut output = std::io::Cursor::new(Vec::new());

        transcript.repaint(&mut output, 40, false, &state).unwrap();
        let text = String::from_utf8(output.into_inner()).unwrap();
        assert!(text.contains("› You run cargo test"));
        assert!(!text.contains("✦ Response"));
        let card_line = text
            .lines()
            .find(|line| line.contains("$ cargo test"))
            .expect("replayed transcript contains the tool card");
        let card_line = card_line
            .strip_prefix(DISABLE_AUTOWRAP)
            .unwrap_or(card_line);
        assert_eq!(UnicodeWidthStr::width(card_line), 40, "{card_line}");
    }

    #[test]
    fn execution_boxes_render_cleanly() {
        let bash = bash_box(
            80,
            false,
            "cargo build",
            "Finished dev profile",
            Some(0),
            Duration::from_millis(150),
        );
        assert!(bash.contains("$ cargo build"));
        assert!(bash.contains("Finished dev profile"));
        assert!(bash.contains("✓ done (150ms)"));

        let tool = tool_box(
            80,
            false,
            "fs.write",
            "src/main.rs",
            "wrote 10 lines",
            true,
            Duration::from_millis(20),
        );
        assert!(tool.contains("fs.write src/main.rs"));
        assert!(tool.contains("✓ completed (20ms)"));

        let diff = diff_row(false, "src/lib.rs", 12, 3);
        assert!(diff.contains("src/lib.rs"));
        assert!(diff.contains("+12"));
        assert!(diff.contains("-3"));
        assert_eq!(tool_card_kind("bash"), ToolCardKind::Bash);
        assert_eq!(tool_card_kind("fs.edit"), ToolCardKind::File);
        assert_eq!(tool_card_kind("curl"), ToolCardKind::Network);
        assert_eq!(tool_card_kind("mcp.search"), ToolCardKind::Mcp);
        assert_eq!(tool_card_kind("search.text"), ToolCardKind::Search);
        let (bash_acc, bash_brd) = tool_card_colors(ToolCardKind::Bash, true);
        assert!(!bash_acc.is_empty());
        assert!(!bash_brd.is_empty());
        assert_eq!(tool_card_colors(ToolCardKind::Bash, false), ("", ""));

        let vivid = builtin_palette("vivid").expect("vivid theme is built in");
        assert_eq!(vivid.accent, "\x1b[38;2;88;166;255m");
        let dracula = builtin_palette("dracula").expect("dracula theme is built in");
        assert_eq!(dracula.accent, "\x1b[38;2;189;147;249m");
        let nord = builtin_palette("nord").expect("nord theme is built in");
        assert_eq!(nord.accent, "\x1b[38;2;136;192;208m");
        let running = tool_running_frame_with_output(
            false,
            "⠋",
            "bash",
            "cargo test",
            420,
            "line one\nline two",
            false,
        );
        assert!(running.contains("line two"));
        assert!(running.contains("e expand"));
        let expanded = tool_running_frame_with_output(
            false,
            "⠙",
            "bash",
            "cargo test",
            840,
            "line one\nline two",
            true,
        );
        assert!(expanded.contains("line one"));

        let state = RunningToolState {
            name: "bash",
            summary: "cargo test",
            frame: "⠋",
            elapsed_ms: 120,
            live_output: "running test",
            expanded: false,
        };
        let running_box = tool_running_box(80, false, &state);
        assert_eq!(running_box.len(), 3);
        assert!(running_box[0].contains("$ cargo test"));
        assert!(running_box[1].contains("running (120ms)"));
        assert!(running_box[2].contains("[e: expand]"));

        let long_output = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let bounded_box = bash_box(
            80,
            false,
            "test_cmd",
            &long_output,
            Some(0),
            Duration::from_millis(50),
        );
        assert!(bounded_box.contains("earlier lines omitted"));
        assert!(bounded_box.contains("20 lines"));
    }

    #[test]
    fn session_choice_resolution_and_rows() {
        let s1 = SessionId::new();
        let s2 = SessionId::new();
        let choices = vec![
            SessionChoice {
                id: s1,
                title: Some("feature work".to_owned()),
                events: 10,
                last_seen: "2m ago".to_owned(),
            },
            SessionChoice {
                id: s2,
                title: None,
                events: 5,
                last_seen: "1h ago".to_owned(),
            },
        ];

        let (rows, selected) = session_rows(&choices, Some(s2));
        assert_eq!(selected, 1);
        assert_eq!(rows.unwrap().len(), 2);

        // Direct number resolution
        assert_eq!(resolve_session_answer("1", &choices, s1).unwrap(), s1);
        assert_eq!(resolve_session_answer("2", &choices, s1).unwrap(), s2);

        // Direct UUID resolution
        assert_eq!(
            resolve_session_answer(&s2.to_string(), &choices, s1).unwrap(),
            s2
        );
    }
}
