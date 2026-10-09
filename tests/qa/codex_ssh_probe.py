#!/usr/bin/env python3
"""Opt-in Linux Codex sandbox compatibility check; no agent turn or network.

The direct parse represents executing the same approved command outside the
Codex sandbox. This script never changes permissions or reads real SSH settings.
"""

import argparse
import json
from pathlib import Path
import shutil
import sys
import tempfile

import gate


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", default="codex")
    args = parser.parse_args()
    artifacts = gate.ROOT / "target/qa-codex-ssh"
    artifacts.mkdir(parents=True, exist_ok=True)
    codex = shutil.which(args.codex)
    if not codex:
        parser.error("Codex CLI is required; unavailable is not a compatibility pass")
    # Current Codex refuses helper binaries under /tmp; use disposable repo build output.
    with tempfile.TemporaryDirectory(prefix="probe-", dir=artifacts) as temporary:
        home = Path(temporary)
        env = gate.isolated_env(home)
        env.pop("CARGO_HOME")
        env.pop("RUSTUP_HOME")
        codex_home = home / "codex"
        codex_home.mkdir(mode=0o700)
        env["CODEX_HOME"] = str(codex_home)
        config = home / "synthetic-ssh-config"
        config.write_text("Host fixture\n  HostName fixture.invalid\n  User qa\n  CanonicalizeHostname no\n")
        config.chmod(0o600)
        command = [sys.executable, str(gate.ROOT / "tests/qa/ssh_config_probe.py"), str(config)]
        checks = [
            ("version", [codex, "--version"]),
            ("outside", command),
            ("inside", [codex, "sandbox", "linux", "--", *command]),
        ]
        summary = {"commit": gate.commit(), "steps": [], "compatible": False}
        try:
            for label, invocation in checks:
                result = gate.run_logged(invocation, artifacts / f"{label}.log", env, 30, cwd=home)
                result["name"] = label
                summary["steps"].append(result)
                if result["exit_code"] or result["timed_out"]:
                    raise ValueError(f"{label} failed; see {artifacts / result['log']}")
            # Codex can write its own diagnostics before the command output.
            records = []
            for label in ("outside", "inside"):
                lines = (artifacts / f"{label}.log").read_text().splitlines()
                payloads = [json.loads(line) for line in lines if line.startswith('{"uid":')]
                if len(payloads) != 1:
                    raise ValueError(f"{label} did not report exactly one SSH ownership/parse record")
                records.append(payloads[0])
            outside, inside = records
            expected = {"hostname": "fixture.invalid", "user": "qa", "canonicalizehostname": "false"}
            for record in records:
                if record["owner"] != record["uid"] or record["mode"] != 0o600 or record["exit_code"] or record["fields"] != expected:
                    raise ValueError("SSH ownership/parse differs from the synthetic fixture contract")
            if inside != outside:
                raise ValueError("Codex sandbox changed SSH ownership or configuration parsing")
            summary["compatible"] = True
            print("Codex SSH compatibility passed; synthetic parse only, no network or model")
        except (OSError, ValueError) as error:
            summary["error"] = str(error)
            print(f"Codex SSH compatibility failed: {error}")
        finally:
            (artifacts / "probe.json").write_text(json.dumps(summary, indent=2) + "\n")
    return 0 if summary["compatible"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
