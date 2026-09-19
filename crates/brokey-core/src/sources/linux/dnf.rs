//! The dnf source: Fedora, RHEL and everything else built on rpm.
//!
//! Unlike pacman and apt there is no database to read directly: rpm's is a
//! SQLite (or Berkeley DB) file whose schema is not a stable interface, and
//! the repository metadata is a set of per-repository XML files behind
//! dnf's own cache layout. So this source asks the command line, through a
//! [`Runner`] that a test replaces with fixtures, and asks it only in forms
//! dnf4 (RHEL, Fedora up to 40) and dnf5 (Fedora 41 and later) both accept:
//!
//! - `dnf repoquery -q --queryformat <format> '*term*'` for what the
//!   repositories offer. Both generations list available packages only
//!   unless told otherwise, so what is installed is a second query,
//!   `dnf repoquery -q --installed --queryformat <format>`, with
//!   `rpm -qa --queryformat` as the fallback when dnf cannot load its
//!   repositories. dnf4 refuses `--installed` and `--available` together,
//!   which is why one query for both was rejected.
//! - `dnf check-update -q`, whose exit code 100 means updates exist.
//!
//! The queryformat is the intersection of the two generations: every tag in
//! it is in dnf4's allowed list and in dnf5's documentation, `%{repoid}`
//! included (dnf4 defines it as an alias of `reponame`). The format carries
//! real tab and newline characters rather than `\t` escapes, so neither
//! generation's unescaping matters, and it ends in a newline because dnf5
//! adds none; dnf4 adds one of its own, which leaves a blank line between
//! rows that the parser skips. dnf4 prints `buildtime` as a UTC date and
//! dnf5 as seconds; `(none)` and `None` both mean absent. Epoch is not in
//! the format, so a package whose epoch differs between two repositories
//! ranks by version-release alone.
//!
//! Versions are ordered by rpm's algorithm ([`rpm_compare`]): like pacman's
//! segment walk, but `~` sorts before everything and `^` after everything
//! except a longer version.
//!
//! This source was written on an Arch machine against fixtures and is
//! marked untested until it has run on a real Fedora. On a machine that is
//! not Fedora-based it reports itself unavailable with the reason.

use super::apt::{SearchWords, apply_component, description_markup, limit, relevance};
use crate::appstream::Catalogue;
use crate::model::*;
use crate::{Error, Op, Query, Result, Source};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// The name of this store's own rpm, so its update is flagged.
const SELF_PACKAGE: &str = "brokey";

/// What a process said. `code` is `None` when it was killed by a signal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn new(code: i32, stdout: impl Into<String>) -> Output {
        Output {
            code: Some(code),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn first_error_line(&self) -> String {
        self.stderr
            .lines()
            .chain(self.stdout.lines())
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("no output")
            .to_string()
    }
}

/// Runs a read-only query and reports what it printed. The one seam
/// between this source and the machine, so every test runs on fixtures;
/// nothing that changes the machine ever goes through it (that is a
/// [`Step`]).
pub trait Runner: Send + Sync {
    fn run(&self, program: &str, args: &[String]) -> Result<Output>;
}

impl<F> Runner for F
where
    F: Fn(&str, &[String]) -> Result<Output> + Send + Sync,
{
    fn run(&self, program: &str, args: &[String]) -> Result<Output> {
        self(program, args)
    }
}

