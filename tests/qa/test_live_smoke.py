#!/usr/bin/env python3
"""Offline checks for live_smoke.py: arguments, dry-run, the sandbox guard, and the
assertion functions fed from the W0 fixtures. Spends no quota and spawns no agent."""

import contextlib
import copy
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))

import live_smoke as ls  # noqa: E402

FIXTURES = Path(__file__).resolve().parents[1] / "fixtures" / "real"
W0_SANDBOX = "/tmp/w0-sandbox"


def entries(agent, name):
    return ls.load_entries(FIXTURES / agent / name)


def failed(results):
    return [name for name, ok, _ in results if not ok]


def dry_run(*argv):
    out = io.StringIO()
    with patch.object(ls.subprocess, "run", side_effect=AssertionError("spawned")), \
            patch.object(ls.rec, "Session", side_effect=AssertionError("spawned")), \
            contextlib.redirect_stdout(out):
        code = ls.main(list(argv))
    return code, out.getvalue()


class ArgumentTests(unittest.TestCase):
    def test_agent_is_required_and_restricted(self):
        for argv in ([], ["--agent", "gemini"]):
            with self.assertRaises(SystemExit) as caught, contextlib.redirect_stderr(io.StringIO()):
                ls.parse_args(argv)
            self.assertEqual(caught.exception.code, 2)

    def test_defaults_and_flags(self):
        args = ls.parse_args(["--agent", "codex", "--dry-run", "--timeout", "30"])
        self.assertEqual((args.agent, args.dry_run, args.timeout), ("codex", True, 30))
        self.assertFalse(ls.parse_args(["--agent", "agy"]).dry_run)


class DryRunTests(unittest.TestCase):
    def test_claude_plan_has_exact_argv_and_two_turns(self):
        code, text = dry_run("--agent", "claude", "--dry-run")
        self.assertEqual(code, 0)
        self.assertIn("--thinking-display summarized", text)
        self.assertIn("--permission-prompt-tool stdio", text)
        self.assertIn("turn 2: Run: touch x.tmp", text)
        self.assertIn("2 real model turns", text)
        self.assertIn("approval answers, in order: deny", text)

    def test_codex_plan_covers_both_policies(self):
        code, text = dry_run("--agent", "codex", "--dry-run")
        self.assertEqual(code, 0)
        self.assertIn("codex app-server", text)
        self.assertIn("sandboxPolicy=readOnly", text)
        self.assertIn("sandboxPolicy=workspaceWrite", text)
        self.assertIn("approval answers, in order: decline", text)
        self.assertIn("approval answers, in order: accept", text)

    def test_agy_plan_is_one_turn_in_the_sandbox(self):
        code, text = dry_run("--agent", "agy", "--dry-run")
        self.assertEqual(code, 0)
        self.assertIn("-p", text)
        self.assertIn("--output-format stream-json", text)
        self.assertIn("1 real model turns", text)
        self.assertIn("live-smoke-", text)
        self.assertNotIn(str(Path.home()), text)

    def test_dry_run_creates_nothing(self):
        before = set(Path(tempfile.gettempdir()).glob("live-smoke-*"))
        dry_run("--agent", "claude", "--dry-run")
        self.assertEqual(before, set(Path(tempfile.gettempdir()).glob("live-smoke-*")))

    def test_turn_budget_is_at_most_four(self):
        for agent, turns in ls.TURNS.items():
            plan = ls.build_plan(agent, None, None, 10, "/tmp/live-smoke-x/sandbox", "/tmp/f")
            self.assertEqual(sum(len(s["prompts"]) for s in plan), turns)
            self.assertLessEqual(turns, 4)


