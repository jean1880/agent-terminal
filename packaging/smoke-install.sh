#!/usr/bin/env bash
# Installs a built package with its distribution's package manager in a clean container of that
# distribution, then checks the result runs: every library the binary links resolves, and
# `agent-terminal --help` exits 0. The package manager pulls the runtime dependencies the package
# declares, so a missing or misnamed one fails here.
#
#   bash packaging/smoke-install.sh <package file>
set -euo pipefail

pkg="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"

case "$pkg" in
    *.deb)
        export DEBIAN_FRONTEND=noninteractive
        apt-get update
        apt-get install -y --no-install-recommends "$pkg"
        ;;
    *.rpm)
        dnf install -y --setopt=install_weak_deps=False "$pkg"
        ;;
    *.pkg.tar.zst)
        # A full upgrade first: -Sy alone is a partial upgrade (new GTK on the image's old glib).
        pacman -Syu --noconfirm
        pacman -U --noconfirm "$pkg"
        ;;
    *)
        echo "unknown package type: $pkg" >&2
        exit 2
        ;;
esac

if ldd /usr/bin/agent-terminal | grep "not found"; then
    echo "unresolved libraries" >&2
    exit 1
fi
agent-terminal --help
test -f /usr/share/applications/ca.nuvek.AgentTerminal.desktop
test -f /usr/share/icons/hicolor/scalable/apps/ca.nuvek.AgentTerminal.svg
echo "smoke test passed: $(basename "$pkg")"
