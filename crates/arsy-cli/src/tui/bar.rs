//! Launch header and live status bar component.
use super::*;
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineEntry {
    pub sequence: u64,
    pub name: String,
}

/// Read-only projection: only canonical envelopes can advance its cursor.
pub struct TuiState {
    workspace: String,
    session: SessionId,
    cursor: u64,
    timeline: Vec<TimelineEntry>,
    streaming: Option<String>,
    sandbox_assurance: SandboxAssurance,
    model_route: Option<ModelRoute>,
    effort: Option<Effort>,
    approval_mode: String,
    /// The model the launch card was last drawn with, so a change to it can be
    /// noticed without every caller reporting it.
    shown: Option<Option<ModelRoute>>,
}

impl TuiState {
    pub fn new(workspace: String, session: SessionId) -> Self {
        let workspace = compact_home(workspace);
        Self {
            workspace,
            session,
            cursor: 0,
            timeline: Vec::new(),
            streaming: None,
            sandbox_assurance: SandboxAssurance::None,
            model_route: None,
            effort: None,
            approval_mode: "default".to_owned(),
            shown: None,
        }
    }

    /// Whether the launch card on screen still says what the session is doing.
    ///
    /// The model can change during a session, so a fresh launch card is
    /// printed at that boundary. Mode changes remain historical: the original
    /// card says how the session opened, the transcript records transitions,
    /// and the live footer says what is active now.
    pub fn card_is_stale(&mut self) -> bool {
        let current = self.model_route.clone();
        if self.shown.as_ref() == Some(&current) {
            return false;
        }
        self.shown = Some(current);
        true
    }

    pub fn session_id(&self) -> SessionId {
        self.session
    }

    pub fn set_session_id(&mut self, session: SessionId) {
        self.session = session;
    }

    pub fn set_sandbox_assurance(&mut self, assurance: SandboxAssurance) {
        self.sandbox_assurance = assurance;
    }

    pub fn set_model_route(&mut self, route: ModelRoute) {
        self.model_route = Some(route);
    }

    pub fn set_effort(&mut self, effort: Option<Effort>) {
        self.effort = effort;
    }

    pub fn set_approval_mode(&mut self, mode: impl Into<String>) {
        self.approval_mode = mode.into();
    }

    /// One line below the launch card explaining the active approval boundary.
    ///
    /// The mode owns this wording; the renderer only applies the visual
    /// hierarchy. Keeping both surfaces on the same source prevents a footer
    /// that says `plan` while the explanatory copy still describes edits.
    pub fn approval_hint(&self) -> String {
        let description = crate::approval::ApprovalMode::parse(&self.approval_mode).map_or(
            "custom approval policy",
            crate::approval::ApprovalMode::description,
        );
        format!("Approval mode: {} — {description}", self.approval_mode)
    }

