//! What each [`Role`] looks like, and the built-in themes that answer.
//!
//! Moved out of the CLI unchanged: the escape codes below are the exact ones
//! every existing theme has always painted, so an operator who picked `ocean`
//! two releases ago still gets the same `ocean`. `[theme]` overrides replace a
//! role's code here rather than anywhere further down, so one lookup serves
//! every widget.

use crate::style::Role;
use std::collections::BTreeMap;

/// One SGR prefix per visual role. Owned strings, because `[theme]` in the
/// configuration can replace any of them with a colour the operator picked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Palette {
    /// Whether this theme has any hue at all.
    ///
    /// `mono` is greys only, and a widget that paints its own colours — the
    /// tool cards do — has to know not to, or a neutral theme comes out in
    /// six of them. The theme says so itself; the alternative was comparing
    /// the palette's escape codes against `mono`'s literals to guess, which
    /// also fired on any theme that happened to share those two values.
    pub hueless: bool,
    pub assistant: String,
    pub dim: String,
    pub accent: String,
    pub ok: String,
    pub err: String,
    pub run: String,
    pub model: String,
    pub cwd: String,
    pub border: String,
    pub bullet: String,
    pub input_bg: String,
    pub tool_bash_accent: String,
    pub tool_file_accent: String,
    pub tool_search_accent: String,
    pub tool_mcp_accent: String,
    pub tool_network_accent: String,
    pub tool_generic_accent: String,
    /// Card backgrounds. Dark, low-saturation versions of the accents: a panel
    /// has to sit under text without drowning it, and a terminal's own
    /// background is dark, so a tint that reads as a panel rather than as a
    /// highlight is only a few points away from it.
    pub tool_bash_bg: String,
    pub tool_file_bg: String,
    pub tool_search_bg: String,
    pub tool_mcp_bg: String,
    pub tool_network_bg: String,
    pub tool_generic_bg: String,
    /// Header strips: the same hues as the body tints, lifted. The header has
    /// to read as a different surface from the output under it.
    pub tool_bash_head_bg: String,
    pub tool_file_head_bg: String,
    pub tool_search_head_bg: String,
    pub tool_mcp_head_bg: String,
    pub tool_network_head_bg: String,
    pub tool_generic_head_bg: String,
    pub tool_bash_border: String,
    pub tool_file_border: String,
    pub tool_search_border: String,
    pub tool_mcp_border: String,
    pub tool_network_border: String,
    pub tool_generic_border: String,
}

/// The role names `[theme]` keys and the picker's error messages use, in the
/// order [`Palette::from_codes`] takes them.
pub const THEME_ROLES: &[&str] = &[
    "assistant",
    "dim",
    "accent",
    "ok",
    "err",
    "run",
    "model",
    "cwd",
    "border",
    "bullet",
    "tool_bash_accent",
    "tool_file_accent",
    "tool_search_accent",
    "tool_mcp_accent",
    "tool_network_accent",
    "tool_generic_accent",
    "tool_bash_bg",
    "tool_file_bg",
    "tool_search_bg",
    "tool_mcp_bg",
    "tool_network_bg",
    "tool_generic_bg",
    "tool_bash_head_bg",
    "tool_file_head_bg",
    "tool_search_head_bg",
    "tool_mcp_head_bg",
    "tool_network_head_bg",
    "tool_generic_head_bg",
    "tool_bash_border",
    "tool_file_border",
    "tool_search_border",
    "tool_mcp_border",
    "tool_network_border",
    "tool_generic_border",
    "input_bg",
];

impl Palette {
    fn from_codes(codes: [&str; 11]) -> Self {
        let mut palette = Self {
            hueless: false,
            assistant: codes[0].to_owned(),
            dim: codes[1].to_owned(),
            accent: codes[2].to_owned(),
            ok: codes[3].to_owned(),
            err: codes[4].to_owned(),
            run: codes[5].to_owned(),
            model: codes[6].to_owned(),
            cwd: codes[7].to_owned(),
            border: codes[8].to_owned(),
            bullet: codes[9].to_owned(),
            input_bg: codes[10].to_owned(),
            tool_bash_accent: String::new(),
            tool_file_accent: String::new(),
            tool_search_accent: String::new(),
            tool_mcp_accent: String::new(),
            tool_network_accent: String::new(),
            tool_generic_accent: String::new(),
            tool_bash_bg: String::new(),
            tool_file_bg: String::new(),
            tool_search_bg: String::new(),
            tool_mcp_bg: String::new(),
            tool_network_bg: String::new(),
            tool_generic_bg: String::new(),
            tool_bash_head_bg: String::new(),
            tool_file_head_bg: String::new(),
            tool_search_head_bg: String::new(),
            tool_mcp_head_bg: String::new(),
            tool_network_head_bg: String::new(),
            tool_generic_head_bg: String::new(),
            tool_bash_border: String::new(),
            tool_file_border: String::new(),
            tool_search_border: String::new(),
            tool_mcp_border: String::new(),
            tool_network_border: String::new(),
            tool_generic_border: String::new(),
        };
        palette.set_tool_card_codes(false);
        palette
    }

