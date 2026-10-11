#!/usr/bin/env python3
"""Opt-in live smoke: assert the protocol CONTRACT FACTS of a real agent CLI (plan D7).

  python3 tests/qa/live_smoke.py --agent claude|codex|agy [--dry-run]

QUOTA WARNING. Every run without --dry-run spends REAL model turns (claude 2, codex 2,
agy 1) on the user's subscription. Get the user's approval per run. --dry-run prints the
exact commands and the turn plan and runs nothing.

Stdlib only, Python 3.11+. The CLI is driven inside a throwaway sandbox repo under the
temp dir (never $HOME; the cwd is passed explicitly and asserted). Spawning, framing and
scrubbing come from tests/qa/record_frames.py, so every frame is masked before it is
stored and the checks run on the same JSONL shape as tests/fixtures/real/*. The check
functions are pure over that shape; test_live_smoke.py proves them against the W0
fixtures without spending quota.

Contract facts asserted:
  claude  `--thinking-display summarized` starts without error and yields non-empty
          thinking; a `can_use_tool` request for `touch x.tmp` arrives and a deny
          control_response lets the turn continue.
  codex   `item/commandExecution/requestApproval` carries a wrapped command and
          `commandActions`; an unoffered `decline` is accepted (item ends `declined`, turn
          continues); on `untrusted` + `workspaceWrite` an edit asks and `accept` applies it.
  agy     `step_update` tool events carry `tool_name` and `tool_info.parameters`; the
          reported cwd is the sandbox.

Exit status: 0 all checks passed, 1 a check or a run failed, 2 bad usage or sandbox guard.
"""

import argparse
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import record_frames as rec  # noqa: E402

MARKER = ".live-smoke-sandbox"
PREFIX = "live-smoke-"
FIXTURE_TEXT = "hello world\n"

THINK_PROMPT = ("Without using any tools: if a train leaves at 3:47pm and the trip takes "
                "2h 38m, what time does it arrive? Think it through, then answer in one line.")
TOUCH_PROMPT = "Run: touch x.tmp"
SHELL_PROMPT = "Run the shell command: ls -la   Do nothing else."
EDIT_PROMPT = ("Edit fixture.txt: change 'hello world' to 'hello there'. "
               "Use a file edit (apply_patch), nothing else.")
AGY_PROMPT = "Read the file fixture.txt with your file-viewing tool. Do nothing else."

TURNS = {"claude": 2, "codex": 2, "agy": 1}


class SandboxError(Exception):
    pass


# ---------------------------------------------------------------- sandbox guard

def assert_sandbox(path):
    """Refuse any cwd that is not a live-smoke sandbox under the temp dir. Returns the
    resolved path. The guard is what keeps a slip (W0's agy run used $HOME) from happening."""
    if not path or not os.path.isabs(str(path)):
        raise SandboxError(f"sandbox must be an absolute path, got {str(path)!r}")
    real = Path(path).resolve()
    home = Path.home().resolve()
    tmp = Path(tempfile.gettempdir()).resolve()
    if real == home or real in home.parents or real == Path("/"):
        raise SandboxError(f"sandbox {real} is $HOME or an ancestor of it")
    if tmp not in real.parents:
        raise SandboxError(f"sandbox {real} is not under {tmp}")
    if not real.parent.name.startswith(PREFIX) or real.parent == tmp:
        raise SandboxError(f"sandbox {real} is not inside a {PREFIX}* work directory")
    if not (real / MARKER).is_file():
        raise SandboxError(f"sandbox {real} lacks the {MARKER} marker")
    return real


def make_workdir():
    """Create <tmp>/live-smoke-XXXX/{sandbox,frames}; return (work, sandbox, frames)."""
    work = Path(tempfile.mkdtemp(prefix=PREFIX))
    sandbox = work / "sandbox"
    frames = work / "frames"
    sandbox.mkdir()
    frames.mkdir()
    (sandbox / MARKER).write_text("throwaway sandbox for tests/qa/live_smoke.py\n")
    (sandbox / "fixture.txt").write_text(FIXTURE_TEXT)
    return work, sandbox, frames


# ---------------------------------------------------------------- plan

