//! What the model is still shown once a turn has run for a while.
//!
//! A tool-using turn grows without bound: twenty rounds of reads and test runs
//! is a transcript larger than the window it has to fit in, and the failure is
//! silent — the provider rejects the request, or worse, quietly drops the front
//! of it, which is where the task was stated.
//!
//! # What gets dropped, and why it is the observations
//!
//! The transcript is not uniformly valuable. The task, the model's own
//! reasoning, and the most recent observations decide the next call; the body
//! of a file read eleven rounds ago does not. So nothing is ever *removed* —
//! removing a tool result would break the call/result pairing every provider
//! requires — and instead the body of an old observation is replaced by a
//! stub naming what it was and where the whole thing still is.
//!
//! ```text
//! [system]  instructions            always kept
//! [user]    the task                always kept
//! [assist]  reasoning + tool calls  always kept — small, and it is the plan
//! [user]    tool results            elided oldest-first, by rank, to fit
//! ```
//!
//! # Why the ranking comes from the kernel
//!
//! [`ContextView::select`] already ranks fragments under a token budget,
//! honours dependencies, and reports what it left out and why. Ranking
//! observations here with a second, private heuristic would mean two
//! selectors whose answers could disagree, and only one of them able to
//! explain itself.

use arsy_kernel::{
    context::{
        Authority, CompactionSummary, Confidence, ContextCandidate, ContextFragment, ContextScope,
        ContextView, EventCitation, FragmentKind, FragmentSourceKind, Freshness, OmissionReason,
        RankingWeights, SelectionPolicy,
    },
    domain::{ArtifactId, ContextViewId, FragmentId, ResourceRef},
    provider::{ModelContent, ModelMessage, ModelRole},
};

/// Roughly four characters to a token across the families this targets.
///
/// Deliberately an estimate: an exact count needs the provider's tokenizer,
/// which differs per model and is not worth a dependency to decide when to
/// elide a stale file read. The budget is set below the real window to absorb
/// the error.
pub const BYTES_PER_TOKEN: usize = 4;

pub fn estimate_tokens(text: &str) -> u32 {
    u32::try_from(text.len().div_ceil(BYTES_PER_TOKEN)).unwrap_or(u32::MAX)
}

/// How the observations in a transcript are ranked against each other.
///
/// Recency dominates: the observation that decides the next call is almost
/// always the last one. Token cost breaks ties, so between two equally stale
/// results the larger one is elided first and buys more room.
const WEIGHTS: RankingWeights = RankingWeights {
    dependency: 0,
    authority: 0,
    relevance: 1,
    recency: 8,
    confidence: 0,
    novelty: 0,
    token_cost: 2,
};

/// What one round of trimming did.
#[derive(Clone, Debug, Default)]
pub struct Trimmed {
    /// Observations whose body was replaced by a stub.
    pub elided: usize,
    /// Dialogue messages folded into a single summary.
    pub summarized: usize,
    /// The estimate before and after, in tokens.
    pub before: u32,
    pub after: u32,
    /// The summary as a citable record, when the caller supplied the events
    /// the compacted messages came from. Appending it is the caller's to do:
    /// this module has a conversation, not a session.
    pub summary: Option<CompactionSummary>,
}

impl Trimmed {
    pub const fn changed(&self) -> bool {
        self.elided > 0 || self.summarized > 0
    }
}

/// The recorded turns a conversation was rebuilt from.
///
/// Supplied so a summary can cite something a reader can still open. Without
/// it the dialogue is still compacted — the alternative is a request the
/// provider rejects — but the summary can only point at the session as a
/// whole rather than at the events it replaced.
#[derive(Clone, Debug, Default)]
pub struct History {
    pub citations: Vec<EventCitation>,
}

/// Messages at the end of the conversation that are never summarized.
///
/// The recent exchange is what the next call is decided from; a summary of it
/// would save tokens by removing the one thing the turn is about.
const KEEP_RECENT: usize = 6;

/// The most a dialogue summary may itself cost.
const MAX_SUMMARY_BYTES: usize = 2 * 1024;

/// Fit `conversation` inside `budget` tokens by eliding stale observations.
///
/// Returns what it did, so a caller can tell the operator rather than letting
/// the transcript shrink invisibly. A conversation already inside the budget is
/// left exactly as it was.
pub fn trim(conversation: &mut [ModelMessage], budget: u32) -> Trimmed {
    trim_observations(conversation, budget)
}

