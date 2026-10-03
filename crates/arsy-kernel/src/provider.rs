//! Model provider boundary: canonical request in, normalized events out.
//!
//! See `docs/09-model-provider-layer.md` and ADR-0012. An adapter owns wire
//! format, authentication, and error normalization; nothing above this module
//! sees a provider dialect. Operations never touch a provider, so a second
//! adapter is a new [`ModelProvider`] impl and nothing else.
//!
//! Model profiles and capability probing live in [`crate::model_profile`] so
//! observing model behavior stays separate from provider wire execution.

pub mod anthropic;
pub mod google_code_assist;
#[cfg(feature = "http")]
pub mod http;
pub mod openai;
pub mod openai_responses;
pub mod replay;
pub mod wire;

use crate::protocol::IdempotencyKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, time::Duration};

/// Provider-qualified model name. The provider half selects the adapter; the
/// model half is passed through to the wire untouched.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ModelKey {
    pub provider: String,
    pub model: String,
}

impl fmt::Display for ModelKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.provider, self.model)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    User,
    Assistant,
}

/// One piece of conversation content in canonical form.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelContent {
    Text {
        text: String,
    },
    /// A tool call the model already made, replayed back as history.
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    /// The result the host produced for a previous tool call.
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    /// An image the operator attached to the prompt.
    ///
    /// Carried inline as base64 rather than as a path or a URL: every dialect
    /// that accepts one accepts bytes, only some accept a URL, and none can
    /// read a file on this machine. The canonical form is the one they all
    /// share, so the adapter re-frames it and never has to go and fetch it.
    Image {
        /// An IANA media type, such as `image/png`.
        media_type: String,
        /// Standard base64, with padding. Not base64url: this is what every
        /// image API in this family expects, and a data URL is built from it
        /// directly.
        data: String,
    },
}

/// Standard base64 with padding.
///
/// Written here rather than taken as a dependency because it is fifteen lines
/// and the workspace already carries a URL-alphabet encoder for OAuth; a crate
/// for this would be a supply-chain surface bought for nothing.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut block = 0_u32;
        for (index, byte) in chunk.iter().enumerate() {
            block |= u32::from(*byte) << (16 - 8 * index);
        }
        for index in 0..=chunk.len() {
            let sextet = (block >> (18 - 6 * index)) & 0b11_1111;
            encoded.push(char::from(ALPHABET[sextet as usize]));
        }
        // Padding to a multiple of four, which the decoders on the other side
        // require and the URL-safe variant omits.
        for _ in chunk.len()..3 {
            encoded.push('=');
        }
    }
    encoded
}

/// A data URL, for the dialects that take one instead of a byte field.
pub fn data_url(media_type: &str, data: &str) -> String {
    format!("data:{media_type};base64,{data}")
}

/// One message's content as a parts array: the text first, then the images.
///
/// Both OpenAI dialects splice a message the same way and differ only in how a
/// part is spelled — `text` against `input_text`, an object `image_url`
/// against a string one. Written once here so a fix to the shape (an empty
/// text, a new field, an ordering rule) lands in both adapters rather than in
/// whichever one the next reader happens to open.
///
/// `images` is emptied, because a caller that has already decided to use the
/// array form has no second use for them.
pub fn content_parts(text_type: &str, text: &str, images: &mut Vec<Value>) -> Vec<Value> {
    let mut parts = Vec::with_capacity(images.len() + 1);
    // An empty text part is not "no text", it is a part saying nothing, and
    // some endpoints reject one.
    if !text.is_empty() {
        parts.push(serde_json::json!({"type": text_type, "text": text}));
    }
    parts.append(images);
    parts
}

#[cfg(test)]
mod base64_tests {
    use super::{base64, data_url};

    /// The RFC 4648 vectors, because every image API on the other side of this
    /// decodes with a strict decoder: a missing pad byte is a rejected request,
    /// not a slightly different string.
    #[test]
    fn encoding_matches_the_standard_alphabet_and_pads_to_four() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // The bytes that differ from the URL-safe alphabet, which is the other
        // encoder in this crate and the wrong one here.
        assert_eq!(base64(&[0xfb, 0xff, 0xbf]), "+/+/");

        assert_eq!(
            data_url("image/png", &base64(b"foo")),
            "data:image/png;base64,Zm9v"
        );
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: Vec<ModelContent>,
}

/// A tool offered to the model. The schema is passed through as-is; adapting it
/// to a provider dialect is the adapter's job.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// How much reasoning the operator asked a turn to spend.
///
/// Named steps rather than a token count, because the dialects spend it
/// differently: one takes a named level, another a token budget. The steps
/// are ordered, and a model offers only some of them (see
/// [`crate::effort::EffortProfile`]). An unset effort sends nothing at all, so
/// a model without the knob, and every request made before this existed, keep
/// the body they already had.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
}

