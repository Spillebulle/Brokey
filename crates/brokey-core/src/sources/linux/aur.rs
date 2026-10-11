//! The AUR: search, the foreign packages on this machine, their updates and
//! how a build is carried out.
//!
//! Metadata comes from the aurweb RPC (v5): a `search` by name and
//! description, and an `info` for exact names that adds dependencies,
//! licences and keywords. The RPC returns search results in no useful order,
//! so they are scored here (exact name, name prefix, name contains,
//! description, with a small popularity boost) and trimmed to the query's
//! limit. What is installed is read from the local pacman database: a
//! package that no configured repository carries is foreign, and foreign is
//! what "installed from the AUR" means to pacman too.
//!
//! **A build never runs as root.** `makepkg` refuses to run as root
//! (`E_ROOT`, see `/usr/bin/makepkg`) because a PKGBUILD is arbitrary shell
//! run at build time, and a store that elevated it would be handing every
//! AUR maintainer the machine. So the build is a user-session step, and the
//! pacman calls around it (the system upgrade and the dependencies before,
//! the built package after) are root steps through `brokey-helper` like
//! every other install. The runner keeps one helper for the whole plan, so
//! an AUR install costs one password however long the build takes.
//!
//! Brokey drives makepkg itself rather than handing the job to paru or yay.
//! Both elevate on their own, through `sudo` or whatever `--sudo` names,
//! once for the dependencies and once for the install, which was two or
//! three password prompts per package and a way to root that did not go
//! through the helper's closed list. What they added (AUR dependencies,
//! signing keys, split packages) is done here: [`Aur::resolve`] walks the
//! dependencies through the RPC, the keys a base's `.SRCINFO` names are
//! imported as the user before the build, and only the packages asked for
//! are installed from the build's output.

use super::alpmdb::{self, Desc, LocalDb, SyncDb, is_package_name, read_includes};
use crate::appstream::Catalogue;
use crate::http::Client;
use crate::model::*;
use crate::system::Dirs;
use crate::transaction::allow;
use crate::vercmp::is_newer;
use crate::{Error, Op, Query, Result, Source};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// The aurweb RPC, version 5.
pub const RPC: &str = "https://aur.archlinux.org/rpc/v5";

/// aurweb accepts up to about 200 `arg[]` values on one `info` call before
/// the URL gets too long for its front end; 150 leaves room for long names.
const INFO_BATCH: usize = 150;

/// The suffixes the AUR uses for another packaging of the same software.
/// Stripped when deciding an exact match and when asking the catalogue, so
/// `yay-bin` scores and draws as `yay`.
const EDITION_SUFFIXES: [&str; 3] = ["-bin", "-git", "-appimage"];

/// pacman's version-control suffixes. Their `pkgver` is whatever the last
/// local build produced, so the AUR's copy is usually older, not newer.
const VCS_SUFFIXES: [&str; 4] = ["-git", "-svn", "-hg", "-bzr"];

/// What aurweb answers when a search term matches more than its limit of
/// packages (5000): the term is too common to be searched on its own.
const TOO_MANY_RESULTS: &str = "Too many package results";

/// Answers an RPC URL with the body the AUR would send, or the error the
/// client would report. Tests script the RPC through it; the application
/// asks the network.
type RpcAnswer = dyn Fn(&str) -> Result<String> + Send + Sync;

/// Where the databases and the build cache are. Injectable so the tests
/// can point at a temporary directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub pacman_conf: PathBuf,
    pub local_db: PathBuf,
    pub sync_dir: PathBuf,
    /// Checkouts, and each build's output under `out/`, live here:
    /// `<cache>/aur`, which is under `~/.cache/brokey`, the directory the
    /// helper installs a package file from.
    pub cache: PathBuf,
}

impl Paths {
    pub fn system() -> Paths {
        Paths {
            pacman_conf: PathBuf::from("/etc/pacman.conf"),
            local_db: PathBuf::from(LocalDb::DEFAULT_PATH),
            sync_dir: PathBuf::from("/var/lib/pacman/sync"),
            cache: Dirs::new().cache.join("aur"),
        }
    }
}

/// One package as the RPC describes it. `search` fills the first block;
/// `info` fills the rest as well.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RpcPackage {
    pub name: String,
    pub description: Option<String>,
    pub version: String,
    pub package_base: String,
    #[serde(rename = "URL")]
    pub url: Option<String>,
    #[serde(default)]
    pub num_votes: u64,
    #[serde(default)]
    pub popularity: f64,
    /// Unix seconds when it was flagged, or null.
    pub out_of_date: Option<i64>,
    pub maintainer: Option<String>,
    #[serde(default)]
    pub first_submitted: i64,
    #[serde(default)]
    pub last_modified: i64,
    #[serde(rename = "URLPath")]
    pub url_path: Option<String>,
    #[serde(rename = "ID", default)]
    pub id: u64,
    #[serde(default)]
    pub depends: Vec<String>,
    #[serde(default)]
    pub make_depends: Vec<String>,
    #[serde(default)]
    pub check_depends: Vec<String>,
    #[serde(default)]
    pub opt_depends: Vec<String>,
    #[serde(default)]
    pub license: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub submitter: Option<String>,
    #[serde(default)]
    pub provides: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    results: Vec<RpcPackage>,
}

/// Parse an RPC answer, turning aurweb's `type: "error"` envelope into an
/// error rather than an empty list.
pub fn parse_response(json: &str) -> Result<Vec<RpcPackage>> {
    let resp: RpcResponse = serde_json::from_str(json).map_err(|e| {
        Error::from_source(
            SourceKind::Aur,
            format!("The AUR sent something that was not the expected JSON: {e}."),
        )
    })?;
    if resp.kind == "error" {
        let why = resp
            .error
            .filter(|w| !w.trim().is_empty())
            .unwrap_or_else(|| "it gave no reason".to_string());
        return Err(Error::from_source(
            SourceKind::Aur,
            format!(
                "The AUR refused the request: {}.",
                why.trim().trim_end_matches('.')
            ),
        ));
    }
    Ok(resp.results)
}

