//! The rules for what may be touched (spec §5): nothing another installed
//! application still uses is offered, and protected places never are.
//!
//! The scan guesses; this decides. A candidate is kept back, and named with
//! what uses it, when it **is, contains, or lies inside** something the
//! machine still claims for another application, when what it points at
//! does, or when it was matched only by a name or a publisher that another
//! installed application also answers to. A protected place is dropped
//! without a word, not even as kept: it was never the application's.
//!
//! The application being removed is excluded by where its claims come from,
//! not by path: its own uninstall entry, its own Windows Installer
//! registration and its own shortcuts claim nothing, but another entry that
//! claims the same folder still does. A Steam game whose folder fell back
//! to Steam's is the case that makes the difference.
//!
//! Where it cannot tell, it keeps: a folder that cannot be listed is claimed
//! whole, a key that cannot be read protects the extension that names it.
//! Everything here reads through [`SystemView`] and changes nothing.

use super::hints::Hints;
use super::scan::{self, CLASSES, Candidate, SHELL_CLASSES, Scanned, UNINSTALL};
use super::view::{Hive, Known, Reparse, SystemView, ViewError};
use crate::model::{Confidence, Findings, ItemKind, KeptItem, LeftoverItem, Package, SourceKind};
use crate::sources::windows::arp::{self, RawEntry};
use crate::sources::windows::shortcut;
use crate::sources::windows::winget::normalise;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Where the Windows Installer records which product uses which file or key.
const USER_DATA: &str = r"Software\Microsoft\Windows\CurrentVersion\Installer\UserData";

/// The packed code a component is registered under when it is installed for
/// good and belongs to no one product any more.
const PERMANENT: &str = "00000000000000000000000000000000";

/// How many folders deep a Start menu is searched for other applications'
/// shortcuts.
const MENU_DEPTH: u8 = 4;

/// One path or registry key another application claims.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Claim {
    /// As [`normal`] makes it.
    at: String,
    used_by: String,
}

/// Another installed application, by the names a scan matches on.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Other {
    name: Option<String>,
    publisher: Option<String>,
    display: String,
}

/// What the machine still claims for applications other than the one
/// being removed: paths and registry keys, each with who claims it, and the
/// names and publishers of everything else installed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Claims {
    claims: Vec<Claim>,
    others: Vec<Other>,
    /// Places that could not be read while gathering these.
    pub unreadable: u32,
}

/// The Windows Installer's packed form of a ProductCode, as the keys and
/// values under `Installer\UserData` are named: the first three groups
/// reversed, each pair of the last two swapped, no braces or dashes.
pub fn packed_product_code(code: &str) -> Option<String> {
    let code = arp::product_code(code.trim())?;
    let groups: Vec<&str> = code[1..code.len() - 1].split('-').collect();
    let mut packed = String::with_capacity(32);
    for group in &groups[..3] {
        packed.extend(group.chars().rev());
    }
    for group in &groups[3..] {
        let chars: Vec<char> = group.chars().collect();
        for pair in chars.chunks(2) {
            packed.extend(pair.iter().rev());
        }
    }
    Some(packed.to_uppercase())
}

/// A registry location taken apart: which hive, the key's names in lower
/// case, and the value's name if it is a value. The 32-bit view written as
/// `HKLM\Software\WOW6432Node\…` is folded into `HKLM32\Software\…`.
struct Reg {
    hive: &'static str,
    parts: Vec<String>,
    value: Option<String>,
}

fn parse_reg(location: &str) -> Option<Reg> {
    let (key, value) = match location.split_once(" | ") {
        Some((key, value)) => (key, Some(value.to_lowercase())),
        None => (location, None),
    };
    let key = key.trim().trim_end_matches('\\');
    let (hive, rest) = key.split_once('\\').unwrap_or((key, ""));
    let mut hive = match hive.to_lowercase().as_str() {
        "hklm" => "hklm",
        "hklm32" => "hklm32",
        "hkcu" => "hkcu",
        _ => return None,
    };
    let mut parts: Vec<String> = if rest.is_empty() {
        Vec::new()
    } else {
        rest.split('\\').map(str::to_lowercase).collect()
    };
    // Only the machine's hive has a 32-bit view of its own.
    if hive == "hklm" && parts.len() > 2 && parts[0] == "software" && parts[1] == "wow6432node" {
        parts.remove(1);
        hive = "hklm32";
    }
    Some(Reg { hive, parts, value })
}

impl Reg {
    fn text(&self) -> String {
        let mut text = self.hive.to_string();
        for part in self.parts.iter().chain(&self.value) {
            text.push('\\');
            text.push_str(part);
        }
        text
    }
}

fn is_system(location: &str) -> bool {
    let lower = location.to_lowercase();
    ["service:", "task:", "firewall:"]
        .iter()
        .any(|p| lower.starts_with(p))
}

/// A location as the guard compares it: lower case, one kind of separator,
/// a value as one more name after its key. `None` for a service, task or
/// firewall rule, which are checked by what they point at.
fn normal(location: &str) -> Option<String> {
    if is_system(location) {
        return None;
    }
    Some(match parse_reg(location) {
        Some(reg) => reg.text(),
        None => shortcut::normalise(location),
    })
}

/// `a` is `b`, lies inside it, or holds it.
fn related(a: &str, b: &str) -> bool {
    a == b || strictly_inside(a, b) || strictly_inside(b, a)
}

fn strictly_inside(path: &str, dir: &str) -> bool {
    path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'\\'
}

fn last_name(location: &str) -> Option<&str> {
    if is_system(location) || location.contains(" | ") {
        return None;
    }
    location
        .trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .filter(|n| !n.is_empty())
}

fn hive_of(word: &str) -> Hive {
    match word {
        "hklm32" => Hive::Hklm32,
        "hkcu" => Hive::Hkcu,
        _ => Hive::Hklm,
    }
}

fn arp_hive(hive: Hive) -> arp::Hive {
    match hive {
        Hive::Hklm => arp::Hive::Machine,
        Hive::Hklm32 => arp::Hive::Machine32,
        Hive::Hkcu => arp::Hive::User,
    }
}

fn entry_from(hive: Hive, key: &str, values: &[(String, String)]) -> RawEntry {
    let get = |name: &str| {
        values
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    RawEntry {
        hive: arp_hive(hive),
        key_name: key.to_string(),
        display_name: get("DisplayName"),
        display_version: get("DisplayVersion"),
        publisher: get("Publisher"),
        install_location: get("InstallLocation"),
        uninstall_string: get("UninstallString"),
        quiet_uninstall_string: get("QuietUninstallString"),
        windows_installer: None,
        display_icon: None,
        system_component: None,
        parent_key_name: None,
        release_type: None,
        estimated_size: None,
        url_info_about: None,
    }
}

/// Whether a package in the store's list is the one being removed: the
/// same reference, or joined to the same uninstall entry.
fn is_excluded(package: &Package, excluding: &Hints) -> bool {
    if package.source == excluding.package.source && package.id == excluding.package.id {
        return true;
    }
    let Some((hive, key)) = &excluding.uninstall_key else {
        return false;
    };
    let id = format!(r"{}\{}", scan::hive_word(*hive), key.trim());
    (package.source == SourceKind::Arp && package.id.eq_ignore_ascii_case(&id))
        || package
            .facts
            .iter()
            .any(|(k, v)| k == arp::REGISTRY_KEY_FACT && v.trim().eq_ignore_ascii_case(&id))
}

fn other(name: &str, publisher: Option<&str>) -> Other {
    Other {
        name: scan::usable(normalise::name(name)),
        publisher: publisher.map(normalise::publisher).and_then(scan::usable),
        display: name.trim().to_string(),
    }
}

/// Gathers the claims.
struct Gather<'a> {
    view: &'a dyn SystemView,
    excluding: Option<&'a Hints>,
    /// The excluded application's own folders, narrow ones only: what lies
    /// in them is the application's, not a claim.
    homes: Vec<String>,
    out: Claims,
}