/// The real thing: spawn it with a C locale so the output is parseable.
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Output> {
        let out = std::process::Command::new(program)
            .args(args)
            .env("LC_ALL", "C.UTF-8")
            .output()
            .map_err(|e| {
                Error::from_source(SourceKind::Dnf, format!("Could not run {program}: {e}."))
            })?;
        Ok(Output {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

pub struct Dnf {
    system: SystemInfo,
    catalogue: Arc<Catalogue>,
    dnf: Option<PathBuf>,
    runner: Box<dyn Runner>,
    /// `dnf --version`, asked once, for the status bar.
    version: OnceLock<Option<String>>,
}

impl Dnf {
    pub fn new(system: &SystemInfo, catalogue: Arc<Catalogue>) -> Dnf {
        Dnf::with_runner(
            system,
            catalogue,
            crate::system::which("dnf"),
            Box::new(SystemRunner),
        )
    }

    /// The same source over a chosen runner, so fixtures can stand in for
    /// a Fedora machine. `dnf` is where the binary is, or `None` when it
    /// is not on the machine.
    pub fn with_runner(
        system: &SystemInfo,
        catalogue: Arc<Catalogue>,
        dnf: Option<PathBuf>,
        runner: Box<dyn Runner>,
    ) -> Dnf {
        Dnf {
            system: system.clone(),
            catalogue,
            dnf,
            runner,
            version: OnceLock::new(),
        }
    }

    fn unavailable_reason(&self) -> Option<String> {
        if !self.system.is_fedora_like() {
            return Some(format!(
                "dnf is for Fedora-based systems; this is {}.",
                self.system.pretty_name
            ));
        }
        if self.dnf.is_none() {
            return Some("dnf is not installed, so packages cannot be changed.".to_string());
        }
        None
    }

    fn version(&self) -> Option<&str> {
        self.version
            .get_or_init(
                || match self.runner.run("dnf", &["--version".to_string()]) {
                    Ok(out) if out.code == Some(0) => parse_version(&out.stdout),
                    _ => None,
                },
            )
            .as_deref()
    }

    fn dnf(&self, args: &[String]) -> Result<Output> {
        self.runner.run("dnf", args)
    }

    /// Rows from one repoquery, or the sentence dnf gave for failing.
    /// `key` is a name or glob; without one the query is every package.
    fn repoquery(&self, installed: bool, key: Option<&str>) -> Result<Vec<Row>> {
        let mut args = vec!["repoquery".to_string(), "-q".to_string()];
        if installed {
            args.push("--installed".to_string());
        }
        args.push("--queryformat".to_string());
        args.push(QUERY_FORMAT.to_string());
        if let Some(key) = key {
            args.push(key.to_string());
        }
        let out = self.dnf(&args)?;
        if out.code != Some(0) {
            return Err(Error::from_source(
                SourceKind::Dnf,
                format!(
                    "dnf repoquery failed: {}. Check the repository configuration and the network.",
                    out.first_error_line()
                ),
            ));
        }
        Ok(parse_rows(&out.stdout))
    }

    /// What is installed, newest row per name: dnf's answer, else rpm's
    /// when dnf cannot load its repositories (a broken metalink must not
    /// blank the Installed page).
    fn installed_rows(&self, key: Option<&str>) -> Result<BTreeMap<String, Row>> {
        let dnf_err = match self.repoquery(true, key) {
            Ok(rows) => return Ok(newest_by_name(rows)),
            Err(e) => e,
        };
        let mut args: Vec<String> = ["-qa", "--queryformat", RPM_FORMAT]
            .iter()
            .map(|s| s.to_string())
            .collect();
        if let Some(key) = key {
            args.push(key.to_string());
        }
        match self.runner.run("rpm", &args) {
            // gpg-pubkey is rpm's record of an imported signing key, not
            // a package on the machine.
            Ok(out) if out.code == Some(0) => Ok(newest_by_name(
                parse_rows(&out.stdout)
                    .into_iter()
                    .filter(|r| r.name != "gpg-pubkey")
                    .collect(),
            )),
            Ok(out) => Err(Error::from_source(
                SourceKind::Dnf,
                format!(
                    "{} rpm -qa failed too: {}.",
                    dnf_err.message,
                    out.first_error_line()
                ),
            )),
            Err(e) => Err(Error::from_source(
                SourceKind::Dnf,
                format!("{} {}", dnf_err.message, e.message),
            )),
        }
    }

    /// The available and installed rows for one name or glob, folded so
    /// one package is one entry.
    fn entries(&self, key: &str) -> Result<BTreeMap<String, Entry>> {
        let available = self.repoquery(false, Some(key))?;
        let installed = self.installed_rows(Some(key))?;
        Ok(combine(available, installed))
    }

    /// The long description, which is multi-line and so cannot share the
    /// tab-separated row. A record separator after each answer, taking the
    /// first, is simpler than an `--info` block parser for two generations
    /// of dnf; rpm answers for a package that is installed but no longer
    /// in any repository.
    fn description(&self, name: &str, available: bool) -> Option<String> {
        let out = if available {
            self.dnf(&[
                "repoquery".to_string(),
                "-q".to_string(),
                "--queryformat".to_string(),
                DESCRIPTION_FORMAT.to_string(),
                name.to_string(),
            ])
        } else {
            self.runner.run(
                "rpm",
                &[
                    "-q".to_string(),
                    "--queryformat".to_string(),
                    DESCRIPTION_FORMAT.to_string(),
                    name.to_string(),
                ],
            )
        };
        let out = out.ok().filter(|o| o.code == Some(0))?;
        out.stdout
            .split(RECORD_SEPARATOR)
            .map(str::trim)
            .find(|d| !d.is_empty() && *d != "None" && *d != "(none)")
            .map(|d| description_markup(&split_paragraphs(d)))
    }

    fn to_package(
        &self,
        name: &str,
        avail: Option<&Row>,
        inst: Option<&Row>,
        full: bool,
    ) -> Package {
        let base = avail.or(inst);
        let mut pkg = Package::new(SourceKind::Dnf, name, name);
        pkg.version = avail.map(|r| r.version.clone());
        pkg.installed_version = inst.map(|r| r.version.clone());
        pkg.installed = inst.is_some();
        pkg.repo = avail.map(|r| r.repo.clone()).filter(|r| is_real_repo(r));
        if let Some(base) = base {
            pkg.summary = base.summary.clone();
            pkg.homepage = base.url.clone();
            pkg.licence = base.licence.clone();
            pkg.installed_size = base.install_size;
            pkg.updated = base.build_time;
        }
        pkg.download_size = avail.and_then(|r| r.download_size);
        if full {
            if let Some(repo) = &pkg.repo {
                pkg.facts.push(("Repository".to_string(), repo.clone()));
            }
            if let Some(licence) = &pkg.licence {
                pkg.facts.push(("Licence".to_string(), licence.clone()));
            }
        }
        if let Some(component) = self.catalogue.by_pkgname(name) {
            apply_component(&mut pkg, component);
        }
        pkg
    }

    fn step(&self, title: String, args: &[&str], weight: u32) -> Step {
        Step {
            source: SourceKind::Dnf,
            title,
            command: Command {
                program: "dnf".to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: vec![("LC_ALL".to_string(), "C.UTF-8".to_string())],
                cwd: None,
            },
            needs_root: true,
            weight,
        }
    }

    fn own(&self, package: &PackageRef) -> Result<()> {
        if package.source != SourceKind::Dnf {
            return Err(Error::from_source(
                SourceKind::Dnf,
                format!(
                    "{} is a {} package, not a dnf package.",
                    package.id,
                    package.source.label()
                ),
            ));
        }
        Ok(())
    }
}

impl Source for Dnf {
    fn kind(&self) -> SourceKind {
        SourceKind::Dnf
    }

    fn status(&self) -> SourceStatus {
        match self.unavailable_reason() {
            Some(reason) => SourceStatus {
                kind: SourceKind::Dnf,
                available: false,
                reason: Some(reason),
                detail: None,
                searchable: false,
                setup: None,
            },
            None => SourceStatus {
                kind: SourceKind::Dnf,
                available: true,
                reason: None,
                detail: self.version().map(|v| format!("dnf {v}")),
                searchable: false,
                setup: None,
            },
        }
    }

    fn search(&self, query: &Query) -> Result<Vec<Package>> {
        if let Some(reason) = self.unavailable_reason() {
            return Err(Error::from_source(SourceKind::Dnf, reason));
        }
        let Some(words) = SearchWords::new(&query.text) else {
            return Ok(Vec::new());
        };
        let entries = self.entries(&search_glob(&query.text))?;
        let mut hits: Vec<(u32, &str)> = entries
            .iter()
            .filter_map(|(name, entry)| {
                let summary = entry
                    .available
                    .as_ref()
                    .or(entry.installed.as_ref())
                    .and_then(|r| r.summary.as_deref());
                relevance(name, summary, &[], &words).map(|score| (score, name.as_str()))
            })
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        Ok(hits
            .iter()
            .take(limit(query))
            .map(|(_, name)| {
                let entry = &entries[*name];
                self.to_package(
                    name,
                    entry.available.as_ref(),
                    entry.installed.as_ref(),
                    false,
                )
            })
            .collect())
    }

    fn installed(&self) -> Result<Vec<Package>> {
        if let Some(reason) = self.unavailable_reason() {
            return Err(Error::from_source(SourceKind::Dnf, reason));
        }
        let rows = self.installed_rows(None)?;
        Ok(rows
            .iter()
            .map(|(name, row)| self.to_package(name, None, Some(row), false))
            .collect())
    }

    fn updates(&self) -> Result<Vec<Update>> {
        if let Some(reason) = self.unavailable_reason() {
            return Err(Error::from_source(SourceKind::Dnf, reason));
        }
        let out = self.dnf(&["check-update".to_string(), "-q".to_string()])?;
        let pending = match out.code {
            Some(100) => parse_check_update(&out.stdout),
            Some(0) => return Ok(Vec::new()),
            _ => {
                return Err(Error::from_source(
                    SourceKind::Dnf,
                    format!(
                        "dnf check-update failed: {}. Check the repository configuration and the network.",
                        out.first_error_line()
                    ),
                ));
            }
        };
        if pending.is_empty() {
            return Ok(Vec::new());
        }
        let installed = self.installed_rows(None)?;
        let mut updates = Vec::new();
        for p in pending {
            let inst = installed.get(&p.name);
            let pkg = self.to_package(&p.name, None, inst, false);
            updates.push(Update {
                package: pkg.reference(),
                name: pkg.name,
                kind: pkg.kind,
                summary: pkg.summary,
                icon: pkg.icon,
                from: inst.map(|r| r.version.clone()),
                to: p.version,
                download_size: None,
                published: None,
                is_self: p.name == SELF_PACKAGE,
            });
        }
        updates.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(updates)
    }

    fn details(&self, id: &str) -> Result<Package> {
        if let Some(reason) = self.unavailable_reason() {
            return Err(Error::from_source(SourceKind::Dnf, reason));
        }
        let entries = self.entries(id)?;
        let Some(entry) = entries.get(id) else {
            return Err(Error::from_source(
                SourceKind::Dnf,
                format!(
                    "{id} is not in any enabled repository and is not installed. Refresh and search again."
                ),
            ));
        };
        let mut pkg = self.to_package(id, entry.available.as_ref(), entry.installed.as_ref(), true);
        if pkg.description.is_none() {
            pkg.description = self.description(id, entry.available.is_some());
        }
        Ok(pkg)
    }

    /// dnf plans are unconditional: checking whether a package is already
    /// installed would cost a second of dnf start-up per plan, and dnf
    /// itself answers "Nothing to do." and exits cleanly in that case.
    /// rpm lists an installed package's files; a package that is not
    /// installed makes it exit non-zero, which is `None` here.
    fn launcher(&self, id: &str) -> Option<crate::launch::Launch> {
        let out = self
            .runner
            .run("rpm", &["-ql".to_string(), id.to_string()])
            .ok()?;
        if out.code != Some(0) {
            return None;
        }
        crate::launch::from_files(out.stdout.lines(), std::path::Path::new("/"), &[id])
    }

    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        if let Some(reason) = self.unavailable_reason() {
            return Err(Error::from_source(SourceKind::Dnf, reason));
        }
        match op {
            Op::Install { package } => {
                self.own(package)?;
                let name = package.id.as_str();
                Ok(vec![self.step(
                    format!("Installing {name}"),
                    &["install", "-y", name],
                    3,
                )])
            }
            Op::Remove { package } => {
                self.own(package)?;
                let name = package.id.as_str();
                Ok(vec![self.step(
                    format!("Removing {name}"),
                    &["remove", "-y", name],
                    2,
                )])
            }
            Op::Update { package } => {
                self.own(package)?;
                let name = package.id.as_str();
                Ok(vec![self.step(
                    format!("Updating {name}"),
                    &["upgrade", "-y", name],
                    3,
                )])
            }
            Op::UpdateAll { .. } => Ok(vec![self.step(
                "Updating all dnf packages".to_string(),
                &["upgrade", "-y"],
                6,
            )]),
            Op::Refresh { .. } => Ok(vec![self.step(
                "Refreshing the dnf metadata".to_string(),
                &["makecache"],
                1,
            )]),
            // The planner expands a setup through `Source::setup`.
            Op::Setup { .. } => Ok(Vec::new()),
        }
    }
}

/// The `check-update` line for one package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub name: String,
    pub arch: String,
    pub version: String,
    pub repo: String,
}

/// One line of a repoquery in [`QUERY_FORMAT`], or of `rpm -qa` in
/// [`RPM_FORMAT`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    /// `version-release`, without the epoch.
    pub version: String,
    /// The repository id; "@System" for an installed row.
    pub repo: String,
    pub summary: Option<String>,
    pub url: Option<String>,
    pub licence: Option<String>,
    pub download_size: Option<u64>,
    pub install_size: Option<u64>,
    /// Unix seconds, from either dnf5's raw number or dnf4's UTC date.
    pub build_time: Option<i64>,
}