/// Whether the error is aurweb's refusal of a term with too many matches.
fn is_too_many_results(e: &Error) -> bool {
    e.message.contains(TOO_MANY_RESULTS)
}

/// The pacman databases, loaded once and reloaded when a file changes.
struct Dbs {
    /// The modification times the load was made from; a different set
    /// means a reload.
    stamp: Vec<(PathBuf, Option<SystemTime>)>,
    local: LocalDb,
    /// Every name a configured repository carries. A local package not in
    /// here is foreign.
    sync_names: HashSet<String>,
    /// Every name a configured repository's packages provide, so a build
    /// dependency on `java-runtime` is installed from there by that name and
    /// pacman picks the provider.
    sync_provides: HashSet<String>,
    /// Every name the local database satisfies: package names and what
    /// they provide, so a dependency on `rust` counts as met by `rustup`.
    satisfied: HashSet<String>,
}

pub struct Aur {
    system: SystemInfo,
    client: Arc<Client>,
    catalogue: Arc<Catalogue>,
    paths: Paths,
    /// A scripted RPC for the tests; `None` asks `client`.
    rpc_answer: Option<Box<RpcAnswer>>,
    dbs: Mutex<Option<Arc<Dbs>>>,
    /// Every package the RPC has described this session, by name. A plan
    /// for a name the page already looked up needs no second round trip,
    /// and the built-in path needs the package base and dependencies.
    known: Mutex<HashMap<String, RpcPackage>>,
}

impl Aur {
    pub fn new(system: &SystemInfo, client: Arc<Client>, catalogue: Arc<Catalogue>) -> Aur {
        Aur {
            system: system.clone(),
            client,
            catalogue,
            paths: Paths::system(),
            rpc_answer: None,
            dbs: Mutex::new(None),
            known: Mutex::new(HashMap::new()),
        }
    }

    /// Answer every RPC URL from a function instead of the network, so a
    /// test can script what aurweb says (a result list, a "too many
    /// results" refusal, a failure) and see which URLs were asked.
    pub fn with_rpc(
        mut self,
        answer: impl Fn(&str) -> Result<String> + Send + Sync + 'static,
    ) -> Aur {
        self.rpc_answer = Some(Box::new(answer));
        self
    }

    pub fn with_paths(mut self, paths: Paths) -> Aur {
        self.paths = paths;
        self
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Remember RPC records so a later plan needs no network.
    pub fn remember(&self, found: &[RpcPackage]) {
        let mut known = self.known.lock().unwrap_or_else(|p| p.into_inner());
        for p in found {
            known.insert(p.name.clone(), p.clone());
        }
    }

    // ---- the RPC -------------------------------------------------------

    fn rpc(&self, url: &str) -> Result<Vec<RpcPackage>> {
        let fetched = match &self.rpc_answer {
            Some(answer) => answer(url),
            None => self.client.get_text(url),
        };
        let text = fetched.map_err(|e| {
            Error::from_source(
                SourceKind::Aur,
                format!(
                    "The AUR did not answer: {}. Check the connection and try again.",
                    e.message
                ),
            )
        })?;
        let found = parse_response(&text)?;
        self.remember(&found);
        Ok(found)
    }

    fn search_rpc(&self, term: &str) -> Result<Vec<RpcPackage>> {
        self.rpc(&format!("{RPC}/search/{}?by=name-desc", urlencode(term)))
    }

    /// `info` for every name, in batches, in one list.
    pub fn info(&self, names: &[&str]) -> Result<Vec<RpcPackage>> {
        let mut names: Vec<&str> = names.to_vec();
        names.sort_unstable();
        names.dedup();
        let mut out = Vec::new();
        for batch in names.chunks(INFO_BATCH) {
            let args: Vec<String> = batch
                .iter()
                .map(|n| format!("arg[]={}", urlencode(n)))
                .collect();
            out.extend(self.rpc(&format!("{RPC}/info?{}", args.join("&")))?);
        }
        Ok(out)
    }

    // ---- the databases -------------------------------------------------

    fn stamp(&self) -> Vec<(PathBuf, Option<SystemTime>)> {
        let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let mut stamp = vec![(self.paths.local_db.clone(), mtime(&self.paths.local_db))];
        let conf = std::fs::read_to_string(&self.paths.pacman_conf).unwrap_or_default();
        for (_, path) in alpmdb::repos(&conf, &self.paths.sync_dir, &read_includes) {
            let t = mtime(&path);
            stamp.push((path, t));
        }
        stamp
    }

    fn dbs(&self) -> Result<Arc<Dbs>> {
        let stamp = self.stamp();
        let mut guard = self.dbs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(d) = guard.as_ref()
            && d.stamp == stamp
        {
            return Ok(d.clone());
        }
        let local = LocalDb::load(&self.paths.local_db).map_err(|e| {
            Error::from_source(
                SourceKind::Aur,
                format!("The pacman database could not be read: {}.", e.message),
            )
        })?;
        let conf = std::fs::read_to_string(&self.paths.pacman_conf).unwrap_or_default();
        let mut sync_names = HashSet::new();
        let mut sync_provides = HashSet::new();
        for (repo, path) in alpmdb::repos(&conf, &self.paths.sync_dir, &read_includes) {
            if !path.exists() {
                continue;
            }
            match SyncDb::load(&repo, &path) {
                Ok(db) => {
                    for (name, desc) in db.packages {
                        sync_provides
                            .extend(desc.all("PROVIDES").iter().map(|p| dep_name(p).to_string()));
                        sync_names.insert(name);
                    }
                }
                Err(e) => log::warn!("skipping {repo}: {e}"),
            }
        }
        let mut satisfied = HashSet::new();
        for desc in local.packages.values() {
            satisfied.insert(desc.name().to_string());
            satisfied.extend(desc.all("PROVIDES").iter().map(|p| dep_name(p).to_string()));
        }
        let dbs = Arc::new(Dbs {
            stamp,
            local,
            sync_names,
            sync_provides,
            satisfied,
        });
        *guard = Some(dbs.clone());
        Ok(dbs)
    }

    /// The local packages no configured repository carries, by name.
    ///
    /// A package Brokey installed from a GitHub release is foreign to
    /// pacman too, but it is the GitHub source's: listing it here would
    /// offer to rebuild it from whatever the AUR has under that name.
    fn foreign(dbs: &Dbs) -> Vec<Desc> {
        let from_github =
            super::github::pacman_names_installed_by_github(&crate::system::Dirs::new().data);
        let mut out: Vec<Desc> = dbs
            .local
            .packages
            .values()
            .filter(|d| !dbs.sync_names.contains(d.name()) && !from_github.contains(d.name()))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name().cmp(b.name()));
        out
    }

