//! The interactive `/settings` dialog: move through the settings the registry
//! names, edit one with the arrow keys, or put one back to its default.
//!
//! The registry lives in the kernel (`config::SETTINGS`), so what this dialog
//! offers and what the loader will accept when it next reads the file are the
//! same table. Editing never asks the operator to type: the arrows move the
//! pending value through the values the kind allows, and Enter writes the one
//! the row shows. A value this dialog offers is therefore always one the
//! loader takes.
//!
//! Writes go to the operator's own `arsy.json` or, after Tab, to the
//! project's `.arsy/arsy.json`. Either way a higher layer still outranks what
//! is set here, and each row says which layer its value came from.
use super::*;

/// The value shape of one setting, as this dialog edits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingKind {
    /// Any non-empty text. Nothing to cycle, so this dialog does not edit it;
    /// the row says where it can be set instead.
    Text,
    /// `true` or `false`.
    Bool,
    /// One of the row's `choices`.
    Choice,
    /// A whole number between the two bounds, inclusive.
    Integer { min: usize, max: usize },
}

/// One row of the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingRow {
    /// The group the dialog lists it under.
    pub section: String,
    /// What the dialog calls it.
    pub label: String,
    /// When a change takes effect: `now`, `next turn`, or `restart`.
    pub applies: String,
    /// The dotted key, as it is written in `arsy.json`.
    pub key: String,
    /// The value in effect: what a layer set, or the built-in default.
    pub value: String,
    pub default: String,
    pub description: String,
    /// The values a `Choice` accepts; empty for every other kind.
    pub choices: Vec<String>,
    /// How this dialog edits the row.
    pub kind: SettingKind,
    /// Whether a layer set this key, as opposed to it standing at default.
    pub set: bool,
    /// The layer that set it — `user`, `project`, `enterprise` — when `set`.
    pub origin: String,
}

/// Which file an edit is written to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsScope {
    /// `~/.arsy/arsy.json`: every workspace this operator opens.
    #[default]
    User,
    /// `<workspace>/.arsy/arsy.json`: this project, for everyone who opens it.
    Project,
}

impl SettingsScope {
    pub const fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }

    const fn other(self) -> Self {
        match self {
            Self::User => Self::Project,
            Self::Project => Self::User,
        }
    }
}

/// Which column the arrows move in, when the dialog is wide enough to show
/// sections as a column of their own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsPane {
    Sections,
    #[default]
    Settings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsAction {
    /// Set the row at this index to the pending value the arrows chose.
    Apply(usize, String),
    /// Remove the row's key, so it stands at its built-in default.
    Reset(usize),
    Close,
}

/// A value being chosen for one row, inside the dialog rather than typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Editing {
    /// The row the pending value belongs to.
    pub index: usize,
    /// The value Enter would set.
    pub pending: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsDialogState {
    pub rows: Vec<SettingRow>,
    pub selected: usize,
    /// The row being edited, and the value Enter would set for it.
    pub editing: Option<Editing>,
    /// What the last action did, shown inside the frame rather than printed
    /// under it.
    pub notice: Option<String>,
    /// The file Enter and `r` write to. Tab switches it.
    pub scope: SettingsScope,
    /// The column ↑/↓ move in.
    pub pane: SettingsPane,
}

impl SettingsDialogState {
    pub fn new(rows: Vec<SettingRow>) -> Self {
        Self {
            rows,
            selected: 0,
            editing: None,
            notice: None,
            scope: SettingsScope::User,
            pane: SettingsPane::Settings,
        }
    }

    /// The sections the rows fall into, in the order the rows list them.
    fn sections(&self) -> Vec<&str> {
        let mut sections: Vec<&str> = Vec::new();
        for row in &self.rows {
            if !sections.contains(&row.section.as_str()) {
                sections.push(&row.section);
            }
        }
        sections
    }

    /// The section the marked row belongs to.
    fn current_section(&self) -> &str {
        self.rows
            .get(self.selected)
            .map_or("", |row| row.section.as_str())
    }

    /// The indexes of the rows in `section`, in order.
    fn rows_in(&self, section: &str) -> Vec<usize> {
        (0..self.rows.len())
            .filter(|&index| self.rows[index].section == section)
            .collect()
    }

    /// Replace the rows after a change, keeping the marker on the same key.
    ///
    /// The change is written, so the value being chosen is settled either way:
    /// the edit ends and the marker stands on the key in the fresh rows.
    pub fn reload(&mut self, rows: Vec<SettingRow>) {
        let key = self.rows.get(self.selected).map(|row| row.key.clone());
        self.selected = key
            .and_then(|key| rows.iter().position(|row| row.key == key))
            .unwrap_or(0);
        self.rows = rows;
        self.editing = None;
    }

