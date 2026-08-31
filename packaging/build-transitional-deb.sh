#!/usr/bin/env bash
#
# Builds the transitional `antigravity-terminal` package for the 2.0.0 rename.
#
# Without this, a machine running the old package never learns that it was
# renamed: apt has no reason to install a package it has never heard of, so the
# old binary simply stays installed and the user keeps running 1.x forever. The
# stub carries no files — it exists only to depend on agent-terminal, so
# `apt upgrade` replaces itself with the new package.
#
# cargo-deb builds one package per invocation, hence the hand-built control tree.
# Retire this once 2.x has been out for a release or two.
set -euo pipefail

VERSION="${1:?usage: $0 <version>}"
OUT_DIR="${2:-.}"

OLD_NAME="antigravity-terminal"
NEW_NAME="agent-terminal"

BUILD_DIR="$(mktemp -d)"
trap 'rm -rf "$BUILD_DIR"' EXIT

mkdir -p "$BUILD_DIR/DEBIAN"
cat > "$BUILD_DIR/DEBIAN/control" <<EOF
Package: $OLD_NAME
Version: $VERSION
Architecture: all
Maintainer: jdesroches
Section: oldlibs
Priority: optional
Depends: $NEW_NAME (>= $VERSION)
Description: transitional package for $NEW_NAME
 This package is a stub that pulls in $NEW_NAME, which replaces it under the
 project's new name. It contains no files and may be removed once $NEW_NAME is
 installed.
EOF

mkdir -p "$OUT_DIR"
dpkg-deb --build --root-owner-group "$BUILD_DIR" \
    "$OUT_DIR/${OLD_NAME}_${VERSION}_all.deb" >/dev/null

echo "$OUT_DIR/${OLD_NAME}_${VERSION}_all.deb"
