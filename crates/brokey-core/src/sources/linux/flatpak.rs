//! The Flatpak source: every remote in the system and user installations,
//! searched through their AppStream catalogues, enriched from Flathub's web
//! API when the remote is Flathub, and driven through the `flatpak` command
//! for the questions only it can answer (which remotes exist, what is
//! installed, what has an update).
//!
//! Flatpak is asked through its command line rather than libflatpak because
//! the library would tie one binary to one distribution's build of it, and
//! the plain output is stable enough to parse: when stdout is not a terminal
//! `app/flatpak-table-printer.c` (flatpak 1.18) joins cells with tabs and
//! prints no title row. Every invocation goes through a [`Runner`], so the
//! parsers are tested against recorded output on a machine with no flatpak
//! at all, which is the machine this was written on.
//!
//! When flatpak is not installed the source is still searchable: Flathub's
//! own search API answers, so a Flathub edition sits beside the
//! distribution's, and [`Source::setup`] says how Flatpak is installed
//! (through the distribution's package manager) and Flathub added before
//! the first install from it, all in one plan.
//!
//! Installs, removals and updates are described as [`Step`]s and never run
//! here. They do not need root: a `--system` operation asks polkit through
//! flatpak's own system helper, so the step runs in the user session and the
//! desktop's agent shows the password dialogue. Sending it through
//! `brokey-helper` instead would make flatpak run as root and put the user's
//! `--user` installation out of reach.

use crate::appstream::{Catalogue, Component};
use crate::http::Client;
use crate::model::*;
use crate::{Error, Op, Query, Result, Setup, Source};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// What a question that needs the `flatpak` command says when there is none.
pub const NOT_INSTALLED: &str = "Flatpak is not installed.";

/// The status reason when flatpak is missing and the store can set it up:
/// Flathub is still searched, through its web API.
pub const NOT_INSTALLED_SEARCHABLE: &str = "Flatpak is not installed. Flathub is searched through its website, and installing from it sets Flatpak up first.";

/// The status reason when flatpak is missing and this system has no
/// package the store knows to install it from.
pub const NOT_INSTALLED_NO_SETUP: &str = "Flatpak is not installed. Flathub is searched through its website; install the flatpak package to install from it.";

/// What the page says when flatpak exists but has nowhere to look.
pub const NO_REMOTES: &str = "No Flatpak remotes are configured. Add Flathub to search it.";

/// The button and the sentence for setting Flatpak up.
pub const SETUP_LABEL: &str = "Install Flatpak";
pub const SETUP_SENTENCE: &str = "Installs Flatpak and adds Flathub, then Flatpak applications can be installed and updated here.";
/// What the confirm dialog says for that setup.
pub const SETUP_NOTICE: &str = "Flatpak is not installed. It is installed and Flathub is added.";

/// The button, sentence and notice for a flatpak with no remotes.
pub const ADD_FLATHUB_LABEL: &str = "Add Flathub";
pub const ADD_FLATHUB_SENTENCE: &str =
    "Adds Flathub, then Flatpak applications can be installed and updated here.";
pub const ADD_FLATHUB_NOTICE: &str = "No Flatpak remotes are configured. Flathub is added.";

/// Flathub's `.flatpakrepo` file, the address the helper's closed list
/// allows `flatpak remote-add` to take.
pub const FLATHUB_REPO_FILE: &str = "https://dl.flathub.org/repo/flathub.flatpakrepo";

/// Brokey's own Flatpak id, so an update to it is marked as the store's
/// own. Kept here until the self-updater exports one name for every source.
const SELF_APP_ID: &str = "io.github.spillebulle.brokey";

const SEARCH_URL: &str = "https://flathub.org/api/v2/search";
const APPSTREAM_URL: &str = "https://flathub.org/api/v2/appstream/";
const SUMMARY_URL: &str = "https://flathub.org/api/v2/summary/";

/// The installs-per-month figure Flathub's most popular applications reach;
/// popularity is that fraction of it, capped at one.
const POPULARITY_CEILING: f64 = 100_000.0;

const LIST_ARGS: [&str; 3] = [
    "list",
    "--app",
    "--columns=application,name,version,branch,origin,installation,size",
];

/// `remote-ls` has no `installation` column (asking for one fails the whole
/// call with "Unknown column"), and without an installation flag it lists
/// both installations at once, which is what an update list wants.
const UPDATES_ARGS: [&str; 4] = [
    "remote-ls",
    "--updates",
    "--app",
    "--columns=application,name,version,branch,origin,download-size",
];

/// Which installation an operation targets. Flatpak keeps two: the system
/// one under `/var/lib/flatpak`, shared by every user, and a per-user one
/// under `~/.local/share/flatpak`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Installation {
    System,
    User,
}

impl Installation {
    /// The command-line flag that selects it.
    pub fn flag(self) -> &'static str {
        match self {
            Installation::System => "--system",
            Installation::User => "--user",
        }
    }

    /// The word flatpak prints in an options cell, and the value of the
    /// "Installation" fact.
    pub fn name(self) -> &'static str {
        match self {
            Installation::System => "system",
            Installation::User => "user",
        }
    }

    pub fn parse(word: &str) -> Option<Installation> {
        match word {
            "system" => Some(Installation::System),
            "user" => Some(Installation::User),
            _ => None,
        }
    }

    fn other(self) -> Installation {
        match self {
            Installation::System => Installation::User,
            Installation::User => Installation::System,
        }
    }
}

/// How the source reaches the `flatpak` command. The real one runs it; a
/// scripted one answers from recorded output. Only read-only queries come
/// through here; anything that changes the machine is a [`Step`].
pub trait Runner: Send + Sync {
    /// Where `flatpak` is, or `None` when it is not installed.
    fn locate(&self) -> Option<PathBuf>;

    /// Run `flatpak` with these arguments and return what it printed.
    fn run(&self, args: &[&str]) -> Result<String>;
}

/// The runner for a real machine: `flatpak` from `PATH`, `LC_ALL=C.UTF-8`
/// so the output is the untranslated one the parsers know.
#[derive(Debug, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn locate(&self) -> Option<PathBuf> {
        crate::system::which("flatpak")
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        crate::system::run("flatpak", args)
    }
}

/// A runner that answers from a script: each entry is an argument list and
/// what `flatpak` printed for it. For tests and the mock backend. An
/// argument list that is not in the script fails the way a broken flatpak
/// would, so a test sees exactly which command the source ran.
#[derive(Debug, Default)]
pub struct ScriptedRunner {
    /// Whether `flatpak` is "installed" at all.
    pub present: bool,
    pub outputs: Vec<(Vec<String>, std::result::Result<String, String>)>,
}

impl ScriptedRunner {
    /// A machine without flatpak.
    pub fn absent() -> ScriptedRunner {
        ScriptedRunner::default()
    }

    /// A machine with flatpak and, so far, no recorded answers.
    pub fn present() -> ScriptedRunner {
        ScriptedRunner {
            present: true,
            outputs: Vec::new(),
        }
    }

    pub fn answers(mut self, args: &[&str], output: &str) -> ScriptedRunner {
        self.outputs.push((
            args.iter().map(|a| a.to_string()).collect(),
            Ok(output.to_string()),
        ));
        self
    }

    pub fn fails(mut self, args: &[&str], message: &str) -> ScriptedRunner {
        self.outputs.push((
            args.iter().map(|a| a.to_string()).collect(),
            Err(message.to_string()),
        ));
        self
    }
}

impl Runner for ScriptedRunner {
    fn locate(&self) -> Option<PathBuf> {
        self.present.then(|| PathBuf::from("/usr/bin/flatpak"))
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        if !self.present {
            return Err(Error::from_source(SourceKind::Flatpak, NOT_INSTALLED));
        }
        let wanted: Vec<&str> = args.to_vec();
        match self
            .outputs
            .iter()
            .find(|(a, _)| a.iter().map(String::as_str).eq(wanted.iter().copied()))
        {
            Some((_, Ok(text))) => Ok(text.clone()),
            Some((_, Err(message))) => {
                Err(Error::from_source(SourceKind::Flatpak, message.clone()))
            }
            None => Err(Error::from_source(
                SourceKind::Flatpak,
                format!("flatpak {} is not in the script.", args.join(" ")),
            )),
        }
    }
}

/// Flathub's web API, behind a trait so no search or detail needs the
/// network to be tested. Answers are raw JSON: the source reads the fields
/// it needs and ignores the rest, because the API adds fields between
/// releases and a typed struct would break on each one.
pub trait FlathubApi: Send + Sync {
    /// `POST /api/v2/search` with `{"query": term, "filters": []}`.
    fn search(&self, term: &str) -> Result<Value>;

    /// `GET /api/v2/appstream/<app id>`.
    fn appstream(&self, app_id: &str) -> Result<Value>;

    /// `GET /api/v2/summary/<app id>`.
    fn summary(&self, app_id: &str) -> Result<Value>;
}

/// The real Flathub, through the shared HTTP client.
pub struct LiveFlathub {
    client: Arc<Client>,
}

impl LiveFlathub {
    pub fn new(client: Arc<Client>) -> LiveFlathub {
        LiveFlathub { client }
    }
}

impl FlathubApi for LiveFlathub {
    fn search(&self, term: &str) -> Result<Value> {
        let body = serde_json::json!({ "query": term, "filters": [] });
        self.client.post_json(SEARCH_URL, &body)
    }

    fn appstream(&self, app_id: &str) -> Result<Value> {
        self.client
            .get_json(&format!("{APPSTREAM_URL}{app_id}"), &[])
    }

    fn summary(&self, app_id: &str) -> Result<Value> {
        self.client.get_json(&format!("{SUMMARY_URL}{app_id}"), &[])
    }
}

