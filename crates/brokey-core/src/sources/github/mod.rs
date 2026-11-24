//! GitHub releases as a source, on both platforms. A search asks the public
//! API for repositories by name, then looks at each one's latest release
//! and keeps only those that publish something this machine can install.
//!
//! What that is, and how it is installed, is the one part that differs by
//! platform, and it lives in [`linux`] and [`windows`]: on Linux a package
//! for this distribution, an AppImage, a Flatpak bundle or an archive; on
//! Windows an MSI, an installer, a bare program or a portable zip. Both are
//! pure functions and are compiled on both platforms, so each platform's
//! choice is tested on the other as well. The source picks between them by
//! [`SystemInfo::platform`], which `system::detect` fills by `#[cfg]`; that
//! is also what lets a test build a Linux source on Windows and the other
//! way round. Everything else here, the API, the cache, the record of what
//! was installed and the version order of updates, is shared.
//!
//! The API is used without a token: ten searches a minute, and sixty other
//! requests an hour, which the release look-ups count against. Answers are
//! kept on disk for a while so that repeating a search or opening a result
//! costs nothing. A token setting would raise the limit and is future work.
//!
//! What this source installs is recorded nowhere else on the machine, so it
//! keeps its own list in `github-installs.json` under the data directory.
//! The runner tells the source when a plan has finished
//! ([`Source::finished`]), and a successful install or update is written
//! there then, a removal forgotten. An Arch package installed this way is
//! foreign to pacman, so the record also keeps the name pacman knows it by,
//! and `linux::pacman_names_installed_by_github` is how the AUR source
//! learns not to claim it. A portable application unpacked on Windows has
//! no record anywhere else either, so the folder it went into is kept too,
//! for whatever needs to find it again.
//!
//! `plan` does no network: it builds steps from the release a search or a
//! details call already fetched, so a plan for a repository nobody has
//! looked at says so instead of quietly asking the API.

pub mod linux;
pub mod windows;

#[cfg(unix)]
pub use linux::pacman_names_installed_by_github;

use crate::appstream::Catalogue;
use crate::http::Client;
use crate::model::*;
use crate::system::{self, Dirs};
use crate::{Error, Op, Query, Result, Source};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const API: &str = "https://api.github.com";
const HEADERS: [(&str, &str); 2] = [
    ("Accept", "application/vnd.github+json"),
    ("X-GitHub-Api-Version", "2022-11-28"),
];
/// A repeated search within this window costs nothing against the limit.
const SEARCH_MAX_AGE: Duration = Duration::from_secs(120);
/// Releases change rarely; half an hour keeps a session of browsing cheap.
const RELEASE_MAX_AGE: Duration = Duration::from_secs(30 * 60);
/// How many of a search's repositories get a release look-up.
const LOOKUPS: usize = 8;
pub const RATE_LIMITED: &str = "GitHub is rate-limiting searches; try again in a minute.";

/// A repository as the search and repository endpoints describe it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Repo {
    pub full_name: String,
    pub name: String,
    pub description: Option<String>,
    pub html_url: String,
    pub owner: Owner,
    pub stargazers_count: u64,
    pub pushed_at: Option<String>,
    pub license: Option<Licence>,
    pub topics: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Owner {
    pub login: String,
    pub avatar_url: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Licence {
    pub spdx_id: Option<String>,
}

/// One release, as `releases/latest` describes it.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Release {
    pub tag_name: String,
    pub name: Option<String>,
    pub published_at: Option<String>,
    pub html_url: String,
    pub body: Option<String>,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub browser_download_url: String,
    /// `"sha256:<hex>"`, which GitHub computes for every asset uploaded
    /// since June 2025 and leaves out (or null) for older ones.
    pub digest: Option<String>,
}