class SandboxGuardTests(unittest.TestCase):
    def test_accepts_a_made_workdir(self):
        work, sandbox, _ = ls.make_workdir()
        try:
            self.assertEqual(ls.assert_sandbox(sandbox), sandbox.resolve())
            self.assertEqual((sandbox / "fixture.txt").read_text(), ls.FIXTURE_TEXT)
        finally:
            import shutil
            shutil.rmtree(work)

    def test_refuses_home_root_relative_and_empty(self):
        for bad in (str(Path.home()), "/", "relative/path", "", None, "/tmp"):
            with self.assertRaises(ls.SandboxError, msg=repr(bad)):
                ls.assert_sandbox(bad)

    def test_refuses_unmarked_or_misnamed_dir(self):
        with tempfile.TemporaryDirectory() as plain:
            with self.assertRaises(ls.SandboxError):
                ls.assert_sandbox(plain)
        with tempfile.TemporaryDirectory(prefix=ls.PREFIX) as work:
            sandbox = Path(work) / "sandbox"
            sandbox.mkdir()
            with self.assertRaises(ls.SandboxError):  # no marker
                ls.assert_sandbox(sandbox)

    def test_refuses_a_symlink_into_home(self):
        with tempfile.TemporaryDirectory(prefix=ls.PREFIX) as work:
            link = Path(work) / "sandbox"
            link.symlink_to(Path.home())
            with self.assertRaises(ls.SandboxError):
                ls.assert_sandbox(link)

    def test_run_scenario_never_spawns_outside_the_guard(self):
        scenario = ls.build_plan("agy", None, None, 5, str(Path.home()), "/tmp")[0]
        with patch.object(ls.subprocess, "run", side_effect=AssertionError("spawned")):
            with self.assertRaises(ls.SandboxError):
                ls.run_scenario(scenario)


class ClaudeCheckTests(unittest.TestCase):
    def setUp(self):
        self.entries = entries("claude", "thinking-and-touch.jsonl")

    def test_real_frames_pass(self):
        self.assertEqual(failed(ls.claude_checks(self.entries, W0_SANDBOX)), [])

    def test_wrong_cwd_fails(self):
        self.assertIn("claude: cwd is the sandbox", failed(ls.claude_checks(self.entries, "/tmp/elsewhere")))

    def test_missing_thinking_fails(self):
        stripped = [e for e in self.entries if "thinking" not in json.dumps(e["frame"])]
        self.assertIn("claude: non-empty thinking", failed(ls.claude_checks(stripped)))

    def test_missing_deny_reply_fails(self):
        no_reply = [e for e in self.entries
                    if not (e["dir"] == "in" and e["frame"].get("type") == "control_response")]
        names = failed(ls.claude_checks(no_reply))
        self.assertIn("claude: deny control_response sent for that request", names)

    def test_allow_reply_is_not_a_deny(self):
        flipped = copy.deepcopy(self.entries)
        for e in flipped:
            if e["dir"] == "in" and e["frame"].get("type") == "control_response":
                e["frame"]["response"]["response"]["behavior"] = "allow"
        self.assertIn("claude: deny control_response sent for that request", failed(ls.claude_checks(flipped)))

    def test_error_result_fails(self):
        broken = copy.deepcopy(self.entries)
        for e in broken:
            if e["dir"] == "out" and e["frame"].get("type") == "result":
                e["frame"]["is_error"] = True
        self.assertIn("claude: --thinking-display summarized starts without error",
                      failed(ls.claude_checks(broken)))


