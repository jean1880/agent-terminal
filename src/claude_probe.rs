//! One short-lived Claude process that answers both the model catalog and the account/usage
//! indicator.
//!
//! The probe spawns `claude --input-format stream-json`, sends `initialize` (models + account),
//! then `get_usage` (plan windows), reads both replies and terminates the process. No prompt is
//! ever sent, so no turn runs and nothing is billed. [`probe_shared`] coalesces concurrent
//! callers onto ONE process. Nothing is logged but the outcome; frame bodies may hold the
//! account email.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use agent_core::catalog::{parse_claude_initialize, CatalogModel};
use agent_core::event::{Account, QuotaWindow};
use agent_core::quota::{claude_account, claude_usage};
use gtk4::glib;
use serde_json::Value;
use tracing::warn;

use crate::agent_proc::{AgentProcess, SpawnSpec};

const INIT_ID: &str = "catalog-1";
const USAGE_ID: &str = "catalog-usage";
/// How long Claude may take to answer `initialize`.
const INIT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `get_usage` may take after that; its failure is not an error.
const USAGE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeProbe {
    pub models: Vec<CatalogModel>,
    pub account: Option<Account>,
    pub windows: Vec<QuotaWindow>,
}

pub type ProbeResult = Result<ClaudeProbe, &'static str>;

fn request(id: &str, subtype: &str) -> String {
    serde_json::json!({
        "type": "control_request",
        "request_id": id,
        "request": {"subtype": subtype}
    })
    .to_string()
}

