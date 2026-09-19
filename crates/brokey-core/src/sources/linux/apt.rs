//! The apt source: Debian, Ubuntu, Mint, Pop!_OS and everything else that
//! keeps its packages in dpkg's database.
//!
//! Nothing here links libapt. The two files apt itself reads are plain text
//! in the Debian control format, so this module reads them directly:
//! `/var/lib/apt/lists/*_Packages` (one stanza per available package, one
//! file per repository, suite and component) and `/var/lib/dpkg/status`
//! (the same stanzas for everything dpkg has ever touched, each with a
//! `Status` line). The list file's name encodes where it came from, which
//! is how a package gets its "bookworm/main" label. Debian's AppStream
//! catalogue is keyed by package name, so [`Catalogue::by_pkgname`] gives
//! the icon, the display name and whether it is an application.
//!
//! Which updates apt will apply is apt's answer, not the newest version in the
//! lists: pins (Pop!_OS prefers its own repository over Ubuntu's), suites apt
//! never upgrades from by itself (backports) and Ubuntu's phased updates all
//! keep a newer version back. So the Updates page lists what
//! `apt-get upgrade --simulate --with-new-pkgs` says it would install, the one
//! read-only question this module asks apt itself, and Update all runs the
//! same command for real. 0.1.3 listed 78 updates on a Pop!_OS machine that
//! apt answered with "is already the newest version".
//!
//! Versions are ordered by dpkg's own algorithm ([`dpkg_compare`]), which is
//! not pacman's: `~` sorts before everything including the end of the
//! string, and letters sort before punctuation. Getting that wrong would
//! offer `1.0~rc1` as an update to `1.0`.
//!
//! This source was written on an Arch machine against fixtures and is
//! marked untested until it has run on a real Debian. On a machine that is
//! not Debian-based it reports itself unavailable with the reason.

use crate::appstream::{Catalogue, Component};
use crate::model::*;
use crate::{Error, Op, Query, Result, Source};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// The name of this store's own Debian package, so its update is flagged.
const SELF_PACKAGE: &str = "brokey";

/// The arguments of apt's answer to "what would an upgrade install". The
/// verb first, so a failure reads "apt-get upgrade failed". Update all runs
/// [`UPGRADE`], the same command without `--simulate`.
const SIMULATE_UPGRADE: [&str; 3] = ["upgrade", "--simulate", "--with-new-pkgs"];
const UPGRADE: [&str; 3] = ["upgrade", "--with-new-pkgs", "-y"];

/// Asks apt what an upgrade would install and returns what it printed.
/// Read-only and without root (apt-get simulates without the lock); a
/// closure so the fixture tests answer from recorded output.
pub type Simulate = Box<dyn Fn() -> Result<String> + Send + Sync>;

pub struct Apt {
    system: SystemInfo,
    catalogue: Arc<Catalogue>,
    lists_dir: PathBuf,
    status_path: PathBuf,
    apt_get: Option<PathBuf>,
    /// Debian's name for the machine's architecture: "amd64", "arm64".
    arch: String,
    simulate: Simulate,
    /// The parsed lists, rebuilt when any file's size or mtime changes.
    /// Parsing bookworm's main list takes a noticeable fraction of a
    /// second, and a search must not pay that every keystroke.
    cache: Mutex<Option<Arc<Index>>>,
}

impl Apt {
    pub const LISTS_DIR: &str = "/var/lib/apt/lists";
    pub const STATUS_PATH: &str = "/var/lib/dpkg/status";

    pub fn new(system: &SystemInfo, catalogue: Arc<Catalogue>) -> Apt {
        Apt::at(
            system,
            catalogue,
            PathBuf::from(Self::LISTS_DIR),
            PathBuf::from(Self::STATUS_PATH),
            crate::system::which("apt-get"),
        )
    }

    /// The same source over chosen files, so the fixtures can stand in
    /// for a Debian machine. `apt_get` is where the binary is, or `None`
    /// when it is not on the machine.
    pub fn at(
        system: &SystemInfo,
        catalogue: Arc<Catalogue>,
        lists_dir: PathBuf,
        status_path: PathBuf,
        apt_get: Option<PathBuf>,
    ) -> Apt {
        Apt {
            arch: debian_arch(&system.arch).to_string(),
            system: system.clone(),
            catalogue,
            lists_dir,
            status_path,
            apt_get,
            simulate: Box::new(|| {
                crate::system::run("apt-get", &SIMULATE_UPGRADE).map_err(|e| {
                    Error::from_source(
                        SourceKind::Apt,
                        format!(
                            "apt could not say which updates it would install. {}",
                            e.message
                        ),
                    )
                })
            }),
            cache: Mutex::new(None),
        }
    }

    /// The same source with apt's upgrade simulation answered by `simulate`.
    pub fn with_simulation(mut self, simulate: Simulate) -> Apt {
        self.simulate = simulate;
        self
    }

    /// Why this machine cannot use apt, or `None` when it can.
    fn unavailable_reason(&self) -> Option<String> {
        if !self.system.is_debian_like() {
            return Some(format!(
                "apt is for Debian-based systems; this is {}.",
                self.system.pretty_name
            ));
        }
        if self.apt_get.is_none() {
            return Some("apt-get is not installed, so packages cannot be changed.".to_string());
        }
        if !self.status_path.exists() {
            return Some(format!(
                "{} does not exist, so installed packages cannot be read.",
                self.status_path.display()
            ));
        }
        None
    }

