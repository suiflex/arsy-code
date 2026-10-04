//! A title for a new session, so it is found again by what it was about
//! rather than by its id.
//!
//! The first line of the first prompt is written as soon as the first turn
//! answers. Under `ui.session_title = "model"` the session's own model is
//! then asked for a shorter one on a background thread, and that replaces the
//! first title only if nothing renamed the session in the meantime.

use arsy_kernel::domain::SessionId;
use arsy_kernel::provider::{
    CanonicalModelRequest, ModelContent, ModelEvent, ModelKey, ModelMessage, ModelProvider,
    ModelRole,
};
use std::sync::Arc;
use unicode_width::UnicodeWidthChar;

/// The widest title kept, in columns.
const MAX_COLUMNS: usize = 60;

/// How much of the first answer the model is shown: enough to say what the
/// session was about, not the whole of it.
const ANSWER_CHARS: usize = 2_000;

const INSTRUCTION: &str = "Write a title of at most six words for this conversation. \
     Reply with the title only: no quotes, no trailing punctuation.";

/// The model asked for a title, and the turn the request belongs to.
pub(crate) struct TitleModel {
    pub provider: Arc<dyn ModelProvider>,
    pub model: ModelKey,
    pub turn: String,
}

/// Title a session that has none, after its first answer.
///
/// Nothing here is allowed to fail a turn: a store that cannot be opened or a
/// model that does not answer leaves the session as it was. The model's
/// thread is handed back only so a test can wait for it; a turn never does.
pub(crate) fn title_new_session(
    store: Arc<arsy_kernel::sqlite::SqliteEventStore>,
    session: SessionId,
    mode: &str,
    prompt: &str,
    answer: &str,
    model: Option<TitleModel>,
) -> Option<std::thread::JoinHandle<()>> {
    if mode == "off" || !matches!(store.session_title(session), Ok(None)) {
        return None;
    }
    let fallback = from_prompt(prompt)?;
    if store.set_session_title(session, &fallback).is_err() || mode != "model" {
        return None;
    }
    let model = model?;
    let request = request(&model, prompt, answer);
    Some(std::thread::spawn(move || {
        let Some(title) = request.and_then(|request| ask(model.provider.as_ref(), &request)) else {
            return;
        };
        // A `/rename` made while the model answered is the operator's choice
        // and stays: the swap happens only while the title is still the
        // fallback, checked and written in one store operation.
        let _ = store.replace_session_title(session, &fallback, &title);
    }))
}

/// The first line of `prompt` that says anything, as a title.
pub(crate) fn from_prompt(prompt: &str) -> Option<String> {
    prompt.lines().find_map(clean)
}

/// `raw` as a single-line title no wider than [`MAX_COLUMNS`], or `None`
/// when nothing printable is left.
pub(crate) fn clean(raw: &str) -> Option<String> {
    let line = raw.lines().find(|line| !line.trim().is_empty())?;
    let words: Vec<&str> = line.split_whitespace().collect();
    let joined = words.join(" ");
    let trimmed = joined
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*' | '#'))
        .trim_end_matches(['.', ':'])
        .trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut title = String::new();
    let mut used = 0;
    for c in trimmed.chars().filter(|c| !c.is_control()) {
        let width = c.width().unwrap_or(0);
        if used + width > MAX_COLUMNS - 1 {
            title.push('…');
            return Some(title);
        }
        used += width;
        title.push(c);
    }
    Some(title)
}

fn request(model: &TitleModel, prompt: &str, answer: &str) -> Option<CanonicalModelRequest> {
    let answer: String = answer.chars().take(ANSWER_CHARS).collect();
    let text = |role, text: String| ModelMessage {
        role,
        content: vec![ModelContent::Text { text }],
    };
    Some(CanonicalModelRequest {
        model: model.model.clone(),
        system: Some(INSTRUCTION.to_owned()),
        messages: vec![
            text(ModelRole::User, prompt.to_owned()),
            text(ModelRole::Assistant, answer),
            text(ModelRole::User, "Title this conversation.".to_owned()),
        ],
        tools: Vec::new(),
        max_output_tokens: 32,
        effort: None,
        idempotency_key: arsy_kernel::protocol::IdempotencyKey::new(format!(
            "{}-title",
            model.turn
        ))
        .ok()?,
    })
}

