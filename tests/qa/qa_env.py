"""Shared helpers for the package and Codex probes: a private environment and a bounded run.

The test suite itself runs through the Makefile (`make check`); this is only for the probes
that start a built or packaged binary.
"""

import os
from pathlib import Path
import signal
import subprocess
import time

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