    /// The distinct suites the lists cover, for the status bar:
    /// "bookworm, bookworm-security".
    fn suites(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.lists_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(list) = ListName::parse(&name.to_string_lossy()) else {
                    continue;
                };
                if !list.is_for(&self.arch) {
                    continue;
                }
                if let Some(suite) = list.suite
                    && !out.contains(&suite)
                {
                    out.push(suite);
                }
            }
        }
        out.sort();
        out
    }

    /// The parsed lists and status file, from the cache when nothing on
    /// disk has changed since.
    fn index(&self) -> Result<Arc<Index>> {
        let stamp = self.stamp()?;
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(index) = cache.as_ref()
            && index.stamp == stamp
        {
            return Ok(index.clone());
        }
        let index = Arc::new(self.load(stamp)?);
        *cache = Some(index.clone());
        Ok(index)
    }

    /// Every file the index depends on with its size and mtime. Cheaper
    /// than hashing and enough: apt rewrites a list rather than editing it.
    fn stamp(&self) -> Result<Vec<Stamp>> {
        let mut stamp = Vec::new();
        for path in self.list_paths()? {
            stamp.push(Stamp::of(path));
        }
        stamp.push(Stamp::of(self.status_path.clone()));
        stamp.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(stamp)
    }

    /// The Packages files for this machine's architecture, in name order.
    /// A missing lists directory is a machine that has never run
    /// `apt update`: nothing available, not an error.
    fn list_paths(&self) -> Result<Vec<PathBuf>> {
        let entries = match std::fs::read_dir(&self.lists_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(Error::from_source(
                    SourceKind::Apt,
                    format!(
                        "apt could not read {}: {e}. Check that the directory is readable.",
                        self.lists_dir.display()
                    ),
                ));
            }
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| {
                ListName::parse(&entry.file_name().to_string_lossy())
                    .is_some_and(|list| list.is_for(&self.arch))
            })
            .map(|entry| entry.path())
            .collect();
        paths.sort();
        Ok(paths)
    }

    fn load(&self, stamp: Vec<Stamp>) -> Result<Index> {
        let mut available: HashMap<String, DebPackage> = HashMap::new();
        for path in self.list_paths()? {
            let file_name = path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            let Some(list) = ListName::parse(&file_name) else {
                continue;
            };
            let text = match read_list(&path) {
                Ok(text) => text,
                Err(e) => {
                    log::warn!("skipping {}: {e}", path.display());
                    continue;
                }
            };
            for mut pkg in parse_control(&text) {
                if pkg.architecture != self.arch && pkg.architecture != "all" {
                    continue;
                }
                pkg.repo = Some(list.repo());
                pkg.origin = Some(list.host.clone());
                match available.get(&pkg.name) {
                    Some(have) if dpkg_compare(&have.version, &pkg.version) != Ordering::Less => {}
                    _ => {
                        available.insert(pkg.name.clone(), pkg);
                    }
                }
            }
        }
        let status = std::fs::read(&self.status_path).map_err(|e| {
            Error::from_source(
                SourceKind::Apt,
                format!(
                    "apt could not read {}: {e}. Check that the file is readable.",
                    self.status_path.display()
                ),
            )
        })?;
        let installed = parse_control(&String::from_utf8_lossy(&status))
            .into_iter()
            .filter(|pkg| pkg.is_installed())
            .map(|pkg| (pkg.name.clone(), pkg))
            .collect();
        Ok(Index {
            stamp,
            available,
            installed,
            upgrades: OnceLock::new(),
        })
    }

    /// One [`Package`] from what the lists and the status file say about a
    /// name. `full` adds what only the detail page draws.
    fn to_package(&self, name: &str, index: &Index, full: bool) -> Option<Package> {
        let avail = index.available.get(name);
        let inst = index.installed.get(name);
        let base = avail.or(inst)?;
        let mut pkg = Package::new(SourceKind::Apt, name, name);
        pkg.version = avail.map(|a| a.version.clone());
        // A newer version apt will not install is not on offer: the version
        // shown is the one apt would upgrade to, or the installed one.
        if let (Some(a), Some(i)) = (avail, inst)
            && dpkg_is_newer(&a.version, &i.version)
            && let Ok(upgrades) = self.upgrades(index)
        {
            pkg.version = Some(
                upgrades
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| i.version.clone()),
            );
        }
        pkg.installed_version = inst.map(|i| i.version.clone());
        pkg.installed = inst.is_some();
        pkg.repo = avail.and_then(|a| a.repo.clone());
        pkg.summary = base.summary.clone();
        pkg.homepage = base.homepage.clone();
        pkg.download_size = avail.and_then(|a| a.size);
        pkg.installed_size = base.installed_size;
        if base.section.as_deref() == Some("fonts") {
            pkg.kind = PackageKind::Font;
        }
        if full {
            if !base.description.is_empty() {
                pkg.description = Some(description_markup(&base.description));
            }
            pkg.facts = base.facts();
        }
        if let Some(component) = self.catalogue.by_pkgname(name) {
            apply_component(&mut pkg, component);
        }
        Some(pkg)
    }

    /// What an upgrade would install, name to version, by apt's own answer.
    /// Asked once per index, and not at all when no list has a newer
    /// version of anything installed, so a machine that is up to date and
    /// the fixtures without updates never run apt-get.
    fn upgrades<'a>(&self, index: &'a Index) -> Result<&'a HashMap<String, String>> {
        static NONE: OnceLock<HashMap<String, String>> = OnceLock::new();
        let any_newer = index.installed.iter().any(|(name, inst)| {
            index
                .available
                .get(name)
                .is_some_and(|avail| dpkg_is_newer(&avail.version, &inst.version))
        });
        if !any_newer {
            return Ok(NONE.get_or_init(HashMap::new));
        }
        index
            .upgrades
            .get_or_init(|| {
                (self.simulate)()
                    .map(|text| parse_simulation(&text, &self.arch).into_iter().collect())
                    .map_err(|e| e.message)
            })
            .as_ref()
            .map_err(|message| Error::from_source(SourceKind::Apt, message.clone()))
    }

    fn pending_updates(&self, index: &Index) -> Result<Vec<Update>> {
        let mut out = Vec::new();
        for (name, to) in self.upgrades(index)? {
            // apt leaves a held package alone and never lists it; checked
            // here too so a stale answer cannot offer one.
            let Some(inst) = index.installed.get(name) else {
                continue;
            };
            if inst.is_held() {
                continue;
            }
            let Some(pkg) = self.to_package(name, index, false) else {
                continue;
            };
            let avail = index.available.get(name).filter(|a| &a.version == to);
            out.push(Update {
                package: pkg.reference(),
                name: pkg.name,
                kind: pkg.kind,
                summary: pkg.summary,
                icon: pkg.icon,
                from: Some(inst.version.clone()),
                to: to.clone(),
                download_size: avail.and_then(|a| a.size),
                published: None,
                is_self: name == SELF_PACKAGE,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn step(&self, title: String, args: &[&str], weight: u32) -> Step {
        Step {
            source: SourceKind::Apt,
            title,
            command: Command {
                program: "apt-get".to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: vec![
                    ("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string()),
                    ("LC_ALL".to_string(), "C.UTF-8".to_string()),
                ],
                cwd: None,
            },
            needs_root: true,
            weight,
        }
    }

    fn own(&self, package: &PackageRef) -> Result<()> {
        if package.source != SourceKind::Apt {
            return Err(Error::from_source(
                SourceKind::Apt,
                format!(
                    "{} is a {} package, not an apt package.",
                    package.id,
                    package.source.label()
                ),
            ));
        }
        Ok(())
    }
}

impl Source for Apt {
    fn kind(&self) -> SourceKind {
        SourceKind::Apt
    }

    fn status(&self) -> SourceStatus {
        match self.unavailable_reason() {
            Some(reason) => SourceStatus {
                kind: SourceKind::Apt,
                available: false,
                reason: Some(reason),
                detail: None,
                searchable: false,
                setup: None,
            },
            None => {
                let suites = self.suites();
                SourceStatus {
                    kind: SourceKind::Apt,
                    available: true,
                    reason: None,
                    detail: if suites.is_empty() {
                        Some("no package lists yet".to_string())
                    } else {
                        Some(suites.join(", "))
                    },
                    searchable: false,
                    setup: None,
                }
            }
        }
    }

    fn search(&self, query: &Query) -> Result<Vec<Package>> {
        if let Some(reason) = self.unavailable_reason() {
            return Err(Error::from_source(SourceKind::Apt, reason));
        }
        let Some(words) = SearchWords::new(&query.text) else {
            return Ok(Vec::new());
        };
        let index = self.index()?;
        let mut hits: Vec<(u32, &str)> = Vec::new();
        for (name, pkg) in index.available.iter().chain(
            index
                .installed
                .iter()
                .filter(|(n, _)| !index.available.contains_key(*n)),
        ) {
            if let Some(score) = relevance(name, pkg.summary.as_deref(), &pkg.description, &words) {
                hits.push((score, name));
            }
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        Ok(hits
            .iter()
            .take(limit(query))
            .filter_map(|(_, name)| self.to_package(name, &index, false))
            .collect())
    }

    fn installed(&self) -> Result<Vec<Package>> {
        if let Some(reason) = self.unavailable_reason() {
            return Err(Error::from_source(SourceKind::Apt, reason));
        }
        let index = self.index()?;
        let mut names: Vec<&String> = index.installed.keys().collect();
        names.sort();
        Ok(names
            .into_iter()
            .filter_map(|name| self.to_package(name, &index, false))
            .collect())
    }

    fn updates(&self) -> Result<Vec<Update>> {
        if let Some(reason) = self.unavailable_reason() {
            return Err(Error::from_source(SourceKind::Apt, reason));
        }
        let index = self.index()?;
        self.pending_updates(&index)
    }

    fn details(&self, id: &str) -> Result<Package> {
        if let Some(reason) = self.unavailable_reason() {
            return Err(Error::from_source(SourceKind::Apt, reason));
        }
        let index = self.index()?;
        self.to_package(id, &index, true).ok_or_else(|| {
            Error::from_source(
                SourceKind::Apt,
                format!("{id} is not in any apt list on this machine and is not installed. Refresh the lists and search again."),
            )
        })
    }

    /// dpkg records a package's files in `info/<name>.list`, or
    /// `info/<name>:<arch>.list` for a multi-arch package, beside `status`.
    fn launcher(&self, id: &str) -> Option<crate::launch::Launch> {
        let info = self.status_path.parent()?.join("info");
        let candidates = [
            info.join(format!("{id}.list")),
            info.join(format!("{id}:{}.list", self.arch)),
        ];
        let text = candidates
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())?;
        crate::launch::from_files(text.lines(), std::path::Path::new("/"), &[id])
    }

    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        if let Some(reason) = self.unavailable_reason() {
            return Err(Error::from_source(SourceKind::Apt, reason));
        }
        let index = self.index()?;
        match op {
            Op::Install { package } => {
                self.own(package)?;
                let name = package.id.as_str();
                if index.installed.contains_key(name) {
                    return Ok(Vec::new());
                }
                if !index.available.contains_key(name) {
                    return Err(Error::from_source(
                        SourceKind::Apt,
                        format!(
                            "{name} is not in any apt list on this machine. Refresh the lists and try again."
                        ),
                    ));
                }
                Ok(vec![self.step(
                    format!("Installing {name}"),
                    &["install", "-y", name],
                    3,
                )])
            }
            Op::Remove { package } => {
                self.own(package)?;
                let name = package.id.as_str();
                if !index.installed.contains_key(name) {
                    return Ok(Vec::new());
                }
                Ok(vec![self.step(
                    format!("Removing {name}"),
                    &["remove", "-y", name],
                    2,
                )])
            }
            Op::Update { package } => {
                self.own(package)?;
                let name = package.id.as_str();
                if !index.installed.contains_key(name) {
                    return Err(Error::from_source(
                        SourceKind::Apt,
                        format!("{name} is not installed, so there is nothing to update."),
                    ));
                }
                if !self.upgrades(&index)?.contains_key(name) {
                    return Ok(Vec::new());
                }
                Ok(vec![self.step(
                    format!("Updating {name}"),
                    &["install", "--only-upgrade", "-y", name],
                    3,
                )])
            }
            Op::UpdateAll { .. } => {
                let count = self.pending_updates(&index)?.len() as u32;
                if count == 0 {
                    return Ok(Vec::new());
                }
                Ok(vec![self.step(
                    format!(
                        "Updating {count} apt {}",
                        if count == 1 { "package" } else { "packages" }
                    ),
                    &UPGRADE,
                    3 * count,
                )])
            }
            Op::Refresh { .. } => Ok(vec![self.step(
                "Refreshing the apt package lists".to_string(),
                &["update"],
                1,
            )]),
            // The planner expands a setup through `Source::setup`.
            Op::Setup { .. } => Ok(Vec::new()),
        }
    }
}

/// A list file with its size and mtime, so a rebuilt index can be told
/// from a stale one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    path: PathBuf,
    modified: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    fn of(path: PathBuf) -> Stamp {
        let meta = std::fs::metadata(&path).ok();
        Stamp {
            modified: meta.as_ref().and_then(|m| m.modified().ok()),
            len: meta.map(|m| m.len()).unwrap_or(0),
            path,
        }
    }
}

/// Everything the lists and the status file say, keyed by package name.
struct Index {
    stamp: Vec<Stamp>,
    /// The newest version of each name across every list for this
    /// architecture, by [`dpkg_compare`].
    available: HashMap<String, DebPackage>,
    /// Status entries whose `Status` ends in "installed".
    installed: HashMap<String, DebPackage>,
    /// apt's answer for these lists and this status file, asked on first
    /// use. The error is kept as its sentence.
    upgrades: OnceLock<std::result::Result<HashMap<String, String>, String>>,
}

/// One stanza of a Packages or status file, reduced to what the store
/// uses. `description` is the long description as paragraphs of plain
/// text; `summary` is the first line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DebPackage {
    pub name: String,
    pub version: String,
    pub architecture: String,
    pub summary: Option<String>,
    pub description: Vec<String>,
    pub homepage: Option<String>,
    pub section: Option<String>,
    /// `Size`: the .deb in bytes.
    pub size: Option<u64>,
    /// `Installed-Size`, converted from the file's kibibytes to bytes.
    pub installed_size: Option<u64>,
    pub maintainer: Option<String>,
    pub depends: Option<String>,
    /// The `Status` line as written: "install ok installed".
    pub status: Option<String>,
    /// "bookworm/main", from the list file's name. `None` in a status file.
    pub repo: Option<String>,
    /// The host the list came from: "deb.debian.org".
    pub origin: Option<String>,
}

impl DebPackage {
    /// dpkg's third status word is "installed" only when the package is
    /// fully there; "config-files", "half-installed" and "unpacked" are not.
    pub fn is_installed(&self) -> bool {
        self.status
            .as_deref()
            .and_then(|s| s.split_whitespace().last())
            .is_some_and(|w| w == "installed")
    }

    /// The first status word is the selection: "hold" means apt-get
    /// upgrade must leave it alone.
    pub fn is_held(&self) -> bool {
        self.status
            .as_deref()
            .and_then(|s| s.split_whitespace().next())
            .is_some_and(|w| w == "hold")
    }

    /// The detail page's key/value list, in drawing order.
    pub fn facts(&self) -> Vec<(String, String)> {
        let mut facts = Vec::new();
        if let Some(section) = &self.section {
            facts.push(("Section".to_string(), section.clone()));
        }
        if let Some(maintainer) = &self.maintainer {
            facts.push(("Maintainer".to_string(), maintainer.clone()));
        }
        if let Some(depends) = &self.depends {
            facts.push(("Depends".to_string(), depends.clone()));
        }
        if let Some(origin) = &self.origin {
            facts.push(("Origin".to_string(), origin.clone()));
        }
        facts
    }
}

/// Parse a file of Debian control stanzas: blank-line separated, one
/// `Field: value` per line, continuation lines indented by a space or a
/// tab. Field names are matched without regard to case, as dpkg does.
/// Fields the store does not use are skipped rather than kept, because a
/// distribution's main list has sixty thousand stanzas of seventeen
/// fields and a generic map of them all is most of the parse time.
pub fn parse_control(text: &str) -> Vec<DebPackage> {
    let mut out = Vec::new();
    let mut cur = DebPackage::default();
    let mut description_lines: Vec<&str> = Vec::new();
    let mut key = Field::Other;
    let mut seen = false;

    let mut finish = |cur: &mut DebPackage, description_lines: &mut Vec<&str>, seen: &mut bool| {
        if *seen && !cur.name.is_empty() {
            cur.description = paragraphs(description_lines);
            out.push(std::mem::take(cur));
        } else {
            *cur = DebPackage::default();
        }
        description_lines.clear();
        *seen = false;
    };

    for line in text.lines() {
        if line.trim().is_empty() {
            finish(&mut cur, &mut description_lines, &mut seen);
            key = Field::Other;
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            match key {
                Field::Description => description_lines.push(line),
                Field::Depends => {
                    if let Some(depends) = cur.depends.as_mut() {
                        depends.push(' ');
                        depends.push_str(line.trim());
                    }
                }
                _ => {}
            }
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        seen = true;
        key = Field::of(name.trim());
        match key {
            Field::Package => cur.name = value.to_string(),
            Field::Version => cur.version = value.to_string(),
            Field::Architecture => cur.architecture = value.to_string(),
            Field::Description => cur.summary = Some(value.to_string()).filter(|s| !s.is_empty()),
            Field::Homepage => cur.homepage = Some(value.to_string()).filter(|s| !s.is_empty()),
            Field::Section => cur.section = Some(value.to_string()).filter(|s| !s.is_empty()),
            Field::Size => cur.size = value.parse().ok(),
            Field::InstalledSize => {
                cur.installed_size = value.parse::<u64>().ok().map(|k| k * 1024)
            }
            Field::Maintainer => cur.maintainer = Some(value.to_string()).filter(|s| !s.is_empty()),
            Field::Depends => cur.depends = Some(value.to_string()).filter(|s| !s.is_empty()),
            Field::Status => cur.status = Some(value.to_string()).filter(|s| !s.is_empty()),
            Field::Other => {}
        }
    }
    finish(&mut cur, &mut description_lines, &mut seen);
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Package,
    Version,
    Architecture,
    Description,
    Homepage,
    Section,
    Size,
    InstalledSize,
    Maintainer,
    Depends,
    Status,
    Other,
}

impl Field {
    fn of(name: &str) -> Field {
        const TABLE: &[(&str, Field)] = &[
            ("Package", Field::Package),
            ("Version", Field::Version),
            ("Architecture", Field::Architecture),
            ("Description", Field::Description),
            ("Homepage", Field::Homepage),
            ("Section", Field::Section),
            ("Size", Field::Size),
            ("Installed-Size", Field::InstalledSize),
            ("Maintainer", Field::Maintainer),
            ("Depends", Field::Depends),
            ("Status", Field::Status),
        ];
        TABLE
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, f)| *f)
            .unwrap_or(Field::Other)
    }
}