impl<'a> Gather<'a> {
    fn new(view: &'a dyn SystemView, excluding: Option<&'a Hints>) -> Gather<'a> {
        let homes = excluding
            .map(|h| {
                [&h.install_dir, &h.install_location]
                    .into_iter()
                    .flatten()
                    .chain(&h.extra_dirs)
                    .map(|p| p.to_string_lossy().into_owned())
                    .filter(|p| shortcut::specific(p))
                    .collect()
            })
            .unwrap_or_default();
        Gather {
            view,
            excluding,
            homes,
            out: Claims::default(),
        }
    }

    fn in_home(&self, path: &str) -> bool {
        self.homes.iter().any(|h| {
            shortcut::normalise(path) == shortcut::normalise(h) || shortcut::inside(path, h)
        })
    }

    fn count(&mut self, error: &ViewError) {
        if matches!(error, ViewError::Unreadable(_)) {
            self.out.unreadable += 1;
        }
    }

    fn folder(&self, which: Known) -> Option<String> {
        self.view
            .known_folder(which)
            .map(|p| p.to_string_lossy().into_owned())
    }

    /// A path another application claims. One too broad to name a single
    /// application (a drive, `Program Files`, the profile) says nothing about
    /// which, and would hold back everything; it is not taken.
    fn path(&mut self, path: &str, used_by: &str) {
        let path = path.trim().trim_matches('"');
        if path.is_empty() || !shortcut::specific(path) {
            return;
        }
        self.out.claims.push(Claim {
            at: shortcut::normalise(path),
            used_by: used_by.to_string(),
        });
    }

    /// A registry key another application claims, unless it is one of the
    /// roots every application's keys are under.
    fn key(&mut self, location: &str, used_by: &str) {
        let Some(reg) = parse_reg(location) else {
            return;
        };
        let broad = match reg.parts.iter().map(String::as_str).collect::<Vec<_>>()[..] {
            [] | ["software"] => true,
            ["software", second] => {
                ["classes", "microsoft", "policies", "wow6432node"].contains(&second)
            }
            _ => false,
        };
        if !broad {
            self.out.claims.push(Claim {
                at: reg.text(),
                used_by: used_by.to_string(),
            });
        }
    }

    /// Every other Add/Remove entry: its folder, its `InstallLocation` and
    /// its own key. Returns the names entries give MSI products, by packed
    /// code, for the components below.
    fn add_remove(&mut self) -> HashMap<String, String> {
        let mut products = HashMap::new();
        let excluded = self
            .excluding
            .and_then(|h| h.uninstall_key.clone())
            .map(|(hive, key)| (hive, key.trim().to_lowercase()));
        for hive in [Hive::Hklm, Hive::Hklm32, Hive::Hkcu] {
            let keys = match self.view.subkeys(hive, UNINSTALL) {
                Ok(keys) => keys,
                Err(e) => {
                    self.count(&e);
                    continue;
                }
            };
            for key in keys {
                if excluded
                    .as_ref()
                    .is_some_and(|(h, k)| *h == hive && *k == key.trim().to_lowercase())
                {
                    continue;
                }
                let path = format!(r"{UNINSTALL}\{key}");
                let values = match self.view.values(hive, &path) {
                    Ok(values) => values,
                    Err(e) => {
                        self.count(&e);
                        continue;
                    }
                };
                let entry = entry_from(hive, &key, &values);
                let name = entry.display_name.clone().unwrap_or_else(|| key.clone());
                if let Some(dir) = arp::install_dir(&entry) {
                    self.path(&dir.to_string_lossy(), &name);
                }
                if let Some(location) = &entry.install_location {
                    self.path(location, &name);
                }
                self.key(&format!(r"{}\{path}", scan::hive_word(hive)), &name);
                if let Some(packed) = packed_product_code(&key) {
                    products.insert(packed, name.clone());
                }
                self.out
                    .others
                    .push(other(&name, entry.publisher.as_deref()));
            }
        }
        products
    }

    /// Windows Installer components: each value names a product by its
    /// packed code and the file or key that product uses. Every product but
    /// the one being removed claims its path.
    fn components(&mut self, mut products: HashMap<String, String>) {
        let excluded = self
            .excluding
            .and_then(|h| h.product_code.as_deref())
            .and_then(packed_product_code);
        let users = match self.view.subkeys(Hive::Hklm, USER_DATA) {
            Ok(users) => users,
            Err(e) => {
                self.count(&e);
                return;
            }
        };
        for user in &users {
            // The names the products registered themselves under, which a
            // product hidden from Add/Remove Programs still has.
            let products_key = format!(r"{USER_DATA}\{user}\Products");
            for packed in self
                .view
                .subkeys(Hive::Hklm, &products_key)
                .unwrap_or_default()
            {
                let properties = format!(r"{products_key}\{packed}\InstallProperties");
                if let Some((_, name)) = self
                    .view
                    .values(Hive::Hklm, &properties)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|(n, v)| n.eq_ignore_ascii_case("DisplayName") && !v.trim().is_empty())
                {
                    products.insert(packed.to_uppercase(), name.trim().to_string());
                }
            }
        }
        for user in users {
            let root = format!(r"{USER_DATA}\{user}\Components");
            let components = match self.view.subkeys(Hive::Hklm, &root) {
                Ok(components) => components,
                Err(e) => {
                    self.count(&e);
                    continue;
                }
            };
            for component in components {
                let values = match self
                    .view
                    .values(Hive::Hklm, &format!(r"{root}\{component}"))
                {
                    Ok(values) => values,
                    Err(e) => {
                        self.count(&e);
                        continue;
                    }
                };
                for (packed, data) in values {
                    let packed = packed.trim().to_uppercase();
                    if packed.is_empty() || excluded.as_deref() == Some(packed.as_str()) {
                        continue;
                    }
                    let used_by = if packed == PERMANENT {
                        "Windows Installer".to_string()
                    } else {
                        products
                            .get(&packed)
                            .cloned()
                            .unwrap_or_else(|| "A Windows Installer product".to_string())
                    };
                    self.component(&data, &used_by);
                }
            }
        }
    }

    /// A component's path: a file (`C:\…`, or `C?\…` for one not installed
    /// locally), or a key behind a two-digit root (`01:` the user's, `02:`
    /// the machine's, `00:` the classes; plus twenty for the 64-bit view).
    fn component(&mut self, data: &str, used_by: &str) {
        let data = data.trim();
        let bytes = data.as_bytes();
        if bytes.len() > 3 && bytes[0].is_ascii_alphabetic() && matches!(bytes[1], b':' | b'?') {
            self.path(&format!("{}:{}", &data[..1], &data[2..]), used_by);
            return;
        }
        let Some((root, rest)) = data.split_once(':') else {
            return;
        };
        let rest = rest.trim_matches('\\');
        if rest.is_empty() {
            return;
        }
        // Which view `02:` means differs by how the product was built, so
        // both are claimed.
        let hives: &[&str] = match root {
            "00" | "20" => &[r"HKCU\Software\Classes", r"HKLM\Software\Classes"],
            "01" | "21" => &["HKCU"],
            "02" | "22" => &["HKLM", "HKLM32"],
            _ => &[],
        };
        for hive in hives {
            self.key(&format!(r"{hive}\{rest}"), used_by);
        }
    }

    /// Each folder in a folder that holds one per package, except the
    /// application's own. One that cannot be listed is claimed whole.
    fn per_package(&mut self, root: &str, whole: &str, name_of: &dyn Fn(&str) -> Option<String>) {
        match self.view.list(Path::new(root)) {
            Ok(listed) => {
                if listed.unreadable > 0 {
                    self.out.unreadable += listed.unreadable;
                    self.path(root, whole);
                }
                for entry in listed.items.iter().filter(|e| e.is_dir) {
                    let path = format!(r"{}\{}", root.trim_end_matches('\\'), entry.name);
                    let own = self.excluding.is_some_and(|h| {
                        h.extra_dirs.iter().any(|d| {
                            shortcut::normalise(&d.to_string_lossy()) == shortcut::normalise(&path)
                        })
                    });
                    if !own {
                        let used_by = name_of(&entry.name).unwrap_or_else(|| entry.name.clone());
                        self.path(&path, &used_by);
                    }
                }
            }
            Err(e) => {
                if matches!(e, ViewError::Unreadable(_)) {
                    self.out.unreadable += 1;
                    self.path(root, whole);
                }
            }
        }
    }

    /// Store packages' folders, and Scoop's and Chocolatey's per-package ones.
    fn package_folders(&mut self, installed: &[Package]) {
        let name_in = |source: SourceKind| {
            move |folder: &str| {
                installed
                    .iter()
                    .find(|p| p.source == source && p.id.eq_ignore_ascii_case(folder))
                    .map(|p| p.name.clone())
            }
        };
        let store = "Microsoft Store applications";
        if let Some(local) = self.folder(Known::LocalAppData) {
            self.per_package(
                &format!(r"{local}\Packages"),
                store,
                &name_in(SourceKind::Msix),
            );
        }
        if let Some(program_files) = self.folder(Known::ProgramFiles) {
            self.path(&format!(r"{program_files}\WindowsApps"), store);
        }
        let mut roots = Vec::new();
        if let Some(profile) = self.folder(Known::Profile) {
            roots.push((format!(r"{profile}\scoop\apps"), "Scoop", SourceKind::Scoop));
        }
        if let Some(data) = self.folder(Known::ProgramData) {
            roots.push((format!(r"{data}\scoop\apps"), "Scoop", SourceKind::Scoop));
            roots.push((
                format!(r"{data}\chocolatey\lib"),
                "Chocolatey",
                SourceKind::Choco,
            ));
        }
        for (root, whole, source) in roots {
            self.per_package(&root, whole, &name_in(source));
        }
    }

    /// The targets of other applications' Start menu shortcuts. A target
    /// that is gone protects nothing; one in the application's own folder
    /// is its own.
    fn shortcuts(&mut self) {
        let own: HashSet<String> = self
            .excluding
            .map(|h| {
                h.shortcuts
                    .iter()
                    .map(|s| shortcut::normalise(&s.to_string_lossy()))
                    .collect()
            })
            .unwrap_or_default();
        for which in [Known::StartMenuMachine, Known::StartMenuUser] {
            if let Some(root) = self.folder(which) {
                self.menu(&root, 0, &own);
            }
        }
    }

    fn menu(&mut self, dir: &str, depth: u8, own: &HashSet<String>) {
        let listed = match self.view.list(Path::new(dir)) {
            Ok(listed) => listed,
            Err(e) => {
                self.count(&e);
                return;
            }
        };
        self.out.unreadable += listed.unreadable;
        for entry in listed.items {
            if entry.reparse == Reparse::NameSurrogate {
                continue;
            }
            let path = format!(r"{dir}\{}", entry.name);
            if entry.is_dir {
                if depth + 1 < MENU_DEPTH {
                    self.menu(&path, depth + 1, own);
                }
                continue;
            }
            if !entry.name.to_lowercase().ends_with(".lnk")
                || own.contains(&shortcut::normalise(&path))
            {
                continue;
            }
            let Some(target) = self.view.shortcut_target(Path::new(&path)) else {
                continue;
            };
            let target = target.to_string_lossy().into_owned();
            if self.in_home(&target) || !self.view.exists(Path::new(&target)) {
                continue;
            }
            let name = entry.name[..entry.name.len() - 4].to_string();
            self.path(&target, &name);
        }
    }

    /// Services and scheduled tasks whose program is outside the
    /// application's folder.
    fn services_and_tasks(&mut self) {
        let env = scan::environment(self.view);
        let services = self.view.services();
        self.out.unreadable += services.unreadable;
        for service in services.items {
            let program = scan::program(&service.image_path, &env);
            if !self.in_home(&program) {
                self.path(&program, &format!("{} (service)", service.name));
            }
        }
        let tasks = self.view.tasks();
        self.out.unreadable += tasks.unreadable;
        for task in tasks.items {
            for action in &task.actions {
                let program = scan::program(action, &env);
                if !self.in_home(&program) {
                    self.path(&program, &format!("{} (scheduled task)", task.path));
                }
            }
        }
    }
}

