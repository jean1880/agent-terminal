#!/usr/bin/env python3
"""Drive `claude` in the app's stream-json mode, record all stdout frames.

usage: python3 -I drive.py <scenario> <outdir> [config_dir|-]
Scenarios are defined in SCEN below. Never prints env values.
"""
import json
import os
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
FAKE = os.path.join(os.path.dirname(HERE), "fake-mcp", "fake_mcp.py")

scenario, outdir = sys.argv[1], sys.argv[2]
cfgdir = sys.argv[3] if len(sys.argv) > 3 else "-"
os.makedirs(outdir, exist_ok=True)
work = os.path.join(HERE, "work")
os.makedirs(work, exist_ok=True)
mcplog = os.path.join(outdir, "mcp-server.ndjson")
open(mcplog, "w").close()


def mcp_cfg(servers):
    path = os.path.join(outdir, "mcp-config.json")
    json.dump({"mcpServers": servers}, open(path, "w"), indent=1)
    return path


def fake(label, extra_env=None):
    e = {"OVERLAY_VAR": "from-overlay"}
    e.update(extra_env or {})
    return {"type": "stdio", "command": "python3", "args": ["-I", FAKE, mcplog, label], "env": e}


def user(text):
    return json.dumps({"type": "user", "message": {"role": "user", "content": text}, "parent_tool_use_id": None})


def control(subtype, rid, **kw):
    return json.dumps({"type": "control_request", "request_id": rid, "request": {"subtype": subtype, **kw}})


BASE = ["--output-format", "stream-json", "--verbose", "--input-format", "stream-json",
        "--permission-prompt-tool", "stdio", "--include-partial-messages"]

# each scenario: dict(servers, extra, steps) ; steps: list of ("send", line) | ("wait_result", n) | ("sleep", s) | ("wait_for", substr, timeout) | ("kill_mcp",)
HAIKU = ["--model", "haiku"]
CALL = "Call the MCP tool mcp__{s}__echo with text 'hi'. Then reply with just the word done."

