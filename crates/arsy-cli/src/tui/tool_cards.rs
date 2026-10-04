//! Reusable boxed renderers for tool execution.
use super::*;
/// Render one animated tool execution frame for a long-running call.
pub fn tool_running_frame(
    colour: bool,
    frame: &str,
    name: &str,
    summary: &str,
    elapsed_ms: u128,
) -> String {
    let kind = tool_card_kind(name);
    if modern_style() {
        return format!(
            "{} {} {}",
            render_row(
                colour,
                &arsy_tui::Line::of(
                    format!("  │ {} {name}", tool_card_icon(kind)),
                    tool_card_accent_role(kind),
                ),
            ),
            paint(
                colour,
                sgr_dim(),
                &fit(summary, terminal_width().saturating_sub(28)),
            ),
            paint(colour, sgr_dim(), &format!("{elapsed_ms}ms")),
        );
    }
    let (accent_sgr, _) = tool_card_colors(kind, colour);
    format!(
        "  {} {} {} · {}ms",
        paint(colour, sgr_run(), frame),
        paint(colour, accent_sgr, name),
        paint(colour, sgr_dim(), summary),
        elapsed_ms
    )
}

/// Render a running tool card with a bounded tail of live stdout/stderr.
pub fn tool_running_frame_with_output(
    colour: bool,
    frame: &str,
    name: &str,
    summary: &str,
    elapsed_ms: u128,
    output: &str,
    expanded: bool,
) -> String {
    let kind = tool_card_kind(name);
    if modern_style() {
        let detail = if expanded {
            output.lines().rev().take(2).collect::<Vec<_>>().join(" · ")
        } else {
            output.lines().last().unwrap_or_default().to_owned()
        };
        return format!(
            "{} {} {}",
            render_row(
                colour,
                &arsy_tui::Line::of(
                    format!("  │ {} {name}", tool_card_icon(kind)),
                    tool_card_accent_role(kind),
                ),
            ),
            paint(
                colour,
                sgr_dim(),
                &fit(
                    &format!("{summary} · {detail}"),
                    terminal_width().saturating_sub(36),
                ),
            ),
            paint(colour, sgr_dim(), &format!("{elapsed_ms}ms")),
        );
    }
    let (accent_sgr, _) = tool_card_colors(kind, colour);
    let detail = if expanded {
        let lines: Vec<&str> = output.lines().rev().take(8).collect();
        let tail = lines.into_iter().rev().collect::<Vec<_>>().join(" │ ");
        if tail.is_empty() {
            format!("{summary} │ expanded")
        } else {
            format!("{summary} │ {tail}")
        }
    } else {
        let tail = output.lines().last().unwrap_or_default();
        if tail.is_empty() {
            summary.to_owned()
        } else {
            format!("{summary} │ {tail}")
        }
    };
    format!(
        "  {} {} {} · {}ms · {}",
        paint(colour, sgr_run(), frame),
        paint(colour, accent_sgr, name),
        paint(
            colour,
            sgr_dim(),
            &fit(&detail, terminal_width().saturating_sub(24))
        ),
        elapsed_ms,
        if expanded { "^O collapse" } else { "^O expand" }
    )
}

/// How many output lines a card shows, running or finished: a fixed tail
/// window, so a card does not change height when its command ends. Ctrl+O
/// lifts the cap.
pub const PREVIEW_LINES: usize = 10;

/// `ui.tool_output`: how much of its output a card shows before Ctrl+O.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolOutput {
    /// Only a line saying how much output there was.
    Collapsed,
    /// The last [`PREVIEW_LINES`].
    Preview,
    /// All of it.
    Expanded,
}

static TOOL_OUTPUT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);

/// Set from the `ui.tool_output` value; an unknown one keeps the default,
/// because the loader has already refused it.
pub fn set_tool_output(mode: &str) {
    let mode = match mode {
        "collapsed" => 0,
        "expanded" => 2,
        _ => 1,
    };
    TOOL_OUTPUT.store(mode, std::sync::atomic::Ordering::Relaxed);
}

pub fn tool_output() -> ToolOutput {
    match TOOL_OUTPUT.load(std::sync::atomic::Ordering::Relaxed) {
        0 => ToolOutput::Collapsed,
        2 => ToolOutput::Expanded,
        _ => ToolOutput::Preview,
    }
}

