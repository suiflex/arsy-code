#!/bin/sh
# Check the ARSY banner: sh crates/arsy-cli/tests/banner_test.sh [binary]
#
# The banner is printed once, before raw mode, so a pseudo-terminal is all it
# takes to see it. `script` supplies the PTY; `stty` inside it sets the window
# size, which is what the card reads before it looks at COLUMNS.
#
# NO_COLOR is set or cleared per case rather than inherited: a shell that
# already exports it would otherwise turn the coloured case into a false
# failure. HOME points at a temporary directory, so no remembered setting is
# read or written.
set -eu

BINARY=${1:-${ARSY_BINARY:-target/debug/arsy}}
[ -x "$BINARY" ] || {
    echo "error: no binary at $BINARY; run: cargo build -p arsy-cli --features tui" >&2
    exit 1
}
BINARY=$(cd "$(dirname "$BINARY")" && pwd)/$(basename "$BINARY")

# The mark's bottom row, from MARK in crates/arsy-cli/src/tui.rs. A row with no
# leading or trailing blanks is the one a card cannot pad into existence.
# A run of glyphs like this only ever reaches the terminal under NO_COLOR,
# because a coloured card escapes every cell of the mark separately.
MARK='*****    ==++++++    +===='
# That row's leftmost cell as a coloured card paints it: the gradient colour
# the cell samples, from `assets/logo.svg`'s own stops.
CELL=$(printf '\033[38;2;53;201;236m*\033[0m')

WORKSPACE=$(mktemp -d "${TMPDIR:-/tmp}/arsy-banner.XXXXXX")
trap 'rm -rf "$WORKSPACE"' EXIT

# What runs on the far side of the PTY: size the window, then become ARSY.
SESSION=$WORKSPACE/session.sh
cat >"$SESSION" <<SCRIPT
stty cols "\$1" rows 40
exec "$BINARY" --workspace "$WORKSPACE"
SCRIPT

# Run the TUI at `columns` wide and return everything it painted.
#
# `/quit` is typed after a pause, because keys sent before raw mode is acquired
# are still line-buffered by the terminal driver. The watchdog is what turns a
# session that never exits into a failed test instead of a hung one.
banner() {
    columns=$1
    out=$WORKSPACE/out.$columns.$$
    # util-linux takes the command behind -c as one string, the BSD one takes
    # it as arguments. Both are handed the same file, so neither has to survive
    # a second round of quoting.
    if [ "$(uname)" = Linux ]; then
        set -- script -q -c "sh $SESSION $columns" /dev/null
    else
        set -- script -q /dev/null sh "$SESSION" "$columns"
    fi
    {
        # Long enough for the launch intro to finish sweeping the mark, which
        # runs before raw mode and so before a typed key is read.
        sleep 2
        printf '/quit\r'
        sleep 1
    } | "$@" >"$out" 2>&1 &
    child=$!
    # The watchdog gives up its copy of this function's stdout, or the command
    # substitution around it would wait out the sleep after a clean exit.
    (sleep 30; kill -9 "$child" 2>/dev/null) >/dev/null 2>&1 &
    watchdog=$!
    wait "$child" 2>/dev/null || true
    kill "$watchdog" 2>/dev/null || true
    cat "$out"
}

wide=$(HOME=$WORKSPACE XDG_CONFIG_HOME=$WORKSPACE/config; export HOME XDG_CONFIG_HOME; unset NO_COLOR; banner 120)
# Any workspace-wide cargo command rebuilds target/debug/arsy without the `tui`
# feature, and that build refuses the bare invocation. It is worth naming, or it
# reads as a banner that stopped drawing its mark. Checked here rather than in
# `banner`, which runs in a command substitution and cannot end the script.
case $wide in
    *ARSY-SCH-1003*)
        echo "error: binary built without the TUI: cargo build -p arsy-cli --features tui" >&2
        exit 1
        ;;
esac
case $wide in
    *"$CELL"*) ;;
    *) echo "error: banner did not paint the logo mark" >&2; exit 1 ;;
esac

plain=$(HOME=$WORKSPACE XDG_CONFIG_HOME=$WORKSPACE/config NO_COLOR=1; export HOME XDG_CONFIG_HOME NO_COLOR; banner 120)
case $plain in
    *"$MARK"*) ;;
    *) echo "error: NO_COLOR dropped the mark instead of its colour" >&2; exit 1 ;;
esac
case $plain in
    *"$CELL"*) echo "error: NO_COLOR was ignored: the mark is still coloured" >&2; exit 1 ;;
esac
case $plain in
    *directory:*) ;;
    *) echo "error: the card lost its fields under NO_COLOR" >&2; exit 1 ;;
esac

# Too narrow to hold both: the text is what a small terminal keeps. The mark
# and its gutter take 29 columns, and the card keeps room for a label and a
# value beside them, so a card narrower than 55 columns drops it and 40 is well
# under that. Run without colour, so the mark it must not have drawn is the
# unpainted one this script can look for.
narrow=$(HOME=$WORKSPACE XDG_CONFIG_HOME=$WORKSPACE/config NO_COLOR=1; export HOME XDG_CONFIG_HOME NO_COLOR; banner 40)
case $narrow in
    *"$MARK"*) echo "error: a 40-column terminal still drew the mark" >&2; exit 1 ;;
esac
case $narrow in
    *directory:*) ;;
    *) echo "error: the narrow card dropped its fields, not just the mark" >&2; exit 1 ;;
esac

echo "PASS: mark painted at 120 columns, unpainted under NO_COLOR, dropped at 40"
