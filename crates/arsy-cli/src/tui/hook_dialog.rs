//! The interactive `/hooks` dialog: move through the lifecycle hooks that are
//! declared, and switch one off or back on.
//!
//! Switching a hook off is ARSY's own record in the operator's `arsy.json`
//! (`hook.disabled`), for the same reason `/mcp` writes `arsy.json` and never
//! another tool's settings file: a file the operator did not write is not a
//! file ARSY rewrites. The engine reads that record when it builds the rules,
//! so a declaration listed as off here really does not run.
//!
//! `a` adds a `command` hook to ARSY's own `guard.json`, the operator's or the
//! project's, and `x` removes one that file declared. Claude's and Codex's
//! files are only ever switched, never edited.
use super::*;

/// One row of the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookChoice {
    /// The key `hook.disabled` holds for this declaration. This is what a
    /// toggle writes, so it must be the key the engine builds for the same
    /// handler.
    pub declaration: String,
    /// The lifecycle the hook runs on, as it is spelled in the file.
    pub event: String,
    /// What the hook runs, shortened for the row.
    pub matcher: String,
    /// The file that declared it, with the tool that owns it.
    pub source: String,
    /// Whether the hook runs: off means a `hook.disabled` entry exists for it.
    pub enabled: bool,
    /// Whether ARSY's own `guard.json` declared it, so `x` may remove it.
    pub removable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookAction {
    /// Flip `hook.disabled` on the declaration at this row.
    Toggle(usize),
    /// Append a hook to the `guard.json` the scope names.
    Add {
        scope: SettingsScope,
        event: String,
        matcher: String,
        command: String,
    },
    /// Remove the declaration at this row from its `guard.json`.
    Remove(usize),
    Close,
}

