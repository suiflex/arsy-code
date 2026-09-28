//! The session side of the TUI: loading the workspace's recorded sessions,
//! resuming one, and rebuilding the conversation it had.

#[cfg(feature = "tui")]
use crate::*;
use arsy_kernel::domain::SessionId;
use serde_json::Value;
use std::path::Path;

/// The title the store holds for one session, which the listing below
/// leaves out while that session has recorded no turn.
pub(crate) fn stored_session_title(workspace: &Path, session: SessionId) -> Option<String> {
    open_store(workspace).ok()?.session_title(session).ok()?
}

pub(crate) fn load_workspace_sessions(workspace: &Path) -> Vec<tui::SessionChoice> {
    let Ok(store) = open_store(workspace) else {
        return Vec::new();
    };
    let Ok(summaries) = store.sessions(30) else {
        return Vec::new();
    };
    summaries
        .into_iter()
        .map(|s| {
            let ts = s.last_event_at_ms.or(s.started_at_ms).unwrap_or_default();
            let last_seen = if ts > 0 {
                let now = arsy_kernel::artifact::unix_time_ms();
                let diff_secs = now.saturating_sub(ts) / 1000;
                if diff_secs < 60 {
                    "just now".to_owned()
                } else if diff_secs < 3600 {
                    format!("{}m ago", diff_secs / 60)
                } else if diff_secs < 86400 {
                    format!("{}h ago", diff_secs / 3600)
                } else {
                    format!("{}d ago", diff_secs / 86400)
                }
            } else {
                "recorded".to_owned()
            };
            tui::SessionChoice {
                id: s.session,
                title: s.title,
                events: s.version.0,
                last_seen,
            }
        })
        .collect()
}