def _ns(**kw):
    base = dict(program=None, extra_arg=[], model=None, thinking_display=None,
                permission_mode="default", approval_policy="untrusted", sandbox="readOnly",
                timeout=180, prompt=[], cwd=None, out=None)
    base.update(kw)
    return argparse.Namespace(**base)


def build_plan(agent, program, model, timeout, sandbox, frames):
    """The ordered scenarios for an agent. Each: name, kind, argv, prompts, decisions, ns,
    out. `sandbox` and `frames` are paths (placeholders in --dry-run)."""
    sandbox = str(sandbox)
    frames = Path(frames)

    def scenario(name, kind, argv, prompts, decisions, ns):
        return {"name": name, "kind": kind, "argv": argv, "prompts": prompts,
                "decisions": decisions, "ns": ns, "out": str(frames / f"{name}.jsonl")}

    if agent == "claude":
        ns = _ns(program=program or "claude", model=model, timeout=timeout, cwd=sandbox,
                 thinking_display="summarized", prompt=[THINK_PROMPT, TOUCH_PROMPT])
        return [scenario("claude-thinking-deny", "claude", rec.claude_argv(ns),
                         ns.prompt, ["deny"], ns)]
    if agent == "codex":
        shell = _ns(program=program or "codex", model=model, timeout=timeout, cwd=sandbox,
                    approval_policy="untrusted", sandbox="readOnly", prompt=[SHELL_PROMPT])
        edit = _ns(program=program or "codex", model=model, timeout=timeout, cwd=sandbox,
                   approval_policy="untrusted", sandbox="workspaceWrite", prompt=[EDIT_PROMPT])
        return [scenario("codex-shell-decline", "codex", rec.codex_argv(shell),
                         shell.prompt, ["decline"], shell),
                scenario("codex-edit-accept", "codex", rec.codex_argv(edit),
                         edit.prompt, ["accept"], edit)]
    ns = _ns(program=program or "agy", model=model, timeout=timeout, cwd=sandbox, prompt=[AGY_PROMPT])
    return [scenario("agy-view-file", "agy", agy_argv(ns), ns.prompt, [], ns)]


def agy_argv(ns):
    argv = [ns.program, "-p", ns.prompt[0], "--output-format", "stream-json", "--mode", "plan"]
    if ns.model:
        argv += ["--model", ns.model]
    return argv


def render_plan(agent, plan, sandbox):
    lines = [f"live_smoke dry-run: agent={agent}; nothing is run, no sandbox is created",
             f"sandbox cwd (explicit, asserted before every spawn): {sandbox}",
             f"turns: {sum(len(s['prompts']) for s in plan)} real model turns per run "
             f"(each spends quota; get the user's approval per run)"]
    for s in plan:
        lines.append(f"scenario {s['name']} ({s['kind']}), cwd={sandbox}")
        lines.append("  command: " + shlex.join(s["argv"]))
        for i, p in enumerate(s["prompts"], 1):
            lines.append(f"  turn {i}: {p}")
        if s["decisions"]:
            lines.append("  approval answers, in order: " + ", ".join(s["decisions"]))
        if s["kind"] == "codex":
            lines.append(f"  approvalPolicy={s['ns'].approval_policy} sandboxPolicy={s['ns'].sandbox}")
    return "\n".join(lines)


# ---------------------------------------------------------------- frame helpers

def load_entries(path):
    """JSONL → list of {dir, frame} entries. Recorder files already have that shape; any
    other line (an agy event) becomes {"dir": "out", "frame": line}."""
    entries = []
    for line in Path(path).read_text().splitlines():
        if not line.strip():
            continue
        obj = json.loads(line)
        if isinstance(obj, dict) and "frame" in obj and "dir" in obj:
            entries.append(obj)
        else:
            entries.append({"dir": "out", "frame": obj})
    return entries


def _frames(entries, direction):
    return [e["frame"] for e in entries if e.get("dir") == direction and isinstance(e["frame"], dict)]


def _index(entries, predicate, start=0):
    for i in range(start, len(entries)):
        frame = entries[i]["frame"]
        if isinstance(frame, dict) and predicate(entries[i]["dir"], frame):
            return i
    return -1


