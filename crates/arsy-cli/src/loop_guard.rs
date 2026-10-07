//! Notice a model that is stuck repeating tool calls that fail, and tell it
//! before stopping it.
//!
//! Two shapes of stuck. The same call failing round after round is a model
//! retrying something that will not work. Every call failing round after
//! round — different calls, all refused by policy or all erroring — is a
//! model that has not noticed its tools are not working. Either way the first
//! sign gets a note on the round's results telling it to change approach, and
//! only a model that carries on regardless is stopped. A round in which any
//! call succeeded is progress and resets both counts, so a model that is
//! getting somewhere is never interrupted.

use arsy_kernel::provider::ModelContent;
use serde_json::Value;

/// Identical failures in a row: warned at the second, stopped at this.
const IDENTICAL_FAILURE_LIMIT: usize = 3;
/// Rounds in a row with every call failing: warned at the warning, stopped
/// at the limit.
const FAILING_ROUNDS_WARNING: usize = 3;
const FAILING_ROUNDS_LIMIT: usize = 6;

/// What a round's results mean for the turn.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Continue,
    /// Keep going, with this note added to the round's results.
    Redirect(String),
    /// End the turn, for this reason.
    Stop(String),
}

#[derive(Debug, Default)]
pub(crate) struct LoopGuard {
    /// The last failing call and how many rounds in a row it failed.
    identical: Option<(String, usize)>,
    failing_rounds: usize,
}

impl LoopGuard {
    /// Judge one round from its calls and their results, paired by position.
    pub(crate) fn observe(
        &mut self,
        calls: &[(String, String, Value)],
        results: &[ModelContent],
    ) -> Verdict {
        let failed = |result: &ModelContent| {
            matches!(result, ModelContent::ToolResult { is_error: true, .. })
        };
        if calls.is_empty() || !results.iter().all(failed) {
            *self = Self::default();
            return Verdict::Continue;
        }
        self.failing_rounds += 1;
        let first = fingerprint(&calls[0].1, &calls[0].2);
        let count = match self.identical.take() {
            Some((last, count)) if last == first => count + 1,
            _ => 1,
        };
        self.identical = Some((first, count));

        if count >= IDENTICAL_FAILURE_LIMIT {
            return Verdict::Stop(format!(
                "the model repeated the same failing tool call {count} times — it is stuck in a \
                 loop rather than out of budget; continue with a narrower task"
            ));
        }
        if self.failing_rounds >= FAILING_ROUNDS_LIMIT {
            return Verdict::Stop(format!(
                "every tool call failed for {} rounds in a row — the tools this turn needs are \
                 not working or not allowed; check the errors above, then continue",
                self.failing_rounds
            ));
        }
        if count == IDENTICAL_FAILURE_LIMIT - 1 {
            return Verdict::Redirect(
                "[SYSTEM: this exact call has now failed twice. Do not repeat it: read the error, \
                 then change the arguments, use a different tool, or explain what is blocking you.]"
                    .to_owned(),
            );
        }
        if self.failing_rounds == FAILING_ROUNDS_WARNING {
            return Verdict::Redirect(format!(
                "[SYSTEM: every tool call in the last {FAILING_ROUNDS_WARNING} rounds failed or was \
                 refused. Stop retrying the same way: work with what you already have, or say \
                 what is blocking you and what you need.]"
            ));
        }
        Verdict::Continue
    }
}

/// Whether a turn failure is one this guard raised, so it is reported as the
/// harness stopping a stuck model and not as a provider fault.
pub(crate) fn stopped_it(message: &str) -> bool {
    message.contains("repeated the same failing tool call")
        || message.contains("every tool call failed for")
}

/// Add a redirect note to the last result in a round, where the model reads it.
pub(crate) fn attach_note(results: &mut [ModelContent], note: &str) {
    if let Some(ModelContent::ToolResult { content, .. }) = results.last_mut() {
        content.push_str("\n\n");
        content.push_str(note);
    }
}

