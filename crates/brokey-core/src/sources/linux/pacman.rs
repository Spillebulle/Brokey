//! The pacman source: the distribution's repositories on Arch and its
//! derivatives, read straight from the sync and local databases (`alpmdb`)
//! and dressed with the AppStream catalogue for names, icons and pictures.
//!
//! Everything is answered from memory after one load. The databases are
//! parsed lazily behind a mutex the first time anything asks, and the load is
//! keyed on the files' mtimes and sizes: a `pacman -Sy` or an install by
//! anything else changes those, and the next call reloads. Polling mtimes on
//! every call costs a handful of `stat`s, which is far cheaper than the
//! alternative of watching the directory with inotify and far more robust
//! than trusting a timer.
//!
//! Update checks need no root. `refresh_into_cache` downloads each
//! repository's `.db` into the store's cache directory and `updates` prefers
//! that copy whenever it is newer than the system's, which is exactly what
//! pacman-contrib's `checkupdates` does with its private `--dbpath`. Search
//! and the installed list read the system's databases, which are what the
//! machine has installed against. An install or an update is planned as
//! `pacman -Syu` with the name: pacman refreshes its own databases and
//! upgrades the whole system in the same transaction, which is the only
//! shape Arch supports. `pacman -S name` against a days-old system database
//! asks the mirror for files it no longer has, and `-Sy name` is the
//! partial upgrade the design refuses, so neither is ever planned.

use super::alpmdb::{Desc, LocalDb, SyncDb, glob_matches, is_package_name, read_includes, repos};
use crate::appstream::{Catalogue, Component};
use crate::http::Client;
use crate::model::*;
use crate::vercmp::vercmp;
use crate::{Error, Op, Query, Result, Source};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How many of a repository's mirrors a refresh tries before giving up on
/// it. pacman tries every one; three bounds the wait when a whole country
/// is offline.
const MIRRORS_TRIED: usize = 3;

/// How long a whole refresh may take. The repositories are fetched in
/// parallel and each request is cut off at the deadline, so an updates
/// check never waits longer than this on a mirror that answers slowly.
pub const REFRESH_BUDGET: Duration = Duration::from_secs(20);

/// What a repository is marked with when the deadline passed before any of
/// its mirrors could be asked.
pub const SKIPPED_FOR_TIME: &str = "Not refreshed: the earlier repositories used the time budget.";

/// Where pacman keeps its files on this machine, and where the store keeps
/// its own copies of the sync databases. Tests point every field at a
/// temporary directory.
#[derive(Clone, Debug)]
pub struct Paths {
    pub conf: PathBuf,
    pub sync_dir: PathBuf,
    pub local_dir: PathBuf,
    /// The store's own sync databases, refreshed without root.
    pub cache_dir: PathBuf,
    /// The `pacman` executable to check for, or `None` to look on `PATH`.
    pub pacman: Option<PathBuf>,
}

impl Paths {
    /// The real machine: `/etc/pacman.conf`, honouring an uncommented
    /// `DBPath`, and the store's cache directory for refreshed databases.
    pub fn system() -> Paths {
        let conf = PathBuf::from("/etc/pacman.conf");
        let db_path = std::fs::read_to_string(&conf)
            .ok()
            .and_then(|c| options(&c).db_path)
            .unwrap_or_else(|| PathBuf::from("/var/lib/pacman"));
        Paths {
            conf,
            sync_dir: db_path.join("sync"),
            local_dir: db_path.join("local"),
            cache_dir: crate::system::Dirs::new().cache.join("pacman-sync"),
            pacman: None,
        }
    }
}

/// The `[options]` section of `pacman.conf`, as far as the store needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub db_path: Option<PathBuf>,
    /// The first `Architecture` value; `None` for `auto` or absent, meaning
    /// this machine's.
    pub architecture: Option<String>,
    pub ignore_pkg: Vec<String>,
    pub ignore_group: Vec<String>,
}

impl Options {
    /// What `$arch` expands to in a server URL. pacman substitutes the first
    /// architecture as plain text, which is how CachyOS's `$arch_v3` becomes
    /// `x86_64_v3`.
    pub fn arch(&self) -> String {
        self.architecture
            .clone()
            .unwrap_or_else(|| std::env::consts::ARCH.to_string())
    }
}

/// Parse the `[options]` section. Keys are case-sensitive, as pacman's are.
pub fn options(conf: &str) -> Options {
    let mut out = Options::default();
    let mut in_options = false;
    for line in conf.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_options = name.trim() == "options";
            continue;
        }
        if !in_options {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "DBPath" if !value.is_empty() => out.db_path = Some(PathBuf::from(value)),
            "Architecture" => {
                out.architecture = value
                    .split_whitespace()
                    .next()
                    .filter(|a| *a != "auto")
                    .map(str::to_string);
            }
            "IgnorePkg" => out
                .ignore_pkg
                .extend(value.split_whitespace().map(str::to_string)),
            "IgnoreGroup" => out
                .ignore_group
                .extend(value.split_whitespace().map(str::to_string)),
            _ => {}
        }
    }
    out
}

/// One repository's servers in the order pacman would try them, with
/// `$repo` and `$arch` already substituted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoMirrors {
    pub name: String,
    pub servers: Vec<String>,
}

