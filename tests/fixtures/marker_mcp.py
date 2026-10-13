#!/usr/bin/env python3
"""A minimal MCP server over stdio for the live tests: one tool, `write_marker`, that writes a file.
It is not read-only, so an approval policy that asks must ask before it runs."""
import json, os, sys

def reply(id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id, "result": result}) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method, id = msg.get("method"), msg.get("id")
    if id is None:
        continue
    if method == "initialize":
        reply(id, {"protocolVersion": msg["params"].get("protocolVersion", "2025-06-18"),
                   "capabilities": {"tools": {}}, "serverInfo": {"name": "marker", "version": "0"}})
    elif method == "tools/list":
        reply(id, {"tools": [{
            "name": "write_marker",
            "description": "Writes the given text to the marker file.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]},
            "annotations": {"readOnlyHint": False, "destructiveHint": True},
        }]})
    elif method == "tools/call":
        text = msg["params"].get("arguments", {}).get("text", "")
        with open(os.environ["MARKER_FILE"], "w") as f:
            f.write(text)
        reply(id, {"content": [{"type": "text", "text": "written"}]})
    else:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": method}}) + "\n")
        sys.stdout.flush()
