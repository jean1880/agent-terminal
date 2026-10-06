//! The live model catalog behind the unified model picker.
//!
//! Two sources feed it, both asynchronously on the GTK main loop and never blocking it:
//! - agy: `agy models` through [`run_side`] (`id<TAB>Display` lines);
//! - Claude: a short-lived `claude --input-format stream-json` process that is sent ONE
//!   `initialize` control request and nothing else (no prompt, so no turn and no cost); the
//!   `models` of its response are read and the process is terminated.
//!
//! The last good lists are persisted at `$XDG_CACHE_HOME/agent-terminal/models.json` (0600,
//! atomic write) so the picker is populated the instant the app starts. A failed refresh keeps
//! the previous list for that agent. Failures are logged without any frame or output body.

// The window wires `shared()` and `refresh()` (parallel change); until then they are unused.
#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use agent_core::adapter::Driver;
use agent_core::catalog::{parse_agy_models, parse_claude_initialize, CatalogModel};
use gtk4::glib;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::agent_proc::{run_side, AgentProcess, SpawnSpec};
use crate::chat::ModelSource;

/// The request id of the one control request sent to Claude.
const REQUEST_ID: &str = "catalog-1";
/// How long Claude may take to answer `initialize`.
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(50);
/// How long `agy models` may take.
const AGY_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------------------------
// Pure pieces
// ---------------------------------------------------------------------------------------------

/// `<cache dir>/agent-terminal/models.json`, from the values of `XDG_CACHE_HOME` and `HOME`
/// (passed in, so tests never read the real environment). A relative or empty value is ignored,
/// per the XDG spec.
pub fn cache_path(xdg_cache_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let absolute = |v: Option<&str>| v.filter(|s| Path::new(s).is_absolute()).map(PathBuf::from);
    let base = absolute(xdg_cache_home).or_else(|| absolute(home).map(|h| h.join(".cache")))?;
    Some(base.join("agent-terminal").join("models.json"))
}

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
struct CacheFile {
    #[serde(default)]
    claude: Vec<CatalogModel>,
    #[serde(default)]
    agy: Vec<CatalogModel>,
}

fn decode_cache(text: &str) -> CacheFile {
    serde_json::from_str(text).unwrap_or_default()
}

/// Writes `bytes` to `path` through a 0600 temp file in the same directory, `sync_all`, then a
/// rename, so a crash never leaves a half-written cache.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("cache path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Whether a stdout line is the `initialize` response this module asked for.
fn is_catalog_response(frame: &Value) -> bool {
    frame.get("type").and_then(Value::as_str) == Some("control_response")
        && frame
            .pointer("/response/request_id")
            .and_then(Value::as_str)
            == Some(REQUEST_ID)
}

fn initialize_request() -> String {
    serde_json::json!({
        "type": "control_request",
        "request_id": REQUEST_ID,
        "request": {"subtype": "initialize"}
    })
    .to_string()
}