/// Flathub from a script: one search answer and per-application answers.
/// Anything not scripted fails the way an offline machine would, which is
/// exactly the failure every caller must tolerate.
#[derive(Debug, Default)]
pub struct ScriptedFlathub {
    pub search: Option<Value>,
    pub appstream: HashMap<String, Value>,
    pub summary: HashMap<String, Value>,
}

impl ScriptedFlathub {
    /// A machine that cannot reach Flathub.
    pub fn offline() -> ScriptedFlathub {
        ScriptedFlathub::default()
    }
}

impl FlathubApi for ScriptedFlathub {
    fn search(&self, _term: &str) -> Result<Value> {
        self.search.clone().ok_or_else(|| offline("flathub.org"))
    }

    fn appstream(&self, app_id: &str) -> Result<Value> {
        self.appstream
            .get(app_id)
            .cloned()
            .ok_or_else(|| offline("flathub.org"))
    }

    fn summary(&self, app_id: &str) -> Result<Value> {
        self.summary
            .get(app_id)
            .cloned()
            .ok_or_else(|| offline("flathub.org"))
    }
}

fn offline(host: &str) -> Error {
    Error::from_source(SourceKind::Flatpak, format!("could not reach {host}"))
}

/// One configured remote in one installation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub url: String,
    pub installation: Installation,
}

impl Remote {
    /// Whether Flathub's web API describes this remote. The beta repository
    /// shares the host but not the API, so the URL has to be the stable one.
    pub fn is_flathub(&self) -> bool {
        self.name == "flathub" || self.url.trim_end_matches('/') == "https://dl.flathub.org/repo"
    }
}

/// One row of `flatpak list --app`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledRow {
    pub application: String,
    pub name: String,
    pub version: Option<String>,
    pub branch: String,
    pub origin: String,
    pub installation: Option<Installation>,
    pub size: Option<u64>,
    /// The size as flatpak printed it, with a plain space before the unit.
    pub size_text: Option<String>,
}

/// One row of `flatpak remote-ls --updates --app`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateRow {
    pub application: String,
    pub name: String,
    pub version: Option<String>,
    pub branch: String,
    pub origin: String,
    pub download_size: Option<u64>,
}

/// A package id taken apart: `flathub/app/org.gimp.GIMP/x86_64/stable` is
/// the remote, then the ref flatpak's own commands take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlatpakRef {
    pub remote: String,
    /// `app` or `runtime`.
    pub kind: String,
    pub app_id: String,
    pub arch: String,
    pub branch: String,
}

impl FlatpakRef {
    pub fn parse(id: &str) -> Result<FlatpakRef> {
        let parts: Vec<&str> = id.split('/').collect();
        let well_formed = parts.len() == 5
            && parts.iter().all(|p| !p.is_empty())
            && (parts[1] == "app" || parts[1] == "runtime");
        if !well_formed {
            return Err(Error::from_source(
                SourceKind::Flatpak,
                format!(
                    "{id} is not a Flatpak ref. A ref looks like flathub/app/org.gimp.GIMP/x86_64/stable."
                ),
            ));
        }
        Ok(FlatpakRef {
            remote: parts[0].to_string(),
            kind: parts[1].to_string(),
            app_id: parts[2].to_string(),
            arch: parts[3].to_string(),
            branch: parts[4].to_string(),
        })
    }

    /// The ref flatpak's commands take: `app/org.gimp.GIMP/x86_64/stable`.
    pub fn bundle(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.kind, self.app_id, self.arch, self.branch
        )
    }

    /// The package id: the remote and the bundle.
    pub fn id(&self) -> String {
        format!("{}/{}", self.remote, self.bundle())
    }
}

/// What Flathub's search knows about one application, reduced to what the
/// row draws from it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FlathubHit {
    pub app_id: String,
    pub name: Option<String>,
    pub summary: Option<String>,
    pub icon: Option<String>,
    pub installs_last_month: Option<u64>,
    pub updated_at: Option<i64>,
    pub verified: bool,
    pub developer: Option<String>,
    pub categories: Vec<String>,
    pub keywords: Vec<String>,
    pub is_app: bool,
}

pub struct Flatpak {
    /// Flatpak's spelling of the machine's architecture: "x86_64", "aarch64".
    arch: String,
    /// Which distribution this is, so a setup knows which package manager
    /// installs flatpak.
    system: SystemInfo,
    runner: Box<dyn Runner>,
    flathub: Box<dyn FlathubApi>,
    /// The remotes' catalogues, read on first use because parsing them is
    /// the slow part and `Store::detect` builds every source up front.
    catalogue: OnceLock<Catalogue>,
    /// Where installs go when the remote exists in both installations.
    /// System is the default because a store on a shared machine is
    /// expected to install for everyone; the Settings page can flip it.
    pub installation: Installation,
}

impl Flatpak {
    pub fn new(system: &SystemInfo, client: Arc<Client>) -> Flatpak {
        Flatpak {
            arch: flatpak_arch(&system.arch),
            system: system.clone(),
            runner: Box::new(SystemRunner),
            flathub: Box::new(LiveFlathub::new(client)),
            catalogue: OnceLock::new(),
            installation: Installation::System,
        }
    }

    /// A source built from its parts, for tests and the mock backend: the
    /// architecture as flatpak spells it, a runner, a Flathub client and a
    /// catalogue already loaded.
    pub fn with_parts(
        arch: &str,
        runner: impl Runner + 'static,
        flathub: impl FlathubApi + 'static,
        catalogue: Catalogue,
    ) -> Flatpak {
        Flatpak {
            arch: arch.to_string(),
            system: crate::system::from_os_release(""),
            runner: Box::new(runner),
            flathub: Box::new(flathub),
            catalogue: OnceLock::from(catalogue),
            installation: Installation::System,
        }
    }

    /// The distribution this source sets Flatpak up on. [`Flatpak::with_parts`]
    /// starts from an unknown one, which has no setup.
    pub fn with_system(mut self, system: &SystemInfo) -> Flatpak {
        self.system = system.clone();
        self
    }

    fn catalogue(&self) -> &Catalogue {
        self.catalogue.get_or_init(Catalogue::load_flatpak)
    }

    fn is_installed(&self) -> bool {
        self.runner.locate().is_some()
    }

    /// The distribution's flatpak package, as an install through its own
    /// source, or `None` on a system the store has no package manager for.
    pub fn flatpak_package(&self) -> Option<PackageRef> {
        let source = if self.system.is_arch_like() {
            SourceKind::Pacman
        } else if self.system.is_debian_like() {
            SourceKind::Apt
        } else if self.system.is_fedora_like() {
            SourceKind::Dnf
        } else {
            return None;
        };
        Some(PackageRef {
            source,
            id: "flatpak".to_string(),
        })
    }

    /// Flathub as it will be once added, in the installation installs go to.
    fn flathub_to_be(&self) -> Remote {
        Remote {
            name: "flathub".to_string(),
            url: "https://dl.flathub.org/repo/".to_string(),
            installation: self.installation,
        }
    }

    /// The step that adds Flathub. The system installation is changed
    /// through the helper, whose closed list allows exactly this command;
    /// the user's is the user's own and needs no password.
    pub fn add_flathub_step(&self) -> Step {
        let args = [
            "remote-add",
            "--if-not-exists",
            self.installation.flag(),
            "flathub",
            FLATHUB_REPO_FILE,
        ];
        match self.installation {
            Installation::System => Step {
                source: SourceKind::Flatpak,
                title: "Adding Flathub".to_string(),
                command: Command {
                    program: "flatpak".to_string(),
                    args: args.iter().map(|a| a.to_string()).collect(),
                    env: Vec::new(),
                    cwd: None,
                },
                needs_root: true,
                weight: 1,
            },
            Installation::User => self.step("Adding Flathub".to_string(), &args, 1),
        }
    }