/// The repositories and their servers, in `pacman.conf` order. `include`
/// answers an `Include =` path (which may be a glob) with the text of every
/// file it names, so the parser is pure and the tests hand it strings.
pub fn mirrors(conf: &str, arch: &str, include: &dyn Fn(&str) -> Vec<String>) -> Vec<RepoMirrors> {
    let mut out: Vec<RepoMirrors> = Vec::new();
    let mut in_repo = false;
    for line in conf.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.trim();
            in_repo = name != "options";
            if in_repo {
                out.push(RepoMirrors {
                    name: name.to_string(),
                    servers: Vec::new(),
                });
            }
            continue;
        }
        let Some(repo) = out.last_mut().filter(|_| in_repo) else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Server" => repo
                .servers
                .push(server_url(value.trim(), &repo.name, arch)),
            "Include" => {
                for text in include(value.trim()) {
                    for inner in text.lines() {
                        let inner = inner.trim();
                        if inner.starts_with('#') {
                            continue;
                        }
                        if let Some((k, v)) = inner.split_once('=')
                            && k.trim() == "Server"
                        {
                            repo.servers.push(server_url(v.trim(), &repo.name, arch));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// pacman's substitution: plain text replacement, so `$arch_v3` with
/// `x86_64` gives `x86_64_v3`, which is what the CachyOS mirrorlists rely on.
fn server_url(template: &str, repo: &str, arch: &str) -> String {
    template
        .replace("$repo", repo)
        .replace("$arch", arch)
        .trim_end_matches('/')
        .to_string()
}

/// What a refresh did, per repository. A repository whose mirrors all
/// failed is listed with the last sentence they failed with; the others are
/// still refreshed, because one dead mirrorlist should not hide the updates
/// the rest have.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Refreshed {
    pub downloaded: Vec<String>,
    pub unchanged: Vec<String>,
    pub failed: Vec<(String, String)>,
}

impl Refreshed {
    /// The sentence for a refresh that achieved nothing: every repository
    /// failed and none was downloaded or found unchanged. `None` when at
    /// least one repository is fresh, or there was nothing to refresh. The
    /// reasons are grouped so one dead mirror is named once, with the
    /// repositories it took down.
    pub fn failure(&self) -> Option<String> {
        if self.failed.is_empty() || !self.downloaded.is_empty() || !self.unchanged.is_empty() {
            return None;
        }
        let mut reasons: Vec<(&str, Vec<&str>)> = Vec::new();
        for (name, why) in &self.failed {
            match reasons.iter_mut().find(|(w, _)| *w == why.as_str()) {
                Some((_, names)) => names.push(name),
                None => reasons.push((why, vec![name])),
            }
        }
        let detail: Vec<String> = reasons
            .iter()
            .map(|(why, names)| format!("{} ({})", why.trim_end_matches('.'), names.join(", ")))
            .collect();
        Some(format!(
            "No package list could be refreshed, so updates are checked against the lists the machine has. {}.",
            detail.join(". ")
        ))
    }
}

/// One repository's outcome inside a refresh, before it is sorted into
/// `Refreshed`.
enum Outcome {
    Downloaded,
    Unchanged,
    Failed(String),
}

/// An installed package that no enabled repository provides: built from
/// the AUR, or installed from a file. The AUR source asks for these.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForeignPackage {
    pub name: String,
    pub version: String,
    /// The `pkgbase`, which is what the AUR keys split packages on.
    pub base: Option<String>,
    pub description: Option<String>,
    pub url: Option<String>,
    /// Unix seconds.
    pub installed_at: Option<i64>,
    pub installed_size: Option<u64>,
}

/// The files a load depends on, with their mtime and size. Two equal stamps
/// mean nothing on disk changed and the loaded databases are still right.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Stamp(Vec<(PathBuf, Option<(SystemTime, u64)>)>);

impl Stamp {
    fn take(paths: &Paths, repo_dbs: &[(String, PathBuf)]) -> Stamp {
        let mut files = vec![paths.conf.clone(), paths.local_dir.clone()];
        for (name, sync_path) in repo_dbs {
            files.push(sync_path.clone());
            files.push(paths.cache_dir.join(format!("{name}.db")));
        }
        Stamp(
            files
                .into_iter()
                .map(|p| (p.clone(), file_stamp(&p)))
                .collect(),
        )
    }
}

fn file_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn mtime(path: &Path) -> Option<SystemTime> {
    file_stamp(path).map(|(t, _)| t)
}

/// One searchable name: the first repository that provides it, with the
/// lower-cased text the query is matched against prepared once at load so
/// a search over fifteen thousand packages allocates nothing.
struct Entry {
    name: String,
    name_l: String,
    component_name_l: Option<String>,
    text_l: String,
    has_component: bool,
}

/// One set of databases, with the repositories in `pacman.conf` order and
/// the first repository that provides a name winning, as pacman's own rule.
struct View {
    dbs: Vec<Arc<SyncDb>>,
    index: HashMap<String, usize>,
}

impl View {
    fn build(dbs: Vec<Arc<SyncDb>>) -> View {
        let mut index = HashMap::new();
        for (i, db) in dbs.iter().enumerate() {
            for name in db.packages.keys() {
                index.entry(name.clone()).or_insert(i);
            }
        }
        View { dbs, index }
    }

    fn get(&self, name: &str) -> Option<(&Desc, &str)> {
        let &i = self.index.get(name)?;
        let db = &self.dbs[i];
        db.get(name).map(|d| (d, db.repo.as_str()))
    }
}

struct Loaded {
    stamp: Stamp,
    /// The system's databases: what `pacman -S` acts on.
    system: View,
    /// The system's databases with the cache's copy substituted wherever it
    /// is newer: what `updates` compares against. `None` when no cached copy
    /// is newer, so the common case builds one index, not two.
    fresh: Option<View>,
    local: LocalDb,
    entries: Vec<Entry>,
    options: Options,
}

impl Loaded {
    fn for_updates(&self) -> &View {
        self.fresh.as_ref().unwrap_or(&self.system)
    }
}

pub struct Pacman {
    paths: Paths,
    catalogue: Arc<Catalogue>,
    dbs: Mutex<Option<Arc<Loaded>>>,
    /// Where installed files are, `/` on a real machine; a fixture tree in
    /// tests, so a test does not read this machine's desktop entries.
    root: PathBuf,
    /// Desktop entries read for installed packages, by `name version`: a
    /// package's files only change when its version does.
    entries: Mutex<HashMap<String, Option<crate::launch::EntryInfo>>>,
    icon_roots: std::sync::OnceLock<Vec<PathBuf>>,
}

impl Pacman {
    pub fn new(_system: &SystemInfo, catalogue: Arc<Catalogue>) -> Pacman {
        Pacman::with_paths(Paths::system(), catalogue)
    }

    pub fn with_paths(paths: Paths, catalogue: Arc<Catalogue>) -> Pacman {
        Pacman {
            paths,
            catalogue,
            dbs: Mutex::new(None),
            root: PathBuf::from("/"),
            entries: Mutex::new(HashMap::new()),
            icon_roots: std::sync::OnceLock::new(),
        }
    }

    /// Read installed files, and icon themes, under `root` instead of `/`.
    pub fn with_root(mut self, root: PathBuf) -> Pacman {
        let icons = root.join("usr/share/icons");
        let _ = self.icon_roots.set(crate::launch::icon_roots_under(&icons));
        self.root = root;
        self
    }

    /// The desktop entry an installed package ships, when it ships one a
    /// launcher lists. This is how an installed package is known to be an
    /// application on a machine with no AppStream catalogue, and the only
    /// way at all for a package no catalogue describes.
    fn installed_entry(&self, name: &str, version: &str) -> Option<crate::launch::EntryInfo> {
        let key = format!("{name} {version}");
        if let Some(known) = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return known.clone();
        }
        let found = super::alpmdb::local_files(&self.paths.local_dir, name).and_then(|files| {
            let roots = self.icon_roots.get_or_init(crate::launch::icon_roots);
            crate::launch::app_entry(files.iter().map(String::as_str), &self.root, &[name], roots)
        });
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, found.clone());
        found
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Every installed package that no enabled repository provides: what
    /// the AUR source lists as installed, and what `installed` here leaves
    /// out. A plain list rather than a `Result`, because the caller is
    /// another source answering for itself: when the databases cannot be
    /// read this source's `status` already says so, and the AUR source then
    /// has nothing foreign to show.
    pub fn foreign_packages(&self) -> Vec<ForeignPackage> {
        let loaded = match self.loaded() {
            Ok(loaded) => loaded,
            Err(e) => {
                log::warn!("pacman: cannot list foreign packages: {}", e.message);
                return Vec::new();
            }
        };
        let mut out: Vec<ForeignPackage> = loaded
            .local
            .packages
            .values()
            .filter(|d| !loaded.system.index.contains_key(d.name()))
            .map(|d| ForeignPackage {
                name: d.name().to_string(),
                version: d.version().to_string(),
                base: d.first("BASE").map(str::to_string),
                description: d.first("DESC").map(str::to_string),
                url: d.first("URL").map(str::to_string),
                installed_at: d.i64("INSTALLDATE"),
                installed_size: d.u64("SIZE"),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Whether an enabled repository provides the name. `false` when the
    /// databases cannot be read, since nothing can then be claimed.
    pub fn is_repo_package(&self, name: &str) -> bool {
        self.loaded()
            .map(|l| l.system.index.contains_key(name))
            .unwrap_or(false)
    }

    /// Download each repository's database into the cache directory from
    /// its mirrors, so `updates` can compare against fresh lists without
    /// root. A conditional request is made against the newer of the system's
    /// and the cache's copy, so a repository that has not changed costs one
    /// small round trip rather than a download. The mtime of a downloaded
    /// file is set from the server's `Last-Modified`, as pacman does, which
    /// is what makes "newer than the system's" a fair comparison.
    ///
    /// The whole refresh is bounded by [`REFRESH_BUDGET`]. `Err` when the
    /// configuration or the cache directory cannot be used, and when every
    /// repository failed so that nothing is fresh: the caller then has a
    /// sentence to report rather than a silent stale answer.
    pub fn refresh_into_cache(&self, client: &Client) -> Result<Refreshed> {
        let refreshed = self.refresh_with_deadline(client, Instant::now() + REFRESH_BUDGET)?;
        match refreshed.failure() {
            Some(sentence) => Err(self.error(sentence)),
            None => Ok(refreshed),
        }
    }

    /// [`refresh_into_cache`](Self::refresh_into_cache) with the deadline
    /// as a parameter and every per-repository outcome reported in the
    /// result, so the budget can be tested without a slow mirror. The
    /// repositories are fetched in parallel; a request is cut off at the
    /// deadline, and a repository that could not be started before it is
    /// listed as failed with [`SKIPPED_FOR_TIME`].
    pub fn refresh_with_deadline(&self, client: &Client, deadline: Instant) -> Result<Refreshed> {
        let conf = std::fs::read_to_string(&self.paths.conf).map_err(|e| {
            self.error(format!(
                "Could not read {}: {e}. The repositories are not known without it.",
                self.paths.conf.display()
            ))
        })?;
        let arch = options(&conf).arch();
        let list = mirrors(&conf, &arch, &read_includes);
        std::fs::create_dir_all(&self.paths.cache_dir).map_err(|e| {
            self.error(format!(
                "Could not create {}: {e}. Check that the cache directory is writable.",
                self.paths.cache_dir.display()
            ))
        })?;
        let outcomes: Vec<Outcome> = std::thread::scope(|scope| {
            let handles: Vec<_> = list
                .iter()
                .map(|repo| scope.spawn(move || self.refresh_one(client, repo, deadline)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("refreshing a repository does not panic"))
                .collect()
        });
        let mut out = Refreshed::default();
        for (repo, outcome) in list.iter().zip(outcomes) {
            match outcome {
                Outcome::Downloaded => out.downloaded.push(repo.name.clone()),
                Outcome::Unchanged => out.unchanged.push(repo.name.clone()),
                Outcome::Failed(why) => out.failed.push((repo.name.clone(), why)),
            }
        }
        Ok(out)
    }

    /// One repository: its mirrors in order until one answers, each request
    /// given what is left of the budget.
    fn refresh_one(&self, client: &Client, repo: &RepoMirrors, deadline: Instant) -> Outcome {
        let target = self.paths.cache_dir.join(format!("{}.db", repo.name));
        let system = self.paths.sync_dir.join(format!("{}.db", repo.name));
        let since = [mtime(&system), mtime(&target)].into_iter().flatten().max();
        if repo.servers.is_empty() {
            return Outcome::Failed("No Server line in pacman.conf or its mirrorlist.".to_string());
        }
        let mut last_error: Option<String> = None;
        for server in repo.servers.iter().take(MIRRORS_TRIED) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let url = format!("{server}/{}.db", repo.name);
            match fetch(client, &url, since, remaining) {
                Ok(Fetched::Unchanged) => {
                    log::debug!("pacman: {} is unchanged at {url}", repo.name);
                    return Outcome::Unchanged;
                }
                Ok(Fetched::Body(bytes, last_modified)) => {
                    if let Err(e) = write_db(&target, &bytes, last_modified) {
                        return Outcome::Failed(e.message);
                    }
                    log::debug!(
                        "pacman: downloaded {} ({} bytes) from {url}",
                        repo.name,
                        bytes.len()
                    );
                    return Outcome::Downloaded;
                }
                Err(e) => {
                    log::debug!("pacman: {url}: {}", e.message);
                    last_error = Some(e.message);
                }
            }
        }
        Outcome::Failed(last_error.unwrap_or_else(|| SKIPPED_FOR_TIME.to_string()))
    }

    fn error(&self, message: String) -> Error {
        Error::from_source(SourceKind::Pacman, message)
    }

    /// The databases, loaded on first use and reloaded when any of their
    /// files changed. The lock is held through a load so two threads asking
    /// at once share one parse.
    fn loaded(&self) -> Result<Arc<Loaded>> {
        let mut guard = self.dbs.lock().unwrap_or_else(|p| p.into_inner());
        let conf = std::fs::read_to_string(&self.paths.conf).map_err(|e| {
            self.error(format!(
                "Could not read {}: {e}. The repositories are not known without it.",
                self.paths.conf.display()
            ))
        })?;
        let repo_dbs = repos(&conf, &self.paths.sync_dir, &read_includes);
        let stamp = Stamp::take(&self.paths, &repo_dbs);
        if let Some(loaded) = guard.as_ref()
            && loaded.stamp == stamp
        {
            return Ok(loaded.clone());
        }
        let loaded = Arc::new(self.load(&conf, &repo_dbs, stamp)?);
        *guard = Some(loaded.clone());
        Ok(loaded)
    }

    fn load(&self, conf: &str, repo_dbs: &[(String, PathBuf)], stamp: Stamp) -> Result<Loaded> {
        let started = Instant::now();
        let options = options(conf);
        // Which file to read for each repository: the system's, and the
        // cache's as well when it is newer. Both are decided before the
        // threads start so the stamp taken a moment ago still describes
        // what was read.
        let plan: Vec<(String, Option<PathBuf>, Option<PathBuf>)> = repo_dbs
            .iter()
            .map(|(name, sync_path)| {
                let cached = self.paths.cache_dir.join(format!("{name}.db"));
                let system = sync_path.exists().then(|| sync_path.clone());
                let cache = match (mtime(&cached), mtime(sync_path)) {
                    (Some(c), Some(s)) if c > s => Some(cached),
                    (Some(_), None) => Some(cached),
                    _ => None,
                };
                (name.clone(), system, cache)
            })
            .collect();
        // One thread per database: the big ones (extra, cachyos-extra) are
        // zstd streams that decompress in pure Rust, and reading them one
        // after another would be most of a second on their own.
        let (local, results) = std::thread::scope(|scope| {
            let local = scope.spawn(|| LocalDb::load(&self.paths.local_dir));
            let handles: Vec<_> = plan
                .iter()
                .map(|(name, system, cache)| {
                    scope.spawn(move || {
                        let system = system.as_ref().map(|p| SyncDb::load(name, p)).transpose()?;
                        let cache = match cache.as_ref().map(|p| (p, SyncDb::load(name, p))) {
                            Some((_, Ok(db))) => Some(db),
                            Some((path, Err(e))) => {
                                // A half-written or corrupt download must
                                // not stick: left in place, its mtime would
                                // make the next refresh ask "modified since"
                                // and get "no".
                                log::warn!(
                                    "pacman: ignoring cached {}: {}",
                                    path.display(),
                                    e.message
                                );
                                let _ = std::fs::remove_file(path);
                                None
                            }
                            None => None,
                        };
                        Ok::<_, Error>((system.map(Arc::new), cache.map(Arc::new)))
                    })
                })
                .collect();
            let local = local
                .join()
                .expect("loading the local database does not panic");
            let results: Vec<_> = handles
                .into_iter()
                .map(|h| h.join().expect("loading a sync database does not panic"))
                .collect();
            (local, results)
        });
        let local = local.map_err(|e| {
            self.error(format!(
                "Could not read the installed packages ({}). Check that {} is readable.",
                e.message,
                self.paths.local_dir.display()
            ))
        })?;
        let mut system_dbs = Vec::new();
        let mut fresh_dbs = Vec::new();
        let mut any_fresh = false;
        for result in results {
            let (system, cache) = result.map_err(|e| {
                self.error(format!(
                    "Could not read a package list ({}). Refresh the package lists to download it again.",
                    e.message
                ))
            })?;
            if cache.is_some() {
                any_fresh = true;
            }
            if let Some(db) = cache.clone().or_else(|| system.clone()) {
                fresh_dbs.push(db);
            }
            if let Some(db) = system {
                system_dbs.push(db);
            }
        }
        let system = View::build(system_dbs);
        let fresh = any_fresh.then(|| View::build(fresh_dbs));
        let entries = self.entries(&system);
        log::debug!(
            "pacman: loaded {} packages from {} repositories and {} installed in {:?}{}",
            entries.len(),
            system.dbs.len(),
            local.packages.len(),
            started.elapsed(),
            if fresh.is_some() {
                " (with newer cached lists for updates)"
            } else {
                ""
            }
        );
        Ok(Loaded {
            stamp,
            system,
            fresh,
            local,
            entries,
            options,
        })
    }

    fn entries(&self, view: &View) -> Vec<Entry> {
        view.index
            .iter()
            .map(|(name, &repo)| {
                let desc = view.dbs[repo].get(name);
                let component = self.component_for(name);
                let mut text_l = desc
                    .and_then(|d| d.first("DESC"))
                    .unwrap_or("")
                    .to_lowercase();
                if let Some(c) = component {
                    if let Some(s) = &c.summary {
                        text_l.push(' ');
                        text_l.push_str(&s.to_lowercase());
                    }
                    for k in &c.keywords {
                        text_l.push(' ');
                        text_l.push_str(&k.to_lowercase());
                    }
                }
                Entry {
                    name: name.clone(),
                    name_l: name.to_lowercase(),
                    component_name_l: component
                        .filter(|c| !c.name.is_empty())
                        .map(|c| c.name.to_lowercase()),
                    text_l,
                    has_component: component.is_some(),
                }
            })
            .collect()
    }

    /// The component the catalogue chose for the package: the desktop
    /// application named like it where the package carries several
    /// (`Catalogue::by_pkgname` ranks them).
    fn component_for(&self, name: &str) -> Option<&Component> {
        self.catalogue.by_pkgname(name)
    }

    /// The `Package` for one name in a view. `full` adds what only the
    /// detail page draws: the whole dependency list.
    fn package(&self, loaded: &Loaded, view: &View, name: &str, full: bool) -> Option<Package> {
        let (desc, repo) = view.get(name)?;
        let local = loaded.local.get(name);
        let component = self.component_for(name);
        let mut p = Package::new(SourceKind::Pacman, name, display_name(name, component));
        p.kind = kind_of(name, component);
        let entry = match (component, local) {
            (None, Some(l)) => self.installed_entry(name, l.version()),
            _ => None,
        };
        p.summary = component
            .and_then(|c| c.summary.clone())
            .or_else(|| desc.first("DESC").map(str::to_string));
        p.description = component.and_then(|c| c.description.clone());
        p.version = Some(desc.version().to_string());
        p.installed = local.is_some();
        p.installed_version = local.map(|l| l.version().to_string());
        p.repo = Some(repo.to_string());
        p.licence = joined(desc.all("LICENSE"));
        p.homepage = desc.first("URL").map(str::to_string);
        p.developer = component.and_then(|c| c.developer.clone());
        p.updated = desc.i64("BUILDDATE");
        p.download_size = desc.u64("CSIZE");
        p.installed_size = desc.u64("ISIZE");
        if let Some(c) = component {
            p.icon = c.icon.clone();
            p.screenshots = c.screenshots.clone();
            p.categories = c.categories.clone();
            p.appstream_id = Some(c.id.clone());
        }
        p.facts = facts(desc, local, full);
        if let Some(e) = entry {
            apply_entry(&mut p, e);
        }
        Some(p)
    }

    fn update(&self, name: &str, desc: &Desc, local: &Desc) -> Update {
        let component = self.component_for(name);
        let entry = component
            .is_none()
            .then(|| self.installed_entry(name, local.version()))
            .flatten();
        let mut update = Update {
            package: PackageRef {
                source: SourceKind::Pacman,
                id: name.to_string(),
            },
            name: display_name(name, component),
            kind: kind_of(name, component),
            summary: component
                .and_then(|c| c.summary.clone())
                .or_else(|| desc.first("DESC").map(str::to_string)),
            icon: component.and_then(|c| c.icon.clone()),
            from: Some(local.version().to_string()),
            to: desc.version().to_string(),
            download_size: desc.u64("CSIZE"),
            published: desc.i64("BUILDDATE"),
            is_self: name == "brokey" || name == "brokey-bin",
        };
        if let Some(e) = entry {
            update.kind = PackageKind::App;
            if let Some(n) = e.name {
                update.name = n;
            }
            if update.icon.is_none() {
                update.icon = e.icon.map(Picture::File);
            }
        }
        update
    }

    /// The name a plan step may carry: ours, and shaped like a package name,
    /// so nothing that looks like an option ever reaches `pacman`'s argv.
    fn own<'a>(&self, package: &'a PackageRef) -> Result<&'a str> {
        if package.source != SourceKind::Pacman {
            return Err(self.error(format!(
                "{} belongs to {}, not to pacman.",
                package.id,
                package.source.label()
            )));
        }
        if !is_package_name(&package.id) {
            return Err(self.error(format!("{} is not a name pacman accepts.", package.id)));
        }
        Ok(&package.id)
    }

    fn step(&self, title: String, args: &[&str], weight: u32) -> Step {
        Step {
            source: SourceKind::Pacman,
            title,
            command: Command {
                program: "pacman".to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: vec![("LC_ALL".to_string(), "C.UTF-8".to_string())],
                cwd: None,
            },
            needs_root: true,
            weight,
        }
    }
}

impl Source for Pacman {
    fn kind(&self) -> SourceKind {
        SourceKind::Pacman
    }

    fn status(&self) -> SourceStatus {
        let kind = self.kind();
        let has_pacman = match &self.paths.pacman {
            Some(p) => p.exists(),
            None => crate::system::which("pacman").is_some(),
        };
        let reason = if !has_pacman {
            Some("pacman is not installed on this machine.".to_string())
        } else if !self.paths.conf.is_file() {
            Some(format!(
                "{} is missing, so the repositories are not known.",
                self.paths.conf.display()
            ))
        } else if !self.paths.local_dir.is_dir() {
            Some(format!(
                "{} is missing, so the installed packages cannot be read.",
                self.paths.local_dir.display()
            ))
        } else {
            None
        };
        if reason.is_some() {
            return SourceStatus {
                kind,
                available: false,
                reason,
                detail: None,
                searchable: false,
                setup: None,
            };
        }
        let conf = std::fs::read_to_string(&self.paths.conf).unwrap_or_default();
        let present: Vec<String> = repos(&conf, &self.paths.sync_dir, &read_includes)
            .into_iter()
            .filter(|(_, path)| path.is_file())
            .map(|(name, _)| name)
            .collect();
        let detail = if present.is_empty() {
            "no package lists yet".to_string()
        } else {
            present.join(", ")
        };
        SourceStatus {
            kind,
            available: true,
            reason: None,
            detail: Some(detail),
            searchable: false,
            setup: None,
        }
    }

    fn search(&self, query: &Query) -> Result<Vec<Package>> {
        let q = query.text.trim().to_lowercase();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let terms: Vec<&str> = q.split_whitespace().collect();
        let loaded = self.loaded()?;
        let mut hits: Vec<(f32, &Entry)> = loaded
            .entries
            .iter()
            .filter_map(|e| {
                let base = score(
                    &e.name_l,
                    e.component_name_l.as_deref(),
                    &e.text_l,
                    &q,
                    &terms,
                )?;
                let mut s = base;
                if e.has_component {
                    s += 0.05;
                }
                if loaded.local.get(&e.name).is_some() {
                    s += 0.03;
                }
                Some((s, e))
            })
            .collect();
        hits.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.1.name.cmp(&b.1.name))
        });
        if query.limit > 0 {
            hits.truncate(query.limit);
        }
        Ok(hits
            .into_iter()
            .filter_map(|(_, e)| self.package(&loaded, &loaded.system, &e.name, false))
            .collect())
    }

    fn installed(&self) -> Result<Vec<Package>> {
        let loaded = self.loaded()?;
        let mut names: Vec<&String> = loaded
            .local
            .packages
            .keys()
            .filter(|n| loaded.system.index.contains_key(*n))
            .collect();
        names.sort();
        Ok(names
            .into_iter()
            .filter_map(|n| self.package(&loaded, &loaded.system, n, false))
            .collect())
    }

    fn updates(&self) -> Result<Vec<Update>> {
        let loaded = self.loaded()?;
        let view = loaded.for_updates();
        let options = &loaded.options;
        let mut out: Vec<Update> = loaded
            .local
            .packages
            .iter()
            .filter_map(|(name, local)| {
                let (desc, _) = view.get(name)?;
                if is_ignored(options, name, desc.all("GROUPS")) {
                    return None;
                }
                (vercmp(desc.version(), local.version()) == Ordering::Greater)
                    .then(|| self.update(name, desc, local))
            })
            .collect();
        out.sort_by(|a, b| a.package.id.cmp(&b.package.id));
        Ok(out)
    }

    fn details(&self, id: &str) -> Result<Package> {
        let loaded = self.loaded()?;
        self.package(&loaded, &loaded.system, id, true)
            .ok_or_else(|| self.error(format!("{id} is not in any enabled repository.")))
    }

    /// Fresh sync databases into the cache, the way `checkupdates` does,
    /// so a check for updates needs no root. `Err` when nothing could be
    /// refreshed, with the sentence the caller reports.
    fn refresh_index(&self) -> Result<()> {
        self.refresh_into_cache(&crate::http::Client::shared())
            .map(|_| ())
    }

    /// An install or an update is `pacman -Syu --needed <name>`: the
    /// databases pacman acts on are refreshed and the whole system brought
    /// up to date in the one transaction, which is what pamac does and the
    /// only install Arch supports. `--needed` keeps an already current name
    /// from being reinstalled. A refresh plans nothing, because
    /// [`refresh_index`](Source::refresh_index) does it without root and a
    /// bare `-Sy` would set up the partial upgrade the design refuses.
    /// The desktop entry among the files pacman recorded for the package,
    /// preferring one named after the package or after its AppStream id.
    fn launcher(&self, id: &str) -> Option<crate::launch::Launch> {
        let files = super::alpmdb::local_files(&self.paths.local_dir, id)?;
        let component_id = self.catalogue.by_pkgname(id).map(|c| c.id.clone());
        let last_segment = component_id
            .as_deref()
            .and_then(|c| c.rsplit('.').next())
            .map(str::to_string);
        let mut preferred: Vec<&str> = vec![id];
        if let Some(c) = component_id.as_deref() {
            preferred.push(c);
        }
        if let Some(l) = last_segment.as_deref() {
            preferred.push(l);
        }
        crate::launch::from_files(files.iter().map(String::as_str), Path::new("/"), &preferred)
    }

    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        let step = match op {
            Op::Install { package } => {
                let name = self.own(package)?;
                self.step(
                    format!("Installing {name} and updating the system"),
                    &["-Syu", "--noconfirm", "--needed", name],
                    3,
                )
            }
            Op::Remove { package } => {
                let name = self.own(package)?;
                self.step(format!("Removing {name}"), &["-Rs", "--noconfirm", name], 3)
            }
            Op::Update { package } => {
                let name = self.own(package)?;
                self.step(
                    format!("Updating {name} and the system"),
                    &["-Syu", "--noconfirm", "--needed", name],
                    3,
                )
            }
            Op::UpdateAll { source } => {
                self.own_source(*source)?;
                self.step(
                    "Updating the system".to_string(),
                    &["-Syu", "--noconfirm"],
                    10,
                )
            }
            Op::Refresh { source } => {
                self.own_source(*source)?;
                return Ok(Vec::new());
            }
            // The planner expands a setup through `Source::setup`.
            Op::Setup { .. } => return Ok(Vec::new()),
        };
        Ok(vec![step])
    }
}