/// Whether a new card opens showing everything. Ctrl+O flips it from there.
pub fn opens_expanded() -> bool {
    tool_output() == ToolOutput::Expanded
}

/// How many output lines a card shows when it is not expanded.
///
/// `expanded` mode rests at the preview, so Ctrl+O on an expanded card has
/// something smaller to fall back to.
fn resting_lines() -> usize {
    resting_lines_for(tool_output())
}

fn resting_lines_for(mode: ToolOutput) -> usize {
    match mode {
        ToolOutput::Collapsed => 0,
        ToolOutput::Preview | ToolOutput::Expanded => PREVIEW_LINES,
    }
}

/// The tail of `lines` a card shows: at most `keep` of them, after a marker
/// saying how many came before.
fn tail_body(lines: &[&str], inner: usize, keep: usize) -> Vec<arsy_tui::Line> {
    let omitted = lines.len().saturating_sub(keep);
    let mut body = Vec::new();
    if omitted > 0 {
        // With nothing kept (`ui.tool_output = collapsed`) none came after.
        let which = if keep == 0 { "" } else { " earlier" };
        body.push(arsy_tui::Line::of(
            format!("… {omitted}{which} lines · ^O expand"),
            arsy_tui::Role::Dim,
        ));
    }
    // Modern cards draw output as text to be read; classic keeps the dim
    // body its golden output was recorded with.
    let base = if modern_style() {
        arsy_tui::Role::Assistant
    } else {
        arsy_tui::Role::Dim
    };
    body.extend(
        lines[omitted..]
            .iter()
            .map(|line| output_line(line, base).fit(inner)),
    );
    body
}

/// A line of command output as plain text: [`output_line`] without colour.
pub(crate) fn printable(line: &str) -> String {
    output_line(line, arsy_tui::Role::Plain).text()
}

/// A line of command output as the spans a card shows.
///
/// `Span::new` drops control characters one by one, which would leave the
/// rest of a colour escape behind as `[31m`. So escape sequences go whole,
/// tabs become the spaces they stood for, and a line redrawn with `\r` (a
/// progress bar) keeps only its last state. The command's own basic colours
/// are kept, mapped onto the theme's roles so they read in every palette;
/// uncoloured text is `base`.
pub(crate) fn output_line(line: &str, base: arsy_tui::Role) -> arsy_tui::Line {
    let line = line
        .rsplit('\r')
        .find(|part| !part.is_empty())
        .unwrap_or_default();
    let mut out = arsy_tui::Line::new();
    let mut run = String::new();
    let mut style = arsy_tui::Style::new(base);
    let mut column = 0;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters, then one final byte in `@`..=`~`. Only
                // `m` (colour) is applied; cursor movement means nothing here.
                Some('[') => {
                    let mut params = String::new();
                    // forgeguard: allow FG-ALG-001 -- advances the one iterator the outer loop reads; linear in the line
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            if c == 'm' {
                                out = out.push(std::mem::take(&mut run), style);
                                style = sgr(&params, style, base);
                            }
                            break;
                        }
                        params.push(c);
                    }
                }
                // OSC: up to BEL or the string terminator `ESC \`.
                Some(']') => {
                    // forgeguard: allow FG-ALG-001 -- advances the one iterator the outer loop reads; linear in the line
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                // Any other escape is two characters long.
                _ => {}
            },
            '\t' => {
                let pad = 4 - column % 4;
                run.extend(std::iter::repeat_n(' ', pad));
                column += pad;
            }
            c => {
                run.push(c);
                column += 1;
            }
        }
    }
    out.push(run, style)
}

/// `style` after one SGR sequence. The eight basic colours and their bright
/// forms become the theme role that means the same thing; a 256-colour or
/// RGB colour is skipped, because a command's exact shade is not worth a
/// palette that no longer matches the theme.
fn sgr(params: &str, mut style: arsy_tui::Style, base: arsy_tui::Role) -> arsy_tui::Style {
    let mut codes = params
        .split(';')
        .map(|code| code.parse::<u16>().unwrap_or(0));
    while let Some(code) = codes.next() {
        match code {
            0 => style = arsy_tui::Style::new(base),
            1 => style = style.bold(),
            22 => style.bold = false,
            // An extended colour's arguments: `5;n` or `2;r;g;b`.
            38 | 48 => {
                match codes.next() {
                    Some(5) => codes.next(),
                    Some(2) => codes.nth(2),
                    _ => None,
                };
            }
            code => {
                if let Some(role) = basic_colour(code, base) {
                    style.role = role;
                }
            }
        }
    }
    style
}

