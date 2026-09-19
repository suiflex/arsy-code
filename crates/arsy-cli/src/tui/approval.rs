//! Approval and plan decision cards.
use super::*;
/// An option in the interactive Ask/Approval dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
}

/// Result of an interactive Ask/Approval dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AskDialogResult {
    Approve {
        note: Option<String>,
    },
    ApproveRule {
        note: Option<String>,
    },
    Revise {
        note: Option<String>,
    },
    Deny {
        note: Option<String>,
    },
    /// Shift+Tab changes mode without submitting the draft.
    CycleMode,
    Cancel,
}

/// State for interactive Ask/Approval modal dialogs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskDialogState {
    pub title: String,
    pub summary: String,
    pub reason: String,
    pub effect: String,
    pub scope: String,
    pub reversibility: String,
    pub diff_preview: Option<String>,
    pub options: Vec<AskOption>,
    pub(super) preview_offset: usize,
    pub(super) preview_height: usize,
    pub selected: usize,
    pub custom_note: String,
    pub editing_note: bool,
    plan_decision: bool,
    rule_approval: bool,
}

impl AskDialogState {
    pub fn for_approval(
        name: &str,
        summary: &str,
        reason: &str,
        diff_preview: Option<String>,
    ) -> Self {
        Self {
            title: format!("APPROVAL REQUIRED: {name}"),
            summary: summary.to_owned(),
            reason: reason.to_owned(),
            effect: format!("{name} · {summary}"),
            scope: summary.to_owned(),
            reversibility: "not reported".to_owned(),
            diff_preview,
            options: vec![
                AskOption {
                    label: "Approve this call once (yes)".to_owned(),
                    description: Some("Execute this tool call and continue".to_owned()),
                },
                AskOption {
                    label: "Always approve for this session (auto)".to_owned(),
                    description: Some(
                        "Auto-approve this and all subsequent calls in this session".to_owned(),
                    ),
                },
                AskOption {
                    label: "Deny this call (no)".to_owned(),
                    description: Some("Decline this tool call and inform the agent".to_owned()),
                },
            ],
            selected: 0,
            custom_note: String::new(),
            preview_offset: 0,
            preview_height: 15,
            editing_note: false,
            plan_decision: false,
            rule_approval: true,
        }
    }

    pub fn for_approval_details(
        effect: impl Into<String>,
        scope: impl Into<String>,
        reversibility: impl Into<String>,
        reason: impl Into<String>,
        diff_preview: Option<String>,
        rule_approval: bool,
    ) -> Self {
        let mut options = vec![AskOption {
            label: "Approve operation".to_owned(),
            description: Some("Execute only this tool call".to_owned()),
        }];
        if rule_approval {
            options.push(AskOption {
                label: "Approve displayed rule".to_owned(),
                description: Some(
                    "Reuse only the displayed capability and resource this session".to_owned(),
                ),
            });
        }
        options.push(AskOption {
            label: "Deny".to_owned(),
            description: Some("Decline this tool call and inform the agent".to_owned()),
        });
        Self {
            title: "APPROVAL REQUIRED".to_owned(),
            summary: String::new(),
            reason: reason.into(),
            effect: effect.into(),
            scope: scope.into(),
            reversibility: reversibility.into(),
            diff_preview,
            options,
            selected: 0,
            custom_note: String::new(),
            preview_offset: 0,
            preview_height: 15,
            editing_note: false,
            plan_decision: false,
            rule_approval,
        }
    }
    pub fn for_plan(preview: impl Into<String>) -> Self {
        Self {
            title: "PLAN READY".to_owned(),
            summary: "Review the repository-aware plan before any implementation begins."
                .to_owned(),
            reason: "Plan Mode blocks workspace mutations until approval.".to_owned(),
            effect: String::new(),
            scope: String::new(),
            reversibility: String::new(),
            diff_preview: Some(preview.into()),
            options: vec![
                AskOption {
                    label: "Approve and implement".to_owned(),
                    description: Some("Enter acceptEdits mode and execute this plan".to_owned()),
                },
                AskOption {
                    label: "Continue planning / revise".to_owned(),
                    description: Some("Stay in Plan Mode and send the optional note".to_owned()),
                },
                AskOption {
                    label: "Cancel planning".to_owned(),
                    description: Some("Leave Plan Mode without implementing".to_owned()),
                },
            ],
            selected: 0,
            custom_note: String::new(),
            preview_offset: 0,
            preview_height: 15,
            editing_note: false,
            plan_decision: true,
            rule_approval: false,
        }
    }
    /// Limit the plan body to the rows the terminal can show, retaining every
    /// line for PageUp/PageDown navigation.
    pub fn set_preview_height(&mut self, rows: usize) {
        self.preview_height = rows.max(3);
        self.clamp_preview();
    }