SCEN = {
    "d1_basic": dict(servers={"fake": fake("A")}, extra=HAIKU + ["--strict-mcp-config", "--permission-mode", "bypassPermissions"],
                     steps=[("send", user(CALL.format(s="fake"))), ("wait_result", 1)]),
    "d1_default_perm": dict(servers={"fake": fake("A")}, extra=HAIKU + ["--strict-mcp-config", "--permission-mode", "default"],
                            steps=[("send", user(CALL.format(s="fake"))), ("wait_result", 1)], auto_allow=True),
    "d1_precedence": dict(servers={"plans": fake("OVERLAY")}, extra=HAIKU + ["--permission-mode", "bypassPermissions"],
                          
                          steps=[("send", user("Call the MCP tool mcp__plans__whoami and tell me the exact text it returns.")), ("wait_result", 1)]),
    "d4_effort": dict(servers={"fake": fake("A")}, extra=["--model", "haiku", "--effort", "low", "--strict-mcp-config", "--permission-mode", "bypassPermissions"],
                      steps=[("send", user("Reply with just: ok")), ("wait_result", 1)]),
    "d4_effort_hi": dict(servers={"fake": fake("A")}, extra=["--model", "haiku", "--effort", "max", "--strict-mcp-config", "--permission-mode", "bypassPermissions"],
                      steps=[("send", user("Reply with just: ok")), ("wait_result", 1)]),
    "d4_badmodel": dict(servers={"fake": fake("A")}, extra=["--model", "claude-nonexistent-9", "--strict-mcp-config", "--permission-mode", "bypassPermissions"],
                      steps=[("send", user("Reply with just: ok")), ("wait_result", 1)]),
    "d4_badeffort": dict(servers={"fake": fake("A")}, extra=["--model", "haiku", "--effort", "bogus", "--strict-mcp-config", "--permission-mode", "bypassPermissions"],
                      steps=[("send", user("Reply with just: ok")), ("wait_result", 1, 20)]),
    "d5_replay": dict(servers={"fake": fake("A")}, extra=HAIKU + ["--replay-user-messages", "--strict-mcp-config", "--permission-mode", "bypassPermissions"],
                    steps=[("send", user("Call mcp__fake__slow with secs 6, then reply with the word alpha.")),
                           ("wait_for", "tool_use", 60), ("sleep", 2),
                           ("send", user("Also append the word BRAVO to your reply.")),
                           ("wait_result", 1, 90), ("sleep", 1),
                           ("send", user("Reply with just: CHARLIE")), ("wait_result", 2, 60)]),
    "d5_idle_a": dict(servers={"fake": fake("A")}, persist=True, extra=HAIKU + ["--session-id", "6f1d2c3a-0000-4000-8000-00000000d5aa", "--strict-mcp-config", "--permission-mode", "bypassPermissions"],
                    steps=[("send", user("Remember the secret word PINEAPPLE. Reply with just: ok")), ("wait_result", 1, 60)]),
    "d5_idle_b": dict(servers={"fake": fake("A")}, persist=True, extra=HAIKU + ["--resume=6f1d2c3a-0000-4000-8000-00000000d5aa", "--strict-mcp-config", "--permission-mode", "bypassPermissions"],
                    steps=[("send", user("What was the secret word? Reply with just the word.")), ("wait_result", 1, 60)]),
    "d6_ask_nouser": dict(servers={"fake": fake("A")}, extra=HAIKU + ["--setting-sources", "local", "--strict-mcp-config", "--permission-mode", "default"], decide="deny_write_allow_rest",
                   steps=[("send", user("First call mcp__fake__echo with text 'q'. Then create a file named ask2_probe.txt containing 'x' in the current directory using your Write tool. Report both outcomes in one line.")), ("wait_result", 1, 90)]),
    "d6_outside": dict(servers={"fake": fake("A")}, extra=HAIKU + ["--setting-sources", "local", "--strict-mcp-config", "--permission-mode", "default"], decide="deny_write_allow_rest",
                   steps=[("send", user("Create the file /home/jdesroches/git/agent-terminal/target/delegation/claude/outside/o_probe.txt containing 'x' using your Write tool. Report the outcome in one line.")), ("wait_result", 1, 90)]),
    "d6_manual": dict(servers={"fake": fake("A")}, extra=HAIKU + ["--setting-sources", "local", "--strict-mcp-config", "--permission-mode", "manual"], decide="deny_write_allow_rest",
                   steps=[("send", user("Create a file named manual_probe.txt containing 'x' in the current directory using your Write tool. Report the outcome in one line.")), ("wait_result", 1, 90)]),
    "d6_restricted": dict(servers={"fake": fake("A")}, extra=HAIKU + ["--restricted", "--strict-mcp-config", "--permission-mode", "default"], decide="deny_write_allow_rest",
                   steps=[("send", user("Do three things in order and report each outcome in one line: (1) call mcp__fake__echo with text 'r'; (2) Write inner_probe.txt containing 'x' in the current directory; (3) Write /home/jdesroches/git/agent-terminal/target/delegation/claude/outside/r_probe.txt containing 'x'.")), ("wait_result", 1, 90)]),
    "d5_busy": dict(servers={"fake": fake("A")}, extra=HAIKU + ["--strict-mcp-config", "--permission-mode", "bypassPermissions"],
                    steps=[("send", user("Call mcp__fake__slow with secs 8, then reply with the word alpha.")),
                           ("wait_for", "tool_use", 60), ("sleep", 2),
                           ("send", user("Also, when finished, append the word BRAVO to your reply.")),
                           ("wait_result", 2, 120)]),
    "d6_plan": dict(servers={"fake": fake("A")}, extra=HAIKU + ["--strict-mcp-config", "--permission-mode", "plan"],
                    steps=[("send", user("Create a file named plan_probe.txt containing 'x' in the current directory using your Write tool, and call mcp__fake__echo with text 'p'.")), ("wait_result", 1)], auto_allow=False),
    "d6_ask": dict(servers={"fake": fake("A")}, extra=HAIKU + ["--strict-mcp-config", "--permission-mode", "default"],
                   steps=[("send", user("Create a file named ask_probe.txt containing 'x' in the current directory using your Write tool, then reply done.")), ("wait_result", 1)], decide="deny_first"),
    "d7_crash": dict(servers={"fake": fake("A")}, extra=HAIKU + ["--strict-mcp-config", "--permission-mode", "bypassPermissions"],
                     steps=[("send", user("Call mcp__fake__crash. Then call mcp__fake__echo with text 'after'. Report what happened in one line.")), ("wait_result", 1, 120)]),
    "d7_interrupt": dict(servers={"fake": fake("A")}, extra=HAIKU + ["--strict-mcp-config", "--permission-mode", "bypassPermissions"],
                         steps=[("send", user("Call mcp__fake__slow with secs 30.")), ("wait_for", "tool_use", 60), ("sleep", 2),
                                ("send", control("interrupt", "int-1")), ("wait_result", 1, 60)]),
    "d7_killmcp": dict(servers={"fake": fake("A")}, extra=HAIKU + ["--strict-mcp-config", "--permission-mode", "bypassPermissions"],
                       steps=[("send", user("Call mcp__fake__slow with secs 30.")), ("wait_for", "tool_use", 60), ("sleep", 2),
                              ("kill_mcp",), ("wait_result", 1, 90)]),
}

