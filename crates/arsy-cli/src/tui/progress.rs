//! Turn progress, tool cards, and provider event projections.
use super::*;

/// Semantic transcript entries that can be rendered again at a new width.
///
/// The terminal owns scrollback, but a resize can leave a live card at its old
/// width. Keeping the small semantic ledger here lets the façade rebuild one
/// coherent full-width display instead of replaying stale escape sequences.
#[derive(Default)]
pub struct Transcript {
    entries: Vec<TranscriptEntry>,
}

enum TranscriptEntry {
    Banner(String),
    User(String),
    Thinking(String),
    Assistant(String),
    Tool {
        name: String,
        summary: String,
        output: String,
        success: bool,
        duration_ms: u64,
    },
    Todos(Vec<String>),
    ModeChange {
        from: String,
        to: String,
    },
    Approval(String),
    McpLog(String),
    Notice(String),
}

impl Transcript {
    pub fn push_user(&mut self, text: &str) {
        self.entries.push(TranscriptEntry::User(text.to_owned()));
    }
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn push_assistant(&mut self, text: &str) {
        if !text.trim().is_empty() {
            self.entries
                .push(TranscriptEntry::Assistant(text.to_owned()));
        }
    }
    pub fn push_banner(&mut self, text: &str) {
        self.entries.push(TranscriptEntry::Banner(text.to_owned()));
    }

    pub fn push_thinking(&mut self, text: &str) {
        if !text.trim().is_empty() {
            self.entries
                .push(TranscriptEntry::Thinking(text.to_owned()));
        }
    }

    pub fn push_todos(&mut self, todos: &[String]) {
        if !todos.is_empty() {
            self.entries.push(TranscriptEntry::Todos(todos.to_owned()));
        }
    }

    pub fn push_mode_change(&mut self, from: &str, to: &str) {
        self.entries.push(TranscriptEntry::ModeChange {
            from: from.to_owned(),
            to: to.to_owned(),
        });
    }

    pub fn push_approval(&mut self, card: &str) {
        self.entries
            .push(TranscriptEntry::Approval(card.to_owned()));
    }

    pub fn push_mcp_log(&mut self, line: &str) {
        self.entries.push(TranscriptEntry::McpLog(line.to_owned()));
    }

    pub fn push_notice(&mut self, text: &str) {
        self.entries.push(TranscriptEntry::Notice(text.to_owned()));
    }
    pub fn push_tool(
        &mut self,
        name: &str,
        summary: &str,
        output: &str,
        success: bool,
        duration: std::time::Duration,
    ) {
        self.entries.push(TranscriptEntry::Tool {
            name: name.to_owned(),
            summary: summary.to_owned(),
            output: output.to_owned(),
            success,
            duration_ms: duration.as_millis().min(u128::from(u64::MAX)) as u64,
        });
    }

    /// Clear native scrollback and replay the semantic transcript at `width`.
    pub fn repaint(
        &self,
        terminal: &mut dyn Write,
        width: usize,
        colour: bool,
        state: &TuiState,
    ) -> std::io::Result<()> {
        write!(terminal, "\x1b[3J\x1b[H\x1b[2J")?;
        writeln!(terminal, "{}", state.render(width, colour))?;
        writeln!(
            terminal,
            "Use /help for commands, /mcp and /hooks to inspect integrations."
        )?;
        for entry in &self.entries {
            write_entry(terminal, width, colour, entry)?;
        }
        terminal.flush()
    }
}