impl Asset {
    /// The SHA-256 GitHub published for this asset, as 64 lower-case hex
    /// digits, or `None` when there is none or it is not that shape. The
    /// shape is checked because the digits become an argument of the step
    /// that checks the download.
    pub fn sha256(&self) -> Option<String> {
        let hex = self.digest.as_deref()?.trim().strip_prefix("sha256:")?;
        (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hex.to_ascii_lowercase())
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct SearchAnswer {
    items: Vec<Repo>,
}

/// What a release asset is, judged from its file name by
/// [`linux::kind_of`] or [`windows::kind_of`]. One enum for both platforms
/// because it is written into `github-installs.json`, and a record means
/// the same thing whichever build reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetKind {
    AppImage,
    Deb,
    Rpm,
    /// `.pkg.tar.zst`, or `.pacman` as electron-builder names the same thing.
    Pacman,
    Flatpak,
    /// A `.tar.gz`, `.tar.xz` or `.zip` with "linux" in its name. Listed so
    /// the repository is found; not installable by a plan.
    Archive,
    /// A Windows Installer package, installed by `msiexec`.
    Msi,
    /// An `.exe` whose name says it is a setup or an installer: the
    /// publisher's own, run in its own window.
    Installer,
    /// Any other `.exe`: the application itself, copied into a folder of
    /// its own.
    Exe,
    /// A `.zip` for Windows, unpacked as a portable application.
    Zip,
}

impl AssetKind {
    /// For sentences: "a Debian package".
    pub fn label(self) -> &'static str {
        match self {
            AssetKind::AppImage => "an AppImage",
            AssetKind::Deb => "a Debian package",
            AssetKind::Rpm => "an RPM package",
            AssetKind::Pacman => "an Arch package",
            AssetKind::Flatpak => "a Flatpak bundle",
            AssetKind::Archive => "an archive",
            AssetKind::Msi => "a Windows Installer package",
            AssetKind::Installer => "an installer",
            AssetKind::Exe => "a program",
            AssetKind::Zip => "a portable application",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
    /// 32-bit x86. Only [`windows::arch_of`] answers it, because Windows
    /// runs such a build on either of the other two; on Linux a 32-bit name
    /// is one that does not say.
    X86,
}

impl Arch {
    /// From `SystemInfo::arch`, which is Rust's spelling.
    pub fn of_machine(arch: &str) -> Option<Arch> {
        match arch {
            "x86_64" => Some(Arch::X86_64),
            "aarch64" => Some(Arch::Aarch64),
            "x86" => Some(Arch::X86),
            _ => None,
        }
    }
}

/// The asset a plan would download, and what was decided about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chosen {
    pub asset: Asset,
    pub kind: AssetKind,
    pub arch: Option<Arch>,
}

/// "v1.2.3" and "V1.2.3" to "1.2.3"; other tags as they are.
pub fn strip_v(tag: &str) -> String {
    let t = tag.trim();
    match t.strip_prefix(['v', 'V']) {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest.to_string(),
        _ => t.to_string(),
    }
}

/// "2026-09-09T16:19:01Z" to unix seconds. GitHub always answers in UTC
/// with a Z, so no zone parsing; anything else is `None`.
pub fn parse_time(s: &str) -> Option<i64> {
    let (date, time) = s.trim().trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.split(':').map(|p| p.parse::<i64>());
    let (h, min, sec) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from civil, Howard Hinnant's algorithm.
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3_600 + min * 60 + sec)
}

/// "12 171" with a thin space, per the copy rules.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{2009}');
        }
        out.push(c);
    }
    out
}

/// GitHub's two ways of saying "too many requests".
pub fn rate_limited(status: u16, remaining: Option<&str>) -> bool {
    status == 429 || (status == 403 && remaining.map(str::trim) == Some("0"))
}

/// A search term as the `q` parameter: spaces become `+`, everything
/// outside the unreserved set is percent-encoded.
pub fn encode(term: &str) -> String {
    let mut out = String::new();
    for b in term.trim().bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Release notes for the page, which sanitises to `<p>`, `<ul>`, `<li>`,
/// `<em>` and `<code>`: paragraphs on blank lines, a run of `- ` lines as a
/// list, everything else escaped. Rendering the rest of Markdown is future
/// work; this keeps a changelog readable rather than one long line.
pub fn notes_markup(body: &str) -> String {
    let text = body.replace("\r\n", "\n");
    let escape = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let mut out = String::new();
    for block in text.split("\n\n").map(str::trim).filter(|b| !b.is_empty()) {
        let lines: Vec<&str> = block.lines().map(str::trim).collect();
        let items: Vec<&str> = lines
            .iter()
            .filter_map(|l| l.strip_prefix("- ").or_else(|| l.strip_prefix("* ")))
            .collect();
        if !items.is_empty() && items.len() == lines.len() {
            out.push_str("<ul>");
            for item in items {
                out.push_str(&format!("<li>{}</li>", escape(item)));
            }
            out.push_str("</ul>");
        } else {
            out.push_str(&format!("<p>{}</p>", escape(&lines.join(" "))));
        }
    }
    out
}

/// One install this source made, as `github-installs.json` records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRecord {
    /// "owner/repo".
    pub repo: String,
    pub name: String,
    pub asset: String,
    /// The release tag with its leading v stripped.
    pub version: String,
    pub kind: AssetKind,
    /// For an Arch package, the name pacman installed it under, read from
    /// the downloaded file's `.PKGINFO` once the install worked. `None` for
    /// every other kind, and in a record written before this was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkgname: Option<String>,
    /// For a portable application on Windows (a zip or a bare program),
    /// the folder it was put in: [`windows::portable_dir`], written once
    /// the install worked. Nothing else on the machine knows where it is,
    /// so this is what finds it again. `None` for every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// The name in `<data>` the records are kept under.
pub fn records_path(data: &Path) -> PathBuf {
    data.join("github-installs.json")
}

/// Every install recorded in the file at `path`. A file that is missing is
/// no installs; one that will not parse is logged and treated as empty
/// rather than failing every listing.
pub fn read_records(path: &Path) -> Vec<InstallRecord> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str(&text) {
        Ok(records) => records,
        Err(e) => {
            log::warn!("{} is not readable and is ignored: {e}", path.display());
            Vec::new()
        }
    }
}

