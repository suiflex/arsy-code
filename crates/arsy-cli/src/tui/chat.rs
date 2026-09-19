//! Composer, input actions, and assistant conversation rendering.
use super::*;

/// The slash commands the composer offers and `/help` prints. One table, so a
/// command cannot appear in the menu and not in the help, or the reverse.
pub const COMMANDS: &[(&str, &str)] = &[
    ("/new", "start a fresh session"),
    ("/clear", "clear conversation context in place"),
    ("/resume", "resume a recorded session; [SESSION_ID]"),
    ("/rename", "rename current session; <TITLE>"),
    (
        "/session",
        "manage sessions; list | rename <TITLE> | delete [ID]",
    ),
    (
        "/approval",
        "set approval mode; default | acceptEdits | plan | auto | dontAsk | bypassPermissions",
    ),
    (
        "/plan",
        "plan a task; show | approve | revise [NOTE] | cancel",
    ),
    ("/todo", "show this session's durable checklist"),
    ("/provider", "choose, add, or remove a provider endpoint"),
    ("/model", "choose the provider model"),
    ("/effort", "set reasoning effort; low | medium | high | off"),
    (
        "/theme",
        "choose the colour theme; dark | ocean | sunset | mono",
    ),
    (
        "/mcp",
        "MCP connections; alone toggles and adopts | list | show NAME, --source claude|codex|omp",
    ),
    ("/hooks", "inspect Claude hooks; list, --event NAME"),
    (
        "/settings",
        "show effective configuration and where each value came from; [KEY]",
    ),
    ("/doctor", "check workspace, storage, and sandbox assurance"),
    (
        "/auth",
        "manage credentials; list | login PROVIDER | set PROVIDER | remove HANDLE",
    ),
    (
        "/compat",
        "explain ecosystem mapping; claude | codex | omp | agents",
    ),
    ("/update", "check for and install arsy-code updates"),
    ("/help", "show these actions"),
    ("/quit", "exit"),
];

/// The rows `/provider` offers under the list of configured providers.
pub const PROVIDER_ACTIONS: &[(&str, &str)] = &[
    (
        "+new",
        "add a provider: name, dialect, URL, model, credential",
    ),
    ("-remove", "remove a provider from the configuration"),
];

pub const AUTH_ACTIONS: &[(&str, &str)] = &[
    (
        "login",
        "sign in to a provider with OAuth (browser / device flow)",
    ),
    ("list", "show saved credentials in catalog"),
    ("set", "store an API key for a provider"),
    ("remove", "delete a credential from catalog"),
];

/// What `/auth` is collecting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthStep {
    Pick,
    LoginProvider,
    SetProvider,
    SetKey,
    RemoveHandle,
    /// A manual-grant login (Anthropic's Claude Code OAuth client, among
    /// those ARSY ships) opened the browser on the previous turn; this one
    /// collects the code the issuer's hosted page showed the operator.
    PasteCode,
}

impl AuthStep {
    pub fn prompt(self, draft: &str, colour: bool) -> String {
        let text = match self {
            Self::Pick => "auth · Up/Down then Enter, or an action".to_owned(),
            Self::LoginProvider => "sign in to which provider · Up/Down then Enter".to_owned(),
            Self::SetProvider => "store key for which provider · Up/Down then Enter".to_owned(),
            Self::SetKey => format!("credential for {draft} · not shown as you type"),
            Self::RemoveHandle => "remove which credential · Up/Down then Enter".to_owned(),
            Self::PasteCode => "paste the code you were shown · Enter when done".to_owned(),
        };
        paint(colour, sgr_dim(), &format!("  {text}"))
    }