    pub fn render(&self, width: usize, colour: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let inner = width.saturating_sub(4);
        let title = format!(" SETTINGS · writes to {} ", self.scope.label());
        let mut lines = vec![dialog_top(&title, width, colour)];
        if self.rows.is_empty() {
            lines.push(dialog_line(
                "  no setting can be written by this build",
                inner,
                colour,
                sgr_dim(),
            ));
        } else if inner < 50 {
            lines.extend(self.single_pane_lines(inner, colour));
        } else if inner < THREE_COLUMNS {
            lines.extend(self.multi_pane_lines(inner, colour));
        } else {
            lines.extend(self.three_pane_lines(inner, colour));
        }
        lines.push(dialog_line("", inner, colour, ""));
        if let Some(notice) = &self.notice {
            lines.push(dialog_line(notice, inner, colour, sgr_dim()));
        }
        let footers: &[&str] = if self.editing.is_some() {
            &["[←/→/↑/↓] Change  [Enter] Set  [r] Reset  [Esc] Close"]
        } else if inner >= THREE_COLUMNS {
            &[
                "[←/→] Column  [↑/↓] Select  [Space] Toggle  [Enter] Edit",
                "[r] Reset  [Tab] User/Project  [Esc] Close",
            ]
        } else {
            &[
                "[↑/↓] Select  [Space] Toggle  [Enter] Edit  [r] Reset",
                "[Tab] User/Project  [Esc] Close",
            ]
        };
        for footer in footers {
            lines.push(dialog_line(footer, inner, colour, sgr_dim()));
        }
        lines.push(paint(
            colour,
            sgr_border(),
            &format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        ));
        lines.join("\n")
    }

