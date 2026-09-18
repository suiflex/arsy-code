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
    let (accent_sgr, border_sgr) = tool_card_colors(kind, colour);

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

    let header_len = visible_len(&header);
    let top_left = "─".repeat(2);
    let top_right = "─".repeat(width.saturating_sub(2 + 2 + header_len));

    let mut lines = vec![format!(
        "{}{}{}{}",
        paint(colour, border_sgr, "╭"),
        paint(colour, border_sgr, &top_left),
        paint(colour, accent_sgr, &header),
        paint(colour, border_sgr, &format!("{top_right}╮")),
    )];

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
        let fitted = fit(&status_row, inner);
        let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
        lines.push(format!(
            "{} {}{pad} {}",
            paint(colour, border_sgr, "│"),
            paint(colour, sgr_dim(), &fitted),
            paint(colour, border_sgr, "│"),
        ));
    } else {
        let status_fitted = fit(&status_lead, inner);
        let pad = " ".repeat(inner.saturating_sub(visible_len(&status_fitted)));
        lines.push(format!(
            "{} {}{pad} {}",
            paint(colour, border_sgr, "│"),
            paint(colour, sgr_run(), &status_fitted),
            paint(colour, border_sgr, "│"),
        ));

        let out_lines: Vec<&str> = state.live_output.lines().collect();
        let tail_count = 6;
        let start = out_lines.len().saturating_sub(tail_count);
        for line in out_lines.iter().skip(start) {
            let line_fmt = format!("   {line}");
            let fitted = fit(&line_fmt, inner);
            let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
            lines.push(format!(
                "{} {}{pad} {}",
                paint(colour, border_sgr, "│"),
                paint(colour, sgr_dim(), &fitted),
                paint(colour, border_sgr, "│"),
            ));
        }
    }

    let toggle_hint = if state.expanded {
        " [e: collapse] "
    } else {
        " [e: expand] "
    };
    let toggle_len = visible_len(toggle_hint);
    let bot_fill = width.saturating_sub(2 + toggle_len);
    let bot_bar = "─".repeat(bot_fill);
    lines.push(format!(
        "{}{}{}{}",
        paint(colour, border_sgr, "╰"),
        paint(colour, border_sgr, &bot_bar),
        paint(colour, sgr_dim(), toggle_hint),
        paint(colour, border_sgr, "╯"),
    ));

    lines
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
    let header_len = visible_len(&header);
    let (accent_sgr, border_sgr) = tool_card_colors(ToolCardKind::Bash, colour);
    let top_left = "─".repeat(2);
    let top_right = "─".repeat(width.saturating_sub(2 + 2 + header_len));
    let mut lines = vec![format!(
        "{}{}{}{}",
        paint(colour, border_sgr, "╭"),
        paint(colour, border_sgr, &top_left),
        paint(colour, accent_sgr, &header),
        paint(colour, border_sgr, &format!("{top_right}╮")),
    )];

    let out_lines: Vec<&str> = output.lines().collect();
    let max_preview = 10;
    if out_lines.len() <= max_preview {
        for line in &out_lines {
            let fitted = fit(line, inner);
            let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
            lines.push(format!(
                "{} {}{pad} {}",
                paint(colour, border_sgr, "│"),
                paint(colour, sgr_dim(), &fitted),
                paint(colour, border_sgr, "│"),
            ));
        }
    } else {
        let omitted = out_lines.len() - max_preview;
        let more = format!("… ({} earlier lines omitted)", omitted);
        let pad = " ".repeat(inner.saturating_sub(visible_len(&more)));
        lines.push(format!(
            "{} {}{pad} {}",
            paint(colour, border_sgr, "│"),
            paint(colour, sgr_dim(), &more),
            paint(colour, border_sgr, "│"),
        ));
        for line in out_lines.iter().skip(omitted) {
            let fitted = fit(line, inner);
            let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
            lines.push(format!(
                "{} {}{pad} {}",
                paint(colour, border_sgr, "│"),
                paint(colour, sgr_dim(), &fitted),
                paint(colour, border_sgr, "│"),
            ));
        }
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
    let status_sgr = match exit_code {
        Some(0) => sgr_ok(),
        Some(_) => sgr_err(),
        None => sgr_run(),
    };
    let max_status_len = inner.saturating_sub(2);
    let fitted_status = fit(&status_text, max_status_len);
    let bot_len = visible_len(&fitted_status);
    let bot_left = "─".repeat(2);
    let bot_right = "─".repeat(width.saturating_sub(2 + 2 + bot_len));
    lines.push(format!(
        "{}{}{}{}",
        paint(colour, border_sgr, "╰"),
        paint(colour, border_sgr, &bot_left),
        paint(colour, status_sgr, &fitted_status),
        paint(colour, border_sgr, &format!("{bot_right}╯")),
    ));
    lines.join("\n")
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
    let header_len = visible_len(&header);
    let (accent_sgr, border_sgr) = tool_card_colors(kind, colour);
    let top_left = "─".repeat(2);
    let top_right = "─".repeat(width.saturating_sub(2 + 2 + header_len));
    let mut lines = vec![format!(
        "{}{}{}{}",
        paint(colour, border_sgr, "╭"),
        paint(colour, border_sgr, &top_left),
        paint(colour, accent_sgr, &header),
        paint(colour, border_sgr, &format!("{top_right}╮")),
    )];

    let out_lines: Vec<&str> = output.lines().collect();
    let max_preview = 10;
    if out_lines.len() <= max_preview {
        for line in &out_lines {
            let fitted = fit(line, inner);
            let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
            lines.push(format!(
                "{} {}{pad} {}",
                paint(colour, border_sgr, "│"),
                paint(colour, sgr_dim(), &fitted),
                paint(colour, border_sgr, "│"),
            ));
        }
    } else {
        let omitted = out_lines.len() - max_preview;
        let more = format!("… ({} earlier lines omitted)", omitted);
        let pad = " ".repeat(inner.saturating_sub(visible_len(&more)));
        lines.push(format!(
            "{} {}{pad} {}",
            paint(colour, border_sgr, "│"),
            paint(colour, sgr_dim(), &more),
            paint(colour, border_sgr, "│"),
        ));
        for line in out_lines.iter().skip(omitted) {
            let fitted = fit(line, inner);
            let pad = " ".repeat(inner.saturating_sub(visible_len(&fitted)));
            lines.push(format!(
                "{} {}{pad} {}",
                paint(colour, border_sgr, "│"),
                paint(colour, sgr_dim(), &fitted),
                paint(colour, border_sgr, "│"),
            ));
        }
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
    let status_sgr = if success { sgr_ok() } else { sgr_err() };
    let max_status_len = inner.saturating_sub(2);
    let fitted_status = fit(&status_text, max_status_len);
    let bot_len = visible_len(&fitted_status);
    let bot_left = "─".repeat(2);
    let bot_right = "─".repeat(width.saturating_sub(2 + 2 + bot_len));
    lines.push(format!(
        "{}{}{}{}",
        paint(colour, border_sgr, "╰"),
        paint(colour, border_sgr, &bot_left),
        paint(colour, status_sgr, &fitted_status),
        paint(colour, border_sgr, &format!("{bot_right}╯")),
    ));
    lines.join("\n")
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

/// Category-specific (accent, border) color pair for tool cards.
pub fn tool_card_colors(kind: ToolCardKind, colour: bool) -> (&'static str, &'static str) {
    if !colour {
        return ("", "");
    }
    // A neutral theme keeps its own two greys rather than taking the six hues
    // below. The theme says whether it has hue; this used to be inferred by
    // comparing the palette's border and accent codes against `mono`'s
    // literals, which also fired on any theme that happened to share them.
    if palette().hueless {
        return (sgr_accent(), sgr_border());
    }
    match kind {
        ToolCardKind::Bash => ("\x1b[38;2;97;175;239m", "\x1b[38;2;60;125;190m"),
        ToolCardKind::File => ("\x1b[38;2;229;192;123m", "\x1b[38;2;176;136;59m"),
        ToolCardKind::Search => ("\x1b[38;2;198;120;221m", "\x1b[38;2;142;78;163m"),
        ToolCardKind::Mcp => ("\x1b[38;2;86;182;194m", "\x1b[38;2;53;127;137m"),
        ToolCardKind::Network => ("\x1b[38;2;152;195;121m", "\x1b[38;2;93;142;67m"),
        ToolCardKind::Generic => ("\x1b[38;2;224;108;117m", "\x1b[38;2;157;72;80m"),
    }
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
