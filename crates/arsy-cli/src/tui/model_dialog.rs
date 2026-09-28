//! The interactive `/model` dialog: 3-pane split view for provider -> model -> effort.
use super::*;

/// Which column of the dialog currently holds navigation focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelPane {
    Provider,
    Model,
    Effort,
}

impl ModelPane {
    pub fn next(self) -> Self {
        match self {
            Self::Provider => Self::Model,
            Self::Model => Self::Effort,
            Self::Effort => Self::Provider,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Provider => Self::Effort,
            Self::Model => Self::Provider,
            Self::Effort => Self::Model,
        }
    }
}

/// Actions resulting from the interactive model dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelDialogAction {
    Apply {
        route: ModelRoute,
        effort: Option<Effort>,
    },
    Close,
}

/// Interactive state for the 3-pane model picker dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelDialogState {
    pub providers: Vec<String>,
    pub models: Vec<ModelChoice>,
    pub effort_choices: Vec<Option<Effort>>,
    pub current_route: ModelRoute,
    pub current_effort: Option<Effort>,
    pub active_pane: ModelPane,
    pub selected_provider: usize,
    pub selected_model: usize,
    pub selected_effort: usize,
    pub notice: Option<String>,
    /// How many provider or model rows one frame may show. A list longer
    /// than the terminal would push the frame past the top of the screen,
    /// and erasing it then erases the conversation above it.
    pub list_rows: usize,
}

impl ModelDialogState {
    pub fn new(
        mut providers: Vec<String>,
        models: Vec<ModelChoice>,
        current_route: ModelRoute,
        current_effort: Option<Effort>,
    ) -> Self {
        // Only add the saved route's provider as a ghost entry if it has models
        // (non-empty name + appears in the model list). Empty route or deleted
        // providers must not appear as "[set]" with no selectable models.
        let route_has_models = !current_route.provider.is_empty()
            && models.iter().any(|m| m.provider == current_route.provider);
        if route_has_models && !providers.iter().any(|p| p == &current_route.provider) {
            providers.push(current_route.provider.clone());
        }
        for m in &models {
            if !providers.iter().any(|p| p == &m.provider) {
                providers.push(m.provider.clone());
            }
        }
        providers.sort();

        let selected_provider = providers
            .iter()
            .position(|p| p == &current_route.provider)
            .unwrap_or(0);

        let effort_choices = effort_choices();
        let selected_effort = effort_choices
            .iter()
            .position(|e| *e == current_effort)
            .unwrap_or_else(|| effort_choices.len().saturating_sub(1));

        let mut state = Self {
            providers,
            models,
            effort_choices,
            current_route,
            current_effort,
            active_pane: ModelPane::Model,
            selected_provider,
            selected_model: 0,
            selected_effort,
            notice: None,
            list_rows: DEFAULT_LIST_ROWS,
        };

        let current_models = state.filtered_models();
        state.selected_model = current_models
            .iter()
            .position(|m| m.slug == state.current_route.model)
            .unwrap_or(0);
        state.refresh_efforts(current_effort);

        state
    }

    /// Offer the efforts the selected model takes, keeping the marked one
    /// where it is still offered and moving it to the nearest one where it
    /// is not: a model listed per effort has only its own levels, and no
    /// `off`.
    fn refresh_efforts(&mut self, marked: Option<Effort>) {
        let levels = self
            .filtered_models()
            .get(self.selected_model)
            .map(|model| model.levels.clone())
            .unwrap_or_default();
        self.effort_choices = allowed_efforts(&levels);
        let snapped = snap_effort(&levels, marked);
        self.selected_effort = self
            .effort_choices
            .iter()
            .position(|choice| *choice == snapped)
            .unwrap_or_else(|| self.effort_choices.len().saturating_sub(1));
    }

    fn marked_effort(&self) -> Option<Effort> {
        self.effort_choices
            .get(self.selected_effort)
            .copied()
            .flatten()
    }

    /// The models that belong to the currently selected provider in Pane 1.
    pub fn filtered_models(&self) -> Vec<&ModelChoice> {
        let Some(provider) = self.providers.get(self.selected_provider) else {
            return Vec::new();
        };
        self.models
            .iter()
            .filter(|m| m.provider == *provider)
            .collect()
    }