/// The package for a repository whose latest release has an asset this
/// machine can install.
pub fn package(repo: &Repo, release: &Release, chosen: &Chosen, catalogue: &Catalogue) -> Package {
    let mut p = Package::new(SourceKind::Github, &repo.full_name, &repo.name);
    p.kind = PackageKind::App;
    p.summary = repo.description.clone();
    p.homepage = Some(repo.html_url.clone());
    p.developer = Some(repo.owner.login.clone());
    p.version = Some(strip_v(&release.tag_name));
    p.updated = repo.pushed_at.as_deref().and_then(parse_time);
    p.download_size = Some(chosen.asset.size);
    p.popularity = Some((repo.stargazers_count as f64 / 20_000.0).min(1.0));
    p.popularity_label = Some(format!("{} stars", thousands(repo.stargazers_count)));
    p.licence = repo
        .license
        .as_ref()
        .and_then(|l| l.spdx_id.clone())
        .filter(|id| id != "NOASSERTION");
    p.icon = Some(Picture::Url(repo.owner.avatar_url.clone()));
    // The catalogue's icon is the application's, the avatar is the
    // author's. Only the icon is taken: setting `appstream_id` from a name
    // match would let the grouper join editions as if it were certain.
    if let Some(component) = catalogue.by_pkgname(&repo.name.to_lowercase())
        && let Some(icon) = &component.icon
    {
        p.icon = Some(icon.clone());
    }
    p.facts = vec![
        ("Stars".to_string(), thousands(repo.stargazers_count)),
        ("Latest release".to_string(), release.tag_name.clone()),
        ("Asset".to_string(), chosen.asset.name.clone()),
    ];
    if let Some(published) = &release.published_at {
        p.facts.push((
            "Published".to_string(),
            published.chars().take(10).collect(),
        ));
    }
    if !repo.topics.is_empty() {
        p.facts.push(("Topics".to_string(), repo.topics.join(", ")));
    }
    let install = match chosen.kind {
        AssetKind::AppImage => Some("The AppImage is placed in ~/.local/bin; no menu entry yet."),
        AssetKind::Msi => Some(
            "Windows Installer installs it, and asks for permission itself if the package is for every user.",
        ),
        AssetKind::Installer => {
            Some("The publisher's installer opens in its own window; finish it there.")
        }
        AssetKind::Exe | AssetKind::Zip => Some(
            "Placed in your Programs folder under Brokey, for you alone; no Start menu entry yet.",
        ),
        _ => None,
    };
    if let Some(install) = install {
        p.facts.push(("Install".to_string(), install.to_string()));
    }
    p
}

/// The last component of a name GitHub gave a file, so that a name with a
/// path in it cannot place the download anywhere but where it was meant.
fn file_name(asset: &str) -> String {
    Path::new(asset)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "release".to_string())
}

fn step(title: String, program: &str, args: Vec<String>, needs_root: bool, weight: u32) -> Step {
    Step {
        source: SourceKind::Github,
        title,
        command: Command {
            program: program.to_string(),
            args,
            env: Vec::new(),
            cwd: None,
        },
        needs_root,
        weight,
    }
}

/// The package for something already installed from a record, with no
/// network: what the Installed page draws.
pub fn record_package(record: &InstallRecord) -> Package {
    let mut p = Package::new(SourceKind::Github, &record.repo, &record.name);
    p.kind = PackageKind::App;
    p.installed = true;
    p.installed_version = Some(record.version.clone());
    p.version = Some(record.version.clone());
    p.homepage = Some(format!("https://github.com/{}", record.repo));
    p.developer = record.repo.split('/').next().map(str::to_string);
    p.facts = vec![
        ("Asset".to_string(), record.asset.clone()),
        ("Installed as".to_string(), record.kind.label().to_string()),
    ];
    p
}

fn err(message: impl Into<String>) -> Error {
    Error::from_source(SourceKind::Github, message)
}

pub struct Github {
    client: Arc<Client>,
    catalogue: Arc<Catalogue>,
    /// Which of [`linux`] and [`windows`] chooses and installs.
    platform: Platform,
    machine: Option<Arch>,
    /// The distribution's own package kind; always `None` on Windows.
    native: Option<AssetKind>,
    /// `<data>/github-installs.json`.
    installs: PathBuf,
    /// `<cache>/downloads`, where curl puts the asset.
    downloads: PathBuf,
    /// `<cache>/github`, API answers by URL.
    cache: PathBuf,
    /// Where a portable application goes on Windows:
    /// [`windows::programs_dir`] on a real machine. `None` when Windows
    /// names no profile folder, and unused on Linux.
    programs: Option<PathBuf>,
    /// Latest releases seen by search, details and updates, so a plan can
    /// be built without the network.
    known: Mutex<HashMap<String, Release>>,
}

