//! GitHub releases as a source. A search asks the public API for
//! repositories by name, then looks at each one's latest release and keeps
//! only those that publish something a Linux machine can install: a
//! package for this distribution, an AppImage, a Flatpak bundle, or a
//! Linux archive as a last resort.
//!
//! The API is used without a token: ten searches a minute, and sixty other
//! requests an hour, which the release look-ups count against. Answers are
//! kept on disk for a while so that repeating a search or opening a result
//! costs nothing. A token setting would raise the limit and is future work.
//!
//! What this source installs is recorded nowhere else on the machine, so it
//! keeps its own list in `github-installs.json` under the data directory.
//! [`Github::record_install`] appends to it. The transaction runner is meant
//! to call that after a successful plan and does not yet, so `installed()`
//! is empty until it does.
//!
//! `plan` does no network: it builds steps from the release a search or a
//! details call already fetched, so a plan for a repository nobody has
//! looked at says so instead of quietly asking the API.

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
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct SearchAnswer {
    items: Vec<Repo>,
}

/// What a release asset is, judged from its file name.
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
}

impl AssetKind {
    pub fn of(name: &str) -> Option<AssetKind> {
        let lower = name.to_lowercase();
        let ends = |s: &str| lower.ends_with(s);
        if ends(".appimage") {
            Some(AssetKind::AppImage)
        } else if ends(".deb") {
            Some(AssetKind::Deb)
        } else if ends(".rpm") {
            Some(AssetKind::Rpm)
        } else if ends(".pkg.tar.zst") || ends(".pacman") {
            Some(AssetKind::Pacman)
        } else if ends(".flatpak") {
            Some(AssetKind::Flatpak)
        } else if (ends(".tar.gz") || ends(".tgz") || ends(".tar.xz") || ends(".zip"))
            && lower.contains("linux")
        {
            Some(AssetKind::Archive)
        } else {
            None
        }
    }