/// The theme role a basic or bright foreground colour code stands for.
fn basic_colour(code: u16, base: arsy_tui::Role) -> Option<arsy_tui::Role> {
    use arsy_tui::Role;
    Some(match code {
        30 | 90 => Role::Dim,
        31 | 91 => Role::Err,
        32 | 92 => Role::Ok,
        33 | 93 => Role::Run,
        34 | 94 => Role::Accent,
        35 | 95 => Role::Model,
        36 | 96 => Role::Cwd,
        37 | 97 | 39 => base,
        _ => return None,
    })
}

/// Execution state passed to format the live running tool card.
pub struct RunningToolState<'a> {
    pub name: &'a str,
    pub summary: &'a str,
    pub frame: &'a str,
    pub elapsed_ms: u128,
    pub live_output: &'a str,
    pub expanded: bool,
    /// The model is still writing the call's arguments, so nothing runs yet
    /// and no key reaches the card.
    pub drafting: bool,
}

/// Render an in-progress animated box for an actively executing tool call.
pub fn tool_running_box(width: usize, colour: bool, state: &RunningToolState<'_>) -> Vec<String> {
    if modern_style() {
        let kind = tool_card_kind(state.name);
        // A command is named in the header while it runs, as it is once it has
        // finished, so the card does not change its title at the end.
        let command_header = kind == ToolCardKind::Bash && !state.summary.trim().is_empty();
        // Collapsed, the card carries the call's newest line, because that is
        // the part a reader needs to know it is alive. Expanded, it carries a
        // tail, for the same reason a finished card does: a test suite says
        // what it is doing at the end, and `e` is how the rest comes back.
        // The same tail window the finished card shows, so the output
        // streams in place and the card keeps its height when it ends.
        // Expanded, it takes what the screen can hold above the composer.
        let lines: Vec<&str> = state
            .live_output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let keep = if state.expanded {
            terminal_rows().saturating_sub(12).max(PREVIEW_LINES)
        } else {
            // Collapsed still carries the newest line, so a quiet card reads
            // as running rather than stuck.
            resting_lines().max(1)
        };
        let mut body = tail_body(&lines, width.saturating_sub(4), keep);
        if body.is_empty() && !command_header {
            body.push(arsy_tui::Line::of(state.summary, arsy_tui::Role::Dim));
        }
        let status = CardStatus {
            lead: if state.drafting {
                format!("{} writing", state.frame)
            } else {
                format!(
                    "{} running · {}",
                    state.frame,
                    if state.expanded {
                        "^O collapse"
                    } else {
                        "^O expand"
                    }
                )
            },
            duration_ms: Some(state.elapsed_ms),
            suffix: String::new(),
            role: arsy_tui::Role::Run,
        };
        return render_modern_card_with_trailer(
            colour,
            ModernCard {
                width,
                kind,
                header: if command_header {
                    let reserve = visible_len(&status.modern())
                        + status.trailer().map_or(0, |t| t.len())
                        + 12;
                    format!(
                        "{} {}",
                        tool_card_icon(kind),
                        fit(state.summary, width.saturating_sub(reserve))
                    )
                } else {
                    format!("{} {}", tool_card_icon(kind), state.name)
                },
                body,
                status: status.modern(),
                status_role: status.role,
                trailer: status.trailer(),
            },
        )
        .lines()
        .map(str::to_owned)
        .collect();
    }
    let width = width.max(MIN_WIDTH);
    let inner = width.saturating_sub(4);
    let kind = tool_card_kind(state.name);
    let icon = tool_card_icon(kind);
    let accent = tool_card_accent_role(kind);
    let border = arsy_tui::Style::new(tool_card_border_role(kind));

    let clean_name = state
        .name
        .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')
        .trim();
    let display_name = if clean_name.is_empty() {
        state.name
    } else {
        clean_name
    };

    let header = if matches!(kind, ToolCardKind::Bash) {
        let max_cmd_len = inner.saturating_sub(4);
        let fitted_cmd = fit(state.summary, max_cmd_len);
        format!(" $ {fitted_cmd} ")
    } else if state.summary.is_empty() {
        format!(" {icon} {display_name} ")
    } else {
        let max_sum_len = inner.saturating_sub(visible_len(display_name) + 5);
        let fitted_sum = fit(state.summary, max_sum_len);
        format!(" {icon} {display_name} {fitted_sum} ")
    };

    let mut body = Vec::new();
    let verb = if state.drafting { "writing" } else { "running" };
    let status_lead = format!(" {} {verb} ({}ms)", state.frame, state.elapsed_ms);
    if !state.expanded {
        let tail = printable(state.live_output.lines().last().unwrap_or_default());
        let tail = tail.trim();
        let status_row = if tail.is_empty() {
            status_lead
        } else {
            let max_tail = inner.saturating_sub(visible_len(&status_lead) + 3);
            let fitted_tail = fit(tail, max_tail);
            format!("{status_lead} · {fitted_tail}")
        };
        body.push(arsy_tui::Line::of(
            fit(&status_row, inner),
            arsy_tui::Role::Dim,
        ));
    } else {
        body.push(arsy_tui::Line::of(
            fit(&status_lead, inner),
            arsy_tui::Role::Run,
        ));

        let out_lines: Vec<&str> = state.live_output.lines().collect();
        let start = out_lines.len().saturating_sub(6);
        body.extend(out_lines.iter().skip(start).map(|line| {
            arsy_tui::Line::of(
                fit(&format!("   {}", printable(line)), inner),
                arsy_tui::Role::Dim,
            )
        }));
    }

    let toggle_hint = if state.drafting {
        ""
    } else if state.expanded {
        " [^O: collapse] "
    } else {
        " [^O: expand] "
    };
    let top = arsy_tui::widget::top_rule(width, Some(&arsy_tui::Line::of(header, accent)), border);
    let mut lines = vec![render_line_segments(colour, &top)];
    lines.extend(body.iter().map(|line| {
        render_line_segments(
            colour,
            &arsy_tui::widget::body_row(line.clone(), inner, border),
        )
    }));
    let bottom = arsy_tui::widget::rule_with_lead(
        width,
        '╰',
        '╯',
        Some(&arsy_tui::Line::of(toggle_hint, arsy_tui::Role::Dim)),
        border,
        0,
    );
    lines.push(render_line_segments(colour, &bottom));
    lines
}
fn render_line_segments(colour: bool, line: &arsy_tui::Line) -> String {
    if !colour {
        return line.text();
    }
    line.spans
        .iter()
        .map(|span| {
            arsy_tui::Line::new()
                .push_span(span.clone())
                .render(palette(), true)
        })
        .collect()
}

