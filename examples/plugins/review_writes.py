"""Example before_tool hook: reject writes unless the filename ends in .draft."""
import json
import sys

event = json.load(sys.stdin)
payload = event.get("payload", {})
reply = {}
if event["event"] == "before_tool" and payload.get("tool") == "write_file":
    if not payload.get("arguments", {}).get("path", "").endswith(".draft"):
        reply = {"deny": "This plugin permits write_file only for .draft files."}
print(json.dumps(reply))
