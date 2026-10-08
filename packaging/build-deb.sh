#!/usr/bin/env bash
# Builds the Debian/Ubuntu package (cargo-deb, [package.metadata.deb]) in an Ubuntu 24.04
# container: agent-terminal_<version>-1_amd64.deb. See common.sh for SRC/OUT.
# shellcheck source=packaging/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
    ca-certificates curl build-essential pkg-config dpkg-dev \
    libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev libgtksourceview-5-dev \
    libglib2.0-dev-bin

ensure_rust
stage_source
cargo build --release --locked
cargo install cargo-deb --locked
# $auto in the metadata derives the runtime dependencies from the binary (dpkg-shlibdeps).
cargo deb --no-build --locked -o "$WORK/pkg/"

deb="$(ls "$WORK"/pkg/*.deb)"
dpkg-deb --info "$deb"
dpkg-deb --contents "$deb"
# The runtime dependencies must name the libraries it links, or apt cannot install it right.
for lib in libgtk-4-1 libadwaita-1-0 libvte-2.91-gtk4-0 libgtksourceview-5-0; do
    dpkg-deb --field "$deb" Depends | grep -q "$lib" || { echo "missing dependency: $lib" >&2; exit 1; }
done
collect "$deb"
