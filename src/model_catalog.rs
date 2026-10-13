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
//! atomic write) so the picker is populated the instant the app starts, with the time of each
//! list's last successful fetch. A failed refresh keeps the previous list for that agent.
//! Failures are logged without any frame or output body.
//!
//! When to fetch: once per agent per run (the start-up round), then only when due
//! ([`crate::freshness::MODEL_TTL`] in the background, [`crate::freshness::MODEL_PICKER_AGE`]
//! when the picker opens), when an agent comes back from missing, or when the user asks.
//! Regaining focus alone never spawns a fetch.

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
use crate::freshness::{age_of, is_due, now_epoch, CatalogStatus, MODEL_PICKER_AGE, MODEL_TTL};
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
    /// When each list was last fetched successfully (epoch seconds), so a restart cannot make an
    /// old list look new. A file from before this field reads as never fetched: Expired.
    #[serde(default)]
    read_at: ReadAt,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq)]
struct ReadAt {
    #[serde(default)]
    claude: Option<i64>,
    #[serde(default)]
    agy: Option<i64>,
    #[serde(default)]
    codex: Option<i64>,
}

impl ReadAt {
    fn get(&self, driver: Driver) -> Option<i64> {
        match driver {
            Driver::Claude => self.claude,
            Driver::Agy => self.agy,
            Driver::Codex => self.codex,
        }
    }

