//! A scripted fake backend for screenshots and manual review without real agents
//! (`agent-terminal --chat-demo`).
//!
//! It replays a canned envelope script that covers every row type (streaming text, reasoning,
//! tool cards including a failed and a declined one, subagent nesting, approvals, a question,
//! the plan, compaction, a model switch, a provider switch, a notice and a turn error), echoes
//! prompts, and answers every control with canned data.
//!
//! `AGENT_TERMINAL_DEMO_STRESS=<n>` preloads `n` mixed items first (the transcript spike).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::time::Duration;

use agent_core::adapter::{Control, Driver, Mode};
use agent_core::caps::Capabilities;
use agent_core::event::{
    Account, AgentCommand, AgentCommandKind, Decision, Envelope, Event, ItemKind, ItemStatus,
    PlanStep, Question, QuestionOption, QuotaWindow, ResponseCapability, StepStatus, StreamKind,
    TurnState,
};
use agent_core::quota::epoch_to_rfc3339;
use gtk4::glib;
use gtk4::prelude::*;
use serde_json::{json, Value};

use super::ChatView;
use crate::account_status::AccountStatus;
use crate::chat::{ChatBackend, EnvelopeSink, ModelSource, SessionStatus};
use agent_core::catalog::{parse_agy_models, CatalogModel};

/// One script step.
enum Step {
    Emit(Envelope),
    Wait(u32),
    /// The thread moves to another agent (what a handoff does to `status()`).
    SetDriver(Driver, Option<String>),
    SetRunning(bool),
}

pub struct DemoBackend {
    sink: RefCell<Option<EnvelopeSink>>,
    status: RefCell<SessionStatus>,
    queue: RefCell<VecDeque<Step>>,
    pumping: Cell<bool>,
    seq: Cell<u64>,
    mcp: RefCell<Vec<(String, bool)>>,
    me: RefCell<Weak<DemoBackend>>,
}

/// The demo backend. Connect it to a view with [`DemoBackend::connect`] after building the view
/// (`ChatView::new` takes the backend, and the backend needs the view's sink).
pub fn demo_backend() -> Rc<DemoBackend> {
    let b = Rc::new(DemoBackend {
        sink: RefCell::new(None),
        status: RefCell::new(SessionStatus {
            driver: Driver::Claude,
            model: Some("claude-opus-5-5".into()),
            effort: Some("medium".into()),
            mode: Mode::Ask,
            running_turn: false,
            alive: true,
            capabilities: Capabilities::claude(),
            commands: demo_commands(),
        }),
        queue: RefCell::new(VecDeque::new()),
        pumping: Cell::new(false),
        seq: Cell::new(0),
        mcp: RefCell::new(vec![
            ("git".into(), true),
            ("browser".into(), true),
            ("homelab-graph".into(), false),
        ]),
        me: RefCell::new(Weak::new()),
    });
    *b.me.borrow_mut() = Rc::downgrade(&b);
    b
}

/// A fixed model list for `--chat-demo`: both agents, with an agy route to Claude models.
pub struct DemoModels;

impl DemoModels {
    pub fn list() -> Vec<CatalogModel> {
        let efforts = || ["low", "medium", "high"].map(str::to_owned).to_vec();
        let claude = |id: &str, display: &str, desc: &str, efforts| CatalogModel {
            driver: Driver::Claude,
            id: id.into(),
            display: display.into(),
            description: Some(desc.into()),
            efforts,
            default_effort: None,
            via: None,
        };
        let mut models = vec![
            claude(
                "claude-opus-5-5",
                "Opus 5.5",
                "Most capable, for complex work",
                efforts(),
            ),
            claude(
                "claude-sonnet-5-5",
                "Sonnet 5.5",
                "Fast and capable for everyday tasks",
                efforts(),
            ),
            claude(
                "claude-haiku-5",
                "Haiku 5",
                "Fastest, for quick answers",
                Vec::new(),
            ),
        ];
        // agy lists one id per effort; the catalog folds them into one row per model.
        models.extend(parse_agy_models(
            "gemini-3.8-flash-high\tGemini 3.8 Flash (High)\n\
             gemini-3.8-flash-medium\tGemini 3.8 Flash (Medium)\n\
             gemini-3.8-flash-low\tGemini 3.8 Flash (Low)\n\
             gemini-3.1-pro-high\tGemini 3.1 Pro (High)\n\
             gemini-3.1-pro-low\tGemini 3.1 Pro (Low)\n\
             claude-opus-5-5-high\tClaude Opus 5.5 (High)\n\
             claude-opus-5-5-medium\tClaude Opus 5.5 (Medium)\n\
             claude-opus-5-5-low\tClaude Opus 5.5 (Low)\n\
             claude-sonnet-5-5-medium\tClaude Sonnet 5.5 (Medium)\n\
             claude-sonnet-5-5-high\tClaude Sonnet 5.5 (High)\n\
             gpt-oss-120b-medium\tGPT-OSS 120B (Medium)\n",
        ));
        models
    }
}