/// The continuation lines of a Description into paragraphs, per policy
/// §5.6.13: one leading space is the continuation marker, a lone " ."
/// separates paragraphs, and a line that then still starts with a space
/// is verbatim and keeps its own line. Wrapped lines join with a space.
fn paragraphs(lines: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut last_verbatim = false;
    for raw in lines {
        let line = raw
            .strip_prefix(' ')
            .or_else(|| raw.strip_prefix('\t'))
            .unwrap_or(raw);
        if line == "." {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            last_verbatim = false;
            continue;
        }
        let verbatim = line.starts_with(' ');
        if !cur.is_empty() {
            cur.push(if verbatim || last_verbatim { '\n' } else { ' ' });
        }
        cur.push_str(if verbatim { line } else { line.trim_end() });
        last_verbatim = verbatim;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Paragraphs as the `<p>` markup the page renders, escaped. Plain text
/// would lose the paragraph breaks; this is the one form every
/// description in the store shares.
pub fn description_markup(paragraphs: &[String]) -> String {
    let mut out = String::new();
    for p in paragraphs {
        out.push_str("<p>");
        for c in p.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                c => out.push(c),
            }
        }
        out.push_str("</p>");
    }
    out
}

/// Where a list file came from, decoded from its name. apt writes the
/// source URI with every `/` turned into `_`, so
/// `deb.debian.org_debian_dists_bookworm_main_binary-amd64_Packages` is
/// host `deb.debian.org`, suite `bookworm`, component `main`, arch `amd64`.
/// A flat repository has no `dists` and no `binary-` part.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListName {
    pub host: String,
    pub suite: Option<String>,
    pub component: Option<String>,
    pub arch: Option<String>,
    /// The path parts of a flat repository, for its label.
    pub path: Vec<String>,
    pub gzipped: bool,
}

