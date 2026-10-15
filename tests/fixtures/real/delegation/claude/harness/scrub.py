#!/usr/bin/env python3
"""Scrub recorded runs into tests/fixtures/real/delegation/claude/. usage: scrub.py <outroot> <destroot>"""
import json
import os
import re
import sys

src, dst = sys.argv[1], sys.argv[2]
MAP = {  # dest name -> source dir
    "d1_basic": "d1_basic", "d1_precedence": "d1_precedence2", "d4_effort_low": "d4_effort", "d4_effort_max": "d4_effort_hi",
    "d4_badmodel": "d4_badmodel", "d4_badeffort": "d4_badeffort", "d5_busy_replay": "d5_replay", "d5_idle_a": "d5_idle_a",
    "d5_idle_b_resume": "d5_idle_b", "d6_plan": "d6_plan", "d6_ask_user_rules": "d6_outside", "d6_restricted": "d6_restricted",
    "d6_default_nouser_mcp_ask": "d6_ask_nouser", "d7_crash": "d7_crash", "d7_interrupt": "d7_interrupt", "d7_killmcp": "d7_killmcp",
}
DROP_SYS = {"commands_changed", "status", "thinking_tokens"}
HOME = "/home/jdesroches"
REPO = HOME + "/git/agent-terminal"


def clean_str(s):
    s = s.replace(REPO, "/REPO").replace(HOME, "/home/USER").replace("-home-jdesroches-git-agent-terminal", "-REPO").replace("-home-jdesroches", "-home-USER").replace("jdesroches", "USER")
    s = re.sub(r"[A-Za-z0-9._-]+@[A-Za-z0-9.-]+\.[a-z]{2,}", "user@example.invalid", s)
    s = re.sub(r"\b(sk-[A-Za-z0-9_-]{8,}|ghp_[A-Za-z0-9]{8,}|AIza[A-Za-z0-9_-]{8,}|eyJ[A-Za-z0-9_.-]{20,})", "FAKE-TOKEN", s)
    return s


def walk(o, key=None):
    if isinstance(o, dict):
        out = {}
        for k, v in o.items():
            if k == "signature" and isinstance(v, str):
                out[k] = "SIGNATURE-SCRUBBED"
            elif k in ("messaging_socket_path",):
                out[k] = "/run/user/UID/cc-socks/PID.sock"
            elif k == "output" and isinstance(v, str) and len(v) > 200:
                out[k] = "HOOK-OUTPUT-SCRUBBED"
            else:
                out[k] = walk(v, k)
        return out
    if isinstance(o, list):
        return [walk(x) for x in o]
    if isinstance(o, str):
        return clean_str(o)
    return o


def reduce_init_response(resp):
    models = [{k: m.get(k) for k in ("value", "resolvedModel", "displayName", "supportsEffort", "supportedEffortLevels")} for m in resp.get("models", [])]
    return {"_scrubbed": "commands/agents/account dropped", "models": models, "claude_code_version": resp.get("claude_code_version"),
            "current_permission_mode": resp.get("current_permission_mode"), "session_state": resp.get("session_state"), "capabilities": resp.get("capabilities")}


for name, sd in MAP.items():
    d = os.path.join(src, sd)
    if not os.path.isdir(d):
        print("skip", name)
        continue
    od = os.path.join(dst, name)
    os.makedirs(od, exist_ok=True)
    with open(os.path.join(od, "frames.ndjson"), "w") as f:
        for line in open(os.path.join(d, "frames.ndjson")):
            r = json.loads(line)
            x = r.get("line")
            if isinstance(x, dict):
                t = x.get("type")
                if t == "rate_limit_event" or (t == "system" and (str(x.get("subtype", "")).startswith("hook") or x.get("subtype") in DROP_SYS)):
                    continue
                if t == "control_response" and "commands" in x.get("response", {}).get("response", {}):
                    r["line"] = {"type": "control_response", "response": {"subtype": "success", "request_id": x["response"]["request_id"], "response": reduce_init_response(x["response"]["response"])}}
                    f.write(json.dumps(r) + "\n")
                    continue
                if t == "system" and x.get("subtype") == "init":
                    x["mcp_servers"] = [m for m in x.get("mcp_servers", []) if m.get("source") == "dynamic"] + [{"_scrubbed": "%d non-dynamic servers dropped" % len([m for m in x.get("mcp_servers", []) if m.get("source") != "dynamic"])}]
                    for k in ("slash_commands", "terminal_slash_commands", "skills", "plugins", "agents", "tools"):
                        if k in x:
                            x[k] = "DROPPED(n=%d)" % len(x[k]) if k != "tools" else [y for y in x[k] if y.startswith("mcp__fake__") or y.startswith("mcp__plans__who")]
            f.write(json.dumps(walk(r)) + "\n")
    with open(os.path.join(od, "mcp-server.ndjson"), "w") as f:
        for line in open(os.path.join(d, "mcp-server.ndjson")):
            r = json.loads(line)
            f.write(json.dumps(walk(r)) + "\n")
    a = open(os.path.join(d, "argv.txt")).read()
    open(os.path.join(od, "argv.txt"), "w").write(clean_str(a))
    print("ok", name)
