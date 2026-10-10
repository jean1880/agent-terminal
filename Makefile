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
	@python3 packaging/bump-version.py "$(BUMP_OPTION)"

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
