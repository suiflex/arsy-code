//! Session picker and session-management dialog.
use super::*;

/// A recorded session choice for `/resume` selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionChoice {
    pub id: SessionId,
    pub title: Option<String>,
    pub events: u64,
    pub last_seen: String,
}

/// Build rows for `/resume` interactive picker.
pub fn session_rows(
    sessions: &[SessionChoice],
    current: Option<SessionId>,
) -> (Option<Vec<(String, String)>>, usize) {
    if sessions.is_empty() {
        return (
            Some(vec![(
                "no recorded sessions".to_owned(),
                "type a task to create a new session".to_owned(),
            )]),
            0,
        );
    }
    let mut selected = 0;
    let rows: Vec<(String, String)> = sessions
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            if Some(s.id) == current {
                selected = idx;
            }
            let label = choice_label(s);
            let desc = format!("{} events · {}", s.events, s.last_seen);
            (label, desc)
        })
        .collect();
    (Some(rows), selected)
}

pub fn session_prompt(sessions: &[SessionChoice], colour: bool) -> String {
    let choices = if sessions.is_empty() {
        "no sessions".to_owned()
    } else {
        format!("Up/Down then Enter, an ID, or 1-{}", sessions.len())
    };
    paint(colour, sgr_dim(), &format!("  resume · {choices}"))
}

/// The top border of a dialog, with its title set into the rule.
pub(super) fn dialog_top(title: &str, width: usize, colour: bool) -> String {
    let top_right = "─".repeat(width.saturating_sub(2 + 2 + visible_len(title)));
    format!(
        "{}{}{}",
        paint(colour, sgr_border(), "╭──"),
        paint(colour, BOLD, title),
        paint(colour, sgr_border(), &format!("{top_right}╮")),
    )
}

/// One row inside a dialog's border, cut to fit and padded to reach it.
pub(super) fn dialog_line(content: &str, inner: usize, colour: bool, sgr: &str) -> String {
    let fitted = fit(content, inner);
    let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
    format!(
        "{} {}{pad} {}",
        paint(colour, sgr_border(), "│"),
        paint(colour, sgr, &fitted),
        paint(colour, sgr_border(), "│"),
    )
}

/// Actions resulting from the interactive session dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionAction {
    Resume(SessionId),
    Rename(SessionId, String),
    Delete(SessionId),
    Cancel,
}

/// Operational mode for the interactive session dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionDialogMode {
    Select,
    Rename,
    ConfirmDelete,
}

/// State for the interactive `/session` dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDialogState {
    pub sessions: Vec<SessionChoice>,
    pub selected: usize,
    pub mode: SessionDialogMode,
    pub rename_buffer: String,
    pub active_session: SessionId,
}

impl SessionDialogState {
    /// `active_title` is what the store holds for the running session. The
    /// list below cannot carry it when that session has recorded nothing yet,
    /// so the caller reads it separately.
    pub fn new(
        sessions: Vec<SessionChoice>,
        active_session: SessionId,
        active_title: Option<String>,
    ) -> Self {
        Self {
            selected: 0,
            sessions: Self::including_active(sessions, active_session, active_title),
            mode: SessionDialogMode::Select,
            rename_buffer: String::new(),
            active_session,
        }
    }

    /// The list with the session this process is running as a real row.
    ///
    /// A session that has not recorded a turn — plan mode, a refused turn, an
    /// operator still deciding — is nowhere in the store, but it is the one
    /// the operator is working in: rename and delete act on rows, so the row
    /// has to exist rather than be drawn by the renderer alone.
    ///
    /// A title can be saved before the first turn is, so the injected row takes
    /// the stored one rather than none: otherwise a rename on a fresh session
    /// is written and then drawn as if it never happened.
    fn including_active(
        sessions: Vec<SessionChoice>,
        active_session: SessionId,
        active_title: Option<String>,
    ) -> Vec<SessionChoice> {
        if sessions.iter().any(|s| s.id == active_session) {
            return sessions;
        }
        let mut listed = vec![SessionChoice {
            id: active_session,
            title: active_title,
            events: 0,
            last_seen: "this session".to_owned(),
        }];
        listed.extend(sessions);
        listed
    }

    /// Replace the list, keeping the active session in it.
    pub fn reload(&mut self, sessions: Vec<SessionChoice>, active_title: Option<String>) {
        self.sessions = Self::including_active(sessions, self.active_session, active_title);
        self.selected = 0;
    }

