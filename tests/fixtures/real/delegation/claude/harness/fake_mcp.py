#!/usr/bin/env python3
"""Fake stdio MCP server. Logs every frame + env var NAMES (never values) as NDJSON.

usage: python3 -I fake_mcp.py <logfile> [server-label]
Tools: echo{text} -> returns "<label>:echo:<text>"; slow{secs}; crash{}; whoami{} -> label.
Env check: FAKE_BEARER sentinel compared to "fake-bearer-123" (a fake), logs only a boolean.
"""
import json
import os
import sys
import time

log_path = sys.argv[1]
label = sys.argv[2] if len(sys.argv) > 2 else "fake"
logf = open(log_path, "a", buffering=1)


def log(kind, **kw):
    logf.write(json.dumps({"t": round(time.time(), 3), "pid": os.getpid(), "label": label, "kind": kind, **kw}) + "\n")


log(
    "start",
    argv=sys.argv[1:],
    env_names=sorted(os.environ.keys()),
    fake_bearer_present="FAKE_BEARER" in os.environ,
    fake_bearer_matches=os.environ.get("FAKE_BEARER") == "fake-bearer-123",
    overlay_var_matches=os.environ.get("OVERLAY_VAR") == "from-overlay",
    cwd=os.getcwd(),
    # allowlisted non-secret values only
    safe_env={k: os.environ.get(k) for k in ("CLAUDE_EFFORT", "CLAUDE_CODE_ENTRYPOINT", "CLAUDECODE", "AI_AGENT") if k in os.environ},
    # sha256 prefixes only (never values): compare to the session_id in the init frame
    session_id_sha8=__import__("hashlib").sha256(os.environ.get("CLAUDE_CODE_SESSION_ID", "").encode()).hexdigest()[:8],
)

TOOLS = [
    {"name": "echo", "description": "Echo text back.", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
    {"name": "slow", "description": "Sleep for secs then return.", "inputSchema": {"type": "object", "properties": {"secs": {"type": "number"}}, "required": ["secs"]}},
    {"name": "crash", "description": "Exit the server process immediately.", "inputSchema": {"type": "object", "properties": {}}},
    {"name": "whoami", "description": "Return the server label.", "inputSchema": {"type": "object", "properties": {}}},
]


def send(obj):
    log("out", frame=obj)
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception as e:  # noqa: BLE001
        log("bad_json", error=str(e), raw=line[:200])
        continue
    log("in", frame=msg)
    m = msg.get("method")
    mid = msg.get("id")
    if m == "initialize":
        pv = msg.get("params", {}).get("protocolVersion", "2025-06-18")
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": pv, "capabilities": {"tools": {"listChanged": False}}, "serverInfo": {"name": "fake-mcp-" + label, "version": "0.0.1"}}})
    elif m == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
    elif m == "tools/call":
        p = msg.get("params", {})
        name = p.get("name")
        args = p.get("arguments", {}) or {}
        log("call", name=name, meta=p.get("_meta"), param_keys=sorted(p.keys()))
        if name == "echo":
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": f"{label}:echo:{args.get('text')}"}]}})
        elif name == "whoami":
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": f"I am {label}"}]}})
        elif name == "slow":
            time.sleep(float(args.get("secs", 5)))
            send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "slow done"}]}})
        elif name == "crash":
            log("crashing")
            os._exit(3)
        else:
            send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32602, "message": "unknown tool"}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "method not found"}})
    # notifications (no id): logged only
log("stdin_closed")