impl Effort {
    pub const ALL: [Self; 6] = [
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|effort| effort.as_str() == raw)
    }

    /// The share of the output budget a dialect that takes a token count should
    /// hand to reasoning.
    pub const fn thinking_share(self) -> (u32, u32) {
        match self {
            Self::Minimal => (1, 8),
            Self::Low => (1, 4),
            Self::Medium => (1, 2),
            Self::High => (4, 5),
            Self::XHigh | Self::Max => (9, 10),
        }
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Provider-independent request. Every field here has the same meaning for
/// every adapter; anything that does not is not allowed in this struct.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CanonicalModelRequest {
    pub model: ModelKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<ModelMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSchema>,
    pub max_output_tokens: u32,
    /// Unset means the request carries no reasoning knob at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// Carried so a retry is provably the same request, not a second one.
    pub idempotency_key: IdempotencyKey,
}

/// Why the model stopped producing output.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Other,
}

/// Normalized stream event. Identical shapes from every adapter.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ModelEvent {
    TextDelta {
        text: String,
    },
    /// One fragment of model reasoning, as the provider chose to expose it.
    ///
    /// Reasoning is display-only: it is never recorded as an answer and never
    /// replayed as history, because a provider that hides its reasoning sends
    /// none of these and the request a caller builds stays identical either
    /// way. Identical fragments from both dialects arrive here.
    ThinkingDelta {
        text: String,
    },
    /// A tool call has started. Arguments are not known yet and the call is not
    /// runnable at this point.
    ToolCallStarted {
        index: usize,
        id: String,
        name: String,
    },
    /// Raw, still-incomplete argument bytes exactly as received.
    ///
    /// Emitted for progress rendering only: the fragment is never parsed and
    /// never dispatched, so a truncated stream cannot execute a partial call.
    ToolCallDelta {
        index: usize,
        fragment: String,
    },
    /// A complete tool call whose arguments parsed. Only this variant is
    /// executable.
    ToolCallCompleted {
        index: usize,
        id: String,
        name: String,
        arguments: Value,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    Completed {
        stop: StopReason,
    },
}

/// Normalized event stream. Lazy so a caller can stop reading early.
pub type ModelEventStream = Box<dyn Iterator<Item = Result<ModelEvent, ProviderError>> + Send>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderDescriptor {
    pub id: String,
    /// Largest number of extra attempts the adapter considers safe.
    pub max_retries: u32,
}

pub trait ModelProvider: Send + Sync {
    fn descriptor(&self) -> &ProviderDescriptor;

    /// Start a streaming completion. Errors are already normalized.
    fn stream(&self, request: &CanonicalModelRequest) -> Result<ModelEventStream, ProviderError>;
}

/// Normalized failure. Adapters map their status codes and error bodies onto
/// these variants so callers never branch on a provider dialect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderError {
    /// Credential missing, rejected, or lacking permission.
    Auth(String),
    /// The request is wrong and will fail again unchanged.
    InvalidRequest(String),
    NotFound(String),
    RateLimited {
        retry_after: Option<Duration>,
    },
    /// Provider-side capacity or fault.
    Server {
        status: u16,
        message: String,
    },
    /// The request never reached a response.
    Transport(String),
    /// A response arrived but did not match the provider's own wire contract.
    Decode(String),
    /// A streamed tool call ended with incomplete JSON arguments.
    IncompleteToolArguments(String),
}

impl ProviderError {
    /// Stable machine-readable code for logs and protocol failures.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Auth(_) => "provider_auth",
            Self::InvalidRequest(_) => "provider_invalid_request",
            Self::NotFound(_) => "provider_not_found",
            Self::RateLimited { .. } => "provider_rate_limited",
            Self::Server { .. } => "provider_server",
            Self::Transport(_) => "provider_transport",
            Self::Decode(_) | Self::IncompleteToolArguments(_) => "provider_decode",
        }
    }

    /// How long to wait before retrying, or `None` when a retry cannot help.
    ///
    /// A hint the provider sent wins over the default backoff; a class that is
    /// deterministic in the request (auth, invalid request, decode) never
    /// retries, because the identical body would fail identically.
    pub fn retry_after(&self, attempt: u32) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => Some(retry_after.unwrap_or(backoff(attempt))),
            Self::Server { status, .. } if *status >= 500 => Some(backoff(attempt)),
            Self::Transport(_) => Some(backoff(attempt)),
            _ => None,
        }
    }
}

