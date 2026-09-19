//! OAuth 2.0 login, driven by configuration or a built-in preset.
//!
//! The authorize, token, and device endpoints and the client identifier come
//! from `[provider.endpoint.<id>.oauth]`, so this serves any issuer an operator
//! registers a client with. The built-in presets (`oauth::presets`) fill the
//! same shape for the few vendors ARSY ships a client for.
//!
//! Three grants, chosen by what the issuer offers. The device grant (RFC 8628)
//! needs no listening socket and works over SSH, so it is preferred when the
//! configuration names a device endpoint. Otherwise the authorization-code
//! grant with PKCE (RFC 7636) runs either against a loopback redirect, or —
//! when the issuer's redirect is not a loopback URL — against its own hosted
//! callback page, which shows the operator a code to paste back instead of a
//! local listener catching it. The manual variant also posts the token
//! exchange as JSON rather than form-encoded, which is the shape Anthropic's
//! OAuth client speaks. A public client sends no secret; an installed-app
//! client whose issuer still demands one (a Google desktop client, say)
//! carries it in `client_secret` — not confidential for software the
//! operator runs, but required for the exchange to succeed.
//!
//! HTTP is the same injected [`WireTransport`] the provider adapters use, so a
//! login is testable without a network and adds no second HTTP path.

use crate::{
    config::OAuth,
    provider::{
        wire::{WireRequest, WireTransport},
        ProviderError,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, TcpListener},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// How long to wait for the operator to finish in their browser.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Refresh this long before real expiry, so a token cannot lapse between the
/// check and the request it was checked for.
pub const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// What a login leaves behind. Stored as JSON in the credential store, so a
/// token never lands in a file the operator has to protect themselves.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds. Absent when the issuer did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// The raw OIDC `id_token`, kept when the issuer sent one: some APIs need a
    /// claim from it (OpenAI scopes ChatGPT-plan calls by an account id carried
    /// there). Not a credential on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

impl TokenSet {
    /// Whether the access token should be refreshed before it is used.
    ///
    /// A token with no stated expiry is taken at face value: the alternative
    /// is refreshing on every request, which is worse for both sides.
    pub fn is_expired(&self, now: u64) -> bool {
        self.expires_at
            .is_some_and(|at| now.saturating_add(EXPIRY_MARGIN.as_secs()) >= at)
    }

    /// A string claim from the `id_token` payload. See [`jwt_claim`].
    pub fn id_token_claim(&self, name: &str) -> Option<String> {
        jwt_claim(self.id_token.as_deref()?, name)
    }
}

/// A string claim from a JWT payload, without verifying the signature: the
/// token came straight from the issuer over TLS in the same exchange, and the
/// claims read here only pick which account to address, never grant anything.
/// The claim may sit at the top level or one object deep (OpenAI nests account
/// details under a `https://api.openai.com/auth` key).
pub fn jwt_claim(token: &str, name: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let value: Value = serde_json::from_slice(&base64url_decode(payload)?).ok()?;
    value
        .get(name)
        .or_else(|| {
            value
                .as_object()?
                .values()
                .filter_map(Value::as_object)
                .find_map(|nested| nested.get(name))
        })
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// What the operator has to do to finish a device login.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DevicePrompt {
    pub verification_uri: String,
    /// Same page with the code already filled in, when the issuer offers it.
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
    device_code: String,
    interval: Duration,
    expires_in: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OAuthError {
    /// The issuer rejected the request, with its own error code.
    Issuer {
        code: String,
        description: String,
    },
    /// The exchange never reached a response, or the response was unusable.
    Transport(String),
    Decode(String),
    /// The operator did not finish in time, or declined.
    Abandoned(String),
    Local(String),
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Issuer { code, description } => {
                write!(
                    formatter,
                    "the issuer rejected the login ({code}): {description}"
                )
            }
            Self::Transport(message) => write!(formatter, "the login request failed: {message}"),
            Self::Decode(message) => {
                write!(formatter, "the issuer's response was unusable: {message}")
            }
            Self::Abandoned(message) => write!(formatter, "the login did not complete: {message}"),
            Self::Local(message) => write!(formatter, "the login could not start: {message}"),
        }
    }
}

impl std::error::Error for OAuthError {}

/// Which grant this configuration selects.
pub const fn uses_device_grant(oauth: &OAuth) -> bool {
    oauth.device_authorization_url.is_some()
}

/// Whether this configuration's authorization-code grant runs against the
/// issuer's own hosted callback page — Anthropic's Claude Code OAuth client,
/// among those ARSY ships, works this way — rather than a loopback redirect
/// [`authorization_code`] can catch itself. Recognised the same way the
/// device grant is: from the shape of the client's own fields, not a
/// separate flag naming the grant. A manual-grant client also takes its
/// token-endpoint fields as JSON rather than form-encoded, which
/// [`begin_manual`]/[`finish_manual`] and [`refresh`] both honour.
pub fn uses_manual_grant(oauth: &OAuth) -> bool {
    oauth
        .redirect_uri
        .as_deref()
        .is_some_and(|uri| crate::config::redirect_loopback_port(uri).is_none())
}

