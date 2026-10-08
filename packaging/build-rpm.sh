#!/usr/bin/env bash
# Builds the Fedora package (cargo-generate-rpm, [package.metadata.generate-rpm]) in a Fedora
# container: agent-terminal-<version>-1.x86_64.rpm. See common.sh for SRC/OUT.
# shellcheck source=packaging/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

dnf install -y --setopt=install_weak_deps=False \
    curl gcc pkgconf-pkg-config tar rpm-build \
    gtk4-devel libadwaita-devel vte291-gtk4-devel gtksourceview5-devel glib2-devel

ensure_rust
stage_source
cargo build --release --locked
cargo install cargo-generate-rpm --locked
# -o names a file unless the directory already exists.
mkdir -p "$WORK/pkg"
cargo generate-rpm -o "$WORK/pkg"

rpm="$(ls "$WORK"/pkg/*.rpm)"
rpm -qip "$rpm"
rpm -qlp "$rpm"
# The requirements must name the libraries it links, or dnf cannot install it right.
requires="$(rpm -qp --requires "$rpm")"
echo "$requires"
for lib in libgtk-4.so libadwaita-1.so libvte-2.91-gtk4.so libgtksourceview-5.so; do
    grep -q "$lib" <<<"$requires" || { echo "missing requirement: $lib" >&2; exit 1; }
done
collect "$rpm"