    pub fn render(&self, width: usize, colour: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let inner = width.saturating_sub(4);
        let rule = "─".repeat(width.saturating_sub(2));

        match self.mode {
            SessionDialogMode::Select => {
                let title = " SESSIONS ";
                let mut lines = vec![dialog_top(title, width, colour)];

                // The session this process is running is a real row: `new`
                // and `reload` keep it in the list, so rename and delete act
                // on what the operator sees.
                for (idx, s) in self.sessions.iter().enumerate() {
                    let is_sel = idx == self.selected;
                    let is_active = s.id == self.active_session;
                    let radio = if is_sel { "(•)" } else { "( )" };
                    let active_tag = if is_active { " [active]" } else { "" };
                    let row_label = format!("{radio} {}. {}{active_tag}", idx + 1, choice_label(s));
                    let sgr = if is_sel { sgr_accent() } else { sgr_dim() };
                    lines.push(dialog_line(&row_label, inner, colour, sgr));
                    let detail = format!("     {} events · {}", s.events, s.last_seen);
                    lines.push(dialog_line(&detail, inner, colour, sgr_dim()));
                }

                lines.push(dialog_line("", inner, colour, ""));
                lines.push(dialog_line(
                    "[↑/↓] Navigate  [Enter] Resume  [r] Rename  [d] Delete  [Esc] Cancel",
                    inner,
                    colour,
                    sgr_dim(),
                ));
                lines.push(paint(colour, sgr_border(), &format!("╰{rule}╯")));
                lines.join("\n")
            }
            SessionDialogMode::Rename => {
                let title = " RENAME SESSION ";
                let mut lines = vec![dialog_top(title, width, colour)];

                if let Some(target) = self.sessions.get(self.selected) {
                    let sess_row = format!("Session: {}", target.id);
                    lines.push(dialog_line(&sess_row, inner, colour, sgr_dim()));
                    if let Some(cur) = &target.title {
                        let cur_row = format!("Current: {cur}");
                        lines.push(dialog_line(&cur_row, inner, colour, sgr_dim()));
                    }
                }
                lines.push(dialog_line("", inner, colour, ""));
                let input_row = format!("New title: {}█", self.rename_buffer);
                lines.push(dialog_line(&input_row, inner, colour, sgr_accent()));
                lines.push(dialog_line("", inner, colour, ""));
                lines.push(dialog_line(
                    "[Enter] Save Title  [Esc] Back to Session List",
                    inner,
                    colour,
                    sgr_dim(),
                ));
                lines.push(paint(colour, sgr_border(), &format!("╰{rule}╯")));
                lines.join("\n")
            }
            SessionDialogMode::ConfirmDelete => {
                let title = " DELETE SESSION ";
                let mut lines = vec![dialog_top(title, width, colour)];

                if let Some(target) = self.sessions.get(self.selected) {
                    let msg = format!("Are you sure you want to delete session {}?", target.id);
                    lines.push(dialog_line(&msg, inner, colour, sgr_err()));
                    if let Some(t) = &target.title {
                        let t_row = format!("Title: \"{t}\"");
                        lines.push(dialog_line(&t_row, inner, colour, sgr_dim()));
                    }
                    lines.push(dialog_line(
                        "This will permanently remove its recorded history and events.",
                        inner,
                        colour,
                        sgr_dim(),
                    ));
                }
                lines.push(dialog_line("", inner, colour, ""));
                lines.push(dialog_line(
                    "[y/Enter] Confirm Delete  [n/Esc] Cancel",
                    inner,
                    colour,
                    sgr_dim(),
                ));
                lines.push(paint(colour, sgr_border(), &format!("╰{rule}╯")));
                lines.join("\n")
            }
        }
    }

    pub fn handle_key(&mut self, key: Key) -> Option<SessionAction> {
        // Each mode reads the same keys differently, so which mode the dialog
        // is in is the whole decision here.
        match self.mode {
            SessionDialogMode::Select => self.select_key(key),
            SessionDialogMode::Rename => self.rename_key(key),
            SessionDialogMode::ConfirmDelete => self.confirm_delete_key(key),
        }
    }

