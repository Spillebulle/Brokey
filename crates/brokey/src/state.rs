//! What the application holds between commands: the store, the settings,
//! the plans that have run this session, and two small caches.
//!
//! Cloning an `AppState` is cheap and shares everything: the value the
//! window manages and the value a transaction thread holds are the same
//! one. That is what lets a worker thread outlive the command that started
//! it without borrowing anything from Tauri.

use crate::commands::{SelfUpdate, selfupdate_adapter};
use crate::settings::Settings;
use brokey_core::transaction::CancelToken;
use brokey_core::updates::UpdateList;
use brokey_core::{Event, Plan, Store};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// What answers a self-update check: `selfupdate_adapter::check` for the
/// window, a table for the tests. The argument is `fresh`.
pub type SelfUpdateChecker = Arc<dyn Fn(bool) -> Result<SelfUpdate, String> + Send + Sync>;

/// How many finished plans the activity list remembers.
pub const KEPT_PLANS: usize = 20;

/// How long an update check stays good. The first check after start is
/// always fresh because the cache starts empty.
pub const UPDATES_MAX_AGE: Duration = Duration::from_secs(10 * 60);

/// How long a self-update check stays good. Longer than the sources' ten
/// minutes because it is one request to GitHub's API, which allows sixty an
/// hour per address, shared with everything else on the network.
pub const SELF_UPDATE_MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// Where a plan has got to. The words are what the page draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanState {
    Running,
    Done,
    Failed,
    Cancelled,
}

/// One plan this session ran or is running, with everything it said.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanStatus {
    pub plan: Plan,
    pub state: PlanState,
    pub events: Vec<Event>,
    /// Unix seconds when it was started.
    pub started: i64,
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    /// Built on first use so the window opens before the package databases
    /// are read. `None` again after a transaction, so the next command sees
    /// the machine as it now is.
    store: RwLock<Option<Arc<Store>>>,
    settings: Mutex<Settings>,
    /// Where the settings are written. Tests point it at a scratch file.
    settings_path: PathBuf,
    plans: Mutex<Vec<PlanStatus>>,
    /// One flag per plan id, set by `cancel_plan`, read by the runner.
    cancels: Mutex<HashMap<String, CancelToken>>,
    updates: Mutex<Option<(Instant, UpdateList)>>,
    /// Whether the sources have been asked to refresh their indexes this
    /// session. The first update check does it; a transaction empties the
    /// update cache but does not unset this, so the minutes-long refresh
    /// runs once, not after every install.
    updates_refreshed: AtomicBool,
    self_update: Mutex<Option<(Instant, SelfUpdate)>>,
    self_update_checker: SelfUpdateChecker,
}

impl AppState {
    /// The state for the window: settings from the usual place.
    pub fn new() -> AppState {
        AppState::at(crate::settings::path())
    }

    /// The state with its settings file at `settings_path`.
    pub fn at(settings_path: PathBuf) -> AppState {
        AppState::build(settings_path, None, Arc::new(selfupdate_adapter::check))
    }

    /// The state with the store already built, so a test runs against
    /// sources it chose rather than the machine's.
    pub fn with_store(settings_path: PathBuf, store: Store) -> AppState {
        AppState::build(
            settings_path,
            Some(store),
            Arc::new(selfupdate_adapter::check),
        )
    }

    /// [`AppState::with_store`] with the self-update check answered by
    /// `checker` instead of GitHub.
    pub fn with_store_and_checker(
        settings_path: PathBuf,
        store: Store,
        checker: SelfUpdateChecker,
    ) -> AppState {
        AppState::build(settings_path, Some(store), checker)
    }

    fn build(settings_path: PathBuf, store: Option<Store>, checker: SelfUpdateChecker) -> AppState {
        let settings = Settings::load_from(&settings_path);
        AppState {
            inner: Arc::new(Inner {
                store: RwLock::new(store.map(Arc::new)),
                settings: Mutex::new(settings),
                settings_path,
                plans: Mutex::new(Vec::new()),
                cancels: Mutex::new(HashMap::new()),
                updates: Mutex::new(None),
                updates_refreshed: AtomicBool::new(false),
                self_update: Mutex::new(None),
                self_update_checker: checker,
            }),
        }
    }

    /// Ask whether a newer Brokey exists, through whatever this state
    /// was built to ask.
    pub fn check_self_update(&self, fresh: bool) -> Result<SelfUpdate, String> {
        (self.inner.self_update_checker)(fresh)
    }

