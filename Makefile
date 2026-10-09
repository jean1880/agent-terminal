# Makefile for Agent Terminal

APP_NAME = agent-terminal
BINARY = target/release/$(APP_NAME)
APP_ID = ca.nuvek.AgentTerminal
DESKTOP_FILE = assets/$(APP_ID).desktop
ICON_FILE = assets/$(APP_ID).svg
LOCAL_BIN = $(HOME)/.local/bin
LOCAL_DESKTOP = $(HOME)/.local/share/applications/$(APP_ID).desktop
LOCAL_ICON_DIR = $(HOME)/.local/share/icons/hicolor/scalable/apps
LOCAL_ICON = $(LOCAL_ICON_DIR)/$(APP_ID).svg

.PHONY: all build start-local clean install uninstall package deps help bump-version bum-version release

all: build

help:
	@echo "Usage:"
	@echo "  make deps                 - Install system dependencies (requires sudo)"
	@echo "  make build                - Build the release binary"
	@echo "  make start-local          - Build and run the app locally (debug)"
	@echo "  make install              - Install binary and desktop entry locally"
	@echo "  make uninstall            - Remove local installation and assets"
	@echo "  make package              - Generate a .deb package using cargo-deb"
	@echo "  make clean                - Remove build artifacts"
	@echo "  make bump-version <option>- Bump SemVer version (option: patch | minor | major | X.Y.Z, default: patch)"
	@echo "  make release <option>     - Bump version, create release doc, commit and tag"

deps:
	@echo "Installing system dependencies..."
	sudo apt update && sudo apt install -y libvte-2.91-gtk4-dev libgtk-4-dev libadwaita-1-dev libgtksourceview-5-dev libglib2.0-dev-bin

build:
	@echo "Building $(APP_NAME) in release mode..."
	cargo build --release

build-macos:
	@echo "Building $(APP_NAME) for macOS (no default features)..."
	cargo build --no-default-features --release

check-macos:
	@echo "Checking $(APP_NAME) for macOS (no default features)..."
	cargo check --no-default-features

start-local:
	@echo "Building and running $(APP_NAME) locally..."
	cargo run

package:
	@echo "Checking for cargo-deb..."
	@command -v cargo-deb >/dev/null 2>&1 || (echo "Installing cargo-deb..." && cargo install cargo-deb)
	@echo "Generating Debian package..."
	cargo deb

install: build
	@echo "Installing binary to $(LOCAL_BIN)..."
	mkdir -p $(LOCAL_BIN)
	cp $(BINARY) $(LOCAL_BIN)/$(APP_NAME)
	@echo "Installing icon to $(LOCAL_ICON_DIR)..."
	mkdir -p $(LOCAL_ICON_DIR)
	cp $(ICON_FILE) $(LOCAL_ICON)
	@echo "Updating desktop entry..."
	mkdir -p $(HOME)/.local/share/applications
	cp $(DESKTOP_FILE) $(LOCAL_DESKTOP)
	sed -i 's|^Exec=.*|Exec=$(LOCAL_BIN)/$(APP_NAME)|' $(LOCAL_DESKTOP)
	@# Icon= stays the theme name; the icon is installed into hicolor above so
	@# lookup resolves it the same way the .deb-installed one does.
	-update-desktop-database $(HOME)/.local/share/applications 2>/dev/null || true
	-gtk4-update-icon-cache -q -t -f $(HOME)/.local/share/icons/hicolor 2>/dev/null || true
	@echo "Done! You can now launch Agent Terminal from your menu."

uninstall:
	@echo "Uninstalling $(APP_NAME) from $(LOCAL_BIN)..."
	rm -f $(LOCAL_BIN)/$(APP_NAME)
	@echo "Removing icon from $(HOME)/.local/share/icons..."
	rm -f $(LOCAL_ICON)
	@echo "Removing desktop entry..."
	rm -f $(LOCAL_DESKTOP)
	@echo "Uninstall complete."

