//! The block pinned over the input while there is work on the list: the
//! checklist and the subagents running under the turn.
//!
//! The checklist used to be printed into the scrollback after every change,
//! so the current state was always somewhere above the last answer. This
//! block stays where the operator looks — over the input — and is rebuilt
//! from recorded state on every frame, so it is never ahead of what the
//! session holds. Each state has its own glyph, so it reads without colour.

use crate::{Line, Role, Style};

/// Where one checklist item stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkState {
    Done,
    /// Being worked on now.
    Active,
    Pending,
    /// Abandoned on purpose; kept so a shrunk list still says what went.
    Dropped,
}

impl WorkState {
    /// Whether anything is still expected to happen to an item.
    pub const fn is_open(self) -> bool {
        !matches!(self, Self::Done | Self::Dropped)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkItem {
    pub text: String,
    pub state: WorkState,
}

/// One subagent still running under the turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRow {
    pub role: String,
    pub goal: String,
    pub secs: u64,
}

/// What the block shows: the checklist under `label` (the approval mode, or
/// `DRAFT` for a plan not yet approved) and the running subagents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Worklist {
    pub label: String,
    pub items: Vec<WorkItem>,
    pub agents: Vec<AgentRow>,
}

impl Worklist {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.agents.is_empty()
    }
}

/// Checklist rows at most, the `+N more` row included, so the block cannot
/// push the input off a short terminal.
const TODO_ROWS: usize = 5;
const AGENT_ROWS: usize = 3;
const BAR: usize = 5;
const RAIL: &str = "  ▍ ";

/// The block's rows, or one summary row when `compact`.
pub fn worklist(list: &Worklist, width: usize, compact: bool) -> Vec<Line> {
    if compact {
        return summary(list, width).into_iter().collect();
    }
    let mut rows = Vec::new();
    if !list.items.is_empty() {
        rows.push(header(list, width));
        let (shown, hidden) = visible(&list.items);
        rows.extend(
            shown
                .into_iter()
                .map(|index| item_row(&list.items[index], width)),
        );
        if hidden > 0 {
            rows.push(more(hidden, width));
        }
    }
    if !list.agents.is_empty() {
        rows.push(
            rail()
                .push("AGENTS", Style::new(Role::Accent).bold())
                .push(format!("  {} running", list.agents.len()), Role::Dim)
                .fit(width),
        );
        let room = if list.agents.len() > AGENT_ROWS {
            AGENT_ROWS - 1
        } else {
            AGENT_ROWS
        };
        rows.extend(
            list.agents
                .iter()
                .take(room)
                .map(|agent| agent_row(agent, width)),
        );
        if list.agents.len() > room {
            rows.push(more(list.agents.len() - room, width));
        }
    }
    rows
}

fn rail() -> Line {
    Line::of(RAIL, Role::Accent)
}

fn done(items: &[WorkItem]) -> usize {
    items
        .iter()
        .filter(|item| item.state == WorkState::Done)
        .count()
}

/// `TODO  2/5  ▰▰▱▱▱` with the label set against the right edge.
fn header(list: &Worklist, width: usize) -> Line {
    let done = done(&list.items);
    let total = list.items.len();
    let filled = done * BAR / total;
    let left = rail()
        .push("TODO", Style::new(Role::Accent).bold())
        .push(format!("  {done}/{total}  "), Role::Dim)
        .push("▰".repeat(filled), Role::Ok)
        .push("▱".repeat(BAR - filled), Role::Dim);
    let label = Line::of(list.label.trim(), Role::Dim);
    if label.is_empty() || left.width() + 2 + label.width() > width {
        return left.fit(width);
    }
    let room = width - label.width();
    join(left.pad_to(room, Role::Plain), label)
}

fn join(mut left: Line, right: Line) -> Line {
    left.spans.extend(right.spans);
    left
}