    /// The store, detected on first use. Detection happens under the write
    /// lock so two commands arriving together share one detection rather
    /// than reading every database twice.
    pub fn store(&self) -> Arc<Store> {
        if let Some(store) = self
            .inner
            .store
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return store.clone();
        }
        let mut slot = self.inner.store.write().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = slot.as_ref() {
            return store.clone();
        }
        let store = Arc::new(Store::detect_with(&self.settings().preferences()));
        *slot = Some(store.clone());
        store
    }

    /// Forget the store so the next command detects it again. The sources
    /// reload their databases by mtime anyway; what this buys is a fresh
    /// availability pass, so installing Flatpak makes the Flatpak source
    /// appear without a restart. A command still holding the old `Arc`
    /// finishes with it.
    pub fn reset_store(&self) {
        *self.inner.store.write().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Whether the store has been built yet, for tests and for the status
    /// bar's first paint.
    pub fn has_store(&self) -> bool {
        self.inner
            .store
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub fn settings(&self) -> Settings {
        self.inner
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Apply and save new settings. The lines this build does not understand
    /// are carried over from the current settings, because the page cannot
    /// have sent them back: it never received them. The new settings apply
    /// to this session even when the file could not be written, and the
    /// error says so; a preference that does not stick is a smaller failure
    /// than one that does not apply.
    pub fn set_settings(&self, mut settings: Settings) -> Result<Settings, String> {
        let mut slot = self
            .inner
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        settings.unknown = std::mem::take(&mut slot.unknown);
        // A change to what the sources act on (the Flatpak installation, the
        // AUR helper) is only seen by a store built after it, so the store is
        // detected again on the next command.
        let changed = slot.preferences() != settings.preferences();
        *slot = settings.clone();
        drop(slot);
        if changed {
            *self.inner.store.write().unwrap_or_else(|e| e.into_inner()) = None;
        }
        settings.save_to(&self.inner.settings_path).map_err(|e| {
            format!(
                "The settings apply for now but could not be saved to {}: {e}. Check that the directory is writable.",
                self.inner.settings_path.display()
            )
        })?;
        Ok(settings)
    }

    pub fn cached_updates(&self, max_age: Duration) -> Option<UpdateList> {
        let slot = self.inner.updates.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref()
            .filter(|(at, _)| at.elapsed() < max_age)
            .map(|(_, list)| list.clone())
    }

    pub fn store_updates(&self, list: UpdateList) {
        *self.inner.updates.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((Instant::now(), list));
    }

    pub fn invalidate_updates(&self) {
        *self.inner.updates.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Whether an update check this session has already asked the sources
    /// to refresh their indexes.
    pub fn updates_refreshed_once(&self) -> bool {
        self.inner.updates_refreshed.load(Ordering::Relaxed)
    }

    pub fn mark_updates_refreshed(&self) {
        self.inner.updates_refreshed.store(true, Ordering::Relaxed);
    }

    /// What a finished transaction invalidates: the update list and the
    /// store, whose sources hold the installed snapshot.
    pub fn invalidate_after_transaction(&self) {
        self.invalidate_updates();
        self.reset_store();
    }

    pub fn cached_self_update(&self, max_age: Duration) -> Option<SelfUpdate> {
        let slot = self
            .inner
            .self_update
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        slot.as_ref()
            .filter(|(at, _)| at.elapsed() < max_age)
            .map(|(_, v)| v.clone())
    }

    pub fn store_self_update(&self, value: SelfUpdate) {
        *self
            .inner
            .self_update
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), value));
    }

    /// Record a plan as running and hand back its cancel flag. Finished
    /// plans beyond the last twenty are forgotten, oldest first; a running
    /// plan is never dropped from the list, however old.
    pub fn add_plan(&self, plan: Plan) -> CancelToken {
        let token = CancelToken::new();
        self.inner
            .cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(plan.id.clone(), token.clone());
        let mut plans = self.inner.plans.lock().unwrap_or_else(|e| e.into_inner());
        plans.push(PlanStatus {
            plan,
            state: PlanState::Running,
            events: Vec::new(),
            started: now_unix(),
        });
        while plans.len() > KEPT_PLANS {
            let Some(oldest_finished) = plans.iter().position(|p| p.state != PlanState::Running)
            else {
                break;
            };
            let gone = plans.remove(oldest_finished);
            self.inner
                .cancels
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&gone.plan.id);
        }
        token
    }

    /// Append an event to its plan. `PlanFinished` also settles the state:
    /// done when it succeeded, cancelled when it failed after a cancel was
    /// asked for, failed otherwise.
    pub fn push_event(&self, id: &str, event: Event) {
        let mut plans = self.inner.plans.lock().unwrap_or_else(|e| e.into_inner());
        let Some(status) = plans.iter_mut().find(|p| p.plan.id == id) else {
            log::warn!("event for unknown plan {id}: {event:?}");
            return;
        };
        if let Event::PlanFinished { ok, .. } = &event {
            status.state = if *ok {
                PlanState::Done
            } else if self.cancel_requested(id) {
                PlanState::Cancelled
            } else {
                PlanState::Failed
            };
        }
        status.events.push(event);
    }

    pub fn plan_state(&self, id: &str) -> Option<PlanState> {
        self.inner
            .plans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|p| p.plan.id == id)
            .map(|p| p.state)
    }

    /// Every plan this session remembers, oldest first.
    pub fn plans(&self) -> Vec<PlanStatus> {
        self.inner
            .plans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Use the runner's own token for a plan, so `cancel` reaches the run.
    pub fn set_cancel_token(&self, id: &str, token: CancelToken) {
        self.inner
            .cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), token);
    }

    /// The operations a known plan carries, for telling the sources how it
    /// ended.
    pub fn plan_ops(&self, id: &str) -> Option<Vec<brokey_core::Op>> {
        self.inner
            .plans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|p| p.plan.id == id)
            .map(|p| p.plan.ops.clone())
    }

    /// The token the runner watches for this plan, if the plan is known.
    pub fn cancel_token(&self, id: &str) -> Option<CancelToken> {
        self.inner
            .cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    pub fn cancel_requested(&self, id: &str) -> bool {
        self.cancel_token(id).is_some_and(|t| t.is_cancelled())
    }

    /// Ask a running plan to stop. Asking twice, or asking a plan that has
    /// already finished, is not an error: the answer is the same.
    pub fn cancel(&self, id: &str) -> Result<(), String> {
        match self.cancel_token(id) {
            Some(token) => {
                token.cancel();
                Ok(())
            }
            None => Err(format!(
                "There is no plan {id} to cancel. It may have been finished for a while and dropped from the list."
            )),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Seconds since the epoch, or zero on a machine whose clock predates it.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brokey_core::{Op, PackageRef, SourceKind};

    /// A directory that never collides with another test or another run,
    /// even when the operating system reuses a process id: the pid alone is
    /// not enough, so this also mixes in a nanosecond timestamp.
    fn scratch_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "brokey-state-{}-{nanos}-{name}",
            std::process::id()
        ))
    }

    fn scratch_state(name: &str) -> AppState {
        AppState::at(scratch_dir(name).join(crate::settings::FILE_NAME))
    }

    fn a_plan(id: &str) -> Plan {
        Plan {
            id: id.to_string(),
            ops: vec![Op::Install {
                package: PackageRef {
                    source: SourceKind::Pacman,
                    id: "steam".into(),
                },
            }],
            steps: Vec::new(),
        }
    }

    fn finished(id: &str, ok: bool) -> Event {
        Event::PlanFinished {
            plan: id.to_string(),
            ok,
            message: String::new(),
        }
    }

    #[test]
    fn a_plan_runs_then_settles_by_its_last_event() {
        let state = scratch_state("settle");
        state.add_plan(a_plan("p1"));
        assert_eq!(state.plan_state("p1"), Some(PlanState::Running));
        state.push_event(
            "p1",
            Event::StepStarted {
                plan: "p1".into(),
                step: 0,
                title: "Installing steam".into(),
            },
        );
        state.push_event("p1", finished("p1", true));
        assert_eq!(state.plan_state("p1"), Some(PlanState::Done));
        assert_eq!(state.plans()[0].events.len(), 2);

        state.add_plan(a_plan("p2"));
        state.push_event("p2", finished("p2", false));
        assert_eq!(state.plan_state("p2"), Some(PlanState::Failed));
    }

    #[test]
    fn a_cancelled_plan_that_stops_is_cancelled_not_failed() {
        let state = scratch_state("cancel");
        let token = state.add_plan(a_plan("p1"));
        assert!(!token.is_cancelled());
        state.cancel("p1").expect("known plan");
        assert!(
            token.is_cancelled(),
            "the runner's token is the one that was set"
        );
        state.push_event("p1", finished("p1", false));
        assert_eq!(state.plan_state("p1"), Some(PlanState::Cancelled));
        // Asking again is harmless; asking for nothing is an error the page can show.
        assert!(state.cancel("p1").is_ok());
        assert!(state.cancel("nope").unwrap_err().contains("no plan nope"));
    }

    #[test]
    fn only_the_last_twenty_finished_plans_are_kept() {
        let state = scratch_state("twenty");
        for i in 0..25 {
            let id = format!("p{i}");
            state.add_plan(a_plan(&id));
            state.push_event(&id, finished(&id, true));
        }
        let plans = state.plans();
        assert_eq!(plans.len(), KEPT_PLANS);
        assert_eq!(plans[0].plan.id, "p5", "the oldest went first");
        assert_eq!(plans[19].plan.id, "p24");
        assert!(
            state.cancel_token("p0").is_none(),
            "a dropped plan's flag goes with it"
        );
    }

    #[test]
    fn a_running_plan_is_never_dropped() {
        let state = scratch_state("running");
        state.add_plan(a_plan("long"));
        for i in 0..25 {
            let id = format!("p{i}");
            state.add_plan(a_plan(&id));
            state.push_event(&id, finished(&id, true));
        }
        assert!(state.plans().iter().any(|p| p.plan.id == "long"));
        assert_eq!(state.plans().len(), KEPT_PLANS);
    }

    #[test]
    fn an_event_for_an_unknown_plan_is_dropped_not_fatal() {
        let state = scratch_state("unknown");
        state.push_event("ghost", finished("ghost", true));
        assert!(state.plans().is_empty());
    }

    #[test]
    fn the_updates_cache_expires_and_can_be_invalidated() {
        let state = scratch_state("updates");
        assert!(
            state.cached_updates(UPDATES_MAX_AGE).is_none(),
            "the first check is always fresh"
        );
        let list = UpdateList {
            checked_at: 42,
            ..UpdateList::default()
        };
        state.store_updates(list.clone());
        assert_eq!(state.cached_updates(UPDATES_MAX_AGE), Some(list));
        assert!(
            state.cached_updates(Duration::ZERO).is_none(),
            "an aged entry is not offered"
        );
        state.invalidate_updates();
        assert!(state.cached_updates(UPDATES_MAX_AGE).is_none());
    }

    #[test]
    fn settings_round_trip_through_the_state_and_keep_unknown_lines() {
        let state = scratch_state("settings");
        let path = state.inner.settings_path.clone();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "theme = dark\ngizmo = 12\n").unwrap();
        let state = AppState::at(path.clone());
        assert_eq!(state.settings().theme, crate::settings::Theme::Dark);

        // What the page sends back has no `unknown`, as after a JSON trip.
        let mut from_page = state.settings();
        from_page.unknown.clear();
        from_page.show_packages = true;
        let saved = state.set_settings(from_page).expect("writable scratch dir");
        assert!(saved.show_packages);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("gizmo = 12"),
            "the unknown line survived: {text}"
        );
        assert!(text.contains("show_packages = true"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_store_is_built_once_and_forgotten_on_request() {
        let state = scratch_state("store");
        assert!(!state.has_store());
        let a = state.store();
        let b = state.store();
        assert!(Arc::ptr_eq(&a, &b));
        state.invalidate_after_transaction();
        assert!(!state.has_store());
        let c = state.store();
        assert!(!Arc::ptr_eq(&a, &c), "detected again");
    }

    #[test]
    fn a_given_store_is_used_until_a_transaction_forgets_it() {
        let dir = scratch_dir("given");
        let state = AppState::with_store(
            dir.join(crate::settings::FILE_NAME),
            Store {
                system: brokey_core::system::from_os_release(""),
                sources: Vec::new(),
            },
        );
        assert!(state.has_store(), "no detection needed");
        assert!(state.store().sources.is_empty());
        assert!(!state.updates_refreshed_once());
        state.mark_updates_refreshed();
        state.invalidate_after_transaction();
        assert!(!state.has_store());
        assert!(
            state.updates_refreshed_once(),
            "a transaction does not ask for the refresh again"
        );
    }

    #[test]
    fn a_setting_the_sources_act_on_detects_the_store_again() {
        let dir = scratch_dir("preferences");
        let empty = || Store {
            system: brokey_core::system::from_os_release(""),
            sources: Vec::new(),
        };
        let state = AppState::with_store(dir.join(crate::settings::FILE_NAME), empty());

        let mut theme_only = state.settings();
        theme_only.theme = crate::settings::Theme::Light;
        state.set_settings(theme_only).unwrap();
        assert!(state.has_store(), "the theme changes nothing a source does");

        let mut user = state.settings();
        user.flatpak_scope = crate::settings::FlatpakScope::User;
        state.set_settings(user).unwrap();
        assert!(
            !state.has_store(),
            "the Flatpak installation is read when the store is built"
        );

        *state.inner.store.write().unwrap() = Some(Arc::new(empty()));
        let mut helper = state.settings();
        helper.aur_helper = crate::settings::AurHelper::Builtin;
        state.set_settings(helper).unwrap();
        assert!(!state.has_store(), "so is the AUR helper");
    }
}