/// A shared handle is a source too, so one loaded set of databases can sit
/// in the store's list and also be handed to the AUR source, which asks it
/// what is foreign. The alternative, a second `Pacman` built by the AUR
/// source, would parse every database twice and hold it twice.
impl Source for Arc<Pacman> {
    fn kind(&self) -> SourceKind {
        Pacman::kind(self)
    }
    fn status(&self) -> SourceStatus {
        Pacman::status(self)
    }
    fn search(&self, query: &Query) -> Result<Vec<Package>> {
        Pacman::search(self, query)
    }
    fn installed(&self) -> Result<Vec<Package>> {
        Pacman::installed(self)
    }
    fn updates(&self) -> Result<Vec<Update>> {
        Pacman::updates(self)
    }
    fn details(&self, id: &str) -> Result<Package> {
        Pacman::details(self, id)
    }
    fn refresh_index(&self) -> Result<()> {
        Source::refresh_index(self.as_ref())
    }

    fn launcher(&self, id: &str) -> Option<crate::launch::Launch> {
        Source::launcher(self.as_ref(), id)
    }

    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        Pacman::plan(self, op)
    }
}

impl Pacman {
    fn own_source(&self, source: SourceKind) -> Result<()> {
        if source == SourceKind::Pacman {
            Ok(())
        } else {
            Err(self.error(format!(
                "That operation is for {}, not for pacman.",
                source.label()
            )))
        }
    }
}