    pub fn rows(self, providers: &[String], handles: &[String]) -> Option<Vec<(String, String)>> {
        let named = |rows: &[(&str, &str)]| {
            Some(
                rows.iter()
                    .map(|(name, description)| ((*name).to_owned(), (*description).to_owned()))
                    .collect(),
            )
        };
        match self {
            Self::Pick => named(AUTH_ACTIONS),
            Self::SetProvider => Some(
                providers
                    .iter()
                    .map(|p| (p.clone(), format!("configured endpoint `{p}`")))
                    .collect(),
            ),
            Self::LoginProvider => {
                let mut rows: Vec<(String, String)> = providers
                    .iter()
                    .map(|p| (p.clone(), format!("configured endpoint `{p}`")))
                    .collect();
                // Built-in presets that are not already configured: signing in
                // to one writes its endpoint.
                for preset in arsy_kernel::oauth::presets::all() {
                    if !providers.iter().any(|p| p == preset.id) {
                        rows.push((preset.id.to_owned(), preset.label.to_owned()));
                    }
                }
                Some(rows)
            }
            Self::RemoveHandle => Some(
                handles
                    .iter()
                    .map(|h| (h.clone(), "saved credential".to_owned()))
                    .collect(),
            ),
            Self::SetKey | Self::PasteCode => None,
        }
    }

    pub fn masked(self) -> bool {
        matches!(self, Self::SetKey)
    }
}
/// The dialects an endpoint can speak. Same two the configuration accepts.
pub const PROVIDER_KINDS: &[(&str, &str)] = &[
    ("openai", "Chat Completions, and anything that speaks it"),
    ("anthropic", "Anthropic Messages"),
];

/// Where a credential typed into the TUI is put.
pub const PROVIDER_STORES: &[(&str, &str)] = &[
    (
        "file",
        "a 0600 file beside the configuration; no unlock prompt",
    ),
    ("keychain", "the OS credential store"),
];

pub const CONFIRM_ROWS: &[(&str, &str)] = &[("no", "keep it"), ("yes", "remove it")];

/// What `/provider` is collecting. One variant per question, so the loop always
/// knows which answer it is holding and what to ask next.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderStep {
    /// Pick a configured provider, or one of the actions under them.
    Pick,
    Name,
    Kind,
    BaseUrl,
    Model,
    Store,
    /// The credential itself, typed masked.
    Key,
    /// Which provider to remove.
    Remove,
    /// Confirm that removal, because it rewrites the operator's file.
    ConfirmRemove,
}

impl ProviderStep {
    /// The prompt line shown under the composer while this step collects.
    pub fn prompt(self, draft: &ProviderDraft, colour: bool) -> String {
        let text = match self {
            Self::Pick => "provider · Up/Down then Enter, or a name".to_owned(),
            Self::Name => "new provider · a short id, letters and dashes".to_owned(),
            Self::Kind => "dialect · Up/Down then Enter, or a name".to_owned(),
            Self::BaseUrl => format!("base URL for {} · the API root", draft.name),
            Self::Model => format!(
                "models for {} · one slug, or several separated by commas",
                draft.name
            ),
            Self::Store => "where to keep the credential · Up/Down then Enter".to_owned(),
            Self::Key => format!("credential for {} · not shown as you type", draft.name),
            Self::Remove => "remove which provider · Up/Down then Enter".to_owned(),
            Self::ConfirmRemove => {
                format!("remove `{}` from the configuration?", draft.name)
            }
        };
        paint(colour, sgr_dim(), &format!("  {text}"))
    }

    /// The rows this step offers, or none when it collects free text.
    /// `running` is the provider this session resolved at startup; `default` is
    /// what the configuration names now. They differ between a switch and the
    /// restart that picks it up, and saying so is the whole point of the
    /// marker.
    pub fn rows(
        self,
        providers: &[String],
        running: &str,
        default: Option<&str>,
    ) -> Option<Vec<(String, String)>> {
        let named = |rows: &[(&str, &str)]| {
            Some(
                rows.iter()
                    .map(|(name, description)| ((*name).to_owned(), (*description).to_owned()))
                    .collect(),
            )
        };
        match self {
            Self::Pick => {
                // Which one is in force has to be on the row: adding a provider
                // makes it the default, and without a marker the one it
                // replaced reads as gone rather than as merely not current.
                let mut rows: Vec<(String, String)> = providers
                    .iter()
                    .map(|name| {
                        let note = if name == running {
                            "in use"
                        } else if default == Some(name.as_str()) {
                            "chosen · in use after a restart"
                        } else {
                            "switch to this provider"
                        };
                        (name.clone(), note.to_owned())
                    })
                    .collect();
                for (name, description) in PROVIDER_ACTIONS {
                    // Nothing to remove until something is configured.
                    if *name == "-remove" && providers.is_empty() {
                        continue;
                    }
                    rows.push(((*name).to_owned(), (*description).to_owned()));
                }
                Some(rows)
            }
            Self::Kind => named(PROVIDER_KINDS),
            Self::Store => named(PROVIDER_STORES),
            Self::ConfirmRemove => named(CONFIRM_ROWS),
            Self::Remove => Some(
                providers
                    .iter()
                    .map(|name| (name.clone(), "remove this one".to_owned()))
                    .collect(),
            ),
            Self::Name | Self::BaseUrl | Self::Model | Self::Key => None,
        }
    }

