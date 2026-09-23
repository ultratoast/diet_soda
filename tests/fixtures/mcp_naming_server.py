"""Configurable local MCP fixture for exposed-name tests."""
import json
import sys


tools = json.loads(sys.argv[1])
page_size = int(sys.argv[2])

for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    if method == "initialize":
        result = {
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "naming-fixture", "version": "1"},
        }
    elif method == "tools/list":
        cursor = request.get("params", {}).get("cursor")
        start = int(cursor) if cursor is not None else 0
        end = len(tools) if page_size <= 0 else min(start + page_size, len(tools))
        result = {"tools": tools[start:end]}
        if end < len(tools):
            result["nextCursor"] = str(end)
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
