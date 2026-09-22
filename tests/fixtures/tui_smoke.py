"""Exercise mode cycling, searchable pickers, help, and cleanup through a real TTY."""
import fcntl
import codecs
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import time

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
before = termios.tcgetattr(slave)
process = subprocess.Popen(
    [sys.argv[1], "--config", sys.argv[2]],
    stdin=slave, stdout=slave, stderr=slave,
    env={**os.environ, "TERM": "xterm-256color"},
)

screen = [[" "] * 100 for _ in range(24)]
row = column = 0
pending = ""
decoder = codecs.getincrementaldecoder("utf-8")()
raw_output = b""


def render(data):
    """Apply cursor-addressed diffs instead of assuming a label arrives intact.

    Ratatui skips cells that already have the right character (including random
    session IDs), so searching the raw byte stream produces intermittent failures.
    This fixture uses single-cell text and only needs CSI cursor/erase handling.
    """
    global row, column, pending
    pending += decoder.decode(data)
    while pending:
        if pending.startswith("\x1b"):
            sequence = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", pending)
            if sequence is None:
                return
            parameters, _, command = sequence.groups()
            pending = pending[sequence.end():]
            if parameters.startswith("?"):
                continue
            values = [int(value or "0") for value in parameters.split(";")]
            amount = values[0] or 1
            if command in ("H", "f"):
                row = amount - 1
                column = (values[1] or 1) - 1 if len(values) > 1 else 0
            elif command == "G":
                column = amount - 1
            elif command == "d":
                row = amount - 1
            elif command == "A":
                row -= amount
            elif command == "B":
                row += amount
            elif command == "C":
                column += amount
            elif command == "D":
                column -= amount
            elif command == "J" and values[0] in (2, 3):
                for line in screen:
                    line[:] = [" "] * 100
            elif command == "K" and 0 <= row < 24:
                start = column if values[0] == 0 else 0
                end = column + 1 if values[0] == 1 else 100
                screen[row][start:end] = [" "] * (end - start)
        else:
            character, pending = pending[0], pending[1:]
            if character == "\r":
                column = 0
            elif character == "\n":
                row += 1
            elif character.isprintable():
                if 0 <= row < 24 and 0 <= column < 100:
                    screen[row][column] = character
                column += 1


def wait_for(expected):
    global raw_output
    output = b""
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        ready, _, _ = select.select([master], [], [], 0.1)
        if ready:
            data = os.read(master, 65536)
            output += data
            raw_output += data
            render(data)
        if expected.decode() in "\n".join("".join(line) for line in screen):
            return
        if process.poll() is not None:
            raise AssertionError(f"Process exited early: {output!r}")
    raise AssertionError(f"Missing {expected!r}: {output!r}")

try:
    wait_for(b"Input")
    assert b"\x1b[?1000h" in raw_output, "startup did not enable mouse capture"
    assert b"\x1b[?1006h" in raw_output, "startup did not enable SGR mouse mode"
    os.write(master, b"/mouse off\r")
    wait_for(b"Mouse capture disabled")
    assert b"\x1b[?1000l" in raw_output, "mouse off did not disable mouse capture"
    assert b"\x1b[?1006l" in raw_output, "mouse off did not disable SGR mouse mode"
    os.write(master, b"/mouse on\r")
    wait_for(b"Mouse capture enabled")
    assert raw_output.count(b"\x1b[?1000h") >= 2, "mouse on did not re-enable mouse capture"
    assert raw_output.count(b"\x1b[?1006h") >= 2, "mouse on did not re-enable SGR mouse mode"
    os.write(master, b"\t")
    time.sleep(0.2)  # Tab changes selection silently while the TUI remains idle.
    os.write(master, b"\x1b[Z")  # Shift+Tab
    time.sleep(0.2)  # Shift+Tab also remains silent.
    os.write(master, b"/model\r")
    wait_for(b"Search")
    os.write(master, b"brwstrgt")
    time.sleep(0.2)
    os.write(master, b"\r")
    wait_for(b"Selected openrouter:vendor/dialog-model")
    os.write(master, b"/theme\r")
    wait_for(b"Themes")
    os.write(master, b"hxx0r")
    time.sleep(0.2)
    os.write(master, b"\r")
    wait_for(b"Theme: haxx0r")
    os.write(master, b"/mcp\r")
    wait_for(b"MCP servers")
    os.write(master, b"\r")
    wait_for(b"[on ]")
    os.write(master, b"\x03")  # Close the picker, keeping its runtime toggle.
    wait_for(b"MCP browser: on")
    os.write(master, b"/help\r")
    wait_for(b"Commands")
    os.write(master, b"q")
    time.sleep(0.2)
    os.write(master, b"/quit\r")
    output = b""
    deadline = time.monotonic() + 10
    while process.poll() is None and time.monotonic() < deadline:
        ready, _, _ = select.select([master], [], [], 0.1)
        if ready:
            data = os.read(master, 65536)
            output += data
            raw_output += data
    assert process.poll() == 0, f"Quit failed: {output!r}"
    assert raw_output.count(b"\x1b[?1000l") >= 2, "clean exit did not disable mouse capture"
    assert raw_output.count(b"\x1b[?1006l") >= 2, "clean exit did not disable SGR mouse mode"
    after = termios.tcgetattr(slave)
    assert before == after, "Terminal modes were not restored"
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)
    os.close(slave)