    /// Whether the answer to this step is a secret.
    pub const fn masked(self) -> bool {
        matches!(self, Self::Key)
    }
}

/// What `/provider` has collected so far.
#[derive(Clone, Debug, Default)]
pub struct ProviderDraft {
    pub name: String,
    pub kind: String,
    pub base_url: String,
    /// Every model the endpoint offers. The first is its default.
    pub models: Vec<String>,
    pub store: String,
}

/// The levels the effort picker offers. Rows in the same shape the command menu
/// takes, so the picker is arrowed and taken with the keys the composer already
/// answers rather than a second selection mechanism.
pub const EFFORT_ROWS: &[(&str, &str)] = &[
    ("low", "least reasoning, fastest and cheapest"),
    ("medium", "balanced"),
    ("high", "most reasoning, slowest and dearest"),
    ("off", "send no reasoning setting at all"),
];

/// ponytail: the menu is capped rather than scrolled. It holds every command
/// there is; give it a window over `menu()` if the table outgrows the cap.
/// Rows the slash menu may occupy on a terminal tall enough for them.
///
/// Above the number of commands, so the whole table is offered rather than
/// silently truncated at the bottom — which is where `/quit` and `/help` live,
/// and hiding the way out is the one thing a menu must not do. A short
/// terminal still narrows it; that bound is the screen's, not this one.
pub(super) const MENU_ROWS: usize = 24;

/// What `/help` prints, built from the same table the menu offers.
pub fn help(colour: bool) -> String {
    let label = COMMANDS
        .iter()
        .map(|(name, _)| name.chars().count())
        .max()
        .unwrap_or(0);
    let mut text = String::new();
    for (name, description) in COMMANDS {
        text.push_str(&format!(
            "  {}{}{}\n",
            paint(colour, sgr_accent(), name),
            " ".repeat(label - name.chars().count() + 2),
            paint(colour, sgr_dim(), description),
        ));
    }
    for line in [
        "Type / to open this menu; Up/Down: input history, or the menu while one is open",
        "Enter: take the highlighted command, or send a line that is already one",
        "Esc/Ctrl-C: cancel turn · Ctrl-D: exit on empty input",
        "Shift+Tab: step the approval mode (default, acceptEdits, plan, auto)",
        // Hooks the engine loaded do run, and `/hooks` marks which; an MCP
        // declaration is still only a reading of a file.
        "MCP connections are not loaded by ARSY; imported declarations grant no authority. `/hooks` marks the hooks that run.",
    ] {
        text.push_str(&paint(colour, sgr_dim(), line));
        text.push('\n');
    }
    text
}

/// The input line ARSY owns: its text, its caret, and how many rows it last
#[derive(Default)]
pub struct Composer {
    pub(super) buffer: String,
    pub(super) caret: usize,
    pub(super) drawn: bool,
    pub(super) top_status: bool,
    pub(super) history: std::collections::VecDeque<String>,
    pub(super) history_index: Option<usize>,
    pub(super) draft: String,
    /// Which menu row Up/Down has landed on, clamped to the matches on use.
    pub(super) selected: usize,
    /// Rows a picker put in front of the reader, offered instead of the command
    /// table for as long as it is collecting an answer.
    pub(super) offered: Option<Vec<(String, String)>>,
    /// Set while the line is a secret being typed: it is painted as bullets,
    /// never kept in history, and never offered a menu.
    pub(super) masked: bool,
    /// Set while the line is a picker answer rather than a task, so the menu
    /// does not offer commands that the picker would not accept.
    pub(super) picking: bool,
    /// Terminal rows, refreshed with the width; `0` means not measured yet.
    pub(super) height: usize,
}

