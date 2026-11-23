//! The Microsoft Store and MSIX: what the Appx state says this user has,
//! and what the Store's own catalogue offers.
//!
//! **Installed** is the Appx state, listed by the Windows Runtime's
//! `PackageManager` for the signed-in user, which is the only place it is
//! listed per user (the registry's `AppModel\Repository` holds every
//! package any user ever had, 1004 keys on the reference machine against
//! 176 registered packages). Each package's own `AppxManifest.xml` is then
//! read for its applications. [`read`] is the only impure part; everything
//! it produces is a [`RawPackage`], and the filter and the conversion are
//! pure functions of that, tested against a fixture exported from the
//! reference machine, the way `arp` is.
//!
//! **What is an application** is [`is_application`]: not a framework, not a
//! resource package, not a bundle, not signed by Windows itself,
//! not one of the two Store-signed packages Windows refuses to remove, and
//! with at least one application the Start menu lists. That last rule is
//! what turns 85 removable packages into the 38 a person would recognise:
//! PowerToys' context menu handlers, codec extensions, runtimes and the
//! identity packages that give a Win32 program (VS Code Insiders, Edge) a
//! package identity all declare `AppListEntry="none"`, and the Win32
//! program is in Add/Remove Programs already.
//!
//! **Search** is the Store's `manifestSearch`, the endpoint winget's own
//! `msstore` source asks. Every row it returns *is* a Store product,
//! identified by its product id, so nothing here matches artwork or
//! anything else to another source's application by name: the spec's
//! "The Store catalogue is never searched by name" struck the Store as an
//! artwork rung for other sources, and this source never offers it as one.
//! Icons for a page of results come from one batched display catalogue
//! request keyed by product id; descriptions and screenshots come on the
//! detail page, keyed by product id or by package family name, both exact.
//!
//! **Installing** is `winget install --source msstore`, so it needs winget,
//! and setting it up chains `Op::Setup { Winget }`. **Removing** is
//! `Remove-AppxPackage` for this user. Neither elevates: a Store
//! application belongs to the user who installed it.
//!
//! A package's id is its package family name (`Microsoft.Paint_8wekyb3d8bbwe`),
//! installed or not, so `msix:<family>` names one application everywhere:
//! the installed list, a search, a plan, and the launcher's
//! `shell:AppsFolder\<family>!<app>`. The one exception is a Win32 product
//! sold through the Store (`XP...`), which has no package family at all and
//! keeps its Store product id. Installing by family asks the display
//! catalogue for the product id at plan time.

use crate::model::{Command, Op, Package, PackageKind, Picture, Screenshot, SourceKind, Step};
use crate::{Error, Query, Result, Setup, Source, SourceStatus, Update};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Where `manifestSearch` lives: the Store's own search, as winget asks it.
pub const SEARCH_URL: &str = "https://storeedgefd.dsx.mp.microsoft.com/v9.0/manifestSearch";

/// One product's page in the same service: title, description, images,
/// rating, for an MSIX product and a Win32 one alike.
pub const PRODUCT_URL: &str = "https://storeedgefd.dsx.mp.microsoft.com/v9.0/products/";

/// The display catalogue: many products in one request, and the one place a
/// package family name leads to its product.
pub const CATALOGUE_URL: &str = "https://displaycatalog.mp.microsoft.com/v7.0/products";

/// How long a catalogue answer is reused. Store listings change slowly and
/// a detail page opened twice in a day should not ask twice.
const CATALOGUE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// The most icons asked for in one display catalogue request. A search
/// shows a page of results; this keeps the request URL ordinary.
const ICONS_PER_REQUEST: usize = 40;

/// The largest logo [`cached_logo`] copies. A Store logo is a few kilobytes;
/// anything near this is not a logo.
const MAX_LOGO_BYTES: u64 = 4 * 1024 * 1024;

/// Packages signed by the Store that Windows will not remove for a user.
/// The public API does not say which packages are non-removable (that flag
/// is only in the state repository, which an unelevated process cannot
/// open), and these two are the Store-signed ones on the reference machine;
/// every other non-removable package there is signed by Windows itself and
/// is dropped for that. App Installer is also winget, which installing from
/// the Store depends on.
pub const PART_OF_WINDOWS: [&str; 2] = ["Microsoft.DesktopAppInstaller", "Microsoft.SecHealthUI"];

/// Who signed a package, as `Package.SignatureKind` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Signature {
    /// Unsigned: registered from a folder in developer mode.
    None,
    /// Signed by its publisher and installed from outside the Store.
    Developer,
    /// Signed by an organisation for its own machines.
    Enterprise,
    /// Signed by the Microsoft Store.
    Store,
    /// Part of Windows.
    System,
}

/// One `<Application>` in a package's manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestApp {
    /// The application's id within the package. With the family name it
    /// makes the Application User Model ID, `family!id`.
    pub id: String,
    /// Whether the Start menu lists it: false for `AppListEntry="none"`.
    pub listed: bool,
}

/// One registered package, as the Appx state and its manifest give it.
/// Nothing is interpreted here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RawPackage {
    /// `Microsoft.Paint_11.2609.441.0_x64__8wekyb3d8bbwe`: what removal names.
    pub full_name: String,
    /// `Microsoft.Paint_8wekyb3d8bbwe`: stable across versions, and this
    /// source's id for an installed package.
    pub family_name: String,
    /// The identity name, `Microsoft.Paint`.
    pub name: String,
    pub version: String,
    /// Resolved by Windows from the manifest's `ms-resource:` reference,
    /// in the user's language. `None` when it would not resolve, and left
    /// unresolved for a package that is not an application, as are
    /// `publisher` and `description`.
    pub display_name: Option<String>,
    pub publisher: Option<String>,
    pub description: Option<String>,
    pub framework: bool,
    pub resource: bool,
    pub bundle: bool,
    pub stub: bool,
    pub signature: Signature,
    pub installed_path: Option<String>,
    /// The manifest's `<Logo>`, relative to `installed_path` and without
    /// the scale qualifier the file on disk carries; see [`resolve_logo`].
    /// Read from the manifest rather than asked of `Package.Logo`, which
    /// resolves the resource index and costs tens of milliseconds a package.
    pub logo: Option<String>,
    #[serde(default)]
    pub applications: Vec<ManifestApp>,
}

/// Whether this package is an application a person would say they have.
///
/// Each rule has a fixture entry where it fires, and the fixture as a whole
/// has a test that the count does not drift.
pub fn is_application(p: &RawPackage) -> bool {
    if p.framework || p.resource || p.bundle {
        return false;
    }
    if !removable(p) {
        return false;
    }
    // Context menu handlers, codecs, runtimes, and the identity packages of
    // Win32 programs that Add/Remove Programs lists already.
    p.applications.iter().any(|a| a.listed)
}

/// Whether Windows lets this user remove the package at all.
pub fn removable(p: &RawPackage) -> bool {
    !p.framework && p.signature != Signature::System && !PART_OF_WINDOWS.contains(&p.name.as_str())
}

/// The Application User Model IDs of what the Start menu lists from this
/// package, `family!id`, which `shell:AppsFolder\` opens.
pub fn app_user_model_ids(p: &RawPackage) -> Vec<String> {
    p.applications
        .iter()
        .filter(|a| a.listed)
        .map(|a| format!("{}!{}", p.family_name, a.id))
        .collect()
}

/// What Brokey reads from an `AppxManifest.xml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    pub applications: Vec<ManifestApp>,
    /// `<Properties><Logo>`: `Assets\StoreLogo.png`.
    pub logo: Option<String>,
}

/// The applications an `AppxManifest.xml` declares.
pub fn manifest_applications(xml: &[u8]) -> Vec<ManifestApp> {
    parse_manifest(xml).applications
}

/// The applications an `AppxManifest.xml` declares, and its logo.
///
/// Elements are matched by local name, so no namespace is resolved:
/// `VisualElements` arrives as `uap:VisualElements` in one manifest and as
/// something else in another. An application is listed unless its
/// `VisualElements` says `AppListEntry="none"`, which is Windows' own
/// default. Malformed XML gives what was read before the fault.
pub fn parse_manifest(xml: &[u8]) -> Manifest {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let xml = xml.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(xml);
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut apps = Vec::new();
    let mut logo = None;

    let attribute = |e: &quick_xml::events::BytesStart, name: &str| -> Option<String> {
        e.attributes()
            .flatten()
            .find(|a| a.key.local_name().as_ref() == name)
            .map(|a| a.value.to_string())
    };

    loop {
        let (e, empty) = match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => (e.into_owned(), false),
            Ok(Event::Empty(e)) => (e.into_owned(), true),
            Ok(Event::End(_)) => {
                stack.pop();
                buf.clear();
                continue;
            }
            Ok(Event::Eof) | Err(_) => break,
            Ok(Event::Text(t)) => {
                let in_logo = stack.len() >= 2
                    && stack[stack.len() - 1] == b"Logo"
                    && stack[stack.len() - 2] == b"Properties";
                if in_logo && logo.is_none() {
                    let text = t.trim().to_string();
                    if !text.is_empty() {
                        logo = Some(text);
                    }
                }
                buf.clear();
                continue;
            }
            Ok(_) => {
                buf.clear();
                continue;
            }
        };
        let local = e.local_name().as_ref().as_bytes().to_vec();
        let parent = stack.last().map(|p| p.as_slice());
        if local == b"Application" && parent == Some(b"Applications".as_slice()) {
            apps.push(ManifestApp {
                id: attribute(&e, "Id").unwrap_or_default(),
                listed: true,
            });
        } else if local == b"VisualElements"
            && parent == Some(b"Application".as_slice())
            && let Some(app) = apps.last_mut()
            && attribute(&e, "AppListEntry").is_some_and(|v| v.eq_ignore_ascii_case("none"))
        {
            app.listed = false;
        }
        if !empty {
            stack.push(local);
        }
        buf.clear();
    }
    apps.retain(|a| !a.id.is_empty());
    Manifest {
        applications: apps,
        logo,
    }
}