    fn single_pane_lines(&self, inner: usize, colour: bool) -> Vec<String> {
        let key_width = self
            .rows
            .iter()
            .map(|row| visible_len(&row.label))
            .max()
            .unwrap_or(0);
        let mut lines: Vec<String> = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let marked = index == self.selected;
                let pending = self
                    .editing
                    .as_ref()
                    .filter(|edit| edit.index == index)
                    .map(|edit| edit.pending.as_str());
                let style = match (marked, row.set) {
                    (true, _) => sgr_accent(),
                    (false, true) => "",
                    _ => sgr_dim(),
                };
                dialog_line(
                    &setting_row(row, marked, key_width, pending),
                    inner,
                    colour,
                    style,
                )
            })
            .collect();
        if let Some(hint) = self.choices_hint() {
            lines.push(dialog_line(&hint, inner, colour, sgr_dim()));
        }
        lines
    }

    fn multi_pane_lines(&self, inner: usize, colour: bool) -> Vec<String> {
        let left_width = (self.label_width() + self.value_width() + 4)
            .max((inner * 35 / 100).clamp(24, 32))
            .min(inner.saturating_sub(24));
        let right_width = inner.saturating_sub(left_width + 3);
        let divider = paint(colour, sgr_border(), " │ ");

        let mut lines = Vec::new();

        let left_hdr = paint(colour, sgr_dim(), "SETTING");
        let right_hdr = paint(colour, sgr_dim(), "DETAIL & CONFIG");
        lines.push(border_line(
            &format!(
                "{}{divider}{}",
                cell(&left_hdr, left_width),
                cell(&right_hdr, right_width)
            ),
            inner,
            colour,
        ));

        let sep_line = format!("{}─┼─{}", "─".repeat(left_width), "─".repeat(right_width));
        lines.push(border_line(
            &paint(colour, sgr_border(), &sep_line),
            inner,
            colour,
        ));

        // Every section, each under its heading. A heading is drawn, never
        // selected: the marker only ever stands on a setting.
        // Rows arrive grouped by section, so a heading goes in wherever the
        // section changes.
        let mut left_lines = Vec::new();
        let mut heading: Option<&str> = None;
        for (index, row) in self.rows.iter().enumerate() {
            if heading != Some(row.section.as_str()) {
                heading = Some(&row.section);
                left_lines.push(paint(colour, sgr_dim(), &row.section.to_uppercase()));
            }
            left_lines.push(self.setting_cell(index, colour));
        }

        let right_lines = self.right_pane_lines(right_width, colour);
        let max_lines = left_lines.len().max(right_lines.len());
        for i in 0..max_lines {
            let left_cell = cell(
                left_lines.get(i).map(String::as_str).unwrap_or(""),
                left_width,
            );
            let right_cell = cell(
                right_lines.get(i).map(String::as_str).unwrap_or(""),
                right_width,
            );
            lines.push(border_line(
                &format!("{left_cell}{divider}{right_cell}"),
                inner,
                colour,
            ));
        }
        lines
    }

    fn right_pane_lines(&self, right_width: usize, colour: bool) -> Vec<String> {
        let Some(row) = self.rows.get(self.selected) else {
            return Vec::new();
        };
        let pending = self
            .editing
            .as_ref()
            .filter(|edit| edit.index == self.selected)
            .map(|edit| edit.pending.as_str());
        let mut lines = Vec::new();

        let origin_label = origin_label(row);
        let origin_badge = if row.set {
            paint(colour, sgr_ok(), &format!("[{origin_label}]"))
        } else {
            paint(colour, sgr_dim(), &format!("[{origin_label}]"))
        };
        lines.push(format!(
            "{}  {origin_badge}",
            paint(colour, BOLD, &row.label)
        ));
        lines.push(paint(
            colour,
            sgr_border(),
            &"─".repeat(right_width.min(visible_len(&row.label) + 12)),
        ));

        for line in wrap_words(&row.description, right_width) {
            lines.push(paint(colour, sgr_assistant(), &line));
        }
        lines.push(String::new());

        let val_str = match pending {
            Some(p) if p != row.value => {
                format!("{} → {}", shown_value(row, &row.value), shown_value(row, p))
            }
            _ => shown_value(row, &row.value),
        };
        let val_styled = if pending.is_some() {
            paint(colour, sgr_accent(), &val_str)
        } else {
            val_str
        };
        lines.push(format!("value:   {val_styled}"));
        lines.push(format!(
            "default: {}",
            paint(colour, sgr_dim(), &row.default)
        ));

        if let Some(hint_text) = row_allowed_hint(row) {
            lines.push(paint(colour, sgr_dim(), &format!("allowed: {hint_text}")));
        } else if row.kind == SettingKind::Text {
            lines.push(paint(colour, sgr_dim(), "free text; set it in arsy.json"));
        }

        lines.push(paint(colour, sgr_dim(), &format!("key:     {}", row.key)));
        lines.push(paint(
            colour,
            sgr_dim(),
            &format!("applies: {}", row.applies),
        ));

        if let Some(p) = pending {
            lines.push(String::new());
            lines.extend(edit_control_lines(row, p, colour));
        }
        lines
    }

    /// Sections | settings in the marked section | detail.
    fn three_pane_lines(&self, inner: usize, colour: bool) -> Vec<String> {
        let section_width = self
            .sections()
            .iter()
            .map(|section| visible_len(section))
            .max()
            .unwrap_or(0)
            + 2;
        let setting_width = self.label_width() + self.value_width() + 4;
        let detail_width = inner.saturating_sub(section_width + setting_width + 6);
        let divider = paint(colour, sgr_border(), " │ ");
        let header = |text: &str, pane: Option<SettingsPane>| {
            if pane.is_some_and(|pane| pane == self.pane) {
                paint(colour, sgr_accent(), text)
            } else {
                paint(colour, sgr_dim(), text)
            }
        };

        let mut lines = vec![border_line(
            &format!(
                "{}{divider}{}{divider}{}",
                cell(
                    &header("SECTION", Some(SettingsPane::Sections)),
                    section_width
                ),
                cell(
                    &header("SETTING", Some(SettingsPane::Settings)),
                    setting_width
                ),
                cell(&header("DETAIL", None), detail_width),
            ),
            inner,
            colour,
        )];
        lines.push(border_line(
            &paint(
                colour,
                sgr_border(),
                &format!(
                    "{}─┼─{}─┼─{}",
                    "─".repeat(section_width),
                    "─".repeat(setting_width),
                    "─".repeat(detail_width)
                ),
            ),
            inner,
            colour,
        ));

        let current = self.current_section().to_owned();
        let section_lines: Vec<String> = self
            .sections()
            .iter()
            .map(|section| {
                let marked = *section == current;
                let line = format!("{}{section}", if marked { "› " } else { "  " });
                match (marked, self.pane) {
                    (true, SettingsPane::Sections) => paint(colour, sgr_accent(), &line),
                    (true, SettingsPane::Settings) => paint(colour, BOLD, &line),
                    _ => paint(colour, sgr_dim(), &line),
                }
            })
            .collect();
        let setting_lines: Vec<String> = self
            .rows_in(&current)
            .into_iter()
            .map(|index| self.setting_cell(index, colour))
            .collect();
        let detail_lines = self.right_pane_lines(detail_width, colour);

        let height = section_lines
            .len()
            .max(setting_lines.len())
            .max(detail_lines.len());
        for i in 0..height {
            let at = |column: &[String], width| {
                cell(column.get(i).map(String::as_str).unwrap_or(""), width)
            };
            lines.push(border_line(
                &format!(
                    "{}{divider}{}{divider}{}",
                    at(&section_lines, section_width),
                    at(&setting_lines, setting_width),
                    at(&detail_lines, detail_width),
                ),
                inner,
                colour,
            ));
        }
        lines
    }

    /// One setting as its column shows it: marker, label, and value.
    fn setting_cell(&self, index: usize, colour: bool) -> String {
        let row = &self.rows[index];
        let marked = index == self.selected;
        let label = format!(
            "{}{:<width$}  ",
            if marked { "› " } else { "  " },
            row.label,
            width = self.label_width()
        );
        let value = shown_value(row, &row.value);
        let value = match (row.kind, row.value.as_str()) {
            (SettingKind::Bool, "true") => paint(colour, sgr_ok(), &value),
            _ => value,
        };
        match (marked, row.set) {
            (true, _) => format!("{}{value}", paint(colour, sgr_accent(), &label)),
            (false, true) => format!("{label}{value}"),
            _ => format!("{}{value}", paint(colour, sgr_dim(), &label)),
        }
    }

    fn label_width(&self) -> usize {
        self.rows
            .iter()
            .map(|row| visible_len(&row.label))
            .max()
            .unwrap_or(0)
    }

    fn value_width(&self) -> usize {
        self.rows
            .iter()
            .map(|row| visible_len(&shown_value(row, &row.value)))
            .max()
            .unwrap_or(0)
    }

    pub fn handle_key(&mut self, key: Key) -> Option<SettingsAction> {
        if self.editing.is_some() {
            return self.handle_edit_key(key);
        }
        match key {
            Key::Up | Key::Down if self.pane == SettingsPane::Sections => {
                self.step_section(key == Key::Down);
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
            Key::Left => {
                self.pane = SettingsPane::Sections;
                None
            }
            Key::Right => {
                self.pane = SettingsPane::Settings;
                None
            }
            Key::Enter | Key::Newline if self.pane == SettingsPane::Sections => {
                self.pane = SettingsPane::Settings;
                None
            }
            // A switch flips where it stands: two values have nothing to
            // choose between, so Space writes the other one straight away.
            Key::Char(' ') => {
                let row = self.rows.get(self.selected)?;
                (row.kind == SettingKind::Bool).then(|| {
                    let flipped = if row.value == "true" { "false" } else { "true" };
                    SettingsAction::Apply(self.selected, flipped.to_owned())
                })
            }
            Key::Char('e' | 'E') | Key::Enter | Key::Newline => self.begin_edit(),
            Key::Char('r' | 'R') => self
                .rows
                .get(self.selected)
                .is_some()
                .then_some(SettingsAction::Reset(self.selected)),
            Key::Tab => {
                self.scope = self.scope.other();
                self.notice = None;
                None
            }
            Key::Interrupt | Key::Eof => Some(SettingsAction::Close),
            _ => None,
        }
    }

    /// One keystroke while a value is being chosen. The arrows move the
    /// pending value through what the kind allows, Enter sets it, and escape
    /// backs out leaving the row as it was.
    fn handle_edit_key(&mut self, key: Key) -> Option<SettingsAction> {
        let index = self.editing.as_ref()?.index;
        let kind = self.rows.get(index)?.kind;
        match (key, kind) {
            (Key::Left | Key::Right, SettingKind::Choice) => {
                self.cycle_pending(index, key == Key::Right);
                None
            }
            (Key::Left | Key::Right, SettingKind::Bool) => {
                self.flip_pending(index);
                None
            }
            (Key::Up | Key::Down | Key::Left | Key::Right, SettingKind::Integer { min, max }) => {
                self.step_pending(index, min, max, matches!(key, Key::Up | Key::Right));
                None
            }
            (Key::Enter | Key::Newline, _) => {
                let pending = self.editing.as_ref()?.pending.clone();
                Some(SettingsAction::Apply(index, pending))
            }
            (Key::Char('r' | 'R'), _) => Some(SettingsAction::Reset(index)),
            // Escape backs out of the edit; a hung-up keyboard closes the
            // dialog rather than holding the session on it.
            (Key::Interrupt, _) => {
                self.editing = None;
                None
            }
            (Key::Eof, _) => Some(SettingsAction::Close),
            _ => None,
        }
    }

    /// Start choosing a value for the marked row. A `Text` row has nothing to
    /// cycle, so the dialog says where it can be set instead of pretending.
    fn begin_edit(&mut self) -> Option<SettingsAction> {
        let row = self.rows.get(self.selected)?;
        match row.kind {
            SettingKind::Text => {
                self.notice = Some(format!("`{}` is free text; set it in arsy.json", row.key));
                None
            }
            _ => {
                self.editing = Some(Editing {
                    index: self.selected,
                    pending: row.value.clone(),
                });
                None
            }
        }
    }

    /// Move a `Choice` row's pending value one option on, wrapping at either
    /// end.
    fn cycle_pending(&mut self, index: usize, forward: bool) {
        let Some(edit) = self.editing.as_mut() else {
            return;
        };
        let choices = &self.rows[index].choices;
        if choices.is_empty() {
            return;
        }
        let at = choices
            .iter()
            .position(|choice| *choice == edit.pending)
            .unwrap_or(0);
        edit.pending = if forward {
            choices[(at + 1) % choices.len()].clone()
        } else {
            choices[(at + choices.len() - 1) % choices.len()].clone()
        };
    }

    /// Move a `Bool` row's pending value to the other side; the wrap of a
    /// two-value cycle is the same step either way.
    fn flip_pending(&mut self, index: usize) {
        let Some(edit) = self.editing.as_mut() else {
            return;
        };
        let _ = index;
        edit.pending = if edit.pending == "true" {
            "false".to_owned()
        } else {
            "true".to_owned()
        };
    }

    /// Step an `Integer` row's pending value by one, held inside its bounds.
    fn step_pending(&mut self, index: usize, min: usize, max: usize, up: bool) {
        let Some(edit) = self.editing.as_mut() else {
            return;
        };
        let _ = index;
        let current = edit.pending.parse::<usize>().unwrap_or(min);
        edit.pending = if up {
            current.saturating_add(1).min(max)
        } else {
            current.saturating_sub(1).max(min)
        }
        .to_string();
    }

    /// The values the row being edited accepts, when there is a fixed set to
    /// name.
    fn choices_hint(&self) -> Option<String> {
        let edit = self.editing.as_ref()?;
        let row = self.rows.get(edit.index)?;
        let hint = match row.kind {
            SettingKind::Choice if !row.choices.is_empty() => {
                format!("one of {}", row.choices.join(" | "))
            }
            SettingKind::Bool => "true or false".to_owned(),
            SettingKind::Integer { min, max } => format!("from {min} to {max}"),
            SettingKind::Choice | SettingKind::Text => return None,
        };
        Some(format!("  {hint}"))
    }

    /// Move the marker one setting, wrapping at either end. Rows are grouped
    /// by section, so walking past a section's last setting lands on the
    /// next section's first, and the section column follows.
    fn step(&mut self, forward: bool) {
        let count = self.rows.len();
        if count == 0 {
            return;
        }
        self.selected = if forward {
            (self.selected + 1) % count
        } else {
            (self.selected + count - 1) % count
        };
    }

    /// Move to the next or previous section, onto its first setting.
    fn step_section(&mut self, forward: bool) {
        let sections: Vec<String> = self.sections().iter().map(|s| (*s).to_owned()).collect();
        let count = sections.len();
        let Some(at) = sections
            .iter()
            .position(|section| section == self.current_section())
        else {
            return;
        };
        let next = &sections[if forward {
            (at + 1) % count
        } else {
            (at + count - 1) % count
        }];
        if let Some(&first) = self.rows_in(next).first() {
            self.selected = first;
        }
    }
}