    /// Installed, to the AUR source, means installed as a foreign package:
    /// a name a configured repository carries (paru on CachyOS, a kernel
    /// the distribution ships) came from that repository, and its pacman
    /// edition is the one that reports it.
    fn mark_installed(&self, pkg: &mut Package) {
        if let Ok(dbs) = self.dbs()
            && !dbs.sync_names.contains(&pkg.name)
            && let Some(d) = dbs.local.get(&pkg.name)
        {
            pkg.installed = true;
            pkg.installed_version = Some(d.version().to_string());
            if pkg.installed_size.is_none() {
                pkg.installed_size = d.u64("SIZE");
            }
            self.lend_entry(pkg);
            note_vcs(pkg);
        }
    }

    /// An installed AUR package that ships a desktop entry a launcher lists
    /// is an application, whatever the catalogue knows: no catalogue
    /// describes the AUR. The entry lends it a name, an icon and categories,
    /// the way `pacman::apply_entry` does for repository packages.
    fn lend_entry(&self, pkg: &mut Package) {
        if pkg.kind == PackageKind::App && pkg.icon.is_some() {
            return;
        }
        let Some(files) = super::alpmdb::local_files(&self.paths.local_db, &pkg.id) else {
            return;
        };
        let base = base_name(&pkg.id).to_string();
        let roots = crate::launch::icon_roots();
        if let Some(entry) = crate::launch::app_entry(
            files.iter().map(String::as_str),
            std::path::Path::new("/"),
            &[&pkg.id, &base],
            &roots,
        ) {
            super::pacman::apply_entry(pkg, entry);
        }
    }

    // ---- mapping -------------------------------------------------------

    /// An RPC record as a [`Package`], with the catalogue's icon and kind
    /// where the catalogue knows the name.
    pub fn to_package(&self, rpc: &RpcPackage) -> Package {
        let mut p = Package::new(SourceKind::Aur, &rpc.name, &rpc.name);
        p.summary = rpc.description.clone().filter(|s| !s.trim().is_empty());
        p.version = Some(rpc.version.clone());
        p.repo = Some("aur".to_string());
        p.homepage = rpc.url.clone().filter(|s| !s.trim().is_empty());
        p.developer = Some(
            rpc.maintainer
                .clone()
                .unwrap_or_else(|| "orphan".to_string()),
        );
        p.updated = Some(rpc.last_modified);
        p.popularity = Some((rpc.popularity / 50.0).clamp(0.0, 1.0));
        p.popularity_label = Some(votes_label(rpc.num_votes));
        p.out_of_date = rpc.out_of_date.is_some();
        p.licence = if rpc.license.is_empty() {
            None
        } else {
            Some(rpc.license.join(", "))
        };
        p.facts = facts(rpc);
        self.decorate(&mut p);
        p
    }

    /// The catalogue can lend an icon, a kind, pictures and a description
    /// to an AUR package it knows by name, or by the name without its
    /// edition suffix (`yay-bin` draws as `yay`). The AppStream id is lent
    /// only on the exact name: the grouper treats a shared id as a certain
    /// match, and `firefox-bin` being Firefox is the suffix rule's guess,
    /// which the name pass makes with its own confidence and the page says
    /// so. Nothing becomes an application unless the catalogue says so: an
    /// AUR record on its own is a package.
    fn decorate(&self, pkg: &mut Package) {
        let exact = self.catalogue.by_pkgname(&pkg.name);
        let component = exact.or_else(|| self.catalogue.by_pkgname(base_name(&pkg.name)));
        let Some(c) = component else { return };
        pkg.icon = c.icon.clone();
        if c.is_app {
            pkg.kind = PackageKind::App;
        }
        if exact.is_some() {
            pkg.appstream_id = Some(c.id.clone());
        }
        if pkg.categories.is_empty() {
            pkg.categories = c.categories.clone();
        }
        if pkg.description.is_none() {
            pkg.description = c.description.clone();
        }
        if pkg.screenshots.is_empty() {
            pkg.screenshots = c.screenshots.clone();
        }
    }

    /// A foreign package from the local database alone, for when the AUR
    /// does not know it or cannot be asked.
    fn local_package(&self, desc: &Desc) -> Package {
        let name = desc.name();
        let mut p = Package::new(SourceKind::Aur, name, name);
        p.summary = desc.first("DESC").map(str::to_string);
        p.version = Some(desc.version().to_string());
        p.installed = true;
        p.installed_version = Some(desc.version().to_string());
        p.repo = Some("aur".to_string());
        p.homepage = desc.first("URL").map(str::to_string);
        let licence = desc.all("LICENSE");
        p.licence = if licence.is_empty() {
            None
        } else {
            Some(licence.join(", "))
        };
        p.developer = desc.first("PACKAGER").map(str::to_string);
        p.updated = desc.i64("BUILDDATE");
        p.installed_size = desc.u64("SIZE");
        self.decorate(&mut p);
        p
    }

