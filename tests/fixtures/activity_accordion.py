"""Exercise the activity accordion against a local OpenAI-compatible SSE server."""
import codecs
import fcntl
import http.server
import json
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import threading
import time


DETAIL = "KNOWN_ACTIVITY_READ_FILE_RESULT_7f31"


class MockHandler(http.server.BaseHTTPRequestHandler):
    calls = 0

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        self.rfile.read(length)
        MockHandler.calls += 1
        if MockHandler.calls == 1:
            events = [
                {"choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_read_file",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": '{"path":"known.txt"}'},
                }]}, "finish_reason": None}]},
                {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]},
            ]
        else:
            events = [
                {"choices": [{"delta": {"content": "The file was read successfully."}, "finish_reason": None}]},
                {"choices": [{"delta": {}, "finish_reason": "stop"}]},
            ]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        for event in events:
            self.wfile.write(("data: " + json.dumps(event) + "\n\n").encode())
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def log_message(self, format, *args):
        pass


def screen_parser(width, height):
    screen = [[" "] * width for _ in range(height)]
    state = {"row": 0, "column": 0, "pending": "", "decoder": codecs.getincrementaldecoder("utf-8")()}

    def render(data):
        state["pending"] += state["decoder"].decode(data)
        while state["pending"]:
            if state["pending"].startswith("\x1b"):
                sequence = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", state["pending"])
                if sequence is None:
                    return
                parameters, _, command = sequence.groups()
                state["pending"] = state["pending"][sequence.end():]
                if parameters.startswith("?"):
                    continue
                values = [int(value or "0") for value in parameters.split(";")]
                amount = values[0] or 1
                if command in ("H", "f"):
                    state["row"] = amount - 1
                    state["column"] = (values[1] or 1) - 1 if len(values) > 1 else 0
                elif command == "G":
                    state["column"] = amount - 1
                elif command == "d":
                    state["row"] = amount - 1
                elif command == "A":
                    state["row"] -= amount
                elif command == "B":
                    state["row"] += amount
                elif command == "C":
                    state["column"] += amount
                elif command == "D":
                    state["column"] -= amount
                elif command == "J" and values[0] in (2, 3):
                    for line in screen:
                        line[:] = [" "] * width
                elif command == "K" and 0 <= state["row"] < height:
                    start = state["column"] if values[0] == 0 else 0
                    end = state["column"] + 1 if values[0] == 1 else width
                    screen[state["row"]][start:end] = [" "] * (end - start)
            else:
                character, state["pending"] = state["pending"][0], state["pending"][1:]
                if character == "\r":
                    state["column"] = 0
                elif character == "\n":
                    state["row"] += 1
                elif character.isprintable():
                    if 0 <= state["row"] < height and 0 <= state["column"] < width:
                        screen[state["row"]][state["column"]] = character
                    state["column"] += 1

    def text():
        return "\n".join("".join(line) for line in screen)

    def find(fragment):
        for index, line in enumerate(screen):
            if fragment in "".join(line):
                return index
        return None

    return render, text, find


def main():
    binary, root = sys.argv[1:]
    workspace = os.path.join(root, "workspace")
    os.makedirs(workspace, exist_ok=True)
    with open(os.path.join(workspace, "known.txt"), "w", encoding="utf-8") as file:
        file.write(DETAIL + "\n")

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), MockHandler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    config = {
        "workspace": workspace,
        "sessions_dir": os.path.join(root, "sessions"),
        "providers": {"openai": {"kind": "openai", "base_url": "http://127.0.0.1:%d/v1" % server.server_port,
                                  "api_key_env": "ACTIVITY_TEST_KEY", "timeout_seconds": 5}},
        "model": {"provider": "openai", "model": "mock/activity", "max_tokens": 128},
        "builtins": ["read_file"],
        "system_prompt": "Use read_file when asked.",
        "theme": "diet_soda",
    }
    config_path = os.path.join(root, "config.json")
    with open(config_path, "w", encoding="utf-8") as file:
        json.dump(config, file)

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    before = termios.tcgetattr(slave)
    process = subprocess.Popen([binary, "--config", config_path], stdin=slave, stdout=slave, stderr=slave,
                               env={**os.environ, "TERM": "xterm-256color", "ACTIVITY_TEST_KEY": "dummy"})
    render, text, find = screen_parser(100, 24)
    raw = b""

    def wait_for(predicate, label):
        nonlocal raw
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                data = os.read(master, 65536)
                raw += data
                render(data)
            if predicate():
                return
            if process.poll() is not None:
                raise AssertionError("process exited while waiting for %s: %r" % (label, raw))
        raise AssertionError("missing %s\n%s" % (label, text()))

    try:
        wait_for(lambda: "Input" in text(), "input prompt")
        os.write(master, b"Read known.txt\r")
        wait_for(lambda: find("[+] tool read_file") is not None and "[ok]" in text(), "collapsed successful activity")
        assert DETAIL not in text(), "tool detail leaked into collapsed activity"

        os.write(master, b"\x1b[17~")
        os.write(master, b"\r")
        wait_for(lambda: find("[-] tool read_file") is not None and DETAIL in text(), "expanded activity")

        row = find("[-] tool read_file")
        assert row is not None
        os.write(master, ("\x1b[<0;4;%dM" % (row + 1)).encode())
        wait_for(lambda: find("[+] tool read_file") is not None and DETAIL not in text(), "mouse-collapsed activity")

        os.write(master, b"\r")
        wait_for(lambda: find("[-] tool read_file") is not None and DETAIL in text(), "keyboard re-expanded activity")
        os.write(master, b"\x1b[17~")
        os.write(master, b"/quit\r")
        wait_for(lambda: process.poll() == 0, "clean quit")
        assert b"\x1b[?1000l" in raw, "mouse capture was not disabled"
        assert b"\x1b[?1006l" in raw, "SGR mouse mode was not disabled"
        assert b"\x1b[?2004l" in raw, "bracketed paste mode was not disabled"
        assert before == termios.tcgetattr(slave), "terminal modes were not restored"
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        server.shutdown()
        os.close(master)
        os.close(slave)


if __name__ == "__main__":
    main()
