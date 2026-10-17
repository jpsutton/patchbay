#!/usr/bin/env python3
"""Minimal MCP server over stdio (newline-delimited JSON-RPC) for Patchbay's smoke tests.

It offers one tool, `pb_echo`, which returns its `text` argument. With PB_MOCK_MCP_LOG set,
each request's method is appended to that file.
"""
import json
import os
import sys

LOG = os.environ.get("PB_MOCK_MCP_LOG")


def reply(id_, result=None, error=None):
    msg = {"jsonrpc": "2.0", "id": id_}
    if error:
        msg["error"] = error
    else:
        msg["result"] = result
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method, id_ = req.get("method"), req.get("id")
    if LOG:
        with open(LOG, "a") as f:
            f.write(f"{method}\n")
    if id_ is None:
        continue  # notifications (e.g. notifications/initialized)
    if method == "initialize":
        reply(id_, {
            "protocolVersion": req.get("params", {}).get("protocolVersion", "2025-03-26"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "pb-mock-mcp", "version": "0.1"},
        })
    elif method == "tools/list":
        reply(id_, {"tools": [{
            "name": "pb_echo",
            "description": "Echo the given text back.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]},
        }]})
    elif method == "tools/call":
        text = req.get("params", {}).get("arguments", {}).get("text", "")
        reply(id_, {"content": [{"type": "text", "text": f"echo: {text}"}]})
    elif method == "ping":
        reply(id_, {})
    else:
        reply(id_, error={"code": -32601, "message": f"unknown method {method}"})
