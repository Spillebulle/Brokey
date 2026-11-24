//! The scan: what an uninstaller left behind, found where applications keep
//! things (spec §3) and ranked by how sure the match is (spec §4).
//!
//! A pure function of the hints and a read-only [`SystemView`]. Nothing here
//! writes, and the guard decides afterwards which of these may be offered.
//! The scan's own care is in what it calls a match at all:
//!
//! - The install folder, and whatever points into it, is `Certain`, but only
//!   for a folder narrow enough to name one application: `shortcut::specific`,
//!   and no well-known folder at or below it. An install folder of
//!   `C:\Program Files` or the Downloads folder is no install folder, and
//!   nothing is matched for pointing into it.
//! - When the uninstaller's folder and `InstallLocation` disagree,
//!   `InstallLocation` wins, unless it is the broad one. A Steam game's
//!   uninstaller is `steam.exe`, and its folder is all of Steam.
//! - A name matches only through the winget normaliser, only at four
//!   characters or more, and never a word on the stop list.
//! - A junction or symbolic link anywhere on a path is never offered and
//!   never followed. A match inside a folder a cloud provider syncs is
//!   counted and not offered.
//! - What could not be read is counted, so the page can say so.
//!
//! Paths are Windows paths held as text, so the scan behaves the same on the
//! Linux machine most of the tests run on.

use super::hints::Hints;
use super::view::{Entry, Hive, Known, Reparse, SystemView, ViewError};
use crate::model::{Confidence, ItemKind, Scope};
use crate::sources::windows::shortcut::{self, inside};
use crate::sources::windows::winget::normalise;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// One thing that looks like a leftover, before the guard has seen it.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub kind: ItemKind,
    /// A full path for a file, folder or shortcut; `HKLM\…`, `HKLM32\…` or
    /// `HKCU\…` for a key, with ` | <value name>` after it for a value;
    /// `service:<name>`, `task:<path>` or `firewall:<rule name>`.
    pub location: String,
    pub size: Option<u64>,
    pub confidence: Confidence,
    pub scope: Scope,
    /// One sentence on why it matched.
    pub reason: String,
}

/// What a scan found, and what it could not or would not offer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scanned {
    pub candidates: Vec<Candidate>,
    /// Matches inside a folder a cloud provider syncs, left out.
    pub not_offered_cloud: u32,
    /// Places the scan could not read.
    pub unreadable: u32,
}

/// Normalised names that never match, as an application's or a publisher's.
const STOP_LIST: [&str; 11] = [
    "common",
    "shared",
    "data",
    "update",
    "temp",
    "microsoft",
    "windows",
    "intel",
    "nvidia",
    "amd",
    "google",
];

/// A normalised name shorter than this matches too much to mean anything.
const SHORTEST_NAME: usize = 4;

const SOFTWARE: &str = "Software";
const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";
const APP_PATHS: &str = r"Software\Microsoft\Windows\CurrentVersion\App Paths";
const RUN_KEYS: [&str; 2] = [
    r"Software\Microsoft\Windows\CurrentVersion\Run",
    r"Software\Microsoft\Windows\CurrentVersion\RunOnce",
];
const CLASSES: &str = r"Software\Classes";

/// Keys directly under `Software` that are never a vendor's, whatever the
/// application is called.
const NOT_A_VENDOR: [&str; 4] = ["classes", "microsoft", "policies", "wow6432node"];

/// Keys under `Software\Classes` that Windows itself owns. An application
/// that set itself as the handler of one of these leaves its command there,
/// and the key is still the protocol's or the shell's after it is gone.
const SHELL_CLASSES: [&str; 20] = [
    "*",
    "allfilesystemobjects",
    "appid",
    "applications",
    "clsid",
    "desktopbackground",
    "directory",
    "drive",
    "exefile",
    "file",
    "folder",
    "ftp",
    "http",
    "https",
    "interface",
    "lnkfile",
    "mailto",
    "systemfileassociations",
    "typelib",
    "unknown",
];

/// How many folders deep a Start menu is searched below `Programs`.
const MENU_DEPTH: u8 = 3;

/// What a scan over one machine looks for.
pub fn scan(hints: &Hints, view: &dyn SystemView) -> Scanned {
    let mut scan = Scan::new(hints, view);
    for home in scan.homes.clone() {
        scan.home(&home);
    }
    scan.by_name(Known::ProgramFiles, false);
    scan.by_name(Known::ProgramFilesX86, false);
    for known in [
        Known::AppData,
        Known::LocalAppData,
        Known::LocalLow,
        Known::ProgramData,
    ] {
        scan.by_name(known, true);
    }
    scan.start_menus();
    scan.desktop();
    scan.your_data();
    scan.registry();
    scan.system();
    scan.out
}

/// Whether a path lies on the far side of a reparse point.
enum Reach {
    Plain,
    Cloud,
    /// Through a junction or a symbolic link: somewhere else entirely.
    Away,
}

/// A Start menu entry worth offering.
enum MenuPart {
    Folder(String),
    Link(String, u64),
}

struct Scan<'a> {
    hints: &'a Hints,
    view: &'a dyn SystemView,
    /// The application's normalised name, when it is specific enough.
    name: Option<String>,
    /// The publisher's normalised name, when it is specific enough.
    publisher: Option<String>,
    /// The application's own folders. Whatever points into one is its.
    homes: Vec<String>,
    /// Every well-known folder: none of them, nor anything holding one, is
    /// ever offered.
    known: Vec<String>,
    /// The folders whose contents are the user's rather than the machine's.
    user_roots: Vec<String>,
    /// The folders whose contents are the user's own files.
    data_roots: Vec<String>,
    windows: Option<String>,
    /// `%NAME%` to what it stands for, from the well-known folders.
    env: HashMap<String, String>,
    listed: HashMap<String, Option<Vec<Entry>>>,
    seen: HashMap<String, usize>,
    clouded: HashSet<String>,
    out: Scanned,
}

fn usable(normalised: String) -> Option<String> {
    (normalised.chars().count() >= SHORTEST_NAME && !STOP_LIST.contains(&normalised.as_str()))
        .then_some(normalised)
}

fn same(a: &str, b: &str) -> bool {
    shortcut::normalise(a) == shortcut::normalise(b)
}

/// `path` is `dir` or lies inside it.
fn within(path: &str, dir: &str) -> bool {
    same(path, dir) || inside(path, dir)
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with(['\\', '/']) {
        format!("{dir}{name}")
    } else {
        format!(r"{dir}\{name}")
    }
}

