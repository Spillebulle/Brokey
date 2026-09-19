//! Every `#[tauri::command]`, thin. The names are the contract with the
//! page (`frontend/src/api.ts` calls them by these exact strings) and every
//! one returns `Result<T, String>` where the `String` is a sentence the
//! page can show as it is.
//!
//! The commands themselves only move values across the IPC boundary; what
//! they do lives in [`logic`], which knows nothing of Tauri so the text
//! mode and the tests run the same code without a window. Anything that
//! touches the store runs on a blocking worker (`spawn_blocking`): the
//! sources are synchronous by design, and a search over the pacman database
//! on the async runtime's threads would stall every other command.
//!
//! A running transaction reaches the page as `transaction://event` with the
//! [`Event`] as payload, and the same events are kept in the plan's
//! [`PlanStatus`] so a page that was elsewhere when they were sent can catch
//! up through `active_plans`.

pub mod selfupdate_adapter;

pub use selfupdate_adapter::SelfUpdate;

use crate::settings::Settings;
use crate::state::{AppState, PlanStatus};
use brokey_core::updates::UpdateList;
use brokey_core::{
    DriversReport, Event, Op, Package, PackageRef, Plan, Query, SearchResult, SourceStatus,
    SystemInfo,
};
use serde::{Deserialize, Serialize};
use tauri::ipc::Invoke;

/// The event name the page listens on for a running transaction.
pub const TRANSACTION_EVENT: &str = "transaction://event";

/// What the confirm step shows: the steps that would run and anything the
/// user should know first (a partial upgrade on Arch, a reboot after
/// firmware).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanPreview {
    pub plan: Plan,
    pub notices: Vec<String>,
}

pub fn handler() -> impl Fn(Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        system_info,
        sources,
        search,
        installed,
        app_details,
        updates,
        plan,
        run_plan,
        cancel_plan,
        active_plans,
        drivers,
        settings_get,
        settings_set,
        self_update_check,
        self_update_apply,
        group_split,
        launch_targets,
        open_app,
        launcher_notices,
    ]
}

/// Run store work on a blocking worker and bring its answer back. A worker
/// that died is reported as a sentence rather than a panic in the page.
async fn blocking<T, F>(work: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(work).await {
        Ok(answer) => answer,
        Err(e) => Err(format!(
            "The store's worker stopped before it answered: {e}. Try again."
        )),
    }
}

/// The page-facing end of a transaction: every event goes out as
/// `transaction://event`. A page that is not listening loses nothing, since
/// the same events are kept on the plan.
fn emitter(app: tauri::AppHandle) -> impl Fn(&Event) + Send + Sync + 'static {
    use tauri::Emitter;
    move |event| {
        if let Err(e) = app.emit(TRANSACTION_EVENT, event) {
            log::warn!("could not send {TRANSACTION_EVENT} to the page: {e}");
        }
    }
}

#[tauri::command]
async fn system_info() -> Result<SystemInfo, String> {
    // Read straight from os-release rather than through the store, so the
    // status bar can paint before the package databases are read.
    blocking(|| Ok(brokey_core::system::detect())).await
}

#[tauri::command]
async fn sources(state: tauri::State<'_, AppState>) -> Result<Vec<SourceStatus>, String> {
    let state = state.inner().clone();
    blocking(move || Ok(logic::sources(&state))).await
}

#[tauri::command]
async fn search(state: tauri::State<'_, AppState>, query: Query) -> Result<SearchResult, String> {
    let state = state.inner().clone();
    blocking(move || Ok(logic::search(&state, query))).await
}

#[tauri::command]
async fn installed(state: tauri::State<'_, AppState>) -> Result<SearchResult, String> {
    let state = state.inner().clone();
    blocking(move || Ok(logic::installed(&state))).await
}

/// For each reference, what opening it would open (an entry's file name or a
/// command line), or `null` when there is nothing to open. The page draws an
/// Open button only where this is not `null`.
#[tauri::command]
async fn launch_targets(
    state: tauri::State<'_, AppState>,
    refs: Vec<PackageRef>,
) -> Result<Vec<Option<String>>, String> {
    let state = state.inner().clone();
    blocking(move || Ok(logic::launch_targets(&state, &refs))).await
}

/// Open an installed package in the user's session.
#[tauri::command]
async fn open_app(state: tauri::State<'_, AppState>, package: PackageRef) -> Result<(), String> {
    let state = state.inner().clone();
    blocking(move || logic::open_app(&state, &package)).await
}