impl ModelSource for DemoModels {
    fn models(&self) -> Vec<CatalogModel> {
        Self::list()
    }

    fn connect_changed(&self, _f: Box<dyn Fn()>) -> u64 {
        0
    }

    fn disconnect(&self, _id: u64) {}
}

/// Static account and quota data for `--chat-demo`: one agent in the amber, one in the red.
pub fn demo_account_status() -> Rc<AccountStatus> {
    let status = Rc::new(AccountStatus::new());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let window = |group: Option<&str>, label: &str, used: f64, in_secs: i64| QuotaWindow {
        group: group.map(str::to_owned),
        label: label.into(),
        used,
        resets_at: Some(epoch_to_rfc3339(now + in_secs)),
    };
    let account = |label: &str, plan: Option<&str>, provider: &str| Account {
        label: label.into(),
        plan: plan.map(str::to_owned),
        provider: Some(provider.into()),
    };
    let quota = |account, windows| Envelope::new(Event::QuotaUpdated { account, windows });
    status.observe(
        Driver::Claude,
        &quota(
            Some(account(
                "user@example.com",
                Some("Claude Pro"),
                "firstParty",
            )),
            vec![
                window(None, "5-hour", 0.42, 3 * 3600 + 47 * 60),
                window(None, "Weekly", 0.78, 3 * 86_400 + 10 * 3600),
                window(None, "Weekly (Fable)", 0.0, 3 * 86_400 + 10 * 3600),
            ],
        ),
    );
    status.observe(
        Driver::Agy,
        &quota(
            Some(account("user@example.com", None, "Google")),
            vec![
                window(
                    Some("Gemini Models"),
                    "Weekly",
                    0.14,
                    3 * 86_400 + 10 * 3600,
                ),
                window(Some("Gemini Models"), "5-hour", 0.93, 55 * 60),
                window(Some("Claude and GPT models"), "Weekly", 0.0, 6 * 86_400),
                window(Some("Claude and GPT models"), "5-hour", 0.0, 5 * 3600),
            ],
        ),
    );
    status
}

fn demo_commands() -> Vec<AgentCommand> {
    let cmd = |name: &str, desc: &str, hint: Option<&str>, kind| AgentCommand {
        name: name.into(),
        description: Some(desc.into()),
        argument_hint: hint.map(str::to_owned),
        kind,
    };
    vec![
        cmd(
            "review",
            "Review a pull request",
            Some("<pr>"),
            AgentCommandKind::Command,
        ),
        cmd(
            "security-review",
            "Security review of pending changes",
            None,
            AgentCommandKind::Command,
        ),
        cmd(
            "init",
            "Write an AGENTS.md for this repo",
            None,
            AgentCommandKind::Command,
        ),
        cmd(
            "doctor",
            "Diagnose the installation",
            None,
            AgentCommandKind::TerminalOnly,
        ),
        cmd(
            "pdf",
            "Read and write PDF files",
            None,
            AgentCommandKind::Skill,
        ),
        cmd(
            "rust-craftsman",
            "Idiomatic, gated Rust",
            None,
            AgentCommandKind::Skill,
        ),
    ]
}

impl DemoBackend {
    pub fn connect(&self, sink: EnvelopeSink) {
        *self.sink.borrow_mut() = Some(sink);
    }

    fn next_id(&self, prefix: &str) -> String {
        self.seq.set(self.seq.get() + 1);
        format!("{prefix}-{}", self.seq.get())
    }

    fn emit_now(&self, env: &Envelope) {
        let sink = self.sink.borrow().clone();
        if let Some(sink) = sink {
            sink(env);
        }
    }

    fn push(&self, steps: impl IntoIterator<Item = Step>) {
        self.queue.borrow_mut().extend(steps);
        self.pump();
    }