fn parent_and_leaf(path: &str) -> Option<(String, String)> {
    let trimmed = path.trim_end_matches(['\\', '/']);
    let (parent, leaf) = trimmed.rsplit_once(['\\', '/'])?;
    let parent = if parent.ends_with(':') {
        format!(r"{parent}\")
    } else {
        parent.to_string()
    };
    Some((parent, leaf.to_string()))
}

fn hive_word(hive: Hive) -> &'static str {
    match hive {
        Hive::Hklm => "HKLM",
        Hive::Hklm32 => "HKLM32",
        Hive::Hkcu => "HKCU",
    }
}

fn rank(confidence: Confidence) -> u8 {
    match confidence {
        Confidence::Certain => 0,
        Confidence::Likely => 1,
        Confidence::Possible => 2,
        Confidence::YourData => 3,
    }
}

fn text_of(path: &Option<std::path::PathBuf>) -> Option<String> {
    path.as_ref()
        .map(|p| p.to_string_lossy().trim().to_string())
        .filter(|p| !p.is_empty())
}

impl<'a> Scan<'a> {
    fn new(hints: &'a Hints, view: &'a dyn SystemView) -> Scan<'a> {
        let folder = |k: Known| {
            view.known_folder(k)
                .map(|p| p.to_string_lossy().into_owned())
        };
        let of = |ks: &[Known]| ks.iter().filter_map(|&k| folder(k)).collect::<Vec<_>>();
        let known = of(&[
            Known::ProgramFiles,
            Known::ProgramFilesX86,
            Known::ProgramData,
            Known::AppData,
            Known::LocalAppData,
            Known::LocalLow,
            Known::Documents,
            Known::SavedGames,
            Known::StartMenuMachine,
            Known::StartMenuUser,
            Known::Desktop,
            Known::Windows,
            Known::Profile,
        ]);
        let user_roots = of(&[
            Known::Profile,
            Known::AppData,
            Known::LocalAppData,
            Known::LocalLow,
            Known::Documents,
            Known::SavedGames,
            Known::StartMenuUser,
            Known::Desktop,
        ]);
        let data_roots = of(&[Known::Documents, Known::SavedGames]);
        let env = [
            ("LOCALAPPDATA", Known::LocalAppData),
            ("APPDATA", Known::AppData),
            ("USERPROFILE", Known::Profile),
            ("PROGRAMFILES", Known::ProgramFiles),
            ("PROGRAMW6432", Known::ProgramFiles),
            ("PROGRAMFILES(X86)", Known::ProgramFilesX86),
            ("PROGRAMDATA", Known::ProgramData),
            ("ALLUSERSPROFILE", Known::ProgramData),
            ("SYSTEMROOT", Known::Windows),
            ("WINDIR", Known::Windows),
        ]
        .into_iter()
        .filter_map(|(name, k)| folder(k).map(|f| (name.to_string(), f)))
        .collect();
        let mut scan = Scan {
            hints,
            view,
            name: usable(normalise::name(&hints.name)),
            publisher: hints
                .publisher
                .as_deref()
                .map(normalise::publisher)
                .and_then(usable),
            homes: Vec::new(),
            known,
            user_roots,
            data_roots,
            windows: folder(Known::Windows),
            env,
            listed: HashMap::new(),
            seen: HashMap::new(),
            clouded: HashSet::new(),
            out: Scanned::default(),
        };
        scan.homes = scan.find_homes();
        scan
    }

    /// The application's own folders, narrow ones only.
    fn find_homes(&self) -> Vec<String> {
        let from_uninstaller = text_of(&self.hints.install_dir);
        let location = text_of(&self.hints.install_location);
        let chosen = match (from_uninstaller, location) {
            // Chrome's uninstaller is below its `InstallLocation`; a broad
            // `InstallLocation` above a narrow uninstaller folder says nothing.
            (Some(dir), Some(location)) if inside(&dir, &location) && !self.narrow(&location) => {
                Some(dir)
            }
            // The same folder; one inside the other, the uninstaller above
            // the application (a Steam game); or two places that disagree,
            // where the uninstaller's folder may be anyone's.
            (Some(_), Some(location)) => Some(location),
            (dir, location) => dir.or(location),
        };
        let mut homes: Vec<String> = Vec::new();
        let extra = self
            .hints
            .extra_dirs
            .iter()
            .map(|d| d.to_string_lossy().into_owned());
        for home in chosen.into_iter().chain(extra) {
            if self.narrow(&home) && !homes.iter().any(|h| same(h, &home)) {
                homes.push(home);
            }
        }
        homes
    }

    /// Whether a folder is narrow enough to be one application's.
    fn narrow(&self, dir: &str) -> bool {
        shortcut::specific(dir)
            && !self.holds_known(dir)
            && !self.windows.as_deref().is_some_and(|w| within(dir, w))
    }

    /// Whether a path is, or holds, a well-known folder.
    fn holds_known(&self, path: &str) -> bool {
        self.known.iter().any(|k| within(k, path))
    }

    fn folder(&self, which: Known) -> Option<String> {
        self.view
            .known_folder(which)
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn is_name(&self, text: &str) -> bool {
        self.name
            .as_deref()
            .is_some_and(|n| normalise::name(text) == n)
    }

    fn is_publisher(&self, text: &str) -> bool {
        self.publisher
            .as_deref()
            .is_some_and(|p| normalise::publisher(text) == p)
    }

    fn app(&self) -> &'a str {
        &self.hints.name
    }

    fn publisher_name(&self) -> &'a str {
        self.hints.publisher.as_deref().unwrap_or_default().trim()
    }

    fn count(&mut self, error: &ViewError) {
        if matches!(error, ViewError::Unreadable(_)) {
            self.out.unreadable += 1;
        }
    }

    /// A folder's entries, read once however often they are asked for.
    fn list(&mut self, dir: &str) -> Option<Vec<Entry>> {
        let key = shortcut::normalise(dir);
        if let Some(found) = self.listed.get(&key) {
            return found.clone();
        }
        let found = match self.view.list(Path::new(dir)) {
            Ok(listed) => {
                self.out.unreadable += listed.unreadable;
                Some(listed.items)
            }
            Err(e) => {
                self.count(&e);
                None
            }
        };
        self.listed.insert(key, found.clone());
        found
    }

    fn subkeys(&mut self, hive: Hive, path: &str) -> Option<Vec<String>> {
        match self.view.subkeys(hive, path) {
            Ok(keys) => Some(keys),
            Err(e) => {
                self.count(&e);
                None
            }
        }
    }

    fn values(&mut self, hive: Hive, path: &str) -> Option<Vec<(String, String)>> {
        match self.view.values(hive, path) {
            Ok(values) => Some(values),
            Err(e) => {
                self.count(&e);
                None
            }
        }
    }

    /// Whether anything on the way to `path`, or `path` itself, is a
    /// reparse point.
    fn reach(&self, path: &str) -> Reach {
        let clean = path.replace('/', "\\");
        let parts: Vec<&str> = clean.trim_end_matches('\\').split('\\').collect();
        let mut cloud = false;
        for end in 2..=parts.len() {
            match self.view.reparse(Path::new(&parts[..end].join("\\"))) {
                Reparse::NameSurrogate => return Reach::Away,
                Reparse::Cloud => cloud = true,
                Reparse::None => {}
            }
        }
        if cloud { Reach::Cloud } else { Reach::Plain }
    }

    /// Whether a command line, or a path, starts something in a home.
    fn points_into(&self, command: &str) -> bool {
        if self.homes.is_empty() {
            return false;
        }
        let command = command.trim();
        let expanded =
            shortcut::expand_with(command, &|name| self.env.get(&name.to_uppercase()).cloned())
                .unwrap_or_else(|| command.to_string());
        let path = expanded.trim_start_matches('"');
        let path = path.strip_prefix(r"\??\").unwrap_or(path);
        self.homes.iter().any(|home| inside(path, home))
    }

    fn push(&mut self, candidate: Candidate) {
        let key = candidate.location.replace('/', "\\").to_lowercase();
        match self.seen.get(&key) {
            Some(&at) => {
                if rank(candidate.confidence) < rank(self.out.candidates[at].confidence) {
                    self.out.candidates[at] = candidate;
                }
            }
            None => {
                self.seen.insert(key, self.out.candidates.len());
                self.out.candidates.push(candidate);
            }
        }
    }

    /// Offer a file, folder or shortcut, if it may be offered at all.
    /// `file_size` is `None` for a folder, which must then be readable.
    fn offer_path(
        &mut self,
        path: &str,
        kind: ItemKind,
        file_size: Option<u64>,
        confidence: Confidence,
        reason: String,
        must_be_narrow: bool,
    ) {
        if self.holds_known(path) || (must_be_narrow && !self.narrow(path)) {
            return;
        }
        match self.reach(path) {
            Reach::Away => return,
            Reach::Cloud => {
                if self.clouded.insert(shortcut::normalise(path)) {
                    self.out.not_offered_cloud += 1;
                }
                return;
            }
            Reach::Plain => {}
        }
        let size = match file_size {
            Some(size) => size,
            None => {
                if self.list(path).is_none() {
                    return;
                }
                self.view.dir_size(Path::new(path))
            }
        };
        let confidence = if self.data_roots.iter().any(|r| inside(path, r)) {
            Confidence::YourData
        } else {
            confidence
        };
        let scope = if self.user_roots.iter().any(|r| within(path, r)) {
            Scope::User
        } else {
            Scope::Machine
        };
        self.push(Candidate {
            kind,
            location: path.to_string(),
            size: Some(size),
            confidence,
            scope,
            reason,
        });
    }

    fn offer_key(&mut self, hive: Hive, path: &str, confidence: Confidence, reason: String) {
        self.push(Candidate {
            kind: ItemKind::RegistryKey,
            location: format!(r"{}\{path}", hive_word(hive)),
            size: None,
            confidence,
            scope: if hive == Hive::Hkcu {
                Scope::User
            } else {
                Scope::Machine
            },
            reason,
        });
    }

    fn offer_value(&mut self, hive: Hive, key: &str, value: &str, reason: String) {
        self.push(Candidate {
            kind: ItemKind::RegistryValue,
            location: format!(r"{}\{key} | {value}", hive_word(hive)),
            size: None,
            confidence: Confidence::Certain,
            scope: if hive == Hive::Hkcu {
                Scope::User
            } else {
                Scope::Machine
            },
            reason,
        });
    }

    fn offer_system(&mut self, kind: ItemKind, location: String, reason: String) {
        self.push(Candidate {
            kind,
            location,
            size: None,
            confidence: Confidence::Certain,
            scope: Scope::Machine,
            reason,
        });
    }

    /// The install folder, and its parent when that is named for the
    /// publisher and holds nothing else.
    fn home(&mut self, home: &str) {
        let app = self.app();
        self.offer_path(
            home,
            ItemKind::Folder,
            None,
            Confidence::Certain,
            format!("It is the folder {app} was installed in."),
            true,
        );
        let Some((parent, leaf)) = parent_and_leaf(home) else {
            return;
        };
        let Some((_, parent_leaf)) = parent_and_leaf(&parent) else {
            return;
        };
        if !self.narrow(&parent) || !self.is_publisher(&parent_leaf) {
            return;
        }
        if let Some(entries) = self.list(&parent)
            && entries.iter().all(|e| e.name.eq_ignore_ascii_case(&leaf))
        {
            let publisher = self.publisher_name();
            self.offer_path(
                &parent,
                ItemKind::Folder,
                None,
                Confidence::Likely,
                format!(
                    "It is named for {publisher} and holds nothing but the folder {app} was installed in."
                ),
                true,
            );
        }
    }

    /// `<Name>` and `<Publisher>\<Name>` in a well-known folder, and
    /// `<Publisher>` itself where `publisher_folder` says so.
    fn by_name(&mut self, which: Known, publisher_folder: bool) {
        let Some(root) = self.folder(which) else {
            return;
        };
        let Some(entries) = self.list(&root) else {
            return;
        };
        let (app, publisher) = (self.app(), self.publisher_name());
        for entry in entries
            .iter()
            .filter(|e| e.is_dir && e.reparse != Reparse::NameSurrogate)
        {
            let path = join(&root, &entry.name);
            if self.is_name(&entry.name) {
                self.offer_path(
                    &path,
                    ItemKind::Folder,
                    None,
                    Confidence::Likely,
                    format!("Its name matches {app}."),
                    true,
                );
            } else if self.is_publisher(&entry.name) {
                let children: Vec<Entry> = self
                    .list(&path)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|c| {
                        c.is_dir && c.reparse != Reparse::NameSurrogate && self.is_name(&c.name)
                    })
                    .collect();
                for child in children {
                    self.offer_path(
                        &join(&path, &child.name),
                        ItemKind::Folder,
                        None,
                        Confidence::Likely,
                        format!("Its name matches {app}, in {publisher}'s folder."),
                        true,
                    );
                }
                if publisher_folder {
                    self.offer_path(
                        &path,
                        ItemKind::Folder,
                        None,
                        Confidence::Possible,
                        format!("It is named for {publisher}, who published {app}."),
                        true,
                    );
                }
            }
        }
    }

    fn start_menus(&mut self) {
        let app = self.app();
        for which in [Known::StartMenuMachine, Known::StartMenuUser] {
            let Some(root) = self.folder(which) else {
                continue;
            };
            // The `Programs` folder itself is never offered, only what is in it.
            let (_, parts) = self.menu_folder(&root, 0);
            for part in parts {
                match part {
                    MenuPart::Folder(path) => self.offer_path(
                        &path,
                        ItemKind::Shortcut,
                        None,
                        Confidence::Certain,
                        format!("It holds only shortcuts to {app}, which is gone."),
                        false,
                    ),
                    MenuPart::Link(path, size) => self.offer_path(
                        &path,
                        ItemKind::Shortcut,
                        Some(size),
                        Confidence::Certain,
                        format!("It is a shortcut to {app}, which is gone."),
                        false,
                    ),
                }
            }
        }
    }

    /// Whether everything in a Start menu folder is a dead shortcut of the
    /// application's, and what in it is.
    fn menu_folder(&mut self, dir: &str, depth: u8) -> (bool, Vec<MenuPart>) {
        let Some(entries) = self.list(dir) else {
            return (false, Vec::new());
        };
        let mut whole = !entries.is_empty();
        let mut parts = Vec::new();
        for entry in &entries {
            let path = join(dir, &entry.name);
            if entry.reparse == Reparse::NameSurrogate {
                whole = false;
            } else if entry.is_dir {
                if depth + 1 >= MENU_DEPTH {
                    whole = false;
                    continue;
                }
                let (all, inner) = self.menu_folder(&path, depth + 1);
                // `Startup` is Windows' own folder, however empty it would be.
                if all && !entry.name.eq_ignore_ascii_case("Startup") {
                    parts.push(MenuPart::Folder(path));
                } else {
                    whole = false;
                    parts.extend(inner);
                }
            } else if self.dead_shortcut(&path) {
                parts.push(MenuPart::Link(path, entry.size));
            } else {
                whole = false;
            }
        }
        (whole, parts)
    }

    /// A shortcut whose target lay in a home and is no longer there.
    fn dead_shortcut(&self, path: &str) -> bool {
        if !path.to_lowercase().ends_with(".lnk") {
            return false;
        }
        let Some(target) = self.view.shortcut_target(Path::new(path)) else {
            return false;
        };
        let target = target.to_string_lossy();
        self.homes.iter().any(|home| inside(&target, home))
            && !self.view.exists(Path::new(&*target))
    }

    fn desktop(&mut self) {
        let Some(root) = self.folder(Known::Desktop) else {
            return;
        };
        let app = self.app();
        for entry in self.list(&root).unwrap_or_default() {
            let path = join(&root, &entry.name);
            if !entry.is_dir && entry.reparse != Reparse::NameSurrogate && self.dead_shortcut(&path)
            {
                self.offer_path(
                    &path,
                    ItemKind::Shortcut,
                    Some(entry.size),
                    Confidence::Certain,
                    format!("It is a shortcut to {app}, which is gone."),
                    false,
                );
            }
        }
    }

    /// `<Name>` in Documents, `Documents\My Games` and Saved Games.
    fn your_data(&mut self) {
        let mut roots = Vec::new();
        if let Some(documents) = self.folder(Known::Documents) {
            roots.push(join(&documents, "My Games"));
            roots.push(documents);
        }
        roots.extend(self.folder(Known::SavedGames));
        let app = self.app();
        for root in roots {
            for entry in self.list(&root).unwrap_or_default() {
                if entry.is_dir
                    && entry.reparse != Reparse::NameSurrogate
                    && self.is_name(&entry.name)
                {
                    self.offer_path(
                        &join(&root, &entry.name),
                        ItemKind::Folder,
                        None,
                        Confidence::YourData,
                        format!("Its name matches {app}, among your own files."),
                        true,
                    );
                }
            }
        }
    }

    fn registry(&mut self) {
        let app = self.app();
        if let Some((hive, key)) = &self.hints.uninstall_key {
            let path = format!(r"{UNINSTALL}\{key}");
            if self.values(*hive, &path).is_some() {
                self.offer_key(
                    *hive,
                    &path,
                    Confidence::Certain,
                    format!("It is the uninstall entry {app} was listed under."),
                );
            }
        }
        for hive in [Hive::Hklm, Hive::Hklm32, Hive::Hkcu] {
            self.software(hive);
            self.app_paths(hive);
            self.run(hive);
        }
        // The 32-bit view of `Software\Classes` is the same keys.
        for hive in [Hive::Hklm, Hive::Hkcu] {
            self.classes(hive);
        }
    }

    /// `Software\<Name>`, `Software\<Publisher>\<Name>`, and
    /// `Software\<Publisher>` when nothing else is left under it.
    fn software(&mut self, hive: Hive) {
        let Some(keys) = self.subkeys(hive, SOFTWARE) else {
            return;
        };
        let (app, publisher) = (self.app(), self.publisher_name());
        for key in keys {
            if NOT_A_VENDOR.contains(&key.to_lowercase().as_str()) {
                continue;
            }
            let path = format!(r"{SOFTWARE}\{key}");
            if self.is_name(&key) {
                self.offer_key(
                    hive,
                    &path,
                    Confidence::Likely,
                    format!("Its name matches {app}."),
                );
            } else if self.is_publisher(&key) {
                let Some(children) = self.subkeys(hive, &path) else {
                    continue;
                };
                let mine: Vec<&String> = children.iter().filter(|c| self.is_name(c)).collect();
                for child in &mine {
                    self.offer_key(
                        hive,
                        &format!(r"{path}\{child}"),
                        Confidence::Likely,
                        format!("Its name matches {app}, under {publisher}."),
                    );
                }
                if mine.len() == children.len() {
                    self.offer_key(
                        hive,
                        &path,
                        Confidence::Possible,
                        format!("It is named for {publisher}, who published {app}, and holds nothing else."),
                    );
                }
            }
        }
    }

    fn app_paths(&mut self, hive: Hive) {
        let app = self.app();
        for exe in self.subkeys(hive, APP_PATHS).unwrap_or_default() {
            let path = format!(r"{APP_PATHS}\{exe}");
            if let Some(values) = self.values(hive, &path)
                && values.iter().any(|(_, data)| self.points_into(data))
            {
                self.offer_key(
                    hive,
                    &path,
                    Confidence::Certain,
                    format!("It tells Windows where a program of {app}'s is."),
                );
            }
        }
    }

    fn run(&mut self, hive: Hive) {
        let app = self.app();
        for key in RUN_KEYS {
            for (name, data) in self.values(hive, key).unwrap_or_default() {
                if self.points_into(&data) {
                    self.offer_value(
                        hive,
                        key,
                        &name,
                        format!("It starts a program from the folder {app} was installed in."),
                    );
                }
            }
        }
    }

    /// ProgIDs and `Applications\<exe>` whose every verb runs something in a
    /// home, and the extensions that refer to those ProgIDs.
    fn classes(&mut self, hive: Hive) {
        let Some(keys) = self.subkeys(hive, CLASSES) else {
            return;
        };
        let app = self.app();
        let mut gone: Vec<String> = Vec::new();
        for key in &keys {
            if key.starts_with('.') || SHELL_CLASSES.contains(&key.to_lowercase().as_str()) {
                continue;
            }
            let path = format!(r"{CLASSES}\{key}");
            if self.opens_only_from_home(hive, &path) {
                self.offer_key(
                    hive,
                    &path,
                    Confidence::Certain,
                    format!("It opens files or links with {app}, which is gone."),
                );
                gone.push(key.clone());
            }
        }
        if keys.iter().any(|k| k.eq_ignore_ascii_case("Applications")) {
            let applications = format!(r"{CLASSES}\Applications");
            for exe in self.subkeys(hive, &applications).unwrap_or_default() {
                let path = format!(r"{applications}\{exe}");
                if self.opens_only_from_home(hive, &path) {
                    self.offer_key(
                        hive,
                        &path,
                        Confidence::Certain,
                        format!("It tells Windows how to open files with {app}, which is gone."),
                    );
                }
            }
        }
        if gone.is_empty() {
            return;
        }
        for extension in keys.iter().filter(|k| k.starts_with('.')) {
            self.extension(hive, extension, &gone);
        }
    }

    /// Whether a class has verbs and every one of them runs something in a
    /// home. One verb that runs anything else means the class is in use.
    fn opens_only_from_home(&mut self, hive: Hive, class: &str) -> bool {
        let shell = format!(r"{class}\shell");
        let Some(verbs) = self.subkeys(hive, &shell) else {
            return false;
        };
        if verbs.is_empty() {
            return false;
        }
        for verb in verbs {
            let command = format!(r"{shell}\{verb}\command");
            let ours = self.values(hive, &command).is_some_and(|values| {
                values
                    .iter()
                    .any(|(name, data)| name.is_empty() && self.points_into(data))
            });
            if !ours {
                return false;
            }
        }
        true
    }

    /// An extension that refers only to ProgIDs being cleared goes with
    /// them; one that refers to others too loses only those references.
    fn extension(&mut self, hive: Hive, extension: &str, gone: &[String]) {
        let path = format!(r"{CLASSES}\{extension}");
        let is_gone = |progid: &str| gone.iter().any(|g| g.eq_ignore_ascii_case(progid));
        let Some(values) = self.values(hive, &path) else {
            return;
        };
        let Some(subkeys) = self.subkeys(hive, &path) else {
            return;
        };
        let open_with = format!(r"{path}\OpenWithProgids");
        let (open_with_values, open_with_empty) = if subkeys
            .iter()
            .any(|s| s.eq_ignore_ascii_case("OpenWithProgids"))
        {
            let Some(values) = self.values(hive, &open_with) else {
                return;
            };
            let Some(below) = self.subkeys(hive, &open_with) else {
                return;
            };
            (values, below.is_empty())
        } else {
            (Vec::new(), true)
        };
        let refers = values
            .iter()
            .any(|(name, data)| name.is_empty() && is_gone(data))
            || open_with_values.iter().any(|(name, _)| is_gone(name));
        if !refers {
            return;
        }
        let only_ours = values
            .iter()
            .all(|(name, data)| name.is_empty() && is_gone(data))
            && subkeys
                .iter()
                .all(|s| s.eq_ignore_ascii_case("OpenWithProgids"))
            && open_with_empty
            && open_with_values.iter().all(|(name, _)| is_gone(name));
        let app = self.app();
        if only_ours {
            self.offer_key(
                hive,
                &path,
                Confidence::Certain,
                format!("It opened these files only with {app}, which is gone."),
            );
            return;
        }
        for (name, _) in open_with_values.iter().filter(|(name, _)| is_gone(name)) {
            self.offer_value(
                hive,
                &open_with,
                name,
                format!("It offers {app}, which is gone, to open these files."),
            );
        }
    }

    /// Services, scheduled tasks and firewall rules that run something in a
    /// home.
    fn system(&mut self) {
        let app = self.app();
        let services = self.view.services();
        self.out.unreadable += services.unreadable;
        for service in services.items {
            if self.points_into(&service.image_path) {
                self.offer_system(
                    ItemKind::Service,
                    format!("service:{}", service.name),
                    format!("It runs a program of {app}'s as a service."),
                );
            }
        }
        let tasks = self.view.tasks();
        self.out.unreadable += tasks.unreadable;
        for task in tasks.items {
            if task.actions.iter().any(|a| self.points_into(a)) {
                self.offer_system(
                    ItemKind::Task,
                    format!("task:{}", task.path),
                    format!("It runs a program of {app}'s on a schedule."),
                );
            }
        }
        let rules = self.view.firewall_rules();
        self.out.unreadable += rules.unreadable;
        for rule in rules.items {
            if self.points_into(&rule.program) {
                self.offer_system(
                    ItemKind::FirewallRule,
                    format!("firewall:{}", rule.name),
                    format!("It lets a program of {app}'s through Windows Firewall."),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cleanup::view::{FixtureView, Hive};
    use crate::model::{PackageRef, SourceKind};
    use std::path::{Path, PathBuf};

    fn machine(name: &str) -> FixtureView {
        FixtureView::load(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/cleanup")
                .join(format!("{name}.json")),
        )
    }

    fn hints(
        name: &str,
        publisher: Option<&str>,
        install_dir: Option<&str>,
        install_location: Option<&str>,
    ) -> Hints {
        Hints {
            package: PackageRef {
                source: SourceKind::Arp,
                id: name.to_string(),
            },
            name: name.to_string(),
            publisher: publisher.map(str::to_string),
            uninstall_key: None,
            product_code: None,
            install_dir: install_dir.map(PathBuf::from),
            install_location: install_location.map(PathBuf::from),
            icon_dir: None,
            shortcuts: Vec::new(),
            extra_dirs: Vec::new(),
        }
    }

    fn at<'a>(scanned: &'a Scanned, location: &str) -> Option<&'a Candidate> {
        scanned
            .candidates
            .iter()
            .find(|c| c.location.eq_ignore_ascii_case(location))
    }

    fn confidence(scanned: &Scanned, location: &str) -> Option<Confidence> {
        at(scanned, location).map(|c| c.confidence)
    }

    fn locations(scanned: &Scanned) -> Vec<String> {
        scanned
            .candidates
            .iter()
            .map(|c| c.location.clone())
            .collect()
    }

    const LOCAL: &str = r"C:\Users\you\AppData\Local";
    const ROAMING: &str = r"C:\Users\you\AppData\Roaming";
    const USER_MENU: &str = r"C:\Users\you\AppData\Roaming\Microsoft\Windows\Start Menu\Programs";
    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    fn discord() -> Scanned {
        scan(
            &hints(
                "Discord",
                Some("Discord Inc."),
                Some(r"C:\Users\you\AppData\Local\Discord"),
                None,
            ),
            &machine("discord"),
        )
    }

    #[test]
    fn discord_leftovers_are_found_with_the_right_confidence() {
        let s = discord();
        let folder = at(&s, &format!(r"{LOCAL}\Discord")).expect("the install folder");
        assert_eq!(folder.confidence, Confidence::Certain);
        assert_eq!(folder.kind, ItemKind::Folder);
        assert_eq!(folder.scope, Scope::User);
        assert_eq!(folder.size, Some(105_000));

        assert_eq!(
            confidence(&s, &format!(r"{ROAMING}\discord")),
            Some(Confidence::Likely)
        );
        let key = at(&s, r"HKCU\Software\Discord").expect("the settings key");
        assert_eq!(key.confidence, Confidence::Likely);
        assert_eq!(key.kind, ItemKind::RegistryKey);
        assert_eq!(key.size, None);

        let run = at(&s, &format!(r"HKCU\{RUN} | Discord")).expect("the Run value");
        assert_eq!(run.confidence, Confidence::Certain);
        assert_eq!(run.kind, ItemKind::RegistryValue);
        assert_eq!(run.scope, Scope::User);

        let menu = at(&s, &format!(r"{USER_MENU}\Discord Inc")).expect("the Start menu folder");
        assert_eq!(menu.confidence, Confidence::Certain);
        assert_eq!(menu.kind, ItemKind::Shortcut);
        // Only the folder is offered, not the shortcut inside it as well.
        assert!(at(&s, &format!(r"{USER_MENU}\Discord Inc\Discord.lnk")).is_none());

        // A folder that holds other shortcuts too gives up only the dead one.
        assert_eq!(
            confidence(&s, &format!(r"{USER_MENU}\Tools\Discord (old).lnk")),
            Some(Confidence::Certain)
        );
        assert!(at(&s, &format!(r"{USER_MENU}\Tools")).is_none());
        assert!(at(&s, &format!(r"{USER_MENU}\Tools\Calculator.lnk")).is_none());
        assert!(at(&s, &format!(r"{USER_MENU}\Notepad.lnk")).is_none());
        assert_eq!(
            confidence(&s, r"C:\Users\you\Desktop\Discord.lnk"),
            Some(Confidence::Certain)
        );
        // A shortcut whose target is still there is not dead, even into the
        // install folder.
        assert!(at(&s, r"C:\Users\you\Desktop\Discord icon.lnk").is_none());
        // Startup is Windows' own folder: its shortcut goes, the folder stays.
        assert_eq!(
            confidence(&s, &format!(r"{USER_MENU}\Startup\Discord Startup.lnk")),
            Some(Confidence::Certain)
        );
        assert!(at(&s, &format!(r"{USER_MENU}\Startup")).is_none());

        // Names that merely start the same way are someone else's.
        assert!(at(&s, &format!(r"{LOCAL}\DiscordHelper")).is_none());
        assert!(at(&s, &format!(r"{ROAMING}\Discord Bot Maker")).is_none());
        assert!(at(&s, &format!(r"HKCU\{RUN} | OneDrive")).is_none());
        assert_eq!(s.not_offered_cloud, 0);
        assert_eq!(s.unreadable, 0);
    }

    #[test]
    fn file_associations_are_offered_only_where_nothing_else_uses_them() {
        let s = discord();
        let classes = r"HKCU\Software\Classes";
        assert_eq!(
            confidence(&s, &format!(r"{classes}\discord")),
            Some(Confidence::Certain)
        );
        // An extension that names nothing but the removed ProgID goes with it.
        assert_eq!(
            confidence(&s, &format!(r"{classes}\.dtheme")),
            Some(Confidence::Certain)
        );
        // One that other ProgIDs share keeps everything but the reference.
        assert!(at(&s, &format!(r"{classes}\.txt")).is_none());
        let reference = at(&s, &format!(r"{classes}\.txt\OpenWithProgids | discord"))
            .expect("the reference to the removed ProgID");
        assert_eq!(reference.kind, ItemKind::RegistryValue);
        // A protocol Windows itself owns is never offered, whatever it points at.
        assert!(at(&s, &format!(r"{classes}\http")).is_none());
        // A ProgID with a verb that points somewhere else is still in use.
        assert!(at(&s, &format!(r"{classes}\txtfile")).is_none());
    }

    #[test]
    fn a_steam_game_offers_its_common_folder_and_my_games_as_your_data() {
        // The uninstall string runs steam.exe, so the uninstaller's folder is
        // Steam's own; `InstallLocation` is the game's.
        let common = r"C:\Program Files (x86)\Steam\steamapps\common";
        let s = scan(
            &hints(
                "Fallout 4",
                Some("Bethesda Game Studios"),
                Some(r"C:\Program Files (x86)\Steam"),
                Some(&format!(r"{common}\Fallout 4")),
            ),
            &machine("steam-game"),
        );
        let game = at(&s, &format!(r"{common}\Fallout 4")).expect("the game folder");
        assert_eq!(game.confidence, Confidence::Certain);
        assert_eq!(game.scope, Scope::Machine);
        let saves = at(&s, r"C:\Users\you\Documents\My Games\Fallout4").expect("My Games");
        assert_eq!(saves.confidence, Confidence::YourData);
        assert_eq!(saves.scope, Scope::User);
        assert_eq!(
            confidence(&s, &format!(r"{LOCAL}\Fallout4")),
            Some(Confidence::Likely)
        );
        assert_eq!(
            confidence(&s, "firewall:Fallout 4"),
            Some(Confidence::Certain)
        );

        // Nothing of Steam itself is offered.
        assert!(at(&s, r"C:\Program Files (x86)\Steam").is_none());
        assert!(at(&s, &format!(r"HKCU\{RUN} | Steam")).is_none());
        assert!(at(&s, "firewall:Steam").is_none());
        assert!(at(&s, "service:Steam Client Service").is_none());
        assert!(at(&s, &format!(r"{common}\Portal 2")).is_none());
        assert!(at(&s, r"C:\Users\you\Documents\My Games\Skyrim").is_none());
    }

    #[test]
    fn a_broad_install_dir_is_never_offered() {
        let view = machine("broad-install-dir");
        let broad = [
            r"C:\Program Files",
            r"C:\Users\you\Downloads",
            r"C:\Users\you\Desktop",
            r"C:\Users\you",
            r"C:\",
        ];
        for dir in broad {
            for h in [
                hints("Widget Maker", Some("Widget Co"), None, Some(dir)),
                hints("Widget Maker", Some("Widget Co"), Some(dir), None),
            ] {
                let s = scan(&h, &view);
                let offered = locations(&s);
                assert!(
                    s.candidates
                        .iter()
                        .all(|c| c.confidence != Confidence::Certain),
                    "{dir}: {offered:?}"
                );
                // A name match is still a name match, and nothing else is.
                assert_eq!(offered, vec![format!(r"{ROAMING}\Widget Maker")], "{dir}");
            }
        }
    }

    #[test]
    fn onedrive_documents_are_counted_not_offered() {
        let s = scan(
            &hints("Stardew Valley", Some("ConcernedApe"), None, None),
            &machine("onedrive-documents"),
        );
        assert_eq!(s.not_offered_cloud, 2);
        assert!(
            s.candidates
                .iter()
                .all(|c| !c.location.to_lowercase().contains(r"\onedrive\")),
            "{:?}",
            locations(&s)
        );
        // Saved games that are not synced are still offered, unticked.
        assert_eq!(
            confidence(&s, r"C:\Users\you\Saved Games\Stardew Valley"),
            Some(Confidence::YourData)
        );
        assert_eq!(
            confidence(&s, &format!(r"{ROAMING}\StardewValley")),
            Some(Confidence::Likely)
        );
    }

    #[test]
    fn a_three_letter_name_matches_nothing() {
        let s = scan(
            &hints("Foo", Some("Foo Ltd"), None, None),
            &machine("short-name"),
        );
        assert!(s.candidates.is_empty(), "{:?}", locations(&s));
    }

    #[test]
    fn a_four_letter_name_matches() {
        let s = scan(&hints("GIMP", None, None, None), &machine("short-name"));
        assert_eq!(
            confidence(&s, &format!(r"{ROAMING}\GIMP")),
            Some(Confidence::Likely)
        );
        assert_eq!(
            confidence(&s, r"HKCU\Software\GIMP"),
            Some(Confidence::Likely)
        );
    }

    #[test]
    fn stop_list_words_never_match() {
        let view = machine("short-name");
        for h in [
            hints("Teams", Some("Microsoft Corporation"), None, None),
            hints("Common", Some("Intel Corporation"), None, None),
            hints("Temp", Some("Intel"), None, None),
        ] {
            let s = scan(&h, &view);
            assert!(s.candidates.is_empty(), "{}: {:?}", h.name, locations(&s));
        }
    }

    const JUNCTIONS: &str = r#"{
      "folders": {
        "AppData": "C:\\Users\\you\\AppData\\Roaming",
        "LocalAppData": "C:\\Users\\you\\AppData\\Local",
        "StartMenuUser": "C:\\Users\\you\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs",
        "Profile": "C:\\Users\\you"
      },
      "reparse": { "C:\\Games": "NameSurrogate" },
      "dirs": {
        "C:\\Users\\you\\AppData\\Local": [
          { "name": "Widget", "is_dir": true, "reparse": "NameSurrogate" }
        ],
        "C:\\Users\\you\\AppData\\Local\\Widget": [{ "name": "a.txt", "size": 1 }],
        "C:\\Users\\you\\AppData\\Roaming": [
          { "name": "Widget Co", "is_dir": true, "reparse": "NameSurrogate" }
        ],
        "C:\\Users\\you\\AppData\\Roaming\\Widget Co": [{ "name": "Widget", "is_dir": true }],
        "C:\\Users\\you\\AppData\\Roaming\\Widget Co\\Widget": [{ "name": "b.txt", "size": 1 }],
        "C:\\Users\\you\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs": [
          { "name": "Widget", "is_dir": true, "reparse": "NameSurrogate" }
        ],
        "C:\\Users\\you\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs\\Widget": [
          { "name": "Widget.lnk", "size": 1 }
        ],
        "C:\\Games\\Widget": [{ "name": "widget.exe", "size": 1 }]
      },
      "shortcuts": {
        "C:\\Users\\you\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs\\Widget\\Widget.lnk": "C:\\Users\\you\\AppData\\Local\\Widget\\gone.exe"
      }
    }"#;

    #[test]
    fn a_junction_inside_appdata_is_not_followed() {
        let view = FixtureView::from_json(JUNCTIONS);
        for home in [r"C:\Users\you\AppData\Local\Widget", r"C:\Games\Widget"] {
            let s = scan(&hints("Widget", Some("Widget Co"), Some(home), None), &view);
            assert!(s.candidates.is_empty(), "{home}: {:?}", locations(&s));
            assert_eq!(s.not_offered_cloud, 0);
        }
    }

    fn acme(install_dir: &str) -> Scanned {
        scan(
            &hints("Acme Studio", Some("Acme Corp"), Some(install_dir), None),
            &machine("vendor"),
        )
    }

    const STUDIO: &str = r"C:\Program Files\Acme\Acme Studio";

    #[test]
    fn a_name_under_its_publisher_is_likely_and_the_publisher_alone_possible() {
        let s = acme(STUDIO);
        assert_eq!(
            confidence(&s, r"C:\ProgramData\Acme\Acme Studio"),
            Some(Confidence::Likely)
        );
        assert_eq!(
            confidence(&s, r"C:\Program Files (x86)\Acme\Acme Studio"),
            Some(Confidence::Likely)
        );
        assert_eq!(
            confidence(&s, r"HKLM\Software\Acme\Acme Studio"),
            Some(Confidence::Likely)
        );
        assert_eq!(
            confidence(&s, r"HKCU\Software\Acme\Acme Studio"),
            Some(Confidence::Likely)
        );
        // A publisher folder in AppData or ProgramData is offered, unticked;
        // the guard drops it while anything else of theirs is installed.
        assert_eq!(
            confidence(&s, &format!(r"{ROAMING}\Acme")),
            Some(Confidence::Possible)
        );
        assert_eq!(
            confidence(&s, r"C:\ProgramData\Acme"),
            Some(Confidence::Possible)
        );
        // A publisher key only when nothing else is left under it.
        assert_eq!(
            confidence(&s, r"HKCU\Software\Acme"),
            Some(Confidence::Possible)
        );
        assert!(at(&s, r"HKLM\Software\Acme").is_none());
        assert!(at(&s, r"HKLM\Software\Acme\Acme Paint").is_none());
        assert!(at(&s, r"C:\Program Files (x86)\Acme\Acme Paint").is_none());
        // Program Files is searched for the name, never for the publisher.
        assert!(at(&s, r"C:\Program Files (x86)\Acme").is_none());
    }

    #[test]
    fn the_install_folders_parent_is_offered_only_when_it_holds_nothing_else() {
        let s = acme(STUDIO);
        assert_eq!(confidence(&s, STUDIO), Some(Confidence::Certain));
        assert_eq!(
            confidence(&s, r"C:\Program Files\Acme"),
            Some(Confidence::Likely)
        );

        let s = acme(r"C:\Program Files (x86)\Acme\Acme Studio");
        assert_eq!(
            confidence(&s, r"C:\Program Files (x86)\Acme\Acme Studio"),
            Some(Confidence::Certain)
        );
        assert!(at(&s, r"C:\Program Files (x86)\Acme").is_none());
    }

    #[test]
    fn what_points_into_the_install_folder_is_certain_and_nothing_else_is() {
        let mut h = hints("Acme Studio", Some("Acme Corp"), Some(STUDIO), None);
        h.uninstall_key = Some((Hive::Hklm, "Acme Studio_is1".to_string()));
        let s = scan(&h, &machine("vendor"));
        let uninstall = r"HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\Acme Studio_is1";
        assert_eq!(confidence(&s, uninstall), Some(Confidence::Certain));
        assert_eq!(at(&s, uninstall).unwrap().scope, Scope::Machine);
        let app_paths = r"HKLM\Software\Microsoft\Windows\CurrentVersion\App Paths";
        assert_eq!(
            confidence(&s, &format!(r"{app_paths}\acmestudio.exe")),
            Some(Confidence::Certain)
        );
        assert!(at(&s, &format!(r"{app_paths}\paint.exe")).is_none());
        // `%ProgramFiles%` is read as the machine's Program Files.
        let tray = at(&s, &format!(r"HKLM32\{RUN} | AcmeTray")).expect("the Run value");
        assert_eq!(tray.confidence, Confidence::Certain);
        assert_eq!(tray.scope, Scope::Machine);
        assert!(at(&s, &format!(r"HKLM32\{RUN} | PaintTray")).is_none());

        let service = at(&s, "service:AcmeStudioSvc").expect("the service");
        assert_eq!(service.kind, ItemKind::Service);
        assert_eq!(service.confidence, Confidence::Certain);
        assert_eq!(service.scope, Scope::Machine);
        assert!(at(&s, "service:AcmePaintSvc").is_none());
        assert!(at(&s, "service:Spooler").is_none());
        let task = at(&s, r"task:\Acme\Studio Update").expect("the task");
        assert_eq!(task.kind, ItemKind::Task);
        assert!(at(&s, r"task:\Acme\Paint Update").is_none());
        let rule = at(&s, "firewall:Acme Studio").expect("the firewall rule");
        assert_eq!(rule.kind, ItemKind::FirewallRule);

        // Without the uninstall key in the hints, no uninstall key is offered.
        assert!(at(&acme(STUDIO), uninstall).is_none());
    }

    #[test]
    fn a_package_managers_folder_counts_as_the_install_folder() {
        let scoop = r"C:\Users\you\scoop\apps\acme-studio";
        let mut h = hints("Acme Studio", Some("Acme Corp"), None, None);
        h.extra_dirs = vec![PathBuf::from(scoop)];
        let s = scan(&h, &machine("vendor"));
        let folder = at(&s, scoop).expect("the scoop folder");
        assert_eq!(folder.confidence, Confidence::Certain);
        assert_eq!(folder.scope, Scope::User);
        assert_eq!(
            confidence(
                &s,
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce | AcmeScoop"
            ),
            Some(Confidence::Certain)
        );

        // A package manager folder that is too broad is no home either.
        let mut h = hints("Acme Studio", Some("Acme Corp"), None, None);
        h.extra_dirs = vec![PathBuf::from(r"C:\Users\you")];
        let s = scan(&h, &machine("vendor"));
        assert!(at(&s, r"C:\Users\you").is_none());
        assert!(
            s.candidates
                .iter()
                .all(|c| c.confidence != Confidence::Certain)
        );
    }

    #[test]
    fn what_cannot_be_read_is_counted_and_not_offered() {
        let s = acme(STUDIO);
        // The unreadable `Local\Acme Studio`, one service and two tasks.
        assert_eq!(s.unreadable, 4);
        assert!(at(&s, &format!(r"{LOCAL}\Acme Studio")).is_none());
    }
}