clean:
	@echo "Cleaning project..."
	cargo clean

# Intercept argument passed to bump-version or release, e.g. `make bump-version patch`
ifeq ($(filter bump-version bum-version release,$(firstword $(MAKECMDGOALS))),$(firstword $(MAKECMDGOALS)))
  BUMP_ARGS := $(wordlist 2,$(words $(MAKECMDGOALS)),$(MAKECMDGOALS))
  $(eval $(BUMP_ARGS):;@:)
endif

BUMP_OPTION ?= $(or $(BUMP_ARGS),patch)

bump-version bum-version:
	@python3 -c '\
import re, pathlib, subprocess, sys; \
kind = "$(BUMP_OPTION)".lower(); \
p = pathlib.Path("Cargo.toml"); \
text = p.read_text(); \
m = re.search(r"^version\s*=\s*\"(\d+)\.(\d+)\.(\d+)\"", text, re.M); \
if not m: sys.exit("Error: Could not find version in Cargo.toml"); \
maj, minor, patch = int(m.group(1)), int(m.group(2)), int(m.group(3)); \
old_v = f"{maj}.{minor}.{patch}"; \
if kind in ("major",): new_v = f"{maj + 1}.0.0"; \
elif kind in ("minor",): new_v = f"{maj}.{minor + 1}.0"; \
elif kind in ("patch", "fix", ""): new_v = f"{maj}.{minor}.{patch + 1}"; \
elif re.match(r"^\d+\.\d+\.\d+", kind): new_v = kind; \
else: sys.exit(f"Error: Unknown bump option: {kind}. Use major, minor, patch, or X.Y.Z"); \
new_text = re.sub(r"^version\s*=\s*\"[^\"]+\"", f"version = \"{new_v}\"", text, count=1, flags=re.M); \
p.write_text(new_text); \
print(f"Updated Cargo.toml: {old_v} -> {new_v}"); \
subprocess.run(["cargo", "check", "--quiet"], check=True); \
print("Updated Cargo.lock"); \
rel_dir = pathlib.Path("docs/releases"); \
rel_dir.mkdir(parents=True, exist_ok=True); \
rel_file = rel_dir / f"v{new_v}.md"; \
if not rel_file.exists(): \
    stub = f"""Agent Terminal {new_v} delivers ...\n\n## Added\n\n- \n\n## Changed\n\n- \n\n## Fixed\n\n- \n\n## Upgrade and install\n\nExisting settings and threads are retained. Download the appropriate package and SHA256SUMS from this release, verify its checksum, then install it:\n\n| Distribution | Command |\n|---|---|\n| Debian 13, Ubuntu 24.04 or newer | `sudo apt install ./agent-terminal_{new_v}-1_amd64.deb` |\n| Fedora 40 or newer | `sudo dnf install ./agent-terminal-{new_v}-1.x86_64.rpm` |\n| Arch Linux | `sudo pacman -U ./agent-terminal-{new_v}-1-x86_64.pkg.tar.zst` |\n"""; \
    rel_file.write_text(stub); \
    print(f"Created release notes stub: {rel_file}"); \
print(f"Bump complete: v{new_v}"); \
'

release: bump-version
	@python3 -c '\
import re, pathlib, subprocess; \
v = re.search(r"^version\s*=\s*\"([^\"]+)\"", pathlib.Path("Cargo.toml").read_text(), re.M).group(1); \
print(f"\nCutting release v{v}..."); \
subprocess.run(["git", "add", "Cargo.toml", "Cargo.lock", f"docs/releases/v{v}.md"], check=True); \
subprocess.run(["git", "commit", "-m", f"chore(release): cut {v}"], check=True); \
subprocess.run(["git", "tag", "-a", f"v{v}", "-m", f"Release v{v}"], check=True); \
print(f"\nSuccessfully cut release v{v}!"); \
print("To publish to CI and repository:"); \
print(f"  git push origin master && git push origin v{v}"); \
'