fn claude_argv(program: &str) -> Vec<String> {
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

// ---------------------------------------------------------------------------------------------
// The catalog
// ---------------------------------------------------------------------------------------------

type Listener = Rc<dyn Fn()>;

pub struct ModelCatalog {
    claude: RefCell<Vec<CatalogModel>>,
    agy: RefCell<Vec<CatalogModel>>,
    listeners: RefCell<Vec<Listener>>,
    cache: Option<PathBuf>,
    refreshing: Cell<bool>,
}

thread_local! {
    static SHARED: Rc<ModelCatalog> = Rc::new(ModelCatalog::with_cache(cache_path(
        std::env::var("XDG_CACHE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )));
}

impl ModelCatalog {
    /// The app-wide catalog (one per main thread), seeded from the cache file.
    pub fn shared() -> Rc<ModelCatalog> {
        SHARED.with(Rc::clone)
    }

    /// A catalog persisting to `cache` (`None`: memory only). The cache is read here, once:
    /// a few KiB at start-up, which is the point of having it.
    fn with_cache(cache: Option<PathBuf>) -> Self {
        let loaded = cache
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| decode_cache(&t))
            .unwrap_or_default();
        Self {
            claude: RefCell::new(loaded.claude),
            agy: RefCell::new(loaded.agy),
            listeners: RefCell::new(Vec::new()),
            cache,
            refreshing: Cell::new(false),
        }
    }

    /// The current snapshot, Claude's models first. Empty until the cache or a refresh lands.
    pub fn models(&self) -> Vec<CatalogModel> {
        let mut all = self.claude.borrow().clone();
        all.extend(self.agy.borrow().iter().cloned());
        all
    }

    /// Calls `f` (on the main thread) every time the snapshot changes.
    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        self.listeners.borrow_mut().push(Rc::new(f));
    }

    /// Replaces one agent's list. An empty list is a failed fetch, not "no models": the previous
    /// list stays. Persists and notifies only on a real change.
    fn set(&self, driver: Driver, models: Vec<CatalogModel>) {
        if models.is_empty() {
            return;
        }
        let slot = match driver {
            Driver::Claude => &self.claude,
            Driver::Agy => &self.agy,
        };
        if *slot.borrow() == models {
            return;
        }
        *slot.borrow_mut() = models;
        self.save();
        // Clone out of the cell: a listener may call back into the catalog.
        let listeners: Vec<Listener> = self.listeners.borrow().clone();
        for l in listeners {
            l();
        }
    }

    fn save(&self) {
        let Some(path) = &self.cache else { return };
        let file = CacheFile {
            claude: self.claude.borrow().clone(),
            agy: self.agy.borrow().clone(),
        };
        let result = serde_json::to_vec(&file)
            .map_err(std::io::Error::other)
            .and_then(|bytes| write_atomic(path, &bytes));
        if let Err(e) = result {
            warn!(error = %e, "could not write the model cache");
        }
    }

    /// Re-fetches both lists in the background. Returns at once; a refresh already in flight
    /// makes this a no-op. Neither program is given a prompt.
    pub fn refresh(self: &Rc<Self>, claude_program: &str, agy_program: &str) {
        if self.refreshing.replace(true) {
            debug!("model catalog refresh already running");
            return;
        }
        let pending = Rc::new(Cell::new(2u8));
        let finish = {
            let me = self.clone();
            move || {
                pending.set(pending.get() - 1);
                if pending.get() == 0 {
                    me.refreshing.set(false);
                }
            }
        };

        let (me, done, program) = (self.clone(), finish.clone(), agy_program.to_owned());
        glib::spawn_future_local(async move {
            let fetch = run_side(vec![program, "models".to_owned()], None);
            match glib::future_with_timeout(AGY_TIMEOUT, fetch).await {
                Ok((out, true)) => {
                    let models = parse_agy_models(&out);
                    if models.is_empty() {
                        warn!("agy models listed nothing");
                    }
                    info!(count = models.len(), "agy model list fetched");
                    me.set(Driver::Agy, models);
                }
                Ok((_, false)) => warn!("agy models failed"),
                Err(_) => warn!("agy models timed out"),
            }
            done();
        });

        let (me, program) = (self.clone(), claude_program.to_owned());
        glib::spawn_future_local(async move {
            match fetch_claude(&program).await {
                Ok(models) => {
                    info!(count = models.len(), "claude model list fetched");
                    me.set(Driver::Claude, models);
                }
                Err(why) => warn!(why, "claude model list not fetched"),
            }
            finish();
        });
    }
}

/// Spawns Claude, sends the single `initialize` request, waits for its response, terminates.
async fn fetch_claude(program: &str) -> Result<Vec<CatalogModel>, &'static str> {
    let answer: Rc<RefCell<Option<Vec<CatalogModel>>>> = Rc::default();
    let exited = Rc::new(Cell::new(false));
    let spec = SpawnSpec {
        argv: claude_argv(program),
        ..SpawnSpec::default()
    };
    let on_line = {
        let answer = answer.clone();
        move |line: &str| {
            let Ok(frame) = serde_json::from_str::<Value>(line) else {
                return;
            };
            if is_catalog_response(&frame) {
                answer
                    .borrow_mut()
                    .get_or_insert_with(|| parse_claude_initialize(&frame));
            }
        }
    };
    let on_exit = {
        let exited = exited.clone();
        move |_| exited.set(true)
    };
    let proc =
        AgentProcess::spawn(&spec, on_line, |_| {}, on_exit).map_err(|_| "claude did not start")?;
    proc.write_line(&initialize_request());

    // Poll on the main loop (no channel crate): the answer is one line, so 50 ms is plenty.
    let mut waited = Duration::ZERO;
    while answer.borrow().is_none() && !exited.get() && waited < CLAUDE_TIMEOUT {
        glib::timeout_future(POLL).await;
        waited += POLL;
    }
    // Dropping `proc` would terminate it too; do it explicitly so the intent is visible.
    proc.terminate();
    let got = answer.borrow_mut().take();
    match got {
        Some(models) if !models.is_empty() => Ok(models),
        Some(_) => Err("claude listed no models"),
        None if exited.get() => Err("claude exited without answering"),
        None => Err("claude timed out"),
    }
}

impl ModelSource for ModelCatalog {
    fn models(&self) -> Vec<CatalogModel> {
        ModelCatalog::models(self)
    }

    fn connect_changed(&self, f: Box<dyn Fn()>) {
        ModelCatalog::connect_changed(self, f);
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn model(driver: Driver, id: &str) -> CatalogModel {
        CatalogModel {
            driver,
            id: id.into(),
            display: id.into(),
            description: None,
            efforts: vec![],
            via: None,
        }
    }

    #[test]
    fn cache_path_follows_xdg_rules() {
        let p = |x, h| cache_path(x, h).map(|p| p.display().to_string());
        let expect = |s: &str| Some(s.to_owned());
        assert_eq!(
            p(Some("/x/cache"), Some("/home/u")),
            expect("/x/cache/agent-terminal/models.json")
        );
        assert_eq!(
            p(None, Some("/home/u")),
            expect("/home/u/.cache/agent-terminal/models.json")
        );
        // Relative or empty XDG values are ignored.
        assert_eq!(
            p(Some("rel"), Some("/home/u")),
            expect("/home/u/.cache/agent-terminal/models.json")
        );
        assert_eq!(
            p(Some(""), Some("/home/u")),
            expect("/home/u/.cache/agent-terminal/models.json")
        );
        assert_eq!(p(None, None), None);
        assert_eq!(p(None, Some("relative")), None);
    }

    #[test]
    fn cache_round_trips_and_garbage_decodes_to_empty() {
        let file = CacheFile {
            claude: vec![model(Driver::Claude, "opus")],
            agy: vec![model(Driver::Agy, "gemini-3.1-pro-high")],
        };
        let text = serde_json::to_string(&file).unwrap();
        assert_eq!(decode_cache(&text), file);
        assert_eq!(decode_cache("not json"), CacheFile::default());
        assert_eq!(decode_cache("{\"agy\": 3}"), CacheFile::default());
        assert_eq!(decode_cache("{}"), CacheFile::default());
    }

    #[test]
    fn atomic_write_is_private_replaces_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("models.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["models.json"]);
    }

    #[test]
    fn catalog_persists_notifies_and_keeps_the_last_good_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        let cat = ModelCatalog::with_cache(Some(path.clone()));
        assert!(cat.models().is_empty());
        let hits = Rc::new(Cell::new(0));
        let h = hits.clone();
        cat.connect_changed(move || h.set(h.get() + 1));

        cat.set(Driver::Agy, vec![model(Driver::Agy, "g")]);
        cat.set(Driver::Claude, vec![model(Driver::Claude, "opus")]);
        assert_eq!(hits.get(), 2);
        // Unchanged and empty (failed) lists neither notify nor clobber.
        cat.set(Driver::Agy, vec![model(Driver::Agy, "g")]);
        cat.set(Driver::Agy, vec![]);
        assert_eq!(hits.get(), 2);
        let ids: Vec<_> = cat.models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["opus", "g"], "Claude first");

        // A fresh catalog is populated from the file straight away.
        let again = ModelCatalog::with_cache(Some(path));
        let ids: Vec<_> = again.models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["opus", "g"]);
    }

    #[test]
    fn only_the_catalog_response_is_recognised() {
        let ours = serde_json::json!({"type": "control_response",
            "response": {"subtype": "success", "request_id": "catalog-1", "response": {"models": []}}});
        let other = serde_json::json!({"type": "control_response",
            "response": {"request_id": "init-1"}});
        let wrong_type = serde_json::json!({"type": "assistant",
            "response": {"request_id": "catalog-1"}});
        assert!(is_catalog_response(&ours));
        assert!(!is_catalog_response(&other));
        assert!(!is_catalog_response(&wrong_type));
    }

    /// A stand-in for `claude` (never the real one): answers the first stdin line.
    fn fake_claude(dir: &Path, body: &str) -> String {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
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

    fn run_fetch(program: String) -> Result<Vec<CatalogModel>, &'static str> {
        use crate::testutil::{in_loop, pump_until};
        in_loop(|ctx| {
            let out = Rc::new(RefCell::new(None));
            let o = out.clone();
            glib::spawn_future_local(async move {
                *o.borrow_mut() = Some(fetch_claude(&program).await);
            });
            assert!(pump_until(ctx, 15, || out.borrow().is_some()), "no result");
            let r = out.borrow_mut().take().unwrap();
            r
        })
    }

    #[test]
    fn fetch_claude_reads_the_initialize_response_and_stops_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let reply = r#"{"type":"control_response","response":{"subtype":"success","request_id":"catalog-1","response":{"models":[{"value":"opus","displayName":"Opus","supportedEffortLevels":["low","high"]}]}}}"#;
        let program = fake_claude(
            dir.path(),
            &format!("echo '{{\"type\":\"system\"}}'\nread line\necho '{reply}'\nexec sleep 30"),
        );
        let models = run_fetch(program).expect("models");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "opus");
        assert_eq!(models[0].efforts, ["low", "high"]);
    }

    #[test]
    fn fetch_claude_reports_a_process_that_dies_silently() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_claude(dir.path(), "exit 1");
        assert_eq!(run_fetch(program), Err("claude exited without answering"));
        assert_eq!(
            run_fetch("/nonexistent/claude".into()),
            Err("claude did not start")
        );
    }

    #[test]
    fn the_request_is_initialize_only() {
        let v: Value = serde_json::from_str(&initialize_request()).unwrap();
        assert_eq!(v["type"], "control_request");
        assert_eq!(v["request_id"], REQUEST_ID);
        assert_eq!(v["request"]["subtype"], "initialize");
        assert!(!initialize_request().contains('\n'));
        assert_eq!(
            claude_argv("claude"),
            [
                "claude",
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json"
            ]
        );
    }
}
