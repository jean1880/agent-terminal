//! One short-lived `codex app-server` that answers the model catalogue and the account/usage
//! indicator.
//!
//! The probe spawns `codex app-server`, sends `initialize`, the `initialized` notification, then
//! `model/list`, `account/read` and `account/rateLimits/read`, reads the replies and terminates
//! the process. It never sends `thread/start` or `turn/start`, so no thread exists, no turn runs
//! and nothing is billed. [`probe_shared`] coalesces concurrent callers onto ONE process. Nothing
//! is logged but the outcome; frame bodies may hold the account email.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use agent_core::catalog::{parse_codex_models, CatalogModel};
use agent_core::event::{Account, QuotaWindow};
use agent_core::quota::{codex_account, codex_rate_limits, codex_signed_out};
use gtk4::glib;
use serde_json::{json, Value};
use tracing::warn;

use crate::agent_proc::{AgentEnv, AgentProcess, SpawnSpec};
use crate::probe::InFlight;

const INIT_ID: u64 = 1;
const MODELS_ID: u64 = 2;
const ACCOUNT_ID: u64 = 3;
const LIMITS_ID: u64 = 4;
/// How long `app-server` may take to answer `initialize` and `model/list`.
const MODELS_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the account and usage replies may take after that; their failure is not an error.
const ACCOUNT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodexProbe {
    pub models: Vec<CatalogModel>,
    pub account: Option<Account>,
    pub windows: Vec<QuotaWindow>,
    /// No one is signed in (`codex login` has not been run, or the login expired).
    pub signed_out: bool,
}

pub type ProbeResult = Result<CodexProbe, &'static str>;

fn argv(program: &str) -> Vec<String> {
    vec![program.to_owned(), "app-server".to_owned()]
}

fn request(id: u64, method: &str, params: Value) -> String {
    json!({"id": id, "method": method, "params": params}).to_string()
}

fn initialize() -> String {
    request(
        INIT_ID,
        "initialize",
        json!({"clientInfo": {
            "name": "agent-terminal",
            "title": "Agent Terminal",
            "version": env!("CARGO_PKG_VERSION"),
        }}),
    )
}

/// A JSON-RPC response frame by id: its `result`, or its `error` (the reply, so a refusal ends a
/// wait at once). `None` for requests and notifications.
fn response(frame: &Value) -> Option<(u64, Result<&Value, String>)> {
    if frame.get("method").is_some() {
        return None;
    }
    let id = frame.get("id")?.as_u64()?;
    if let Some(result) = frame.get("result") {
        return Some((id, Ok(result)));
    }
    let message = frame
        .get("error")?
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("error")
        .to_owned();
    Some((id, Err(message)))
}

/// Spawns `codex app-server` and runs the requests. See the module docs.
pub async fn probe_codex(program: &str, env: &AgentEnv) -> ProbeResult {
    let replies: Rc<RefCell<HashMap<u64, Result<Value, String>>>> = Rc::default();
    let exited = Rc::new(Cell::new(false));
    // The same environment a Codex thread runs in, so the account shown is the thread's.
    let spec = SpawnSpec {
        argv: argv(program),
        env: env.env.clone(),
        unset: env.unset.clone(),
        ..SpawnSpec::default()
    };
    let on_line = {
        let replies = replies.clone();
        move |line: &str| {
            let Ok(frame) = serde_json::from_str::<Value>(line) else {
                return;
            };
            if let Some((id, result)) = response(&frame) {
                replies.borrow_mut().insert(id, result.cloned());
            }
        }
    };
    let on_exit = {
        let exited = exited.clone();
        move |_| exited.set(true)
    };
    let proc =
        AgentProcess::spawn(&spec, on_line, |_| {}, on_exit).map_err(|_| "codex did not start")?;
    proc.write_line(&initialize());

    let wait = |ids: &'static [u64], limit: Duration| {
        let (replies, exited) = (replies.clone(), exited.clone());
        async move {
            let mut waited = Duration::ZERO;
            while !ids.iter().all(|id| replies.borrow().contains_key(id))
                && !exited.get()
                && waited < limit
            {
                glib::timeout_future(POLL).await;
                waited += POLL;
            }
        }
    };
    wait(&[INIT_ID], MODELS_TIMEOUT).await;
    // An error reply to `initialize` is the answer: fail now, not after the timeout.
    let init_refused = matches!(replies.borrow().get(&INIT_ID), Some(Err(_)));
    if init_refused {
        proc.terminate();
        return Err("codex refused initialize");
    }
    if replies.borrow().contains_key(&INIT_ID) {
        proc.write_line(&json!({"method": "initialized"}).to_string());
        proc.write_line(&request(MODELS_ID, "model/list", json!({"limit": 100})));
        proc.write_line(&request(ACCOUNT_ID, "account/read", json!({})));
        proc.write_line(&request(LIMITS_ID, "account/rateLimits/read", Value::Null));
        wait(&[MODELS_ID], MODELS_TIMEOUT).await;
        wait(&[ACCOUNT_ID, LIMITS_ID], ACCOUNT_TIMEOUT).await;
    }
    // Dropping `proc` would terminate it too; do it explicitly so the intent is visible.
    proc.terminate();

    let mut replies = replies.borrow_mut();
    let models = match replies.remove(&MODELS_ID) {
        Some(Ok(models)) => models,
        Some(Err(_)) => return Err("codex refused model/list"),
        None => {
            return Err(if exited.get() {
                "codex exited without answering"
            } else {
                "codex timed out"
            });
        }
    };
    let ok = |id: u64| replies.get(&id).and_then(|r| r.as_ref().ok());
    Ok(CodexProbe {
        models: parse_codex_models(&models),
        account: ok(ACCOUNT_ID).and_then(codex_account),
        windows: ok(LIMITS_ID).map(codex_rate_limits).unwrap_or_default(),
        signed_out: ok(ACCOUNT_ID).is_some_and(codex_signed_out),
    })
}