/// What the machine still claims for applications other than `excluding`,
/// the one being removed or force-removed: its own install folder,
/// uninstall entry and Windows Installer registration are not claims.
/// `installed` is every source's installed list.
pub fn claims(view: &dyn SystemView, installed: &[Package], excluding: Option<&Hints>) -> Claims {
    let mut gather = Gather::new(view, excluding);
    let products = gather.add_remove();
    gather.components(products);
    gather.package_folders(installed);
    gather.shortcuts();
    gather.services_and_tasks();
    for package in installed {
        if excluding.is_some_and(|h| is_excluded(package, h)) {
            continue;
        }
        gather
            .out
            .others
            .push(other(&package.name, package.developer.as_deref()));
    }
    gather.out
}

impl Claims {
    /// Who uses what a candidate is or points at, if anyone else does.
    fn user_of(&self, candidate: &Candidate) -> Option<String> {
        let places = normal(&candidate.location).into_iter().chain(
            candidate
                .points_to
                .as_ref()
                .map(|p| shortcut::normalise(&p.to_string_lossy())),
        );
        for at in places {
            if let Some(claim) = self.claims.iter().find(|c| related(&at, &c.at)) {
                return Some(claim.used_by.clone());
            }
        }
        // A match by name or by publisher alone: someone else with that
        // name, or that publisher, may be what put it there.
        if candidate.points_to.is_some() || candidate.confidence == Confidence::Certain {
            return None;
        }
        let leaf = last_name(&candidate.location)?;
        let publisher = scan::usable(normalise::publisher(leaf));
        let name = scan::usable(normalise::name(leaf));
        self.others
            .iter()
            .find(|o| publisher.is_some() && o.publisher == publisher)
            .or_else(|| {
                self.others
                    .iter()
                    .find(|o| name.is_some() && o.name == name)
            })
            .map(|o| o.display.clone())
    }
}

/// The places that are never offered, from this machine's folders.
struct Places {
    /// Never offered, nor anything that holds one.
    roots: Vec<String>,
    /// Never offered, nor anything in it.
    windows: Option<String>,
    /// A folder protection depends on did not resolve: no path is offered,
    /// because what it would have protected cannot be told apart.
    unresolved: bool,
}

/// Whether a path names a place by another spelling than its own, so that
/// comparing it with the protected places as text would say nothing: an
/// 8.3 short name (`PROGRA~1`), an NTFS stream (`::$INDEX_ALLOCATION`), or a
/// name ending in a dot or a space, which Windows drops when it opens it.
pub(super) fn another_spelling(path: &str) -> bool {
    let trimmed = path.trim_start().trim_end_matches(['\\', '/']);
    let mut names = trimmed.split(['\\', '/']);
    let drive = names.next().unwrap_or_default();
    // A colon belongs only right after the drive letter.
    let odd_drive = drive.contains(':') && !(drive.len() == 2 && drive.ends_with(':'));
    odd_drive
        || names.any(|name| {
            let bytes = name.as_bytes();
            name.contains(':')
                || name.ends_with(['.', ' '])
                || bytes
                    .windows(2)
                    .any(|w| w[0] == b'~' && w[1].is_ascii_digit())
        })
}

/// The shapes of a profile's folders, whatever this machine's well-known
/// folders say: `Users\<name>`, its `AppData` and the three folders in it.
/// `Users\Public` and `Users\Default` are profiles of this shape too.
fn profile_shape(parts: &[&str]) -> bool {
    matches!(
        parts,
        ["users"]
            | ["users", _]
            | ["users", _, "appdata"]
            | ["users", _, "appdata", "local" | "roaming" | "locallow"]
    )
}

impl Places {
    fn new(view: &dyn SystemView) -> Places {
        let folder = |k: Known| {
            view.known_folder(k)
                .map(|p| shortcut::normalise(&p.to_string_lossy()))
        };
        let mut roots = Vec::new();
        let mut unresolved = false;
        for which in [
            Known::ProgramFiles,
            Known::ProgramFilesX86,
            Known::ProgramData,
            Known::AppData,
            Known::LocalAppData,
            Known::LocalLow,
            Known::Profile,
            Known::Documents,
            Known::SavedGames,
            Known::Desktop,
            Known::Downloads,
            Known::Pictures,
            Known::Music,
            Known::Videos,
            Known::StartMenuMachine,
            Known::StartMenuUser,
        ] {
            let Some(root) = folder(which) else {
                // The user's own media folders protect only themselves, and
                // the profile's shape below covers them; the rest are what
                // the brief's protected places are made of.
                unresolved |= !matches!(
                    which,
                    Known::Documents
                        | Known::SavedGames
                        | Known::Desktop
                        | Known::Downloads
                        | Known::Pictures
                        | Known::Music
                        | Known::Videos
                );
                continue;
            };
            // The Start menu folder is `Programs`; the one above it is the
            // Start menu itself.
            if matches!(which, Known::StartMenuMachine | Known::StartMenuUser)
                && let Some((parent, leaf)) = root.rsplit_once('\\')
                && leaf == "programs"
            {
                roots.push(parent.to_string());
            }
            roots.push(root);
        }
        // Folders that hold one folder per application.
        for (which, tail) in [
            (Known::ProgramFiles, "common files"),
            (Known::ProgramFiles, "windowsapps"),
            (Known::ProgramFilesX86, "common files"),
            (Known::LocalAppData, "packages"),
            (Known::ProgramData, "package cache"),
        ] {
            if let Some(root) = folder(which) {
                roots.push(format!(r"{root}\{tail}"));
            }
        }
        let windows = folder(Known::Windows);
        Places {
            roots,
            unresolved: unresolved || windows.is_none(),
            windows,
        }
    }

