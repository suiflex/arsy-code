//! Model choices and model picker rendering.
use super::*;
/// A model one provider offers. The provider rides on the row, so the picker
/// lists every configured provider's models and a single answer can move the
/// route to another provider as well as to another model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelChoice {
    pub provider: String,
    pub slug: String,
    pub name: String,
    /// The effort levels this model is listed with as separate variants;
    /// empty for a model that takes any effort.
    pub levels: Vec<Effort>,
}

/// Offer every configured provider's models, grouped under their provider, by
/// number; `current` is marked where it appears.
///
/// Accepts a list index, a slug typed in full, or an empty line to keep
/// `current`.
pub fn render_model_list(
    writer: &mut impl Write,
    models: &[ModelChoice],
    current: &ModelRoute,
    colour: bool,
) -> std::io::Result<()> {
    // The current row wins the mark even if two providers list one slug.
    let selected = models
        .iter()
        .position(|choice| choice.provider == current.provider && choice.slug == current.model);
    let mut last_provider: Option<&str> = None;
    for (index, choice) in models.iter().enumerate() {
        if last_provider != Some(choice.provider.as_str()) {
            writeln!(
                writer,
                "{}",
                paint(colour, sgr_accent(), &format!("  [{}]", choice.provider))
            )?;
            last_provider = Some(&choice.provider);
        }
        let marker = if Some(index) == selected { "›" } else { " " };
        writeln!(
            writer,
            "    {} {} {}  {}",
            paint(colour, sgr_accent(), marker),
            paint(colour, sgr_dim(), &format!("{}.", index + 1)),
            paint(colour, sgr_model(), &choice.slug),
            paint(colour, sgr_dim(), &choice.name),
        )?;
    }
    Ok(())
}

/// The rows the effort picker offers, in the order it numbers them.
pub fn effort_choices() -> Vec<Option<Effort>> {
    let mut choices: Vec<Option<Effort>> = Effort::ALL.into_iter().map(Some).collect();
    choices.push(None);
    choices
}

/// Which offered row the mark starts on, so the picker opens on what is set.
pub fn effort_row(current: Option<Effort>) -> usize {
    effort_choices()
        .iter()
        .position(|choice| *choice == current)
        .unwrap_or(0)
}

pub fn effort_prompt(current: Option<Effort>, colour: bool) -> String {
    let current = current.map_or_else(|| "off".to_owned(), |effort| effort.to_string());
    paint(
        colour,
        sgr_dim(),
        &format!(
            "  effort [{current}] · Up/Down then Enter, a name, or 1-{}",
            effort_choices().len()
        ),
    )
}

/// Take an answer to the effort picker: a list number, a level name, `off`, or
/// an empty line to keep what is set.
///
/// Rejected answers report why, for the same reason the model picker does: an
/// accepted answer is written to the user configuration.
pub fn resolve_effort_answer(
    line: &str,
    current: Option<Effort>,
) -> Result<Option<Effort>, String> {
    let answer = line.trim();
    if answer.is_empty() {
        return Ok(current);
    }
    if let Ok(number) = answer.parse::<usize>() {
        return effort_choices()
            .get(
                number
                    .checked_sub(1)
                    .ok_or_else(|| format!("`{answer}` is out of range; the list starts at 1"))?,
            )
            .copied()
            .ok_or_else(|| format!("`{answer}` is not on the list"));
    }
    match answer {
        "off" | "none" | "unset" => Ok(None),
        _ => Effort::parse(answer).map(Some).ok_or_else(|| {
            format!(
                "`{}` is not an effort level; use {}, or off",
                safe_text(answer),
                Effort::ALL.map(Effort::as_str).join(", "),
            )
        }),
    }
}

/// The rows the model picker offers, in the order it numbers them.
pub fn model_rows(
    models: &[ModelChoice],
    current: &ModelRoute,
) -> (Option<Vec<(String, String)>>, usize) {
    if models.is_empty() {
        return (None, 0);
    }
    let selected = models
        .iter()
        .position(|choice| choice.provider == current.provider && choice.slug == current.model)
        .unwrap_or(0);
    let rows = models
        .iter()
        .map(|choice| {
            let label = format!("[{}] {}", choice.provider, choice.slug);
            let desc = if choice.name.is_empty() || choice.name == choice.slug {
                format!("on {}", choice.provider)
            } else {
                format!("{} · on {}", choice.name, choice.provider)
            };
            (label, desc)
        })
        .collect();
    (Some(rows), selected)
}