    fn with_hueless(mut self) -> Self {
        self.hueless = true;
        self.set_tool_card_codes(true);
        self
    }

    fn set_tool_card_codes(&mut self, hueless: bool) {
        let accents = if hueless {
            [self.accent.as_str(); 6]
        } else {
            [
                "\x1b[38;2;97;175;239m",
                "\x1b[38;2;229;192;123m",
                "\x1b[38;2;198;120;221m",
                "\x1b[38;2;86;182;194m",
                "\x1b[38;2;152;195;121m",
                "\x1b[38;2;224;108;117m",
            ]
        };
        let borders = if hueless {
            [self.border.as_str(); 6]
        } else {
            [
                "\x1b[38;2;60;125;190m",
                "\x1b[38;2;176;136;59m",
                "\x1b[38;2;142;78;163m",
                "\x1b[38;2;53;127;137m",
                "\x1b[38;2;93;142;67m",
                "\x1b[38;2;157;72;80m",
            ]
        };
        // Backgrounds, not foregrounds: `48` rather than `38`. A neutral theme
        // takes the composer's own surface for every category, because six
        // tints is exactly the hue a `mono` operator asked not to have.
        let backgrounds = if hueless {
            [self.input_bg.as_str(); 6]
        } else {
            [
                "\x1b[48;2;22;32;45m",
                "\x1b[48;2;44;36;20m",
                "\x1b[48;2;38;24;46m",
                "\x1b[48;2;18;38;41m",
                "\x1b[48;2;24;40;22m",
                "\x1b[48;2;46;26;28m",
            ]
        };
        let headers = if hueless {
            [self.input_bg.as_str(); 6]
        } else {
            [
                "\x1b[48;2;33;48;68m",
                "\x1b[48;2;66;54;29m",
                "\x1b[48;2;57;36;69m",
                "\x1b[48;2;27;57;62m",
                "\x1b[48;2;36;60;33m",
                "\x1b[48;2;69;39;42m",
            ]
        };
        self.tool_bash_head_bg = headers[0].to_owned();
        self.tool_file_head_bg = headers[1].to_owned();
        self.tool_search_head_bg = headers[2].to_owned();
        self.tool_mcp_head_bg = headers[3].to_owned();
        self.tool_network_head_bg = headers[4].to_owned();
        self.tool_generic_head_bg = headers[5].to_owned();
        self.tool_bash_bg = backgrounds[0].to_owned();
        self.tool_file_bg = backgrounds[1].to_owned();
        self.tool_search_bg = backgrounds[2].to_owned();
        self.tool_mcp_bg = backgrounds[3].to_owned();
        self.tool_network_bg = backgrounds[4].to_owned();
        self.tool_generic_bg = backgrounds[5].to_owned();
        self.tool_bash_accent = accents[0].to_owned();
        self.tool_file_accent = accents[1].to_owned();
        self.tool_search_accent = accents[2].to_owned();
        self.tool_mcp_accent = accents[3].to_owned();
        self.tool_network_accent = accents[4].to_owned();
        self.tool_generic_accent = accents[5].to_owned();
        self.tool_bash_border = borders[0].to_owned();
        self.tool_file_border = borders[1].to_owned();
        self.tool_search_border = borders[2].to_owned();
        self.tool_mcp_border = borders[3].to_owned();
        self.tool_network_border = borders[4].to_owned();
        self.tool_generic_border = borders[5].to_owned();
    }