/// The tab-separated row every dnf query asks for. Real tabs and a real
/// newline, for the reasons in the module documentation.
pub const QUERY_FORMAT: &str = "%{name}\t%{version}-%{release}\t%{repoid}\t%{summary}\t%{url}\t%{license}\t%{downloadsize}\t%{installsize}\t%{buildtime}\n";

/// `rpm -qa`'s version of the same row: no repository, no download size.
pub const RPM_FORMAT: &str =
    "%{NAME}\t%{VERSION}-%{RELEASE}\t\t%{SUMMARY}\t%{URL}\t%{LICENSE}\t\t%{SIZE}\t%{BUILDTIME}\n";

const RECORD_SEPARATOR: char = '\u{1e}';
const DESCRIPTION_FORMAT: &str = "%{description}\u{1e}\n";

/// The glob for a search: every word, in order, anywhere in the name.
/// It always begins with `*`, so a term beginning with `-` is never read
/// as an option.
pub fn search_glob(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    format!("*{}*", words.join("*"))
}

/// Parse rows of [`QUERY_FORMAT`] or [`RPM_FORMAT`]. A short line (fewer
/// than the three fields a package needs) is skipped; dnf4 prints its own
/// newline after the format's, so blank lines are expected.
pub fn parse_rows(text: &str) -> Vec<Row> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 3 || f[0].is_empty() {
                return None;
            }
            let field = |i: usize| {
                f.get(i)
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty() && *s != "None" && *s != "(none)")
            };
            Some(Row {
                name: f[0].to_string(),
                version: f[1].to_string(),
                repo: f[2].to_string(),
                summary: field(3).map(str::to_string),
                url: field(4).map(str::to_string),
                licence: field(5).map(str::to_string),
                download_size: field(6).and_then(|s| s.parse().ok()).filter(|n| *n > 0),
                install_size: field(7).and_then(|s| s.parse().ok()).filter(|n| *n > 0),
                build_time: field(8).and_then(parse_time),
            })
        })
        .collect()
}