/// Which file in a directory a manifest's logo is. A manifest names
/// `StoreLogo.png` and the package ships `StoreLogo.scale-100.png`,
/// `StoreLogo.scale-200.png` and so on, which Windows picks between by the
/// display's scale. Brokey draws the logo in a 96-pixel hero at most, so the
/// scale nearest 200 is the one wanted; a high-contrast variant never is.
/// The exact name wins when the package ships one. Pure: `names` is the
/// directory's listing.
pub fn logo_variant(logo: &str, names: &[String]) -> Option<String> {
    if let Some(exact) = names.iter().find(|n| n.eq_ignore_ascii_case(logo)) {
        return Some(exact.clone());
    }
    let (stem, ext) = logo.rsplit_once('.')?;
    let prefix = format!("{}.", stem.to_ascii_lowercase());
    let suffix = format!(".{}", ext.to_ascii_lowercase());
    let mut best: Option<(u32, &String)> = None;
    for name in names {
        let lower = name.to_ascii_lowercase();
        let Some(middle) = lower
            .strip_prefix(&prefix)
            .and_then(|r| r.strip_suffix(&suffix))
        else {
            continue;
        };
        if middle.contains("contrast") {
            continue;
        }
        let distance = middle
            .split('_')
            .find_map(|q| q.strip_prefix("scale-"))
            .and_then(|n| n.parse::<u32>().ok())
            .map(|n| n.abs_diff(200))
            // A target-size or other qualifier, after every scale.
            .unwrap_or(10_000);
        if best.is_none_or(|(d, n)| distance < d || (distance == d && name < n)) {
            best = Some((distance, name));
        }
    }
    best.map(|(_, n)| n.clone())
}

/// The file a package's manifest logo is on disk, through [`logo_variant`].
/// A manifest separates with a backslash on every platform.
pub fn resolve_logo(installed_path: &Path, logo: &str) -> Option<PathBuf> {
    let parts: Vec<&str> = logo.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
    let (file, dirs) = parts.split_last()?;
    let dir = dirs
        .iter()
        .fold(installed_path.to_path_buf(), |d, p| d.join(p));
    let names: Vec<String> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect();
    logo_variant(file, &names).map(|name| dir.join(name))
}

/// Whether `s` is shaped like a package family name: an identity name, an
/// underscore, and the 13-character publisher id. The Store's search puts
/// other things in the same field for a Win32 product (a path, or
/// `zoom.us.Zoom Video Meetings`), and those are not families.
pub fn is_family_name(s: &str) -> bool {
    let Some((name, publisher)) = s.rsplit_once('_') else {
        return false;
    };
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && publisher.len() == 13
        && publisher
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Whether `s` is a Store product id: twelve characters for an MSIX
/// product (`9PCFS5B6T72H`), fourteen for a Win32 one (`XP99J3KP4XZ4VV`),
/// upper-case letters and digits only. It is passed to winget as an
/// argument, so nothing else is let through.
pub fn is_product_id(s: &str) -> bool {
    matches!(s.len(), 12 | 14)
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Whether `s` can be a package full name. It goes inside a PowerShell
/// string literal, so only the characters a full name is made of pass:
/// letters, digits, `.`, `_`, `-` and the `~` of a bundle's resource id.
pub fn is_full_name(s: &str) -> bool {
    !s.is_empty()
        && s.contains('_')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~'))
}

/// The package family name a full name belongs to:
/// `Microsoft.WindowsTerminal_1.21.2361.0_x64__8wekyb3d8bbwe` is
/// `Microsoft.WindowsTerminal_8wekyb3d8bbwe`. A full name is five fields
/// joined by `_` (name, version, architecture, resource id, publisher id),
/// and an identity name cannot contain `_`, so the first and the last are
/// the family. `None` for anything not shaped like a full name.
pub fn family_from_full_name(full_name: &str) -> Option<String> {
    let fields: Vec<&str> = full_name.split('_').collect();
    let [name, _version, _arch, _resource, publisher] = fields.as_slice() else {
        return None;
    };
    let family = format!("{name}_{publisher}");
    is_family_name(&family).then_some(family)
}

// ── Installed ───────────────────────────────────────────────────────────────

/// What a package is called: the name Windows resolved, else its identity.
pub fn display_name(p: &RawPackage) -> String {
    p.display_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(&p.name)
        .to_string()
}

/// "Microsoft Store" for a Store-signed package, and the plain truth for
/// the rest.
pub fn provenance(signature: Signature) -> &'static str {
    match signature {
        Signature::Store => "Microsoft Store",
        Signature::Developer => "Outside the Store, signed by its publisher",
        Signature::Enterprise => "Outside the Store, signed by an organisation",
        Signature::None => "Outside the Store, unsigned",
        Signature::System => "Windows",
    }
}

/// An installed package as the page's `Package`. `icon` is what the
/// package's own logo became; see [`cached_logo`].
pub fn to_package(p: &RawPackage, icon: Option<Picture>) -> Package {
    let name = display_name(p);
    let mut pkg = Package::new(SourceKind::Msix, &p.family_name, &name);
    pkg.kind = PackageKind::App;
    pkg.installed = true;
    pkg.version = Some(p.version.clone());
    pkg.installed_version = Some(p.version.clone());
    pkg.developer = p.publisher.clone().filter(|s| !s.trim().is_empty());
    // A manifest's description is often just the name again.
    let description = p
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case(&name));
    pkg.summary = description.and_then(summary_from);
    pkg.icon = icon;
    pkg.facts = vec![
        ("Package family".to_string(), p.family_name.clone()),
        (
            "Installed from".to_string(),
            provenance(p.signature).to_string(),
        ),
    ];
    pkg
}

/// Copy a package's logo into Brokey's cache and name the copy. The asset
/// protocol does not admit `C:\Program Files\WindowsApps`, and must not,
/// for the reason the spec's "Why no icon appears today" gives, so the
/// logo is served from `$CACHE/icons` beside the icons `arp` extracts.
/// The name is a hash of the source path, which carries the package's full
/// name and therefore its version: an update writes a new entry.
pub fn cached_logo(cache: &Path, logo: &Path) -> Option<PathBuf> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    logo.to_string_lossy().to_lowercase().hash(&mut hasher);
    let ext = logo
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("png");
    let dir = cache.join("icons");
    let dest = dir.join(format!("msix-{:016x}.{ext}", hasher.finish()));
    if dest.is_file() {
        return Some(dest);
    }
    let size = std::fs::metadata(logo).ok()?.len();
    if size == 0 || size > MAX_LOGO_BYTES {
        return None;
    }
    let bytes = std::fs::read(logo).ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    let tmp = dir.join(format!(
        "msix-{:016x}.{}.tmp",
        hasher.finish(),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes).ok()?;
    if std::fs::rename(&tmp, &dest).is_err() {
        let _ = std::fs::remove_file(&tmp);
        if !dest.is_file() {
            return None;
        }
    }
    Some(dest)
}

/// A short line from a description: its first sentence or line, when that
/// is short enough to be a summary.
pub fn summary_from(description: &str) -> Option<String> {
    let first = description.lines().map(str::trim).find(|l| !l.is_empty())?;
    if first.chars().count() <= 160 {
        return Some(first.to_string());
    }
    let end = first.find(". ").map(|i| i + 1)?;
    let sentence = &first[..end];
    (sentence.chars().count() <= 160).then(|| sentence.to_string())
}

/// Plain Store text as the paragraphs the page draws: one `<p>` per line,
/// escaped, blank lines dropped.
pub fn paragraphs(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            let escaped = l
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            format!("<p>{escaped}</p>")
        })
        .collect()
}

// ── The Store catalogue ─────────────────────────────────────────────────────

/// One row of a Store search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub product_id: String,
    pub name: String,
    pub publisher: Option<String>,
    /// The package family, for an MSIX product. A Win32 product has none.
    pub family: Option<String>,
}

#[derive(Deserialize)]
struct SearchAnswer {
    #[serde(rename = "Data", default)]
    data: Vec<SearchRow>,
}

#[derive(Deserialize)]
struct SearchRow {
    #[serde(rename = "PackageIdentifier")]
    id: String,
    #[serde(rename = "PackageName")]
    name: String,
    #[serde(rename = "Publisher", default)]
    publisher: Option<String>,
    #[serde(rename = "Versions", default)]
    versions: Vec<SearchVersion>,
}

#[derive(Deserialize)]
struct SearchVersion {
    #[serde(rename = "PackageFamilyNames", default)]
    families: Vec<String>,
}

/// The body `manifestSearch` takes. Without the market filter the service
/// answers nothing for names it plainly carries: `spotify` returns no rows
/// without it and Spotify with it.
pub fn search_body(text: &str, market: &str) -> serde_json::Value {
    serde_json::json!({
        "Query": {"KeyWord": text, "MatchType": "Substring"},
        "Filters": [{
            "PackageMatchField": "Market",
            "RequestMatch": {"KeyWord": market, "MatchType": "CaseInsensitive"}
        }]
    })
}

/// A `manifestSearch` answer as rows. A row whose id is not a product id is
/// dropped, because it could never be installed.
pub fn parse_search(json: &str) -> Result<Vec<SearchHit>> {
    let answer: SearchAnswer = serde_json::from_str(json).map_err(|e| {
        Error::from_source(
            SourceKind::Msix,
            format!("The Store sent a search answer Brokey could not read: {e}."),
        )
    })?;
    Ok(hits(answer))
}

fn hits(answer: SearchAnswer) -> Vec<SearchHit> {
    answer
        .data
        .into_iter()
        .filter(|r| is_product_id(&r.id))
        .map(|r| SearchHit {
            family: r
                .versions
                .iter()
                .flat_map(|v| v.families.iter())
                .find(|f| is_family_name(f))
                .cloned(),
            product_id: r.id,
            name: r.name,
            publisher: r.publisher.filter(|p| !p.trim().is_empty()),
        })
        .collect()
}

/// What the Store says about one product, from either of its two services.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Listing {
    pub product_id: String,
    pub title: Option<String>,
    pub publisher: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub categories: Vec<String>,
    pub icon: Option<String>,
    pub screenshots: Vec<Screenshot>,
    /// Average and count, over all time.
    pub rating: Option<(f64, u64)>,
    pub family: Option<String>,
}

/// One image as either service describes it.
struct Image<'a> {
    purpose: &'a str,
    url: &'a str,
    width: Option<u32>,
    height: Option<u32>,
    caption: Option<&'a str>,
}