    /// The installed list: every foreign package, with the AUR's record
    /// laid over the local one where the AUR knows the name. `info` is
    /// `None` when the AUR could not be asked, in which case every package
    /// keeps its local data and nothing is called "not in the AUR".
    pub fn installed_from(&self, foreign: &[Desc], info: Option<&[RpcPackage]>) -> Vec<Package> {
        let by_name: HashMap<&str, &RpcPackage> = info
            .unwrap_or(&[])
            .iter()
            .map(|p| (p.name.as_str(), p))
            .collect();
        foreign
            .iter()
            .map(|desc| match by_name.get(desc.name()) {
                Some(rpc) => {
                    let mut p = self.to_package(rpc);
                    p.installed = true;
                    p.installed_version = Some(desc.version().to_string());
                    p.installed_size = desc.u64("SIZE");
                    self.lend_entry(&mut p);
                    note_vcs(&mut p);
                    p
                }
                None => {
                    let mut p = self.local_package(desc);
                    self.lend_entry(&mut p);
                    if info.is_some() {
                        p.repo = Some("local".to_string());
                        p.facts.push((
                            "Origin".to_string(),
                            "Not in the AUR. It was installed from a file or from a repository that is no longer configured.".to_string(),
                        ));
                    }
                    p
                }
            })
            .collect()
    }

    /// Which foreign packages the AUR has a newer version of. `local` is
    /// (name, installed version).
    ///
    /// A version-control package (`-git`, `-svn`, `-hg`, `-bzr`) is listed
    /// only when the AUR's `pkgver` is newer than the local one, the same
    /// rule as everything else. paru's `--devel` (rebuild every VCS package
    /// whose upstream moved) was rejected: it would need a network round
    /// trip per package to answer "are there updates", and "always" is not
    /// an answer the Updates page can draw honestly.
    pub fn updates_from(&self, local: &[(&str, &str)], info: &[RpcPackage]) -> Vec<Update> {
        let by_name: HashMap<&str, &RpcPackage> =
            info.iter().map(|p| (p.name.as_str(), p)).collect();
        let mut out = Vec::new();
        for (name, installed) in local {
            let Some(rpc) = by_name.get(name) else {
                continue;
            };
            if !is_newer(&rpc.version, installed) {
                continue;
            }
            let pkg = self.to_package(rpc);
            out.push(Update {
                package: pkg.reference(),
                name: pkg.name.clone(),
                kind: pkg.kind,
                summary: pkg.summary.clone(),
                icon: pkg.icon.clone(),
                from: Some(installed.to_string()),
                to: rpc.version.clone(),
                download_size: None,
                published: Some(rpc.last_modified),
                is_self: is_self(name),
            });
        }
        out
    }

    /// The foreign packages and, where the AUR answered, their records.
    fn foreign_with_info(&self) -> Result<(Vec<Desc>, Option<Vec<RpcPackage>>)> {
        let dbs = self.dbs()?;
        let foreign = Self::foreign(&dbs);
        if foreign.is_empty() {
            return Ok((foreign, Some(Vec::new())));
        }
        let names: Vec<&str> = foreign.iter().map(Desc::name).collect();
        let info = match self.info(&names) {
            Ok(i) => Some(i),
            Err(e) => {
                log::warn!("the AUR could not be asked about the installed packages: {e}");
                None
            }
        };
        Ok((foreign, info))
    }

    // ---- plans ---------------------------------------------------------