/// Ask the issuer to start a device login. The caller shows the prompt and
/// then calls [`poll_device`].
pub fn begin_device(
    transport: &dyn WireTransport,
    oauth: &OAuth,
) -> Result<DevicePrompt, OAuthError> {
    let url = oauth
        .device_authorization_url
        .as_deref()
        .ok_or_else(|| OAuthError::Local("this provider has no device endpoint".to_owned()))?;
    let value = post_form(
        transport,
        url,
        &[
            ("client_id", oauth.client_id.as_str()),
            ("scope", &oauth.scopes.join(" ")),
        ],
    )?;
    Ok(DevicePrompt {
        verification_uri: string(&value, "verification_uri")?,
        verification_uri_complete: value
            .get("verification_uri_complete")
            .and_then(Value::as_str)
            .map(str::to_owned),
        user_code: string(&value, "user_code")?,
        device_code: string(&value, "device_code")?,
        // RFC 8628 makes both optional and names these defaults.
        interval: Duration::from_secs(seconds(&value, "interval").unwrap_or(5)),
        expires_in: Duration::from_secs(seconds(&value, "expires_in").unwrap_or(600)),
    })
}

/// Wait for the operator to approve the device login.
///
/// `sleep` is injected so a test observes the backoff instead of really
/// waiting. `slow_down` widens the interval permanently, as the RFC requires:
/// retrying at the old rate after being told to slow down gets the client
/// blocked.
pub fn poll_device(
    transport: &dyn WireTransport,
    oauth: &OAuth,
    prompt: &DevicePrompt,
    sleep: &mut dyn FnMut(Duration),
) -> Result<TokenSet, OAuthError> {
    let mut interval = prompt.interval;
    let mut waited = Duration::ZERO;
    loop {
        if waited >= prompt.expires_in.min(LOGIN_TIMEOUT) {
            return Err(OAuthError::Abandoned(
                "the device code expired before it was approved".to_owned(),
            ));
        }
        sleep(interval);
        waited += interval;
        match post_form(
            transport,
            &oauth.token_url,
            &with_secret(
                &[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("device_code", &prompt.device_code),
                    ("client_id", &oauth.client_id),
                ],
                oauth.client_secret.as_deref(),
            ),
        ) {
            Ok(value) => return token_set(&value),
            Err(OAuthError::Issuer { code, description }) => match code.as_str() {
                "authorization_pending" => {}
                "slow_down" => interval += Duration::from_secs(5),
                "access_denied" => {
                    return Err(OAuthError::Abandoned("the login was declined".to_owned()))
                }
                "expired_token" => {
                    return Err(OAuthError::Abandoned(
                        "the device code expired before it was approved".to_owned(),
                    ))
                }
                _ => return Err(OAuthError::Issuer { code, description }),
            },
            Err(error) => return Err(error),
        }
    }
}

/// One authorization-code login with PKCE, over a loopback redirect.
///
/// The listener is bound before the URL is built, because the redirect has to
/// name the port the issuer will send the browser back to. `visit` is handed
/// the authorization URL: a caller opens it, or prints it.
pub fn authorization_code(
    transport: &dyn WireTransport,
    oauth: &OAuth,
    visit: &mut dyn FnMut(&str),
) -> Result<TokenSet, OAuthError> {
    // A registered redirect names its port; the listener has to be on that
    // one. With none, port 0 takes a free port so two logins cannot collide.
    let (listener, redirect) = match &oauth.redirect_uri {
        Some(uri) => {
            let port = crate::config::redirect_loopback_port(uri).ok_or_else(|| {
                OAuthError::Local(format!("`{uri}` is not a loopback redirect with a port"))
            })?;
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
                .map_err(|error| OAuthError::Local(format!("port {port}: {error}")))?;
            (listener, uri.clone())
        }
        None => {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .map_err(|error| OAuthError::Local(error.to_string()))?;
            let port = listener
                .local_addr()
                .map_err(|error| OAuthError::Local(error.to_string()))?
                .port();
            (listener, format!("http://127.0.0.1:{port}/callback"))
        }
    };

    let verifier = random_token();
    let challenge = base64url(&sha256(verifier.as_bytes()));
    // Bound to this one attempt: a callback carrying a different state is a
    // response to somebody else's login and is refused.
    let state = random_token();
    let scope = oauth.scopes.join(" ");
    let mut params = vec![
        ("response_type", "code"),
        ("client_id", oauth.client_id.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("scope", scope.as_str()),
        ("state", state.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
    ];
    params.extend(
        oauth
            .authorize_params
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    visit(&format!("{}?{}", oauth.authorize_url, form_encode(&params)));

    let code = await_callback(&listener, &state)?;
    let value = post_form(
        transport,
        &oauth.token_url,
        &with_secret(
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", &redirect),
                ("client_id", &oauth.client_id),
                ("code_verifier", &verifier),
            ],
            oauth.client_secret.as_deref(),
        ),
    )?;
    token_set(&value)
}

/// What the operator has to do to finish a manual authorization-code login:
/// visit `authorize_url`, approve, and paste back the code the issuer's
/// hosted callback page shows. `verifier` is kept so [`finish_manual`] can
/// validate what comes back and complete the PKCE exchange; a caller that
/// cannot hold the whole struct across two turns (a TUI collecting the
/// pasted line as its own separate step, say) only needs to keep `verifier`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManualPrompt {
    pub authorize_url: String,
    pub verifier: String,
}

/// Start a manual authorization-code login with PKCE, over the issuer's own
/// hosted callback page rather than a loopback redirect.
///
/// Anthropic's Claude Code OAuth client works this way: nothing local can
/// receive `oauth.redirect_uri`, because it names a page Anthropic serves
/// itself. That page shows the operator a `code#state` pair to paste back —
/// [`finish_manual`] takes it from there. Split from the exchange itself,
/// the way [`begin_device`]/[`poll_device`] are, so a caller that has to
/// wait for the operator through its own input loop (rather than blocking
/// here on one) can hold `verifier` in the meantime and finish later.
pub fn begin_manual(oauth: &OAuth) -> Result<ManualPrompt, OAuthError> {
    let redirect = oauth.redirect_uri.as_deref().ok_or_else(|| {
        OAuthError::Local("this provider has no redirect_uri configured".to_owned())
    })?;
    let verifier = random_token();
    let challenge = base64url(&sha256(verifier.as_bytes()));
    let scope = oauth.scopes.join(" ");
    let mut params = vec![
        ("response_type", "code"),
        ("client_id", oauth.client_id.as_str()),
        ("redirect_uri", redirect),
        ("scope", scope.as_str()),
        ("state", verifier.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
    ];
    params.extend(
        oauth
            .authorize_params
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    Ok(ManualPrompt {
        authorize_url: format!("{}?{}", oauth.authorize_url, form_encode(&params)),
        verifier,
    })
}

/// Finish a manual login: validate the operator's pasted code against
/// `verifier` from [`begin_manual`], and exchange it for a token set.
///
/// `state` doubles as a second PKCE verifier the operator relays by hand, so
/// a code copied from someone else's login is refused just like an
/// unmatched loopback callback would be. The hosted page shows
/// `code#state`; a bare code (no separator) is tolerated too, matched
/// against `verifier` — which the operator can never have mistyped, because
/// they never saw it.
pub fn finish_manual(
    transport: &dyn WireTransport,
    oauth: &OAuth,
    verifier: &str,
    pasted: &str,
) -> Result<TokenSet, OAuthError> {
    let redirect = oauth.redirect_uri.as_deref().ok_or_else(|| {
        OAuthError::Local("this provider has no redirect_uri configured".to_owned())
    })?;
    let pasted = pasted.trim();
    let (code, state) = pasted.split_once('#').unwrap_or((pasted, verifier));
    if state != verifier {
        return Err(OAuthError::Abandoned(
            "the pasted code did not belong to this login".to_owned(),
        ));
    }

    let value = post_json(
        transport,
        &oauth.token_url,
        &with_secret(
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect),
                ("client_id", &oauth.client_id),
                ("code_verifier", verifier),
                ("state", state),
            ],
            oauth.client_secret.as_deref(),
        ),
    )?;
    token_set(&value)
}

/// The token-endpoint fields with `client_secret` appended when the client has
/// one. Public clients send none; some installed-app clients must.
fn with_secret<'a>(
    base: &[(&'a str, &'a str)],
    secret: Option<&'a str>,
) -> Vec<(&'a str, &'a str)> {
    let mut fields = base.to_vec();
    if let Some(secret) = secret {
        fields.push(("client_secret", secret));
    }
    fields
}