    /// Picking a session from the list, or opening one of the other modes on
    /// the one the marker stands on.
    fn select_key(&mut self, key: Key) -> Option<SessionAction> {
        match key {
            Key::Up => {
                self.step(false);
                None
            }
            Key::Down => {
                self.step(true);
                None
            }
            Key::Enter | Key::Newline => Some(
                self.marked()
                    .map_or(SessionAction::Cancel, SessionAction::Resume),
            ),
            Key::Char('r' | 'R') => {
                if let Some(target) = self.sessions.get(self.selected) {
                    self.rename_buffer = target.title.clone().unwrap_or_default();
                    self.mode = SessionDialogMode::Rename;
                }
                None
            }
            Key::Char('d' | 'D') => {
                if !self.sessions.is_empty() {
                    self.mode = SessionDialogMode::ConfirmDelete;
                }
                None
            }
            // A number takes that row directly, counting from one.
            Key::Char(digit) if digit.is_ascii_digit() && digit != '0' => {
                let index = (digit as usize) - ('1' as usize);
                let target = self.sessions.get(index)?;
                let id = target.id;
                self.selected = index;
                Some(SessionAction::Resume(id))
            }
            Key::Interrupt => Some(SessionAction::Cancel),
            _ => None,
        }
    }

    /// Typing a new title. Leaving without one returns to the list.
    fn rename_key(&mut self, key: Key) -> Option<SessionAction> {
        match key {
            Key::Enter | Key::Newline => {
                let title = self.rename_buffer.trim().to_owned();
                if title.is_empty() {
                    // Nothing typed is not a rename: committing it would
                    // write an empty title the list then renders as ` · ""`.
                    // Staying in the mode is also what leaves the operator
                    // their buffer to finish.
                    return None;
                }
                let id = self.marked()?;
                Some(SessionAction::Rename(id, title))
            }
            Key::Char(character) => {
                self.rename_buffer.push(character);
                None
            }
            Key::Backspace => {
                self.rename_buffer.pop();
                None
            }
            Key::Interrupt => {
                self.mode = SessionDialogMode::Select;
                None
            }
            _ => None,
        }
    }

    fn confirm_delete_key(&mut self, key: Key) -> Option<SessionAction> {
        match key {
            Key::Enter | Key::Newline | Key::Char('y' | 'Y') => {
                let id = self.marked()?;
                Some(SessionAction::Delete(id))
            }
            Key::Char('n' | 'N') | Key::Interrupt => {
                self.mode = SessionDialogMode::Select;
                None
            }
            _ => None,
        }
    }

    /// The session the marker stands on. Taking `None` returns to the list,
    /// because a mode opened on a row that is no longer there has nothing to
    /// act on.
    fn marked(&mut self) -> Option<SessionId> {
        match self.sessions.get(self.selected) {
            Some(target) => Some(target.id),
            None => {
                self.mode = SessionDialogMode::Select;
                None
            }
        }
    }

    /// Move the marker one row, wrapping at either end.
    fn step(&mut self, forward: bool) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = if forward {
            if self.selected + 1 >= self.sessions.len() {
                0
            } else {
                self.selected + 1
            }
        } else {
            self.selected
                .checked_sub(1)
                .unwrap_or_else(|| self.sessions.len().saturating_sub(1))
        };
    }
}

/// The label the picker shows for a session, and the one `Composer::finish`
/// restores into the line when Enter picks a row: what the session is called,
/// then the first block of its id, kept for reference. `session_rows` and the
/// session dialog build the same string.
fn choice_label(choice: &SessionChoice) -> String {
    let id = choice.id.to_string();
    let short = id.split('-').next().unwrap_or(&id);
    match &choice.title {
        Some(title) => format!("{title} · {short}"),
        None => format!("(untitled) · {short}"),
    }
}

/// The fewest characters of a session's UUID the picker will resolve.
///
/// The first block of a UUID, which is what an operator copies off a row.
/// Also the line between "this is a row number" and "this is an ID": no
/// realistic list has ten million rows.
const SHORTEST_PREFIX: usize = 8;

