//! Wrapping holds its two promises across every shape of input.
//!
//! These drive it over a spread of widths and awkward texts and assert the only two things a caller can actually rely on: no row
//! is wider than the width it asked for, and no text goes missing.

use arsy_tui::{wrap, Line, Role};

/// Every input, wrapped at every width from 1 to 40, must keep both promises.
#[test]
fn no_row_overflows_and_no_text_is_lost() {
    let cases = [
        "",
        " ",
        "   ",
        "a",
        "one two three",
        "the gateway call inherited the outer request deadline and never retried",
        "https://example.internal/a/very/long/path/with/no/spaces/at/all/whatsoever",
        "trailing spaces      ",
        "      leading spaces",
        "double  spaces  between  words",
        "日本語のテキストです",
        "mixed 日本語 and latin text together",
        "ends-with-a-very-long-unbreakable-token-aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "a b c d e f g h i j k l m n o p q r s t u v w x y z",
    ];

    for case in cases {
        for width in 1..=40 {
            let line = Line::of(case, Role::Plain);
            let rows = wrap(&line, width);

            assert!(
                !rows.is_empty(),
                "wrapping {case:?} at {width} produced no rows at all"
            );
            assert!(
                rows.len() == 1 || rows.iter().all(|row| !row.text().is_empty()),
                "wrapping {case:?} at {width} produced an empty row: {:?}",
                rows.iter().map(Line::text).collect::<Vec<_>>()
            );
            for row in &rows {
                assert!(
                    row.width() <= width,
                    "wrapping {case:?} at {width} produced a {}-column row {:?}",
                    row.width(),
                    row.text()
                );
            }

            // Spaces are where breaks happen, so a break may consume one, and
            // a glyph too wide for the whole width has nowhere to go at all.
            // Nothing else may go missing, be duplicated, or be reordered.
            let keep = |c: &char| {
                !c.is_whitespace()
                    && unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0) <= width
            };
            let before: String = case.chars().filter(keep).collect();
            let after: String = rows
                .iter()
                .flat_map(|row| row.text().chars().collect::<Vec<_>>())
                .filter(|c| !c.is_whitespace())
                .collect();
            assert_eq!(
                before, after,
                "wrapping {case:?} at {width} changed the text"
            );
        }
    }
}

/// A row built from several spans wraps by the whole row's width, not each
/// span's, and every span's style survives into whichever row it lands on.
#[test]
fn wrapping_spans_keeps_every_style() {
    let line = Line::of("alpha beta ", Role::Ok)
        .push("gamma delta ", Role::Err)
        .push("epsilon zeta", Role::Accent);

    for width in 1..=40 {
        let rows = wrap(&line, width);
        for row in &rows {
            assert!(row.width() <= width, "{:?}", row.text());
        }
        let text: String = rows
            .iter()
            .flat_map(|row| row.text().chars().collect::<Vec<_>>())
            .filter(|c| !c.is_whitespace())
            .collect();
        assert_eq!(text, "alphabetagammadeltaepsilonzeta", "at width {width}");

        let roles: Vec<Role> = rows
            .iter()
            .flat_map(|row| row.spans.iter().map(|span| span.style.role))
            .collect();
        for role in [Role::Ok, Role::Err, Role::Accent] {
            assert!(roles.contains(&role), "{role:?} lost at width {width}");
        }
    }
}

/// An emoji with a variation selector, a skin tone, or joiners is one glyph:
/// measured as one, kept on one row, and never cut in half by a fit.
#[test]
fn emoji_sequences_are_measured_and_kept_whole() {
    let cases = [
        "\u{26A0}\u{FE0F}\u{26A0}\u{FE0F}\u{26A0}\u{FE0F}\u{26A0}\u{FE0F}",
        "ok \u{2714}\u{FE0F} done \u{2764}\u{FE0F} yes",
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} family",
        "\u{1F44D}\u{1F3FD}\u{1F44D}\u{1F3FD}\u{1F44D}\u{1F3FD}",
    ];
    for case in cases {
        for width in 2..=20 {
            let line = Line::of(case, Role::Plain);
            let rows = wrap(&line, width);
            for row in &rows {
                assert!(
                    row.width() <= width,
                    "{case:?} at {width}: {:?}",
                    row.text()
                );
                assert!(
                    !row.text().ends_with('\u{200D}'),
                    "{case:?} split at {width}"
                );
                assert!(
                    !row.text().starts_with('\u{FE0F}'),
                    "{case:?} split at {width}"
                );
            }
            let joined: String = rows
                .iter()
                .map(Line::text)
                .collect::<String>()
                .replace(' ', "");
            assert_eq!(joined, case.replace(' ', ""), "{case:?} at {width}");

            let fitted = line.fit(width);
            assert!(
                fitted.width() <= width,
                "{case:?} fit {width}: {:?}",
                fitted.text()
            );
        }
    }
}
