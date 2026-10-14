#!/usr/bin/env python3
"""Regression checks for the probes' private environment and bounded runs."""

import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

import qa_env


class QaEnvTests(unittest.TestCase):
    def test_home_and_settings_are_private_and_credentials_do_not_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            with patch.dict(os.environ, {"OPENAI_API_KEY": "synthetic-value", "DBUS_SESSION_BUS_ADDRESS": "synthetic-value"}):
                env = qa_env.isolated_env(home)
            self.assertEqual(env["HOME"], directory)
            self.assertNotIn("OPENAI_API_KEY", env)
            self.assertNotIn("DBUS_SESSION_BUS_ADDRESS", env)
            for name in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_RUNTIME_DIR"):
                self.assertTrue(Path(env[name]).is_relative_to(home))
                self.assertEqual(Path(env[name]).stat().st_mode & 0o777, 0o700)

    def test_timeout_is_failure_and_captures_exit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = qa_env.run_logged(
                [sys.executable, "-c", "import time; print('synthetic waiting', flush=True); time.sleep(30)"],
                root / "timeout.log", qa_env.isolated_env(root), 0.2,
            )
            self.assertTrue(result["timed_out"])
            self.assertNotEqual(result["exit_code"], 0)
            self.assertIn("synthetic waiting", (root / "timeout.log").read_text())

    def test_missing_executable_fails_and_keeps_a_diagnostic(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = qa_env.run_logged([str(root / "missing")], root / "spawn.log", qa_env.isolated_env(root), 1)
            self.assertEqual(result["exit_code"], 127)
            self.assertFalse(result["timed_out"])
            self.assertIn("Could not start", (root / "spawn.log").read_text())


if __name__ == "__main__":
    unittest.main()
