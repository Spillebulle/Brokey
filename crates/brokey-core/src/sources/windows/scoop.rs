//! Scoop: installed applications read from `apps\<name>\current\manifest.json`
//! under `%SCOOP%` (or `~\scoop` when that is unset), and search against the
//! main bucket, on disk when Scoop is installed and from GitHub when it is
//! not.
//!
//! A manifest's fields do not have one shape each: `license` is a plain
//! string in one real manifest and an object carrying `identifier` in
//! another, and `bin` is a list in one, absent in another, and documented as
//! also being a plain string or a list of lists (`[["path", "alias"]]`).
//! [`Manifest::licence`] and [`Manifest::bin_names`] read every shape, and
//! never `unwrap` on which one a given manifest happens to use.
//!
//! **Nothing here ever needs Administrator.** Scoop's whole point is that it
//! installs into the user's own profile rather than `C:\Program Files`, so
//! [`operation_step`] never sets `needs_root`; a step of this source asking
//! for it would be a defect, not a setting. This is also why no Scoop step
//! ever reaches `transaction/allow.rs`'s closed list: that list exists to
//! bound what an elevated step may run, and nothing here elevates.
//!
//! `Source::setup` answers `None` here on purpose; see its doc comment.

use crate::http::Client;
use crate::model::{Command, Op, Package, SourceKind, SourceStatus, Step};
use crate::{Error, Query, Result, Setup, Source, Update};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// `%SCOOP%`, or `~\scoop` when that is unset. Scoop itself respects the
/// `SCOOP` environment variable; a user who has never set it gets everything
/// under their own profile, because that is where Scoop's own installer puts
/// it.
pub fn install_root() -> PathBuf {
    std::env::var("SCOOP")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join("scoop"))
}

fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\Users\Default"))
}

/// Where `scoop` is, if it is anywhere on `PATH`. Scoop's own installer adds
/// its shims directory to `PATH`, so this is enough; unlike `choco.exe` and
/// `winget.exe`, there is no second, fixed place to fall back to worth
/// guessing at.
pub fn scoop_exe() -> Option<PathBuf> {
    crate::system::which("scoop")
}

/// The program a step names: the full path to Scoop's own shim when it can
/// be found, the bare name otherwise. Resolution happens here, in the
/// unelevated process, the same as `choco_program` and `winget_program`, and
/// for the same reason: the elevated helper must never search for a program
/// itself. Unlike Chocolatey's and winget's steps, nothing this resolves is
/// ever handed to the helper: see the module doc comment.
pub fn scoop_program() -> String {
    scoop_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "scoop".to_string())
}

pub const NOT_INSTALLED: &str = "Scoop is not installed, so nothing can be installed, updated \
     or removed through it. Its main bucket answers over HTTP, so Brokey still searches it.";

/// A manifest's fields, the shape scoop.rs actually reads. Scoop manifests
/// carry far more (`architecture`, `checkver`, `autoupdate`, `notes`,
/// `persist`, `post_install`, `suggest`...); none of that is read here,
/// because none of it is anything the store draws or acts on.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    license: Option<LicenseField>,
    #[serde(default)]
    bin: Option<BinField>,
}

/// `license` as the manifest schema documents it: a plain string
/// (`nodejs.json`'s `"MIT"`), or an object whose `identifier` is the
/// licence's name (`7zip.json`'s
/// `{"identifier": "BSD-2-Clause, ...", "url": "..."}`). `serde`'s untagged
/// matching tries `Name` first and only falls to `Identifier` when the value
/// is not a bare string, so a manifest never has to say which shape it used.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
enum LicenseField {
    Name(String),
    Identifier { identifier: String },
}

/// `bin` as the manifest schema documents it: a single executable, a list of
/// executables, or a list in which any entry may itself be `[path, alias]`
/// or `[path, alias, arguments]`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
enum BinField {
    One(String),
    Many(Vec<BinEntry>),
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
enum BinEntry {
    Name(String),
    Aliased(Vec<String>),
}

impl Manifest {
    /// The licence text, whichever shape the manifest wrote it in. `None`
    /// when the manifest carries no `license` field at all.
    pub fn licence(&self) -> Option<String> {
        self.license.as_ref().map(|l| match l {
            LicenseField::Name(s) => s.clone(),
            LicenseField::Identifier { identifier } => identifier.clone(),
        })
    }