#[cfg(feature = "tui")]
/// The conversation a resumed session continues from.
///
/// Built from completed turns only. A turn that failed or was interrupted
/// wrote no completion, so its prompt is not replayed: a question the model
/// never answered, restored as history, reads as something that happened and
/// is worse than a gap.
///
/// Each completed turn contributes the exchange it recorded — prompt,
/// replies, tool calls, tool results — or, for a stream written before
/// transcripts existed, whatever the two ends of it can be reconstructed from.
pub(crate) fn reconstruct_session_conversation(
    workspace: &Path,
    session: SessionId,
) -> (Vec<ModelMessage>, arsy_code::agent::budget::History) {
    let mut history = arsy_code::agent::budget::History::default();
    let Ok(store) = open_store(workspace) else {
        return (Vec::new(), history);
    };
    // Read in pages: a long session outgrows any one page, and a cut-off
    // read would resume it without its latest turns.
    const PAGE: usize = 1000;
    let mut events = Vec::new();
    loop {
        let from = events
            .last()
            .map_or(1, |event: &arsy_kernel::event::EventEnvelope| {
                event.sequence + 1
            });
        let Ok(page) = store.read(session, from, PAGE) else {
            return (Vec::new(), history);
        };
        let full = page.len() == PAGE;
        events.extend(page);
        if !full {
            break;
        }
    }
    let inline = |event: &arsy_kernel::event::EventEnvelope| {
        let arsy_kernel::event::EventPayload::Inline { data } = &event.payload else {
            return None;
        };
        Some(data.clone())
    };
    let turn_of = |data: &Value| {
        data.get("turn_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    // Two passes, because a transcript is written just before its turn is
    // closed and a turn that never closed must contribute nothing. One pass
    // could not know, at the transcript, whether the completion would come.
    let mut completed = std::collections::HashSet::new();
    let mut prompts: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for event in &events {
        let Some(data) = inline(event) else { continue };
        match event.kind.as_str() {
            "turn.completed" => {
                completed.insert(turn_of(&data));
            }
            "turn.started" => {
                if let Some(prompt) = data.get("prompt").and_then(Value::as_str) {
                    prompts.insert(turn_of(&data), prompt.to_owned());
                }
            }
            _ => {}
        }
    }

    let mut messages = Vec::new();
    let mut transcribed = std::collections::HashSet::new();
    for event in &events {
        let Some(data) = inline(event) else { continue };
        let turn = turn_of(&data);
        if !completed.contains(&turn) {
            continue;
        }
        match event.kind.as_str() {
            "turn.transcript" => {
                let recorded = transcript::restore(data.get("transcript").unwrap_or(&Value::Null));
                if !recorded.is_empty() {
                    transcribed.insert(turn);
                    messages.extend(recorded);
                    // What a later compaction of this prefix would cite: the
                    // event that holds the exchange verbatim.
                    history.citations.push(arsy_kernel::context::EventCitation {
                        id: event.id,
                        sequence: event.sequence,
                    });
                }
            }
            // A stream written before transcripts existed, or one whose
            // transcript did not survive. The question it was asked is what it
            // has, and it is better than nothing.
            "turn.completed" if !transcribed.contains(&turn) => {
                if let Some(prompt) = prompts.remove(&turn) {
                    messages.push(ModelMessage {
                        role: ModelRole::User,
                        content: vec![ModelContent::Text { text: prompt }],
                    });
                }
            }
            _ => {}
        }
    }
    (messages, history)
}

/// Put a restored conversation back on screen: each prompt, each answer, and
/// each tool call as the card it ran as.
///
/// The conversation is what the model will be given, so drawing from it shows
/// the operator exactly what the resumed session remembers. Thinking and todo
/// blocks are not part of it and are not replayed.
pub(crate) fn replay_into(transcript: &mut tui::Transcript, messages: &[ModelMessage]) {
    let mut calls: std::collections::HashMap<&str, (&str, String)> =
        std::collections::HashMap::new();
    let contents = messages.iter().flat_map(|message| {
        message
            .content
            .iter()
            .map(move |content| (message.role, content))
    });
    for (role, content) in contents {
        match (role, content) {
            (ModelRole::User, ModelContent::Text { text }) => transcript.push_user(text),
            (ModelRole::Assistant, ModelContent::Text { text }) => transcript.push_assistant(text),
            (
                _,
                ModelContent::ToolCall {
                    id,
                    name,
                    arguments,
                },
            ) => {
                let summary = arsy_code::agent::tool(name)
                    .map_or_else(|| name.clone(), |tool| tool.summary(arguments));
                calls.insert(id, (name, summary));
            }
            (
                _,
                ModelContent::ToolResult {
                    id,
                    content,
                    is_error,
                },
            ) => {
                if let Some((name, summary)) = calls.remove(id.as_str()) {
                    transcript.push_tool(
                        name,
                        &summary,
                        content,
                        !is_error,
                        std::time::Duration::ZERO,
                    );
                }
            }
            _ => {}
        }
    }
}

/// The providers configured right now, in the order the configuration lists
/// them. Read fresh each time `/provider` opens, so an edit made outside ARSY
/// is not hidden behind a stale list.
#[cfg(feature = "tui")]
pub(crate) fn configured_providers(invocation: &Invocation) -> Vec<String> {
    crate::provider::configuration(invocation)
        .map(|config| config.endpoint_ids())
        .unwrap_or_default()
}

#[cfg(all(test, feature = "tui"))]
mod tests {
    use super::*;

    /// A resumed conversation is drawn as it ran: the prompt, the tool card
    /// with the result it got, and the answer after it.
    #[test]
    fn a_resumed_conversation_is_drawn_again() {
        let text = |role, text: &str| ModelMessage {
            role,
            content: vec![ModelContent::Text {
                text: text.to_owned(),
            }],
        };
        let messages = vec![
            text(ModelRole::User, "list the crates"),
            ModelMessage {
                role: ModelRole::Assistant,
                content: vec![ModelContent::ToolCall {
                    id: "call-1".to_owned(),
                    name: "shell".to_owned(),
                    arguments: serde_json::json!({ "command": "ls crates" }),
                }],
            },
            ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::ToolResult {
                    id: "call-1".to_owned(),
                    content: "arsy-kernel\narsy-cli".to_owned(),
                    is_error: false,
                }],
            },
            text(ModelRole::Assistant, "Two crates: kernel and cli."),
        ];
        let mut transcript = tui::Transcript::default();
        replay_into(&mut transcript, &messages);
        let mut screen = Vec::new();
        let state = tui::TuiState::new("/tmp/w".to_owned(), SessionId::new());
        transcript.repaint(&mut screen, 100, false, &state).unwrap();
        let screen = String::from_utf8(screen).unwrap();
        for expected in ["list the crates", "arsy-cli", "Two crates: kernel and cli."] {
            assert!(
                screen.contains(expected),
                "{expected} missing from {screen}"
            );
        }
        let prompt = screen.find("list the crates").unwrap();
        let answer = screen.find("Two crates").unwrap();
        assert!(prompt < answer, "{screen}");
    }
}