/// The mockup's tool card: a tinted panel, the category's icon and name in its
/// accent, the duration pinned to the right of the top rule, and the status let
/// into the bottom one.
///
/// The status rides in the header beside the name, because a panel has no
/// bottom rule to carry it.
struct ModernCard {
    width: usize,
    kind: ToolCardKind,
    header: String,
    body: Vec<arsy_tui::Line>,
    status: String,
    status_role: arsy_tui::Role,
    /// The duration, when the call has finished and there is one to pin.
    trailer: Option<String>,
}

fn render_modern_card_with_trailer(colour: bool, card: ModernCard) -> String {
    let ModernCard {
        width,
        kind,
        header,
        body,
        status,
        status_role,
        trailer,
    } = card;
    let accent = tool_card_accent_role(kind);
    // The header arrives padded, because the classic card lets its title into
    // a rule and needs a space either side. A panel does not, so the padding
    // would read as a gap after the stripe.
    let mut title = arsy_tui::Line::of(header.trim(), accent);
    if !status.is_empty() {
        title = title
            .push("  ", arsy_tui::Role::Plain)
            .push(status, status_role);
    }
    let mut spec = arsy_tui::widget::PanelSpec::new(
        width,
        accent,
        tool_card_head_bg_role(kind),
        tool_card_bg_role(kind),
        &body,
    )
    .title(title);
    if let Some(trailer) = trailer {
        spec = spec.trailer(arsy_tui::Line::of(trailer, arsy_tui::Role::Dim));
    }
    arsy_tui::widget::panel(&spec)
        .iter()
        .map(|row| render_row(colour, row))
        .collect::<Vec<_>>()
        .join("\n")
}