/// Trade a refresh token for a fresh access token.
///
/// An issuer that rotates refresh tokens returns a new one; one that does not
/// returns none, so the existing token is carried forward rather than lost.
pub fn refresh(
    transport: &dyn WireTransport,
    oauth: &OAuth,
    tokens: &TokenSet,
) -> Result<TokenSet, OAuthError> {
    let refresh_token = tokens
        .refresh_token
        .as_deref()
        .ok_or_else(|| OAuthError::Abandoned("this login cannot be refreshed".to_owned()))?;
    let fields = with_secret(
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &oauth.client_id),
        ],
        oauth.client_secret.as_deref(),
    );
    let value = if uses_manual_grant(oauth) {
        post_json(transport, &oauth.token_url, &fields)
    } else {
        post_form(transport, &oauth.token_url, &fields)
    }?;
    let mut refreshed = token_set(&value)?;
    // Carry forward anything the refresh response left out.
    if refreshed.refresh_token.is_none() {
        refreshed.refresh_token = tokens.refresh_token.clone();
    }
    if refreshed.id_token.is_none() {
        refreshed.id_token = tokens.id_token.clone();
    }
    Ok(refreshed)
}

/// Read the one redirect the issuer sends the browser to.
///
/// Only the first request is answered, and the operator sees a plain page
/// rather than a blank tab. The authorization code is in the query string, so
/// the request line alone is enough; the body is never read.
fn await_callback(listener: &TcpListener, state: &str) -> Result<String, OAuthError> {
    listener
        .set_nonblocking(false)
        .and_then(|()| listener.take_error())
        .map_err(|error| OAuthError::Local(error.to_string()))?;
    let (stream, _) = listener
        .accept()
        .map_err(|error| OAuthError::Local(error.to_string()))?;
    let mut request = String::new();
    BufReader::new(&stream)
        .read_line(&mut request)
        .map_err(|error| OAuthError::Local(error.to_string()))?;
    let query = request
        .split_whitespace()
        .nth(1)
        .and_then(|target| target.split_once('?'))
        .map(|(_, query)| query.to_owned())
        .unwrap_or_default();
    let parameters = parse_query(&query);
    let find = |name: &str| {
        parameters
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };

    let outcome = match (find("error"), find("state"), find("code")) {
        (Some(code), _, _) => Err(OAuthError::Issuer {
            description: find("error_description").unwrap_or_else(|| code.clone()),
            code,
        }),
        (None, returned, _) if returned.as_deref() != Some(state) => Err(OAuthError::Abandoned(
            "the callback did not belong to this login".to_owned(),
        )),
        (None, _, Some(code)) => Ok(code),
        (None, _, None) => Err(OAuthError::Abandoned(
            "the callback carried no authorization code".to_owned(),
        )),
    };
    let page = callback_page(outcome.is_ok());
    let mut stream = stream;
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
        page.len()
    );
    outcome
}