/// The sources whose applications the running session cannot list yet, each
/// with the sentence that says so.
#[tauri::command]
async fn launcher_notices(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<(brokey_core::SourceKind, String)>, String> {
    let state = state.inner().clone();
    blocking(move || {
        #[cfg(unix)]
        {
            Ok(state.store().launcher_notices())
        }
        #[cfg(windows)]
        {
            // No source lists applications a session cannot see yet.
            let _ = state;
            Ok(Vec::new())
        }
    })
    .await
}

#[tauri::command]
async fn app_details(
    state: tauri::State<'_, AppState>,
    refs: Vec<PackageRef>,
) -> Result<Vec<Package>, String> {
    let state = state.inner().clone();
    blocking(move || Ok(logic::app_details(&state, refs))).await
}

#[tauri::command]
async fn updates(state: tauri::State<'_, AppState>, force: bool) -> Result<UpdateList, String> {
    let state = state.inner().clone();
    blocking(move || Ok(logic::updates(&state, force))).await
}

#[tauri::command]
async fn plan(state: tauri::State<'_, AppState>, ops: Vec<Op>) -> Result<PlanPreview, String> {
    let state = state.inner().clone();
    blocking(move || logic::preview(&state, ops)).await
}

#[tauri::command]
async fn run_plan(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    ops: Vec<Op>,
) -> Result<String, String> {
    let state = state.inner().clone();
    let emit = emitter(app);
    blocking(move || logic::run_plan(&state, ops, emit)).await
}

#[tauri::command]
fn cancel_plan(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    state.cancel(&id)
}

#[tauri::command]
fn active_plans(state: tauri::State<'_, AppState>) -> Result<Vec<PlanStatus>, String> {
    Ok(state.plans())
}

#[tauri::command]
async fn drivers(state: tauri::State<'_, AppState>) -> Result<DriversReport, String> {
    let state = state.inner().clone();
    blocking(move || Ok(logic::drivers(&state))).await
}

#[tauri::command]
fn settings_get(state: tauri::State<'_, AppState>) -> Result<Settings, String> {
    Ok(state.settings())
}

#[tauri::command]
fn settings_set(state: tauri::State<'_, AppState>, settings: Settings) -> Result<Settings, String> {
    state.set_settings(settings)
}

#[tauri::command]
async fn self_update_check(
    state: tauri::State<'_, AppState>,
    force: bool,
) -> Result<SelfUpdate, String> {
    let state = state.inner().clone();
    blocking(move || logic::self_update_check(&state, force)).await
}

#[tauri::command]
async fn self_update_apply(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let state = state.inner().clone();
    let emit = emitter(app);
    blocking(move || logic::self_update_apply(&state, emit)).await
}

#[tauri::command]
fn group_split(state: tauri::State<'_, AppState>, package: PackageRef) -> Result<Settings, String> {
    logic::group_split(&state, package)
}

/// A core error as the page shows it: trimmed, ending in a full stop.
/// Sources already write sentences; this only closes the ones that forgot.
pub fn sentence(message: &str) -> String {
    let text = message.trim();
    if text.is_empty() {
        return "Something went wrong and the source did not say what.".to_string();
    }
    if text.ends_with(['.', '!', '?']) {
        text.to_string()
    } else {
        format!("{text}.")
    }
}

/// What the commands do, with no Tauri in it.
pub mod logic {
    use super::{PlanPreview, SelfUpdate, selfupdate_adapter, sentence};
    use crate::settings::Settings;
    use crate::state::PlanState;
    use crate::state::{AppState, SELF_UPDATE_MAX_AGE, UPDATES_MAX_AGE};
    use brokey_core::Plan;
    use brokey_core::transaction::runner::Runner;
    use brokey_core::updates::UpdateList;
    use brokey_core::{
        DriversReport, Event, Op, Package, PackageRef, Query, SearchResult, SourceStatus, Store,
    };
    use std::sync::Arc;

    /// The most results a source is asked for. Above this the grouper and
    /// the page are sorting rows nobody scrolls to.
    pub const MAX_LIMIT: usize = 300;
    /// What a query that names no limit gets; the same figure as
    /// `Query::new`.
    pub const DEFAULT_LIMIT: usize = 200;

    pub fn sources(state: &AppState) -> Vec<SourceStatus> {
        state.store().statuses()
    }

    /// The query as the store sees it: the user's enabled sources when the
    /// page named none, and a limit the sources can honour.
    pub fn shape_query(mut query: Query, settings: &Settings) -> Query {
        if query.sources.is_none() {
            query.sources = Some(settings.enabled_sources.clone());
        }
        if query.split.is_empty() {
            query.split = settings.split.clone();
        }
        query.limit = match query.limit {
            0 => DEFAULT_LIMIT,
            n => n.min(MAX_LIMIT),
        };
        query
    }

    pub fn search(state: &AppState, query: Query) -> SearchResult {
        let query = shape_query(query, &state.settings());
        state.store().search(&query)
    }

    pub fn installed(state: &AppState) -> SearchResult {
        state.store().installed()
    }

    /// The full record for each reference, asked for in parallel. A source
    /// that fails costs its own reference and a log line, never the call:
    /// the detail page draws what it got.
    pub fn app_details(state: &AppState, refs: Vec<PackageRef>) -> Vec<Package> {
        let store = state.store();
        std::thread::scope(|scope| {
            let handles: Vec<_> = refs
                .iter()
                .map(|r| {
                    let store = &store;
                    scope.spawn(move || details_one(store, r))
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| {
                    h.join().unwrap_or_else(|_| {
                        log::warn!(
                            "a source panicked while answering details; its package is left out"
                        );
                        None
                    })
                })
                .collect()
        })
    }

    fn details_one(store: &Store, r: &PackageRef) -> Option<Package> {
        let Some(source) = store.source(r.source) else {
            log::warn!(
                "details asked of {}, which is not a source on this machine",
                r.source.label()
            );
            return None;
        };
        let status = source.status();
        if !status.available {
            log::warn!(
                "details asked of {} for {} but it is unavailable: {}",
                r.source.label(),
                r.id,
                status.reason.unwrap_or_default()
            );
            return None;
        }
        match source.details(&r.id) {
            Ok(p) => Some(p),
            Err(e) => {
                log::warn!(
                    "{} could not give details for {}: {}",
                    r.source.label(),
                    r.id,
                    e.message
                );
                None
            }
        }
    }

    /// The merged update list, from the cache when it is under ten minutes
    /// old and the page did not insist. A forced check, and the first check
    /// of a session, ask the sources to refresh their indexes first (root
    /// free), so what the page shows is what the machine would get. Only
    /// the first: a finished transaction empties the cache, and the check
    /// after it reads the databases the transaction just wrote rather than
    /// fetching them all again.
    pub fn updates(state: &AppState, force: bool) -> UpdateList {
        if !force && let Some(list) = state.cached_updates(UPDATES_MAX_AGE) {
            return list;
        }
        let store = state.store();
        let list = if force || !state.updates_refreshed_once() {
            state.mark_updates_refreshed();
            store.updates_refreshed()
        } else {
            store.updates()
        };
        state.store_updates(list.clone());
        list
    }

    pub fn drivers(state: &AppState) -> DriversReport {
        brokey_core::drivers::report(&state.store())
    }

    pub fn launch_targets(state: &AppState, refs: &[PackageRef]) -> Vec<Option<String>> {
        #[cfg(unix)]
        {
            let store = state.store();
            refs.iter()
                .map(|r| store.launcher(r).map(|l| l.describe()))
                .collect()
        }
        #[cfg(windows)]
        {
            // No source opens anything on Windows yet; see `open_app`.
            let _ = state;
            refs.iter().map(|_| None).collect()
        }
    }

    /// Opening an application is not a transaction, so it does not go
    /// through a Plan: nothing changes on the machine and nothing runs as
    /// root. `gio launch` returns once the application is started, so its
    /// answer is waited for and a failure is a sentence; a command that runs
    /// for the application's lifetime (`flatpak run`) is started in a
    /// process group of its own and left to run, with a thread reaping it.
    pub fn open_app(state: &AppState, package: &PackageRef) -> Result<(), String> {
        #[cfg(unix)]
        {
            let store = state.store();
            let Some(launch) = store.launcher(package) else {
                return not_something_to_open(package);
            };
            start(&launch)
        }
        #[cfg(windows)]
        {
            let _ = state;
            not_something_to_open(package)
        }
    }

    /// The sentence for a package with nothing to open: not installed
    /// through a source that has a launcher, or (Windows, for now) no
    /// source has one at all.
    fn not_something_to_open(package: &PackageRef) -> Result<(), String> {
        Err(format!(
            "{} is not something Brokey can open. It may not be installed, or it has no application to start.",
            package.id
        ))
    }

    /// Start what a [`brokey_core::launch::Launch`] describes, detached from
    /// the store's own process.
    #[cfg(unix)]
    pub fn start(launch: &brokey_core::launch::Launch) -> Result<(), String> {
        use std::os::unix::process::CommandExt;
        let spec = launch.command();
        let mut command = std::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .process_group(0);
        let program = spec.program.clone();
        let not_started = |e: std::io::Error| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "{program} is not installed, so Brokey cannot open applications. Install glib2 (it provides gio) and try again."
                )
            } else {
                format!("Could not start {program}: {e}.")
            }
        };
        if launch.returns_at_once() {
            // Not a pipe: the application gio starts inherits gio's stderr,
            // and a pipe would stay open for the application's lifetime, so
            // waiting for it to close would wait for the application to be
            // quit. A file closes nothing and still keeps gio's own words.
            let log = brokey_core::system::Dirs::new()
                .cache
                .join(format!("open-{}.log", std::process::id()));
            if let Some(dir) = log.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let stderr = std::fs::File::create(&log)
                .map(std::process::Stdio::from)
                .unwrap_or_else(|_| std::process::Stdio::null());
            let status = command.stderr(stderr).status().map_err(not_started)?;
            let err = std::fs::read_to_string(&log).unwrap_or_default();
            let _ = std::fs::remove_file(&log);
            if status.success() {
                return Ok(());
            }
            let first = err
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .trim_end_matches('.');
            return Err(if first.is_empty() {
                format!("{} did not open.", launch.describe())
            } else {
                format!("{} did not open: {first}.", launch.describe())
            });
        }
        let mut child = command
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(not_started)?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    /// A dry run for the confirm step.
    pub fn preview(state: &AppState, ops: Vec<Op>) -> Result<PlanPreview, String> {
        let store = state.store();
        let plan = store.plan(&ops).map_err(|e| sentence(&e.message))?;
        let notices = notices(&store, &ops);
        Ok(PlanPreview { plan, notices })
    }

    /// What the user should read before confirming: the partial-upgrade
    /// notice on Arch, and whatever else the plan builder knows.
    pub fn notices(store: &Store, ops: &[Op]) -> Vec<String> {
        brokey_core::transaction::notices(store, ops)
    }

    /// Build the plan and start it. The id comes back at once; everything
    /// after that arrives as events.
    pub fn run_plan(
        state: &AppState,
        ops: Vec<Op>,
        emit: impl Fn(&Event) + Send + Sync + 'static,
    ) -> Result<String, String> {
        let store = state.store();
        let plan = store.plan(&ops).map_err(|e| sentence(&e.message))?;
        if plan.steps.is_empty() {
            return Err(
                "There is nothing to do. Everything asked for is already in place.".to_string(),
            );
        }
        Ok(start_plan(state, store, plan, emit))
    }

    /// Record the plan and run it on a thread of its own. The thread holds a
    /// clone of the state, not a borrow, so it outlives the command that
    /// started it, and the store the plan was built from, so the sources
    /// that built it are the ones told how it ended even when another
    /// plan has reset the state's store in the meantime. Every event is
    /// appended to the plan's record before it is sent, so a page that
    /// reacts to an event by asking `active_plans` already sees it there.
    ///
    /// Built on the runner, which elevates root steps the way each platform
    /// does: `pkexec` and the helper's closed list on Linux, `ShellExecuteEx`
    /// with `runas` and the helper's closed list on Windows. `run_plan` and
    /// `self_update_apply` are the callers, on both platforms.
    pub fn start_plan(
        state: &AppState,
        store: Arc<Store>,
        plan: Plan,
        emit: impl Fn(&Event) + Send + Sync + 'static,
    ) -> String {
        let id = plan.id.clone();
        state.add_plan(plan.clone());
        // The runner is made here, before the thread, so its cancel token
        // is on record by the time the id reaches the page.
        let runner = Runner::new();
        state.set_cancel_token(&id, runner.cancel_token());
        let emit: Arc<dyn Fn(&Event) + Send + Sync> = Arc::new(emit);
        let worker_state = state.clone();
        let worker_emit = emit.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("plan {id}"))
            .spawn(move || {
                run_to_completion(&worker_state, &store, &runner, &plan, worker_emit.as_ref())
            });
        if let Err(e) = spawned {
            // No thread means no runner, so settle the plan here rather than
            // leave the panel waiting on something nobody is running.
            let event = Event::PlanFinished {
                plan: id.clone(),
                ok: false,
                message: format!("Could not start a worker for the plan: {e}. Try again."),
            };
            state.push_event(&id, event.clone());
            emit(&event);
        }
        id
    }

    /// What happens to every event a plan produces: recorded on the plan,
    /// acted on, then sent to the page, in that order. `store` is the one
    /// the plan was built from, not the state's current one, which another
    /// plan may have reset since.
    pub fn deliver(
        state: &AppState,
        store: &Store,
        id: &str,
        event: Event,
        emit: &(dyn Fn(&Event) + Send + Sync),
    ) {
        let finished = match &event {
            Event::PlanFinished { ok, .. } => Some(*ok),
            _ => None,
        };
        state.push_event(id, event.clone());
        let mut sets_up = false;
        if let Some(ok) = finished
            && let Some(ops) = state.plan_ops(id)
        {
            // The sources that keep their own records (GitHub) learn how it
            // went before the state's store is dropped below.
            store.finished(&ops, ok);
            sets_up = ops.iter().any(|op| matches!(op, Op::Setup { .. }));
        }
        // The machine changed under the sources. The store is detected
        // again on the next command; the sources reload by mtime anyway,
        // but `Store::detect` also re-runs availability, which is what
        // makes a newly installed Flatpak appear without a restart. A plan
        // that sets a source up does it even when a later step failed:
        // the tool may be installed by then, and no status is cached
        // across a detection.
        if finished == Some(true) || (finished == Some(false) && sets_up) {
            state.invalidate_after_transaction();
        }
        emit(&event);
    }

    fn run_to_completion(
        state: &AppState,
        store: &Store,
        runner: &Runner,
        plan: &Plan,
        emit: &(dyn Fn(&Event) + Send + Sync),
    ) {
        let id = plan.id.clone();
        let mut sink = |event: Event| deliver(state, store, &id, event, emit);
        let outcome = runner.execute(plan, &mut sink);
        // The runner ends every run with PlanFinished; this only guards
        // the invariant the panel relies on, so it never waits for ever.
        if state.plan_state(&id) == Some(PlanState::Running) {
            sink(Event::PlanFinished {
                plan: id.clone(),
                ok: outcome.ok,
                message: sentence(&outcome.message),
            });
        }
    }

    /// Whether a newer Brokey exists. Cached for an hour in memory and
    /// six hours on disk; a forced check (the button) goes past both to
    /// GitHub, an unforced one (start-up) reads the caches and asks only
    /// when the setting allows it, because that is what the setting
    /// promises.
    pub fn self_update_check(state: &AppState, force: bool) -> Result<SelfUpdate, String> {
        if !force && let Some(cached) = state.cached_self_update(SELF_UPDATE_MAX_AGE) {
            return Ok(cached);
        }
        if !force && !state.settings().self_update_check {
            return Ok(selfupdate_adapter::unchecked());
        }
        let result = state.check_self_update(force)?;
        state.store_self_update(result.clone());
        Ok(result)
    }

    /// Apply the remedy from the last check, checking first, past every
    /// cache, if there was none this hour.
    pub fn self_update_apply(
        state: &AppState,
        emit: impl Fn(&Event) + Send + Sync + 'static,
    ) -> Result<String, String> {
        let update = match state.cached_self_update(SELF_UPDATE_MAX_AGE) {
            Some(u) => u,
            None => {
                let u = state.check_self_update(true)?;
                state.store_self_update(u.clone());
                u
            }
        };
        let plan = selfupdate_adapter::plan(&update)?;
        if plan.steps.is_empty() {
            return Err("The update has no steps to run on this machine. The check says how to get it instead.".to_string());
        }
        Ok(start_plan(state, state.store(), plan, emit))
    }

    /// Remember that this edition does not belong to its group.
    pub fn group_split(state: &AppState, package: PackageRef) -> Result<Settings, String> {
        let mut settings = state.settings();
        settings.add_split(package.source, &package.id);
        state.set_settings(settings)
    }
}

