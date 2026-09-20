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
use crate::model::{Command, Op, Package, PackageKind, Picture, SourceKind, SourceStatus, Step};
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
/// filesystem, never the Windows API, so this runs on Linux too: it looks
/// for a program literally named `choco` on `PATH` there (there is almost
/// never one) and for `bin/choco.exe` under the default root (there is
/// never one), so it answers `None` in practice on Linux, not because
/// either check is skipped for that platform.
pub fn choco_exe() -> Option<PathBuf> {
    if let Some(p) = crate::system::which("choco") {
        return Some(p);
    }
    let candidate = install_root().join("bin").join("choco.exe");
    candidate.is_file().then_some(candidate)
}

/// The program a step names: the full path to `choco.exe`, which
/// [`choco_exe`] found. There is no fallback to a bare name, because
/// [`Choco::plan`] refuses before it gets here when nothing was found, and
/// the closed list in `transaction::allow` would refuse a bare name anyway,
/// which is a refusal about the wrong thing.
///
/// Resolution happens in the unelevated process, the same as
/// `winget::winget_program` and `scoop::step_program` and for the same
/// reason: the elevated helper must never search for a program itself.
/// What was found is the argument rather than a second probe, so
/// `Choco::plan` is a function of the field it resolved once and not of the
/// environment a test happens to run in.
pub fn choco_program(exe: &std::path::Path) -> String {
    exe.to_string_lossy().into_owned()
}

pub const NOT_INSTALLED: &str = "Chocolatey is not installed, so nothing can be installed, \
     updated or removed through it. Its community feed answers over HTTP, so Brokey still \
     searches it.";