    /// Plays queued steps on the main loop, one timeout per wait.
    fn pump(&self) {
        if self.pumping.replace(true) {
            return;
        }
        loop {
            let step = self.queue.borrow_mut().pop_front();
            match step {
                None => {
                    self.pumping.set(false);
                    return;
                }
                Some(Step::Emit(env)) => self.emit_now(&env),
                Some(Step::SetDriver(driver, model)) => {
                    let mut s = self.status.borrow_mut();
                    s.driver = driver;
                    s.model = model;
                    s.capabilities = Capabilities::of(driver);
                }
                Some(Step::SetRunning(r)) => self.status.borrow_mut().running_turn = r,
                Some(Step::Wait(ms)) => {
                    let me = self.me.borrow().clone();
                    glib::timeout_add_local_once(Duration::from_millis(u64::from(ms)), move || {
                        if let Some(me) = me.upgrade() {
                            me.pumping.set(false);
                            me.pump();
                        }
                    });
                    return;
                }
            }
        }
    }

    /// Plays the full demo script.
    pub fn play_script(&self) {
        self.push(script());
    }

    /// `n` mixed stored items for [`ChatView::replay`]: the transcript spike.
    pub fn stress_history(n: usize) -> Vec<Envelope> {
        (0..n).flat_map(stress_item).collect()
    }

    fn reply_turn(&self, text: &str) {
        let user = self.next_id("user");
        let reply = self.next_id("reply");
        let mut steps = vec![
            Step::Emit(started(&user, ItemKind::UserMessage, "", None, None)),
            Step::Emit(snapshot(&user, StreamKind::Assistant, text)),
            Step::SetRunning(true),
            Step::Emit(Envelope::new(Event::TurnStarted { model: None })),
            Step::Wait(250),
        ];
        let body = if text.starts_with('/') {
            format!(
                "*(demo)* `{}` would be sent to the agent as a slash command.",
                text.split_whitespace().next().unwrap_or(text)
            )
        } else {
            format!(
                "This is the **demo backend**, so nothing ran. You said:\n\n> {}\n\n\
                 Try `/` for commands, `@` for files, or `/model` to open the picker.",
                text.lines().next().unwrap_or("")
            )
        };
        steps.push(Step::Emit(started(
            &reply,
            ItemKind::AssistantMessage,
            "",
            None,
            None,
        )));
        steps.extend(stream(&reply, StreamKind::Assistant, &body, 6, 30));
        steps.push(Step::Emit(completed(
            &reply,
            ItemStatus::Completed,
            None,
            None,
        )));
        steps.push(Step::SetRunning(false));
        steps.push(Step::Emit(turn_done(TurnState::Completed, None)));
        self.push(steps);
    }

    fn control_reply(&self, control: &Control) -> Value {
        let driver = self.status.borrow().driver;
        match control {
            Control::ListModels => match driver {
                Driver::Claude => json!({"models": [
                    {"value": "claude-opus-5-5", "displayName": "Opus 5.5", "description": "Most capable, for complex work"},
                    {"value": "claude-sonnet-5-5", "displayName": "Sonnet 5.5", "description": "Fast and capable for everyday tasks"},
                    {"value": "claude-haiku-5", "displayName": "Haiku 5", "description": "Fastest, for quick answers"}
                ]}),
                Driver::Agy => json!([
                    {"id": "gemini-3.1-pro", "display": "Gemini 3.1 Pro"},
                    {"id": "gemini-3.1-pro-low", "display": "Gemini 3.1 Pro (low)"},
                    {"id": "gemini-3-flash", "display": "Gemini 3 Flash"}
                ]),
                Driver::Codex => json!([
                    {"id": "gpt-5-codex", "display": "GPT-5 Codex"}
                ]),
            },
            Control::McpStatus => {
                let servers: Vec<Value> = self
                    .mcp
                    .borrow()
                    .iter()
                    .map(|(name, on)| {
                        if *on {
                            json!({"name": name, "status": "connected",
                                   "serverInfo": {"name": format!("{name}-mcp"), "version": "1.4.0"}})
                        } else {
                            json!({"name": name, "status": "disabled"})
                        }
                    })
                    .collect();
                json!({ "mcpServers": servers })
            }
            Control::McpToggle { server, enabled } => {
                for (name, on) in self.mcp.borrow_mut().iter_mut() {
                    if name == server {
                        *on = *enabled;
                    }
                }
                Value::Null
            }
            Control::McpReconnect { .. } => Value::Null,
            Control::GetSettings => json!({
                "effective": {
                    "model": "claude-opus-5-5",
                    "permissions": {"defaultMode": "default", "allow": ["Bash(cargo test:*)", "Read"]},
                    "env": {"RUST_LOG": "info"},
                    "includeCoAuthoredBy": false,
                    "statusLine": {"type": "command", "command": "~/.claude/statusline.sh"}
                },
                "sources": ["user", "project"]
            }),
            Control::FileSuggestions { query } => {
                let files = [
                    "src/main.rs",
                    "src/config.rs",
                    "src/chat/view.rs",
                    "src/chat/mod.rs",
                    "crates/agent-core/src/event.rs",
                    "crates/agent-core/src/commands.rs",
                    "AGENTS.md",
                    "Cargo.toml",
                ];
                let q = query.to_lowercase();
                let hits: Vec<&str> = files
                    .iter()
                    .copied()
                    .filter(|f| f.to_lowercase().contains(&q))
                    .collect();
                json!(hits)
            }
            Control::ContextUsage => json!({
                "totalTokens": 64_210, "maxTokens": 200_000, "autoCompactThreshold": 160_000,
                "categories": [
                    {"name": "System prompt", "tokens": 3_120},
                    {"name": "Tools", "tokens": 11_840},
                    {"name": "MCP tools", "tokens": 6_400},
                    {"name": "Messages", "tokens": 42_850}
                ]
            }),
            Control::Usage => json!({
                "session": {"input_tokens": 182_400, "output_tokens": 9_310, "cost_usd": 1.42},
                "quota": {"five_hour": {"used_percent": 38, "resets_at": "16:00"},
                          "weekly": {"used_percent": 12, "resets_at": "Mon 09:00"}}
            }),
        }
    }
}