impl ListName {
    /// `None` for anything that is not a Packages list (Release files,
    /// translations, the `lock` and `partial` entries).
    pub fn parse(file_name: &str) -> Option<ListName> {
        let (stem, gzipped) = match file_name.strip_suffix("_Packages") {
            Some(stem) => (stem, false),
            None => (file_name.strip_suffix("_Packages.gz")?, true),
        };
        let parts: Vec<&str> = stem.split('_').collect();
        let (host, rest) = parts.split_first()?;
        if host.is_empty() {
            return None;
        }
        let mut list = ListName {
            host: host.to_string(),
            gzipped,
            ..ListName::default()
        };
        if let Some(i) = rest.iter().position(|p| *p == "dists") {
            list.suite = rest.get(i + 1).map(|s| s.to_string());
            list.component = rest
                .get(i + 2)
                .filter(|c| !c.starts_with("binary-"))
                .map(|c| c.to_string());
            list.path = rest[..i].iter().map(|p| p.to_string()).collect();
        } else {
            list.path = rest
                .iter()
                .filter(|p| **p != ".")
                .map(|p| p.to_string())
                .collect();
        }
        list.arch = rest
            .iter()
            .find_map(|p| p.strip_prefix("binary-"))
            .map(|a| a.to_string());
        Some(list)
    }

