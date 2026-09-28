//! Terminal byte decoding and semantic input actions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Char(char),
    Backspace,
    WordBackspace,
    Left,
    Right,
    WordLeft,
    WordRight,
    Home,
    End,
    Delete,
    Up,
    Down,
    Enter,
    /// Shift+Enter or Alt+Enter to insert a newline without submitting.
    Newline,
    /// Shift+Tab: step to the next approval mode without leaving the line.
    CycleMode,
    /// Tab to cycle focus between panes or fields.
    Tab,
    /// Page up/down scroll a long plan preview.
    PageUp,
    PageDown,
    /// Ctrl-C, or Escape once it is known to stand alone.
    Interrupt,
    /// Ctrl-D on an empty line.
    Eof,
    /// Ctrl-O: show or hide the last tool call's whole output.
    Expand,
    /// A bracketed paste began; the pasted text follows as ordinary keys.
    PasteStart,
    /// The bracketed paste that began with `PasteStart` ended.
    PasteEnd,
}

/// Turns the raw byte stream into keys, holding back partial UTF-8 characters
/// and partial escape sequences.
#[derive(Default)]
pub struct Keys {
    pending: Vec<u8>,
    pasting: bool,
    /// The last pasted byte was `\r`, so a `\n` right after it is the same
    /// line break rather than a second one.
    pasted_cr: bool,
}

impl Keys {
    pub fn feed(&mut self, byte: u8) -> Option<Key> {
        if self.pending.first() == Some(&0x1b) {
            return self.feed_escape(byte);
        }
        if self.pasting {
            // A paste carries whatever the clipboard held. Its line breaks stay
            // line breaks — as `Newline`, never `Enter`, so a paste cannot
            // submit itself — tabs become spaces, and its other control bytes
            // are dropped, so pasted text cannot act as if it had been typed.
            let after_cr = std::mem::replace(&mut self.pasted_cr, byte == b'\r');
            match byte {
                b'\r' => return Some(Key::Newline),
                b'\n' if after_cr => return None,
                b'\n' => return Some(Key::Newline),
                b'\t' => return Some(Key::Char(' ')),
                _ => {}
            }
            if byte != 0x1b && (byte < 0x20 || byte == 0x7f) {
                return None;
            }
        }
        if byte < 0x80 && !self.pending.is_empty() {
            self.pending.clear();
        }
        if byte == 0x1b {
            self.pending.push(byte);
            return None;
        }
        if byte < 0x80 {
            return control_key(byte);
        }
        self.character(byte)
    }

    /// Assemble a character from the bytes held so far.
    ///
    /// A byte above ASCII is part of a multi-byte character, so it is held
    /// until the character is whole.
    fn character(&mut self, byte: u8) -> Option<Key> {
        self.pending.push(byte);
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let character = text.chars().next();
                self.pending.clear();
                character.map(Key::Char)
            }
            // Incomplete is normal mid-character; invalid means the stream is
            // not UTF-8, and holding the bytes would stall every later key.
            Err(error) if error.error_len().is_none() => None,
            Err(_) => {
                self.pending.clear();
                None
            }
        }
    }

    fn feed_escape(&mut self, byte: u8) -> Option<Key> {
        if self.pending.len() == 1 {
            if let Some(key) = alt_key(byte) {
                self.pending.clear();
                return Some(key);
            }
            // Not the start of a sequence, so the Escape stood alone and this
            // byte is the next key — or nothing this build binds, in which
            // case the Escape is what the operator meant.
            if !matches!(byte, b'[' | b'O') {
                self.pending.clear();
                return self.feed(byte).or(Some(Key::Interrupt));
            }
        }
        self.pending.push(byte);
        if self.pending.len() > 32 {
            self.pending.clear();
            return None;
        }
        if !(0x40..=0x7e).contains(&byte) || self.pending.len() == 2 {
            return None;
        }
        let sequence = std::mem::take(&mut self.pending);
        // Bracketed paste brackets the text rather than standing for a key.
        if let Some(pasting) = match sequence.as_slice() {
            b"\x1b[200~" => Some(true),
            b"\x1b[201~" => Some(false),
            _ => None,
        } {
            let changed = self.pasting != pasting;
            self.pasting = pasting;
            self.pasted_cr = false;
            return changed.then_some(if pasting {
                Key::PasteStart
            } else {
                Key::PasteEnd
            });
        }
        if self.pasting {
            return None;
        }
        sequence_key(&sequence)
    }

    /// A lone Escape is only distinguishable from a sequence by the absence of
    /// what would follow it, so the caller reports the pause.
    pub fn flush_escape(&mut self) -> Option<Key> {
        let interrupt = self.pending.as_slice() == [0x1b] && !self.pasting;
        if self.pending.first() == Some(&0x1b) && !self.pasting {
            self.pending.clear();
        }
        interrupt.then_some(Key::Interrupt)
    }
}