class CodexCheckTests(unittest.TestCase):
    def test_shell_decline_real_frames_pass(self):
        results = ls.codex_shell_checks(entries("codex", "ask-shell-decline.jsonl"), W0_SANDBOX)
        self.assertEqual(failed(results), [])

    def test_decline_must_be_unoffered(self):
        rows = copy.deepcopy(entries("codex", "ask-shell-decline.jsonl"))
        for e in rows:
            if e["frame"].get("method") == "item/commandExecution/requestApproval":
                e["frame"]["params"]["availableDecisions"].append("decline")
        self.assertIn("codex: decline was not offered (the premise of the probe)",
                      failed(ls.codex_shell_checks(rows)))

    def test_unwrapped_command_fails(self):
        rows = copy.deepcopy(entries("codex", "ask-shell-decline.jsonl"))
        for e in rows:
            if e["frame"].get("method") == "item/commandExecution/requestApproval":
                e["frame"]["params"]["command"] = "ls -la"
        self.assertIn("codex: command is the wrapped shell string", failed(ls.codex_shell_checks(rows)))

    def test_missing_command_actions_fails(self):
        rows = copy.deepcopy(entries("codex", "ask-shell-decline.jsonl"))
        for e in rows:
            if e["frame"].get("method") == "item/commandExecution/requestApproval":
                e["frame"]["params"]["commandActions"] = []
        self.assertIn("codex: commandActions is a non-empty list", failed(ls.codex_shell_checks(rows)))

    def test_item_not_declined_fails(self):
        rows = copy.deepcopy(entries("codex", "ask-shell-decline.jsonl"))
        for e in rows:
            f = e["frame"]
            if f.get("method") == "item/completed" and f["params"]["item"].get("type") == "commandExecution":
                f["params"]["item"]["status"] = "failed"
        self.assertIn("codex: unoffered decline accepted, item ends declined",
                      failed(ls.codex_shell_checks(rows)))

    def test_edit_accept_real_frames_pass(self):
        results = ls.codex_edit_checks(entries("codex", "untrusted-workspacewrite.jsonl"), W0_SANDBOX)
        self.assertEqual(failed(results), [])

    def test_declined_edit_is_not_an_accept(self):
        # The decline fixture is the negative case for the accept check.
        names = failed(ls.codex_edit_checks(entries("codex", "ask-edit-decline.jsonl")))
        self.assertIn("codex: accept sent as the answer", names)
        self.assertIn("codex: accept applies the edit (fileChange item completed)", names)

    def test_wrong_cwd_fails(self):
        rows = entries("codex", "untrusted-workspacewrite.jsonl")
        self.assertIn("codex: cwd is the sandbox", failed(ls.codex_edit_checks(rows, "/tmp/elsewhere")))

    def test_no_approval_fails_cleanly(self):
        results = ls.codex_shell_checks([])
        self.assertEqual(failed(results), ["codex: commandExecution requestApproval arrives"])


class AgyCheckTests(unittest.TestCase):
    def setUp(self):
        self.rows = entries("agy", "read-outside-project.jsonl")

    def test_tool_events_carry_name_and_parameters(self):
        self.assertEqual(failed(ls.agy_checks(self.rows)), [])

    def test_the_w0_cwd_slip_is_caught(self):
        # W0's fixture recorded cwd "~" (the home directory): the sandbox check must flag it.
        self.assertIn("agy: init cwd is the sandbox", failed(ls.agy_checks(self.rows, W0_SANDBOX)))

    def test_cwd_in_the_sandbox_passes(self):
        rows = copy.deepcopy(self.rows)
        for e in rows:
            if e["frame"].get("event") == "init":
                e["frame"]["init"]["cwd"] = W0_SANDBOX
        self.assertEqual(failed(ls.agy_checks(rows, W0_SANDBOX)), [])

    def test_missing_parameters_fails(self):
        rows = copy.deepcopy(self.rows)
        for e in rows:
            update = e["frame"].get("step_update")
            if update and update.get("state") == "ACTIVE":
                del update["tool_info"]["parameters"]
        self.assertIn("agy: tool step_update carries tool_info.parameters", failed(ls.agy_checks(rows)))

    def test_missing_tool_name_fails(self):
        rows = copy.deepcopy(self.rows)
        for e in rows:
            update = e["frame"].get("step_update")
            if update and update.get("state") == "ACTIVE":
                del update["tool_name"]
        self.assertIn("agy: tool step_update carries tool_name", failed(ls.agy_checks(rows)))


class ReportTests(unittest.TestCase):
    def test_report_prints_one_line_per_check_and_counts_failures(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            count = ls.report([("a", True, ""), ("b", False, "why")])
        self.assertEqual(count, 1)
        self.assertEqual(out.getvalue().splitlines(), ["PASS a", "FAIL b (why)"])


if __name__ == "__main__":
    unittest.main()
