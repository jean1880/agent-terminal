<!--
Thanks for contributing. The title follows Conventional Commits, e.g.
"fix(approvals): a late Always answer allows once". See CONTRIBUTING.md.
Delete any section that doesn't apply.
-->

## What and why

<!-- What this changes, and the problem it solves. Link the issue so it closes on merge. -->

Fixes #

## How it works

<!-- The approach, and anything a reviewer should look at closely: a tricky path, a trade-off, an alternative you ruled out. -->

## Testing

<!-- What you ran and what you checked by hand, so a reviewer can do the same. -->

- [ ] `make check` passes
- [ ] Added or updated tests (a bug fix includes a test that fails without it)
- Checked by hand:

## Screenshots

<!-- For any visible change: before and after, or a short recording. -->

## Checklist

- [ ] The change is focused, and anything bigger than a small fix was discussed in an issue first
- [ ] User-facing changes are documented (README, delegation guide, screenshots)
- [ ] No new runtime dependency, network request or blocking call on the main thread
- [ ] Anything persisted, handed off or injected into a prompt goes through `agent_core::redact`
- [ ] No `.unwrap()`/`.expect()` on runtime paths

## AI assistance

<!--
Delete this section if you used no AI tools. Otherwise, name the tools and say what they did
(e.g. "Claude Code wrote the first draft of the parser; I rewrote the error handling and wrote
the tests"). You must understand and be able to explain every line. See CONTRIBUTING.md.
-->
