"""Configurable local MCP fixture for per-server connection gate tests."""
import json
import os
import sys
import time


marker = os.environ.get("MCP_GATE_MARKER")
if marker:
    with open(marker, "a", encoding="utf-8") as file:
        file.write("spawned\n")

delay = float(os.environ.get("MCP_GATE_INITIALIZE_DELAY", "0"))
fail_count = int(os.environ.get("MCP_GATE_FAIL_INITIALIZE_COUNT", "0"))
fail_call_count = int(os.environ.get("MCP_GATE_FAIL_CALL_COUNT", "0"))
spawn_count = 0
if marker:
    with open(marker, "r", encoding="utf-8") as file:
        spawn_count = sum(1 for _ in file)

for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    if method == "initialize":
        if delay:
            time.sleep(delay)
        if spawn_count <= fail_count:
            result = {"capabilities": {"tools": {}}}
        else:
            result = {
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "gate-fixture", "version": "1"},
            }
    elif method == "tools/list":
        result = {
            "tools": [{
                "name": "echo",
                "description": "Gate fixture tool",
                "inputSchema": {"type": "object"},
            }]
        }
    elif method == "tools/call":
        if spawn_count <= fail_call_count:
            break
        text = request["params"]["arguments"].get("text", "success")
        result = {"content": [{"type": "text", "text": text}]}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