/// Fit a conversation inside `budget`, compacting the dialogue when eliding
/// observations is not enough.
///
/// Two stages, in this order because they cost different things. Eliding an
/// old tool result loses a body the model can fetch again; summarizing the
/// dialogue loses the model's own words, which nothing can reconstruct. So the
/// cheap loss is taken first, and the expensive one only when the transcript
/// still does not fit — which is the case the observation pass cannot reach at
/// all, because a dialogue larger than the budget leaves the observations
/// nothing to shrink into.
pub fn fit(
    conversation: &mut Vec<ModelMessage>,
    budget: u32,
    history: Option<&History>,
) -> Trimmed {
    let mut trimmed = trim_observations(conversation, budget);
    if trimmed.after <= budget {
        return trimmed;
    }
    let before = trimmed.before;
    let compacted = compact_dialogue(conversation, budget, history);
    trimmed.summarized = compacted.summarized;
    trimmed.summary = compacted.summary;
    trimmed.before = before;
    trimmed.after = total_tokens(conversation);
    trimmed
}

/// Fold the older dialogue now, whatever the budget: `/compact`.
///
/// The same fold `fit` falls back to, taken on request. A conversation too
/// short to fold is left as it was, and the result says so by not changing.
pub fn compact(conversation: &mut Vec<ModelMessage>, history: Option<&History>) -> Trimmed {
    let before = total_tokens(conversation);
    let mut trimmed = compact_dialogue(conversation, 0, history);
    trimmed.before = before;
    trimmed.after = total_tokens(conversation);
    trimmed
}

/// The request-sized copy of `conversation`, leaving the original whole.
///
/// Compaction changes a context view, never canonical history: a caller that
/// remembers where a turn began in `conversation` can still slice from there
/// after the view has been folded down to a fraction of its length.
pub fn view(
    conversation: &[ModelMessage],
    budget: u32,
    history: Option<&History>,
) -> (Vec<ModelMessage>, Trimmed) {
    let mut view = conversation.to_vec();
    let trimmed = fit(&mut view, budget, history);
    (view, trimmed)
}

/// Replace the oldest dialogue with one summary that says where it went.
///
/// The task is kept, the recent exchange is kept, and everything between them
/// becomes a single message. The cut is chosen so it never lands between a
/// tool call and its result: a provider rejects a result whose call it cannot
/// see, so a compaction that split a pair would turn an oversized request into
/// a rejected one.
fn compact_dialogue(
    conversation: &mut Vec<ModelMessage>,
    budget: u32,
    history: Option<&History>,
) -> Trimmed {
    let Some(cut) = compaction_cut(conversation) else {
        return Trimmed::default();
    };
    // Index 0 is the task. Everything from 1 up to the cut is folded.
    let folded: Vec<ModelMessage> = conversation.drain(1..cut).collect();
    let count = folded.len();
    let text = describe(&folded, history);
    conversation.insert(
        1,
        ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text { text: text.clone() }],
        },
    );
    // A conversation that still does not fit has a task and a recent exchange
    // larger than the budget on their own. Nothing here can help with that,
    // and pretending otherwise by cutting the task would lose the one message
    // the turn cannot proceed without.
    let _ = budget;
    Trimmed {
        summarized: count,
        summary: history.and_then(|history| citable(&text, &history.citations)),
        ..Trimmed::default()
    }
}

/// Where the dialogue may be cut: after the task, before the recent exchange,
/// and never immediately before a message that carries a tool result.
fn compaction_cut(conversation: &[ModelMessage]) -> Option<usize> {
    let limit = conversation.len().checked_sub(KEEP_RECENT)?;
    // At least two messages have to be folded for a summary to be worth its
    // own message.
    (2..=limit)
        .rev()
        .find(|cut| {
            conversation.get(*cut).is_none_or(|message| {
                !message
                    .content
                    .iter()
                    .any(|item| matches!(item, ModelContent::ToolResult { .. }))
            })
        })
        .filter(|cut| *cut >= 2)
}