    /// The label a package from this list carries: "bookworm/main", or
    /// the host and path for a flat repository.
    pub fn repo(&self) -> String {
        match (&self.suite, &self.component) {
            (Some(suite), Some(component)) => format!("{suite}/{component}"),
            (Some(suite), None) => suite.clone(),
            _ => {
                if self.path.is_empty() {
                    self.host.clone()
                } else {
                    format!("{}/{}", self.host, self.path.join("/"))
                }
            }
        }
    }

    /// A list for this architecture, or one that does not say (a flat
    /// repository carries every architecture in one file).
    pub fn is_for(&self, arch: &str) -> bool {
        self.arch.as_deref().is_none_or(|a| a == arch)
    }
}

/// Debian's name for a Rust target architecture. Only the two the store
/// targets matter; the rest pass through so a mismatch is visible rather
/// than silently "amd64".
pub fn debian_arch(rust_arch: &str) -> &str {
    match rust_arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "i386",
        "arm" => "armhf",
        "powerpc64" => "ppc64el",
        other => other,
    }
}

/// A list file's text. apt keeps lists uncompressed unless
/// `Acquire::GzipIndexes` is set, in which case they are gzip; xz and lz4
/// lists are not read because the workspace has no decoder for them.
fn read_list(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|e| {
        Error::from_source(
            SourceKind::Apt,
            format!("could not read {}: {e}", path.display()),
        )
    })?;
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes.as_slice())
            .read_to_end(&mut out)
            .map_err(|e| {
                Error::from_source(
                    SourceKind::Apt,
                    format!("{} is not valid gzip: {e}", path.display()),
                )
            })?;
        return Ok(String::from_utf8_lossy(&out).into_owned());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// What the AppStream catalogue adds to a package: the display name, the
