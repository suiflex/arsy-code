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
        if expanded { "e collapse" } else { "e expand" }
    )
}

/// Execution state passed to format the live running tool card.
pub struct RunningToolState<'a> {
    pub name: &'a str,
    pub summary: &'a str,
    pub frame: &'a str,
    pub elapsed_ms: u128,
    pub live_output: &'a str,
    pub expanded: bool,
}

/// Render an in-progress animated box for an actively executing tool call.
pub fn tool_running_box(width: usize, colour: bool, state: &RunningToolState<'_>) -> Vec<String> {
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
    let status_lead = format!(" {} running ({}ms)", state.frame, state.elapsed_ms);
    if !state.expanded {
        let tail = state.live_output.lines().last().unwrap_or_default().trim();
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
            arsy_tui::Line::of(fit(&format!("   {line}"), inner), arsy_tui::Role::Dim)
        }));
    }

    let toggle_hint = if state.expanded {
        " [e: collapse] "
    } else {
        " [e: expand] "
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

fn render_completed_box(
    width: usize,
    colour: bool,
    kind: ToolCardKind,
    header: String,
    body: Vec<arsy_tui::Line>,
    status: String,
    status_role: arsy_tui::Role,
) -> String {
    let border = arsy_tui::Style::new(tool_card_border_role(kind));
    let spec = arsy_tui::widget::BoxSpec::new(width, border, &body)
        .top(arsy_tui::Line::of(header, tool_card_accent_role(kind)))
        .bottom(arsy_tui::Line::of(status, status_role));
    arsy_tui::widget::bordered_box(&spec)
        .iter()
        .map(|line| render_line_segments(colour, line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A styled bash execution frame with command, output, and duration.
pub fn bash_box(
    width: usize,
    colour: bool,
    command: &str,
    output: &str,
    exit_code: Option<i32>,
    duration: std::time::Duration,
) -> String {
    let width = width.max(MIN_WIDTH);
    let inner = width.saturating_sub(4);
    let max_cmd_len = inner.saturating_sub(4);
    let fitted_command = fit(command, max_cmd_len);
    let header = format!(" $ {fitted_command} ");

    let out_lines: Vec<&str> = output.lines().collect();
    let max_preview = 10;
    let mut body = Vec::new();
    if out_lines.len() <= max_preview {
        body.extend(
            out_lines
                .iter()
                .map(|line| arsy_tui::Line::of(fit(line, inner), arsy_tui::Role::Dim)),
        );
    } else {
        let omitted = out_lines.len() - max_preview;
        body.push(arsy_tui::Line::of(
            format!("… ({} earlier lines omitted)", omitted),
            arsy_tui::Role::Dim,
        ));
        body.extend(
            out_lines
                .iter()
                .skip(omitted)
                .map(|line| arsy_tui::Line::of(fit(line, inner), arsy_tui::Role::Dim)),
        );
    }

    let total_lines = out_lines.len();
    let status_lead = match exit_code {
        Some(0) => format!(" ✓ done ({}ms)", duration.as_millis()),
        Some(code) => format!(" ✗ exit {code} ({}ms)", duration.as_millis()),
        None => format!(" ⚙ running ({}ms)", duration.as_millis()),
    };
    let status_text = if total_lines > max_preview {
        format!(" {status_lead} · {total_lines} lines ")
    } else {
        format!(" {status_lead} ")
    };
    let status_role = match exit_code {
        Some(0) => arsy_tui::Role::Ok,
        Some(_) => arsy_tui::Role::Err,
        None => arsy_tui::Role::Run,
    };
    render_completed_box(
        width,
        colour,
        ToolCardKind::Bash,
        header,
        body,
        fit(&status_text, inner.saturating_sub(2)),
        status_role,
    )
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

    let out_lines: Vec<&str> = output.lines().collect();
    let max_preview = 10;
    let mut body = Vec::new();
    if out_lines.len() <= max_preview {
        body.extend(
            out_lines
                .iter()
                .map(|line| arsy_tui::Line::of(fit(line, inner), arsy_tui::Role::Dim)),
        );
    } else {
        let omitted = out_lines.len() - max_preview;
        body.push(arsy_tui::Line::of(
            format!("… ({} earlier lines omitted)", omitted),
            arsy_tui::Role::Dim,
        ));
        body.extend(
            out_lines
                .iter()
                .skip(omitted)
                .map(|line| arsy_tui::Line::of(fit(line, inner), arsy_tui::Role::Dim)),
        );
    }

    let total_lines = out_lines.len();
    let status_lead = if success {
        format!(" ✓ completed ({}ms)", duration.as_millis())
    } else {
        format!(" ✗ failed ({}ms)", duration.as_millis())
    };
    let status_text = if total_lines > max_preview {
        format!(" {status_lead} · {total_lines} lines ")
    } else {
        format!(" {status_lead} ")
    };
    render_completed_box(
        width,
        colour,
        kind,
        header,
        body,
        fit(&status_text, inner.saturating_sub(2)),
        if success {
            arsy_tui::Role::Ok
        } else {
            arsy_tui::Role::Err
        },
    )
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

fn tool_card_accent_role(kind: ToolCardKind) -> arsy_tui::Role {
    match kind {
        ToolCardKind::Bash => arsy_tui::Role::ToolBashAccent,
        ToolCardKind::File => arsy_tui::Role::ToolFileAccent,
        ToolCardKind::Search => arsy_tui::Role::ToolSearchAccent,
        ToolCardKind::Mcp => arsy_tui::Role::ToolMcpAccent,
        ToolCardKind::Network => arsy_tui::Role::ToolNetworkAccent,
        ToolCardKind::Generic => arsy_tui::Role::ToolGenericAccent,
    }
}

fn tool_card_border_role(kind: ToolCardKind) -> arsy_tui::Role {
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
    let kind = tool_card_kind(name);
    match kind {
        ToolCardKind::Bash => bash_box(
            width,
            colour,
            summary,
            output,
            Some(i32::from(!success)),
            duration,
        ),
        _ => tool_box(width, colour, name, summary, output, success, duration),
    }
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