/// Exponential backoff, capped. Deterministic: jitter belongs to the caller's
/// scheduler, not to a value used in assertions.
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500 * 2u64.saturating_pow(attempt.min(6)))
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auth(message) => write!(formatter, "provider authentication failed: {message}"),
            Self::InvalidRequest(message) => {
                write!(formatter, "provider rejected request: {message}")
            }
            Self::NotFound(message) => write!(formatter, "provider resource not found: {message}"),
            Self::RateLimited { retry_after } => match retry_after {
                Some(delay) => write!(formatter, "provider rate limited, retry in {delay:?}"),
                None => formatter.write_str("provider rate limited"),
            },
            Self::Server { status, message } => {
                write!(formatter, "provider server error {status}: {message}")
            }
            Self::Transport(message) => write!(formatter, "provider transport failed: {message}"),
            Self::Decode(message) | Self::IncompleteToolArguments(message) => {
                write!(formatter, "provider response undecodable: {message}")
            }
        }
    }
}

impl std::error::Error for ProviderError {}

/// Start a stream, retrying only classes that a retry can fix.
///
/// The *same* request value is resubmitted every attempt, so its idempotency
/// key is unchanged and a provider that deduplicates sees one logical request
/// rather than several. `sleep` is injected so the delay is observable in a
/// test instead of really elapsing.
pub fn stream_with_retry(
    provider: &dyn ModelProvider,
    request: &CanonicalModelRequest,
    sleep: &mut dyn FnMut(Duration),
) -> Result<ModelEventStream, ProviderError> {
    let max_retries = provider.descriptor().max_retries;
    let mut attempt = 0;
    loop {
        match provider.stream(request) {
            Ok(stream) => return Ok(stream),
            Err(error) => match error.retry_after(attempt) {
                Some(delay) if attempt < max_retries => {
                    sleep(delay);
                    attempt += 1;
                }
                _ => return Err(error),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, sync::Mutex};

    struct FlakyProvider {
        descriptor: ProviderDescriptor,
        failures: Mutex<u32>,
        seen_keys: Mutex<Vec<IdempotencyKey>>,
    }

    impl ModelProvider for FlakyProvider {
        fn descriptor(&self) -> &ProviderDescriptor {
            &self.descriptor
        }

        fn stream(
            &self,
            request: &CanonicalModelRequest,
        ) -> Result<ModelEventStream, ProviderError> {
            self.seen_keys
                .lock()
                .unwrap()
                .push(request.idempotency_key.clone());
            let mut failures = self.failures.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                return Err(ProviderError::Server {
                    status: 503,
                    message: "overloaded".to_owned(),
                });
            }
            Ok(Box::new(std::iter::once(Ok(ModelEvent::Completed {
                stop: StopReason::EndTurn,
            }))))
        }
    }

    fn request() -> CanonicalModelRequest {
        CanonicalModelRequest {
            model: ModelKey {
                provider: "stub".to_owned(),
                model: "m".to_owned(),
            },
            system: None,
            messages: vec![ModelMessage {
                role: ModelRole::User,
                content: vec![ModelContent::Text {
                    text: "hi".to_owned(),
                }],
            }],
            tools: Vec::new(),
            max_output_tokens: 64,
            effort: None,
            idempotency_key: IdempotencyKey::new("turn-1").unwrap(),
        }
    }

    #[test]
    fn retries_resubmit_the_same_idempotency_key_and_honour_the_hint() {
        let provider = FlakyProvider {
            descriptor: ProviderDescriptor {
                id: "stub".to_owned(),
                max_retries: 3,
            },
            failures: Mutex::new(2),
            seen_keys: Mutex::new(Vec::new()),
        };
        let slept = RefCell::new(Vec::new());

        let stream = stream_with_retry(&provider, &request(), &mut |delay| {
            slept.borrow_mut().push(delay);
        })
        .unwrap();

        assert_eq!(stream.count(), 1);
        let keys = provider.seen_keys.lock().unwrap();
        assert_eq!(keys.len(), 3, "two retries after the first attempt");
        assert!(
            keys.windows(2).all(|pair| pair[0] == pair[1]),
            "every attempt carries the original idempotency key"
        );
        assert_eq!(
            slept.into_inner(),
            vec![Duration::from_millis(500), Duration::from_millis(1000)]
        );
    }

    #[test]
    fn deterministic_failures_are_not_retried() {
        let provider = FlakyProvider {
            descriptor: ProviderDescriptor {
                id: "stub".to_owned(),
                max_retries: 3,
            },
            failures: Mutex::new(0),
            seen_keys: Mutex::new(Vec::new()),
        };
        assert_eq!(
            ProviderError::Auth("bad key".to_owned()).retry_after(0),
            None
        );
        assert_eq!(
            ProviderError::InvalidRequest("too long".to_owned()).retry_after(0),
            None
        );
        assert_eq!(
            ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(7)),
            }
            .retry_after(4),
            Some(Duration::from_secs(7)),
            "a provider hint overrides the default backoff"
        );
        // The trait is object-safe and usable without knowing the adapter.
        assert_eq!(provider.descriptor().id, "stub");
    }
}