/// Where a row's value came from, as the dialog names it.
fn origin_label(row: &SettingRow) -> &str {
    if row.set && !row.origin.is_empty() {
        &row.origin
    } else if row.set {
        "set"
    } else {
        "default"
    }
}

/// One setting as a row: marker, key, value, and where it came from. While the
/// row is being edited, the value shown is the one Enter would set.
fn setting_row(row: &SettingRow, marked: bool, key_width: usize, pending: Option<&str>) -> String {
    let key = format!("{:<key_width$}", row.label);
    let origin = origin_label(row);
    let value = match pending {
        Some(pending) if pending != row.value => {
            format!(
                "{} → {}",
                shown_value(row, &row.value),
                shown_value(row, pending)
            )
        }
        _ => shown_value(row, &row.value),
    };
    format!(
        "{} {key}  {value}  {origin:<7}  {}",
        if marked { "›" } else { " " },
        row.description,
    )
}

/// The width from which sections get a column of their own.
const THREE_COLUMNS: usize = 86;

/// A value as the dialog shows it: a switch as on or off rather than the
/// `true` or `false` written in the file.
fn shown_value(row: &SettingRow, value: &str) -> String {
    match (row.kind, value) {
        (SettingKind::Bool, "true") => "● on".to_owned(),
        (SettingKind::Bool, "false") => "○ off".to_owned(),
        _ => value.to_owned(),
    }
}

