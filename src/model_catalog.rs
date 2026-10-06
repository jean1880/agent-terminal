//! The live model catalog behind the unified model picker.
//!
//! Three sources feed it, all asynchronously on the GTK main loop and never blocking it:
//! - agy: `agy models` through [`run_side`] (`id<TAB>Display` lines);
//! - Claude: the shared [`crate::claude_probe`] (an `initialize` control request and no prompt,
//!   so no turn and no cost); the `models` of its response are read;
//! - Codex: the shared [`crate::codex_probe`] (`initialize` then `model/list` on a throwaway
//!   `codex app-server`; no thread or turn is ever started).
//!
//! Every probe runs in the environment its agent's threads get (see [`ProbeTargets`]).
//!
//! The last good lists are persisted at `$XDG_CACHE_HOME/agent-terminal/models.json` (0600,
//! atomic write) so the picker is populated the instant the app starts. A failed refresh keeps
//! the previous list for that agent. Failures are logged without any frame or output body.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use agent_core::adapter::Driver;
use agent_core::catalog::{parse_agy_models, CatalogModel};
use gtk4::glib;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::agent_proc::{run_side, AGY_TIMEOUT};
use crate::chat::ModelSource;
use crate::probe::{join_n, ListenerSet, ProbeTargets};
use crate::{claude_probe, codex_probe};

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
    #[serde(default)]
    codex: Vec<CatalogModel>,
}

