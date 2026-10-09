#!/usr/bin/env python3
"""Bind package bytes and version to the source commit of a trusted CI run."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def packages(directory):
    return sorted(path for path in directory.iterdir() if path.is_file() and (
        path.name.endswith(".deb") or path.name.endswith(".rpm") or path.name.endswith(".pkg.tar.zst")
    ))


def verify(package, expected_commit, expected_version):
    manifest = json.loads(Path(str(package) + ".identity.json").read_text())
    expected = {
        "file": package.name, "sha256": digest(package),
        "commit": expected_commit, "version": expected_version,
    }
    if manifest != expected:
        raise ValueError("package identity does not match the requested commit/version/hash")
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["create", "verify"])
    parser.add_argument("directory", type=Path)
    parser.add_argument("--commit", default=os.environ.get("PACKAGE_SOURCE_COMMIT"))
    parser.add_argument("--version", default=tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"])
    args = parser.parse_args()
    if not args.commit or not re.fullmatch(r"[0-9a-f]{40}", args.commit):
        parser.error("an exact source commit (40 hex characters) is required")
    assets = packages(args.directory)
    if len(assets) != 1:
        parser.error("expected exactly one package artifact")
    package = assets[0]
    if args.action == "create":
        manifest = {"file": package.name, "sha256": digest(package), "commit": args.commit, "version": args.version}
        Path(str(package) + ".identity.json").write_text(json.dumps(manifest, indent=2) + "\n")
    else:
        verify(package, args.commit, args.version)
    print(f"{args.action}: {package.name}, commit {args.commit}, version {args.version}")


if __name__ == "__main__":
    main()
