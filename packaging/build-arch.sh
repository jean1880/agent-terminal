#!/usr/bin/env bash
# Builds the Arch Linux package (makepkg, packaging/arch/PKGBUILD) in an Arch container:
# agent-terminal-<version>-1-x86_64.pkg.tar.zst. See common.sh for SRC/OUT.
# shellcheck source=packaging/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/common.sh"

pacman -Syu --noconfirm --needed \
    base-devel rust pkgconf python \
    gtk4 libadwaita vte4 gtksourceview5 glib2 pango cairo gdk-pixbuf2 graphene

stage_source
# makepkg refuses to run as root: build as an unprivileged user that owns the staged tree.
useradd -m builder 2>/dev/null || true
chown -R builder: "$WORK"
sed -i "s/^pkgver=.*/pkgver=$(version)/" "$WORK/packaging/arch/PKGBUILD"
mkdir -p "$WORK/pkg"
chown builder: "$WORK/pkg"
su builder -c "cd '$WORK/packaging/arch' && PKGDEST='$WORK/pkg' makepkg --noconfirm --nodeps"

pkg="$(ls "$WORK"/pkg/*.pkg.tar.zst)"
pacman -Qip "$pkg"
pacman -Qlp "$pkg"
collect "$pkg"