/// icon, whether it is an application, and the rest of the metadata a
/// control stanza does not carry.
pub(crate) fn apply_component(pkg: &mut Package, c: &Component) {
    if !c.name.is_empty() {
        pkg.name = c.name.clone();
    }
    if c.is_app {
        pkg.kind = PackageKind::App;
    }
    pkg.appstream_id = Some(c.id.clone());
    if pkg.summary.is_none() {
        pkg.summary = c.summary.clone();
    }
    if pkg.description.is_none() {
        pkg.description = c.description.clone();
    }
    if pkg.homepage.is_none() {
        pkg.homepage = c.homepage.clone();
    }
    if pkg.licence.is_none() {
        pkg.licence = c.licence.clone();
    }
    if pkg.developer.is_none() {
        pkg.developer = c.developer.clone();
    }
    if pkg.icon.is_none() {
        pkg.icon = c.icon.clone();
    }
    if pkg.screenshots.is_empty() {
        pkg.screenshots = c.screenshots.clone();
    }
    if pkg.categories.is_empty() {
        pkg.categories = c.categories.clone();
    }
}

/// A query lowered and split, once per search rather than once per
/// package.
pub(crate) struct SearchWords {
    phrase: String,
    words: Vec<String>,
}

impl SearchWords {
    /// `None` for a blank query: a search for nothing returns nothing
    /// rather than every package.
    pub(crate) fn new(text: &str) -> Option<SearchWords> {
        let words: Vec<String> = text.split_whitespace().map(|w| w.to_lowercase()).collect();
        if words.is_empty() {
            return None;
        }
        Some(SearchWords {
            phrase: words.join(" "),
            words,
        })
    }

    pub(crate) fn words(&self) -> &[String] {
        &self.words
    }
}

/// How well a package matches, higher first, `None` for no match. The
/// same ladder the distribution sources share: the whole query against
/// the name (exact, prefix, anywhere), then every word in the name, then
/// every word somewhere in name and summary, then in the description too.
/// Lives here until a shared helper exists; the dnf source borrows it.
/// The pacman source was written in parallel; if its ladder differs, the
/// shared helper is where the two meet.
pub(crate) fn relevance(
    name: &str,
    summary: Option<&str>,
    paragraphs: &[String],
    query: &SearchWords,
) -> Option<u32> {
    // Hyphens and underscores are the spaces of package names, so "image
    // editor" is a prefix of "image-editor-pro" and "gimp data" is exactly
    // "gimp-data". Only the phrase checks need this; a word never holds a
    // space.
    let key: String = name
        .chars()
        .map(|c| if c == '-' || c == '_' { ' ' } else { c })
        .collect();
    let phrase = query.phrase.as_str();
    if key.eq_ignore_ascii_case(phrase) {
        return Some(100);
    }
    if key.len() >= phrase.len()
        && key.as_bytes()[..phrase.len()].eq_ignore_ascii_case(phrase.as_bytes())
    {
        return Some(90);
    }
    if contains_ci(&key, phrase) {
        return Some(70);
    }
    let words = query.words();
    if words.iter().all(|w| contains_ci(name, w)) {
        return Some(60);
    }
    let in_summary = |w: &str| contains_ci(name, w) || summary.is_some_and(|s| contains_ci(s, w));
    if words.iter().all(|w| in_summary(w)) {
        return Some(40);
    }
    if words
        .iter()
        .all(|w| in_summary(w) || paragraphs.iter().any(|p| contains_ci(p, w)))
    {
        return Some(20);
    }
    None
}

/// ASCII case-insensitive substring test without allocating. `needle`
/// must already be lower case.
pub(crate) fn contains_ci(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let (hay, needle) = (hay.as_bytes(), needle.as_bytes());
    hay.len() >= needle.len()
        && hay
            .windows(needle.len())
            .any(|w| w.eq_ignore_ascii_case(needle))
}

/// `Query.limit` as a count; zero means no limit, so a `Query` built with
/// `Default` still answers.
pub(crate) fn limit(query: &Query) -> usize {
    if query.limit == 0 {
        usize::MAX
    } else {
        query.limit
    }
}

