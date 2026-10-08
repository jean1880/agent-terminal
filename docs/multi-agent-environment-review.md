# Multi-agent environment review

The environment review action opens a fresh chat thread in the target thread's working folder and sends the bundled `multi-agent-environment-review` skill as its first request. Confirm the folder shown before starting. Cancelling leaves the thread list unchanged.

Choose **Review Multi-Agent Environment…** from the New Thread menu to review the current session's folder, or from a sidebar thread's context menu to review that thread's recorded folder. The sidebar target takes precedence over the selected page and the agent profile's default folder. This is the session's fixed working folder; commands run in a terminal drawer do not change it. A review is a new conversation and does not carry the target's transcript into the request. Missing folders fail instead of falling back to home. With no current session, the New Thread action uses the application's starting folder.

The skill is embedded in the binary from `assets/skills/multi-agent-environment-review/SKILL.md`. It does not install a global skill or depend on a particular homelab, MCP service, or agent configuration. The review uses the target thread's agent when available, otherwise the application's default available agent; the confirmation names it. It uses that agent's normal process environment and permission controls.

The review inspects workspace instructions and relevant agent configuration metadata. It asks for no file changes, installations, network probes, service calls, or additional agents. Secret values and unrelated global configuration are outside its scope. The agent returns its report in chat with these fixed sections:

1. Scope and evidence
2. Summary
3. Scorecard
4. Agent/MCP matrix
5. Skill suggestions
6. Ordered improvement guide
7. Limits

The scorecard covers eight aspects, each scored 0–5 or unknown. Its overall score averages only supported numeric scores and includes coverage. A declared agent or MCP registration is distinguished from tested availability. Suggestions include concrete verification, undo, and approval requirements; applying them is a separate task.

The skill's read-only instructions define the requested behaviour. Actual tool access and approval enforcement remain those of the chosen agent and profile; a review prompt does not create an operating-system sandbox. In particular, do not infer equivalent permissions or MCP tools across harnesses from matching configuration names.

Validation should cover action target encoding, presence in the sidebar menu, confirmation cancellation, preservation of the target folder despite a different selected page or profile folder, one-time delivery of the bundled prompt, missing-agent handling, and the skill's seven-section/eight-aspect contract. Run repository formatting, lint, and test checks; use the supported preview path for GUI checks. Do not install a developer binary over the packaged application.
