#!/usr/bin/env python3
"""Coverage of production code: workspace tests plus every native case, instrumented.

Needs cargo-llvm-cov (`cargo install --locked cargo-llvm-cov`, `rustup component add
llvm-tools-preview`) and the gate's native tools. Test code is not counted: lines inside
`#[cfg(test)]` items, and files only declared under `#[cfg(test)]`, are dropped before the
totals are taken, so a test cannot raise the figure by covering itself.
"""

import argparse
import json
from pathlib import Path
import re
import sys
import tempfile

from gate import ROOT, isolated_env, run_logged

FLOOR_FILE = ROOT / "tests/qa/coverage-floor.json"


def strip_code(text):
    """`text` with comments, strings and char literals blanked, newlines kept, so braces
    counted in the result are only the code's."""
    out, i, n = [], 0, len(text)
    while i < n:
        c = text[i]
        if text.startswith("//", i):
            j = text.find("\n", i)
            i = n if j < 0 else j
        elif text.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth:
                if text.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif text.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    out.append("\n" if text[i] == "\n" else " ")
                    i += 1
        elif (m := re.match(r'b?r(#*)"', text[i:])) and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            end = '"' + m.group(1)
            j = text.find(end, i + m.end())
            j = n if j < 0 else j + len(end)
            out.append("\n" * text.count("\n", i, j))
            i = j
        elif c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            out.append("\n" * text.count("\n", i, j + 1))
            i = j + 1
        elif c == "'" and (m := re.match(r"'(\\.[^']*|[^\\'])'", text[i:])):
            i += m.end()
        else:
            out.append(c)
            i += 1
    return "".join(out)


def test_only(path):
    """Line numbers (1-based) of `path` inside `#[cfg(test)]` items, and the module files
    those items declare (`#[cfg(test)] mod name;`)."""
    code = strip_code(path.read_text(errors="replace"))
    lines, files = set(), []
    for m in re.finditer(r"#\[cfg\(test\)\]", code):
        rest = code[m.end():]
        decl = re.match(r"(?:\s*#\[[^\]]*\])*\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;", rest)
        if decl:
            name = decl.group(1)
            base = path.parent if path.name in ("main.rs", "lib.rs", "mod.rs") else path.with_suffix("")
            files += [base / f"{name}.rs", base / name / "mod.rs"]
            continue
        semi, brace = rest.find(";"), rest.find("{")
        if brace < 0 or (0 <= semi < brace):
            end = m.end() + max(semi, 0)
        else:
            depth, j = 0, brace
            while j < len(rest):
                depth += {"{": 1, "}": -1}.get(rest[j], 0)
                if depth == 0:
                    break
                j += 1
            end = m.end() + j
        first = code.count("\n", 0, m.start()) + 1
        last = code.count("\n", 0, end) + 1
        lines.update(range(first, last + 1))
    return lines, files


