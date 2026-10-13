#!/usr/bin/env python3
"""Regression checks for fail-closed test selection and bounded isolation."""

import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

import gate


class GateTests(unittest.TestCase):
    def test_exact_selection_rejects_zero_ignored_and_multiple_tests(self):
        name = "chat::view::tests::native"
        good = f"running 1 test\ntest {name} ... synthetic geometry\nok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 42 filtered out\n"
        self.assertTrue(gate.exact_native_result(good, name))
        self.assertFalse(gate.exact_native_result(good, name + "_renamed"))
        self.assertFalse(gate.exact_native_result("running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored;", name))
        self.assertFalse(gate.exact_native_result(good.replace("1 passed; 0 failed; 0 ignored;", "0 passed; 0 failed; 1 ignored;"), name))
        self.assertFalse(gate.exact_native_result(good + good, name))

    def test_home_and_settings_are_private_and_credentials_do_not_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            with patch.dict(os.environ, {"OPENAI_API_KEY": "synthetic-value", "DBUS_SESSION_BUS_ADDRESS": "synthetic-value"}):
                env = gate.isolated_env(home)
            self.assertEqual(env["HOME"], directory)
            self.assertNotIn("OPENAI_API_KEY", env)
            self.assertNotIn("DBUS_SESSION_BUS_ADDRESS", env)
            for name in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_RUNTIME_DIR"):
                self.assertTrue(Path(env[name]).is_relative_to(home))
                self.assertEqual(Path(env[name]).stat().st_mode & 0o777, 0o700)

    def test_timeout_is_failure_and_captures_exit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = gate.run_logged(
                [sys.executable, "-c", "import time; print('synthetic waiting', flush=True); time.sleep(30)"],
                root / "timeout.log", gate.isolated_env(root), 0.2,
            )
            self.assertTrue(result["timed_out"])
            self.assertNotEqual(result["exit_code"], 0)
            self.assertIn("synthetic waiting", (root / "timeout.log").read_text())

    def test_native_names_are_explicit_not_measurement_tests(self):
        names = json.loads((gate.ROOT / "tests/qa/native-tests.json").read_text())
        self.assertGreaterEqual(len(names), 4)
        self.assertEqual(len(names), len(set(names)))
        self.assertTrue(all("stream_cost" not in name for name in names))

    def test_an_ignored_test_outside_both_lists_is_reported_and_so_is_a_stale_exclusion(self):
        listing = "a::tests::native_one: test\na::tests::bench: test\nb::tests::forgotten: test\n\n3 tests, 0 benchmarks\n"
        orphans, stale = gate.unaccounted_ignored(listing, ["a::tests::native_one"], {"a::tests::bench": "why", "c::gone": "why"})
        self.assertEqual(orphans, ["b::tests::forgotten"])
        self.assertEqual(stale, ["c::gone"])
        self.assertEqual(gate.unaccounted_ignored(listing.replace("b::tests::forgotten: test\n", ""), ["a::tests::native_one"], {"a::tests::bench": "why"}), ([], []))

    def test_every_exclusion_gives_a_reason_and_none_is_also_native(self):
        excluded = json.loads((gate.ROOT / "tests/qa/native-excluded.json").read_text())
        native = json.loads((gate.ROOT / "tests/qa/native-tests.json").read_text())
        self.assertTrue(all(isinstance(reason, str) and reason.strip() for reason in excluded.values()))
        self.assertFalse(set(excluded) & set(native))

    def test_the_test_binary_is_the_one_app_bin_artifact(self):
        def artifact(name, kind, test, executable):
            return json.dumps({"reason": "compiler-artifact", "target": {"name": name, "kind": kind}, "profile": {"test": test}, "executable": executable})
        log = "\n".join([
            "   Compiling agent-terminal v3.5.0",
            artifact("agent-core", ["lib"], True, "/t/agent_core-1"),
            artifact("agent-terminal", ["bin"], False, "/t/agent-terminal"),
            artifact("agent-terminal", ["bin"], True, "/t/agent_terminal-2"),
            '{"reason": "build-finished", "success": true}',
        ])
        self.assertEqual(gate.test_binary(log), "/t/agent_terminal-2")
        with self.assertRaises(RuntimeError):
            gate.test_binary("no json here")
        with self.assertRaises(RuntimeError):
            gate.test_binary(log + "\n" + artifact("agent-terminal", ["bin"], True, "/t/other"))

    def test_parallel_cases_start_their_display_search_apart(self):
        commands = [gate.native_command("/t/bin", "x::y", index) for index in range(25)]
        starts = [int(command[command.index("-n") + 1]) for command in commands]
        self.assertEqual(len(set(starts)), len(starts))
        self.assertTrue(all(b - a >= 10 for a, b in zip(starts, starts[1:])))
        self.assertEqual(commands[0][-5:], ["x::y", "--exact", "--ignored", "--nocapture", "--test-threads=1"])

    def test_missing_executable_fails_and_keeps_a_diagnostic(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = gate.run_logged([str(root / "missing")], root / "spawn.log", gate.isolated_env(root), 1)
            self.assertEqual(result["exit_code"], 127)
            self.assertFalse(result["timed_out"])
            self.assertIn("Could not start", (root / "spawn.log").read_text())

    def test_delivery_refuses_working_tree_edits_before_running_commands(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(sys, "argv", ["gate.py", "--delivery", "--artifacts", directory]), patch.object(gate, "dirty", return_value=True), patch.object(gate, "run_logged") as runner:
                self.assertEqual(gate.main(), 1)
            runner.assert_not_called()
            result = json.loads((Path(directory) / "gate.json").read_text())
            self.assertFalse(result["passed"])
            self.assertTrue(result["working_tree_dirty"])


if __name__ == "__main__":
    unittest.main()
