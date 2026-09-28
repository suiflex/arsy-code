//! The live row a context compaction draws while it runs.
//!
//! A compaction happens between the operator's prompt and the model's first
//! word, and a turn that goes quiet there reads as a hang. This row says what
//! is being done, how far along it is, and how much there was to fit.

use crate::{Line, Role};

/// The steps a compaction goes through, in the order it takes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionStep {
    /// Measuring the conversation against the budget.
    Measuring,
    /// Replacing stale tool results with stubs.
    Eliding,
    /// Folding the older dialogue into one summary.
    Folding,
    /// Writing the compaction to the session.
    Recording,
}

impl CompactionStep {
    pub const ALL: [Self; 4] = [
        Self::Measuring,
        Self::Eliding,
        Self::Folding,
        Self::Recording,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Measuring => "measuring the context",
            Self::Eliding => "eliding stale tool results",
            Self::Folding => "folding earlier messages",
            Self::Recording => "recording the compaction",
        }
    }

    /// One-based position among [`Self::ALL`].
    fn number(self) -> usize {
        Self::ALL.iter().position(|step| *step == self).unwrap_or(0) + 1
    }
}

const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// The row for a compaction at `step`: a spinner frame, a bar of the steps
/// taken, the step's name, and the tokens there were to fit — against
/// `budget` when the budget forced it, alone when `/compact` asked.
pub fn compaction_progress(
    step: CompactionStep,
    tokens: u32,
    budget: Option<u32>,
    frame: usize,
    width: usize,
) -> Line {
    let done = step.number();
    let total = CompactionStep::ALL.len();
    let size = match budget {
        Some(budget) => format!(" · {tokens} tokens over a {budget} budget"),
        None => format!(" · {tokens} tokens"),
    };
    Line::of(FRAMES[frame % FRAMES.len()], Role::Accent)
        .push(" CONTEXT ", Role::Dim)
        .push("compacting ", Role::Accent)
        .push("▰".repeat(done), Role::Ok)
        .push("▱".repeat(total - done), Role::Dim)
        .push(format!(" {done}/{total} "), Role::Dim)
        .push(step.label(), Role::Assistant)
        .push(size, Role::Dim)
        .fit(width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_row_names_the_step_and_how_far_along_it_is() {
        let row = compaction_progress(CompactionStep::Folding, 91_204, Some(64_000), 0, 120);
        assert_eq!(
            row.text(),
            "⠋ CONTEXT compacting ▰▰▰▱ 3/4 folding earlier messages · 91204 tokens over a \
             64000 budget"
        );
        let manual = compaction_progress(CompactionStep::Recording, 500, None, 9, 120);
        assert!(manual.text().starts_with("⠙ CONTEXT compacting ▰▰▰▰ 4/4"));
        assert!(manual.text().ends_with("· 500 tokens"));
    }

    #[test]
    fn the_row_fits_a_narrow_terminal() {
        let row = compaction_progress(CompactionStep::Eliding, 91_204, Some(64_000), 0, 30);
        assert!(row.width() <= 30);
    }
}