fn decode_cache(text: &str) -> CacheFile {
    serde_json::from_str(text).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// The catalog
// ---------------------------------------------------------------------------------------------

pub struct ModelCatalog {
    claude: RefCell<Vec<CatalogModel>>,
    agy: RefCell<Vec<CatalogModel>>,
    codex: RefCell<Vec<CatalogModel>>,
    /// The agents whose list was fetched in this run (not just read from the cache).
    fresh: RefCell<Vec<Driver>>,
    listeners: ListenerSet,
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
            codex: RefCell::new(loaded.codex),
            fresh: RefCell::new(Vec::new()),
            listeners: ListenerSet::default(),
            cache,
            refreshing: Cell::new(false),
        }
    }

    /// The current snapshot, Claude's models first, then agy's, then Codex's. Empty until the
    /// cache or a refresh lands.
    pub fn models(&self) -> Vec<CatalogModel> {
        let mut all = self.claude.borrow().clone();
        all.extend(self.agy.borrow().iter().cloned());
        all.extend(self.codex.borrow().iter().cloned());
        all
    }

    /// Calls `f` (on the main thread) every time the snapshot changes. The id disconnects it.
    pub fn connect_changed(&self, f: impl Fn() + 'static) -> u64 {
        self.listeners.add(f)
    }

    pub fn disconnect(&self, id: u64) {
        self.listeners.remove(id);
    }

    /// Tells the listeners the snapshot, or what filters it (availability), changed.
    pub fn notify(&self) {
        self.listeners.notify();
    }

    /// One agent's models (the cache or the last fetch), whatever the agent's availability.
    pub fn models_of(&self, driver: Driver) -> Vec<CatalogModel> {
        self.slot(driver).borrow().clone()
    }

    /// Whether `driver`'s list came from a fetch in this run. The cache alone is not enough to
    /// call a model retired: a list that has not been refreshed yet may be out of date.
    pub fn is_fresh(&self, driver: Driver) -> bool {
        self.fresh.borrow().contains(&driver)
    }

    fn slot(&self, driver: Driver) -> &RefCell<Vec<CatalogModel>> {
        match driver {
            Driver::Claude => &self.claude,
            Driver::Agy => &self.agy,
            Driver::Codex => &self.codex,
        }
    }

    /// Replaces one agent's list with a fetched one. An empty list is a failed fetch, not "no
    /// models": the previous list stays (and stays unfresh). Persists on a change, and notifies on
    /// a change or when the list first becomes fresh.
    fn set(&self, driver: Driver, models: Vec<CatalogModel>) {
        if models.is_empty() {
            return;
        }
        let newly_fresh = {
            let mut fresh = self.fresh.borrow_mut();
            let new = !fresh.contains(&driver);
            if new {
                fresh.push(driver);
            }
            new
        };
        let slot = self.slot(driver);
        let changed = *slot.borrow() != models;
        if changed {
            *slot.borrow_mut() = models;
            self.save();
        }
        if changed || newly_fresh {
            self.listeners.notify();
        }
    }

    fn save(&self) {
        let Some(path) = &self.cache else { return };
        let file = CacheFile {
            claude: self.claude.borrow().clone(),
            agy: self.agy.borrow().clone(),
            codex: self.codex.borrow().clone(),
        };
        let result = serde_json::to_vec(&file)
            .map_err(std::io::Error::other)
            .and_then(|bytes| agent_kit::fsutil::write_private_atomic(path, &bytes));
        if let Err(e) = result {
            warn!(error = %e, "could not write the model cache");
        }
    }

    /// Re-fetches every agent's list in the background. Returns at once; a refresh already in
    /// flight makes this a no-op. No program is given a prompt, and each runs in the environment
    /// its threads get.
    pub fn refresh(self: &Rc<Self>, targets: &ProbeTargets) {
        if self.refreshing.replace(true) {
            debug!("model catalog refresh already running");
            return;
        }
        if targets.count() == 0 {
            self.refreshing.set(false);
            return; // nothing is ready: nothing is spawned
        }
        let finish = {
            let me = self.clone();
            join_n(targets.count(), move || me.refreshing.set(false))
        };
        for driver in Driver::ALL {
            let Some(target) = targets.get(driver) else {
                continue; // missing, disabled or still being detected
            };
            let (me, done) = (self.clone(), finish.clone());
            match driver {
                // `agy models`: `id<TAB>Display` lines.
                Driver::Agy => {
                    let target = target.clone();
                    glib::spawn_future_local(async move {
                        let (out, ok) = run_side(
                            vec![target.program, "models".to_owned()],
                            None,
                            &target.env,
                            AGY_TIMEOUT,
                        )
                        .await;
                        if ok {
                            let models = parse_agy_models(&out);
                            if models.is_empty() {
                                warn!("agy models listed nothing");
                            }
                            info!(count = models.len(), "agy model list fetched");
                            me.set(Driver::Agy, models);
                        } else {
                            warn!("agy models failed or timed out");
                        }
                        done();
                    });
                }
                // One Claude process serves this and the usage indicator (see `claude_probe`).
                Driver::Claude => {
                    claude_probe::probe_shared(&target.program, &target.env, move |result| {
                        if let Ok(probe) = result {
                            info!(count = probe.models.len(), "claude model list fetched");
                            me.set(Driver::Claude, probe.models.clone());
                        }
                        done();
                    });
                }
                // Likewise one `codex app-server`, asked for models and the account, never a turn.
                Driver::Codex => {
                    codex_probe::probe_shared(&target.program, &target.env, move |result| {
                        if let Ok(probe) = result {
                            info!(count = probe.models.len(), "codex model list fetched");
                            me.set(Driver::Codex, probe.models.clone());
                        }
                        done();
                    });
                }
            }
        }
    }
}

/// What an in-thread banner says about a model its agent no longer offers: `None` when nothing is
/// wrong. `agent` is the agent's name.
pub fn retirement_banner(agent: &str, notice: &agent_core::catalog::ModelNotice) -> Option<String> {
    use agent_core::catalog::ModelNotice;
    match notice {
        ModelNotice::None => None,
        ModelNotice::Retired {
            model,
            replacement: Some(r),
        } => Some(format!(
            "{model} is no longer offered by {agent}. Your next message continues on {}.",
            r.display
        )),
        ModelNotice::Retired {
            model,
            replacement: None,
        } => Some(format!(
            "{model} is no longer offered by {agent}. Choose another model."
        )),
    }
}

/// `models` without those of agents that are not `ready`. The catalogue keeps (and caches) every
/// agent's list; the pickers show only what can be used right now.
pub fn only_ready(models: Vec<CatalogModel>, ready: impl Fn(Driver) -> bool) -> Vec<CatalogModel> {
    models.into_iter().filter(|m| ready(m.driver)).collect()
}

impl ModelSource for ModelCatalog {
    /// The picker's rows: agents that are not ready are left out, cached models or not.
    fn models(&self) -> Vec<CatalogModel> {
        let availability = crate::availability::AgentAvailability::shared();
        only_ready(ModelCatalog::models(self), |d| availability.is_ready(d))
    }