    /// The escape a role is painted with, or `None` for [`Role::Plain`], which
    /// is the terminal's own foreground rather than a colour anyone chose.
    pub fn code(&self, role: Role) -> Option<&str> {
        match role {
            Role::Plain => None,
            Role::ToolBashAccent
            | Role::ToolFileAccent
            | Role::ToolSearchAccent
            | Role::ToolMcpAccent
            | Role::ToolNetworkAccent
            | Role::ToolGenericAccent
            | Role::ToolBashBg
            | Role::ToolFileBg
            | Role::ToolSearchBg
            | Role::ToolMcpBg
            | Role::ToolNetworkBg
            | Role::ToolGenericBg
            | Role::ToolBashHeadBg
            | Role::ToolFileHeadBg
            | Role::ToolSearchHeadBg
            | Role::ToolMcpHeadBg
            | Role::ToolNetworkHeadBg
            | Role::ToolGenericHeadBg
            | Role::ToolBashBorder
            | Role::ToolFileBorder
            | Role::ToolSearchBorder
            | Role::ToolMcpBorder
            | Role::ToolNetworkBorder
            | Role::ToolGenericBorder => self.tool_code(role),
            _ => self.base_code(role),
        }
    }

    fn base_code(&self, role: Role) -> Option<&str> {
        Some(match role {
            Role::Assistant => &self.assistant,
            Role::Dim => &self.dim,
            Role::Accent => &self.accent,
            Role::Ok => &self.ok,
            Role::Err => &self.err,
            Role::Run => &self.run,
            Role::Model => &self.model,
            Role::Cwd => &self.cwd,
            Role::Border => &self.border,
            Role::Bullet => &self.bullet,
            Role::InputBg => &self.input_bg,
            _ => return None,
        })
    }

    fn tool_code(&self, role: Role) -> Option<&str> {
        Some(match role {
            Role::ToolBashAccent => &self.tool_bash_accent,
            Role::ToolFileAccent => &self.tool_file_accent,
            Role::ToolSearchAccent => &self.tool_search_accent,
            Role::ToolMcpAccent => &self.tool_mcp_accent,
            Role::ToolNetworkAccent => &self.tool_network_accent,
            Role::ToolGenericAccent => &self.tool_generic_accent,
            Role::ToolBashBg => &self.tool_bash_bg,
            Role::ToolFileBg => &self.tool_file_bg,
            Role::ToolSearchBg => &self.tool_search_bg,
            Role::ToolMcpBg => &self.tool_mcp_bg,
            Role::ToolNetworkBg => &self.tool_network_bg,
            Role::ToolGenericBg => &self.tool_generic_bg,
            Role::ToolBashHeadBg => &self.tool_bash_head_bg,
            Role::ToolFileHeadBg => &self.tool_file_head_bg,
            Role::ToolSearchHeadBg => &self.tool_search_head_bg,
            Role::ToolMcpHeadBg => &self.tool_mcp_head_bg,
            Role::ToolNetworkHeadBg => &self.tool_network_head_bg,
            Role::ToolGenericHeadBg => &self.tool_generic_head_bg,
            Role::ToolBashBorder => &self.tool_bash_border,
            Role::ToolFileBorder => &self.tool_file_border,
            Role::ToolSearchBorder => &self.tool_search_border,
            Role::ToolMcpBorder => &self.tool_mcp_border,
            Role::ToolNetworkBorder => &self.tool_network_border,
            Role::ToolGenericBorder => &self.tool_generic_border,
            _ => return None,
        })
    }

    fn slot(&mut self, role: &str) -> Option<&mut String> {
        match role {
            "assistant" | "dim" | "accent" | "ok" | "err" | "run" | "model" | "cwd" | "border"
            | "bullet" | "input_bg" => self.base_slot(role),
            _ => self.tool_slot(role),
        }
    }

    fn base_slot(&mut self, role: &str) -> Option<&mut String> {
        Some(match role {
            "assistant" => &mut self.assistant,
            "dim" => &mut self.dim,
            "accent" => &mut self.accent,
            "ok" => &mut self.ok,
            "err" => &mut self.err,
            "run" => &mut self.run,
            "model" => &mut self.model,
            "cwd" => &mut self.cwd,
            "border" => &mut self.border,
            "bullet" => &mut self.bullet,
            "input_bg" => &mut self.input_bg,
            _ => return None,
        })
    }

