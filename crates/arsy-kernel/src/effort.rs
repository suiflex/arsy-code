//! Which reasoning efforts a model takes.
//!
//! Models differ: one takes no effort at all, one takes `low` and `high` only,
//! one cannot reason with the knob off. Offering every level to every model
//! sent a field some hosts reject and others silently ignore, and let an
//! operator pick a level that did nothing. A profile says what one model
//! offers, and a requested level is clamped to it before anything is sent.

use crate::provider::Effort;

/// The efforts one model offers, in ladder order.
///
/// Empty means the model takes no effort, and nothing is sent.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EffortProfile {
    pub efforts: Vec<Effort>,
    /// The model always reasons: `off` is not offered.
    pub requires: bool,
}

impl EffortProfile {
    /// A model that takes no effort.
    pub const fn none() -> Self {
        Self {
            efforts: Vec::new(),
            requires: false,
        }
    }

    pub fn of(efforts: &[Effort], requires: bool) -> Self {
        let mut efforts = efforts.to_vec();
        efforts.sort_unstable();
        efforts.dedup();
        Self { efforts, requires }
    }

    /// What is offered before any model is chosen: the common three and `off`.
    pub fn unrouted() -> Self {
        Self::of(&[Effort::Low, Effort::Medium, Effort::High], false)
    }

    pub fn takes_effort(&self) -> bool {
        !self.efforts.is_empty()
    }

    /// What Ctrl+T steps through: each level, then `off` unless the model
    /// always reasons. A model without the knob offers `off` alone.
    pub fn choices(&self) -> Vec<Option<Effort>> {
        let mut choices: Vec<Option<Effort>> = self.efforts.iter().copied().map(Some).collect();
        if !self.requires || choices.is_empty() {
            choices.push(None);
        }
        choices
    }

    /// The level a request for `wanted` gets: `wanted` when offered, else the
    /// highest offered level below it, else the lowest offered. Never rounded
    /// up, so a clamp never spends more than was asked for. `off` on a model
    /// that always reasons gets the highest level up to `high`.
    pub fn clamp(&self, wanted: Option<Effort>) -> Option<Effort> {
        let lowest = *self.efforts.first()?;
        let wanted = match wanted {
            Some(wanted) => wanted,
            None if self.requires => Effort::High,
            None => return None,
        };
        Some(
            self.efforts
                .iter()
                .copied()
                .filter(|level| *level <= wanted)
                .max()
                .unwrap_or(lowest),
        )
    }
}

use Effort::{High, Low, Max, Medium, Minimal, XHigh};

const LOW_TO_HIGH: &[Effort] = &[Low, Medium, High];
const MINIMAL_TO_HIGH: &[Effort] = &[Minimal, Low, Medium, High];
const MINIMAL_TO_XHIGH: &[Effort] = &[Minimal, Low, Medium, High, XHigh];
const LOW_TO_MAX: &[Effort] = &[Low, Medium, High, XHigh, Max];
const LOW_HIGH_MAX: &[Effort] = &[Low, High, Max];

/// Model families and what they offer, most specific prefix first.
///
/// Matched against the model id lower-cased, after any `vendor/` prefix, with
/// `.` read as `-` so `claude-opus-4.7` and `claude-opus-4-7` are one family.
/// The levels are the ones most providers list for the family in oh-my-pi's
/// bundled catalog (`packages/catalog/src/models.json`); a host that differs
/// is declared under `efforts` in its endpoint configuration.
const FAMILIES: &[(&str, &[Effort], bool)] = &[
    ("claude-opus-4-6", &[Low, Medium, High, Max], false),
    ("claude-opus-4-7", LOW_TO_MAX, false),
    ("claude-opus-4-8", LOW_TO_MAX, false),
    ("claude-opus-5", LOW_TO_MAX, false),
    ("claude-sonnet-5", LOW_TO_MAX, false),
    ("claude-fable-5", LOW_TO_MAX, false),
    ("claude-opus-4", LOW_TO_HIGH, false),
    ("claude-sonnet-4", LOW_TO_HIGH, false),
    ("claude-haiku-4", LOW_TO_HIGH, false),
    ("claude-3-7", LOW_TO_HIGH, false),
    ("gpt-5-6", LOW_TO_MAX, false),
    ("gpt-6", LOW_TO_MAX, false),
    ("gpt-5", MINIMAL_TO_HIGH, false),
    ("gpt-4", &[], false),
    ("gpt-3", &[], false),
    ("o1", LOW_TO_HIGH, true),
    ("o3", LOW_TO_HIGH, true),
    ("o4", LOW_TO_HIGH, true),
    ("deepseek-v4", LOW_HIGH_MAX, false),
    ("kimi-k3", LOW_HIGH_MAX, true),
    ("kimi-k2", MINIMAL_TO_HIGH, false),
    ("glm-5", LOW_HIGH_MAX, true),
    ("qwen3", MINIMAL_TO_HIGH, false),
    ("minimax-m3", MINIMAL_TO_XHIGH, false),
    ("gemini-2-5-pro", MINIMAL_TO_HIGH, true),
    ("gemini-2-5-flash", MINIMAL_TO_HIGH, false),
];

