#!/usr/bin/env python3
import pathlib
import re
import subprocess
import sys

def main():
    kind = (sys.argv[1] if len(sys.argv) > 1 else "patch").lower()
    p = pathlib.Path("Cargo.toml")
    text = p.read_text()
    m = re.search(r'^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"', text, re.M)
    if not m:
        sys.exit("Error: Could not find version in Cargo.toml")

    maj, minor, patch = int(m.group(1)), int(m.group(2)), int(m.group(3))
    old_v = f"{maj}.{minor}.{patch}"

    if kind in ("major",):
        new_v = f"{maj + 1}.0.0"
    elif kind in ("minor",):
        new_v = f"{maj}.{minor + 1}.0"
    elif kind in ("patch", "fix", ""):
        new_v = f"{maj}.{minor}.{patch + 1}"
    elif re.match(r"^\d+\.\d+\.\d+", kind):
        new_v = kind
    else:
        sys.exit(f"Error: Unknown bump option: {kind}. Use major, minor, patch, or X.Y.Z")

    new_text = re.sub(
        r'^version\s*=\s*"[^"]+"',
        f'version = "{new_v}"',
        text,
        count=1,
        flags=re.M,
    )
    p.write_text(new_text)
    print(f"Updated Cargo.toml: {old_v} -> {new_v}")

    subprocess.run(["cargo", "check", "--quiet"], check=True)
    print("Updated Cargo.lock")

    rel_dir = pathlib.Path("docs/releases")
    rel_dir.mkdir(parents=True, exist_ok=True)
    rel_file = rel_dir / f"v{new_v}.md"
    if not rel_file.exists():
        stub = f"""Agent Terminal {new_v} delivers ...

## Added

- 

## Changed

- 

## Fixed

- 

## Upgrade and install

Existing settings and threads are retained. Download the appropriate package and SHA256SUMS from this release, verify its checksum, then install it:

| Distribution | Command |
|---|---|
| Debian 13, Ubuntu 24.04 or newer | `sudo apt install ./agent-terminal_{new_v}-1_amd64.deb` |
| Fedora 40 or newer | `sudo dnf install ./agent-terminal-{new_v}-1.x86_64.rpm` |
| Arch Linux | `sudo pacman -U ./agent-terminal-{new_v}-1-x86_64.pkg.tar.zst` |
"""
        rel_file.write_text(stub)
        print(f"Created release notes stub: {rel_file}")

    print(f"Bump complete: v{new_v}")

if __name__ == "__main__":
    main()
