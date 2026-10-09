# Source and delivery gates

Run `python3 tests/qa/gate.py` from any directory. It keeps formatting, Clippy
and workspace tests, then runs the exact ignored tests in `native-tests.json`.
Every native case has a private Xvfb display, D-Bus session and temporary
HOME/XDG settings. Zero tests, ignored tests and timeouts fail. Measurement-only
tests remain excluded. `--native-only` is a development shortcut and does not
satisfy the delivery gate. Python 3.11+, Rust with rustfmt/Clippy, the project's
native development libraries, Xvfb, xauth and dbus-run-session are required.
CI and beta use `--delivery`, which refuses a dirty checkout or a changed commit;
local developer runs record working-tree edits instead of claiming commit-only proof.
For a targeted developer rerun, combine `--native-only` with
`--native-test <exact name from native-tests.json>`; this cannot satisfy delivery.

Diagnostics live in `target/qa-artifacts`: the selected name, exit status,
timeout state and exact source commit/version accompany logs. Native tests print
their synthetic fixture/geometry evidence into those logs. Native tests receive
`AGENT_TERMINAL_QA_ARTIFACTS`; the UX matrix saves snapshots before geometry
assertions in `screenshots/`. Capture failure cannot hide assertion failures. CI uploads
the directory even on failure. No user history or tokens are copied.

CI on master, beta branches, PRs and manual runs calls `quality-gate.yml`.
Release invokes that same workflow on its own checkout; builds depend on it,
and Publish additionally requires its tested commit to equal the release SHA.
The beta task in debian-maintainer runs the checked-out script before packaging.
Its source config must be deployed through the normal pipeline before the live
mounted config adopts this gate; editing the file does not deploy it.

Release packages carry `<package>.identity.json` with their checksum, source
commit and Cargo version. The opt-in **packaged identity and startup smoke**
workflow takes a successful Release workflow run ID and exact commit, downloads
its named `package-deb` artifact, checks provenance and checksum, installs only
on a disposable Ubuntu 24.04 hosted runner, and checks the installed executable
against the package bytes and compiled startup version. The process gets a
temporary home and no tokens. Existing release install-smoke jobs still run.
**Packaged widget/provider UX remains a gap:** there is no shipped QA entry point;
this workflow proves package identity and startup, while source native tests
prove the synthetic UX scenarios. It must not be described as deployed UX proof.

The separate opt-in **Codex Linux SSH compatibility** workflow requires an exact
Codex npm version. Locally, use `python3 tests/qa/codex_ssh_probe.py`. It invokes
`codex sandbox linux -- <command>` directly, following the [official CLI
reference](https://learn.chatgpt.com/docs/developer-commands?surface=cli), and
compares a synthetic, user-owned mode-0600 SSH config's ownership and `ssh -G`
parse inside/outside that sandbox. No SSH connection, assistant turn, credentials
or model inference occurs. A failed or unavailable runner is a failed probe;
ordinary CI does not require Codex. The comparison represents an approved direct
command, without changing application or global security settings.

Rollback: revert the CI/QA source changes and beta configuration together through
their repositories' normal review/deployment process. Do not deploy or install
anything on a daily-driver host to test these scripts. Retain a release blocker
until equivalent native and delivery gating is restored.
