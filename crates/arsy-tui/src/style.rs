//! What a span means, not what colour it is.
//!
//! The renderer used to build rows as already-escaped strings, which made a
//! row a dead end: its width could only be recovered by stripping the escapes
//! back out, and its colours could not be recovered at all. A [`Role`] says
//! what a piece of text *is*, and the palette decides what that looks like at
//! the moment it is serialised — so the same row can be measured, wrapped,
//! re-themed, or handed to a different renderer.

/// One visual role the palette has a colour for.
///
/// These are the roles `[theme]` already names, unchanged: an operator's
/// existing configuration has to keep meaning what it meant.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Role {
    /// Ordinary text with no role of its own: the terminal's own foreground.
    #[default]
    Plain,
    Assistant,
    Dim,
    Accent,
    Ok,
    Err,
    Run,
    Model,
    Cwd,
    Border,
    Bullet,
    /// A background, not a foreground — the composer's surface.
    InputBg,
}

impl Role {
    /// The name `[theme]` keys use. `Plain` has none: it is the absence of a
    /// role rather than a role an operator can recolour.
    pub const fn key(self) -> Option<&'static str> {
        Some(match self {
            Self::Plain => return None,
            Self::Assistant => "assistant",
            Self::Dim => "dim",
            Self::Accent => "accent",
            Self::Ok => "ok",
            Self::Err => "err",
            Self::Run => "run",
            Self::Model => "model",
            Self::Cwd => "cwd",
            Self::Border => "border",
            Self::Bullet => "bullet",
            Self::InputBg => "input_bg",
        })
    }

    /// The role a `[theme]` key names.
    pub fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "assistant" => Self::Assistant,
            "dim" => Self::Dim,
            "accent" => Self::Accent,
            "ok" => Self::Ok,
            "err" => Self::Err,
            "run" => Self::Run,
            "model" => Self::Model,
            "cwd" => Self::Cwd,
            "border" => Self::Border,
            "bullet" => Self::Bullet,
            "input_bg" => Self::InputBg,
            _ => return None,
        })
    }
}

/// A role plus the attributes that are not colours.
///
/// Bold is separate from the role because it composes with every one of them:
/// a bold accent and a bold error are both things the renderer paints, and a
/// `BoldAccent` role for each combination would be a palette of products
/// rather than of meanings.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Style {
    pub role: Role,
    pub bold: bool,
}

impl Style {
    pub const PLAIN: Self = Self {
        role: Role::Plain,
        bold: false,
    };

    pub const fn new(role: Role) -> Self {
        Self { role, bold: false }
    }

    #[must_use]
    pub const fn bold(self) -> Self {
        Self { bold: true, ..self }
    }

    /// Whether serialising this needs any escape at all, so an unstyled run is
    /// written as plain text rather than as a reset around nothing.
    pub const fn is_plain(self) -> bool {
        matches!(self.role, Role::Plain) && !self.bold
    }
}

impl From<Role> for Style {
    fn from(role: Role) -> Self {
        Self::new(role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every role an operator can name in `[theme]` round-trips, and `Plain`
    /// stays unnameable — it is the absence of a role, not a twelfth colour.
    #[test]
    fn theme_keys_round_trip_through_roles() {
        let named = [
            Role::Assistant,
            Role::Dim,
            Role::Accent,
            Role::Ok,
            Role::Err,
            Role::Run,
            Role::Model,
            Role::Cwd,
            Role::Border,
            Role::Bullet,
            Role::InputBg,
        ];
        for role in named {
            let key = role.key().expect("a named role has a key");
            assert_eq!(Role::from_key(key), Some(role), "{key}");
        }
        assert_eq!(Role::Plain.key(), None);
        assert_eq!(Role::from_key("chartreuse"), None);
        assert_eq!(named.len(), 11, "the theme surface is eleven roles");
    }

    #[test]
    fn a_plain_style_needs_no_escape_but_a_bold_one_does() {
        assert!(Style::PLAIN.is_plain());
        assert!(!Style::PLAIN.bold().is_plain());
        assert!(!Style::new(Role::Accent).is_plain());
    }
}