impl Github {
    pub fn new(system: &SystemInfo, client: Arc<Client>, catalogue: Arc<Catalogue>) -> Github {
        let dirs = Dirs::new();
        let mut github = Github::with_dirs(system, client, catalogue, &dirs.data, &dirs.cache);
        github.programs = windows::programs_dir();
        github
    }

    /// The same source with its files somewhere else, for tests. Portable
    /// applications go under `<data>/Programs`, so a test never writes into
    /// the user's real profile.
    pub fn with_dirs(
        system: &SystemInfo,
        client: Arc<Client>,
        catalogue: Arc<Catalogue>,
        data: &Path,
        cache: &Path,
    ) -> Github {
        Github {
            client,
            catalogue,
            platform: system.platform,
            machine: Arch::of_machine(&system.arch),
            native: match system.platform {
                Platform::Linux => linux::native(system),
                Platform::Windows => None,
            },
            installs: records_path(data),
            downloads: cache.join("downloads"),
            cache: cache.join("github"),
            programs: Some(data.join("Programs")),
            known: Mutex::new(HashMap::new()),
        }
    }

    /// Where a portable application goes, or the sentence that says why
    /// there is nowhere.
    fn programs(&self) -> Result<&Path> {
        self.programs.as_deref().ok_or_else(|| {
            err("Windows did not say where this user's local application folder is, so nothing can be unpacked into it.")
        })
    }

    /// Every install this source recorded; see [`read_records`].
    pub fn records(&self) -> Vec<InstallRecord> {
        read_records(&self.installs)
    }

    /// Remember an install so `installed()` and `updates()` know about it.
    /// [`Source::finished`] calls it after a plan that worked.
    pub fn record_install(&self, record: InstallRecord) -> Result<()> {
        let mut records = self.records();
        records.retain(|r| r.repo != record.repo);
        records.push(record);
        self.write_records(&records)
    }

    /// Forget an install after a successful removal.
    pub fn forget_install(&self, repo: &str) -> Result<()> {
        let mut records = self.records();
        records.retain(|r| r.repo != repo);
        self.write_records(&records)
    }

    fn write_records(&self, records: &[InstallRecord]) -> Result<()> {
        if let Some(parent) = self.installs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(records).map_err(|e| err(e.to_string()))?;
        std::fs::write(&self.installs, text)?;
        Ok(())
    }

    /// Make a release known to `plan` without the network, as search and
    /// details do.
    pub fn remember_release(&self, repo: &str, release: Release) {
        if let Ok(mut known) = self.known.lock() {
            known.insert(repo.to_string(), release);
        }
    }

    fn cache_path(&self, url: &str) -> PathBuf {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in url.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        self.cache.join(format!("{h:016x}.json"))
    }

    /// A cached answer younger than `max_age`; `Some("")` records a 404.
    fn cached(&self, url: &str, max_age: Option<Duration>) -> Option<String> {
        let path = self.cache_path(url);
        if let Some(max_age) = max_age {
            let age = std::fs::metadata(&path)
                .ok()?
                .modified()
                .ok()?
                .elapsed()
                .ok()?;
            if age >= max_age {
                return None;
            }
        }
        std::fs::read_to_string(&path).ok()
    }

    fn store(&self, url: &str, text: &str) {
        let path = self.cache_path(url);
        let _ = std::fs::create_dir_all(&self.cache);
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// GET through the cache. `None` is a 404, which is also cached: most
    /// repositories have no release, and asking again each search would
    /// spend the hourly limit on the same "no". The raw client rather than
    /// `Client::get_json` because a rate limit is a 403 whose meaning is in
    /// the `X-RateLimit-Remaining` header, and `get_json` folds every
    /// non-success status into one sentence.
    fn fetch(&self, url: &str, max_age: Duration) -> Result<Option<String>> {
        if let Some(text) = self.cached(url, Some(max_age)) {
            return Ok((!text.is_empty()).then_some(text));
        }
        let mut req = self.client.raw().get(url);
        for (k, v) in HEADERS {
            req = req.header(k, v);
        }
        let resp = req.send().map_err(|e| {
            err(if e.is_timeout() {
                "GitHub did not answer in time; try again.".to_string()
            } else if e.is_connect() {
                "Could not reach GitHub; check the connection.".to_string()
            } else {
                format!("GitHub: {e}.")
            })
        })?;
        let status = resp.status().as_u16();
        let remaining = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if rate_limited(status, remaining.as_deref()) {
            return Err(err(RATE_LIMITED));
        }
        if status == 404 {
            self.store(url, "");
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            return Err(err(format!("GitHub answered {status}; try again later.")));
        }
        let text = resp.text().map_err(|e| err(format!("GitHub: {e}.")))?;
        self.store(url, &text);
        Ok(Some(text))
    }

    fn fetch_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        max_age: Duration,
    ) -> Result<Option<T>> {
        match self.fetch(url, max_age)? {
            Some(text) => parse(&text).map(Some),
            None => Ok(None),
        }
    }