pub fn resolve_session_answer(
    answer: &str,
    sessions: &[SessionChoice],
    current: SessionId,
) -> Result<SessionId, String> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(current);
    }
    // A row number, when there is a row with that number.
    //
    // An answer that parses as a number and names no row is usually a
    // mistyped row number — but roughly one UUID in forty begins with eight
    // decimal digits, and refusing that prefix as an out-of-range row makes
    // the picker reject an answer naming exactly one session. So a *short*
    // numeric answer stays a number, and one at least as long as the
    // shortest prefix the picker accepts is allowed to be read as one.
    let out_of_range = match answer.parse::<usize>() {
        Ok(number) => match number.checked_sub(1).and_then(|idx| sessions.get(idx)) {
            Some(choice) => return Ok(choice.id),
            None if sessions.is_empty() => return Err("no sessions found".to_owned()),
            None if answer.len() < SHORTEST_PREFIX => {
                return Err(format!("no session {number}; choose 1-{}", sessions.len()))
            }
            None => Some(number),
        },
        Err(_) => None,
    };
    // The row as the picker wrote it. A titled row comes back as
    // `<uuid> · <title>`, which is neither a number nor a UUID on its own.
    if let Some(choice) = sessions.iter().find(|s| choice_label(s) == answer) {
        return Ok(choice.id);
    }
    // A leading prefix of a session's UUID.
    if let Some(choice) = sessions
        .iter()
        .find(|s| s.id.to_string().starts_with(answer))
    {
        return Ok(choice.id);
    }
    // The title the operator gave it, so the name they chose is as good an
    // answer as the ID they never read.
    if let Some(choice) = sessions.iter().find(|s| {
        s.title
            .as_deref()
            .is_some_and(|title| title.starts_with(answer))
    }) {
        return Ok(choice.id);
    }
    // A UUID this list does not hold is still a UUID the store may know.
    if let Ok(id) = answer.parse::<SessionId>() {
        return Ok(id);
    }
    // Nothing read it as anything else, so it really was a number naming a
    // row that is not there — and that is the more useful message.
    if let Some(number) = out_of_range {
        return Err(format!("no session {number}; choose 1-{}", sessions.len()));
    }
    Err(format!("`{answer}` is not a valid session ID"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(title: Option<&str>) -> SessionChoice {
        SessionChoice {
            id: SessionId::new(),
            title: title.map(str::to_owned),
            events: 3,
            last_seen: "2m ago".to_owned(),
        }
    }

    fn dialog(sessions: Vec<SessionChoice>) -> SessionDialogState {
        // The active session is the list's first row, so `new` injects
        // nothing and the rows are exactly the ones the test passed.
        let active = sessions.first().map(|s| s.id).unwrap_or_default();
        SessionDialogState::new(sessions, active, None)
    }

    #[test]
    fn up_and_down_wrap_around_the_list() {
        let mut dialog = dialog(vec![session(None), session(None)]);

        assert_eq!(dialog.handle_key(Key::Up), None);
        assert_eq!(
            dialog.selected,
            dialog.sessions.len() - 1,
            "Up from the first row wraps to the last"
        );
        assert_eq!(dialog.handle_key(Key::Down), None);
        assert_eq!(dialog.selected, 0);
        assert_eq!(dialog.handle_key(Key::Down), None);
        assert_eq!(
            dialog.selected, 1,
            "Down from the last row wraps to the first"
        );
    }

    #[test]
    fn enter_resumes_the_marked_row_and_interrupt_cancels() {
        let first = session(None);
        let second = session(None);
        let (first_id, second_id) = (first.id, second.id);
        let mut dialog = dialog(vec![first, second]);

        assert_eq!(
            dialog.handle_key(Key::Enter),
            Some(SessionAction::Resume(first_id))
        );
        dialog.handle_key(Key::Down);
        assert_eq!(
            dialog.handle_key(Key::Newline),
            Some(SessionAction::Resume(second_id))
        );
        assert_eq!(
            dialog.handle_key(Key::Interrupt),
            Some(SessionAction::Cancel)
        );
    }

    #[test]
    fn a_digit_resumes_that_row() {
        let first = session(None);
        let second = session(Some("feature work"));
        let (first_id, second_id) = (first.id, second.id);
        let mut dialog = dialog(vec![first, second]);

        assert_eq!(
            dialog.handle_key(Key::Char('2')),
            Some(SessionAction::Resume(second_id))
        );
        assert_eq!(dialog.selected, 1, "the marker follows the row picked");
        assert_eq!(
            dialog.handle_key(Key::Char('1')),
            Some(SessionAction::Resume(first_id))
        );
        assert_eq!(dialog.handle_key(Key::Char('9')), None, "no such row");
        assert_eq!(
            dialog.handle_key(Key::Char('0')),
            None,
            "rows count from one"
        );
        assert_eq!(dialog.mode, SessionDialogMode::Select);
    }

    #[test]
    fn r_opens_rename_on_the_marked_row() {
        let sessions = vec![session(None), session(Some("feature work"))];
        let second = sessions[1].id;
        let mut dialog = dialog(sessions);

        assert_eq!(dialog.handle_key(Key::Char('r')), None);
        assert_eq!(dialog.mode, SessionDialogMode::Rename);
        assert_eq!(dialog.rename_buffer, "", "an untitled row starts empty");
        assert_eq!(dialog.handle_key(Key::Interrupt), None);
        assert_eq!(dialog.mode, SessionDialogMode::Select);

        dialog.handle_key(Key::Down);
        assert_eq!(dialog.selected, 1);
        dialog.handle_key(Key::Char('R'));
        assert_eq!(dialog.mode, SessionDialogMode::Rename);
        assert_eq!(
            dialog.rename_buffer, "feature work",
            "a titled row starts from the title it has"
        );

        dialog.handle_key(Key::Backspace);
        dialog.handle_key(Key::Char('!'));
        assert_eq!(
            dialog.handle_key(Key::Enter),
            Some(SessionAction::Rename(second, "feature wor!".to_owned()))
        );

        // Esc drops the buffer and returns to the list.
        dialog.rename_buffer = "discard me".to_owned();
        assert_eq!(dialog.handle_key(Key::Interrupt), None);
        assert_eq!(dialog.mode, SessionDialogMode::Select);
    }

    #[test]
    fn an_empty_title_is_not_committed() {
        let target = session(Some("feature work"));
        let id = target.id;
        let mut dialog = dialog(vec![target]);
        dialog.handle_key(Key::Char('r'));
        for _ in 0.."feature work".len() {
            dialog.handle_key(Key::Backspace);
        }

        assert_eq!(
            dialog.handle_key(Key::Enter),
            None,
            "nothing typed, nothing renamed"
        );
        assert_eq!(
            dialog.mode,
            SessionDialogMode::Rename,
            "the operator keeps typing"
        );
        assert_eq!(dialog.handle_key(Key::Newline), None);
        assert_eq!(dialog.mode, SessionDialogMode::Rename);

        // Whitespace is no more of a title than nothing is.
        dialog.handle_key(Key::Char(' '));
        assert_eq!(dialog.handle_key(Key::Enter), None);
        assert_eq!(dialog.mode, SessionDialogMode::Rename);
        dialog.handle_key(Key::Char('x'));
        assert_eq!(
            dialog.handle_key(Key::Enter),
            Some(SessionAction::Rename(id, "x".to_owned()))
        );
    }

    #[test]
    fn d_asks_before_deleting() {
        let sessions = vec![session(None), session(Some("feature work"))];
        let second = sessions[1].id;
        let mut dialog = dialog(sessions);

        dialog.handle_key(Key::Down);
        assert_eq!(dialog.handle_key(Key::Char('d')), None);
        assert_eq!(dialog.mode, SessionDialogMode::ConfirmDelete);
        assert!(dialog.render(80, false).contains("permanently remove"));

        assert_eq!(dialog.handle_key(Key::Char('n')), None);
        assert_eq!(dialog.mode, SessionDialogMode::Select);

        // Enter confirms as readily as `y` does, once the mode is armed.
        dialog.handle_key(Key::Char('d'));
        assert_eq!(
            dialog.handle_key(Key::Enter),
            Some(SessionAction::Delete(second))
        );

        // Esc backs out of the confirmation, as `n` does.
        dialog.mode = SessionDialogMode::Select;
        dialog.handle_key(Key::Char('d'));
        assert_eq!(dialog.handle_key(Key::Interrupt), None);
        assert_eq!(dialog.mode, SessionDialogMode::Select);
    }

    /// A fresh session is not in the store's list until it records a turn, but
    /// its title is saved the moment it is renamed. The row must show it.
    #[test]
    fn a_session_with_no_turns_shows_the_title_it_was_given() {
        let mut dialog = SessionDialogState::new(Vec::new(), SessionId::new(), None);
        assert_eq!(dialog.sessions[0].title, None);

        dialog.reload(Vec::new(), Some("feature work".to_owned()));

        assert_eq!(dialog.sessions[0].title.as_deref(), Some("feature work"));
        assert!(
            dialog.render(80, false).contains("feature work"),
            "the saved title is drawn"
        );
    }

    #[test]
    fn a_store_without_this_session_leaves_one_row() {
        // The list always holds the running session, even when the store has
        // recorded nothing: it is the row rename and delete act on.
        let mut dialog = SessionDialogState::new(Vec::new(), SessionId::new(), None);
        assert_eq!(dialog.sessions.len(), 1);
        assert_eq!(dialog.sessions[0].id, dialog.active_session);

        assert_eq!(
            dialog.handle_key(Key::Enter),
            Some(SessionAction::Resume(dialog.active_session))
        );
        assert_eq!(dialog.handle_key(Key::Char('d')), None);
        assert_eq!(dialog.mode, SessionDialogMode::ConfirmDelete);
        assert_eq!(dialog.handle_key(Key::Interrupt), None);
        assert_eq!(dialog.handle_key(Key::Char('r')), None);
        assert_eq!(dialog.mode, SessionDialogMode::Rename);
        assert_eq!(dialog.handle_key(Key::Interrupt), None);
        assert_eq!(dialog.handle_key(Key::Up), None);
        assert_eq!(dialog.selected, 0);
    }

    #[test]
    fn a_row_is_named_by_its_number_label_uuid_or_title() {
        let first = session(Some("feature work"));
        let second = session(None);
        let (first_id, second_id) = (first.id, second.id);
        let sessions = vec![first, second];
        let current = SessionId::new();
        let uuid = first_id.to_string();

        assert_eq!(
            resolve_session_answer("1", &sessions, current).unwrap(),
            first_id
        );
        assert_eq!(
            resolve_session_answer("2", &sessions, current).unwrap(),
            second_id
        );

        // The whole label, which is what Enter on a row restores into the
        // line, taken from the picker itself so the two cannot drift.
        let (rows, _) = session_rows(&sessions, None);
        let label = rows.unwrap()[0].0.clone();
        assert_eq!(label, format!("feature work · {}", &uuid[..8]));
        assert_eq!(
            resolve_session_answer(&label, &sessions, current).unwrap(),
            first_id
        );

        // An untitled row says so, with its id behind, and still resolves.
        let untitled = session_rows(&sessions, None).0.unwrap()[1].0.clone();
        assert_eq!(
            untitled,
            format!("(untitled) · {}", &second_id.to_string()[..8])
        );
        assert_eq!(
            resolve_session_answer(&untitled, &sessions, current).unwrap(),
            second_id
        );

        // The UUID behind a titled row's label, and a prefix of one.
        assert_eq!(
            resolve_session_answer(&uuid, &sessions, current).unwrap(),
            first_id
        );
        assert_eq!(
            resolve_session_answer(&uuid[..8], &sessions, current).unwrap(),
            first_id
        );

        // The title, so the name they chose works as well as the ID.
        assert_eq!(
            resolve_session_answer("feat", &sessions, current).unwrap(),
            first_id
        );

        // An untitled row is its UUID, as it always was.
        assert_eq!(
            resolve_session_answer(&second_id.to_string(), &sessions, current).unwrap(),
            second_id
        );
    }

    #[test]
    fn a_uuid_prefix_of_only_digits_is_still_a_uuid_prefix() {
        // Roughly one UUID in forty starts with eight decimal digits, and
        // such a prefix parses as a row number far outside the list. It used
        // to be refused as one, which made the picker reject an answer that
        // names exactly one session — and made the test above fail about
        // that often.
        let digits = SessionId::from_uuid("89501541-0000-4000-8000-000000000000".parse().unwrap());
        let sessions = vec![SessionChoice {
            id: digits,
            title: None,
            events: 1,
            last_seen: "now".to_owned(),
        }];
        let current = SessionId::new();
        assert_eq!(
            resolve_session_answer("89501541", &sessions, current).unwrap(),
            digits
        );
        // A short number is a row number, not a prefix, even where the one
        // session's UUID starts with that digit: typing `8` in a one-row
        // list must not silently resume something.
        assert_eq!(
            resolve_session_answer("8", &sessions, current).unwrap_err(),
            "no session 8; choose 1-1"
        );
        // And a long one that matches nothing is still reported as a row.
        assert_eq!(
            resolve_session_answer("99999999", &sessions, current).unwrap_err(),
            "no session 99999999; choose 1-1"
        );
    }

    #[test]
    fn an_answer_that_names_nothing_is_rejected() {
        let sessions = vec![session(Some("feature work"))];
        let current = SessionId::new();

        assert_eq!(
            resolve_session_answer("nothing like this", &sessions, current).unwrap_err(),
            "`nothing like this` is not a valid session ID"
        );
        assert_eq!(
            resolve_session_answer("4", &sessions, current).unwrap_err(),
            "no session 4; choose 1-1"
        );
        assert_eq!(
            resolve_session_answer("1", &[], current).unwrap_err(),
            "no sessions found"
        );
        assert_eq!(
            resolve_session_answer("  ", &sessions, current).unwrap(),
            current,
            "an empty line keeps the session it is already in"
        );
    }
}