def _same_dir(a, b):
    try:
        return Path(a).resolve() == Path(b).resolve()
    except (OSError, TypeError, ValueError):
        return False


def check(name, ok, detail=""):
    return (name, bool(ok), detail)


# ---------------------------------------------------------------- claude checks

def claude_checks(entries, sandbox=None):
    out = _frames(entries, "out")
    results = []
    init_ok = any(f.get("type") == "control_response"
                  and f.get("response", {}).get("subtype") == "success" for f in out)
    results.append(check("claude: initialize answered success", init_ok))
    errors = [f for f in out if f.get("type") == "result" and f.get("is_error")]
    results.append(check("claude: --thinking-display summarized starts without error",
                         init_ok and not errors, f"{len(errors)} error result(s)" if errors else ""))
    thinking = "".join(
        f["event"]["delta"].get("thinking") or ""
        for f in out if f.get("type") == "stream_event"
        and f["event"].get("type") == "content_block_delta"
        and f["event"].get("delta", {}).get("type") == "thinking_delta")
    for f in out:
        if f.get("type") == "assistant":
            for block in f.get("message", {}).get("content", []):
                if block.get("type") == "thinking":
                    thinking = thinking or (block.get("thinking") or "")
    results.append(check("claude: non-empty thinking", thinking.strip(), f"{len(thinking)} chars"))
    if sandbox is not None:
        init = [f for f in out if f.get("type") == "system" and f.get("subtype") == "init"]
        results.append(check("claude: cwd is the sandbox",
                             bool(init) and _same_dir(init[0].get("cwd"), sandbox),
                             f"reported {init[0].get('cwd') if init else None!r}"))
    ask = _index(entries, lambda d, f: d == "out" and f.get("type") == "control_request"
                 and f.get("request", {}).get("subtype") == "can_use_tool"
                 and "touch x.tmp" in str(f["request"].get("input", {}).get("command", "")))
    results.append(check("claude: can_use_tool request for `touch x.tmp`", ask >= 0))
    if ask < 0:
        return results
    request_id = entries[ask]["frame"].get("request_id")
    reply = _index(entries, lambda d, f: d == "in" and f.get("type") == "control_response"
                   and f.get("response", {}).get("request_id") == request_id
                   and f["response"].get("response", {}).get("behavior") == "deny", ask)
    results.append(check("claude: deny control_response sent for that request", reply >= 0))
    final = _index(entries, lambda d, f: d == "out" and f.get("type") == "result"
                   and not f.get("is_error"), max(reply, ask))
    results.append(check("claude: turn continues to a successful result after the deny", final >= 0))
    return results


# ---------------------------------------------------------------- codex checks

def _codex_approval(entries, method):
    at = _index(entries, lambda d, f: d == "out" and f.get("method") == method)
    return at, (entries[at]["frame"] if at >= 0 else None)


def _codex_reply(entries, frame, after):
    return _index(entries, lambda d, f: d == "in" and f.get("id") == frame.get("id")
                  and "result" in f, after)


def _codex_item_done(entries, item_id, item_type, after):
    at = _index(entries, lambda d, f: d == "out" and f.get("method") == "item/completed"
                and f["params"]["item"].get("id") == item_id
                and f["params"]["item"].get("type") == item_type, after)
    return at, (entries[at]["frame"]["params"]["item"] if at >= 0 else None)


def _codex_turn_done(entries, after):
    at = _index(entries, lambda d, f: d == "out" and f.get("method") == "turn/completed", after)
    status = entries[at]["frame"]["params"]["turn"].get("status") if at >= 0 else None
    return at, status


def _codex_cwd_check(entries, sandbox):
    start = [f for f in _frames(entries, "out") if isinstance(f.get("result"), dict)
             and "thread" in f["result"]]
    got = None
    if start:
        envs = start[0]["result"]["thread"].get("environments") or [{}]
        got = envs[0].get("cwd")
    return check("codex: cwd is the sandbox", got is not None and _same_dir(got, sandbox),
                 f"reported {got!r}")