impl Composer {
    pub fn restore(&mut self, text: String) {
        self.caret = text.chars().count();
        self.buffer = text;
        self.selected = 0;
    }

    /// The commands the line offers right now.
    ///
    /// The menu is only open while the line is still one word: once an argument
    /// is being typed the command is already chosen, and a list of commands
    /// would cover the terminal for the rest of the line.
    pub fn set_picking(&mut self, picking: bool) {
        self.picking = picking;
        self.selected = 0;
    }

    /// Collect the line as a secret. Nothing about it reaches the screen, the
    /// scrollback, or the history a later Up would walk back into.
    pub fn set_masked(&mut self, masked: bool) {
        self.masked = masked;
    }

    /// Put a picker's rows in the menu, marked at `selected`.
    ///
    /// Called once per line from the prompt state, so a selection never
    /// outlives the answer it was made for.
    pub fn offer(&mut self, rows: Option<Vec<(String, String)>>, selected: usize) {
        self.offered = rows;
        self.selected = selected;
    }

    /// The same, for a picker whose rows are a fixed table.
    pub fn offer_table(&mut self, rows: Option<&[(&str, &str)]>, selected: usize) {
        self.offer(
            rows.map(|rows| {
                rows.iter()
                    .map(|(name, description)| ((*name).to_owned(), (*description).to_owned()))
                    .collect()
            }),
            selected,
        );
    }

    /// Measured with the width, and on the same schedule.
    pub fn set_height(&mut self, rows: usize) {
        self.height = rows;
    }

    fn all_matches(&self) -> Vec<(String, String)> {
        if self.masked {
            return Vec::new();
        }
        if let Some(rows) = &self.offered {
            return rows
                .iter()
                .filter(|(name, _)| name.starts_with(&self.buffer))
                .cloned()
                .collect();
        }
        if self.picking || !self.buffer.starts_with('/') || self.buffer.contains(' ') {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(&self.buffer))
            .map(|(name, description)| ((*name).to_owned(), (*description).to_owned()))
            .collect()
    }

    pub(super) fn menu_window(&self) -> (Vec<(String, String)>, usize) {
        let capacity = self.menu_capacity();
        if capacity == 0 {
            return (Vec::new(), 0);
        }
        let matches = self.all_matches();
        let total = matches.len();
        if total == 0 {
            return (Vec::new(), 0);
        }
        let selected = self.selected.min(total - 1);
        if total <= capacity {
            return (matches, selected);
        }
        let start = if selected >= capacity {
            selected + 1 - capacity
        } else {
            0
        };
        let window = matches[start..(start + capacity).min(total)].to_vec();
        (window, selected - start)
    }

    pub fn menu(&self) -> Vec<(String, String)> {
        self.all_matches()
    }

    /// The row the picker is on right now, for a live preview of a choice
    /// before Enter takes it. `None` when no menu is open.
    pub fn highlighted(&self) -> Option<String> {
        let matches = self.all_matches();
        let idx = self.selected.min(matches.len().checked_sub(1)?);
        matches.get(idx).map(|(name, _)| name.clone())
    }

    /// How many menu rows the terminal can hold.
    fn menu_capacity(&self) -> usize {
        match self.height {
            0 => MENU_ROWS,
            rows => MENU_ROWS.min(rows.saturating_sub(4)),
        }
    }

    /// Move the mark over the open menu. Both ends wrap, so a short list is
    /// never a dead end in one direction, and the index is clamped to the
    /// current matches first: a selection left over from a wider list must not
    /// step outside a narrowed one.
    fn mark(&mut self, down: bool) -> Action {
        let matches = self.all_matches();
        if matches.is_empty() {
            return Action::None;
        }
        let last = matches.len().saturating_sub(1);
        let selected = self.selected.min(last);
        self.selected = if down {
            if selected >= last {
                0
            } else {
                selected + 1
            }
        } else {
            selected.checked_sub(1).unwrap_or(last)
        };
        Action::Redraw
    }