    /// The executables `bin` names, whichever of the three documented shapes
    /// it was written in. Only the path of an aliased entry is kept; the
    /// alias and any arguments after it are dropped, because nothing here
    /// runs anything by its alias. Empty, not an error, when `bin` is absent
    /// (`nodejs.json` has none).
    pub fn bin_names(&self) -> Vec<String> {
        match &self.bin {
            None => Vec::new(),
            Some(BinField::One(s)) => vec![s.clone()],
            Some(BinField::Many(entries)) => entries
                .iter()
                .filter_map(|e| match e {
                    BinEntry::Name(s) => Some(s.clone()),
                    BinEntry::Aliased(parts) => parts.first().cloned(),
                })
                .collect(),
        }
    }
}

fn manifest_json_error(e: &serde_json::Error) -> Error {
    Error::from_source(
        SourceKind::Scoop,
        format!("This manifest is not valid JSON: {e}."),
    )
}

/// Parse one manifest's bytes.
pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest> {
    serde_json::from_slice(bytes).map_err(|e| manifest_json_error(&e))
}

/// One manifest's `Package`, from its id (the manifest's own file name,
/// without `.json`, which is also the name Scoop and its buckets know it
/// by) and its parsed fields. `installed` stays `false`; [`to_installed_package`]
/// and [`joined`] are what set it, the same split as
/// `choco::to_package_from_nuspec` and `choco::joined`.
pub fn to_package(id: &str, m: &Manifest) -> Package {
    let mut p = Package::new(SourceKind::Scoop, id.to_string(), id.to_string());
    p.version = Some(m.version.clone());
    p.description = m.description.clone();
    p.homepage = m.homepage.clone();
    p.licence = m.licence();
    let bins = m.bin_names();
    if !bins.is_empty() {
        p.facts.push(("Executables".to_string(), bins.join(", ")));
    }
    p
}

/// An installed app's `Package`, read from its `current\manifest.json`.
pub fn to_installed_package(id: &str, m: &Manifest) -> Package {
    let mut p = to_package(id, m);
    p.installed = true;
    p.installed_version = Some(m.version.clone());
    p
}

/// The list of manifest names in ScoopInstaller/Main's bucket, from GitHub's
/// git tree API. `sha` is the `bucket` directory's own tree entry, out of the
/// repository's root tree; `sha` names that tree in turn, whose entries are
/// the manifests themselves.
#[derive(Clone, Debug, Deserialize)]
struct TreeEntry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Tree {
    tree: Vec<TreeEntry>,
}

fn tree_json_error(e: &serde_json::Error) -> Error {
    Error::from_source(
        SourceKind::Scoop,
        format!("GitHub did not answer with the tree Brokey expected: {e}."),
    )
}

fn parse_tree(bytes: &[u8]) -> Result<Tree> {
    serde_json::from_slice(bytes).map_err(|e| tree_json_error(&e))
}

/// The `bucket` directory's own sha, out of the repository's root tree. `None`
/// when there is no such entry, which would mean GitHub reorganised the
/// repository under Brokey.
fn bucket_sha(master: &Tree) -> Option<&str> {
    master
        .tree
        .iter()
        .find(|e| e.path == "bucket" && e.kind == "tree")
        .map(|e| e.sha.as_str())
}

/// The manifest names out of the bucket's own tree: every blob whose path
/// ends `.json`, with that suffix removed. A non-`.json` blob (there is a
/// `README.md` in the real bucket) and a nested tree entry are both left out
/// by the `.json` blob filter.
fn manifest_names(bucket: &Tree) -> Vec<String> {
    bucket
        .tree
        .iter()
        .filter(|e| e.kind == "blob")
        .filter_map(|e| e.path.strip_suffix(".json"))
        .map(str::to_string)
        .collect()
}

pub const MASTER_TREE_URL: &str =
    "https://api.github.com/repos/ScoopInstaller/Main/git/trees/master";

pub fn bucket_tree_url(sha: &str) -> String {
    format!("https://api.github.com/repos/ScoopInstaller/Main/git/trees/{sha}")
}

pub fn manifest_url(name: &str) -> String {
    format!("https://raw.githubusercontent.com/ScoopInstaller/Main/master/bucket/{name}.json")
}

/// How long a cached answer (the bucket's name list, or one manifest) is
/// trusted before it is fetched again. The same length as
/// `winget::index::MAX_AGE`, for the same reason: this is Brokey's own
/// snapshot of somebody else's catalogue, not a live query.
const CACHE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Every manifest name in ScoopInstaller/Main, fetched (and cached) as two
/// requests: the root tree, to find the `bucket` directory's own sha, then
/// that tree, whose entries are the manifests.
fn main_bucket_names(client: &Client) -> Result<Vec<String>> {
    let master_text = client.get_text_cached(MASTER_TREE_URL, CACHE_MAX_AGE)?;
    let master = parse_tree(master_text.as_bytes())?;
    let sha = bucket_sha(&master).ok_or_else(|| {
        Error::from_source(
            SourceKind::Scoop,
            "GitHub's tree for ScoopInstaller/Main has no bucket directory, so Brokey cannot \
             search Scoop's main bucket."
                .to_string(),
        )
    })?;
    let bucket_text = client.get_text_cached(&bucket_tree_url(sha), CACHE_MAX_AGE)?;
    let bucket = parse_tree(bucket_text.as_bytes())?;
    Ok(manifest_names(&bucket))
}

/// One manifest, fetched (and cached) by name. `None` on any failure: a name
/// that is not in Main answers 404 with an HTML body, which is neither valid
/// JSON nor a reason to fail the whole search, so it is skipped the same way
/// a network error or an unreadable local file is.
fn fetch_manifest(client: &Client, name: &str) -> Option<Manifest> {
    let text = client
        .get_text_cached(&manifest_url(name), CACHE_MAX_AGE)
        .ok()?;
    parse_manifest(text.as_bytes()).ok()
}

/// Case-insensitive substring match of `query` against a list of manifest
/// names, whether the names came off disk or off GitHub's tree. Scoop bucket
/// names are the package ids themselves (`7zip`, `nodejs`), so this is the
/// whole of what a bucket search does once the names are in hand.
pub fn matching_names(names: &[String], query: &str) -> Vec<String> {
    let needle = query.to_lowercase();
    names
        .iter()
        .filter(|n| n.to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

/// Every manifest file under every local bucket, paired with its path.
/// Best-effort throughout, the same as `choco::nuspec_paths`: a bucket, or a
/// `bucket` subdirectory, that cannot be read is simply nothing, not an
/// error.
fn local_bucket_manifest_paths(scoop_root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(buckets) = std::fs::read_dir(scoop_root.join("buckets")) else {
        return out;
    };
    for bucket in buckets.flatten() {
        let bucket_dir = bucket.path().join("bucket");
        let Ok(files) = std::fs::read_dir(&bucket_dir) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            let is_json = path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
            if is_json && let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.push((stem.to_string(), path));
            }
        }
    }
    out
}

/// Matching manifests from the buckets already on disk, parsed. A manifest
/// that matches but does not parse is left out, the same as a `.nuspec` that
/// does not.
fn local_candidates(root: &Path, query: &str, limit: usize) -> Vec<(String, Manifest)> {
    let paths = local_bucket_manifest_paths(root);
    let names: Vec<String> = paths.iter().map(|(n, _)| n.clone()).collect();
    matching_names(&names, query)
        .into_iter()
        .take(limit)
        .filter_map(|name| {
            let path = &paths.iter().find(|(n, _)| n == &name)?.1;
            let bytes = std::fs::read(path).ok()?;
            let manifest = parse_manifest(&bytes).ok()?;
            Some((name, manifest))
        })
        .collect()
}

/// Matching manifests from the main bucket on GitHub, fetched one at a time
/// for the matches only, never for the whole 1,654-name list.
fn network_candidates(
    client: &Client,
    query: &str,
    limit: usize,
) -> Result<Vec<(String, Manifest)>> {
    let names = main_bucket_names(client)?;
    let matches = matching_names(&names, query);
    Ok(matches
        .into_iter()
        .take(limit)
        .filter_map(|name| {
            let manifest = fetch_manifest(client, &name)?;
            Some((name, manifest))
        })
        .collect())
}

/// [`Scoop::search`]'s installed-join, pulled out as a pure function of its
/// inputs, the same as `choco::joined`: mark each match installed (and carry
/// its installed version) when the installed list holds the same name,
/// matched case-insensitively.
fn joined(
    matches: &[(String, Manifest)],
    installed: &[(String, Manifest)],
    limit: usize,
) -> Vec<Package> {
    matches
        .iter()
        .take(limit)
        .map(|(name, m)| {
            let mut p = to_package(name, m);
            if let Some((_, local)) = installed.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
                p.installed = true;
                p.installed_version = Some(local.version.clone());
            }
            p
        })
        .collect()
}

