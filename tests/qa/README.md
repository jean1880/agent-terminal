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

Native cases run the built test binary directly, so `--jobs N` can run them side by
side; each still has its own display (`xvfb-run -a -n` starting points are spaced
apart), D-Bus session and home. Every `#[ignore]`d test must either be listed in
`native-tests.json` or appear, with its reason, in `native-excluded.json`; the
`ignored-inventory` step fails on any that are in neither, and on exclusions that no
longer name an ignored test. Without this, an `#[ignore]` quietly drops a test from
every run.

## Running only what a change needs

`python3 tests/qa/affected.py` (add `--dry-run` to only print the plan) compares against
the merge base with `origin/master`, including uncommitted edits, and runs:
- the packages the change touches, plus the ones that depend on them (agent-core →
  agent-kit → app);
- the native cases selected by the area rules in `native-areas.json` (first matching
  prefix wins; a Rust or asset path that no rule matches selects every case);
- the QA scripts' own unit tests, when `tests/qa/` changed.

Docs and workflow edits select nothing, and anything it does not recognize selects
everything. It is a developer shortcut only: CI and delivery always run the whole gate.

## Coverage

`python3 tests/qa/coverage.py` (needs `cargo install --locked cargo-llvm-cov` and
`rustup component add llvm-tools-preview`) runs the workspace tests and every native
case under instrumentation, and reports **production** line coverage. Lines inside
`#[cfg(test)]` items, and files declared only under `#[cfg(test)]`, are not counted, so
tests cannot raise the figure by covering their own code. It fails when the figure drops
below `lines` in `coverage-floor.json`; `--update-floor` raises the floor after an
improvement, so coverage can ratchet up toward `target` but never quietly slip. `--html`
writes a browsable report and `--skip-native` gives a faster run whose (lower) figure is
not checked against the floor. **A release always takes the full run:** `release.yml`
calls `coverage.yml` (the whole workspace and every native case, never `--skip-native`
or an affected subset), and Publish requires the commit it covered to equal the release
SHA, as it already does for the gate. Output: `target/qa-artifacts/coverage/summary.json`
(per crate and per file, worst first) and `lcov.info`.

## Mutation testing

Coverage says a line ran; it cannot say a test checked it. `cargo mutants` (config in
`.cargo/mutants.toml`) changes `agent-core` and `agent-kit` one small edit at a time (a
`<` to `<=`, a return value replaced, a match arm deleted) and runs the tests. A mutant
every test still passes is listed in `mutants.out/missed.txt`: code that runs but that
nothing verifies. Fix it with a test that asserts the behaviour, never by excluding it.

**Always cap its memory.** A mutant can turn a bounded loop into an unbounded one; one
grew to 30 GB and took a workstation down. Locally:

```
systemd-run --user --scope --quiet -p MemoryMax=16G -p MemorySwapMax=0 \
    cargo mutants --package agent-core --jobs 3 --output target/mutants-core
```

Scope a run to what changed with `--in-diff <(git diff origin/master)` or `--file <path>`.
In CI, `mutants.yml` runs weekly (and by hand) under `ulimit -v`, and lists missed mutants
in the run summary; it reports, it does not gate.

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

## Recording real protocol frames

`tests/qa/record_frames.py` (stdlib only) spawns `claude` (stream-json with the
stdio permission prompt, the argv agent-terminal uses) or `codex app-server`
(JSON-RPC) in a **throwaway sandbox repo**, sends each `--prompt` as one real
turn, answers approval requests from the ordered `--decision` list, and writes
every raw line in both directions to a JSONL file (`dir` is `in` to the agent,
`out` from it). Secret-looking text (`sk-…`, `ghp_…`, `AIza…`, `Bearer …`) is
masked before it is written. Each prompt spends real quota, so keep runs small;
save results under `tests/fixtures/real/<agent>/<scenario>.jsonl`.

```
python3 tests/qa/record_frames.py codex --cwd /tmp/sandbox --out /tmp/ask-ls.jsonl \
    --approval-policy untrusted --sandbox readOnly --prompt "Run ls -la" --decision decline
python3 tests/qa/record_frames.py claude --cwd /tmp/sandbox --out /tmp/touch.jsonl \
    --thinking-display summarized --prompt "Run: touch x.tmp" --decision deny
```

Never point it at a real project, and re-grep the output for your canary before
committing a fixture.

## Live contract smoke (opt-in, spends quota)

`tests/qa/live_smoke.py` (stdlib only) drives the real CLI and asserts contract
facts only. It reuses the recorder's spawning, framing and scrub, runs in a
throwaway sandbox under the temp dir (the cwd is passed explicitly and asserted;
`$HOME` is refused), and exits non-zero with one `PASS`/`FAIL` line per check.

```
python3 tests/qa/live_smoke.py --agent claude --dry-run   # prints commands and turn plan, runs nothing
python3 tests/qa/live_smoke.py --agent codex
python3 tests/qa/live_smoke.py --agent agy
```

**Quota warning: each run spends real turns (claude 2, codex 2, agy 1). Get the
user's approval per run.** Never run it in CI. Always `--dry-run` first.

- claude: `--thinking-display summarized` starts and yields thinking; a `can_use_tool`
  for `touch x.tmp` arrives and a deny lets the turn continue.
- codex: a shell `requestApproval` carries a wrapped command and `commandActions`; an
  unoffered `decline` is accepted (item `declined`, turn continues); on `untrusted` +
  `workspaceWrite` an edit asks and `accept` applies it.
- agy: tool `step_update` events carry `tool_name` and `tool_info.parameters`; the
  reported cwd is the sandbox.

`--keep` retains the sandbox and the scrubbed frames. The check functions are proven
against the W0 fixtures without quota: `python3 -m unittest tests/qa/test_live_smoke.py`.

Rollback: revert the CI/QA source changes and beta configuration together through
their repositories' normal review/deployment process. Do not deploy or install
anything on a daily-driver host to test these scripts. Retain a release blocker
until equivalent native and delivery gating is restored.
