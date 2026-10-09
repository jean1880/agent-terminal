#!/usr/bin/env python3
"""Reject unrelated/failed runs and modified package bytes before installation."""

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

import gate
import package_smoke

spec = importlib.util.spec_from_file_location("artifact_manifest", gate.ROOT / "packaging/artifact-manifest.py")
manifest = importlib.util.module_from_spec(spec)
spec.loader.exec_module(manifest)


class PackageSmokeTests(unittest.TestCase):
    def test_producer_run_must_be_exact_successful_release(self):
        commit = "a" * 40
        run = {"head_sha": commit, "status": "completed", "conclusion": "success", "path": ".github/workflows/release.yml", "event": "push"}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "run.json"
            path.write_text(json.dumps(run))
            package_smoke.verify_run(path, commit)
            for key, bad in [("head_sha", "b" * 40), ("conclusion", "failure"), ("status", "in_progress"), ("path", ".github/workflows/ci.yml"), ("event", "pull_request")]:
                path.write_text(json.dumps({**run, key: bad}))
                with self.assertRaises(ValueError):
                    package_smoke.verify_run(path, commit)

    def test_package_bytes_commit_and_version_are_all_required(self):
        with tempfile.TemporaryDirectory() as directory:
            package = Path(directory) / "synthetic.deb"
            package.write_bytes(b"synthetic package bytes")
            identity = {"file": package.name, "sha256": manifest.digest(package), "commit": "a" * 40, "version": "3.0.0"}
            Path(str(package) + ".identity.json").write_text(json.dumps(identity))
            manifest.verify(package, "a" * 40, "3.0.0")
            with self.assertRaises(ValueError):
                manifest.verify(package, "b" * 40, "3.0.0")
            with self.assertRaises(ValueError):
                manifest.verify(package, "a" * 40, "3.0.1")
            package.write_bytes(b"replaced package bytes")
            with self.assertRaises(ValueError):
                manifest.verify(package, "a" * 40, "3.0.0")


if __name__ == "__main__":
    unittest.main()