    /// The name a plan step may carry: ours, and shaped like a package
    /// name, so nothing that looks like an option ever reaches paru's or
    /// pacman's argv. The same rule as the pacman source's.
    fn own<'a>(&self, package: &'a PackageRef) -> Result<&'a str> {
        if package.source != SourceKind::Aur {
            return Err(Error::from_source(
                SourceKind::Aur,
                format!(
                    "{} belongs to {}, not to the AUR.",
                    package.id,
                    package.source.label()
                ),
            ));
        }
        if !is_package_name(&package.id) {
            return Err(Error::from_source(
                SourceKind::Aur,
                format!("{} is not a name the AUR accepts.", package.id),
            ));
        }
        Ok(&package.id)
    }

    fn step(
        title: String,
        program: &str,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        needs_root: bool,
        weight: u32,
    ) -> Step {
        Step {
            source: SourceKind::Aur,
            title,
            command: Command {
                program: program.to_string(),
                args,
                env: vec![("LC_ALL".to_string(), "C.UTF-8".to_string())],
                cwd,
            },
            needs_root,
            weight,
        }
    }

    /// Fetch a URL's text the way the RPC is fetched: through the scripted
    /// answer in the tests, through the client otherwise.
    fn fetch_text(&self, url: &str) -> Result<String> {
        match &self.rpc_answer {
            Some(answer) => answer(url),
            None => self.client.get_text(url),
        }
    }

    /// The full record for one name, from `info`: a search record carries
    /// no dependencies, so a cached one will not do here.
    fn described(&self, name: &str) -> Result<Option<RpcPackage>> {
        Ok(self.info(&[name])?.into_iter().find(|p| p.name == name))
    }

    /// The AUR package that satisfies `dep`: one by that name, else the
    /// most voted one that provides it.
    fn aur_provider(&self, dep: &str) -> Result<Option<RpcPackage>> {
        if let Some(p) = self.described(dep)? {
            return Ok(Some(p));
        }
        let found = match self.rpc(&format!("{RPC}/search/{}?by=provides", urlencode(dep))) {
            Ok(found) => found,
            // aurweb answers "no such package" as an empty list, and a term
            // too common to search for is not a provider either.
            Err(e) if is_too_many_results(&e) => Vec::new(),
            Err(e) => return Err(e),
        };
        let best = found
            .into_iter()
            .filter(|p| p.name == dep || p.provides.iter().any(|v| dep_name(v) == dep))
            .max_by(|a, b| {
                a.num_votes
                    .cmp(&b.num_votes)
                    .then_with(|| b.name.cmp(&a.name))
            });
        match best {
            Some(p) => self.described(&p.name),
            None => Ok(None),
        }
    }

    /// What building `targets` takes: the repository packages to install
    /// first, and the AUR package bases to build, dependencies before what
    /// needs them. A dependency the machine already satisfies, by name or
    /// by something installed providing it, is left alone; one a configured
    /// repository carries or provides is installed from there; anything
    /// else must come from the AUR, by name or by what a package there
    /// provides. One that no repository and no AUR package provides is a
    /// sentence before anything runs, rather than makepkg's "Missing
    /// dependencies" twenty minutes into a build.
    pub fn resolve(&self, targets: &[&str]) -> Result<Resolution> {
        let dbs = self.dbs()?;
        let mut walk = Walk {
            dbs: &dbs,
            repo: Vec::new(),
            order: Vec::new(),
            visiting: HashSet::new(),
            planned: HashSet::new(),
        };
        for target in targets {
            let rpc = self.described(target)?.ok_or_else(|| {
                Error::from_source(
                    SourceKind::Aur,
                    format!("{target} is not in the AUR. It may have been deleted or merged into another package."),
                )
            })?;
            self.visit(&mut walk, rpc, false)?;
        }
        let mut builds: Vec<Build> = Vec::new();
        for (rpc, as_dep) in walk.order {
            match builds.iter_mut().find(|b| b.base == rpc.package_base) {
                Some(build) => build.names.push((rpc.name, as_dep)),
                None => builds.push(Build {
                    base: rpc.package_base,
                    names: vec![(rpc.name, as_dep)],
                }),
            }
        }
        Ok(Resolution {
            repo: walk.repo,
            builds,
        })
    }

    fn visit(&self, walk: &mut Walk<'_>, rpc: RpcPackage, as_dep: bool) -> Result<()> {
        if walk.planned.contains(&rpc.name) {
            // Asked for by name after being pulled in as a dependency: the
            // user wants it, so it is installed as explicit.
            if !as_dep && let Some(entry) = walk.order.iter_mut().find(|(p, _)| p.name == rpc.name)
            {
                entry.1 = false;
            }
            return Ok(());
        }
        if !walk.visiting.insert(rpc.name.clone()) {
            // A cycle; makepkg reports it better than a guess here would.
            return Ok(());
        }
        let deps: Vec<String> = rpc
            .depends
            .iter()
            .chain(&rpc.make_depends)
            .chain(&rpc.check_depends)
            .map(|d| dep_name(d).to_string())
            .collect();
        for dep in deps {
            if walk.dbs.satisfied.contains(&dep) || walk.planned.contains(&dep) {
                continue;
            }
            if walk.dbs.sync_names.contains(&dep) || walk.dbs.sync_provides.contains(&dep) {
                if !walk.repo.contains(&dep) {
                    walk.repo.push(dep);
                }
                continue;
            }
            match self.aur_provider(&dep)? {
                Some(provider) => self.visit(walk, provider, true)?,
                None => {
                    return Err(Error::from_source(
                        SourceKind::Aur,
                        format!(
                            "{} needs {dep}, which no configured repository and no AUR package provides, so it cannot be built here.",
                            rpc.name
                        ),
                    ));
                }
            }
        }
        walk.visiting.remove(&rpc.name);
        walk.planned.insert(rpc.name.clone());
        walk.planned
            .extend(rpc.provides.iter().map(|p| dep_name(p).to_string()));
        walk.order.push((rpc, as_dep));
        Ok(())
    }

    /// The PGP keys a package base's sources are signed with, from its
    /// `.SRCINFO`. Empty when the file cannot be fetched: makepkg then says
    /// which key is missing, and the failure sentence names it.
    fn signing_keys(&self, base: &str) -> Vec<String> {
        match self.fetch_text(&srcinfo_url(base)) {
            Ok(text) => validpgpkeys(&text),
            Err(e) => {
                log::warn!(".SRCINFO for {base} could not be fetched: {e}");
                Vec::new()
            }
        }
    }

    /// The steps that install or update `targets` and everything they need.
    ///
    /// Every step that needs root goes through the helper, and the runner
    /// keeps one helper for the whole plan, so this costs one password
    /// however long the builds take. Nothing here calls pkexec, sudo, paru
    /// or yay: makepkg builds as the user, with no `-s` and no `-i`, which
    /// are the two flags that would make it elevate on its own.
    ///
    /// 1. The system is brought up to date, with base-devel and git if they
    ///    are missing: Arch supports no partial upgrade, and installing
    ///    dependencies from databases that are weeks old is one.
    /// 2. The repository dependencies, as dependencies.
    /// 3. For each package base, dependencies first: fetch it, import the
    ///    keys its sources are signed with, build it as the user into a
    ///    directory of its own, install the packages asked for from there
    ///    (only those, not every member of a split base, and never the
    ///    `-debug` package), then remove the build output.
    pub fn build_steps(&self, targets: &[&str]) -> Result<Vec<Step>> {
        let plan = self.resolve(targets)?;
        let dbs = self.dbs()?;
        let mut steps = Vec::new();
        let tools: Vec<&str> = BUILD_TOOLS
            .iter()
            .copied()
            .filter(|t| !dbs.satisfied.contains(*t))
            .collect();
        let mut args = vec![
            "-Syu".to_string(),
            "--noconfirm".to_string(),
            "--needed".to_string(),
        ];
        args.extend(tools.iter().map(|t| t.to_string()));
        steps.push(Self::step(
            if tools.is_empty() {
                "Updating the system before building".to_string()
            } else {
                format!(
                    "Updating the system and installing {} before building",
                    join_and(&tools)
                )
            },
            "pacman",
            args,
            None,
            true,
            3,
        ));
        if !plan.repo.is_empty() {
            let mut args = vec![
                "-S".to_string(),
                "--noconfirm".to_string(),
                "--needed".to_string(),
                "--asdeps".to_string(),
            ];
            args.extend(plan.repo.iter().cloned());
            steps.push(Self::step(
                "Installing what the build needs".to_string(),
                "pacman",
                args,
                None,
                true,
                3,
            ));
        }
        for build in &plan.builds {
            steps.extend(self.base_steps(build));
        }
        Ok(steps)
    }

    fn base_steps(&self, build: &Build) -> Vec<Step> {
        let base = &build.base;
        let dir = self.paths.cache.join(base);
        let out = self.paths.cache.join("out").join(base);
        let mut steps = Vec::new();
        // A checkout from an earlier build is updated rather than cloned
        // again; its downloaded sources are most of what a rebuild would
        // fetch. Reset rather than pulled: makepkg writes a VCS package's
        // new pkgver back into the tracked PKGBUILD, and `pull --ff-only`
        // refuses a dirty checkout.
        if dir.join(".git").is_dir() {
            let dir = dir.display().to_string();
            steps.push(Self::step(
                format!("Fetching {base}"),
                "git",
                vec![
                    "-C".to_string(),
                    dir.clone(),
                    "fetch".to_string(),
                    "--depth".to_string(),
                    "1".to_string(),
                    "origin".to_string(),
                ],
                None,
                false,
                1,
            ));
            steps.push(Self::step(
                format!("Updating the checkout of {base}"),
                "git",
                vec![
                    "-C".to_string(),
                    dir,
                    "reset".to_string(),
                    "--hard".to_string(),
                    "FETCH_HEAD".to_string(),
                ],
                None,
                false,
                1,
            ));
        } else {
            steps.push(Self::step(
                format!("Fetching {base}"),
                "git",
                vec![
                    "clone".to_string(),
                    "--depth".to_string(),
                    "1".to_string(),
                    format!("https://aur.archlinux.org/{base}.git"),
                    dir.display().to_string(),
                ],
                None,
                false,
                1,
            ));
        }
        let keys = self.signing_keys(base);
        if !keys.is_empty() {
            // Only the keys the keyring lacks are fetched, so a keyserver
            // that is down costs nothing once they are there. The keys are
            // hexadecimal fingerprints (`validpgpkeys` checks that) and are
            // passed as arguments, never put into the script.
            let mut args = vec!["-c".to_string(), IMPORT_KEYS.to_string(), "sh".to_string()];
            args.extend(keys);
            steps.push(Self::step(
                format!("Importing the keys {base} is signed with"),
                "sh",
                args,
                None,
                false,
                1,
            ));
        }
        let mut make = Self::step(
            format!("Building {base}"),
            "makepkg",
            vec![
                "--force".to_string(),
                "--clean".to_string(),
                "--noconfirm".to_string(),
            ],
            Some(dir),
            false,
            8,
        );
        // makepkg reads PKGDEST from the environment before its
        // configuration, so this wins over a PKGDEST in makepkg.conf.
        make.command
            .env
            .push(("PKGDEST".to_string(), out.display().to_string()));
        steps.push(make);
        for as_dep in [false, true] {
            let names: Vec<&str> = build
                .names
                .iter()
                .filter(|(_, d)| *d == as_dep)
                .map(|(n, _)| n.as_str())
                .collect();
            if names.is_empty() {
                continue;
            }
            let mut args = vec!["-U".to_string(), "--noconfirm".to_string()];
            if as_dep {
                args.push("--asdeps".to_string());
            }
            args.extend(names.iter().map(|n| allow::built_selector(&out, n)));
            steps.push(Self::step(
                format!("Installing {}", join_and(&names)),
                "pacman",
                args,
                None,
                true,
                2,
            ));
        }
        steps.push(Self::step(
            format!("Removing the build output of {base}"),
            "rm",
            vec![
                "-rf".to_string(),
                "--".to_string(),
                out.display().to_string(),
            ],
            None,
            false,
            1,
        ));
        steps
    }

    fn update_all_steps(&self) -> Result<Vec<Step>> {
        let names: Vec<String> = self.updates()?.into_iter().map(|u| u.package.id).collect();
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        self.build_steps(&names)
    }
}

