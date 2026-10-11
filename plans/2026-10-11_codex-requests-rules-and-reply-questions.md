# Codex requests, Codex rules, and questions in replies

Status: ACTIVE · 2026-10-11 · ships in the combined 3.3.0 release (tag held until merged)

## Goal

Close the Codex gaps left by `2026-10-10_seamless-cross-agent-experience.md`, and add the
question polyfill (Part II only of `2026-10-09_agent-terminal-mcp-multiplexer-and-question-polyfill.md`).
Experience and code quality come first: every new surface must read clearly, answer honestly,
fail closed, and be covered by tests that fail on the old behaviour.

1. **Codex permission requests** (`item/permissions/requestApproval`): today refused with a notice.
2. **Codex MCP forms** (`mcpServer/elicitation/request`): today refused with a notice.
3. **Codex rule import and export** (`~/.codex/rules/*.rules`): no Codex source or target today.
4. **Codex file creations titled `Edit`**: a real bug (every `fileChange` is `ToolKind::Edit`).
5. **Literal `**` in Codex reasoning summaries**: fixed by `2706eed4`; needs one live Codex turn.
6. **Question polyfill**: a question with choices at the end of a finished reply becomes an answer card.

## User decisions (2026-10-11)

- Codex export writes its own file, `~/.codex/rules/agent-terminal.rules` (never `default.rules`).
- A permission request's Allow for session is Codex's own session grant (`scope: "session"`,
  nothing on disk). Allow once is `scope: "turn"`. No Always allow.
- A link form shows the server, its message and the full link, with Open in Browser
  (http/https only), then Done or Decline. Nothing opens automatically.
- The polyfill is included; the 3.3.0 tag waits for it.

## Verified facts (codex-cli 0.161.0, `codex app-server generate-json-schema`)

- `PermissionsRequestApprovalParams`: `threadId`, `turnId`, `itemId`, `cwd`, `reason?`,
  `startedAtMs`, `environmentId?`, `permissions: { fileSystem?: { read?: [path], write?: [path],
  entries?: [{ access: read|write|deny, path: {type: path|glob_pattern|special, ...} }],
  globScanMaxDepth? }, network?: { enabled? } }`.
- `PermissionsRequestApprovalResponse`: `{ permissions: GrantedPermissionProfile (same shape),
  scope: "turn"|"session" (default turn), strictAutoReview? }`. There is no deny value: a refusal
  grants nothing (`permissions: {}`).
- `McpServerElicitationRequestParams`: `serverName`, `threadId`, `turnId?`, and one of
  `mode: "form"` + `requestedSchema` (MCP 2025-11-25 primitive schema: string with format/min/max
  length, number/integer with min/max, boolean, single and multi-select enums titled or not),
  `mode: "openai/form" | "openaiForm"` (an opaque schema), or `mode: "url"` + `url` + `elicitationId`.
  Each carries `message`.
- `McpServerElicitationRequestResponse`: `{ action: accept|decline|cancel, content? }`.
- Codex rules: Starlark `prefix_rule(pattern=[...], decision="allow"|"prompt"|"forbidden",
  justification=..., match=[...], not_match=[...])`; a pattern position may be a list of
  alternatives. Codex loads every `.rules` file under `rules/` beside each config layer
  (user layer `$CODEX_HOME/rules`, default `~/.codex/rules`). Rules are prefix matches: there is
  no exact-match form. Source: developers.openai.com/codex/rules.

## Wave 0: the contract (on master before builders start)

- `agent_core::form`: `Form { Fields{fields} | Link{url} }`, `FormField`, `FieldInput { Text |
  Number | Boolean | Choice | Unknown }`, `ChoiceOption{value,label}`, `TextFormat`,
  `FormAction { Accept | Decline | Cancel }`, `validate(fields, content) -> Result<(), Vec<FieldError>>`
  (shared by card and adapter), `openable_link(url)` (http/https, host, no credentials, printable
  ASCII only).
- `Event::FormRequested { server, message, form }` (`Envelope::request` = the answer id),
  `Event::FormResolved { action }`.
- `Command::SubmitForm { request, action, content }`.
- `Event::QuestionRequested { questions, source: QuestionSource }`, `QuestionSource { Reply | Agent }`
  (default and `other` = `Agent`).

## Lanes (worktrees, parallel; the orchestrator owns master, merges and commits)

### L1 Codex requests (agent-core codex.rs, session.rs plumbing, view cards)