    /// Walk the submitted lines. Going back past the newest returns the draft
    /// that was stashed on the way in, so browsing history cannot lose a line
    /// that was being typed.
    fn recall(&mut self, back: bool) -> Action {
        self.history_index = if back {
            Some(match self.history_index {
                Some(index) => index.saturating_sub(1),
                None => {
                    self.draft = self.buffer.clone();
                    self.history.len() - 1
                }
            })
        } else {
            self.history_index
                .map(|index| index + 1)
                .filter(|index| *index < self.history.len())
        };
        self.buffer = self
            .history_index
            .map_or_else(|| self.draft.clone(), |index| self.history[index].clone());
        self.caret = self.buffer.chars().count();
        Action::Redraw
    }

    pub fn press(&mut self, key: Key) -> Action {
        // Editing the line, moving through it, and what ends it are three
        // separate readings of the same key; the first that owns the key
        // answers.
        if let Some(action) = self.edit(key) {
            return action;
        }
        if let Some(action) = self.navigate(key) {
            return action;
        }
        self.finish(key)
    }

    /// The keys that change the text of the line.
    fn edit(&mut self, key: Key) -> Option<Action> {
        match key {
            Key::Char(character) if !character.is_control() => {
                self.buffer.insert(self.byte_at(self.caret), character);
                self.caret += 1;
            }
            Key::Newline => {
                self.buffer.insert(self.byte_at(self.caret), '\n');
                self.caret += 1;
            }
            Key::Backspace if self.caret > 0 => {
                self.buffer.remove(self.byte_at(self.caret - 1));
                self.caret -= 1;
            }
            Key::Delete if self.caret < self.buffer.chars().count() => {
                self.buffer.remove(self.byte_at(self.caret));
            }
            Key::WordBackspace => {
                self.word_backspace();
                return Some(Action::Redraw);
            }
            _ => return None,
        }
        // Any edit makes the menu's mark stale: it belonged to the line as it
        // read before the key.
        self.selected = 0;
        Some(Action::Redraw)
    }

    /// The keys that move through the line, the menu, or the history.
    fn navigate(&mut self, key: Key) -> Option<Action> {
        match key {
            // An open menu owns Up/Down: it is the list in front of the reader,
            // and history is still one Escape or Backspace away. The ends wrap,
            // so a short list is never a dead end in one direction.
            Key::Up if !self.menu().is_empty() => Some(self.mark(false)),
            Key::Down if !self.menu().is_empty() => Some(self.mark(true)),
            Key::Up if !self.history.is_empty() => Some(self.recall(true)),
            Key::Down if self.history_index.is_some() => Some(self.recall(false)),
            Key::Left if self.caret > 0 => {
                self.caret -= 1;
                Some(Action::Redraw)
            }
            Key::Right if self.caret < self.buffer.chars().count() => {
                self.caret += 1;
                Some(Action::Redraw)
            }
            Key::WordLeft => {
                self.word_left();
                Some(Action::Redraw)
            }
            Key::WordRight => {
                self.word_right();
                Some(Action::Redraw)
            }
            Key::Home => {
                self.caret = 0;
                Some(Action::Redraw)
            }
            Key::End => {
                self.caret = self.buffer.chars().count();
                Some(Action::Redraw)
            }
            _ => None,
        }
    }

    /// The keys that end the line, one way or another.
    fn finish(&mut self, key: Key) -> Action {
        match key {
            // Enter takes the highlighted command, unless the line already is
            // one: otherwise a typed-out `/quit` would refuse to send itself.
            Key::Enter if self.completion().is_some() => {
                self.restore(self.completion().unwrap_or_default());
                Action::Redraw
            }
            Key::Enter => Action::Submit(self.submit()),
            // Ctrl-C clears a drafted line first, and only quits once there is
            // nothing left to lose.
            Key::Interrupt if !self.buffer.is_empty() => {
                self.take();
                Action::Redraw
            }
            // Shift+Tab is handled by the session loop immediately. The draft
            // stays in the composer, and no synthetic task enters history.
            Key::CycleMode if !self.picking && !self.masked => Action::CycleMode,
            Key::Interrupt | Key::Eof if self.buffer.is_empty() => Action::Quit,
            _ => Action::None,
        }
    }

    /// Take the line and remember it, unless it is blank, masked, or the same
    /// as the line before it.
    fn submit(&mut self) -> String {
        let line = self.take();
        if !self.masked && !line.trim().is_empty() && self.history.back() != Some(&line) {
            self.history.push_back(line.clone());
            if self.history.len() > 100 {
                self.history.pop_front();
            }
        }
        line
    }