/// One transcript entry, as the rows it occupies.
///
/// Split from the repaint so that walking the transcript and drawing one of
/// its entries are separate readings, and neither has to carry the other.
fn write_entry(
    terminal: &mut dyn Write,
    width: usize,
    colour: bool,
    entry: &TranscriptEntry,
) -> std::io::Result<()> {
    match entry {
        TranscriptEntry::Banner(text) => writeln!(terminal, "{text}"),
        TranscriptEntry::User(text) => write_user(terminal, width, colour, text),
        TranscriptEntry::Thinking(text) => {
            writeln!(terminal, "{}", thinking_box(width, colour, text))
        }
        TranscriptEntry::Assistant(text) => write_assistant(terminal, width, colour, text),
        TranscriptEntry::Tool {
            name,
            summary,
            output,
            success,
            duration_ms,
        } => {
            let card = tool_card(
                width,
                colour,
                name,
                summary,
                output,
                *success,
                std::time::Duration::from_millis(*duration_ms),
            );
            write!(terminal, "{DISABLE_AUTOWRAP}")?;
            writeln!(terminal, "{card}")?;
            write!(terminal, "{ENABLE_AUTOWRAP}")
        }
        TranscriptEntry::Todos(todos) => {
            for todo in todos {
                writeln!(
                    terminal,
                    "  {} {}",
                    paint(colour, sgr_bullet(), "•"),
                    safe_text(todo)
                )?;
            }
            Ok(())
        }
        TranscriptEntry::ModeChange { from, to } => {
            writeln!(terminal, "{}", mode_row(from, to, colour))
        }
        TranscriptEntry::Approval(card) => writeln!(terminal, "{card}"),
        TranscriptEntry::McpLog(line) => writeln!(terminal, "{}", paint(colour, sgr_dim(), line)),
        TranscriptEntry::Notice(text) => writeln!(terminal, "{}", hook_note_row(colour, text)),
    }
}

/// What the operator typed: the first row carries the marker, the rest are
/// continuations of the same message.
fn write_user(
    terminal: &mut dyn Write,
    width: usize,
    colour: bool,
    text: &str,
) -> std::io::Result<()> {
    if modern_style() {
        for (index, line) in text.lines().enumerate() {
            let prefix = if index == 0 { "›" } else { "·" };
            let row = format!(" {prefix} {}", fit(line, width.saturating_sub(3)));
            let pad = " ".repeat(width.saturating_sub(visible_len(&row)));
            writeln!(
                terminal,
                "{}{}{}",
                if colour { sgr_input_bg() } else { "" },
                row,
                if colour { format!("{pad}{RESET}") } else { pad }
            )?;
        }
        return Ok(());
    }
    for (index, line) in text.lines().enumerate() {
        let prompt = if index == 0 { "› You" } else { "·" };
        writeln!(
            terminal,
            "{} {}",
            paint(colour, BOLD, prompt),
            paint(colour, sgr_assistant(), &fit(line, width.saturating_sub(6)))
        )?;
    }
    Ok(())
}

fn write_assistant(
    terminal: &mut dyn Write,
    width: usize,
    colour: bool,
    text: &str,
) -> std::io::Result<()> {
    writeln!(terminal, "{}", assistant_block(width, colour, text))
}

/// Status dot colours from the brainless `CodexExec` component.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Status {
    Ok,
    Error,
    Run,
}

impl Status {
    fn colour(self) -> &'static str {
        match self {
            Self::Ok => sgr_ok(),
            Self::Error => sgr_err(),
            Self::Run => sgr_run(),
        }
    }
}

fn exec_row(colour: bool, status: Status, command: &str, result: Option<&str>) -> String {
    let head = format!(
        "  {} {}",
        paint(colour, status.colour(), "•"),
        paint(colour, sgr_accent(), command),
    );
    match result {
        Some(result) => format!("{head}  {}", paint(colour, sgr_dim(), result)),
        None => head,
    }
}

/// Render one `codex exec --json` JSONL event as brainless rows.
///
/// Returns `None` for events with no visual form, and for anything that does
/// not parse — a projection must never abort the turn it is displaying.
pub fn render_codex_event(line: &str, colour: bool) -> Option<String> {
    let event: Value = serde_json::from_str(line).ok()?;
    match event.get("type")?.as_str()? {
        // Working is transient composer status, not permanent scrollback.
        "turn.started" => None,
        "item.started" | "item.updated" | "item.completed" => {
            render_codex_item(event.get("item")?, colour)
        }
        "error" => Some(error_row(
            colour,
            event.get("message").and_then(Value::as_str)?,
        )),
        "turn.failed" => Some(error_row(
            colour,
            event
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Turn failed"),
        )),
        _ => None,
    }
}

