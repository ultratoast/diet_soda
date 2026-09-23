"""Check terminal and redirected headless output using one local model mock."""
import http.server
import json
import os
import pty
import select
import socketserver
import struct
import subprocess
import sys
import termios
import time
import fcntl


MODEL_TEXT = "model\nline\t\x1b[31mred\x1b[0m\x1b]0;title\x07\u202ehidden\u200b\u2028end\u200d"


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        body = (
            'data: ' + json.dumps({"choices": [{"delta": {"content": MODEL_TEXT}}]}) + "\n\n"
            'data: ' + json.dumps({"choices": [{"delta": {}, "finish_reason": "stop"}]}) + "\n\n"
            "data: [DONE]\n\n"
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        pass


def run(binary, config_path, terminal):
    command = [binary, "--config", config_path, "--prompt", "say it"]
    if not terminal:
        return subprocess.run(command, capture_output=True, timeout=8, check=False)

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave,
                               env={**os.environ, "TERM": "xterm-256color"})
    output = bytearray()
    deadline = time.monotonic() + 8
    try:
        while process.poll() is None and time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                output.extend(os.read(master, 65536))
        if process.poll() is None:
            process.kill()
            process.wait()
            raise AssertionError("headless PTY process did not terminate")
        return subprocess.CompletedProcess(command, process.returncode, bytes(output), b"")
    finally:
        os.close(master)
        os.close(slave)


with socketserver.TCPServer(("127.0.0.1", 0), Handler) as server:
    import threading
    threading.Thread(target=server.serve_forever, daemon=True).start()
    config = {
        "providers": {"openai": {"kind": "openai", "base_url": f"http://127.0.0.1:{server.server_address[1]}", "timeout_seconds": 5}},
        "model": {"provider": "openai", "model": "mock", "max_tokens": 128},
        "workspace": os.path.dirname(config_path := sys.argv[2]),
    }
    with open(config_path, "w", encoding="utf-8") as file:
        json.dump(config, file)
    binary = sys.argv[1]
    tty = run(binary, config_path, True)
    raw = tty.stdout.decode("utf-8", errors="replace").replace("\r\n", "\n")
    assert "model\nline [31mred[0m]0;titlehiddenend\u200d" in raw, repr(raw)
    assert "\x1b[31m" not in raw and "\x1b]0;" not in raw
    assert "\u202e" not in raw and "\u200b" not in raw and "\u2028" not in raw

    piped = run(binary, config_path, False)
    assert piped.returncode == 0, piped.stderr.decode(errors="replace")
    assert piped.stdout.decode("utf-8") == MODEL_TEXT + "\n"
