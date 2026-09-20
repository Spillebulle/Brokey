//! Chocolatey: installed packages read from `.nuspec` files under
//! `%ChocolateyInstall%\lib`, and search against the community feed's OData
//! v2 endpoint, which needs no local tool at all.
//!
//! Two readers live here, both pure functions of bytes, so they run without
//! Chocolatey, without the network, and on Linux: [`parse_nuspec`] turns one
//! installed package's `<metadata>` into a [`Nuspec`], and [`parse_search`]
//! turns the feed's Atom XML into a list of [`SearchEntry`]. The feed's own
//! trap is that a package's id and its display name come out of two
//! elements with almost the same name: `<title>` is the id (`7zip`) and
//! `<d:Title>` is the name (`7-Zip`); `<id>` is an OData URL and is never
//! read as one.
//!
//! `Source::setup` answers `None` here on purpose; see its doc comment.

use crate::http::Client;
use crate::model::{Command, Op, Package, Picture, SourceKind, SourceStatus, Step};
use crate::{Error, Query, Result, Setup, Source, Update};
use quick_xml::Reader;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesRef, Event};
use std::path::PathBuf;
use std::sync::Arc;

/// Where Chocolatey lives when `%ChocolateyInstall%` is unset.
pub const DEFAULT_ROOT: &str = r"C:\ProgramData\chocolatey";

/// `%ChocolateyInstall%`, or its default when the variable is unset.
fn install_root() -> PathBuf {
    PathBuf::from(std::env::var("ChocolateyInstall").unwrap_or_else(|_| DEFAULT_ROOT.to_string()))
}

/// Where `choco.exe` is, if it is anywhere: on `PATH`, else the default
/// bin directory under `%ChocolateyInstall%`. Reads the environment and the
/// filesystem, never the Windows API, so it runs (and always answers `None`)
/// on Linux as well.
pub fn choco_exe() -> Option<PathBuf> {
    if let Some(p) = crate::system::which("choco") {
        return Some(p);
    }
    let candidate = install_root().join("bin").join("choco.exe");
    candidate.is_file().then_some(candidate)
}

/// The program a step names: the full path to `choco.exe` when it can be
/// found, the bare name otherwise. Resolution happens here, in the
/// unelevated process, the same as `winget::winget_program` and for the
/// same reason: the elevated helper must never search for a program itself.
pub fn choco_program() -> String {
    choco_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "choco.exe".to_string())
}

pub const NOT_INSTALLED: &str = "Chocolatey is not installed, so nothing can be installed, \
     updated or removed through it. Its community feed answers over HTTP, so Brokey still \
     searches it.";

/// One `.nuspec`'s `<metadata>`, the fields this source reads. Everything
/// but `id` and `version` is optional because Chocolatey does not require
/// the rest, and the reference machine's own packages leave some of them out
/// (`choco-core-extension.nuspec` has no `<iconUrl>`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Nuspec {
    pub id: String,
    pub version: String,
    pub title: Option<String>,
    pub authors: Option<String>,
    pub description: Option<String>,
    pub summary: Option<String>,
    pub project_url: Option<String>,
    pub license_url: Option<String>,
    pub icon_url: Option<String>,
    /// Space separated, the way NuGet writes them.
    pub tags: Option<String>,
}

/// The elements read out of a `.nuspec`'s `<metadata>`. Everything else in
/// the file (`<owners>`, `<dependencies>`, `<requireLicenseAcceptance>`, and
/// so on) is walked over and ignored: the stack still tracks it, so its
/// `Start`/`End` pair never confuses which element is currently open, but
/// its text is never captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetaTag {
    Package,
    Metadata,
    Id,
    Version,
    Title,
    Authors,
    Description,
    Summary,
    ProjectUrl,
    LicenseUrl,
    IconUrl,
    Tags,
    Other,
}