pub fn working_row(colour: bool) -> String {
    format!(
        "  {} {}",
        paint(colour, sgr_bullet(), "•"),
        paint(colour, BOLD, "Working…"),
    )
}

/// The top border of a thinking section box.
pub fn thinking_box_top(width: usize, colour: bool) -> String {
    if modern_style() {
        // The mockup leaves reasoning unboxed: a marker line and the text
        // indented under it. A rail here would frame the one part of the
        // transcript that is explicitly an aside.
        return paint(colour, sgr_accent(), "  ✻ Thinking");
    }
    render_row(
        colour,
        &arsy_tui::widget::top_rule(
            width,
            Some(&arsy_tui::Line::of(" ✻ Thinking ", arsy_tui::Role::Accent)),
            arsy_tui::Role::Border.into(),
        ),
    )
}

pub fn thinking_box_row(width: usize, colour: bool, text: &str) -> String {
    if modern_style() {
        return paint(colour, sgr_dim(), &format!("  {}", text.trim_end()));
    }
    render_row(
        colour,
        &arsy_tui::widget::body_row(
            arsy_tui::Line::of(text.trim_end(), arsy_tui::Role::Dim),
            arsy_tui::widget::interior(width),
            arsy_tui::Role::Border.into(),
        ),
    )
}

pub fn thinking_box_bottom(width: usize, colour: bool) -> String {
    if modern_style() {
        // Nothing closes an unboxed aside; the next row is its own marker.
        return String::new();
    }
    render_row(
        colour,
        &arsy_tui::widget::bottom_rule(width, None, arsy_tui::Role::Border.into()),
    )
}

pub fn thinking_box(width: usize, colour: bool, body: &str) -> String {
    let mut rows = vec![thinking_box_top(width, colour)];
    for line in body.lines() {
        rows.push(thinking_box_row(width, colour, line));
    }
    // The modern block has no closing row, so an empty one is dropped rather
    // than left to print as a blank line under every aside.
    let bottom = thinking_box_bottom(width, colour);
    if !bottom.is_empty() {
        rows.push(bottom);
    }
    rows.join("\n")
}

/// The composer status line while a turn runs: a spinner, the phase the turn
/// is in, the seconds elapsed, and the cancel hint.
///
/// `phase` is `Connecting…` until the provider produced its first event, then
/// `Working…`; a connect that takes a minute is otherwise indistinguishable
/// from a hang. The spinner frames make the wait visibly alive, which is the
/// whole point: a static line reads as a dead terminal, not as a working one.
pub fn turn_status(
    colour: bool,
    phase: TurnPhase,
    elapsed: std::time::Duration,
    tick: usize,
    queued: usize,
) -> String {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let label = match phase {
        TurnPhase::Cancelling => "Cancelling…",
        TurnPhase::Connecting => "Connecting…",
        TurnPhase::Working => "Working…",
        TurnPhase::Answering => "Answering…",
    };
    let mut status = format!(
        "  {} {} · {}s",
        paint(colour, sgr_run(), FRAMES[tick % FRAMES.len()]),
        paint(colour, BOLD, label),
        elapsed.as_secs(),
    );
    if queued > 0 {
        status.push_str(&paint(colour, sgr_dim(), &format!(" · {queued} queued")));
    }
    status.push_str(&paint(colour, sgr_dim(), " · Esc cancel"));
    status
}

/// Which phase a running turn is in, for the composer status line.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum TurnPhase {
    Connecting,
    Working,
    Answering,
    Cancelling,
}

/// One line of streamed model text, styled as the Codex projection styles an
/// assistant message, so both routes read the same in scrollback.
pub fn assistant_row(colour: bool, text: &str) -> String {
    paint(colour, sgr_assistant(), text.trim_end())
}