/// The typed equivalent of Shift+Tab, kept for command documentation.
pub const CYCLE_APPROVAL_MODE: &str = "/approval cycle";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Submit(String),
    /// Change approval mode without submitting or queueing the draft.
    CycleMode,
    /// Show or hide the last tool call's whole output.
    Expand,
    Quit,
    Redraw,
    None,
}

/// The key a control byte stands for, if this build binds one.
fn control_key(byte: u8) -> Option<Key> {
    match byte {
        0x01 => Some(Key::Home),
        0x03 => Some(Key::Interrupt),
        0x04 => Some(Key::Eof),
        0x05 => Some(Key::End),
        0x09 => Some(Key::Tab),
        0x0f => Some(Key::Expand),
        0x17 => Some(Key::WordBackspace),
        b'\r' | b'\n' => Some(Key::Enter),
        0x7f | 0x08 => Some(Key::Backspace),
        // Any other control byte is not bound to an action.
        0x00..=0x1f => None,
        _ => Some(Key::Char(char::from(byte))),
    }
}

/// The key `ESC` followed directly by `byte` stands for.
///
/// This is the Alt/Meta form a terminal sends for a word-wise edit, and it is
/// answered before the byte is considered as the start of a sequence.
fn alt_key(byte: u8) -> Option<Key> {
    match byte {
        b'\r' | b'\n' => Some(Key::Newline),
        b'b' | b'B' => Some(Key::WordLeft),
        b'f' | b'F' => Some(Key::WordRight),
        0x7f | 0x08 => Some(Key::WordBackspace),
        _ => None,
    }
}

/// The key a complete escape sequence stands for.
///
/// Spelled as a table because that is what it is: several terminals send
/// several encodings of the same key, and a reader checking whether one is
/// covered should be able to look rather than follow a chain of comparisons.
fn sequence_key(sequence: &[u8]) -> Option<Key> {
    #[rustfmt::skip]
    const SEQUENCES: &[(&[u8], Key)] = &[
        (b"\x1b[5~", Key::PageUp),
        (b"\x1b[6~", Key::PageDown),
        (b"\x1b[13;2u", Key::Newline),
        (b"\x1b[13;3u", Key::Newline),
        (b"\x1b[13;5u", Key::Newline),
        (b"\x1b[27;2;13~", Key::Newline),
        (b"\x1b[27;3;13~", Key::Newline),
        (b"\x1b[27;5;13~", Key::Newline),
        (b"\x1bOM", Key::Newline),
        (b"\x1b[13~", Key::Newline),
        (b"\x1b[1;3D", Key::WordLeft),
        (b"\x1b[1;5D", Key::WordLeft),
        (b"\x1b[5D", Key::WordLeft),
        (b"\x1b[1;4D", Key::WordLeft),
        (b"\x1b[1;3C", Key::WordRight),
        (b"\x1b[1;5C", Key::WordRight),
        (b"\x1b[5C", Key::WordRight),
        (b"\x1b[1;4C", Key::WordRight),
        (b"\x1b[1;9D", Key::Home),
        (b"\x1b[1;2D", Key::Home),
        (b"\x1b[1;9C", Key::End),
        (b"\x1b[1;2C", Key::End),
        (b"\x1b[3;3~", Key::WordBackspace),
        (b"\x1b[3;5~", Key::WordBackspace),
        // Shift+Tab. `CSI Z` is what every terminal here sends; the modified
        // form is what a terminal in kitty-style key reporting sends instead.
        (b"\x1b[Z", Key::CycleMode),
        (b"\x1b[1;2Z", Key::CycleMode),
    ];

    if let Some((_, key)) = SEQUENCES.iter().find(|(bytes, _)| *bytes == sequence) {
        return Some(*key);
    }
    // The unmodified forms, which differ only in their final byte.
    match (sequence.last(), sequence.get(2)) {
        (Some(b'A'), _) => Some(Key::Up),
        (Some(b'B'), _) => Some(Key::Down),
        (Some(b'~'), Some(b'3')) => Some(Key::Delete),
        (Some(b'D'), _) => Some(Key::Left),
        (Some(b'C'), _) => Some(Key::Right),
        (Some(b'H'), _) | (Some(b'~'), Some(b'1')) | (Some(b'~'), Some(b'7')) => Some(Key::Home),
        (Some(b'F'), _) | (Some(b'~'), Some(b'4')) | (Some(b'~'), Some(b'8')) => Some(Key::End),
        _ => None,
    }
}