/// How well a name matches. `None` is no match at all. The name scores are
/// pacman's own name first, then the component's ("Visual Studio Code" for
/// `code`), so a person typing what the launcher shows still lands on it.
fn score(
    name_l: &str,
    component_name_l: Option<&str>,
    text_l: &str,
    q: &str,
    terms: &[&str],
) -> Option<f32> {
    let by_name = |n: &str| {
        if n == q {
            Some(1.0)
        } else if n.starts_with(q) {
            Some(0.9)
        } else if n.contains(q) {
            Some(0.7)
        } else {
            None
        }
    };
    let best = [Some(name_l), component_name_l]
        .into_iter()
        .flatten()
        .filter_map(by_name)
        .fold(None, |acc: Option<f32>, s| {
            Some(acc.map_or(s, |a| a.max(s)))
        });
    if best.is_some() {
        return best;
    }
    let all_terms = terms
        .iter()
        .all(|t| name_l.contains(t) || word_contains(text_l, t));
    all_terms.then_some(0.4)
}

/// Whether `IgnorePkg` or `IgnoreGroup` in `pacman.conf` keeps the package
/// out of an upgrade. Both allow shell globs (`nvidia*`), matched the way
/// pacman matches them.
fn is_ignored(options: &Options, name: &str, groups: &[String]) -> bool {
    options
        .ignore_pkg
        .iter()
        .any(|pattern| glob_matches(pattern, name))
        || groups.iter().any(|group| {
            options
                .ignore_group
                .iter()
                .any(|pattern| glob_matches(pattern, group))
        })
}