    fn clamp_preview(&mut self) {
        let total = self
            .diff_preview
            .as_deref()
            .map_or(0, |preview| preview.lines().count());
        self.preview_offset = self
            .preview_offset
            .min(total.saturating_sub(self.preview_height));
    }

    fn scroll_preview(&mut self, down: bool) {
        let amount = self.preview_height.max(1);
        self.preview_offset = if down {
            self.preview_offset.saturating_add(amount)
        } else {
            self.preview_offset.saturating_sub(amount)
        };
        self.clamp_preview();
    }

    pub fn render(&self, width: usize, colour: bool) -> String {
        if modern_style() {
            return self.render_modern(width, colour);
        }
        let width = width.max(MIN_WIDTH);
        let inner = width.saturating_sub(4);
        let mut lines = vec![render_row(
            colour,
            &arsy_tui::widget::top_rule(
                width,
                Some(&arsy_tui::Line::of(
                    format!(" {} ", self.title),
                    arsy_tui::Style::PLAIN.bold(),
                )),
                arsy_tui::Role::Border.into(),
            ),
        )];

        if !self.summary.is_empty() {
            let row = format!("Summary: {}", self.summary);
            lines.push(Self::box_line(&row, inner, colour, arsy_tui::Role::Dim));
        }
        if !self.reason.is_empty() {
            let row = format!("Reason:  {}", self.reason);
            lines.push(Self::box_line(&row, inner, colour, arsy_tui::Role::Dim));
        }

        if self.diff_preview.is_some() {
            lines.extend(self.preview_rows(inner, colour));
        }

        lines.push(Self::box_line("", inner, colour, arsy_tui::Role::Plain));
        lines.extend(self.option_rows(inner, colour));
        if self.editing_note || !self.custom_note.is_empty() {
            lines.push(Self::box_line("", inner, colour, arsy_tui::Role::Plain));
            lines.push(Self::box_line(
                &self.note_row(),
                inner,
                colour,
                arsy_tui::Role::Assistant,
            ));
        }

        lines.push(Self::box_line("", inner, colour, arsy_tui::Role::Plain));
        lines.push(Self::box_line(
            self.hint(),
            inner,
            colour,
            arsy_tui::Role::Dim,
        ));
        lines.push(render_row(
            colour,
            &arsy_tui::widget::bottom_rule(width, None, arsy_tui::Role::Border.into()),
        ));
        lines.join("\n")
    }

