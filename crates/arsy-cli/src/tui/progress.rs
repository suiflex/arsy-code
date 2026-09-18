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
        TranscriptEntry::ModeChange { from, to } => writeln!(
            terminal,
            "{} {} {}",
            paint(colour, sgr_dim(), "  MODE"),
            paint(colour, sgr_accent(), from),
            paint(colour, sgr_ok(), &format!("→ {to}")),
        ),
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
        return format!(
            "{} {}",
            paint(colour, sgr_border(), "  │"),
            paint(colour, sgr_accent(), "✻ Thinking")
        );
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
        return format!(
            "{} {}",
            paint(colour, sgr_border(), "  │"),
            paint(colour, sgr_dim(), text.trim_end())
        );
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
        return paint(colour, sgr_border(), "  ╰");
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
    rows.push(thinking_box_bottom(width, colour));
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
        let mut rows = vec![paint(colour, sgr_assistant(), "  ◂ Response")];
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

fn modern_tool_card(
    colour: bool,
    title: &str,
    detail: &str,
    status: &str,
    status_colour: &str,
) -> String {
    format!(
        "{} {}\n{} {}\n{} {}",
        paint(colour, sgr_accent(), "  │"),
        paint(colour, sgr_accent(), title),
        paint(colour, sgr_accent(), "  │"),
        paint(colour, sgr_dim(), detail),
        paint(colour, sgr_accent(), "  ╰"),
        paint(colour, status_colour, status),
    )
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
            sgr_run(),
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
        return modern_tool_card(colour, &format!("⚙ {name}"), summary, "running…", sgr_run());
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
            if ok { sgr_ok() } else { sgr_err() },
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

fn render_codex_item(item: &Value, colour: bool) -> Option<String> {
    let text = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default();
    match item.get("type")?.as_str()? {
        "agent_message" => Some(paint(colour, sgr_assistant(), text("text").trim())),
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
            let (status, result) = match exit {
                Some(0) => (Status::Ok, "→ done".to_owned()),
                Some(code) => (Status::Error, format!("→ exit {code}")),
                None => (Status::Run, "→ running".to_owned()),
            };
            Some(exec_row(
                colour,
                status,
                &format!("Ran {}", first_line(unwrap_shell(text("command")))),
                Some(&result),
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
        // todo_list has no brainless row; unknown kinds still get a dim marker
        // so a codex upgrade never renders as silence.
        "todo_list" => None,
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