/// How a finished card reports itself.
///
/// The two styles spend the same facts differently: classic puts the duration
/// inside the status on the bottom rule, the mockup pins it to the top right
/// and leaves the status to say only what happened. Carrying them apart means
/// neither has to unpick the other's string.
struct CardStatus {
    /// What happened, with no duration in it.
    lead: String,
    /// `None` when nothing measured it — a Codex item that sends no duration.
    /// The card then shows no trailer rather than a `0ms` that was never true.
    duration_ms: Option<u128>,
    /// `· 42 lines`, when the output was elided.
    suffix: String,
    role: arsy_tui::Role,
}

impl CardStatus {
    fn classic(&self) -> String {
        // Two leading spaces, because the lead used to carry one of its own
        // and the surrounding format added the other. The classic card is the
        // operator's second option and is held to the byte.
        match self.duration_ms {
            Some(ms) => format!("  {} ({ms}ms){} ", self.lead, self.suffix),
            None => format!("  {}{} ", self.lead, self.suffix),
        }
    }

    fn modern(&self) -> String {
        format!("{}{}", self.lead, self.suffix)
    }

    fn trailer(&self) -> Option<String> {
        self.duration_ms.map(|ms| format!("{ms}ms"))
    }
}

fn render_completed_box(
    width: usize,
    colour: bool,
    kind: ToolCardKind,
    header: String,
    body: Vec<arsy_tui::Line>,
    status: &CardStatus,
) -> String {
    if modern_style() {
        return render_modern_card_with_trailer(
            colour,
            ModernCard {
                width,
                kind,
                header,
                body,
                status: status.modern(),
                status_role: status.role,
                trailer: status.trailer(),
            },
        );
    }
    let inner = width.max(MIN_WIDTH).saturating_sub(4);
    let border = arsy_tui::Style::new(tool_card_border_role(kind));
    let spec = arsy_tui::widget::BoxSpec::new(width, border, &body)
        .top(arsy_tui::Line::of(header, tool_card_accent_role(kind)))
        .bottom(arsy_tui::Line::of(
            fit(&status.classic(), inner.saturating_sub(2)),
            status.role,
        ));
    arsy_tui::widget::bordered_box(&spec)
        .iter()
        .map(|line| render_line_segments(colour, line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn compact_tool(name: &str) -> bool {
    matches!(name, "fs.read" | "fs.list" | "git.status" | "git_status")
}

fn compact_command(command: &str) -> Option<&'static str> {
    let command = command.trim();
    if command.starts_with("sed -n ") || command.starts_with("cat ") {
        Some("fs.read")
    } else if command.starts_with("test -") {
        Some("fs.check")
    } else if command.starts_with("git status") {
        Some("git.status")
    } else {
        None
    }
}

/// A styled bash execution frame with command, output, and duration.
/// A command's output without the `evidence: <id>` line the model is given.
/// The id is for the model and the audit trail; a reader has the card.
fn without_evidence(output: &str) -> &str {
    let trimmed = output.trim_end();
    match trimmed.rsplit_once('\n') {
        Some((head, last)) if last.starts_with("evidence: ") => head.trim_end(),
        None if trimmed.starts_with("evidence: ") => "",
        _ => output,
    }
}

pub fn bash_box(
    width: usize,
    colour: bool,
    command: &str,
    output: &str,
    exit_code: Option<i32>,
    duration: Option<std::time::Duration>,
) -> String {
    bash_box_keeping(
        width,
        colour,
        command,
        output,
        exit_code,
        duration,
        PREVIEW_LINES,
    )
}

fn bash_box_keeping(
    width: usize,
    colour: bool,
    command: &str,
    output: &str,
    exit_code: Option<i32>,
    duration: Option<std::time::Duration>,
    keep: usize,
) -> String {
    if modern_style() && exit_code == Some(0) {
        if let Some(name) = compact_command(command) {
            let lines = output.lines().count();
            let detail = if lines == 0 {
                command.to_owned()
            } else {
                format!("{command} · {lines} lines")
            };
            return tool_result_row(colour, name, true, &detail);
        }
    }
    let width = width.max(MIN_WIDTH);
    let inner = width.saturating_sub(4);
    let out_lines: Vec<&str> = without_evidence(output).trim_end().lines().collect();
    let body = tail_body(&out_lines, inner, keep);

    let total_lines = out_lines.len();
    let status = CardStatus {
        lead: match exit_code {
            Some(0) => "✓ done".to_owned(),
            Some(code) => format!("✗ exit {code}"),
            None => "⚙ running".to_owned(),
        },
        duration_ms: duration.map(|taken| taken.as_millis()),
        suffix: if total_lines > keep {
            format!(" · {total_lines} lines")
        } else {
            String::new()
        },
        role: match exit_code {
            Some(0) => arsy_tui::Role::Ok,
            Some(_) => arsy_tui::Role::Err,
            None => arsy_tui::Role::Run,
        },
    };
    // The command gives way to the status, not the other way round: a long
    // command is cut where `done` or the exit code would have been.
    let reserve = if modern_style() {
        visible_len(&status.modern()) + status.trailer().map_or(0, |t| t.len()) + 8
    } else {
        4
    };
    let header = format!(" $ {} ", fit(command, inner.saturating_sub(reserve)));
    render_completed_box(width, colour, ToolCardKind::Bash, header, body, &status)
}

/// A styled tool execution box for filesystem, search, or MCP operations.
pub fn tool_box(
    width: usize,
    colour: bool,
    name: &str,
    summary: &str,
    output: &str,
    success: bool,
    duration: std::time::Duration,
) -> String {
    tool_box_keeping(
        width,
        colour,
        name,
        summary,
        output,
        success,
        Some(duration),
        PREVIEW_LINES,
    )
}

#[allow(clippy::too_many_arguments)]
fn tool_box_keeping(
    width: usize,
    colour: bool,
    name: &str,
    summary: &str,
    output: &str,
    success: bool,
    duration: Option<std::time::Duration>,
    keep: usize,
) -> String {
    if modern_style() && success && compact_tool(name) {
        let lines = output.lines().count();
        let detail = if lines == 0 {
            format!("{summary} · done")
        } else {
            format!("{summary} · {lines} lines")
        };
        return tool_result_row(colour, name, true, &detail);
    }
    let width = width.max(MIN_WIDTH);
    let inner = width.saturating_sub(4);
    let kind = tool_card_kind(name);
    let icon = tool_card_icon(kind);
    let clean_name = name
        .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')
        .trim();
    let display_name = if clean_name.is_empty() {
        name
    } else {
        clean_name
    };
    let prefix = format!(" {icon} {display_name} ");
    let prefix_len = visible_len(&prefix);
    let max_summary_len = inner.saturating_sub(prefix_len + 1);
    let fitted_summary = fit(summary, max_summary_len);
    let header = if summary.is_empty() {
        prefix
    } else {
        format!("{prefix}{fitted_summary} ")
    };

    let out_lines: Vec<&str> = output.trim_end().lines().collect();
    let body = tail_body(&out_lines, inner, keep);

    let total_lines = out_lines.len();
    let status = CardStatus {
        lead: if success {
            "✓ completed".to_owned()
        } else {
            "✗ failed".to_owned()
        },
        duration_ms: duration.map(|taken| taken.as_millis()),
        suffix: if total_lines > keep {
            format!(" · {total_lines} lines")
        } else {
            String::new()
        },
        role: if success {
            arsy_tui::Role::Ok
        } else {
            arsy_tui::Role::Err
        },
    };
    render_completed_box(width, colour, kind, header, body, &status)
}

/// Tool categories used to keep verbose cards visually consistent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolCardKind {
    Bash,
    File,
    Network,
    Mcp,
    Search,
    Generic,
}

pub fn tool_card_kind(name: &str) -> ToolCardKind {
    let clean = name
        .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')
        .trim();
    if matches!(clean, "bash" | "shell.execute") {
        ToolCardKind::Bash
    } else if clean.starts_with("mcp.") || clean == "mcp" || clean.starts_with("mcp_") {
        ToolCardKind::Mcp
    } else if matches!(
        clean,
        "fs.read"
            | "fs.list"
            | "fs.write"
            | "fs.edit"
            | "fs.delete"
            | "fs.move"
            | "edit"
            | "apply_patch"
            | "view_file"
            | "write_to_file"
            | "replace_file_content"
            | "list_dir"
    ) || clean.starts_with("fs.")
        || clean.ends_with("_file")
        || clean.ends_with("_dir")
    {
        ToolCardKind::File
    } else if clean.starts_with("search.")
        || clean.contains("search")
        || clean.contains("grep")
        || clean.contains("find")
    {
        ToolCardKind::Search
    } else if clean == "network.connect"
        || clean == "curl"
        || clean == "http"
        || clean.contains("url")
        || clean.contains("fetch")
    {
        ToolCardKind::Network
    } else {
        ToolCardKind::Generic
    }
}

pub fn tool_card_icon(kind: ToolCardKind) -> &'static str {
    match kind {
        ToolCardKind::Bash => "$",
        ToolCardKind::File => "✎",
        ToolCardKind::Network => "⇄",
        ToolCardKind::Mcp => "⌘",
        ToolCardKind::Search => "⌕",
        ToolCardKind::Generic => "⚙",
    }
}