Permission request:
- Adapter maps it to `Event::ApprovalRequested { tool: "codex_permissions", title, input, reason,
  options: [Allow, AllowForSession, Deny, Cancel], remembers: None }`. `input` is a normalised,
  display-ready summary: `{ "cwd", "read": [paths], "write": [paths], "deny": [paths],
  "globs": [...], "network": bool|null, "requested": <the raw profile> }`.
- Answers: Allow → `{permissions: requested, scope: "turn"}`; AllowForSession → `scope: "session"`;
  Deny → `{permissions: {}}`; Cancel (Stop turn) → `{permissions: {}}` then `turn/interrupt`, like
  the command approval's Stop turn. Grant exactly what was asked, never more.
- Never auto-answered by app rules and never creates an app rule (session or always). The card has
  no Always allow. The card reads as a sentence: "Codex wants to write outside the sandbox" with
  the paths listed one per line, network named plainly, the reason shown, and a dim note that
  Allow for session lasts until this Codex session ends.
- Special paths read as words: `project_roots` "the project", `tmpdir` "the temp folder",
  `slash_tmp` "/tmp", `root` "the whole filesystem" (shown as a warning), `minimal` "system
  libraries". Glob entries show their pattern. Anything unrecognised shows as raw JSON under
  Details and the request is still answerable.

Form request:
- `mode: "form"` → `Event::FormRequested` with `Form::Fields` mapped from the schema (titled
  options keep `const`/`title`; legacy `enumNames` pair by index; a required list marks fields).
  A schema the mapping cannot express fully (an unknown primitive) still renders, with that field
  as `Unknown`; Submit stays disabled if it is required.
- `mode: "url"` with an `openable_link` → `Form::Link`. A link that fails `openable_link` is
  declined with a notice naming the server and why ("the link is not a plain web address").