/// Render a Markdown response inside its own width-safe card.
pub fn assistant_block(width: usize, colour: bool, text: &str) -> String {
    if modern_style() {
        let body = arsy_tui::render_markdown(text, width.max(MIN_WIDTH).saturating_sub(4), None);
        // `✦`, the marker the mockup uses. The response is deliberately not a
        // card: the mockup leaves the answer unboxed so it reads as prose
        // rather than as one more piece of machinery.
        let mut rows = vec![paint(colour, sgr_assistant(), "  ✦ Response")];
        rows.extend(
            body.iter()
                .map(|line| format!("  {}", render_row(colour, line))),
        );
        return rows.join("\n");
    }

    let width = width.max(MIN_WIDTH);
    let body = arsy_tui::render_markdown(text, arsy_tui::widget::interior(width), None);
    let spec = arsy_tui::widget::BoxSpec::new(width, arsy_tui::Role::Border.into(), &body)
        .top(arsy_tui::Line::of(" ✦ Response ", arsy_tui::Role::Accent));
    arsy_tui::widget::bordered_box(&spec)
        .iter()
        .map(|line| render_row(colour, line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Header for the final assistant response, separating it from tool trace.
pub fn assistant_header(colour: bool) -> String {
    paint(colour, sgr_assistant(), "  ✦ Response")
}

/// What a lifecycle hook said about a call, dimmed so it reads as an aside.
pub fn hook_note_row(colour: bool, note: &str) -> String {
    paint(colour, sgr_dim(), &format!("  hook: {}", safe_text(note)))
}

/// Shown when a turn is stopped from the keyboard.
pub fn interrupted_row(colour: bool) -> String {
    exec_row(colour, Status::Run, "Interrupted", None)
}

/// Draw a lifecycle card through the shared widget, on the category panel the
/// tool's name resolves to.
fn lifecycle_card(
    colour: bool,
    title: &str,
    detail: &str,
    status: &str,
    status_role: arsy_tui::Role,
) -> String {
    let width = terminal_width();
    let kind = tool_card_kind(title);
    let body = if detail.trim().is_empty() {
        Vec::new()
    } else {
        vec![arsy_tui::Line::of(detail, arsy_tui::Role::Dim)]
    };
    let spec = arsy_tui::widget::CardSpec::new(width, tool_card_border_role(kind), &body)
        .title(arsy_tui::Line::of(
            format!(" {title} "),
            tool_card_accent_role(kind),
        ))
        .status(arsy_tui::Line::of(format!(" {status} "), status_role))
        .background(tool_card_bg_role(kind));
    arsy_tui::widget::card(&spec)
        .iter()
        .map(|row| render_row(colour, row))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A lifecycle card — asked for, running, finished — with no duration to pin,
/// drawn by the same widget as a completed call so the transcript does not
/// change shape as a call moves through its states.
fn modern_tool_card(
    colour: bool,
    title: &str,
    detail: &str,
    status: &str,
    status_role: arsy_tui::Role,
) -> String {
    lifecycle_card(colour, title, detail, status, status_role)
}

/// A tool the model wants to run, waiting on the operator's answer. The
/// command or the file list is shown, because that is what is being agreed to.
pub fn tool_prompt_row(colour: bool, name: &str, summary: &str) -> String {
    if modern_style() {
        return modern_tool_card(
            colour,
            &format!("⚙ {name}"),
            summary,
            "approval required",
            arsy_tui::Role::Run,
        );
    }
    exec_row(
        colour,
        Status::Run,
        &format!("{name} {summary}"),
        Some("run it? y / n"),
    )
}

pub fn tool_running_row(colour: bool, name: &str, summary: &str) -> String {
    if modern_style() {
        return modern_tool_card(
            colour,
            &format!("⚙ {name}"),
            summary,
            "running…",
            arsy_tui::Role::Run,
        );
    }
    exec_row(
        colour,
        Status::Run,
        &format!("{name} {summary}"),
        Some("running…"),
    )
}

pub fn tool_result_row(colour: bool, name: &str, ok: bool, detail: &str) -> String {
    if modern_style() {
        return modern_tool_card(
            colour,
            &format!("⚙ {name}"),
            detail,
            if ok { "completed" } else { "failed" },
            if ok {
                arsy_tui::Role::Ok
            } else {
                arsy_tui::Role::Err
            },
        );
    }
    exec_row(
        colour,
        if ok { Status::Ok } else { Status::Error },
        name,
        Some(detail),
    )
}

fn error_row(colour: bool, message: &str) -> String {
    format!(
        "  {} {}",
        paint(colour, sgr_err(), "•"),
        paint(colour, sgr_err(), &unwrap_api_error(message.trim())),
    )
}

/// Provider transport errors arrive as an embedded JSON body; the readable
/// sentence is one level in.
fn unwrap_api_error(message: &str) -> String {
    serde_json::from_str::<Value>(message)
        .ok()
        .and_then(|body| {
            body.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| message.to_owned())
}

/// The line a finished turn leaves behind: which session it belonged to and
/// how to pick it up again.
///
/// The mockup also counts files changed, rules granted and events recorded.
/// Those are not tracked on a turn today, and a footer that stated them would
/// be stating numbers nobody counted, so it carries what is actually known.
pub fn session_footer(session: &str, colour: bool) -> String {
    let short = session.split('-').next().unwrap_or(session);
    format!(
        "{} {} {}",
        paint(colour, sgr_dim(), "  session"),
        paint(colour, sgr_accent(), short),
        paint(colour, sgr_dim(), "· resume with /resume"),
    )
}

/// What the approval mode changed from and to.
///
/// The one row in the transcript that changes what the harness is allowed to
/// do, so it is written where it happened rather than left to be inferred from
/// what stopped asking.
pub fn mode_row(from: &str, to: &str, colour: bool) -> String {
    format!(
        "{} {} {}",
        paint(colour, sgr_dim(), "  MODE"),
        paint(colour, sgr_accent(), from),
        paint(colour, sgr_ok(), &format!("→ {to}")),
    )
}

/// The plan, as the mockup draws it: a count line and one row per item,
/// marked `[x]` done, `[~]` in progress, `[ ]` still to do, with the row the
/// model is on marked by the bullet beside it.
fn codex_todo_block(item: &Value, colour: bool) -> Option<String> {
    let items = item.get("items").and_then(Value::as_array)?;
    if items.is_empty() {
        return None;
    }
    let state = |entry: &Value| {
        entry
            .get("status")
            .or_else(|| entry.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("pending")
            .to_owned()
    };
    let done = items
        .iter()
        .filter(|entry| state(entry) == "completed")
        .count();

    let mut rows = vec![paint(
        colour,
        sgr_dim(),
        &format!("  {done} of {} TODO(s) done", items.len()),
    )];
    for entry in items {
        let label = entry
            .get("text")
            .or_else(|| entry.get("title"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if label.trim().is_empty() {
            continue;
        }
        let (mark, role, current) = match state(entry).as_str() {
            "completed" => ("[x]", sgr_ok(), false),
            "in_progress" => ("[~]", sgr_run(), true),
            _ => ("[ ]", sgr_dim(), false),
        };
        rows.push(format!(
            "{} {} {}",
            paint(colour, sgr_accent(), if current { "  ›" } else { "   " }),
            paint(colour, role, mark),
            paint(colour, sgr_dim(), &safe_text(label)),
        ));
    }
    Some(rows.join("\n"))
}

/// What a Codex item says the command printed, if it says anything.
///
/// The CLI does not always send it, and there is no field in this repository's
/// fixtures for it, so an absent one means "unknown" and the card shows no
/// output region rather than an empty one claiming the command was silent.
fn codex_output(item: &Value) -> String {
    for key in ["aggregated_output", "output", "stdout"] {
        if let Some(text) = item.get(key).and_then(Value::as_str) {
            if !text.trim().is_empty() {
                return text.to_owned();
            }
        }
    }
    String::new()
}

/// How long the item took, when it says. `None` leaves the card's trailer off
/// rather than printing a `0ms` nobody measured.
fn codex_duration(item: &Value) -> Option<std::time::Duration> {
    item.get("duration_ms")
        .and_then(Value::as_u64)
        .map(std::time::Duration::from_millis)
}

fn render_codex_item(item: &Value, colour: bool) -> Option<String> {
    let text = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default();
    match item.get("type")?.as_str()? {
        // The answer, through the same markdown projection the native route
        // uses. This went out as one flat painted string, which is why an
        // operator on the Codex route saw `**bold**` with its asterisks.
        "agent_message" => {
            let body = text("text").trim();
            (!body.is_empty()).then(|| assistant_block(terminal_width(), colour, body))
        }
        // Codex reports some failures as an item rather than a top-level event.
        "error" => Some(error_row(colour, text("message"))),
        "reasoning" => {
            let body = text("text").trim();
            if body.is_empty() {
                return None;
            }
            Some(thinking_box(terminal_width(), colour, body))
        }
        "command_execution" => {
            let exit = item.get("exit_code").and_then(Value::as_i64);
            let command = unwrap_shell(text("command"));
            if !modern_style() {
                let (status, result) = match exit {
                    Some(0) => (Status::Ok, "→ done".to_owned()),
                    Some(code) => (Status::Error, format!("→ exit {code}")),
                    None => (Status::Run, "→ running".to_owned()),
                };
                return Some(exec_row(
                    colour,
                    status,
                    &format!("Ran {}", first_line(command)),
                    Some(&result),
                ));
            }
            // Only fields the item actually carries. Codex does not always
            // send the output, and a card that invented an empty output
            // region would claim the command printed nothing.
            let output = codex_output(item);
            Some(bash_box(
                terminal_width(),
                colour,
                command,
                &output,
                exit.map(|code| code as i32),
                codex_duration(item),
            ))
        }
        "file_change" => {
            let rows = item
                .get("changes")?
                .as_array()?
                .iter()
                .map(|change| {
                    let path = change.get("path").and_then(Value::as_str).unwrap_or("?");
                    let verb = match change.get("kind").and_then(Value::as_str) {
                        Some("add") => "Added",
                        Some("delete") => "Deleted",
                        _ => "Edited",
                    };
                    exec_row(colour, Status::Ok, &format!("{verb} {path}"), None)
                })
                .collect::<Vec<_>>();
            (!rows.is_empty()).then(|| rows.join("\n"))
        }
        "mcp_tool_call" if modern_style() => {
            let name = format!("{}.{}", text("server"), text("tool"));
            let detail = item
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let failed = matches!(item.get("status").and_then(Value::as_str), Some("failed"));
            Some(tool_box(
                terminal_width(),
                colour,
                "mcp.call",
                &name,
                detail,
                !failed,
                codex_duration(item).unwrap_or_default(),
            ))
        }
        "mcp_tool_call" => Some(exec_row(
            colour,
            item_status(item),
            &format!("{}.{}", text("server"), text("tool")),
            Some(
                item.pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| match item.get("status").and_then(Value::as_str) {
                        Some("in_progress") => "→ running",
                        Some("failed") => "→ failed",
                        _ => "→ done",
                    }),
            ),
        )),
        "web_search" => Some(exec_row(
            colour,
            Status::Ok,
            &format!("Searched {}", first_line(text("query"))),
            None,
        )),
        // The mockup shows the plan; this used to drop it on the floor, so a
        // Codex session's TODO list was invisible however long it ran.
        "todo_list" => codex_todo_block(item, colour),
        other => Some(exec_row(colour, item_status(item), other, None)),
    }
}

fn item_status(item: &Value) -> Status {
    match item.get("status").and_then(Value::as_str) {
        Some("failed") => Status::Error,
        Some("in_progress") => Status::Run,
        _ => Status::Ok,
    }
}