def codex_shell_checks(entries, sandbox=None):
    results = []
    if sandbox is not None:
        results.append(_codex_cwd_check(entries, sandbox))
    at, req = _codex_approval(entries, "item/commandExecution/requestApproval")
    results.append(check("codex: commandExecution requestApproval arrives", at >= 0))
    if at < 0:
        return results
    params = req["params"]
    command = str(params.get("command", ""))
    wrapped = (" -lc " in command or " -c " in command) and "'" in command
    results.append(check("codex: command is the wrapped shell string", wrapped, command))
    actions = params.get("commandActions")
    results.append(check("codex: commandActions is a non-empty list",
                         isinstance(actions, list) and len(actions) > 0
                         and all("type" in a for a in actions)))
    offered = params.get("availableDecisions") or []
    reply = _codex_reply(entries, req, at)
    sent = entries[reply]["frame"]["result"].get("decision") if reply >= 0 else None
    results.append(check("codex: decline was not offered (the premise of the probe)",
                         "decline" not in offered, f"offered {offered}"))
    results.append(check("codex: decline sent as the answer", sent == "decline", f"sent {sent!r}"))
    done_at, item = _codex_item_done(entries, params.get("itemId"), "commandExecution", at)
    results.append(check("codex: unoffered decline accepted, item ends declined",
                         item is not None and item.get("status") == "declined",
                         f"status {item.get('status') if item else None!r}"))
    turn_at, status = _codex_turn_done(entries, max(done_at, at))
    results.append(check("codex: turn continues to completion after the decline",
                         turn_at >= 0 and status == "completed", f"turn status {status!r}"))
    return results


def codex_edit_checks(entries, sandbox=None):
    results = []
    if sandbox is not None:
        results.append(_codex_cwd_check(entries, sandbox))
    at, req = _codex_approval(entries, "item/fileChange/requestApproval")
    results.append(check("codex: untrusted+workspaceWrite edit asks (fileChange requestApproval)",
                         at >= 0))
    if at < 0:
        return results
    reply = _codex_reply(entries, req, at)
    sent = entries[reply]["frame"]["result"].get("decision") if reply >= 0 else None
    results.append(check("codex: accept sent as the answer", sent == "accept", f"sent {sent!r}"))
    done_at, item = _codex_item_done(entries, req["params"].get("itemId"), "fileChange", at)
    results.append(check("codex: accept applies the edit (fileChange item completed)",
                         item is not None and item.get("status") == "completed",
                         f"status {item.get('status') if item else None!r}"))
    turn_at, status = _codex_turn_done(entries, max(done_at, at))
    results.append(check("codex: turn completes after the accept",
                         turn_at >= 0 and status == "completed", f"turn status {status!r}"))
    return results


# ---------------------------------------------------------------- agy checks

def agy_checks(entries, sandbox=None):
    events = [f for f in _frames(entries, "out") if "event" in f]
    results = []
    tools = [e["step_update"] for e in events
             if e.get("event") == "step_update" and e["step_update"].get("step_type") == "tool"
             and e["step_update"].get("state") == "ACTIVE"]
    results.append(check("agy: a tool step_update (ACTIVE) arrives", bool(tools)))
    named = [t for t in tools if isinstance(t.get("tool_name"), str) and t["tool_name"]]
    results.append(check("agy: tool step_update carries tool_name", bool(tools) and len(named) == len(tools),
                         ", ".join(sorted({t["tool_name"] for t in named}))))
    params = [t for t in tools if isinstance(t.get("tool_info", {}).get("parameters"), dict)
              and t["tool_info"]["parameters"]]
    results.append(check("agy: tool step_update carries tool_info.parameters",
                         bool(tools) and len(params) == len(tools),
                         ", ".join(sorted({k for t in params for k in t["tool_info"]["parameters"]}))))
    if sandbox is not None:
        init = [e for e in events if e.get("event") == "init"]
        got = init[0].get("init", {}).get("cwd") if init else None
        results.append(check("agy: init cwd is the sandbox",
                             got is not None and _same_dir(got, sandbox), f"reported {got!r}"))
    return results


CHECKS = {"claude-thinking-deny": claude_checks, "codex-shell-decline": codex_shell_checks,
          "codex-edit-accept": codex_edit_checks, "agy-view-file": agy_checks}


