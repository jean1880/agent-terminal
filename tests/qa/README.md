# Testing

Everything runs through the Makefile, with standard tools: `cargo nextest` for the tests and
`cargo llvm-cov` for coverage (`make test-deps` installs both). Nothing here is run by hand.

| Command | What it runs | Where it runs |
|---|---|---|
| `make test` | The suite, each test in its own process, in parallel (about 35 s) | You, while working |
| `make check` | fmt, Clippy, the no-default-features build, the suite, the display tests, and these scripts' unit tests | CI on every push and PR (`gate / check`, required), and a release |
| `make coverage` | The suite and the display tests instrumented; HTML and lcov in `target/qa-artifacts/coverage`; fails under `COVERAGE_FLOOR` (Makefile) | CI on every PR; a release requires it on the release commit |
| `make test-one T=<name>` | The tests whose names contain `<name>`, display tests included, one at a time | You, rerunning one test |
| `make test-stress T=<name> [N=30]` | The tests whose names contain `<name>`, `N` times over, no retries, failures summarised at the end. Tests in each pass run in parallel, as in `make test`, so they load each other | You, proving a flaky test fixed (a fix needs a clean stress run, not one green CI) |
| `make bench T=<name>` | The benchmarks and measurements whose names contain `<name>`, optimised build (`measure` profile: release without LTO), output shown, the `default-filter` exclusions included, one at a time | You, measuring a cost (`resize_cost`, `append_bench`, `stream_cost`) |
| `make test-gdb T=<name>` | The test whose name contains `<name>` under gdb in batch mode: on a crash, the backtrace, the faulting instructions and the library (or JIT code) holding them, with the GTK renderer named | CI, when a gate or coverage run fails; you, when a test dies by a signal with no panic text |
| `make hooks` | Opt in to a pre-commit hook: fmt and Clippy only | Your clone |

Every run gets a private Xvfb display and D-Bus session (`xvfb-run`, `dbus-run-session`),
private XDG directories under `target/test-home` (emptied before every run, since CI caches
`target/`), in-memory GSettings and no global or
system git config, so no test touches your settings, thread store or keyring.

**Display tests** present windows and measure layout. They are `#[ignore]`d so `make test`
stays fast, and `make check` runs all of them (`--run-ignored only`) one at a time, so none
is measured while another loads the machine. A new `#[ignore]`d test is picked up
automatically; the only ones never run are named in `.config/nextest.toml`'s
`default-filter`, each with its reason (network, a benchmark, a measurement). Screenshots the
layout tests take land in `target/qa-artifacts/screenshots`, which CI uploads.

**Coverage** counts every line llvm-cov instruments. `COVERAGE_FLOOR` only goes up: raise it
when coverage rises, never lower it to get a change through.

**Releases**: `release.yml` runs `make check` and `make coverage` on its own checkout, and
Publish requires both to have passed on exactly the release commit, with nothing edited or
generated in the tree during the run. The debian-maintainer beta task runs `make check` on
the checkout it packages.

## Environment variables

You do not need to set any of these: every `make` test target sets the first group itself
(`DISPLAY_ENV` in the Makefile). To change one for `make test-one` or `make bench`, copy
`.env.example` to `.env` (git-ignored) and uncomment the line; `.env` is read on top of the
defaults, and only by those two (never `make test-stress`, which must run as CI does). Its format: `KEY=VALUE` per line, no quotes, no spaces in values, `#`
comments, absolute paths (no `~` or `$HOME`).

**Set by the Makefile for every test run**

| Variable | Value | Why |
|---|---|---|
| `XDG_CONFIG_HOME` | `target/test-home/config` | `config.json` is read and written here, never in your `~/.config` |
| `XDG_STATE_HOME` | `target/test-home/state` | The thread store, hand-off briefs and approval rules: never your real ones |
| `XDG_CACHE_HOME` | `target/test-home/cache` | The model catalogue cache |
| `XDG_DATA_HOME` | `target/test-home/data` | GLib and GTK's own user data |
| `GSETTINGS_BACKEND` | `memory` | Nothing is written to your dconf database |
| `GIT_CONFIG_NOSYSTEM` | `1` | Repository tests ignore the system git config… |
| `GIT_CONFIG_GLOBAL` | `/dev/null` | …and yours, so they behave as on CI (which has no git identity) |
| `GDK_BACKEND` | `x11` | The tests draw on the private Xvfb display `xvfb-run` starts |
| `GTK_A11Y` | `test` | GTK's test accessibility backend: no AT-SPI bus needed |
| `LANG` | `C.UTF-8` | Text measurement and sorting do not depend on your locale |
| `AGENT_TERMINAL_QA_ARTIFACTS` | `target/qa-artifacts/screenshots` | Where layout tests save screenshots; unset, they take none |

**Opt-in**

| Variable | Effect |
|---|---|
| `RUST_LOG` | Log filter for the app's output in a failing test's stderr (`debug`, `agent_terminal::window=debug`, …) |
| `AGENT_TERMINAL_BENCH_DB` | A scratch **copy** of a real `threads.db` for the store's append benchmark (`make bench T=append_bench`). Never your live store |

**Make variables** (on the command line: `make test NEXTEST_PROFILE=ci`)

| Variable | Effect |
|---|---|
| `T` | `make test-one`, `make test-stress`, `make bench` and `make test-gdb`: the test name filter (required) |
| `N` | `make test-stress`: how many times to run (default `STRESS_COUNT`, 30) |
| `ARGS` | Extra `cargo nextest run` flags for `test`, `test-display`, `test-one`, `test-stress` and `bench` (`ARGS=--no-capture`, `ARGS='--retries 2'`); not `coverage` |
| `BENCH_PROFILE` | `make bench`'s cargo profile: `measure` (default, release without LTO), `release` (exactly what ships, slow to build) or `dev` |
| `NEXTEST_PROFILE` | The `.config/nextest.toml` profile (`default`; CI uses `ci`) |
| `COVERAGE_FLOOR` | `make coverage`'s minimum line coverage |

**Never set these yourself.** The app sets them for the agent processes it starts, and tests
check that it does: `AGENT_TERMINAL_APPROVAL_SOCKET` (agy's approval hook), `AGENT_TERMINAL_HOOK_BIN`
(set from the running binary), `AGENT_TERMINAL_DELEGATION_ENDPOINT`, `_INSTANCE`, `_CAP` and
`_APP_PID` (delegated workers), and `AGENT_TERMINAL_RESTART_FIXTURE` (a test re-running itself as
a child process).

The app's own runtime variables (demo mode, logging, where it keeps its files) are in the main
README, under "Environment variables".

## Mutation testing

Coverage says a line ran; it cannot say a test checked it. `cargo mutants` (config in
`.cargo/mutants.toml`) changes `agent-core` and `agent-kit` one small edit at a time and
runs the tests; a mutant every test still passes is code nothing verifies. Fix it with a
test that asserts the behaviour.

**CI only. Never run it on a workstation.** By design some mutants turn a bounded loop into
one that allocates until it is killed; a run here once crashed a desktop. `mutants.yml` runs
it weekly, and by hand (Actions → Mutation testing → Run workflow), on GitHub's disposable
runners under `ulimit -v`, and lists missed mutants in the run summary. It reports; it does
not gate.

## Packaged builds and probes

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
