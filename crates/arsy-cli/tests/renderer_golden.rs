//! What the renderer paints today, byte for byte.
//!
//! The TUI is being split into its own presentation crate and given a second
//! look-and-feel. The existing one has to survive that intact — it is what
//! every operator's muscle memory is trained on, and "it looks about the same"
//! is not a thing a reviewer can check. So this captures the real output of
//! every widget at a fixed width, with colour on and off, into a file beside
//! the test, and fails when a byte of it changes.
//!
//! Regenerate deliberately, never casually: `ARSY_GOLDEN=overwrite cargo test
//! -p arsy-cli --features tui --test renderer_golden`, then read the diff. A
//! change here is a change every operator sees.
//!
//! The `COLOUR OFF` half is the visible text, with no escapes in it at all.
//! When a refactor moves where an escape run starts or ends — the same colour
//! written as one run instead of two — that half does not move, which is what
//! separates "painted differently" from "looks different".
#![cfg(feature = "tui")]

use arsy_cli::tui;
use std::{fmt::Write as _, path::PathBuf, time::Duration};

/// One terminal width for every case, so a golden file can be eyeballed and a
/// wrapping change shows up as a diff rather than as a reflow everywhere.
const WIDTH: usize = 72;

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("classic-renderer.txt")
}

/// A section header and the rendered block under it, so a failing diff says
/// which widget moved rather than only which line.
fn section(out: &mut String, title: &str, body: &str) {
    let _ = writeln!(out, "=== {title}");
    let _ = writeln!(out, "{}", body.trim_end_matches('\n'));
    let _ = writeln!(out);
}

fn render_all(colour: bool) -> String {
    let mut out = String::new();
    let quick = Duration::from_millis(88);

    section(
        &mut out,
        "bash_box · success",
        &tui::bash_box(
            WIDTH,
            colour,
            "pnpm test checkout",
            "PASS src/checkout/request.test.ts (7)\n10 passed",
            Some(0),
            quick,
        ),
    );
    section(
        &mut out,
        "bash_box · failure",
        &tui::bash_box(
            WIDTH,
            colour,
            "cargo build",
            "error: could not compile `arsy-cli`",
            Some(1),
            quick,
        ),
    );
    section(
        &mut out,
        "bash_box · command wider than the card",
        &tui::bash_box(
            WIDTH,
            colour,
            "cargo clippy --workspace --all-targets --all-features --locked -- -D warnings",
            "",
            Some(0),
            quick,
        ),
    );
    section(
        &mut out,
        "tool_card · fs.read",
        &tui::tool_card(
            WIDTH,
            colour,
            "fs.read",
            "AGENTS.md",
            "1  # Instructions\n2  Follow them.",
            true,
            quick,
        ),
    );
    section(
        &mut out,
        "tool_card · fs.edit",
        &tui::tool_card(
            WIDTH,
            colour,
            "fs.edit",
            "src/checkout/request.ts",
            "- await chargeGateway(order)\n+ await chargeGateway(order, opts)",
            true,
            quick,
        ),
    );
    section(
        &mut out,
        "tool_card · failed",
        &tui::tool_card(
            WIDTH,
            colour,
            "code.references",
            "chargeGateway",
            "No such file or directory (os error 2)",
            false,
            quick,
        ),
    );
    section(
        &mut out,
        "thinking_box",
        &tui::thinking_box(
            WIDTH,
            colour,
            "The gateway call has no deadline of its own, so it inherits the outer one.",
        ),
    );
    section(
        &mut out,
        "diff_row",
        &tui::diff_row(colour, "src/checkout/request.ts", 4, 2),
    );
    section(&mut out, "assistant_header", &tui::assistant_header(colour));
    section(
        &mut out,
        "assistant_row",
        &tui::assistant_row(colour, "The gateway call now carries its own deadline."),
    );
    section(
        &mut out,
        "assistant_row · markdown is printed as written",
        &tui::assistant_row(colour, "**bold**, `code`, and a # heading"),
    );

    let approval = tui::AskDialogState::for_approval(
        "process.exec",
        "pnpm test checkout · outbound network",
        "the integration tests reach the gateway sandbox over the network",
        None,
    );
    section(&mut out, "approval card", &approval.render(WIDTH, colour));

    let mut arrowed = tui::AskDialogState::for_approval(
        "fs.edit",
        "src/checkout/request.ts",
        "writes outside the workspace",
        Some("- await chargeGateway(order)\n+ await chargeGateway(order, opts)".to_owned()),
    );
    arrowed.selected = 1;
    section(
        &mut out,
        "approval card · a diff preview, second answer arrowed onto",
        &arrowed.render(WIDTH, colour),
    );
    out
}

#[test]
fn the_classic_renderer_paints_exactly_what_it_painted_before() {
    let mut captured = String::from(
        "# The classic renderer, captured before the arsy-tui extraction.\n\
         # Regenerate with ARSY_GOLDEN=overwrite; a diff here is a diff every\n\
         # operator sees. Escapes are shown as \\e so the file stays readable.\n\n",
    );
    for (label, colour) in [("COLOUR OFF", false), ("COLOUR ON", true)] {
        let _ = writeln!(captured, "######## {label}\n");
        captured.push_str(&render_all(colour).replace('\x1b', "\\e"));
    }

    let path = golden_path();
    if std::env::var("ARSY_GOLDEN").as_deref() == Ok("overwrite") {
        std::fs::create_dir_all(path.parent().expect("the golden file has a parent"))
            .expect("the golden directory is writable");
        std::fs::write(&path, &captured).expect("the golden file is writable");
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "{}: {error}\nrun `ARSY_GOLDEN=overwrite cargo test -p arsy-cli --features tui \
             --test renderer_golden` to create it",
            path.display()
        )
    });

    if expected != captured {
        // The first differing line, because a whole-file dump of escape codes
        // tells a reader nothing about which widget moved.
        let mismatch = expected
            .lines()
            .zip(captured.lines())
            .enumerate()
            .find(|(_, (want, got))| want != got);
        match mismatch {
            Some((index, (want, got))) => panic!(
                "the renderer changed at line {}:\n  was: {want}\n  now: {got}",
                index + 1
            ),
            None => panic!(
                "the renderer changed length: {} lines before, {} now",
                expected.lines().count(),
                captured.lines().count()
            ),
        }
    }
}
