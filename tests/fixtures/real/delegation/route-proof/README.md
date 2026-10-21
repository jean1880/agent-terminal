# Installed-provider route gate proofs

Recorded 2026-10-15 by `make delegation-proof`. A passing provider proof never clears the
separate application/source-review gate.

- Claude Code 2.1.300: real restricted CLI, deterministic loopback Anthropic model,
  real MCP process, host Bash denial, generated-home canary scan.
- Codex 0.161.0: real app-server, deterministic loopback Responses model, actual
  `additional_tools` inventory and valid collaboration tool execution, real MCP
  startup, generated-home scan and fresh-process resume.
- agy 1.3.5: real cloud model, isolated home/workspace, one existing auth file
  bound read-only by bwrap, actual application approval-hook client and Unix
  fixture approval server. No real configuration was changed.

`codex-results.json` intentionally records `G-X2: false`: native spawning really
executes despite the reported disabled features. Do not interpret the passing
parent-only file as permission to admit Codex children. Other individual gates
pass. Each provider-specific result file is authoritative; `results.json` is
only the last invocation's selection.

The `*.proof.json` files are versioned, structured **gate projections**, not
provider transcripts. Each has `fixture_version`, `provider`, `evidence_kind`
and an explicit `frames` array. Export retains only configuration flags, MCP
startup status, native-worker lifecycle, permission decisions and exact synthetic
attestation inputs. It drops user input, assistant/model prose, stderr, account
information, filesystem paths and HTTP headers. Conversation and native IDs are
consistent fixture pseudonyms. `at` is a relative receipt timestamp with a common
origin across the exported bundle, preserving the attestation deadline evidence.

Private temporary recordings are never copied into the published fixture set.
The fixture model captures tool names and emits its declared deterministic tool
request; these are installed-CLI enforcement proofs, not cloud-model behaviour
claims for Claude or Codex.