# ---------------------------------------------------------------- running

def run_agy(scenario):
    ns = scenario["ns"]
    cwd = assert_sandbox(ns.cwd)
    proc = subprocess.run(scenario["argv"], cwd=str(cwd), stdin=subprocess.DEVNULL,
                          capture_output=True, text=True, timeout=ns.timeout)
    with open(scenario["out"], "w") as log:
        for seq, line in enumerate((l for l in proc.stdout.splitlines() if l.strip()), 1):
            log.write(json.dumps({"seq": seq, "t": 0, "dir": "out", "frame": rec.store(line)}) + "\n")
    return proc.returncode


def run_scenario(scenario):
    """Run one scenario in the (asserted) sandbox. Returns the process-level result."""
    ns = scenario["ns"]
    ns.cwd = str(assert_sandbox(ns.cwd))
    ns.out = scenario["out"]
    if scenario["kind"] == "agy":
        return run_agy(scenario)
    decisions = rec.Decisions(scenario["decisions"])
    runner = rec.run_claude if scenario["kind"] == "claude" else rec.run_codex
    return runner(ns, decisions)


def disk_checks(name, sandbox):
    sandbox = Path(sandbox)
    if name == "claude-thinking-deny":
        return [check("claude: deny honoured, x.tmp was not created", not (sandbox / "x.tmp").exists())]
    if name == "codex-edit-accept":
        text = (sandbox / "fixture.txt").read_text()
        return [check("codex: accepted edit is on disk", "hello there" in text)]
    if name == "codex-shell-decline":
        return []
    return []


def report(results):
    failed = 0
    for name, ok, detail in results:
        print(f"{'PASS' if ok else 'FAIL'} {name}" + (f" ({detail})" if detail else ""))
        failed += 0 if ok else 1
    return failed


def parse_args(argv=None):
    p = argparse.ArgumentParser(
        description="Opt-in live contract smoke for claude, codex and agy. Each run spends "
                    "real turns: get the user's approval per run.")
    p.add_argument("--agent", required=True, choices=("claude", "codex", "agy"))
    p.add_argument("--dry-run", action="store_true",
                   help="print the exact commands and turn plan; run nothing")
    p.add_argument("--program", help="CLI binary (default: the agent name)")
    p.add_argument("--model", help="model passed to the CLI")
    p.add_argument("--timeout", type=int, default=180, help="seconds per turn (default 180)")
    p.add_argument("--keep", action="store_true",
                   help="keep the sandbox and the scrubbed frame files; print their path")
    return p.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)
    if args.dry_run:
        work = Path(tempfile.gettempdir()) / (PREFIX + "XXXX")
        plan = build_plan(args.agent, args.program, args.model, args.timeout,
                          work / "sandbox", work / "frames")
        print(render_plan(args.agent, plan, work / "sandbox"))
        return 0
    print(f"live_smoke: {args.agent}: this spends {TURNS[args.agent]} real model turn(s)",
          file=sys.stderr)
    work, sandbox, frames = make_workdir()
    try:
        sandbox = assert_sandbox(sandbox)
    except SandboxError as err:
        print(f"FAIL sandbox guard: {err}")
        return 2
    plan = build_plan(args.agent, args.program, args.model, args.timeout, sandbox, frames)
    results = []
    try:
        for scenario in plan:
            started = time.monotonic()
            try:
                code = run_scenario(scenario)
            except (SandboxError, OSError, subprocess.SubprocessError) as err:
                results.append(check(f"{scenario['name']}: run", False, type(err).__name__ + ": " + str(err)))
                continue
            results.append(check(f"{scenario['name']}: run finished",
                                 code == 0, f"exit {code}, {time.monotonic() - started:.0f}s"))
            if Path(scenario["out"]).exists():
                results += CHECKS[scenario["name"]](load_entries(scenario["out"]), sandbox)
                results += disk_checks(scenario["name"], sandbox)
    finally:
        if args.keep:
            print(f"kept: {work}", file=sys.stderr)
        else:
            shutil.rmtree(work, ignore_errors=True)
    return 1 if report(results) else 0


if __name__ == "__main__":
    sys.exit(main())
