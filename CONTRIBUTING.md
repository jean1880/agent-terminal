# Contributing to Agent Terminal

Thanks for taking the time to help. Bug reports, ideas, documentation fixes and code are all
welcome. This guide says how to get a change from an idea to a merged pull request.

Everyone taking part is expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Contents

- [Ways to help](#ways-to-help)
- [Reporting a bug](#reporting-a-bug)
- [Suggesting a feature](#suggesting-a-feature)
- [Before you write code](#before-you-write-code)
- [Setting up](#setting-up)
- [Making a change](#making-a-change)
- [Testing](#testing)
- [Commits](#commits)
- [Pull requests](#pull-requests)
- [AI-assisted contributions](#ai-assisted-contributions)
- [Licence](#licence)

## Ways to help

- **Report a bug** that you can reproduce, with the details asked for in the bug form.
- **Confirm or narrow down** an open bug: a second distribution, desktop or agent version helps.
- **Improve the documentation**: the [README](README.md), the
  [delegation guide](docs/delegation/user-guide.md) and the troubleshooting section all take
  pull requests.
- **Pick up an issue** labelled
  [`good first issue`](https://github.com/jean1880/agent-terminal/labels/good%20first%20issue)
  or [`help wanted`](https://github.com/jean1880/agent-terminal/labels/help%20wanted). Say in
  the issue that you're taking it, so two people don't do the same work.

**Security problems are not reported in issues.** Follow [SECURITY.md](SECURITY.md) and use
GitHub's private vulnerability reporting.

## Reporting a bug

[Open a bug report](https://github.com/jean1880/agent-terminal/issues/new?template=bug_report.yml).
Before you do:

1. **Search the [open and closed issues](https://github.com/jean1880/agent-terminal/issues?q=is%3Aissue)**.
   If it's already there, add a 👍 reaction and any new detail rather than a "me too".
2. **Try the latest release.** Fixes land on `master` and ship in the next release, so a bug
   already fixed there won't be fixed in an older one.
3. **Check whether the agent CLI is at fault.** Run the same request in `claude`, `agy` or
   `codex` directly. If it fails there too, report it to that CLI's vendor.
4. **Collect the log**, from the journal or from a terminal:

   ```bash
   journalctl --user -t agent-terminal -b --since "15 min ago"
   RUST_LOG=debug agent-terminal
   ```

**Review everything you paste for secrets.** Logs, settings and transcripts can hold API keys,
tokens, hostnames and file paths. Agent Terminal masks the secrets it recognises before it
stores them, but not every log line passes through that masking, and the masking only catches
known formats. Replace anything sensitive with `****` before you post.

## Suggesting a feature

[Open a feature request](https://github.com/jean1880/agent-terminal/issues/new?template=feature_request.yml)
and start from the problem you have, not the solution you picture. "I lose track of which
thread is waiting for me" leads to a better design than "add a red dot".

Some requests are better as configuration than as code. Agent Terminal ships no knowledge of
any one machine, so something that reads a particular file or runs a particular command
usually belongs in a [status indicator](README.md#status-indicators) or a
[profile](README.md#profiles) instead.

## Before you write code

**Open an issue first for anything bigger than a small fix**: a new feature, a new setting,
a change of behaviour, a new dependency or a refactor. A short discussion up front avoids a
pull request that cannot be merged as written. Typo fixes, small documentation fixes and
obvious bug fixes can go straight to a pull request.

Then read [AGENTS.md](AGENTS.md). Despite the name, it is the project's architecture guide for
people as much as for coding agents, and reviews hold changes to it. The rules that most often
decide a review:

- **One standalone binary.** Assets are embedded and icons are bundled. No new runtime
  dependencies, and the minimum GTK, libadwaita and VTE versions (the Cargo version features)
  are not raised as a side effect.
- **The main thread never blocks.** Anything that runs a process, reads a large file or touches
  the store goes off the main thread (`gio::spawn_blocking`, the store writer).
- **No new network traffic.** The update check is the app's only request, and a new one won't be
  accepted.
- **Everything persisted or injected is redacted** through `agent_core::redact`. A new store
  write, hand-off or injected prompt must take the same path.
- **No `.unwrap()` or `.expect()` on runtime paths.** Show the failure to the user instead.
- **The user's git state is never touched.** Checkpoints, undo and worktrees have strict
  rules, and tests that prove each of them.
- **Pure logic goes in the crates.** Code with no GTK and no I/O goes in `crates/agent-core`,
  and GTK-free I/O goes in `crates/agent-kit`, so both can be tested without a display.

## Setting up

You need a current stable Rust (`rust-toolchain.toml` follows stable) and the development
packages listed in [Building from source](README.md#building-from-source). The tests also need
`xvfb-run` and `dbus-run-session` (Debian/Ubuntu: `sudo apt install xvfb dbus-daemon`).

```bash
git clone https://github.com/jean1880/agent-terminal.git
cd agent-terminal
make deps          # Debian/Ubuntu: the development packages
make test-deps     # cargo-nextest and cargo-llvm-cov
make hooks         # optional: fmt and Clippy before each commit
make start-local   # build and run
```

**Run a development build on its own session bus.** Agent Terminal is single-instance. If
the packaged app is already running, a plain `./target/debug/agent-terminal` hands over to it
and exits, so you end up looking at the old build:

```bash
dbus-run-session -- ./target/debug/agent-terminal
```

To keep your real settings and threads out of it, point `XDG_CONFIG_HOME` and `XDG_STATE_HOME`
at a scratch folder. To try the interface with no agent running, use the demo modes in
[Command-line options](README.md#command-line-options).

Don't use `make install` on a machine that has the packaged app installed. It puts a second
copy earlier in your `PATH`, and that copy is then never updated. Run `make uninstall` when
you're done with it.

## Making a change

1. **Branch from an up-to-date `master`**:

   ```bash
   git fetch origin
   git switch -c fix/short-description origin/master
   ```

   Use the commit types below as branch prefixes: `feat/`, `fix/`, `docs/`, `refactor/`, …

2. **Keep the change focused.** One pull request should do one thing. Unrelated clean-ups go in
   their own pull request, which makes both easier to review.
3. **Add or update tests** with the change (see [Testing](#testing)).
4. **Update the documentation** if anything a user sees changes: the README for features,
   settings, shortcuts and options, and the screenshots if the interface they show changes.
5. **Write prose in Canadian English** (colour, behaviour, centre, cancelled, grey), as the
   rest of the documentation is. Code identifiers keep their own spelling.

`docs/` holds only release notes, screenshots and user guides, and `make lint` rejects anything
else there. Design notes, investigations and review write-ups belong in the issue or the pull
request.

## Testing

Everything runs through the Makefile. [tests/qa/README.md](tests/qa/README.md) has the details.

| Command | When |
|---|---|
| `make test` | While you work: the whole suite, in parallel |
| `make test-one T=<name>` | Rerun the tests whose names contain `<name>` |
| `make check` | Before you push: what CI runs (fmt, Clippy, the no-default-features build, the suite, the display tests) |
| `make coverage` | When you add a lot of code: coverage must stay above `COVERAGE_FLOOR` |
| `make test-stress T=<name>` | To show a flaky test is really fixed |

Tests get a private display, D-Bus session, XDG folders and git config, so they never touch
your settings, threads or keyring.

What a change needs:

- **A bug fix** comes with a test that fails without the fix.
- **New logic** comes with unit tests. Put pure logic where it can be tested without GTK.
- **Changes to detection, startup commands, path resolution, config serialisation or
  migration** update the tests beside that code.
- **A new variant** in an array that drives a dropdown (`CliClient::ALL`, `ThemeChoice::ALL`,
  `DiffBase::ALL`, …) keeps its exhaustive-match test.
- **Interface changes** get a screenshot or short recording in the pull request.

Never lower `COVERAGE_FLOOR` to get a change through. Mutation testing (`cargo mutants`) runs
in CI only. Don't run it locally: its mutants can loop and allocate without bound.

## Commits

Commits follow [Conventional Commits](https://www.conventionalcommits.org/):

```text
fix(approvals): a late Always answer with saved rules off allows once
feat(threads): make agent activity and session discovery visible
docs(readme): describe first start and setup troubleshooting
```

- **Types**: `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `style`.
- **Scope**: the area changed, as in recent history: `threads`, `delegation`, `approvals`,
  `setup`, `usage`, `readme`, …
- **Subject**: says what is now true, in lower case, without a full stop.
- **Body**: explains why, when the subject alone doesn't. Mention a breaking change as
  `BREAKING CHANGE:` in the footer.

Commit messages, pull request descriptions and code comments state facts about the code.
Don't point to notes, chats or documents a reader cannot open.

## Pull requests

1. **Run `make check`** and make sure it passes.
2. **Open the pull request against `master`** and fill in the template. Its title follows the
   same Conventional Commits format.
3. **Link the issue** with `Fixes #123`, so it closes on merge.
4. **Required checks**: `gate / check` and `cargo-deny (bans licenses sources)` must pass,
   code scanning (CodeQL) must report no high-severity alerts, and every review conversation
   must be resolved before merging.
5. **Keep it up to date** by rebasing on `master` when it falls behind or conflicts. Don't
   force-push over a review in progress without saying what changed.

A pull request may be closed if it hasn't been discussed and changes behaviour, adds a
dependency, or works against the architecture rules above. That isn't a judgement of the
work, and an issue is the place to find a version that fits.

**Releases** are cut by the maintainer, who writes the release notes in `docs/releases/`. Leave
the version in `Cargo.toml` alone.

## AI-assisted contributions

Agent Terminal is a tool for working with coding agents, and it is built with them. Using AI
in a contribution is fine. The responsibility for it stays with you:

- **Say so.** The pull request template asks which tools you used and for what. Undisclosed
  AI-generated work will be closed.
- **Understand every line.** You must be able to explain what the change does and how it fits
  the rest of the code, without the tool. Reviewers will ask.
- **Edit what you post.** Issues and comments written with AI help must be checked and trimmed
  by you before they're posted. Leave out padding and anything you haven't verified.
- **No AI-generated media.** Icons, images, screenshots and recordings must be your own or the
  app's real output.
- **Run it yourself.** "The agent says the tests pass" is not testing. Show the commands you ran.

## Licence

Agent Terminal is released under the [MIT Licence](LICENSE). By contributing, you agree that
your contribution is licensed under the same terms, and that you have the right to submit it.
Third-party code or assets must have a compatible licence and be recorded in
[THIRD_PARTY.md](THIRD_PARTY.md). `cargo-deny` checks the licences of dependencies.