    fn connect_changed(&self, f: Box<dyn Fn()>) -> u64 {
        ModelCatalog::connect_changed(self, f)
    }

    fn disconnect(&self, id: u64) {
        ModelCatalog::disconnect(self, id);
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
            default_effort: None,
            via: None,
        }
    }

    #[test]
    fn the_picker_hides_agents_that_are_not_ready_but_the_cache_keeps_them() {
        let dir = tempfile::tempdir().unwrap();
        let cat = ModelCatalog::with_cache(Some(dir.path().join("models.json")));
        cat.set(Driver::Claude, vec![model(Driver::Claude, "opus")]);
        cat.set(Driver::Agy, vec![model(Driver::Agy, "g")]);
        let shown = only_ready(cat.models(), |d| d == Driver::Claude);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].driver, Driver::Claude);
        assert_eq!(
            cat.models_of(Driver::Agy).len(),
            1,
            "kept for when it is back"
        );
    }

    #[test]
    fn the_retirement_banner_names_the_model_the_agent_and_where_it_goes() {
        use agent_core::catalog::{ModelNotice, Replacement};
        assert_eq!(retirement_banner("Claude", &ModelNotice::None), None);
        let with = ModelNotice::Retired {
            model: "claude-sonnet-3-7".into(),
            replacement: Some(Replacement {
                model: "sonnet".into(),
                display: "Sonnet".into(),
                effort: None,
            }),
        };
        assert_eq!(
            retirement_banner("Claude", &with).as_deref(),
            Some("claude-sonnet-3-7 is no longer offered by Claude. Your next message continues on Sonnet.")
        );
        let without = ModelNotice::Retired {
            model: "x".into(),
            replacement: None,
        };
        assert!(retirement_banner("Codex", &without)
            .is_some_and(|t| t.ends_with("Choose another model.")));
    }

    #[test]
    fn a_list_is_fresh_only_after_a_fetch_in_this_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        let cat = ModelCatalog::with_cache(Some(path.clone()));
        cat.set(Driver::Claude, vec![model(Driver::Claude, "opus")]);
        assert!(cat.is_fresh(Driver::Claude));
        assert!(!cat.is_fresh(Driver::Agy));
        // The same list read back from the cache is known but not fresh.
        let again = ModelCatalog::with_cache(Some(path));
        assert_eq!(again.models_of(Driver::Claude).len(), 1);
        assert!(!again.is_fresh(Driver::Claude));
        // Fetching the same list again makes it fresh, and tells the listeners once.
        let hits = Rc::new(Cell::new(0));
        let h = hits.clone();
        again.connect_changed(move || h.set(h.get() + 1));
        again.set(Driver::Claude, vec![model(Driver::Claude, "opus")]);
        again.set(Driver::Claude, vec![model(Driver::Claude, "opus")]);
        assert!(again.is_fresh(Driver::Claude));
        assert_eq!(hits.get(), 1);
        // A failed (empty) fetch changes nothing.
        again.set(Driver::Agy, vec![]);
        assert!(!again.is_fresh(Driver::Agy));
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
            codex: vec![model(Driver::Codex, "gpt-5-codex")],
        };
        let text = serde_json::to_string(&file).unwrap();
        assert_eq!(decode_cache(&text), file);
        assert_eq!(decode_cache("not json"), CacheFile::default());
        assert_eq!(decode_cache("{\"agy\": 3}"), CacheFile::default());
        assert_eq!(decode_cache("{}"), CacheFile::default());
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
        cat.set(Driver::Codex, vec![model(Driver::Codex, "gpt")]);
        assert_eq!(hits.get(), 3);
        // Unchanged and empty (failed) lists neither notify nor clobber.
        cat.set(Driver::Agy, vec![model(Driver::Agy, "g")]);
        cat.set(Driver::Agy, vec![]);
        assert_eq!(hits.get(), 3);
        let ids: Vec<_> = cat.models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["opus", "g", "gpt"], "Claude first, Codex last");

        // The cache is private (the shared helper is tested in agent-kit).
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // A fresh catalog is populated from the file straight away.
        let again = ModelCatalog::with_cache(Some(path));
        let ids: Vec<_> = again.models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["opus", "g", "gpt"]);
    }
}