    /// Protected whatever else is offered alongside it.
    fn fixed(&self, location: &str, view: &dyn SystemView) -> bool {
        let lower = location.trim().to_lowercase();
        if let Some(task) = lower.strip_prefix("task:") {
            return task.trim_start_matches('\\').starts_with(r"microsoft\");
        }
        if is_system(&lower) {
            return false;
        }
        match parse_reg(location) {
            Some(reg) => key_protected(&reg),
            None => self.path_protected(location, view),
        }
    }

    fn path_protected(&self, path: &str, view: &dyn SystemView) -> bool {
        if self.unresolved || another_spelling(path) {
            return true;
        }
        let n = shortcut::normalise(path);
        let parts: Vec<&str> = n.split('\\').collect();
        let drive = parts[0];
        if parts.len() < 2 || !(drive.len() == 2 && drive.ends_with(':')) {
            return true;
        }
        if parts[1..]
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
        {
            return true;
        }
        if profile_shape(&parts[1..]) {
            return true;
        }
        if self.windows.as_deref().is_some_and(|w| related(&n, w)) {
            return true;
        }
        if self.roots.iter().any(|r| *r == n || strictly_inside(r, &n)) {
            return true;
        }
        // Through a junction or a symbolic link: somewhere else entirely.
        let original: Vec<&str> = path
            .trim()
            .trim_end_matches(['\\', '/'])
            .split(['\\', '/'])
            .collect();
        (2..=original.len()).any(|end| {
            view.reparse(Path::new(&original[..end].join("\\"))) == Reparse::NameSurrogate
        })
    }
}

/// `Software\Microsoft` and below, but for an application's own uninstall
/// entry, `App Paths` key and `Run` values; the `Software\Classes` root and
/// the shell's own classes; `Software\Policies`; `Software\WOW6432Node`
/// itself; and anything outside `Software`.
fn key_protected(reg: &Reg) -> bool {
    let parts: Vec<&str> = reg.parts.iter().map(String::as_str).collect();
    if parts.len() < 2
        || parts[0] != "software"
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return true;
    }
    let value = reg.value.is_some();
    match parts[1] {
        "wow6432node" | "policies" => true,
        "microsoft" => !matches!(
            (&parts[2..], value),
            (
                [
                    "windows",
                    "currentversion",
                    "uninstall" | "app paths",
                    _,
                    ..
                ],
                _
            ) | (["windows", "currentversion", "run" | "runonce"], true)
        ),
        "classes" => match &parts[2..] {
            [] | ["applications"] => true,
            ["applications", _, ..] => false,
            // `Wow6432Node` is the 32-bit view of every class at once.
            [first, ..] => *first == "wow6432node" || SHELL_CLASSES.contains(first),
        },
        _ => false,
    }
}

/// Whether a `Software\Classes\.<ext>` key still names a ProgID that is
/// there and is not going with it. `clearing` is what is offered alongside,
/// as [`normal`] makes it. A key that cannot be read is taken as in use.
fn extension_in_use(location: &str, view: &dyn SystemView, clearing: &HashSet<String>) -> bool {
    let Some(reg) = parse_reg(location) else {
        return false;
    };
    let in_extension = reg.parts.len() >= 3
        && reg.parts[0] == "software"
        && reg.parts[1] == "classes"
        && reg.parts[2].starts_with('.');
    // A value deeper in, such as one `OpenWithProgids` reference, is only
    // that reference.
    if !in_extension || (reg.value.is_some() && reg.parts.len() > 3) {
        return false;
    }
    let hive = hive_of(reg.hive);
    let path = format!(r"{CLASSES}\{}", reg.parts[2]);
    let mut progids = Vec::new();
    match view.values(hive, &path) {
        Ok(values) => progids.extend(
            values
                .into_iter()
                .filter(|(name, _)| name.is_empty())
                .map(|(_, data)| data),
        ),
        Err(ViewError::Missing(_)) => return false,
        Err(ViewError::Unreadable(_)) => return true,
    }
    match view.values(hive, &format!(r"{path}\OpenWithProgids")) {
        Ok(values) => progids.extend(values.into_iter().map(|(name, _)| name)),
        Err(ViewError::Missing(_)) => {}
        Err(ViewError::Unreadable(_)) => return true,
    }
    progids
        .iter()
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .any(|progid| {
            [Hive::Hkcu, Hive::Hklm].into_iter().any(|h| {
                let class = format!(r"{CLASSES}\{progid}");
                let there = !matches!(view.subkeys(h, &class), Err(ViewError::Missing(_)));
                let going = clearing.contains(&format!(
                    r"{}\software\classes\{progid}",
                    scan::hive_word(h).to_lowercase()
                ));
                there && !going
            })
        })
}

/// Whether a location is one that is never offered, whatever matched it:
/// the Windows directory, the roots applications are kept under, the
/// registry's shared keys, an extension another ProgID still answers for,
/// anything reached through a junction or a symbolic link.
pub fn is_protected(location: &str, view: &dyn SystemView) -> bool {
    Places::new(view).fixed(location, view) || extension_in_use(location, view, &HashSet::new())
}

/// Whether a candidate can hold others: a folder or a key, not a value.
fn holds(candidate: &Candidate) -> bool {
    !matches!(
        candidate.kind,
        ItemKind::RegistryValue
            | ItemKind::File
            | ItemKind::Service
            | ItemKind::Task
            | ItemKind::FirewallRule
    )
}

/// What may be offered, and what was held back with who uses it. Protected
/// places are in neither. What lies inside an offered folder or key goes
/// with it, so it is not offered again, unless the match on it is surer;
/// what holds something kept is kept.
pub fn guard(
    candidates: Vec<Candidate>,
    claims: &Claims,
    view: &dyn SystemView,
) -> (Vec<Candidate>, Vec<KeptItem>) {
    let places = Places::new(view);
    let mut kept: Vec<KeptItem> = Vec::new();
    let mut offered: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        if places.fixed(&candidate.location, view) {
            continue;
        }
        match claims.user_of(&candidate) {
            Some(used_by) => kept.push(KeptItem {
                location: candidate.location,
                used_by,
            }),
            None => offered.push(candidate),
        }
    }

    // Moving a folder moves what is in it, kept or not.
    let held: Vec<(String, String)> = kept
        .iter()
        .filter_map(|k| normal(&k.location).map(|n| (n, k.used_by.clone())))
        .collect();
    let mut still = Vec::new();
    for candidate in offered {
        let holder = holds(&candidate)
            .then(|| normal(&candidate.location))
            .flatten()
            .and_then(|at| {
                held.iter()
                    .find(|(k, _)| strictly_inside(k, &at))
                    .map(|(_, by)| by.clone())
            });
        match holder {
            Some(used_by) => kept.push(KeptItem {
                location: candidate.location,
                used_by,
            }),
            None => still.push(candidate),
        }
    }

    let clearing: HashSet<String> = still.iter().filter_map(|c| normal(&c.location)).collect();
    still.retain(|c| !extension_in_use(&c.location, view, &clearing));

    // What lies inside an offered folder or key goes with it, unless the
    // match on it is surer than on what holds it: a Likely item is never
    // hidden inside a Possible folder that is not ticked.
    let containers: Vec<(String, u8)> = still
        .iter()
        .filter(|c| holds(c))
        .filter_map(|c| normal(&c.location).map(|at| (at, scan::rank(c.confidence))))
        .collect();
    still.retain(|c| {
        let rank = scan::rank(c.confidence);
        normal(&c.location).is_none_or(|at| {
            !containers
                .iter()
                .any(|(dir, held)| strictly_inside(&at, dir) && rank >= *held)
        })
    });
    (still, kept)
}