thread_local! {
    /// The callers waiting for the probe that is running, if any.
    static IN_FLIGHT: InFlight<ProbeResult> = const { InFlight::new() };
}

/// Runs [`probe_codex`], or joins the one already running; `done` gets the result on the main
/// loop. The catalogue and the usage indicator both call this, so one refresh costs one process.
pub fn probe_shared(program: &str, env: &AgentEnv, done: impl FnOnce(&ProbeResult) + 'static) {
    if !IN_FLIGHT.with(|f| f.join(done)) {
        return;
    }
    let (program, env) = (program.to_owned(), env.clone());
    glib::spawn_future_local(async move {
        let result = probe_codex(&program, &env).await;
        if let Err(why) = &result {
            warn!(why, "codex probe failed");
        }
        IN_FLIGHT.with(|f| f.finish(&result));
    });
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    use super::*;
    use crate::testutil::{in_loop, pump_until};

    /// A stand-in for `codex` (never the real one). It appends every request line it receives to
    /// `requests.log` beside it, and answers by id.
    fn fake_codex(dir: &Path, body: &str) -> String {
        let path = dir.join("fake-codex");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(&path)
            .unwrap();
        f.write_all(format!("#!/bin/sh\nD=$(dirname \"$0\")\n{body}\n").as_bytes())
            .unwrap();
        path.display().to_string()
    }

    const MODELS: &str = r#"{"id":2,"result":{"data":[{"id":"a","model":"gpt-5-codex","displayName":"GPT-5 Codex","supportedReasoningEfforts":[{"reasoningEffort":"low"},{"reasoningEffort":"high"}],"defaultReasoningEffort":"low","hidden":false,"isDefault":true}],"nextCursor":null}}"#;
    const ACCOUNT: &str = r#"{"id":3,"result":{"account":{"type":"chatgpt","email":"u@example.com","planType":"plus"},"requiresOpenaiAuth":true}}"#;
    const LIMITS: &str = r#"{"id":4,"result":{"rateLimits":{"primary":{"usedPercent":30,"windowDurationMins":300,"resetsAt":1790000000}}}}"#;

    fn answering() -> String {
        format!(
            "while read line; do\necho \"$line\" >> \"$D/requests.log\"\ncase \"$line\" in\n*'\"initialize\"'*) echo '{{\"id\":1,\"result\":{{}}}}';;\n*model/list*) echo '{MODELS}';;\n*account/read*) echo '{ACCOUNT}';;\n*account/rateLimits/read*) echo '{LIMITS}';;\nesac\ndone"
        )
    }

    fn run_probe(program: String) -> ProbeResult {
        in_loop(|ctx| {
            let out = Rc::new(RefCell::new(None));
            let o = out.clone();
            glib::spawn_future_local(async move {
                *o.borrow_mut() = Some(probe_codex(&program, &AgentEnv::default()).await);
            });
            assert!(pump_until(ctx, 20, || out.borrow().is_some()), "no result");
            let r = out.borrow_mut().take().unwrap();
            r
        })
    }

    #[test]
    fn probe_reads_models_account_and_usage_and_never_starts_a_thread_or_turn() {
        let dir = tempfile::tempdir().unwrap();
        let p = run_probe(fake_codex(dir.path(), &answering())).expect("probe");
        assert_eq!(p.models.len(), 1);
        assert_eq!(p.models[0].id, "gpt-5-codex");
        assert_eq!(p.models[0].efforts, ["low", "high"]);
        assert_eq!(p.account.expect("account").label, "u@example.com");
        assert_eq!(p.windows.len(), 1);
        assert_eq!(p.windows[0].used, 0.3);

        let log = std::fs::read_to_string(dir.path().join("requests.log")).unwrap();
        for forbidden in ["thread/start", "thread/resume", "turn/start", "turn/steer"] {
            assert!(!log.contains(forbidden), "the probe sent {forbidden}");
        }
        assert!(log.contains("\"method\":\"initialized\""));
        assert!(log.contains("model/list"));
        // `initialized` follows the initialize reply and precedes every other request.
        let pos = |needle: &str| log.find(needle).expect(needle);
        assert!(pos("\"initialize\"") < pos("\"initialized\""));
        assert!(pos("\"initialized\"") < pos("model/list"));
    }

    #[test]
    fn a_missing_account_reply_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        // Answers initialize and model/list only, then exits.
        let body = format!(
            "while read line; do\ncase \"$line\" in\n*'\"initialize\"'*) echo '{{\"id\":1,\"result\":{{}}}}';;\n*model/list*) echo '{MODELS}'; exit 0;;\nesac\ndone"
        );
        let p = run_probe(fake_codex(dir.path(), &body)).expect("probe");
        assert_eq!(p.models.len(), 1);
        assert!(p.account.is_none() && p.windows.is_empty());
    }

    #[test]
    fn an_error_reply_to_initialize_fails_at_once_not_after_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        // Answers initialize with an error and then stays alive: only the reply can end the wait.
        let body = "while read line; do\ncase \"$line\" in\n*'\"initialize\"'*) echo '{\"id\":1,\"error\":{\"code\":-1,\"message\":\"no\"}}';;\nesac\ndone";
        let started = std::time::Instant::now();
        assert_eq!(
            run_probe(fake_codex(dir.path(), body)),
            Err("codex refused initialize")
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited out the timeout"
        );
    }

    #[test]
    fn a_process_that_dies_or_is_missing_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            run_probe(fake_codex(dir.path(), "exit 1")),
            Err("codex exited without answering")
        );
        assert_eq!(
            run_probe("/nonexistent/codex".into()),
            Err("codex did not start")
        );
    }

    #[test]
    fn concurrent_callers_share_one_process() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("launches");
        let body = format!("echo x >> '{}'\n{}", count.display(), answering());
        let program = fake_codex(dir.path(), &body);
        let got = in_loop(|ctx| {
            let got = Rc::new(Cell::new(0));
            for _ in 0..3 {
                let got = got.clone();
                probe_shared(&program, &AgentEnv::default(), move |r| {
                    assert!(r.is_ok());
                    got.set(got.get() + 1);
                });
            }
            assert!(pump_until(ctx, 20, || got.get() == 3), "callers not served");
            got.get()
        });
        assert_eq!(got, 3);
        let launches = std::fs::read_to_string(count).unwrap();
        assert_eq!(launches.lines().count(), 1, "one process for three callers");
    }

    #[test]
    fn frames_and_argv() {
        assert_eq!(argv("codex"), ["codex", "app-server"]);
        let v: Value = serde_json::from_str(&initialize()).unwrap();
        assert_eq!(v["method"], "initialize");
        assert_eq!(v["params"]["clientInfo"]["name"], "agent-terminal");
        let f: Value = serde_json::from_str(MODELS).unwrap();
        assert_eq!(response(&f).map(|(id, _)| id), Some(2));
        let refused: Value = serde_json::from_str(r#"{"id":1,"error":{"message":"no"}}"#).unwrap();
        assert_eq!(response(&refused), Some((1, Err("no".to_owned()))));
        let note = json!({"method": "x", "id": 5, "params": {}});
        assert_eq!(response(&note), None, "a server request is not a response");
        // An error reply is a reply (the wait ends), carrying its message.
        assert_eq!(
            response(&json!({"id": 5, "error": {"message": "no"}})),
            Some((5, Err("no".to_owned())))
        );
    }
}
