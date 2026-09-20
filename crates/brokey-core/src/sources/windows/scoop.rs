//! Scoop: installed applications read from `apps\<name>\current\manifest.json`
//! under `%SCOOP%` (or `~\scoop` when that is unset), and search against the
//! buckets under `buckets\<bucket>\bucket` when any manifest is there, and
//! against the main bucket on GitHub when none is. The choice is the
//! presence of a bucket on disk, not the presence of Scoop: an installed
//! Scoop that has fetched no bucket still searches, and a bucket left behind
//! by a Scoop that has gone is still read.
//!
//! A manifest's fields do not have one shape each: `license` is a plain
//! string in one real manifest and an object carrying `identifier` in
//! another, and `bin` is a list in one, absent in another, and documented as
//! also being a plain string or a list of lists (`[["path", "alias"]]`).
//! [`Manifest::licence`] and [`Manifest::bin_names`] read every shape the
//! schema documents, and never `unwrap` on which one a given manifest
//! happens to use. A shape neither of them recognises costs that one field
//! and nothing else: the field reads as absent and the rest of the manifest
//! still becomes a package. Buckets are third-party and are not schema
//! checked, so an unmodelled `license` object must never be able to make an
//! installed application disappear from the store.
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
/// it. A `SCOOP` set to nothing at all counts as unset rather than as a
/// relative path. When `%USERPROFILE%` is unset too, the home directory
/// falls back to `C:\Users\Default`, which holds no Scoop installation, so
/// the source then finds nothing instead of reading another user's profile.
pub fn install_root() -> PathBuf {
    root_from(std::env::var("SCOOP").ok(), &home_dir())
}

/// [`install_root`]'s choice as a pure function of the two values it reads,
/// so a test can make that choice without touching the process's own
/// environment.
fn root_from(scoop: Option<String>, home: &Path) -> PathBuf {
    scoop
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("scoop"))
}

/// `%USERPROFILE%`, or `C:\Users\Default` when it is unset. See
/// [`install_root`] for what that fallback means.
fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\Users\Default"))
}

/// Where Scoop's own shim is, if it is anywhere: on `PATH` first, then
/// `shims\scoop.cmd` under [`install_root`]. Scoop's installer writes
/// `shims\scoop.ps1` and `shims\scoop.cmd` and no `scoop.exe` at all, and
/// adds that directory to `PATH`, so the first step answers on an ordinary
/// machine: `crate::system::which` reads `PATHEXT`, which is how it matches
/// a `.cmd` against a bare name. The second step is for a process that did
/// not inherit that `PATH`, the same two-step search `choco_exe` makes.
pub fn scoop_exe() -> Option<PathBuf> {
    exe_from(crate::system::which("scoop"), &install_root())
}

/// [`scoop_exe`]'s two-step search as a pure function of what `PATH`
/// answered and where the root is, so a test can make both without touching
/// the machine's own `PATH` or its real installation.
fn exe_from(on_path: Option<PathBuf>, root: &Path) -> Option<PathBuf> {
    on_path.or_else(|| shim_in(root))
}

/// `shims\scoop.cmd` under a Scoop root, when that file is there. Pure in
/// its argument, so a test can point it at a temporary directory rather than
/// at the machine's own installation.
fn shim_in(root: &Path) -> Option<PathBuf> {
    let candidate = root.join("shims").join("scoop.cmd");
    candidate.is_file().then_some(candidate)
}

/// The program a step names: the full path to Scoop's own shim when it was
/// found, `scoop.cmd` otherwise. That extension is not decoration.
/// `std::process::Command` does run a `.cmd` and does search `PATH`, but it
/// appends `.exe` to an extensionless name and never consults `PATHEXT`, so
/// a bare `scoop` could only ever fail to spawn; all three cases were
/// measured on Windows against a real shim. Resolution happens here, in the
/// unelevated process, the same as `choco_program` and `winget_program`, and
/// for the same reason: the elevated helper must never search for a program
/// itself. Unlike Chocolatey's and winget's steps, nothing this resolves is
/// ever handed to the helper: see the module doc comment.
pub fn step_program(exe: Option<&Path>) -> String {
    exe.map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "scoop.cmd".to_string())
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
    #[serde(default, deserialize_with = "lenient")]
    license: Option<LicenseField>,
    #[serde(default, deserialize_with = "lenient")]
    bin: Option<BinField>,
}

/// Read an optional field whose value has more than one documented shape,
/// and answer `None` for a value matching none of them. Without this an
/// untagged enum that matches no variant fails the whole `Manifest`, and
/// every caller here drops a manifest that does not parse, so one
/// unmodelled shape in a decorative field would erase the package: out of
/// the installed list, out of search, out of `details`, with no error
/// anywhere. A field that is present but unreadable is worth exactly that
/// field.
fn lenient<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
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
    /// when the manifest carries no `license` field at all, and also when it
    /// carries one in a shape this does not recognise: an unreadable licence
    /// costs the licence line and not the package.
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
    /// (`nodejs.json` has none), and empty in the same way when `bin` is
    /// present in a shape this does not recognise: the "Executables" row is
    /// then missing and the package is not.
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