    /// The row the mark is on, for a caller that needs to see the selection
    /// without pressing Enter to find out.
    pub fn marked(&self) -> Option<String> {
        let menu = self.menu();
        menu.get(self.selected.min(menu.len().checked_sub(1)?))
            .map(|(name, _)| name.clone())
    }

    /// The command Enter would fill in, or `None` when the line is already one
    /// and Enter should send it.
    fn completion(&self) -> Option<String> {
        let menu = self.menu();
        let selected = menu.get(self.selected.min(menu.len().checked_sub(1)?))?;
        (!menu.iter().any(|(name, _)| *name == self.buffer)).then(|| selected.0.clone())
    }

    fn take(&mut self) -> String {
        self.caret = 0;
        self.history_index = None;
        self.selected = 0;
        self.draft.clear();
        std::mem::take(&mut self.buffer)
    }

    fn byte_at(&self, caret: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(caret)
            .map_or(self.buffer.len(), |(at, _)| at)
    }

    fn word_left(&mut self) {
        let chars: Vec<char> = self.buffer.chars().collect();
        let mut idx = self.caret.min(chars.len());
        while idx > 0 && !chars[idx - 1].is_alphanumeric() {
            idx -= 1;
        }
        while idx > 0 && chars[idx - 1].is_alphanumeric() {
            idx -= 1;
        }
        self.caret = idx;
    }

    fn word_right(&mut self) {
        let chars: Vec<char> = self.buffer.chars().collect();
        let len = chars.len();
        let mut idx = self.caret.min(len);
        while idx < len && chars[idx].is_alphanumeric() {
            idx += 1;
        }
        while idx < len && !chars[idx].is_alphanumeric() {
            idx += 1;
        }
        self.caret = idx;
    }

    fn word_backspace(&mut self) {
        let chars: Vec<char> = self.buffer.chars().collect();
        let old_caret = self.caret.min(chars.len());
        let mut idx = old_caret;
        while idx > 0 && !chars[idx - 1].is_alphanumeric() {
            idx -= 1;
        }
        while idx > 0 && chars[idx - 1].is_alphanumeric() {
            idx -= 1;
        }
        let start_byte = self.byte_at(idx);
        let end_byte = self.byte_at(old_caret);
        self.buffer.drain(start_byte..end_byte);
        self.caret = idx;
        self.selected = 0;
    }

    fn caret_line_col(&self) -> (usize, usize, usize) {
        let chars: Vec<char> = self.buffer.chars().collect();
        let total_chars = chars.len();
        let caret = self.caret.min(total_chars);
        let mut line_idx = 0;
        let mut col_offset = 0;
        for &ch in &chars[..caret] {
            if ch == '\n' {
                line_idx += 1;
                col_offset = 0;
            } else {
                col_offset += 1;
            }
        }
        let total_lines = self.buffer.split('\n').count().max(1);
        (line_idx, col_offset, total_lines)
    }

