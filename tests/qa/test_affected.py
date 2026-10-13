#!/usr/bin/env python3
"""Regression checks that change selection never drops a check it cannot rule out."""

import json
import unittest

import affected

NATIVE = json.loads((affected.ROOT / "tests/qa/native-tests.json").read_text())
RULES = json.loads((affected.ROOT / "tests/qa/native-areas.json").read_text())["rules"]


def select(*paths):
    return affected.select(list(paths), NATIVE, RULES)


class AffectedTests(unittest.TestCase):
    def test_docs_and_workflows_select_nothing(self):
        self.assertEqual(select("README.md", "docs/releases/v3.5.0.md", ".github/workflows/ci.yml"), ([], [], False))

    def test_a_core_change_selects_core_and_through_rdeps_what_uses_it(self):
        packages, cases, _ = select("crates/agent-core/src/redact.rs")
        self.assertEqual(packages, ["agent-core"])
        self.assertEqual(affected.filterset(packages), "rdeps(agent-core)")
        self.assertEqual(cases, [case for case in NATIVE if case.startswith("chat::view::")])
        self.assertTrue(cases)

    def test_a_kit_change_skips_the_native_cases(self):
        self.assertEqual(select("crates/agent-kit/src/store.rs"), (["agent-kit"], [], False))

    def test_changes_in_two_crates_join_their_filters(self):
        packages, _, _ = select("crates/agent-kit/src/git.rs", "crates/agent-core/src/event.rs")
        self.assertEqual(affected.filterset(packages), "rdeps(agent-core) | rdeps(agent-kit)")

    def test_an_app_change_tests_only_the_app_and_its_area(self):
        packages, cases, _ = select("src/window/imp/closing.rs")
        self.assertEqual(packages, ["agent-terminal"])
        self.assertEqual(cases, ["window::imp::closing::tests::native_closing_waits_for_agents_then_closes"])

    def test_an_unknown_or_shared_path_selects_everything(self):
        for path in ("Cargo.lock", "rust-toolchain.toml", "src/palette.rs", "src/main.rs"):
            packages, cases, _ = select(path)
            self.assertEqual(cases, NATIVE, path)
            self.assertIn("agent-terminal", packages, path)
        self.assertEqual(select("Cargo.toml")[0], ["agent-core", "agent-kit", "agent-terminal"])

    def test_qa_script_changes_run_the_qa_unit_tests_only(self):
        self.assertEqual(select("tests/qa/gate.py"), ([], [], True))

    def test_every_rule_prefix_names_a_real_case(self):
        # A renamed module would otherwise leave its rule selecting nothing, silently.
        for rule in RULES:
            for prefix in rule["tests"]:
                self.assertTrue(any(case.startswith(prefix) for case in NATIVE), prefix)


if __name__ == "__main__":
    unittest.main()