/// What a model offers when nothing more specific is known about it, or
/// `None` for a model this table does not know — which takes no effort until
/// its endpoint declares one.
pub fn builtin(model: &str) -> Option<EffortProfile> {
    let id = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase()
        .replace('.', "-");
    // Gemini 3 names its families by size; Pro takes two levels, the rest four.
    if id.starts_with("gemini-3") {
        let levels: &[Effort] = if id.contains("-pro") {
            &[Low, High]
        } else {
            MINIMAL_TO_HIGH
        };
        return Some(EffortProfile::of(levels, true));
    }
    FAMILIES
        .iter()
        .find(|(family, _, _)| id.starts_with(family))
        .map(|(_, levels, requires)| EffortProfile::of(levels, *requires))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clamp_never_rounds_up() {
        let profile = EffortProfile::of(&[Low, High, Max], false);
        assert_eq!(profile.clamp(Some(Medium)), Some(Low));
        assert_eq!(profile.clamp(Some(XHigh)), Some(High));
        assert_eq!(
            profile.clamp(Some(Minimal)),
            Some(Low),
            "the lowest when none is below"
        );
        assert_eq!(profile.clamp(Some(Max)), Some(Max));
        assert_eq!(profile.clamp(None), None);
        assert_eq!(profile.choices(), [Some(Low), Some(High), Some(Max), None]);
    }

    #[test]
    fn a_model_that_always_reasons_offers_no_off() {
        let profile = EffortProfile::of(&[Low, High, Max], true);
        assert_eq!(profile.clamp(None), Some(High));
        assert_eq!(profile.choices(), [Some(Low), Some(High), Some(Max)]);
    }

    #[test]
    fn a_model_without_the_knob_takes_nothing() {
        let profile = EffortProfile::none();
        assert!(!profile.takes_effort());
        assert_eq!(profile.clamp(Some(High)), None);
        assert_eq!(profile.choices(), [None]);
    }

    #[test]
    fn families_resolve_through_vendor_prefixes_and_dotted_versions() {
        let levels = |model: &str| builtin(model).map(|profile| profile.efforts);
        assert_eq!(
            levels("anthropic/claude-opus-4.7"),
            Some(LOW_TO_MAX.to_vec())
        );
        assert_eq!(
            levels("claude-opus-4-6"),
            Some(vec![Low, Medium, High, Max])
        );
        assert_eq!(levels("claude-sonnet-4.6"), Some(LOW_TO_HIGH.to_vec()));
        assert_eq!(levels("openai/gpt-5.6-sol"), Some(LOW_TO_MAX.to_vec()));
        assert_eq!(levels("gpt-5.1"), Some(MINIMAL_TO_HIGH.to_vec()));
        assert_eq!(levels("gpt-4o"), Some(Vec::new()), "known to take none");
        assert_eq!(levels("moonshot/kimi-k3"), Some(LOW_HIGH_MAX.to_vec()));
        assert_eq!(levels("gemini/gemini-3.1-pro"), Some(vec![Low, High]));
        assert_eq!(levels("gemini-3.8-flash"), Some(MINIMAL_TO_HIGH.to_vec()));
        assert_eq!(levels("vikey/plan"), None, "an unknown router alias");
    }
}