    fn tool_slot(&mut self, role: &str) -> Option<&mut String> {
        Some(match role {
            "tool_bash_accent" => &mut self.tool_bash_accent,
            "tool_file_accent" => &mut self.tool_file_accent,
            "tool_search_accent" => &mut self.tool_search_accent,
            "tool_mcp_accent" => &mut self.tool_mcp_accent,
            "tool_network_accent" => &mut self.tool_network_accent,
            "tool_generic_accent" => &mut self.tool_generic_accent,
            "tool_bash_bg" => &mut self.tool_bash_bg,
            "tool_file_bg" => &mut self.tool_file_bg,
            "tool_search_bg" => &mut self.tool_search_bg,
            "tool_mcp_bg" => &mut self.tool_mcp_bg,
            "tool_network_bg" => &mut self.tool_network_bg,
            "tool_generic_bg" => &mut self.tool_generic_bg,
            "tool_bash_head_bg" => &mut self.tool_bash_head_bg,
            "tool_file_head_bg" => &mut self.tool_file_head_bg,
            "tool_search_head_bg" => &mut self.tool_search_head_bg,
            "tool_mcp_head_bg" => &mut self.tool_mcp_head_bg,
            "tool_network_head_bg" => &mut self.tool_network_head_bg,
            "tool_generic_head_bg" => &mut self.tool_generic_head_bg,
            "tool_bash_border" => &mut self.tool_bash_border,
            "tool_file_border" => &mut self.tool_file_border,
            "tool_search_border" => &mut self.tool_search_border,
            "tool_mcp_border" => &mut self.tool_mcp_border,
            "tool_network_border" => &mut self.tool_network_border,
            "tool_generic_border" => &mut self.tool_generic_border,
            _ => return None,
        })
    }

    /// Replace roles from a `name -> "#rrggbb"` map. A `#rrggbb` becomes a
    /// foreground prefix; `input_bg` a background one. An unknown role or a
    /// malformed colour is rejected rather than ignored, so a typo in the
    /// configuration is seen.
    pub fn with_overrides(mut self, overrides: &BTreeMap<String, String>) -> Result<Self, String> {
        for (role, hex) in overrides {
            // A background role overridden with a foreground escape would
            // paint the text the operator's colour and leave the panel
            // untouched, so which layer a role lives on is decided here rather
            // than by the caller.
            let background = role == "input_bg" || role.ends_with("_bg");
            let code =
                hex_to_sgr(hex, background).map_err(|why| format!("[theme].{role}: {why}"))?;
            *self.slot(role).ok_or_else(|| {
                format!(
                    "[theme] has no role `{role}`; expected one of {}",
                    THEME_ROLES.join(", ")
                )
            })? = code;
        }
        Ok(self)
    }
}

/// `#rrggbb` (with or without the hash) as a truecolor SGR prefix.
///
/// The length and digit check is the whole validation, so the parse after it
/// cannot fail; it is written infallibly rather than with an error arm no
/// input can reach.
pub fn hex_to_sgr(hex: &str, background: bool) -> Result<String, String> {
    let body = hex.strip_prefix('#').unwrap_or(hex);
    if body.len() != 6 || !body.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("`{hex}` is not a #rrggbb colour"));
    }
    let channel = |at: usize| u8::from_str_radix(&body[at..at + 2], 16).unwrap_or(0);
    let (red, green, blue) = (channel(0), channel(2), channel(4));
    let lead = if background { 48 } else { 38 };
    Ok(format!("\x1b[{lead};2;{red};{green};{blue}m"))
}

/// The themes `/theme` offers: name, then the line the picker shows. All are
/// built for a dark terminal — the surface the composer already draws over —
/// so the picker can preview one by just repainting.
pub const THEMES: &[(&str, &str)] = &[
    ("dark", "the original — grey text, cyan accents"),
    ("ocean", "cool — teal and blue"),
    ("sunset", "warm — amber and rose"),
    ("vivid", "vibrant — high-contrast, multi-colored accents"),
    ("dracula", "iconic — purple, cyan, green, and pink"),
    ("nord", "arctic — frost and cool pastel accents"),
    ("mono", "neutral — greys only, no hue"),
];

/// The theme in force when nothing has been chosen: the original palette.
pub const DEFAULT_THEME: &str = "dark";

