"""Exercise mode cycling, the model picker, help, and cleanup through a real TTY."""
import fcntl
import os
import pty
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

def wait_for(expected):
    output = b""
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        ready, _, _ = select.select([master], [], [], 0.1)
        if ready:
            output += os.read(master, 65536)
        if expected in output:
            return
        if process.poll() is not None:
            raise AssertionError(f"Process exited early: {output!r}")
    raise AssertionError(f"Missing {expected!r}: {output!r}")

try:
    wait_for(b"Input")
    os.write(master, b"\t")
    wait_for(b"research")
    os.write(master, b"\x1b[Z")  # Shift+Tab
    wait_for(b"default")
    os.write(master, b"/model\r")
    wait_for(b"Search")
    os.write(master, b"brwstrgt")
    time.sleep(0.2)
    os.write(master, b"\r")
    wait_for(b"Selected openrouter:vendor/dialog-model")
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
            output += os.read(master, 65536)
    assert process.poll() == 0, f"Quit failed: {output!r}"
    after = termios.tcgetattr(slave)
    assert before == after, "Terminal modes were not restored"
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    os.close(master)
    os.close(slave)