impl ChatBackend for DemoBackend {
    fn send_prompt(&self, text: &str) {
        self.reply_turn(text);
    }

    fn interrupt(&self) {
        // Drop whatever the current turn still had queued, then close it as interrupted.
        let running = self.status.borrow().running_turn;
        if !running {
            return;
        }
        self.queue.borrow_mut().clear();
        self.status.borrow_mut().running_turn = false;
        self.push([Step::Emit(turn_done(TurnState::Interrupted, None))]);
    }

    fn respond_approval(&self, request: &str, decision: Decision) {
        self.push([
            Step::Wait(300),
            Step::Emit(Envelope::new(Event::ApprovalResolved { decision }).request(request)),
        ]);
    }

    fn answer_questions(&self, request: &str, answers: Value) {
        let summary = answers
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(q, a)| format!("{q} → {}", a.as_str().unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        self.push([
            Step::Wait(300),
            Step::Emit(Envelope::new(Event::QuestionResolved { answered: true }).request(request)),
            Step::Emit(Envelope::new(Event::Notice {
                text: format!("Answers sent:\n{summary}"),
            })),
        ]);
    }

    fn switch(&self, driver: Driver, model: Option<String>, effort: Option<String>) {
        let current = self.status.borrow().driver;
        if driver == current {
            let model = model.unwrap_or_else(|| "default".into());
            {
                let mut status = self.status.borrow_mut();
                status.model = Some(model.clone());
                if effort.is_some() {
                    status.effort = effort;
                }
            }
            self.push([
                Step::Emit(Envelope::new(Event::ModelChanged {
                    model: model.clone(),
                })),
                Step::Emit(Envelope::new(Event::Notice {
                    text: format!("Set model to {model}"),
                })),
            ]);
        } else {
            let model = model.or_else(|| {
                Some(match driver {
                    Driver::Claude => "claude-opus-5-5".into(),
                    Driver::Agy => "gemini-3.1-pro-high".into(),
                    Driver::Codex => "gpt-5-codex".into(),
                })
            });
            self.status.borrow_mut().effort = effort;
            self.push([
                Step::SetDriver(driver, model.clone()),
                Step::Emit(Envelope::new(Event::SessionStarted {
                    native_id: self.next_id("native"),
                    model,
                    cwd: None,
                })),
            ]);
        }
    }

    fn set_mode(&self, mode: Mode) {
        self.status.borrow_mut().mode = mode;
        self.push([Step::Emit(Envelope::new(Event::ModeChanged { mode }))]);
    }

    fn control(&self, control: Control) -> String {
        let id = self.next_id("ctl");
        let ok = Some(self.control_reply(&control));
        self.push([
            Step::Wait(180),
            Step::Emit(Envelope::new(Event::ControlResult { ok, error: None }).request(&id)),
        ]);
        id
    }

    fn status(&self) -> SessionStatus {
        self.status.borrow().clone()
    }
}

// ---------------------------------------------------------------------------------------------
// Script helpers
// ---------------------------------------------------------------------------------------------

fn started(
    id: &str,
    kind: ItemKind,
    title: &str,
    input: Option<Value>,
    parent: Option<&str>,
) -> Envelope {
    Envelope::new(Event::ItemStarted {
        kind,
        title: title.into(),
        input,
        parent: parent.map(str::to_owned),
    })
    .item(id)
}

fn snapshot(id: &str, stream: StreamKind, text: &str) -> Envelope {
    Envelope::new(Event::ContentSnapshot {
        stream,
        text: text.into(),
    })
    .item(id)
}

fn completed(id: &str, status: ItemStatus, output: Option<&str>, error: Option<&str>) -> Envelope {
    Envelope::new(Event::ItemCompleted {
        status,
        output: output.map(str::to_owned),
        error: error.map(str::to_owned),
    })
    .item(id)
}

fn turn_done(state: TurnState, error: Option<&str>) -> Envelope {
    Envelope::new(Event::TurnCompleted {
        state,
        usage: None,
        cost_usd: None,
        error: error.map(str::to_owned),
    })
}

/// Streams `text` as deltas of about `chunk` words every `ms` milliseconds.
fn stream(id: &str, stream: StreamKind, text: &str, chunk: usize, ms: u32) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut buf = String::new();
    let mut words = 0;
    for piece in text.split_inclusive(' ') {
        buf.push_str(piece);
        words += 1;
        if words >= chunk {
            steps.push(Step::Emit(
                Envelope::new(Event::ContentDelta {
                    stream,
                    text: std::mem::take(&mut buf),
                })
                .item(id),
            ));
            steps.push(Step::Wait(ms));
            words = 0;
        }
    }
    if !buf.is_empty() {
        steps.push(Step::Emit(
            Envelope::new(Event::ContentDelta { stream, text: buf }).item(id),
        ));
    }
    steps
}