pub(super) fn tool_card_accent_role(kind: ToolCardKind) -> arsy_tui::Role {
    match kind {
        ToolCardKind::Bash => arsy_tui::Role::ToolBashAccent,
        ToolCardKind::File => arsy_tui::Role::ToolFileAccent,
        ToolCardKind::Search => arsy_tui::Role::ToolSearchAccent,
        ToolCardKind::Mcp => arsy_tui::Role::ToolMcpAccent,
        ToolCardKind::Network => arsy_tui::Role::ToolNetworkAccent,
        ToolCardKind::Generic => arsy_tui::Role::ToolGenericAccent,
    }
}

/// The panel a card of this category sits on. A neutral theme resolves all six
/// to the composer's own surface, so `mono` stays neutral without a branch here.
pub(super) fn tool_card_bg_role(kind: ToolCardKind) -> arsy_tui::Role {
    match kind {
        ToolCardKind::Bash => arsy_tui::Role::ToolBashBg,
        ToolCardKind::File => arsy_tui::Role::ToolFileBg,
        ToolCardKind::Search => arsy_tui::Role::ToolSearchBg,
        ToolCardKind::Mcp => arsy_tui::Role::ToolMcpBg,
        ToolCardKind::Network => arsy_tui::Role::ToolNetworkBg,
        ToolCardKind::Generic => arsy_tui::Role::ToolGenericBg,
    }
}