/// The error [`Choco::plan`] answers with when Chocolatey is not on the
/// machine. `NOT_INSTALLED` is the status reason, which says the source is
/// still searchable; this is the sentence for someone who has pressed
/// Install, so it says what to do instead.
pub const CANNOT_PLAN: &str = "Chocolatey is not installed, so Brokey cannot install, update \
     or remove anything through it. Install Chocolatey from https://chocolatey.org/install \
     first, then try again.";

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
/// `<package>` may carry any `xmlns`; elements are matched by their local
/// name, so the namespace never has to be resolved.
///
/// A leading UTF-8 byte order mark is stripped before anything else looks
/// at the bytes: `choco-vlc-nightly.nuspec`, a real file off the reference
/// machine, carries one, and every real Chocolatey package can. This is
/// belt and braces rather than what makes that fixture parse: quick-xml
/// itself already strips one leading BOM character on the very first read,
/// unconditionally, independent of its `encoding` feature (confirmed
/// against `quick_xml` 0.42.0's own reader and its `bom_from_reader` /
/// `bom_from_str` tests). The explicit strip earns its keep for a reader
/// that decodes the bytes to a `String` first and looks for `<?xml` as a
/// literal prefix, which really does choke on the three stray bytes. That
/// is not what this function does, but it might be what a future version
/// of it does.
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
    if nuspec.version.trim().is_empty() {
        return Err(Error::from_source(
            SourceKind::Choco,
            format!(
                "{}'s .nuspec has no <version>, so it is not a package Brokey can show.",
                nuspec.id
            ),
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

/// What every package this source builds is drawn as.
///
/// Neither a `.nuspec` nor a feed entry says whether it describes an
/// application: there is no such field in either, so nothing can be read
/// off the feed to decide it one package at a time. This source takes
/// winget's position instead, which `winget::to_package` states by setting
/// the same value: Chocolatey exists to install applications, so its
/// entries are applications.
///
/// Leaving `Package::new`'s `PackageKind::Package` in place is not the
/// neutral choice it looks like. The Search page starts on its Applications
/// filter, so a package drawn as `Package` is hidden behind the "hidden by
/// the Applications filter" line the moment it arrives, and Chocolatey
/// would be invisible on a default search.
const KIND: PackageKind = PackageKind::App;

/// An installed package's `Package`, from its `.nuspec`. `installed` is
/// always `true`: this only ever runs on a `.nuspec` this source found under
/// `lib`, and there is no other way this source learns about a package.
pub fn to_package_from_nuspec(n: &Nuspec) -> Package {
    let name = n.title.clone().unwrap_or_else(|| n.id.clone());
    let mut p = Package::new(SourceKind::Choco, n.id.clone(), name);
    p.kind = KIND;
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
        p.facts.push(("Licence URL".to_string(), licence.clone()));
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
    p.kind = KIND;
    p.summary = e.summary.clone();
    p.description = e.description.clone();
    p.version = e.version.clone();
    p.homepage = e.project_url.clone();
    p.icon = e.icon_url.clone().map(Picture::Url);
    p.categories = split_tags(e.tags.as_deref());
    if let Some(licence) = &e.license_url {
        p.facts.push(("Licence URL".to_string(), licence.clone()));
    }
    p
}

/// Percent-encode a query value for [`search_url`]. Unreserved characters
/// (RFC 3986) pass through; everything else becomes `%XX`.
///
/// The value lands inside an OData v2 single-quoted string literal,
/// `searchTerm='<value>'`. An apostrophe in the term cannot simply be
/// percent-encoded like anything else: the server percent-decodes the whole
/// query string before it ever reads the literal, so a `%27` arrives back
/// as a plain `'` and still closes the literal early, `Error in query
/// syntax` the same way a missing parameter does. OData escapes a quote
/// inside such a literal by doubling it instead, so that happens first,
/// before the doubled quotes are themselves percent-encoded along with
/// everything else.
fn percent_encode(s: &str) -> String {
    let doubled = s.replace('\'', "''");
    let mut out = String::with_capacity(doubled.len());
    for b in doubled.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// [`Choco::search`]'s installed-join, pulled out as a pure function of its
/// inputs so it can be tested without the network or the filesystem: turn
/// each search entry into a `Package`, then mark it installed (and carry its
/// installed version) when `installed` holds a `Nuspec` with the same id,
/// matched case-insensitively. Mirrors `winget::plan_with`'s split between
/// an environment-touching method and the pure function it calls.
fn joined(entries: &[SearchEntry], installed: &[Nuspec], limit: usize) -> Vec<Package> {
    entries
        .iter()
        .take(limit)
        .map(|e| {
            let mut p = to_package_from_entry(e);
            if let Some(local) = installed.iter().find(|n| n.id.eq_ignore_ascii_case(&e.id)) {
                p.installed = true;
                p.installed_version = Some(local.version.clone());
            }
            p
        })
        .collect()
}

pub const SEARCH_BASE: &str = "https://community.chocolatey.org/api/v2/Search()";

/// Where the network half of this source gets its text. [`Client`] is the
/// only implementation outside this file's own tests, which use one of
/// their own so that a test of the search path never reaches the feed and
/// can count the requests a search makes. The same shape as `scoop::Http`,
/// and deliberately a second trait rather than one shared with it: the line
/// that implements it for `Client` is exactly where the two differ. Scoop's
/// bucket listing is Brokey's snapshot of somebody else's catalogue and is
/// cached for a day; a Chocolatey search is a live query against a feed and
/// is not cached at all.
trait Http: Send + Sync {
    fn text(&self, url: &str) -> Result<String>;
}

impl Http for Client {
    fn text(&self, url: &str) -> Result<String> {
        self.get_text(url)
    }
}

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

/// The query [`Choco::details`] asks: the one package whose id is `id`,
/// rather than a ranked search whose first few answers are then looked
/// through. `tolower(Id) eq '<id>'` in the `$filter` is what makes it a
/// lookup, and the whole `$filter` is one OData expression, so the spaces
/// around `and` and `eq` are percent-encoded while the rest of the query
/// string is written the way [`search_url`] writes it.
///
/// Three things about this were measured against the live feed rather than
/// assumed, because none of them is what an OData endpoint has to do.
/// `Id eq '7ZIP'` answers nothing for the package whose id is `7zip`: the
/// comparison is case sensitive, which does not match
/// `eq_ignore_ascii_case` on this side, so `tolower(Id)` is what the filter
/// has to say and `id` is lowered to meet it. `searchTerm` still has to
/// carry the id: with `searchTerm=''` beside this filter the feed answers
/// a well-formed feed with no entries in it, for a package that is really
/// there. And an id no package has answers an empty feed rather than an
/// error, which is what lets the caller tell "not in the feed" from "the
/// feed would not answer".
///
/// `$top` is 1 because the filter admits one package. The caller still
/// checks the id it gets back, so a feed that ignored the filter would be
/// caught rather than believed.
pub fn details_url(id: &str) -> String {
    format!(
        "{SEARCH_BASE}?$filter=IsLatestVersion%20and%20tolower(Id)%20eq%20'{id}'&$top=1&searchTerm='{id}'&targetFramework=''&includePrerelease=false",
        id = percent_encode(&id.to_lowercase())
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

/// Every package directory under `lib` that holds a `.nuspec`, paired with
/// that file's path. Best-effort throughout: a directory that cannot be
/// read (Chocolatey not installed, or nothing installed yet) is simply
/// nothing, not an error, the same as `arp::read`'s missing keys.
///
/// A directory with no `.nuspec` at all is left out rather than counted.
/// This is a deliberate choice, not an oversight: on the reference machine
/// `lib\chocolatey` (Chocolatey's own package) holds only `chocolatey.nupkg`
/// and no `.nuspec`, so this source has no id, no version and no name for
/// it, nothing a `Package` could honestly carry. `status()`'s count and
/// `installed()`'s list therefore both read one fewer than the number of
/// directories under `lib`, on a machine with Chocolatey installed this way.
fn nuspec_paths(lib_dir: &std::path::Path) -> Vec<PathBuf> {
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
        let found = files.flatten().map(|f| f.path()).find(|p| {
            p.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("nuspec"))
        });
        if let Some(path) = found {
            out.push(path);
        }
    }
    out
}

/// Every installed package, parsed. A `.nuspec` that exists but does not
/// parse (truncated, not XML, missing `<id>` or `<version>`) is left out
/// the same way a missing directory is: best-effort, not an error.
fn read_installed(lib_dir: &std::path::Path) -> Vec<Nuspec> {
    nuspec_paths(lib_dir)
        .iter()
        .filter_map(|path| std::fs::read(path).ok())
        .filter_map(|bytes| parse_nuspec(&bytes).ok())
        .collect()
}

/// How many installed packages there are, from the directory listing alone:
/// whether a `.nuspec` is present, never its contents. `status()` runs
/// before every search, installed list, updates run and plan, and again
/// whenever the page redraws its source list, and `winget::cached_detail`'s
/// doc comment states the same rule for the same reason. So this has to
/// stay a `read_dir` rather than a parse of every package, once there are
/// two hundred of them rather than five.
fn count_installed(lib_dir: &std::path::Path) -> usize {
    nuspec_paths(lib_dir).len()
}

/// The Chocolatey source: local packages read from `lib`, search against
/// the community feed through the shared HTTP client.
pub struct Choco {
    http: Arc<dyn Http>,
    /// `%ChocolateyInstall%\lib` on a real machine, from `Choco::new`; a
    /// temporary directory in every test, from `Choco::with_lib_dir`, so
    /// that no test reads the user's real installation.
    lib_dir: PathBuf,
    /// `choco.exe`, resolved once in [`Choco::new`] and never probed again,
    /// the same field `Scoop` keeps for the same reason. The machine's
    /// `PATH` and `%ChocolateyInstall%` are read there and nowhere else in
    /// this source, so `status` and `plan` are a function of this field
    /// rather than of the environment a test happens to run in, and a test
    /// can force it either way.
    exe: Option<PathBuf>,
}

impl Choco {
    pub fn new(client: Arc<Client>) -> Choco {
        Choco {
            http: client,
            lib_dir: install_root().join("lib"),
            exe: choco_exe(),
        }
    }

    #[cfg(test)]
    fn with_lib_dir(http: Arc<dyn Http>, lib_dir: PathBuf, exe: Option<PathBuf>) -> Choco {
        Choco { http, lib_dir, exe }
    }
}

impl Source for Choco {
    fn kind(&self) -> SourceKind {
        SourceKind::Choco
    }

    /// Available when `choco.exe` was on `PATH` or under
    /// `%ChocolateyInstall%` when this source was built. When it was not,
    /// the source stays searchable the way winget does, because the feed is
    /// HTTP and needs no tool.
    fn status(&self) -> SourceStatus {
        match &self.exe {
            Some(_) => SourceStatus {
                kind: SourceKind::Choco,
                available: true,
                reason: None,
                detail: Some(count(count_installed(&self.lib_dir))),
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
        let body = self.http.text(&search_url(text, query.limit))?;
        let entries = parse_search(body.as_bytes())?;
        let installed = read_installed(&self.lib_dir);
        Ok(joined(&entries, &installed, query.limit))
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

    /// The installed `.nuspec` first, then the feed. The feed half looks
    /// the exact id up rather than running a ranked search and picking
    /// through its first few answers, which is what `Scoop::details` does
    /// and for the same reason: a search is cut down to a handful of
    /// results before anything compares ids, so a package that is really
    /// in the feed reads as missing whenever the ranking does not put it
    /// in that handful. Chocolatey's naming convention makes near misses
    /// the ordinary case, `vlc`, `vlc.install`, `vlc.portable`,
    /// `vlc-nightly`, and the user has usually just clicked the row.
    ///
    /// The lookup [`details_url`] builds answers the one package or none,
    /// so nothing is truncated before the comparison. The comparison stays
    /// anyway: what comes back is a feed the machine on the other end
    /// wrote, and checking the id is cheaper than trusting it.
    fn details(&self, id: &str) -> Result<Package> {
        if let Some(n) = read_installed(&self.lib_dir)
            .iter()
            .find(|n| n.id.eq_ignore_ascii_case(id))
        {
            return Ok(to_package_from_nuspec(n));
        }
        let body = self.http.text(&details_url(id))?;
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

    /// An operation for another source's package plans nothing rather than
    /// failing. The store never asks for one: `transaction::plan`'s
    /// `Gatherer` looks the operation's own source up with `Store::source`
    /// and asks that source alone. So this arm is a guard on the trait's
    /// contract, which a caller holding a `dyn Source` relies on, rather
    /// than the path anything takes in the running application.
    ///
    /// An operation that is Chocolatey's, on a machine that has no
    /// Chocolatey, is an error rather than a step. Every step here needs
    /// Administrator, so without this the button worked, the confirm dialog
    /// counted a password prompt, Windows asked for Administrator, and only
    /// then did the step fail; and since the closed list learned to refuse a
    /// program an unprivileged account can replace, what the user read was
    /// the sentence about a replaceable program, which is not their problem.
    /// The error is raised before a `Step` is built, so
    /// `transaction::plan::build` fails on it before there is a plan for the
    /// dialog to count prompts in.
    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        let (kind, id) = match op {
            Op::Install { package } if package.source == SourceKind::Choco => {
                (OpKind::Install, &package.id)
            }
            Op::Update { package } if package.source == SourceKind::Choco => {
                (OpKind::Update, &package.id)
            }
            Op::Remove { package } if package.source == SourceKind::Choco => {
                (OpKind::Remove, &package.id)
            }
            _ => return Ok(Vec::new()),
        };
        let Some(exe) = self.exe.as_deref() else {
            return Err(Error::from_source(
                SourceKind::Choco,
                CANNOT_PLAN.to_string(),
            ));
        };
        Ok(vec![operation_step(kind, id, &choco_program(exe))])
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

    /// The `choco.exe` a test hands the source when it needs it to think
    /// Chocolatey is installed. Nothing here ever runs it and nothing ever
    /// reads it: `plan` only writes the path into a step, which is why this
    /// need not exist on the machine running the test.
    fn a_choco_exe() -> Option<PathBuf> {
        Some(PathBuf::from(r"C:\ProgramData\chocolatey\bin\choco.exe"))
    }

    /// A stand-in for the community feed, the same test client `scoop.rs`
    /// uses against GitHub: every URL answers out of a table, anything not
    /// in it fails the way an unreachable host does, and every request is
    /// recorded so a test can count and order them. Nothing here opens a
    /// socket, so these tests run offline and on a machine without
    /// Chocolatey.
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
                Error::from_source(SourceKind::Choco, format!("nothing answers {url} here."))
            })
        }
    }

    /// An `Http` that refuses every request, for the tests whose answer has
    /// to come off disk: one that reached for the feed fails here rather
    /// than quietly waiting on a socket or reading the real
    /// community.chocolatey.org.
    struct NoNetwork;

    impl Http for NoNetwork {
        fn text(&self, url: &str) -> Result<String> {
            Err(Error::from_source(
                SourceKind::Choco,
                format!("this test must not ask the network for {url}."),
            ))
        }
    }

    fn no_network() -> Arc<dyn Http> {
        Arc::new(NoNetwork)
    }

    /// One feed reply carrying exactly the entries named, in that order, in
    /// the shape `parse_search` reads: an Atom `<feed>` whose `<title>` is
    /// the id and whose `<d:Title>` is the display name.
    fn feed_of(entries: &[(&str, &str, &str)]) -> String {
        let mut out = String::from(
            r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:d="http://schemas.microsoft.com/ado/2007/08/dataservices" xmlns:m="http://schemas.microsoft.com/ado/2007/08/dataservices/metadata">"#,
        );
        for (id, name, version) in entries {
            out.push_str(&format!(
                r#"
  <entry>
    <id>https://community.chocolatey.org/api/v2/Packages(Id='{id}')</id>
    <title type="text">{id}</title>
    <m:properties>
      <d:Title>{name}</d:Title>
      <d:Version>{version}</d:Version>
    </m:properties>
  </entry>"#
            ));
        }
        out.push_str("\n</feed>");
        out
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
        assert_eq!(
            p.categories,
            vec!["vlc-nightly", "vlc", "videolan", "admin"],
            "the fixture's <tags> are space-separated, not comma-separated"
        );
    }

    /// The file this reads from begins with a UTF-8 byte order mark, pinned
    /// by the first assertion below so this test still means something if
    /// the fixture is ever replaced. `quick_xml` already strips one leading
    /// BOM character on its very first read, unconditionally, before
    /// `parse_nuspec`'s own belt-and-braces strip ever runs (see that
    /// function's doc comment); this test exists to pin the outcome, not the
    /// mechanism: the id comes out as exactly `vlc-nightly`, with nothing
    /// stray on the front, however the BOM was actually removed.
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

    /// Invented bytes, not a fixture off the reference machine: every real
    /// `.nuspec` chocolatey itself writes carries an `<id>`, so there is
    /// nothing genuine to copy here. This exists purely to pin that the
    /// guard at the top of `parse_nuspec` actually rejects the case it
    /// claims to.
    #[test]
    fn a_nuspec_without_an_id_is_rejected() {
        let bytes = br#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <version>1.0.0</version>
  </metadata>
</package>"#;
        assert!(parse_nuspec(bytes).is_err());
    }

    /// Same reasoning as `a_nuspec_without_an_id_is_rejected`: invented, not
    /// copied, because every real `.nuspec` carries a `<version>`.
    #[test]
    fn a_nuspec_without_a_version_is_rejected() {
        let bytes = br#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>no-version-example</id>
  </metadata>
</package>"#;
        let err = parse_nuspec(bytes).expect_err("a missing <version> must be rejected");
        assert!(
            err.to_string().contains("no-version-example"),
            "the error should name the package: {err}"
        );
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

    /// Both halves of this source build applications, not bare packages.
    /// The Search page starts on its Applications filter, so a package
    /// drawn as `PackageKind::Package` never reaches the user at all: it is
    /// counted as hidden and nothing else. This asserts the kind on a
    /// package built from a `.nuspec` and on one built from a feed entry,
    /// because the two are built by different functions and only one of
    /// them was ever exercised by the older tests.
    #[test]
    fn a_chocolatey_package_is_an_application() {
        let n = parse_nuspec(&fixture_bytes("choco-vlc-nightly.nuspec")).expect("it parses");
        assert_eq!(to_package_from_nuspec(&n).kind, PackageKind::App);

        let entries = parse_search(&fixture_bytes("search.xml")).expect("it parses");
        let from_feed = entries.first().expect("the fixture has entries");
        assert_eq!(to_package_from_entry(from_feed).kind, PackageKind::App);
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
        let p = to_package_from_entry(sevenzip);
        assert_eq!(
            p.categories,
            vec!["7zip", "zip", "archiver", "admin", "foss"],
            "the fixture's <d:Tags> are space-separated, not comma-separated"
        );
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
        let choco = Choco::with_lib_dir(no_network(), std::env::temp_dir(), None);
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

        let choco = Choco::with_lib_dir(no_network(), lib.path().to_path_buf(), None);
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
        let choco = Choco::with_lib_dir(no_network(), missing, None);
        assert_eq!(choco.installed().unwrap(), Vec::new());
    }

    /// Pins the deliberate choice documented on `nuspec_paths`: on the
    /// reference machine `lib\chocolatey` (Chocolatey's own package) holds
    /// only `chocolatey.nupkg`, no `.nuspec`, so it has no id, no version and
    /// no name this source could honestly show. A directory like that is
    /// left out of both the count and the list, not counted as a package
    /// Brokey cannot describe.
    #[test]
    fn a_package_directory_without_a_nuspec_is_not_counted_or_shown() {
        let lib = tempfile::tempdir().expect("a temporary directory");
        let with_nuspec = lib.path().join("vlc-nightly");
        std::fs::create_dir_all(&with_nuspec).unwrap();
        std::fs::write(
            with_nuspec.join("vlc-nightly.nuspec"),
            fixture_bytes("choco-vlc-nightly.nuspec"),
        )
        .unwrap();

        let without_nuspec = lib.path().join("chocolatey");
        std::fs::create_dir_all(&without_nuspec).unwrap();
        std::fs::write(without_nuspec.join("chocolatey.nupkg"), b"not xml").unwrap();

        assert_eq!(
            count_installed(lib.path()),
            1,
            "the directory with no .nuspec must not be counted"
        );
        let choco = Choco::with_lib_dir(no_network(), lib.path().to_path_buf(), None);
        let installed = choco.installed().unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].id, "vlc-nightly");
    }

    /// `count_installed` only has to see that a `.nuspec` is present; it
    /// must not need to parse it, because `status()` calls this on every
    /// redraw and a truncated or corrupt file on disk should not make the
    /// count wrong or slow. `read_installed`, which does parse, correctly
    /// excludes the same file.
    #[test]
    fn count_installed_does_not_require_a_nuspec_to_parse() {
        let lib = tempfile::tempdir().expect("a temporary directory");
        let broken = lib.path().join("not-really-a-package");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(
            broken.join("not-really-a-package.nuspec"),
            b"not xml at all",
        )
        .unwrap();

        assert_eq!(count_installed(lib.path()), 1);
        assert_eq!(read_installed(lib.path()), Vec::new());
    }

    #[test]
    fn the_search_url_carries_every_fussy_parameter() {
        assert_eq!(
            search_url("steam", 40),
            "https://community.chocolatey.org/api/v2/Search()?$filter=IsLatestVersion&$top=40&searchTerm='steam'&targetFramework=''&includePrerelease=false"
        );
    }

    /// An apostrophe in the search term cannot survive a plain percent-encode:
    /// the server percent-decodes the whole query string before it reads the
    /// `searchTerm='...'` literal, so a lone `%27` comes back as `'` and
    /// still closes the literal early. OData's own escape is to double the
    /// quote first, and only then percent-encode everything, `'` included.
    #[test]
    fn an_apostrophe_in_the_search_term_is_doubled_then_percent_encoded() {
        let url = search_url("bob's", 10);
        assert!(
            url.contains("searchTerm='bob%27%27s'"),
            "expected a doubled, percent-encoded quote: {url}"
        );

        let spaced = search_url("visual studio code", 10);
        assert!(
            spaced.contains("searchTerm='visual%20studio%20code'"),
            "{spaced}"
        );
    }

    /// Covers each of the three per-package operations, not just `Install`:
    /// a routing bug that sent `Update` or `Remove` down the `Install` arm
    /// (a mutation the first draft of this test suite missed entirely,
    /// because it only ever planned an `Install`) shows up here as the wrong
    /// verb in `args[0]`.
    /// `joined` is `Choco::search`'s installed-join pulled out pure, so this
    /// exercises the match (case-insensitively) and the miss without the
    /// network or the filesystem: `winget::plan_with` is the template.
    #[test]
    fn joined_marks_installed_packages_case_insensitively() {
        let entries = vec![
            SearchEntry {
                id: "7zip".to_string(),
                name: "7-Zip".to_string(),
                version: Some("26.3.0".to_string()),
                ..Default::default()
            },
            SearchEntry {
                id: "GoogleChrome".to_string(),
                name: "Google Chrome".to_string(),
                version: Some("140.0.0".to_string()),
                ..Default::default()
            },
        ];
        let installed = vec![Nuspec {
            id: "7ZIP".to_string(),
            version: "26.2.1".to_string(),
            ..Default::default()
        }];

        let packages = joined(&entries, &installed, 10);
        assert_eq!(packages.len(), 2);
        let sevenzip = packages.iter().find(|p| p.id == "7zip").unwrap();
        assert!(sevenzip.installed, "matched case-insensitively");
        assert_eq!(sevenzip.installed_version.as_deref(), Some("26.2.1"));
        let chrome = packages.iter().find(|p| p.id == "GoogleChrome").unwrap();
        assert!(!chrome.installed);
        assert_eq!(chrome.installed_version, None);
    }

    #[test]
    fn joined_respects_the_limit() {
        let entries = vec![
            SearchEntry {
                id: "a".to_string(),
                ..Default::default()
            },
            SearchEntry {
                id: "b".to_string(),
                ..Default::default()
            },
        ];
        assert_eq!(joined(&entries, &[], 1).len(), 1);
    }

    #[test]
    fn plan_only_answers_for_its_own_packages_and_always_needs_root() {
        let choco = Choco::with_lib_dir(no_network(), std::env::temp_dir(), a_choco_exe());
        let mine = PackageRef {
            source: SourceKind::Choco,
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
                "upgrade",
            ),
            (
                Op::Remove {
                    package: mine.clone(),
                },
                "uninstall",
            ),
        ];
        for (op, verb) in cases {
            let steps = choco.plan(&op).unwrap();
            assert_eq!(steps.len(), 1, "{op:?}");
            assert_eq!(steps[0].command.args[0], verb, "{op:?}");
            assert!(steps[0].needs_root, "{op:?}");
            assert_eq!(steps[0].source, SourceKind::Choco, "{op:?}");
        }

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

    /// A machine without Chocolatey plans nothing, and says so, rather than
    /// building a step that cannot work. The assertion is the error and not
    /// an empty plan: an empty plan is a button that does nothing at all,
    /// which is its own defect.
    ///
    /// What this saves is measured rather than guessed. Every Chocolatey
    /// step sets `needs_root`, so without this the confirm dialog counted a
    /// password prompt, Windows asked for Administrator, and the step then
    /// failed; and since the closed list learned to refuse a program an
    /// unprivileged account can replace, the sentence the user read was the
    /// one about a replaceable program, which is a true thing to say about
    /// `C:\ProgramData\chocolatey\bin\choco.exe` on a machine where anyone
    /// could have made it and no help at all to somebody whose actual
    /// problem is that Chocolatey is not installed.
    #[test]
    fn plan_refuses_when_chocolatey_is_not_installed() {
        let choco = Choco::with_lib_dir(no_network(), std::env::temp_dir(), None);
        let mine = PackageRef {
            source: SourceKind::Choco,
            id: "7zip".to_string(),
        };
        let ops = [
            Op::Install {
                package: mine.clone(),
            },
            Op::Update {
                package: mine.clone(),
            },
            Op::Remove {
                package: mine.clone(),
            },
        ];
        for op in ops {
            let err = choco
                .plan(&op)
                .expect_err("nothing can be planned without choco.exe");
            assert_eq!(err.message, CANNOT_PLAN, "{op:?}");
            assert_eq!(err.source_kind, Some(SourceKind::Choco), "{op:?}");
            assert!(
                err.message.contains("Chocolatey is not installed"),
                "the error names the missing tool: {err}"
            );
            assert!(
                err.message.contains("Install Chocolatey"),
                "the error says what to do: {err}"
            );
        }

        let other = choco
            .plan(&Op::Install {
                package: PackageRef {
                    source: SourceKind::Winget,
                    id: "Valve.Steam".to_string(),
                },
            })
            .expect("another source's operation is nothing to do, not an error");
        assert!(other.is_empty(), "{other:?}");
    }

    /// The constraint is that a source is never silently skipped. Without
    /// Chocolatey the status still says it is unavailable, still says why,
    /// and still says the community feed can be searched anyway. The same
    /// shape as `scoop::status_without_scoop_is_unavailable_searchable_and_says_why`.
    #[test]
    fn status_without_chocolatey_is_unavailable_searchable_and_says_why() {
        let lib = tempfile::tempdir().expect("a temporary directory");
        let choco = Choco::with_lib_dir(no_network(), lib.path().to_path_buf(), None);

        let status = choco.status();
        assert_eq!(status.kind, SourceKind::Choco);
        assert!(!status.available, "no choco.exe was found");
        assert!(
            status.searchable,
            "the community feed answers over HTTP without Chocolatey"
        );
        assert_eq!(status.reason.as_deref(), Some(NOT_INSTALLED));
        assert_eq!(status.detail, None);
        assert!(status.setup.is_none());
    }

    /// With Chocolatey, the status counts what is under `lib`. The count is
    /// the directory listing rather than a parse, which
    /// `count_installed_does_not_require_a_nuspec_to_parse` is what pins;
    /// this pins that `status` is the thing that reports it.
    #[test]
    fn status_with_chocolatey_counts_the_installed_packages() {
        let lib = tempfile::tempdir().expect("a temporary directory");
        for (dir, fixture) in [
            ("vlc-nightly", "choco-vlc-nightly.nuspec"),
            ("chocolatey-core.extension", "choco-core-extension.nuspec"),
        ] {
            let package = lib.path().join(dir);
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(
                package.join(format!("{dir}.nuspec")),
                fixture_bytes(fixture),
            )
            .unwrap();
        }
        let choco = Choco::with_lib_dir(no_network(), lib.path().to_path_buf(), a_choco_exe());

        let status = choco.status();
        assert!(status.available);
        assert_eq!(status.reason, None);
        assert_eq!(status.detail.as_deref(), Some("2 packages"));
        assert!(status.searchable);
    }

    /// One package reads as "1 package" and not "1 packages".
    #[test]
    fn status_counts_one_package_in_the_singular() {
        let lib = tempfile::tempdir().expect("a temporary directory");
        let package = lib.path().join("vlc-nightly");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("vlc-nightly.nuspec"),
            fixture_bytes("choco-vlc-nightly.nuspec"),
        )
        .unwrap();
        let choco = Choco::with_lib_dir(no_network(), lib.path().to_path_buf(), a_choco_exe());

        assert_eq!(choco.status().detail.as_deref(), Some("1 package"));
    }

    /// The network half of `search`, end to end and offline: the one URL
    /// the source asks for, the reply parsed, and the answer joined against
    /// what is under `lib`. `FakeHttp` records what was asked, so this
    /// pins that a search is one request and that it is the request
    /// `search_url` builds, rather than the shape of the URL alone.
    #[test]
    fn search_asks_the_feed_once_and_joins_what_is_installed() {
        let lib = tempfile::tempdir().expect("a temporary directory");
        let package = lib.path().join("vlc-nightly");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("vlc-nightly.nuspec"),
            fixture_bytes("choco-vlc-nightly.nuspec"),
        )
        .unwrap();

        let query = Query::new("vlc");
        let url = search_url("vlc", query.limit);
        let http = Arc::new(FakeHttp::answering(vec![(
            url.clone(),
            feed_of(&[
                ("vlc", "VLC media player", "3.0.21"),
                ("vlc-nightly", "VLC Nightly", "4.0.0.20250701"),
            ]),
        )]));
        let choco = Choco::with_lib_dir(http.clone(), lib.path().to_path_buf(), None);

        let found = choco.search(&query).expect("the feed answered");
        assert_eq!(http.asked(), vec![url], "a search is one request");
        assert_eq!(found.len(), 2, "{found:?}");

        let nightly = found
            .iter()
            .find(|p| p.id == "vlc-nightly")
            .expect("it is in the reply");
        assert!(nightly.installed, "it is under lib as well as in the feed");
        assert_eq!(
            nightly.installed_version.as_deref(),
            Some("4.0.0.20250625"),
            "the version on disk, not the feed's"
        );
        assert_eq!(nightly.version.as_deref(), Some("4.0.0.20250701"));

        let player = found.iter().find(|p| p.id == "vlc").unwrap();
        assert!(!player.installed);
        assert_eq!(player.installed_version, None);
    }

    /// An empty query answers nothing rather than the whole feed, and asks
    /// the feed nothing: the fake would fail every request here, so an
    /// answer of `Ok` is itself the assertion that no request was made.
    #[test]
    fn an_empty_query_searches_for_nothing() {
        let lib = tempfile::tempdir().expect("a temporary directory");
        let choco = Choco::with_lib_dir(no_network(), lib.path().to_path_buf(), None);
        assert_eq!(choco.search(&Query::new("")).unwrap(), Vec::new());
        assert_eq!(choco.search(&Query::new("   ")).unwrap(), Vec::new());
    }

    /// A feed that answers with something that is not a search feed is an
    /// error out of `search`, not an empty result list, so the page says
    /// the source failed rather than that Chocolatey has nothing.
    #[test]
    fn a_search_reply_that_is_not_a_feed_is_an_error_from_search() {
        let query = Query::new("vlc");
        let url = search_url("vlc", query.limit);
        let http = Arc::new(FakeHttp::answering(vec![(
            url,
            "<html><body>502 Bad Gateway</body></html>".to_string(),
        )]));
        let lib = tempfile::tempdir().expect("a temporary directory");
        let choco = Choco::with_lib_dir(http, lib.path().to_path_buf(), None);

        let err = choco.search(&query).expect_err("that is not a feed");
        assert_eq!(err.source_kind, Some(SourceKind::Choco));
    }

    /// `details` looks the exact id up instead of taking a ranked search's
    /// first few answers and hunting through them. The version this
    /// replaces asked `search_url(id, 5)` and then looked for the id among
    /// those five, so a package that is really in the feed read as missing
    /// whenever the ranking did not put it in the first five, which
    /// Chocolatey's naming convention makes ordinary: `vlc`, `vlc.install`,
    /// `vlc.portable`, `vlc-nightly`.
    ///
    /// The fake answers only the exact-id URL, and answers the old
    /// five-result search URL with a crowd of decoys that does not contain
    /// the id. So a `details` that went back to searching would either fail
    /// outright or find nothing, and this test would say so either way.
    #[test]
    fn details_looks_the_exact_id_up_rather_than_searching() {
        let decoys = feed_of(&[
            ("0zip", "Decoy", "1.0"),
            ("17zip", "Decoy", "1.0"),
            ("27zip", "Decoy", "1.0"),
            ("37zip", "Decoy", "1.0"),
            ("47zip", "Decoy", "1.0"),
        ]);
        let http = Arc::new(FakeHttp::answering(vec![
            (details_url("7zip"), feed_of(&[("7zip", "7-Zip", "26.3.0")])),
            (search_url("7zip", 5), decoys),
        ]));
        let lib = tempfile::tempdir().expect("a temporary directory");
        let choco = Choco::with_lib_dir(http.clone(), lib.path().to_path_buf(), None);

        let p = choco.details("7zip").expect("7zip is in the feed");
        assert_eq!(p.id, "7zip");
        assert_eq!(p.name, "7-Zip");
        assert_eq!(p.version.as_deref(), Some("26.3.0"));
        assert!(!p.installed);
        assert_eq!(
            http.asked(),
            vec![details_url("7zip")],
            "one request, and it is the lookup"
        );
    }

    /// The installed `.nuspec` is the answer when there is one, and the
    /// feed is not asked at all: the client here fails every request, so
    /// reaching it would fail this test rather than pass it quietly.
    #[test]
    fn details_answers_the_installed_nuspec_without_asking_the_feed() {
        let lib = tempfile::tempdir().expect("a temporary directory");
        let package = lib.path().join("vlc-nightly");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("vlc-nightly.nuspec"),
            fixture_bytes("choco-vlc-nightly.nuspec"),
        )
        .unwrap();
        let choco = Choco::with_lib_dir(no_network(), lib.path().to_path_buf(), None);

        let p = choco.details("VLC-NIGHTLY").expect("matched without case");
        assert!(p.installed);
        assert_eq!(p.installed_version.as_deref(), Some("4.0.0.20250625"));
    }

    /// An id the feed has no package for answers an empty feed rather than
    /// an error, measured against the live endpoint, so the sentence has to
    /// come from here. It names the id and says both places that were
    /// looked in.
    #[test]
    fn details_says_where_it_looked_when_it_finds_nothing() {
        let http = Arc::new(FakeHttp::answering(vec![(
            details_url("no-such-package"),
            feed_of(&[]),
        )]));
        let lib = tempfile::tempdir().expect("a temporary directory");
        let choco = Choco::with_lib_dir(http, lib.path().to_path_buf(), None);

        let err = choco
            .details("no-such-package")
            .expect_err("no package has that id");
        assert!(err.message.contains("no-such-package"), "{err}");
        assert!(err.message.contains("not installed"), "{err}");
        assert!(err.message.ends_with('.'), "a sentence: {err}");
        assert_eq!(err.source_kind, Some(SourceKind::Choco));
    }

    /// The lookup's URL, pinned whole, because every part of it was
    /// measured against the live feed and any of them dropped changes the
    /// answer: `tolower(Id)` rather than `Id`, because the feed's `eq` is
    /// case sensitive and this side's comparison is not; a `searchTerm`
    /// carrying the id, because an empty one answers no entries for a
    /// package that is really there; and `targetFramework` and
    /// `includePrerelease`, which `search_url` already pins for the same
    /// reason.
    #[test]
    fn the_details_url_is_a_lookup_and_not_a_search() {
        assert_eq!(
            details_url("7zip"),
            "https://community.chocolatey.org/api/v2/Search()?$filter=IsLatestVersion%20and%20tolower(Id)%20eq%20'7zip'&$top=1&searchTerm='7zip'&targetFramework=''&includePrerelease=false"
        );
        assert_eq!(
            details_url("GoogleChrome"),
            details_url("googlechrome"),
            "the id is lowered to meet tolower(Id)"
        );
        assert!(
            details_url("bob's").contains("'bob%27%27s'"),
            "an apostrophe is doubled then encoded, the same as in a search: {}",
            details_url("bob's")
        );
    }

    #[test]
    fn refresh_and_update_all_plan_nothing() {
        let choco = Choco::with_lib_dir(no_network(), std::env::temp_dir(), None);
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
