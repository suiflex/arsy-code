"""Run against a TUI build: python3 crates/arsy-cli/tests/tui_smoke.py target/debug/arsy.

Uses a real PTY and a provider that always reports unavailable. No credentials,
provider traffic, or user configuration writes are required: HOME points at the
temporary workspace, so a remembered model can neither be read nor written.

The PTY is drained on a thread for as long as the child lives. A pseudo-terminal
holds about a kilobyte before it blocks its writer, and ARSY repaints its input
block on every keystroke, so a reader that pauses between writes stalls the
process it is testing rather than the test.
"""
import json
import os
from pathlib import Path
import fcntl
import pty
import re
import select
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


class Terminal:
    """A PTY whose output is drained continuously into one buffer."""

    def __init__(self, master, child):
        self.master = master
        self.child = child
        self.received = b""
        self.lock = threading.Lock()
        self.stop = threading.Event()
        self.reader = threading.Thread(target=self._drain, daemon=True)
        self.reader.start()

    def _drain(self):
        while not self.stop.is_set():
            if select.select([self.master], [], [], 0.05)[0]:
                try:
                    chunk = os.read(self.master, 65536)
                except OSError:
                    return
                if not chunk:
                    return
                with self.lock:
                    self.received += chunk

    def send(self, keys):
        os.write(self.master, keys)

    def down(self, count=1):
        for _ in range(count):
            self.send(b"\x1b[B")

    def length(self):
        with self.lock:
            return len(self.received)

    def after(self, mark):
        """Everything received after `mark`, a length taken earlier."""
        with self.lock:
            return self.received[mark:]

    def resize(self, columns, rows):
        size(self.master, columns, rows)

    def settled(self, quiet=0.3, deadline=5.0):
        """Whether output stopped arriving for `quiet` seconds.

        False is the answer an implementation that keeps repainting gives, not
        an error: the caller is the one that knows whether silence was owed.
        """
        end = time.monotonic() + deadline
        last = self.length()
        unchanged_since = time.monotonic()
        while time.monotonic() < end:
            time.sleep(0.02)
            current = self.length()
            if current != last:
                last = current
                unchanged_since = time.monotonic()
            elif time.monotonic() - unchanged_since >= quiet:
                return True
        return False

    def expect_after(self, mark, text, timeout=10):
        """Wait for `text` among the bytes received since `mark`.

        `expect` searches everything the session ever painted, which cannot
        tell a frame written for this resize from the frame the last one left.
        """
        needle = text if isinstance(text, bytes) else text.encode()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if needle in self.after(mark):
                return
            if self.child.poll() is not None:
                break
            time.sleep(0.02)
        raise AssertionError(
            f"missing {needle!r} after byte {mark}: {self.after(mark)[-2000:]!r}"
        )

    def expect(self, text, timeout=10):
        needle = text if isinstance(text, bytes) else text.encode()
        deadline = time.monotonic() + timeout
        exited = None
        while time.monotonic() < deadline:
            with self.lock:
                received = self.received
            if needle in received:
                return
            # A build without the `tui` feature refuses the bare invocation.
            # Any workspace-wide cargo command rebuilds target/debug/arsy
            # without it, so this is what a stale binary looks like and it is
            # worth naming instead of spending the timeout on it.
            if b"ARSY-SCH-1003" in received:
                raise AssertionError(
                    "the binary was built without the TUI: run "
                    "`cargo build -p arsy-cli --features tui` and retry"
                )
            # One more pass after the child exits, so its final bytes are read.
            if exited:
                break
            exited = self.child.poll() is not None
            time.sleep(0.02)
        with self.lock:
            tail = self.received[-2000:]
        raise AssertionError(f"missing {text!r}: {tail!r}")

    def close(self):
        self.stop.set()
        self.reader.join(timeout=2)


def workspace(root):
    (root / ".claude").mkdir()
    (root / ".mcp.json").write_text(json.dumps({
        "mcpServers": {"docs": {"command": "never-execute-this", "args": ["--serve"]}}
    }))
    (root / ".claude/settings.json").write_text(json.dumps({
        "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "never-execute-this"}]}]}
    }))