    /// A search answered by Flathub's API alone, for a machine where
    /// flatpak cannot answer (not installed, or no remote yet). Every hit
    /// is an application from Flathub in the installation installs go to.
    fn search_flathub(&self, query: &Query) -> Result<Vec<Package>> {
        let term = query.text.trim();
        if term.is_empty() {
            return Ok(Vec::new());
        }
        let json = self.flathub.search(term).map_err(|e| {
            Error::from_source(
                SourceKind::Flatpak,
                format!(
                    "Flathub's search did not answer ({}). Check the connection and try again.",
                    e.message
                ),
            )
        })?;
        let remote = self.flathub_to_be();
        let mut scored: Vec<(f32, Package)> = parse_hits(&json)
            .values()
            .map(|hit| {
                let p = self.package_from_hit(hit, &remote);
                let component = Component {
                    name: p.name.clone(),
                    id: hit.app_id.clone(),
                    summary: p.summary.clone(),
                    keywords: hit.keywords.clone(),
                    ..Component::default()
                };
                (score(&component, term).max(0.2), p)
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()))
        });
        scored.truncate(query.limit);
        Ok(scored.into_iter().map(|(_, p)| p).collect())
    }

    fn require_installed(&self) -> Result<()> {
        match self.runner.locate() {
            Some(_) => Ok(()),
            None => Err(Error::from_source(SourceKind::Flatpak, NOT_INSTALLED)),
        }
    }

    /// Every remote in both installations. One installation failing to
    /// list is logged and skipped; both failing, or one failing with the
    /// other empty, is an error, because "no remotes" would be a lie.
    pub fn remotes(&self) -> Result<Vec<Remote>> {
        let mut remotes = Vec::new();
        let mut failures = Vec::new();
        for installation in [Installation::System, Installation::User] {
            match self
                .runner
                .run(&["remotes", installation.flag(), "--columns=name,url,options"])
            {
                Ok(text) => remotes.extend(parse_remotes(&text, installation)),
                Err(e) => failures.push(e.message),
            }
        }
        if let Some(first) = failures.first()
            && remotes.is_empty()
        {
            return Err(Error::from_source(
                SourceKind::Flatpak,
                format!(
                    "Flatpak could not list its remotes ({first}). Check that flatpak works from a terminal."
                ),
            ));
        }
        for failure in failures {
            log::warn!("one Flatpak installation did not list its remotes: {failure}");
        }
        Ok(remotes)
    }

    /// What `flatpak list` says, or nothing when it cannot say: a search
    /// still succeeds without the installed marks.
    fn installed_rows(&self) -> Vec<InstalledRow> {
        match self.runner.run(&LIST_ARGS) {
            Ok(text) => parse_list(&text),
            Err(e) => {
                log::warn!(
                    "flatpak list failed, installed marks are missing: {}",
                    e.message
                );
                Vec::new()
            }
        }
    }

    /// The catalogue's component for a ref, trying the index first and then
    /// the plain list, because the index is keyed on the bundle and an
    /// installed application's bundle may name a branch the catalogue lacks.
    fn component_for(&self, remote: &str, app_id: &str, bundle: &str) -> Option<&Component> {
        let catalogue = self.catalogue();
        catalogue
            .by_bundle(bundle)
            .filter(|c| c.origin == remote)
            .or_else(|| {
                catalogue
                    .components()
                    .iter()
                    .find(|c| c.origin == remote && c.bundle.as_deref() == Some(bundle))
            })
            .or_else(|| {
                catalogue
                    .components()
                    .iter()
                    .find(|c| c.origin == remote && c.id == app_id)
            })
            .or_else(|| {
                catalogue
                    .components()
                    .iter()
                    .find(|c| c.id == app_id && c.bundle.is_some())
            })
    }

    fn package_from_component(&self, component: &Component, remote: &str, bundle: &str) -> Package {
        let app_id = bundle_app_id(bundle).unwrap_or(&component.id).to_string();
        let mut p = Package::new(
            SourceKind::Flatpak,
            format!("{remote}/{bundle}"),
            component.name.clone(),
        );
        if p.name.is_empty() {
            p.name = app_id.clone();
        }
        p.kind = kind_of(component.is_app, bundle);
        p.summary = component.summary.clone();
        p.description = component.description.clone();
        p.version = component.latest_release.as_ref().map(|(v, _)| v.clone());
        p.updated = component.latest_release.as_ref().and_then(|(_, t)| *t);
        p.repo = Some(remote.to_string());
        p.licence = component.licence.clone();
        p.homepage = component.homepage.clone();
        p.developer = component.developer.clone();
        p.icon = component.icon.clone();
        p.screenshots = component.screenshots.clone();
        p.categories = component.categories.clone();
        p.appstream_id = Some(app_id);
        p.sandboxed = true;
        p
    }

    /// A package for something installed that no catalogue describes: an
    /// application from a remote since removed, or one installed from a
    /// bundle file.
    fn package_from_row(&self, row: &InstalledRow) -> Package {
        let bundle = format!("app/{}/{}/{}", row.application, self.arch, row.branch);
        let name = if row.name.is_empty() {
            row.application.clone()
        } else {
            row.name.clone()
        };
        let mut p = Package::new(
            SourceKind::Flatpak,
            format!("{}/{bundle}", row.origin),
            name,
        );
        p.kind = PackageKind::App;
        p.repo = Some(row.origin.clone());
        p.appstream_id = Some(row.application.clone());
        p.sandboxed = true;
        p
    }

    fn package_from_installed(&self, row: &InstalledRow) -> Package {
        let bundle = format!("app/{}/{}/{}", row.application, self.arch, row.branch);
        let mut p = match self.component_for(&row.origin, &row.application, &bundle) {
            Some(component) => self.package_from_component(component, &row.origin, &bundle),
            None => self.package_from_row(row),
        };
        apply_installed(&mut p, row);
        p
    }

    fn package_from_hit(&self, hit: &FlathubHit, remote: &Remote) -> Package {
        let bundle = format!("app/{}/{}/stable", hit.app_id, self.arch);
        let component = Component {
            id: hit.app_id.clone(),
            origin: remote.name.clone(),
            bundle: Some(bundle.clone()),
            name: hit.name.clone().unwrap_or_else(|| hit.app_id.clone()),
            summary: hit.summary.clone(),
            developer: hit.developer.clone(),
            categories: hit.categories.clone(),
            keywords: hit.keywords.clone(),
            icon: hit.icon.clone().map(Picture::Url),
            is_app: hit.is_app,
            ..Component::default()
        };
        let mut p = self.package_from_component(&component, &remote.name, &bundle);
        apply_hit(&mut p, hit);
        p
    }

    /// The name the activity panel says: the catalogue's, else the installed
    /// list's, else the last segment of the id ("GIMP" for org.gimp.GIMP).
    fn display_name(&self, r: &FlatpakRef, installed: &[InstalledRow]) -> String {
        if let Some(c) = self.component_for(&r.remote, &r.app_id, &r.bundle())
            && !c.name.is_empty()
        {
            return c.name.clone();
        }
        if let Some(row) = find_installed(installed, &r.app_id, &r.branch)
            && !row.name.is_empty()
        {
            return row.name.clone();
        }
        r.app_id.rsplit('.').next().unwrap_or(&r.app_id).to_string()
    }

    /// Where an install of something from this remote goes: the preferred
    /// installation when the remote exists there, else the one it does exist
    /// in, else the preference (and flatpak will say the remote is missing).
    fn installation_for_remote(&self, remote: &str) -> Installation {
        let remotes = self.remotes().unwrap_or_default();
        let exists_in = |i: Installation| {
            remotes
                .iter()
                .any(|r| r.name == remote && r.installation == i)
        };
        if exists_in(self.installation) || !exists_in(self.installation.other()) {
            self.installation
        } else {
            self.installation.other()
        }
    }

    fn step(&self, title: String, args: &[&str], weight: u32) -> Step {
        Step {
            source: SourceKind::Flatpak,
            title,
            command: Command {
                program: "flatpak".to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                // LC_ALL so the verbs the output readers know are the
                // untranslated ones: flatpak's operation lines go through
                // gettext. FLATPAK_FANCY_OUTPUT=0 forces the plain printer;
                // app/flatpak-tty-utils.c (1.18) is plain when that variable
                // is "0", when G_MESSAGES_DEBUG is set, or when stdout is not
                // a terminal. The runner pipes stdout, so it is plain anyway;
                // the variable covers a runner that gives flatpak a pty for
                // polkit's text agent.
                env: vec![
                    ("LC_ALL".to_string(), "C.UTF-8".to_string()),
                    ("FLATPAK_FANCY_OUTPUT".to_string(), "0".to_string()),
                ],
                cwd: None,
            },
            needs_root: false,
            weight,
        }
    }
}

impl Source for Flatpak {
    fn kind(&self) -> SourceKind {
        SourceKind::Flatpak
    }

    fn status(&self) -> SourceStatus {
        let kind = SourceKind::Flatpak;
        if !self.is_installed() {
            let setup = self.flatpak_package().map(|_| SourceSetup {
                label: SETUP_LABEL.to_string(),
                sentence: SETUP_SENTENCE.to_string(),
            });
            let reason = if setup.is_some() {
                NOT_INSTALLED_SEARCHABLE
            } else {
                NOT_INSTALLED_NO_SETUP
            };
            return SourceStatus {
                kind,
                available: false,
                reason: Some(reason.to_string()),
                detail: None,
                searchable: true,
                setup,
            };
        }
        match self.remotes() {
            Err(e) => SourceStatus {
                kind,
                available: false,
                reason: Some(e.message),
                detail: None,
                searchable: false,
                setup: None,
            },
            Ok(remotes) if remotes.is_empty() => SourceStatus {
                kind,
                available: false,
                reason: Some(NO_REMOTES.to_string()),
                detail: Some("no remotes".to_string()),
                searchable: true,
                setup: Some(SourceSetup {
                    label: ADD_FLATHUB_LABEL.to_string(),
                    sentence: ADD_FLATHUB_SENTENCE.to_string(),
                }),
            },
            Ok(remotes) => {
                let mut names: Vec<&str> = Vec::new();
                for r in &remotes {
                    if !names.contains(&r.name.as_str()) {
                        names.push(&r.name);
                    }
                }
                SourceStatus {
                    kind,
                    available: true,
                    reason: None,
                    detail: Some(names.join(", ")),
                    searchable: false,
                    setup: None,
                }
            }
        }
    }