def production_lines(lcov_text):
    """{relative path: (covered, total)} counting only production lines."""
    records, current = {}, None
    for line in lcov_text.splitlines():
        if line.startswith("SF:"):
            current = Path(line[3:])
            records[current] = {}
        elif line.startswith("DA:") and current is not None:
            number, count = line[3:].split(",")[:2]
            records[current][int(number)] = int(count)
    test_files, test_lines = set(), {}
    for path in records:
        if path.exists():
            test_lines[path], declared = test_only(path)
            test_files.update(p.resolve() for p in declared)
    result = {}
    for path, hits in records.items():
        if path.resolve() in test_files:
            continue
        kept = {n: c for n, c in hits.items() if n not in test_lines.get(path, ())}
        if kept:
            result[str(path.relative_to(ROOT))] = (sum(1 for c in kept.values() if c), len(kept))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--artifacts", type=Path, default=ROOT / "target/qa-artifacts/coverage")
    parser.add_argument("--skip-native", action="store_true", help="workspace tests only (faster; reads lower)")
    parser.add_argument("--report-only", action="store_true", help="re-read the last run's profiles")
    parser.add_argument("--native-timeout", type=int, default=600)
    parser.add_argument("--html", action="store_true", help="also write an HTML report")
    parser.add_argument("--update-floor", action="store_true", help="raise the floor to this run's figure")
    args = parser.parse_args()
    artifacts = args.artifacts.resolve()
    artifacts.mkdir(parents=True, exist_ok=True)
    native = json.loads((ROOT / "tests/qa/native-tests.json").read_text())

    steps = []
    if not args.report_only:
        steps.append(("clean", ["cargo", "llvm-cov", "clean", "--workspace"], 300))
        steps.append(("workspace", ["xvfb-run", "-a", "dbus-run-session", "--", "cargo", "llvm-cov", "nextest", "--no-report", "--locked", "--workspace", "--all-targets", "--profile", "ci"], 2400))
        if not args.skip_native:
            for index, test in enumerate(native):
                steps.append((f"native-{index + 1}", ["xvfb-run", "-a", "dbus-run-session", "--", "cargo", "llvm-cov", "--no-report", "--locked", "-p", "agent-terminal", "--bin", "agent-terminal", "--", test, "--exact", "--ignored", "--test-threads=1"], args.native_timeout))
    lcov = artifacts / "lcov.info"
    steps.append(("report", ["cargo", "llvm-cov", "report", "--workspace", "--lcov", "--output-path", str(lcov)], 600))
    if args.html:
        steps.append(("html", ["cargo", "llvm-cov", "report", "--workspace", "--html", "--output-dir", str(artifacts)], 600))

    for label, command, timeout in steps:
        with tempfile.TemporaryDirectory(prefix="agent-terminal-cov-") as directory:
            result = run_logged(command, artifacts / f"{label}.log", isolated_env(Path(directory)), timeout)
        print(f"{label}: exit={result['exit_code']} timeout={result['timed_out']} {result['elapsed_seconds']}s", flush=True)
        if result["exit_code"] or result["timed_out"]:
            print(f"Coverage failed at {label}; see {artifacts / result['log']}", flush=True)
            return 1

    files = production_lines(lcov.read_text())
    covered = sum(c for c, _ in files.values())
    total = sum(t for _, t in files.values())
    percent = round(100 * covered / total, 2) if total else 0.0
    crates = {}
    for name, (c, t) in files.items():
        key = "/".join(name.split("/")[:2]) if name.startswith("crates/") else "app"
        crates.setdefault(key, [0, 0])
        crates[key][0] += c
        crates[key][1] += t
    summary = {
        "production_lines": {"covered": covered, "total": total, "percent": percent},
        "native_included": not args.skip_native,
        "crates": {k: round(100 * c / t, 2) for k, (c, t) in sorted(crates.items())},
        "files": {k: {"covered": c, "total": t, "percent": round(100 * c / t, 2)}
                  for k, (c, t) in sorted(files.items(), key=lambda kv: kv[1][0] - kv[1][1])},
    }
    (artifacts / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    for name, row in list(summary["files"].items())[:15]:
        print(f"  {row['total'] - row['covered']:>5} uncovered  {row['percent']:>6.2f}%  {name}")
    for name, value in summary["crates"].items():
        print(f"  {name}: {value}%")
    print(f"Production line coverage: {percent}% ({covered}/{total})", flush=True)

    floor = json.loads(FLOOR_FILE.read_text())
    if args.skip_native:
        print("Workspace-only run: the floor is for the full run and is not checked.")
        return 0
    if args.update_floor and percent > floor["lines"]:
        floor["lines"] = percent
        FLOOR_FILE.write_text(json.dumps(floor, indent=2) + "\n")
        print(f"Floor raised to {percent}%")
    if percent < floor["lines"]:
        print(f"Coverage {percent}% is under the floor {floor['lines']}% (target {floor['target']}%)")
        return 1
    print(f"Floor {floor['lines']}% held; target {floor['target']}%")
    return 0


if __name__ == "__main__":
    sys.exit(main())