/// A Store image URL the page can fetch. The display catalogue writes them
/// scheme-relative, `//store-images.s-microsoft.com/...`.
fn absolute(url: &str) -> Option<String> {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("//") {
        Some(format!("https://{rest}"))
    } else if url.starts_with("https://") {
        Some(url.to_string())
    } else {
        None
    }
}

/// The square logo to draw: the smallest at least 150 pixels across, so it
/// is sharp in the 96-pixel hero at a scale of 1.5, else the largest there
/// is. Logos and tiles are both square renditions of the same artwork.
fn choose_icon(images: &[Image]) -> Option<String> {
    let mut square: Vec<(&Image, u32)> = images
        .iter()
        .filter(|i| {
            i.purpose.eq_ignore_ascii_case("logo") || i.purpose.eq_ignore_ascii_case("tile")
        })
        .filter_map(|i| match (i.width, i.height) {
            (Some(w), Some(h)) if w == h && w > 0 => Some((i, w)),
            _ => None,
        })
        .collect();
    square.sort_by_key(|(_, w)| *w);
    square
        .iter()
        .find(|(_, w)| *w >= 150)
        .or(square.last())
        .and_then(|(i, _)| absolute(i.url))
}

fn screenshots(images: &[Image]) -> Vec<Screenshot> {
    images
        .iter()
        .filter(|i| i.purpose.eq_ignore_ascii_case("screenshot"))
        .filter_map(|i| {
            Some(Screenshot {
                image: Picture::Url(absolute(i.url)?),
                thumbnail: None,
                caption: i
                    .caption
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .map(str::to_string),
                width: i.width,
                height: i.height,
            })
        })
        .collect()
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[derive(Deserialize)]
struct EdgeAnswer {
    #[serde(rename = "Payload")]
    payload: EdgeProduct,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct EdgeProduct {
    product_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    publisher_name: Option<String>,
    #[serde(default)]
    short_description: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    app_website_url: Option<String>,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    images: Vec<EdgeImage>,
    #[serde(default)]
    average_rating: Option<f64>,
    #[serde(default)]
    rating_count: Option<u64>,
    #[serde(default)]
    package_family_names: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct EdgeImage {
    image_type: String,
    url: String,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    caption: Option<String>,
}

/// One product's page from `storeedgefd`'s `products/<id>`.
pub fn parse_product(json: &str) -> Result<Listing> {
    let answer: EdgeAnswer = serde_json::from_str(json).map_err(|e| {
        Error::from_source(
            SourceKind::Msix,
            format!("The Store sent a product page Brokey could not read: {e}."),
        )
    })?;
    let p = answer.payload;
    let images: Vec<Image> = p
        .images
        .iter()
        .map(|i| Image {
            purpose: &i.image_type,
            url: &i.url,
            width: i.width,
            height: i.height,
            caption: i.caption.as_deref(),
        })
        .collect();
    let description = nonempty(p.description);
    Ok(Listing {
        icon: choose_icon(&images),
        screenshots: screenshots(&images),
        product_id: p.product_id,
        title: nonempty(p.title),
        publisher: nonempty(p.publisher_name),
        summary: nonempty(p.short_description)
            .and_then(|s| summary_from(&s))
            .or_else(|| description.as_deref().and_then(summary_from)),
        description,
        homepage: nonempty(p.app_website_url).filter(|u| u.starts_with("http")),
        categories: p.categories,
        // A product nobody has rated says 0.0 from 0 ratings.
        rating: match (p.average_rating, p.rating_count) {
            (Some(avg), Some(n)) if n > 0 => Some((avg, n)),
            _ => None,
        },
        family: p
            .package_family_names
            .into_iter()
            .find(|f| is_family_name(f)),
    })
}

#[derive(Deserialize)]
struct CatalogueAnswer {
    #[serde(rename = "Products", default)]
    products: Vec<CatalogueProduct>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CatalogueProduct {
    product_id: String,
    #[serde(default)]
    localized_properties: Vec<CatalogueLocalised>,
    #[serde(default)]
    properties: Option<CatalogueProperties>,
    #[serde(default)]
    market_properties: Vec<CatalogueMarket>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CatalogueLocalised {
    #[serde(default)]
    product_title: Option<String>,
    #[serde(default)]
    publisher_name: Option<String>,
    #[serde(default)]
    short_description: Option<String>,
    #[serde(default)]
    product_description: Option<String>,
    #[serde(default)]
    publisher_website_uri: Option<String>,
    #[serde(default)]
    images: Vec<CatalogueImage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CatalogueImage {
    image_purpose: String,
    uri: String,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    caption: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CatalogueProperties {
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    package_family_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CatalogueMarket {
    #[serde(default)]
    usage_data: Vec<CatalogueUsage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CatalogueUsage {
    aggregate_time_span: String,
    #[serde(default)]
    average_rating: Option<f64>,
    #[serde(default)]
    rating_count: Option<u64>,
}

/// A display catalogue answer, from `products?bigIds=` or
/// `products/lookup`, as listings. The `browse` template carries only the
/// title and the images, and the rest stays empty.
pub fn parse_catalogue(json: &str) -> Result<Vec<Listing>> {
    let answer: CatalogueAnswer = serde_json::from_str(json).map_err(|e| {
        Error::from_source(
            SourceKind::Msix,
            format!("The Store catalogue sent an answer Brokey could not read: {e}."),
        )
    })?;
    Ok(answer
        .products
        .into_iter()
        .map(|p| {
            let local = p.localized_properties.into_iter().next();
            let (images, title, publisher, short, description, homepage) = match local {
                Some(l) => (
                    l.images,
                    l.product_title,
                    l.publisher_name,
                    l.short_description,
                    l.product_description,
                    l.publisher_website_uri,
                ),
                None => (Vec::new(), None, None, None, None, None),
            };
            let pictures: Vec<Image> = images
                .iter()
                .map(|i| Image {
                    purpose: &i.image_purpose,
                    url: &i.uri,
                    width: i.width,
                    height: i.height,
                    caption: i.caption.as_deref(),
                })
                .collect();
            let description = nonempty(description);
            let rating = p
                .market_properties
                .iter()
                .flat_map(|m| m.usage_data.iter())
                .find(|u| u.aggregate_time_span == "AllTime")
                .and_then(|u| match (u.average_rating, u.rating_count) {
                    (Some(avg), Some(n)) if n > 0 => Some((avg, n)),
                    _ => None,
                });
            let (category, family) = match p.properties {
                Some(pr) => (pr.category, pr.package_family_name),
                None => (None, None),
            };
            Listing {
                icon: choose_icon(&pictures),
                screenshots: screenshots(&pictures),
                product_id: p.product_id,
                title: nonempty(title),
                publisher: nonempty(publisher),
                summary: nonempty(short)
                    .and_then(|s| summary_from(&s))
                    .or_else(|| description.as_deref().and_then(summary_from)),
                description,
                homepage: nonempty(homepage).filter(|u| u.starts_with("http")),
                categories: nonempty(category).into_iter().collect(),
                rating,
                family: family.filter(|f| is_family_name(f)),
            }
        })
        .collect())
}

/// "4.1 from 81 556 ratings", with the thin grouping the rest of the page
/// uses for counts.
pub fn rating_label(avg: f64, count: u64) -> String {
    let digits = count.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(' ');
        }
        grouped.push(c);
    }
    let noun = if count == 1 { "rating" } else { "ratings" };
    format!("{avg:.1} from {grouped} {noun}")
}

/// A search hit as the page's `Package`, joined to what this machine has.
/// Its id is its package family name; only a Win32 product, which has none,
/// is named by its Store product id.
pub fn hit_to_package(
    hit: &SearchHit,
    installed: Option<&RawPackage>,
    icon: Option<&String>,
) -> Package {
    let id = installed
        .map(|p| p.family_name.clone())
        .or_else(|| hit.family.clone())
        .unwrap_or_else(|| hit.product_id.clone());
    let mut pkg = Package::new(SourceKind::Msix, id, hit.name.trim());
    pkg.kind = PackageKind::App;
    pkg.developer = hit.publisher.clone();
    pkg.icon = icon.cloned().map(Picture::Url);
    if let Some(p) = installed {
        pkg.installed = true;
        pkg.installed_version = Some(p.version.clone());
    }
    pkg.facts = vec![("Store ID".to_string(), hit.product_id.clone())];
    if let Some(family) = &hit.family {
        pkg.facts
            .push(("Package family".to_string(), family.clone()));
    }
    pkg
}

/// Fill a package in from its Store listing. What the package already
/// knows exactly (an installed logo, its own version) is kept.
pub fn enrich(pkg: &mut Package, listing: &Listing) {
    if let Some(title) = &listing.title
        && !pkg.installed
    {
        pkg.name = title.clone();
    }
    if pkg.developer.is_none() {
        pkg.developer = listing.publisher.clone();
    }
    if let Some(summary) = &listing.summary {
        pkg.summary = Some(summary.clone());
    }
    if let Some(description) = &listing.description {
        pkg.description = Some(paragraphs(description));
    }
    if pkg.icon.is_none() {
        pkg.icon = listing.icon.clone().map(Picture::Url);
    }
    if pkg.screenshots.is_empty() {
        pkg.screenshots = listing.screenshots.clone();
    }
    if pkg.homepage.is_none() {
        pkg.homepage = listing.homepage.clone();
    }
    if pkg.categories.is_empty() {
        pkg.categories = listing.categories.clone();
    }
    if let Some((avg, n)) = listing.rating {
        pkg.popularity_label = Some(rating_label(avg, n));
    }
    if !pkg.facts.iter().any(|(k, _)| k == "Store ID") {
        pkg.facts
            .insert(0, ("Store ID".to_string(), listing.product_id.clone()));
    }
}

// ── Plans ───────────────────────────────────────────────────────────────────

/// Installing a Store product through winget's `msstore` source. The flags
/// are winget's own source's, for the reasons `winget::operation_step`
/// gives, with `--source msstore` so a product id can never resolve to a
/// winget package that happens to share it. Never elevated: a Store
/// application is installed for the user who asked, and an elevated winget
/// would install it for whoever answered the prompt.
pub fn install_step(product_id: &str, name: &str, program: &str) -> Step {
    Step {
        source: SourceKind::Msix,
        title: format!("Installing {name} from the Store"),
        command: Command {
            program: program.to_string(),
            args: [
                "install",
                "--exact",
                "--id",
                product_id,
                "--source",
                "msstore",
                "--silent",
                "--disable-interactivity",
                "--accept-source-agreements",
                "--accept-package-agreements",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            env: Vec::new(),
            cwd: None,
        },
        needs_root: false,
        weight: 10,
    }
}

/// Removing a package for this user through the Appx API. The full name
/// was checked by [`is_full_name`] and sits in a single-quoted PowerShell
/// literal, where none of its characters means anything.
pub fn removal_step(full_name: &str, name: &str) -> Step {
    Step {
        source: SourceKind::Msix,
        title: format!("Removing {name}"),
        command: Command {
            program: "powershell.exe".to_string(),
            args: vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                format!("Remove-AppxPackage -Package '{full_name}'"),
            ],
            env: Vec::new(),
            cwd: None,
        },
        needs_root: false,
        weight: 4,
    }
}

const SETUP_NOTICE: &str = "Installing from the Store goes through winget, which is not installed. winget is set up first.";

// ── The source ──────────────────────────────────────────────────────────────

/// The Store and MSIX source. The Appx state is read once, the first time
/// anything asks, and the store is detected again after every plan.
pub struct Msix {
    client: Arc<crate::http::Client>,
    cache: PathBuf,
    packages: OnceLock<std::result::Result<Vec<RawPackage>, String>>,
}

impl Msix {
    pub fn new(client: Arc<crate::http::Client>) -> Msix {
        Msix {
            client,
            cache: crate::system::Dirs::new().cache,
            packages: OnceLock::new(),
        }
    }

    /// Built from packages already in hand, with its own cache directory.
    #[cfg(test)]
    fn with_packages(packages: Vec<RawPackage>, cache: PathBuf) -> Msix {
        let lock = OnceLock::new();
        let _ = lock.set(Ok(packages));
        Msix {
            client: Arc::new(crate::http::Client::new(cache.join("http"))),
            cache,
            packages: lock,
        }
    }

    fn packages(&self) -> std::result::Result<&[RawPackage], &str> {
        match self.packages.get_or_init(read) {
            Ok(p) => Ok(p),
            Err(e) => Err(e),
        }
    }

    fn applications(&self) -> Vec<&RawPackage> {
        self.packages()
            .map(|all| all.iter().filter(|p| is_application(p)).collect())
            .unwrap_or_default()
    }

    /// The registered package of a family, if this user has one.
    fn by_family(&self, family: &str) -> Option<&RawPackage> {
        self.packages()
            .ok()?
            .iter()
            .find(|p| p.family_name.eq_ignore_ascii_case(family) && !p.resource)
    }

    /// The Application User Model IDs an installed package's Start menu
    /// entries open, `family!id`. Empty when the family is not installed or
    /// lists nothing. For the launcher: `shell:AppsFolder\<AUMID>`.
    pub fn app_user_model_ids(&self, family: &str) -> Vec<String> {
        self.by_family(family)
            .map(app_user_model_ids)
            .unwrap_or_default()
    }

    fn package(&self, p: &RawPackage) -> Package {
        let icon = match (&p.installed_path, &p.logo) {
            (Some(dir), Some(logo)) => resolve_logo(Path::new(dir), logo)
                .and_then(|file| cached_logo(&self.cache, &file))
                .map(Picture::File),
            _ => None,
        };
        to_package(p, icon)
    }

    /// Icons for a page of search hits, keyed by product id: one display
    /// catalogue request per [`ICONS_PER_REQUEST`]. The display catalogue
    /// carries MSIX products only, so a Win32 product's icon waits for its
    /// detail page. A failure costs the icons and nothing else.
    fn icons(&self, hits: &[SearchHit], market: &str, language: &str) -> HashMap<String, String> {
        let ids: Vec<&str> = hits
            .iter()
            .map(|h| h.product_id.as_str())
            .filter(|id| id.len() == 12)
            .collect();
        let mut out = HashMap::new();
        for chunk in ids.chunks(ICONS_PER_REQUEST) {
            let url = format!(
                "{CATALOGUE_URL}?bigIds={}&market={market}&languages={language},neutral&fieldsTemplate=browse",
                chunk.join(",")
            );
            match self
                .client
                .get_text_cached(&url, CATALOGUE_MAX_AGE)
                .and_then(|text| parse_catalogue(&text))
            {
                Ok(listings) => {
                    for l in listings {
                        if let Some(icon) = l.icon {
                            out.insert(l.product_id, icon);
                        }
                    }
                }
                Err(e) => log::warn!("Store icons: {e}"),
            }
        }
        out
    }

    fn listing_by_product(&self, id: &str) -> Result<Listing> {
        let (market, language) = locale();
        let url = format!(
            "{PRODUCT_URL}{id}?market={market}&locale={language}&deviceFamily=Windows.Desktop"
        );
        let text = self.client.get_text_cached(&url, CATALOGUE_MAX_AGE)?;
        parse_product(&text)
    }

    fn listing_by_family(&self, family: &str) -> Result<Option<Listing>> {
        let (market, language) = locale();
        let url = format!(
            "{CATALOGUE_URL}/lookup?alternateId=PackageFamilyName&value={family}&market={market}&languages={language},neutral&fieldsTemplate=details"
        );
        let text = self.client.get_text_cached(&url, CATALOGUE_MAX_AGE)?;
        Ok(parse_catalogue(&text)?.into_iter().next())
    }
}

impl Source for Msix {
    fn kind(&self) -> SourceKind {
        SourceKind::Msix
    }

    fn status(&self) -> SourceStatus {
        match self.packages() {
            Ok(_) => {
                let n = self.applications().len();
                SourceStatus {
                    kind: SourceKind::Msix,
                    available: true,
                    reason: None,
                    detail: Some(if n == 1 {
                        "1 application".to_string()
                    } else {
                        format!("{n} applications")
                    }),
                    searchable: true,
                    setup: None,
                }
            }
            Err(reason) => SourceStatus {
                kind: SourceKind::Msix,
                available: false,
                reason: Some(reason.to_string()),
                detail: None,
                // The Store's search answers without the Appx state.
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
        let (market, language) = locale();
        let answer: SearchAnswer = self
            .client
            .post_json(SEARCH_URL, &search_body(text, &market))
            .map_err(|e| {
                Error::from_source(
                    SourceKind::Msix,
                    format!("The Store's search could not be reached: {e}."),
                )
            })?;
        let mut found = hits(answer);
        found.truncate(query.limit.max(1));
        let icons = self.icons(&found, &market, &language);
        Ok(found
            .iter()
            .map(|hit| {
                let installed = hit.family.as_deref().and_then(|f| self.by_family(f));
                hit_to_package(hit, installed, icons.get(&hit.product_id))
            })
            .collect())
    }

    fn installed(&self) -> Result<Vec<Package>> {
        let all = self
            .packages()
            .map_err(|e| Error::from_source(SourceKind::Msix, e.to_string()))?;
        Ok(all
            .iter()
            .filter(|p| is_application(p))
            .map(|p| self.package(p))
            .collect())
    }

    /// The Store updates what it installed itself, on its own schedule, and
    /// winget's `msstore` source cannot say which of them it would update
    /// without asking the Store about each one. Until that is measured,
    /// nothing is listed rather than a list that might be wrong.
    fn updates(&self) -> Result<Vec<Update>> {
        Ok(Vec::new())
    }

    fn details(&self, id: &str) -> Result<Package> {
        if is_family_name(id) {
            let local = self.by_family(id).map(|p| self.package(p));
            let listing = match self.listing_by_family(id) {
                Ok(listing) => listing,
                Err(e) if local.is_some() => {
                    log::warn!("Store listing for {id}: {e}");
                    None
                }
                Err(e) => return Err(e),
            };
            let mut pkg = match (local, &listing) {
                (Some(pkg), _) => pkg,
                (None, Some(l)) => {
                    let mut pkg = Package::new(
                        SourceKind::Msix,
                        id,
                        l.title.clone().unwrap_or_else(|| id.to_string()),
                    );
                    pkg.kind = PackageKind::App;
                    pkg
                }
                (None, None) => {
                    return Err(Error::from_source(
                        SourceKind::Msix,
                        format!("{id} is not installed and the Store does not list it."),
                    ));
                }
            };
            if let Some(l) = &listing {
                enrich(&mut pkg, l);
            }
            return Ok(pkg);
        }
        if is_product_id(id) {
            let listing = self.listing_by_product(id)?;
            let installed = listing.family.as_deref().and_then(|f| self.by_family(f));
            let hit = SearchHit {
                product_id: listing.product_id.clone(),
                name: listing.title.clone().unwrap_or_else(|| id.to_string()),
                publisher: listing.publisher.clone(),
                family: listing.family.clone(),
            };
            let mut pkg = hit_to_package(&hit, None, None);
            if let Some(p) = installed {
                pkg.installed = true;
                pkg.installed_version = Some(p.version.clone());
            }
            enrich(&mut pkg, &listing);
            return Ok(pkg);
        }
        Err(Error::from_source(
            SourceKind::Msix,
            format!("{id} is neither a Store product id nor a package family name."),
        ))
    }

    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        self.plan_with(op, &super::winget::winget_program(), &|family| {
            Ok(self.listing_by_family(family)?.map(|l| l.product_id))
        })
    }

    /// Nothing to set up while winget is here. Without it, winget is set up
    /// first and the Store needs nothing of its own afterwards.
    fn setup(&self) -> Option<Setup> {
        #[cfg(windows)]
        if super::winget::winget_exe().is_some() {
            return None;
        }
        Some(Setup {
            ops: vec![Op::Setup {
                source: SourceKind::Winget,
            }],
            steps: Vec::new(),
            notice: SETUP_NOTICE.to_string(),
        })
    }

    /// The package's first Start menu entry, as `shell:AppsFolder\<AUMID>`.
    /// A package that lists none (a context menu handler, a codec) is not
    /// an application and has nothing to open.
    fn launcher(&self, id: &str) -> Option<crate::launch::Launch> {
        let aumid = self.app_user_model_ids(id).into_iter().next()?;
        crate::launch::apps_folder(&aumid)
    }
}

impl Msix {
    /// [`Source::plan`] with the path to `winget.exe` and the family to
    /// product id lookup as parameters, so a test drives it without winget
    /// and without the network. The lookup is a read-only question to the
    /// display catalogue, the shape `winget::details` already has.
    fn plan_with(
        &self,
        op: &Op,
        winget: &str,
        product_of: &dyn Fn(&str) -> Result<Option<String>>,
    ) -> Result<Vec<Step>> {
        match op {
            Op::Install { package } if package.source == SourceKind::Msix => {
                let id = package.id.as_str();
                if is_product_id(id) {
                    return Ok(vec![install_step(id, id, winget)]);
                }
                if !is_family_name(id) {
                    return Err(Error::from_source(
                        SourceKind::Msix,
                        format!(
                            "{id} is neither a package family name nor a Store product id, so it cannot be installed from the Store."
                        ),
                    ));
                }
                if self.by_family(id).is_some() {
                    // Already here: nothing to do.
                    return Ok(Vec::new());
                }
                let name = id.split('_').next().unwrap_or(id);
                match product_of(id)? {
                    Some(product) if is_product_id(&product) => {
                        Ok(vec![install_step(&product, name, winget)])
                    }
                    _ => Err(Error::from_source(
                        SourceKind::Msix,
                        format!(
                            "The Store does not list {id}, so it cannot be installed from there."
                        ),
                    )),
                }
            }
            Op::Remove { package } if package.source == SourceKind::Msix => {
                let id = package.id.as_str();
                let Some(p) = is_family_name(id).then(|| self.by_family(id)).flatten() else {
                    return Err(Error::from_source(
                        SourceKind::Msix,
                        format!(
                            "{id} is not installed for this user, so there is nothing to remove."
                        ),
                    ));
                };
                if !removable(p) {
                    return Err(Error::from_source(
                        SourceKind::Msix,
                        format!(
                            "{} is part of Windows and cannot be removed.",
                            display_name(p)
                        ),
                    ));
                }
                if !is_full_name(&p.full_name) {
                    return Err(Error::from_source(
                        SourceKind::Msix,
                        format!(
                            "{} has a package name Brokey will not pass to PowerShell: {}.",
                            display_name(p),
                            p.full_name
                        ),
                    ));
                }
                Ok(vec![removal_step(&p.full_name, &display_name(p))])
            }
            // No updates are listed (see `updates`), refresh has no index of
            // its own to bring up to date, and setup is the planner's.
            _ => Ok(Vec::new()),
        }
    }
}

/// The Store market and language: the user's region and display locale
/// from `HKCU\Control Panel\International`, which is what the Store app
/// itself goes by. `US` and `en-US` when either is missing or malformed.
pub fn locale() -> (String, String) {
    #[cfg(windows)]
    {
        let key = windows_registry::CURRENT_USER.open(r"Control Panel\International");
        let language = key
            .as_ref()
            .ok()
            .and_then(|k| k.get_string("LocaleName").ok());
        let market = key
            .ok()
            .and_then(|k| k.open("Geo").ok())
            .and_then(|k| k.get_string("Name").ok());
        normalise_locale(market.as_deref(), language.as_deref())
    }
    #[cfg(not(windows))]
    normalise_locale(None, None)
}

/// [`locale`]'s checks, pure: a market is two letters and a language tag is
/// letters and hyphens, because both go into a URL unescaped.
pub fn normalise_locale(market: Option<&str>, language: Option<&str>) -> (String, String) {
    let market = market
        .map(str::trim)
        .filter(|m| m.len() == 2 && m.bytes().all(|b| b.is_ascii_alphabetic()))
        .map(str::to_ascii_uppercase)
        .unwrap_or_else(|| "US".to_string());
    let language = language
        .map(str::trim)
        .filter(|l| {
            (2..=20).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        .map(str::to_string)
        .unwrap_or_else(|| "en-US".to_string());
    (market, language)
}

/// The Appx state for the signed-in user. The only impure function in the
/// module, and the only one that touches the Windows Runtime.
#[cfg(windows)]
pub fn read() -> std::result::Result<Vec<RawPackage>, String> {
    use windows::ApplicationModel::PackageSignatureKind;
    use windows::Management::Deployment::PackageManager;
    use windows::core::HSTRING;

    let failed = |e: windows::core::Error| {
        format!(
            "The list of installed Store applications could not be read: {}.",
            e.message().trim_end_matches(['.', '\r', '\n'])
        )
    };
    let manager = PackageManager::new().map_err(failed)?;
    // An empty security id is the calling user, which needs no elevation.
    let found = manager
        .FindPackagesByUserSecurityId(&HSTRING::new())
        .map_err(failed)?;
    let text = |h: windows::core::Result<HSTRING>| -> Option<String> {
        h.ok()
            .map(|s| s.to_string())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && !s.starts_with("ms-resource:") && !s.starts_with("@{"))
    };
    let mut out = Vec::new();
    for p in found {
        let Ok(id) = p.Id() else { continue };
        let (Some(full_name), Some(family_name), Some(name)) =
            (text(id.FullName()), text(id.FamilyName()), text(id.Name()))
        else {
            continue;
        };
        let version = id
            .Version()
            .map(|v| format!("{}.{}.{}.{}", v.Major, v.Minor, v.Build, v.Revision))
            .unwrap_or_default();
        let installed_path = text(p.InstalledPath());
        let manifest = installed_path
            .as_deref()
            .and_then(|dir| std::fs::read(Path::new(dir).join("AppxManifest.xml")).ok())
            .map(|xml| parse_manifest(&xml))
            .unwrap_or_default();
        let signature = match p.SignatureKind() {
            Ok(PackageSignatureKind::Store) => Signature::Store,
            Ok(PackageSignatureKind::System) => Signature::System,
            Ok(PackageSignatureKind::Enterprise) => Signature::Enterprise,
            Ok(PackageSignatureKind::Developer) => Signature::Developer,
            _ => Signature::None,
        };
        let mut raw = RawPackage {
            full_name,
            family_name,
            name,
            version,
            display_name: None,
            publisher: None,
            description: None,
            framework: p.IsFramework().unwrap_or(false),
            resource: p.IsResourcePackage().unwrap_or(false),
            bundle: p.IsBundle().unwrap_or(false),
            stub: p.IsStub().unwrap_or(false),
            signature,
            installed_path,
            logo: manifest.logo,
            applications: manifest.applications,
        };
        // Resolving a display name loads the package's resource index, and
        // the first time costs tens of milliseconds a package: eleven
        // seconds for the reference machine's 176. Only what can be shown
        // is worth it, which is the applications.
        if is_application(&raw) {
            raw.display_name = text(p.DisplayName());
            raw.publisher = text(p.PublisherDisplayName());
            raw.description = text(p.Description());
        }
        out.push(raw);
    }
    Ok(out)
}

/// There is no Appx state off Windows, and `sources::all` never builds this
/// source there.
#[cfg(not(windows))]
pub fn read() -> std::result::Result<Vec<RawPackage>, String> {
    Err("The Microsoft Store is only on Windows.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PackageRef;

    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/windows/msix")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is missing: {e}", path.display()))
    }

    fn packages() -> Vec<RawPackage> {
        serde_json::from_str(&fixture("packages.json")).expect("the fixture parses")
    }

    fn by_name(name: &str) -> RawPackage {
        packages()
            .into_iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("{name} is not in the fixture"))
    }

    fn source() -> (Msix, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a temporary directory");
        (
            Msix::with_packages(packages(), dir.path().to_path_buf()),
            dir,
        )
    }

    fn op_ref(id: &str) -> PackageRef {
        PackageRef {
            source: SourceKind::Msix,
            id: id.to_string(),
        }
    }

    // ── The manifest ────────────────────────────────────────────────────

    #[test]
    fn paint_lists_its_one_application() {
        let apps = manifest_applications(fixture("manifest-paint.xml").as_bytes());
        assert_eq!(
            apps,
            [ManifestApp {
                id: "App".to_string(),
                listed: true
            }]
        );
    }

    #[test]
    fn a_context_menu_handler_lists_nothing() {
        let apps = manifest_applications(fixture("manifest-context-menu.xml").as_bytes());
        assert_eq!(apps.len(), 1);
        assert!(!apps[0].listed, "AppListEntry=\"none\" is not listed");
    }

    #[test]
    fn every_application_is_read_and_each_is_judged_alone() {
        let apps = manifest_applications(fixture("manifest-sandbox.xml").as_bytes());
        let ids: Vec<&str> = apps.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["App", "AppCli", "AppService"]);
        assert!(apps.iter().all(|a| !a.listed));
    }

    #[test]
    fn an_application_without_app_list_entry_is_listed_and_one_outside_applications_is_not_read() {
        let xml = br#"<Package xmlns="x" xmlns:uap="y">
            <Extensions><Application Id="NotOne"/></Extensions>
            <Applications>
              <Application Id="A"><uap:VisualElements DisplayName="A"/></Application>
              <Application Id="B"><uap:VisualElements AppListEntry="None"/></Application>
              <Application Id="C"/>
            </Applications></Package>"#;
        let apps = manifest_applications(xml);
        assert_eq!(
            apps,
            [
                ManifestApp {
                    id: "A".into(),
                    listed: true
                },
                ManifestApp {
                    id: "B".into(),
                    listed: false
                },
                ManifestApp {
                    id: "C".into(),
                    listed: true
                },
            ]
        );
    }

    #[test]
    fn malformed_xml_gives_what_was_read_before_the_fault() {
        let xml = br#"<Package><Applications><Application Id="A"></Applications>"#;
        let apps = manifest_applications(xml);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].id, "A");
        assert!(manifest_applications(b"not xml at all").is_empty());
    }

    // ── The filter ──────────────────────────────────────────────────────

    #[test]
    fn a_store_application_with_a_listed_entry_is_an_application() {
        assert!(is_application(&by_name("Microsoft.Paint")));
    }

    #[test]
    fn an_msix_from_outside_the_store_is_an_application_too() {
        let p = by_name("com.tinyspeck.slackdesktop");
        assert_eq!(p.signature, Signature::Developer);
        assert!(is_application(&p));
    }

    #[test]
    fn a_framework_is_not_an_application() {
        let p = by_name("Microsoft.VCLibs.140.00.UWPDesktop");
        assert!(p.framework);
        assert!(!is_application(&p));
    }

    #[test]
    fn a_resource_package_is_not_an_application() {
        // `FindPackagesByUserSecurityId` does not list resource packages by
        // default, so the reference machine's export has none; the rule is
        // still there for a caller that asks for them.
        let mut p = by_name("Microsoft.Paint");
        p.resource = true;
        assert!(!is_application(&p));
        p.resource = false;
        p.bundle = true;
        assert!(!is_application(&p));
    }

    #[test]
    fn a_stub_is_still_an_application() {
        // Sticky Notes, Clock, Maps and To Do are stubs on the reference
        // machine and every one of them is in the Start menu.
        let p = by_name("Microsoft.MicrosoftStickyNotes");
        assert!(p.stub);
        assert!(is_application(&p));
    }

    #[test]
    fn what_windows_signed_is_not_an_application() {
        let p = by_name("windows.immersivecontrolpanel");
        assert_eq!(p.signature, Signature::System);
        assert!(p.applications.iter().any(|a| a.listed));
        assert!(!is_application(&p), "listed, but part of Windows");
    }

    #[test]
    fn the_two_store_signed_parts_of_windows_are_not_applications() {
        for name in PART_OF_WINDOWS {
            let p = by_name(name);
            assert_eq!(p.signature, Signature::Store, "{name}");
            assert!(!is_application(&p), "{name}");
            assert!(!removable(&p), "{name}");
        }
    }

    #[test]
    fn a_package_whose_every_entry_is_hidden_is_not_an_application() {
        for name in [
            "Microsoft.PowerToys.FileLocksmithContextMenu",
            "Microsoft.HEVCVideoExtension",
            "Microsoft.VisualStudioCodeInsiders",
        ] {
            let p = by_name(name);
            assert!(removable(&p), "{name} could be removed");
            assert!(!p.applications.is_empty(), "{name} declares applications");
            assert!(!is_application(&p), "{name} lists none of them");
        }
    }

    #[test]
    fn a_language_pack_is_not_an_application() {
        let p = by_name("Microsoft.LanguageExperiencePacken-GB");
        assert!(p.applications.iter().all(|a| !a.listed));
        assert!(!is_application(&p));
    }

    /// The fixture's count, pinned so a rule that drifts shows up here.
    #[test]
    fn the_fixture_has_the_applications_it_has() {
        let all = packages();
        let names: Vec<String> = all
            .iter()
            .filter(|p| is_application(p))
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(
            names,
            [
                "Microsoft.Paint",
                "com.tinyspeck.slackdesktop",
                "Microsoft.WindowsCalculator",
                "CanonicalGroupLimited.Ubuntu",
                "Microsoft.MicrosoftStickyNotes",
                "Microsoft.WindowsTerminalPreview",
            ],
            "of {} packages",
            all.len()
        );
    }

    // ── Installed packages ──────────────────────────────────────────────

    #[test]
    fn an_installed_package_is_named_by_its_family_and_says_where_it_came_from() {
        let p = to_package(&by_name("Microsoft.Paint"), None);
        assert_eq!(p.id, "Microsoft.Paint_8wekyb3d8bbwe");
        assert_eq!(p.name, "Paint");
        assert_eq!(p.kind, PackageKind::App);
        assert!(p.installed);
        assert_eq!(p.installed_version.as_deref(), Some("11.2609.441.0"));
        assert_eq!(p.developer.as_deref(), Some("Microsoft Corporation"));
        assert!(
            p.facts
                .contains(&("Installed from".to_string(), "Microsoft Store".to_string()))
        );
        let slack = to_package(&by_name("com.tinyspeck.slackdesktop"), None);
        assert!(slack.facts.contains(&(
            "Installed from".to_string(),
            "Outside the Store, signed by its publisher".to_string()
        )));
    }

    #[test]
    fn a_description_that_repeats_the_name_is_not_a_summary() {
        let mut raw = by_name("Microsoft.Paint");
        raw.description = Some("Paint".to_string());
        assert_eq!(to_package(&raw, None).summary, None);
        raw.description = Some("Draw and edit pictures.".to_string());
        assert_eq!(
            to_package(&raw, None).summary.as_deref(),
            Some("Draw and edit pictures.")
        );
    }

    #[test]
    fn an_unresolved_display_name_falls_back_to_the_identity() {
        let mut raw = by_name("Microsoft.Paint");
        raw.display_name = None;
        assert_eq!(display_name(&raw), "Microsoft.Paint");
    }

    #[test]
    fn the_launcher_gets_the_listed_entries_as_app_user_model_ids() {
        assert_eq!(
            app_user_model_ids(&by_name("Microsoft.Paint")),
            ["Microsoft.Paint_8wekyb3d8bbwe!App"]
        );
        assert!(
            app_user_model_ids(&by_name("Microsoft.PowerToys.FileLocksmithContextMenu")).is_empty()
        );
        let (msix, _dir) = source();
        assert_eq!(
            msix.app_user_model_ids("Microsoft.Paint_8wekyb3d8bbwe"),
            ["Microsoft.Paint_8wekyb3d8bbwe!App"]
        );
        assert!(
            msix.app_user_model_ids("Nobody.Here_0000000000000")
                .is_empty()
        );
    }

    #[test]
    fn opening_an_installed_package_is_its_first_start_menu_entry() {
        let (msix, _dir) = source();
        let launch = msix.launcher("Microsoft.Paint_8wekyb3d8bbwe").unwrap();
        assert_eq!(
            launch.windows_argument().as_deref(),
            Some(r"shell:AppsFolder\Microsoft.Paint_8wekyb3d8bbwe!App")
        );
        // Listed nothing, and not installed: no button either way.
        let hidden = by_name("Microsoft.PowerToys.FileLocksmithContextMenu").family_name;
        assert_eq!(msix.launcher(&hidden), None);
        assert_eq!(msix.launcher("Nobody.Here_0000000000000"), None);
        assert_eq!(msix.launcher("9PCFS5B6T72H"), None);
    }

    #[test]
    fn installed_and_status_answer_from_the_fixture() {
        let (msix, _dir) = source();
        let status = msix.status();
        assert!(status.available);
        assert!(status.searchable);
        assert_eq!(status.detail.as_deref(), Some("6 applications"));
        assert!(status.setup.is_none());
        let installed = msix.installed().unwrap();
        assert_eq!(installed.len(), 6);
        assert!(
            installed
                .iter()
                .all(|p| p.source == SourceKind::Msix && p.installed)
        );
    }

    #[test]
    fn a_source_that_cannot_read_the_appx_state_says_why_and_stays_searchable() {
        let lock = OnceLock::new();
        let _ = lock.set(Err("The list could not be read.".to_string()));
        let dir = tempfile::tempdir().unwrap();
        let msix = Msix {
            client: Arc::new(crate::http::Client::new(dir.path().join("http"))),
            cache: dir.path().to_path_buf(),
            packages: lock,
        };
        let status = msix.status();
        assert!(!status.available);
        assert!(status.searchable);
        assert_eq!(
            status.reason.as_deref(),
            Some("The list could not be read.")
        );
        assert!(msix.installed().is_err());
    }

    #[test]
    fn a_logo_is_copied_into_the_cache_once() {
        let dir = tempfile::tempdir().unwrap();
        let logo = dir.path().join("StoreLogo.scale-100.png");
        std::fs::write(&logo, b"\x89PNG not really").unwrap();
        let cache = dir.path().join("cache");
        let first = cached_logo(&cache, &logo).expect("copied");
        assert!(first.starts_with(cache.join("icons")));
        assert_eq!(first.extension().and_then(|e| e.to_str()), Some("png"));
        std::fs::remove_file(&logo).unwrap();
        assert_eq!(
            cached_logo(&cache, &logo),
            Some(first),
            "served from the cache"
        );
        assert_eq!(cached_logo(&cache, &dir.path().join("gone.png")), None);
    }

    #[test]
    fn the_logo_is_the_scale_nearest_200_and_never_high_contrast() {
        let names: Vec<String> = [
            "PaintStoreLogo.scale-100.png",
            "PaintStoreLogo.scale-400.png",
            "PaintStoreLogo.scale-200_contrast-black.png",
            "PaintStoreLogo.scale-150.png",
            "PaintStoreLogo.scale-200.png",
            "PaintAppList.scale-200.png",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            logo_variant("PaintStoreLogo.png", &names).as_deref(),
            Some("PaintStoreLogo.scale-200.png")
        );
        assert_eq!(
            logo_variant("PaintStoreLogo.png", &names[..2]).as_deref(),
            Some("PaintStoreLogo.scale-100.png"),
            "100 and 400 are 100 and 200 away from 200"
        );
        let exact = vec![
            "storelogo.png".to_string(),
            "StoreLogo.scale-200.png".to_string(),
        ];
        assert_eq!(
            logo_variant("StoreLogo.png", &exact).as_deref(),
            Some("storelogo.png")
        );
        assert_eq!(logo_variant("StoreLogo.png", &names), None);
        let target = vec!["Logo.targetsize-48.png".to_string()];
        assert_eq!(
            logo_variant("Logo.png", &target).as_deref(),
            Some("Logo.targetsize-48.png")
        );
    }

    #[test]
    fn a_logo_is_found_on_disk_under_the_package() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Assets")).unwrap();
        std::fs::write(
            dir.path().join("Assets").join("StoreLogo.scale-200.png"),
            b"x",
        )
        .unwrap();
        assert_eq!(
            resolve_logo(dir.path(), r"Assets\StoreLogo.png"),
            Some(dir.path().join("Assets").join("StoreLogo.scale-200.png"))
        );
        assert_eq!(resolve_logo(dir.path(), r"Missing\Logo.png"), None);
    }

    #[test]
    fn the_manifest_names_its_logo() {
        assert_eq!(
            parse_manifest(fixture("manifest-paint.xml").as_bytes())
                .logo
                .as_deref(),
            Some(r"Assets\PaintStoreLogo.png")
        );
        assert_eq!(
            parse_manifest(fixture("manifest-sandbox.xml").as_bytes())
                .logo
                .as_deref(),
            Some(r"Images\StoreLogo.png")
        );
    }

    // ── Shapes ──────────────────────────────────────────────────────────

    #[test]
    fn family_names_product_ids_and_full_names_are_told_apart() {
        assert!(is_family_name("Microsoft.Paint_8wekyb3d8bbwe"));
        assert!(is_family_name("5319275A.WhatsAppDesktop_cv1g1gvanyjgm"));
        assert!(!is_family_name("zoom.us.Zoom Video Meetings"));
        assert!(!is_family_name(
            r"{6D809377-6AF0-444B-8957-A3773F02200E}\CELSYS\CLIP STUDIO 1.5\CLIP STUDIO\CLIPStudio.exe"
        ));
        assert!(!is_family_name("Microsoft.Paint_8WEKYB3D8BBWE"));
        assert!(!is_family_name("9PCFS5B6T72H"));

        assert!(is_product_id("9PCFS5B6T72H"));
        assert!(is_product_id("XP99J3KP4XZ4VV"));
        assert!(!is_product_id("9pcfs5b6t72h"));
        assert!(!is_product_id("--source"));
        assert!(!is_product_id("Microsoft.Paint_8wekyb3d8bbwe"));

        assert!(is_full_name(
            "Microsoft.Paint_11.2609.441.0_x64__8wekyb3d8bbwe"
        ));
        assert!(is_full_name(
            "Microsoft.Paint_11.2507.371.0_neutral_~_8wekyb3d8bbwe"
        ));
        assert!(!is_full_name("x_'; Remove-Item C:\\ -Recurse; '"));
        assert!(!is_full_name(""));
    }

    // ── The Store catalogue ─────────────────────────────────────────────

    #[test]
    fn a_search_answer_reads_as_hits_with_their_families() {
        let hits = parse_search(&fixture("search-paint.json")).unwrap();
        assert_eq!(hits.len(), 7);
        assert_eq!(
            hits[0],
            SearchHit {
                product_id: "9PCFS5B6T72H".into(),
                name: "Paint".into(),
                publisher: Some("Microsoft Corporation".into()),
                family: Some("Microsoft.Paint_8wekyb3d8bbwe".into()),
            }
        );
        let clip = hits
            .iter()
            .find(|h| h.product_id.starts_with("XP"))
            .unwrap();
        assert_eq!(clip.family, None, "a path is not a family");
    }

    #[test]
    fn a_search_answer_that_is_not_json_is_an_error_that_says_so() {
        let e = parse_search("<html>").unwrap_err();
        assert!(e.message.starts_with("The Store sent a search answer"));
        assert_eq!(parse_search(r#"{"Data":[]}"#).unwrap(), []);
    }

    #[test]
    fn the_search_body_carries_the_market() {
        let body = search_body("spotify", "NO");
        assert_eq!(body["Query"]["KeyWord"], "spotify");
        assert_eq!(body["Filters"][0]["PackageMatchField"], "Market");
        assert_eq!(body["Filters"][0]["RequestMatch"]["KeyWord"], "NO");
    }

    #[test]
    fn a_hit_already_installed_takes_its_family_as_its_id() {
        let hits = parse_search(&fixture("search-paint.json")).unwrap();
        let paint = by_name("Microsoft.Paint");
        let icon = "https://store-images.s-microsoft.com/image/x".to_string();
        let installed = hit_to_package(&hits[0], Some(&paint), Some(&icon));
        assert_eq!(installed.id, "Microsoft.Paint_8wekyb3d8bbwe");
        assert!(installed.installed);
        assert_eq!(
            installed.installed_version.as_deref(),
            Some("11.2609.441.0")
        );
        assert_eq!(installed.icon, Some(Picture::Url(icon)));
        let other = hit_to_package(&hits[1], None, None);
        assert_eq!(
            other.id, "ibisinc.ibisPaint_sxbx2qs82h9wr",
            "the family, installed or not"
        );
        assert!(!other.installed);
        let win32 = hits.iter().find(|h| h.family.is_none()).unwrap();
        assert_eq!(
            hit_to_package(win32, None, None).id,
            win32.product_id,
            "a Win32 product has no family to be named by"
        );
        assert!(
            other
                .facts
                .contains(&("Store ID".to_string(), "9PFDF2ZD4Z4N".to_string()))
        );
    }

    #[test]
    fn a_batch_of_icons_comes_from_the_catalogue_by_product_id() {
        let listings = parse_catalogue(&fixture("catalogue-browse.json")).unwrap();
        let ids: Vec<&str> = listings.iter().map(|l| l.product_id.as_str()).collect();
        assert_eq!(
            ids,
            ["9PCFS5B6T72H", "9PFDF2ZD4Z4N"],
            "the Win32 product asked for is not in the display catalogue"
        );
        let icon = listings[0].icon.as_deref().unwrap();
        assert!(
            icon.starts_with("https://store-images.s-microsoft.com/"),
            "{icon}"
        );
    }

    #[test]
    fn the_icon_is_the_smallest_square_at_least_150_across() {
        let images = [
            Image {
                purpose: "Logo",
                url: "//a/50",
                width: Some(50),
                height: Some(50),
                caption: None,
            },
            Image {
                purpose: "Tile",
                url: "//a/300",
                width: Some(300),
                height: Some(300),
                caption: None,
            },
            Image {
                purpose: "Tile",
                url: "//a/150",
                width: Some(150),
                height: Some(150),
                caption: None,
            },
            Image {
                purpose: "Screenshot",
                url: "//a/shot",
                width: Some(1366),
                height: Some(768),
                caption: None,
            },
            Image {
                purpose: "Tile",
                url: "//a/wide",
                width: Some(310),
                height: Some(150),
                caption: None,
            },
        ];
        assert_eq!(choose_icon(&images).as_deref(), Some("https://a/150"));
        assert_eq!(
            choose_icon(&images[..1]).as_deref(),
            Some("https://a/50"),
            "else the largest there is"
        );
        assert_eq!(choose_icon(&images[3..4]), None);
    }

    #[test]
    fn an_installed_familys_listing_comes_from_the_lookup() {
        let listings = parse_catalogue(&fixture("catalogue-lookup-paint.json")).unwrap();
        let l = &listings[0];
        assert_eq!(l.product_id, "9PCFS5B6T72H");
        assert_eq!(l.title.as_deref(), Some("Paint"));
        assert_eq!(l.family.as_deref(), Some("Microsoft.Paint_8wekyb3d8bbwe"));
        assert_eq!(l.categories, ["Multimedia design"]);
        assert!(l.description.as_deref().unwrap().starts_with("Paint is"));
        assert!(!l.screenshots.is_empty());
        assert!(l.screenshots.iter().all(|s| s.caption.is_some()));
        assert!(l.rating.is_some());
    }

    #[test]
    fn a_product_page_reads_for_an_msix_and_a_win32_product_alike() {
        let paint = parse_product(&fixture("product-paint.json")).unwrap();
        assert_eq!(paint.product_id, "9PCFS5B6T72H");
        assert_eq!(
            paint.family.as_deref(),
            Some("Microsoft.Paint_8wekyb3d8bbwe")
        );
        assert_eq!(paint.screenshots.len(), 6);
        assert_eq!(
            paint.screenshots[0].caption.as_deref(),
            Some("Simple and powerful creative toolset.")
        );
        assert!(paint.icon.is_some());

        let zoom = parse_product(&fixture("product-zoom.json")).unwrap();
        assert_eq!(zoom.product_id, "XP99J3KP4XZ4VV");
        assert_eq!(zoom.family, None, "a Win32 product has no family");
        assert_eq!(zoom.homepage.as_deref(), Some("https://zoom.us/"));
        assert_eq!(zoom.rating, None, "0.0 from 0 ratings is no rating");
        assert!(zoom.icon.as_deref().unwrap().starts_with("https://"));
        assert!(zoom.summary.is_some());
    }

    #[test]
    fn a_listing_fills_in_what_the_package_lacks_and_keeps_what_it_knows() {
        let listing = parse_product(&fixture("product-paint.json")).unwrap();
        let mut pkg = to_package(
            &by_name("Microsoft.Paint"),
            Some(Picture::File(PathBuf::from("cached.png"))),
        );
        enrich(&mut pkg, &listing);
        assert_eq!(pkg.icon, Some(Picture::File(PathBuf::from("cached.png"))));
        assert_eq!(pkg.screenshots.len(), 6);
        assert!(pkg.description.as_deref().unwrap().starts_with("<p>"));
        assert_eq!(
            pkg.facts[0],
            ("Store ID".to_string(), "9PCFS5B6T72H".to_string())
        );
        assert!(
            pkg.popularity_label
                .as_deref()
                .unwrap()
                .ends_with("ratings")
        );
    }

    #[test]
    fn store_text_becomes_escaped_paragraphs_and_a_short_summary() {
        assert_eq!(
            paragraphs("One <b>\n\n Two & three "),
            "<p>One &lt;b&gt;</p><p>Two &amp; three</p>"
        );
        assert_eq!(
            summary_from("Short.\nLonger second line."),
            Some("Short.".into())
        );
        let long = format!("{} Then more. {}", "A first sentence.", "x".repeat(200));
        assert_eq!(summary_from(&long), Some("A first sentence.".into()));
        assert_eq!(summary_from(&"y".repeat(300)), None);
    }

    #[test]
    fn a_rating_reads_with_grouped_digits() {
        assert_eq!(rating_label(4.1, 81556), "4.1 from 81 556 ratings");
        assert_eq!(rating_label(5.0, 1), "5.0 from 1 rating");
        assert_eq!(rating_label(3.75, 1234567), "3.8 from 1 234 567 ratings");
    }

    #[test]
    fn a_market_and_a_language_are_checked_before_they_reach_a_url() {
        assert_eq!(
            normalise_locale(Some("no"), Some("en-GB")),
            ("NO".to_string(), "en-GB".to_string())
        );
        assert_eq!(
            normalise_locale(Some("NOR"), Some("en&x=1")),
            ("US".to_string(), "en-US".to_string())
        );
        assert_eq!(
            normalise_locale(None, None),
            ("US".to_string(), "en-US".to_string())
        );
    }

    // ── Plans ───────────────────────────────────────────────────────────

    fn no_lookup(_: &str) -> Result<Option<String>> {
        Ok(None)
    }

    const WINGET: &str = r"C:\Program Files\WindowsApps\Microsoft.DesktopAppInstaller_1.29.290.0_x64__8wekyb3d8bbwe\winget.exe";

    #[test]
    fn installing_goes_through_wingets_store_source_without_elevating() {
        let (msix, _dir) = source();
        let steps = msix
            .plan_with(
                &Op::Install {
                    package: op_ref("9PFDF2ZD4Z4N"),
                },
                WINGET,
                &no_lookup,
            )
            .unwrap();
        assert_eq!(steps.len(), 1);
        let s = &steps[0];
        assert_eq!(s.source, SourceKind::Msix);
        assert!(!s.needs_root, "a Store application is the user's own");
        assert_eq!(s.command.program, WINGET);
        let args: Vec<&str> = s.command.args.iter().map(String::as_str).collect();
        assert_eq!(
            args,
            [
                "install",
                "--exact",
                "--id",
                "9PFDF2ZD4Z4N",
                "--source",
                "msstore",
                "--silent",
                "--disable-interactivity",
                "--accept-source-agreements",
                "--accept-package-agreements",
            ]
        );
    }

    #[test]
    fn installing_what_is_here_does_nothing_and_a_name_is_refused() {
        let (msix, _dir) = source();
        let here = Op::Install {
            package: op_ref("Microsoft.Paint_8wekyb3d8bbwe"),
        };
        assert!(
            msix.plan_with(&here, WINGET, &no_lookup)
                .unwrap()
                .is_empty()
        );
        let nonsense = Op::Install {
            package: op_ref("--override"),
        };
        let e = msix.plan_with(&nonsense, WINGET, &no_lookup).unwrap_err();
        assert!(
            e.message.contains("is neither a package family name nor"),
            "{}",
            e.message
        );
    }

    #[test]
    fn installing_by_family_asks_the_catalogue_for_the_product() {
        let (msix, _dir) = source();
        let op = Op::Install {
            package: op_ref("ibisinc.ibisPaint_sxbx2qs82h9wr"),
        };
        let lookup = |family: &str| -> Result<Option<String>> {
            assert_eq!(family, "ibisinc.ibisPaint_sxbx2qs82h9wr");
            Ok(Some("9PFDF2ZD4Z4N".to_string()))
        };
        let steps = msix.plan_with(&op, WINGET, &lookup).unwrap();
        assert_eq!(steps.len(), 1);
        assert!(steps[0].command.args.iter().any(|a| a == "9PFDF2ZD4Z4N"));
        assert_eq!(
            steps[0].title,
            "Installing ibisinc.ibisPaint from the Store"
        );
        let e = msix.plan_with(&op, WINGET, &no_lookup).unwrap_err();
        assert!(
            e.message.starts_with("The Store does not list"),
            "{}",
            e.message
        );
        let bad = |_: &str| -> Result<Option<String>> { Ok(Some("--force".to_string())) };
        assert!(
            msix.plan_with(&op, WINGET, &bad).is_err(),
            "only a product id reaches winget"
        );
    }

    #[test]
    fn a_full_name_reduces_to_its_family() {
        assert_eq!(
            family_from_full_name("Microsoft.WindowsTerminal_1.21.2361.0_x64__8wekyb3d8bbwe")
                .as_deref(),
            Some("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
        );
        assert_eq!(
            family_from_full_name("Microsoft.Paint_11.2507.371.0_neutral_~_8wekyb3d8bbwe")
                .as_deref(),
            Some("Microsoft.Paint_8wekyb3d8bbwe")
        );
        for p in packages() {
            assert_eq!(
                family_from_full_name(&p.full_name).as_deref(),
                Some(p.family_name.as_str()),
                "{}",
                p.full_name
            );
        }
        assert_eq!(family_from_full_name("Microsoft.Paint_8wekyb3d8bbwe"), None);
        assert_eq!(family_from_full_name("a_b_c_d_E"), None);
    }

    #[test]
    fn removing_is_remove_appxpackage_with_the_full_name_for_this_user() {
        let (msix, _dir) = source();
        let steps = msix
            .plan_with(
                &Op::Remove {
                    package: op_ref("Microsoft.Paint_8wekyb3d8bbwe"),
                },
                WINGET,
                &no_lookup,
            )
            .unwrap();
        assert_eq!(steps.len(), 1);
        let s = &steps[0];
        assert!(!s.needs_root);
        assert_eq!(s.title, "Removing Paint");
        assert_eq!(s.command.program, "powershell.exe");
        assert_eq!(
            s.command.args.last().unwrap(),
            "Remove-AppxPackage -Package 'Microsoft.Paint_11.2609.441.0_x64__8wekyb3d8bbwe'"
        );
    }

    #[test]
    fn removing_a_part_of_windows_or_something_absent_is_refused_with_a_reason() {
        let (msix, _dir) = source();
        let windows = by_name("Microsoft.SecHealthUI");
        let e = msix
            .plan_with(
                &Op::Remove {
                    package: op_ref(&windows.family_name),
                },
                WINGET,
                &no_lookup,
            )
            .unwrap_err();
        assert!(
            e.message
                .ends_with("is part of Windows and cannot be removed."),
            "{}",
            e.message
        );
        let e = msix
            .plan_with(
                &Op::Remove {
                    package: op_ref("9PCFS5B6T72H"),
                },
                WINGET,
                &no_lookup,
            )
            .unwrap_err();
        assert!(
            e.message.contains("is not installed for this user"),
            "{}",
            e.message
        );
    }

    #[test]
    fn a_full_name_that_could_escape_the_literal_is_never_planned() {
        let mut raw = by_name("Microsoft.Paint");
        raw.full_name = "Microsoft.Paint_1'; Remove-Item C:\\; '_x64__8wekyb3d8bbwe".to_string();
        let dir = tempfile::tempdir().unwrap();
        let msix = Msix::with_packages(vec![raw], dir.path().to_path_buf());
        let e = msix
            .plan_with(
                &Op::Remove {
                    package: op_ref("Microsoft.Paint_8wekyb3d8bbwe"),
                },
                WINGET,
                &no_lookup,
            )
            .unwrap_err();
        assert!(
            e.message.contains("will not pass to PowerShell"),
            "{}",
            e.message
        );
    }

    #[test]
    fn updates_refreshes_and_other_sources_plan_nothing() {
        let (msix, _dir) = source();
        assert!(msix.updates().unwrap().is_empty());
        for op in [
            Op::Update {
                package: op_ref("Microsoft.Paint_8wekyb3d8bbwe"),
            },
            Op::UpdateAll {
                source: SourceKind::Msix,
            },
            Op::Refresh {
                source: SourceKind::Msix,
            },
            Op::Install {
                package: PackageRef {
                    source: SourceKind::Winget,
                    id: "9PCFS5B6T72H".into(),
                },
            },
        ] {
            assert!(
                msix.plan_with(&op, WINGET, &no_lookup).unwrap().is_empty(),
                "{op:?}"
            );
        }
    }

    /// The spec's setup table: the Store has nothing of its own to set up,
    /// and chains winget's.
    #[cfg(not(windows))]
    #[test]
    fn setting_the_store_up_is_setting_winget_up() {
        let (msix, _dir) = source();
        let setup = msix.setup().expect("winget is never present here");
        assert_eq!(
            setup.ops,
            [Op::Setup {
                source: SourceKind::Winget
            }]
        );
        assert!(setup.steps.is_empty());
        assert!(setup.notice.contains("winget"));
    }

    #[cfg(windows)]
    #[test]
    fn setting_the_store_up_is_setting_winget_up() {
        let (msix, _dir) = source();
        match msix.setup() {
            None => assert!(super::super::winget::winget_exe().is_some()),
            Some(setup) => {
                assert_eq!(
                    setup.ops,
                    [Op::Setup {
                        source: SourceKind::Winget
                    }]
                );
                assert!(setup.steps.is_empty());
            }
        }
    }

    /// Reads this machine's real Appx state. Not network, but not a
    /// fixture either, so it is ignored with the live tests.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn live_the_appx_state_reads_and_filters() {
        let all = read().expect("the Appx state reads");
        let apps: Vec<&RawPackage> = all.iter().filter(|p| is_application(p)).collect();
        eprintln!("{} packages, {} applications", all.len(), apps.len());
        for p in &apps {
            eprintln!("  {} ({})", display_name(p), p.family_name);
        }
        assert!(!all.is_empty());
        if let Ok(path) = std::env::var("BROKEY_APPX_DUMP") {
            std::fs::write(path, serde_json::to_string_pretty(&all).unwrap()).unwrap();
        }
    }

    #[test]
    #[ignore]
    fn live_the_store_answers_a_search() {
        let dir = tempfile::tempdir().unwrap();
        let msix = Msix::with_packages(Vec::new(), dir.path().to_path_buf());
        let found = msix.search(&Query::new("paint")).unwrap();
        assert!(found.iter().any(|p| p.name == "Paint"), "{found:?}");
        assert!(found.iter().any(|p| p.icon.is_some()));
        let paint = found.iter().find(|p| p.name == "Paint").unwrap();
        assert_eq!(paint.id, "Microsoft.Paint_8wekyb3d8bbwe");
    }

    #[test]
    #[ignore]
    fn live_a_detail_page_reads_by_family_and_by_product_id() {
        let dir = tempfile::tempdir().unwrap();
        let msix = Msix::with_packages(Vec::new(), dir.path().to_path_buf());
        let by_family = msix.details("Microsoft.Paint_8wekyb3d8bbwe").unwrap();
        assert_eq!(by_family.name, "Paint");
        assert!(!by_family.screenshots.is_empty());
        assert!(by_family.description.is_some());
        let win32 = msix.details("XP99J3KP4XZ4VV").unwrap();
        assert!(win32.icon.is_some());
        assert!(!win32.screenshots.is_empty());
        let product = msix
            .listing_by_family("Microsoft.Paint_8wekyb3d8bbwe")
            .unwrap();
        assert_eq!(
            product.map(|l| l.product_id).as_deref(),
            Some("9PCFS5B6T72H")
        );
    }
}