/// The model's title, read to the end of its answer.
fn ask(provider: &dyn ModelProvider, request: &CanonicalModelRequest) -> Option<String> {
    let stream =
        arsy_kernel::provider::stream_with_retry(provider, request, &mut std::thread::sleep)
            .ok()?;
    let mut text = String::new();
    for event in stream {
        if let ModelEvent::TextDelta { text: delta } = event.ok()? {
            text.push_str(&delta);
        }
    }
    clean(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::provider::{ModelEventStream, ProviderDescriptor, ProviderError};
    use arsy_kernel::sqlite::{Durability, SqliteEventStore};
    use std::sync::{mpsc, Mutex};

    /// Answers with `title` once `go` is signalled, so a test can act while
    /// the request is still out.
    struct Titler {
        descriptor: ProviderDescriptor,
        title: &'static str,
        go: Mutex<mpsc::Receiver<()>>,
    }

    impl ModelProvider for Titler {
        fn descriptor(&self) -> &ProviderDescriptor {
            &self.descriptor
        }

        fn stream(&self, _: &CanonicalModelRequest) -> Result<ModelEventStream, ProviderError> {
            let _ = self.go.lock().unwrap().recv();
            let text = self.title.to_owned();
            Ok(Box::new(std::iter::once(Ok(ModelEvent::TextDelta {
                text,
            }))))
        }
    }

    fn titled(rename_first: bool) -> Option<String> {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteEventStore::open(directory.path().join("s.db"), Durability::Memory).unwrap(),
        );
        let session = SessionId::new();
        let (go, wait) = mpsc::channel();
        let model = TitleModel {
            provider: Arc::new(Titler {
                descriptor: ProviderDescriptor {
                    id: "stub".to_owned(),
                    max_retries: 0,
                },
                title: "\"Login bug fix.\"",
                go: Mutex::new(wait),
            }),
            model: ModelKey {
                provider: "stub".to_owned(),
                model: "m".to_owned(),
            },
            turn: "t1".to_owned(),
        };
        let handle = title_new_session(
            Arc::clone(&store),
            session,
            "model",
            "fix the login bug\nit fails on submit",
            "Done.",
            Some(model),
        )
        .expect("the model is asked");
        assert_eq!(
            store.session_title(session).unwrap().as_deref(),
            Some("fix the login bug"),
            "the prompt's line is written at once"
        );
        if rename_first {
            store.set_session_title(session, "mine").unwrap();
        }
        go.send(()).unwrap();
        handle.join().unwrap();
        store.session_title(session).unwrap()
    }

    #[test]
    fn the_models_title_replaces_the_prompts_line() {
        assert_eq!(titled(false).as_deref(), Some("Login bug fix"));
    }

    #[test]
    fn a_rename_made_while_the_model_answered_wins() {
        assert_eq!(titled(true).as_deref(), Some("mine"));
    }

    #[test]
    fn a_prompt_titles_from_its_first_line_that_says_anything() {
        assert_eq!(
            from_prompt("\n\n  fix   the login\nbug please").as_deref(),
            Some("fix the login")
        );
        assert_eq!(from_prompt("   \n\t"), None);
        let long = "word ".repeat(40);
        let title = from_prompt(&long).unwrap();
        assert!(title.ends_with('…'));
        assert!(title.chars().count() <= MAX_COLUMNS);
    }

    #[test]
    fn a_model_answer_is_trimmed_to_a_bare_title() {
        assert_eq!(
            clean("\"Refactor auth middleware.\"\n").as_deref(),
            Some("Refactor auth middleware")
        );
        assert_eq!(clean("## Title: x").as_deref(), Some("Title: x"));
        assert_eq!(clean("\"\""), None);
    }
}