def isolated(root):
    """An environment that sees only this workspace.

    Inspection reads the operator's own Claude and Codex configuration as well
    as the workspace's — that is the point of the command — so a test that
    inherited the real one would assert on whatever the machine running it
    happens to have installed. `HOME` is what every home is derived from, so
    moving it is enough, and the variables that override it are removed rather
    than redirected: the run is expected to write `.arsy/` under this root and
    is checked for it afterwards, and pointing the compatibility homes at an
    empty directory stops the workspace's own hooks being found at all.
    """
    environment = dict(
        os.environ,
        PATH=os.environ['PATH'],
        HOME=str(root),
        XDG_CONFIG_HOME=str(root / "config"),
    )
    for override in ("CLAUDE_CONFIG_DIR", "CODEX_HOME", "ARSY_CONFIG_HOME"):
        environment.pop(override, None)
    return environment


def non_interactive(binary, root, environment):
    listed = subprocess.run([str(binary), "--workspace", str(root), "mcp", "list", "--output", "json"], capture_output=True, text=True, timeout=10, env=environment)
    assert listed.returncode == 0, listed.stderr
    records = [json.loads(line) for line in listed.stdout.splitlines()]
    assert len(records) == 1 and records[0]["type"] == "result"
    names = [entry["name"] for entry in records[0]["payload"]["entries"]]
    assert names[0] == "docs", names
    missing = subprocess.run([str(binary), "--workspace", str(root), "mcp", "show", "missing", "--output", "json"], capture_output=True, text=True, timeout=10, env=environment)
    assert missing.returncode == 2
    assert [json.loads(line)["type"] for line in missing.stdout.splitlines()] == ["diagnostic", "result"]


DRAFT = "0123456789" * 7


def size(master, columns, rows):
    """Give a PTY a window size, the way a terminal emulator would."""
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))


def session_leader():
    """Make the child the session leader of the PTY it was handed.

    Without this the child keeps whatever controlling terminal the test
    process has — or inherits none — and `/dev/tty`, which is where a size is
    read from, is not the PTY under test. A `preexec_fn` rather than a wrapper
    because the child is the one that has to claim the terminal.
    """
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def composer_rows(width, text):
    """What one line of input prints as in this width, as the composer wraps it.

    Classic keeps `width - 4` columns for the text: three for the frame and the
    marker, one for the caret to sit on at a row's end. A wrap continues on a
    row marked with a blank.
    """
    room = max(width - 4, 1)
    chunks = [text[index:index + room] for index in range(0, len(text), room)] or [""]
    return "".join(
        f"{'›' if index == 0 else ' '} {chunk}\r\n"
        for index, chunk in enumerate(chunks)
    ).encode()


def menu_rows(received):
    """The command rows of the classic frames in `received`, in order.

    A menu row is indented, marked with the selection cursor, and starts with
    the command itself; the status row below it starts with the model instead.
    """
    rows = []
    for line in received.split(b"\n"):
        match = re.match(rb"^ {2,4}(?:\xe2\x80\xba )?(/[a-z]+)", line.rstrip(b"\r"))
        if match:
            rows.append(match.group(1).decode())
    return rows