/// apt-get's plain output as a step message for the activity panel:
/// "Unpacking gimp (2.10.34-1) ..." becomes "Unpacking gimp". There is no
/// fraction: apt-get says what it is doing, not how far along it is, and
/// the store never invents one.
pub fn parse_progress(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    const VERBS: &[&str] = &[
        "Unpacking ",
        "Setting up ",
        "Removing ",
        "Purging configuration files for ",
        "Processing triggers for ",
    ];
    for verb in VERBS {
        if let Some(rest) = line.strip_prefix(verb) {
            let name = rest
                .split(" (")
                .next()
                .unwrap_or(rest)
                .trim_end_matches(" ...")
                .trim();
            return Some(format!("{verb}{name}"));
        }
    }
    if let Some(rest) = line.strip_prefix("Preparing to unpack ") {
        // ".../gimp_2.10.34-1_amd64.deb ..." to the package name.
        let file = rest
            .trim_end_matches(" ...")
            .rsplit('/')
            .next()
            .unwrap_or(rest);
        let name = file.split('_').next().unwrap_or(file);
        return Some(format!("Preparing to unpack {name}"));
    }
    if let Some(rest) = line.strip_prefix("Selecting previously unselected package ") {
        return Some(format!("Selecting {}", rest.trim_end_matches('.')));
    }
    if line.starts_with("Get:") {
        // "Get:1 http://host/debian bookworm/main amd64 gimp amd64 2.10.34-1 [5,000 kB]"
        // has the name in the fifth field; an index line
        // "Get:1 http://host/debian bookworm InRelease [151 kB]" names its file.
        let fields: Vec<&str> = line.split_whitespace().collect();
        return match fields.len() {
            n if n >= 8 => Some(format!("Downloading {}", fields[4])),
            n if n >= 4 => Some(format!("Downloading {} {}", fields[2], fields[3])),
            _ => None,
        };
    }
    if let Some(rest) = line.strip_prefix("Fetched ") {
        let mut it = rest.split_whitespace();
        return match (it.next(), it.next()) {
            (Some(n), Some(unit)) => Some(format!("Fetched {n} {unit}")),
            _ => None,
        };
    }
    if line.starts_with("(Reading database") {
        return Some("Reading the package database".to_string());
    }
    for phrase in [
        "Reading package lists",
        "Building dependency tree",
        "Reading state information",
    ] {
        if line.starts_with(phrase) {
            return Some(phrase.to_string());
        }
    }
    None
}

/// The upgrades in `apt-get --simulate` output, as (name, new version).
/// Each is a line `Inst name [installed] (new origin [arch])`; a line with
/// no bracketed installed version is a new package pulled in, not an
/// upgrade. A foreign-architecture package (`libc6:i386`) is left out, as
/// the lists are read for this architecture only.
pub fn parse_simulation(text: &str, arch: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("Inst ")?;
            let (name, rest) = rest.split_once(' ')?;
            let name = match name.split_once(':') {
                Some((base, a)) if a == arch || a == "all" => base,
                Some(_) => return None,
                None => name,
            };
            let rest = rest.trim_start().strip_prefix('[')?;
            let (_, rest) = rest.split_once(']')?;
            let version = rest
                .trim_start()
                .strip_prefix('(')?
                .split_whitespace()
                .next()?;
            Some((name.to_string(), version.to_string()))
        })
        .collect()
}

/// dpkg's version ordering, transcribed from `lib/dpkg/version.c`.
/// `Ordering::Greater` means `a` is newer. A version is
/// `[epoch:]upstream[-revision]`: the epoch is the number before the first
/// colon (0 when absent), the revision everything after the last hyphen
/// (empty when absent, and an empty revision equals "0").
pub fn dpkg_compare(a: &str, b: &str) -> Ordering {
    let (ea, ua, ra) = split_version(a);
    let (eb, ub, rb) = split_version(b);
    ea.cmp(&eb)
        .then_with(|| verrevcmp(ua, ub))
        .then_with(|| verrevcmp(ra, rb))
}

/// `a` is strictly newer than `b` by dpkg's ordering.
pub fn dpkg_is_newer(a: &str, b: &str) -> bool {
    dpkg_compare(a, b) == Ordering::Greater
}

fn split_version(v: &str) -> (u64, &str, &str) {
    let v = v.trim();
    let (epoch, rest) = match v.split_once(':') {
        // dpkg refuses a non-numeric epoch outright; reading it as 0 lets a
        // malformed list entry sort somewhere rather than fail the parse.
        Some((e, rest)) => (e.trim().parse().unwrap_or(0), rest),
        None => (0, v),
    };
    match rest.rsplit_once('-') {
        Some((upstream, revision)) => (epoch, upstream, revision),
        None => (epoch, rest, ""),
    }
}

/// dpkg's `order()`: digits are 0 (they are never compared here), letters
/// are themselves, `~` is below everything, and any other character sits
/// above every letter. The end of the string is 0, which is why `~` sorts
/// before the end and `a` after it.
fn order(c: u8) -> i32 {
    if c.is_ascii_digit() {
        0
    } else if c.is_ascii_alphabetic() {
        c as i32
    } else if c == b'~' {
        -1
    } else if c != 0 {
        c as i32 + 256
    } else {
        0
    }
}

