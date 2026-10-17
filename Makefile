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

.PHONY: all build start-local clean install uninstall package deps help bump-version bum-version release \
	test check coverage hooks lint test-display test-one test-stress test-qa test-deps bench

# ---- Tests -----------------------------------------------------------------------------------
# Tools: cargo-nextest and cargo-llvm-cov (`make test-deps`), plus xvfb-run and dbus-run-session.
# GTK tests need a display: every run gets a private Xvfb and D-Bus session, and private XDG
# directories, so no test reads or writes the real settings, thread store or keyring.
# Line coverage as llvm-cov counts it (86.5% on 2026-10-13). Raise it as coverage rises.
COVERAGE_FLOOR ?= 86
TEST_HOME := $(CURDIR)/target/test-home
QA_ARTIFACTS := $(CURDIR)/target/qa-artifacts
# What each variable is for: .env.example, and tests/qa/README.md ("Environment variables").
DISPLAY_ENV = XDG_CONFIG_HOME=$(TEST_HOME)/config XDG_DATA_HOME=$(TEST_HOME)/data \
	XDG_STATE_HOME=$(TEST_HOME)/state XDG_CACHE_HOME=$(TEST_HOME)/cache \
	GSETTINGS_BACKEND=memory GTK_A11Y=test GDK_BACKEND=x11 LANG=C.UTF-8 \
	GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null \
	AGENT_TERMINAL_QA_ARTIFACTS=$(QA_ARTIFACTS)/screenshots
DISPLAY_RUN = env $(DISPLAY_ENV) xvfb-run -a dbus-run-session --
# DISPLAY_RUN with .env (git-ignored, optional) on top: copy .env.example, which documents it.
DOTENV_RUN = env $(DISPLAY_ENV) $$(if [ -f .env ]; then grep -v '^[[:space:]]*\#' .env; fi) \
	xvfb-run -a dbus-run-session --
NEXTEST_PROFILE ?= default
# Extra cargo-nextest flags for every test target, e.g. ARGS=--no-capture,
# ARGS='--cargo-profile release' (timings) or ARGS='--retries 2'.
ARGS ?=
NEXTEST = cargo nextest run --locked --workspace --all-targets --profile $(NEXTEST_PROFILE) $(ARGS)

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
	@echo "  make test                 - Run the test suite (parallel, about 35 s)"
	@echo "  make test-one T=<name>    - Tests matching <name>, display tests included (.env overrides: see .env.example)"
	@echo "  make test-stress T=<name> [N=30] - Tests matching <name>, run N times under a display, to prove a flaky test fixed"
	@echo "  make bench T=<name>       - Timing tests matching <name>: release build, output shown"
	@echo "  ARGS='<nextest flags>'    - Extra flags for any test target (e.g. ARGS=--no-capture)"
	@echo "  make check                - Everything CI and a release run: lint, all tests, display tests"
	@echo "  make coverage             - Instrumented run, HTML report, fails under COVERAGE_FLOOR ($(COVERAGE_FLOOR)%)"
	@echo "  make hooks                - Opt in to the pre-commit hook (fmt and clippy)"
	@echo "  make test-deps            - Install cargo-nextest and cargo-llvm-cov"
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

test-deps:
	rustup component add llvm-tools-preview
	cargo install --locked cargo-nextest cargo-llvm-cov

$(TEST_HOME):
	mkdir -p -m 700 $(TEST_HOME)/config $(TEST_HOME)/data $(TEST_HOME)/state $(TEST_HOME)/cache

# The suite, each test in its own process (.config/nextest.toml).
test: $(TEST_HOME)
	$(DISPLAY_RUN) $(NEXTEST)

# Tests that present windows and measure layout: #[ignore]d so `test` stays fast, run here one
# at a time so none is measured while another loads the machine.
test-display: $(TEST_HOME)
	$(DISPLAY_RUN) $(NEXTEST) --run-ignored only --test-threads 1

# The tests whose names match T, display tests included, one at a time, in DISPLAY_ENV with
# .env (git-ignored, optional) on top: copy .env.example, which documents every variable.
test-one: $(TEST_HOME)
	@test -n "$(T)" || { echo "usage: make test-one T=<test name> [ARGS=<nextest flags>]"; exit 2; }
	$(DOTENV_RUN) $(NEXTEST) --run-ignored all --test-threads 1 -E 'test($(T))'

# test-one for benchmarks and measurements: optimised build, output shown (they print their
# numbers), and the tests .config/nextest.toml's default-filter keeps out of every other run
# included. --no-capture also runs them one at a time. BENCH_PROFILE=dev measures the debug build.
BENCH_PROFILE ?= release
bench: $(TEST_HOME)
	@test -n "$(T)" || { echo "usage: make bench T=<test name> [BENCH_PROFILE=dev]"; exit 2; }
	$(DOTENV_RUN) $(NEXTEST) --cargo-profile $(BENCH_PROFILE) --no-capture --ignore-default-filter \
		--run-ignored all -E 'test($(T))'

# The tests whose names match T, run N times over (default 30) in DISPLAY_ENV, to prove a flaky
# test fixed or catch one: no retries, every iteration reported, failures summarised at the end.
# Tests in one iteration still run in parallel, as `test` runs them, so they load each other.
STRESS_COUNT ?= 30
test-stress: $(TEST_HOME)
	@test -n "$(T)" || { echo "usage: make test-stress T=<test name> [N=<runs>]"; exit 2; }
	$(DOTENV_RUN) $(NEXTEST) --retries 0 --no-fail-fast --stress-count $(or $(N),$(STRESS_COUNT)) \
		--hide-progress-bar --status-level fail --final-status-level fail -E 'test($(T))'

# The QA scripts' own unit tests (package smoke, live-smoke checks): no network, no quota.
test-qa:
	python3 -m unittest discover -s tests/qa -p 'test_*.py'

# Opt-in installed-provider proofs against local fixture models; never the user's workspace
# or registration. The helper owns its isolated homes and bounded process groups.
.PHONY: delegation-proof
DELEGATION_PROOF_ARGS ?= $(PROOF_ARGS)
delegation-proof:
	python3 tests/qa/delegation_proof.py $(DELEGATION_PROOF_ARGS)

.PHONY: delegation-smoke
DELEGATION_BINARY ?= target/debug/agent-terminal
delegation-smoke:
	python3 tests/qa/delegation_smoke.py $(DELEGATION_BINARY)

lint:
	cargo fmt --all -- --check
	cargo clippy --locked --workspace --all-targets -- -D warnings
	cargo check --locked --no-default-features

# What CI and a release run.
check: lint test test-display test-qa

# The same two runs instrumented. COVERAGE_FLOOR only ever goes up.
coverage: $(TEST_HOME)
	cargo llvm-cov clean --workspace
	$(DISPLAY_RUN) cargo llvm-cov nextest --no-report --locked --workspace --all-targets --profile $(NEXTEST_PROFILE)
	$(DISPLAY_RUN) cargo llvm-cov nextest --no-report --locked --workspace --all-targets --profile $(NEXTEST_PROFILE) \
		--run-ignored only --test-threads 1
	cargo llvm-cov report --workspace --html --output-dir $(QA_ARTIFACTS)/coverage
	cargo llvm-cov report --workspace --lcov --output-path $(QA_ARTIFACTS)/coverage/lcov.info
	cargo llvm-cov report --workspace --summary-only --fail-under-lines $(COVERAGE_FLOOR)

hooks:
	git config core.hooksPath .githooks
	@echo "Pre-commit hook on: fmt and clippy run before each commit."

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