def window_case(binary):
    """Idle traffic, a live resize, and the menu's bound on its own terminal.

    Separate from the flow above, and separately isolated, because it needs a
    controlling terminal: the child is made a session leader so `/dev/tty` is
    this PTY, and the size is set with `TIOCSWINSZ` rather than by a shell. Both
    behaviours under test are about bytes reaching a terminal — a composer that
    stops writing when nothing changed, and one that follows a resize instead of
    the size the shell that started it happened to have — so they are checked
    against a real PTY rather than a mock.
    """
    with tempfile.TemporaryDirectory(prefix="arsy-tui-window-") as directory:
        root = Path(directory)
        workspace(root)
        # Pin classic rendering so draft wrapping can be inspected directly.
        (root / ".arsy").mkdir()
        (root / ".arsy/arsy.json").write_text(
            json.dumps({"schema_version": 1, "ui": {"style": "classic"}})
        )
        environment = isolated(root)
        # Stale on purpose: a width the terminal no longer has, so a composer
        # reading the environment instead of the window shows up as a draft
        # that never wraps.
        environment["COLUMNS"] = "160"
        environment["LINES"] = "60"

        master, slave = pty.openpty()
        size(master, 100, 24)
        # Read through the master: it is the same tty, and a slave whose session
        # leader has exited is revoked, so it cannot be asked afterwards.
        original = termios.tcgetattr(master)
        child = subprocess.Popen(
            [str(binary), "--workspace", str(root), "--no-color"],
            stdin=slave, stdout=slave, stderr=slave, env=environment,
            preexec_fn=session_leader,
        )
        terminal = Terminal(master, child)

        def frame_after(mark, timeout=10):
            """The bytes written after `mark`, once the terminal has settled."""
            assert terminal.settled(deadline=timeout), (
                f"the terminal never settled after byte {mark}"
            )
            return terminal.after(mark)

        try:
            terminal.expect("Provider unavailable")
            # The launch card is painted before raw mode is acquired, and a key
            # sent then is still line-buffered by the terminal driver, where
            # Ctrl-C is a signal rather than a key. The composer row is painted
            # from inside the key loop.
            terminal.expect("›")
            # Startup opens the model picker; Ctrl-C on an empty composer is the
            # action that closes it onto the task prompt.
            terminal.send(b"\x03")
            assert child.poll() is None, "closing the model picker ended the session"

            terminal.send(DRAFT.encode())
            # 70 characters fit one input row at 100 columns, and the row is the
            # whole draft: no wrap, and no submission from the characters.
            terminal.expect(composer_rows(100, DRAFT))
            assert child.poll() is None, "typed characters submitted the line"

            # Nothing typed and nothing moving: the frame for the last keystroke
            # is out, and from there the terminal must stay silent.
            assert terminal.settled(), "an idle composer never stopped repainting"
            idle = terminal.length()
            time.sleep(0.5)
            assert terminal.length() == idle, (
                f"an idle composer wrote {terminal.length() - idle} more bytes"
            )

            # No keypress, just a narrower window: the same draft has to reflow,
            # which it can only do from a measurement — the environment still
            # says 160 columns.
            mark = terminal.length()
            terminal.resize(40, 24)
            terminal.expect_after(mark, composer_rows(40, DRAFT))
            assert child.poll() is None, "the resize submitted the line"

            mark = terminal.length()
            terminal.resize(100, 24)
            terminal.expect_after(mark, composer_rows(100, DRAFT))

            # Still live: a key typed after the resize is in the next frame, not
            # held back with the frames that were.
            mark = terminal.length()
            terminal.send(b"Z")
            terminal.expect_after(mark, composer_rows(100, DRAFT + "Z"))

            # Ctrl-C clears the draft rather than ending the session, and the
            # empty composer is what the next frame shows.
            mark = terminal.length()
            terminal.send(b"\x03")
            terminal.expect_after(mark, composer_rows(100, ""))
            assert child.poll() is None, "clearing the draft ended the session"

            # The menu is bounded by the terminal's height: three rows fit above
            # a one-row composer in seven, and the whole table comes back when
            # the terminal does. The frame is read alone, so the count is one
            # frame's and not a redraw's.
            mark = terminal.length()
            terminal.send(b"/")
            wide = menu_rows(frame_after(mark))
            assert len(wide) > 3, f"a 24-row terminal offered {len(wide)} menu rows"
            assert len(wide) <= 24 - 4, f"the menu exceeded its rows: {len(wide)}"

            mark = terminal.length()
            terminal.resize(100, 7)
            short = menu_rows(frame_after(mark))
            assert 0 < len(short) <= 7 - 4, (
                f"a 7-row terminal offered {len(short)} menu rows"
            )

            mark = terminal.length()
            terminal.resize(100, 24)
            tall = menu_rows(frame_after(mark))
            assert len(tall) == len(wide), (
                f"the menu did not grow back: {len(tall)} rows against {len(wide)}"
            )

            # Leave the menu, then quit the way the flow above does.
            terminal.send(b"\x03")
            time.sleep(0.3)
            terminal.send(b"/quit\r")
            assert child.wait(timeout=10) == 0
            assert termios.tcgetattr(master) == original, "terminal modes were not restored"
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            terminal.close()
            os.close(master)
            os.close(slave)


