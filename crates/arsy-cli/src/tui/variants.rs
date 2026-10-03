//! Models an endpoint lists once per reasoning effort.
//!
//! Some endpoints — Google Antigravity is the one this was written for — list
//! `gemini-3.8-flash-low`, `-medium` and `-high` as three models, where the
//! suffix is the effort. Offered as three models, one can be picked with a
//! different effort set beside it, and the two contradict each other. Grouped
//! here, the base model is offered once, the effort picks the variant, and
//! the request goes to the variant that effort names.
use super::*;

/// The suffixes that name an effort level, and the level each names.
const SUFFIXES: [(&str, Effort); 3] = [
    ("-low", Effort::Low),
    ("-medium", Effort::Medium),
    ("-high", Effort::High),
];

/// A slug's base and the effort its suffix names, when it has one.
///
/// `-extra-low` is not `-low` on an `-extra` model: it is a level of its own
/// with no effort to match it, so that slug stays a model of its own.
fn suffixed(slug: &str) -> Option<(&str, Effort)> {
    SUFFIXES.iter().find_map(|(suffix, effort)| {
        slug.strip_suffix(suffix)
            .filter(|base| !base.is_empty() && !base.ends_with("-extra"))
            .map(|base| (base, *effort))
    })
}

/// The effort levels `base` is listed with, lowest first; empty when `base`
/// is an ordinary model that takes any effort.
pub fn variant_levels(models: &[String], base: &str) -> Vec<Effort> {
    let mut levels: Vec<Effort> = models
        .iter()
        .filter_map(|slug| suffixed(slug).filter(|(stem, _)| *stem == base))
        .map(|(_, effort)| effort)
        .collect();
    levels.sort_by_key(|effort| Effort::ALL.iter().position(|level| level == effort));
    levels.dedup();
    levels
}

/// The list with every effort variant folded into its base, in the place the
/// family first appears, and each model's effort levels beside it.
pub fn collapse_variants(models: &[String]) -> Vec<(String, Vec<Effort>)> {
    let mut collapsed: Vec<(String, Vec<Effort>)> = Vec::new();
    for slug in models {
        let name = suffixed(slug).map_or(slug.as_str(), |(base, _)| base);
        if collapsed.iter().any(|(seen, _)| seen == name) {
            continue;
        }
        let levels = variant_levels(models, name);
        collapsed.push((name.to_owned(), levels));
    }
    collapsed
}

/// A variant slug as its base and the effort it names, when `slug` is one of
/// a family the list holds.
pub fn split_variant(models: &[String], slug: &str) -> Option<(String, Effort)> {
    let (base, effort) = suffixed(slug)?;
    variant_levels(models, base)
        .contains(&effort)
        .then(|| (base.to_owned(), effort))
}

/// What a family listed once per effort offers: its own levels and no `off`,
/// since every model it lists runs at one of them.
pub fn family_profile(levels: &[Effort]) -> EffortProfile {
    EffortProfile::of(levels, true)
}

/// The slug a request for `base` at `effort` is sent to: the variant the
/// effort clamps to, or `base` itself when it is not a family.
pub fn variant_for(models: &[String], base: &str, effort: Option<Effort>) -> String {
    let levels = variant_levels(models, base);
    match family_profile(&levels).clamp(effort) {
        Some(effort) => format!("{base}-{}", effort.as_str()),
        None => base.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Antigravity's `fetchAvailableModels` answered on a real account.
    fn antigravity() -> Vec<String> {
        [
            "claude-sonnet-4-6",
            "gemini-3.1-pro-high",
            "gemini-3.1-pro-low",
            "gemini-3.5-flash-extra-low",
            "gemini-3.5-flash-low",
            "gemini-3.8-flash-high",
            "gemini-3.8-flash-low",
            "gemini-3.8-flash-medium",
            "gemini-3.8-flash-tiered",
            "gpt-oss-120b-medium",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    #[test]
    fn variants_fold_into_their_base_with_its_levels() {
        let collapsed = collapse_variants(&antigravity());
        let names: Vec<&str> = collapsed.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "claude-sonnet-4-6",
                "gemini-3.1-pro",
                "gemini-3.5-flash-extra-low",
                "gemini-3.5-flash",
                "gemini-3.8-flash",
                "gemini-3.8-flash-tiered",
                "gpt-oss-120b",
            ]
        );
        let levels = |name: &str| {
            collapsed
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, levels)| levels.clone())
                .unwrap()
        };
        assert_eq!(
            levels("gemini-3.8-flash"),
            [Effort::Low, Effort::Medium, Effort::High]
        );
        assert_eq!(levels("gemini-3.1-pro"), [Effort::Low, Effort::High]);
        assert!(levels("claude-sonnet-4-6").is_empty());
        assert!(levels("gemini-3.8-flash-tiered").is_empty());
    }

    #[test]
    fn the_effort_picks_the_variant_a_request_goes_to() {
        let models = antigravity();
        assert_eq!(
            variant_for(&models, "gemini-3.8-flash", Some(Effort::High)),
            "gemini-3.8-flash-high"
        );
        // Pro has no medium: the level below is taken, never the dearer one
        // above, and off runs at the family's highest up to high.
        assert_eq!(
            variant_for(&models, "gemini-3.1-pro", Some(Effort::Medium)),
            "gemini-3.1-pro-low"
        );
        assert_eq!(
            variant_for(&models, "gemini-3.1-pro", None),
            "gemini-3.1-pro-high"
        );
        assert_eq!(
            variant_for(&models, "claude-sonnet-4-6", Some(Effort::High)),
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn a_saved_variant_splits_into_base_and_effort() {
        let models = antigravity();
        assert_eq!(
            split_variant(&models, "gemini-3.8-flash-high"),
            Some(("gemini-3.8-flash".to_owned(), Effort::High))
        );
        assert_eq!(split_variant(&models, "gemini-3.8-flash-tiered"), None);
        assert_eq!(split_variant(&models, "claude-sonnet-4-6"), None);
    }

    #[test]
    fn a_family_offers_only_its_own_levels() {
        assert_eq!(
            family_profile(&[Effort::Low, Effort::High]).choices(),
            [Some(Effort::Low), Some(Effort::High)]
        );
        assert_eq!(family_profile(&[]).clamp(None), None);
    }
}