/// dpkg's `verrevcmp()`, byte for byte. Alternating runs: the non-digit
/// prefix compared by `order`, then the digit run compared as a number
/// with leading zeros dropped, repeated until both strings end.
fn verrevcmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let at = |s: &[u8], k: usize| s.get(k).copied().unwrap_or(0);
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() || j < b.len() {
        let mut first_diff = 0i32;
        while (i < a.len() && !at(a, i).is_ascii_digit())
            || (j < b.len() && !at(b, j).is_ascii_digit())
        {
            let (vc, rc) = (order(at(a, i)), order(at(b, j)));
            if vc != rc {
                return vc.cmp(&rc);
            }
            i += 1;
            j += 1;
        }
        while at(a, i) == b'0' {
            i += 1;
        }
        while at(b, j) == b'0' {
            j += 1;
        }
        while at(a, i).is_ascii_digit() && at(b, j).is_ascii_digit() {
            if first_diff == 0 {
                first_diff = at(a, i) as i32 - at(b, j) as i32;
            }
            i += 1;
            j += 1;
        }
        if at(a, i).is_ascii_digit() {
            return Ordering::Greater;
        }
        if at(b, j).is_ascii_digit() {
            return Ordering::Less;
        }
        if first_diff != 0 {
            return first_diff.cmp(&0);
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmp(a: &str, b: &str) -> i8 {
        match dpkg_compare(a, b) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        }
    }

    #[test]
    fn the_policy_manual_s_sort_order_holds() {
        // Policy §5.6.12: "~~", "~~a", "~", the empty part, "a", earliest first.
        let parts = ["1.0~~", "1.0~~a", "1.0~", "1.0", "1.0a"];
        for pair in parts.windows(2) {
            assert_eq!(cmp(pair[0], pair[1]), -1, "{} < {}", pair[0], pair[1]);
        }
    }

    #[test]
    fn an_absent_revision_is_zero_and_an_absent_epoch_too() {
        assert_eq!(cmp("1.0", "1.0-0"), 0);
        assert_eq!(cmp("1.0", "1.0-1"), -1);
        assert_eq!(cmp("0:1.0", "1.0"), 0);
        assert_eq!(cmp("1:1.0", "1.0"), 1);
        assert!(dpkg_is_newer("1:0.1", "9.9"));
    }

    #[test]
    fn letters_sort_before_punctuation_and_leading_zeros_do_not_count() {
        assert_eq!(cmp("1.0a", "1.0+"), -1);
        assert_eq!(cmp("1.01", "1.1"), 0);
        assert_eq!(cmp("1.0-1-1", "1.0-2"), 1);
    }

    #[test]
    fn a_stanza_with_a_wrapped_description_reads_back() {
        let text = "Package: demo\nVersion: 1.0-1\nArchitecture: all\nInstalled-Size: 2\nDepends: libc6 (>= 2.34),\n libfoo\nDescription: one line summary\n First paragraph, wrapped\n across two lines.\n .\n  verbatim line\n  another verbatim\n Back to prose.\nHomepage: https://example.org\n\nPackage: second\nVersion: 2\nArchitecture: amd64\nDescription: short\n";
        let pkgs = parse_control(text);
        assert_eq!(pkgs.len(), 2);
        let demo = &pkgs[0];
        assert_eq!(demo.summary.as_deref(), Some("one line summary"));
        assert_eq!(
            demo.description,
            vec![
                "First paragraph, wrapped across two lines.".to_string(),
                " verbatim line\n another verbatim\nBack to prose.".to_string()
            ]
        );
        assert_eq!(demo.installed_size, Some(2048));
        assert_eq!(demo.depends.as_deref(), Some("libc6 (>= 2.34), libfoo"));
        assert_eq!(pkgs[1].description, Vec::<String>::new());
        assert_eq!(pkgs[1].summary.as_deref(), Some("short"));
    }

    #[test]
    fn field_names_match_without_regard_to_case() {
        let pkgs = parse_control("package: x\nVERSION: 1\narchitecture: all\n");
        assert_eq!(pkgs[0].name, "x");
        assert_eq!(pkgs[0].version, "1");
    }

    #[test]
    fn status_words_decide_installed_and_held() {
        let mut p = DebPackage::default();
        for (status, installed, held) in [
            ("install ok installed", true, false),
            ("hold ok installed", true, true),
            ("deinstall ok config-files", false, false),
            ("install ok half-installed", false, false),
            ("install ok unpacked", false, false),
        ] {
            p.status = Some(status.to_string());
            assert_eq!(p.is_installed(), installed, "{status}");
            assert_eq!(p.is_held(), held, "{status}");
        }
        p.status = None;
        assert!(!p.is_installed());
    }

    #[test]
    fn markup_escapes_what_the_page_must_not_interpret() {
        let m = description_markup(&["a <b> & c".to_string(), "two".to_string()]);
        assert_eq!(m, "<p>a &lt;b&gt; &amp; c</p><p>two</p>");
    }

    #[test]
    fn relevance_ranks_the_name_first() {
        let q = SearchWords::new("GIMP").unwrap();
        assert_eq!(relevance("gimp", None, &[], &q), Some(100));
        assert_eq!(relevance("gimp-data", None, &[], &q), Some(90));
        assert_eq!(relevance("libgimp2.0", None, &[], &q), Some(70));
        assert_eq!(relevance("krita", Some("like GIMP"), &[], &q), Some(40));
        assert_eq!(
            relevance("krita", None, &["as good as gimp".to_string()], &q),
            Some(20)
        );
        assert_eq!(relevance("krita", None, &[], &q), None);
        let two = SearchWords::new("image editor").unwrap();
        assert_eq!(relevance("image-editor-pro", None, &[], &two), Some(90));
        assert_eq!(relevance("image_editor", None, &[], &two), Some(100));
        assert_eq!(relevance("editor-image", None, &[], &two), Some(60));
        assert_eq!(
            relevance(
                "gimp",
                Some("The GNU Image Manipulation Program, an editor"),
                &[],
                &two
            ),
            Some(40)
        );
        assert!(SearchWords::new("   ").is_none());
    }

    #[test]
    fn a_component_lends_its_name_icon_and_kind() {
        let mut pkg = Package::new(SourceKind::Apt, "gimp", "gimp");
        pkg.summary = Some("from the stanza".to_string());
        let c = Component {
            id: "org.gimp.GIMP".to_string(),
            name: "GIMP".to_string(),
            summary: Some("from the catalogue".to_string()),
            is_app: true,
            icon: Some(Picture::File(PathBuf::from("/x/gimp.png"))),
            licence: Some("GPL-3.0-or-later".to_string()),
            categories: vec!["Graphics".to_string()],
            ..Component::default()
        };
        apply_component(&mut pkg, &c);
        assert_eq!(pkg.name, "GIMP");
        assert_eq!(pkg.kind, PackageKind::App);
        assert_eq!(pkg.appstream_id.as_deref(), Some("org.gimp.GIMP"));
        assert_eq!(pkg.summary.as_deref(), Some("from the stanza"));
        assert_eq!(pkg.icon, Some(Picture::File(PathBuf::from("/x/gimp.png"))));
        assert_eq!(pkg.licence.as_deref(), Some("GPL-3.0-or-later"));
        assert_eq!(pkg.categories, vec!["Graphics".to_string()]);
    }
}