/// A hook being described before it is added.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookDraft {
    /// Which of scope, event, matcher, command has the cursor.
    pub field: usize,
    pub scope: SettingsScope,
    /// An index into `arsy_code::hook::EXTERNAL_EVENTS`.
    pub event: usize,
    pub matcher: String,
    pub command: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookDialogMode {
    List,
    Adding(HookDraft),
    ConfirmRemove(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookDialogState {
    pub choices: Vec<HookChoice>,
    pub selected: usize,
    /// What the last action did, shown inside the frame rather than printed
    /// under it.
    pub notice: Option<String>,
    pub mode: HookDialogMode,
}

impl HookDialogState {
    pub fn new(choices: Vec<HookChoice>) -> Self {
        Self {
            choices,
            selected: 0,
            notice: None,
            mode: HookDialogMode::List,
        }
    }

    /// Replace the rows after a change, keeping the marker on the same
    /// declaration.
    pub fn reload(&mut self, choices: Vec<HookChoice>) {
        let declaration = self
            .choices
            .get(self.selected)
            .map(|choice| choice.declaration.clone());
        self.selected = declaration
            .and_then(|declaration| {
                choices
                    .iter()
                    .position(|choice| choice.declaration == declaration)
            })
            .unwrap_or(0)
            .min(choices.len().saturating_sub(1));
        self.choices = choices;
        self.mode = HookDialogMode::List;
    }

    pub fn render(&self, width: usize, colour: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let inner = width.saturating_sub(4);
        match &self.mode {
            HookDialogMode::List => self.render_list(width, inner, colour),
            HookDialogMode::Adding(draft) => {
                render_draft(draft, self.notice.as_deref(), width, inner, colour)
            }
            HookDialogMode::ConfirmRemove(index) => {
                let mut lines = vec![dialog_top(" REMOVE HOOK ", width, colour)];
                if let Some(choice) = self.choices.get(*index) {
                    lines.push(dialog_line(
                        &format!("Remove the {} hook on `{}`?", choice.event, choice.matcher),
                        inner,
                        colour,
                        sgr_err(),
                    ));
                    lines.push(dialog_line(&choice.source, inner, colour, sgr_dim()));
                }
                lines.push(dialog_line("", inner, colour, ""));
                lines.push(dialog_line(
                    "[y/Enter] Remove  [n/Esc] Back",
                    inner,
                    colour,
                    sgr_dim(),
                ));
                lines.push(paint(
                    colour,
                    sgr_border(),
                    &format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
                ));
                lines.join("\n")
            }
        }
    }

    fn render_list(&self, width: usize, inner: usize, colour: bool) -> String {
        let mut lines = vec![dialog_top(" HOOKS ", width, colour)];
        if self.choices.is_empty() {
            lines.push(dialog_line(
                "  no lifecycle hook is declared",
                inner,
                colour,
                sgr_dim(),
            ));
        }
        let width_of = |column: usize| {
            self.choices
                .iter()
                .map(|choice| visible_len(&hook_cell(choice, column)))
                .max()
                .unwrap_or(0)
        };
        let (event_width, matcher_width) = (width_of(0), width_of(1));
        lines.extend(self.choices.iter().enumerate().map(|(index, choice)| {
            let marked = index == self.selected;
            let style = match (marked, choice.enabled) {
                (true, _) => sgr_accent(),
                (false, true) => "",
                _ => sgr_dim(),
            };
            dialog_line(
                &hook_row(choice, marked, event_width, matcher_width),
                inner,
                colour,
                style,
            )
        }));
        lines.push(dialog_line("", inner, colour, ""));
        if let Some(notice) = &self.notice {
            lines.push(dialog_line(notice, inner, colour, sgr_dim()));
        }
        lines.push(dialog_line(
            "[↑/↓] Navigate  [Space/Enter] Toggle  [a] Add  [x] Remove  [Esc] Close",
            inner,
            colour,
            sgr_dim(),
        ));
        lines.push(paint(
            colour,
            sgr_border(),
            &format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        ));
        lines.join("\n")
    }

    pub fn handle_key(&mut self, key: Key) -> Option<HookAction> {
        match &self.mode {
            HookDialogMode::List => self.list_key(key),
            HookDialogMode::Adding(_) => self.adding_key(key),
            HookDialogMode::ConfirmRemove(index) => self.confirm_remove_key(*index, key),
        }
    }

    fn list_key(&mut self, key: Key) -> Option<HookAction> {
        match key {
            Key::Char('a' | 'A') => {
                self.notice = None;
                self.mode = HookDialogMode::Adding(HookDraft::default());
                None
            }
            Key::Char('x' | 'X') => {
                self.begin_remove();
                None
            }
            Key::Up => {
                self.step(false);
                None
            }
            Key::Down => {
                self.step(true);
                None
            }
            Key::Char(' ') | Key::Enter | Key::Newline => self
                .choices
                .get(self.selected)
                .is_some()
                .then_some(HookAction::Toggle(self.selected)),
            Key::Interrupt | Key::Eof => Some(HookAction::Close),
            _ => None,
        }
    }

    /// Ask before removing the marked hook, or say why it cannot be.
    fn begin_remove(&mut self) {
        match self.choices.get(self.selected) {
            Some(choice) if choice.removable => {
                self.mode = HookDialogMode::ConfirmRemove(self.selected);
            }
            Some(_) => {
                self.notice = Some(
                    "only a hook in ARSY's own guard.json can be removed here; switch this one off instead"
                        .to_owned(),
                );
            }
            None => {}
        }
    }

    fn adding_key(&mut self, key: Key) -> Option<HookAction> {
        let HookDialogMode::Adding(draft) = &mut self.mode else {
            return None;
        };
        let (action, done) = draft_key(draft, key);
        // Kept only until the next key, which is the operator acting on it.
        self.notice = action.as_ref().err().map(|reason| (*reason).to_owned());
        if done {
            self.mode = HookDialogMode::List;
        }
        action.ok().flatten()
    }

    fn confirm_remove_key(&mut self, index: usize, key: Key) -> Option<HookAction> {
        match key {
            Key::Enter | Key::Newline | Key::Char('y' | 'Y') => {
                self.mode = HookDialogMode::List;
                Some(HookAction::Remove(index))
            }
            Key::Char('n' | 'N') | Key::Interrupt => {
                self.mode = HookDialogMode::List;
                None
            }
            Key::Eof => Some(HookAction::Close),
            _ => None,
        }
    }

    /// Move the marker one row, wrapping at either end.
    fn step(&mut self, forward: bool) {
        let count = self.choices.len();
        if count == 0 {
            return;
        }
        self.selected = if forward {
            (self.selected + 1) % count
        } else {
            (self.selected + count - 1) % count
        };
    }
}

/// One key while a hook is being described: the action it produced, if any,
/// and whether the form is finished. An `Err` is a notice, not a failure.
fn draft_key(draft: &mut HookDraft, key: Key) -> (Result<Option<HookAction>, &'static str>, bool) {
    match key {
        Key::Tab | Key::Down => draft.field = (draft.field + 1) % 4,
        Key::Up => draft.field = (draft.field + 3) % 4,
        Key::Enter | Key::Newline => return submit_draft(draft),
        Key::Interrupt => return (Ok(None), true),
        Key::Eof => return (Ok(Some(HookAction::Close)), true),
        other => edit_draft_field(draft, other),
    }
    (Ok(None), false)
}

/// Change the field the cursor is on: the arrows choose the file and the
/// event, and typing edits the matcher and the command.
fn edit_draft_field(draft: &mut HookDraft, key: Key) {
    let events = arsy_code::hook::EXTERNAL_EVENTS.len();
    match (draft.field, key) {
        (0, Key::Left | Key::Right) => {
            draft.scope = match draft.scope {
                SettingsScope::User => SettingsScope::Project,
                SettingsScope::Project => SettingsScope::User,
            };
        }
        (1, Key::Left) => draft.event = (draft.event + events - 1) % events,
        (1, Key::Right) => draft.event = (draft.event + 1) % events,
        (2, Key::Char(character)) => draft.matcher.push(character),
        (3, Key::Char(character)) => draft.command.push(character),
        (2, Key::Backspace) => {
            draft.matcher.pop();
        }
        (3, Key::Backspace) => {
            draft.command.pop();
        }
        _ => {}
    }
}

fn submit_draft(draft: &HookDraft) -> (Result<Option<HookAction>, &'static str>, bool) {
    if draft.command.trim().is_empty() {
        return (Err("a hook needs a command to run"), false);
    }
    let action = HookAction::Add {
        scope: draft.scope,
        event: arsy_code::hook::EXTERNAL_EVENTS[draft.event].to_owned(),
        matcher: draft.matcher.trim().to_owned(),
        command: draft.command.trim().to_owned(),
    };
    (Ok(Some(action)), true)
}

/// The add form: one row per field, the one with the cursor marked.
fn render_draft(
    draft: &HookDraft,
    notice: Option<&str>,
    width: usize,
    inner: usize,
    colour: bool,
) -> String {
    let mut lines = vec![dialog_top(" ADD HOOK ", width, colour)];
    let fields = [
        ("file", format!("‹ {} ›", draft.scope.label())),
        (
            "event",
            format!("‹ {} ›", arsy_code::hook::EXTERNAL_EVENTS[draft.event]),
        ),
        ("matcher", draft.matcher.clone()),
        ("command", draft.command.clone()),
    ];
    for (index, (name, value)) in fields.iter().enumerate() {
        let marked = index == draft.field;
        let cursor = if marked && index >= 2 { "█" } else { "" };
        lines.push(dialog_line(
            &format!(
                "{} {name:<8} {value}{cursor}",
                if marked { "›" } else { " " }
            ),
            inner,
            colour,
            if marked { sgr_accent() } else { sgr_dim() },
        ));
    }
    lines.push(dialog_line("", inner, colour, ""));
    // Why Enter did not add the hook, shown on the form it was pressed in
    // rather than only once the form is left.
    if let Some(notice) = notice {
        lines.push(dialog_line(notice, inner, colour, sgr_err()));
    }
    if draft.scope == SettingsScope::Project {
        lines.push(dialog_line(
            "a project hook runs only once this directory is trusted",
            inner,
            colour,
            sgr_dim(),
        ));
    }
    lines.push(dialog_line(
        "[Tab/↑/↓] Field  [←/→] Choose  [Enter] Add  [Esc] Back",
        inner,
        colour,
        sgr_dim(),
    ));
    lines.push(paint(
        colour,
        sgr_border(),
        &format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
    ));
    lines.join("\n")
}

/// One column of a row, named so the two width calls cannot drift apart.
fn hook_cell(choice: &HookChoice, column: usize) -> String {
    match column {
        0 => choice.event.clone(),
        1 => choice.matcher.clone(),
        _ => choice.source.clone(),
    }
}

/// One hook as a row: marker, state, event, matcher, where it came from.
fn hook_row(choice: &HookChoice, marked: bool, event_width: usize, matcher_width: usize) -> String {
    let event = format!("{:<event_width$}", hook_cell(choice, 0));
    let matcher = format!("{:<matcher_width$}", hook_cell(choice, 1));
    format!(
        "{} {}  {event}  {matcher}  {}",
        if marked { "›" } else { " " },
        if choice.enabled { "● on " } else { "○ off" },
        choice.source,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(declaration: &str, enabled: bool) -> HookChoice {
        HookChoice {
            declaration: declaration.to_owned(),
            event: "PreToolUse".to_owned(),
            matcher: "Bash".to_owned(),
            source: "claude · user".to_owned(),
            enabled,
            removable: false,
        }
    }

    #[test]
    fn a_hook_is_described_field_by_field_and_added() {
        let mut dialog = HookDialogState::new(vec![choice("a", true)]);
        assert_eq!(dialog.handle_key(Key::Char('a')), None);
        assert!(dialog.render(80, false).contains("ADD HOOK"));

        dialog.handle_key(Key::Right); // file: project
        dialog.handle_key(Key::Tab);
        dialog.handle_key(Key::Right); // event: the second one
        dialog.handle_key(Key::Tab);
        for character in "fs.read".chars() {
            dialog.handle_key(Key::Char(character));
        }
        dialog.handle_key(Key::Tab);
        assert_eq!(dialog.handle_key(Key::Enter), None, "no command yet");
        assert!(dialog.notice.is_some());
        for character in "deny.sh".chars() {
            dialog.handle_key(Key::Char(character));
        }
        assert_eq!(
            dialog.handle_key(Key::Enter),
            Some(HookAction::Add {
                scope: SettingsScope::Project,
                event: arsy_code::hook::EXTERNAL_EVENTS[1].to_owned(),
                matcher: "fs.read".to_owned(),
                command: "deny.sh".to_owned(),
            })
        );
        assert_eq!(dialog.mode, HookDialogMode::List);
    }

    /// Enter on a form with no command says why on the form itself, and the
    /// next key clears it.
    #[test]
    fn the_add_form_shows_why_it_did_not_add() {
        let mut dialog = HookDialogState::new(Vec::new());
        dialog.handle_key(Key::Char('a'));
        assert_eq!(dialog.handle_key(Key::Enter), None);
        assert!(dialog
            .render(80, false)
            .contains("a hook needs a command to run"));
        dialog.handle_key(Key::Tab);
        assert!(!dialog.render(80, false).contains("a hook needs a command"));
    }

    #[test]
    fn only_a_hook_from_arsys_own_file_can_be_removed() {
        let mut own = choice("mine", true);
        own.removable = true;
        let mut dialog = HookDialogState::new(vec![choice("theirs", true), own]);
        assert_eq!(dialog.handle_key(Key::Char('x')), None);
        assert_eq!(dialog.mode, HookDialogMode::List, "Claude's file stays");
        assert!(dialog.notice.is_some());

        dialog.handle_key(Key::Down);
        dialog.handle_key(Key::Char('x'));
        assert_eq!(dialog.mode, HookDialogMode::ConfirmRemove(1));
        assert_eq!(
            dialog.handle_key(Key::Char('y')),
            Some(HookAction::Remove(1))
        );
    }

    #[test]
    fn space_toggles_a_declaration_and_up_wraps_to_the_last() {
        let mut dialog = HookDialogState::new(vec![choice("a", true), choice("b", true)]);
        assert_eq!(
            dialog.handle_key(Key::Char(' ')),
            Some(HookAction::Toggle(0))
        );
        assert_eq!(dialog.handle_key(Key::Up), None);
        assert_eq!(dialog.selected, 1);
        assert_eq!(dialog.handle_key(Key::Enter), Some(HookAction::Toggle(1)));
        assert_eq!(dialog.handle_key(Key::Interrupt), Some(HookAction::Close));
    }

    #[test]
    fn the_rows_name_what_each_hook_runs_and_where_it_came_from() {
        let dialog = HookDialogState::new(vec![choice("a", false)]);
        let frame = dialog.render(80, false);
        assert!(frame.contains(" HOOKS "), "{frame}");
        assert!(frame.contains("PreToolUse"), "{frame}");
        assert!(frame.contains("Bash"), "{frame}");
        assert!(frame.contains("claude · user"), "{frame}");
        assert!(frame.contains("○ off"), "{frame}");
        // The empty case says so rather than drawing an empty frame.
        let empty = HookDialogState::new(Vec::new()).render(80, false);
        assert!(empty.contains("no lifecycle hook is declared"), "{empty}");
    }

    #[test]
    fn a_reload_keeps_the_marker_on_the_same_declaration() {
        let mut dialog = HookDialogState::new(vec![choice("a", true), choice("b", true)]);
        dialog.selected = 1;
        dialog.reload(vec![
            choice("a", true),
            choice("b", false),
            choice("c", true),
        ]);
        assert_eq!(dialog.selected, 1);
        assert!(dialog.render(80, false).contains("○ off"));
    }
}