fn argv(program: &str) -> Vec<String> {
    [
        program,
        "--output-format",
        "stream-json",
        "--verbose",
        "--input-format",
        "stream-json",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The `request_id` of a `control_response` frame.
fn response_id(frame: &Value) -> Option<&str> {
    if frame.get("type").and_then(Value::as_str) != Some("control_response") {
        return None;
    }
    frame
        .pointer("/response/request_id")
        .and_then(Value::as_str)
}

/// Spawns Claude and runs the two requests. See the module docs.
pub async fn probe_claude(program: &str) -> ProbeResult {
    let init: Rc<RefCell<Option<Value>>> = Rc::default();
    let usage: Rc<RefCell<Option<Value>>> = Rc::default();
    let exited = Rc::new(Cell::new(false));
    let spec = SpawnSpec {
        argv: argv(program),
        ..SpawnSpec::default()
    };
    let on_line = {
        let (init, usage) = (init.clone(), usage.clone());
        move |line: &str| {
            let Ok(frame) = serde_json::from_str::<Value>(line) else {
                return;
            };
            match response_id(&frame) {
                Some(INIT_ID) => *init.borrow_mut() = Some(frame),
                Some(USAGE_ID) => *usage.borrow_mut() = Some(frame),
                _ => {}
            }
        }
    };
    let on_exit = {
        let exited = exited.clone();
        move |_| exited.set(true)
    };
    let proc =
        AgentProcess::spawn(&spec, on_line, |_| {}, on_exit).map_err(|_| "claude did not start")?;
    proc.write_line(&request(INIT_ID, "initialize"));

    let wait = |slot: &Rc<RefCell<Option<Value>>>, limit: Duration| {
        let (slot, exited) = (slot.clone(), exited.clone());
        async move {
            let mut waited = Duration::ZERO;
            while slot.borrow().is_none() && !exited.get() && waited < limit {
                glib::timeout_future(POLL).await;
                waited += POLL;
            }
        }
    };
    wait(&init, INIT_TIMEOUT).await;
    if init.borrow().is_some() {
        proc.write_line(&request(USAGE_ID, "get_usage"));
        wait(&usage, USAGE_TIMEOUT).await;
    }
    // Dropping `proc` would terminate it too; do it explicitly so the intent is visible.
    proc.terminate();

    let init = init.borrow_mut().take();
    let Some(init) = init else {
        return Err(if exited.get() {
            "claude exited without answering"
        } else {
            "claude timed out"
        });
    };
    let windows = usage
        .borrow_mut()
        .take()
        .map(|u| claude_usage(&u))
        .unwrap_or_default();
    Ok(ClaudeProbe {
        models: parse_claude_initialize(&init),
        account: claude_account(&init),
        windows,
    })
}

type Waiter = Box<dyn FnOnce(&ProbeResult)>;

thread_local! {
    /// `Some` while a probe is running: the callers waiting for its result.
    static IN_FLIGHT: RefCell<Option<Vec<Waiter>>> = const { RefCell::new(None) };
}

/// Runs [`probe_claude`], or joins the one already running; `done` gets the result on the main
/// loop. The model catalog and the usage indicator both call this, so one refresh costs one
/// Claude process.
pub fn probe_shared(program: &str, done: impl FnOnce(&ProbeResult) + 'static) {
    let start = IN_FLIGHT.with(|f| {
        let mut f = f.borrow_mut();
        match f.as_mut() {
            Some(waiters) => {
                waiters.push(Box::new(done));
                false
            }
            None => {
                *f = Some(vec![Box::new(done)]);
                true
            }
        }
    });
    if !start {
        return;
    }
    let program = program.to_owned();
    glib::spawn_future_local(async move {
        let result = probe_claude(&program).await;
        if let Err(why) = &result {
            warn!(why, "claude probe failed");
        }
        let waiters = IN_FLIGHT
            .with(|f| f.borrow_mut().take())
            .unwrap_or_default();
        for w in waiters {
            w(&result);
        }
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    use super::*;
    use crate::testutil::{in_loop, pump_until};

    /// A stand-in for `claude` (never the real one).
    pub(crate) fn fake_claude(dir: &Path, body: &str) -> String {
        let path = dir.join("fake-claude");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(&path)
            .unwrap();
        f.write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
            .unwrap();
        path.display().to_string()
    }

    const INIT_REPLY: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"catalog-1","response":{"account":{"email":"user@example.com","subscriptionType":"Claude Pro","apiProvider":"firstParty"},"models":[{"value":"opus","displayName":"Opus","supportedEffortLevels":["low","high"]}]}}}"#;
    const USAGE_REPLY: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"catalog-usage","response":{"rate_limits":{"limits":[{"kind":"session","percent":50,"resets_at":"2026-10-06T16:30:00+00:00"}]}}}}"#;

    fn answering() -> String {
        format!(
            "echo '{{\"type\":\"system\"}}'\nwhile read line; do\ncase \"$line\" in\n*initialize*) echo '{INIT_REPLY}';;\n*get_usage*) echo '{USAGE_REPLY}';;\nesac\ndone"
        )
    }

    pub(crate) fn run_probe(program: String) -> ProbeResult {
        in_loop(|ctx| {
            let out = Rc::new(RefCell::new(None));
            let o = out.clone();
            glib::spawn_future_local(async move {
                *o.borrow_mut() = Some(probe_claude(&program).await);
            });
            assert!(pump_until(ctx, 15, || out.borrow().is_some()), "no result");
            let r = out.borrow_mut().take().unwrap();
            r
        })
    }

    #[test]
    fn probe_reads_models_account_and_usage_from_one_process() {
        let dir = tempfile::tempdir().unwrap();
        let p = run_probe(fake_claude(dir.path(), &answering())).expect("probe");
        assert_eq!(p.models.len(), 1);
        assert_eq!(p.models[0].efforts, ["low", "high"]);
        assert_eq!(p.account.expect("account").label, "user@example.com");
        assert_eq!(p.windows.len(), 1);
        assert_eq!(p.windows[0].used, 0.5);
    }

    #[test]
    fn a_missing_usage_reply_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        // Answers initialize, then exits: get_usage never comes back.
        let body = format!("read line\necho '{INIT_REPLY}'\nread line\nexit 0");
        let p = run_probe(fake_claude(dir.path(), &body)).expect("probe");
        assert_eq!(p.models.len(), 1);
        assert!(p.windows.is_empty());
    }

    #[test]
    fn a_process_that_dies_or_is_missing_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            run_probe(fake_claude(dir.path(), "exit 1")),
            Err("claude exited without answering")
        );
        assert_eq!(
            run_probe("/nonexistent/claude".into()),
            Err("claude did not start")
        );
    }

    #[test]
    fn concurrent_callers_share_one_process() {
        let dir = tempfile::tempdir().unwrap();
        // Counts its own launches in a file next to the script.
        let count = dir.path().join("launches");
        let body = format!("echo x >> '{}'\n{}", count.display(), answering());
        let program = fake_claude(dir.path(), &body);
        let got = in_loop(|ctx| {
            let got = Rc::new(Cell::new(0));
            for _ in 0..3 {
                let got = got.clone();
                probe_shared(&program, move |r| {
                    assert!(r.is_ok());
                    got.set(got.get() + 1);
                });
            }
            assert!(pump_until(ctx, 15, || got.get() == 3), "callers not served");
            got.get()
        });
        assert_eq!(got, 3);
        let launches = std::fs::read_to_string(count).unwrap();
        assert_eq!(launches.lines().count(), 1, "one process for three callers");
    }

    #[test]
    fn requests_and_argv() {
        let v: Value = serde_json::from_str(&request(INIT_ID, "initialize")).unwrap();
        assert_eq!(v["type"], "control_request");
        assert_eq!(v["request"]["subtype"], "initialize");
        assert!(!request(USAGE_ID, "get_usage").contains('\n'));
        assert_eq!(argv("claude")[..2], ["claude", "--output-format"]);
        let f: Value = serde_json::from_str(INIT_REPLY).unwrap();
        assert_eq!(response_id(&f), Some(INIT_ID));
        assert_eq!(response_id(&serde_json::json!({"type": "assistant"})), None);
    }
}