fn cell(text: &str, width: usize) -> String {
    let fitted = fit(text, width);
    let pad = " ".repeat(width.saturating_sub(visible_len(&fitted)));
    format!("{fitted}{pad}")
}

fn border_line(content: &str, inner: usize, colour: bool) -> String {
    let body = cell(content, inner);
    let border = paint(colour, sgr_border(), "│");
    format!("{border} {body} {border}")
}

fn row_allowed_hint(row: &SettingRow) -> Option<String> {
    match row.kind {
        SettingKind::Choice if !row.choices.is_empty() => {
            Some(format!("one of {}", row.choices.join(" | ")))
        }
        SettingKind::Choice => None,
        SettingKind::Bool => Some("true or false".to_owned()),
        SettingKind::Integer { min, max } => Some(format!("from {min} to {max}")),
        SettingKind::Text => None,
    }
}

fn edit_control_lines(row: &SettingRow, pending: &str, colour: bool) -> Vec<String> {
    let mut lines = Vec::new();
    match row.kind {
        SettingKind::Choice => {
            let pills: Vec<String> = row
                .choices
                .iter()
                .map(|c| {
                    if c == pending {
                        if colour {
                            format!("{}[• {c}]{RESET}", sgr_accent())
                        } else {
                            format!("[• {c}]")
                        }
                    } else {
                        paint(colour, sgr_dim(), &format!("  {c}  "))
                    }
                })
                .collect();
            lines.push(format!("select:  {}", pills.join(" ")));
        }
        SettingKind::Bool => {
            let true_pill = if pending == "true" {
                if colour {
                    format!("{}[• true]{RESET}", sgr_accent())
                } else {
                    "[• true]".to_owned()
                }
            } else {
                paint(colour, sgr_dim(), "  true  ")
            };
            let false_pill = if pending == "false" {
                if colour {
                    format!("{}[• false]{RESET}", sgr_accent())
                } else {
                    "[• false]".to_owned()
                }
            } else {
                paint(colour, sgr_dim(), "  false  ")
            };
            lines.push(format!("toggle:  {true_pill}  {false_pill}"));
        }
        SettingKind::Integer { min, max } => {
            lines.push(format!("step:    ◄ {pending} ►  ({min}..{max})"));
        }
        SettingKind::Text => {}
    }
    lines
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![text.to_owned()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if visible_len(&current) + 1 + visible_len(word) <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(current);
            current = word.to_owned();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice_row(key: &str, value: &str, set: bool) -> SettingRow {
        SettingRow {
            section: "Appearance".to_owned(),
            label: key.to_owned(),
            applies: "now".to_owned(),
            key: key.to_owned(),
            value: value.to_owned(),
            default: "modern".to_owned(),
            description: "how an interactive transcript is drawn".to_owned(),
            choices: vec!["modern".to_owned(), "classic".to_owned()],
            kind: SettingKind::Choice,
            set,
            origin: if set {
                "user".to_owned()
            } else {
                String::new()
            },
        }
    }

    fn bool_row(value: &str) -> SettingRow {
        SettingRow {
            section: "Compatibility".to_owned(),
            label: "OMP".to_owned(),
            applies: "next turn".to_owned(),
            key: "compat.omp.enabled".to_owned(),
            value: value.to_owned(),
            default: "true".to_owned(),
            description: "read OMP's files as a lower layer".to_owned(),
            choices: Vec::new(),
            kind: SettingKind::Bool,
            set: false,
            origin: String::new(),
        }
    }

    fn integer_row(value: &str) -> SettingRow {
        SettingRow {
            section: "Execution".to_owned(),
            label: "Parallel tools".to_owned(),
            applies: "restart".to_owned(),
            key: "execution.max_parallel".to_owned(),
            value: value.to_owned(),
            default: "8".to_owned(),
            description: "how many tools may run at once".to_owned(),
            choices: Vec::new(),
            kind: SettingKind::Integer { min: 1, max: 3 },
            set: false,
            origin: String::new(),
        }
    }

    #[test]
    fn enter_begins_editing_r_resets_and_esc_closes() {
        let mut dialog = SettingsDialogState::new(vec![
            choice_row("ui.style", "modern", true),
            choice_row("credentials.store", "file", false),
        ]);
        assert_eq!(dialog.handle_key(Key::Enter), None);
        assert_eq!(
            dialog.editing,
            Some(Editing {
                index: 0,
                pending: "modern".to_owned()
            })
        );
        dialog.editing = None;
        assert_eq!(
            dialog.handle_key(Key::Char('r')),
            Some(SettingsAction::Reset(0))
        );
        assert_eq!(dialog.handle_key(Key::Down), None);
        assert_eq!(dialog.selected, 1);
        assert_eq!(dialog.handle_key(Key::Up), None);
        assert_eq!(dialog.selected, 0);
        assert_eq!(
            dialog.handle_key(Key::Interrupt),
            Some(SettingsAction::Close)
        );
    }

    #[test]
    fn the_rows_name_the_key_the_value_and_whether_a_layer_set_it() {
        let dialog = SettingsDialogState::new(vec![choice_row("ui.style", "classic", true)]);
        let frame = dialog.render(80, false);
        assert!(frame.contains(" SETTINGS "), "{frame}");
        assert!(frame.contains("ui.style"), "{frame}");
        assert!(frame.contains("classic"), "{frame}");
        assert!(frame.contains("set"), "{frame}");
        let empty = SettingsDialogState::new(Vec::new()).render(80, false);
        assert!(empty.contains("no setting can be written"), "{empty}");
    }

    #[test]
    fn the_arrows_cycle_a_choice_with_wrap() {
        let mut dialog = SettingsDialogState::new(vec![choice_row("ui.style", "modern", true)]);
        dialog.handle_key(Key::Enter);
        dialog.handle_key(Key::Right);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "classic");
        dialog.handle_key(Key::Right);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "modern");
        dialog.handle_key(Key::Left);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "classic");
        // While a value is being chosen, the row shows the one Enter would
        // set, and the footer names the arrows.
        let frame = dialog.render(80, false);
        assert!(frame.contains("modern → classic"), "{frame}");
        assert!(frame.contains("[←/→/↑/↓] Change"), "{frame}");
        assert!(frame.contains("one of modern | classic"), "{frame}");
    }

    #[test]
    fn a_bool_flips_between_true_and_false() {
        let mut dialog = SettingsDialogState::new(vec![bool_row("true")]);
        dialog.handle_key(Key::Enter);
        dialog.handle_key(Key::Right);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "false");
        dialog.handle_key(Key::Left);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "true");
        let frame = dialog.render(80, false);
        assert!(frame.contains("true or false"), "{frame}");
    }

    #[test]
    fn an_integer_steps_within_its_bounds() {
        let mut dialog = SettingsDialogState::new(vec![integer_row("2")]);
        dialog.handle_key(Key::Enter);
        dialog.handle_key(Key::Right);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "3");
        // Held at the maximum.
        dialog.handle_key(Key::Up);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "3");
        dialog.handle_key(Key::Left);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "2");
        dialog.handle_key(Key::Down);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "1");
        // Held at the minimum.
        dialog.handle_key(Key::Left);
        assert_eq!(dialog.editing.as_ref().unwrap().pending, "1");
        let frame = dialog.render(80, false);
        assert!(frame.contains("from 1 to 3"), "{frame}");
    }

    #[test]
    fn enter_applies_the_pending_value() {
        let mut dialog = SettingsDialogState::new(vec![choice_row("ui.style", "modern", true)]);
        dialog.handle_key(Key::Enter);
        dialog.handle_key(Key::Right);
        assert_eq!(
            dialog.handle_key(Key::Enter),
            Some(SettingsAction::Apply(0, "classic".to_owned()))
        );
    }

    #[test]
    fn esc_backs_out_of_an_edit_leaving_the_row_as_it_was() {
        let mut dialog = SettingsDialogState::new(vec![choice_row("ui.style", "modern", true)]);
        dialog.handle_key(Key::Enter);
        dialog.handle_key(Key::Right);
        assert_eq!(dialog.handle_key(Key::Interrupt), None);
        assert_eq!(dialog.editing, None);
        // The marker navigates again, and the row kept its written value:
        // what the arrows moved was only the pending one.
        assert_eq!(dialog.handle_key(Key::Down), None);
        assert_eq!(dialog.selected, 0);
        let frame = dialog.render(80, false);
        assert!(frame.contains("modern"), "{frame}");
        assert!(!frame.contains("→"), "{frame}");
        assert!(frame.contains("[↑/↓] Select"), "{frame}");
    }

    /// Tab chooses the file an edit lands in, and the frame says which; each
    /// row says which layer its value came from.
    #[test]
    fn tab_switches_the_file_edits_are_written_to() {
        let mut row = choice_row("ui.style", "classic", true);
        row.origin = "project".to_owned();
        let mut dialog = SettingsDialogState::new(vec![row]);
        assert_eq!(dialog.scope, SettingsScope::User);
        assert!(dialog.render(100, false).contains("writes to user"));
        assert!(dialog.render(100, false).contains("[project]"));

        assert_eq!(dialog.handle_key(Key::Tab), None);
        assert_eq!(dialog.scope, SettingsScope::Project);
        assert!(dialog.render(100, false).contains("writes to project"));

        dialog.handle_key(Key::Tab);
        assert_eq!(dialog.scope, SettingsScope::User);
    }

    #[test]
    fn a_text_row_is_not_edited_here() {
        let mut dialog = SettingsDialogState::new(vec![SettingRow {
            section: "Appearance".to_owned(),
            label: "model.default".to_owned(),
            applies: "restart".to_owned(),
            key: "model.default".to_owned(),
            value: "m".to_owned(),
            default: String::new(),
            description: "which model a turn uses".to_owned(),
            choices: Vec::new(),
            kind: SettingKind::Text,
            set: false,
            origin: String::new(),
        }]);
        assert_eq!(dialog.handle_key(Key::Enter), None);
        assert_eq!(dialog.editing, None);
        let notice = dialog.notice.as_ref().expect("a notice");
        assert!(notice.contains("arsy.json"), "{notice}");
        let frame = dialog.render(80, false);
        assert!(frame.contains("free text; set it in arsy.json"), "{frame}");
        assert!(!frame.contains("allowed:"), "{frame}");
    }

    #[test]
    fn a_reload_ends_the_edit_and_keeps_the_marker_on_the_same_key() {
        let mut dialog = SettingsDialogState::new(vec![
            choice_row("ui.style", "modern", true),
            choice_row("credentials.store", "file", false),
        ]);
        dialog.selected = 1;
        dialog.handle_key(Key::Enter);
        dialog.reload(vec![choice_row("credentials.store", "classic", true)]);
        assert_eq!(dialog.editing, None);
        assert_eq!(dialog.selected, 0);
        assert!(dialog.render(80, false).contains("set"));
    }

    #[test]
    fn multi_pane_layout_renders_split_columns_and_updates_detail() {
        let mut dialog = SettingsDialogState::new(vec![
            choice_row("ui.style", "modern", true),
            bool_row("false"),
        ]);
        let frame = dialog.render(80, false);
        assert!(frame.contains("SETTING"), "{frame}");
        assert!(frame.contains("DETAIL & CONFIG"), "{frame}");
        assert!(frame.contains(" │ "), "{frame}");
        assert!(frame.contains("─┼─"), "{frame}");
        assert!(frame.contains("ui.style"), "{frame}");
        assert!(
            frame.contains("how an interactive transcript is drawn"),
            "{frame}"
        );
        assert!(frame.contains("value:   modern"), "{frame}");

        dialog.handle_key(Key::Down);
        assert_eq!(dialog.selected, 1);
        let frame2 = dialog.render(80, false);
        assert!(frame2.contains("compat.omp.enabled"), "{frame2}");
        assert!(
            frame2.contains("read OMP's files as a lower layer"),
            "{frame2}"
        );
        assert!(frame2.contains("true or false"), "{frame2}");
    }

    /// Wide enough, sections get a column: ←/→ move between columns, ↑/↓ in
    /// the section column jump to the next section's first setting, and the
    /// detail says the key and when a change applies.
    #[test]
    fn a_wide_dialog_has_a_section_column() {
        let mut dialog = SettingsDialogState::new(vec![
            choice_row("ui.style", "modern", true),
            integer_row("2"),
            bool_row("true"),
        ]);
        let frame = dialog.render(120, false);
        for expected in ["SECTION", "SETTING", "DETAIL", "› Appearance", "Execution"] {
            assert!(frame.contains(expected), "{expected}: {frame}");
        }
        assert!(frame.contains("key:     ui.style"), "{frame}");
        assert!(frame.contains("applies: now"), "{frame}");
        assert!(
            !frame.contains("Parallel tools"),
            "only the marked section's settings: {frame}"
        );

        dialog.handle_key(Key::Left);
        assert_eq!(dialog.pane, SettingsPane::Sections);
        dialog.handle_key(Key::Down);
        assert_eq!(dialog.selected, 1, "the next section, on its first setting");
        dialog.handle_key(Key::Down);
        assert_eq!(dialog.selected, 2);
        let frame = dialog.render(120, false);
        assert!(frame.contains("OMP"), "{frame}");
        assert!(frame.contains("● on"), "a switch reads on or off: {frame}");
        dialog.handle_key(Key::Enter);
        assert_eq!(dialog.pane, SettingsPane::Settings);
        for width in [120, 80, 40] {
            // forgeguard: allow FG-ALG-001 -- three widths times one frame's rows
            for line in dialog.render(width, true).lines() {
                assert_eq!(visible_len(line), width.max(MIN_WIDTH), "{width}: {line}");
            }
        }
    }

    /// Space writes the other value of a switch straight away, and does
    /// nothing on a row that is not one.
    #[test]
    fn space_flips_a_switch() {
        let mut dialog = SettingsDialogState::new(vec![bool_row("true"), integer_row("2")]);
        assert_eq!(
            dialog.handle_key(Key::Char(' ')),
            Some(SettingsAction::Apply(0, "false".to_owned()))
        );
        dialog.handle_key(Key::Down);
        assert_eq!(dialog.handle_key(Key::Char(' ')), None);
    }

    #[test]
    fn narrow_terminal_falls_back_to_single_pane() {
        let dialog = SettingsDialogState::new(vec![choice_row("ui.style", "modern", true)]);
        let frame = dialog.render(40, false);
        assert!(frame.contains("ui.style"), "{frame}");
        assert!(
            !frame.contains("DETAIL & CONFIG"),
            "narrow mode does not render multi-pane header"
        );
    }

    #[test]
    fn editing_shows_interactive_controls_in_right_pane() {
        let mut dialog = SettingsDialogState::new(vec![
            choice_row("ui.style", "modern", true),
            bool_row("true"),
            integer_row("2"),
        ]);
        dialog.handle_key(Key::Enter);
        let frame_choice = dialog.render(80, false);
        assert!(frame_choice.contains("select:"), "{frame_choice}");
        assert!(frame_choice.contains("[• modern]"), "{frame_choice}");

        dialog.handle_key(Key::Interrupt);
        dialog.handle_key(Key::Down);
        dialog.handle_key(Key::Enter);
        let frame_bool = dialog.render(80, false);
        assert!(frame_bool.contains("toggle:"), "{frame_bool}");
        assert!(frame_bool.contains("[• true]"), "{frame_bool}");

        dialog.handle_key(Key::Interrupt);
        dialog.handle_key(Key::Down);
        dialog.handle_key(Key::Enter);
        let frame_int = dialog.render(80, false);
        assert!(frame_int.contains("step:"), "{frame_int}");
        assert!(frame_int.contains("◄ 2 ►"), "{frame_int}");
    }

    #[test]
    fn multi_pane_preserves_ansi_escapes_without_leaking_raw_sequences() {
        let dialog = SettingsDialogState::new(vec![
            choice_row("ui.style", "modern", true),
            bool_row("true"),
        ]);
        let frame = dialog.render(80, true);
        for (idx, line) in frame.lines().enumerate() {
            assert_eq!(
                visible_len(line),
                80,
                "line {idx} width mismatch: visible_len={}, line={line:?}",
                visible_len(line)
            );
        }
        assert!(
            frame.contains("\x1b["),
            "should contain ANSI escapes when colour is true"
        );
        let stripped = strip_sgr(&frame);
        assert!(
            !stripped.contains("[38;2;"),
            "ANSI escape leaked without leading ESC: {frame}"
        );
        assert!(
            !stripped.contains("mSETTING"),
            "leaked SGR terminator in header: {frame}"
        );
        assert!(
            !stripped.contains("[0m"),
            "leaked RESET terminator: {frame}"
        );
    }
}