    fn search(&self, query: &Query) -> Result<Vec<Package>> {
        if !self.is_installed() {
            return self.search_flathub(query);
        }
        let remotes = self.remotes()?;
        if remotes.is_empty() {
            return self.search_flathub(query);
        }
        let term = query.text.trim();
        if term.is_empty() {
            return Ok(Vec::new());
        }
        let installed = self.installed_rows();
        let catalogue = self.catalogue();
        let mut scored: Vec<(f32, Package)> = Vec::new();
        for component in catalogue.components() {
            let Some(bundle) = component.bundle.as_deref() else {
                continue;
            };
            // A catalogue left behind by a removed remote must not offer
            // what can no longer be installed.
            if !remotes.iter().any(|r| r.name == component.origin) {
                continue;
            }
            let s = score(component, term);
            if s <= 0.0 {
                continue;
            }
            let mut p = self.package_from_component(component, &component.origin, bundle);
            if let Some(r) = FlatpakRef::parse(&p.id).ok()
                && let Some(row) = find_installed(&installed, &r.app_id, &r.branch)
            {
                apply_installed(&mut p, row);
            }
            scored.push((s, p));
        }

        // Flathub knows what the catalogue cannot: how many people install
        // a thing, and whether its developer is verified. Best effort: a
        // search that cannot reach flathub.org is still a search.
        if let Some(flathub) = remotes.iter().find(|r| r.is_flathub()) {
            match self.flathub.search(term) {
                Ok(json) => {
                    let hits = parse_hits(&json);
                    for (_, p) in scored.iter_mut() {
                        if p.repo.as_deref() == Some(flathub.name.as_str())
                            && let Some(hit) = p.appstream_id.as_ref().and_then(|id| hits.get(id))
                        {
                            apply_hit(p, hit);
                        }
                    }
                    // Until the first `flatpak update --appstream` the
                    // catalogue for Flathub is empty; Flathub's own search
                    // stands in so the source is not blind for its first
                    // hours on a machine.
                    let has_catalogue = catalogue
                        .components()
                        .iter()
                        .any(|c| c.origin == flathub.name);
                    if !has_catalogue {
                        for hit in hits.values() {
                            let mut p = self.package_from_hit(hit, flathub);
                            if let Some(row) = find_installed(&installed, &hit.app_id, "stable") {
                                apply_installed(&mut p, row);
                            }
                            let component = Component {
                                name: p.name.clone(),
                                id: hit.app_id.clone(),
                                summary: p.summary.clone(),
                                keywords: hit.keywords.clone(),
                                ..Component::default()
                            };
                            // Flathub matched it on something this scorer
                            // does not see (a description, a translation),
                            // so it is listed, just after the rest.
                            let s = score(&component, term).max(0.2);
                            scored.push((s, p));
                        }
                    }
                }
                Err(e) => log::info!("Flathub search skipped: {}", e.message),
            }
        }

        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()))
        });
        scored.truncate(query.limit);
        Ok(scored.into_iter().map(|(_, p)| p).collect())
    }

    fn installed(&self) -> Result<Vec<Package>> {
        self.require_installed()?;
        let text = self.runner.run(&LIST_ARGS).map_err(|e| {
            Error::from_source(
                SourceKind::Flatpak,
                format!("Flatpak could not list what is installed ({}).", e.message),
            )
        })?;
        Ok(parse_list(&text)
            .iter()
            .map(|row| self.package_from_installed(row))
            .collect())
    }

    fn updates(&self) -> Result<Vec<Update>> {
        self.require_installed()?;
        let text = self.runner.run(&UPDATES_ARGS).map_err(|e| {
            Error::from_source(
                SourceKind::Flatpak,
                format!(
                    "Flatpak could not check for updates ({}). Check the connection and try again.",
                    e.message
                ),
            )
        })?;
        let installed = self.installed_rows();
        let mut updates = Vec::new();
        for row in parse_remote_ls(&text) {
            let bundle = format!("app/{}/{}/{}", row.application, self.arch, row.branch);
            let component = self.component_for(&row.origin, &row.application, &bundle);
            let current = find_installed(&installed, &row.application, &row.branch);
            let name = component
                .map(|c| c.name.clone())
                .filter(|n| !n.is_empty())
                .or_else(|| current.map(|r| r.name.clone()).filter(|n| !n.is_empty()))
                .unwrap_or_else(|| {
                    if row.name.is_empty() {
                        row.application.clone()
                    } else {
                        row.name.clone()
                    }
                });
            updates.push(Update {
                package: PackageRef {
                    source: SourceKind::Flatpak,
                    id: format!("{}/{bundle}", row.origin),
                },
                name,
                kind: PackageKind::App,
                summary: component.and_then(|c| c.summary.clone()),
                icon: component.and_then(|c| c.icon.clone()),
                from: current.and_then(|r| r.version.clone()),
                // A version the remote's catalogue does not state is still an
                // update: a rebuild against a newer runtime carries none.
                to: row
                    .version
                    .clone()
                    .unwrap_or_else(|| "newer build".to_string()),
                download_size: row.download_size,
                published: None,
                is_self: row.application == SELF_APP_ID,
            });
        }
        Ok(updates)
    }

    fn details(&self, id: &str) -> Result<Package> {
        let r = FlatpakRef::parse(id)?;
        let present = self.is_installed();
        let bundle = r.bundle();
        let installed = if present {
            self.installed_rows()
        } else {
            Vec::new()
        };
        let current = find_installed(&installed, &r.app_id, &r.branch);
        let mut package = self
            .component_for(&r.remote, &r.app_id, &bundle)
            .map(|c| self.package_from_component(c, &r.remote, &bundle));

        let remotes = if present {
            self.remotes().unwrap_or_default()
        } else {
            Vec::new()
        };
        let is_flathub = remotes
            .iter()
            .find(|x| x.name == r.remote)
            .map(Remote::is_flathub)
            .unwrap_or(r.remote == "flathub");

        let mut facts: Vec<(String, String)> = vec![
            ("Remote".to_string(), r.remote.clone()),
            ("Branch".to_string(), r.branch.clone()),
        ];
        if let Some(installation) = current.and_then(|row| row.installation) {
            facts.push(("Installation".to_string(), installation.name().to_string()));
        }

        let mut verified: Option<bool> = None;
        if is_flathub {
            match self.flathub.appstream(&r.app_id) {
                Ok(json) => {
                    let base = package.take().unwrap_or_else(|| {
                        let mut p = Package::new(SourceKind::Flatpak, r.id(), r.app_id.clone());
                        p.kind = PackageKind::App;
                        p.repo = Some(r.remote.clone());
                        p.appstream_id = Some(r.app_id.clone());
                        p.sandboxed = true;
                        p
                    });
                    let (p, v) = apply_appstream(base, &json);
                    verified = v;
                    package = Some(p);
                }
                Err(e) => log::info!("Flathub appstream for {} skipped: {}", r.app_id, e.message),
            }
            match self.flathub.summary(&r.app_id) {
                Ok(json) => {
                    if let Some(p) = package.as_mut() {
                        apply_summary(p, &json, &mut facts);
                    }
                }
                Err(e) => log::info!("Flathub summary for {} skipped: {}", r.app_id, e.message),
            }
        }

        let mut p = match (package, current) {
            (Some(p), _) => p,
            (None, Some(row)) => self.package_from_row(row),
            (None, None) if !present => {
                return Err(Error::from_source(
                    SourceKind::Flatpak,
                    format!(
                        "Flathub did not describe {}, and Flatpak is not installed to ask. Check the connection and try again.",
                        r.app_id
                    ),
                ));
            }
            (None, None) => {
                return Err(Error::from_source(
                    SourceKind::Flatpak,
                    format!(
                        "{} is not in any Flatpak remote on this machine. Refresh the Flatpak catalogues and try again.",
                        r.app_id
                    ),
                ));
            }
        };
        if let Some(row) = current {
            apply_installed(&mut p, row);
        }
        if !facts.iter().any(|(k, _)| k == "Download size")
            && let Some(bytes) = p.download_size
        {
            facts.push(("Download size".to_string(), format_size(bytes)));
        }
        if !facts.iter().any(|(k, _)| k == "Installed size")
            && let Some(bytes) = p.installed_size
        {
            facts.push(("Installed size".to_string(), format_size(bytes)));
        }
        if let Some(licence) = &p.licence {
            facts.push(("Licence".to_string(), licence.clone()));
        }
        if let Some(v) = verified {
            facts.push((
                "Verified".to_string(),
                if v { "Yes" } else { "No" }.to_string(),
            ));
        }
        p.facts = facts;
        Ok(p)
    }

    /// The exported desktop entry for an installed application, which opens
    /// whether or not the session lists the export directory yet. An
    /// application installed without one is run with `flatpak run`.
    fn launcher(&self, id: &str) -> Option<crate::launch::Launch> {
        let flatpak_ref = FlatpakRef::parse(id).ok()?;
        if flatpak_ref.kind != "app" {
            return None;
        }
        let entry = format!("{}.desktop", flatpak_ref.app_id);
        if let Some(path) = export_dirs()
            .into_iter()
            .map(|d| d.join(&entry))
            .find(|p| p.is_file())
        {
            return Some(crate::launch::Launch::Entry(path));
        }
        let installed = installation_roots()
            .iter()
            .any(|root| root.join("app").join(&flatpak_ref.app_id).is_dir());
        (installed && self.is_installed()).then(|| {
            crate::launch::Launch::Command(crate::launch::command(
                "flatpak",
                &["run", &flatpak_ref.app_id],
            ))
        })
    }

    fn launcher_notice(&self) -> Option<String> {
        crate::launch::notice_for("Flatpak", &export_dirs(), &crate::launch::this_session())
    }

    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        match op {
            Op::Install { package } if package.source == SourceKind::Flatpak => {
                let r = FlatpakRef::parse(&package.id)?;
                // Without flatpak (a plan that sets it up first) nothing is
                // installed and the remote will be in the preferred
                // installation, where the setup adds it.
                let present = self.is_installed();
                let installed = if present {
                    self.installed_rows()
                } else {
                    Vec::new()
                };
                if find_installed(&installed, &r.app_id, &r.branch).is_some() {
                    return Ok(Vec::new());
                }
                let name = self.display_name(&r, &installed);
                let installation = if present {
                    self.installation_for_remote(&r.remote)
                } else {
                    self.installation
                };
                let bundle = r.bundle();
                Ok(vec![self.step(
                    format!("Installing {name} from {}", r.remote),
                    &[
                        "install",
                        "-y",
                        "--noninteractive",
                        installation.flag(),
                        &r.remote,
                        &bundle,
                    ],
                    6,
                )])
            }
            Op::Remove { package } if package.source == SourceKind::Flatpak => {
                let r = FlatpakRef::parse(&package.id)?;
                let installed = self.installed_rows();
                let row = find_installed(&installed, &r.app_id, &r.branch);
                // The list is the truth about where it lives; the preference
                // only decides when the list could not be read.
                if row.is_none() && !installed.is_empty() {
                    return Ok(Vec::new());
                }
                let installation = row
                    .and_then(|r| r.installation)
                    .unwrap_or(self.installation);
                let name = self.display_name(&r, &installed);
                let bundle = r.bundle();
                Ok(vec![self.step(
                    format!("Removing {name}"),
                    &[
                        "uninstall",
                        "-y",
                        "--noninteractive",
                        installation.flag(),
                        &bundle,
                    ],
                    3,
                )])
            }
            Op::Update { package } if package.source == SourceKind::Flatpak => {
                let r = FlatpakRef::parse(&package.id)?;
                let installed = self.installed_rows();
                let name = self.display_name(&r, &installed);
                let bundle = r.bundle();
                Ok(vec![self.step(
                    format!("Updating {name}"),
                    &["update", "-y", "--noninteractive", &bundle],
                    6,
                )])
            }
            Op::UpdateAll {
                source: SourceKind::Flatpak,
            } => Ok(vec![self.step(
                "Updating every Flatpak application".to_string(),
                &["update", "-y", "--noninteractive"],
                10,
            )]),
            Op::Refresh {
                source: SourceKind::Flatpak,
            } => Ok(vec![self.step(
                "Refreshing the Flatpak catalogues".to_string(),
                &["update", "--appstream", "-y"],
                2,
            )]),
            _ => Ok(Vec::new()),
        }
    }

    /// Without flatpak: install the distribution's flatpak package, then add
    /// Flathub. With flatpak and no remote at all: add Flathub. Otherwise,
    /// and on a system with no package manager the store knows, nothing.
    fn setup(&self) -> Option<Setup> {
        if !self.is_installed() {
            let package = self.flatpak_package()?;
            return Some(Setup {
                ops: vec![Op::Install { package }],
                steps: vec![self.add_flathub_step()],
                notice: SETUP_NOTICE.to_string(),
            });
        }
        match self.remotes() {
            Ok(remotes) if remotes.is_empty() => Some(Setup {
                ops: Vec::new(),
                steps: vec![self.add_flathub_step()],
                notice: ADD_FLATHUB_NOTICE.to_string(),
            }),
            _ => None,
        }
    }
}