    fn set(&mut self, driver: Driver, at: Option<i64>) {
        match driver {
            Driver::Claude => self.claude = at,
            Driver::Agy => self.agy = at,
            Driver::Codex => self.codex = at,
        }
    }
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
    /// The agents whose fetch in this run has finished, whether or not it succeeded.
    attempted: RefCell<Vec<Driver>>,
    listeners: ListenerSet,
    /// Told when a fetch starts or ends, or fails: the picker's footer. Separate from
    /// `listeners`, which mean "the lists changed" and re-judge every thread's model.
    status_listeners: ListenerSet,
    cache: Option<PathBuf>,
    refreshing: Cell<bool>,
    /// Each list's last successful fetch (persisted with the lists).
    read_at: Cell<ReadAt>,
    /// Each agent's last finished fetch in this run, successful or not: the due clock, so a
    /// failing agent is not fetched again on every check.
    tried_at: Cell<ReadAt>,
    /// The agents whose last fetch failed (no list, an error or a timeout).
    failed: RefCell<Vec<Driver>>,
    /// Agents to fetch at the next check whatever their clock says (missing, now found).
    forced: RefCell<Vec<Driver>>,
    /// The probe targets of the last scan, for a refresh the picker asks for.
    targets: RefCell<ProbeTargets>,
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
    pub(crate) fn with_cache(cache: Option<PathBuf>) -> Self {
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
            attempted: RefCell::new(Vec::new()),
            listeners: ListenerSet::default(),
            status_listeners: ListenerSet::default(),
            cache,
            refreshing: Cell::new(false),
            read_at: Cell::new(loaded.read_at),
            tried_at: Cell::new(ReadAt::default()),
            failed: RefCell::new(Vec::new()),
            forced: RefCell::new(Vec::new()),
            targets: RefCell::new(ProbeTargets::default()),
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
    /// call a model retired: a list that has not been refreshed yet may be out of date. This is
    /// the only predicate argv decisions use; how old a list *looks* is [`Self::status`]'s job.
    pub fn is_fresh(&self, driver: Driver) -> bool {
        self.fresh.borrow().contains(&driver)
    }

    /// Whether `driver`'s fetch in this run has finished, successfully or not: past this point
    /// waiting longer for a fresh list gains nothing.
    pub fn is_settled(&self, driver: Driver) -> bool {
        self.is_fresh(driver) || self.attempted.borrow().contains(&driver)
    }

    /// Whether a session start may skip waiting for `driver`'s fetch: it has settled, or the
    /// cached list was fetched within [`MODEL_TTL`]. A start that skips the wait still never
    /// ports a model on the cached list ([`Self::is_fresh`] stays false); the fetch landing
    /// later re-judges the thread.
    pub fn may_start_without_waiting(&self, driver: Driver, now: i64) -> bool {
        self.is_settled(driver)
            || (!self.slot(driver).borrow().is_empty()
                && age_of(self.read_at.get().get(driver), now).is_some_and(|a| a < MODEL_TTL))
    }

    fn mark_attempted(&self, driver: Driver) {
        self.mark_attempted_at(driver, now_epoch());
    }

    fn mark_attempted_at(&self, driver: Driver, now: i64) {
        let mut tried = self.tried_at.get();
        tried.set(driver, Some(now));
        self.tried_at.set(tried);
        let mut attempted = self.attempted.borrow_mut();
        if !attempted.contains(&driver) {
            attempted.push(driver);
        }
    }

    /// Records a failed fetch: the list stays, and is shown as not refreshed.
    fn mark_failed(&self, driver: Driver) {
        let mut failed = self.failed.borrow_mut();
        if !failed.contains(&driver) {
            failed.push(driver);
        }
    }

    /// Fetch `driver` at the next check whatever its clock says (it was missing and is back).
    pub fn mark_due(&self, driver: Driver) {
        let mut forced = self.forced.borrow_mut();
        if !forced.contains(&driver) {
            forced.push(driver);
        }
    }

    /// Whether `driver`'s list should be fetched at `now` by a background check: forced, not yet
    /// fetched in this run (the one start-up round), or older than `ttl` since its last success
    /// or attempt. An empty list is retried after [`MODEL_PICKER_AGE`] instead of `ttl`.
    fn is_due(&self, driver: Driver, now: i64, ttl: i64) -> bool {
        if self.forced.borrow().contains(&driver) || !self.attempted.borrow().contains(&driver) {
            return true;
        }
        let last = match (
            self.read_at.get().get(driver),
            self.tried_at.get().get(driver),
        ) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let ttl = if self.slot(driver).borrow().is_empty() {
            ttl.min(MODEL_PICKER_AGE)
        } else {
            ttl
        };
        is_due(last, now, ttl)
    }

    /// The footer's view of the lists of the `shown` agents.
    pub fn status_of(&self, shown: impl Fn(Driver) -> bool) -> CatalogStatus {
        let read_at = self.read_at.get();
        let shown: Vec<Driver> = Driver::ALL.into_iter().filter(|d| shown(*d)).collect();
        let failed = shown.iter().any(|d| self.failed.borrow().contains(d));
        // The oldest read among the shown agents that have a list; one without a time is unknown.
        let mut oldest: Option<Option<i64>> = None;
        for d in shown {
            if self.slot(d).borrow().is_empty() {
                continue;
            }
            let at = read_at.get(d);
            oldest = Some(match (oldest, at) {
                (None, at) => at,
                (Some(Some(o)), Some(a)) => Some(o.min(a)),
                (Some(_), _) => None,
            });
        }
        CatalogStatus {
            refreshing: self.refreshing.get(),
            failed,
            read_at: oldest.flatten(),
        }
    }

    /// Calls `f` when a fetch starts or ends; the id disconnects it ([`Self::disconnect_status`]).
    pub fn connect_status_changed(&self, f: impl Fn() + 'static) -> u64 {
        self.status_listeners.add(f)
    }

    pub fn disconnect_status(&self, id: u64) {
        self.status_listeners.remove(id);
    }

    fn slot(&self, driver: Driver) -> &RefCell<Vec<CatalogModel>> {
        match driver {
            Driver::Claude => &self.claude,
            Driver::Agy => &self.agy,
            Driver::Codex => &self.codex,
        }
    }

    /// Replaces one agent's list with a fetched one. An empty list is a failed fetch, not "no
    /// models": the previous list stays (and stays unfresh). Persists the list and the time of
    /// this success, and notifies on a change or when the list first becomes fresh.
    fn set(&self, driver: Driver, models: Vec<CatalogModel>) {
        self.set_at(driver, models, now_epoch());
    }

    fn set_at(&self, driver: Driver, models: Vec<CatalogModel>, now: i64) {
        if models.is_empty() {
            self.mark_failed(driver);
            return;
        }
        self.failed.borrow_mut().retain(|d| *d != driver);
        let mut read_at = self.read_at.get();
        read_at.set(driver, Some(now));
        self.read_at.set(read_at);
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
        }
        // Saved on every success, unchanged list or not: the time is what makes it Fresh after
        // a restart. At most once per fetch, so about once an hour per agent.
        self.save();
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
            read_at: self.read_at.get(),
        };
        let result = serde_json::to_vec(&file)
            .map_err(std::io::Error::other)
            .and_then(|bytes| agent_kit::fsutil::write_private_atomic(path, &bytes));
        if let Err(e) = result {
            warn!(error = %e, "could not write the model cache");
        }
    }

    /// Re-fetches every ready agent's list in the background, whatever its clock says (Settings
    /// changed the agent, or the user asked). Returns at once; a refresh already in flight makes
    /// this a no-op. No program is given a prompt, and each runs in the environment its threads
    /// get.
    pub fn refresh(self: &Rc<Self>, targets: &ProbeTargets) {
        *self.targets.borrow_mut() = targets.clone();
        let all: Vec<Driver> = Driver::ALL
            .into_iter()
            .filter(|d| targets.get(*d).is_some())
            .collect();
        self.fetch(targets, &all);
    }

    /// The background check (a scan on focus, the periodic timer): fetches only the lists that
    /// are due at `now` against [`MODEL_TTL`] ([`Self::is_due`]). Nothing due: nothing spawns.
    pub fn refresh_if_due(self: &Rc<Self>, targets: &ProbeTargets, now: i64) {
        *self.targets.borrow_mut() = targets.clone();
        self.fetch_due(now, MODEL_TTL);
    }

    /// The picker opened: lists older than [`MODEL_PICKER_AGE`] are fetched in the background
    /// while the cached ones show. With `force` (its Refresh button) every ready list is.
    pub fn refresh_for_picker(self: &Rc<Self>, force: bool, now: i64) {
        if force {
            let targets = self.targets.borrow().clone();
            self.refresh(&targets);
        } else {
            self.fetch_due(now, MODEL_PICKER_AGE);
        }
    }

    fn fetch_due(self: &Rc<Self>, now: i64, ttl: i64) {
        let targets = self.targets.borrow().clone();
        let due: Vec<Driver> = Driver::ALL
            .into_iter()
            .filter(|d| targets.get(*d).is_some() && self.is_due(*d, now, ttl))
            .collect();
        self.fetch(&targets, &due);
    }

    /// Fetches `drivers`' lists (each must have a target).
    fn fetch(self: &Rc<Self>, targets: &ProbeTargets, drivers: &[Driver]) {
        if drivers.is_empty() {
            return; // nothing is ready or due: nothing is spawned
        }
        if self.refreshing.replace(true) {
            debug!("model catalog refresh already running");
            return;
        }
        self.forced.borrow_mut().retain(|d| !drivers.contains(d));
        self.status_listeners.notify();
        let finish = {
            let me = self.clone();
            join_n(u8::try_from(drivers.len()).unwrap_or(u8::MAX), move || {
                me.refreshing.set(false);
                me.status_listeners.notify();
            })
        };
        for &driver in drivers {
            let Some(target) = targets.get(driver) else {
                finish(); // cannot happen (callers filter), but never leave the join hanging
                continue;
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
                            me.mark_failed(Driver::Agy);
                        }
                        me.mark_attempted(Driver::Agy);
                        done();
                    });
                }
                // One Claude process serves this and the usage indicator (see `claude_probe`).
                Driver::Claude => {
                    claude_probe::probe_shared(&target.program, &target.env, move |result| {
                        match result {
                            Ok(probe) => {
                                info!(count = probe.models.len(), "claude model list fetched");
                                me.set(Driver::Claude, probe.models.clone());
                            }
                            Err(_) => me.mark_failed(Driver::Claude),
                        }
                        me.mark_attempted(Driver::Claude);
                        done();
                    });
                }
                // Likewise one `codex app-server`, asked for models and the account, never a turn.
                Driver::Codex => {
                    codex_probe::probe_shared(&target.program, &target.env, move |result| {
                        match result {
                            Ok(probe) => {
                                info!(count = probe.models.len(), "codex model list fetched");
                                me.set(Driver::Codex, probe.models.clone());
                            }
                            Err(_) => me.mark_failed(Driver::Codex),
                        }
                        me.mark_attempted(Driver::Codex);
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

    /// The footer covers the agents the picker shows (the ready ones).
    fn status(&self) -> Option<CatalogStatus> {
        let availability = crate::availability::AgentAvailability::shared();
        Some(self.status_of(|d| availability.is_ready(d)))
    }

    fn refresh(self: Rc<Self>, force: bool) {
        self.refresh_for_picker(force, now_epoch());
    }

    fn connect_status_changed(&self, f: Box<dyn Fn()>) -> Option<u64> {
        Some(ModelCatalog::connect_status_changed(self, f))
    }

    fn disconnect_status(&self, id: u64) {
        ModelCatalog::disconnect_status(self, id);
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
        // A finished fetch settles the wait even when it failed, without making the list fresh.
        assert!(!again.is_settled(Driver::Agy));
        again.mark_attempted(Driver::Agy);
        assert!(again.is_settled(Driver::Agy));
        assert!(!again.is_fresh(Driver::Agy));
        assert!(again.is_settled(Driver::Claude), "fresh is settled");
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
            read_at: ReadAt {
                claude: Some(1_700_000_000),
                agy: None,
                codex: Some(1_700_000_100),
            },
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

    use crate::freshness::{freshness, Freshness, DUE_SLACK};

    const NOW: i64 = 1_800_000_000;

    /// A catalog whose Claude list was fetched (in this run) at `at`.
    fn fetched_at(at: i64) -> ModelCatalog {
        let cat = ModelCatalog::with_cache(None);
        cat.set_at(Driver::Claude, vec![model(Driver::Claude, "opus")], at);
        cat.mark_attempted_at(Driver::Claude, at);
        cat
    }

    #[test]
    fn a_list_is_due_once_per_run_then_by_its_clock() {
        let cat = ModelCatalog::with_cache(None);
        // Never fetched in this run: due (the start-up round).
        assert!(cat.is_due(Driver::Claude, NOW, MODEL_TTL));
        let cat = fetched_at(NOW);
        assert!(!cat.is_due(Driver::Claude, NOW + 60, MODEL_TTL), "fresh");
        assert!(!cat.is_due(Driver::Claude, NOW + MODEL_TTL - DUE_SLACK - 1, MODEL_TTL));
        assert!(cat.is_due(Driver::Claude, NOW + MODEL_TTL, MODEL_TTL));
        // The picker's shorter age.
        assert!(!cat.is_due(Driver::Claude, NOW + 60, MODEL_PICKER_AGE));
        assert!(cat.is_due(Driver::Claude, NOW + MODEL_PICKER_AGE, MODEL_PICKER_AGE));
        // A timestamp in the future (the clock moved back) is due.
        assert!(cat.is_due(Driver::Claude, NOW - 3600, MODEL_TTL));
        // Back from missing: due whatever the clock says, until a fetch starts.
        cat.mark_due(Driver::Claude);
        assert!(cat.is_due(Driver::Claude, NOW + 60, MODEL_TTL));
    }

    #[test]
    fn a_failed_fetch_restarts_the_clock_but_keeps_the_list_and_says_so() {
        let cat = fetched_at(NOW);
        // A failed fetch at NOW + TTL: not retried on the next check, and the list stays.
        cat.set_at(Driver::Claude, vec![], NOW + MODEL_TTL);
        cat.mark_attempted_at(Driver::Claude, NOW + MODEL_TTL);
        assert!(!cat.is_due(Driver::Claude, NOW + MODEL_TTL + 60, MODEL_TTL));
        assert_eq!(cat.models_of(Driver::Claude).len(), 1);
        let st = cat.status_of(|d| d == Driver::Claude);
        assert!(st.failed);
        assert_eq!(st.read_at, Some(NOW), "age counts from the last success");
        // An empty list is retried after the picker's age, not the full TTL.
        let empty = ModelCatalog::with_cache(None);
        empty.mark_attempted_at(Driver::Agy, NOW);
        assert!(!empty.is_due(Driver::Agy, NOW + 60, MODEL_TTL));
        assert!(empty.is_due(Driver::Agy, NOW + MODEL_PICKER_AGE, MODEL_TTL));
        // The next success clears the failure.
        cat.set_at(
            Driver::Claude,
            vec![model(Driver::Claude, "opus")],
            NOW + 2 * MODEL_TTL,
        );
        assert!(!cat.status_of(|d| d == Driver::Claude).failed);
    }

    #[test]
    fn an_old_cache_without_times_reads_as_expired_and_due() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        // The format before read times were stored.
        let old = serde_json::json!({ "claude": [model(Driver::Claude, "opus")] });
        std::fs::write(&path, old.to_string()).unwrap();
        let cat = ModelCatalog::with_cache(Some(path));
        assert_eq!(cat.models_of(Driver::Claude).len(), 1, "still shown");
        let st = cat.status_of(|d| d == Driver::Claude);
        assert_eq!(st.read_at, None);
        assert_eq!(
            freshness(age_of(st.read_at, NOW), MODEL_TTL),
            Freshness::Expired
        );
        assert!(!cat.may_start_without_waiting(Driver::Claude, NOW));
        assert!(cat.is_due(Driver::Claude, NOW, MODEL_TTL));
    }

    #[test]
    fn the_read_time_survives_a_restart_so_old_lists_stay_old() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        let cat = ModelCatalog::with_cache(Some(path.clone()));
        cat.set_at(Driver::Claude, vec![model(Driver::Claude, "opus")], NOW);
        // The same list again an hour later: unchanged, but its time is saved.
        cat.set_at(
            Driver::Claude,
            vec![model(Driver::Claude, "opus")],
            NOW + 3600,
        );
        let again = ModelCatalog::with_cache(Some(path));
        assert_eq!(
            again.status_of(|d| d == Driver::Claude).read_at,
            Some(NOW + 3600)
        );
        assert!(!again.is_fresh(Driver::Claude), "cached is not this run");
        // The oldest shown list sets the footer's age; one with no list is left out.
        again.set_at(Driver::Codex, vec![model(Driver::Codex, "gpt")], NOW + 7200);
        assert_eq!(again.status_of(|_| true).read_at, Some(NOW + 3600));
    }

    #[test]
    fn a_cached_list_within_the_ttl_starts_a_thread_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        ModelCatalog::with_cache(Some(path.clone())).set_at(
            Driver::Claude,
            vec![model(Driver::Claude, "opus")],
            NOW,
        );
        let cat = ModelCatalog::with_cache(Some(path));
        assert!(
            !cat.is_settled(Driver::Claude),
            "nothing fetched in this run"
        );
        assert!(cat.may_start_without_waiting(Driver::Claude, NOW + 60));
        // Beyond the TTL the start waits for this run's fetch, as before.
        assert!(!cat.may_start_without_waiting(Driver::Claude, NOW + MODEL_TTL));
        // An agent with no cached list waits too.
        assert!(!cat.may_start_without_waiting(Driver::Agy, NOW + 60));
    }

    /// A fake `claude` that leaves a mark each time it is started.
    fn marking_claude(dir: &std::path::Path) -> (ProbeTargets, std::path::PathBuf) {
        let mark = dir.join("started");
        let program = crate::claude_probe::tests::fake_claude(
            dir,
            &format!("echo x >> '{}'\nexit 1", mark.display()),
        );
        let targets = ProbeTargets::new(vec![(
            Driver::Claude,
            crate::probe::ProbeTarget {
                program,
                env: crate::agent_proc::AgentEnv::default(),
            },
        )]);
        (targets, mark)
    }

    #[test]
    fn a_check_with_a_fresh_list_spawns_nothing_and_a_due_one_does() {
        use crate::testutil::{in_loop, pump_until};
        let dir = tempfile::tempdir().unwrap();
        let (targets, mark) = marking_claude(dir.path());
        in_loop(|ctx| {
            let cat = Rc::new(fetched_at(NOW));
            // Focus a minute later: the list is fresh, so nothing is spawned.
            cat.refresh_if_due(&targets, NOW + 60);
            assert!(!cat.refreshing.get(), "no fetch started");
            pump_until(ctx, 1, || false);
            assert!(!mark.exists(), "focus with fresh data spawned an agent");
            // An hour later it is due: the probe runs (and fails: the list stays).
            cat.refresh_if_due(&targets, NOW + MODEL_TTL);
            assert!(cat.refreshing.get());
            assert!(pump_until(ctx, 15, || !cat.refreshing.get()));
            assert!(mark.exists());
            assert_eq!(cat.models_of(Driver::Claude).len(), 1);
            assert!(cat.status_of(|_| true).failed);
        });
    }
}