def main():
    binary = Path(sys.argv[1]).resolve()
    with tempfile.TemporaryDirectory(prefix="arsy-tui-") as directory:
        root = Path(directory)
        workspace(root)
        environment = isolated(root)
        non_interactive(binary, root, environment)

        master, slave = pty.openpty()
        original = termios.tcgetattr(slave)
        child = subprocess.Popen(
            [str(binary), "--workspace", str(root), "--no-color"],
            stdin=slave, stdout=slave, stderr=slave, env=environment,
        )
        terminal = Terminal(master, child)
        try:
            terminal.expect("Provider unavailable")
            # Cancel the startup model picker without remembering a route.
            terminal.expect("›")
            terminal.send(b"\x03")
            terminal.send(b"/help\r")
            terminal.expect("Up/Down: input history")
            terminal.send(b"/plan\r")
            terminal.expect("Plan Mode active")
            terminal.expect("PLAN")
            terminal.send(b"/plan cancel\r")
            terminal.expect("Planning cancelled. Approval mode: default.")

            # Shift+Tab changes the mode where the card names it: the launch
            # card is re-rendered with the new mode, and no MODE row is added
            # to the scrollback, so cycling modes does not stack rows.
            terminal.send(b"\x1b[Z")
            terminal.expect("acceptEdits")
            terminal.send(b"\x1b[Z")
            terminal.expect("⏸ PLAN")
            with terminal.lock:
                mode_rows = terminal.received.count(b"MODE ")
                assert mode_rows == 0, (
                    f"Shift+Tab printed {mode_rows} MODE rows into the scrollback"
                )
            terminal.send(b"/approval default\r")
            terminal.expect("Approval mode: default")
            with terminal.lock:
                approval_announcements = terminal.received.count(b"Approval mode:")
                assert approval_announcements <= 2, (
                    f"Shift+Tab printed {approval_announcements} approval announcements"
                )

            # `/` opens the command menu and typing filters it; Down and Up
            # move the marker, which is what the two expectations below prove.
            terminal.send(b"/")
            terminal.expect("› /new")
            terminal.send(b"h")
            terminal.expect("› /hooks")
            terminal.send(b"\x1b[B")
            terminal.expect("› /help")
            terminal.send(b"\x1b[A")
            terminal.expect("› /hooks")
            # Enter takes the highlighted command; a second one sends it, and
            # bare `/hooks` opens the manager. Nothing is toggled, so the
            # transcript stays clean and Esc closes the dialog.
            terminal.send(b"\r\r")
            terminal.expect(" HOOKS ")
            terminal.expect("after_turn")
            # A lone Escape is only known once nothing follows it, and the
            # placeholder this prompt opened with is already in the reader's
            # buffer, so the next send has to wait for the close instead of
            # expecting text that was printed before the dialog opened.
            terminal.send(b"\x1b")
            time.sleep(0.5)

            # Inspection reports what a connection would run, never the record.
            # Named rather than bare: a bare `/mcp` opens the toggle dialog,
            # which is a different surface with a different answer.
            terminal.send(b"/mcp show docs\r")
            terminal.expect("docs · stdio · not loaded")
            terminal.expect("command: never-execute-this --serve")
            terminal.send(b"/hooks --event Stop\r")
            terminal.expect("lifecycle: after_turn")
            terminal.send(b"/hooks --event NoSuchEvent\r")
            terminal.expect("Filters applied: --event NoSuchEvent")
            terminal.send(b"/unknown\r")
            terminal.expect("Unknown command")

            # `/model` opens the model/effort dialog; leaving it applies nothing
            # and returns to the task prompt instead of ending the session.
            terminal.send(b"/model\r")
            terminal.expect("MODEL & EFFORT")
            terminal.send(b"\x03")
            assert child.poll() is None, "leaving the model dialog ended the session"
            assert not (root / ".arsy/state/model").exists()

            # The effort picker is arrowed and taken like the command menu. It
            # opens marked at the current setting, which is unset here, and the
            # ends wrap.
            terminal.send(b"/effort\r")
            terminal.expect("least reasoning")
            terminal.expect("\u203a off")
            terminal.send(b"\x1b[B")
            terminal.expect("\u203a low")
            terminal.send(b"\r\r")
            terminal.expect("Effort: low")
            assert (root / ".arsy/state/effort").exists(), "an accepted effort was not remembered"

            # Leaving the picker cancels the picker, not the session: a command
            # that was never run before proves the task prompt came back.
            terminal.send(b"/effort\r")
            terminal.expect("\u203a low")
            terminal.send(b"\x03")
            assert child.poll() is None, "leaving the effort picker ended the session"
            terminal.send(b"/effort high\r")
            terminal.expect("Effort: high")

            # `/theme` picks a colour theme the same way, and remembers it
            # beside the effort file.
            terminal.send(b"/theme\r")
            terminal.expect("greys only, no hue")
            terminal.expect("› dark")
            terminal.send(b"\x1b[B")
            terminal.expect("› ocean")
            terminal.send(b"\r\r")
            terminal.expect("Theme: ocean")
            assert (root / ".arsy/state/theme").exists(), "the theme choice was not remembered"
            terminal.send(b"/theme\r")
            terminal.expect("› ocean")
            terminal.send(b"\x03")
            assert child.poll() is None, "leaving the theme picker ended the session"
            terminal.send(b"/theme mono\r")
            terminal.expect("Theme: mono")

            # `/provider` (and its alias `/auth`) open one 3-pane dialog:
            # access -> provider -> manage. Adding a custom endpoint navigates
            # to Custom access, the new-endpoint row, then Add, which hands off
            # to the same field-by-field wizard; the credential is typed masked
            # and the configuration ARSY writes is the one it reads back.
            terminal.send(b"/provider\r")
            terminal.expect("Switch Pane")            # the dialog is open
            terminal.down(2)                          # access: OAuth -> Key -> Custom
            terminal.send(b"\t")                       # focus the provider list (+new)
            terminal.send(b"\t")                       # focus manage (Add)
            terminal.send(b"\r")                       # Add -> manual add wizard
            terminal.expect("new provider")
            terminal.send(b"acme\r")
            terminal.expect("dialect")
            terminal.send(b"openai\r")
            terminal.expect("base URL for acme")
            terminal.send(b"https://acme.test/v1\r")
            terminal.expect("models for acme")
            terminal.send(b"acme-1, acme-2\r")
            terminal.expect("where to keep the credential")
            terminal.send(b"file\r")
            terminal.expect("not shown as you type")
            terminal.send(b"sk-provider-wizard-value\r")
            terminal.expect("Added provider acme")

            written = root / ".arsy/arsy.json"
            body = json.loads(written.read_text())
            endpoint = body["provider"]["endpoint"]["acme"]
            assert body["provider"]["default"] == "acme", body
            assert endpoint["base_url"] == "https://acme.test/v1", body
            assert endpoint["credential"] == "secret://file/acme.key", body
            # One host, several models: a list on the endpoint rather than a
            # second endpoint duplicating its URL and credential.
            assert endpoint["model"] == "acme-1", body
            assert endpoint["models"] == ["acme-2"], body

            key = written.parent / "secrets" / "acme.key"
            assert key.read_text() == "sk-provider-wizard-value", "the credential was mangled"
            assert oct(key.stat().st_mode & 0o777) == "0o600", oct(key.stat().st_mode)
            # The credential must not be anywhere the terminal kept.
            with terminal.lock:
                assert b"sk-provider-wizard-value" not in terminal.received

            # `/auth` is an alias for the same dialog; Esc leaves it without
            # touching the session.
            terminal.send(b"/auth\r")
            terminal.expect("Switch Pane")
            terminal.send(b"\x03")
            assert child.poll() is None, "leaving the provider dialog ended the session"

            # Removing asks first, and `no` leaves the configuration alone. The
            # configured endpoint is reachable under Custom access; its Manage
            # column offers Use, Set key, Fetch models, then Remove.
            terminal.send(b"/provider\r")
            terminal.down(2)                          # access: -> Custom
            terminal.send(b"\t")                       # list: acme is row 0
            terminal.send(b"\t")                       # manage: Use is action 0
            terminal.down(3)                          # manage: -> Remove
            terminal.send(b"\r")                       # hand off to the confirm step
            terminal.expect("remove `acme` from the configuration?")
            terminal.send(b"no\r")
            terminal.expect("Provider unchanged")
            assert "acme" in json.loads(written.read_text())["provider"]["endpoint"]

            terminal.send(b"/provider\r")
            terminal.down(2)
            terminal.send(b"\t\t")
            terminal.down(3)
            terminal.send(b"\r")
            terminal.expect("remove `acme` from the configuration?")
            terminal.send(b"yes\r")
            terminal.expect("Removed provider acme")
            assert "acme" not in json.loads(written.read_text()).get("provider", {}).get(
                "endpoint", {}
            )

            terminal.send(b"\x1b[200~/quit\n\x1b[201~")
            time.sleep(0.15)
            assert child.poll() is None, "pasted newline must not submit /quit"
            terminal.send(b"\x03/quit\r")
            assert child.wait(timeout=5) == 0
            assert termios.tcgetattr(slave) == original, "terminal modes were not restored"
            # The store is opened when the session starts, so it exists even
            # for one that only inspected. What an inspection must not do is
            # record a turn into it.
            import sqlite3 as _sql
            events = _sql.connect(root / ".arsy/state/sessions.sqlite3").execute(
                "SELECT COUNT(*) FROM events"
            ).fetchone()[0]
            assert events == 0, f"inspection recorded {events} events"
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            terminal.close()
            os.close(master)
            os.close(slave)
    window_case(binary)
    print("PASS: JSON success/failure, PTY Plan Mode, inspection, filtering, help, model, effort and provider flows, safe paste, exit, terminal restoration, idle traffic, live resize, menu height")


if __name__ == "__main__":
    main()