/// The two installations' roots: the system one shared by every user, and
/// the user's own.
pub fn installation_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/var/lib/flatpak")];
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(home) = data_home {
        roots.push(home.join("flatpak"));
    }
    roots
}

/// Where each installation exports its applications' desktop entries; the
/// directories flatpak's profile script adds to `XDG_DATA_DIRS`.
pub fn export_dirs() -> Vec<PathBuf> {
    installation_roots()
        .into_iter()
        .map(|r| r.join("exports/share/applications"))
        .collect()
}

/// Flatpak's name for the architecture Rust reports.
pub fn flatpak_arch(rust_arch: &str) -> String {
    match rust_arch {
        "x86" => "i386".to_string(),
        other => other.to_string(),
    }
}

fn kind_of(is_app: bool, bundle: &str) -> PackageKind {
    if bundle.starts_with("runtime/") {
        PackageKind::Runtime
    } else if is_app {
        PackageKind::App
    } else {
        PackageKind::Package
    }
}

fn bundle_app_id(bundle: &str) -> Option<&str> {
    bundle.split('/').nth(1).filter(|s| !s.is_empty())
}

/// The row for exactly this application on exactly this branch. A looser
/// match on the id alone was rejected: it would mark the beta edition
/// installed because the stable one is, and the grouping rules say an
/// edition never claims what is not so.
fn find_installed<'a>(
    rows: &'a [InstalledRow],
    app_id: &str,
    branch: &str,
) -> Option<&'a InstalledRow> {
    rows.iter()
        .find(|r| r.application == app_id && r.branch == branch)
}

fn apply_installed(p: &mut Package, row: &InstalledRow) {
    p.installed = true;
    p.installed_version = row.version.clone();
    if p.version.is_none() {
        p.version = row.version.clone();
    }
    if row.size.is_some() {
        p.installed_size = row.size;
    }
    let mut facts: Vec<(String, String)> = Vec::new();
    if let Some(installation) = row.installation {
        facts.push(("Installation".to_string(), installation.name().to_string()));
    }
    facts.push(("Branch".to_string(), row.branch.clone()));
    if let Some(size) = &row.size_text {
        facts.push(("Size".to_string(), size.clone()));
    }
    for fact in facts {
        if !p.facts.iter().any(|(k, _)| *k == fact.0) {
            p.facts.push(fact);
        }
    }
}

fn apply_hit(p: &mut Package, hit: &FlathubHit) {
    if let Some(n) = hit.installs_last_month {
        p.popularity = Some((n as f64 / POPULARITY_CEILING).min(1.0));
        p.popularity_label = Some(format!("{} installs last month", thousands(n)));
    }
    if hit.updated_at.is_some() {
        p.updated = hit.updated_at;
    }
    if p.developer.is_none() {
        p.developer = hit.developer.clone();
    }
    if p.icon.is_none() {
        p.icon = hit.icon.clone().map(Picture::Url);
    }
    if p.summary.is_none() {
        p.summary = hit.summary.clone();
    }
    if hit.verified && !p.facts.iter().any(|(k, _)| k == "Verified") {
        p.facts.push(("Verified".to_string(), "Yes".to_string()));
    }
}

/// Flathub's appstream answer onto a package. Flathub is fresher than the
/// catalogue on disk, so its releases and screenshots (which come with
/// sizes) win; a local icon file is kept because it needs no fetch.
/// Returns the package and whether Flathub says the developer is verified.
fn apply_appstream(mut p: Package, json: &Value) -> (Package, Option<bool>) {
    if (p.name.is_empty() || p.name == p.appstream_id.clone().unwrap_or_default())
        && let Some(name) = str_of(json, "name")
    {
        p.name = name;
    }
    if p.summary.is_none() {
        p.summary = str_of(json, "summary");
    }
    if let Some(description) = str_of(json, "description") {
        p.description = Some(description);
    }
    if p.developer.is_none() {
        p.developer = str_of(json, "developer_name");
    }
    if p.licence.is_none() {
        p.licence = str_of(json, "project_license");
    }
    if p.homepage.is_none() {
        p.homepage = json.get("urls").and_then(|u| str_of(u, "homepage"));
    }
    if p.icon.is_none() {
        p.icon = str_of(json, "icon").map(Picture::Url);
    }
    if let Some(shots) = json.get("screenshots").and_then(Value::as_array) {
        let screenshots: Vec<Screenshot> = shots.iter().filter_map(screenshot_from).collect();
        if !screenshots.is_empty() {
            p.screenshots = screenshots;
        }
    }
    if let Some(release) = json
        .get("releases")
        .and_then(Value::as_array)
        .and_then(|r| r.first())
    {
        if let Some(version) = str_of(release, "version") {
            p.version = Some(version);
        }
        if let Some(t) = release.get("timestamp").and_then(json_i64) {
            p.updated = Some(t);
        }
    }
    if p.categories.is_empty()
        && let Some(categories) = json.get("categories").and_then(Value::as_array)
    {
        p.categories = categories
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
    }
    let verified = json
        .get("metadata")
        .and_then(|m| m.get("flathub::verification::verified"))
        .and_then(Value::as_bool);
    (p, verified)
}

/// Flathub's summary answer: sizes and the runtime, into the package and
/// the facts in the order the detail page draws them.
fn apply_summary(p: &mut Package, json: &Value, facts: &mut Vec<(String, String)>) {
    if let Some(runtime) = json.get("metadata").and_then(|m| str_of(m, "runtime")) {
        facts.push(("Runtime".to_string(), runtime));
    }
    if let Some(bytes) = json.get("download_size").and_then(json_u64) {
        p.download_size = Some(bytes);
        facts.push(("Download size".to_string(), format_size(bytes)));
    }
    if let Some(bytes) = json.get("installed_size").and_then(json_u64) {
        if p.installed_size.is_none() {
            p.installed_size = Some(bytes);
        }
        facts.push(("Installed size".to_string(), format_size(bytes)));
    }
}

/// One of Flathub's screenshots: the original rendition as the image and
/// the 624-wide one as the thumbnail, which is what a rail draws.
fn screenshot_from(shot: &Value) -> Option<Screenshot> {
    let sizes = shot.get("sizes").and_then(Value::as_array)?;
    let width_of = |s: &Value| s.get("width").and_then(json_u64).map(|w| w as u32);
    let src_of = |s: &Value| str_of(s, "src");
    let original = sizes
        .iter()
        .find(|s| src_of(s).is_some_and(|src| src.contains("_orig")))
        .or_else(|| sizes.iter().max_by_key(|s| width_of(s).unwrap_or(0)))?;
    let image = Picture::Url(src_of(original)?);
    let thumbnail = sizes
        .iter()
        .find(|s| width_of(s) == Some(624))
        .and_then(src_of)
        .map(Picture::Url);
    Some(Screenshot {
        image,
        thumbnail,
        caption: str_of(shot, "caption"),
        width: width_of(original),
        height: original.get("height").and_then(json_u64).map(|h| h as u32),
    })
}