    pub fn apply(&mut self, event: &EventEnvelope) -> Result<(), TuiError> {
        if event.session != self.session {
            return Err(TuiError::WrongSession);
        }
        let expected = self.cursor.checked_add(1).ok_or(TuiError::CursorOverflow)?;
        if event.sequence != expected {
            return Err(TuiError::Gap {
                expected,
                actual: event.sequence,
            });
        }
        self.cursor = event.sequence;
        self.timeline.push(TimelineEntry {
            sequence: event.sequence,
            name: event.kind.clone(),
        });
        if event.kind == "model.delta" {
            self.streaming = match &event.payload {
                EventPayload::Inline { data } => data
                    .get("text")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned),
                EventPayload::Artifact { .. } => None,
            };
        }
        if self.timeline.len() > MAX_TIMELINE_EVENTS {
            self.timeline.remove(0);
        }
        Ok(())
    }

    /// The launch card: a bordered box with `>_ ARSY CODE` and its label rows.
    pub fn render(&self, width: usize, colour: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let inner = width.saturating_sub(4);
        let mut rows = vec![
            format!(
                "{} {}{}",
                paint(colour, sgr_dim(), ">_"),
                paint(colour, BOLD, "ARSY CODE"),
                paint(
                    colour,
                    sgr_dim(),
                    &format!(" (v{})", env!("CARGO_PKG_VERSION"))
                ),
            ),
            String::new(),
        ];
        if let Some(route) = &self.model_route {
            rows.push(format!(
                "{}   {}",
                label_row(colour, "model:", &route.to_string(), sgr_model()),
                paint(colour, sgr_dim(), "/model to change"),
            ));
        }
        rows.push(label_row(colour, "directory:", &self.workspace, sgr_cwd()));
        rows.push(label_row(
            colour,
            "sandbox:",
            &format!("{} · read-only", self.sandbox_assurance),
            sgr_dim(),
        ));
        rows.push(label_row(
            colour,
            "session:",
            &self.session.to_string(),
            sgr_dim(),
        ));
        if modern_style() {
            let mode = if self.approval_mode == "plan" {
                "PLAN"
            } else {
                self.approval_mode.as_str()
            };
            rows.push(label_row(colour, "mode:", mode, sgr_accent()));
        }
        if let Some(entry) = self.timeline.last() {
            rows.push(label_row(
                colour,
                "event:",
                &format!("{} {}", entry.sequence, entry.name),
                sgr_dim(),
            ));
        }
        if let Some(text) = &self.streaming {
            rows.push(paint(colour, sgr_assistant(), text));
        }

        // One blank line inside each border, so the card breathes rather than
        // starting on the rule.
        let mut rows = beside_logo(rows, inner, colour);
        rows.insert(0, String::new());
        rows.push(String::new());
        let border = arsy_tui::Role::Border.into();
        let mut lines = vec![render_row(
            colour,
            &arsy_tui::widget::top_rule(width, None, border),
        )];
        // The body rows are painted strings — `label_row` styles them, and on
        // a Kitty terminal `beside_logo` splices an image escape into them —
        // so they cannot go through the span model, which strips escapes from
        // untrusted text. They move when the logo does.
        for row in &rows {
            let row = fit(row, inner);
            let pad = " ".repeat(inner.saturating_sub(visible_len(&row)));
            lines.push(format!(
                "{} {row}{pad} {}",
                paint(colour, sgr_border(), "│"),
                paint(colour, sgr_border(), "│"),
            ));
        }
        lines.push(render_row(
            colour,
            &arsy_tui::widget::bottom_rule(width, None, border),
        ));
        lines.join("\n")
    }

    /// The status row shown under the composer: warm model, green directory,
    /// branch at the right edge.
    ///
    /// `branch` is passed rather than kept, because it belongs to the checkout
    /// and can change while the session is open.
    ///
    /// A narrow terminal gives up the fields in the order they can be spared:
    /// the workspace path shrinks to its last segments, then disappears, and
    /// only then is the branch dropped. The branch is never shortened, because
    /// half a branch name reads as a different branch — and it is the field a
    /// reader is least able to reconstruct from anything else on screen.
    pub fn status_row(&self, width: usize, colour: bool, branch: Option<&str>) -> String {
        const INDENT: usize = 2;
        const GAP: usize = 2;
        /// Below this a path has lost the segments that identify it.
        const PATH_FLOOR: usize = 6;

        let width = width.max(MIN_WIDTH);
        let route = self
            .model_route
            .as_ref()
            .map_or_else(|| "no model".to_owned(), ModelRoute::to_string);
        let effort_label = match self.effort {
            None => "○ off".to_owned(),
            Some(Effort::Low) => "◔ low".to_owned(),
            Some(Effort::Medium) => "◑ medium".to_owned(),
            Some(Effort::High) => "● high".to_owned(),
        };
        // Always named, never blank. A row that says nothing about the mode
        // leaves the reader to remember which one they are in, and Shift+Tab
        // can change it between two glances at the screen — so the one moment
        // an operator most needs to see the mode is the moment the row would
        // have been silent. `default` is spelled `manual` here because that is
        // what it does; `/approval manual` is an accepted spelling of it.
        let mode_label = match self.approval_mode.as_str() {
            "default" => "⚙ manual".to_owned(),
            "plan" => "⏸ PLAN".to_owned(),
            mode => format!("⚙ {mode}"),
        };
        let mode_label = Some(mode_label.as_str());
        let branch = branch.unwrap_or_default();

        let model_label = format!("✦ {route}");
        let head = INDENT
            + visible_len(&model_label)
            + GAP
            + visible_len(&effort_label)
            + mode_label.map_or(0, |label| GAP + visible_len(label));
        let branch_label = if branch.is_empty() {
            String::new()
        } else {
            format!("⎇ {branch}")
        };
        let right = if branch_label.is_empty() {
            0
        } else {
            GAP + visible_len(&branch_label)
        };

        // Whatever is left over once the fields that cannot shrink are placed.
        let budget = width.saturating_sub(head + GAP + right);
        let ws_icon_len = visible_len("📁 ");
        let path_budget = budget.saturating_sub(ws_icon_len);
        let workspace = (path_budget >= PATH_FLOOR)
            .then(|| format!("📁 {}", shrink_path(&self.workspace, path_budget)));

        let mut row = format!(
            "{}{}{}{}",
            " ".repeat(INDENT),
            paint(colour, sgr_model(), &model_label),
            " ".repeat(GAP),
            paint(colour, sgr_dim(), &effort_label),
        );
        let mut used = head;
        if let Some(label) = mode_label {
            row.push_str(&" ".repeat(GAP));
            row.push_str(&paint(colour, sgr_accent(), label));
        }
        if let Some(workspace) = &workspace {
            row.push_str(&" ".repeat(GAP));
            row.push_str(&paint(colour, sgr_cwd(), workspace));
            used += GAP + visible_len(workspace);
        }
        // Only now is there a final answer on whether the branch fits.
        if !branch_label.is_empty() {
            if let Some(gap) = width.checked_sub(used + visible_len(&branch_label)) {
                if gap >= GAP {
                    row.push_str(&" ".repeat(gap));
                    row.push_str(&paint(colour, sgr_accent(), &branch_label));
                    return row;
                }
            }
        }
        fit(&row, width)
    }

    pub fn render_approval(request: &ApprovalRequest, width: usize) -> String {
        [
            "APPROVAL REQUIRED".to_owned(),
            format!("Effect: {}", request.intended_effect()),
            format!("Scope: {}", request.scope()),
            format!("Reversibility: {}", request.reversibility()),
            format!("Reason: {}", request.reason),
            "Choices: [d] deny  [o] approve operation  [r] approve displayed rule".to_owned(),
        ]
        .into_iter()
        .map(|line| fit(&line, width.max(MIN_WIDTH)))
        .collect::<Vec<_>>()
        .join("\n")
            + "\n"
    }
}
fn compact_home(path: String) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return path;
    };
    let home = std::path::Path::new(&home);
    let path_ref = std::path::Path::new(&path);
    let Ok(relative) = path_ref.strip_prefix(home) else {
        return path;
    };
    if relative.as_os_str().is_empty() {
        "~".to_owned()
    } else {
        format!("~/{}", relative.display())
    }
}