    /// Paint the block — pad, input, pad, menu, status — with the status row
    /// at the bottom, so model, effort, directory and branch anchor the prompt.
    pub fn render(&mut self, width: usize, colour: bool, status: &str) -> String {
        let width = width.max(MIN_WIDTH);
        let status = fit(status, width);
        let room = width.saturating_sub(3);
        let menu = self.menu_rows(width, colour);
        let (line_idx, col_offset, total_lines) = self.caret_line_col();
        let mut frame = String::new();
        if self.drawn {
            frame.push_str(RESET);
            let lines_above = line_idx + if self.top_status { 2 } else { 1 };
            frame.push_str(&format!("\x1b[{}A", lines_above));
            frame.push('\r');
            frame.push_str(CLEAR_BELOW);
        }
        self.drawn = true;
        self.top_status = false;
        let surface = if colour {
            format!("{}{CLEAR_EOL}", sgr_input_bg())
        } else {
            String::new()
        };
        // Top surface pad
        frame.push_str(&surface);
        frame.push('\n');
        // Input lines
        let mut active_caret_col = col_offset;
        if self.masked {
            let (text, caret) = self.window(room);
            active_caret_col = caret;
            frame.push_str(&format!(
                "{surface}{} {text}{}\n",
                if colour {
                    format!("{}›", sgr_input_bg())
                } else {
                    "›".to_owned()
                },
                if colour { CLEAR_EOL } else { "" },
            ));
        } else {
            for (idx, line) in self.buffer.split('\n').enumerate() {
                let prompt_char = if idx == 0 { "›" } else { "·" };
                let prompt_str = if colour {
                    format!("{}{prompt_char}", sgr_input_bg())
                } else {
                    prompt_char.to_owned()
                };
                let chars: Vec<char> = line.chars().collect();
                let is_active = idx == line_idx;
                let (fitted_line, _) = if is_active {
                    let (w_text, w_caret) = Self::window_line(&chars, col_offset, room);
                    active_caret_col = w_caret;
                    (w_text, w_caret)
                } else {
                    Self::window_line(&chars, 0, room)
                };
                frame.push_str(&format!(
                    "{surface}{prompt_str} {fitted_line}{}\n",
                    if colour { CLEAR_EOL } else { "" },
                ));
            }
        }
        // Bottom surface pad
        frame.push_str(&format!("{surface}{}\n", if colour { RESET } else { "" }));
        for row in &menu {
            frame.push_str(row);
            frame.push('\n');
        }
        frame.push_str(&status);
        // Back onto the active input row, over the bottom pad, the menu, and the status row
        let lines_below = (total_lines.saturating_sub(1 + line_idx)) + 1 + menu.len() + 1;
        frame.push_str(&format!(
            "\x1b[{}A\r\x1b[{}C",
            lines_below,
            active_caret_col + 2
        ));
        frame
    }

    /// Paint the block with the live status (e.g. spinner and elapsed seconds)
    /// at the top, directly under the streaming output and above the input box,
    /// and the footer (model, effort, directory, branch) at the bottom.
    pub fn render_turn(
        &mut self,
        width: usize,
        colour: bool,
        status: &str,
        footer: &str,
    ) -> String {
        let width = width.max(MIN_WIDTH);
        let status = fit(status, width);
        let footer = fit(footer, width);
        let room = width.saturating_sub(3);
        let menu = self.menu_rows(width, colour);
        let (line_idx, col_offset, total_lines) = self.caret_line_col();
        let mut frame = String::new();
        if self.drawn {
            frame.push_str(RESET);
            let lines_above = line_idx + if self.top_status { 2 } else { 1 };
            frame.push_str(&format!("\x1b[{}A", lines_above));
            frame.push('\r');
            frame.push_str(CLEAR_BELOW);
        }
        self.drawn = true;
        self.top_status = true;
        let surface = if colour {
            format!("{}{CLEAR_EOL}", sgr_input_bg())
        } else {
            String::new()
        };
        frame.push_str(&status);
        frame.push('\n');
        // Top surface pad
        frame.push_str(&surface);
        frame.push('\n');
        // Input lines
        let mut active_caret_col = col_offset;
        if self.masked {
            let (text, caret) = self.window(room);
            active_caret_col = caret;
            frame.push_str(&format!(
                "{surface}{} {text}{}\n",
                if colour {
                    format!("{}›", sgr_input_bg())
                } else {
                    "›".to_owned()
                },
                if colour { CLEAR_EOL } else { "" },
            ));
        } else {
            for (idx, line) in self.buffer.split('\n').enumerate() {
                let prompt_char = if idx == 0 { "›" } else { "·" };
                let prompt_str = if colour {
                    format!("{}{prompt_char}", sgr_input_bg())
                } else {
                    prompt_char.to_owned()
                };
                let chars: Vec<char> = line.chars().collect();
                let is_active = idx == line_idx;
                let (fitted_line, _) = if is_active {
                    let (w_text, w_caret) = Self::window_line(&chars, col_offset, room);
                    active_caret_col = w_caret;
                    (w_text, w_caret)
                } else {
                    Self::window_line(&chars, 0, room)
                };
                frame.push_str(&format!(
                    "{surface}{prompt_str} {fitted_line}{}\n",
                    if colour { CLEAR_EOL } else { "" },
                ));
            }
        }
        // Bottom surface pad
        frame.push_str(&format!("{surface}{}\n", if colour { RESET } else { "" }));
        for row in &menu {
            frame.push_str(row);
            frame.push('\n');
        }
        frame.push_str(&footer);
        // Back onto the active input row, over the bottom pad, the menu, and the footer
        let lines_below = (total_lines.saturating_sub(1 + line_idx)) + 1 + menu.len() + 1;
        frame.push_str(&format!(
            "\x1b[{}A\r\x1b[{}C",
            lines_below,
            active_caret_col + 2
        ));
        frame
    }

