//! Brokey's library: every source, the metadata, the grouping, the plans
//! and the rules. No window, no root, no global state. `docs/architecture.md`
//! is the design; `CLAUDE.md` lists the invariants.
//!
//! The shape in one paragraph: a [`Source`] answers questions (search,
//! installed, updates, details) and describes how an operation would be
//! carried out as [`Step`]s, but never runs anything. A [`Store`] holds every
//! source this machine can use, fans a query out across them, and groups the
//! results into [`App`]s. A [`Plan`] is what the transaction runner executes,
//! sending the root steps to the helper and running the rest in the session.

pub mod appstream;
pub mod drivers;
pub mod group;
pub mod http;
#[cfg(unix)]
pub mod launch;
pub mod model;
pub mod selfupdate;
pub mod sources;
pub mod system;
pub mod transaction;
pub mod updates;
pub mod vercmp;

pub use model::*;

use std::fmt;

/// What went wrong, in a sentence a user can act on. Sources wrap their
/// underlying errors with `context` so the page never shows a bare I/O
/// error.
#[derive(Debug)]
pub struct Error {
    pub message: String,
    pub source_kind: Option<SourceKind>,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Error {
        Error {
            message: message.into(),
            source_kind: None,
        }
    }
    pub fn from_source(kind: SourceKind, message: impl Into<String>) -> Error {
        Error {
            message: message.into(),
            source_kind: Some(kind),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Error {
        Error::new(format!("{e:#}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::new(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A search, as the page sends it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Query {
    pub text: String,
    /// `None` means every available source.
    pub sources: Option<Vec<SourceKind>>,
    /// Per source. Sources return their best matches first.
    pub limit: usize,
    /// Editions the user has split out of their group, as `source:id`
    /// (the `split` setting). Each becomes a row of its own.
    #[serde(default)]
    pub split: Vec<String>,
}

impl Query {
    pub fn new(text: impl Into<String>) -> Query {
        Query {
            text: text.into(),
            sources: None,
            limit: 200,
            split: Vec::new(),
        }
    }
}

/// What a search returns: the grouped rows, and the sources that failed with
/// the sentence they failed with. A failed source is never silent.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    pub apps: Vec<App>,
    pub failed: Vec<(SourceKind, String)>,
    /// Which sources were asked, so the page can say "searched pacman, AUR, Flatpak".
    pub searched: Vec<SourceKind>,
}

/// One place software comes from. Every method is synchronous and may block
/// on the network or the disk; the application runs them on worker threads.
/// A source **never runs anything**: `plan` describes, the runner executes.
pub trait Source: Send + Sync {
    fn kind(&self) -> SourceKind;

    /// Whether this machine can use the source, with a reason when it cannot.
    fn status(&self) -> SourceStatus;

    /// The best matches for the query, most relevant first, at most
    /// `query.limit`. Sources fill `installed` and `installed_version`
    /// themselves so a search result is complete without a second call.
    fn search(&self, query: &Query) -> Result<Vec<Package>>;

    /// Everything this source has installed on the machine.
    fn installed(&self) -> Result<Vec<Package>>;

    /// Everything this source could bring up to date.
    fn updates(&self) -> Result<Vec<Update>>;

    /// The full record for one thing, including what search leaves out
    /// (description, screenshots, facts).
    fn details(&self, id: &str) -> Result<Package>;

    /// How the operation would be carried out. An empty list means the
    /// source has nothing to do for it (already installed, nothing to update).
    fn plan(&self, op: &Op) -> Result<Vec<Step>>;

    /// Bring the source's own index up to date without root, where it can
    /// (pacman downloads fresh sync databases into the cache the way
    /// `checkupdates` does). The default does nothing; a failure is logged
    /// by the caller and the on-disk index answers as before.
    fn refresh_index(&self) -> Result<()> {
        Ok(())
    }

    /// Told after a plan carrying `op` has finished, so a source that keeps
    /// its own record of what it installed (GitHub) can update it. Never
    /// runs anything.
    fn finished(&self, _op: &Op, _ok: bool) {}

    /// How this source would be set up on this machine when it is not
    /// available: the operations other sources carry out first (installing
    /// the tool's package through the distribution's source), then the
    /// source's own steps (adding Flathub, enabling snapd's socket). `None`
    /// when there is nothing to set up, or no way to do it here. The
    /// planner expands `Op::Setup` through it; a source never runs it.
    fn setup(&self) -> Option<Setup> {
        None
    }

    /// How to open an installed package: its desktop entry, or a command
    /// for a format with its own way to run what it installed. `None` when
    /// the package is not installed through this source or has nothing a
    /// person opens (a library, a font, a command-line tool). Never runs
    /// anything; see [`launch`].
    #[cfg(unix)]
    fn launcher(&self, _id: &str) -> Option<launch::Launch> {
        None
    }

    /// A sentence when this source's applications are installed but the
    /// running desktop session cannot list them (Flatpak or snapd set up
    /// after the session started). `None` when there is nothing to say.
    #[cfg(unix)]
    fn launcher_notice(&self) -> Option<String> {
        None
    }
}

/// What setting a source up takes. See [`Source::setup`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setup {
    /// Operations for other sources, planned first and in this order.
    pub ops: Vec<Op>,
    /// The source's own steps, after those.
    pub steps: Vec<Step>,
    /// The sentence the confirm dialog shows: "Flatpak is not installed.
    /// It is installed and Flathub is added."
    pub notice: String,
}

/// The sources this machine has, and the operations across them.
pub struct Store {
    pub system: SystemInfo,
    pub sources: Vec<Box<dyn Source>>,
}

/// The user's choices that change how a source behaves, as opposed to which
/// sources are searched (the page's business). Read from Settings by the
/// application and passed to [`Store::detect_with`]; the text mode uses the
/// defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preferences {
    /// Flatpak installs, and the Flathub remote a setup adds, go to the
    /// user's own installation rather than the system one.
    pub flatpak_user: bool,
    /// "paru", "yay" or "builtin"; `None` picks whichever is installed.
    pub aur_helper: Option<String>,
}

impl Store {
    /// Build every source the machine could have, in interface order. Each
    /// reports its own availability; unavailable sources stay in the list so
    /// the page can say why.
    pub fn detect() -> Store {
        Store::detect_with(&Preferences::default())
    }

    /// [`Store::detect`], with the user's preferences applied to the sources.
    pub fn detect_with(preferences: &Preferences) -> Store {
        let system = system::detect();
        let client = http::Client::shared();
        let catalogue = appstream::Catalogue::load_system(&system);
        let sources = sources::all(&system, client, catalogue, preferences);
        Store { system, sources }
    }

    /// How to open an installed package, asked of its own source.
    #[cfg(unix)]
    pub fn launcher(&self, package: &PackageRef) -> Option<launch::Launch> {
        self.source(package.source)?.launcher(&package.id)
    }

    /// Every source's launcher notice, in interface order.
    #[cfg(unix)]
    pub fn launcher_notices(&self) -> Vec<(SourceKind, String)> {
        self.sources
            .iter()
            .filter_map(|s| s.launcher_notice().map(|n| (s.kind(), n)))
            .collect()
    }

    pub fn statuses(&self) -> Vec<SourceStatus> {
        self.sources.iter().map(|s| s.status()).collect()
    }

    pub fn source(&self, kind: SourceKind) -> Option<&dyn Source> {
        self.sources
            .iter()
            .find(|s| s.kind() == kind)
            .map(|s| s.as_ref())
    }

    /// The sources a command may ask: the available ones, and for a search
    /// also those that can answer through their public store while their
    /// tool is missing (`SourceStatus::searchable`). Installed and updates
    /// need the tool, so they never take the second kind.
    fn selected(&self, wanted: &Option<Vec<SourceKind>>, searching: bool) -> Vec<&dyn Source> {
        self.sources
            .iter()
            .map(|s| s.as_ref())
            .filter(|s| wanted.as_ref().is_none_or(|w| w.contains(&s.kind())))
            .filter(|s| {
                let status = s.status();
                status.available || (searching && status.searchable)
            })
            .collect()
    }

    /// Search every selected source at once and group the results: the
    /// available ones, and the searchable ones whose tool is not installed.
    pub fn search(&self, query: &Query) -> SearchResult {
        let sources = self.selected(&query.sources, true);
        let searched: Vec<SourceKind> = sources.iter().map(|s| s.kind()).collect();
        let results: Vec<(SourceKind, Result<Vec<Package>>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = sources
                .iter()
                .map(|s| {
                    let kind = s.kind();
                    scope.spawn(move || (kind, s.search(query)))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("a source panicked"))
                .collect()
        });
        let mut packages = Vec::new();
        let mut failed = Vec::new();
        for (kind, result) in results {
            match result {
                Ok(mut found) => packages.append(&mut found),
                Err(e) => failed.push((kind, e.message)),
            }
        }
        let apps = group::group_with(packages, &query.text, &query.split);
        SearchResult {
            apps,
            failed,
            searched,
        }
    }

    /// Everything installed, across every available source, grouped.
    pub fn installed(&self) -> SearchResult {
        let sources = self.selected(&None, false);
        let searched: Vec<SourceKind> = sources.iter().map(|s| s.kind()).collect();
        let results: Vec<(SourceKind, Result<Vec<Package>>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = sources
                .iter()
                .map(|s| {
                    let kind = s.kind();
                    scope.spawn(move || (kind, s.installed()))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("a source panicked"))
                .collect()
        });
        let mut packages = Vec::new();
        let mut failed = Vec::new();
        for (kind, result) in results {
            match result {
                Ok(mut found) => packages.append(&mut found),
                Err(e) => failed.push((kind, e.message)),
            }
        }
        let apps = group::group(packages, "");
        SearchResult {
            apps,
            failed,
            searched,
        }
    }

    /// Every update, across every available source.
    pub fn updates(&self) -> updates::UpdateList {
        updates::collect(self)
    }

    /// The same, after every source has been asked to refresh its index
    /// without root first.
    pub fn updates_refreshed(&self) -> updates::UpdateList {
        updates::collect_with(self, true)
    }

    /// Tell each operation's source how its plan ended.
    pub fn finished(&self, ops: &[Op], ok: bool) {
        for op in ops {
            if let Some(source) = self.source(op.source()) {
                source.finished(op, ok);
            }
        }
    }

    /// Build a plan for a list of operations: refresh steps first, then
    /// the steps in the order of the operations, with adjacent package
    /// manager calls joined. A `Setup` operation is expanded through the
    /// source's [`Source::setup`].
    pub fn plan(&self, ops: &[Op]) -> Result<Plan> {
        transaction::plan::build(self, ops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A source whose availability is set by the test and which records
    /// every question it is asked.
    struct Fake {
        kind: SourceKind,
        available: bool,
        searchable: bool,
        asked: std::sync::Arc<Mutex<Vec<(SourceKind, &'static str)>>>,
    }

    impl Fake {
        fn ask(&self, what: &'static str) {
            self.asked.lock().unwrap().push((self.kind, what));
        }
    }

    impl Source for Fake {
        fn kind(&self) -> SourceKind {
            self.kind
        }
        fn status(&self) -> SourceStatus {
            SourceStatus {
                kind: self.kind,
                available: self.available,
                reason: (!self.available).then(|| "Not installed.".to_string()),
                detail: None,
                searchable: self.searchable,
                setup: None,
            }
        }
        fn search(&self, query: &Query) -> Result<Vec<Package>> {
            self.ask("search");
            Ok(vec![Package::new(
                self.kind,
                format!("{}-{}", self.kind.id(), query.text),
                query.text.clone(),
            )])
        }
        fn installed(&self) -> Result<Vec<Package>> {
            self.ask("installed");
            Ok(Vec::new())
        }
        fn updates(&self) -> Result<Vec<Update>> {
            self.ask("updates");
            Ok(Vec::new())
        }
        fn details(&self, id: &str) -> Result<Package> {
            Err(Error::new(format!("{id} is not known.")))
        }
        fn plan(&self, _op: &Op) -> Result<Vec<Step>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn a_searchable_source_is_searched_without_its_tool_but_never_asked_what_it_installed() {
        let asked = std::sync::Arc::new(Mutex::new(Vec::new()));
        let fake = |kind, available, searchable| -> Box<dyn Source> {
            Box::new(Fake {
                kind,
                available,
                searchable,
                asked: asked.clone(),
            })
        };
        let store = Store {
            system: system::from_os_release("ID=arch\n"),
            sources: vec![
                fake(SourceKind::Pacman, true, false),
                fake(SourceKind::Flatpak, false, true),
                fake(SourceKind::Fwupd, false, false),
            ],
        };
        let result = store.search(&Query::new("gimp"));
        assert_eq!(result.searched, [SourceKind::Pacman, SourceKind::Flatpak]);
        assert!(result.failed.is_empty());
        let mut sources: Vec<SourceKind> = result
            .apps
            .iter()
            .flat_map(|a| a.editions.iter().map(|e| e.package.source))
            .collect();
        sources.sort_by_key(|k| k.id());
        sources.dedup();
        assert_eq!(sources.len(), 2, "{:?}", result.apps);

        let only_flatpak = Query {
            sources: Some(vec![SourceKind::Flatpak]),
            ..Query::new("gimp")
        };
        assert_eq!(store.search(&only_flatpak).searched, [SourceKind::Flatpak]);

        asked.lock().unwrap().clear();
        assert_eq!(store.installed().searched, [SourceKind::Pacman]);
        store.updates();
        assert!(
            asked
                .lock()
                .unwrap()
                .iter()
                .all(|(kind, _)| *kind == SourceKind::Pacman),
            "installed and updates need the tool: {:?}",
            asked.lock().unwrap()
        );
    }
}
