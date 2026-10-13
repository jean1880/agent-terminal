#!/usr/bin/env python3
"""Run only the checks a change needs. A developer shortcut: it never satisfies delivery.

Changed paths (against the merge base with --base, plus uncommitted edits) select:
the packages they belong to and, through nextest's rdeps(), every package that uses them,
native cases by tests/qa/native-areas.json, and the QA scripts' own unit tests. Anything
unrecognised selects everything, so a miss costs time, never coverage. CI still runs the
whole gate.
"""

import argparse
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
APP = "agent-terminal"
# The package each path belongs to; nextest's rdeps() adds the packages that use it.
PACKAGES = {
    "crates/agent-core/": "agent-core",
    "crates/agent-kit/": "agent-kit",
    "src/": APP,
    "tests/icons.rs": APP,
    "tests/fixtures/": APP,
    "assets/": APP,
    "build.rs": APP,
}
EVERYTHING = ["agent-core", "agent-kit", APP]
# Paths no Rust test reads.
INERT = (".md", ".txt", "LICENSE", ".github/", "docs/", "packaging/", "tests/qa/", "tests/support/", ".gitignore")


def changed(base):
    files = subprocess.check_output(
        ["git", "diff", "--name-only", f"{base}...HEAD"], cwd=ROOT, text=True, timeout=30,
    ).split()
    status = subprocess.check_output(
        ["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT, text=True, timeout=30,
    )
    files += [line[3:].split(" -> ")[-1] for line in status.splitlines() if line]
    return sorted(set(files))


def select(paths, native, rules):
    """(changed packages, native cases, run the QA unit tests) for `paths`."""
    packages, cases, qa = set(), set(), False
    for path in paths:
        if path.startswith("tests/qa/"):
            qa = True
        if path.endswith(INERT) or path.startswith(INERT):
            continue
        owner = next((pkg for prefix, pkg in PACKAGES.items() if path.startswith(prefix)), None)
        packages.update(EVERYTHING if owner is None else [owner])
        rule = next((r for r in rules if any(path.startswith(p) for p in r["paths"])), None)
        prefixes = [""] if rule is None else rule["tests"]
        cases.update(t for t in native if any(t.startswith(p) for p in prefixes))
    return sorted(packages, key=EVERYTHING.index), [t for t in native if t in cases], qa


def filterset(packages):
    """The nextest filter for `packages` and everything that depends on them."""
    return " | ".join(f"rdeps({name})" for name in packages)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base", default="origin/master", help="compare against the merge base with this ref")
    parser.add_argument("--dry-run", action="store_true", help="print the plan and run nothing")
    parser.add_argument("--jobs", type=int, default=2, help="native cases run side by side")
    parser.add_argument("paths", nargs="*", help="use these paths instead of git's")
    args = parser.parse_args()
    native = json.loads((ROOT / "tests/qa/native-tests.json").read_text())
    rules = json.loads((ROOT / "tests/qa/native-areas.json").read_text())["rules"]
    paths = args.paths or changed(args.base)
    packages, cases, qa = select(paths, native, rules)

    commands = []
    if qa:
        for script in sorted((ROOT / "tests/qa").glob("test_*.py")):
            commands.append([sys.executable, str(script)])
    if packages:
        commands.append(["xvfb-run", "-a", "dbus-run-session", "--", "cargo", "nextest", "run", "--locked",
                         "--workspace", "--all-targets", "-E", filterset(packages)])
    if cases:
        commands.append([sys.executable, str(ROOT / "tests/qa/gate.py"), "--native-only", "--jobs", str(args.jobs),
                         *[arg for case in cases for arg in ("--native-test", case)]])
    print(f"{len(paths)} changed path(s); packages: {packages or 'none'}; native cases: {len(cases)}/{len(native)}; QA scripts: {qa}")
    for command in commands:
        print("  " + " ".join(f"'{arg}'" if " " in arg else arg for arg in command))
    if args.dry_run or not commands:
        return 0
    for command in commands:
        if subprocess.run(command, cwd=ROOT).returncode:
            return 1
    print("Affected checks passed (not a delivery gate: CI runs everything).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
