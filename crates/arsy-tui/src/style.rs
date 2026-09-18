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
    /// Category-specific accents used by tool cards.
    ToolBashAccent,
    ToolFileAccent,
    ToolSearchAccent,
    ToolMcpAccent,
    ToolNetworkAccent,
    ToolGenericAccent,
    /// Category-specific card backgrounds, which is what gives a tool card the
    /// tinted panel the mockup draws rather than a bare outline.
    ToolBashBg,
    ToolFileBg,
    ToolSearchBg,
    ToolMcpBg,
    ToolNetworkBg,
    ToolGenericBg,
    /// Category-specific borders used by tool cards.
    ToolBashBorder,
    ToolFileBorder,
    ToolSearchBorder,
    ToolMcpBorder,
    ToolNetworkBorder,
    ToolGenericBorder,
    /// A background, not a foreground — the composer's surface.
    InputBg,
}

impl Role {
    /// The name `[theme]` keys use. `Plain` has none: it is the absence of a
    /// role rather than a role an operator can recolour.
    pub const fn key(self) -> Option<&'static str> {
        match self {
            Self::Plain => None,
            Self::ToolBashAccent
            | Self::ToolFileAccent
            | Self::ToolSearchAccent
            | Self::ToolMcpAccent
            | Self::ToolNetworkAccent
            | Self::ToolGenericAccent
            | Self::ToolBashBg
            | Self::ToolFileBg
            | Self::ToolSearchBg
            | Self::ToolMcpBg
            | Self::ToolNetworkBg
            | Self::ToolGenericBg
            | Self::ToolBashBorder
            | Self::ToolFileBorder
            | Self::ToolSearchBorder
            | Self::ToolMcpBorder
            | Self::ToolNetworkBorder
            | Self::ToolGenericBorder => Self::tool_key(self),
            _ => Self::base_key(self),
        }
    }

    const fn base_key(self) -> Option<&'static str> {
        Some(match self {
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
            _ => return None,
        })
    }

    const fn tool_key(self) -> Option<&'static str> {
        Some(match self {
            Self::ToolBashAccent => "tool_bash_accent",
            Self::ToolFileAccent => "tool_file_accent",
            Self::ToolSearchAccent => "tool_search_accent",
            Self::ToolMcpAccent => "tool_mcp_accent",
            Self::ToolNetworkAccent => "tool_network_accent",
            Self::ToolGenericAccent => "tool_generic_accent",
            Self::ToolBashBg => "tool_bash_bg",
            Self::ToolFileBg => "tool_file_bg",
            Self::ToolSearchBg => "tool_search_bg",
            Self::ToolMcpBg => "tool_mcp_bg",
            Self::ToolNetworkBg => "tool_network_bg",
            Self::ToolGenericBg => "tool_generic_bg",
            Self::ToolBashBorder => "tool_bash_border",
            Self::ToolFileBorder => "tool_file_border",
            Self::ToolSearchBorder => "tool_search_border",
            Self::ToolMcpBorder => "tool_mcp_border",
            Self::ToolNetworkBorder => "tool_network_border",
            Self::ToolGenericBorder => "tool_generic_border",
            _ => return None,
        })
    }

    /// The role a `[theme]` key names.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::base_from_key(key).or_else(|| Self::tool_from_key(key))
    }

    fn base_from_key(key: &str) -> Option<Self> {
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

    fn tool_from_key(key: &str) -> Option<Self> {
        Some(match key {
            "tool_bash_accent" => Self::ToolBashAccent,
            "tool_file_accent" => Self::ToolFileAccent,
            "tool_search_accent" => Self::ToolSearchAccent,
            "tool_mcp_accent" => Self::ToolMcpAccent,
            "tool_network_accent" => Self::ToolNetworkAccent,
            "tool_generic_accent" => Self::ToolGenericAccent,
            "tool_bash_bg" => Self::ToolBashBg,
            "tool_file_bg" => Self::ToolFileBg,
            "tool_search_bg" => Self::ToolSearchBg,
            "tool_mcp_bg" => Self::ToolMcpBg,
            "tool_network_bg" => Self::ToolNetworkBg,
            "tool_generic_bg" => Self::ToolGenericBg,
            "tool_bash_border" => Self::ToolBashBorder,
            "tool_file_border" => Self::ToolFileBorder,
            "tool_search_border" => Self::ToolSearchBorder,
            "tool_mcp_border" => Self::ToolMcpBorder,
            "tool_network_border" => Self::ToolNetworkBorder,
            "tool_generic_border" => Self::ToolGenericBorder,
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
    /// The panel this text sits on, when it sits on one.
    ///
    /// Separate from `role` because a tinted card paints both at once: the
    /// title keeps its category accent while the whole row carries the
    /// category's background. Folding the two into one role would need a
    /// role per pair rather than one per meaning.
    pub bg: Option<Role>,
    pub bold: bool,
}

impl Style {
    pub const PLAIN: Self = Self {
        role: Role::Plain,
        bg: None,
        bold: false,
    };

    pub const fn new(role: Role) -> Self {
        Self {
            role,
            bg: None,
            bold: false,
        }
    }

    #[must_use]
    pub const fn bold(self) -> Self {
        Self { bold: true, ..self }
    }

    /// Put this text on a panel.
    #[must_use]
    pub const fn on(self, bg: Role) -> Self {
        Self {
            bg: Some(bg),
            ..self
        }
    }

    /// Whether serialising this needs any escape at all, so an unstyled run is
    /// written as plain text rather than as a reset around nothing.
    pub const fn is_plain(self) -> bool {
        matches!(self.role, Role::Plain) && self.bg.is_none() && !self.bold
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
    /// stays unnameable — it is the absence of a role rather than a colour.
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
            Role::ToolBashAccent,
            Role::ToolFileAccent,
            Role::ToolSearchAccent,
            Role::ToolMcpAccent,
            Role::ToolNetworkAccent,
            Role::ToolGenericAccent,
            Role::ToolBashBorder,
            Role::ToolFileBorder,
            Role::ToolSearchBorder,
            Role::ToolMcpBorder,
            Role::ToolNetworkBorder,
            Role::ToolGenericBorder,
            Role::InputBg,
        ];
        for role in named {
            let key = role.key().expect("a named role has a key");
            assert_eq!(Role::from_key(key), Some(role), "{key}");
        }
        assert_eq!(Role::Plain.key(), None);
        assert_eq!(Role::from_key("chartreuse"), None);
        assert_eq!(named.len(), 23, "the theme surface is twenty-three roles");
    }

    #[test]
    fn a_plain_style_needs_no_escape_but_a_bold_one_does() {
        assert!(Style::PLAIN.is_plain());
        assert!(!Style::PLAIN.bold().is_plain());
        assert!(!Style::new(Role::Accent).is_plain());
    }
}
