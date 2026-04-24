# Build and installation configuration
PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
APPID = com.google.gemini-terminal

all: build-frontend build-backend

build-frontend:
	cd frontend && npm run build

build-backend:
	cargo build --release

install:
	install -D -m 755 target/release/gemini-terminal $(DESTDIR)$(BINDIR)/gemini-terminal
	install -D -m 644 assets/gemini-terminal.desktop $(DESTDIR)$(DATADIR)/applications/$(APPID).desktop
	install -D -m 644 assets/gemini_logo.png $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APPID).png

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/gemini-terminal
	rm -f $(DESTDIR)$(DATADIR)/applications/$(APPID).desktop
	rm -f $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/$(APPID).png

clean:
	cargo clean
	rm -rf frontend/dist

.PHONY: all build-frontend build-backend install uninstall clean