/// Parse `dnf check-update`. Rows are `name.arch  version  repo`. dnf4
/// wraps a name too long for its column onto its own line with the rest
/// indented on the next, and lists replacements under an "Obsoleting
/// Packages" heading; dnf5 prints no headings and puts each obsoleted
/// package indented under its replacement. So an indented line with two
/// fields is the rest of a wrapped row, an indented line with three is an
/// obsoleted package and not an update, and everything else that is not
/// a row (the metadata line, security notes) is skipped.
pub fn parse_check_update(text: &str) -> Vec<Pending> {
    let mut out = Vec::new();
    let mut carried: Option<&str> = None;
    for line in text.lines() {
        let indented = line.starts_with(' ') || line.starts_with('\t');
        let trimmed = line.trim();
        if trimmed.to_ascii_lowercase().starts_with("obsoleting") {
            break;
        }
        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        match fields.as_slice() {
            [name_arch] if split_name_arch(name_arch).is_some() => carried = Some(name_arch),
            [version, repo] if looks_like_version(version) => {
                if let Some(name_arch) = carried.take()
                    && let Some((name, arch)) = split_name_arch(name_arch)
                {
                    out.push(Pending {
                        name: name.to_string(),
                        arch: arch.to_string(),
                        version: version.to_string(),
                        repo: repo.to_string(),
                    });
                }
            }
            [name_arch, version, repo] if looks_like_version(version) => {
                carried = None;
                if indented {
                    continue;
                }
                if let Some((name, arch)) = split_name_arch(name_arch) {
                    out.push(Pending {
                        name: name.to_string(),
                        arch: arch.to_string(),
                        version: version.to_string(),
                        repo: repo.to_string(),
                    });
                }
            }
            _ => carried = None,
        }
    }
    out
}

