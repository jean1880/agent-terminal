#!/usr/bin/env python3
"""Record the raw structured-protocol frames of a real agent CLI into a JSONL file.

Stdlib only, Python 3.11+. Durable tooling for refreshing tests/fixtures/real/*.

  claude  `claude --input-format stream-json --output-format stream-json
          --permission-prompt-tool stdio ...` (the argv agent-terminal uses)
  codex   `codex app-server` JSON-RPC: initialize, initialized, thread/start,
          then one turn/start per prompt

Every raw line in both directions is written as one JSON object per line:
  {"seq": N, "t": seconds, "dir": "in" | "out", "frame": {...}}
`in` is to the agent (what the app sends), `out` is from the agent, the same
convention as tests/support/scripted_codex.py. Text is masked (sk-..., ghp_...,
AIza..., Bearer ..., IPv4; home, host and user names → ~, host, user) BEFORE it is written; hook frames are
dropped and machine lists (skills, agents, plugins, MCP servers) emptied, so
fixtures carry no trace of the recording machine. `rescrub <file>...` re-applies
these rules to existing fixtures without running any agent. Each prompt is one real model turn;
the run ends after the last turn. Run it ONLY inside a throwaway sandbox repo.

Approval requests are answered from --decision, consumed in order (the last one
repeats). Codex decisions are sent verbatim ("accept", "decline", "cancel",
"acceptForSession", or a JSON object). A Codex MCP form (`mcpServer/elicitation/request`,
which is also how Codex asks to approve an MCP tool call) takes the next decision as its
action: "accept" (sent with empty content), "decline" or "cancel"; any other decision is
sent as "decline". Claude decisions are
allow | deny.

Example (Codex, Ask mode, decline the first approval):
  python3 tests/qa/record_frames.py codex --cwd /tmp/w0-sandbox \
      --approval-policy untrusted --sandbox readOnly \
      --prompt "Run ls -la" --decision decline --out /tmp/ask-ls.jsonl
"""

import argparse
import getpass
import json
import queue
import re
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

