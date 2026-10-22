#!/usr/bin/env bash
# Installs a built package with its distribution's package manager in a clean container of that
# distribution, then checks the result runs: every library the binary links resolves, and
# `agent-terminal --help` exits 0. The package manager pulls the runtime dependencies the package
# declares, so a missing or misnamed one fails here. Last, `agent-terminal --uninstall --yes`
# must remove a user's data and the package.
#
#   bash packaging/smoke-install.sh <package file>
set -euo pipefail

if [[ -d "$1" ]]; then
    shopt -s nullglob
    packages=("$1"/*.deb "$1"/*.rpm "$1"/*.pkg.tar.zst)
    if [[ ${#packages[@]} -ne 1 ]]; then
        echo "expected exactly one package in $1" >&2
        exit 2
    fi
    set -- "${packages[0]}"
fi
pkg="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"

case "$pkg" in
    *.deb)
        export DEBIAN_FRONTEND=noninteractive
        apt-get update
        apt-get install -y --no-install-recommends "$pkg" python3
        ;;
    *.rpm)
        dnf install -y --setopt=install_weak_deps=False "$pkg" python3
        ;;
    *.pkg.tar.zst)
        # A full upgrade first: -Sy alone is a partial upgrade (new GTK on the image's old glib).
        pacman -Syu --noconfirm --needed python
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
python3 tests/qa/delegation_smoke.py /usr/bin/agent-terminal
test -f /usr/share/applications/ca.nuvek.AgentTerminal.desktop
test -f /usr/share/icons/hicolor/scalable/apps/ca.nuvek.AgentTerminal.svg

# The uninstall, through the app itself as a user runs it: this user's data goes, a neighbour's
# folder stays, and the package is removed with its own manager (root here, so no sudo). A home
# of its own, owned by root, and no sudo variables: the uninstall refuses to run for a user
# through sudo (package-smoke.yml starts this script with sudo).
user_home="$(mktemp -d)"
data=("$user_home/.config/agent-terminal" "$user_home/.local/state/agent-terminal" "$user_home/.cache/agent-terminal")
mkdir -p "${data[@]}" "$user_home/.config/smoke-neighbour"
echo '{}' > "$user_home/.config/agent-terminal/config.json"
env -u SUDO_USER -u SUDO_UID -u SUDO_GID -u SUDO_COMMAND -u PKEXEC_UID \
    -u XDG_CONFIG_HOME -u XDG_STATE_HOME -u XDG_CACHE_HOME -u XDG_DATA_HOME -u XDG_RUNTIME_DIR \
    HOME="$user_home" agent-terminal --uninstall --yes
for dir in "${data[@]}"; do
    if [[ -e "$dir" ]]; then
        echo "uninstall left $dir" >&2
        exit 1
    fi
done
test -d "$user_home/.config/smoke-neighbour"
if [[ -e /usr/bin/agent-terminal ]]; then
    echo "uninstall left the package installed" >&2
    exit 1
fi
echo "smoke test passed: $(basename "$pkg")"
