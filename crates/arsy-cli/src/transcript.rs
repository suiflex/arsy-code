//! What a turn said, written down so the next process can read it back.
//!
//! # Why the prompt alone was not enough
//!
//! A session used to record the operator's prompt and nothing else, so
//! resuming replayed the questions and none of the answers. The model then
//! re-derived work it had already done, re-read files it had already read, and
//! contradicted conclusions it had reached in the turn before — all of which
//! looked like the model being unreliable and was the harness forgetting.
//!
//! So a completed turn records its whole exchange: the prompt, the assistant's
//! replies, the tool calls it made, and the results it was given.
//!
//! # Why it is compacted rather than stored whole
//!
//! A single `bash` result can be sixteen kilobytes. A dozen turns of those is
//! a transcript no context window would accept, so a long result keeps its
//! head and its tail with the middle elided. The tail is the part that matters
//! twice over: a failing command says why at the end, and `bash` ends its
//! result with the `evidence: <id>` line that names the artifact holding the
//! whole thing. Nothing is lost — it is addressable.
//!
//! # Why a turn that did not finish contributes nothing
//!
//! An interrupted or failed turn never writes a transcript, so resuming does
//! not replay a question the model never answered or a tool call whose result
//! it never saw. A half-turn in the history is worse than no turn: it reads as
//! something that happened.

use arsy_kernel::provider::{ModelContent, ModelMessage};
use serde_json::Value;

/// The most one tool result may contribute to a persisted transcript.
///
/// Half of what a live result may be, because a transcript holds many of them
/// and the live one is already bounded for the turn that saw it.
const MAX_RESULT_BYTES: usize = 4 * 1024;

/// Leave room for the `turn_id` wrapper and JSON escaping under the event
/// store's 64 KiB inline payload limit. A turn past this keeps its earliest
/// messages that fit and drops the tail of its tool traffic, which is the part
/// still recoverable from artifacts. An oversized first message is already
/// present in `turn.started` if it is the prompt.
const MAX_TURN_BYTES: usize = 48 * 1024;

/// The turn's exchange, compacted, as the value recorded on `turn.completed`.
///
/// Reasoning state is left out: it is opaque, often several KiB of encrypted
/// text, and a provider reads it only within the turn that produced it —
/// recorded here it would only crowd out the tool traffic a resume needs.
pub fn persistable(messages: &[ModelMessage]) -> Value {
    let mut kept: Vec<ModelMessage> = Vec::with_capacity(messages.len());
    let mut budget = MAX_TURN_BYTES;
    for message in messages {
        let compacted = ModelMessage {
            role: message.role,
            content: message
                .content
                .iter()
                .filter(|item| !matches!(item, ModelContent::Reasoning { .. }))
                .map(compact)
                .collect(),
        };
        let cost = serde_json::to_vec(&compacted).map_or(usize::MAX, |bytes| bytes.len());
        if cost > budget {
            if let Some(last) = kept.last_mut() {
                last.content
                    .retain(|item| !matches!(item, ModelContent::ToolCall { .. }));
            }
            if kept.last().is_some_and(|last| last.content.is_empty()) {
                kept.pop();
            }
            break;
        }
        budget = budget.saturating_sub(cost);
        kept.push(compacted);
    }
    serde_json::to_value(kept).unwrap_or(Value::Null)
}

/// Read a recorded transcript back, ignoring anything that no longer parses.
///
/// A stream written by an older build has no transcript at all, and a
/// malformed one is history that cannot be trusted as history: either way the
/// answer is "this turn contributes nothing", not "this session cannot open".
pub fn restore(recorded: &Value) -> Vec<ModelMessage> {
    serde_json::from_value(recorded.clone()).unwrap_or_default()
}

fn compact(content: &ModelContent) -> ModelContent {
    match content {
        ModelContent::ToolResult {
            id,
            content,
            is_error,
        } => ModelContent::ToolResult {
            id: id.clone(),
            content: elide(content),
            is_error: *is_error,
        },
        other => other.clone(),
    }
}

/// Keep both ends of a long result.
///
/// The head says what the call was and how it started; the tail carries the
/// failure and the `evidence:` line that names where the whole output lives.
/// Cutting only the head would throw away the id; cutting only the tail would
/// throw away the error.
fn elide(text: &str) -> String {
    if text.len() <= MAX_RESULT_BYTES {
        return text.to_owned();
    }
    let half = MAX_RESULT_BYTES / 2;
    let head = boundary_before(text, half);
    let tail = boundary_after(text, text.len() - half);
    format!(
        "{}\n[{} bytes elided; the full result is in this turn's evidence]\n{}",
        &text[..head],
        text.len() - head - (text.len() - tail),
        &text[tail..]
    )
}