sc = SCEN[scenario]
if sc.get("project_mcp"):
    # project-scope .mcp.json in the throwaway work dir (NOT the real home)
    pdir = os.path.join(outdir, "proj")
    os.makedirs(pdir, exist_ok=True)
    json.dump({"mcpServers": sc["project_mcp"]}, open(os.path.join(pdir, ".mcp.json"), "w"))
    work = pdir
cfg = mcp_cfg(sc["servers"])
argv = ["claude"] + BASE + ["--mcp-config", cfg] + sc["extra"]
if not sc.get("persist"):
    argv.append("--no-session-persistence")
env = dict(os.environ)
env["FAKE_BEARER"] = "fake-bearer-123"
env["NO_COLOR"] = "1"
if cfgdir != "-":
    env["CLAUDE_CONFIG_DIR"] = cfgdir
with open(os.path.join(outdir, "argv.txt"), "w") as f:
    f.write(" ".join(argv) + "\n")

p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, cwd=work, env=env, text=True, bufsize=1)
frames = []
lock = threading.Lock()
out = open(os.path.join(outdir, "frames.ndjson"), "w", buffering=1)
err = open(os.path.join(outdir, "stderr.txt"), "w", buffering=1)
t0 = time.time()


def wr(line):
    try:
        p.stdin.write(line + "\n")
        p.stdin.flush()
        out.write(json.dumps({"dir": "->", "t": round(time.time() - t0, 2), "line": json.loads(line)}) + "\n")
    except Exception as e:  # noqa: BLE001
        out.write(json.dumps({"dir": "!!", "error": str(e)}) + "\n")


def reader():
    for line in p.stdout:
        line = line.strip()
        if not line:
            continue
        try:
            obj = json.loads(line)
        except Exception:  # noqa: BLE001
            obj = {"raw": line[:300]}
        with lock:
            frames.append(obj)
        out.write(json.dumps({"dir": "<-", "t": round(time.time() - t0, 2), "line": obj}) + "\n")
        # permission requests
        if obj.get("type") == "control_request" and obj.get("request", {}).get("subtype") == "can_use_tool":
            rq = obj["request"]
            if sc.get("decide") == "deny_write_allow_rest":
                body = ({"behavior": "deny", "message": "denied by test"} if rq.get("tool_name") in ("Write", "Edit") else {"behavior": "allow", "updatedInput": rq.get("input", {})})
            elif sc.get("decide") == "deny_first":
                body = {"behavior": "deny", "message": "denied by test"}
            elif sc.get("auto_allow", True):
                body = {"behavior": "allow", "updatedInput": rq.get("input", {})}
            else:
                body = {"behavior": "deny", "message": "denied by test"}
            wr(json.dumps({"type": "control_response", "response": {"subtype": "success", "request_id": obj["request_id"], "response": body}}))


def errr():
    for line in p.stderr:
        err.write(line)


threading.Thread(target=reader, daemon=True).start()
threading.Thread(target=errr, daemon=True).start()


def count(pred):
    with lock:
        return sum(1 for f in frames if pred(f))


def wait(pred, n, timeout):
    end = time.time() + timeout
    while time.time() < end:
        if count(pred) >= n or p.poll() is not None:
            return count(pred) >= n
        time.sleep(0.2)
    return False


# initialize control handshake like the app
wr(control("initialize", "init-1"))
time.sleep(1.0)
for step in sc["steps"]:
    k = step[0]
    if k == "send":
        wr(step[1])
    elif k == "sleep":
        time.sleep(step[1])
    elif k == "wait_result":
        wait(lambda f: f.get("type") == "result", step[1], step[2] if len(step) > 2 else 90)
    elif k == "wait_for":
        wait(lambda f, s=step[1]: s in json.dumps(f), 1, step[2])
    elif k == "kill_mcp":
        os.system("pkill -f 'fake_mcp.py " + mcplog + "'")
        out.write(json.dumps({"dir": "##", "note": "killed mcp server", "t": round(time.time() - t0, 2)}) + "\n")
time.sleep(1.0)
try:
    p.stdin.close()
except Exception:  # noqa: BLE001
    pass
try:
    p.wait(timeout=20)
except subprocess.TimeoutExpired:
    p.kill()
out.write(json.dumps({"dir": "##", "note": "exit", "rc": p.returncode, "t": round(time.time() - t0, 2)}) + "\n")
print("done", scenario, "rc", p.returncode)