    /// For sentences: "a Debian package".
    pub fn label(self) -> &'static str {
        match self {
            AssetKind::AppImage => "an AppImage",
            AssetKind::Deb => "a Debian package",
            AssetKind::Rpm => "an RPM package",
            AssetKind::Pacman => "an Arch package",
            AssetKind::Flatpak => "a Flatpak bundle",
            AssetKind::Archive => "an archive",
        }
    }

    /// The package kind this distribution installs natively, if any.
    pub fn native(system: &SystemInfo) -> Option<AssetKind> {
        if system.is_arch_like() {
            Some(AssetKind::Pacman)
        } else if system.is_debian_like() {
            Some(AssetKind::Deb)
        } else if system.is_fedora_like() {
            Some(AssetKind::Rpm)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// From `SystemInfo::arch`, which is Rust's spelling.
    pub fn of_machine(arch: &str) -> Option<Arch> {
        match arch {
            "x86_64" => Some(Arch::X86_64),
            "aarch64" => Some(Arch::Aarch64),
            _ => None,
        }
    }

    /// Guessed from an asset's name; `None` when the name does not say.
    pub fn guess(name: &str) -> Option<Arch> {
        let lower = name.to_lowercase();
        if ["aarch64", "arm64"].iter().any(|s| lower.contains(s)) {
            Some(Arch::Aarch64)
        } else if ["x86_64", "x86-64", "amd64", "x64"]
            .iter()
            .any(|s| lower.contains(s))
        {
            Some(Arch::X86_64)
        } else {
            None
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

/// The best Linux asset for this machine: one for its architecture before
/// one that does not say, never one for the other; the distribution's own
/// package kind first, then an AppImage, then a Flatpak, then a package for
/// another distribution, then an archive.
pub fn pick_asset(
    assets: &[Asset],
    machine: Option<Arch>,
    native: Option<AssetKind>,
) -> Option<Chosen> {
    let mut candidates: Vec<(u8, u8, Chosen)> = assets
        .iter()
        .filter_map(|asset| {
            let kind = AssetKind::of(&asset.name)?;
            let arch = Arch::guess(&asset.name);
            let arch_rank = match (arch, machine) {
                (Some(a), Some(m)) if a == m => 0,
                (None, _) | (Some(_), None) => 1,
                (Some(_), Some(_)) => return None,
            };
            let kind_rank = match kind {
                k if Some(k) == native => 0,
                AssetKind::AppImage => 1,
                AssetKind::Flatpak => 2,
                AssetKind::Archive => 4,
                _ => 3,
            };
            Some((
                arch_rank,
                kind_rank,
                Chosen {
                    asset: asset.clone(),
                    kind,
                    arch,
                },
            ))
        })
        .collect();
    candidates.sort_by(|a, b| (a.0, a.1, &a.2.asset.name).cmp(&(b.0, b.1, &b.2.asset.name)));
    candidates.into_iter().next().map(|c| c.2)
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
}

/// The package for a repository whose latest release has a Linux asset.
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
    if chosen.kind == AssetKind::AppImage {
        p.facts.push((
            "Install".to_string(),
            "The AppImage is placed in ~/.local/bin; no menu entry yet.".to_string(),
        ));
    }
    p
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

/// What a plan needs to install one release, with no network in it.
pub struct InstallTarget<'a> {
    pub name: &'a str,
    pub chosen: &'a Chosen,
    pub downloads: &'a Path,
    pub home: &'a Path,
    pub native: Option<AssetKind>,
    pub has_curl: bool,
}

/// The file the asset is downloaded to. The name comes from GitHub, so
/// only its last path component is used.
fn download_path(downloads: &Path, asset: &str) -> PathBuf {
    let file = Path::new(asset)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "release".to_string());
    downloads.join(file)
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

/// Download with curl, then install by kind. curl rather than the store's
/// own client because a download is a step the runner logs and can cancel,
/// and a plan must not fetch anything itself.
pub fn install_steps(target: &InstallTarget) -> Result<Vec<Step>> {
    let asset = &target.chosen.asset;
    let url = &asset.browser_download_url;
    let kind = target.chosen.kind;
    if kind == AssetKind::Archive {
        return Err(err(format!(
            "This release is an archive, not a package; download it from {url} and unpack it by hand."
        )));
    }
    if matches!(kind, AssetKind::Deb | AssetKind::Rpm | AssetKind::Pacman)
        && Some(kind) != target.native
    {
        return Err(err(format!(
            "{} is {}, which this machine cannot install; download it by hand from {url} if you want it.",
            asset.name,
            kind.label()
        )));
    }
    if !target.has_curl {
        return Err(err(
            "curl is needed to download GitHub releases; install it and try again.",
        ));
    }
    let path = download_path(target.downloads, &asset.name);
    let path_str = path.to_string_lossy().into_owned();
    let mut steps = vec![step(
        format!("Downloading {}", asset.name),
        "curl",
        ["-L", "-sS", "--fail", "--create-dirs", "-o", &path_str, url]
            .into_iter()
            .map(str::to_string)
            .collect(),
        false,
        4,
    )];
    let title = format!("Installing {}", target.name);
    let s = |args: &[&str], needs_root: bool, weight: u32, program: &str| {
        step(
            title.clone(),
            program,
            args.iter().map(|a| a.to_string()).collect(),
            needs_root,
            weight,
        )
    };
    let install = match kind {
        AssetKind::Pacman => s(&["-U", "--noconfirm", &path_str], true, 4, "pacman"),
        AssetKind::Deb => {
            let mut st = s(&["install", "-y", &path_str], true, 4, "apt-get");
            st.command
                .env
                .push(("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string()));
            st
        }
        AssetKind::Rpm => s(&["install", "-y", &path_str], true, 4, "dnf"),
        AssetKind::Flatpak => s(
            &["install", "-y", "--noninteractive", "--user", &path_str],
            false,
            4,
            "flatpak",
        ),
        AssetKind::AppImage => {
            let dest = appimage_path(target.home, target.name);
            s(
                &["-Dm755", &path_str, &dest.to_string_lossy()],
                false,
                1,
                "install",
            )
        }
        AssetKind::Archive => unreachable!("archives were refused above"),
    };
    steps.push(install);
    Ok(steps)
}

fn appimage_path(home: &Path, name: &str) -> PathBuf {
    home.join(".local/bin").join(name)
}

/// Removing what a record says was installed. Only an AppImage is a file
/// this source owns; a package went into the package manager's database
/// and is removed there, and a Flatpak bundle's application id is not in
/// the record.
pub fn remove_steps(record: &InstallRecord, home: &Path) -> Result<Vec<Step>> {
    match record.kind {
        AssetKind::AppImage => {
            let path = appimage_path(home, &record.name);
            Ok(vec![step(
                format!("Removing {}", record.name),
                "rm",
                vec!["-f".to_string(), path.to_string_lossy().into_owned()],
                false,
                1,
            )])
        }
        kind => Err(err(format!(
            "{} was installed as {}; remove it with the tool that installed it.",
            record.name,
            kind.label()
        ))),
    }
}

fn err(message: impl Into<String>) -> Error {
    Error::from_source(SourceKind::Github, message)
}

pub struct Github {
    client: Arc<Client>,
    catalogue: Arc<Catalogue>,
    machine: Option<Arch>,
    native: Option<AssetKind>,
    /// `<data>/github-installs.json`.
    installs: PathBuf,
    /// `<cache>/downloads`, where curl puts the asset.
    downloads: PathBuf,
    /// `<cache>/github`, API answers by URL.
    cache: PathBuf,
    /// Latest releases seen by search, details and updates, so a plan can
    /// be built without the network.
    known: Mutex<HashMap<String, Release>>,
}

impl Github {
    pub fn new(system: &SystemInfo, client: Arc<Client>, catalogue: Arc<Catalogue>) -> Github {
        let dirs = Dirs::new();
        Github::with_dirs(system, client, catalogue, &dirs.data, &dirs.cache)
    }

    /// The same source with its files somewhere else, for tests.
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
            machine: Arch::of_machine(&system.arch),
            native: AssetKind::native(system),
            installs: data.join("github-installs.json"),
            downloads: cache.join("downloads"),
            cache: cache.join("github"),
            known: Mutex::new(HashMap::new()),
        }
    }

    /// Every install this source recorded. A file that will not parse is
    /// logged and treated as empty rather than failing every listing.
    pub fn records(&self) -> Vec<InstallRecord> {
        let Ok(text) = std::fs::read_to_string(&self.installs) else {
            return Vec::new();
        };
        match serde_json::from_str(&text) {
            Ok(records) => records,
            Err(e) => {
                log::warn!(
                    "{} is not readable and is ignored: {e}",
                    self.installs.display()
                );
                Vec::new()
            }
        }
    }

    /// Remember an install so `installed()` and `updates()` know about it.
    /// Meant for the transaction runner after a successful plan; not wired
    /// there yet.
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
        pick_asset(&release.assets, self.machine, self.native)
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
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        install_steps(&InstallTarget {
            name,
            chosen: &chosen,
            downloads: &self.downloads,
            home: &home,
            native: self.native,
            has_curl: system::which("curl").is_some(),
        })
    }
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
    fn launcher(&self, id: &str) -> Option<crate::launch::Launch> {
        let record = self.records().into_iter().find(|r| r.repo == id)?;
        if record.kind != AssetKind::AppImage {
            return None;
        }
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        let path = appimage_path(&home, &record.name);
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
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/tmp"));
                remove_steps(&record, &home)
            }
            // The planner expands a setup through `Source::setup`.
            Op::Refresh { .. } | Op::Setup { .. } => Ok(Vec::new()),
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
                })
            }
            Op::Remove { package } => self.forget_install(&package.id),
            Op::UpdateAll { .. } | Op::Refresh { .. } | Op::Setup { .. } => Ok(()),
        };
        if let Err(e) = outcome {
            log::warn!("GitHub could not update its install record: {}", e.message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_kinds_by_name() {
        assert_eq!(
            AssetKind::of("Heroic-2.22.1-linux-x86_64.AppImage"),
            Some(AssetKind::AppImage)
        );
        assert_eq!(
            AssetKind::of("nvim-linux-x86_64.appimage"),
            Some(AssetKind::AppImage)
        );
        assert_eq!(
            AssetKind::of("Heroic-2.22.1-linux-amd64.deb"),
            Some(AssetKind::Deb)
        );
        assert_eq!(
            AssetKind::of("Heroic-2.22.1-linux-x86_64.rpm"),
            Some(AssetKind::Rpm)
        );
        assert_eq!(
            AssetKind::of("Heroic-2.22.1-linux-x64.pacman"),
            Some(AssetKind::Pacman)
        );
        assert_eq!(
            AssetKind::of("brokey-0.1.0-1-x86_64.pkg.tar.zst"),
            Some(AssetKind::Pacman)
        );
        assert_eq!(AssetKind::of("app.flatpak"), Some(AssetKind::Flatpak));
        assert_eq!(
            AssetKind::of("Heroic-2.22.1-linux-x64.tar.xz"),
            Some(AssetKind::Archive)
        );
        assert_eq!(
            AssetKind::of("nvim-linux-x86_64.tar.gz"),
            Some(AssetKind::Archive)
        );
        assert_eq!(AssetKind::of("nvim-macos-x86_64.tar.gz"), None);
        assert_eq!(AssetKind::of("Heroic-2.22.1-Setup-x64.exe"), None);
        assert_eq!(AssetKind::of("Heroic-2.22.1-macOS-x64.zip"), None);
        assert_eq!(AssetKind::of("latest-linux.yml"), None);
    }

    #[test]
    fn architectures_by_name() {
        assert_eq!(Arch::guess("Heroic-linux-amd64.deb"), Some(Arch::X86_64));
        assert_eq!(Arch::guess("Heroic-linux-x64.pacman"), Some(Arch::X86_64));
        assert_eq!(
            Arch::guess("nvim-linux-x86_64.appimage"),
            Some(Arch::X86_64)
        );
        assert_eq!(
            Arch::guess("nvim-linux-arm64.appimage"),
            Some(Arch::Aarch64)
        );
        assert_eq!(Arch::guess("thing-aarch64.rpm"), Some(Arch::Aarch64));
        assert_eq!(Arch::guess("thing.AppImage"), None);
        assert_eq!(Arch::of_machine("x86_64"), Some(Arch::X86_64));
        assert_eq!(Arch::of_machine("riscv64"), None);
    }

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.to_string(),
            size: 1,
            browser_download_url: format!("https://example.invalid/{name}"),
        }
    }

    #[test]
    fn an_asset_for_the_other_architecture_is_never_picked() {
        let assets = [asset("tool-linux-arm64.AppImage")];
        assert!(pick_asset(&assets, Some(Arch::X86_64), None).is_none());
        assert!(pick_asset(&assets, Some(Arch::Aarch64), None).is_some());
        // A name that does not say is taken on any machine.
        let assets = [asset("tool.AppImage")];
        assert!(pick_asset(&assets, Some(Arch::Aarch64), None).is_some());
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
    fn download_names_cannot_escape_the_directory() {
        let p = download_path(Path::new("/cache/downloads"), "../../evil.deb");
        assert_eq!(p, PathBuf::from("/cache/downloads/evil.deb"));
    }
}