/// What [`Aur::resolve`] found: what to install from the repositories, and
/// what to build, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    pub repo: Vec<String>,
    pub builds: Vec<Build>,
}

/// One package base to build, with the names to install from it and
/// whether each is installed as a dependency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Build {
    pub base: String,
    pub names: Vec<(String, bool)>,
}

struct Walk<'a> {
    dbs: &'a Dbs,
    repo: Vec<String>,
    order: Vec<(RpcPackage, bool)>,
    visiting: HashSet<String>,
    planned: HashSet<String>,
}

/// What every build needs and a fresh Arch install may not have.
const BUILD_TOOLS: [&str; 2] = ["base-devel", "git"];

/// Fetches whichever of the keys in `$@` the user's keyring lacks.
const IMPORT_KEYS: &str = "missing=''; for k in \"$@\"; do gpg --batch --list-keys \"$k\" >/dev/null 2>&1 || missing=\"$missing $k\"; done; [ -z \"$missing\" ] || gpg --batch --recv-keys $missing";

/// The `validpgpkeys` of a `.SRCINFO`, kept only when they are the
/// hexadecimal fingerprints makepkg requires.
pub fn validpgpkeys(srcinfo: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in srcinfo.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        if key.trim() != "validpgpkeys" {
            continue;
        }
        let value = value.trim().to_ascii_uppercase();
        if (16..=40).contains(&value.len())
            && value.chars().all(|c| c.is_ascii_hexdigit())
            && !out.contains(&value)
        {
            out.push(value);
        }
    }
    out
}

fn srcinfo_url(base: &str) -> String {
    format!(
        "https://aur.archlinux.org/cgit/aur.git/plain/.SRCINFO?h={}",
        urlencode(base)
    )
}