/// The header strip a panel of this category wears.
pub(super) fn tool_card_head_bg_role(kind: ToolCardKind) -> arsy_tui::Role {
    match kind {
        ToolCardKind::Bash => arsy_tui::Role::ToolBashHeadBg,
        ToolCardKind::File => arsy_tui::Role::ToolFileHeadBg,
        ToolCardKind::Search => arsy_tui::Role::ToolSearchHeadBg,
        ToolCardKind::Mcp => arsy_tui::Role::ToolMcpHeadBg,
        ToolCardKind::Network => arsy_tui::Role::ToolNetworkHeadBg,
        ToolCardKind::Generic => arsy_tui::Role::ToolGenericHeadBg,
    }
}

pub(super) fn tool_card_border_role(kind: ToolCardKind) -> arsy_tui::Role {
    match kind {
        ToolCardKind::Bash => arsy_tui::Role::ToolBashBorder,
        ToolCardKind::File => arsy_tui::Role::ToolFileBorder,
        ToolCardKind::Search => arsy_tui::Role::ToolSearchBorder,
        ToolCardKind::Mcp => arsy_tui::Role::ToolMcpBorder,
        ToolCardKind::Network => arsy_tui::Role::ToolNetworkBorder,
        ToolCardKind::Generic => arsy_tui::Role::ToolGenericBorder,
    }
}

/// Category-specific (accent, border) color pair for tool cards.
pub fn tool_card_colors(kind: ToolCardKind, colour: bool) -> (&'static str, &'static str) {
    if !colour {
        return ("", "");
    }
    (
        palette()
            .code(tool_card_accent_role(kind))
            .expect("tool card accent role has a palette code"),
        palette()
            .code(tool_card_border_role(kind))
            .expect("tool card border role has a palette code"),
    )
}

