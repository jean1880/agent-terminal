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