/// "a", "a and b", "a, b and c".
fn join_and(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

impl Source for Aur {
    fn kind(&self) -> SourceKind {
        SourceKind::Aur
    }

    fn status(&self) -> SourceStatus {
        let unavailable = |reason: &str| SourceStatus {
            kind: SourceKind::Aur,
            available: false,
            reason: Some(reason.to_string()),
            detail: None,
            searchable: false,
            setup: None,
        };
        if !self.system.is_arch_like() {
            return unavailable("The AUR needs an Arch-based system.");
        }
        // makepkg comes with pacman, and base-devel and git are installed by
        // the first build that needs them, so an Arch system can always
        // build.
        SourceStatus {
            kind: SourceKind::Aur,
            available: true,
            reason: None,
            detail: Some("makepkg".to_string()),
            searchable: false,
            setup: None,
        }
    }

    /// The RPC matches one substring, so it is asked for one word at a
    /// time, longest first (the most selective), and `rank` checks the
    /// rest here. aurweb refuses a word that matches more than 5000
    /// packages ("python", "lib", "git"); such a word is passed over for
    /// the next, and only when every word is that common is the query
    /// refused, with what to do about it.
    fn search(&self, query: &Query) -> Result<Vec<Package>> {
        let mut words: Vec<String> = query
            .text
            .split_whitespace()
            .map(|w| w.to_lowercase())
            .collect();
        words.sort_by_key(|w| std::cmp::Reverse(w.chars().count()));
        words.dedup();
        // aurweb refuses a term under two characters; an error toast on the
        // first keystroke is worse than an empty list.
        words.retain(|w| w.chars().count() >= 2);
        let mut found = None;
        for term in &words {
            match self.search_rpc(term) {
                Ok(list) => {
                    found = Some(list);
                    break;
                }
                Err(e) if is_too_many_results(&e) => {
                    log::debug!("aur: {term} matches too many packages, trying the next word");
                }
                Err(e) => return Err(e),
            }
        }
        let Some(found) = found else {
            if words.is_empty() {
                return Ok(Vec::new());
            }
            return Err(Error::from_source(
                SourceKind::Aur,
                format!(
                    "The AUR has too many packages matching {}. Add another word to narrow the search.",
                    query.text.trim()
                ),
            ));
        };
        let ranked = rank(&query.text, found, query.limit);
        let mut out = Vec::with_capacity(ranked.len());
        for rpc in &ranked {
            let mut p = self.to_package(rpc);
            self.mark_installed(&mut p);
            out.push(p);
        }
        Ok(out)
    }

    fn installed(&self) -> Result<Vec<Package>> {
        let (foreign, info) = self.foreign_with_info()?;
        Ok(self.installed_from(&foreign, info.as_deref()))
    }

    fn updates(&self) -> Result<Vec<Update>> {
        let dbs = self.dbs()?;
        let foreign = Self::foreign(&dbs);
        if foreign.is_empty() {
            return Ok(Vec::new());
        }
        let names: Vec<&str> = foreign.iter().map(Desc::name).collect();
        let info = self.info(&names)?;
        let local: Vec<(&str, &str)> = foreign.iter().map(|d| (d.name(), d.version())).collect();
        Ok(self.updates_from(&local, &info))
    }

    fn details(&self, id: &str) -> Result<Package> {
        let rpc = self
            .info(&[id])?
            .into_iter()
            .find(|p| p.name == id)
            .ok_or_else(|| Error::from_source(SourceKind::Aur, format!("{id} is not in the AUR. It may have been deleted or merged into another package.")))?;
        let mut pkg = self.to_package(&rpc);
        self.mark_installed(&mut pkg);
        // The PKGBUILD is what a careful user reads before building. A
        // failed fetch costs the detail page one block, not the page.
        match self.client.get_text(&pkgbuild_url(&rpc.package_base)) {
            Ok(text) => pkg.facts.push(("PKGBUILD".to_string(), text)),
            Err(e) => log::warn!(
                "PKGBUILD for {} could not be fetched: {e}",
                rpc.package_base
            ),
        }
        Ok(pkg)
    }

    /// An AUR package is installed by pacman, so its desktop entry is in
    /// pacman's record of its files, found the same way the pacman source
    /// finds one. The name without its -bin, -git or -appimage suffix is
    /// also tried, since that is what upstream names the entry.
    fn launcher(&self, id: &str) -> Option<crate::launch::Launch> {
        let files = super::alpmdb::local_files(&self.paths.local_db, id)?;
        let base = base_name(id);
        crate::launch::from_files(
            files.iter().map(String::as_str),
            std::path::Path::new("/"),
            &[id, base],
        )
    }

    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        match op {
            Op::Install { package } | Op::Update { package } => {
                let name = self.own(package)?;
                self.build_steps(&[name])
            }
            Op::Remove { package } => {
                let name = self.own(package)?;
                Ok(vec![Self::step(
                    format!("Removing {name}"),
                    "pacman",
                    vec![
                        "-Rs".to_string(),
                        "--noconfirm".to_string(),
                        name.to_string(),
                    ],
                    None,
                    true,
                    2,
                )])
            }
            // The planner expands a setup through `Source::setup`.
            Op::Refresh { .. } | Op::Setup { .. } => Ok(Vec::new()),
            Op::UpdateAll { .. } => self.update_all_steps(),
        }
    }
}

/// What an installed version-control package's page says, since its
/// version is whatever the last local build produced and the AUR's
/// `pkgver` is usually older: Brokey cannot tell when upstream has new
/// commits, so it says so instead of letting "no updates" stand.
pub const VCS_NOTE: &str = "Brokey cannot tell when this package has new commits upstream, because its version comes from the last build. Rebuild it to get the latest.";

fn note_vcs(pkg: &mut Package) {
    if is_vcs(&pkg.name) {
        pkg.facts
            .push(("Updates".to_string(), VCS_NOTE.to_string()));
    }
}

fn pkgbuild_url(base: &str) -> String {
    format!(
        "https://aur.archlinux.org/cgit/aur.git/plain/PKGBUILD?h={}",
        urlencode(base)
    )
}

pub fn is_self(name: &str) -> bool {
    name == "brokey" || name == "brokey-bin"
}

fn votes_label(votes: u64) -> String {
    if votes == 1 {
        "1 vote".to_string()
    } else {
        format!("{votes} votes")
    }
}

/// The detail page's key/value list, in the order it is drawn.
fn facts(rpc: &RpcPackage) -> Vec<(String, String)> {
    let mut f: Vec<(String, String)> = Vec::new();
    let mut push = |k: &str, v: String| f.push((k.to_string(), v));
    push(
        "Maintainer",
        rpc.maintainer
            .clone()
            .unwrap_or_else(|| "none (orphaned)".to_string()),
    );
    push("Votes", rpc.num_votes.to_string());
    push("Popularity", format!("{:.2}", rpc.popularity));
    push("Package base", rpc.package_base.clone());
    push("First submitted", date(rpc.first_submitted));
    push("Last modified", date(rpc.last_modified));
    if let Some(since) = rpc.out_of_date {
        push("Out of date since", date(since));
    }
    if !rpc.license.is_empty() {
        push("Licence", rpc.license.join(", "));
    }
    if !rpc.depends.is_empty() {
        push("Depends", rpc.depends.join(", "));
    }
    if !rpc.make_depends.is_empty() {
        push("Make depends", rpc.make_depends.join(", "));
    }
    if !rpc.opt_depends.is_empty() {
        push("Opt depends", rpc.opt_depends.join(", "));
    }
    push(
        "AUR page",
        format!("https://aur.archlinux.org/packages/{}", rpc.name),
    );
    f
}