fn word_contains(text_l: &str, term: &str) -> bool {
    text_l
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| !w.is_empty() && w.contains(term))
}

/// What a desktop entry lends a package no catalogue describes: it is an
/// application, called what its launcher calls it, with that icon and those
/// categories. The package's own description stays the summary where it has
/// one; the entry's comment fills in where it does not.
pub fn apply_entry(p: &mut Package, entry: crate::launch::EntryInfo) {
    p.kind = PackageKind::App;
    if let Some(name) = entry.name.filter(|n| !n.trim().is_empty()) {
        p.name = name;
    }
    if p.summary.as_deref().is_none_or(|s| s.trim().is_empty()) {
        p.summary = entry.comment;
    }
    if p.icon.is_none() {
        p.icon = entry.icon.map(Picture::File);
    }
    if p.categories.is_empty() {
        p.categories = entry.categories;
    }
}

fn display_name(name: &str, component: Option<&Component>) -> String {
    component
        .map(|c| c.name.trim())
        .filter(|n| !n.is_empty())
        .unwrap_or(name)
        .to_string()
}

/// An application when the catalogue says so; a font by the naming
/// convention Arch uses for every font package, since fonts carry no
/// component; a plain package otherwise.
fn kind_of(name: &str, component: Option<&Component>) -> PackageKind {
    if component.is_some_and(|c| c.is_app) {
        PackageKind::App
    } else if name.starts_with("ttf-")
        || name.starts_with("otf-")
        || name.ends_with("-fonts")
        || name.ends_with("-font")
    {
        PackageKind::Font
    } else {
        PackageKind::Package
    }
}