    fn window_line(chars: &[char], caret_in_line: usize, room: usize) -> (String, usize) {
        let budget = room.saturating_sub(1);
        let width = |character: &char| character.width().unwrap_or(0);
        let mut start = caret_in_line.min(chars.len());
        let mut caret = 0;
        while start > 0 && caret + width(&chars[start - 1]) <= budget {
            start -= 1;
            caret += width(&chars[start]);
        }
        let mut used = 0;
        let text = chars[start..]
            .iter()
            .take_while(|character| {
                used += width(character);
                used <= budget
            })
            .collect();
        (text, caret)
    }

    /// One row per offered command, marked at the selection.
    fn menu_rows(&self, width: usize, colour: bool) -> Vec<String> {
        let (window, visible_selected) = self.menu_window();
        let Some(last) = window.len().checked_sub(1) else {
            return Vec::new();
        };
        let selected = visible_selected.min(last);
        let label = window
            .iter()
            .map(|(name, _)| name.chars().count())
            .max()
            .unwrap_or(0);
        window
            .iter()
            .enumerate()
            .map(|(index, (name, description))| {
                let chosen = index == selected;
                let row = format!(
                    "  {} {}{}{}",
                    paint(colour, sgr_accent(), if chosen { "›" } else { " " }),
                    paint(
                        colour,
                        if chosen { sgr_accent() } else { sgr_bullet() },
                        name
                    ),
                    " ".repeat(label.saturating_sub(name.chars().count()) + 2),
                    paint(colour, sgr_dim(), description),
                );
                fit(&row, width)
            })
            .collect()
    }

    /// Erase the block so turn output starts on a clean row, and keep the
    /// submitted line in the scrollback the way a shell would.
    pub fn commit(&mut self, submitted: &str, colour: bool) -> String {
        let mut out = self.clear();
        for (i, line) in submitted.lines().enumerate() {
            let prompt = if i == 0 { "› You" } else { "·" };
            out.push_str(&format!(
                "{} {}\n",
                paint(colour, BOLD, prompt),
                paint(colour, sgr_assistant(), line),
            ));
        }
        if submitted.trim().is_empty() {
            out.push('\n');
        }
        out
    }

    pub fn clear(&mut self) -> String {
        if !std::mem::take(&mut self.drawn) {
            return String::new();
        }
        let (line_idx, _, _) = self.caret_line_col();
        let lines_above = line_idx + if self.top_status { 2 } else { 1 };
        format!("{RESET}\x1b[{}A\r{CLEAR_BELOW}", lines_above)
    }

    /// Forget terminal coordinates after a display rebuild.
    pub fn invalidate(&mut self) {
        self.drawn = false;
        self.top_status = false;
    }

    /// Slide the visible text so the caret stays on the row instead of
    /// wrapping, which would break the block's row count.
    pub(super) fn window(&self, room: usize) -> (String, usize) {
        // A masked line is one bullet per character, so what is painted is the
        // same width as what was typed and the caret still lands where the
        // reader expects it.
        let characters: Vec<char> = if self.masked {
            std::iter::repeat_n('•', self.buffer.chars().count()).collect()
        } else {
            self.buffer.chars().collect()
        };
        let budget = room.saturating_sub(1);
        let width = |character: &char| character.width().unwrap_or(0);
        let mut start = self.caret;
        let mut caret = 0;
        while start > 0 && caret + width(&characters[start - 1]) <= budget {
            start -= 1;
            caret += width(&characters[start]);
        }
        let mut used = 0;
        let text = characters[start..]
            .iter()
            .take_while(|character| {
                used += width(character);
                used <= budget
            })
            .collect();
        (text, caret)
    }
}