    fn render_modern(&self, width: usize, colour: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let inner = width.saturating_sub(4);
        if self.plan_decision {
            return self.render_modern_plan(width, colour);
        }

        let label_width = 15usize;
        let field = |label: &str, value: &str| {
            arsy_tui::Line::of(format!("{label:<label_width$}"), arsy_tui::Role::Dim).push(
                fit(value, inner.saturating_sub(label_width)),
                arsy_tui::Role::Plain,
            )
        };
        let body = vec![
            field("Effect:", &self.effect),
            field("Scope:", &self.scope),
            field("Reversibility:", &self.reversibility),
            field("Reason:", &self.reason),
            arsy_tui::Line::new(),
            if self.rule_approval {
                arsy_tui::Line::of("[d]", arsy_tui::Role::Dim)
                    .push(" deny  ", arsy_tui::Role::Dim)
                    .push("[o]", arsy_tui::Role::Dim)
                    .push(" approve operation  ", arsy_tui::Role::Dim)
                    .push("[r]", arsy_tui::Role::Run)
                    .push(" approve displayed rule", arsy_tui::Role::Dim)
            } else {
                arsy_tui::Line::of("[d]", arsy_tui::Role::Dim)
                    .push(" deny  ", arsy_tui::Role::Dim)
                    .push("[o]", arsy_tui::Role::Run)
                    .push(" approve operation", arsy_tui::Role::Dim)
            },
        ];
        let title = arsy_tui::Line::of(
            " A P P R O V A L   R E Q U I R E D ",
            arsy_tui::Style::new(arsy_tui::Role::Run).bold(),
        );
        let spec =
            arsy_tui::widget::BoxSpec::new(width, arsy_tui::Style::new(arsy_tui::Role::Run), &body)
                .top(title);
        arsy_tui::widget::bordered_box(&spec)
            .iter()
            .map(|line| render_row(colour, line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_modern_plan(&self, width: usize, colour: bool) -> String {
        let inner = width.saturating_sub(4);
        let accent = |text: &str| paint(colour, sgr_accent(), text);
        let dim = |text: &str| paint(colour, sgr_dim(), text);
        let mut lines = vec![accent(&format!("  ┌ {}", self.title))];
        if !self.summary.is_empty() {
            lines.push(format!("  │ {}", dim(&fit(&self.summary, inner))));
        }
        if let Some(diff) = &self.diff_preview {
            lines.push(format!("  │ {}", accent("Plan preview:")));
            let limit = self.preview_height.max(1);
            let start = self
                .preview_offset
                .min(diff.lines().count().saturating_sub(limit));
            for line in diff.lines().skip(start).take(limit) {
                lines.push(format!("  │ {}", dim(&fit(line, inner))));
            }
        }
        for (index, option) in self.options.iter().enumerate() {
            let marker = if index == self.selected { "›" } else { " " };
            lines.push(format!("  │ {marker} {}. {}", index + 1, option.label));
        }
        lines.push(format!("  └ {}", dim(self.hint())));
        lines.join("\n")
    }

    /// The scrolled window onto the diff or the plan, with the line that says
    /// how much of it is not on screen.
    fn preview_rows(&self, inner: usize, colour: bool) -> Vec<String> {
        let Some(diff) = &self.diff_preview else {
            return Vec::new();
        };
        let mut rows = vec![
            Self::box_line("", inner, colour, arsy_tui::Role::Plain),
            Self::box_line(
                if self.plan_decision {
                    "Plan preview:"
                } else {
                    "Proposed Changes:"
                },
                inner,
                colour,
                arsy_tui::Role::Accent,
            ),
        ];
        let preview_lines: Vec<&str> = diff.lines().collect();
        let max_preview = self.preview_height.max(1);
        let start = self
            .preview_offset
            .min(preview_lines.len().saturating_sub(max_preview));
        rows.extend(
            preview_lines
                .iter()
                .skip(start)
                .take(max_preview)
                .map(|line| Self::render_diff_line(line, inner, colour)),
        );
        if preview_lines.len() > max_preview {
            let end = (start + max_preview).min(preview_lines.len());
            let more = if self.plan_decision {
                format!(
                    "… lines {}-{} of {} · PgUp/PgDn scroll",
                    start + 1,
                    end,
                    preview_lines.len()
                )
            } else {
                format!("… ({} more lines omitted)", preview_lines.len() - end)
            };
            rows.push(Self::box_line(&more, inner, colour, arsy_tui::Role::Dim));
        }
        rows
    }

    /// The answers, each marked with whether it is the one arrowed onto.
    fn option_rows(&self, inner: usize, colour: bool) -> Vec<String> {
        let mut rows = Vec::new();
        for (index, option) in self.options.iter().enumerate() {
            let selected = index == self.selected;
            let radio = if selected { "(•)" } else { "( )" };
            let role = if selected {
                arsy_tui::Role::Accent
            } else {
                arsy_tui::Role::Dim
            };
            let label = format!("{radio} {}. {}", index + 1, option.label);
            rows.push(Self::box_line(&label, inner, colour, role));
            if let Some(description) = &option.description {
                let row = format!("     {description}");
                rows.push(Self::box_line(&row, inner, colour, arsy_tui::Role::Dim));
            }
        }
        rows
    }

    fn note_row(&self) -> String {
        if self.editing_note {
            format!("Note: {}█", self.custom_note)
        } else {
            format!("Note: {}", self.custom_note)
        }
    }

    /// The keys this dialog answers to, in the state it is in.
    fn hint(&self) -> &'static str {
        if self.editing_note {
            return "[Enter] Done Note  [Esc] Clear Note";
        }
        let note = if self.custom_note.is_empty() {
            "Add"
        } else {
            "Edit"
        };
        if !modern_style() && !self.plan_decision {
            return if note == "Add" {
                "[↑/↓] Navigate  [1-3] Choose  [n] Add Note  [y] Yes  [a] Auto  [d] Deny  [Enter] Confirm"
            } else {
                "[↑/↓] Navigate  [1-3] Choose  [n] Edit Note  [y] Yes  [a] Auto  [d] Deny  [Enter] Confirm"
            };
        }
        match (self.plan_decision, note) {
            (true, "Add") => "[↑/↓] Navigate  [PgUp/PgDn] Scroll plan  [1-3] Choose  [e] Add Note  [i] Implement  [r] Revise  [c] Cancel",
            (true, _) => "[↑/↓] Navigate  [PgUp/PgDn] Scroll plan  [1-3] Choose  [e] Edit Note  [i] Implement  [r] Revise  [c] Cancel",
            (false, "Add") => "[d] deny  [o] approve operation  [r] approve displayed rule  [n] note",
            (false, _) => "[d] deny  [o] approve operation  [r] approve displayed rule  [n] edit note",
        }
    }

    fn render_diff_line(line: &str, inner: usize, colour: bool) -> String {
        let fitted = fit(line, inner);
        let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
        if !colour {
            return format!("│ {fitted}{pad} │");
        }
        if line.starts_with('+') {
            let text = format!("\x1b[38;2;120;225;145m\x1b[48;2;25;50;35m{fitted}\x1b[0m");
            format!(
                "{} {text}{pad} {}",
                paint(true, sgr_border(), "│"),
                paint(true, sgr_border(), "│")
            )
        } else if line.starts_with('-') {
            let text = format!("\x1b[38;2;255;120;135m\x1b[48;2;55;25;30m{fitted}\x1b[0m");
            format!(
                "{} {text}{pad} {}",
                paint(true, sgr_border(), "│"),
                paint(true, sgr_border(), "│")
            )
        } else if line.starts_with('@') || line.starts_with('[') || line.starts_with('$') {
            format!(
                "{} {}{pad} {}",
                paint(true, sgr_border(), "│"),
                paint(true, sgr_accent(), &fitted),
                paint(true, sgr_border(), "│")
            )
        } else {
            format!(
                "{} {}{pad} {}",
                paint(true, sgr_border(), "│"),
                paint(true, sgr_dim(), &fitted),
                paint(true, sgr_border(), "│")
            )
        }
    }
    fn box_line(content: &str, inner: usize, colour: bool, role: arsy_tui::Role) -> String {
        render_row(
            colour,
            &arsy_tui::widget::body_row(
                arsy_tui::Line::of(content, role),
                inner,
                arsy_tui::Role::Border.into(),
            ),
        )
    }

    fn current_note(&self) -> Option<String> {
        let trimmed = self.custom_note.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    }

    pub fn handle_key(&mut self, key: Key) -> Option<AskDialogResult> {
        // A note takes every key while it is being typed, and a plan preview
        // takes the keys that scroll it. Only what is left decides the answer.
        if self.editing_note {
            self.edit_note(key);
            return None;
        }
        if self.plan_decision {
            if let Some(handled) = self.scroll_plan(key) {
                return handled;
            }
        }
        self.answer(key)
    }

    /// A key typed into the note. Every key is consumed: a note is being
    /// written, so nothing here answers the dialog.
    fn edit_note(&mut self, key: Key) {
        match key {
            Key::Enter | Key::Newline => self.editing_note = false,
            Key::Char(character) => self.custom_note.push(character),
            Key::Backspace => {
                self.custom_note.pop();
            }
            Key::Interrupt => {
                self.editing_note = false;
                self.custom_note.clear();
            }
            _ => {}
        }
    }

    /// The keys that move the plan preview.
    ///
    /// `None` when the key was not one of them and the dialog should go on to
    /// read it as an answer; `Some` when it was, carrying whatever that key
    /// resolved to.
    fn scroll_plan(&mut self, key: Key) -> Option<Option<AskDialogResult>> {
        match key {
            Key::PageUp => self.scroll_preview(false),
            Key::PageDown => self.scroll_preview(true),
            Key::Home => self.preview_offset = 0,
            Key::End => {
                self.preview_offset = usize::MAX;
                self.clamp_preview();
            }
            Key::CycleMode => return Some(Some(AskDialogResult::CycleMode)),
            _ => return None,
        }
        Some(None)
    }

    /// The keys that choose an answer, or move the marker between them.
    fn answer(&mut self, key: Key) -> Option<AskDialogResult> {
        match key {
            Key::Up => {
                self.selected = self
                    .selected
                    .checked_sub(1)
                    .unwrap_or_else(|| self.options.len().saturating_sub(1));
                None
            }
            Key::Down => {
                self.selected = if self.selected + 1 >= self.options.len() {
                    0
                } else {
                    self.selected + 1
                };
                None
            }
            Key::Enter | Key::Newline | Key::Char(' ') => Some(self.marked()),
            Key::Char(character) => self.shortcut(character),
            Key::Interrupt => Some(AskDialogResult::Cancel),
            _ => None,
        }
    }

    /// Taking the answer the marker stands on. An index past the answers
    /// cannot happen, and approving is the reading that asks again.
    fn marked(&self) -> AskDialogResult {
        match self.selected {
            1 if self.plan_decision => self.revise(),
            1 if self.rule_approval => self.approve_rule(),
            1 => self.deny(),
            2 => self.deny(),
            _ => self.approve(),
        }
    }

    /// The letters and numbers that answer without moving the marker.
    ///
    /// The plan dialog spells its answers with the words it offers — implement,
    /// revise, cancel — and an approval spells the exact effect of each key.
    /// The numbers mean the same thing in both.
    fn shortcut(&mut self, character: char) -> Option<AskDialogResult> {
        if self.plan_decision {
            match character {
                'e' | 'E' => {
                    self.editing_note = true;
                    return None;
                }
                'i' | 'I' => return Some(self.approve()),
                'r' | 'R' => return Some(self.revise()),
                'c' | 'C' => return Some(self.deny()),
                _ => {}
            }
        } else if matches!(character, 'n' | 'N') {
            self.editing_note = true;
            return None;
        }
        match character {
            '1' | 'y' | 'Y' | 'o' | 'O' => Some(self.approve()),
            '2' | 'a' | 'A' | 'r' | 'R' if self.rule_approval => Some(self.approve_rule()),
            '2' | '3' | 'd' | 'D' => Some(self.deny()),
            _ => None,
        }
    }

    fn approve(&self) -> AskDialogResult {
        AskDialogResult::Approve {
            note: self.current_note(),
        }
    }

    fn approve_rule(&self) -> AskDialogResult {
        AskDialogResult::ApproveRule {
            note: self.current_note(),
        }
    }

    fn revise(&self) -> AskDialogResult {
        AskDialogResult::Revise {
            note: self.current_note(),
        }
    }

    fn deny(&self) -> AskDialogResult {
        AskDialogResult::Deny {
            note: self.current_note(),
        }
    }
}
