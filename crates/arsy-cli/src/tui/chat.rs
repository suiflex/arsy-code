//! Composer, input actions, and assistant conversation rendering.
use super::*;

/// The slash commands the composer offers and `/help` prints. One table, so a
/// command cannot appear in the menu and not in the help, or the reverse.
pub const COMMANDS: &[(&str, &str)] = &[
    ("/new", "start a fresh session"),
    ("/clear", "clear conversation context in place"),
    ("/compact", "fold the older conversation into a summary now"),
    ("/resume", "resume a recorded session; [SESSION_ID]"),
    ("/rename", "rename current session; <TITLE>"),
    (
        "/session",
        "sessions; alone opens the manager | list | rename <TITLE> | delete [ID]",
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
    (
        "/agents",
        "show agents; pause|resume|cancel|steer ATTEMPT [MESSAGE]",
    ),
    (
        "/provider",
        "providers: choose access (OAuth/key/custom/local), add, sign in, or remove",
    ),
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
    (
        "/hooks",
        "lifecycle hooks; alone opens the manager | list, --event NAME",
    ),
    (
        "/skill",
        "skills; alone opens the manager | list [--source KIND]",
    ),
    (
        "/settings",
        "settings; alone opens the editor | show effective configuration, [KEY]",
    ),
    (
        "/storage",
        "where ARSY keeps files and how big each is; clean caches, views, history",
    ),
    ("/doctor", "check workspace, storage, and sandbox assurance"),
    (
        "/auth",
        "alias of /provider (access, sign-in, and credentials)",
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

/// Where a credential typed into the TUI is put. One row, because there is one
/// place: a 0600 file beside the configuration.
pub const PROVIDER_STORES: &[(&str, &str)] = &[(
    "file",
    "a 0600 file beside the configuration; no unlock prompt",
)];

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
    /// Rows drawn above the input box in the last frame: the live status and
    /// the queued follow-ups over it. The erase moves up past all of them.
    pub(super) top_rows: usize,
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
    /// The text of a bracketed paste still arriving, collected so it lands in
    /// the line as one edit rather than one redraw per character.
    pub(super) pasting: Option<String>,
    /// Large pastes shown in the line as a placeholder, with the text each
    /// one stands for. The placeholder is swapped back when the line is taken.
    pub(super) pastes: Vec<(String, String)>,
    /// Lines sent while a turn ran, oldest first, each marked whether it
    /// steers the running turn or waits for it to end. Kept here rather than
    /// in the round's outcome because the composer outlives every round of a
    /// turn, and drawn above the input while they wait.
    pub(super) held: Vec<(String, bool)>,
    /// The row the caret was drawn on, counted in wrapped rows. The erase
    /// before the next frame moves up by this, not by where the caret is
    /// now: a key has already moved it by then.
    pub(super) caret_row: usize,
    /// Wrapped input rows in the last frame, which the menu makes room for.
    pub(super) input_rows: usize,
}

/// The input as it is drawn: each row with its marker, and where the caret
/// sits among them.
pub(super) struct InputRows {
    pub(super) rows: Vec<(&'static str, String)>,
    pub(super) caret_row: usize,
    pub(super) caret_col: usize,
}

/// Wrap `text` into rows no wider than `room` columns, measured as printed,
/// and find the row and column of the caret at character `caret`.
///
/// The first row of the first line is marked `›`, the first row of each
/// later line `·`, and a row the wrap continued is left unmarked, so a
/// newline the operator typed still reads apart from a wrap they did not.
/// One column is kept free so the caret has somewhere to sit at a row's end.
pub(super) fn wrap_input(text: &str, caret: usize, room: usize) -> InputRows {
    let budget = room.saturating_sub(1).max(1);
    let mut rows = vec![("›", String::new())];
    let mut used = 0;
    let (mut caret_row, mut caret_col) = (0, 0);
    for (index, character) in text.chars().enumerate() {
        if index == caret {
            (caret_row, caret_col) = (rows.len() - 1, used);
        }
        if character == '\n' {
            rows.push(("·", String::new()));
            used = 0;
            continue;
        }
        let width = character.width().unwrap_or(0);
        if used + width > budget {
            rows.push((" ", String::new()));
            used = 0;
        }
        if let Some((_, row)) = rows.last_mut() {
            row.push(character);
        }
        used += width;
    }
    if caret >= text.chars().count() {
        (caret_row, caret_col) = (rows.len() - 1, used);
    }
    InputRows {
        rows,
        caret_row,
        caret_col,
    }
}

/// A paste longer than this many lines is shown as a placeholder.
const PASTE_INLINE_LINES: usize = 2;
/// A paste longer than this many characters is shown as a placeholder.
const PASTE_INLINE_CHARS: usize = 800;

impl Composer {
    /// Whether nothing has been typed into the line.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Keep a line to run as a follow-up once the turn ends.
    pub fn hold(&mut self, line: String) {
        self.held.push((line, false));
    }

    /// Keep a line to steer the running turn: it joins the conversation at
    /// the next point the turn talks to the model again. A turn that ends
    /// first runs it as a follow-up, like a held line.
    pub fn steer(&mut self, line: String) {
        self.held.push((line, true));
    }

    /// The steering lines, oldest first, leaving the follow-ups where they are.
    pub fn take_steering(&mut self) -> Vec<String> {
        let (steering, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|(_, steer)| *steer);
        self.held = waiting;
        steering.into_iter().map(|(line, _)| line).collect()
    }

    /// How many lines are waiting.
    pub fn held_len(&self) -> usize {
        self.held.len()
    }

    /// The lines `hold` kept, oldest first.
    pub fn take_held(&mut self) -> Vec<String> {
        std::mem::take(&mut self.held)
            .into_iter()
            .map(|(line, _)| line)
            .collect()
    }

    pub fn restore(&mut self, text: String) {
        self.pastes.clear();
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
            rows => MENU_ROWS.min(rows.saturating_sub(3 + self.input_rows.max(1))),
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
        if let Some(pasted) = self.pasting.as_mut() {
            match key {
                Key::Char(character) if !character.is_control() => {
                    pasted.push(character);
                    return Action::None;
                }
                Key::Newline => {
                    pasted.push('\n');
                    return Action::None;
                }
                Key::PasteEnd => {
                    let text = self.pasting.take().unwrap_or_default();
                    return self.paste(&text);
                }
                _ => {}
            }
        }
        match key {
            Key::PasteStart => {
                self.pasting = Some(String::new());
                return Action::None;
            }
            Key::PasteEnd => return Action::None,
            _ => {}
        }
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
            Key::Backspace if self.paste_before_caret().is_some() => {
                let index = self.paste_before_caret().unwrap_or_default();
                let (token, _) = self.pastes.remove(index);
                let end = self.byte_at(self.caret);
                self.buffer.drain(end - token.len()..end);
                self.caret -= token.chars().count();
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

    /// Up or Down: through a picker's rows, the history being walked, the
    /// command menu, or into the history, in that order.
    fn vertical(&mut self, down: bool) -> Option<Action> {
        // When a picker offers rows, Up/Down always navigate the menu, even
        // if filtering narrows the matches to none. This prevents Up/Down
        // from falling through to history navigation.
        if self.offered.is_some() {
            return Some(self.mark(down));
        }
        // Walking the history keeps walking it. A recalled `/model` opens the
        // command menu, and letting that menu take the next Up left every
        // line sent before the command out of reach.
        if self.history_index.is_some() {
            return Some(self.recall(!down));
        }
        // An open menu owns Up/Down: it is the list in front of the reader,
        // and history is still one Escape or Backspace away. The ends wrap,
        // so a short list is never a dead end in one direction.
        if !self.menu().is_empty() {
            return Some(self.mark(down));
        }
        (!down && !self.history.is_empty()).then(|| self.recall(true))
    }

    /// The keys that move through the line, the menu, or the history.
    fn navigate(&mut self, key: Key) -> Option<Action> {
        match key {
            Key::Up => self.vertical(false),
            Key::Down => self.vertical(true),
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
            // Ctrl-O expands the last tool call's output, the way Shift+Tab
            // changes the mode where it stands: the composer hands the key up
            // rather than turning it into text. A control byte rather than a
            // letter, so a prompt that starts with `e` is still typing.
            Key::Expand if !self.picking && !self.masked => Action::Expand,
            Key::CycleMode if !self.picking && !self.masked => Action::CycleMode,
            Key::CycleEffort if !self.picking && !self.masked => Action::CycleEffort,
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
        let mut line = std::mem::take(&mut self.buffer);
        for (token, text) in self.pastes.drain(..) {
            line = line.replacen(&token, &text, 1);
        }
        line
    }

    /// Put a finished paste into the line at the caret.
    ///
    /// A short paste goes in as typed, line breaks included. A long one goes
    /// in as a placeholder, so the line stays readable and the whole text is
    /// still what is sent. A secret or a picker answer is one line, so there
    /// its line breaks become spaces.
    fn paste(&mut self, text: &str) -> Action {
        if text.is_empty() {
            return Action::None;
        }
        let lines = text.lines().count();
        let inserted = if self.masked || self.picking {
            text.replace('\n', " ")
        } else if lines > PASTE_INLINE_LINES || text.chars().count() > PASTE_INLINE_CHARS {
            let number = self.pastes.len() + 1;
            let token = if lines > 1 {
                format!("[Pasted text #{number} +{lines} lines]")
            } else {
                format!("[Pasted text #{number} {} chars]", text.chars().count())
            };
            self.pastes.push((token.clone(), text.to_owned()));
            token
        } else {
            text.to_owned()
        };
        self.buffer.insert_str(self.byte_at(self.caret), &inserted);
        self.caret += inserted.chars().count();
        self.selected = 0;
        Action::Redraw
    }

    /// The placeholder that ends right at the caret, if any, so Backspace
    /// removes a collapsed paste whole instead of one bracket at a time.
    fn paste_before_caret(&self) -> Option<usize> {
        let before = &self.buffer[..self.byte_at(self.caret)];
        self.pastes
            .iter()
            .position(|(token, _)| before.ends_with(token.as_str()))
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

    /// The rows this frame draws for the input, windowed around the caret
    /// when the input is taller than the screen can hold with the rest of
    /// the block, and remembered so the menu and the next erase use them.
    fn input_rows(&mut self, room: usize) -> InputRows {
        let mut input = if self.masked {
            let (text, caret) = self.window(room);
            InputRows {
                rows: vec![("›", text)],
                caret_row: 0,
                caret_col: caret,
            }
        } else {
            wrap_input(&self.buffer, self.caret, room)
        };
        let most = match self.height {
            0 => usize::MAX,
            rows => rows.saturating_sub(6).max(1),
        };
        if input.rows.len() > most {
            let start = (input.caret_row + 1).saturating_sub(most);
            input.rows = input.rows.split_off(start);
            input.rows.truncate(most);
            input.caret_row -= start;
        }
        self.input_rows = input.rows.len();
        input
    }

    /// Move back to the top of the block last drawn, and clear it.
    fn erase_drawn(&self) -> String {
        let lines_above = self.caret_row + 1 + self.top_rows;
        format!("{RESET}\x1b[{lines_above}A\r{CLEAR_BELOW}")
    }

    /// Paint the block — pad, input, pad, menu, status — with the status row
    /// at the bottom, so model, effort, directory and branch anchor the prompt.
    pub fn render(&mut self, width: usize, colour: bool, status: &str) -> String {
        if modern_style() {
            return self.render_modern(width, colour, None, status);
        }
        let width = width.max(MIN_WIDTH);
        let status = fit(status, width);
        self.render_classic(width, colour, None, &status)
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
        if modern_style() {
            return self.render_modern(width, colour, Some(status), footer);
        }
        let width = width.max(MIN_WIDTH);
        let status = fit(status, width);
        let footer = fit(footer, width);
        self.render_classic(width, colour, Some(&status), &footer)
    }

    /// The filled-slab composer: an optional live status on top, the input
    /// between two surface pads, the menu, and `bottom` as the last row.
    fn render_classic(
        &mut self,
        width: usize,
        colour: bool,
        status: Option<&str>,
        bottom: &str,
    ) -> String {
        let room = width.saturating_sub(3);
        let input = self.input_rows(room);
        let menu = self.menu_rows(width, colour);
        let mut frame = String::new();
        if self.drawn {
            frame.push_str(&self.erase_drawn());
        }
        self.drawn = true;
        self.caret_row = input.caret_row;
        let top = self.top_block(width, colour, status);
        self.top_rows = top.len();
        let surface = if colour {
            format!("{}{CLEAR_EOL}", sgr_input_bg())
        } else {
            String::new()
        };
        for row in &top {
            frame.push_str(row);
            frame.push('\n');
        }
        // Top surface pad
        frame.push_str(&surface);
        frame.push('\n');
        for (marker, text) in &input.rows {
            let marker = if colour {
                format!("{}{marker}", sgr_input_bg())
            } else {
                (*marker).to_owned()
            };
            frame.push_str(&format!(
                "{surface}{marker} {text}{}\n",
                if colour { CLEAR_EOL } else { "" },
            ));
        }
        // Bottom surface pad
        frame.push_str(&format!("{surface}{}\n", if colour { RESET } else { "" }));
        for row in &menu {
            frame.push_str(row);
            frame.push('\n');
        }
        frame.push_str(bottom);
        // Back onto the caret's row, over the rows below it, the bottom pad,
        // the menu, and the last row.
        let lines_below = input.rows.len() - 1 - input.caret_row + 1 + menu.len() + 1;
        frame.push_str(&format!(
            "\x1b[{}A\r\x1b[{}C",
            lines_below,
            input.caret_col + 2
        ));
        frame
    }

    /// The mockup's composer: a quiet outlined field, with transient status
    /// above it and the session footer below. Multiline input keeps the same
    /// frame and adds body rows rather than reverting to the old filled slab.
    fn render_modern(
        &mut self,
        width: usize,
        colour: bool,
        status: Option<&str>,
        footer: &str,
    ) -> String {
        let width = width.max(MIN_WIDTH);
        let room = width.saturating_sub(6);
        let input = self.input_rows(room);
        let menu = self.menu_rows(width, colour);
        let mut frame = String::new();
        if self.drawn {
            frame.push_str(&self.erase_drawn());
        }
        self.drawn = true;
        self.caret_row = input.caret_row;
        let top = self.top_block(width, colour, status);
        self.top_rows = top.len();
        for row in &top {
            frame.push_str(row);
            frame.push('\n');
        }

        let border = |text: &str| paint(colour, sgr_border(), text);
        frame.push_str(&border(&format!(
            "┌{}┐",
            "─".repeat(width.saturating_sub(2))
        )));
        frame.push('\n');

        for (index, (marker, text)) in input.rows.iter().enumerate() {
            let placeholder =
                index == 0 && self.buffer.is_empty() && !self.picking && self.offered.is_none();
            let text = if placeholder {
                paint(colour, sgr_dim(), "ask for the next change")
            } else {
                text.clone()
            };
            let body = format!(
                "{} {} {}",
                border("│"),
                paint(colour, sgr_accent(), marker),
                text,
            );
            let pad = " ".repeat(width.saturating_sub(1 + visible_len(&body)));
            frame.push_str(&format!("{body}{pad}{}\n", border("│")));
        }

        frame.push_str(&border(&format!(
            "└{}┘",
            "─".repeat(width.saturating_sub(2))
        )));
        frame.push('\n');
        for row in &menu {
            frame.push_str(row);
            frame.push('\n');
        }
        frame.push_str(&fit(footer, width));

        let lines_below = input.rows.len() - 1 - input.caret_row + 1 + menu.len() + 1;
        frame.push_str(&format!(
            "\x1b[{lines_below}A\r\x1b[{}C",
            input.caret_col + 4
        ));
        frame
    }

    /// The rows above the input box while a turn runs: every follow-up the
    /// operator queued, oldest first, then the live status.
    ///
    /// A queued line is shown where it waits, so the operator can see it was
    /// taken and what will run next; without it a line sent mid-turn simply
    /// vanished until the turn ended. Only the first row of each is shown, and
    /// at most `QUEUED_SHOWN` of them, so the block cannot push the input off
    /// a short terminal.
    fn top_block(&self, width: usize, colour: bool, status: Option<&str>) -> Vec<String> {
        const QUEUED_SHOWN: usize = 3;
        let Some(status) = status else {
            return Vec::new();
        };
        // A blank row keeps the live status off the last line of the
        // conversation; it is part of the composer, so it goes when it does.
        let mut rows = vec![String::new()];
        rows.extend(self.held.iter().take(QUEUED_SHOWN).map(|(line, steer)| {
            let first = line.lines().next().unwrap_or_default();
            let label = if *steer { "steer ›" } else { "queued ›" };
            fit(
                &format!(
                    "  {} {}",
                    paint(colour, sgr_dim(), label),
                    paint(colour, sgr_dim(), first)
                ),
                width,
            )
        }));
        if self.held.len() > QUEUED_SHOWN {
            rows.push(paint(
                colour,
                sgr_dim(),
                &format!("  … {} more queued", self.held.len() - QUEUED_SHOWN),
            ));
        }
        rows.push(fit(status, width));
        rows
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
        if modern_style() && !submitted.trim().is_empty() {
            // The same strip the repaint path draws, so a prompt does not
            // change appearance the moment something forces a redraw.
            out.push_str(&prompt_strip(terminal_width(), colour, submitted));
            out.push('\n');
            return out;
        }
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
        self.erase_drawn()
    }

    /// Forget terminal coordinates after a display rebuild.
    pub fn invalidate(&mut self) {
        self.drawn = false;
        self.top_rows = 0;
        self.caret_row = 0;
    }

    /// Slide the visible text so the caret stays on one row. Used for a
    /// masked line: bullets are not worth wrapping.
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