/// `python3.11.x86_64` is name `python3.11`, arch `x86_64`: the arch is
/// what follows the last dot, and must look like one.
fn split_name_arch(s: &str) -> Option<(&str, &str)> {
    let (name, arch) = s.rsplit_once('.')?;
    let ok = !name.is_empty()
        && !arch.is_empty()
        && arch.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && arch.bytes().any(|b| b.is_ascii_alphabetic());
    ok.then_some((name, arch))
}

fn looks_like_version(s: &str) -> bool {
    s.bytes().next().is_some_and(|b| b.is_ascii_digit())
}

/// `dnf --version`: dnf4 prints "4.18.2" on its first line, dnf5 prints
/// "dnf5 version 5.1.17". The number is what the status bar shows; it is
/// the first word that starts with a digit, because the first digit on
/// dnf5's line is the 5 in its own name.
pub fn parse_version(text: &str) -> Option<String> {
    let first = text.lines().next()?;
    let word = first
        .split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))?;
    Some(
        word.chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect(),
    )
}

/// A build time as dnf5 and rpm print it (seconds) or as dnf4's
/// queryformat does ("2023-12-19 14:00", UTC).
pub fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<i64>() {
        return Some(n);
    }
    let (date, time) = s.split_once(' ')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':').map(|p| p.parse::<i64>().ok());
    let (h, min) = (t.next()??, t.next()??);
    let sec = t.next().flatten().unwrap_or(0);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(y, m, day) * 86_400 + h * 3600 + min * 60 + sec)
}