pub fn model_prompt(models: &[ModelChoice], current: &ModelRoute, colour: bool) -> String {
    let choices = if models.is_empty() {
        "a slug".to_owned()
    } else {
        format!("Up/Down then Enter, a name, or 1-{}", models.len())
    };
    paint(
        colour,
        sgr_dim(),
        &format!("  model [{}] · {choices}", current),
    )
}

/// Resolve a picker answer: a list index, a slug typed in full, or an empty
/// line to keep the current model.
///
/// A rejected answer is returned as the sentence to show, because the picker is
/// the only guard before the slug is passed to the provider CLI *and* written to
/// the user configuration: an accepted typo would otherwise fail every later
/// turn, in every later session, with a provider error that names the wrong
/// cause.
pub fn resolve_model(
    answer: &str,
    models: &[ModelChoice],
    current: &ModelRoute,
) -> Result<ModelRoute, String> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(current.clone());
    }
    if let Ok(number) = answer.parse::<usize>() {
        return match number.checked_sub(1).and_then(|index| models.get(index)) {
            // The row names the provider, so one answer can move the turn to
            // another provider and its model at once.
            Some(choice) => Ok(ModelRoute {
                provider: choice.provider.clone(),
                model: choice.slug.clone(),
            }),
            None if models.is_empty() => Err("no models are listed; type a model slug".to_owned()),
            None => Err(format!("no model {number}; choose 1-{}", models.len())),
        };
    }
    // `[provider] model` bracketed notation from the interactive picker.
    if let Some(rest) = answer.strip_prefix('[') {
        if let Some((provider, model_part)) = rest.split_once(']') {
            let model_slug = model_part.trim();
            if let Some(choice) = models
                .iter()
                .find(|c| c.provider == provider && c.slug == model_slug)
            {
                return Ok(ModelRoute {
                    provider: choice.provider.clone(),
                    model: choice.slug.clone(),
                });
            }
            validate_slug(model_slug)?;
            return Ok(ModelRoute {
                provider: provider.to_owned(),
                model: model_slug.to_owned(),
            });
        }
    }
    validate_slug(answer)?;
    // `provider/model` when answering with a qualified name.
    if let Some((provider, slug)) = answer.split_once('/') {
        if let Some(choice) = models
            .iter()
            .find(|c| c.provider == provider && c.slug == slug)
        {
            return Ok(ModelRoute {
                provider: choice.provider.clone(),
                model: choice.slug.clone(),
            });
        }
        validate_slug(slug)?;
        return Ok(ModelRoute {
            provider: provider.to_owned(),
            model: slug.to_owned(),
        });
    }
    // An exact match on a listed slug carries its provider.
    if let Some(choice) = models.iter().find(|c| c.slug == answer) {
        return Ok(ModelRoute {
            provider: choice.provider.clone(),
            model: choice.slug.clone(),
        });
    }
    Ok(ModelRoute {
        provider: current.provider.clone(),
        model: answer.to_owned(),
    })
}
/// Accept what a provider slug can contain and nothing else. The picker shares
/// its line with the composer, so a mistyped slash command arrives here as text.
pub fn validate_slug(slug: &str) -> Result<(), String> {
    // The length is checked first because the messages below quote the answer,
    // and a paste arrives here as one line: bracketed paste turns a whole file
    // into a single composer line, which must not be echoed back in full.
    if slug.chars().count() > 64 {
        return Err("a model slug is at most 64 characters".to_owned());
    }
    if slug.starts_with('/') {
        return Err(format!(
            "`{slug}` is a command, not a model; press Enter to keep the current one"
        ));
    }
    if !slug.chars().any(char::is_alphanumeric)
        || !slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':' | '/'))
    {
        return Err(format!(
            "`{slug}` is not a model slug; use letters, digits, or - . _ : /"
        ));
    }
    Ok(())
}
