import json
import os
import sys
import time

event = json.load(sys.stdin)
with open(os.environ["HOOK_RECORDER"], "a", encoding="utf-8") as output:
    output.write(json.dumps(event, separators=(",", ":")) + "\n")
    output.flush()

behavior = os.environ.get("HOOK_BEHAVIOR", "ok")
if behavior == "deny":
    print(json.dumps({"deny": "fixture policy"}))
elif behavior == "large":
    print("x" * 64001)
elif behavior == "timeout":
    time.sleep(2)
elif behavior == "nonzero":
    sys.exit(7)