fn joined(values: &[String]) -> Option<String> {
    (!values.is_empty()).then(|| values.join(", "))
}

fn facts(desc: &Desc, local: Option<&Desc>, full: bool) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(p) = desc.first("PACKAGER") {
        out.push(("Packager".to_string(), p.to_string()));
    }
    if let Some(t) = desc.i64("BUILDDATE") {
        out.push(("Build date".to_string(), date(t)));
    }
    if let Some(t) = local.and_then(|l| l.i64("INSTALLDATE")) {
        out.push(("Install date".to_string(), date(t)));
    }
    let depends = desc.all("DEPENDS");
    let depends_value = if depends.is_empty() {
        "None".to_string()
    } else if full {
        depends.join(", ")
    } else {
        count(depends.len(), "package")
    };
    out.push(("Depends".to_string(), depends_value));
    if let Some(g) = joined(desc.all("GROUPS")) {
        out.push(("Groups".to_string(), g));
    }
    if let Some(p) = joined(desc.all("PROVIDES")) {
        out.push(("Provides".to_string(), p));
    }
    if let Some(a) = desc.first("ARCH") {
        out.push(("Architecture".to_string(), a.to_string()));
    }
    out
}

fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Unix seconds as an ISO date in UTC, "2026-08-21". Build dates are what
/// pacman prints in local time; the date alone is what the page draws.
pub fn date(unix: i64) -> String {
    let (y, m, d) = civil_from_days(unix.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's algorithm, proleptic Gregorian. Used for the two dates
/// this module formats and the one it parses, in preference to a date crate
/// the workspace does not otherwise need.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// The IMF-fixdate form HTTP wants: "Sun, 06 Nov 1994 08:49:37 GMT".
fn http_date(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let weekday = WEEKDAYS[(days + 4).rem_euclid(7) as usize];
    format!(
        "{weekday}, {d:02} {} {y:04} {:02}:{:02}:{:02} GMT",
        MONTHS[(m - 1) as usize],
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// The IMF-fixdate form back to unix seconds. The two obsolete forms HTTP
/// still allows (RFC 850, asctime) are not sent by any mirror and are not
/// parsed; a header that does not fit leaves the mtime as the download time.
fn parse_http_date(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 6 || parts[5] != "GMT" {
        return None;
    }
    let d: u32 = parts[1].parse().ok()?;
    let m = MONTHS.iter().position(|m| *m == parts[2])? as u32 + 1;
    let y: i64 = parts[3].parse().ok()?;
    let mut clock = parts[4].split(':');
    let h: i64 = clock.next()?.parse().ok()?;
    let min: i64 = clock.next()?.parse().ok()?;
    let sec: i64 = clock.next()?.parse().ok()?;
    if clock.next().is_some() || d == 0 || d > 31 || h > 23 || min > 59 || sec > 60 {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400 + h * 3600 + min * 60 + sec)
}

fn unix_of(t: SystemTime) -> Option<i64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

enum Fetched {
    Unchanged,
    Body(Vec<u8>, Option<i64>),
}

/// One conditional request, allowed at most `timeout` (what is left of the
/// refresh budget) for the whole exchange.
fn fetch(
    client: &Client,
    url: &str,
    since: Option<SystemTime>,
    timeout: Duration,
) -> Result<Fetched> {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or(url)
        .to_string();
    let mut request = client.raw().get(url).timeout(timeout);
    if let Some(t) = since.and_then(unix_of) {
        request = request.header("If-Modified-Since", http_date(t));
    }
    let response = request.send().map_err(|e| {
        Error::from_source(
            SourceKind::Pacman,
            if e.is_timeout() {
                format!("{host} did not answer in time.")
            } else if e.is_connect() {
                format!("Could not reach {host}.")
            } else {
                format!("{host}: {e}.")
            },
        )
    })?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(Fetched::Unchanged);
    }
    if !response.status().is_success() {
        return Err(Error::from_source(
            SourceKind::Pacman,
            format!("{host} answered {}.", response.status()),
        ));
    }
    let last_modified = response
        .headers()
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_http_date);
    let bytes = response
        .bytes()
        .map_err(|e| Error::from_source(SourceKind::Pacman, format!("{host}: {e}.")))?
        .to_vec();
    if !looks_like_db(&bytes) {
        return Err(Error::from_source(
            SourceKind::Pacman,
            format!("{host} sent something that is not a package database."),
        ));
    }
    Ok(Fetched::Body(bytes, last_modified))
}

/// gzip, zstd, or a plain tar with its `ustar` mark. A captive portal's
/// HTML page is none of these and must not be cached as a database.
fn looks_like_db(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x1f, 0x8b])
        || bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd])
        || bytes.get(257..262) == Some(b"ustar")
}