impl MetaTag {
    fn of(parent: Option<MetaTag>, name: &str) -> MetaTag {
        match (parent, name) {
            (None, "package") => MetaTag::Package,
            (Some(MetaTag::Package), "metadata") => MetaTag::Metadata,
            (Some(MetaTag::Metadata), "id") => MetaTag::Id,
            (Some(MetaTag::Metadata), "version") => MetaTag::Version,
            (Some(MetaTag::Metadata), "title") => MetaTag::Title,
            (Some(MetaTag::Metadata), "authors") => MetaTag::Authors,
            (Some(MetaTag::Metadata), "description") => MetaTag::Description,
            (Some(MetaTag::Metadata), "summary") => MetaTag::Summary,
            (Some(MetaTag::Metadata), "projectUrl") => MetaTag::ProjectUrl,
            (Some(MetaTag::Metadata), "licenseUrl") => MetaTag::LicenseUrl,
            (Some(MetaTag::Metadata), "iconUrl") => MetaTag::IconUrl,
            (Some(MetaTag::Metadata), "tags") => MetaTag::Tags,
            _ => MetaTag::Other,
        }
    }
}

fn is_meta_field(tag: MetaTag) -> bool {
    matches!(
        tag,
        MetaTag::Id
            | MetaTag::Version
            | MetaTag::Title
            | MetaTag::Authors
            | MetaTag::Description
            | MetaTag::Summary
            | MetaTag::ProjectUrl
            | MetaTag::LicenseUrl
            | MetaTag::IconUrl
            | MetaTag::Tags
    )
}

fn set_meta_field(n: &mut Nuspec, tag: MetaTag, value: &str) {
    let some = (!value.is_empty()).then(|| value.to_string());
    match tag {
        MetaTag::Id => n.id = value.to_string(),
        MetaTag::Version => n.version = value.to_string(),
        MetaTag::Title => n.title = some,
        MetaTag::Authors => n.authors = some,
        MetaTag::Description => n.description = some,
        MetaTag::Summary => n.summary = some,
        MetaTag::ProjectUrl => n.project_url = some,
        MetaTag::LicenseUrl => n.license_url = some,
        MetaTag::IconUrl => n.icon_url = some,
        MetaTag::Tags => n.tags = some,
        MetaTag::Package | MetaTag::Metadata | MetaTag::Other => {}
    }
}

/// Resolve one `&ref;` into `text`, the same choice the AppStream reader
/// makes: a numeric reference decodes to its character, a predefined named
/// one (`&amp;`, `&lt;`, `&gt;`, `&apos;`, `&quot;`) decodes through
/// quick-xml's own table, and anything else is kept exactly as written.
/// None of the fixtures this module reads carry an entity, so this exists
/// for the real feed and real packages this cannot see in a test.
fn append_ref(text: &mut String, r: &BytesRef) {
    match r.resolve_char_ref() {
        Ok(Some(ch)) => text.push(ch),
        _ => match resolve_predefined_entity(r) {
            Some(s) => text.push_str(s),
            None => {
                text.push('&');
                text.push_str(r);
                text.push(';');
            }
        },
    }
}

fn nuspec_xml_error(e: &quick_xml::Error) -> Error {
    Error::from_source(
        SourceKind::Choco,
        format!("This package's .nuspec is not well-formed XML: {e}."),
    )
}