/// One entry in a git tree: a `blob` (a file, and in the bucket's tree a
/// manifest) or a `tree` (a directory), with the sha that names it.
#[derive(Clone, Debug, Deserialize)]
struct TreeEntry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}

/// One reply from GitHub's git tree API. `truncated` is GitHub's own flag
/// for "this is not all of it": a tree too large for one reply comes back
/// shorter and says so here rather than failing, so it has to be read or a
/// partial name list would pass for the whole bucket.
#[derive(Clone, Debug, Deserialize)]
struct Tree {
    tree: Vec<TreeEntry>,
    #[serde(default)]
    truncated: bool,
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

/// One manifest's address in Main. `name` is written into the path as it
/// stands, with no encoding, so only a name `is_safe_name` accepts may reach
/// it. `fetch_manifest` is the only caller outside the tests and it makes
/// that check.
pub fn manifest_url(name: &str) -> String {
    format!("https://raw.githubusercontent.com/ScoopInstaller/Main/master/bucket/{name}.json")
}

/// Whether a name is one plain path segment, which every manifest name in a
/// bucket is by construction: ASCII letters and digits, `-`, `_` and `.`,
/// not empty, and never `..` in any part of it. `Scoop::details` takes its
/// id from its caller rather than from a bucket listing, and the address
/// above is built by interpolation, so without this a `..` would climb out
/// of the `bucket/` segment and fetch some other file from the same
/// repository to be parsed as a manifest. A name outside this set is
/// refused rather than fetched, and reads as a name the bucket does not
/// have.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// How long a cached answer (the bucket's name list, or one manifest) is
/// trusted before it is fetched again. The same length as
/// `winget::index::MAX_AGE`, for the same reason: this is Brokey's own
/// snapshot of somebody else's catalogue, not a live query.
const CACHE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Where the network half of this source gets its text. [`Client`] is the
/// only implementation outside this file's own tests, which use one of their
/// own so that a test of the search path never reaches GitHub and can count
/// the requests a search makes.
trait Http: Send + Sync {
    fn text(&self, url: &str) -> Result<String>;
}

impl Http for Client {
    fn text(&self, url: &str) -> Result<String> {
        self.get_text_cached(url, CACHE_MAX_AGE)
    }
}

fn truncated_tree_error() -> Error {
    Error::from_source(
        SourceKind::Scoop,
        "GitHub did not answer with the tree Brokey expected: it truncated the list, so some of \
         Scoop's main bucket would be missing from a search."
            .to_string(),
    )
}

/// Every manifest name in ScoopInstaller/Main, fetched (and cached) as two
/// requests: the root tree, to find the `bucket` directory's own sha, then
/// the tree that sha names, whose entries are the manifests.
fn main_bucket_names(http: &dyn Http) -> Result<Vec<String>> {
    let master = parse_tree(http.text(MASTER_TREE_URL)?.as_bytes())?;
    if master.truncated {
        return Err(truncated_tree_error());
    }
    let sha = bucket_sha(&master).ok_or_else(|| {
        Error::from_source(
            SourceKind::Scoop,
            "GitHub's tree for ScoopInstaller/Main has no bucket directory, so Brokey cannot \
             search Scoop's main bucket."
                .to_string(),
        )
    })?;
    let bucket = parse_tree(http.text(&bucket_tree_url(sha))?.as_bytes())?;
    if bucket.truncated {
        return Err(truncated_tree_error());
    }
    Ok(manifest_names(&bucket))
}

/// One manifest, fetched (and cached) by name. `None` on any failure: a name
/// that is not in Main answers 404 with an HTML body, which is neither valid
/// JSON nor a reason to fail the whole search, so it is skipped the same way
/// a network error or an unreadable local file is. A name [`is_safe_name`]
/// refuses is `None` before any request is made at all.
fn fetch_manifest(http: &dyn Http, name: &str) -> Option<Manifest> {
    if !is_safe_name(name) {
        return None;
    }
    let text = http.text(&manifest_url(name)).ok()?;
    parse_manifest(text.as_bytes()).ok()
}

/// Case-insensitive substring match of `query` against a list of manifest
/// names, whether the names came off disk or off GitHub's tree, best match
/// first: the exact name, then the names that start with the query, then the
/// rest. Scoop bucket names are the package ids themselves (`7zip`,
/// `nodejs`), so this is the whole of what a bucket search does once the
/// names are in hand. Ranking is what `Query`'s "sources return their best
/// matches first" asks for: both bucket listings arrive in alphabetical
/// order, which would otherwise put an exact match wherever its initial
/// falls. The sort is stable, so names of equal rank keep that order.
pub fn matching_names(names: &[String], query: &str) -> Vec<String> {
    let needle = query.to_lowercase();
    let mut out: Vec<String> = names
        .iter()
        .filter(|n| n.to_lowercase().contains(&needle))
        .cloned()
        .collect();
    out.sort_by_key(|n| rank(n, &needle));
    out
}

/// 0 for the exact name, 1 for a name starting with the query, 2 for any
/// other substring match. Case-insensitive; `needle` is already lowered.
fn rank(name: &str, needle: &str) -> u8 {
    let lowered = name.to_lowercase();
    if lowered == needle {
        0
    } else if lowered.starts_with(needle) {
        1
    } else {
        2
    }
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

/// Matching manifests from the buckets already on disk, parsed, once the
/// bucket listing is in hand. A manifest that matches but does not parse is
/// left out, the same as a `.nuspec` that does not. The listing is a
/// parameter because the caller has already read it, to decide whether
/// there is a bucket on disk at all.
fn local_candidates_from(
    paths: &[(String, PathBuf)],
    query: &str,
    limit: usize,
) -> Vec<(String, Manifest)> {
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

/// The one bucket manifest whose name is exactly `id`, matched the way Scoop
/// matches names. No substring search and no limit: a manifest's name is the
/// package id, so there is nothing here to rank or to truncate.
fn local_manifest(paths: &[(String, PathBuf)], id: &str) -> Option<(String, Manifest)> {
    let (name, path) = paths.iter().find(|(n, _)| n.eq_ignore_ascii_case(id))?;
    let bytes = std::fs::read(path).ok()?;
    Some((name.clone(), parse_manifest(&bytes).ok()?))
}

/// How many manifests one uncached network search fetches, however many
/// names matched. Each is a separate request run after the last one, so the
/// page waits for all of them before it draws a row; a two-letter query
/// matches hundreds of the bucket's 1,654 names and `Query::new` asks for
/// 200, which would be 200 round trips before anything appears. The matches
/// past the cap are dropped rather than answered as bare names: 25 rows
/// carrying a version is a better thing to look at than 25 rows followed by
/// 175 empty ones. The local path is capped by `query.limit` alone and not
/// by this: it reads files rather than making a request for each match.
const NETWORK_DETAIL_LIMIT: usize = 25;

/// Matching manifests from the main bucket on GitHub, fetched one at a time
/// for the matches only, never for the whole 1,654-name list, and never more
/// than [`NETWORK_DETAIL_LIMIT`] of them.
fn network_candidates(
    http: &dyn Http,
    query: &str,
    limit: usize,
) -> Result<Vec<(String, Manifest)>> {
    let names = main_bucket_names(http)?;
    let matches = matching_names(&names, query);
    Ok(matches
        .into_iter()
        .take(limit.min(NETWORK_DETAIL_LIMIT))
        .filter_map(|name| {
            let manifest = fetch_manifest(http, &name)?;
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
    http: Arc<dyn Http>,
    /// `%SCOOP%` (or `~\scoop`) on a real machine, from [`Scoop::new`]; a
    /// temporary directory in every test, from `Scoop::with_root`. Scoop is
    /// not installed on the machine this was written on, so the installed
    /// half of every test here builds its own directory rather than reading
    /// a real one.
    root: PathBuf,
    /// Scoop's own shim, resolved once in [`Scoop::new`] and never probed
    /// again. The machine's `PATH` is read there and nowhere else in this
    /// source, so `status` and `plan` are a function of this field rather
    /// than of the environment a test happens to run in, and a test can
    /// force it either way. Together with `root` and `http` that makes every
    /// `Source` method here answerable from a fixture.
    exe: Option<PathBuf>,
}

impl Scoop {
    pub fn new(client: Arc<Client>) -> Scoop {
        Scoop {
            http: client,
            root: install_root(),
            exe: scoop_exe(),
        }
    }

    #[cfg(test)]
    fn with_root(http: Arc<dyn Http>, root: PathBuf, exe: Option<PathBuf>) -> Scoop {
        Scoop { http, root, exe }
    }

    fn apps_dir(&self) -> PathBuf {
        self.root.join("apps")
    }

    /// Manifests matching `query`: from the buckets already on disk when
    /// there are any, from the main bucket on GitHub when there are none.
    /// The choice is the presence of a bucket rather than the presence of
    /// the shim, because a user who has Scoop but has never fetched a bucket
    /// would otherwise get a silently empty search instead of the fallback
    /// that is one line away.
    fn candidates(&self, query: &str, limit: usize) -> Result<Vec<(String, Manifest)>> {
        let paths = local_bucket_manifest_paths(&self.root);
        if paths.is_empty() {
            network_candidates(self.http.as_ref(), query, limit)
        } else {
            Ok(local_candidates_from(&paths, query, limit))
        }
    }
}

impl Source for Scoop {
    fn kind(&self) -> SourceKind {
        SourceKind::Scoop
    }

    /// Available when Scoop's shim was found when this source was built.
    /// When it was not, the source stays searchable against the main bucket
    /// on GitHub, the way winget's catalogue and Chocolatey's feed keep
    /// those sources searchable without their own tool, and it says why it
    /// is unavailable rather than being skipped in silence.
    fn status(&self) -> SourceStatus {
        match &self.exe {
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

    /// Always empty in this task, and not because the comparison cannot be
    /// made. When Scoop is installed both halves are on disk, the installed
    /// manifest under `apps` and the bucket's under `buckets`, so comparing
    /// their versions is a pure function of two files rather than a call to
    /// `scoop status`. It is not written here because this task does not ask
    /// for it; it is recorded as a follow-up.
    fn updates(&self) -> Result<Vec<Update>> {
        Ok(Vec::new())
    }

    /// The installed manifest first, then the bucket's. The bucket half
    /// looks the exact name up rather than running a search and picking
    /// through its first few answers: a manifest's name is the package id,
    /// so one lookup answers, and on the network path it is one request
    /// instead of the two tree requests plus a fetch per near miss.
    fn details(&self, id: &str) -> Result<Package> {
        if let Some((name, m)) = read_installed(&self.apps_dir())
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(id))
        {
            return Ok(to_installed_package(&name, &m));
        }
        let paths = local_bucket_manifest_paths(&self.root);
        if !paths.is_empty() {
            return local_manifest(&paths, id)
                .map(|(name, m)| to_package(&name, &m))
                .ok_or_else(|| {
                    Error::from_source(
                        SourceKind::Scoop,
                        format!("{id} is not installed and is not in any bucket Brokey can see."),
                    )
                });
        }
        fetch_manifest(self.http.as_ref(), id)
            .map(|m| to_package(id, &m))
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
        let program = step_program(self.exe.as_deref());
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

    fn fixture_text(name: &str) -> String {
        String::from_utf8(fixture_bytes(name)).expect("the fixtures are UTF-8")
    }

    /// A bucket on disk: `<root>\buckets\main\bucket\<name>`, the nesting
    /// Scoop itself uses.
    fn write_bucket(root: &Path, files: &[(&str, Vec<u8>)]) {
        let bucket = root.join("buckets").join("main").join("bucket");
        std::fs::create_dir_all(&bucket).expect("a bucket directory");
        for (name, bytes) in files {
            std::fs::write(bucket.join(name), bytes).expect("a bucket manifest");
        }
    }

    /// An installed app on disk: `<root>\apps\<name>\current\manifest.json`.
    fn write_installed_app(root: &Path, name: &str, bytes: &[u8]) {
        let current = root.join("apps").join(name).join("current");
        std::fs::create_dir_all(&current).expect("an app directory");
        std::fs::write(current.join("manifest.json"), bytes).expect("a manifest");
    }

    /// A stand-in for GitHub: every URL answers out of a table, anything not
    /// in it fails the way an unreachable host does, and every request is
    /// recorded so a test can count and order them. Nothing here opens a
    /// socket, so these tests run offline and on a machine without Scoop.
    struct FakeHttp {
        replies: std::collections::HashMap<String, String>,
        asked: std::sync::Mutex<Vec<String>>,
    }

    impl FakeHttp {
        fn answering(replies: Vec<(String, String)>) -> FakeHttp {
            FakeHttp {
                replies: replies.into_iter().collect(),
                asked: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().expect("the request log").clone()
        }
    }

    impl Http for FakeHttp {
        fn text(&self, url: &str) -> Result<String> {
            self.asked
                .lock()
                .expect("the request log")
                .push(url.to_string());
            self.replies.get(url).cloned().ok_or_else(|| {
                Error::from_source(SourceKind::Scoop, format!("nothing answers {url} here."))
            })
        }
    }

    /// An `Http` that refuses every request, for the tests whose answer has
    /// to come off disk: one that reached for the network fails here rather
    /// than quietly waiting on a socket or reading the real GitHub.
    struct NoNetwork;

    impl Http for NoNetwork {
        fn text(&self, url: &str) -> Result<String> {
            Err(Error::from_source(
                SourceKind::Scoop,
                format!("this test must not ask the network for {url}."),
            ))
        }
    }

    fn no_network() -> Arc<dyn Http> {
        Arc::new(NoNetwork)
    }

    /// The local search path from a root on disk, which is what
    /// `Scoop::candidates` runs when a bucket is there: the bucket listing,
    /// then the matches out of it.
    fn local_candidates(root: &Path, query: &str, limit: usize) -> Vec<(String, Manifest)> {
        local_candidates_from(&local_bucket_manifest_paths(root), query, limit)
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

        let scoop = Scoop::with_root(no_network(), root.path().to_path_buf(), None);
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
        let scoop = Scoop::with_root(no_network(), root.path().join("does-not-exist"), None);
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
        let scoop = Scoop::with_root(no_network(), root.path().to_path_buf(), None);
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
        let scoop = Scoop::with_root(no_network(), std::env::temp_dir(), None);
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

        // An empty query is a substring of every name, so both manifests
        // match and a limit of 1 must still answer exactly one of them.
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
        let scoop = Scoop::with_root(no_network(), std::env::temp_dir(), None);
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
        let scoop = Scoop::with_root(no_network(), std::env::temp_dir(), None);
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

    /// Scoop's installer writes `shims\scoop.ps1` and `shims\scoop.cmd` and
    /// no `scoop.exe` at all, and `std::process::Command` appends `.exe` to
    /// an extensionless name rather than reading `PATHEXT`, so a bare
    /// `scoop` is a program that can only fail to spawn. That literal is the
    /// whole of what was measured, so the literal is what this asserts.
    #[test]
    fn the_fallback_program_is_scoop_cmd_and_not_a_bare_name() {
        assert_eq!(step_program(None), "scoop.cmd");
        let shim = PathBuf::from(r"C:\Users\someone\scoop\shims\scoop.cmd");
        assert_eq!(
            step_program(Some(&shim)),
            r"C:\Users\someone\scoop\shims\scoop.cmd"
        );
    }

    /// The second half of the search for Scoop's shim: the fixed place under
    /// the root, for a process whose `PATH` does not carry Scoop's shims
    /// directory.
    #[test]
    fn the_shim_under_the_root_is_found_when_path_has_none() {
        let root = tempfile::tempdir().expect("a temporary directory");
        assert_eq!(shim_in(root.path()), None, "nothing is there yet");
        assert_eq!(
            exe_from(None, root.path()),
            None,
            "neither PATH nor the root has one"
        );

        let shims = root.path().join("shims");
        std::fs::create_dir_all(&shims).unwrap();
        std::fs::write(shims.join("scoop.cmd"), b"@echo off\r\n").unwrap();
        assert_eq!(shim_in(root.path()), Some(shims.join("scoop.cmd")));
        assert_eq!(
            exe_from(None, root.path()),
            Some(shims.join("scoop.cmd")),
            "the root is the fallback when PATH has no shim"
        );

        let on_path = PathBuf::from(r"C:\elsewhere\shims\scoop.cmd");
        assert_eq!(
            exe_from(Some(on_path.clone()), root.path()),
            Some(on_path),
            "PATH is asked first"
        );
    }

    /// A step names the program this source resolved when it was built, not
    /// whatever the machine running the test has on its own `PATH`.
    #[test]
    fn a_step_names_the_resolved_shim_or_falls_back_to_scoop_cmd() {
        let package = PackageRef {
            source: SourceKind::Scoop,
            id: "7zip".to_string(),
        };
        let shim = PathBuf::from(r"C:\Users\someone\scoop\shims\scoop.cmd");
        let found = Scoop::with_root(no_network(), std::env::temp_dir(), Some(shim.clone()));
        let steps = found
            .plan(&Op::Install {
                package: package.clone(),
            })
            .unwrap();
        assert_eq!(steps[0].command.program, shim.to_string_lossy());

        let missing = Scoop::with_root(no_network(), std::env::temp_dir(), None);
        let steps = missing.plan(&Op::Install { package }).unwrap();
        assert_eq!(steps[0].command.program, "scoop.cmd");
    }

    /// `SCOOP` set to nothing at all is not a root. Without the guard it
    /// becomes an empty path, and every `apps` and `buckets` read then
    /// happens relative to whatever directory Brokey was started in.
    #[test]
    fn an_empty_scoop_variable_falls_back_to_the_profile() {
        let home = Path::new(r"C:\Users\someone");
        assert_eq!(root_from(None, home), home.join("scoop"));
        assert_eq!(root_from(Some(String::new()), home), home.join("scoop"));
        assert_eq!(root_from(Some("   ".to_string()), home), home.join("scoop"));
        assert_eq!(
            root_from(Some(r"D:\scoop".to_string()), home),
            PathBuf::from(r"D:\scoop")
        );
    }

    /// `license` and `bin` are optional and decorative, so a shape neither
    /// enum models must cost that field alone. Every caller here drops a
    /// manifest that fails to parse, so rejecting the whole manifest would
    /// take the application out of the installed list, out of search and out
    /// of `details` with no error anywhere.
    #[test]
    fn a_manifest_with_an_unexpected_licence_shape_still_parses() {
        let m = parse_manifest(br#"{"version":"1","license":{"url":"x"}}"#)
            .expect("a licence object with no identifier costs the licence, not the package");
        assert_eq!(m.version, "1");
        assert_eq!(m.licence(), None);

        let m = parse_manifest(br#"{"version":"1","bin":{"path":"a.exe"}}"#)
            .expect("a bin object costs the executables row, not the package");
        assert_eq!(m.bin_names(), Vec::<String>::new());

        let m = parse_manifest(br#"{"version":"1","license":7,"bin":[{"path":"a.exe"}]}"#)
            .expect("neither field is worth the package");
        assert_eq!(m.licence(), None);
        assert_eq!(m.bin_names(), Vec::<String>::new());

        let p = to_package("oddball", &m);
        assert_eq!(p.version.as_deref(), Some("1"));
        assert_eq!(p.licence, None);
        assert!(!p.facts.iter().any(|(k, _)| k == "Executables"));
    }

    /// `Query`'s contract is that a source returns its best matches first.
    /// Both bucket listings arrive alphabetically, so an exact match has to
    /// be lifted out of the alphabet rather than left wherever its initial
    /// puts it.
    #[test]
    fn the_exact_match_comes_first_then_prefixes_then_the_rest() {
        let names = vec![
            "7zip".to_string(),
            "gow".to_string(),
            "golangci-lint".to_string(),
            "mongodb".to_string(),
            "django".to_string(),
            "go".to_string(),
        ];
        assert_eq!(
            matching_names(&names, "go"),
            vec!["go", "gow", "golangci-lint", "mongodb", "django"]
        );
        assert_eq!(
            matching_names(&names, "GO")[0],
            "go",
            "the ranking is case-insensitive too"
        );
    }

    /// A bucket file named `.JSON` is still a manifest: Windows file names
    /// are case-insensitive, so the extension check has to be as well.
    #[test]
    fn an_uppercase_manifest_extension_is_still_a_manifest() {
        let root = tempfile::tempdir().expect("a temporary directory");
        write_bucket(
            root.path(),
            &[("SHOUTING.JSON", fixture_bytes("nodejs.json"))],
        );

        let found = local_candidates(root.path(), "shouting", 10);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, "SHOUTING");
    }

    /// The constraint is that a source is never silently skipped. Without
    /// Scoop the status still says it is unavailable, still says why, and
    /// still says the main bucket can be searched anyway.
    #[test]
    fn status_without_scoop_is_unavailable_searchable_and_says_why() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let scoop = Scoop::with_root(no_network(), root.path().to_path_buf(), None);

        let status = scoop.status();
        assert_eq!(status.kind, SourceKind::Scoop);
        assert!(!status.available, "no shim was found");
        assert!(
            status.searchable,
            "the main bucket answers over HTTP without Scoop"
        );
        assert_eq!(status.reason.as_deref(), Some(NOT_INSTALLED));
        assert_eq!(status.detail, None);
        assert!(status.setup.is_none());
    }

    #[test]
    fn status_with_scoop_counts_the_installed_apps() {
        let root = tempfile::tempdir().expect("a temporary directory");
        write_installed_app(root.path(), "7zip", &fixture_bytes("7zip.json"));
        write_installed_app(root.path(), "nodejs", &fixture_bytes("nodejs.json"));
        let scoop = Scoop::with_root(
            no_network(),
            root.path().to_path_buf(),
            Some(root.path().join("shims").join("scoop.cmd")),
        );

        let status = scoop.status();
        assert!(status.available);
        assert_eq!(status.reason, None);
        assert_eq!(status.detail.as_deref(), Some("2 packages"));
        assert!(status.searchable);
    }

    /// An empty query answers nothing rather than the whole bucket. The same
    /// bucket answers a real query in the same test, so an empty answer here
    /// is the guard and not an empty bucket.
    #[test]
    fn an_empty_query_searches_for_nothing() {
        let root = tempfile::tempdir().expect("a temporary directory");
        write_bucket(
            root.path(),
            &[
                ("7zip.json", fixture_bytes("7zip.json")),
                ("nodejs.json", fixture_bytes("nodejs.json")),
            ],
        );
        let scoop = Scoop::with_root(no_network(), root.path().to_path_buf(), None);

        assert_eq!(scoop.search(&Query::new("")).unwrap(), Vec::new());
        assert_eq!(scoop.search(&Query::new("   ")).unwrap(), Vec::new());
        assert_eq!(scoop.search(&Query::new("7zip")).unwrap().len(), 1);
    }

    /// Search joins its matches to the installed list, so a bucket entry the
    /// user already has is drawn as installed and carries the version on
    /// disk beside the bucket's.
    #[test]
    fn search_marks_a_match_that_is_already_installed() {
        let root = tempfile::tempdir().expect("a temporary directory");
        write_bucket(
            root.path(),
            &[
                ("7zip.json", fixture_bytes("7zip.json")),
                ("nodejs.json", fixture_bytes("nodejs.json")),
            ],
        );
        write_installed_app(
            root.path(),
            "7zip",
            br#"{"version":"25.00","description":"an older 7zip"}"#,
        );
        let scoop = Scoop::with_root(no_network(), root.path().to_path_buf(), None);

        let found = scoop.search(&Query::new("7zip")).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].installed, "it is in apps as well as in the bucket");
        assert_eq!(found[0].installed_version.as_deref(), Some("25.00"));
        assert_eq!(found[0].version.as_deref(), Some("26.03"));

        let found = scoop.search(&Query::new("nodejs")).unwrap();
        assert!(!found[0].installed);
        assert_eq!(found[0].installed_version, None);
    }

    /// `details` must find the manifest whose name is the id even when many
    /// alphabetically earlier names hold that id as a substring. The version
    /// this replaces ran an unranked substring search, took its first five
    /// answers and only then looked for the exact name, so a package sitting
    /// in the bucket was reported as missing.
    #[test]
    fn details_finds_the_exact_name_behind_a_crowd_of_decoys() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let mut files: Vec<(&str, Vec<u8>)> = vec![
            ("07zip.json", fixture_bytes("nodejs.json")),
            ("17zip.json", fixture_bytes("nodejs.json")),
            ("27zip.json", fixture_bytes("nodejs.json")),
            ("37zip.json", fixture_bytes("nodejs.json")),
            ("47zip.json", fixture_bytes("nodejs.json")),
            ("57zip.json", fixture_bytes("nodejs.json")),
        ];
        files.push(("7zip.json", fixture_bytes("7zip.json")));
        write_bucket(root.path(), &files);
        let scoop = Scoop::with_root(no_network(), root.path().to_path_buf(), None);

        let p = scoop.details("7zip").expect("7zip is in the bucket");
        assert_eq!(p.id, "7zip");
        assert_eq!(p.version.as_deref(), Some("26.03"));
        assert!(!p.installed);
    }

    /// The installed manifest is the answer when there is one: it carries
    /// the version actually on the machine, and the page draws the app as
    /// installed from it.
    #[test]
    fn details_answers_the_installed_manifest_first() {
        let root = tempfile::tempdir().expect("a temporary directory");
        write_bucket(root.path(), &[("7zip.json", fixture_bytes("7zip.json"))]);
        write_installed_app(
            root.path(),
            "7zip",
            br#"{"version":"25.00","description":"an older 7zip"}"#,
        );
        let scoop = Scoop::with_root(no_network(), root.path().to_path_buf(), None);

        let p = scoop.details("7zip").expect("7zip is installed");
        assert!(p.installed, "the installed manifest answers first");
        assert_eq!(p.installed_version.as_deref(), Some("25.00"));
        assert_eq!(p.version.as_deref(), Some("25.00"));
    }

    /// The error names the place that was actually searched: every bucket on
    /// disk when there are buckets, the main bucket on GitHub when there are
    /// none.
    #[test]
    fn details_names_the_place_it_looked_when_it_finds_nothing() {
        let root = tempfile::tempdir().expect("a temporary directory");
        write_bucket(root.path(), &[("7zip.json", fixture_bytes("7zip.json"))]);
        let local = Scoop::with_root(no_network(), root.path().to_path_buf(), None);
        let err = local
            .details("not-in-any-bucket")
            .expect_err("nothing of that name is on disk");
        assert!(
            err.message.contains("is not in any bucket Brokey can see"),
            "{err}"
        );

        let empty = tempfile::tempdir().expect("a temporary directory");
        let http = Arc::new(FakeHttp::answering(Vec::new()));
        let network = Scoop::with_root(http.clone(), empty.path().to_path_buf(), None);
        let err = network
            .details("not-in-main")
            .expect_err("nothing answers for it");
        assert!(
            err.message.contains("is not in Scoop's main bucket"),
            "{err}"
        );
        assert_eq!(
            http.asked(),
            vec![manifest_url("not-in-main")],
            "one request for the exact name, not a search"
        );
    }

    /// The second tree request must be for the tree the first one named. The
    /// root tree asked for twice would answer the repository's own top-level
    /// files as if they were manifests.
    #[test]
    fn the_second_tree_request_is_the_bucket_the_first_one_named() {
        let http = FakeHttp::answering(vec![
            (
                MASTER_TREE_URL.to_string(),
                tree_json(&[
                    ("bucket", "tree", "sha-bucket"),
                    ("README.md", "blob", "sha-readme"),
                ]),
            ),
            (
                bucket_tree_url("sha-bucket"),
                tree_json(&[
                    ("7zip.json", "blob", "sha-7zip"),
                    ("nodejs.json", "blob", "sha-nodejs"),
                ]),
            ),
        ]);

        let mut names = main_bucket_names(&http).expect("both trees answer");
        names.sort();
        assert_eq!(names, vec!["7zip".to_string(), "nodejs".to_string()]);
        assert_eq!(
            http.asked(),
            vec![MASTER_TREE_URL.to_string(), bucket_tree_url("sha-bucket")]
        );
    }

    /// GitHub answers a tree too large for one reply by sending fewer
    /// entries and setting `truncated`, so an unread flag would turn a
    /// partial bucket into a search that quietly misses packages.
    #[test]
    fn a_truncated_tree_is_an_error_not_a_short_bucket() {
        let truncated = format!(
            r#"{{"sha":"root","truncated":true,"tree":[{}]}}"#,
            r#"{"path":"bucket","type":"tree","sha":"sha-bucket"}"#
        );
        let http = FakeHttp::answering(vec![(MASTER_TREE_URL.to_string(), truncated.clone())]);
        let err = main_bucket_names(&http).expect_err("a truncated root tree is an error");
        assert!(err.message.contains("truncated"), "{err}");

        let http = FakeHttp::answering(vec![
            (
                MASTER_TREE_URL.to_string(),
                tree_json(&[("bucket", "tree", "sha-bucket")]),
            ),
            (bucket_tree_url("sha-bucket"), truncated),
        ]);
        let err = main_bucket_names(&http).expect_err("a truncated bucket tree is an error");
        assert!(err.message.contains("truncated"), "{err}");
    }

    /// A name that is not in Main answers 404 with an HTML body. That is a
    /// skip, never a panic: this runs on the search thread, and an unwrap
    /// here would take the whole search down with it.
    #[test]
    fn a_manifest_that_is_not_in_the_bucket_is_skipped_not_a_panic() {
        let http = FakeHttp::answering(vec![(
            manifest_url("ghost"),
            "<!DOCTYPE html><html>404: Not Found</html>".to_string(),
        )]);
        assert_eq!(fetch_manifest(&http, "ghost"), None, "an HTML 404 body");
        assert_eq!(
            fetch_manifest(&http, "unreachable"),
            None,
            "a request that fails outright"
        );
    }

    /// A manifest's address is built by interpolation, and `details` takes
    /// its id from its caller rather than from a bucket listing, so anything
    /// that is not one plain path segment is refused before a URL exists. A
    /// `..` would otherwise climb out of the `bucket/` segment and fetch
    /// another file from the same repository.
    #[test]
    fn an_id_that_is_not_a_plain_name_is_never_fetched() {
        let http = FakeHttp::answering(vec![(manifest_url("7zip"), fixture_text("7zip.json"))]);
        for id in [
            "..",
            "../../README",
            "a/../..",
            "bucket/7zip",
            r"bucket\7zip",
            "",
            "7zip?raw=1",
            "7 zip",
        ] {
            assert_eq!(fetch_manifest(&http, id), None, "{id:?} must be refused");
        }
        assert!(
            http.asked().is_empty(),
            "no address was built for any of them: {:?}",
            http.asked()
        );

        assert!(
            fetch_manifest(&http, "7zip").is_some(),
            "a real name is still fetched"
        );
        assert_eq!(http.asked(), vec![manifest_url("7zip")]);
    }

    /// The same guard where the caller's string actually arrives: `details`
    /// on a machine with no bucket on disk.
    #[test]
    fn details_never_builds_an_address_out_of_an_id_that_is_not_a_name() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let http = Arc::new(FakeHttp::answering(Vec::new()));
        let scoop = Scoop::with_root(http.clone(), root.path().to_path_buf(), None);

        for id in ["..", "../../README.md", r"..\..\README.md"] {
            assert!(scoop.details(id).is_err(), "{id:?} must not answer");
        }
        assert!(
            http.asked().is_empty(),
            "nothing was requested: {:?}",
            http.asked()
        );
    }

    /// The network path makes one request per match, run one after another,
    /// so the number of matches has to be capped: `Query::new` asks for 200
    /// and a short query matches hundreds of the bucket's names, which would
    /// be hundreds of round trips before the page drew a single row.
    #[test]
    fn a_network_search_fetches_at_most_twenty_five_manifests() {
        let names: Vec<String> = (0..200).map(|i| format!("tool{i:03}")).collect();
        let blobs: Vec<String> = names
            .iter()
            .map(|n| format!(r#"{{"path":"{n}.json","type":"blob","sha":"sha-{n}"}}"#))
            .collect();
        let replies = || {
            let mut replies = vec![
                (
                    MASTER_TREE_URL.to_string(),
                    tree_json(&[("bucket", "tree", "sha-bucket")]),
                ),
                (
                    bucket_tree_url("sha-bucket"),
                    format!(r#"{{"sha":"bucket","tree":[{}]}}"#, blobs.join(",")),
                ),
            ];
            for n in &names {
                replies.push((manifest_url(n), fixture_text("nodejs.json")));
            }
            FakeHttp::answering(replies)
        };

        let http = replies();
        let found = network_candidates(&http, "tool", 200).expect("the bucket answers");
        assert_eq!(found.len(), 25, "the cap, not the query's limit of 200");
        let fetched = http
            .asked()
            .iter()
            .filter(|u| u.starts_with("https://raw.githubusercontent.com"))
            .count();
        assert_eq!(fetched, 25, "one request per match, capped at 25");

        let http = replies();
        let found = network_candidates(&http, "tool", 3).expect("the bucket answers");
        assert_eq!(found.len(), 3, "a limit below the cap is still the limit");
    }

    /// Scoop installed but no bucket fetched is not an empty bucket: the
    /// search falls back to the main bucket on GitHub, the same as a machine
    /// without Scoop, rather than answering nothing and saying nothing. The
    /// shim is present here, so a source that chose its path by the shim
    /// would answer nothing.
    #[test]
    fn a_root_without_a_bucket_searches_github_instead() {
        let root = tempfile::tempdir().expect("a temporary directory");
        write_installed_app(root.path(), "7zip", &fixture_bytes("7zip.json"));
        let http = Arc::new(FakeHttp::answering(vec![
            (
                MASTER_TREE_URL.to_string(),
                tree_json(&[("bucket", "tree", "sha-bucket")]),
            ),
            (
                bucket_tree_url("sha-bucket"),
                tree_json(&[("nodejs.json", "blob", "sha-nodejs")]),
            ),
            (manifest_url("nodejs"), fixture_text("nodejs.json")),
        ]));
        let scoop = Scoop::with_root(
            http.clone(),
            root.path().to_path_buf(),
            Some(root.path().join("shims").join("scoop.cmd")),
        );

        let found = scoop.search(&Query::new("nodejs")).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].id, "nodejs");
        assert!(
            http.asked().contains(&manifest_url("nodejs")),
            "{:?}",
            http.asked()
        );
    }
}