/// An extractive summary: what each folded message was, in order.
///
/// Extractive rather than generated, because generating one costs a model call
/// in the middle of a turn that is already over budget — and a summary the
/// harness invented is a summary nobody can check. Every line here is text
/// that was actually in the conversation.
fn describe(folded: &[ModelMessage], history: Option<&History>) -> String {
    let mut text = format!(
        "[{} earlier messages were compacted to stay within the context budget. ",
        folded.len()
    );
    match history.filter(|history| !history.citations.is_empty()) {
        Some(history) => text.push_str(&format!(
            "The originals are events {} of this session; `arsy session export` reads them.]\n",
            history
                .citations
                .iter()
                .map(|citation| citation.sequence.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        None => text.push_str(
            "The originals are recorded as this session's `turn.transcript` events; \
             `arsy session export` reads them.]\n",
        ),
    }
    for message in folded {
        for item in &message.content {
            let line = match item {
                ModelContent::Text { text } => format!("{:?}: {}", message.role, head(text)),
                ModelContent::ToolCall { name, .. } => format!("called {name}"),
                ModelContent::ToolResult { id, is_error, .. } => {
                    format!("result of {id}{}", if *is_error { " (failed)" } else { "" })
                }
                ModelContent::Image { media_type, .. } => {
                    format!("an attached {media_type}")
                }
            };
            if text.len() + line.len() > MAX_SUMMARY_BYTES {
                text.push_str("...\n");
                return text;
            }
            text.push_str(&line);
            text.push('\n');
        }
    }
    text
}

/// The first sentence's worth of a message, on one line.
fn head(text: &str) -> String {
    let trimmed = text.trim().replace('\n', " ");
    let mut cut = 160.min(trimmed.len());
    while cut > 0 && !trimmed.is_char_boundary(cut) {
        cut -= 1;
    }
    if cut < trimmed.len() {
        format!("{}...", &trimmed[..cut])
    } else {
        trimmed
    }
}

/// The summary as a record that cites the events it replaced.
///
/// `None` when the citations cannot form a valid summary — no events, or
/// duplicates. A summary that cannot be checked is not appended: the
/// conversation is still compacted, and the record simply is not claimed.
fn citable(text: &str, citations: &[EventCitation]) -> Option<CompactionSummary> {
    let fragment = ContextFragment::new(
        FragmentId::new(),
        FragmentKind::Summary,
        ResourceRef::new("event", "compaction").ok()?,
        FragmentSourceKind::Derived,
        ContextScope::Global,
        ArtifactId::new(),
        estimate_tokens(text),
        // Derived from a conversation, which is not the operator speaking.
        Authority::Untrusted,
        Confidence::new(10_000).ok()?,
        Freshness::Current,
        Vec::new(),
    )
    .ok()?;
    CompactionSummary::new(fragment, citations.to_vec()).ok()
}

fn trim_observations(conversation: &mut [ModelMessage], budget: u32) -> Trimmed {
    let before = total_tokens(conversation);
    if before <= budget {
        return Trimmed {
            before,
            after: before,
            ..Trimmed::default()
        };
    }

    // Everything that is not an elidable observation is fixed cost. What is
    // left of the budget is what the observations have to fit into.
    let observations = observations(conversation);
    let fixed = before.saturating_sub(
        observations
            .iter()
            .map(|observation| observation.tokens)
            .fold(0u32, u32::saturating_add),
    );
    let remaining = budget.saturating_sub(fixed);

    let fragments: Vec<ContextFragment> = observations
        .iter()
        .filter_map(|observation| {
            ContextFragment::new(
                FragmentId::new(),
                FragmentKind::ToolState,
                ResourceRef::new("tool", observation.id.clone()).ok()?,
                FragmentSourceKind::Tool,
                ContextScope::Global,
                // The fragment's content id is not read by the selector, which
                // ranks on the signals below; the stub tells the model to read
                // the file again, so nothing here has to find the artifact.
                ArtifactId::new(),
                observation.tokens,
                // A tool result is the workspace talking, not the operator: it
                // is never trusted context, and the kernel enforces that.
                Authority::Untrusted,
                Confidence::new(10_000).ok()?,
                Freshness::Current,
                Vec::new(),
            )
            .ok()
        })
        .collect();
    let candidates: Vec<ContextCandidate<'_>> = fragments
        .iter()
        .zip(&observations)
        .map(|(fragment, observation)| ContextCandidate {
            fragment,
            residency: "local",
            relevance: 10_000,
            recency: observation.recency,
            novelty: 0,
        })
        .collect();

    let Ok(view) = ContextView::select(
        ContextViewId::new(),
        &candidates,
        &SelectionPolicy {
            scope: ContextScope::Global,
            require_trusted: false,
            allowed_residencies: Vec::new(),
            budget: remaining,
        },
        &WEIGHTS,
    ) else {
        // A selector that cannot answer must not silently keep everything: the
        // request would be rejected. Fall back to eliding the oldest half,
        // which is the same shape of answer with a worse ranking.
        return elide(conversation, &observations, observations.len() / 2, before);
    };

    let dropped: Vec<usize> = view
        .omissions
        .iter()
        .filter(|omission| omission.reason == OmissionReason::TokenBudget)
        .filter_map(|omission| {
            fragments
                .iter()
                .position(|fragment| fragment.id == omission.fragment)
        })
        .collect();
    elide_indexed(conversation, &observations, &dropped, before)
}

/// One tool result in the transcript, and where it lives.
struct Observation {
    /// The tool call id, which is also how the artifact is found again.
    id: String,
    message: usize,
    content: usize,
    tokens: u32,
    /// Higher is newer, in basis points, which is the scale the ranker uses.
    recency: u16,
}

fn observations(conversation: &[ModelMessage]) -> Vec<Observation> {
    let mut found = Vec::new();
    for (message, entry) in conversation.iter().enumerate() {
        for (content, item) in entry.content.iter().enumerate() {
            if let ModelContent::ToolResult {
                id,
                content: body,
                is_error,
            } = item
            {
                // A failed call is small and is exactly what the model needs in
                // order not to repeat it, so it is not worth eliding.
                if *is_error || body.starts_with(ELIDED) {
                    continue;
                }
                found.push(Observation {
                    id: id.clone(),
                    message,
                    content,
                    tokens: estimate_tokens(body),
                    recency: 0,
                });
            }
        }
    }
    let last = found.len().saturating_sub(1).max(1);
    for (position, observation) in found.iter_mut().enumerate() {
        observation.recency = u16::try_from(position * 10_000 / last)
            .unwrap_or(10_000)
            .min(10_000);
    }
    found
}

const ELIDED: &str = "[earlier result elided";

fn elide(
    conversation: &mut [ModelMessage],
    observations: &[Observation],
    count: usize,
    before: u32,
) -> Trimmed {
    let oldest: Vec<usize> = (0..count.min(observations.len())).collect();
    elide_indexed(conversation, observations, &oldest, before)
}

fn elide_indexed(
    conversation: &mut [ModelMessage],
    observations: &[Observation],
    which: &[usize],
    before: u32,
) -> Trimmed {
    let mut elided = 0;
    for index in which {
        let Some(observation) = observations.get(*index) else {
            continue;
        };
        let Some(item) = conversation
            .get_mut(observation.message)
            .and_then(|message| message.content.get_mut(observation.content))
        else {
            continue;
        };
        if let ModelContent::ToolResult { id, content, .. } = item {
            // The stub says what was there and how to get it back, so the model
            // can re-read rather than assume the file was empty.
            *content = format!(
                "{ELIDED} to stay within the context budget; call {id} produced \
                 roughly {} tokens. Read the file or re-run the command if you need it again.]",
                observation.tokens
            );
            elided += 1;
        }
    }
    let after = total_tokens(conversation);
    Trimmed {
        elided,
        before,
        after,
        ..Trimmed::default()
    }
}

fn total_tokens(conversation: &[ModelMessage]) -> u32 {
    conversation
        .iter()
        .flat_map(|message| &message.content)
        .map(|item| match item {
            ModelContent::Text { text } => estimate_tokens(text),
            ModelContent::ToolResult { content, .. } => estimate_tokens(content),
            ModelContent::ToolCall {
                name, arguments, ..
            } => estimate_tokens(name) + estimate_tokens(&arguments.to_string()),
            // An image costs the model tokens, but not in proportion to its
            // base64 length — which is all this function can see. Counting the
            // encoding would make one screenshot look like the whole
            // transcript and elide every observation to make room for it.
            ModelContent::Image { .. } => 0,
        })
        .fold(0u32, u32::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arsy_kernel::provider::ModelRole;
    use serde_json::json;

    fn call(id: &str) -> ModelMessage {
        ModelMessage {
            role: ModelRole::Assistant,
            content: vec![ModelContent::ToolCall {
                id: id.to_owned(),
                name: "fs.read".to_owned(),
                arguments: json!({"path": "big.rs"}),
            }],
        }
    }

    fn result(id: &str, body: &str) -> ModelMessage {
        ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::ToolResult {
                id: id.to_owned(),
                content: body.to_owned(),
                is_error: false,
            }],
        }
    }

    /// `/compact` folds the older dialogue even when the budget does not
    /// ask for it, keeps the task and the recent exchange, and leaves a
    /// conversation too short to fold as it was.
    #[test]
    fn compact_folds_on_request_and_keeps_the_ends() {
        let mut conversation = transcript(6);
        let task = conversation[0].clone();
        let last = conversation.last().cloned().unwrap();
        let trimmed = compact(&mut conversation, None);
        assert!(trimmed.summarized >= 2, "{trimmed:?}");
        assert!(trimmed.after < trimmed.before, "{trimmed:?}");
        assert_eq!(conversation[0], task);
        assert_eq!(conversation.last(), Some(&last));

        let mut short = transcript(1);
        let before = short.clone();
        assert!(!compact(&mut short, None).changed());
        assert_eq!(short, before);
    }

    fn transcript(rounds: usize) -> Vec<ModelMessage> {
        let mut conversation = vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "the task".to_owned(),
            }],
        }];
        for round in 0..rounds {
            let id = format!("call-{round}");
            conversation.push(call(&id));
            conversation.push(result(&id, &"x".repeat(4_000)));
        }
        conversation
    }

    fn bodies(conversation: &[ModelMessage]) -> Vec<&str> {
        conversation
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|item| match item {
                ModelContent::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_conversation_inside_its_budget_is_untouched() {
        let mut conversation = transcript(2);
        let original = conversation.clone();

        let trimmed = trim(&mut conversation, 100_000);

        assert!(!trimmed.changed());
        assert_eq!(trimmed.before, trimmed.after);
        assert_eq!(conversation, original);
    }

    #[test]
    fn an_overlong_conversation_loses_its_stalest_observations_first() {
        let mut conversation = transcript(6);

        let trimmed = trim(&mut conversation, 1_500);

        assert!(trimmed.changed(), "{trimmed:?}");
        assert!(trimmed.after < trimmed.before);
        assert!(
            trimmed.after <= 1_500,
            "the point of the budget is to be met: {trimmed:?}"
        );
        let bodies = bodies(&conversation);
        // The newest observation survives; the oldest does not.
        assert!(!bodies[0].starts_with('x'), "the oldest was elided");
        assert!(
            bodies.last().is_some_and(|body| body.starts_with('x')),
            "the newest observation is what decides the next call"
        );
        // The task is never touched.
        assert!(matches!(
            conversation[0].content.first(),
            Some(ModelContent::Text { text }) if text == "the task"
        ));
    }

    #[test]
    fn every_tool_call_still_has_a_result_after_trimming() {
        let mut conversation = transcript(8);

        trim(&mut conversation, 1_000);

        let calls: Vec<&str> = conversation
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|item| match item {
                ModelContent::ToolCall { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        let answered: Vec<&str> = conversation
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|item| match item {
                ModelContent::ToolResult { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            calls, answered,
            "eliding a body must never remove the result a provider requires"
        );
    }

    #[test]
    fn an_elided_observation_says_what_it_was_and_is_not_elided_twice() {
        let mut conversation = transcript(6);

        trim(&mut conversation, 1_500);
        let once = bodies(&conversation)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(once[0].contains("call-0"), "{}", once[0]);
        assert!(once[0].contains("tokens"), "{}", once[0]);

        // Trimming again finds nothing left to take from the stubs.
        trim(&mut conversation, 1_500);
        assert_eq!(bodies(&conversation), once);
    }

    /// The case the observation pass cannot reach: dialogue alone larger than
    /// the budget. Eliding tool results does nothing, and the request would be
    /// rejected by the provider rather than merely be expensive.
    #[test]
    fn a_dialogue_that_alone_exceeds_the_budget_is_compacted_into_a_summary() {
        let mut conversation = vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "the task".to_owned(),
            }],
        }];
        for round in 0..12 {
            conversation.push(ModelMessage {
                role: ModelRole::Assistant,
                content: vec![ModelContent::Text {
                    text: format!("round {round}: {}", "reasoning ".repeat(400)),
                }],
            });
        }
        let before = total_tokens(&conversation);

        // Eliding observations alone changes nothing: there are none.
        let mut untouched = conversation.clone();
        assert!(!trim(&mut untouched, 2_000).changed());

        let trimmed = fit(&mut conversation, 2_000, None);

        assert!(trimmed.summarized > 0, "{trimmed:?}");
        assert_eq!(trimmed.before, before);
        assert!(
            trimmed.after < before,
            "compaction is supposed to make it smaller: {trimmed:?}"
        );
        // The task is message zero and is never the part cut.
        assert!(matches!(
            conversation[0].content.first(),
            Some(ModelContent::Text { text }) if text == "the task"
        ));
        let ModelContent::Text { text } = &conversation[1].content[0] else {
            panic!("the summary replaces the folded messages");
        };
        assert!(text.contains("were compacted"), "{text}");
        assert!(
            text.contains("turn.transcript"),
            "with no citations it still says where the originals are: {text}"
        );
        // The recent exchange survives untouched, because it is what the next
        // call is decided from.
        assert_eq!(conversation.last(), untouched.last());
    }

    /// A turn remembers where it began as an index into the conversation.
    /// Compacting the conversation itself shrank it below that index, and
    /// slicing the turn's messages out of it for the transcript panicked.
    #[test]
    fn a_view_compacts_the_request_without_shortening_the_conversation() {
        let mut conversation = transcript(0);
        for round in 0..40 {
            conversation.push(ModelMessage {
                role: ModelRole::Assistant,
                content: vec![ModelContent::Text {
                    text: format!("round {round}: {}", "reasoning ".repeat(400)),
                }],
            });
        }
        let base = conversation.len() - 4;
        let original = conversation.clone();

        let (view, trimmed) = view(&conversation, 2_000, None);

        assert!(trimmed.summarized > 0, "{trimmed:?}");
        assert!(view.len() < base, "the view is what got smaller");
        assert_eq!(conversation, original, "canonical history is untouched");
        assert_eq!(conversation[base..].len(), 4);
    }

    /// A summary that can name the events it replaced produces a record, so
    /// the originals stay addressable rather than merely alluded to.
    #[test]
    fn a_compaction_cites_the_events_the_conversation_was_rebuilt_from() {
        let mut conversation = vec![ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::Text {
                text: "the task".to_owned(),
            }],
        }];
        for round in 0..12 {
            conversation.push(ModelMessage {
                role: ModelRole::Assistant,
                content: vec![ModelContent::Text {
                    text: format!("round {round}: {}", "reasoning ".repeat(400)),
                }],
            });
        }
        let history = History {
            citations: vec![
                EventCitation {
                    id: arsy_kernel::domain::EventId::new(),
                    sequence: 4,
                },
                EventCitation {
                    id: arsy_kernel::domain::EventId::new(),
                    sequence: 9,
                },
            ],
        };

        let trimmed = fit(&mut conversation, 2_000, Some(&history));

        let summary = trimmed.summary.expect("citations produce a record");
        assert_eq!(summary.citations, history.citations);
        assert_eq!(summary.summary.kind, FragmentKind::Summary);
        let ModelContent::Text { text } = &conversation[1].content[0] else {
            panic!("the summary replaces the folded messages");
        };
        assert!(
            text.contains("events 4, 9"),
            "the message names what a reader can open: {text}"
        );
    }

    /// A provider rejects a tool result whose call it cannot see, so a
    /// compaction that folded a call and left its result behind would turn an
    /// oversized request into a rejected one.
    #[test]
    fn compaction_never_leaves_a_result_without_its_call() {
        let mut conversation = transcript(14);
        for message in &mut conversation {
            for item in &mut message.content {
                if let ModelContent::Text { text } = item {
                    *text = "y".repeat(20_000);
                }
            }
        }

        fit(&mut conversation, 500, None);

        let calls: Vec<&str> = conversation
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|item| match item {
                ModelContent::ToolCall { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        let answered: Vec<&str> = conversation
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|item| match item {
                ModelContent::ToolResult { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(calls, answered, "every surviving result still has its call");
    }

    #[test]
    fn a_failed_result_is_kept_so_the_model_does_not_repeat_the_call() {
        let mut conversation = transcript(6);
        conversation.push(call("failed"));
        conversation.push(ModelMessage {
            role: ModelRole::User,
            content: vec![ModelContent::ToolResult {
                id: "failed".to_owned(),
                content: "no line matched the context".to_owned(),
                is_error: true,
            }],
        });

        trim(&mut conversation, 1_000);

        assert!(bodies(&conversation).contains(&"no line matched the context"));
    }
}