/// Render one completed verbose card with a typed header and bounded body.
pub fn tool_card(
    width: usize,
    colour: bool,
    name: &str,
    summary: &str,
    output: &str,
    success: bool,
    duration: std::time::Duration,
) -> String {
    tool_card_view(
        width,
        colour,
        name,
        summary,
        output,
        success,
        Some(duration),
        opens_expanded(),
    )
}

/// [`tool_card`], or with every output line when `expanded` (Ctrl+O).
///
/// `duration` is `None` for a call replayed from a stored session, which kept
/// what the call said but not how long it took.
#[allow(clippy::too_many_arguments)]
pub fn tool_card_view(
    width: usize,
    colour: bool,
    name: &str,
    summary: &str,
    output: &str,
    success: bool,
    duration: Option<std::time::Duration>,
    expanded: bool,
) -> String {
    let keep = if expanded {
        usize::MAX
    } else {
        resting_lines()
    };
    match tool_card_kind(name) {
        ToolCardKind::Bash => bash_box_keeping(
            width,
            colour,
            summary,
            output,
            Some(exit_code(output, success)),
            duration,
            keep,
        ),
        _ => tool_box_keeping(
            width, colour, name, summary, output, success, duration, keep,
        ),
    }
}

/// The exit code a finished command reported.
///
/// The tool result carries it only as the sentence the model is shown, which
/// is also all a replayed session has. A failure that names no code (a signal,
/// a deadline) reads as `1`.
fn exit_code(output: &str, success: bool) -> i32 {
    if success {
        return 0;
    }
    output
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("Command exited with code "))
        .and_then(|code| code.trim().parse().ok())
        .unwrap_or(1)
}

/// A diff row showing modified file paths and change stats.
pub fn diff_row(colour: bool, path: &str, added: usize, deleted: usize) -> String {
    format!(
        "  {} {} {} {}",
        paint(colour, sgr_bullet(), "•"),
        paint(colour, sgr_accent(), path),
        paint(colour, sgr_ok(), &format!("+{added}")),
        paint(colour, sgr_err(), &format!("-{deleted}")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_drops_escapes_whole_and_expands_tabs() {
        assert_eq!(printable("\x1b[31merror\x1b[0m: no"), "error: no");
        assert_eq!(printable("\x1b]8;;https://x\x07link\x1b]8;;\x1b\\"), "link");
        assert_eq!(printable("a\tb"), "a   b");
        assert_eq!(printable("ab\tc"), "ab  c");
        assert_eq!(printable(" 10%\r 50%\r100%"), "100%");
        assert_eq!(printable("done\r"), "done");
    }

    #[test]
    fn a_commands_basic_colours_become_theme_roles() {
        use arsy_tui::Role;
        let line = output_line(
            "ok \x1b[31mred\x1b[0m \x1b[1;32mgreen\x1b[39m \x1b[38;5;196mx\x1b[m",
            Role::Assistant,
        );
        let spans: Vec<(&str, Role, bool)> = line
            .spans
            .iter()
            .map(|span| (span.text(), span.style.role, span.style.bold))
            .collect();
        assert_eq!(
            spans,
            [
                ("ok ", Role::Assistant, false),
                ("red", Role::Err, false),
                (" ", Role::Assistant, false),
                ("green", Role::Ok, true),
                (" ", Role::Assistant, true),
                ("x", Role::Assistant, true),
            ]
        );
    }

    #[test]
    fn exit_code_reads_the_sentence_the_model_was_shown() {
        assert_eq!(exit_code("ok", true), 0);
        assert_eq!(
            exit_code("boom\n\nCommand exited with code 3\nevidence: 1f", false),
            3
        );
        assert_eq!(exit_code("Command was terminated by a signal", false), 1);
    }

    #[test]
    fn each_tool_output_mode_rests_at_its_own_height() {
        assert_eq!(resting_lines_for(ToolOutput::Collapsed), 0);
        assert_eq!(resting_lines_for(ToolOutput::Preview), PREVIEW_LINES);
        assert_eq!(resting_lines_for(ToolOutput::Expanded), PREVIEW_LINES);
        let lines = ["a", "b", "c"];
        let collapsed = tail_body(&lines, 40, 0);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].text(), "… 3 lines · ^O expand");
        assert!(tail_body(&lines, 40, 1)[0]
            .text()
            .contains("2 earlier lines"));
    }
}
