#!/usr/bin/env bash
# Shared by the build-*.sh scripts (sourced, not run). Each runs as root in a clean container of
# its distribution: the release workflow's jobs, or `docker run` locally.
#
#   SRC  the source tree (read-only is fine): default, the repository this file is in
#   OUT  where the package lands: default $SRC/dist
#
# The tree is copied to a scratch directory first, so the checkout is never written to.

set -euo pipefail

SRC="${SRC:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
OUT="${OUT:-$SRC/dist}"
WORK="$(mktemp -d /tmp/agent-terminal-build.XXXXXX)"

# The package version: the [package] version in Cargo.toml (its first `version =` line).
version() {
    sed -n 's/^version = "\(.*\)"$/\1/p' "$SRC/Cargo.toml" | head -n 1
}

# Copies the tree to $WORK, without build output or git metadata, and enters it.
stage_source() {
    tar --exclude=./target --exclude=./dist --exclude=./.git -C "$SRC" -cf - . \
        | tar -C "$WORK" -xf -
    cd "$WORK"
}

# A current stable Rust toolchain through rustup (distribution packages lag behind the crates'
# minimum Rust), unless one is already on PATH.
ensure_rust() {
    export PATH="$HOME/.cargo/bin:$PATH"
    if ! command -v cargo >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --profile minimal --default-toolchain stable
    fi
    cargo --version
}

# Puts the built package $1 in $OUT and lists them.
collect() {
    mkdir -p "$OUT"
    cp -v "$1" "$OUT/"
    ls -l "$OUT"
}