/// The hits of a Flathub search answer, keyed by application id.
pub fn parse_hits(json: &Value) -> HashMap<String, FlathubHit> {
    let mut hits = HashMap::new();
    let Some(list) = json.get("hits").and_then(Value::as_array) else {
        return hits;
    };
    for hit in list {
        let Some(app_id) = str_of(hit, "app_id") else {
            continue;
        };
        let mut categories: Vec<String> = str_of(hit, "main_categories").into_iter().collect();
        if let Some(sub) = hit.get("sub_categories").and_then(Value::as_array) {
            categories.extend(sub.iter().filter_map(Value::as_str).map(str::to_string));
        }
        let kind = str_of(hit, "type").unwrap_or_default();
        hits.insert(
            app_id.clone(),
            FlathubHit {
                app_id,
                name: str_of(hit, "name"),
                summary: str_of(hit, "summary"),
                icon: str_of(hit, "icon"),
                installs_last_month: hit.get("installs_last_month").and_then(json_u64),
                updated_at: hit.get("updated_at").and_then(json_i64),
                verified: hit
                    .get("verification_verified")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                developer: str_of(hit, "developer_name"),
                categories,
                keywords: hit
                    .get("keywords")
                    .and_then(Value::as_array)
                    .map(|k| {
                        k.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                is_app: kind.is_empty()
                    || kind == "desktop-application"
                    || kind == "console-application",
            },
        );
    }
    hits
}

/// `flatpak remotes --columns=name,url,options`. The options cell names
/// the installation only when more than one was listed, so the caller says
/// which one it asked for.
pub fn parse_remotes(text: &str, asked: Installation) -> Vec<Remote> {
    const OPTION_WORDS: [&str; 5] = [
        "disabled",
        "oci",
        "no-enumerate",
        "no-gpg-verify",
        "filtered",
    ];
    let mut remotes = Vec::new();
    for line in text.lines() {
        let cells: Vec<&str> = line.split('\t').map(str::trim).collect();
        if cells.len() < 2 || cells[0].is_empty() || cells[0] == "Name" {
            continue;
        }
        let options: Vec<&str> = cells
            .get(2)
            .map(|o| {
                o.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if options.contains(&"disabled") {
            continue;
        }
        let mut installation = asked;
        let mut foreign = None;
        for option in &options {
            if let Some(i) = Installation::parse(option) {
                installation = i;
            } else if !OPTION_WORDS.contains(option) {
                foreign = Some(*option);
            }
        }
        // A third, named installation (from /etc/flatpak/installations.d)
        // needs `--installation=NAME`, which no step here builds; listing
        // its remotes would offer installs that then fail.
        if let Some(name) = foreign {
            log::info!(
                "skipping remote {} in installation {name}, which Brokey does not manage",
                cells[0]
            );
            continue;
        }
        remotes.push(Remote {
            name: cells[0].to_string(),
            url: cells[1].to_string(),
            installation,
        });
    }
    remotes
}

/// `flatpak list --app --columns=application,name,version,branch,origin,installation,size`.
pub fn parse_list(text: &str) -> Vec<InstalledRow> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let cells: Vec<&str> = line.split('\t').map(str::trim).collect();
        if cells.len() < 5 || cells[0].is_empty() || cells[0] == "Application ID" {
            continue;
        }
        let size_text = cells
            .get(6)
            .filter(|s| !s.is_empty())
            .map(|s| plain_spaces(s));
        rows.push(InstalledRow {
            application: cells[0].to_string(),
            name: cells[1].to_string(),
            version: Some(cells[2].to_string()).filter(|v| !v.is_empty()),
            branch: cells[3].to_string(),
            origin: cells[4].to_string(),
            installation: cells.get(5).and_then(|c| Installation::parse(c)),
            size: cells.get(6).and_then(|s| parse_size(s)),
            size_text,
        });
    }
    rows
}

/// `flatpak remote-ls --updates --app --columns=application,name,version,branch,origin,download-size`.
pub fn parse_remote_ls(text: &str) -> Vec<UpdateRow> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let cells: Vec<&str> = line.split('\t').map(str::trim).collect();
        if cells.len() < 5 || cells[0].is_empty() || cells[0] == "Application ID" {
            continue;
        }
        rows.push(UpdateRow {
            application: cells[0].to_string(),
            name: cells[1].to_string(),
            version: Some(cells[2].to_string()).filter(|v| !v.is_empty()),
            branch: cells[3].to_string(),
            origin: cells[4].to_string(),
            download_size: cells.get(5).and_then(|s| parse_size(s)),
        });
    }
    rows
}

/// A size as GLib's `g_format_size` prints it: "268.5 MB" with U+00A0
/// before the unit, "980 bytes", "1 byte"; the IEC spellings too, in case a
/// distribution builds flatpak with them. Bytes, or `None` for anything else.
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ','))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    if number.is_empty() {
        return None;
    }
    let value: f64 = number.replace(',', ".").parse().ok()?;
    let unit = unit
        .trim_matches(|c: char| c.is_whitespace())
        .to_ascii_lowercase();
    let factor: f64 = match unit.as_str() {
        "" | "b" | "byte" | "bytes" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "pb" => 1e15,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * factor).round() as u64)
}

/// Bytes as the page shows them, in the same SI units flatpak prints so a
/// user sees one number in both places: "99.7 MB", "980 bytes".
pub fn format_size(bytes: u64) -> String {
    const UNITS: [(f64, &str); 5] = [
        (1e15, "PB"),
        (1e12, "TB"),
        (1e9, "GB"),
        (1e6, "MB"),
        (1e3, "kB"),
    ];
    for (factor, unit) in UNITS {
        if bytes as f64 >= factor {
            return format!("{:.1} {unit}", bytes as f64 / factor);
        }
    }
    if bytes == 1 {
        "1 byte".to_string()
    } else {
        format!("{bytes} bytes")
    }
}

/// "64 374": digits in groups of three with a plain space, as §12 asks.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// GLib's no-break and thin spaces become plain ones, so a fact reads the
/// same as every other string on the page.
fn plain_spaces(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect()
}

/// How well a component matches a query, 0 to 1. The ladder is the same one
/// every source uses so a Flatpak edition sorts beside a pacman one: the
/// name exactly, the last segment of the id exactly ("gimp" for
/// org.gimp.GIMP), the name's start, a keyword, a word in the name, anywhere
/// in the name, a keyword's start, the id, the summary, and last every word
/// of a multi-word query somewhere.
pub fn score(component: &Component, term: &str) -> f32 {
    let q = term.trim().to_lowercase();
    if q.is_empty() {
        return 0.0;
    }
    let name = component.name.to_lowercase();
    let id = component.id.to_lowercase();
    let id_tail = id.rsplit('.').next().unwrap_or("").to_string();
    let summary = component.summary.as_deref().unwrap_or("").to_lowercase();
    let keywords: Vec<String> = component
        .keywords
        .iter()
        .map(|k| k.to_lowercase())
        .collect();
    if name == q || id_tail == q {
        return 1.0;
    }
    if name.starts_with(&q) {
        return 0.9;
    }
    if keywords.contains(&q) {
        return 0.8;
    }
    if name
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| !w.is_empty() && w.starts_with(&q))
    {
        return 0.7;
    }
    if name.contains(&q) {
        return 0.6;
    }
    if keywords.iter().any(|k| k.starts_with(&q)) {
        return 0.5;
    }
    if id.contains(&q) {
        return 0.45;
    }
    if summary.contains(&q) {
        return 0.3;
    }
    let words: Vec<&str> = q.split_whitespace().collect();
    if words.len() > 1 {
        let haystack = format!("{name} {id} {summary} {}", keywords.join(" "));
        if words.iter().all(|w| haystack.contains(w)) {
            return 0.25;
        }
    }
    0.0
}

/// One line of the progress flatpak's interactive transaction prints
/// (`app/flatpak-cli-transaction.c`, 1.18) when stdout is not a terminal:
/// `Installing 1/3…` when an operation starts, then, as it goes,
/// `Installing 1/3… █████████             45%  1.2 MB/s  00:12`: the message,
/// a space, a bar of up to twenty cells (full blocks, one partial block, then
/// spaces), a space, the percentage padded to three, then two spaces and
/// `<bytes>/s` once a second has passed, and two more spaces and the time
/// left as `MM:SS` (or `HH:MM:SS`) once it has run long enough to guess.
/// `Updating` and `Uninstalling` are the other verbs; a single operation has
/// no `1/1`. Fancy-mode lines that leak through (a `\r` and ANSI colour) are
/// read too.
///
/// These lines appear when flatpak runs without `--noninteractive`. The
/// steps this source builds pass that flag, and flatpak then uses its quiet
/// transaction, whose one line per operation [`parse_operation`] reads. Both
/// are covered so the runner can show whichever it is given.
///
/// The fraction is of the whole transaction: operation two of three at 45%
/// is 0.483, so the rail never runs backwards. A line that starts an
/// operation gives the fraction the finished ones add up to; anything that
/// is not progress gives `None` and belongs in the log.
pub fn parse_progress(line: &str) -> Option<(f32, String)> {
    let line = strip_ansi(line);
    let line = line.rsplit('\r').next().unwrap_or("").trim();
    let (verb, rest) = ["Installing", "Updating", "Uninstalling"]
        .iter()
        .find_map(|v| line.strip_prefix(v).map(|rest| (*v, rest)))?;
    let rest = rest.trim_start();
    let (counts, rest) = match rest.split_once('…').or_else(|| rest.split_once("...")) {
        Some((before, after)) => (before.trim(), after),
        None => return None,
    };
    let (op, total) = if counts.is_empty() {
        (None, None)
    } else {
        let (a, b) = counts.split_once('/')?;
        (Some(a.parse::<u32>().ok()?), Some(b.parse::<u32>().ok()?))
    };
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let percent_at = tokens.iter().position(|t| {
        t.strip_suffix('%')
            .is_some_and(|n| n.parse::<u32>().is_ok())
    });
    let percent = percent_at.map(|i| {
        tokens[i]
            .trim_end_matches('%')
            .parse::<u32>()
            .unwrap_or(0)
            .min(100)
    });
    let tail: Vec<&str> = percent_at
        .map(|i| tokens[i + 1..].to_vec())
        .unwrap_or_default();

    let done = op.map(|o| o.saturating_sub(1)).unwrap_or(0) as f32;
    let within = percent.map(|p| p as f32 / 100.0).unwrap_or(0.0);
    let fraction = match total {
        Some(n) if n > 0 => ((done + within) / n as f32).clamp(0.0, 1.0),
        _ => within,
    };

    let mut message = match (op, total) {
        (Some(o), Some(n)) => format!("{verb} {o} of {n}"),
        _ => verb.to_string(),
    };
    if let Some(p) = percent {
        message.push_str(&format!(": {p}%"));
        // "1.2 MB/s  00:12": the speed is two tokens because of the space
        // GLib puts before the unit, then the time left when there is one.
        let speed: Vec<&str> = tail
            .iter()
            .copied()
            .take_while(|t| !t.contains(':'))
            .collect();
        if !speed.is_empty() {
            message.push_str(&format!(", {}", speed.join(" ")));
        }
        if let Some(left) = tail.iter().find(|t| t.contains(':')) {
            message.push_str(&format!(", {left} left"));
        }
    }
    Some((fraction, message))
}

