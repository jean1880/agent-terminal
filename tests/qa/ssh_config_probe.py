#!/usr/bin/env python3
"""Parse a synthetic SSH config without connecting, and report ownership."""

import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    config = Path(sys.argv[1])
    metadata = config.stat()
    result = subprocess.run(
        ["ssh", "-G", "-F", str(config), "fixture"],
        text=True, capture_output=True, timeout=10,
    )
    fields = {}
    for line in result.stdout.splitlines():
        key, _, value = line.partition(" ")
        if key in {"hostname", "user", "canonicalizehostname"}:
            fields[key] = value
    print(json.dumps({
        "uid": os.getuid(), "owner": metadata.st_uid,
        "mode": metadata.st_mode & 0o777, "exit_code": result.returncode,
        "fields": fields, "stderr": result.stderr,
    }))
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