/// What makes two calls the same call.
pub(crate) fn fingerprint(name: &str, arguments: &Value) -> String {
    // A timeout is execution metadata, not command identity. Otherwise a
    // provider can evade duplicate protection by changing only the deadline.
    let identity = if name == "bash" {
        arguments.get("command").cloned().unwrap_or(Value::Null)
    } else {
        arguments.clone()
    };
    format!("{name}\0{identity}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, path: &str) -> (String, String, Value) {
        ("id".to_owned(), name.to_owned(), json!({ "path": path }))
    }

    fn result(failed: bool) -> ModelContent {
        ModelContent::ToolResult {
            id: "id".to_owned(),
            content: "out".to_owned(),
            is_error: failed,
        }
    }

    #[test]
    fn the_same_failing_call_is_warned_once_then_stopped() {
        let mut guard = LoopGuard::default();
        let calls = [call("fs.read", "missing.rs")];
        assert_eq!(guard.observe(&calls, &[result(true)]), Verdict::Continue);
        assert!(matches!(
            guard.observe(&calls, &[result(true)]),
            Verdict::Redirect(note) if note.contains("failed twice")
        ));
        assert!(matches!(
            guard.observe(&calls, &[result(true)]),
            Verdict::Stop(reason) if reason.contains("same failing tool call 3 times")
        ));
    }

    #[test]
    fn different_calls_that_all_fail_are_warned_then_stopped() {
        // A model whose every call is refused, trying a new one each round.
        let mut guard = LoopGuard::default();
        let mut verdicts = Vec::new();
        for round in 0..FAILING_ROUNDS_LIMIT {
            verdicts.push(guard.observe(&[call("fs.read", &format!("f{round}"))], &[result(true)]));
        }
        assert_eq!(verdicts[0], Verdict::Continue);
        assert!(matches!(&verdicts[2], Verdict::Redirect(note) if note.contains("last 3 rounds")));
        assert!(matches!(&verdicts[5], Verdict::Stop(reason) if reason.contains("6 rounds")));
    }

    #[test]
    fn one_call_that_works_is_progress_and_resets_the_count() {
        let mut guard = LoopGuard::default();
        let calls = [call("fs.read", "missing.rs")];
        guard.observe(&calls, &[result(true)]);
        guard.observe(&calls, &[result(true)]);
        let mixed = [call("fs.read", "missing.rs"), call("fs.list", ".")];
        assert_eq!(
            guard.observe(&mixed, &[result(true), result(false)]),
            Verdict::Continue
        );
        assert_eq!(guard.observe(&calls, &[result(true)]), Verdict::Continue);
    }

    #[test]
    fn its_own_stops_are_recognised_and_nothing_else_is() {
        let mut guard = LoopGuard::default();
        let calls = [call("fs.read", "missing.rs")];
        let stop = (0..3)
            .map(|_| guard.observe(&calls, &[result(true)]))
            .last()
            .unwrap();
        let Verdict::Stop(reason) = stop else {
            panic!("three identical failures stop the turn");
        };
        assert!(stopped_it(&reason));
        assert!(!stopped_it("provider server error 500"));
    }

    #[test]
    fn a_note_rides_on_the_last_result() {
        let mut results = vec![result(true), result(true)];
        attach_note(&mut results, "[SYSTEM: change approach]");
        assert!(matches!(
            &results[1],
            ModelContent::ToolResult { content, .. } if content.ends_with("[SYSTEM: change approach]")
        ));
        assert!(
            matches!(&results[0], ModelContent::ToolResult { content, .. } if content == "out")
        );
    }

    #[test]
    fn a_bash_timeout_does_not_make_a_command_a_new_call() {
        assert_eq!(
            fingerprint("bash", &json!({"command": "make", "timeout": 10})),
            fingerprint("bash", &json!({"command": "make", "timeout": 99}))
        );
    }
}