    fn release_url(repo: &str) -> String {
        format!("{API}/repos/{repo}/releases/latest")
    }

    /// The latest release, or `None` when the repository has none.
    fn latest_release(&self, repo: &str) -> Result<Option<Release>> {
        let release: Option<Release> =
            self.fetch_json(&Self::release_url(repo), RELEASE_MAX_AGE)?;
        if let Some(r) = &release {
            self.remember_release(repo, r.clone());
        }
        Ok(release)
    }

    /// The release `plan` can use: what this session saw, else what is on
    /// disk from an earlier one, at any age. Never the network.
    fn known_release(&self, repo: &str) -> Result<Release> {
        if let Ok(known) = self.known.lock()
            && let Some(r) = known.get(repo)
        {
            return Ok(r.clone());
        }
        if let Some(text) = self.cached(&Self::release_url(repo), None)
            && !text.is_empty()
        {
            return parse(&text);
        }
        Err(err(format!(
            "The latest release of {repo} is not known yet; search for it or open its page first."
        )))
    }

    fn choose(&self, release: &Release) -> Option<Chosen> {
        match self.platform {
            Platform::Linux => linux::pick_asset(&release.assets, self.machine, self.native),
            Platform::Windows => windows::pick_asset(&release.assets, self.machine),
        }
    }

    fn mark_installed(&self, p: &mut Package, records: &[InstallRecord]) {
        if let Some(r) = records.iter().find(|r| r.repo == p.id) {
            p.installed = true;
            p.installed_version = Some(r.version.clone());
        }
    }

    fn install_plan(&self, repo: &str, release: &Release) -> Result<Vec<Step>> {
        let chosen = self.choose(release).ok_or_else(|| {
            err(format!(
                "The latest release of {repo} has no file this machine can install."
            ))
        })?;
        let name = repo.rsplit('/').next().unwrap_or(repo);
        match self.platform {
            Platform::Linux => linux::install_steps(&linux::InstallTarget {
                name,
                chosen: &chosen,
                downloads: &self.downloads,
                home: &home(),
                native: self.native,
                has_curl: system::which("curl").is_some(),
                has_libfuse2: linux::has_libfuse2(),
            }),
            Platform::Windows => {
                // The id becomes two folder names here, not only a URL.
                if !valid_repo_id(repo) {
                    return Err(err(format!(
                        "{repo} is not a GitHub repository name (owner/repo)."
                    )));
                }
                if system::which("curl").is_none() {
                    return Err(err(
                        "curl.exe, which Windows has included since version 1803, is not on this machine, so GitHub releases cannot be downloaded.",
                    ));
                }
                windows::install_steps(&windows::InstallTarget {
                    name,
                    repo,
                    chosen: &chosen,
                    downloads: &self.downloads,
                    programs: self.programs()?,
                    msiexec: &crate::sources::windows::arp::msiexec_program(),
                })
            }
        }
    }

    fn remove_plan(&self, record: &InstallRecord) -> Result<Vec<Step>> {
        match self.platform {
            Platform::Linux => linux::remove_steps(record, &home()),
            Platform::Windows => {
                if !valid_repo_id(&record.repo) {
                    return Err(err(format!(
                        "{} is not a GitHub repository name (owner/repo).",
                        record.repo
                    )));
                }
                let present = record.kind == AssetKind::Msi
                    && windows::download_path(&self.downloads, &record.repo, &record.asset)
                        .is_file();
                windows::remove_steps(
                    record,
                    &self.downloads,
                    self.programs()?,
                    &crate::sources::windows::arp::msiexec_program(),
                    present,
                )
            }
        }
    }
}

/// `$HOME`, or `/tmp` when it is not set. Linux only in meaning.
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn parse<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|e| {
        err(format!(
            "GitHub sent something that was not the expected JSON: {e}."
        ))
    })
}

/// The search endpoint's answer, parsed. Public so fixtures can feed it.
pub fn parse_search(text: &str) -> Result<Vec<Repo>> {
    parse::<SearchAnswer>(text).map(|a| a.items)
}

pub fn parse_release(text: &str) -> Result<Release> {
    parse(text)
}

pub fn parse_repo(text: &str) -> Result<Repo> {
    parse(text)
}