/// Put the mark to the left of the card's text, vertically centred against it.
///
/// The mark is dropped when the card is too narrow to hold both, so a small
/// terminal keeps the text it needs instead of a cropped picture.
fn beside_logo(text: Vec<String>, inner: usize, colour: bool) -> Vec<String> {
    let drawn = colour && logo_graphics();
    let mark_width = if drawn { LOGO_WIDTH } else { BLOCK_LOGO_WIDTH };
    if inner < mark_width + LOGO_GAP + LABEL_WIDTH + 12 {
        return text;
    }
    // Whichever column is shorter is centred against the other, so the mark
    // sits beside the middle of the label rows rather than being pinned to
    // their first line.
    //
    // A terminal that draws images gets the mark as one, laid out as though it
    // were blank space: the escape leaves the cursor where it stands and the
    // image covers the cells the half-blocks would have filled.
    let blank = " ".repeat(mark_width);
    let logo: Vec<String> = if drawn {
        std::iter::once(format!("{}{blank}", logo_graphic()))
            .chain(std::iter::repeat_n(blank.clone(), LOGO_HEIGHT - 1))
            .collect()
    } else {
        logo(colour).to_vec()
    };
    let mark_offset = text.len().saturating_sub(logo.len()) / 2;
    let text_offset = logo.len().saturating_sub(text.len()) / 2;
    (0..(logo.len() + mark_offset).max(text.len() + text_offset))
        .map(|row| {
            let mark = row
                .checked_sub(mark_offset)
                .and_then(|index| logo.get(index))
                .cloned()
                .unwrap_or_else(|| " ".repeat(mark_width));
            let line = row
                .checked_sub(text_offset)
                .and_then(|index| text.get(index))
                .map_or("", String::as_str);
            format!("{mark}{}{line}", " ".repeat(LOGO_GAP))
        })
        .collect()
}

fn label_row(colour: bool, label: &str, value: &str, value_colour: &str) -> String {
    format!(
        "{}{}{}",
        paint(colour, sgr_dim(), label),
        " ".repeat(LABEL_WIDTH.saturating_sub(label.chars().count()) + 1),
        paint(colour, value_colour, value),
    )
}