MASKS = [
    (re.compile(r"sk-[A-Za-z0-9_\-]{8,}"), lambda m: "sk-****" + m.group(0)[-4:]),
    (re.compile(r"gh[pousr]_[A-Za-z0-9]{16,}"), lambda m: m.group(0)[:4] + "****"),
    (re.compile(r"AIza[A-Za-z0-9_\-]{20,}"), lambda m: "AIza****"),
    (re.compile(r"(?i)(bearer\s+)[A-Za-z0-9._\-]{16,}"), lambda m: m.group(1) + "****"),
    (re.compile(r"\b(?:\d{1,3}\.){3}(\d{1,3})\b"), lambda m: "x.x.x." + m.group(1)),
    (re.compile(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}"), lambda m: "user@example.com"),
]

# Frames carrying the recording machine's state, not protocol: its hook output (session
# context), its MCP servers' startup, its remote-control host.
DROP_METHODS = ("hook/completed", "hook/started", "mcpServer/startupStatus/updated",
                "remoteControl/status/changed")

# Keys that describe the recording machine (its skills, agents, plugins, MCP
# servers, memory), not the protocol. Kept as keys so the frame shape survives,
# emptied so fixtures can ship in a public repo.
ENV_KEYS = ("tools", "mcp_servers", "slash_commands", "commands", "agents", "skills",
            "plugins", "memory_paths", "output_style", "organization")

# Machine identity, replaced longest-first: home path, host name, user name.
IDENTITY = sorted([(str(Path.home()), "~"), (socket.gethostname(), "host"),
                   (getpass.getuser(), "user")], key=lambda p: -len(p[0]))


def redact(text):
    for pattern, replace in MASKS:
        text = pattern.sub(replace, text)
    for value, placeholder in IDENTITY:
        if value:
            text = text.replace(value, placeholder)
    return text


def is_dropped(frame):
    """Hook frames: Codex `hook/*` methods and Claude `system` `hook_*` subtypes."""
    if not isinstance(frame, dict):
        return False
    if frame.get("method") in DROP_METHODS:
        return True
    return frame.get("type") == "system" and str(frame.get("subtype", "")).startswith("hook")


def scrub_env(value):
    """Empty every ENV_KEYS value, at any depth, keeping its type."""
    if isinstance(value, dict):
        return {k: (type(v)() if k in ENV_KEYS and isinstance(v, (list, dict, str)) else scrub_env(v))
                for k, v in value.items()}
    if isinstance(value, list):
        return [scrub_env(v) for v in value]
    return value


def store(raw):
    """The fixture form of one raw line: redacted, home-relative, machine lists emptied."""
    try:
        return scrub_env(json.loads(redact(raw)))
    except ValueError:
        return {"raw": redact(raw)}


def rescrub(paths):
    """Re-apply the current drop and scrub rules to existing fixtures, in place. Recorder
    fixtures ({seq, dir, frame} lines) are renumbered; any other JSONL (an agy transcript
    excerpt) is scrubbed line by line. No agent is run."""
    for path in paths:
        kept = []
        for line in Path(path).read_text().splitlines():
            if not line.strip():
                continue
            entry = json.loads(line)
            if isinstance(entry, dict) and "frame" in entry and "dir" in entry:
                if is_dropped(entry["frame"]):
                    continue
                entry["frame"] = store(json.dumps(entry["frame"]))
                entry["seq"] = len(kept) + 1
            elif is_dropped(entry):
                continue
            else:
                entry = store(line)
            kept.append(json.dumps(entry))
        tmp = Path(path).with_suffix(".tmp")
        tmp.write_text("\n".join(kept) + "\n")
        tmp.replace(path)
        print(f"{path}: {len(kept)} frames")
    return 0


class Session:
    def __init__(self, argv, cwd, out_path, timeout):
        self.timeout = timeout
        self.start = time.monotonic()
        self.seq = 0
        self.log = Path(out_path).open("w")
        self.lines = queue.Queue()
        self.proc = subprocess.Popen(
            argv, cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, bufsize=1)
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        for line in self.proc.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def _record(self, direction, raw):
        raw = raw.strip()
        try:
            frame = json.loads(raw)
        except ValueError:
            frame = {"raw": raw}
        if is_dropped(frame):
            return frame
        stored = store(raw)
        self.seq += 1
        self.log.write(json.dumps({"seq": self.seq, "t": round(time.monotonic() - self.start, 2),
                                   "dir": direction, "frame": stored}) + "\n")
        self.log.flush()
        return frame

    def send(self, frame):
        raw = json.dumps(frame)
        self._record("in", raw)
        self.proc.stdin.write(raw + "\n")
        self.proc.stdin.flush()

    def recv(self, deadline):
        """Next frame from the agent, or None on exit/timeout."""
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return None
            try:
                line = self.lines.get(timeout=remaining)
            except queue.Empty:
                return None
            if line is None:
                return None
            if line.strip():
                return self._record("out", line)

    def close(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.terminate()
        self.log.close()


class Decisions:
    def __init__(self, items):
        self.items = list(items)

    def next(self, default):
        if not self.items:
            return default
        item = self.items.pop(0) if len(self.items) > 1 else self.items[0]
        if item[:1] in "{[":
            return json.loads(item)
        return item


def claude_argv(args):
    argv = [args.program, *args.extra_arg,
            "--output-format", "stream-json", "--verbose",
            "--input-format", "stream-json",
            "--permission-prompt-tool", "stdio", "--include-partial-messages"]
    if args.model:
        argv += ["--model", args.model]
    if args.thinking_display:
        argv += ["--thinking-display", args.thinking_display]
    argv += ["--permission-mode", args.permission_mode]
    return argv


def codex_argv(args):
    return [args.program, *args.extra_arg, "app-server"]


def run_claude(args, decisions):
    s = Session(claude_argv(args), args.cwd, args.out, args.timeout)
    s.send({"type": "control_request", "request_id": "init-1",
            "request": {"subtype": "initialize"}})
    for prompt in args.prompt:
        s.send({"type": "user", "message": {"role": "user", "content": prompt},
                "parent_tool_use_id": None})
        deadline = time.monotonic() + args.timeout
        while True:
            frame = s.recv(deadline)
            if frame is None:
                print("claude: exited or timed out mid-turn", file=sys.stderr)
                s.close()
                return 1
            if frame.get("type") == "result":
                break
            request = frame.get("request") or {}
            if frame.get("type") == "control_request" and request.get("subtype") == "can_use_tool":
                allow = decisions.next("deny") == "allow"
                body = {"behavior": "allow", "updatedInput": request.get("input", {})} if allow \
                    else {"behavior": "deny", "message": "The user denied this request"}
                body["toolUseID"] = request.get("tool_use_id")
                s.send({"type": "control_response", "response": {
                    "subtype": "success", "request_id": frame["request_id"], "response": body}})
    s.close()
    return 0


SANDBOXES = ("readOnly", "workspaceWrite", "dangerFullAccess")


def run_codex(args, decisions):
    s = Session(codex_argv(args), args.cwd, args.out, args.timeout)

    def await_result(request_id, deadline):
        while True:
            frame = s.recv(deadline)
            if frame is None:
                return None
            if frame.get("id") == request_id and "method" not in frame:
                return frame

    deadline = time.monotonic() + args.timeout
    s.send({"id": 1, "method": "initialize", "params": {"clientInfo": {
        "name": "agent-terminal", "title": "Agent Terminal", "version": "w0-recorder"}}})
    if await_result(1, deadline) is None:
        s.close()
        return 1
    s.send({"method": "initialized"})
    params = {"cwd": args.cwd, "approvalsReviewer": "user",
              "config": {"tools.update_plan.enabled": True}}
    if args.model:
        params["model"] = args.model
    s.send({"id": 2, "method": "thread/start", "params": params})
    started = await_result(2, deadline)
    try:
        thread = started["result"]["thread"]["id"]
    except (TypeError, KeyError):
        s.close()
        return 1
    next_id = 2
    for prompt in args.prompt:
        next_id += 1
        turn = {"threadId": thread, "input": [{"type": "text", "text": prompt}],
                "cwd": args.cwd, "approvalPolicy": args.approval_policy,
                "approvalsReviewer": "user", "sandboxPolicy": {"type": args.sandbox},
                "summary": "detailed"}
        if args.model:
            turn["model"] = args.model
        s.send({"id": next_id, "method": "turn/start", "params": turn})
        deadline = time.monotonic() + args.timeout
        while True:
            frame = s.recv(deadline)
            if frame is None:
                print("codex: exited or timed out mid-turn", file=sys.stderr)
                s.close()
                return 1
            method = frame.get("method", "")
            if method == "turn/completed":
                break
            if "id" in frame and method:
                if method.endswith("requestApproval"):
                    s.send({"id": frame["id"], "result": {"decision": decisions.next("decline")}})
                elif method == "mcpServer/elicitation/request":
                    action = decisions.next("decline")
                    if action not in ("accept", "decline", "cancel"):
                        action = "decline"  # a form takes no decision object or session scope
                    result = {"action": action}
                    if action == "accept":
                        result["content"] = {}
                    s.send({"id": frame["id"], "result": result})
                else:
                    s.send({"id": frame["id"], "error": {
                        "code": -32601, "message": "recorder does not answer " + method}})
    s.close()
    return 0


def main():
    if sys.argv[1:2] == ["rescrub"]:
        return rescrub(sys.argv[2:])
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("mode", choices=("claude", "codex"))
    p.add_argument("--cwd", required=True, help="sandbox repo; never a real project")
    p.add_argument("--out", required=True, help="JSONL path to write")
    p.add_argument("--prompt", action="append", required=True, help="one real turn each")
    p.add_argument("--decision", action="append", default=[], help="approval answers, in order")
    p.add_argument("--program", help="CLI binary (default: the mode name)")
    p.add_argument("--model")
    p.add_argument("--timeout", type=int, default=240, help="seconds per turn")
    p.add_argument("--extra-arg", action="append", default=[], help="extra CLI argument")
    p.add_argument("--approval-policy", default="untrusted", help="codex approvalPolicy")
    p.add_argument("--sandbox", default="readOnly", choices=SANDBOXES, help="codex sandboxPolicy type")
    p.add_argument("--permission-mode", default="default", help="claude --permission-mode")
    p.add_argument("--thinking-display", help="claude --thinking-display value, e.g. summarized")
    args = p.parse_args()
    args.program = args.program or args.mode
    decisions = Decisions(args.decision)
    return run_claude(args, decisions) if args.mode == "claude" else run_codex(args, decisions)


if __name__ == "__main__":
    sys.exit(main())