/// Which items to draw, in list order, and how many open ones were left out.
///
/// A list that fits is drawn whole. Otherwise the open items come first —
/// the ones being worked before the ones still pending — with the last item
/// closed before them kept as context. When nothing is open, the last items
/// closed are what the list has to say.
fn visible(items: &[WorkItem]) -> (Vec<usize>, usize) {
    if items.len() <= TODO_ROWS {
        return ((0..items.len()).collect(), 0);
    }
    let open: Vec<usize> = (0..items.len())
        .filter(|index| items[*index].state.is_open())
        .collect();
    let Some(first) = open.first().copied() else {
        return (
            (items.len().saturating_sub(TODO_ROWS)..items.len()).collect(),
            0,
        );
    };
    let context = (0..first)
        .rev()
        .find(|index| !items[*index].state.is_open());
    let mut room = TODO_ROWS - usize::from(context.is_some());
    if open.len() > room {
        room -= 1;
    }
    let (current, queued): (Vec<usize>, Vec<usize>) = open
        .iter()
        .partition(|index| items[**index].state == WorkState::Active);
    let mut shown: Vec<usize> = current.into_iter().chain(queued).take(room).collect();
    let hidden = open.len() - shown.len();
    shown.extend(context);
    shown.sort_unstable();
    (shown, hidden)
}

fn item_row(item: &WorkItem, width: usize) -> Line {
    let (mark, mark_style, text_style) = match item.state {
        WorkState::Done => ("✓", Style::new(Role::Ok), Style::new(Role::Dim).strike()),
        WorkState::Active => (
            "▸",
            Style::new(Role::Accent).bold(),
            Style::new(Role::Assistant).bold(),
        ),
        WorkState::Pending => ("○", Style::new(Role::Dim), Style::new(Role::Dim)),
        WorkState::Dropped => ("✗", Style::new(Role::Dim), Style::new(Role::Dim).strike()),
    };
    rail()
        .push(mark, mark_style)
        .push(" ", Role::Plain)
        .push(first_line(&item.text), text_style)
        .fit(width)
}

/// `◆ explorer  goal…   12s`, the time against the right edge.
fn agent_row(agent: &AgentRow, width: usize) -> Line {
    let secs = elapsed(agent.secs);
    let left = rail()
        .push("◆ ", Role::Accent)
        .push(&agent.role, Role::Accent)
        .push("  ", Role::Plain);
    let room = width.saturating_sub(left.width() + secs.len() + 1);
    let goal = Line::of(first_line(&agent.goal), Role::Assistant).fit(room);
    let row = join(left, goal);
    if row.width() + 1 + secs.len() > width {
        return row.fit(width);
    }
    row.pad_to(width - secs.len(), Role::Plain)
        .push(secs, Role::Dim)
}

fn more(hidden: usize, width: usize) -> Line {
    rail()
        .push(format!("  +{hidden} more"), Role::Dim)
        .fit(width)
}

/// The one row a short terminal has room for.
fn summary(list: &Worklist, width: usize) -> Option<Line> {
    let mut row = rail();
    if !list.items.is_empty() {
        row = row.push("TODO", Style::new(Role::Accent).bold()).push(
            format!(" {}/{}", done(&list.items), list.items.len()),
            Role::Dim,
        );
        let next = list
            .items
            .iter()
            .find(|item| item.state == WorkState::Active)
            .map(|item| ("▸", item))
            .or_else(|| {
                list.items
                    .iter()
                    .find(|item| item.state.is_open())
                    .map(|item| ("○", item))
            });
        if let Some((mark, item)) = next {
            row = row
                .push(format!(" {mark} "), Role::Accent)
                .push(first_line(&item.text), Role::Assistant);
        }
    }
    if !list.agents.is_empty() {
        let lead = if list.items.is_empty() { "" } else { " · " };
        let count = list.agents.len();
        let noun = if count == 1 { "agent" } else { "agents" };
        row = row.push(format!("{lead}{count} {noun} running"), Role::Dim);
    }
    (!list.is_empty()).then(|| row.fit(width))
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default().trim()
}