/// Write the download beside its final name and rename, so a reader never
/// sees half a file, then stamp it with the server's time.
fn write_db(target: &Path, bytes: &[u8], last_modified: Option<i64>) -> Result<()> {
    let describe = |e: std::io::Error| {
        Error::from_source(
            SourceKind::Pacman,
            format!(
                "Could not write {}: {e}. Check that the cache directory is writable.",
                target.display()
            ),
        )
    };
    let tmp = target.with_extension("part");
    std::fs::write(&tmp, bytes).map_err(describe)?;
    std::fs::rename(&tmp, target).map_err(describe)?;
    if let Some(t) = last_modified
        && t >= 0
        && let Ok(file) = std::fs::File::options().write(true).open(target)
    {
        let _ = file.set_modified(UNIX_EPOCH + Duration::from_secs(t as u64));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONF: &str = "[options]\nHoldPkg = pacman glibc\nArchitecture = auto\nIgnorePkg = linux-cachyos\nIgnorePkg = steam paru nvidia*\nIgnoreGroup = base-devel\n\n[cachyos-v3] \nInclude = /etc/pacman.d/cachyos-v3-mirrorlist \n\n#[core-testing]\n#Include = /etc/pacman.d/mirrorlist\n\n[core]\nServer = https://first.example/$repo/os/$arch\nInclude = /etc/pacman.d/mirrorlist\n\n[custom]\nSigLevel = Optional TrustAll\nServer = file:///home/custompkgs\n";

    fn includes(path: &str) -> Vec<String> {
        match path {
            "/etc/pacman.d/cachyos-v3-mirrorlist" => vec![
                "## CachyOS v3\nServer = https://mirror.krfoss.org/cachyos/repo/$arch_v3/$repo\nServer = https://cdn77.cachyos.org/repo/$arch_v3/$repo\n".to_string(),
            ],
            "/etc/pacman.d/mirrorlist" => vec!["Server = https://fastly.mirror.pkgbuild.com/$repo/os/$arch\n".to_string()],
            _ => Vec::new(),
        }
    }

    #[test]
    fn options_read_the_options_section_only() {
        let o = options(CONF);
        assert_eq!(o.db_path, None);
        assert_eq!(o.architecture, None);
        assert_eq!(o.ignore_pkg, ["linux-cachyos", "steam", "paru", "nvidia*"]);
        assert_eq!(o.ignore_group, ["base-devel"]);
        let explicit = options(
            "[options]\nDBPath = /mnt/pacman/\nArchitecture = x86_64 x86_64_v3\n[core]\nIgnorePkg = not-here\n",
        );
        assert_eq!(explicit.db_path, Some(PathBuf::from("/mnt/pacman/")));
        assert_eq!(explicit.arch(), "x86_64");
        assert!(explicit.ignore_pkg.is_empty());
    }

    #[test]
    fn mirrors_substitute_repo_and_arch_the_way_cachyos_needs() {
        let m = mirrors(CONF, "x86_64", &includes);
        let names: Vec<&str> = m.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["cachyos-v3", "core", "custom"]);
        assert_eq!(
            m[0].servers,
            [
                "https://mirror.krfoss.org/cachyos/repo/x86_64_v3/cachyos-v3",
                "https://cdn77.cachyos.org/repo/x86_64_v3/cachyos-v3"
            ]
        );
        // A Server line before the Include comes first, as pacman orders them.
        assert_eq!(
            m[1].servers,
            [
                "https://first.example/core/os/x86_64",
                "https://fastly.mirror.pkgbuild.com/core/os/x86_64"
            ]
        );
        assert_eq!(m[2].servers, ["file:///home/custompkgs"]);
    }

    #[test]
    fn scoring_prefers_the_name_then_the_description() {
        let terms = ["steam"];
        assert_eq!(
            score("steam", None, "valve's client", "steam", &terms),
            Some(1.0)
        );
        assert_eq!(
            score("steam-native-runtime", None, "", "steam", &terms),
            Some(0.9)
        );
        assert_eq!(score("lib32-steam", None, "", "steam", &terms), Some(0.7));
        assert_eq!(
            score(
                "proton",
                None,
                "runs windows games through steam",
                "steam",
                &terms
            ),
            Some(0.4)
        );
        assert_eq!(
            score("proton", None, "runs windows games", "steam", &terms),
            None
        );
        // The component's name counts as a name: "code" is Visual Studio Code.
        assert_eq!(
            score(
                "code",
                Some("visual studio code"),
                "",
                "visual studio code",
                &["visual", "studio", "code"]
            ),
            Some(1.0)
        );
        // Every term of a multi-word query has to land somewhere.
        assert_eq!(
            score(
                "firefox",
                None,
                "standalone web browser from mozilla",
                "web browser",
                &["web", "browser"]
            ),
            Some(0.4)
        );
        assert_eq!(
            score(
                "firefox",
                None,
                "standalone web browser from mozilla",
                "web server",
                &["web", "server"]
            ),
            None
        );
    }

    #[test]
    fn kinds_come_from_the_component_or_the_font_convention() {
        let app = Component {
            is_app: true,
            ..Default::default()
        };
        assert_eq!(kind_of("steam", Some(&app)), PackageKind::App);
        assert_eq!(kind_of("ttf-dejavu", None), PackageKind::Font);
        assert_eq!(kind_of("noto-fonts", None), PackageKind::Font);
        assert_eq!(kind_of("bash", None), PackageKind::Package);
        let library = Component::default();
        assert_eq!(kind_of("gtk4", Some(&library)), PackageKind::Package);
    }

    #[test]
    fn dates_format_and_http_dates_round_trip() {
        assert_eq!(date(1_785_004_799), "2026-07-25");
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_http_date("Wed, 09 Sep 2026 21:56:04 GMT"),
            Some(1_788_990_964)
        );
        assert_eq!(http_date(1_788_990_964), "Wed, 09 Sep 2026 21:56:04 GMT");
        assert_eq!(parse_http_date("not a date"), None);
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 CET"), None);
        for t in [
            0i64,
            86_399,
            86_400,
            1_000_000_000,
            1_788_990_964,
            4_102_444_800,
        ] {
            assert_eq!(parse_http_date(&http_date(t)), Some(t), "{t}");
        }
    }

    #[test]
    fn ignorepkg_and_ignoregroup_take_globs_as_pacman_conf_allows() {
        let o = options(CONF);
        let none: &[String] = &[];
        let games = &["games".to_string()];
        assert!(is_ignored(&o, "linux-cachyos", none), "an exact entry");
        assert!(
            !is_ignored(&o, "linux-cachyos-headers", none),
            "exact means exact"
        );
        assert!(is_ignored(&o, "nvidia-utils", none), "IgnorePkg = nvidia*");
        assert!(is_ignored(&o, "nvidia", none));
        assert!(
            !is_ignored(&o, "lib32-nvidia-utils", none),
            "the glob is anchored"
        );
        assert!(
            is_ignored(&o, "gcc", &["base-devel".to_string()]),
            "by group"
        );
        assert!(!is_ignored(&o, "gcc", games));
        let globbed = options("[options]\nIgnoreGroup = base-*\nIgnorePkg = linux-cachyos*\n");
        assert!(is_ignored(&globbed, "gcc", &["base-devel".to_string()]));
        assert!(is_ignored(&globbed, "linux-cachyos-headers", none));
        assert!(!is_ignored(&globbed, "firefox", games));
    }

    #[test]
    fn a_refresh_with_nothing_fresh_is_a_sentence_and_one_fresh_repository_is_not() {
        let mut r = Refreshed::default();
        assert_eq!(r.failure(), None, "nothing to refresh is not a failure");
        r.failed
            .push(("core".into(), "Could not reach mirror.example.".into()));
        r.failed
            .push(("extra".into(), "Could not reach mirror.example.".into()));
        r.failed.push(("custom".into(), SKIPPED_FOR_TIME.into()));
        assert_eq!(
            r.failure().as_deref(),
            Some(
                "No package list could be refreshed, so updates are checked against the lists the machine has. Could not reach mirror.example (core, extra). Not refreshed: the earlier repositories used the time budget (custom)."
            )
        );
        r.unchanged.push("multilib".into());
        assert_eq!(r.failure(), None, "one fresh repository is a refresh");
    }

    #[test]
    fn facts_count_dependencies_unless_asked_for_all_of_them() {
        let desc = Desc::parse(
            "%NAME%\nsteam\n\n%PACKAGER%\nLevente Polyak\n\n%BUILDDATE%\n1785004799\n\n%DEPENDS%\nbash\ncoreutils\n\n%GROUPS%\ngames\n\n%ARCH%\nx86_64\n",
        );
        let local = Desc::parse("%NAME%\nsteam\n\n%INSTALLDATE%\n1788991402\n");
        let short = facts(&desc, Some(&local), false);
        assert_eq!(
            short,
            [
                ("Packager".to_string(), "Levente Polyak".to_string()),
                ("Build date".to_string(), "2026-07-25".to_string()),
                ("Install date".to_string(), "2026-09-09".to_string()),
                ("Depends".to_string(), "2 packages".to_string()),
                ("Groups".to_string(), "games".to_string()),
                ("Architecture".to_string(), "x86_64".to_string()),
            ]
        );
        let full = facts(&desc, None, true);
        assert_eq!(
            full[2],
            ("Depends".to_string(), "bash, coreutils".to_string())
        );
        assert!(!full.iter().any(|(k, _)| k == "Install date"));
        let none = facts(&Desc::parse("%NAME%\nx\n"), None, false);
        assert_eq!(none, [("Depends".to_string(), "None".to_string())]);
    }

    #[test]
    fn a_database_is_told_from_a_captive_portal_page() {
        assert!(looks_like_db(&[0x1f, 0x8b, 0x08, 0x00]));
        assert!(looks_like_db(&[0x28, 0xb5, 0x2f, 0xfd, 0x00]));
        let mut tar = vec![0u8; 512];
        tar[257..262].copy_from_slice(b"ustar");
        assert!(looks_like_db(&tar));
        assert!(!looks_like_db(
            b"<html><body>Sign in to the network</body></html>"
        ));
        assert!(!looks_like_db(b""));
    }

    #[test]
    fn plan_steps_describe_and_never_run() {
        let pacman = Pacman::with_paths(
            Paths {
                conf: PathBuf::from("/nonexistent/pacman.conf"),
                sync_dir: PathBuf::from("/nonexistent/sync"),
                local_dir: PathBuf::from("/nonexistent/local"),
                cache_dir: PathBuf::from("/nonexistent/cache"),
                pacman: Some(PathBuf::from("/nonexistent/pacman")),
            },
            Arc::new(Catalogue::default()),
        );
        let r = |id: &str| PackageRef {
            source: SourceKind::Pacman,
            id: id.to_string(),
        };
        let install = pacman
            .plan(&Op::Install {
                package: r("steam"),
            })
            .unwrap();
        assert_eq!(install.len(), 1);
        assert_eq!(install[0].title, "Installing steam and updating the system");
        assert_eq!(install[0].command.program, "pacman");
        assert_eq!(
            install[0].command.args,
            ["-Syu", "--noconfirm", "--needed", "steam"]
        );
        assert!(install[0].needs_root);
        assert_eq!(install[0].weight, 3);
        assert_eq!(
            install[0].command.env,
            [("LC_ALL".to_string(), "C.UTF-8".to_string())]
        );
        let remove = pacman
            .plan(&Op::Remove {
                package: r("steam"),
            })
            .unwrap();
        assert_eq!(remove[0].command.args, ["-Rs", "--noconfirm", "steam"]);
        assert_eq!(remove[0].title, "Removing steam");
        let update = pacman
            .plan(&Op::Update {
                package: r("steam"),
            })
            .unwrap();
        assert_eq!(
            update[0].command.args,
            ["-Syu", "--noconfirm", "--needed", "steam"]
        );
        assert_eq!(update[0].title, "Updating steam and the system");
        let all = pacman
            .plan(&Op::UpdateAll {
                source: SourceKind::Pacman,
            })
            .unwrap();
        assert_eq!(all[0].command.args, ["-Syu", "--noconfirm"]);
        assert_eq!(all[0].weight, 10);
        assert_eq!(all[0].title, "Updating the system");
        // A refresh needs no root step: refresh_index does it into the
        // cache, and a bare -Sy is the partial-upgrade setup.
        let refresh = pacman
            .plan(&Op::Refresh {
                source: SourceKind::Pacman,
            })
            .unwrap();
        assert!(refresh.is_empty());
        assert!(
            pacman
                .plan(&Op::Refresh {
                    source: SourceKind::Aur
                })
                .is_err()
        );
        assert!(pacman.plan(&Op::Install { package: r("-Rs") }).is_err());
        assert!(
            pacman
                .plan(&Op::UpdateAll {
                    source: SourceKind::Aur
                })
                .is_err()
        );
        let foreign = PackageRef {
            source: SourceKind::Aur,
            id: "paru".to_string(),
        };
        assert!(pacman.plan(&Op::Install { package: foreign }).is_err());
    }

    #[test]
    fn status_says_why_when_it_cannot_work() {
        let paths = Paths {
            conf: PathBuf::from("/nonexistent/pacman.conf"),
            sync_dir: PathBuf::from("/nonexistent/sync"),
            local_dir: PathBuf::from("/nonexistent/local"),
            cache_dir: PathBuf::from("/nonexistent/cache"),
            pacman: Some(PathBuf::from("/nonexistent/pacman")),
        };
        let s = Pacman::with_paths(paths.clone(), Arc::new(Catalogue::default())).status();
        assert!(!s.available);
        assert_eq!(
            s.reason.as_deref(),
            Some("pacman is not installed on this machine.")
        );
        let with_pacman = Paths {
            pacman: Some(std::env::current_exe().unwrap()),
            ..paths
        };
        let s = Pacman::with_paths(with_pacman, Arc::new(Catalogue::default())).status();
        assert!(!s.available);
        assert!(s.reason.unwrap().contains("pacman.conf is missing"));
    }
}
