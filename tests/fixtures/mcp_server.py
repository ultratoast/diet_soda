"""Local, deterministic MCP fixture; uses only Python's standard library."""
import json
import os
import sys
import time


def write_stderr():
    prefix = os.environ.get("MCP_FIXTURE_STDERR_PREFIX", "")
    suffix = os.environ.get("MCP_FIXTURE_STDERR_SUFFIX", "")
    payload = (prefix + suffix).encode()
    if os.environ.get("MCP_FIXTURE_INVALID_STDERR") == "1":
        payload = b"fixture diagnostic \xff\xfe\n"
    sys.stderr.buffer.write(payload)
    sys.stderr.buffer.flush()

failure = os.environ.get("MCP_FIXTURE_FAILURE", "")

for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    if method == "initialize":
        write_stderr()
        if failure == "initialize":
            break
        else:
            result = {
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fixture", "version": "1"},
            }
    elif method == "tools/list":
        write_stderr()
        result = {"tools": [{
            "name": "echo", "description": "Echo fixture input",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]},
        }]}
    elif method == "tools/call":
        write_stderr()
        if failure == "call":
            break
        if request["params"]["arguments"]["text"] == "__wait__":
            time.sleep(30)
        result = {"content": [{"type": "text", "text": request["params"]["arguments"]["text"]}]}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