#[cfg(test)]
mod tests {
    // The logic functions share names with the Tauri wrappers, so only the
    // logic side is glob-imported and the rest is named.
    use super::logic::*;
    use super::{PlanPreview, selfupdate_adapter, sentence};
    use crate::settings::Settings;
    use crate::state::{AppState, PlanState, SelfUpdateChecker};
    use brokey_core::updates::UpdateList;
    use brokey_core::{
        Event, Op, Package, PackageRef, Plan, Query, Source, SourceKind, SourceStatus, Step, Store,
        Update,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    fn scratch_path(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir()
            .join(format!("brokey-cmd-{}-{nanos}-{name}", std::process::id()))
            .join(crate::settings::FILE_NAME)
    }

    /// A state over the machine's own sources, for the tests whose subject
    /// is the plan machinery rather than a source.
    fn scratch_state(name: &str) -> AppState {
        AppState::at(scratch_path(name))
    }

    /// A store with no sources: nothing is read, nothing is fetched.
    fn empty_store() -> Store {
        Store {
            system: brokey_core::system::from_os_release(""),
            sources: Vec::new(),
        }
    }

    /// A state over `store`, so a test never detects the machine's sources.
    fn state_with(name: &str, store: Store) -> AppState {
        AppState::with_store(scratch_path(name), store)
    }

    /// A source that answers nothing and counts what it was asked.
    #[derive(Default)]
    struct Counts {
        refreshes: AtomicUsize,
        finished: Mutex<Vec<(Op, bool)>>,
    }

    struct Fake {
        kind: SourceKind,
        counts: Arc<Counts>,
    }

    impl Fake {
        fn store(kind: SourceKind) -> (Store, Arc<Counts>) {
            let counts = Arc::new(Counts::default());
            let store = Store {
                system: brokey_core::system::from_os_release(""),
                sources: vec![Box::new(Fake {
                    kind,
                    counts: counts.clone(),
                })],
            };
            (store, counts)
        }
    }

    impl Source for Fake {
        fn kind(&self) -> SourceKind {
            self.kind
        }
        fn status(&self) -> SourceStatus {
            SourceStatus {
                kind: self.kind,
                available: true,
                reason: None,
                detail: None,
                searchable: false,
                setup: None,
            }
        }
        fn search(&self, _query: &Query) -> brokey_core::Result<Vec<Package>> {
            Ok(Vec::new())
        }
        fn installed(&self) -> brokey_core::Result<Vec<Package>> {
            Ok(Vec::new())
        }
        fn updates(&self) -> brokey_core::Result<Vec<Update>> {
            Ok(Vec::new())
        }
        fn details(&self, id: &str) -> brokey_core::Result<Package> {
            Err(brokey_core::Error::new(format!("{id} is not known.")))
        }
        fn plan(&self, _op: &Op) -> brokey_core::Result<Vec<Step>> {
            Ok(Vec::new())
        }
        fn refresh_index(&self) -> brokey_core::Result<()> {
            self.counts.refreshes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn finished(&self, op: &Op, ok: bool) {
            self.counts
                .finished
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((op.clone(), ok));
        }
    }

    fn steam() -> PackageRef {
        PackageRef {
            source: SourceKind::Pacman,
            id: "steam".into(),
        }
    }

    /// A source with one root step to remove, so a plan can reach the
    /// runner on Windows without needing the machine's real winget
    /// catalogue.
    #[cfg(windows)]
    struct FakeWinget;

    #[cfg(windows)]
    impl Source for FakeWinget {
        fn kind(&self) -> SourceKind {
            SourceKind::Winget
        }
        fn status(&self) -> SourceStatus {
            SourceStatus {
                kind: SourceKind::Winget,
                available: true,
                reason: None,
                detail: None,
                searchable: false,
                setup: None,
            }
        }
        fn search(&self, _query: &Query) -> brokey_core::Result<Vec<Package>> {
            Ok(Vec::new())
        }
        fn installed(&self) -> brokey_core::Result<Vec<Package>> {
            Ok(Vec::new())
        }
        fn updates(&self) -> brokey_core::Result<Vec<Update>> {
            Ok(Vec::new())
        }
        fn details(&self, id: &str) -> brokey_core::Result<Package> {
            Err(brokey_core::Error::new(format!("{id} is not known.")))
        }
        fn plan(&self, _op: &Op) -> brokey_core::Result<Vec<Step>> {
            Ok(vec![Step {
                source: SourceKind::Winget,
                title: "Removing Test.App".into(),
                command: brokey_core::Command {
                    program: "winget".into(),
                    args: vec![
                        "uninstall".into(),
                        "--id".into(),
                        "Test.App".into(),
                        "--silent".into(),
                    ],
                    env: Vec::new(),
                    cwd: None,
                },
                needs_root: true,
                weight: 1,
            }])
        }
        fn refresh_index(&self) -> brokey_core::Result<()> {
            Ok(())
        }
        fn finished(&self, _op: &Op, _ok: bool) {}
    }

    #[test]
    fn a_sentence_ends_with_a_full_stop_and_nothing_is_said_twice() {
        assert_eq!(
            sentence("could not reach aur.archlinux.org"),
            "could not reach aur.archlinux.org."
        );
        assert_eq!(
            sentence("Flatpak is not installed. "),
            "Flatpak is not installed."
        );
        assert_eq!(sentence("Really?"), "Really?");
        assert!(sentence("   ").contains("did not say what"));
    }

    #[test]
    fn a_query_takes_the_enabled_sources_and_a_sane_limit() {
        let settings = Settings {
            enabled_sources: vec![SourceKind::Pacman, SourceKind::Aur],
            ..Settings::default()
        };

        let q = shape_query(Query::new("steam"), &settings);
        assert_eq!(q.sources, Some(vec![SourceKind::Pacman, SourceKind::Aur]));
        assert_eq!(q.limit, DEFAULT_LIMIT);

        let explicit = Query {
            text: "steam".into(),
            sources: Some(vec![SourceKind::Flatpak]),
            limit: 5000,
            split: Vec::new(),
        };
        let q = shape_query(explicit, &settings);
        assert_eq!(
            q.sources,
            Some(vec![SourceKind::Flatpak]),
            "the page's own choice stands"
        );
        assert_eq!(q.limit, MAX_LIMIT);

        let zero = Query {
            text: "x".into(),
            sources: None,
            limit: 0,
            split: Vec::new(),
        };
        assert_eq!(shape_query(zero, &settings).limit, DEFAULT_LIMIT);
    }

    #[test]
    fn details_leaves_out_what_failed_and_never_fails_itself() {
        let state = scratch_state("details");
        // A name no source has, and a Flatpak ref on a machine that may
        // have no Flatpak: each costs its own reference and nothing else.
        let got = app_details(
            &state,
            vec![
                PackageRef {
                    source: SourceKind::Pacman,
                    id: "no-such-package-brokey-test".into(),
                },
                PackageRef {
                    source: SourceKind::Flatpak,
                    id: "nowhere/app/io.example.NoSuchApp/x86_64/stable".into(),
                },
            ],
        );
        assert!(got.is_empty());
        assert!(app_details(&state, Vec::new()).is_empty());
    }

    #[test]
    fn updates_come_from_the_cache_unless_forced() {
        let state = state_with("updates", empty_store());
        state.store_updates(UpdateList {
            checked_at: 42,
            ..UpdateList::default()
        });
        assert_eq!(updates(&state, false).checked_at, 42);
        let fresh = updates(&state, true);
        assert_ne!(fresh.checked_at, 42, "a forced check asks the sources");
        assert_eq!(
            updates(&state, false).checked_at,
            fresh.checked_at,
            "and is cached in turn"
        );
    }

    #[test]
    fn the_index_refresh_runs_once_a_session_and_again_only_when_forced() {
        let (store, counts) = Fake::store(SourceKind::Pacman);
        let state = state_with("refresh-once", store);
        let refreshes = || counts.refreshes.load(Ordering::SeqCst);

        updates(&state, false);
        assert_eq!(refreshes(), 1, "the first check of a session refreshes");
        updates(&state, false);
        assert_eq!(refreshes(), 1, "the second is answered from the cache");

        // A finished transaction empties the update cache (and forgets the
        // store, which `state.rs` covers: the flag survives that too); the
        // check after it reads what is on disk rather than fetching every
        // package list again.
        state.invalidate_updates();
        updates(&state, false);
        assert_eq!(refreshes(), 1, "no refresh after a transaction");
        updates(&state, true);
        assert_eq!(refreshes(), 2, "a forced check always refreshes");
    }

    // pacman is always a source on Linux, whether or not it is installed.
    // Windows has its own sources (Add/Remove Programs, winget) since Task
    // 8, but pacman itself is Linux-only by design and never one of them,
    // so a plan asking for a pacman package still has nothing to preview
    // against there.
    #[cfg(unix)]
    #[test]
    fn a_preview_is_a_dry_run_with_notices() {
        let state = scratch_state("preview");
        let preview = preview(&state, vec![Op::Install { package: steam() }]).unwrap();
        assert!(preview.plan.id.starts_with("plan-"));
        assert_eq!(preview.plan.ops.len(), 1);
        assert!(preview.notices.is_empty(), "nothing to say on this branch");
        assert!(
            state.plans().is_empty(),
            "a preview runs nothing and records nothing"
        );
    }

    // The AUR is always a source on Linux (its refresh is a no-op by
    // design, its index being the RPC), so this reaches "nothing to do".
    // The AUR is Arch's own user repository, so unlike pacman it was never
    // in line for a Windows counterpart; on Windows this reaches "is not a
    // source on this machine" instead, Task 8 or no Task 8.
    #[cfg(unix)]
    #[test]
    fn a_plan_with_nothing_to_do_is_refused_before_it_starts() {
        let state = scratch_state("empty");
        let err = run_plan(
            &state,
            vec![Op::Refresh {
                source: SourceKind::Aur,
            }],
            |_| {},
        )
        .unwrap_err();
        assert!(err.starts_with("There is nothing to do."), "{err}");
        assert!(state.plans().is_empty());
    }

    // Exercises `start_plan`, which the runner now backs on both
    // platforms.
    #[test]
    fn a_started_plan_settles_and_tells_the_page_when_the_runner_stops() {
        let state = state_with("start", empty_store());
        // A session step that fails at once: no helper, no prompt, and the
        // runner still has to end the plan with a sentence. `sh` and `cmd`
        // are what each platform's own shell always provides, so the test
        // needs nothing beyond what a bare install of either OS has.
        #[cfg(unix)]
        let command = brokey_core::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 3".into()],
            env: Vec::new(),
            cwd: None,
        };
        #[cfg(windows)]
        let command = brokey_core::Command {
            program: "cmd".into(),
            args: vec!["/C".into(), "exit 3".into()],
            env: Vec::new(),
            cwd: None,
        };
        let plan = Plan {
            id: "plan-test".into(),
            ops: vec![Op::Install { package: steam() }],
            steps: vec![brokey_core::Step {
                source: SourceKind::Pacman,
                title: "Failing on purpose".into(),
                command,
                needs_root: false,
                weight: 1,
            }],
        };
        let (tx, rx) = mpsc::channel::<Event>();
        let id = start_plan(&state, state.store(), plan, move |e| {
            let _ = tx.send(e.clone());
        });
        assert_eq!(id, "plan-test");

        // The step fails; the plan must still end with a PlanFinished the
        // page can act on, after whatever came before it.
        let finished = loop {
            let event = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("the page is told");
            if matches!(event, Event::PlanFinished { .. }) {
                break event;
            }
        };
        match &finished {
            Event::PlanFinished { plan, ok, message } => {
                assert_eq!(plan, "plan-test");
                assert!(!ok);
                assert!(message.ends_with('.'), "a sentence: {message}");
            }
            other => panic!("expected PlanFinished, got {other:?}"),
        }
        // Wait for the worker to record the state, which it does before it sends.
        assert_eq!(state.plan_state("plan-test"), Some(PlanState::Failed));
        let status = &state.plans()[0];
        assert_eq!(
            status.events.last(),
            Some(&finished),
            "the record and the page agree"
        );
        assert!(status.started > 0);
    }

    /// A winget removal is a root step; on Windows it must reach the
    /// runner rather than be refused here for being on Windows. Building
    /// against a real winget source would need the machine's actual
    /// catalogue, so the plan comes from a fake that hands back one root
    /// step instead; what is under test is that `run_plan` starts it
    /// rather than answering with a platform sentence.
    #[cfg(windows)]
    #[test]
    fn a_winget_removal_reaches_the_runner_rather_than_a_platform_refusal() {
        let store = Store {
            system: brokey_core::system::from_os_release(""),
            sources: vec![Box::new(FakeWinget)],
        };
        let state = state_with("winget-removal", store);
        let package = PackageRef {
            source: SourceKind::Winget,
            id: "Test.App".into(),
        };

        let preview = preview(
            &state,
            vec![Op::Remove {
                package: package.clone(),
            }],
        )
        .unwrap();
        assert_eq!(preview.plan.steps.len(), 1, "the fake source's root step");

        let id = run_plan(
            &state,
            vec![Op::Remove {
                package: package.clone(),
            }],
            |_| {},
        )
        .unwrap();
        assert!(
            !id.is_empty(),
            "the plan reached the runner and was given an id"
        );
        assert!(
            state.plans().iter().any(|p| p.plan.id == id),
            "a started plan is on record"
        );
    }

    #[test]
    fn a_successful_plan_forgets_the_store_and_the_update_list() {
        let state = state_with("invalidate", empty_store());
        let plan = Plan {
            id: "plan-ok".into(),
            ops: Vec::new(),
            steps: Vec::new(),
        };
        let store = state.store();
        state.store_updates(UpdateList::default());
        state.add_plan(plan);
        // The sink's policy is what is under test, so drive it the way the
        // runner would rather than through a runner that cannot succeed here.
        let (tx, rx) = mpsc::channel::<Event>();
        let emit = move |e: &Event| {
            let _ = tx.send(e.clone());
        };
        deliver(
            &state,
            &store,
            "plan-ok",
            Event::Log {
                plan: "plan-ok".into(),
                step: 0,
                line: "resolving dependencies...".into(),
                stderr: false,
            },
            &emit,
        );
        assert!(state.has_store(), "a log line changes nothing");
        deliver(
            &state,
            &store,
            "plan-ok",
            Event::PlanFinished {
                plan: "plan-ok".into(),
                ok: true,
                message: "Installed.".into(),
            },
            &emit,
        );
        assert_eq!(state.plan_state("plan-ok"), Some(PlanState::Done));
        assert!(
            !state.has_store(),
            "the store is detected again on the next command"
        );
        assert!(
            state.cached_updates(Duration::from_secs(600)).is_none(),
            "the update list is stale"
        );
        assert_eq!(rx.try_iter().count(), 2, "both events reached the page");
    }

    /// The sources that built a plan are told how it ended, even when
    /// another plan finished first and reset the state's store: a GitHub
    /// install record would otherwise be lost.
    #[test]
    fn the_sources_that_built_a_plan_are_told_how_it_ended_after_a_reset() {
        let (store, counts) = Fake::store(SourceKind::Github);
        let state = state_with("finished", store);
        let op = Op::Install {
            package: PackageRef {
                source: SourceKind::Github,
                id: "sharkdp/bat".into(),
            },
        };
        let bound = state.store();
        state.add_plan(Plan {
            id: "plan-b".into(),
            ops: vec![op.clone()],
            steps: Vec::new(),
        });
        // Plan A finishes first and resets the store under plan B.
        state.invalidate_after_transaction();
        assert!(!state.has_store());
        deliver(
            &state,
            &bound,
            "plan-b",
            Event::PlanFinished {
                plan: "plan-b".into(),
                ok: true,
                message: "Installed.".into(),
            },
            &|_| {},
        );
        let finished = counts.finished.lock().unwrap();
        assert_eq!(finished.as_slice(), &[(op, true)]);
    }

    /// A plan that sets a source up forgets the store however it ended, so
    /// the next `sources` or `search` detects the tool the plan installed.
    /// Any other failed plan leaves the store alone.
    #[test]
    fn a_setup_plan_forgets_the_store_even_when_it_fails() {
        for (ops, ok, forgotten) in [
            (
                vec![Op::Setup {
                    source: SourceKind::Flatpak,
                }],
                false,
                true,
            ),
            (
                vec![Op::Setup {
                    source: SourceKind::Snap,
                }],
                true,
                true,
            ),
            (vec![Op::Install { package: steam() }], false, false),
        ] {
            let (store, _) = Fake::store(SourceKind::Pacman);
            let state = state_with("setup-reset", store);
            let bound = state.store();
            state.add_plan(Plan {
                id: "plan-s".into(),
                ops,
                steps: Vec::new(),
            });
            deliver(
                &state,
                &bound,
                "plan-s",
                Event::PlanFinished {
                    plan: "plan-s".into(),
                    ok,
                    message: "Finished.".into(),
                },
                &|_| {},
            );
            assert_eq!(!state.has_store(), forgotten, "ok {ok}");
        }
    }

    #[test]
    fn a_self_update_check_respects_the_setting_unless_forced() {
        // A checker that answers from a table and remembers whether it was
        // asked to go past the disk cache, so GitHub is never asked here.
        let asked: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = asked.clone();
        let checker: SelfUpdateChecker = Arc::new(move |fresh: bool| {
            seen.lock().unwrap().push(fresh);
            Ok(selfupdate_adapter::unchecked())
        });
        let state =
            AppState::with_store_and_checker(scratch_path("selfupdate"), empty_store(), checker);
        let mut settings = state.settings();
        settings.self_update_check = false;
        state.set_settings(settings).unwrap();

        let unforced = self_update_check(&state, false).unwrap();
        assert_eq!(unforced.current, env!("CARGO_PKG_VERSION"));
        assert!(
            state
                .cached_self_update(Duration::from_secs(3600))
                .is_none(),
            "nothing was asked, so nothing was cached"
        );
        assert!(asked.lock().unwrap().is_empty(), "the setting was honoured");

        let forced = self_update_check(&state, true).unwrap();
        assert_eq!(forced.current, env!("CARGO_PKG_VERSION"));
        assert!(
            state
                .cached_self_update(Duration::from_secs(3600))
                .is_some()
        );
        assert_eq!(
            asked.lock().unwrap().as_slice(),
            &[true],
            "a forced check goes past the disk cache"
        );

        let err = self_update_apply(&state, |_| {}).unwrap_err();
        assert!(err.starts_with("Brokey is up to date"), "{err}");
        assert_eq!(
            asked.lock().unwrap().len(),
            1,
            "apply used the check from a moment ago"
        );
    }

    #[test]
    fn splitting_a_group_is_remembered_once() {
        let state = scratch_state("split");
        let first = group_split(&state, steam()).unwrap();
        let second = group_split(&state, steam()).unwrap();
        assert_eq!(first.split, vec!["pacman:steam".to_string()]);
        assert_eq!(second.split, first.split);
        assert_eq!(state.settings().split, first.split);
    }

    #[test]
    fn the_preview_serialises_with_its_two_fields() {
        let json = serde_json::to_value(PlanPreview {
            plan: Plan {
                id: "p".into(),
                ops: Vec::new(),
                steps: Vec::new(),
            },
            notices: vec!["A notice.".into()],
        })
        .unwrap();
        assert_eq!(json["plan"]["id"], "p");
        assert_eq!(json["notices"][0], "A notice.");
    }
}