/// Parse one `.nuspec` file's bytes into its `<metadata>`.
///
/// A leading UTF-8 byte order mark is stripped before anything else looks
/// at the bytes: `choco-vlc-nightly.nuspec`, a real file off the reference
/// machine, carries one, and every real Chocolatey package can.
/// `<package>` may carry any `xmlns`; elements are matched by their local
/// name, so the namespace never has to be resolved.
pub fn parse_nuspec(bytes: &[u8]) -> Result<Nuspec> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let mut reader = Reader::from_reader(bytes);
    let mut buf = Vec::new();
    let mut stack: Vec<MetaTag> = Vec::new();
    let mut text = String::new();
    let mut nuspec = Nuspec::default();
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| nuspec_xml_error(&e))?;
        match event {
            Event::Start(e) => {
                let name = e.local_name().into_inner();
                let tag = MetaTag::of(stack.last().copied(), name);
                if is_meta_field(tag) {
                    text.clear();
                }
                stack.push(tag);
            }
            Event::Text(t) => {
                if stack.last().copied().is_some_and(is_meta_field) {
                    text.push_str(&t);
                }
            }
            Event::CData(t) => {
                if stack.last().copied().is_some_and(is_meta_field) {
                    text.push_str(&t);
                }
            }
            Event::GeneralRef(r) => {
                if stack.last().copied().is_some_and(is_meta_field) {
                    append_ref(&mut text, &r);
                }
            }
            Event::End(_) => {
                if let Some(tag) = stack.pop()
                    && is_meta_field(tag)
                {
                    set_meta_field(&mut nuspec, tag, text.trim());
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if nuspec.id.trim().is_empty() {
        return Err(Error::from_source(
            SourceKind::Choco,
            "This .nuspec has no <id>, so it is not a package Brokey can show.".to_string(),
        ));
    }
    Ok(nuspec)
}

/// Space-separated tags, the way both a `.nuspec` and the feed write them
/// (not comma separated, which is the mistake to avoid).
fn split_tags(tags: Option<&str>) -> Vec<String> {
    tags.map(|t| t.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// An installed package's `Package`, from its `.nuspec`. `installed` is
/// always `true`: this only ever runs on a `.nuspec` this source found under
/// `lib`, and there is no other way this source learns about a package.
pub fn to_package_from_nuspec(n: &Nuspec) -> Package {
    let name = n.title.clone().unwrap_or_else(|| n.id.clone());
    let mut p = Package::new(SourceKind::Choco, n.id.clone(), name);
    p.installed = true;
    p.installed_version = Some(n.version.clone());
    p.version = Some(n.version.clone());
    p.summary = n.summary.clone();
    p.description = n.description.clone();
    p.developer = n.authors.clone();
    p.homepage = n.project_url.clone();
    p.icon = n.icon_url.clone().map(Picture::Url);
    p.categories = split_tags(n.tags.as_deref());
    if let Some(licence) = &n.license_url {
        p.facts.push(("Licence".to_string(), licence.clone()));
    }
    p
}

/// One `<entry>` of a community feed search reply: the fields this source
/// reads, already told apart from the OData properties that look similar
/// but are not the same thing (see the module doc comment).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchEntry {
    /// From the Atom `<title>`, not from `<id>` (an OData URL) and not from
    /// any `d:Id`, which does not exist.
    pub id: String,
    /// From `<d:Title>`.
    pub name: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub version: Option<String>,
    pub icon_url: Option<String>,
    pub project_url: Option<String>,
    pub license_url: Option<String>,
    pub tags: Option<String>,
}

/// The elements read out of one feed `<entry>`. `Id` here names the Atom
/// `<title>`, which is the package id; the entry's own `<id>` element (an
/// OData URL) and everything else in the entry (`<author>`, `<link>`,
/// `<category>`, `<content>`, `<updated>`) falls to `Other` and is walked
/// over rather than read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryTag {
    Feed,
    Entry,
    Id,
    Name,
    Summary,
    Description,
    Version,
    IconUrl,
    ProjectUrl,
    LicenseUrl,
    Tags,
    Properties,
    Other,
}

impl EntryTag {
    fn of(parent: Option<EntryTag>, name: &str) -> EntryTag {
        match (parent, name) {
            (None, "feed") => EntryTag::Feed,
            (Some(EntryTag::Feed) | None, "entry") => EntryTag::Entry,
            (Some(EntryTag::Entry), "title") => EntryTag::Id,
            (Some(EntryTag::Entry), "summary") => EntryTag::Summary,
            (Some(EntryTag::Entry), "properties") => EntryTag::Properties,
            (Some(EntryTag::Properties), "Version") => EntryTag::Version,
            (Some(EntryTag::Properties), "Title") => EntryTag::Name,
            (Some(EntryTag::Properties), "Description") => EntryTag::Description,
            (Some(EntryTag::Properties), "IconUrl") => EntryTag::IconUrl,
            (Some(EntryTag::Properties), "ProjectUrl") => EntryTag::ProjectUrl,
            (Some(EntryTag::Properties), "LicenseUrl") => EntryTag::LicenseUrl,
            (Some(EntryTag::Properties), "Tags") => EntryTag::Tags,
            _ => EntryTag::Other,
        }
    }
}

fn is_entry_field(tag: EntryTag) -> bool {
    matches!(
        tag,
        EntryTag::Id
            | EntryTag::Name
            | EntryTag::Summary
            | EntryTag::Description
            | EntryTag::Version
            | EntryTag::IconUrl
            | EntryTag::ProjectUrl
            | EntryTag::LicenseUrl
            | EntryTag::Tags
    )
}

fn set_entry_field(entry: &mut SearchEntry, tag: EntryTag, value: &str) {
    let some = (!value.is_empty()).then(|| value.to_string());
    match tag {
        EntryTag::Id => entry.id = value.to_string(),
        EntryTag::Name => entry.name = value.to_string(),
        EntryTag::Summary => entry.summary = some,
        EntryTag::Description => entry.description = some,
        EntryTag::Version => entry.version = some,
        EntryTag::IconUrl => entry.icon_url = some,
        EntryTag::ProjectUrl => entry.project_url = some,
        EntryTag::LicenseUrl => entry.license_url = some,
        EntryTag::Tags => entry.tags = some,
        EntryTag::Feed | EntryTag::Entry | EntryTag::Properties | EntryTag::Other => {}
    }
}

fn feed_xml_error(e: &quick_xml::Error) -> Error {
    Error::from_source(
        SourceKind::Choco,
        format!("The Chocolatey feed did not answer with well-formed XML: {e}."),
    )
}

/// Parse a community feed search reply into its entries.
///
/// The reply is only accepted when its root is an Atom `<feed>`: a query
/// the feed refuses answers 400 with an OData error body, which is XML with
/// a different root, and a proxy or an outage can answer with plain text or
/// an HTML page, neither of which has a `<feed>` at all. Any of those comes
/// back as an error here rather than as a silent empty list, and rather
/// than a panic.
pub fn parse_search(bytes: &[u8]) -> Result<Vec<SearchEntry>> {
    let mut reader = Reader::from_reader(bytes);
    let mut buf = Vec::new();
    let mut stack: Vec<EntryTag> = Vec::new();
    let mut text = String::new();
    let mut draft: Option<SearchEntry> = None;
    let mut out = Vec::new();
    let mut saw_feed = false;
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| feed_xml_error(&e))?;
        match event {
            Event::Start(e) => {
                let name = e.local_name().into_inner();
                let tag = EntryTag::of(stack.last().copied(), name);
                if tag == EntryTag::Feed {
                    saw_feed = true;
                }
                if tag == EntryTag::Entry {
                    draft = Some(SearchEntry::default());
                }
                if is_entry_field(tag) {
                    text.clear();
                }
                stack.push(tag);
            }
            Event::Text(t) => {
                if stack.last().copied().is_some_and(is_entry_field) {
                    text.push_str(&t);
                }
            }
            Event::CData(t) => {
                if stack.last().copied().is_some_and(is_entry_field) {
                    text.push_str(&t);
                }
            }
            Event::GeneralRef(r) => {
                if stack.last().copied().is_some_and(is_entry_field) {
                    append_ref(&mut text, &r);
                }
            }
            Event::End(_) => {
                if let Some(tag) = stack.pop() {
                    if tag == EntryTag::Entry {
                        if let Some(entry) = draft.take() {
                            out.push(entry);
                        }
                    } else if is_entry_field(tag)
                        && let Some(entry) = &mut draft
                    {
                        set_entry_field(entry, tag, text.trim());
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if !saw_feed {
        return Err(Error::from_source(
            SourceKind::Choco,
            "The Chocolatey feed did not answer with a search feed. It may have rejected the \
             search, or the connection may have returned something else entirely."
                .to_string(),
        ));
    }
    Ok(out)
}

/// A search entry's `Package`. `installed` and `installed_version` stay
/// `false`/`None` here; [`Choco::search`] fills them in afterwards by
/// joining against what is actually on the machine, the way `Source::search`
/// promises callers a search result already answers `installed`.
pub fn to_package_from_entry(e: &SearchEntry) -> Package {
    let name = if e.name.trim().is_empty() {
        e.id.clone()
    } else {
        e.name.clone()
    };
    let mut p = Package::new(SourceKind::Choco, e.id.clone(), name);
    p.summary = e.summary.clone();
    p.description = e.description.clone();
    p.version = e.version.clone();
    p.homepage = e.project_url.clone();
    p.icon = e.icon_url.clone().map(Picture::Url);
    p.categories = split_tags(e.tags.as_deref());
    if let Some(licence) = &e.license_url {
        p.facts.push(("Licence".to_string(), licence.clone()));
    }
    p
}

/// Percent-encode a query value for [`search_url`]. Unreserved characters
/// (RFC 3986) pass through; everything else becomes `%XX`.
fn percent_encode(s: &str) -> String {
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

pub const SEARCH_BASE: &str = "https://community.chocolatey.org/api/v2/Search()";

/// The exact query the plan pins: dropping `targetFramework` or
/// `includePrerelease` answers 400 with "Error in query syntax", not a
/// useful message, so both stay even though this source never asks for a
/// prerelease.
pub fn search_url(query: &str, limit: usize) -> String {
    format!(
        "{SEARCH_BASE}?$filter=IsLatestVersion&$top={limit}&searchTerm='{}'&targetFramework=''&includePrerelease=false",
        percent_encode(query)
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    Install,
    Update,
    Remove,
}

/// One `choco.exe` call. Every one needs Administrator: the default install
/// root is under `C:\ProgramData`, which only Administrator can write.
pub fn operation_step(kind: OpKind, id: &str, program: &str) -> Step {
    let (verb, title, extra): (&str, String, &[&str]) = match kind {
        OpKind::Install => ("install", format!("Installing {id}"), &[]),
        OpKind::Update => ("upgrade", format!("Updating {id}"), &[]),
        OpKind::Remove => (
            "uninstall",
            format!("Removing {id}"),
            &["--remove-dependencies"],
        ),
    };
    let mut args = vec![verb.to_string(), id.to_string(), "-y".to_string()];
    args.extend(extra.iter().map(|a| a.to_string()));
    Step {
        source: SourceKind::Choco,
        title,
        command: Command {
            program: program.to_string(),
            args,
            env: Vec::new(),
            cwd: None,
        },
        needs_root: true,
        weight: 10,
    }
}

fn count(n: usize) -> String {
    if n == 1 {
        "1 package".to_string()
    } else {
        format!("{n} packages")
    }
}

/// Every `.nuspec` under `lib`, best-effort: a directory that cannot be read
/// (Chocolatey not installed, or nothing installed yet) is simply nothing,
/// not an error, the same as `arp::read`'s missing keys.
fn read_installed(lib_dir: &std::path::Path) -> Vec<Nuspec> {
    let mut out = Vec::new();
    let Ok(packages) = std::fs::read_dir(lib_dir) else {
        return out;
    };
    for package in packages.flatten() {
        let dir = package.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("nuspec"))
                && let Ok(bytes) = std::fs::read(&path)
                && let Ok(nuspec) = parse_nuspec(&bytes)
            {
                out.push(nuspec);
                break;
            }
        }
    }
    out
}

/// The Chocolatey source: local packages read from `lib`, search against
/// the community feed through the shared HTTP client.
pub struct Choco {
    client: Arc<Client>,
    /// `%ChocolateyInstall%\lib` on a real machine; a temporary directory in
    /// every test, so a test run never reads the user's real installation.
    lib_dir: PathBuf,
}

impl Choco {
    pub fn new(client: Arc<Client>) -> Choco {
        Choco {
            client,
            lib_dir: install_root().join("lib"),
        }
    }

    #[cfg(test)]
    fn with_lib_dir(client: Arc<Client>, lib_dir: PathBuf) -> Choco {
        Choco { client, lib_dir }
    }
}

impl Source for Choco {
    fn kind(&self) -> SourceKind {
        SourceKind::Choco
    }

    /// Available when `choco.exe` is on `PATH` or under `%ChocolateyInstall%`.
    /// When it is not, the source stays searchable the way winget does,
    /// because the feed is HTTP and needs no tool.
    fn status(&self) -> SourceStatus {
        match choco_exe() {
            Some(_) => SourceStatus {
                kind: SourceKind::Choco,
                available: true,
                reason: None,
                detail: Some(count(read_installed(&self.lib_dir).len())),
                searchable: true,
                setup: None,
            },
            None => SourceStatus {
                kind: SourceKind::Choco,
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
        let body = self.client.get_text(&search_url(text, query.limit))?;
        let entries = parse_search(body.as_bytes())?;
        let installed = read_installed(&self.lib_dir);
        Ok(entries
            .iter()
            .take(query.limit)
            .map(|e| {
                let mut p = to_package_from_entry(e);
                if let Some(local) = installed.iter().find(|n| n.id.eq_ignore_ascii_case(&e.id)) {
                    p.installed = true;
                    p.installed_version = Some(local.version.clone());
                }
                p
            })
            .collect())
    }

    fn installed(&self) -> Result<Vec<Package>> {
        Ok(read_installed(&self.lib_dir)
            .iter()
            .map(to_package_from_nuspec)
            .collect())
    }

    /// No version comparison against the feed happens here: only
    /// `choco upgrade` itself can say what has moved, and answering
    /// anything else would be inventing an update this source cannot
    /// confirm.
    fn updates(&self) -> Result<Vec<Update>> {
        Ok(Vec::new())
    }

    fn details(&self, id: &str) -> Result<Package> {
        if let Some(n) = read_installed(&self.lib_dir)
            .iter()
            .find(|n| n.id.eq_ignore_ascii_case(id))
        {
            return Ok(to_package_from_nuspec(n));
        }
        let body = self.client.get_text(&search_url(id, 5))?;
        let entries = parse_search(body.as_bytes())?;
        entries
            .iter()
            .find(|e| e.id.eq_ignore_ascii_case(id))
            .map(to_package_from_entry)
            .ok_or_else(|| {
                Error::from_source(
                    SourceKind::Choco,
                    format!("{id} is not installed and is not in the Chocolatey community feed."),
                )
            })
    }

    /// An operation for another source's package plans nothing: the store
    /// asks every source about every operation, and this is the one that
    /// belongs to Chocolatey.
    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        let program = choco_program();
        let step = match op {
            Op::Install { package } if package.source == SourceKind::Choco => {
                operation_step(OpKind::Install, &package.id, &program)
            }
            Op::Update { package } if package.source == SourceKind::Choco => {
                operation_step(OpKind::Update, &package.id, &program)
            }
            Op::Remove { package } if package.source == SourceKind::Choco => {
                operation_step(OpKind::Remove, &package.id, &program)
            }
            _ => return Ok(Vec::new()),
        };
        Ok(vec![step])
    }

    /// Deliberately `None`. The spec's table has Chocolatey's bootstrap
    /// extracting `chocolatey.nupkg` into `C:\ProgramData\chocolatey` and
    /// running its bundled install script, elevated; the invariant that
    /// nothing downloaded is run before it is verified means checking, for
    /// this bootstrap, that the extracted `choco.exe` is Authenticode-signed
    /// by Chocolatey Software, Inc. before any of that runs. That is a
    /// `WinVerifyTrust` call and a new elevated path through the helper's
    /// closed list: larger and more security-sensitive than everything else
    /// in this module put together, and it gets its own plan and its own
    /// review. Until that lands, Chocolatey is searchable but not
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
            .join("tests/fixtures/choco")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()))
    }

    #[test]
    fn a_nuspec_becomes_a_package() {
        let n = parse_nuspec(&fixture_bytes("choco-vlc-nightly.nuspec")).expect("it parses");
        let p = to_package_from_nuspec(&n);
        assert_eq!(p.id, "vlc-nightly");
        assert_eq!(p.version.as_deref(), Some("4.0.0.20250625"));
        assert_eq!(p.name, "VLC Nightly");
        assert!(
            p.description
                .as_deref()
                .is_some_and(|d| d.contains("VideoLAN")),
            "{:?}",
            p.description
        );
        assert_eq!(p.developer.as_deref(), Some("VideoLAN Organization"));
        assert_eq!(
            p.homepage.as_deref(),
            Some("https://nightlies.videolan.org/index.html")
        );
        assert!(p.installed);
    }

    /// The file this reads from begins with a UTF-8 byte order mark. A
    /// reader that hands those three bytes to the XML parser as part of the
    /// document either fails outright or reads a mangled id; this checks the
    /// id is exactly `vlc-nightly`, with nothing stray on the front.
    #[test]
    fn a_byte_order_mark_does_not_stop_the_reader() {
        let bytes = fixture_bytes("choco-vlc-nightly.nuspec");
        assert_eq!(
            &bytes[..3],
            [0xEF, 0xBB, 0xBF],
            "the fixture still has its BOM"
        );
        let n = parse_nuspec(&bytes).expect("it parses despite the BOM");
        assert_eq!(n.id, "vlc-nightly");
    }

    #[test]
    fn a_nuspec_without_a_title_falls_back_to_its_id() {
        let n = parse_nuspec(&fixture_bytes("no-title.nuspec")).expect("it parses");
        assert_eq!(n.title, None);
        let p = to_package_from_nuspec(&n);
        assert_eq!(p.name, n.id);
        assert_eq!(p.name, "no-title-example");
    }

    #[test]
    fn an_icon_url_becomes_a_url_picture() {
        let with_icon = parse_nuspec(&fixture_bytes("choco-vlc-nightly.nuspec")).unwrap();
        let p = to_package_from_nuspec(&with_icon);
        assert!(
            matches!(p.icon, Some(Picture::Url(_))),
            "expected a Picture::Url, got {:?}",
            p.icon
        );

        let without_icon = parse_nuspec(&fixture_bytes("choco-core-extension.nuspec")).unwrap();
        let p2 = to_package_from_nuspec(&without_icon);
        assert_eq!(
            p2.icon, None,
            "no <iconUrl> means no icon, not an empty one"
        );
    }

    #[test]
    fn a_search_reply_becomes_packages() {
        let entries = parse_search(&fixture_bytes("search.xml")).expect("it parses");
        assert_eq!(entries.len(), 2, "{entries:?}");
        let packages: Vec<Package> = entries.iter().map(to_package_from_entry).collect();
        assert!(packages.iter().any(|p| p.id == "7zip" && p.name == "7-Zip"));
        assert!(
            packages
                .iter()
                .any(|p| p.id == "GoogleChrome" && p.name == "Google Chrome")
        );
    }

    /// The mistake this source is most likely to make: taking the id from
    /// `d:Title` (the display name, "7-Zip") or the name from `<title>`
    /// (the id, "7zip"). There is no `d:Id` at all.
    #[test]
    fn the_id_comes_from_the_atom_title_and_the_name_from_d_title() {
        let entries = parse_search(&fixture_bytes("search.xml")).unwrap();
        let sevenzip = entries
            .iter()
            .find(|e| e.id == "7zip")
            .expect("7zip is in the fixture");
        assert_eq!(sevenzip.id, "7zip");
        assert_eq!(sevenzip.name, "7-Zip");
        assert_ne!(sevenzip.id, sevenzip.name);
    }

    #[test]
    fn a_reply_that_is_not_xml_is_an_error_not_a_panic() {
        assert!(parse_search(b"Error in query syntax.").is_err());
        assert!(parse_search(b"<html><body>Not Found</body></html>").is_err());
        assert!(parse_search(b"").is_err());
    }

    /// The feed's own error body for the fussy query in the module doc
    /// comment is itself well-formed XML, just not a `<feed>`. A reader that
    /// accepts any XML document, not specifically a feed, would answer an
    /// empty list here instead of an error.
    #[test]
    fn an_odata_error_body_is_an_error_not_an_empty_list() {
        let body = br#"<?xml version="1.0" encoding="utf-8" standalone="yes"?>
<m:error xmlns:m="http://schemas.microsoft.com/ado/2007/08/dataservices/metadata">
  <m:message xml:lang="en-US">Error in query syntax.</m:message>
</m:error>"#;
        assert!(parse_search(body).is_err());
    }

    #[test]
    fn every_operation_needs_administrator() {
        let cases = [
            (OpKind::Install, vec!["install", "7zip", "-y"]),
            (OpKind::Update, vec!["upgrade", "7zip", "-y"]),
            (
                OpKind::Remove,
                vec!["uninstall", "7zip", "-y", "--remove-dependencies"],
            ),
        ];
        for (kind, expected) in cases {
            let step = operation_step(kind, "7zip", "choco.exe");
            assert_eq!(step.command.args, expected, "{kind:?}");
            assert_eq!(step.command.program, "choco.exe");
            assert!(step.needs_root, "{kind:?} must need Administrator");
            assert_eq!(step.source, SourceKind::Choco);
        }
    }

    #[test]
    fn setup_is_deliberately_none() {
        let choco = Choco::with_lib_dir(Client::shared(), std::env::temp_dir());
        assert!(choco.setup().is_none());
    }

    #[test]
    fn installed_reads_nuspec_files_under_the_lib_directory() {
        let lib = tempfile::tempdir().expect("a temporary directory");
        let pkg_dir = lib.path().join("vlc-nightly");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("vlc-nightly.nuspec"),
            fixture_bytes("choco-vlc-nightly.nuspec"),
        )
        .unwrap();

        let choco = Choco::with_lib_dir(Client::shared(), lib.path().to_path_buf());
        let installed = choco.installed().unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].id, "vlc-nightly");
        assert!(installed[0].installed);
    }

    /// A missing `lib` directory (Chocolatey not installed, or nothing
    /// installed yet) is an empty list, not an error.
    #[test]
    fn a_missing_lib_directory_is_simply_empty() {
        let lib = tempfile::tempdir().expect("a temporary directory");
        let missing = lib.path().join("does-not-exist");
        let choco = Choco::with_lib_dir(Client::shared(), missing);
        assert_eq!(choco.installed().unwrap(), Vec::new());
    }

    #[test]
    fn the_search_url_carries_every_fussy_parameter() {
        let url = search_url("steam", 40);
        assert!(url.contains("targetFramework=''"), "{url}");
        assert!(url.contains("includePrerelease=false"), "{url}");
        assert!(url.contains("searchTerm='steam'"), "{url}");
        assert!(url.contains("$top=40"), "{url}");
        assert!(url.contains("$filter=IsLatestVersion"), "{url}");
    }

    #[test]
    fn plan_only_answers_for_its_own_packages_and_always_needs_root() {
        let choco = Choco::new(Client::shared());
        let mine = PackageRef {
            source: SourceKind::Choco,
            id: "7zip".to_string(),
        };
        let steps = choco
            .plan(&Op::Install {
                package: mine.clone(),
            })
            .unwrap();
        assert_eq!(steps.len(), 1);
        assert!(steps[0].needs_root);
        assert_eq!(steps[0].source, SourceKind::Choco);

        let someone_elses = PackageRef {
            source: SourceKind::Winget,
            id: "Valve.Steam".to_string(),
        };
        assert!(
            choco
                .plan(&Op::Install {
                    package: someone_elses,
                })
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn refresh_and_update_all_plan_nothing() {
        let choco = Choco::new(Client::shared());
        for op in [
            Op::Refresh {
                source: SourceKind::Choco,
            },
            Op::UpdateAll {
                source: SourceKind::Choco,
            },
            Op::Setup {
                source: SourceKind::Choco,
            },
        ] {
            assert!(choco.plan(&op).unwrap().is_empty(), "{op:?}");
        }
    }
}
