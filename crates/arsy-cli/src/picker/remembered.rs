//! The remembered `/model`, `/effort` and `/theme` choices: where they live
//! beside the user configuration, and how a session reads them back.

#[cfg(feature = "tui")]
use crate::*;
use arsy_kernel::provider::Effort;
use std::io::{self, Write};
use std::path::PathBuf;
/// Persist the picked model, reporting only that persistence failed — the
/// choice still applies to this session.
#[cfg(feature = "tui")]
pub(crate) fn remember_model(route: &tui::ModelRoute, emitter: &mut Emitter) {
    if let Err(error) = save_route(route) {
        emitter.diagnostic(&Diagnostic::warning(
            "ARSY-UIX-1001",
            format!("the model choice was not remembered: {error}"),
            "check that the ARSY user configuration directory is writable",
        ));
    }
}

#[cfg(feature = "tui")]
pub(crate) fn remember_effort(effort: Option<Effort>, emitter: &mut Emitter) {
    if let Err(error) = save_effort(effort) {
        emitter.diagnostic(&Diagnostic::warning(
            "ARSY-UIX-1001",
            format!("the effort choice was not remembered: {error}"),
            "check that the ARSY user configuration directory is writable",
        ));
    }
}

/// The remembered model lives in the configuration home's `state` directory,
/// with the other choices the terminal remembers.
#[cfg(feature = "tui")]
fn model_store() -> Option<PathBuf> {
    arsy_kernel::config::home_file(arsy_kernel::config::STATE_DIRECTORY, "model")
}

/// The route chosen last time, as `provider/model`.
///
/// The model is re-validated on read: a file written by an older build that
/// accepted anything must not keep selecting an unusable model on every later
/// start.
#[cfg(feature = "tui")]
pub(crate) fn saved_route() -> Option<tui::ModelRoute> {
    let raw = std::fs::read_to_string(model_store()?).ok()?;
    let raw = raw.trim();
    let route = (!raw.is_empty())
        .then(|| tui::ModelRoute::parse(raw))
        .flatten()?;
    tui::validate_slug(&route.model).ok()?;
    Some(route)
}

#[cfg(feature = "tui")]
pub(crate) fn save_route(route: &tui::ModelRoute) -> io::Result<()> {
    let path = model_store()
        .ok_or_else(|| io::Error::other("this platform has no user configuration directory"))?;
    replace_file(&path, format!("{route}\n").as_bytes())
}

/// The remembered reasoning effort, beside the remembered model.
#[cfg(feature = "tui")]
#[cfg(feature = "tui")]
fn effort_store() -> Option<PathBuf> {
    arsy_kernel::config::home_file(arsy_kernel::config::STATE_DIRECTORY, "effort")
}

/// The effort chosen last time, re-validated on read for the same reason the
/// model is: an unreadable file must not decide what a turn sends.
#[cfg(feature = "tui")]
pub(crate) fn saved_effort() -> Option<Effort> {
    Effort::parse(std::fs::read_to_string(effort_store()?).ok()?.trim())
}

/// `None` clears the choice, so a turn goes back to carrying no reasoning knob.
#[cfg(feature = "tui")]
pub(crate) fn save_effort(effort: Option<Effort>) -> io::Result<()> {
    let path = effort_store()
        .ok_or_else(|| io::Error::other("this platform has no user configuration directory"))?;
    match effort {
        Some(effort) => replace_file(&path, format!("{effort}\n").as_bytes()),
        None => match std::fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        },
    }
}

/// The remembered colour theme, beside the remembered effort.
#[cfg(feature = "tui")]
fn theme_store() -> Option<PathBuf> {
    arsy_kernel::config::home_file(arsy_kernel::config::STATE_DIRECTORY, "theme")
}

/// The theme chosen last time, kept only if it is still a built-in name: a
/// file written by a build that knew a theme this one dropped must not select
/// nothing.
#[cfg(feature = "tui")]
pub(crate) fn saved_theme() -> Option<String> {
    let raw = std::fs::read_to_string(theme_store()?).ok()?;
    let name = raw.trim().to_owned();
    tui::builtin_palette(&name).map(|_| name)
}

#[cfg(feature = "tui")]
pub(crate) fn save_theme(name: &str) -> io::Result<()> {
    let path = theme_store()
        .ok_or_else(|| io::Error::other("this platform has no user configuration directory"))?;
    replace_file(&path, format!("{name}\n").as_bytes())
}

#[cfg(feature = "tui")]
pub(crate) fn remember_theme(name: &str, emitter: &mut Emitter) {
    if let Err(error) = save_theme(name) {
        emitter.diagnostic(&Diagnostic::warning(
            "ARSY-UIX-1001",
            format!("the theme choice was not remembered: {error}"),
            "check that the ARSY user configuration directory is writable",
        ));
    }
}