/// The list the page shows: the guarded candidates with their ids, Certain
/// and Likely ticked, Possible and Your data not. An id is the item's place
/// in this list, which is fixed once the scan has finished.
pub fn findings(
    scan_id: String,
    hints: &Hints,
    scanned: Scanned,
    claims: &Claims,
    view: &dyn SystemView,
) -> Findings {
    let (offered, kept) = guard(scanned.candidates, claims, view);
    let items = offered
        .into_iter()
        .enumerate()
        .map(|(index, c)| LeftoverItem {
            id: format!("item-{index}"),
            kind: c.kind,
            location: c.location,
            size: c.size,
            ticked: matches!(c.confidence, Confidence::Certain | Confidence::Likely),
            confidence: c.confidence,
            scope: c.scope,
            reason: c.reason,
        })
        .collect();
    Findings {
        scan: scan_id,
        package: hints.package.clone(),
        name: hints.name.clone(),
        items,
        kept,
        not_offered_cloud: scanned.not_offered_cloud,
        unreadable: scanned.unreadable + claims.unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cleanup::scan::scan;
    use crate::cleanup::view::{FixtureView, Hive};
    use crate::model::{Confidence, ItemKind, PackageRef, Scope, SourceKind};
    use crate::sources::windows::arp;
    use std::path::{Path, PathBuf};

    const WRITER: &str = "{12345678-9ABC-DEF0-1234-56789ABCDEF0}";
    const WRITER_PACKED: &str = "87654321CBA90FED21436587A9CBED0F";
    const PF: &str = r"C:\Program Files";
    const X86: &str = r"C:\Program Files (x86)";
    const PD: &str = r"C:\ProgramData";
    const ROAMING: &str = r"C:\Users\you\AppData\Roaming";
    const LOCAL: &str = r"C:\Users\you\AppData\Local";
    const MENU: &str = r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs";
    const USER_MENU: &str = r"C:\Users\you\AppData\Roaming\Microsoft\Windows\Start Menu\Programs";
    const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";
    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    fn machine(name: &str) -> FixtureView {
        FixtureView::load(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/cleanup")
                .join(format!("{name}.json")),
        )
    }

    /// A machine with its well-known folders and nothing else installed.
    fn bare() -> FixtureView {
        FixtureView::from_json(
            r#"{ "folders": {
                "ProgramFiles": "C:\\Program Files",
                "ProgramFilesX86": "C:\\Program Files (x86)",
                "ProgramData": "C:\\ProgramData",
                "AppData": "C:\\Users\\you\\AppData\\Roaming",
                "LocalAppData": "C:\\Users\\you\\AppData\\Local",
                "LocalLow": "C:\\Users\\you\\AppData\\LocalLow",
                "Documents": "C:\\Users\\you\\Documents",
                "SavedGames": "C:\\Users\\you\\Saved Games",
                "StartMenuMachine": "C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs",
                "StartMenuUser": "C:\\Users\\you\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs",
                "Desktop": "C:\\Users\\you\\Desktop",
                "Windows": "C:\\Windows",
                "Profile": "C:\\Users\\you"
            } }"#,
        )
    }

    fn blank_hints(name: &str, publisher: Option<&str>) -> Hints {
        Hints {
            package: PackageRef {
                source: SourceKind::Arp,
                id: name.to_string(),
            },
            name: name.to_string(),
            publisher: publisher.map(str::to_string),
            uninstall_key: None,
            product_code: None,
            install_dir: None,
            install_location: None,
            icon_dir: None,
            shortcuts: Vec::new(),
            extra_dirs: Vec::new(),
        }
    }

    /// Acme Writer, an MSI product, as its hints stood before it was removed.
    fn writer() -> Hints {
        Hints {
            package: PackageRef {
                source: SourceKind::Arp,
                id: format!(r"HKLM\{WRITER}"),
            },
            uninstall_key: Some((Hive::Hklm, WRITER.to_string())),
            product_code: Some(WRITER.to_string()),
            install_location: Some(PathBuf::from(format!(r"{PF}\Acme\Acme Writer"))),
            shortcuts: vec![PathBuf::from(format!(r"{MENU}\Acme Writer.lnk"))],
            ..blank_hints("Acme Writer", Some("Acme"))
        }
    }

    fn package(source: SourceKind, id: &str, name: &str, developer: Option<&str>) -> Package {
        let mut p = Package::new(source, id, name);
        p.developer = developer.map(str::to_string);
        p.installed = true;
        p
    }

    /// What the store says is installed on `guard-acme`, Acme Writer
    /// included, as it is while it is being force-removed.
    fn installed() -> Vec<Package> {
        let mut writer = package(
            SourceKind::Winget,
            "Acme.Writer",
            "Acme Writer",
            Some("Acme"),
        );
        writer.facts.push((
            arp::REGISTRY_KEY_FACT.to_string(),
            format!(r"HKLM\{WRITER}"),
        ));
        vec![
            package(
                SourceKind::Arp,
                r"HKLM\AcmePaint",
                "Acme Paint",
                Some("Acme"),
            ),
            writer,
            package(
                SourceKind::Arp,
                r"HKLM\Python312",
                "Python 3.12.0 (64-bit)",
                Some("Python Software Foundation"),
            ),
            package(SourceKind::Scoop, "ripgrep", "ripgrep", None),
            package(SourceKind::Choco, "git", "Git", None),
            package(
                SourceKind::Msix,
                "Microsoft.WindowsCalculator_8wekyb3d8bbwe",
                "Windows Calculator",
                Some("Microsoft Corporation"),
            ),
        ]
    }

    fn candidate(
        kind: ItemKind,
        location: &str,
        confidence: Confidence,
        points_to: Option<&str>,
    ) -> Candidate {
        Candidate {
            kind,
            location: location.to_string(),
            size: None,
            confidence,
            scope: Scope::Machine,
            reason: "It matched.".to_string(),
            points_to: points_to.map(PathBuf::from),
        }
    }

    fn folder(location: &str, confidence: Confidence) -> Candidate {
        candidate(ItemKind::Folder, location, confidence, None)
    }

    fn key(location: &str, confidence: Confidence) -> Candidate {
        candidate(ItemKind::RegistryKey, location, confidence, None)
    }

    fn locations(list: &[Candidate]) -> Vec<String> {
        list.iter().map(|c| c.location.clone()).collect()
    }

    fn is_offered(list: &[Candidate], location: &str) -> bool {
        list.iter()
            .any(|c| c.location.eq_ignore_ascii_case(location))
    }

    fn used_by<'a>(kept: &'a [KeptItem], location: &str) -> Option<&'a str> {
        kept.iter()
            .find(|k| k.location.eq_ignore_ascii_case(location))
            .map(|k| k.used_by.as_str())
    }

    #[test]
    fn packed_product_code_is_the_windows_installer_transform() {
        // A pair Windows Installer documentation and tooling quote widely.
        assert_eq!(
            packed_product_code("{AC76BA86-7AD7-1033-7B44-AC0F074E4100}").as_deref(),
            Some("68AB67CA7DA73301B744CAF070E41400")
        );
        assert_eq!(packed_product_code(WRITER).as_deref(), Some(WRITER_PACKED));
        assert_eq!(
            packed_product_code(&WRITER.to_lowercase()).as_deref(),
            Some(WRITER_PACKED)
        );
        assert_eq!(packed_product_code("Steam"), None);
        assert_eq!(packed_product_code("{12345678-9ABC-DEF0-1234}"), None);
    }

    #[test]
    fn two_apps_from_one_publisher_keep_the_publisher_folder_and_key() {
        let view = machine("guard-acme");
        let hints = writer();
        let scanned = scan(&hints, &view);
        let publisher = [
            format!(r"{PF}\Acme"),
            format!(r"{ROAMING}\Acme"),
            r"HKCU\Software\Acme".to_string(),
        ];
        for location in &publisher {
            assert!(
                is_offered(&scanned.candidates, location),
                "the scan offers {location}"
            );
        }
        let (offered, kept) = guard(
            scanned.candidates,
            &claims(&view, &installed(), Some(&hints)),
            &view,
        );
        for location in &publisher {
            assert_eq!(used_by(&kept, location), Some("Acme Paint"), "{location}");
            assert!(!is_offered(&offered, location), "{location}");
        }
        // What is the application's own, under its publisher, still goes.
        for own in [
            format!(r"{PF}\Acme\Acme Writer"),
            format!(r"{ROAMING}\Acme\Acme Writer"),
            r"HKCU\Software\Acme\Acme Writer".to_string(),
            format!(r"{PD}\Acme Writer"),
        ] {
            assert!(
                is_offered(&offered, &own),
                "{own} in {:?}",
                locations(&offered)
            );
        }
        // Acme Paint's own folder was never the scan's to offer.
        assert!(!is_offered(
            &offered,
            &format!(r"{ROAMING}\Acme\Acme Paint")
        ));
    }

    #[test]
    fn a_publisher_folder_is_offered_when_nothing_else_of_theirs_is_installed() {
        let view = bare();
        let hints = writer();
        let candidates = vec![
            folder(&format!(r"{ROAMING}\Acme"), Confidence::Possible),
            key(r"HKCU\Software\Acme", Confidence::Possible),
        ];
        let (offered, kept) = guard(candidates, &claims(&view, &[], Some(&hints)), &view);
        assert_eq!(offered.len(), 2, "{kept:?}");
        assert!(kept.is_empty());
    }

    #[test]
    fn an_msi_component_shared_with_another_product_is_kept() {
        let view = machine("guard-acme");
        let candidates = vec![
            folder(&format!(r"{PF}\Common Files\Acme"), Confidence::Possible),
            key(r"HKLM\Software\Acme\Shared", Confidence::Likely),
            key(r"HKLM32\Software\Acme\Shared", Confidence::Likely),
            key(r"HKLM\Software\WOW6432Node\Acme\Shared", Confidence::Likely),
            // It holds what the other product claims.
            key(r"HKLM\Software\Acme", Confidence::Possible),
            // Only the product being removed registered anything in here.
            folder(&format!(r"{PD}\Acme Writer"), Confidence::Likely),
        ];
        let (offered, kept) = guard(
            candidates,
            &claims(&view, &installed(), Some(&writer())),
            &view,
        );
        for location in [
            format!(r"{PF}\Common Files\Acme"),
            r"HKLM\Software\Acme\Shared".to_string(),
            r"HKLM32\Software\Acme\Shared".to_string(),
            r"HKLM\Software\WOW6432Node\Acme\Shared".to_string(),
            r"HKLM\Software\Acme".to_string(),
        ] {
            assert_eq!(
                used_by(&kept, &location),
                Some("Acme Runtime"),
                "{location}"
            );
        }
        assert_eq!(locations(&offered), vec![format!(r"{PD}\Acme Writer")]);
    }

    fn writers_own() -> Vec<Candidate> {
        vec![
            folder(&format!(r"{PF}\Acme\Acme Writer"), Confidence::Certain),
            folder(&format!(r"{PD}\Acme Writer"), Confidence::Likely),
            key(&format!(r"HKLM\{UNINSTALL}\{WRITER}"), Confidence::Certain),
            candidate(
                ItemKind::Service,
                "service:AcmeWriterSvc",
                Confidence::Certain,
                Some(&format!(r"{PF}\Acme\Acme Writer\svc.exe")),
            ),
        ]
    }

    #[test]
    fn the_product_being_force_removed_does_not_shield_its_own_files() {
        let view = machine("guard-acme");
        let hints = writer();
        let (offered, kept) = guard(
            writers_own(),
            &claims(&view, &installed(), Some(&hints)),
            &view,
        );
        assert!(kept.is_empty(), "{kept:?}");
        assert_eq!(offered.len(), 4);

        // Without the exclusion, its own entry, components, shortcut and
        // service would shield every one of them.
        let (offered, kept) = guard(writers_own(), &claims(&view, &installed(), None), &view);
        assert!(offered.is_empty(), "{:?}", locations(&offered));
        for c in writers_own() {
            assert_eq!(
                used_by(&kept, &c.location),
                Some("Acme Writer"),
                "{}",
                c.location
            );
        }
    }

    #[test]
    fn kept_items_name_who_uses_them() {
        let view = machine("guard-acme");
        let claims = claims(&view, &installed(), Some(&writer()));
        let expected = [
            (format!(r"{X86}\Acme Paint"), "Acme Paint"),
            (r"C:\Python312".to_string(), "Python 3.12.0 (64-bit)"),
            (format!(r"{PD}\AcmeUpdate"), "AcmeUpdate (service)"),
            (format!(r"{PD}\AcmeCheck"), r"\Acme\Check (scheduled task)"),
            (format!(r"{PD}\Toolbox"), "Toolbox"),
            (format!(r"{PD}\AcmeLicences"), "Windows Installer"),
            (
                format!(r"{LOCAL}\Packages\Microsoft.WindowsCalculator_8wekyb3d8bbwe"),
                "Windows Calculator",
            ),
            (r"C:\Users\you\scoop\apps\ripgrep".to_string(), "ripgrep"),
            (format!(r"{PD}\chocolatey\lib\git"), "Git"),
        ];
        let candidates = expected
            .iter()
            .map(|(location, _)| folder(location, Confidence::Possible))
            .chain([
                // A shortcut whose target is gone claims nothing.
                folder(&format!(r"{PD}\Gone"), Confidence::Possible),
                // An entry whose `InstallLocation` is all of Program Files
                // names no one application's folder, so it claims none.
                folder(&format!(r"{PF}\Something"), Confidence::Possible),
            ])
            .collect();
        let (offered, kept) = guard(candidates, &claims, &view);
        for (location, by) in &expected {
            assert_eq!(used_by(&kept, location), Some(*by), "{location}");
        }
        assert_eq!(
            locations(&offered),
            vec![format!(r"{PD}\Gone"), format!(r"{PF}\Something")]
        );
    }

    #[test]
    fn a_name_another_installed_application_shares_is_kept() {
        let view = machine("guard-acme");
        let hints = blank_hints("Python 3.11.4 (64-bit)", Some("Python Software Foundation"));
        let scanned = scan(&hints, &view);
        let named = [
            format!(r"{ROAMING}\Python"),
            r"HKCU\Software\Python".to_string(),
        ];
        for location in &named {
            assert!(is_offered(&scanned.candidates, location), "{location}");
        }
        let (offered, kept) = guard(
            scanned.candidates,
            &claims(&view, &installed(), Some(&hints)),
            &view,
        );
        for location in &named {
            assert_eq!(
                used_by(&kept, location),
                Some("Python 3.12.0 (64-bit)"),
                "{location}"
            );
            assert!(!is_offered(&offered, location));
        }

        // Nothing else of that name installed: offered.
        let view = bare();
        let candidates = named
            .iter()
            .map(|l| folder(l, Confidence::Likely))
            .collect();
        let (offered, kept) = guard(candidates, &claims(&view, &[], Some(&hints)), &view);
        assert_eq!(offered.len(), 2);
        assert!(kept.is_empty());
    }

    #[test]
    fn what_points_into_another_applications_folder_is_kept() {
        let view = machine("guard-acme");
        let candidates = vec![
            candidate(
                ItemKind::RegistryValue,
                &format!(r"HKCU\{RUN} | Paint"),
                Confidence::Certain,
                Some(&format!(r"{X86}\Acme Paint\paint.exe")),
            ),
            candidate(
                ItemKind::RegistryValue,
                &format!(r"HKCU\{RUN} | Writer"),
                Confidence::Certain,
                Some(&format!(r"{PF}\Acme\Acme Writer\writer.exe")),
            ),
        ];
        let (offered, kept) = guard(
            candidates,
            &claims(&view, &installed(), Some(&writer())),
            &view,
        );
        assert_eq!(
            used_by(&kept, &format!(r"HKCU\{RUN} | Paint")),
            Some("Acme Paint")
        );
        assert_eq!(locations(&offered), vec![format!(r"HKCU\{RUN} | Writer")]);
    }

    #[test]
    fn a_steam_game_whose_home_is_all_of_steam_offers_nothing_of_steam() {
        let view = machine("guard-steam");
        // No `InstallLocation`, so the game's folder fell back to the
        // uninstaller's, which is Steam's own.
        let hints = Hints {
            uninstall_key: Some((Hive::Hklm32, "Steam App 377160".to_string())),
            install_dir: Some(PathBuf::from(format!(r"{X86}\Steam"))),
            ..blank_hints("Fallout 4", Some("Bethesda Game Studios"))
        };
        let scanned = scan(&hints, &view);
        let steam = format!(r"{X86}\Steam");
        assert!(is_offered(&scanned.candidates, &steam));
        let (offered, kept) = guard(scanned.candidates, &claims(&view, &[], Some(&hints)), &view);
        for location in [
            steam.clone(),
            format!(r"HKCU\{RUN} | Steam"),
            "firewall:Steam".to_string(),
        ] {
            assert_eq!(used_by(&kept, &location), Some("Steam"), "{location}");
        }
        assert!(
            offered
                .iter()
                .all(|c| !c.location.to_lowercase().contains("steam") && c.points_to.is_none()),
            "{:?}",
            locations(&offered)
        );
        // What is the game's alone is still offered.
        assert!(is_offered(&offered, &format!(r"{LOCAL}\Fallout4")));
        assert!(is_offered(
            &offered,
            r"C:\Users\you\Documents\My Games\Fallout4"
        ));
    }

    #[test]
    fn what_lies_inside_an_offered_folder_or_key_goes_with_it() {
        let view = bare();
        let candidates = vec![
            folder(&format!(r"{PD}\Acme Writer\cache"), Confidence::Likely),
            folder(&format!(r"{PD}\Acme Writer"), Confidence::Likely),
            // Certain, so that the Certain value inside it is covered too.
            key(r"HKCU\Software\Acme Writer", Confidence::Certain),
            key(r"HKCU\Software\Acme Writer\Prefs", Confidence::Likely),
            candidate(
                ItemKind::RegistryValue,
                r"HKCU\Software\Acme Writer | Last",
                Confidence::Certain,
                Some(r"C:\Gone\writer.exe"),
            ),
            // Only a name in common: not inside.
            folder(&format!(r"{PD}\Acme Writer Extras"), Confidence::Likely),
        ];
        let (offered, kept) = guard(candidates, &claims(&view, &[], Some(&writer())), &view);
        assert_eq!(
            locations(&offered),
            vec![
                format!(r"{PD}\Acme Writer"),
                r"HKCU\Software\Acme Writer".to_string(),
                format!(r"{PD}\Acme Writer Extras"),
            ]
        );
        assert!(kept.is_empty());
    }

    #[test]
    fn a_folder_holding_something_kept_is_kept_too() {
        let view = bare();
        let python = vec![package(
            SourceKind::Arp,
            r"HKLM\Python312",
            "Python 3.12.0 (64-bit)",
            None,
        )];
        let candidates = vec![
            key(r"HKCU\Software\Vendor", Confidence::Possible),
            key(r"HKCU\Software\Vendor\Python", Confidence::Likely),
        ];
        let hints = blank_hints("Python 3.11", None);
        let (offered, kept) = guard(candidates, &claims(&view, &python, Some(&hints)), &view);
        assert!(offered.is_empty());
        assert_eq!(
            used_by(&kept, r"HKCU\Software\Vendor"),
            Some("Python 3.12.0 (64-bit)")
        );
    }

    #[test]
    fn an_extension_goes_only_with_the_progid_it_names() {
        let view = machine("guard-acme");
        let claims = claims(&view, &installed(), Some(&writer()));
        let exe = format!(r"{PF}\Acme\Acme Writer\writer.exe");
        let progid = candidate(
            ItemKind::RegistryKey,
            r"HKCU\Software\Classes\AcmeWriter.Doc",
            Confidence::Certain,
            Some(&exe),
        );
        let acw = candidate(
            ItemKind::RegistryKey,
            r"HKCU\Software\Classes\.acw",
            Confidence::Certain,
            Some(&exe),
        );
        let txt = candidate(
            ItemKind::RegistryKey,
            r"HKCU\Software\Classes\.txt",
            Confidence::Certain,
            Some(&exe),
        );
        let (offered, kept) = guard(vec![progid, acw.clone(), txt], &claims, &view);
        assert_eq!(
            locations(&offered),
            vec![
                r"HKCU\Software\Classes\AcmeWriter.Doc".to_string(),
                r"HKCU\Software\Classes\.acw".to_string(),
            ]
        );
        assert!(kept.is_empty(), "a protected key is not even kept");
        // Without its ProgID going too, the extension still names one.
        let (offered, kept) = guard(vec![acw], &claims, &view);
        assert!(offered.is_empty() && kept.is_empty());
    }

    #[test]
    fn protected_locations_are_never_offered() {
        let view = machine("guard-acme");
        let p = |l: &str| is_protected(l, &view);
        // The Windows directory and everything in it.
        assert!(p(r"C:\Windows"));
        assert!(p(r"C:\Windows\System32\acme.dll"));
        // The roots, and anything that holds one.
        assert!(p(PF));
        assert!(p(X86));
        assert!(p(PD));
        assert!(p(ROAMING));
        assert!(p(LOCAL));
        assert!(p(r"C:\Users\you\AppData\LocalLow"));
        assert!(p(r"C:\Users\you\AppData"));
        assert!(p(r"C:\Users\you"));
        assert!(p(r"C:\Users"));
        assert!(p(r"C:\"));
        assert!(p(MENU));
        assert!(p(r"C:\ProgramData\Microsoft\Windows\Start Menu"));
        assert!(p(USER_MENU));
        assert!(p(
            r"C:\Users\you\AppData\Roaming\Microsoft\Windows\Start Menu"
        ));
        // Anything that is not a plain absolute path, or goes through a link.
        assert!(p("Acme"));
        assert!(p(r"\\server\share\Acme"));
        assert!(p(r"C:\Acme\..\Windows"));
        assert!(p(r"C:\ProgramData\Linked\Acme"));
        // Other spellings of a protected place: 8.3 short names, NTFS
        // streams, a name ending in a dot or a space.
        assert!(p(r"C:\PROGRA~1"));
        assert!(p(r"C:\PROGRA~2"));
        assert!(p(r"C:\PROGRA~3"));
        assert!(p(r"C:\PROGRA~1\Acme"));
        assert!(p(r"C:\Users\you\APPDATA~1\Acme"));
        assert!(p(r"C:\Program Files::$INDEX_ALLOCATION"));
        assert!(p(r"C:\Program Files\Acme::$DATA"));
        assert!(p(r"C:\Program Files."));
        assert!(p(r"C:\Program Files "));
        assert!(p(r"C:\Program Files.\Acme"));
        assert!(p(r"C:\Program Files \Acme"));
        // The shapes of any profile, whatever the well-known folders say.
        assert!(p(r"D:\Users"));
        assert!(p(r"D:\Users\other"));
        assert!(p(r"D:\Users\other\AppData"));
        assert!(p(r"D:\Users\other\AppData\Local"));
        assert!(p(r"D:\Users\other\AppData\Roaming"));
        assert!(p(r"D:\Users\other\AppData\LocalLow"));
        assert!(p(r"C:\Users\Public"));
        assert!(p(r"C:\Users\Default"));
        assert!(p(r"HKCU\Software\WOW6432Node\Acme"));
        assert!(p(r"HKLM\Software\Classes\Wow6432Node"));
        assert!(p(r"HKLM\Software\Classes\Wow6432Node\CLSID\{0}"));
        assert!(p(r"HKLM\Software\WOW6432Node\Classes\Wow6432Node\Acme"));
        // The registry outside an application's own keys.
        assert!(p("HKLM"));
        assert!(p(r"HKLM\Software"));
        assert!(p(r"HKLM32\Software"));
        assert!(p(r"HKLM\System\CurrentControlSet\Services\Acme"));
        assert!(p(r"HKLM\Software\Microsoft"));
        assert!(p(r"HKCU\Software\Microsoft\Office\Acme"));
        assert!(p(&format!(r"HKLM\{UNINSTALL}")));
        assert!(p(&format!(r"HKCU\{RUN}")));
        assert!(p(
            r"HKLM\Software\Microsoft\Windows\CurrentVersion\Explorer | Acme"
        ));
        assert!(p(r"HKLM\Software\Policies\Acme"));
        assert!(p(r"HKLM\Software\WOW6432Node"));
        assert!(p(r"HKCU\Software\Classes"));
        assert!(p(r"HKCU\Software\Classes\CLSID\{0}"));
        // An extension another ProgID still answers for.
        assert!(p(r"HKCU\Software\Classes\.txt"));
        assert!(p(r"HKU\S-1-5-18\Software\Acme"));
        assert!(p(r"task:\Microsoft\Windows\Defrag"));

        // An application's own entries under `Software\Microsoft` are not.
        for open in [
            format!(r"HKLM\{UNINSTALL}\Acme"),
            format!(r"HKLM\Software\WOW6432Node\{UNINSTALL}\Acme"),
            format!(r"HKLM32\{UNINSTALL}\Acme"),
            r"HKLM\Software\Microsoft\Windows\CurrentVersion\App Paths\acme.exe".to_string(),
            format!(r"HKCU\{RUN} | Acme"),
            format!(r"HKCU\{RUN}Once | Acme"),
            r"HKCU\Software\Classes\Applications\acme.exe".to_string(),
            r"HKCU\Software\Acme".to_string(),
            format!(r"{PF}\Acme"),
            format!(r"{ROAMING}\Acme Writer"),
            format!(r"{MENU}\Acme"),
            r"D:\Users\other\AppData\Local\Acme".to_string(),
            r"C:\Users\Public\Desktop\Acme.lnk".to_string(),
            r"C:\Program Files\Acme 1.0".to_string(),
            r"C:\Program Files\Acme~Tools".to_string(),
            "service:AcmeSvc".to_string(),
            r"task:\Acme\Update".to_string(),
            "firewall:Acme".to_string(),
        ] {
            assert!(!p(&open), "{open}");
        }

        // The guard drops them without counting them as kept.
        let candidates = vec![
            folder(PF, Confidence::Possible),
            folder(USER_MENU, Confidence::Certain),
            key(r"HKLM\Software\Microsoft", Confidence::Likely),
            key(r"HKLM\Software\WOW6432Node", Confidence::Likely),
        ];
        let (offered, kept) = guard(candidates, &Claims::default(), &view);
        assert!(offered.is_empty() && kept.is_empty());
    }

    #[test]
    fn findings_tick_certain_and_likely_only() {
        let view = bare();
        let hints = writer();
        let scanned = Scanned {
            candidates: vec![
                folder(&format!(r"{PD}\Acme Writer"), Confidence::Certain),
                folder(&format!(r"{LOCAL}\Acme Writer"), Confidence::Likely),
                folder(&format!(r"{ROAMING}\Acme Writer"), Confidence::Possible),
                folder(r"C:\Users\you\Documents\Acme Writer", Confidence::YourData),
            ],
            not_offered_cloud: 2,
            unreadable: 3,
        };
        let f = findings(
            "scan-1".to_string(),
            &hints,
            scanned,
            &claims(&view, &[], Some(&hints)),
            &view,
        );
        assert_eq!(f.scan, "scan-1");
        assert_eq!(f.package, hints.package);
        assert_eq!(f.name, "Acme Writer");
        let ticks: Vec<(Confidence, bool)> =
            f.items.iter().map(|i| (i.confidence, i.ticked)).collect();
        assert_eq!(
            ticks,
            vec![
                (Confidence::Certain, true),
                (Confidence::Likely, true),
                (Confidence::Possible, false),
                (Confidence::YourData, false),
            ]
        );
        let mut ids: Vec<&str> = f.items.iter().map(|i| i.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 4, "every item has its own id");
        assert_eq!(f.not_offered_cloud, 2);
        assert_eq!(f.unreadable, 3);
        assert!(f.kept.is_empty());
    }

    #[test]
    fn a_short_name_for_program_files_is_never_a_home_nor_offered() {
        // `UninstallString = C:\PROGRA~1\uninst.exe` derives `C:\PROGRA~1`,
        // which `shortcut::specific` would take for one application's folder.
        let view = FixtureView::from_json(
            r#"{ "folders": {
                "ProgramFiles": "C:\\Program Files",
                "ProgramFilesX86": "C:\\Program Files (x86)",
                "ProgramData": "C:\\ProgramData",
                "AppData": "C:\\Users\\you\\AppData\\Roaming",
                "LocalAppData": "C:\\Users\\you\\AppData\\Local",
                "LocalLow": "C:\\Users\\you\\AppData\\LocalLow",
                "StartMenuMachine": "C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs",
                "StartMenuUser": "C:\\Users\\you\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs",
                "Windows": "C:\\Windows",
                "Profile": "C:\\Users\\you"
              },
              "dirs": {
                "C:\\PROGRA~1": [{ "name": "uninst.exe", "size": 10 }, { "name": "Other", "is_dir": true }],
                "C:\\PROGRA~1\\Other": [{ "name": "other.exe", "size": 10 }]
              },
              "registry": { "Hkcu": {
                "Software\\Microsoft\\Windows\\CurrentVersion\\Run": {
                  "values": [["Other", "C:\\PROGRA~1\\Other\\other.exe"]]
                }
              } }
            }"#,
        );
        let hints = Hints {
            install_dir: Some(PathBuf::from(r"C:\PROGRA~1")),
            ..blank_hints("Shortname Tool", None)
        };
        let scanned = scan(&hints, &view);
        // Not a home, so nothing is matched for pointing into it either.
        assert!(
            scanned.candidates.is_empty(),
            "{:?}",
            locations(&scanned.candidates)
        );

        // And should anything offer it, the guard does not.
        let candidates = vec![
            folder(r"C:\PROGRA~1", Confidence::Certain),
            folder(r"C:\Program Files::$INDEX_ALLOCATION", Confidence::Certain),
        ];
        let (offered, kept) = guard(candidates, &claims(&view, &[], Some(&hints)), &view);
        assert!(offered.is_empty() && kept.is_empty());
    }

    #[test]
    fn nothing_is_offered_when_a_protected_folder_does_not_resolve() {
        // No Windows directory and no LocalLow: what they protect cannot be
        // told, so no path is offered at all.
        let view = FixtureView::from_json(
            r#"{ "folders": {
                "ProgramFiles": "C:\\Program Files",
                "ProgramFilesX86": "C:\\Program Files (x86)",
                "ProgramData": "C:\\ProgramData",
                "AppData": "C:\\Users\\you\\AppData\\Roaming",
                "LocalAppData": "C:\\Users\\you\\AppData\\Local",
                "StartMenuMachine": "C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs",
                "StartMenuUser": "C:\\Users\\you\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs",
                "Profile": "C:\\Users\\you"
            } }"#,
        );
        assert!(is_protected(r"C:\Program Files\Acme", &view));
        assert!(is_protected(r"C:\Windows\System32", &view));
        // Registry keys are judged by their own names and still may be.
        assert!(!is_protected(r"HKCU\Software\Acme", &view));
        // With every folder known, the same path may be offered.
        assert!(!is_protected(r"C:\Program Files\Acme", &bare()));
    }

    #[test]
    fn a_surer_match_inside_a_less_sure_folder_stays_on_the_list() {
        let view = bare();
        let candidates = vec![
            folder(&format!(r"{ROAMING}\Acme"), Confidence::Possible),
            folder(&format!(r"{ROAMING}\Acme\Acme Writer"), Confidence::Likely),
            folder(&format!(r"{LOCAL}\Acme Writer"), Confidence::Likely),
            folder(&format!(r"{LOCAL}\Acme Writer\logs"), Confidence::Possible),
            folder(&format!(r"{LOCAL}\Acme Writer\cache"), Confidence::Likely),
            folder(&format!(r"{PD}\Acme Writer"), Confidence::YourData),
            folder(&format!(r"{PD}\Acme Writer\data"), Confidence::Possible),
        ];
        let (offered, kept) = guard(candidates, &claims(&view, &[], Some(&writer())), &view);
        assert_eq!(
            locations(&offered),
            vec![
                format!(r"{ROAMING}\Acme"),
                // Likely is ticked and its Possible parent is not: it is
                // never hidden inside it.
                format!(r"{ROAMING}\Acme\Acme Writer"),
                format!(r"{LOCAL}\Acme Writer"),
                format!(r"{PD}\Acme Writer"),
                // Possible is surer than Your data.
                format!(r"{PD}\Acme Writer\data"),
            ]
        );
        assert!(kept.is_empty());
    }

    #[test]
    fn only_the_machine_hive_has_a_32_bit_view_to_fold() {
        assert_eq!(
            normal(r"HKLM\Software\WOW6432Node\Acme"),
            normal(r"HKLM32\Software\Acme")
        );
        assert_ne!(
            normal(r"HKCU\Software\WOW6432Node\Acme"),
            normal(r"HKCU\Software\Acme")
        );
    }
}
