# Makefile for Gemini Terminal

APP_NAME = gemini-terminal
BINARY = target/release/$(APP_NAME)
DESKTOP_FILE = assets/gemini-terminal.desktop
ICON_FILE = assets/gemini_logo.svg
LOCAL_BIN = $(HOME)/.local/bin
LOCAL_DESKTOP = $(HOME)/.local/share/applications/gemini-terminal.desktop
LOCAL_ICON = $(HOME)/.local/share/icons/gemini_logo.svg

.PHONY: all build clean install uninstall package deps help

all: build

help:
	@echo "Usage:"
	@echo "  make deps      - Install system dependencies (requires sudo)"
	@echo "  make build     - Build the release binary"
	@echo "  make install   - Install binary and desktop entry locally"
	@echo "  make uninstall - Remove local installation and assets"
	@echo "  make package   - Generate a .deb package using cargo-deb"
	@echo "  make clean     - Remove build artifacts"

deps:
	@echo "Installing system dependencies..."
	sudo apt update && sudo apt install -y libvte-2.91-gtk4-dev libgtk-4-dev

build:
	@echo "Building $(APP_NAME) in release mode..."
	cargo build --release

package:
	@echo "Checking for cargo-deb..."
	@command -v cargo-deb >/dev/null 2>&1 || (echo "Installing cargo-deb..." && cargo install cargo-deb)
	@echo "Generating Debian package..."
	cargo deb

install: build
	@echo "Installing binary to $(LOCAL_BIN)..."
	mkdir -p $(LOCAL_BIN)
	cp $(BINARY) $(LOCAL_BIN)/$(APP_NAME)
	@echo "Installing icon to $(HOME)/.local/share/icons..."
	mkdir -p $(HOME)/.local/share/icons
	cp $(ICON_FILE) $(LOCAL_ICON)
	@echo "Updating desktop entry..."
	mkdir -p $(HOME)/.local/share/applications
	cp $(DESKTOP_FILE) $(LOCAL_DESKTOP)
	sed -i 's|^Exec=.*|Exec=$(LOCAL_BIN)/$(APP_NAME)|' $(LOCAL_DESKTOP)
	sed -i 's|^Icon=.*|Icon=$(LOCAL_ICON)|' $(LOCAL_DESKTOP)
	@echo "Done! You can now launch Gemini Terminal from your menu."

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
