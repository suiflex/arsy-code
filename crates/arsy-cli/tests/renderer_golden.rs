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
use std::{fmt::Write as _, path::PathBuf, sync::Mutex, time::Duration};

static STYLE_LOCK: Mutex<()> = Mutex::new(());

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
            Some(quick),
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
            Some(quick),
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
            Some(quick),
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
    section(
        &mut out,
        "assistant_block · markdown response",
        &tui::assistant_block(
            WIDTH,
            colour,
            "# Checkout\n\n**Ready** with `deadline`.\n\n```toml\nretries = 2\n```",
        ),
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
    let _style_lock = STYLE_LOCK.lock().unwrap_or_else(|held| held.into_inner());
    tui::set_render_style(tui::RenderStyle::Classic);
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

/// The modern style draws the mockup: tinted panels, no line-drawing.
///
/// This test has twice asserted the wrong shape — first that a card had no
/// border when the mockup shows panels, then that it *had* one when the mockup
/// shows none. The mockup's tool cards are blocks of colour: a header strip
/// with the name and a right-aligned duration, body rows on a darker tint, and
/// nothing drawn with `╭ │ ╰` at all.
#[test]
fn modern_renderer_uses_the_mockup_transcript_language() {
    let _style_lock = STYLE_LOCK.lock().unwrap_or_else(|held| held.into_inner());
    tui::set_render_style(tui::RenderStyle::Modern);

    let tool = tui::tool_card(
        WIDTH,
        false,
        "fs.edit",
        "src/checkout/request.ts",
        "+ await chargeGateway(order, opts)",
        true,
        Duration::from_millis(88),
    );
    for glyph in ['╭', '╮', '╰', '╯', '│', '─'] {
        assert!(
            !tool.contains(glyph),
            "a panel is colour, not line-drawing; found {glyph} in:\n{tool}"
        );
    }
    let rows: Vec<&str> = tool.lines().collect();
    assert!(rows[0].starts_with('▌'), "the accent stripe: {}", rows[0]);
    assert!(rows[0].contains("fs.edit"), "{}", rows[0]);
    assert!(
        rows[0].trim_end().ends_with("88ms"),
        "the duration is at the right edge: {}",
        rows[0]
    );
    for row in &rows {
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(*row),
            WIDTH,
            "every row fills the width: {row:?}"
        );
    }

    // With colour on, the tint has to actually be painted, and the header has
    // to differ from the body or one call runs into the next.
    let painted = tui::tool_card(
        WIDTH,
        true,
        "mcp.call",
        "observability",
        "p99 30.0s",
        true,
        Duration::from_millis(612),
    );
    let head = painted.lines().next().expect("a header");
    let body = painted.lines().nth(1).expect("a body row");
    assert!(head.contains("\x1b[48;"), "the header is tinted");
    assert!(body.contains("\x1b[48;"), "the body is tinted");
    assert_ne!(
        head.split('m').next(),
        body.split('m').next(),
        "the header and body tints differ"
    );

    // A running call is a bullet, not a panel: drawing one for both states is
    // what made the same command appear twice.
    let running = tui::tool_running_row(false, "process.exec", "pnpm test checkout");
    assert!(running.trim_start().starts_with('•'), "{running}");
    assert_eq!(running.lines().count(), 1, "{running}");

    // The mockup marks the answer with `✦` and leaves it unboxed.
    let response = tui::assistant_block(WIDTH, false, "# Checkout\n\nready");
    assert!(response.starts_with("  ✦ Response"), "{response}");
    assert!(!response.contains('╭'), "the response is not a card");

    // One prompt strip, whichever path drew it.
    let strip = tui::prompt_strip(WIDTH, false, "find the cause");
    assert!(strip.starts_with(" › find the cause"), "{strip:?}");
    assert_eq!(unicode_width::UnicodeWidthStr::width(strip.as_str()), WIDTH);

    let approval = tui::AskDialogState::for_approval(
        "process.exec",
        "pnpm test checkout",
        "the integration tests reach the gateway",
        None,
    )
    .render(WIDTH, false);
    assert!(approval.contains("APPROVAL REQUIRED"), "{approval}");

    tui::set_render_style(tui::RenderStyle::Classic);
}

/// Print the Codex route's rows for a human to look at.
///
/// `cargo test -p arsy-cli --features tui --test renderer_golden -- --nocapture --ignored`
#[test]
#[ignore = "a visual check, not an assertion"]
fn show_codex() {
    let _style_lock = STYLE_LOCK.lock().unwrap_or_else(|held| held.into_inner());
    tui::set_render_style(tui::RenderStyle::Modern);

    let answer = "## Gambaran Cepat\n\nThe gateway call inherited the outer \
                  deadline, so the **retry** never ran.\n\n- one\n- two\n";
    let events = [
        serde_json::json!({"type": "item.completed", "item": {
            "type": "command_execution",
            "command": "bash -lc 'cargo test --workspace'",
            "exit_code": 0,
            "aggregated_output": "test result: ok. 623 passed",
            "duration_ms": 8410,
        }}),
        serde_json::json!({"type": "item.completed", "item": {
            "type": "todo_list",
            "items": [
                {"text": "reproduce the timeout", "status": "completed"},
                {"text": "inspect the request path", "status": "completed"},
                {"text": "give the gateway a deadline", "status": "in_progress"},
                {"text": "run the checkout tests", "status": "pending"},
            ],
        }}),
        serde_json::json!({"type": "item.completed", "item": {
            "type": "agent_message", "text": answer,
        }}),
    ];
    // The renderer's own output, with no spacing added here: the blank line
    // above a block is the writer's job, and faking it in this check would
    // show a layout the code does not actually produce.
    for event in events {
        match tui::render_codex_event(&event.to_string(), false) {
            Some(row) => println!("{row}"),
            None => println!("(nothing drawn)"),
        }
    }
    tui::set_render_style(tui::RenderStyle::Classic);
}