fn valid_repo_id(id: &str) -> bool {
    let mut parts = id.split('/');
    // A dot is legal in a name, so ".." would pass a character test alone;
    // every part needs a letter or digit before it is used in a URL.
    let ok = |s: &str| {
        s.chars().any(|c| c.is_ascii_alphanumeric())
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    matches!((parts.next(), parts.next(), parts.next()), (Some(o), Some(r), None) if ok(o) && ok(r))
}

impl Source for Github {
    fn kind(&self) -> SourceKind {
        SourceKind::Github
    }

    fn status(&self) -> SourceStatus {
        SourceStatus {
            kind: SourceKind::Github,
            available: true,
            reason: None,
            detail: Some("public API, 10 searches a minute without a token".to_string()),
            searchable: false,
            setup: None,
        }
    }

    fn search(&self, query: &Query) -> Result<Vec<Package>> {
        let term = query.text.trim();
        if term.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!(
            "{API}/search/repositories?q={}+in:name&sort=stars&per_page=15",
            encode(term)
        );
        let Some(text) = self.fetch(&url, SEARCH_MAX_AGE)? else {
            return Ok(Vec::new());
        };
        let repos: Vec<Repo> = parse_search(&text)?
            .into_iter()
            .take(LOOKUPS.min(query.limit.max(1)))
            .collect();
        // One look-up per repository, at once: eight sequential round trips
        // would make every search a few seconds long.
        let looked: Vec<(Repo, Result<Option<Release>>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = repos
                .into_iter()
                .map(|repo| {
                    scope.spawn(move || {
                        let release = self.latest_release(&repo.full_name);
                        (repo, release)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("a release look-up panicked"))
                .collect()
        });
        let records = self.records();
        let mut out = Vec::new();
        for (repo, result) in looked {
            let release = match result {
                Ok(Some(release)) => release,
                Ok(None) => continue,
                Err(e) if e.message == RATE_LIMITED => return Err(e),
                Err(e) => {
                    log::warn!("{}: {}", repo.full_name, e.message);
                    continue;
                }
            };
            let Some(chosen) = self.choose(&release) else {
                continue;
            };
            let mut p = package(&repo, &release, &chosen, &self.catalogue);
            self.mark_installed(&mut p, &records);
            out.push(p);
        }
        Ok(out)
    }

    fn installed(&self) -> Result<Vec<Package>> {
        Ok(self.records().iter().map(record_package).collect())
    }

    /// One release look-up per recorded install; a newer tag by pacman's
    /// version order is an update.
    fn updates(&self) -> Result<Vec<Update>> {
        let mut out = Vec::new();
        for record in self.records() {
            let Some(release) = self.latest_release(&record.repo)? else {
                continue;
            };
            let to = strip_v(&release.tag_name);
            if crate::vercmp::vercmp(&to, &record.version) != std::cmp::Ordering::Greater {
                continue;
            }
            out.push(Update {
                package: PackageRef {
                    source: SourceKind::Github,
                    id: record.repo.clone(),
                },
                name: record.name.clone(),
                kind: PackageKind::App,
                summary: release.name.clone(),
                icon: None,
                from: Some(record.version.clone()),
                to,
                download_size: self.choose(&release).map(|c| c.asset.size),
                published: release.published_at.as_deref().and_then(parse_time),
                is_self: false,
            });
        }
        Ok(out)
    }

    fn details(&self, id: &str) -> Result<Package> {
        if !valid_repo_id(id) {
            return Err(err(format!(
                "{id} is not a GitHub repository name (owner/repo)."
            )));
        }
        let repo: Repo = self
            .fetch_json(&format!("{API}/repos/{id}"), RELEASE_MAX_AGE)?
            .ok_or_else(|| err(format!("{id} is not a repository on GitHub.")))?;
        let release = self.latest_release(id)?.ok_or_else(|| {
            err(format!(
                "{id} has no published release, so there is nothing to install."
            ))
        })?;
        let chosen = self.choose(&release).ok_or_else(|| {
            err(format!(
                "The latest release of {id} has no file this machine can install."
            ))
        })?;
        let mut p = package(&repo, &release, &chosen, &self.catalogue);
        p.description = release
            .body
            .as_deref()
            .filter(|b| !b.trim().is_empty())
            .map(notes_markup);
        self.mark_installed(&mut p, &self.records());
        Ok(p)
    }

    /// Only an AppImage this source placed is opened from here: a package
    /// it installed through pacman, apt or dnf belongs to that source's
    /// record, under a name the release does not state.
    #[cfg(unix)]
    fn launcher(&self, id: &str) -> Option<crate::launch::Launch> {
        let record = self.records().into_iter().find(|r| r.repo == id)?;
        if record.kind != AssetKind::AppImage {
            return None;
        }
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        let path = linux::appimage_path(&home, &record.name);
        path.is_file().then(|| {
            crate::launch::Launch::Command(crate::launch::command(path.display().to_string(), &[]))
        })
    }

    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        match op {
            Op::Install { package } | Op::Update { package } => {
                let release = self.known_release(&package.id)?;
                self.install_plan(&package.id, &release)
            }
            Op::UpdateAll { .. } => {
                let mut steps = Vec::new();
                for record in self.records() {
                    let Ok(release) = self.known_release(&record.repo) else {
                        continue;
                    };
                    if crate::vercmp::vercmp(&strip_v(&release.tag_name), &record.version)
                        == std::cmp::Ordering::Greater
                    {
                        steps.extend(self.install_plan(&record.repo, &release)?);
                    }
                }
                Ok(steps)
            }
            Op::Remove { package } => {
                let record = self
                    .records()
                    .into_iter()
                    .find(|r| r.repo == package.id)
                    .ok_or_else(|| {
                        err(format!(
                            "{} was not installed by Brokey from GitHub.",
                            package.id
                        ))
                    })?;
                self.remove_plan(&record)
            }
            // The planner expands a setup through `Source::setup`.
            Op::Refresh { .. } | Op::Setup { .. } => Ok(Vec::new()),
            // Cleanup is planned by the planner, not by a source.
            Op::ClearLeftovers { .. }
            | Op::Restore { .. }
            | Op::EmptyHolding { .. }
            | Op::ForceRemove { .. } => Ok(Vec::new()),
        }
    }

    /// The record this source keeps of what it installed is written here,
    /// after the runner says the plan worked, from the same release the plan
    /// was built from. A failed plan leaves the record as it was.
    fn finished(&self, op: &Op, ok: bool) {
        if !ok {
            return;
        }
        let outcome = match op {
            Op::Install { package } | Op::Update { package } => {
                let Ok(release) = self.known_release(&package.id) else {
                    return;
                };
                let Some(chosen) = self.choose(&release) else {
                    return;
                };
                // The file is still where the plan downloaded it. Reading
                // its metadata runs nothing; it is how a `.pacman` file is
                // known by the name pacman gave it.
                #[cfg(unix)]
                let pkgname = (chosen.kind == AssetKind::Pacman)
                    .then(|| {
                        crate::sources::linux::alpmdb::package_file_name(&linux::download_path(
                            &self.downloads,
                            &chosen.asset.name,
                        ))
                        .or_else(|| linux::pkgname_from_file_name(&chosen.asset.name))
                    })
                    .flatten();
                #[cfg(not(unix))]
                let pkgname = None;
                // Where a portable application went, for whatever needs to
                // find it again: nothing else on the machine records it.
                let path = match (chosen.kind, &self.programs) {
                    (AssetKind::Zip | AssetKind::Exe, Some(programs)) => Some(
                        windows::portable_dir(programs, &package.id)
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    _ => None,
                };
                self.record_install(InstallRecord {
                    repo: package.id.clone(),
                    name: package
                        .id
                        .rsplit('/')
                        .next()
                        .unwrap_or(&package.id)
                        .to_string(),
                    asset: chosen.asset.name.clone(),
                    version: strip_v(&release.tag_name),
                    kind: chosen.kind,
                    pkgname,
                    path,
                })
            }
            Op::Remove { package } => self.forget_install(&package.id),
            Op::UpdateAll { .. }
            | Op::Refresh { .. }
            | Op::Setup { .. }
            | Op::ClearLeftovers { .. }
            | Op::Restore { .. }
            | Op::EmptyHolding { .. }
            | Op::ForceRemove { .. } => Ok(()),
        };
        if let Err(e) = outcome {
            log::warn!("GitHub could not update its install record: {}", e.message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::collections::HashSet;

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.to_string(),
            size: 1,
            browser_download_url: format!("https://example.invalid/{name}"),
            digest: None,
        }
    }

    #[test]
    fn machines_by_rusts_spelling() {
        assert_eq!(Arch::of_machine("x86_64"), Some(Arch::X86_64));
        assert_eq!(Arch::of_machine("aarch64"), Some(Arch::Aarch64));
        assert_eq!(Arch::of_machine("riscv64"), None);
    }

    #[test]
    fn tags_lose_their_v() {
        assert_eq!(strip_v("v2.22.1"), "2.22.1");
        assert_eq!(strip_v("V1.0"), "1.0");
        assert_eq!(strip_v("2.22.1"), "2.22.1");
        assert_eq!(strip_v("vulkan-1"), "vulkan-1");
    }

    #[test]
    fn times_and_numbers() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2026-08-09T11:42:30Z"), Some(1_786_275_750));
        assert_eq!(parse_time("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(parse_time("yesterday"), None);
        assert_eq!(thousands(12_171), "12\u{2009}171");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000_000), "1\u{2009}000\u{2009}000");
    }

    #[test]
    fn search_terms_are_encoded() {
        assert_eq!(encode("heroic"), "heroic");
        assert_eq!(encode("games launcher"), "games+launcher");
        assert_eq!(encode("c++ ide"), "c%2B%2B+ide");
    }

    #[test]
    fn release_notes_become_paragraphs_and_lists() {
        let m = notes_markup("Hi!\r\n\r\n- fixed <a>\r\n- added b\r\n\r\nSee & enjoy");
        assert_eq!(
            m,
            "<p>Hi!</p><ul><li>fixed &lt;a&gt;</li><li>added b</li></ul><p>See &amp; enjoy</p>"
        );
    }

    #[test]
    fn repository_ids_are_owner_slash_repo() {
        assert!(valid_repo_id("neovim/neovim"));
        assert!(valid_repo_id("Heroic-Games-Launcher/HeroicGamesLauncher"));
        assert!(!valid_repo_id("neovim"));
        assert!(!valid_repo_id("a/b/c"));
        assert!(!valid_repo_id("../etc"));
    }

    #[test]
    fn a_digest_is_used_only_when_it_is_a_sha256() {
        let with = |digest: Option<&str>| Asset {
            digest: digest.map(str::to_string),
            ..asset("tool.AppImage")
        };
        let hex = "66ED041A93AC2817B744D3D0985194ADFA408C8D35F64D9A8967FA5E2A58F2C1";
        assert_eq!(
            with(Some(&format!("sha256:{hex}"))).sha256(),
            Some(hex.to_ascii_lowercase())
        );
        assert_eq!(with(None).sha256(), None);
        assert_eq!(with(Some(&format!("sha512:{hex}"))).sha256(), None);
        assert_eq!(with(Some("sha256:abc")).sha256(), None);
        assert_eq!(
            with(Some(&format!("sha256:{}", "g".repeat(64)))).sha256(),
            None
        );
        assert_eq!(
            with(Some(&format!("sha256:$(reboot){}", &hex[9..]))).sha256(),
            None,
            "only hex digits ever reach the check's argv"
        );
        // GitHub sends null for an asset uploaded before digests existed.
        let old: Asset = serde_json::from_str(
            r#"{"name":"a.AppImage","size":1,"browser_download_url":"u","digest":null}"#,
        )
        .unwrap();
        assert_eq!(old.sha256(), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_record_written_before_pkgname_existed_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            records_path(dir.path()),
            r#"[{"repo":"o/brokey","name":"brokey","asset":"brokey-0.1.5-1-x86_64.pkg.tar.zst","version":"0.1.5","kind":"pacman"}]"#,
        )
        .unwrap();
        let records = read_records(&records_path(dir.path()));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].pkgname, None);
        assert_eq!(
            pacman_names_installed_by_github(dir.path()),
            HashSet::from(["brokey".to_string()])
        );
        assert!(pacman_names_installed_by_github(&dir.path().join("nowhere")).is_empty());
        // A record without a name leaves the key out rather than writing null.
        let text = serde_json::to_string(&records[0]).unwrap();
        assert!(!text.contains("pkgname"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn a_finished_install_records_the_name_pacman_gave_the_package() {
        let dir = tempfile::tempdir().unwrap();
        let mut system = crate::system::from_os_release("ID=cachyos\nID_LIKE=arch\n");
        system.arch = "x86_64".to_string();
        let source = Github::with_dirs(
            &system,
            Arc::new(Client::new(dir.path().join("http"))),
            Arc::new(Catalogue::default()),
            &dir.path().join("data"),
            &dir.path().join("cache"),
        );
        let release = Release {
            tag_name: "v2.22.1".to_string(),
            assets: vec![asset("Heroic-2.22.1-linux-x64.pacman")],
            ..Release::default()
        };
        source.remember_release("Heroic-Games-Launcher/HeroicGamesLauncher", release);
        // The file as the plan's download left it: fpm's xz, .PKGINFO first.
        let pkginfo = "pkgname = heroic\npkgver = 2.22.1-1\n";
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(pkginfo.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, ".PKGINFO", pkginfo.as_bytes())
            .unwrap();
        let tar = builder.into_inner().unwrap();
        let mut xz = Vec::new();
        lzma_rs::xz_compress(&mut &tar[..], &mut xz).unwrap();
        let downloads = dir.path().join("cache/downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        std::fs::write(downloads.join("Heroic-2.22.1-linux-x64.pacman"), xz).unwrap();

        let op = Op::Install {
            package: PackageRef {
                source: SourceKind::Github,
                id: "Heroic-Games-Launcher/HeroicGamesLauncher".to_string(),
            },
        };
        source.finished(&op, true);
        let records = source.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind, AssetKind::Pacman);
        assert_eq!(records[0].pkgname.as_deref(), Some("heroic"));
        assert_eq!(
            pacman_names_installed_by_github(&dir.path().join("data")),
            HashSet::from(["heroic".to_string()])
        );
    }
}