fn count(n: usize) -> String {
    if n == 1 {
        "1 package".to_string()
    } else {
        format!("{n} packages")
    }
}

/// Every app directory under `apps` that holds a `current\manifest.json`,
/// paired with that file's path. Best-effort, the same as
/// `choco::nuspec_paths`: an app directory without a `current` junction (a
/// half-finished install) is left out rather than counted.
fn manifest_paths(apps_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(apps) = std::fs::read_dir(apps_dir) else {
        return out;
    };
    for app in apps.flatten() {
        let dir = app.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(name) = dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let manifest = dir.join("current").join("manifest.json");
        if manifest.is_file() {
            out.push((name.to_string(), manifest));
        }
    }
    out
}

/// How many apps are installed, from the directory listing alone: whether a
/// `current\manifest.json` is present, never its contents. Same reasoning as
/// `choco::count_installed`: `status()` runs on every redraw and must stay a
/// `read_dir`, not a parse of every app.
fn count_installed(apps_dir: &Path) -> usize {
    manifest_paths(apps_dir).len()
}

/// Every installed app, parsed. A manifest that exists but does not parse is
/// left out, the same as a `.nuspec` that does not.
fn read_installed(apps_dir: &Path) -> Vec<(String, Manifest)> {
    manifest_paths(apps_dir)
        .into_iter()
        .filter_map(|(name, path)| {
            let bytes = std::fs::read(&path).ok()?;
            let manifest = parse_manifest(&bytes).ok()?;
            Some((name, manifest))
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    Install,
    Update,
    Remove,
}

/// One `scoop` call. **Never needs Administrator**: see the module doc
/// comment for why that is not a setting.
pub fn operation_step(kind: OpKind, id: &str, program: &str) -> Step {
    let (verb, title) = match kind {
        OpKind::Install => ("install", format!("Installing {id}")),
        OpKind::Update => ("update", format!("Updating {id}")),
        OpKind::Remove => ("uninstall", format!("Removing {id}")),
    };
    Step {
        source: SourceKind::Scoop,
        title,
        command: Command {
            program: program.to_string(),
            args: vec![verb.to_string(), id.to_string()],
            env: Vec::new(),
            cwd: None,
        },
        needs_root: false,
        weight: 10,
    }
}

/// The Scoop source: local apps read from `apps`, search against buckets on
/// disk or, failing that, the main bucket on GitHub.
pub struct Scoop {
    client: Arc<Client>,
    /// `%SCOOP%` (or `~\scoop`) on a real machine, from `Scoop::new`; a
    /// temporary directory in every test, from `Scoop::with_root`, so that no
    /// test reads the user's real installation. Scoop is not installed on
    /// the machine this was written on, so the installed half of every test
    /// here builds its own directory rather than reading a real one.
    root: PathBuf,
}

impl Scoop {
    pub fn new(client: Arc<Client>) -> Scoop {
        Scoop {
            client,
            root: install_root(),
        }
    }

    #[cfg(test)]
    fn with_root(client: Arc<Client>, root: PathBuf) -> Scoop {
        Scoop { client, root }
    }

    fn apps_dir(&self) -> PathBuf {
        self.root.join("apps")
    }

    /// Manifests matching `query`: from the buckets already on disk when
    /// Scoop is installed, from the main bucket on GitHub when it is not.
    fn candidates(&self, query: &str, limit: usize) -> Result<Vec<(String, Manifest)>> {
        if scoop_exe().is_some() {
            Ok(local_candidates(&self.root, query, limit))
        } else {
            network_candidates(&self.client, query, limit)
        }
    }
}

impl Source for Scoop {
    fn kind(&self) -> SourceKind {
        SourceKind::Scoop
    }

    /// Available when `scoop` is on `PATH`. When it is not, the source stays
    /// searchable against the main bucket on GitHub, the way winget's
    /// catalogue and Chocolatey's feed keep those sources searchable without
    /// their own tool.
    fn status(&self) -> SourceStatus {
        match scoop_exe() {
            Some(_) => SourceStatus {
                kind: SourceKind::Scoop,
                available: true,
                reason: None,
                detail: Some(count(count_installed(&self.apps_dir()))),
                searchable: true,
                setup: None,
            },
            None => SourceStatus {
                kind: SourceKind::Scoop,
                available: false,
                reason: Some(NOT_INSTALLED.to_string()),
                detail: None,
                searchable: true,
                setup: None,
            },
        }
    }

    fn search(&self, query: &Query) -> Result<Vec<Package>> {
        let text = query.text.trim();
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let matches = self.candidates(text, query.limit)?;
        let installed = read_installed(&self.apps_dir());
        Ok(joined(&matches, &installed, query.limit))
    }

    fn installed(&self) -> Result<Vec<Package>> {
        Ok(read_installed(&self.apps_dir())
            .iter()
            .map(|(name, m)| to_installed_package(name, m))
            .collect())
    }

    /// No version comparison against the bucket happens here: only
    /// `scoop status` itself can say what has moved, the same reasoning as
    /// `choco::Choco::updates`.
    fn updates(&self) -> Result<Vec<Update>> {
        Ok(Vec::new())
    }

    fn details(&self, id: &str) -> Result<Package> {
        if let Some((name, m)) = read_installed(&self.apps_dir())
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(id))
        {
            return Ok(to_installed_package(&name, &m));
        }
        let matches = self.candidates(id, 5)?;
        matches
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(id))
            .map(|(name, m)| to_package(&name, &m))
            .ok_or_else(|| {
                Error::from_source(
                    SourceKind::Scoop,
                    format!("{id} is not installed and is not in Scoop's main bucket."),
                )
            })
    }

    /// An operation for another source's package plans nothing: the store
    /// asks every source about every operation, and this is the one that
    /// belongs to Scoop.
    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        let program = scoop_program();
        let step = match op {
            Op::Install { package } if package.source == SourceKind::Scoop => {
                operation_step(OpKind::Install, &package.id, &program)
            }
            Op::Update { package } if package.source == SourceKind::Scoop => {
                operation_step(OpKind::Update, &package.id, &program)
            }
            Op::Remove { package } if package.source == SourceKind::Scoop => {
                operation_step(OpKind::Remove, &package.id, &program)
            }
            _ => return Ok(Vec::new()),
        };
        Ok(vec![step])
    }

    /// Deliberately `None`. The spec pins Scoop's installer to a named commit
    /// of `ScoopInstaller/Install` rather than the redirecting
    /// `get.scoop.sh`, under the invariant that nothing downloaded is run
    /// before it is verified. Scoop's bootstrap does not elevate, so it is
    /// the smaller half of the work Chocolatey's `setup` doc comment
    /// describes, but it is still fetching a script and running it, and it
    /// belongs with Chocolatey's in the plan that builds the verification
    /// rather than ahead of it. Until then, Scoop is searchable but not
    /// installable from inside Brokey, and `status()` says so; this method
    /// stays `None` on purpose, not because it was forgotten.
    fn setup(&self) -> Option<Setup> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PackageRef;

    fn fixture_bytes(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/scoop")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()))
    }

    fn sevenzip() -> Manifest {
        parse_manifest(&fixture_bytes("7zip.json")).expect("7zip.json parses")
    }

    fn nodejs() -> Manifest {
        parse_manifest(&fixture_bytes("nodejs.json")).expect("nodejs.json parses")
    }

    #[test]
    fn a_manifest_becomes_a_package() {
        let m = sevenzip();
        let p = to_package("7zip", &m);
        assert_eq!(p.id, "7zip");
        assert_eq!(p.name, "7zip");
        assert_eq!(p.source, SourceKind::Scoop);
        assert_eq!(p.version.as_deref(), Some("26.03"));
        assert!(
            p.description
                .as_deref()
                .is_some_and(|d| d.contains("archiver")),
            "{:?}",
            p.description
        );
        assert_eq!(p.homepage.as_deref(), Some("https://www.7-zip.org"));
        assert_eq!(
            p.licence.as_deref(),
            Some("BSD-2-Clause, BSD-3-Clause, LGPL-2.1-or-later")
        );
        assert!(
            p.facts
                .iter()
                .any(|(k, v)| k == "Executables" && v.contains("7z.exe")),
            "{:?}",
            p.facts
        );
        assert!(!p.installed);
    }

    #[test]
    fn a_licence_is_read_whether_it_is_a_string_or_an_object() {
        assert_eq!(nodejs().licence().as_deref(), Some("MIT"));
        assert_eq!(
            sevenzip().licence().as_deref(),
            Some("BSD-2-Clause, BSD-3-Clause, LGPL-2.1-or-later")
        );
    }

    #[test]
    fn a_manifest_without_a_bin_is_not_an_error() {
        let m = nodejs();
        assert_eq!(m.bin_names(), Vec::<String>::new());
        let p = to_package("nodejs", &m);
        assert!(
            !p.facts.iter().any(|(k, _)| k == "Executables"),
            "{:?}",
            p.facts
        );
    }

    /// `bin` is documented as a plain string, a list of strings, or a list
    /// in which any entry may itself be `[path, alias]` or
    /// `[path, alias, arguments]`. Each shape has to be handled, not just
    /// the list-of-strings one the real fixture happens to use.
    #[test]
    fn bin_names_handles_every_documented_shape() {
        #[derive(Deserialize)]
        struct Holder {
            bin: BinField,
        }
        let one: Holder = serde_json::from_str(r#"{"bin": "app.exe"}"#).unwrap();
        assert_eq!(
            Manifest {
                version: "1".to_string(),
                description: None,
                homepage: None,
                license: None,
                bin: Some(one.bin),
            }
            .bin_names(),
            vec!["app.exe".to_string()]
        );

        let many: Holder = serde_json::from_str(r#"{"bin": ["a.exe", "b.exe"]}"#).unwrap();
        assert_eq!(
            Manifest {
                version: "1".to_string(),
                description: None,
                homepage: None,
                license: None,
                bin: Some(many.bin),
            }
            .bin_names(),
            vec!["a.exe".to_string(), "b.exe".to_string()]
        );

        let aliased: Holder = serde_json::from_str(
            r#"{"bin": [["a.exe", "a-alias"], ["b.exe", "b-alias", "--flag"]]}"#,
        )
        .unwrap();
        assert_eq!(
            Manifest {
                version: "1".to_string(),
                description: None,
                homepage: None,
                license: None,
                bin: Some(aliased.bin),
            }
            .bin_names(),
            vec!["a.exe".to_string(), "b.exe".to_string()]
        );

        let mixed: Holder =
            serde_json::from_str(r#"{"bin": ["a.exe", ["b.exe", "b-alias"]]}"#).unwrap();
        assert_eq!(
            Manifest {
                version: "1".to_string(),
                description: None,
                homepage: None,
                license: None,
                bin: Some(mixed.bin),
            }
            .bin_names(),
            vec!["a.exe".to_string(), "b.exe".to_string()]
        );
    }

    #[test]
    fn an_installed_app_reads_its_version_from_the_current_manifest() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let current = root.path().join("apps").join("7zip").join("current");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(current.join("manifest.json"), fixture_bytes("7zip.json")).unwrap();

        let scoop = Scoop::with_root(Client::shared(), root.path().to_path_buf());
        let installed = scoop.installed().unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].id, "7zip");
        assert!(installed[0].installed);
        assert_eq!(installed[0].installed_version.as_deref(), Some("26.03"));
        assert_eq!(installed[0].version.as_deref(), Some("26.03"));
    }

    #[test]
    fn a_missing_apps_directory_is_simply_empty() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let scoop = Scoop::with_root(Client::shared(), root.path().join("does-not-exist"));
        assert_eq!(scoop.installed().unwrap(), Vec::new());
    }

    /// An app directory without a `current\manifest.json` (a half-finished
    /// or broken install) is left out rather than counted, the same choice
    /// `choco::nuspec_paths` makes for a package directory with no
    /// `.nuspec`.
    #[test]
    fn an_app_without_a_current_manifest_is_not_counted_or_shown() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let current = root.path().join("apps").join("7zip").join("current");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(current.join("manifest.json"), fixture_bytes("7zip.json")).unwrap();

        let half_installed = root.path().join("apps").join("nodejs");
        std::fs::create_dir_all(&half_installed).unwrap();

        assert_eq!(count_installed(&root.path().join("apps")), 1);
        let scoop = Scoop::with_root(Client::shared(), root.path().to_path_buf());
        let installed = scoop.installed().unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].id, "7zip");
    }

    /// `count_installed` only has to see that a manifest is present; it must
    /// not need to parse it, the same reasoning as
    /// `choco::count_installed_does_not_require_a_nuspec_to_parse`.
    #[test]
    fn count_installed_does_not_require_a_manifest_to_parse() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let current = root.path().join("apps").join("broken").join("current");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(current.join("manifest.json"), b"not json at all").unwrap();

        assert_eq!(count_installed(&root.path().join("apps")), 1);
        assert_eq!(read_installed(&root.path().join("apps")), Vec::new());
    }

    /// The whole point of Scoop: everything it does installs into the
    /// user's own profile, never `C:\Program Files` or the registry, so
    /// nothing it runs ever needs Administrator. A step here asking for it
    /// would be a defect, not a setting.
    #[test]
    fn nothing_scoop_does_ever_needs_administrator() {
        for kind in [OpKind::Install, OpKind::Update, OpKind::Remove] {
            let step = operation_step(kind, "7zip", "scoop");
            assert!(!step.needs_root, "{kind:?} must never need Administrator");
            assert_eq!(step.source, SourceKind::Scoop);
        }
    }

    #[test]
    fn setup_is_deliberately_none() {
        let scoop = Scoop::with_root(Client::shared(), std::env::temp_dir());
        assert!(scoop.setup().is_none());
    }

    #[test]
    fn a_manifest_that_is_not_json_is_an_error_not_a_panic() {
        assert!(parse_manifest(b"not json at all").is_err());
        assert!(parse_manifest(b"<!DOCTYPE html><html>404: Not Found</html>").is_err());
        assert!(parse_manifest(b"").is_err());
    }

    #[test]
    fn a_bucket_with_no_matches_is_an_empty_list_not_an_error() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let bucket = root.path().join("buckets").join("main").join("bucket");
        std::fs::create_dir_all(&bucket).unwrap();
        std::fs::write(bucket.join("7zip.json"), fixture_bytes("7zip.json")).unwrap();
        std::fs::write(bucket.join("nodejs.json"), fixture_bytes("nodejs.json")).unwrap();

        let found = local_candidates(root.path(), "definitely-not-in-this-bucket", 10);
        assert_eq!(found, Vec::new());
    }

    #[test]
    fn local_candidates_finds_a_real_match() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let bucket = root.path().join("buckets").join("main").join("bucket");
        std::fs::create_dir_all(&bucket).unwrap();
        std::fs::write(bucket.join("7zip.json"), fixture_bytes("7zip.json")).unwrap();
        std::fs::write(bucket.join("nodejs.json"), fixture_bytes("nodejs.json")).unwrap();

        let found = local_candidates(root.path(), "7zip", 10);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "7zip");
    }

    #[test]
    fn local_candidates_respects_the_limit() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let bucket = root.path().join("buckets").join("main").join("bucket");
        std::fs::create_dir_all(&bucket).unwrap();
        std::fs::write(bucket.join("7zip.json"), fixture_bytes("7zip.json")).unwrap();
        std::fs::write(bucket.join("nodejs.json"), fixture_bytes("nodejs.json")).unwrap();

        // Both fixtures' file stems contain the letter "o": "nodejs" does,
        // "7zip" does not, so a wider net is needed to hit the limit.
        let found = local_candidates(root.path(), "", 1);
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn a_missing_buckets_directory_is_simply_empty() {
        let root = tempfile::tempdir().expect("a temporary directory");
        assert_eq!(local_candidates(root.path(), "anything", 10), Vec::new());
    }

    #[test]
    fn matching_names_is_case_insensitive_and_substring() {
        let names = vec![
            "7zip".to_string(),
            "nodejs".to_string(),
            "NodeJS-LTS".to_string(),
        ];
        assert_eq!(matching_names(&names, "node"), vec!["nodejs", "NodeJS-LTS"]);
        assert_eq!(matching_names(&names, "NODE"), vec!["nodejs", "NodeJS-LTS"]);
        assert_eq!(
            matching_names(&names, "zzz"),
            Vec::<String>::new(),
            "a bucket with no matches is an empty list"
        );
    }

    #[test]
    fn joined_marks_installed_packages_case_insensitively() {
        let seven = sevenzip();
        let node = nodejs();
        let matches = vec![
            ("7zip".to_string(), seven.clone()),
            ("nodejs".to_string(), node.clone()),
        ];
        let mut older_seven = seven.clone();
        older_seven.version = "25.00".to_string();
        let installed = vec![("7ZIP".to_string(), older_seven)];

        let packages = joined(&matches, &installed, 10);
        assert_eq!(packages.len(), 2);
        let p7 = packages.iter().find(|p| p.id == "7zip").unwrap();
        assert!(p7.installed, "matched case-insensitively");
        assert_eq!(p7.installed_version.as_deref(), Some("25.00"));
        let pnode = packages.iter().find(|p| p.id == "nodejs").unwrap();
        assert!(!pnode.installed);
        assert_eq!(pnode.installed_version, None);
    }

    #[test]
    fn joined_respects_the_limit() {
        let m = nodejs();
        let matches = vec![("a".to_string(), m.clone()), ("b".to_string(), m.clone())];
        assert_eq!(joined(&matches, &[], 1).len(), 1);
    }

    #[test]
    fn plan_only_answers_for_its_own_packages_and_never_needs_root() {
        let scoop = Scoop::with_root(Client::shared(), std::env::temp_dir());
        let mine = PackageRef {
            source: SourceKind::Scoop,
            id: "7zip".to_string(),
        };
        let cases = [
            (
                Op::Install {
                    package: mine.clone(),
                },
                "install",
            ),
            (
                Op::Update {
                    package: mine.clone(),
                },
                "update",
            ),
            (
                Op::Remove {
                    package: mine.clone(),
                },
                "uninstall",
            ),
        ];
        for (op, verb) in cases {
            let steps = scoop.plan(&op).unwrap();
            assert_eq!(steps.len(), 1, "{op:?}");
            assert_eq!(steps[0].command.args[0], verb, "{op:?}");
            assert_eq!(steps[0].command.args[1], "7zip", "{op:?}");
            assert!(!steps[0].needs_root, "{op:?}");
            assert_eq!(steps[0].source, SourceKind::Scoop, "{op:?}");
        }

        let someone_elses = PackageRef {
            source: SourceKind::Winget,
            id: "Valve.Steam".to_string(),
        };
        assert!(
            scoop
                .plan(&Op::Install {
                    package: someone_elses,
                })
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn refresh_update_all_and_setup_plan_nothing() {
        let scoop = Scoop::with_root(Client::shared(), std::env::temp_dir());
        for op in [
            Op::Refresh {
                source: SourceKind::Scoop,
            },
            Op::UpdateAll {
                source: SourceKind::Scoop,
            },
            Op::Setup {
                source: SourceKind::Scoop,
            },
        ] {
            assert!(scoop.plan(&op).unwrap().is_empty(), "{op:?}");
        }
    }

    #[test]
    fn one_package_is_singular() {
        assert_eq!(count(1), "1 package");
        assert_eq!(count(5), "5 packages");
    }

    #[test]
    fn the_bucket_urls_are_scoops_own() {
        assert_eq!(
            MASTER_TREE_URL,
            "https://api.github.com/repos/ScoopInstaller/Main/git/trees/master"
        );
        assert_eq!(
            bucket_tree_url("abc123"),
            "https://api.github.com/repos/ScoopInstaller/Main/git/trees/abc123"
        );
        assert_eq!(
            manifest_url("7zip"),
            "https://raw.githubusercontent.com/ScoopInstaller/Main/master/bucket/7zip.json"
        );
    }

    fn tree_json(entries: &[(&str, &str, &str)]) -> String {
        let items: Vec<String> = entries
            .iter()
            .map(|(path, kind, sha)| {
                format!(r#"{{"path":"{path}","type":"{kind}","sha":"{sha}"}}"#)
            })
            .collect();
        format!(r#"{{"sha":"root","tree":[{}]}}"#, items.join(","))
    }

    /// A "bucket-scripts" entry contains "bucket" as a substring and comes
    /// first, so a match on path.contains("bucket") rather than the exact
    /// name would answer its sha instead of the real bucket's.
    #[test]
    fn bucket_sha_finds_the_bucket_entry_among_others() {
        let master = parse_tree(
            tree_json(&[
                (".github", "tree", "sha-github"),
                ("bucket-scripts", "tree", "sha-bucket-scripts"),
                ("bucket", "tree", "sha-bucket"),
                ("README.md", "blob", "sha-readme"),
            ])
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(bucket_sha(&master), Some("sha-bucket"));
    }

    #[test]
    fn bucket_sha_is_none_when_there_is_no_bucket_directory() {
        let master =
            parse_tree(tree_json(&[(".github", "tree", "sha-github")]).as_bytes()).unwrap();
        assert_eq!(bucket_sha(&master), None);
    }

    /// Only `.json` blobs are manifests. A non-`.json` blob (the real bucket
    /// has a `README.md`) and a nested tree entry both have to be filtered
    /// out, not just the file extension stripped from everything.
    #[test]
    fn manifest_names_reads_only_json_blobs() {
        let bucket = parse_tree(
            tree_json(&[
                ("7zip.json", "blob", "sha-7zip"),
                ("nodejs.json", "blob", "sha-nodejs"),
                ("README.md", "blob", "sha-readme"),
                ("scripts", "tree", "sha-scripts"),
                // A directory that happens to be named like a manifest; only
                // a blob counts, never a tree entry, whatever its name.
                ("weird.json", "tree", "sha-weird-dir"),
            ])
            .as_bytes(),
        )
        .unwrap();
        let mut names = manifest_names(&bucket);
        names.sort();
        assert_eq!(names, vec!["7zip".to_string(), "nodejs".to_string()]);
    }

    #[test]
    fn a_tree_reply_that_is_not_json_is_an_error_not_a_panic() {
        assert!(parse_tree(b"not json").is_err());
        assert!(parse_tree(b"").is_err());
    }
}