/// The page the browser lands on after the redirect. Self-contained — no
/// network fetch — and carries the ARSY wordmark so it reads as ARSY's own,
/// not a blank tab.
fn callback_page(ok: bool) -> String {
    let (class, glyph, headline, hint) = if ok {
        (
            "ok",
            "\u{2713}",
            "Signed in",
            "You can close this tab and return to the terminal.",
        )
    } else {
        (
            "err",
            "\u{2717}",
            "Sign-in failed",
            "Check the terminal for what went wrong.",
        )
    };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>ARSY</title>\
<style>:root{{color-scheme:dark}}\
body{{margin:0;min-height:100vh;display:grid;place-items:center;background:#1a1a1a;\
color:#c9c9c9;font:15px/1.6 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}}\
.card{{text-align:center;padding:2.5rem 3rem}}\
.mark{{font-size:2rem;letter-spacing:.15em;color:#f6e2b7}}\
.mark b{{color:#5cc2e0}}\
.status{{margin-top:1.5rem;font-size:1.05rem}}\
.status.ok{{color:#4ea96f}}.status.err{{color:#e0af68}}\
.hint{{margin-top:.5rem;color:#7a7a7a;font-size:.9rem}}</style></head>\
<body><div class=\"card\"><div class=\"mark\"><b>&gt;_</b> ARSY</div>\
<div class=\"status {class}\">{glyph} {headline}</div>\
<div class=\"hint\">{hint}</div></div></body></html>"
    )
}

/// `post_form` and `post_json` differ only in how the fields are carried on
/// the wire; every issuer here needs one or the other. An error response is
/// a documented part of these flows — device polling answers
/// `authorization_pending` with a 400 — so a non-2xx body is decoded rather
/// than discarded.
fn post_form(
    transport: &dyn WireTransport,
    url: &str,
    fields: &[(&str, &str)],
) -> Result<Value, OAuthError> {
    post(
        transport,
        url,
        "application/x-www-form-urlencoded",
        form_encode(fields),
    )
}

/// Same exchange as [`post_form`], but with the fields as a JSON object
/// instead — the shape Anthropic's OAuth token endpoint requires
/// ([`uses_manual_grant`]).
fn post_json(
    transport: &dyn WireTransport,
    url: &str,
    fields: &[(&str, &str)],
) -> Result<Value, OAuthError> {
    let mut body = serde_json::Map::with_capacity(fields.len());
    for (key, value) in fields {
        body.insert((*key).to_owned(), Value::String((*value).to_owned()));
    }
    post(
        transport,
        url,
        "application/json",
        Value::Object(body).to_string(),
    )
}

fn post(
    transport: &dyn WireTransport,
    url: &str,
    content_type: &str,
    body: String,
) -> Result<Value, OAuthError> {
    let response = transport
        .send(WireRequest {
            url: url.to_owned(),
            headers: vec![
                ("content-type".to_owned(), content_type.to_owned()),
                ("accept".to_owned(), "application/json".to_owned()),
            ],
            body,
        })
        .map_err(|error| match error {
            ProviderError::Transport(message) => OAuthError::Transport(message),
            other => OAuthError::Local(other.to_string()),
        })?;
    let status = response.status;
    let body = response
        .lines
        .collect::<Result<Vec<_>, _>>()
        .map_err(OAuthError::Transport)?
        .join("\n");
    let value: Value = serde_json::from_str(&body)
        .map_err(|error| OAuthError::Decode(format!("http {status}: {error}")))?;
    if let Some(code) = value.get("error").and_then(Value::as_str) {
        return Err(OAuthError::Issuer {
            code: code.to_owned(),
            description: value
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or("no description")
                .to_owned(),
        });
    }
    if !(200..300).contains(&status) {
        return Err(OAuthError::Decode(format!("http {status}")));
    }
    Ok(value)
}

fn token_set(value: &Value) -> Result<TokenSet, OAuthError> {
    let owned = |name| value.get(name).and_then(Value::as_str).map(str::to_owned);
    Ok(TokenSet {
        access_token: string(value, "access_token")?,
        refresh_token: owned("refresh_token"),
        expires_at: seconds(value, "expires_in").map(|lifetime| now().saturating_add(lifetime)),
        id_token: owned("id_token"),
    })
}

fn string(value: &Value, name: &str) -> Result<String, OAuthError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| OAuthError::Decode(format!("the response has no `{name}`")))
}

fn seconds(value: &Value, name: &str) -> Option<u64> {
    value.get(name).and_then(|value| {
        value
            .as_u64()
            // Some issuers send these as strings.
            .or_else(|| value.as_str().and_then(|raw| raw.parse().ok()))
    })
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// 256 bits of entropy, base64url-encoded.
///
/// Version-4 UUIDs are the workspace's existing source of cryptographic
/// randomness, so this needs no additional dependency.
fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64url(&bytes)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

/// Base64 without padding and with the URL alphabet, which is the only form
/// PKCE accepts.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut block = 0_u32;
        for (index, byte) in chunk.iter().enumerate() {
            block |= u32::from(*byte) << (16 - 8 * index);
        }
        // Three bytes make four characters; a short final chunk makes fewer,
        // and the padding those would need is omitted.
        for index in 0..=chunk.len() {
            let sextet = (block >> (18 - 6 * index)) & 0b11_1111;
            out.push(char::from(ALPHABET[sextet as usize]));
        }
    }
    out
}

/// Inverse of [`base64url`], ignoring any padding. `None` on a character
/// outside the URL alphabet. Used only to read a JWT payload.
fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let mut sextets = Vec::with_capacity(text.len());
    for byte in text.bytes() {
        sextets.push(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => continue,
            _ => return None,
        });
    }
    let mut out = Vec::with_capacity(sextets.len() / 4 * 3);
    for chunk in sextets.chunks(4) {
        let mut block = 0_u32;
        for (index, sextet) in chunk.iter().enumerate() {
            block |= u32::from(*sextet) << (18 - 6 * index);
        }
        for index in 0..chunk.len().saturating_sub(1) {
            out.push((block >> (16 - 8 * index)) as u8);
        }
    }
    Some(out)
}

fn form_encode(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(name, value)| format!("{}={}", percent_encode(name), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encoding for a form field: everything but the unreserved set, which
/// is the conservative choice and always accepted.
fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn percent_decode(raw: &str) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let mut bytes = raw.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'+' => out.push(b' '),
            b'%' => {
                let hex: String = bytes.by_ref().take(2).map(char::from).collect();
                match u8::from_str_radix(&hex, 16) {
                    Ok(decoded) => out.push(decoded),
                    // Not an escape after all; keep it verbatim rather than
                    // dropping input.
                    Err(_) => out.extend_from_slice(format!("%{hex}").as_bytes()),
                }
            }
            other => out.push(other),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(name), percent_decode(value))
        })
        .collect()
}

/// Built-in OAuth clients for the vendors ARSY can sign in to directly.
///
/// A preset is the same shape a hand-written `[provider.endpoint.<id>.oauth]`
/// would take, plus the endpoint facts a login has to know to make the result
/// usable: which wire dialect it speaks, its API root, and the models to offer.
/// `arsy auth login <id>` uses one when `<id>` names no configured endpoint.
///
/// The client identifiers here are public (they ship in the vendors' own
/// clients); the Google entry also carries the non-confidential "secret" its
/// desktop client type still requires in the token exchange.
pub mod presets {
    use crate::config::{Dialect, OAuth};

    /// One built-in login target.
    pub struct Preset {
        pub id: &'static str,
        /// One line for the `/auth` picker.
        pub label: &'static str,
        pub dialect: Dialect,
        pub base_url: &'static str,
        /// Offered by `/model` after the login; the first is the default.
        pub models: &'static [&'static str],
        /// Built fresh because [`OAuth`] owns its strings.
        build_oauth: fn() -> OAuth,
    }

    impl Preset {
        pub fn oauth(&self) -> OAuth {
            (self.build_oauth)()
        }
    }

    /// Every preset, in the order the picker should list them.
    pub fn all() -> &'static [Preset] {
        PRESETS
    }

    /// The preset `id` names, if any.
    pub fn get(id: &str) -> Option<&'static Preset> {
        let canonical = match id {
            "codex" | "openai-codex" | "codex-oauth" => "codex-oauth",
            "antigravity" | "google-antigravity" => "antigravity",
            "claude" | "claude-pro" | "anthropic-oauth" | "claude-oauth" => "claude-oauth",
            other => other,
        };
        PRESETS.iter().find(|preset| preset.id == canonical)
    }

    fn owned(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    static PRESETS: &[Preset] = &[
        // OpenAI Codex, signed in with a ChatGPT account. The access token is a
        // bearer for the Codex Responses backend; the account it is scoped to
        // rides in a claim the adapter reads back out.
        Preset {
            id: "codex-oauth",
            label: "OpenAI Codex — sign in with a ChatGPT account",
            dialect: Dialect::OpenaiResponses,
            base_url: "https://chatgpt.com/backend-api/codex",
            models: &["gpt-5-codex", "gpt-5", "gpt-5-mini"],
            build_oauth: || OAuth {
                authorize_url: "https://auth.openai.com/oauth/authorize".to_owned(),
                token_url: "https://auth.openai.com/oauth/token".to_owned(),
                device_authorization_url: None,
                client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_owned(),
                client_secret: None,
                scopes: owned(&[
                    "openid",
                    "profile",
                    "email",
                    "offline_access",
                    "api.connectors.read",
                    "api.connectors.invoke",
                ]),
                redirect_uri: Some("http://localhost:1455/auth/callback".to_owned()),
                authorize_params: vec![
                    ("id_token_add_organizations".to_owned(), "true".to_owned()),
                    ("codex_cli_simplified_flow".to_owned(), "true".to_owned()),
                ],
            },
        },
        // Google Antigravity, signed in with a Google account. Talks to Cloud
        // Code Assist; the client is a Google "desktop app" type, so the token
        // exchange still wants the (non-secret) client secret.
        Preset {
            id: "antigravity",
            label: "Google Antigravity — sign in with a Google account",
            dialect: Dialect::GoogleCodeAssist,
            base_url: "https://daily-cloudcode-pa.googleapis.com",
            models: &[
                "gemini-3.8-flash",
                "gemini-3.7-flash",
                "gemini-3.7-pro",
                "gemini-3.1-pro",
                "gemini-3-flash",
                "gemini-3-pro",
                "gemini-2.5-flash",
                "gemini-2.5-pro",
                "claude-3-7-sonnet",
                "claude-sonnet-4-5",
                "claude-sonnet-4-6",
                "claude-opus-4-5",
                "claude-opus-4-6",
                "gpt-5",
                "gpt-5-codex",
                "gpt-oss",
            ],
            build_oauth: || OAuth {
                authorize_url: "https://accounts.google.com/o/oauth2/auth".to_owned(),
                token_url: "https://oauth2.googleapis.com/token".to_owned(),
                device_authorization_url: None,
                client_id:
                    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com"
                        .to_owned(),
                client_secret: Some("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".to_owned()),
                scopes: owned(&[
                    "https://www.googleapis.com/auth/cloud-platform",
                    "https://www.googleapis.com/auth/userinfo.email",
                    "https://www.googleapis.com/auth/userinfo.profile",
                    "https://www.googleapis.com/auth/cclog",
                    "https://www.googleapis.com/auth/experimentsandconfigs",
                ]),
                redirect_uri: Some("http://localhost:36742/oauth-callback".to_owned()),
                authorize_params: vec![
                    ("access_type".to_owned(), "offline".to_owned()),
                    ("prompt".to_owned(), "consent".to_owned()),
                ],
            },
        },
        // Claude Pro/Max, signed in with a Claude.ai account. Anthropic's
        // OAuth client does not support a loopback redirect: the redirect is
        // a page Anthropic hosts itself, which shows the operator a code to
        // paste back rather than a local listener catching it, and its token
        // endpoint takes a JSON body instead of form-encoded fields like
        // every other issuer here. `oauth::uses_manual_grant` picks this up
        // from the shape of `redirect_uri` alone, the same way the device
        // grant is picked up from `device_authorization_url`.
        Preset {
            id: "claude-oauth",
            label: "Claude Pro/Max — sign in with a Claude.ai account",
            dialect: Dialect::Anthropic,
            base_url: "https://api.anthropic.com",
            models: &[
                "claude-sonnet-5",
                "claude-opus-5",
                "claude-fable-5-1",
                "claude-haiku-4-5",
            ],
            build_oauth: || OAuth {
                authorize_url: "https://claude.ai/oauth/authorize".to_owned(),
                token_url: "https://console.anthropic.com/v1/oauth/token".to_owned(),
                device_authorization_url: None,
                client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_owned(),
                client_secret: None,
                scopes: owned(&["org:create_api_key", "user:profile", "user:inference"]),
                redirect_uri: Some("https://console.anthropic.com/oauth/code/callback".to_owned()),
                authorize_params: vec![("code".to_owned(), "true".to_owned())],
            },
        },
    ];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::wire::WireResponse;
    use std::sync::Mutex;

    /// Answers each request with the next canned response, and records what it
    /// was sent.
    struct FakeIssuer {
        responses: Mutex<VecDeque>,
        sent: Mutex<Vec<String>>,
    }

    type VecDeque = std::collections::VecDeque<(u16, String)>;

    impl FakeIssuer {
        fn new(responses: Vec<(u16, &str)>) -> Self {
            Self {
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .map(|(status, body)| (status, body.to_owned()))
                        .collect(),
                ),
                sent: Mutex::new(Vec::new()),
            }
        }
    }

    impl WireTransport for FakeIssuer {
        fn send(&self, request: WireRequest) -> Result<WireResponse, ProviderError> {
            self.sent.lock().unwrap().push(request.body);
            let (status, body) = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("a response was canned for every request");
            Ok(WireResponse {
                status,
                headers: Vec::new(),
                lines: Box::new(std::iter::once(Ok(body))),
            })
        }
    }

    fn oauth(device: bool) -> OAuth {
        OAuth {
            authorize_url: "https://issuer.test/authorize".to_owned(),
            token_url: "https://issuer.test/token".to_owned(),
            device_authorization_url: device.then(|| "https://issuer.test/device".to_owned()),
            client_id: "arsy cli".to_owned(),
            scopes: vec!["offline_access".to_owned(), "models:read".to_owned()],
            ..OAuth::default()
        }
    }

    #[test]
    fn a_device_login_waits_out_pending_and_honours_slow_down() {
        let issuer = FakeIssuer::new(vec![
            (
                200,
                r#"{"device_code":"dev-1","user_code":"WXYZ-1234","verification_uri":"https://issuer.test/activate","interval":2,"expires_in":600}"#,
            ),
            (400, r#"{"error":"authorization_pending"}"#),
            (400, r#"{"error":"slow_down"}"#),
            (
                200,
                r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600}"#,
            ),
        ]);
        let prompt = begin_device(&issuer, &oauth(true)).unwrap();
        assert_eq!(prompt.user_code, "WXYZ-1234");

        let mut slept = Vec::new();
        let tokens = poll_device(&issuer, &oauth(true), &prompt, &mut |delay| {
            slept.push(delay);
        })
        .unwrap();

        assert_eq!(tokens.access_token, "at-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-1"));
        assert!(!tokens.is_expired(now()));
        assert!(
            tokens.is_expired(now() + 3600),
            "the stated lifetime is kept"
        );
        assert_eq!(
            slept,
            [
                Duration::from_secs(2),
                Duration::from_secs(2),
                Duration::from_secs(7),
            ],
            "slow_down widens the interval for every later attempt, not just the next one"
        );

        let sent = issuer.sent.lock().unwrap();
        assert!(
            sent[0].contains("scope=offline_access%20models%3Aread"),
            "scopes are space-joined and form-encoded: {}",
            sent[0]
        );
        assert!(sent[1].contains("device_code=dev-1"));
        assert!(
            sent[1].contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"),
            "{}",
            sent[1]
        );
    }

    #[test]
    fn a_declined_or_failed_device_login_is_reported_not_retried_forever() {
        for (body, expected) in [
            (r#"{"error":"access_denied"}"#, "declined"),
            (r#"{"error":"expired_token"}"#, "expired"),
        ] {
            let issuer = FakeIssuer::new(vec![
                (
                    200,
                    r#"{"device_code":"dev-1","user_code":"C","verification_uri":"https://issuer.test/a"}"#,
                ),
                (400, body),
            ]);
            let prompt = begin_device(&issuer, &oauth(true)).unwrap();
            let error = poll_device(&issuer, &oauth(true), &prompt, &mut |_| {}).unwrap_err();
            assert!(
                matches!(&error, OAuthError::Abandoned(message) if message.contains(expected)),
                "{body} produced {error:?}"
            );
        }
    }

    #[test]
    fn a_refresh_keeps_a_token_the_issuer_did_not_rotate() {
        let issuer = FakeIssuer::new(vec![
            (200, r#"{"access_token":"at-2","expires_in":"3600"}"#),
            (200, r#"{"access_token":"at-3","refresh_token":"rt-2"}"#),
        ]);
        let existing = TokenSet {
            access_token: "at-1".to_owned(),
            refresh_token: Some("rt-1".to_owned()),
            expires_at: Some(0),
            id_token: None,
        };

        let kept = refresh(&issuer, &oauth(false), &existing).unwrap();
        assert_eq!(kept.access_token, "at-2");
        assert_eq!(
            kept.refresh_token.as_deref(),
            Some("rt-1"),
            "an issuer that does not rotate leaves the existing token usable"
        );
        assert!(kept.expires_at.is_some(), "a string lifetime is still read");

        let rotated = refresh(&issuer, &oauth(false), &existing).unwrap();
        assert_eq!(rotated.refresh_token.as_deref(), Some("rt-2"));
        assert_eq!(
            rotated.expires_at, None,
            "no stated lifetime means the token is taken at face value"
        );
        assert!(!rotated.is_expired(now() + 10_000));

        let cannot = refresh(
            &issuer,
            &oauth(false),
            &TokenSet {
                access_token: "at".to_owned(),
                refresh_token: None,
                expires_at: None,
                id_token: None,
            },
        );
        assert!(matches!(cannot, Err(OAuthError::Abandoned(_))));
    }

    #[test]
    fn base64url_round_trips_and_reads_a_jwt_claim() {
        for sample in [
            b"".as_slice(),
            b"f",
            b"fo",
            b"foo",
            b"foob",
            &[251, 255, 190],
        ] {
            assert_eq!(base64url_decode(&base64url(sample)).unwrap(), sample);
        }
        assert!(base64url_decode("not base64 !!").is_none());

        // A JWT is header.payload.signature; only the payload is read, and a
        // claim may sit one level in (OpenAI nests it under an `.../auth` key).
        let payload = base64url(
            br#"{"sub":"u1","https://api.openai.com/auth":{"chatgpt_account_id":"acct-9"}}"#,
        );
        let token = TokenSet {
            access_token: "at".to_owned(),
            refresh_token: None,
            expires_at: None,
            id_token: Some(format!("hdr.{payload}.sig")),
        };
        assert_eq!(token.id_token_claim("sub").as_deref(), Some("u1"));
        assert_eq!(
            token.id_token_claim("chatgpt_account_id").as_deref(),
            Some("acct-9")
        );
        assert_eq!(token.id_token_claim("missing"), None);
        assert_eq!(
            TokenSet {
                id_token: None,
                ..token
            }
            .id_token_claim("sub"),
            None
        );
    }

    #[test]
    fn loopback_redirect_ports_are_recognised() {
        use crate::config::redirect_loopback_port;
        assert_eq!(
            redirect_loopback_port("http://localhost:1455/auth/callback"),
            Some(1455)
        );
        assert_eq!(
            redirect_loopback_port("http://127.0.0.1:36742/oauth-callback"),
            Some(36742)
        );
        assert_eq!(redirect_loopback_port("https://example.com:443/x"), None);
        assert_eq!(redirect_loopback_port("http://localhost/callback"), None);
        assert_eq!(redirect_loopback_port("not a url"), None);
    }

    #[test]
    fn with_secret_appends_only_when_there_is_one() {
        let base = [("grant_type", "refresh_token")];
        assert_eq!(with_secret(&base, None), base.to_vec());
        assert_eq!(
            with_secret(&base, Some("shh")),
            vec![("grant_type", "refresh_token"), ("client_secret", "shh")]
        );
    }

    #[test]
    fn pkce_values_match_rfc_7636() {
        // The worked example from RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            base64url(&sha256(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        // Padding is omitted at every remainder, and the alphabet is URL-safe.
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(&[251, 255, 190]), "-_--");
        let token = random_token();
        assert_eq!(token.len(), 43, "32 bytes, unpadded");
        assert_ne!(token, random_token());
    }

    #[test]
    fn the_callback_page_is_self_contained_and_branded() {
        for ok in [true, false] {
            let page = callback_page(ok);
            assert!(page.starts_with("<!doctype html>"));
            assert!(page.contains("ARSY"));
            // No off-origin fetch: the page has to render on a machine that
            // just finished an auth flow and may have no route out.
            assert!(!page.contains("http://") && !page.contains("https://"));
            assert!(page.contains(if ok { "Signed in" } else { "Sign-in failed" }));
        }
    }

    #[test]
    fn a_callback_is_only_accepted_for_the_login_that_started_it() {
        let state = "the-state";
        for (query, ok) in [
            ("code=abc&state=the-state", true),
            ("state=the-state", false),
            ("code=abc&state=someone-elses", false),
            ("code=abc", false),
            ("error=access_denied&state=the-state", false),
        ] {
            let parameters = parse_query(query);
            let find = |name: &str| {
                parameters
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
            };
            let accepted = find("error").is_none()
                && find("state").as_deref() == Some(state)
                && find("code").is_some();
            assert_eq!(accepted, ok, "{query}");
        }
        assert_eq!(
            parse_query("a=one+two&b=%2Fslash%2F&c"),
            [
                ("a".to_owned(), "one two".to_owned()),
                ("b".to_owned(), "/slash/".to_owned()),
                ("c".to_owned(), String::new()),
            ]
        );
    }

    fn manual_oauth() -> OAuth {
        OAuth {
            authorize_url: "https://issuer.test/authorize".to_owned(),
            token_url: "https://issuer.test/token".to_owned(),
            client_id: "arsy cli".to_owned(),
            scopes: vec!["user:inference".to_owned()],
            redirect_uri: Some("https://issuer.test/code/callback".to_owned()),
            authorize_params: vec![("code".to_owned(), "true".to_owned())],
            ..OAuth::default()
        }
    }

    #[test]
    fn uses_manual_grant_is_true_only_for_a_non_loopback_redirect() {
        assert!(!uses_manual_grant(&oauth(false)), "no redirect_uri at all");
        assert!(!uses_manual_grant(&OAuth {
            redirect_uri: Some("http://localhost:1455/callback".to_owned()),
            ..oauth(false)
        }));
        assert!(uses_manual_grant(&manual_oauth()));
    }

    #[test]
    fn manual_grant_begins_a_hosted_url_and_finishes_with_a_matching_pasted_code_as_json() {
        let oauth = manual_oauth();

        for (paste_wrong_state, expect_ok) in [(false, true), (true, false)] {
            let issuer = FakeIssuer::new(vec![(
                200,
                r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600}"#,
            )]);
            let prompt = begin_manual(&oauth).unwrap();
            assert!(prompt
                .authorize_url
                .starts_with("https://issuer.test/authorize?"));
            assert!(
                prompt.authorize_url.contains("code=true"),
                "{}",
                prompt.authorize_url
            );

            let state = if paste_wrong_state {
                "someone-elses".to_owned()
            } else {
                prompt.verifier.clone()
            };
            let result = finish_manual(
                &issuer,
                &oauth,
                &prompt.verifier,
                &format!("the-code#{state}"),
            );
            assert_eq!(
                result.is_ok(),
                expect_ok,
                "wrong state = {paste_wrong_state}"
            );
            if expect_ok {
                let tokens = result.unwrap();
                assert_eq!(tokens.access_token, "at-1");
                let sent = issuer.sent.lock().unwrap();
                let body: Value = serde_json::from_str(&sent[0])
                    .expect("the token exchange body is JSON, not form-encoded");
                assert_eq!(body["code"], "the-code");
                assert_eq!(body["grant_type"], "authorization_code");
                assert_eq!(body["client_id"], "arsy cli");
            } else {
                assert!(matches!(result.unwrap_err(), OAuthError::Abandoned(_)));
            }
        }
    }

    #[test]
    fn manual_grant_tolerates_a_pasted_code_with_no_state_suffix() {
        let issuer = FakeIssuer::new(vec![(200, r#"{"access_token":"at-1"}"#)]);
        let oauth = manual_oauth();
        let prompt = begin_manual(&oauth).unwrap();
        let tokens = finish_manual(&issuer, &oauth, &prompt.verifier, "bare-code").unwrap();
        assert_eq!(tokens.access_token, "at-1");
        let sent = issuer.sent.lock().unwrap();
        let body: Value = serde_json::from_str(&sent[0]).unwrap();
        assert_eq!(body["code"], "bare-code");
    }

    #[test]
    fn a_refresh_against_a_manual_grant_client_posts_json_not_form() {
        let issuer = FakeIssuer::new(vec![(200, r#"{"access_token":"at-2"}"#)]);
        let existing = TokenSet {
            access_token: "at-1".to_owned(),
            refresh_token: Some("rt-1".to_owned()),
            expires_at: Some(0),
            id_token: None,
        };
        let refreshed = refresh(&issuer, &manual_oauth(), &existing).unwrap();
        assert_eq!(refreshed.access_token, "at-2");
        let sent = issuer.sent.lock().unwrap();
        let body: Value =
            serde_json::from_str(&sent[0]).expect("a manual-grant client refreshes with JSON too");
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "rt-1");
    }

    #[test]
    fn claude_oauth_preset_resolves_by_its_aliases_and_uses_the_manual_grant() {
        for alias in ["claude-oauth", "claude", "claude-pro", "anthropic-oauth"] {
            let preset = presets::get(alias).unwrap_or_else(|| panic!("no preset for {alias}"));
            assert_eq!(preset.id, "claude-oauth");
            assert_eq!(preset.dialect, crate::config::Dialect::Anthropic);
        }
        let oauth = presets::get("claude-oauth").unwrap().oauth();
        assert!(
            uses_manual_grant(&oauth),
            "Anthropic's redirect is a hosted page, not a loopback listener"
        );
        assert!(oauth.scopes.iter().any(|scope| scope == "user:inference"));
    }
}
