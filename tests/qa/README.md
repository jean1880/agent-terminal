# Testing

Everything runs through the Makefile, with standard tools: `cargo nextest` for the tests and
`cargo llvm-cov` for coverage (`make test-deps` installs both). Nothing here is run by hand.

| Command | What it runs | Where it runs |
|---|---|---|
| `make test` | The suite, each test in its own process, in parallel (about 35 s) | You, while working |
| `make check` | fmt, Clippy, the no-default-features build, the suite, the display tests, and these scripts' unit tests | CI on every push and PR (`gate / check`, required), and a release |
| `make coverage` | The suite and the display tests instrumented; HTML and lcov in `target/qa-artifacts/coverage`; fails under `COVERAGE_FLOOR` (Makefile) | CI on every PR; a release requires it on the release commit |
| `make hooks` | Opt in to a pre-commit hook: fmt and Clippy only | Your clone |

Every run gets a private Xvfb display and D-Bus session (`xvfb-run`, `dbus-run-session`),
private XDG directories under `target/test-home`, in-memory GSettings and no global or
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
