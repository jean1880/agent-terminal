#!/usr/bin/env python3
"""Summarise frames.ndjson + mcp-server.ndjson compactly. usage: summ.py <outdir>"""
import json
import sys

d = sys.argv[1]
SKIP_STREAM = {"content_block_delta", "message_delta", "message_start", "message_stop", "content_block_start", "content_block_stop"}
for line in open(d + "/frames.ndjson"):
    r = json.loads(line)
    l = r.get("line")
    if r["dir"] == "##":
        print("##", r)
        continue
    if not isinstance(l, dict):
        print(r)
        continue
    t = l.get("type")
    if t == "control_response" and "commands" in json.dumps(l)[:400]:
        print(r["t"], r["dir"], "control_response(initialize) keys", list(l["response"]["response"].keys()))
        continue
    if t == "rate_limit_event" or (t == "system" and (str(l.get("subtype", "")).startswith("hook") or l.get("subtype") in ("commands_changed", "status", "thinking_tokens"))):
        continue
    if t == "stream_event":
        ev = l.get("event", {})
        if ev.get("type") in SKIP_STREAM:
            continue
    if t == "system" and l.get("subtype") == "init":
        keep = {k: l.get(k) for k in ("session_id", "model", "permissionMode", "mcp_servers", "apiKeySource", "per_turn_effort_active")}
        keep["mcp_tools_n"] = len([x for x in l.get("tools", []) if x.startswith("mcp__")]); keep["mcp_tools_fake"] = [x for x in l.get("tools", []) if x.startswith("mcp__fake") or x.startswith("mcp__dup") or x.startswith("mcp__plans__who")]; keep["mcp_servers"] = [(s["name"], s["status"], s["source"]) for s in l.get("mcp_servers", [])]
        print(r["t"], r["dir"], "INIT", json.dumps(keep))
        continue
    if t == "assistant":
        c = l["message"]["content"]
        print(r["t"], r["dir"], "assistant model=%s" % l["message"].get("model"), json.dumps(c)[:400], "parent=", l.get("parent_tool_use_id"))
        continue
    if t == "user":
        print(r["t"], r["dir"], "user", json.dumps(l)[:500])
        continue
    if t == "result":
        keep = {k: l.get(k) for k in ("subtype", "is_error", "terminal_reason", "result", "num_turns", "stop_reason", "permission_denials", "modelUsage", "session_id")}
        print(r["t"], r["dir"], "RESULT", json.dumps(keep)[:900])
        continue
    print(r["t"], r["dir"], json.dumps(l)[:400])
if len(sys.argv) > 2:
    sys.exit(0)
print("---- mcp server log")
for line in open(d + "/mcp-server.ndjson"):
    r = json.loads(line)
    if r["kind"] == "start":
        r["env_names"] = "%d names" % len(r["env_names"])
    print(json.dumps(r)[:600])