/// Days since 1970-01-01 for a proleptic Gregorian date; Howard Hinnant's
/// algorithm, which needs no table of month lengths.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A package as the two queries see it: the newest available row and the
/// installed row, so one name is one result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub available: Option<Row>,
    pub installed: Option<Row>,
}

/// Fold the available rows (newest per name; an `@System` row, should a
/// generation print one, counts as installed) and the installed rows into
/// one entry per name.
pub fn combine(available: Vec<Row>, installed: BTreeMap<String, Row>) -> BTreeMap<String, Entry> {
    let mut out: BTreeMap<String, Entry> = BTreeMap::new();
    for row in available {
        let entry = out.entry(row.name.clone()).or_default();
        let slot = if row.repo == "@System" {
            &mut entry.installed
        } else {
            &mut entry.available
        };
        if slot
            .as_ref()
            .is_none_or(|have| rpm_compare(&row.version, &have.version) == Ordering::Greater)
        {
            *slot = Some(row);
        }
    }
    for (name, row) in installed {
        out.entry(name).or_default().installed = Some(row);
    }
    out
}

/// The newest row per name: a kernel has several versions installed at
/// once and the page names one.
pub fn newest_by_name(rows: Vec<Row>) -> BTreeMap<String, Row> {
    let mut out: BTreeMap<String, Row> = BTreeMap::new();
    for row in rows {
        match out.get(&row.name) {
            Some(have) if rpm_compare(&have.version, &row.version) != Ordering::Less => {}
            _ => {
                out.insert(row.name.clone(), row);
            }
        }
    }
    out
}

/// "@System", "@commandline" and an empty column are where a package is,
/// not a repository the page should name.
fn is_real_repo(repo: &str) -> bool {
    !repo.is_empty() && !repo.starts_with('@')
}