    pub fn handle_key(&mut self, key: Key) -> Option<ModelDialogAction> {
        match key {
            Key::Left | Key::CycleMode => {
                self.active_pane = self.active_pane.prev();
                None
            }
            Key::Right | Key::Tab => {
                self.active_pane = self.active_pane.next();
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
            Key::Enter | Key::Newline => {
                let provider = self
                    .providers
                    .get(self.selected_provider)
                    .cloned()
                    .unwrap_or_else(|| self.current_route.provider.clone());
                let filtered = self.filtered_models();
                let Some(model) = filtered.get(self.selected_model) else {
                    self.notice = Some(format!("no models listed for {provider}"));
                    return None;
                };
                let effort = self
                    .effort_choices
                    .get(self.selected_effort)
                    .copied()
                    .flatten();

                Some(ModelDialogAction::Apply {
                    route: ModelRoute {
                        provider,
                        model: model.slug.clone(),
                    },
                    effort,
                })
            }
            Key::Interrupt | Key::Eof => Some(ModelDialogAction::Close),
            _ => None,
        }
    }

    fn step(&mut self, forward: bool) {
        match self.active_pane {
            ModelPane::Provider => {
                let count = self.providers.len();
                if count > 0 {
                    let marked = self.marked_effort();
                    self.selected_provider = if forward {
                        (self.selected_provider + 1) % count
                    } else {
                        (self.selected_provider + count - 1) % count
                    };
                    self.selected_model = 0;
                    self.refresh_efforts(marked);
                }
            }
            ModelPane::Model => {
                let count = self.filtered_models().len();
                if count > 0 {
                    let marked = self.marked_effort();
                    self.selected_model = if forward {
                        (self.selected_model + 1) % count
                    } else {
                        (self.selected_model + count - 1) % count
                    };
                    self.refresh_efforts(marked);
                }
            }
            ModelPane::Effort => {
                let count = self.effort_choices.len();
                if count > 0 {
                    self.selected_effort = if forward {
                        (self.selected_effort + 1) % count
                    } else {
                        (self.selected_effort + count - 1) % count
                    };
                }
            }
        }
    }

    pub fn render(&self, width: usize, colour: bool) -> String {
        let width = width.max(MIN_WIDTH);
        let inner = width.saturating_sub(4);
        let mut lines = vec![dialog_top(" MODEL & EFFORT ", width, colour)];
        if inner < 65 {
            lines.extend(self.single_pane_lines(inner, colour));
        } else {
            lines.extend(self.multi_pane_lines(inner, colour));
        }
        lines.push(dialog_line("", inner, colour, ""));
        if let Some(notice) = &self.notice {
            lines.push(dialog_line(notice, inner, colour, sgr_dim()));
        }
        let footer = "[←/→/Tab] Switch Pane  [↑/↓] Select  [Enter] Confirm  [Esc] Close";
        lines.push(dialog_line(footer, inner, colour, sgr_dim()));
        lines.push(dialog_line(
            "model route applies to subsequent session turns",
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

    fn multi_pane_lines(&self, inner: usize, colour: bool) -> Vec<String> {
        let available = inner.saturating_sub(6);
        let provider_width = (available * 24 / 100).clamp(14, 20);
        let effort_width = (available * 28 / 100).clamp(18, 24);
        let model_width = available.saturating_sub(provider_width + effort_width);
        let divider = paint(colour, sgr_border(), " │ ");

        let mut lines = Vec::new();

        let p_hdr = self.pane_header("PROVIDER", ModelPane::Provider, colour);
        let m_hdr = self.pane_header("MODEL", ModelPane::Model, colour);
        let e_hdr = self.pane_header("EFFORT & STATUS", ModelPane::Effort, colour);
        lines.push(border_line(
            &format!(
                "{}{divider}{}{divider}{}",
                cell(&p_hdr, provider_width),
                cell(&m_hdr, model_width),
                cell(&e_hdr, effort_width),
            ),
            inner,
            colour,
        ));

        let sep = format!(
            "{}─┼─{}─┼─{}",
            "─".repeat(provider_width),
            "─".repeat(model_width),
            "─".repeat(effort_width)
        );
        lines.push(border_line(
            &paint(colour, sgr_border(), &sep),
            inner,
            colour,
        ));

        let p_lines = window(
            self.provider_lines(colour),
            self.selected_provider,
            self.list_rows,
            colour,
        );
        let m_lines = window(
            self.model_lines(colour),
            self.selected_model,
            self.list_rows,
            colour,
        );
        let e_lines = self.effort_lines(colour);

        let max_lines = p_lines.len().max(m_lines.len()).max(e_lines.len()).max(6);
        for i in 0..max_lines {
            let p_cell = cell(
                p_lines.get(i).map(String::as_str).unwrap_or(""),
                provider_width,
            );
            let m_cell = cell(
                m_lines.get(i).map(String::as_str).unwrap_or(""),
                model_width,
            );
            let e_cell = cell(
                e_lines.get(i).map(String::as_str).unwrap_or(""),
                effort_width,
            );
            lines.push(border_line(
                &format!("{p_cell}{divider}{m_cell}{divider}{e_cell}"),
                inner,
                colour,
            ));
        }

        lines
    }

    fn provider_lines(&self, colour: bool) -> Vec<String> {
        self.providers
            .iter()
            .enumerate()
            .map(|(index, provider)| {
                let is_selected = index == self.selected_provider;
                let is_current = provider == &self.current_route.provider;
                let prefix = match (self.active_pane == ModelPane::Provider, is_selected) {
                    (true, true) => "› ",
                    (false, true) => "· ",
                    _ => "  ",
                };
                let badge = if is_current { " [set]" } else { "" };
                let text = format!("{prefix}{provider}{badge}");
                if self.active_pane == ModelPane::Provider && is_selected {
                    paint(colour, sgr_accent(), &text)
                } else if is_selected {
                    paint(colour, BOLD, &text)
                } else {
                    paint(colour, sgr_dim(), &text)
                }
            })
            .collect()
    }

    fn model_lines(&self, colour: bool) -> Vec<String> {
        let filtered = self.filtered_models();
        if filtered.is_empty() {
            return vec![paint(colour, sgr_dim(), "  (no models listed)")];
        }
        filtered
            .iter()
            .enumerate()
            .map(|(index, choice)| {
                let is_selected = index == self.selected_model;
                let is_current = choice.provider == self.current_route.provider
                    && choice.slug == self.current_route.model;
                let prefix = match (self.active_pane == ModelPane::Model, is_selected) {
                    (true, true) => "› ",
                    (false, true) => "· ",
                    _ => "  ",
                };
                let badge = if is_current { " [set]" } else { "" };
                let text = format!("{prefix}{}{badge}", choice.slug);
                if self.active_pane == ModelPane::Model && is_selected {
                    paint(colour, sgr_accent(), &text)
                } else if is_selected {
                    paint(colour, BOLD, &text)
                } else {
                    paint(colour, sgr_dim(), &text)
                }
            })
            .collect()
    }

    fn effort_lines(&self, colour: bool) -> Vec<String> {
        let mut lines = Vec::new();
        for (index, effort) in self.effort_choices.iter().enumerate() {
            let is_selected = index == self.selected_effort;
            let is_current = *effort == self.current_effort;
            let name = effort.map_or("off", Effort::as_str);
            let prefix = match (self.active_pane == ModelPane::Effort, is_selected) {
                (true, true) => "› ",
                (false, true) => "· ",
                _ => "  ",
            };
            let badge = if is_current { " [set]" } else { "" };
            let text = format!("{prefix}{name}{badge}");
            if self.active_pane == ModelPane::Effort && is_selected {
                lines.push(paint(colour, sgr_accent(), &text));
            } else if is_selected {
                lines.push(paint(colour, BOLD, &text));
            } else {
                lines.push(paint(colour, sgr_dim(), &text));
            }
        }

        lines.push(String::new());
        let prov = self
            .providers
            .get(self.selected_provider)
            .map(String::as_str)
            .unwrap_or("?");
        let filtered = self.filtered_models();
        let slug = filtered
            .get(self.selected_model)
            .map(|m| m.slug.as_str())
            .unwrap_or(self.current_route.model.as_str());
        let eff_label = self
            .effort_choices
            .get(self.selected_effort)
            .copied()
            .flatten()
            .map_or("off", Effort::as_str);

        lines.push(paint(colour, sgr_dim(), "route:"));
        lines.push(paint(colour, sgr_dim(), &format!("  {prov}/{slug}")));
        lines.push(paint(colour, sgr_dim(), &format!("effort: {eff_label}")));

        lines
    }

    fn pane_header(&self, title: &str, pane: ModelPane, colour: bool) -> String {
        if self.active_pane == pane {
            if colour {
                format!("{BOLD}{title}{RESET}")
            } else {
                format!("[{title}]")
            }
        } else {
            paint(colour, sgr_dim(), title)
        }
    }

    fn single_pane_lines(&self, inner: usize, colour: bool) -> Vec<String> {
        let mut lines = Vec::new();
        let filtered = self.filtered_models();
        let prov = self
            .providers
            .get(self.selected_provider)
            .map(String::as_str)
            .unwrap_or("?");
        let provider_marker = if self.active_pane == ModelPane::Provider {
            "›"
        } else {
            "·"
        };
        lines.push(paint(
            colour,
            sgr_dim(),
            &format!("{provider_marker} provider: {prov}  [Tab] switch pane"),
        ));
        if filtered.is_empty() {
            lines.push(paint(colour, sgr_dim(), "  (no models listed)"));
        } else {
            let mut rows = Vec::new();
            for (index, choice) in filtered.iter().enumerate() {
                let marked = index == self.selected_model;
                let prefix = match (self.active_pane == ModelPane::Model, marked) {
                    (true, true) => "› ",
                    (false, true) => "· ",
                    _ => "  ",
                };
                let is_current = choice.provider == self.current_route.provider
                    && choice.slug == self.current_route.model;
                let badge = if is_current { " [set]" } else { "" };
                let line = format!("{prefix}[{}] {}{badge}", choice.provider, choice.slug);
                if self.active_pane == ModelPane::Model && marked {
                    rows.push(paint(colour, sgr_accent(), &line));
                } else {
                    rows.push(paint(colour, sgr_dim(), &line));
                }
            }
            lines.extend(window(rows, self.selected_model, self.list_rows, colour));
        }
        let eff = self
            .effort_choices
            .get(self.selected_effort)
            .copied()
            .flatten()
            .map_or("off", Effort::as_str);
        let effort_marker = if self.active_pane == ModelPane::Effort {
            "›"
        } else {
            "·"
        };
        lines.push(paint(
            colour,
            sgr_dim(),
            &format!("{effort_marker} effort: {eff}"),
        ));
        lines
            .into_iter()
            .map(|l| border_line(&l, inner, colour))
            .collect()
    }
}

/// Rows a list shows when the caller has not measured the terminal.
const DEFAULT_LIST_ROWS: usize = 12;

/// Keep at most `rows` of `lines`, scrolled so `selected` stays visible, and
/// say how many were left out on each side.
fn window(lines: Vec<String>, selected: usize, rows: usize, colour: bool) -> Vec<String> {
    let rows = rows.max(3);
    if lines.len() <= rows {
        return lines;
    }
    // Two rows go to the "more" hints, so the list itself gets the rest.
    let shown = rows - 2;
    let start = selected.saturating_sub(shown / 2).min(lines.len() - shown);
    let end = start + shown;
    let mut out = Vec::with_capacity(rows);
    out.push(paint(colour, sgr_dim(), &format!("  ↑ {} more", start)));
    out.extend(lines[start..end].iter().cloned());
    out.push(paint(
        colour,
        sgr_dim(),
        &format!("  ↓ {} more", lines.len() - end),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_model(provider: &str, slug: &str) -> ModelChoice {
        ModelChoice {
            provider: provider.to_owned(),
            slug: slug.to_owned(),
            name: slug.to_owned(),
            levels: Vec::new(),
        }
    }

    /// A model listed once per effort offers only its own levels, with no
    /// `off`, and an effort it lacks moves to the nearest one it has.
    #[test]
    fn a_variant_family_offers_only_its_own_efforts() {
        let mut pro = sample_model("antigravity", "gemini-3.1-pro");
        pro.levels = vec![Effort::Low, Effort::High];
        let claude = sample_model("antigravity", "claude-sonnet-4-6");
        let mut state = ModelDialogState::new(
            vec!["antigravity".to_owned()],
            vec![pro, claude],
            ModelRoute {
                provider: "antigravity".to_owned(),
                model: "gemini-3.1-pro".to_owned(),
            },
            Some(Effort::Medium),
        );
        assert_eq!(
            state.effort_choices,
            [Some(Effort::Low), Some(Effort::High)]
        );
        assert_eq!(state.marked_effort(), Some(Effort::High));

        // Moving to an ordinary model offers every level and `off` again.
        state.handle_key(Key::Down);
        assert_eq!(state.effort_choices, effort_choices());
        assert_eq!(state.marked_effort(), Some(Effort::High));
    }

    #[test]
    fn new_initializes_with_current_provider_and_model_selected() {
        let models = vec![
            sample_model("codex", "gpt-5.6-luna"),
            sample_model("codex", "gpt-5.6-mini"),
            sample_model("anthropic", "claude-3-7-sonnet"),
        ];
        let current_route = ModelRoute {
            provider: "codex".to_owned(),
            model: "gpt-5.6-mini".to_owned(),
        };
        let state = ModelDialogState::new(
            vec!["codex".to_owned(), "anthropic".to_owned()],
            models,
            current_route,
            Some(Effort::High),
        );

        assert_eq!(state.providers[state.selected_provider], "codex");
        assert_eq!(
            state.filtered_models()[state.selected_model].slug,
            "gpt-5.6-mini"
        );
        assert_eq!(
            state.effort_choices[state.selected_effort],
            Some(Effort::High)
        );
        assert_eq!(state.active_pane, ModelPane::Model);
    }

    #[test]
    fn tab_and_arrows_switch_panes_with_wrapping() {
        let mut state = ModelDialogState::new(
            vec!["codex".to_owned()],
            vec![sample_model("codex", "gpt-5.6-luna")],
            ModelRoute {
                provider: "codex".to_owned(),
                model: "gpt-5.6-luna".to_owned(),
            },
            None,
        );

        assert_eq!(state.active_pane, ModelPane::Model);
        state.handle_key(Key::Tab);
        assert_eq!(state.active_pane, ModelPane::Effort);
        state.handle_key(Key::Tab);
        assert_eq!(state.active_pane, ModelPane::Provider);
        state.handle_key(Key::Tab);
        assert_eq!(state.active_pane, ModelPane::Model);

        state.handle_key(Key::Left);
        assert_eq!(state.active_pane, ModelPane::Provider);
        state.handle_key(Key::Right);
        assert_eq!(state.active_pane, ModelPane::Model);
        state.handle_key(Key::Right);
        assert_eq!(state.active_pane, ModelPane::Effort);
    }

    #[test]
    fn switching_provider_filters_models_and_resets_index() {
        let models = vec![
            sample_model("anthropic", "claude-3-7-sonnet"),
            sample_model("codex", "gpt-5.6-luna"),
            sample_model("codex", "gpt-5.6-mini"),
        ];
        let mut state = ModelDialogState::new(
            vec!["anthropic".to_owned(), "codex".to_owned()],
            models,
            ModelRoute {
                provider: "codex".to_owned(),
                model: "gpt-5.6-mini".to_owned(),
            },
            None,
        );

        assert_eq!(state.filtered_models().len(), 2);
        assert_eq!(state.selected_model, 1);

        state.handle_key(Key::Left); // Focus Provider pane
        assert_eq!(state.active_pane, ModelPane::Provider);
        state.handle_key(Key::Up); // Move to anthropic
        assert_eq!(state.providers[state.selected_provider], "anthropic");
        assert_eq!(state.filtered_models().len(), 1);
        assert_eq!(state.filtered_models()[0].slug, "claude-3-7-sonnet");
        assert_eq!(state.selected_model, 0);
    }

    #[test]
    fn enter_returns_applied_route_and_effort() {
        let models = vec![
            sample_model("codex", "gpt-5.6-luna"),
            sample_model("codex", "gpt-5.6-mini"),
        ];
        let mut state = ModelDialogState::new(
            vec!["codex".to_owned()],
            models,
            ModelRoute {
                provider: "codex".to_owned(),
                model: "gpt-5.6-luna".to_owned(),
            },
            None,
        );

        state.handle_key(Key::Down); // select gpt-5.6-mini
        state.handle_key(Key::Right); // move to Effort pane
        state.handle_key(Key::Down); // step effort
        let action = state.handle_key(Key::Enter);

        match action {
            Some(ModelDialogAction::Apply { route, effort }) => {
                assert_eq!(route.provider, "codex");
                assert_eq!(route.model, "gpt-5.6-mini");
                assert_eq!(effort, state.effort_choices[state.selected_effort]);
            }
            _ => panic!("expected ModelDialogAction::Apply, got {action:?}"),
        }
    }

    #[test]
    fn enter_on_provider_without_models_reports_notice_without_applying() {
        let mut state = ModelDialogState::new(
            vec!["anthropic".to_owned(), "codex".to_owned()],
            vec![sample_model("codex", "gpt-5.6-mini")],
            ModelRoute {
                provider: "codex".to_owned(),
                model: "gpt-5.6-mini".to_owned(),
            },
            None,
        );
        state.handle_key(Key::Left);
        state.handle_key(Key::Up);

        assert_eq!(state.filtered_models().len(), 0);
        assert_eq!(state.handle_key(Key::Enter), None);
        assert_eq!(
            state.notice.as_deref(),
            Some("no models listed for anthropic")
        );
    }

    #[test]
    fn interrupt_and_eof_close_dialog() {
        let mut state = ModelDialogState::new(
            vec!["codex".to_owned()],
            vec![sample_model("codex", "gpt-5.6-luna")],
            ModelRoute {
                provider: "codex".to_owned(),
                model: "gpt-5.6-luna".to_owned(),
            },
            None,
        );

        assert_eq!(
            state.handle_key(Key::Interrupt),
            Some(ModelDialogAction::Close)
        );
        assert_eq!(state.handle_key(Key::Eof), Some(ModelDialogAction::Close));
    }

    #[test]
    fn render_multi_pane_preserves_width_and_ansi() {
        let models = vec![
            sample_model("codex", "gpt-5.6-luna"),
            sample_model("anthropic", "claude-3-7-sonnet"),
        ];
        let state = ModelDialogState::new(
            vec!["codex".to_owned(), "anthropic".to_owned()],
            models,
            ModelRoute {
                provider: "codex".to_owned(),
                model: "gpt-5.6-luna".to_owned(),
            },
            Some(Effort::Medium),
        );

        let frame = state.render(80, true);
        for (idx, line) in frame.lines().enumerate() {
            assert_eq!(
                visible_len(line),
                80,
                "line {idx} width mismatch: visible_len={}, line={line:?}",
                visible_len(line)
            );
        }
        assert!(frame.contains("\x1b["), "contains ANSI escapes");
        let stripped = strip_sgr(&frame);
        assert!(
            !stripped.contains("[38;2;"),
            "no raw leaked ANSI in {frame}"
        );
        assert!(!stripped.contains("[0m"), "no raw leaked reset in {frame}");
        assert!(stripped.contains("PROVIDER"), "{frame}");
        assert!(stripped.contains("MODEL"), "{frame}");
        assert!(stripped.contains("EFFORT"), "{frame}");
    }

    #[test]
    fn narrow_viewport_falls_back_to_single_pane() {
        let state = ModelDialogState::new(
            vec!["codex".to_owned()],
            vec![sample_model("codex", "gpt-5.6-luna")],
            ModelRoute {
                provider: "codex".to_owned(),
                model: "gpt-5.6-luna".to_owned(),
            },
            None,
        );

        let frame = state.render(50, false);
        for (idx, line) in frame.lines().enumerate() {
            assert_eq!(
                visible_len(line),
                50,
                "narrow line {idx} width mismatch: visible_len={}, line={line:?}",
                visible_len(line)
            );
        }
        assert!(frame.contains("provider: codex"), "{frame}");
    }

    #[test]
    fn a_long_model_list_is_windowed_around_the_selection() {
        let models: Vec<_> = (0..200)
            .map(|n| sample_model("codex", &format!("model-{n:03}")))
            .collect();
        let mut state = ModelDialogState::new(
            vec!["codex".to_owned()],
            models,
            ModelRoute {
                provider: "codex".to_owned(),
                model: "model-150".to_owned(),
            },
            None,
        );
        state.list_rows = 10;
        for width in [100, 50] {
            let frame = strip_sgr(&state.render(width, false));
            assert!(frame.lines().count() <= 10 + 10, "{frame}");
            assert!(frame.contains("model-150"), "{frame}");
            assert!(frame.contains("↑ 146 more"), "{frame}");
            assert!(frame.contains("↓ 46 more"), "{frame}");
        }
    }
}
