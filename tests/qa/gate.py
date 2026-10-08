#!/usr/bin/env python3
"""Shared source gate. No live app, provider, credentials or user settings."""

import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time
import tomllib

ROOT = Path(__file__).resolve().parents[2]


def isolated_env(home):
    """Keep build tools, but do not pass agent tokens or the user's GTK session."""
    env = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "LANG": "C.UTF-8",
        "HOME": str(home),
        "CARGO_TERM_COLOR": "never",
        "CARGO_HOME": os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")),
        "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup")),
        "GSETTINGS_BACKEND": "memory",
        "GTK_A11Y": "test",
        "GDK_BACKEND": "x11",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": "/dev/null",
    }
    if "CARGO_TARGET_DIR" in os.environ:
        env["CARGO_TARGET_DIR"] = os.environ["CARGO_TARGET_DIR"]
    for key, folder in [
        ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache"),
        ("XDG_RUNTIME_DIR", "runtime"), ("TMPDIR", "tmp"),
    ]:
        path = home / folder
        path.mkdir(mode=0o700)
        env[key] = str(path)
    return env


def exact_native_result(output, test):
    """A renamed/missing test must fail even when libtest exits successfully."""
    return (
        re.findall(r"^running (\d+) tests?$", output, re.MULTILINE) == ["1"]
        # --nocapture can put diagnostic output between the test name and "ok".
        and re.findall(r"^test (\S+) \.\.\.", output, re.MULTILINE) == [test]
        and re.findall(
            r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;",
            output, re.MULTILINE,
        ) == [("1", "0", "0")]
    )


def run_logged(command, log, env, timeout, cwd=ROOT):
    """Bound the complete process group, including fixture/tool descendants."""
    started = time.monotonic()
    with log.open("wb") as stream:
        timed_out = False
        try:
            process = subprocess.Popen(
                command, cwd=cwd, env=env, stdout=stream, stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        except OSError as error:
            stream.write(f"Could not start test command: {error}\n".encode())
            code = 127
        else:
            try:
                code = process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                timed_out = True
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    # It exited between wait's deadline and the signal.
                    pass
                code = process.wait()
    return {
        "command": command, "exit_code": code, "timed_out": timed_out,
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "log": log.name,
    }


def commit():
    return subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True, timeout=10,
    ).strip()


def dirty():
    return bool(subprocess.check_output(
        ["git", "status", "--porcelain", "--untracked-files=normal"],
        cwd=ROOT, text=True, timeout=10,
    ).strip())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native-only", action="store_true", help="developer check; not a delivery gate")
    parser.add_argument("--native-test", action="append", default=[], help="exact named developer case; requires --native-only")
    parser.add_argument("--delivery", action="store_true", help="require clean source matching the tested commit")
    parser.add_argument("--artifacts", type=Path, default=ROOT / "target/qa-artifacts")
    parser.add_argument("--native-timeout", type=int, default=240)
    args = parser.parse_args()
    if args.native_timeout < 1:
        parser.error("--native-timeout must be positive")
    if args.delivery and args.native_only:
        parser.error("--native-only cannot satisfy --delivery")
    if args.native_test and not args.native_only:
        parser.error("--native-test requires --native-only and cannot satisfy delivery")
    artifacts = args.artifacts.resolve()
    artifacts.mkdir(parents=True, exist_ok=True)
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    identity = {"commit": commit(), "version": version, "working_tree_dirty": dirty(), "delivery": args.delivery, "native_only": args.native_only, "steps": []}
    summary = artifacts / "gate.json"

    def save():
        summary.write_text(json.dumps(identity, indent=2) + "\n")

    save()
    try:
        if args.delivery and identity["working_tree_dirty"]:
            raise RuntimeError("delivery requires a clean checkout of the exact tested commit")
        native = json.loads((ROOT / "tests/qa/native-tests.json").read_text())
        if not isinstance(native, list) or not native or any(
            not isinstance(test, str) or not re.fullmatch(r"[a-zA-Z0-9_:]+", test)
            for test in native
        ) or len(set(native)) != len(native):
            raise ValueError("native-tests.json must contain unique, exact test names")
        if args.native_test:
            if any(test not in native for test in args.native_test):
                raise ValueError("--native-test must name an exact case from native-tests.json")
            native = [test for test in native if test in args.native_test]
        identity["selected_native_tests"] = native
        save()
        steps = []
        if not args.native_only:
            steps.extend([
                ("format", ["cargo", "fmt", "--all", "--", "--check"], 120, None),
                ("clippy", ["cargo", "clippy", "--locked", "--workspace", "--all-targets", "--", "-D", "warnings"], 1200, None),
                ("workspace", ["xvfb-run", "-a", "dbus-run-session", "--", "cargo", "test", "--locked", "--workspace", "--all-targets"], 1200, None),
            ])
        for index, test in enumerate(native):
            steps.append((
                f"native-{index + 1}",
                ["xvfb-run", "-a", "dbus-run-session", "--", "cargo", "test", "--locked", "--offline", "-p", "agent-terminal", "--bin", "agent-terminal", test, "--", "--exact", "--ignored", "--nocapture", "--test-threads=1"],
                args.native_timeout, test,
            ))
        for label, command, timeout, test in steps:
            # Every GTK case gets a new process, D-Bus session, display and home.
            with tempfile.TemporaryDirectory(prefix="agent-terminal-qa-") as directory:
                env = isolated_env(Path(directory))
                if test:
                    screenshots = artifacts / "screenshots" / label
                    screenshots.mkdir(parents=True, exist_ok=True)
                    env["AGENT_TERMINAL_QA_ARTIFACTS"] = str(screenshots)
                result = run_logged(command, artifacts / f"{label}.log", env, timeout)
            result["name"] = test or label
            if test:
                result["exact_test_passed"] = exact_native_result(
                    (artifacts / result["log"]).read_text(errors="replace"), test,
                )
            identity["steps"].append(result)
            save()
            print(f"{label}: exit={result['exit_code']} timeout={result['timed_out']} log={result['log']}", flush=True)
            if result["exit_code"] or result["timed_out"] or (test and not result["exact_test_passed"]):
                raise RuntimeError(f"{test or label} failed; see {artifacts / result['log']}")
        if commit() != identity["commit"]:
            raise RuntimeError("checkout commit changed while the gate was running")
        if args.delivery and dirty():
            raise RuntimeError("delivery checkout changed while the gate was running")
        identity["passed"] = True
        save()
        suffix = " with working-tree edits" if identity["working_tree_dirty"] else ""
        print(f"Gate passed: {identity['commit']} (v{version}){suffix}", flush=True)
        return 0
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        identity["passed"] = False
        identity["error"] = str(error)
        save()
        print(f"Gate failed: {error}", flush=True)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