// Script data: one positional call per card reads better than a builder here.
#[allow(clippy::too_many_arguments)]
fn tool(
    id: &str,
    kind: ItemKind,
    title: &str,
    input: Value,
    parent: Option<&str>,
    status: ItemStatus,
    output: Option<&str>,
    error: Option<&str>,
    ms: u32,
) -> Vec<Step> {
    vec![
        Step::Emit(started(id, kind, title, Some(input), parent)),
        Step::Wait(ms),
        Step::Emit(completed(id, status, output, error)),
        Step::Wait(120),
    ]
}

const ANSWER: &str = "## Atomic config writes\n\n\
The loader now writes through a **temp file in the same directory**, calls `sync_all`, and \
renames it over the old file, so a crash can never leave a half-written config.\n\n\
What changed:\n\n\
1. `save()` goes through a new `write_atomic()` helper.\n\
2. The temp file is created `0600`, so secrets in the config are never world-readable.\n\
3. A failed rename removes the temp file instead of leaving litter behind.\n\n\
```rust\nfn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {\n    \
let dir = path.parent().unwrap_or(Path::new(\".\"));\n    \
let mut tmp = tempfile::NamedTempFile::new_in(dir)?;\n    \
tmp.write_all(bytes)?;\n    tmp.as_file().sync_all()?;\n    \
tmp.persist(path).map_err(|e| e.error)?;\n    Ok(())\n}\n```\n\n\
| Check | Result |\n|---|---|\n| `cargo test -p agent-kit` | 48 passed |\n| clippy | clean |\n\n\
> The one place that still writes in place is the **legacy migration**; it runs once and is \
covered by `migration_is_atomic`.\n\n\
See the [Rust docs on `File::sync_all`](https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all) \
for why the sync comes before the rename.";