/// rpm descriptions are wrapped prose with blank lines between
/// paragraphs; wrapped lines join with a space.
fn split_paragraphs(text: &str) -> Vec<String> {
    text.split("\n\n")
        .map(|p| {
            p.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|p| !p.is_empty())
        .collect()
}

/// rpm's full comparison of `[epoch:]version[-release]`.
/// `Ordering::Greater` means `a` is newer. A missing epoch is 0, as
/// libsolv (which decides dnf's updates) reads it; a missing release loses
/// to any release, as libsolv's `pool_evrcmp_str` has it in compare mode.
pub fn rpm_compare(a: &str, b: &str) -> Ordering {
    let (ea, va, ra) = split_evr(a);
    let (eb, vb, rb) = split_evr(b);
    ea.cmp(&eb)
        .then_with(|| rpmvercmp(va, vb))
        .then_with(|| match (ra, rb) {
            (Some(ra), Some(rb)) => rpmvercmp(ra, rb),
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        })
}

/// `a` is strictly newer than `b` by rpm's ordering.
pub fn rpm_is_newer(a: &str, b: &str) -> bool {
    rpm_compare(a, b) == Ordering::Greater
}

/// rpm's `rpmverParse`: the epoch is a leading run of digits before a
/// colon, the release everything after the last hyphen.
fn split_evr(v: &str) -> (u64, &str, Option<&str>) {
    let v = v.trim();
    let digits = v.bytes().take_while(u8::is_ascii_digit).count();
    let (epoch, rest) = if v.as_bytes().get(digits) == Some(&b':') {
        (v[..digits].parse().unwrap_or(0), &v[digits + 1..])
    } else {
        (0, v)
    };
    match rest.rsplit_once('-') {
        Some((ver, rel)) => (epoch, ver, Some(rel)),
        None => (epoch, rest, None),
    }
}

/// rpm's `rpmvercmp()` from `lib/rpmvercmp.c`, byte for byte. Nothing here
/// is a design choice; the two places it differs from pacman's are that
/// `~` sorts below everything (so `1.0~rc1` is older than `1.0`) and `^`
/// sorts above a shorter version but below a longer one (so `1.0^git1` is
/// newer than `1.0` and older than `1.0.1`).
pub fn rpmvercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (one, two) = (a.as_bytes(), b.as_bytes());
    let at = |s: &[u8], k: usize| s.get(k).copied().unwrap_or(0);
    let is_sep = |c: u8| c != 0 && !c.is_ascii_alphanumeric() && c != b'~' && c != b'^';
    let (mut i, mut j) = (0usize, 0usize);
    while i < one.len() || j < two.len() {
        while is_sep(at(one, i)) {
            i += 1;
        }
        while is_sep(at(two, j)) {
            j += 1;
        }
        let (c1, c2) = (at(one, i), at(two, j));
        if c1 == b'~' || c2 == b'~' {
            if c1 != b'~' {
                return Ordering::Greater;
            }
            if c2 != b'~' {
                return Ordering::Less;
            }
            i += 1;
            j += 1;
            continue;
        }
        if c1 == b'^' || c2 == b'^' {
            if c1 == 0 {
                return Ordering::Less;
            }
            if c2 == 0 {
                return Ordering::Greater;
            }
            if c1 != b'^' {
                return Ordering::Greater;
            }
            if c2 != b'^' {
                return Ordering::Less;
            }
            i += 1;
            j += 1;
            continue;
        }
        if c1 == 0 || c2 == 0 {
            break;
        }
        let (s1, s2) = (i, j);
        let isnum = c1.is_ascii_digit();
        if isnum {
            while at(one, i).is_ascii_digit() {
                i += 1;
            }
            while at(two, j).is_ascii_digit() {
                j += 1;
            }
        } else {
            while at(one, i).is_ascii_alphabetic() {
                i += 1;
            }
            while at(two, j).is_ascii_alphabetic() {
                j += 1;
            }
        }
        // Segments of different kinds: the numeric one is newer.
        if s2 == j {
            return if isnum {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        let (mut seg1, mut seg2) = (&one[s1..i], &two[s2..j]);
        if isnum {
            seg1 = strip_zeros(seg1);
            seg2 = strip_zeros(seg2);
            if seg1.len() != seg2.len() {
                return seg1.len().cmp(&seg2.len());
            }
        }
        let rc = seg1.cmp(seg2);
        if rc != Ordering::Equal {
            return rc;
        }
    }
    match (at(one, i) == 0, at(two, j) == 0) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        _ => Ordering::Greater,
    }
}

fn strip_zeros(s: &[u8]) -> &[u8] {
    let n = s.iter().take_while(|&&c| c == b'0').count();
    &s[n..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(a: &str, b: &str) -> i8 {
        match rpmvercmp(a, b) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        }
    }

    #[test]
    fn tilde_and_caret_sort_as_rpm_s_own_tests_say() {
        // Rows from rpm's tests/rpmvercmp.at.
        assert_eq!(seg("1.0~rc1", "1.0"), -1);
        assert_eq!(seg("1.0~rc1", "1.0~rc2"), -1);
        assert_eq!(seg("1.0~rc1~git123", "1.0~rc1"), -1);
        assert_eq!(seg("1.0^", "1.0"), 1);
        assert_eq!(seg("1.0^git1", "1.0"), 1);
        assert_eq!(seg("1.0^git1", "1.01"), -1);
        assert_eq!(seg("1.0^20160101", "1.0.1"), -1);
        assert_eq!(seg("1.0^20160102", "1.0^20160101^git1"), 1);
        assert_eq!(seg("1.0~rc1^git1", "1.0~rc1"), 1);
        assert_eq!(seg("1.0^git1", "1.0^git1~pre"), 1);
    }

    #[test]
    fn a_missing_release_loses_and_a_missing_epoch_is_zero() {
        assert_eq!(rpm_compare("1.0-1", "1.0"), Ordering::Greater);
        assert_eq!(rpm_compare("0:1.0-1", "1.0-1"), Ordering::Equal);
        assert_eq!(rpm_compare("1:0.1-1", "9.9-1"), Ordering::Greater);
        assert!(rpm_is_newer("1.0-1.fc40", "1.0-1.fc39"));
        assert!(!rpm_is_newer("1.0-1.fc39", "1.0-1.fc39"));
    }

    #[test]
    fn both_generations_versions_parse() {
        assert_eq!(
            parse_version("4.18.2\n  Installed: dnf-0:4.18.2-1.fc39.noarch\n"),
            Some("4.18.2".to_string())
        );
        assert_eq!(
            parse_version("dnf5 version 5.1.17\ndnf5 plugin API version 2.0\n"),
            Some("5.1.17".to_string())
        );
        assert_eq!(parse_version("nonsense"), None);
    }

    #[test]
    fn the_queryformat_is_nine_tab_separated_tags_and_a_newline() {
        assert_eq!(QUERY_FORMAT.matches('\t').count(), 8);
        assert!(QUERY_FORMAT.ends_with('\n'));
        assert!(QUERY_FORMAT.contains("\t%{repoid}\t"));
        assert!(!QUERY_FORMAT.contains('\\'), "real tabs, never escapes");
        assert_eq!(RPM_FORMAT.matches('\t').count(), 8);
    }

    #[test]
    fn times_come_in_both_shapes() {
        assert_eq!(parse_time("1703000000"), Some(1_703_000_000));
        assert_eq!(parse_time("1970-01-01 00:00"), Some(0));
        assert_eq!(parse_time("2023-12-19 16:13"), Some(1_703_002_380));
        assert_eq!(parse_time("2000-02-29 12:00:30"), Some(951_825_630));
        assert_eq!(parse_time(""), None);
        assert_eq!(parse_time("yesterday"), None);
    }

    #[test]
    fn rows_treat_none_and_zero_as_absent() {
        let rows =
            parse_rows("a\t1-1\tfedora\tSummary\tNone\t(none)\t0\t\t\nshort\n\nb\t2-1\t@System\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].url, None);
        assert_eq!(rows[0].licence, None);
        assert_eq!(rows[0].download_size, None);
        assert_eq!(rows[0].build_time, None);
        assert_eq!(rows[1].summary, None);
        assert_eq!(rows[1].repo, "@System");
    }

    #[test]
    fn the_glob_never_starts_with_a_dash() {
        assert_eq!(search_glob("-foo"), "*-foo*");
        assert_eq!(search_glob("gnome  terminal"), "*gnome*terminal*");
    }

    #[test]
    fn descriptions_split_on_blank_lines() {
        assert_eq!(
            split_paragraphs("one\nline\n\ntwo\n"),
            vec!["one line".to_string(), "two".to_string()]
        );
    }

    #[test]
    fn combining_keeps_the_newest_available_row_and_marks_installed() {
        let row = |name: &str, version: &str, repo: &str| Row {
            name: name.to_string(),
            version: version.to_string(),
            repo: repo.to_string(),
            ..Row::default()
        };
        let available = vec![
            row("gimp", "2.10.36-4.fc40", "fedora"),
            row("gimp", "2.10.38-1.fc40", "updates"),
        ];
        let installed = newest_by_name(vec![
            row("gimp", "2.10.36-4.fc40", "@System"),
            row("kernel", "6.9.7-200.fc40", "@System"),
            row("kernel", "6.9.9-200.fc40", "@System"),
        ]);
        let entries = combine(available, installed);
        assert_eq!(
            entries["gimp"]
                .available
                .as_ref()
                .map(|r| r.version.as_str()),
            Some("2.10.38-1.fc40")
        );
        assert_eq!(
            entries["gimp"]
                .installed
                .as_ref()
                .map(|r| r.version.as_str()),
            Some("2.10.36-4.fc40")
        );
        assert_eq!(entries["kernel"].available, None);
        assert_eq!(
            entries["kernel"]
                .installed
                .as_ref()
                .map(|r| r.version.as_str()),
            Some("6.9.9-200.fc40")
        );
    }
}