#[cfg(feature = "tui")]
pub(crate) fn apply_theme(
    answer: &str,
    current: &mut String,
    roles: &std::collections::BTreeMap<String, String>,
    stdout: &mut io::Stdout,
    emitter: &mut Emitter,
) -> io::Result<bool> {
    let picked = match tui::resolve_theme_answer(answer, current) {
        Ok(picked) => picked,
        Err(reason) => {
            writeln!(stdout, "{}", tui::safe_text(&reason))?;
            return Ok(false);
        }
    };
    tui::set_palette(&picked, roles);
    *current = picked;
    remember_theme(current, emitter);
    writeln!(stdout, "Theme: {current}")?;
    Ok(true)
}

/// The models `provider`'s endpoint lists, as configured, variants and all.
#[cfg(feature = "tui")]
pub(crate) fn endpoint_slugs(invocation: &Invocation, provider: &str) -> Vec<String> {
    crate::provider::configuration(invocation)
        .ok()
        .and_then(|config| {
            config
                .endpoints()
                .find(|endpoint| endpoint.id == provider)
                .map(|endpoint| endpoint.models.clone())
        })
        .unwrap_or_default()
}

/// Models a provider refused an effort for during this session, as
/// `provider/model`. Learned rather than configured, so it is not saved: a
/// host that adds reasoning later gets it back on the next start.
#[cfg(feature = "tui")]
static NO_EFFORT: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Bumped on every model learned into `NO_EFFORT`, so the prompt loop can tell
/// that the footer it drew is out of date.
#[cfg(feature = "tui")]
static NO_EFFORT_LEARNED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(feature = "tui")]
pub(crate) fn learn_no_effort(provider: &str, model: &str) {
    let mut learned = NO_EFFORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if learned.insert(format!("{provider}/{model}")) {
        NO_EFFORT_LEARNED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(feature = "tui")]
pub(crate) fn no_effort_learned() -> usize {
    NO_EFFORT_LEARNED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The efforts `model` on `endpoint` takes, from the first source that says:
/// a refusal learned this session, the endpoint's `efforts` configuration, a
/// family the endpoint lists once per effort, then the built-in table. A
/// model none of them knows takes no effort, rather than being sent a field
/// its host may reject or silently ignore.
#[cfg(feature = "tui")]
pub(crate) fn effort_profile(
    endpoint: &arsy_kernel::config::Endpoint,
    model: &str,
) -> tui::EffortProfile {
    let refused = NO_EFFORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&format!("{}/{model}", endpoint.id));
    if refused {
        return tui::EffortProfile::none();
    }
    if let Some(configured) = endpoint.configured_effort(model) {
        return configured;
    }
    let levels = tui::variant_levels(&endpoint.models, model);
    if !levels.is_empty() {
        return tui::family_profile(&levels);
    }
    arsy_kernel::effort::builtin(model).unwrap_or_default()
}

/// What the routed model takes; see [`effort_profile`]. A route whose
/// endpoint cannot be read is judged on the model's name alone.
#[cfg(feature = "tui")]
pub(crate) fn route_effort(invocation: &Invocation, route: &tui::ModelRoute) -> tui::EffortProfile {
    if route.model.is_empty() {
        return tui::EffortProfile::unrouted();
    }
    crate::provider::configuration(invocation)
        .ok()
        .and_then(|config| {
            config
                .endpoints()
                .find(|endpoint| endpoint.id == route.provider)
                .map(|endpoint| effort_profile(endpoint, &route.model))
        })
        .unwrap_or_else(|| arsy_kernel::effort::builtin(&route.model).unwrap_or_default())
}

#[cfg(feature = "tui")]
pub(crate) fn endpoint_models(invocation: &Invocation) -> Vec<tui::ModelChoice> {
    crate::provider::configuration(invocation)
        .map(|config| {
            config
                .endpoints()
                .flat_map(|endpoint| {
                    tui::collapse_variants(&endpoint.models)
                        .into_iter()
                        .map(move |(slug, _)| tui::ModelChoice {
                            provider: endpoint.id.clone(),
                            effort: effort_profile(endpoint, &slug),
                            slug,
                            name: format!("on {}", endpoint.id),
                        })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The palette the session paints with: a built-in base — the `[theme]` base,
/// else the remembered theme, else the default — with any `[theme]` role
/// overrides on top. Returns the base name (for the `/theme` picker) and the
/// palette, or the reason an override was rejected.
#[cfg(feature = "tui")]
pub(crate) fn resolve_palette(
    theme: &arsy_kernel::config::Theme,
) -> (String, Result<tui::Palette, String>) {
    let base = theme
        .base
        .clone()
        .or_else(saved_theme)
        .unwrap_or_else(|| tui::DEFAULT_THEME.to_owned());
    let palette = tui::builtin_palette(&base).unwrap_or_else(|| {
        tui::builtin_palette(tui::DEFAULT_THEME).expect("the default theme is built in")
    });
    let built = if theme.roles.is_empty() {
        Ok(palette)
    } else {
        palette.with_overrides(&theme.roles)
    };
    (base, built)
}