fn script() -> Vec<Step> {
    let mut s = vec![
        Step::Emit(Envelope::new(Event::SessionStarted {
            native_id: "demo-session".into(),
            model: Some("claude-opus-5-5".into()),
            cwd: Some("~/git/agent-terminal".into()),
        })),
        Step::Emit(Envelope::new(Event::ModeChanged { mode: Mode::Ask })),
        Step::Emit(Envelope::new(Event::UsageUpdated {
            used: 18_400,
            max: Some(200_000),
            auto_compact_at: Some(160_000),
        })),
        Step::Emit(started("u1", ItemKind::UserMessage, "", None, None)),
        Step::Emit(snapshot(
            "u1",
            StreamKind::Assistant,
            "Make config writes atomic, and add a test that proves a crash mid-write can't corrupt it.",
        )),
        Step::SetRunning(true),
        Step::Emit(Envelope::new(Event::TurnStarted {
            model: Some("claude-opus-5-5".into()),
        })),
        Step::Wait(200),
        Step::Emit(started("th1", ItemKind::Reasoning, "", None, None)),
    ];
    s.extend(stream(
        "th1",
        StreamKind::Reasoning,
        "The user wants atomic writes. First find every place the config is written, then \
         replace in-place writes with temp-file + rename. A test can simulate a crash by \
         dropping the temp file before persist.",
        5,
        25,
    ));
    s.push(Step::Emit(completed(
        "th1",
        ItemStatus::Completed,
        None,
        None,
    )));
    s.push(Step::Emit(started(
        "a1",
        ItemKind::AssistantMessage,
        "",
        None,
        None,
    )));
    s.extend(stream(
        "a1",
        StreamKind::Assistant,
        "I'll start by finding every place the config is written.",
        4,
        30,
    ));
    s.push(Step::Emit(completed(
        "a1",
        ItemStatus::Completed,
        None,
        None,
    )));
    s.push(Step::Emit(Envelope::new(Event::PlanUpdated {
        steps: vec![
            PlanStep {
                text: "Find every config writer".into(),
                status: StepStatus::InProgress,
            },
            PlanStep {
                text: "Route writes through write_atomic()".into(),
                status: StepStatus::Pending,
            },
            PlanStep {
                text: "Add a crash-safety test".into(),
                status: StepStatus::Pending,
            },
        ],
    })));
    // A subagent with nested tool cards.
    s.push(Step::Emit(started(
        "task1",
        ItemKind::Subagent,
        "Explore",
        Some(json!({"description": "Find every config writer", "subagent_type": "Explore"})),
        None,
    )));
    s.extend(tool(
        "t-grep",
        ItemKind::Command,
        "Bash",
        json!({"command": "rg -n 'fs::write|File::create' src/"}),
        Some("task1"),
        ItemStatus::Completed,
        Some("src/config.rs:412:    fs::write(&path, toml)?;\nsrc/config.rs:977:    let mut f = File::create(&legacy)?;"),
        None,
        350,
    ));
    s.extend(tool(
        "t-read",
        ItemKind::FileRead,
        "Read",
        json!({"file_path": "src/config.rs"}),
        Some("task1"),
        ItemStatus::Completed,
        Some("1180 lines"),
        None,
        250,
    ));
    s.push(Step::Emit(completed(
        "task1",
        ItemStatus::Completed,
        Some("Two writers: Config::save (src/config.rs:412) and the legacy migration (src/config.rs:977)."),
        None,
    )));
    s.push(Step::Emit(Envelope::new(Event::PlanUpdated {
        steps: vec![
            PlanStep {
                text: "Find every config writer".into(),
                status: StepStatus::Completed,
            },
            PlanStep {
                text: "Route writes through write_atomic()".into(),
                status: StepStatus::InProgress,
            },
            PlanStep {
                text: "Add a crash-safety test".into(),
                status: StepStatus::Pending,
            },
        ],
    })));
    // An approval that the "user" answers.
    s.push(Step::Emit(
        Envelope::new(Event::ApprovalRequested {
            tool: "Edit".into(),
            title: Some("Edit src/config.rs".into()),
            input: json!({"file_path": "src/config.rs", "old_string": "fs::write(&path, toml)?;", "new_string": "write_atomic(&path, toml.as_bytes())?;"}),
            reason: Some("Ask mode: edits need your approval.".into()),
            options: vec![Decision::Allow, Decision::AllowForSession, Decision::Deny],
            response: ResponseCapability::Live,
        })
        .request("ap1"),
    ));
    s.push(Step::Wait(700));
    s.push(Step::Emit(
        Envelope::new(Event::ApprovalResolved {
            decision: Decision::AllowForSession,
        })
        .request("ap1"),
    ));
    s.extend(tool(
        "t-edit",
        ItemKind::FileChange,
        "Edit",
        json!({"file_path": "src/config.rs"}),
        None,
        ItemStatus::Completed,
        Some("@@ -409,7 +409,7 @@\n-    fs::write(&path, toml)?;\n+    write_atomic(&path, toml.as_bytes())?;"),
        None,
        300,
    ));
    s.extend(tool(
        "t-env",
        ItemKind::FileChange,
        "Write",
        json!({"file_path": ".env"}),
        None,
        ItemStatus::Declined,
        None,
        Some("The user denied writing .env."),
        200,
    ));
    s.extend(tool(
        "t-test",
        ItemKind::Command,
        "Bash",
        json!({"command": "cargo test -p agent-kit config::"}),
        None,
        ItemStatus::Failed,
        Some("running 12 tests\ntest config::tests::save_is_atomic ... FAILED\n\nfailures:\n    tempfile not found in scope"),
        Some("exit status 101"),
        450,
    ));
    s.extend(tool(
        "t-mcp",
        ItemKind::McpTool,
        "git · diff",
        json!({"path": "src/config.rs"}),
        None,
        ItemStatus::Completed,
        Some("1 file changed, 24 insertions(+), 3 deletions(-)"),
        None,
        200,
    ));
    s.push(Step::Emit(started(
        "a2",
        ItemKind::AssistantMessage,
        "",
        None,
        None,
    )));
    s.extend(stream("a2", StreamKind::Assistant, ANSWER, 5, 22));
    s.push(Step::Emit(completed(
        "a2",
        ItemStatus::Completed,
        None,
        None,
    )));
    s.push(Step::Emit(Envelope::new(Event::PlanUpdated {
        steps: vec![
            PlanStep {
                text: "Find every config writer".into(),
                status: StepStatus::Completed,
            },
            PlanStep {
                text: "Route writes through write_atomic()".into(),
                status: StepStatus::Completed,
            },
            PlanStep {
                text: "Add a crash-safety test".into(),
                status: StepStatus::InProgress,
            },
        ],
    })));
    s.push(Step::SetRunning(false));
    s.push(Step::Emit(turn_done(TurnState::Completed, None)));
    s.push(Step::Emit(Envelope::new(Event::UsageUpdated {
        used: 64_210,
        max: Some(200_000),
        auto_compact_at: Some(160_000),
    })));
    s.push(Step::Wait(300));
    s.push(Step::Emit(Envelope::new(Event::Compacted {
        manual: true,
        before: 21_478,
        after: Some(2_082),
    })));
    s.push(Step::Emit(Envelope::new(Event::ModelChanged {
        model: "claude-sonnet-5-5".into(),
    })));
    s.push(Step::Emit(Envelope::new(Event::Notice {
        text: "Set model to claude-sonnet-5-5".into(),
    })));
    s.push(Step::Emit(Envelope::new(Event::RateLimited {
        resets_at: Some("16:00".into()),
        detail: None,
    })));
    // A question.
    s.push(Step::Emit(started(
        "u2",
        ItemKind::UserMessage,
        "",
        None,
        None,
    )));
    s.push(Step::Emit(snapshot(
        "u2",
        StreamKind::Assistant,
        "Ask me before you pick a test approach.",
    )));
    s.push(Step::Emit(
        Envelope::new(Event::QuestionRequested {
            questions: vec![
                Question {
                    id: "q1".into(),
                    header: "Approach".into(),
                    question: "How should the crash be simulated?".into(),
                    options: vec![
                        QuestionOption {
                            label: "Drop before persist".into(),
                            description: Some("Fast, no processes".into()),
                        },
                        QuestionOption {
                            label: "Kill a child process".into(),
                            description: Some("Closest to a real crash".into()),
                        },
                    ],
                    multi_select: false,
                },
                Question {
                    id: "q2".into(),
                    header: "Coverage".into(),
                    question: "Which writers should the test cover?".into(),
                    options: vec![
                        QuestionOption {
                            label: "Config::save".into(),
                            description: None,
                        },
                        QuestionOption {
                            label: "Legacy migration".into(),
                            description: None,
                        },
                        QuestionOption {
                            label: "Theme export".into(),
                            description: None,
                        },
                    ],
                    multi_select: true,
                },
            ],
        })
        .request("qq1"),
    ));
    s.push(Step::Wait(400));
    // Provider switch to agy, a reply, then a failing turn and a pending approval.
    s.push(Step::SetDriver(
        Driver::Agy,
        Some("gemini-3.1-pro-high".into()),
    ));
    s.push(Step::Emit(Envelope::new(Event::SessionStarted {
        native_id: "agy-conv".into(),
        model: Some("gemini-3.1-pro-high".into()),
        cwd: None,
    })));
    s.push(Step::Emit(started(
        "u3",
        ItemKind::UserMessage,
        "",
        None,
        None,
    )));
    s.push(Step::Emit(snapshot(
        "u3",
        StreamKind::Assistant,
        "Clean the build directory, then summarise what's left to do.",
    )));
    s.push(Step::SetRunning(true));
    s.push(Step::Emit(Envelope::new(Event::TurnStarted {
        model: None,
    })));
    s.push(Step::Emit(started(
        "a3",
        ItemKind::AssistantMessage,
        "",
        None,
        None,
    )));
    s.extend(stream(
        "a3",
        StreamKind::Assistant,
        "Picking up from the handoff: the atomic writer is in, and the **crash-safety test** is \
         still open. I'll clean `target/` first.",
        4,
        30,
    ));
    s.push(Step::Emit(completed(
        "a3",
        ItemStatus::Completed,
        None,
        None,
    )));
    s.push(Step::Emit(
        Envelope::new(Event::ApprovalRequested {
            tool: "run_command".into(),
            title: Some("Run cargo clean".into()),
            input: json!({"CommandLine": "cargo clean", "Cwd": "~/git/agent-terminal"}),
            reason: None,
            options: vec![Decision::Allow, Decision::Deny],
            response: ResponseCapability::Live,
        })
        .request("ap2"),
    ));
    s.push(Step::SetRunning(false));
    s.push(Step::Emit(turn_done(
        TurnState::Failed,
        Some("agy: quota exhausted for gemini-3.1-pro (resets 16:00)"),
    )));
    s
}