fn elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str, state: WorkState) -> WorkItem {
        WorkItem {
            text: text.into(),
            state,
        }
    }

    fn texts(rows: &[Line]) -> Vec<String> {
        rows.iter().map(Line::text).collect()
    }

    #[test]
    fn each_state_has_its_own_glyph_and_a_finished_item_is_struck() {
        let list = Worklist {
            label: "DRAFT".into(),
            items: vec![
                item("fix the proof", WorkState::Done),
                item("wire the steps", WorkState::Active),
                item("add tests", WorkState::Pending),
            ],
            agents: Vec::new(),
        };
        let rows = worklist(&list, 60, false);
        assert_eq!(
            texts(&rows),
            [
                "  ▍ TODO  1/3  ▰▱▱▱▱                                   DRAFT",
                "  ▍ ✓ fix the proof",
                "  ▍ ▸ wire the steps",
                "  ▍ ○ add tests",
            ]
        );
        assert!(rows[1].spans.iter().any(|span| span.style.strike));
        assert!(!rows[2].spans.iter().any(|span| span.style.strike));

        let dropped = Worklist {
            label: String::new(),
            items: vec![
                item("old idea", WorkState::Dropped),
                item("new idea", WorkState::Pending),
            ],
            agents: Vec::new(),
        };
        let rows = worklist(&dropped, 60, false);
        assert_eq!(rows[1].text(), "  ▍ ✗ old idea");
        assert!(rows[1].spans.iter().any(|span| span.style.strike));
    }

    #[test]
    fn a_long_list_keeps_the_work_in_hand_and_counts_the_rest() {
        let mut items: Vec<WorkItem> = (0..3)
            .map(|n| item(&format!("old {n}"), WorkState::Done))
            .collect();
        items.extend((0..6).map(|n| item(&format!("next {n}"), WorkState::Pending)));
        items.push(item("late start", WorkState::Active));
        let list = Worklist {
            label: String::new(),
            items,
            agents: Vec::new(),
        };
        let rows = texts(&worklist(&list, 60, false));
        assert_eq!(
            rows,
            [
                "  ▍ TODO  3/10  ▰▱▱▱▱",
                "  ▍ ✓ old 2",
                "  ▍ ○ next 0",
                "  ▍ ○ next 1",
                "  ▍ ▸ late start",
                "  ▍   +4 more",
            ]
        );
    }

    #[test]
    fn a_finished_list_shows_what_was_finished_last() {
        let items = (0..7)
            .map(|n| item(&format!("step {n}"), WorkState::Done))
            .collect();
        let list = Worklist {
            label: "DRAFT".into(),
            items,
            agents: Vec::new(),
        };
        let rows = texts(&worklist(&list, 60, false));
        assert_eq!(rows.len(), 1 + TODO_ROWS);
        assert!(rows[0].contains("7/7  ▰▰▰▰▰"));
        assert_eq!(rows[1], "  ▍ ✓ step 2");
    }

    #[test]
    fn agents_show_their_role_goal_and_time_against_the_edge() {
        let list = Worklist {
            label: String::new(),
            items: Vec::new(),
            agents: (0..4)
                .map(|n| AgentRow {
                    role: "explorer".into(),
                    goal: format!("find caller {n}\nmore detail"),
                    secs: 59 + n,
                })
                .collect(),
        };
        let rows = texts(&worklist(&list, 40, false));
        assert_eq!(
            rows,
            [
                "  ▍ AGENTS  4 running",
                "  ▍ ◆ explorer  find caller 0        59s",
                "  ▍ ◆ explorer  find caller 1      1m00s",
                "  ▍   +2 more",
            ]
        );
    }

    #[test]
    fn every_row_fits_the_width_it_was_given() {
        let list = Worklist {
            label: "DRAFT".into(),
            items: vec![item(&"long ".repeat(40), WorkState::Active)],
            agents: vec![AgentRow {
                role: "reviewer".into(),
                goal: "word ".repeat(40),
                secs: 3,
            }],
        };
        let widest = |width: usize, compact: bool| {
            worklist(&list, width, compact)
                .iter()
                .map(Line::width)
                .max()
                .unwrap_or(0)
        };
        for width in [12, 24, 80] {
            assert!(widest(width, false) <= width, "full block at {width}");
            assert!(widest(width, true) <= width, "summary row at {width}");
        }
    }

    #[test]
    fn a_short_terminal_gets_one_summary_row() {
        let list = Worklist {
            label: "DRAFT".into(),
            items: vec![
                item("done", WorkState::Done),
                item("doing", WorkState::Active),
            ],
            agents: vec![AgentRow {
                role: "explorer".into(),
                goal: "look".into(),
                secs: 1,
            }],
        };
        assert_eq!(
            texts(&worklist(&list, 80, true)),
            ["  ▍ TODO 1/2 ▸ doing · 1 agent running"]
        );
        assert!(worklist(&Worklist::default(), 80, true).is_empty());
        assert!(worklist(&Worklist::default(), 80, false).is_empty());
    }
}
