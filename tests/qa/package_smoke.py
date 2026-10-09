#!/usr/bin/env python3
"""Verify a trusted package's identity and prompt-less executable startup.

This does not claim packaged widget/provider UX coverage. Native fixture tests
run in the shared source gate; the shipped executable has no QA test entry point.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile
import tomllib

import gate


def verify_run(path, expected_commit):
    run = json.loads(path.read_text())
    if (
        run.get("head_sha") != expected_commit or run.get("status") != "completed"
        or run.get("conclusion") != "success" or run.get("path") != ".github/workflows/release.yml"
        or run.get("event") not in {"push", "workflow_dispatch"}
    ):
        raise ValueError("producer must be a successful Release workflow for the exact source commit")


def installed(directory, expected_commit):
    artifacts = gate.ROOT / "target/qa-package"
    artifacts.mkdir(parents=True, exist_ok=True)
    version = tomllib.loads((gate.ROOT / "Cargo.toml").read_text())["package"]["version"]
    packages = sorted(directory.glob("*.deb"))
    if len(packages) != 1:
        raise ValueError("expected exactly one Debian package")
    package = packages[0]
    manifest = json.loads(Path(str(package) + ".identity.json").read_text())
    with package.open("rb") as stream:
        package_hash = hashlib.file_digest(stream, "sha256").hexdigest()
    if manifest != {"file": package.name, "commit": expected_commit, "version": version, "sha256": package_hash}:
        raise ValueError("package identity mismatch")
    with tempfile.TemporaryDirectory(prefix="agent-terminal-package-") as temporary:
        home = Path(temporary)
        env = gate.isolated_env(home)
        # Cargo homes are unnecessary for the installed app. No user homes remain.
        env.pop("CARGO_HOME")
        env.pop("RUSTUP_HOME")
        env["PATH"] = "/usr/bin:/bin"
        env["RUST_LOG"] = "info"
        extracted = home / "package"
        subprocess.run(["dpkg-deb", "--extract", str(package.resolve()), str(extracted)], check=True, timeout=30, env=env)
        source_binary = extracted / "usr/bin/agent-terminal"
        installed_binary = Path("/usr/bin/agent-terminal")
        with source_binary.open("rb") as stream:
            expected_hash = hashlib.file_digest(stream, "sha256").hexdigest()
        with installed_binary.open("rb") as stream:
            installed_hash = hashlib.file_digest(stream, "sha256").hexdigest()
        if expected_hash != installed_hash:
            raise ValueError("installed executable bytes differ from the downloaded package")
        result = gate.run_logged([str(installed_binary), "--help"], artifacts / "startup.log", env, 30)
        output = (artifacts / "startup.log").read_text(errors="replace")
        reported = re.findall(r"Starting Agent Terminal \(v([^\)]+)\)", output)
        result.update({"commit": expected_commit, "version": version, "reported_versions": reported, "installed_binary_sha256": installed_hash, "passed": False, "coverage": "package identity and startup; source-native UX is gated separately"})
        report = artifacts / "identity.json"
        report.write_text(json.dumps(result, indent=2) + "\n")
        if result["exit_code"] or result["timed_out"]:
            raise ValueError("installed executable failed prompt-less startup")
        if reported != [version]:
            raise ValueError("installed executable did not report the expected compiled version")
        result["passed"] = True
        report.write_text(json.dumps(result, indent=2) + "\n")
    print(f"Installed startup passed: v{version}, source {expected_commit}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["verify-run", "installed"])
    parser.add_argument("path", type=Path)
    parser.add_argument("commit")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.commit):
        parser.error("expected a 40-character source commit")
    if args.action == "verify-run":
        verify_run(args.path, args.commit)
    else:
        installed(args.path, args.commit)


if __name__ == "__main__":
    main()