/// One stress item (several envelopes): user, assistant, tool and reasoning in rotation.
fn stress_item(i: usize) -> Vec<Envelope> {
    let id = format!("s{i}");
    match i % 4 {
        0 => vec![
            started(&id, ItemKind::UserMessage, "", None, None),
            snapshot(
                &id,
                StreamKind::Assistant,
                &format!("Stress prompt number {i}"),
            ),
        ],
        1 => vec![
            started(&id, ItemKind::AssistantMessage, "", None, None),
            snapshot(
                &id,
                StreamKind::Assistant,
                &format!("Reply **{i}** with `code` and a list:\n\n- one\n- two"),
            ),
            completed(&id, ItemStatus::Completed, None, None),
        ],
        2 => vec![
            started(
                &id,
                ItemKind::Command,
                "Bash",
                Some(json!({"command": format!("echo {i}")})),
                None,
            ),
            completed(&id, ItemStatus::Completed, Some(&i.to_string()), None),
        ],
        _ => vec![
            started(&id, ItemKind::Reasoning, "", None, None),
            snapshot(&id, StreamKind::Reasoning, "Thinking about the next step."),
            completed(&id, ItemStatus::Completed, None, None),
        ],
    }
}

/// `agent-terminal --chat-demo`: a window holding only a [`ChatView`] on the demo backend.
pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("com.jdesroches.AgentTerminal.ChatDemo")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(|app| {
        crate::icons::register();
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
        let backend = demo_backend();
        let view = ChatView::new(backend.clone());
        backend.connect(view.sink());
        view.set_model_source(Rc::new(DemoModels));
        view.set_account_status(demo_account_status());
        view.connect_action(|action| tracing::info!(?action, "chat demo: view action"));

        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&adw::WindowTitle::new(
            "Atomic config writes",
            "~/git/agent-terminal",
        )));
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&view));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Agent Terminal: chat demo")
            .default_width(1000)
            .default_height(860)
            .content(&toolbar)
            .build();
        window.present();
        view.focus_composer();

        let stress = std::env::var("AGENT_TERMINAL_DEMO_STRESS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        if stress > 0 {
            view.replay(&DemoBackend::stress_history(stress));
        }
        // A short pause so the window is on screen (and a remote display attached) before the
        // script streams.
        glib::timeout_add_local_once(Duration::from_millis(1500), move || backend.play_script());
    });
    // GApplication must not see `--chat-demo` (it would reject an unknown option).
    let argv0 = std::env::args()
        .next()
        .unwrap_or_else(|| "agent-terminal".into());
    app.run_with_args(&[argv0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_models_cover_both_agents_and_a_via_route() {
        let m = DemoModels.models();
        assert!(m.iter().any(|m| m.driver == Driver::Claude));
        assert!(m.iter().any(|m| m.driver == Driver::Agy));
        let via: Vec<_> = m.iter().filter(|m| m.via.is_some()).collect();
        assert_eq!(via.len(), 3);
        assert!(via.iter().all(|m| m.driver == Driver::Agy));
        assert!(m.iter().any(|m| !m.efforts.is_empty()));
    }
}