/// A built-in theme's palette, or `None` when the name is not one.
pub fn builtin_palette(name: &str) -> Option<Palette> {
    // `dark` is the historical `brainless` palette, unchanged.
    Some(match name {
        "dark" => Palette::from_codes([
            "\x1b[38;2;201;201;201m",
            "\x1b[38;2;122;122;122m",
            "\x1b[38;2;92;194;224m",
            "\x1b[38;2;78;169;111m",
            "\x1b[38;2;247;118;142m",
            "\x1b[38;2;224;175;104m",
            "\x1b[38;2;246;226;183m",
            "\x1b[38;2;171;223;167m",
            "\x1b[38;2;58;58;58m",
            "\x1b[38;2;167;167;167m",
            "\x1b[48;2;53;53;53m",
        ]),
        // assistant, dim, accent, ok, err, run, model, cwd, border, bullet, input_bg
        "vivid" | "omp" => Palette::from_codes([
            "\x1b[38;2;230;237;243m",
            "\x1b[38;2;139;148;158m",
            "\x1b[38;2;88;166;255m",
            "\x1b[38;2;63;185;80m",
            "\x1b[38;2;248;81;73m",
            "\x1b[38;2;227;179;65m",
            "\x1b[38;2;210;168;255m",
            "\x1b[38;2;86;211;100m",
            "\x1b[38;2;88;166;255m",
            "\x1b[38;2;255;166;87m",
            "\x1b[48;2;22;27;34m",
        ]),
        "dracula" => Palette::from_codes([
            "\x1b[38;2;248;248;242m",
            "\x1b[38;2;98;114;164m",
            "\x1b[38;2;189;147;249m",
            "\x1b[38;2;80;250;123m",
            "\x1b[38;2;255;85;85m",
            "\x1b[38;2;241;250;140m",
            "\x1b[38;2;255;121;198m",
            "\x1b[38;2;139;233;253m",
            "\x1b[38;2;189;147;249m",
            "\x1b[38;2;255;184;108m",
            "\x1b[48;2;40;42;54m",
        ]),
        "nord" => Palette::from_codes([
            "\x1b[38;2;236;239;244m",
            "\x1b[38;2;129;161;193m",
            "\x1b[38;2;136;192;208m",
            "\x1b[38;2;163;190;140m",
            "\x1b[38;2;191;97;106m",
            "\x1b[38;2;235;203;139m",
            "\x1b[38;2;180;142;173m",
            "\x1b[38;2;143;188;187m",
            "\x1b[38;2;136;192;208m",
            "\x1b[38;2;208;135;112m",
            "\x1b[48;2;46;52;64m",
        ]),
        // assistant, dim, accent, ok, err, run, model, cwd, border, bullet, input_bg
        "ocean" => Palette::from_codes([
            "\x1b[38;2;205;214;224m",
            "\x1b[38;2;107;122;137m",
            "\x1b[38;2;79;201;201m",
            "\x1b[38;2;95;208;160m",
            "\x1b[38;2;244;132;156m",
            "\x1b[38;2;217;176;106m",
            "\x1b[38;2;215;230;230m",
            "\x1b[38;2;143;214;192m",
            "\x1b[38;2;55;67;76m",
            "\x1b[38;2;127;149;160m",
            "\x1b[48;2;36;48;56m",
        ]),
        "sunset" => Palette::from_codes([
            "\x1b[38;2;224;212;200m",
            "\x1b[38;2;138;122;108m",
            "\x1b[38;2;230;168;79m",
            "\x1b[38;2;168;201;106m",
            "\x1b[38;2;244;125;146m",
            "\x1b[38;2;224;138;74m",
            "\x1b[38;2;242;226;183m",
            "\x1b[38;2;188;212;154m",
            "\x1b[38;2;74;63;56m",
            "\x1b[38;2;160;140;124m",
            "\x1b[48;2;51;42;36m",
        ]),
        "mono" => Palette::from_codes([
            "\x1b[38;2;220;220;220m",
            "\x1b[38;2;122;122;122m",
            "\x1b[38;2;245;245;245m",
            "\x1b[38;2;200;200;200m",
            "\x1b[38;2;235;235;235m",
            "\x1b[38;2;180;180;180m",
            "\x1b[38;2;235;235;235m",
            "\x1b[38;2;205;205;205m",
            "\x1b[38;2;74;74;74m",
            "\x1b[38;2;160;160;160m",
            "\x1b[48;2;42;42;42m",
        ])
        .with_hueless(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every name the picker offers has a palette, and every role has a code
    /// except `Plain`, which is the terminal's own foreground.
    #[test]
    fn every_offered_theme_has_a_palette_for_every_role() {
        for (name, description) in THEMES {
            let palette = builtin_palette(name).unwrap_or_else(|| panic!("{name} has no palette"));
            assert!(!description.is_empty(), "{name} has no description");
            for key in THEME_ROLES {
                let role = Role::from_key(key).unwrap_or_else(|| panic!("{key} is not a role"));
                let code = palette
                    .code(role)
                    .unwrap_or_else(|| panic!("{key} has no code"));
                assert!(code.starts_with("\x1b["), "{name}.{key} is not an escape");
            }
        }
        assert!(builtin_palette("chartreuse").is_none());
        assert!(builtin_palette(DEFAULT_THEME).is_some());
        assert!(builtin_palette("dark")
            .expect("dark")
            .code(Role::Plain)
            .is_none());
    }

    /// Every card tint is a real background, and an operator who recolours one
    /// gets a background back — not a foreground that would paint their text
    /// instead of the panel under it.
    #[test]
    fn card_tints_are_backgrounds_on_every_theme_and_through_an_override() {
        let tints = [
            Role::ToolBashBg,
            Role::ToolFileBg,
            Role::ToolSearchBg,
            Role::ToolMcpBg,
            Role::ToolNetworkBg,
            Role::ToolGenericBg,
            Role::ToolBashHeadBg,
            Role::ToolFileHeadBg,
            Role::ToolSearchHeadBg,
            Role::ToolMcpHeadBg,
            Role::ToolNetworkHeadBg,
            Role::ToolGenericHeadBg,
        ];
        for (name, _) in THEMES {
            let palette = builtin_palette(name).expect("a built-in theme");
            for tint in tints {
                let code = palette.code(tint).expect("a tint has a colour");
                assert!(
                    code.starts_with("\x1b[48;"),
                    "{name}.{tint:?} is not a background"
                );
            }
        }

        // A neutral theme takes the composer's surface rather than six hues.
        let mono = builtin_palette("mono").expect("mono");
        for tint in tints {
            assert_eq!(mono.code(tint), Some(mono.input_bg.as_str()));
        }

        let mut roles = BTreeMap::new();
        roles.insert("tool_mcp_bg".to_owned(), "#102030".to_owned());
        let painted = builtin_palette("dark")
            .expect("dark")
            .with_overrides(&roles)
            .expect("valid override");
        assert_eq!(painted.tool_mcp_bg, "\x1b[48;2;16;32;48m");
    }

    /// `input_bg` is a background; everything else is a foreground. A palette
    /// that got this backwards would paint the composer's text invisible.
    #[test]
    fn only_input_bg_is_a_background() {
        for (name, _) in THEMES {
            let palette = builtin_palette(name).expect("a built-in theme");
            assert!(palette.input_bg.starts_with("\x1b[48;"), "{name}");
            assert!(palette.assistant.starts_with("\x1b[38;"), "{name}");
        }
    }

    /// Only `mono` says it has no hue, and it says so itself rather than being
    /// recognised by its colours.
    #[test]
    fn the_neutral_theme_declares_itself() {
        assert!(builtin_palette("mono").expect("mono").hueless);
        for (name, _) in THEMES.iter().filter(|(name, _)| *name != "mono") {
            assert!(
                !builtin_palette(name).expect("a built-in theme").hueless,
                "{name} has hue"
            );
        }
        // An override changes a colour, never whether the theme has hue: a
        // recoloured `mono` is still the neutral theme.
        let mut roles = BTreeMap::new();
        roles.insert("accent".to_owned(), "#ff0000".to_owned());
        assert!(
            builtin_palette("mono")
                .expect("mono")
                .with_overrides(&roles)
                .expect("valid override")
                .hueless
        );
    }

    #[test]
    fn overrides_replace_only_the_named_roles_and_reject_the_rest() {
        assert_eq!(hex_to_sgr("#ff0000", false).unwrap(), "\x1b[38;2;255;0;0m");
        assert_eq!(hex_to_sgr("00ff80", true).unwrap(), "\x1b[48;2;0;255;128m");
        assert!(hex_to_sgr("#fff", false).is_err());
        assert!(hex_to_sgr("#gggggg", false).is_err());

        let mut roles = BTreeMap::new();
        roles.insert("accent".to_owned(), "#123456".to_owned());
        let base = builtin_palette("dark").expect("dark");
        let painted = base.clone().with_overrides(&roles).expect("valid override");
        assert_eq!(painted.accent, "\x1b[38;2;18;52;86m");
        assert_eq!(painted.dim, base.dim, "an untouched role keeps its colour");

        roles.insert("chartreuse".to_owned(), "#123456".to_owned());
        let refused = builtin_palette("dark")
            .expect("dark")
            .with_overrides(&roles);
        assert!(refused.unwrap_err().contains("chartreuse"));
    }
}