/// The name without its `-bin`, `-git` or `-appimage` suffix, repeatedly,
/// so `foo-git-bin` is `foo` too. `steam-native-runtime` is left alone.
pub fn base_name(name: &str) -> &str {
    let mut n = name;
    loop {
        let Some(stripped) = EDITION_SUFFIXES.iter().find_map(|s| n.strip_suffix(s)) else {
            return n;
        };
        if stripped.is_empty() {
            return n;
        }
        n = stripped;
    }
}

pub fn is_vcs(name: &str) -> bool {
    VCS_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// How well a record matches the query, 0 to 1.3: the text score (exact
/// name 1.0, name prefix 0.9, name contains 0.7, description 0.4) plus a
/// popularity boost of `log10(1 + popularity) / 3` capped at 0.3. The cap
/// means a hugely popular prefix match can outrank an exact match with no
/// votes, which is what a user typing "proton" wants.
pub fn score(term: &str, name: &str, description: Option<&str>, popularity: f64) -> f64 {
    let words: Vec<String> = term.split_whitespace().map(|w| w.to_lowercase()).collect();
    let joined = words.join("-");
    let spaced = words.join(" ");
    let n = name.to_lowercase();
    let text = if n == joined || base_name(&n) == joined {
        1.0
    } else if n.starts_with(&joined) {
        0.9
    } else if n.contains(&joined) {
        0.7
    } else if description.is_some_and(|d| {
        let d = d.to_lowercase();
        d.contains(&joined) || d.contains(&spaced)
    }) {
        0.4
    } else {
        0.0
    };
    text + ((1.0 + popularity.max(0.0)).log10() / 3.0).min(0.3)
}

/// Keep the records where every word of the query appears in the name or
/// the description, best score first, at most `limit`. The RPC was asked
/// for one word; this is where the others are checked.
pub fn rank(term: &str, found: Vec<RpcPackage>, limit: usize) -> Vec<RpcPackage> {
    let words: Vec<String> = term.split_whitespace().map(|w| w.to_lowercase()).collect();
    let mut scored: Vec<(f64, RpcPackage)> = found
        .into_iter()
        .filter(|p| {
            let hay =
                format!("{} {}", p.name, p.description.as_deref().unwrap_or("")).to_lowercase();
            words.iter().all(|w| hay.contains(w.as_str()))
        })
        .map(|p| {
            (
                score(term, &p.name, p.description.as_deref(), p.popularity),
                p,
            )
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.name.cmp(&b.1.name))
    });
    scored.truncate(limit);
    scored.into_iter().map(|(_, p)| p).collect()
}

/// `libalpm.so>=14` is a dependency on `libalpm.so`.
fn dep_name(dep: &str) -> &str {
    dep.split(['<', '>', '=', ':']).next().unwrap_or(dep).trim()
}

/// Percent-encode everything but RFC 3986's unreserved characters.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Unix seconds as `YYYY-MM-DD` in UTC (Howard Hinnant's civil-from-days).
/// The style guide wants a date past a day's age; the AUR's timestamps are
/// months and years old.
pub fn date(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_utc_calendar_days() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_603_068_230), "2020-10-19");
        assert_eq!(date(1_765_515_783), "2025-12-12");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(-86_400), "1969-12-31");
    }

    #[test]
    fn url_encoding_keeps_unreserved_and_encodes_the_rest() {
        assert_eq!(urlencode("steam"), "steam");
        assert_eq!(urlencode("gpu screen"), "gpu%20screen");
        assert_eq!(urlencode("c++/x~y"), "c%2B%2B%2Fx~y");
    }

    #[test]
    fn an_error_envelope_is_an_error() {
        let e = parse_response(r#"{"version":5,"type":"error","resultcount":0,"results":[],"error":"Query arg too small."}"#).unwrap_err();
        assert_eq!(
            e.message,
            "The AUR refused the request: Query arg too small."
        );
        assert_eq!(e.source_kind, Some(SourceKind::Aur));
        assert!(parse_response("not json").is_err());
        // The sentence closes whatever aurweb sends: no reason, or one
        // without its full stop.
        let e = parse_response(r#"{"version":5,"type":"error"}"#).unwrap_err();
        assert_eq!(e.message, "The AUR refused the request: it gave no reason.");
        let e =
            parse_response(r#"{"type":"error","error":"Too many package results"}"#).unwrap_err();
        assert_eq!(
            e.message,
            "The AUR refused the request: Too many package results."
        );
        assert!(is_too_many_results(&e));
        let e = parse_response(r#"{"type":"error","error":"  "}"#).unwrap_err();
        assert!(e.message.ends_with("it gave no reason."));
    }

    #[test]
    fn a_search_envelope_parses_with_only_search_fields() {
        let found = parse_response(
            r#"{"version":5,"type":"search","resultcount":1,"results":[{"Description":null,"FirstSubmitted":1,"ID":2,"LastModified":3,"Maintainer":null,"Name":"x","NumVotes":0,"OutOfDate":null,"PackageBase":"x","PackageBaseID":4,"Popularity":0,"URL":null,"URLPath":"/p","Version":"1-1"}]}"#,
        )
        .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "x");
        assert!(found[0].depends.is_empty());
        assert!(found[0].maintainer.is_none());
    }

    #[test]
    fn dependency_names_lose_their_constraints() {
        assert_eq!(dep_name("libalpm.so>=14"), "libalpm.so");
        assert_eq!(dep_name("python>=3.12"), "python");
        assert_eq!(dep_name("gtk3"), "gtk3");
        assert_eq!(dep_name("foo=1.0"), "foo");
    }

    #[test]
    fn votes_read_as_english() {
        assert_eq!(votes_label(0), "0 votes");
        assert_eq!(votes_label(1), "1 vote");
        assert_eq!(votes_label(1257), "1257 votes");
    }
}