fn boundary_before(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn boundary_after(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::provider::ModelRole;

    fn text(role: ModelRole, body: &str) -> ModelMessage {
        ModelMessage {
            role,
            content: vec![ModelContent::Text {
                text: body.to_owned(),
            }],
        }
    }

    #[test]
    fn a_turns_calls_and_results_survive_being_written_down_and_read_back() {
        let exchange = vec![
            text(ModelRole::User, "fix the failing test"),
            ModelMessage {
                role: ModelRole::Assistant,
                content: vec![
                    ModelContent::Text {
                        text: "running it first".to_owned(),
                    },
                    ModelContent::ToolCall {
                        id: "call-1".to_owned(),
                        name: "bash".to_owned(),
                        arguments: serde_json::json!({"command": "cargo test"}),
                    },
                ],
            },
            ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::ToolResult {
                    id: "call-1".to_owned(),
                    content: "1 failed\n\nevidence: art-1".to_owned(),
                    is_error: true,
                }],
            },
            text(ModelRole::Assistant, "the assertion is inverted"),
        ];

        let restored = restore(&persistable(&exchange));
        assert_eq!(
            restored, exchange,
            "an exchange that fits is recorded exactly"
        );
    }

    /// The head says what ran; the tail carries the failure and the evidence
    /// id. A compaction that kept only one of them would lose the other.
    #[test]
    fn a_long_result_keeps_both_ends_and_names_where_the_rest_is() {
        let body = format!(
            "starting the suite\n{}\nassertion failed at line 9\n\nevidence: art-42",
            "x".repeat(MAX_RESULT_BYTES * 2)
        );
        let exchange = vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::ToolResult {
                id: "call-1".to_owned(),
                content: body.clone(),
                is_error: true,
            }],
        }];

        let restored = restore(&persistable(&exchange));
        let ModelContent::ToolResult { content, .. } = &restored[0].content[0] else {
            panic!("a tool result is restored as a tool result");
        };
        assert!(content.len() < body.len());
        assert!(content.starts_with("starting the suite"), "{content}");
        assert!(content.ends_with("evidence: art-42"), "{content}");
        assert!(content.contains("bytes elided"), "{content}");
    }

    #[test]
    fn a_turn_past_the_budget_keeps_its_earliest_messages() {
        let exchange = vec![
            text(ModelRole::User, "the question"),
            text(ModelRole::Assistant, &"y".repeat(MAX_TURN_BYTES / 2)),
            // On its own larger than the whole budget, so it cannot follow.
            text(ModelRole::Assistant, &"z".repeat(MAX_TURN_BYTES)),
        ];
        let restored = restore(&persistable(&exchange));
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0], exchange[0], "the prompt is never the part cut");
    }

    #[test]
    fn a_long_turn_fits_in_one_inline_event() {
        let exchange = (0..100)
            .map(|_| text(ModelRole::Assistant, &"x".repeat(1024)))
            .collect::<Vec<_>>();
        let payload = serde_json::json!({"turn_id": "00000000-0000-0000-0000-000000000000", "transcript": persistable(&exchange)});
        assert!(
            serde_json::to_vec(&payload).unwrap().len()
                <= arsy_kernel::event::MAX_INLINE_EVENT_BYTES
        );
        assert!(restore(&payload["transcript"]).len() < exchange.len());
    }

    #[test]
    fn an_oversized_first_message_does_not_overflow_the_event() {
        let exchange = vec![text(ModelRole::User, &"x".repeat(MAX_TURN_BYTES + 1))];
        let payload = serde_json::json!({"turn_id": "00000000-0000-0000-0000-000000000000", "transcript": persistable(&exchange)});
        assert!(restore(&payload["transcript"]).is_empty());
        assert!(
            serde_json::to_vec(&payload).unwrap().len()
                <= arsy_kernel::event::MAX_INLINE_EVENT_BYTES
        );
    }

    #[test]
    fn cutting_a_result_also_removes_its_unanswered_call() {
        let exchange = vec![
            text(ModelRole::User, "inspect the failure"),
            text(ModelRole::Assistant, &"x".repeat(MAX_TURN_BYTES - 2000)),
            ModelMessage {
                role: ModelRole::Assistant,
                content: vec![
                    ModelContent::Text {
                        text: "checking".to_owned(),
                    },
                    ModelContent::ToolCall {
                        id: "call-1".to_owned(),
                        name: "bash".to_owned(),
                        arguments: serde_json::json!({"command": "cargo test"}),
                    },
                ],
            },
            ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::ToolResult {
                    id: "call-1".to_owned(),
                    content: "z".repeat(MAX_RESULT_BYTES),
                    is_error: false,
                }],
            },
        ];
        let restored = restore(&persistable(&exchange));
        assert_eq!(restored.len(), 3);
        assert_eq!(restored[2], text(ModelRole::Assistant, "checking"));
    }

    /// A stream from a build that recorded no transcript, and a corrupted one,
    /// both contribute nothing rather than failing the session.
    #[test]
    fn an_absent_or_unreadable_transcript_is_empty_not_fatal() {
        assert!(restore(&Value::Null).is_empty());
        assert!(restore(&serde_json::json!("not a transcript")).is_empty());
        assert!(restore(&serde_json::json!([{"role": "wizard"}])).is_empty());
    }

    #[test]
    fn reasoning_state_is_not_recorded() {
        let exchange = vec![ModelMessage {
            role: ModelRole::Assistant,
            content: vec![
                ModelContent::Reasoning {
                    state: serde_json::json!({"payload": "x".repeat(4096)}),
                },
                ModelContent::Text {
                    text: "kept".to_owned(),
                },
            ],
        }];
        let restored = restore(&persistable(&exchange));
        assert_eq!(
            restored[0].content,
            vec![ModelContent::Text {
                text: "kept".to_owned()
            }]
        );
    }
}
