# Security policy

## Supported versions

Only the latest release (and `master`) receives security fixes.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/jean1880/agent-terminal/security/advisories/new),
not in a public issue. Only the maintainers see the report.

Include the version, the steps to reproduce and the impact you expect. Leave out real secrets,
tokens and hostnames: a placeholder shows the problem just as well.

You should get an acknowledgement within a week. Once a fix is released, the advisory is published
with credit to the reporter unless you prefer otherwise.

## Scope

Agent Terminal drives local agent CLIs and keeps a local thread store. Of particular interest:

- secrets reaching the store, a hand-off brief or an injected prompt without redaction;
- an agent gaining approval or permissions it was not granted (the agy approval hook, Codex
  permission requests);
- undo, checkpoint or worktree operations touching files or git state outside what they pinned;
- anything that makes the app listen on a network port.

Vulnerabilities in the agent CLIs themselves (Claude Code, agy, Codex) belong with their vendors.