- `mode: "openai/form" | "openaiForm"` → declined with a notice ("…asked for a form agent-terminal
  cannot show yet; it was declined"). Declined, not errored: the server and turn carry on.
- `Command::SubmitForm`: Accept re-validates with `form::validate` (an invalid answer is
  `AdapterError::Invalid`, never sent) and sends `{action: "accept", content}`; a Link's Accept
  sends `{action: "accept"}`; Decline `{action: "decline"}`; Cancel `{action: "cancel"}` then
  `turn/interrupt`. Emits `FormResolved`. Stale / settled requests follow the approval rules
  already in codex.rs (`settled_requests`, `stale_explicit_turn`, expiry on exit).
- View: a form card in the interruption shelf (same frame and keyboard rules as the question
  card). Text → `Entry` (email/uri/date hints as placeholders), Number → `SpinButton` honouring
  integer/min/max, Boolean → `Switch`, single Choice → radio-style toggle group (dropdown when
  more than 6), multi Choice → check buttons. Defaults prefilled. Errors shown under each field as
  the user leaves it, using `form::validate`; Submit disabled until valid. Buttons: Submit,
  Decline, Stop turn. The server name is always visible ("From the <server> tool server").
  Link card: message, the full link in a selectable monospace label, Open in Browser
  (`gtk::UriLauncher`, only after `openable_link`), Done, Decline, Stop turn.
- Masking: message, field titles/descriptions and the link go through `masked_for_view`; the
  typed answer is never stored (only `FormResolved{action}` is).
- caps: Codex gains the form capability as a control; Claude and agy explain its absence.

### L2 Codex rules import and export (always_allow.rs, agents_prefs.rs)

- Import: every `*.rules` file in `$CODEX_HOME/rules` (default `~/.codex/rules`). A small,
  fail-closed reader for `prefix_rule(...)` calls only: string tokens and lists of string
  alternatives (expanded, capped at 16 combinations per rule), `decision` (missing = allow).
  Only `allow` imports. Anything else (comments are skipped; any other call, variable,
  concatenation, unbalanced bracket) skips that rule, and the import toast counts skipped rules.
  A Codex prefix rule is a prefix match, so it becomes the app rule `<tokens> *`, and must pass
  `check(Scope::Always)` like every import; most will be skipped as too broad, and the toast
  says so.
- Export: only global shell rules that are already prefix rules (`... *`) and pass the checks,
  written to `$CODEX_HOME/rules/agent-terminal.rules`: a generated header ("Written by
  agent-terminal. Edits here are replaced on the next export."), one `prefix_rule(pattern=[...],
  decision="allow", justification="agent-terminal")` per rule, tokens escaped as Starlark string
  literals. Exact rules cannot be expressed (Codex has no exact match; exporting one would widen
  it) and are skipped; the toast and the export warning say how many. Atomic write under the
  existing lock; the file is created `0600`; the directory `0700` only if created.
- The export warning gains: "Codex follows these rules outside agent-terminal too."
- Settings → Agents shows Codex as a source and target when its rules folder exists, and View
  Differences lists Codex differences like the others. `permission_sources` is unchanged.
- The reader never executes anything; it parses text.

### L3 Question polyfill (agent-core question_extract.rs, session.rs, view, config)

- `agent_core::question_extract::extract(text: &str) -> Option<Question>` over pulldown-cmark
  (move `pulldown-cmark` to a workspace dependency; agent-core stays pure). Conservative: the
  reply's last block is a list of 2–6 items, directly preceded or followed by a paragraph ending
  in `?`; items are single-line, at most 120 characters, no code spans that look like commands,
  no nested lists, not inside a code block, quote or table; a list of steps ("1. Run …" imperative
  procedure) is not a choice. A false negative costs nothing (the user types); a false positive is
  noise, so precision wins. A labelled corpus of true and false cases (at least 12 true, 24 false,
  from real Claude, Codex and agy reply shapes) is the test.
- Where: Codex and agy threads only (Claude asks with its own tool). Only when a turn completed
  normally, the agent raised no native question in it, and the user has not typed since.
- Event: `QuestionRequested { source: Reply }` with request id `reply:<assistant item id>`; the
  card is labelled "Suggested from the reply" and offers the options plus "Type your own answer"
  (focuses the composer). Choosing sends the option label as the next prompt (exactly the label,
  which the user can see). Typing in the composer, sending any prompt, or a new turn withdraws it
  (`QuestionResolved { answered: false }`). It is never persisted as pending: on reopen it is not
  re-offered.
- Settings → General: "Offer answer cards for questions in replies" (default on), applied on change.

### L4 Codex `Create` title (orchestrator)

- A Codex `fileChange` whose every change is `kind.type == "add"` is titled `Create <path>`;
  mixed changes stay `Edit`. Test with W0 frames plus a synthetic add.

### L5 Live check (orchestrator, after merge)

- `live_smoke.py --agent codex` plus one Codex turn asking for a bold-labelled summary to confirm
  `**` renders bold. Permission and form requests are checked with recorded frames (they cannot be
  triggered on demand); if a live trigger is found, record it as a fixture.

## Binding addendum (reviews of 2026-10-11, Hive #31; overrides the lane text above)

Contract changes already made (wave 0): `answer()` replaces `validate()` and returns exactly what
is sent (nulls and blanks dropped; a blank never satisfies required); `check_form()` (unique
names, real options); `Form::Unknown`; `FormField::label()` humanises keys; `link_host()` (refuses
`\`, credentials, non-ASCII) and `link_concern()` (local, bare IP, plain http: warn, not refuse);
`asks_for_secret()`.

L1, permissions (red-team #364, #365, #371; UX2, UX4, UX5, UX12):
- The grant is built from the card's typed model, never by echoing `requested`. The adapter
  parses the request into typed entries: absolute path (relative ones resolved against the
  request's `cwd` and shown resolved), glob, special (with its `subpath` shown and resolved),
  network. Any entry it cannot type exactly (`special.kind: unknown`, a path with control,
  bidi or zero-width characters, an unknown access mode, unknown fields) makes the request
  unrenderable: the card shows the raw request expanded and offers only Deny and Stop turn.
- Title composed from what is asked ("Codex asks to write to 2 folders and use the network";
  never "write" for a read-only or network-only request; no "sandbox"). Entries one per line,
  capped with Show all; `deny` entries under "Still blocked". Outcome lines: "Allowed for this
  turn", "Allowed for this Codex session", "Denied".
- Allow (this turn) only, with a plain warning, when any write or read reaches: the filesystem
  root, the home folder itself, a glob containing `**` at or above home, `~/.codex` or
  `$CODEX_HOME`, or any path `always_allow`'s sensitive list names (dotfiles, `.git`, agent
  instruction files). Allow for session is not offered then.
- After a session grant, a notice says what was granted, and the header shows an "Extra access"
  chip (tooltip lists it) until the Codex process restarts. A mode switch while a session grant
  is live posts a notice that the grant still applies until the thread restarts.
- The shelf shows no one-click Allow for `codex_permissions`, only "Review" (scrolls to the
  card). `dialog_prefill` is empty for it and Allow for session never opens the rule dialog.
- `strictAutoReview` is not sent (its behaviour is unverified).

L1, forms (red-team #367, #368, #372; UX1, UX3, UX6–UX9, UX12):
- A form is a `PendingInterruption::Form`: shelf entry, sidebar NeedsAnswer, notification,
  composer hint, like a question.
- The adapter declines (with a notice naming the server and the reason) a form that fails
  `check_form`, has a field `asks_for_secret` matches ("asks for a password or other secret;
  agent-terminal never passes secrets to tool servers"), exceeds 20 fields, 50 options per
  field, or a 4 000-character message, or a link that fails `link_host` or that
  `masked_for_view` would change. The notice for a refused link shows it as selectable, masked
  text and says what to do (open it yourself if you trust it, then tell the agent).
- Fields represent "not answered": text and numbers are `Entry` (numbers validated, never a
  SpinButton default of 0), a boolean is a Yes/No toggle pair with neither picked unless a
  default exists, a single choice has no pick unless a default exists and an optional one can be
  cleared, multi choice is check buttons with a "Choose 1 to 3" hint. Only touched or defaulted
  fields are sent.
- Required fields carry "Required"; Submit is disabled with a one-line reason ("Fill in Repo
  name") until `answer()` passes; field errors show when a field loses focus or on a Submit
  attempt.
- The server's message is a quote under a "<server> tool server" chip, never the app's or
  Codex's voice. Under the buttons: "Sent to <server>."
- Link card: the message, "Opens <host>" on its own line, `link_concern` as a warning when
  present, the full link selectable in monospace. Open in Browser is the suggested button until
  used, then Done is; after opening, "Opened in your browser. Choose Done when you have
  finished."; a launcher failure says so and leaves Copy Link. Buttons: Open in Browser, Done,
  Decline, Stop turn (buttons wrap).
- Keyboard: no focus stealing when the card appears; Tab order follows the fields then the
  buttons; Enter in the last field submits only when valid; Escape does nothing; every control
  has an accessible name.
- A form with no `turnId` is answered without touching the active turn: its Stop turn becomes
  "Dismiss" (cancel only).

L2, Codex rules (red-team #363, #366, #370, #372):
- Export only global prefix rules whose program has no entry in `EXEC_LONG`, `EXEC_SHORT` or
  `FIND_ACTIONS` (so `find`, `rg`, `jq`, `git log`, `git show`, `tar`, … never export), tokens
  printable ASCII only, escaped as Starlark strings by a dedicated function (never `{:?}`).
- Before the atomic rename, if a `codex` binary is found, `codex execpolicy check --rules <tmp>
  -- true` must parse it; a failure keeps the old file and reports the error. A gate test
  exercises the generator against the real parser when `codex` is installed (skips otherwise).
- An export with no eligible rules rewrites the file to its header only.
- The Codex home is resolved through the Codex profile's environment (`AgentEnv`), not the GUI's
  own `$CODEX_HOME`; Settings and the toast show the resolved folder.
- The toast counts: exported, skipped as exact rules, skipped as able to run other programs.

L3, polyfill (UX1, UX10, UX11; red-team #369):
- A reply question is not an interruption: no shelf, no NeedsAnswer, no composer hint. It
  renders inline under the reply as muted chips, with "Dismiss" and "Don't suggest these".
- A chip click puts its text in the composer and focuses it; nothing is sent until the user
  presses Enter. No "Type your own answer" state is needed.
- Option text is built from the rendered plain text of the item (markdown stripped). An item
  holding a link, inline HTML, an image, or any invisible, zero-width, bidi or tag character
  disqualifies the whole list (no chips offered).
- Setting: "Suggest quick replies when a reply ends with a question" (General, default on).
  One debug log line per offer decision (offered, or why not).

## Proof and gates

- Each lane: unit tests that fail on the old behaviour, fmt, clippy, workspace tests under xvfb,
  `python3 tests/qa/gate.py` before merge; rust-reviewer after every Rust change.
- Security: L1 (link opening, permission grants) and L2 (writing Codex's rules, parsing Starlark)
  get a fresh red-team plus both peer harnesses (`cross-agent codex`, `cross-agent agy`).
- Experience: an experience-designer review of the three new cards with `preview_app` screenshots.

## Rollback

Each lane is its own commit range on master; `git revert` per lane. The polyfill has a Settings
switch. Codex export writes only its own file; deleting `~/.codex/rules/agent-terminal.rules`
undoes it.

## Log

- 2026-10-11: Wave 0 contract written (`form.rs`, `FormRequested`/`FormResolved`, `SubmitForm`,
  `QuestionSource`); workspace builds, clippy clean, agent-core tests pass.