/// One line of the quiet output flatpak prints for a `--noninteractive`
/// step (`app/flatpak-quiet-transaction.c`, 1.18): `Installing
/// app/org.gimp.GIMP/x86_64/stable`, `Updating runtime/org.gnome.Platform/x86_64/50`
/// or `Uninstalling …`, one per operation and nothing while it runs. There
/// is no count and no percentage, so the sentence comes without a fraction:
/// the runner draws it beside an empty rail rather than inventing one.
/// Anything else gives `None` and belongs in the log.
pub fn parse_operation(line: &str) -> Option<String> {
    let line = line.trim();
    let (verb, rest) = ["Installing", "Updating", "Uninstalling"]
        .iter()
        .find_map(|v| line.strip_prefix(v).map(|rest| (*v, rest)))?;
    let rest = rest.trim();
    if rest.contains(char::is_whitespace) {
        return None;
    }
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() != 4 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    // The id is what a person recognises; a runtime keeps its branch
    // because "org.gnome.Platform" alone does not say which one.
    match parts[0] {
        "app" => Some(format!("{verb} {}", parts[1])),
        "runtime" => Some(format!("{verb} the {} {} runtime", parts[1], parts[3])),
        _ => None,
    }
}

fn strip_ansi(line: &str) -> String {
    static ANSI: OnceLock<regex::Regex> = OnceLock::new();
    let re = ANSI.get_or_init(|| {
        regex::Regex::new("\x1b\\[[0-9;?]*[ -/]*[@-~]").expect("a fixed pattern compiles")
    });
    re.replace_all(line, "").into_owned()
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Flathub sends some numbers as strings ("1920", "1776384000").
fn json_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn json_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(id: &str, name: &str, summary: &str, keywords: &[&str]) -> Component {
        Component {
            id: id.to_string(),
            origin: "flathub".to_string(),
            bundle: Some(format!("app/{id}/x86_64/stable")),
            name: name.to_string(),
            summary: Some(summary.to_string()),
            keywords: keywords.iter().map(|k| k.to_string()).collect(),
            is_app: true,
            ..Component::default()
        }
    }

    #[test]
    fn remotes_from_a_listing_of_both_installations() {
        let text = "flathub\thttps://dl.flathub.org/repo/\tsystem\nfedora\toci+https://registry.fedoraproject.org\tsystem,oci\nflathub\thttps://dl.flathub.org/repo/\tuser\nold\thttps://example.org/repo\tuser,disabled\n";
        let remotes = parse_remotes(text, Installation::System);
        assert_eq!(remotes.len(), 3, "the disabled remote is left out");
        assert_eq!(remotes[0].installation, Installation::System);
        assert_eq!(remotes[1].name, "fedora");
        assert_eq!(remotes[2].installation, Installation::User);
        assert!(remotes[0].is_flathub());
        assert!(!remotes[1].is_flathub());
    }

    #[test]
    fn remotes_from_one_installation_take_the_asked_one() {
        let text = "flathub\thttps://dl.flathub.org/repo/\t\nfedora\toci+https://registry.fedoraproject.org\toci\n";
        let remotes = parse_remotes(text, Installation::User);
        assert_eq!(remotes.len(), 2);
        assert!(remotes.iter().all(|r| r.installation == Installation::User));
    }

    #[test]
    fn a_named_installation_is_skipped() {
        let text = "flathub\thttps://dl.flathub.org/repo/\textra\n";
        assert!(parse_remotes(text, Installation::System).is_empty());
    }

    #[test]
    fn flathub_beta_is_not_flathub() {
        let beta = Remote {
            name: "flathub-beta".into(),
            url: "https://dl.flathub.org/beta-repo/".into(),
            installation: Installation::User,
        };
        assert!(!beta.is_flathub());
        let renamed = Remote {
            name: "hub".into(),
            url: "https://dl.flathub.org/repo".into(),
            installation: Installation::System,
        };
        assert!(renamed.is_flathub());
    }

    #[test]
    fn list_rows_read_back_with_sizes() {
        let text = "org.gimp.GIMP\tGNU Image Manipulation Program\t3.2.4\tstable\tflathub\tsystem\t268.5\u{a0}MB\norg.example.NoVersion\tNoVersion\t\tstable\tflathub-beta\tuser\t980\u{a0}bytes\n";
        let rows = parse_list(text);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].version.as_deref(), Some("3.2.4"));
        assert_eq!(rows[0].installation, Some(Installation::System));
        assert_eq!(rows[0].size, Some(268_500_000));
        assert_eq!(rows[0].size_text.as_deref(), Some("268.5 MB"));
        assert_eq!(rows[1].version, None);
        assert_eq!(rows[1].installation, Some(Installation::User));
        assert_eq!(rows[1].size, Some(980));
    }

    #[test]
    fn remote_ls_rows_read_back() {
        let text =
            "org.gimp.GIMP\tGNU Image Manipulation Program\t3.2.6\tstable\tflathub\t99.7\u{a0}MB\n";
        let rows = parse_remote_ls(text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].version.as_deref(), Some("3.2.6"));
        assert_eq!(rows[0].download_size, Some(99_700_000));
    }

    #[test]
    fn sizes_in_every_spelling() {
        assert_eq!(parse_size("268.5\u{a0}MB"), Some(268_500_000));
        assert_eq!(parse_size("268.5 MB"), Some(268_500_000));
        assert_eq!(parse_size("268.5MB"), Some(268_500_000));
        assert_eq!(parse_size("1.2\u{a0}GB"), Some(1_200_000_000));
        assert_eq!(parse_size("13.0\u{a0}kB"), Some(13_000));
        assert_eq!(parse_size("980 bytes"), Some(980));
        assert_eq!(parse_size("1 byte"), Some(1));
        assert_eq!(parse_size("1.5\u{a0}MiB"), Some(1_572_864));
        assert_eq!(parse_size("12,3 MB"), Some(12_300_000));
        assert_eq!(parse_size("4096"), Some(4096));
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("lots"), None);
        assert_eq!(parse_size("12 parsecs"), None);
    }

    #[test]
    fn sizes_format_the_way_flatpak_prints_them() {
        assert_eq!(format_size(99_676_175), "99.7 MB");
        assert_eq!(format_size(268_452_352), "268.5 MB");
        assert_eq!(format_size(1_200_000_000), "1.2 GB");
        assert_eq!(format_size(980), "980 bytes");
        assert_eq!(format_size(1), "1 byte");
        assert_eq!(format_size(1000), "1.0 kB");
    }

    #[test]
    fn thousands_get_spaces() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(953), "953");
        assert_eq!(thousands(2465), "2 465");
        assert_eq!(thousands(64374), "64 374");
        assert_eq!(thousands(1_234_567), "1 234 567");
    }

    #[test]
    fn a_ref_takes_apart_and_goes_back() {
        let r = FlatpakRef::parse("flathub/app/org.gimp.GIMP/x86_64/stable").unwrap();
        assert_eq!(r.remote, "flathub");
        assert_eq!(r.app_id, "org.gimp.GIMP");
        assert_eq!(r.arch, "x86_64");
        assert_eq!(r.branch, "stable");
        assert_eq!(r.bundle(), "app/org.gimp.GIMP/x86_64/stable");
        assert_eq!(r.id(), "flathub/app/org.gimp.GIMP/x86_64/stable");
        for bad in [
            "org.gimp.GIMP",
            "flathub/app/org.gimp.GIMP",
            "flathub/thing/a/b/c",
            "flathub//a/b/c",
            "",
        ] {
            let e = FlatpakRef::parse(bad).unwrap_err();
            assert!(
                e.message.contains("is not a Flatpak ref"),
                "{bad}: {}",
                e.message
            );
            assert!(!e.message.contains('\u{2014}'));
        }
    }

    #[test]
    fn architectures_use_flatpak_s_names() {
        assert_eq!(flatpak_arch("x86_64"), "x86_64");
        assert_eq!(flatpak_arch("aarch64"), "aarch64");
        assert_eq!(flatpak_arch("x86"), "i386");
    }

    #[test]
    fn scoring_ladder() {
        let gimp = component(
            "org.gimp.GIMP",
            "GNU Image Manipulation Program",
            "High-end image creation and manipulation",
            &["GIMP", "Photoshop"],
        );
        assert_eq!(score(&gimp, "gimp"), 1.0, "the id's last segment");
        assert_eq!(
            score(&gimp, "GNU Image Manipulation Program"),
            1.0,
            "the name, any case"
        );
        assert_eq!(score(&gimp, "gnu im"), 0.9, "the start of the name");
        assert_eq!(score(&gimp, "photoshop"), 0.8, "a keyword");
        assert_eq!(
            score(&gimp, "manip"),
            0.7,
            "the start of a word in the name"
        );
        assert_eq!(score(&gimp, "ge manip"), 0.6, "anywhere in the name");
        assert_eq!(score(&gimp, "photo"), 0.5, "the start of a keyword");
        assert_eq!(score(&gimp, "org.gimp"), 0.45, "the id");
        assert_eq!(score(&gimp, "high-end"), 0.3, "the summary");
        assert_eq!(
            score(&gimp, "creation program"),
            0.25,
            "every word somewhere"
        );
        assert_eq!(score(&gimp, "spreadsheet"), 0.0);
        assert_eq!(score(&gimp, "   "), 0.0);
    }

    #[test]
    fn progress_lines_from_a_three_step_install() {
        let (f, m) = parse_progress("Installing 1/3…").unwrap();
        assert_eq!(f, 0.0);
        assert_eq!(m, "Installing 1 of 3");

        let (f, m) =
            parse_progress("Installing 1/3… █████████▏            45%  1.2\u{a0}MB/s  00:12")
                .unwrap();
        assert!((f - 0.15).abs() < 1e-6, "{f}");
        assert_eq!(m, "Installing 1 of 3: 45%, 1.2 MB/s, 00:12 left");

        let (f, m) =
            parse_progress("Installing 2/3… ████████████████████ 100%  3.4\u{a0}MB/s").unwrap();
        assert!((f - 2.0 / 3.0).abs() < 1e-6, "{f}");
        assert_eq!(m, "Installing 2 of 3: 100%, 3.4 MB/s");

        let (f, m) = parse_progress("Installing 3/3…                       0%").unwrap();
        assert!((f - 2.0 / 3.0).abs() < 1e-6, "{f}");
        assert_eq!(m, "Installing 3 of 3: 0%");
    }

    #[test]
    fn progress_lines_for_one_operation_and_other_verbs() {
        assert_eq!(
            parse_progress("Installing…"),
            Some((0.0, "Installing".to_string()))
        );
        let (f, m) = parse_progress("Updating… ██████████           50%").unwrap();
        assert!((f - 0.5).abs() < 1e-6);
        assert_eq!(m, "Updating: 50%");
        assert_eq!(
            parse_progress("Uninstalling 2/2…"),
            Some((0.5, "Uninstalling 2 of 2".to_string()))
        );
        assert_eq!(
            parse_progress("Updating..."),
            Some((0.0, "Updating".to_string()))
        );
        let (f, m) =
            parse_progress("Installing… ████████████████████ 100%  1.6\u{a0}MB/s  00:00").unwrap();
        assert_eq!(f, 1.0);
        assert_eq!(m, "Installing: 100%, 1.6 MB/s, 00:00 left");
    }

    #[test]
    fn progress_lines_from_fancy_mode_are_read_too() {
        let line =
            "\rInstalling 1/2… \x1b[2m████████\x1b[22m             40%  2.0\u{a0}MB/s  00:05";
        let (f, m) = parse_progress(line).unwrap();
        assert!((f - 0.2).abs() < 1e-6, "{f}");
        assert_eq!(m, "Installing 1 of 2: 40%, 2.0 MB/s, 00:05 left");
    }

    #[test]
    fn lines_that_are_not_progress() {
        for line in [
            "Warning: Failed to get id for something",
            "Installing required authenticator for remote flathub",
            "Info: org.gimp.GIMP was skipped",
            "Installation complete.",
            "",
            "        ID                          Branch   Op   Remote    Download",
            " 1. [✓] org.gimp.GIMP              stable   i    flathub   99.7 MB / 99.7 MB",
        ] {
            assert_eq!(parse_progress(line), None, "{line:?}");
        }
    }

    #[test]
    fn quiet_operation_lines_become_sentences_without_a_fraction() {
        assert_eq!(
            parse_operation("Installing app/org.gimp.GIMP/x86_64/stable"),
            Some("Installing org.gimp.GIMP".to_string())
        );
        assert_eq!(
            parse_operation("Installing runtime/org.gnome.Platform/x86_64/50\n"),
            Some("Installing the org.gnome.Platform 50 runtime".to_string())
        );
        assert_eq!(
            parse_operation("Updating app/org.mozilla.firefox/x86_64/stable"),
            Some("Updating org.mozilla.firefox".to_string())
        );
        assert_eq!(
            parse_operation("Uninstalling app/org.gimp.GIMP/x86_64/stable"),
            Some("Uninstalling org.gimp.GIMP".to_string())
        );
        for line in [
            "Installing 1/3…",
            "Installing…",
            "Installing 1/3… ████████             40%",
            "Info: org.gimp.GIMP was skipped",
            "Installing required authenticator for remote flathub",
            "org.gimp.GIMP already installed",
            "Installing thing/org.gimp.GIMP/x86_64/stable",
            "",
        ] {
            assert_eq!(parse_operation(line), None, "{line:?}");
        }
        assert_eq!(
            parse_progress("Installing app/org.gimp.GIMP/x86_64/stable"),
            None,
            "a quiet line carries no fraction and must not be read as one"
        );
    }

    #[test]
    fn hits_read_the_fields_the_row_needs() {
        let json: Value = serde_json::json!({
            "hits": [
                {"app_id": "org.gimp.GIMP", "name": "GNU Image Manipulation Program", "installs_last_month": 64374,
                 "updated_at": 1788654532, "verification_verified": true, "developer_name": "The GIMP team",
                 "icon": "https://dl.flathub.org/x.png", "main_categories": "graphics", "sub_categories": ["2DGraphics"],
                 "type": "desktop-application", "keywords": ["GIMP"]},
                {"app_id": "org.example.Sparse"},
                {"name": "no id"}
            ]
        });
        let hits = parse_hits(&json);
        assert_eq!(hits.len(), 2);
        let gimp = &hits["org.gimp.GIMP"];
        assert_eq!(gimp.installs_last_month, Some(64374));
        assert!(gimp.verified);
        assert_eq!(gimp.categories, ["graphics", "2DGraphics"]);
        assert!(gimp.is_app);
        let sparse = &hits["org.example.Sparse"];
        assert_eq!(sparse.installs_last_month, None);
        assert!(!sparse.verified);
        assert!(sparse.is_app, "an unknown type is listed as an application");
    }

    #[test]
    fn a_hit_becomes_popularity_and_a_fact() {
        let mut p = Package::new(
            SourceKind::Flatpak,
            "flathub/app/org.gimp.GIMP/x86_64/stable",
            "GIMP",
        );
        let hit = FlathubHit {
            app_id: "org.gimp.GIMP".into(),
            installs_last_month: Some(64374),
            updated_at: Some(1788654532),
            verified: true,
            developer: Some("The GIMP team".into()),
            icon: Some("https://dl.flathub.org/x.png".into()),
            ..FlathubHit::default()
        };
        apply_hit(&mut p, &hit);
        apply_hit(&mut p, &hit);
        assert!((p.popularity.unwrap() - 0.64374).abs() < 1e-9);
        assert_eq!(
            p.popularity_label.as_deref(),
            Some("64 374 installs last month")
        );
        assert_eq!(p.updated, Some(1788654532));
        assert_eq!(p.developer.as_deref(), Some("The GIMP team"));
        assert_eq!(
            p.icon,
            Some(Picture::Url("https://dl.flathub.org/x.png".into()))
        );
        assert_eq!(
            p.facts,
            vec![("Verified".to_string(), "Yes".to_string())],
            "applied twice, listed once"
        );

        let mut big = Package::new(SourceKind::Flatpak, "x", "x");
        apply_hit(
            &mut big,
            &FlathubHit {
                installs_last_month: Some(2_500_000),
                ..FlathubHit::default()
            },
        );
        assert_eq!(big.popularity, Some(1.0), "capped");
    }

    #[test]
    fn screenshots_take_the_original_and_the_624_thumbnail() {
        let shot = serde_json::json!({
            "caption": "Editing",
            "sizes": [
                {"width": "1920", "height": "1080", "src": "https://x/image-1_orig.png"},
                {"width": "624", "height": "351", "src": "https://x/image-1_624x351@1.png"},
                {"width": "224", "height": "126", "src": "https://x/image-1_224x126@1.png"}
            ]
        });
        let s = screenshot_from(&shot).unwrap();
        assert_eq!(s.image, Picture::Url("https://x/image-1_orig.png".into()));
        assert_eq!(
            s.thumbnail,
            Some(Picture::Url("https://x/image-1_624x351@1.png".into()))
        );
        assert_eq!((s.width, s.height), (Some(1920), Some(1080)));
        assert_eq!(s.caption.as_deref(), Some("Editing"));

        let no_orig = serde_json::json!({"sizes": [{"width": 752, "src": "https://x/a.png"}, {"width": 1248, "src": "https://x/b.png"}]});
        let s = screenshot_from(&no_orig).unwrap();
        assert_eq!(
            s.image,
            Picture::Url("https://x/b.png".into()),
            "the widest stands in"
        );
        assert_eq!(s.thumbnail, None);
        assert_eq!(
            screenshot_from(&serde_json::json!({"caption": "empty"})),
            None
        );
    }

    #[test]
    fn the_copy_has_no_em_dashes() {
        for s in [NOT_INSTALLED, NO_REMOTES] {
            assert!(!s.contains('\u{2014}'));
            assert!(s.ends_with('.'));
        }
    }
}
