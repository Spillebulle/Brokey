//! What a scan searches by, taken from the machine before the uninstaller
//! runs. Once the application is uninstalled its Add/Remove Programs entry
//! is gone and so is the one fact that says where it lived, so the hints are
//! read first and carried through the removal.
//!
//! Everything here reads the machine and changes nothing.

use super::steam::{self, Read};
use super::view::Hive;
use crate::Store;
use crate::model::{Package, PackageRef, SourceKind};
use crate::sources::windows::arp::{self, RawEntry};
use crate::sources::windows::{icon, shortcut};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What is known about one installed application, before it is removed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hints {
    pub package: PackageRef,
    pub name: String,
    pub publisher: Option<String>,
    /// The uninstall key the application was listed under.
    pub uninstall_key: Option<(Hive, String)>,
    /// The MSI ProductCode, when the Windows Installer owns the product.
    pub product_code: Option<String>,
    /// Derived from the uninstaller, as the launcher derives it.
    pub install_dir: Option<PathBuf>,
    /// What the installer wrote as `InstallLocation`.
    pub install_location: Option<PathBuf>,
    /// The folder `DisplayIcon` points into.
    pub icon_dir: Option<PathBuf>,
    /// Start Menu shortcuts whose target lies in the application's folder.
    pub shortcuts: Vec<PathBuf>,
    /// Folders its source put it in: Scoop's `apps\<name>`, Chocolatey's
    /// `lib\<id>`, a portable GitHub download.
    pub extra_dirs: Vec<PathBuf>,
}

fn non_empty(text: &Option<String>) -> Option<String> {
    text.as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// The hints an Add/Remove Programs entry gives. `shortcuts` are the Start
/// Menu shortcuts already found for it; see [`shortcuts_into`]. `read` reads
/// Steam's own files, for an entry that is a Steam game; see [`steam`].
pub fn from_arp(entry: &RawEntry, shortcuts: Vec<PathBuf>, read: Read) -> Hints {
    let mut hints = arp_hints(entry, shortcuts);
    if let Some(id) = steam::app_id(&entry.key_name) {
        // The uninstaller's folder is Steam's, and its icon is in Steam's
        // folder too: neither is the game's. The manifest says where the
        // game is; without it, only an `InstallLocation` can.
        let steam_dir = hints.install_dir.take();
        let game = steam_dir
            .as_ref()
            .and_then(|steam| steam::game(&steam.to_string_lossy(), id, read));
        hints.icon_dir = None;
        // An `InstallLocation` that is Steam's folder, or holds it, is no
        // more the game's than the uninstaller's folder is.
        if let (Some(steam), Some(location)) = (&steam_dir, &hints.install_location) {
            let (steam, location) = (steam.to_string_lossy(), location.to_string_lossy());
            if shortcut::normalise(&steam) == shortcut::normalise(&location)
                || shortcut::inside(&steam, &location)
            {
                hints.install_location = None;
            }
        }
        match game {
            Some(game) => {
                hints.install_dir = Some(game.folder);
                hints.extra_dirs = game.by_id;
            }
            None => hints.install_dir = hints.install_location.clone(),
        }
    }
    hints
}

fn arp_hints(entry: &RawEntry, shortcuts: Vec<PathBuf>) -> Hints {
    // A GUID names a product only when the Windows Installer says it owns it;
    // driver packages are keyed by a GUID and are not MSI products.
    let product_code = (entry.windows_installer == Some(1))
        .then(|| arp::product_code(&entry.key_name))
        .flatten()
        .map(str::to_string);
    let icon_dir = entry
        .display_icon
        .as_deref()
        .and_then(icon::reference)
        .and_then(|r| arp::windows_parent(&r.path.to_string_lossy()))
        .map(PathBuf::from);
    Hints {
        package: PackageRef {
            source: SourceKind::Arp,
            id: arp::package_id(entry),
        },
        name: non_empty(&entry.display_name).unwrap_or_else(|| entry.key_name.clone()),
        publisher: non_empty(&entry.publisher),
        uninstall_key: Some((entry.hive.into(), entry.key_name.clone())),
        product_code,
        install_dir: arp::install_dir(entry),
        install_location: non_empty(&entry.install_location).map(PathBuf::from),
        icon_dir,
        shortcuts,
        extra_dirs: Vec::new(),
    }
}

/// The folders whose shortcuts belong to this application: the directory the
/// launcher searches, when it is narrow enough to say which application a
/// shortcut belongs to. For a Steam game that would be Steam's folder, and a
/// shortcut to `steam.exe` is Steam's; its own folder is the game's.
fn shortcut_dirs(entry: &RawEntry, hints: &Hints) -> Vec<String> {
    let dir = if steam::app_id(&entry.key_name).is_some() {
        hints
            .install_dir
            .as_ref()
            .map(|d| d.to_string_lossy().into_owned())
    } else {
        shortcut::search_dir(entry)
    };
    dir.filter(|d| shortcut::specific(d)).into_iter().collect()
}

/// The shortcuts among `all` that start something inside one of `dirs`.
/// A directory too broad to name one application is never searched, so a
/// shortcut into `Program Files` itself belongs to nobody.
pub fn shortcuts_into(all: &[shortcut::Shortcut], dirs: &[String]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = all
        .iter()
        .filter(|s| {
            dirs.iter()
                .any(|d| shortcut::specific(d) && shortcut::inside(&s.target, d))
        })
        .map(|s| s.path.clone())
        .collect();
    found.sort();
    found.dedup();
    found
}

/// The machine's Start Menu shortcuts. Windows has them; nothing else does.
#[cfg(windows)]
pub fn start_menu() -> Vec<shortcut::Shortcut> {
    shortcut::start_menu().to_vec()
}

#[cfg(not(windows))]
pub fn start_menu() -> Vec<shortcut::Shortcut> {
    Vec::new()
}

/// [`Store::leftover_hints`] with the Start Menu shortcuts and the file
/// reader handed in, so a test needs no Start Menu and no Steam.
pub fn for_package(
    store: &Store,
    package: &PackageRef,
    start_menu: &[shortcut::Shortcut],
    read: Read,
) -> Option<Hints> {
    let source = store.source(package.source)?;
    let entry_of = |key: &str| {
        store
            .source(SourceKind::Arp)
            .and_then(|arp| arp.add_remove_entry(key))
    };
    match package.source {
        SourceKind::Arp => {
            let entry = source.add_remove_entry(&package.id)?;
            let mut hints = from_arp(&entry, Vec::new(), read);
            hints.shortcuts = shortcuts_into(start_menu, &shortcut_dirs(&entry, &hints));
            Some(hints)
        }
        SourceKind::Winget | SourceKind::Choco | SourceKind::Scoop | SourceKind::Github => {
            let installed: Package = source
                .installed()
                .ok()?
                .into_iter()
                .find(|p| p.id == package.id)?;
            let extra_dirs = source.own_folders(&package.id);
            let entry = installed
                .facts
                .iter()
                .find(|(k, _)| k == arp::REGISTRY_KEY_FACT)
                .and_then(|(_, key)| entry_of(key.trim()));
            let mut dirs: Vec<String> =
                extra_dirs.iter().map(|d| d.display().to_string()).collect();
            let mut hints = match &entry {
                Some(entry) => {
                    let hints = from_arp(entry, Vec::new(), read);
                    dirs.extend(shortcut_dirs(entry, &hints));
                    hints
                }
                None => blank(package, String::new()),
            };
            hints.package = package.clone();
            if !installed.name.trim().is_empty() {
                hints.name = installed.name.clone();
            }
            hints.publisher = non_empty(&installed.developer).or(hints.publisher);
            hints.shortcuts = shortcuts_into(start_menu, &dirs);
            hints.extra_dirs.extend(extra_dirs);
            Some(hints)
        }
        // An MSIX package has no uninstall key and no install folder of its
        // own to search: the scan looks in the general locations only.
        SourceKind::Msix => {
            let installed = source
                .installed()
                .ok()?
                .into_iter()
                .find(|p| p.id == package.id)?;
            let mut hints = blank(package, installed.name);
            hints.publisher = non_empty(&installed.developer);
            Some(hints)
        }
        _ => None,
    }
}

fn blank(package: &PackageRef, name: String) -> Hints {
    Hints {
        package: package.clone(),
        name,
        publisher: None,
        uninstall_key: None,
        product_code: None,
        install_dir: None,
        install_location: None,
        icon_dir: None,
        shortcuts: Vec::new(),
        extra_dirs: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Op, Step};
    use crate::sources::windows::arp::Arp;
    use crate::{Query, Result, Source, SourceStatus, Update};

    fn fixture() -> Vec<RawEntry> {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/windows/arp/entries.json"
        ))
        .expect("the fixture is checked in");
        serde_json::from_str(&text).expect("the fixture parses")
    }

    fn named(entries: &[RawEntry], key: &str) -> RawEntry {
        entries
            .iter()
            .find(|e| e.key_name == key)
            .unwrap_or_else(|| panic!("{key} is in the fixture"))
            .clone()
    }

    #[test]
    fn from_arp_takes_the_derived_install_dir_and_product_code() {
        let entry = named(&fixture(), "{6f320b93-ee3c-4826-85e0-000000000002}");
        let hints = from_arp(&entry, Vec::new(), &no_files);
        assert_eq!(
            hints.product_code.as_deref(),
            Some("{6f320b93-ee3c-4826-85e0-000000000002}")
        );
        assert_eq!(
            hints.uninstall_key,
            Some((
                Hive::Hkcu,
                "{6f320b93-ee3c-4826-85e0-000000000002}".to_string()
            ))
        );
        // The uninstaller is msiexec, which says nothing, so the install
        // location is what the directory comes from.
        assert_eq!(
            hints.install_dir,
            Some(PathBuf::from(r"C:\Users\test\AppData\Local\Example"))
        );
        assert_eq!(hints.name, "A per-user application");
        assert_eq!(hints.publisher.as_deref(), Some("Example Ltd"));
    }

    #[test]
    fn a_guid_the_windows_installer_does_not_own_is_not_a_product_code() {
        let entry = named(&fixture(), "{ca4f4d7f-6b8b-4f8d-9d2f-000000000001}");
        assert_eq!(from_arp(&entry, Vec::new(), &no_files).product_code, None);
    }

    #[test]
    fn from_arp_on_an_unquoted_steam_path_finds_the_steam_folder() {
        let entry = named(&fixture(), "Steam");
        let hints = from_arp(&entry, Vec::new(), &no_files);
        assert_eq!(
            hints.install_dir,
            Some(PathBuf::from(r"C:\Program Files (x86)\Steam"))
        );
        assert_eq!(hints.install_location, None);
        assert_eq!(hints.uninstall_key.map(|(h, _)| h), Some(Hive::Hklm32));
    }

    fn no_files(_: &std::path::Path) -> Option<String> {
        None
    }

    fn steam_library() -> crate::cleanup::view::FixtureView {
        crate::cleanup::view::FixtureView::load(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cleanup/steam-library.json"
        )))
    }

    /// A Steam game's entry as Steam writes it.
    fn steam_app(id: &str, name: &str, install_location: &str) -> RawEntry {
        serde_json::from_value(serde_json::json!({
            "hive": "machine",
            "key_name": format!("Steam App {id}"),
            "display_name": name,
            "publisher": "Valve",
            "install_location": install_location,
            "uninstall_string": format!(
                r#""C:\Program Files (x86)\Steam\steam.exe" steam://uninstall/{id}"#
            ),
            "display_icon": r"C:\Program Files (x86)\Steam\steam\games\d841.ico",
        }))
        .expect("an entry")
    }

    #[test]
    fn a_steam_game_takes_its_folder_from_its_manifest_and_never_steams() {
        use crate::cleanup::view::SystemView;
        let view = steam_library();
        let read = |p: &std::path::Path| view.read_text(p);
        let hints = from_arp(&steam_app("1422450", "Deadlock", ""), Vec::new(), &read);
        assert_eq!(
            hints.install_dir,
            Some(PathBuf::from(r"D:\SteamLibrary\steamapps\common\Deadlock"))
        );
        assert_eq!(hints.install_location, None);
        assert_eq!(
            hints.extra_dirs,
            vec![
                PathBuf::from(r"D:\SteamLibrary\steamapps\shadercache\1422450"),
                PathBuf::from(r"D:\SteamLibrary\steamapps\workshop\content\1422450"),
            ]
        );
        // The icon Steam gave it is in Steam's folder, which is not the game's.
        assert_eq!(hints.icon_dir, None);
    }

    #[test]
    fn a_steam_game_without_a_manifest_has_no_folder_rather_than_steams() {
        let hints = from_arp(&steam_app("1422450", "Deadlock", ""), Vec::new(), &no_files);
        assert_eq!(hints.install_dir, None);
        assert!(hints.extra_dirs.is_empty());
        // An `InstallLocation`, when Steam wrote one, is the game's.
        let common = r"D:\SteamLibrary\steamapps\common\Deadlock";
        let hints = from_arp(
            &steam_app("1422450", "Deadlock", common),
            Vec::new(),
            &no_files,
        );
        assert_eq!(hints.install_dir, Some(PathBuf::from(common)));
        // One that is Steam's own folder, or holds it, is not.
        for steams in [r"C:\Program Files (x86)\Steam\", r"C:\Program Files (x86)"] {
            let entry = steam_app("1422450", "Deadlock", steams);
            let hints = from_arp(&entry, Vec::new(), &no_files);
            assert_eq!(hints.install_dir, None, "{steams}");
            assert_eq!(hints.install_location, None, "{steams}");
        }
    }

    #[test]
    fn steam_itself_keeps_its_own_folder() {
        use crate::cleanup::view::SystemView;
        let view = steam_library();
        let read = |p: &std::path::Path| view.read_text(p);
        let entry = named(&fixture(), "Steam");
        assert_eq!(
            from_arp(&entry, Vec::new(), &read).install_dir,
            Some(PathBuf::from(r"C:\Program Files (x86)\Steam"))
        );
    }

    #[test]
    fn the_icon_folder_is_where_display_icon_points() {
        let entry = named(&fixture(), "Obsidian");
        assert_eq!(
            from_arp(&entry, Vec::new(), &no_files).icon_dir,
            Some(PathBuf::from(r"C:\Program Files\Obsidian"))
        );
    }

    fn link(path: &str, target: &str) -> shortcut::Shortcut {
        shortcut::Shortcut {
            path: PathBuf::from(path),
            target: target.to_string(),
            arguments: String::new(),
        }
    }

    #[test]
    fn a_shortcut_into_the_folder_is_taken_and_one_into_a_broad_folder_is_not() {
        let all = vec![
            link(
                r"C:\Menu\Steam.lnk",
                r"C:\Program Files (x86)\Steam\steam.exe",
            ),
            link(
                r"C:\Menu\Other.lnk",
                r"C:\Program Files (x86)\Other\other.exe",
            ),
        ];
        let dirs = vec![r"C:\Program Files (x86)\Steam".to_string()];
        assert_eq!(
            shortcuts_into(&all, &dirs),
            vec![PathBuf::from(r"C:\Menu\Steam.lnk")]
        );
        let broad = vec![r"C:\Program Files (x86)".to_string()];
        assert!(shortcuts_into(&all, &broad).is_empty());
    }

    /// A package manager's source that lists one package and says where
    /// its own folder is.
    struct Manager {
        kind: SourceKind,
        package: Package,
        folders: Vec<PathBuf>,
    }

    impl Source for Manager {
        fn kind(&self) -> SourceKind {
            self.kind
        }
        fn status(&self) -> SourceStatus {
            SourceStatus {
                kind: self.kind,
                available: true,
                reason: None,
                detail: None,
                searchable: true,
                setup: None,
            }
        }
        fn search(&self, _query: &Query) -> Result<Vec<Package>> {
            Ok(Vec::new())
        }
        fn installed(&self) -> Result<Vec<Package>> {
            Ok(vec![self.package.clone()])
        }
        fn updates(&self) -> Result<Vec<Update>> {
            Ok(Vec::new())
        }
        fn details(&self, id: &str) -> Result<Package> {
            Err(crate::Error::new(format!("{id} is not known.")))
        }
        fn plan(&self, _op: &Op) -> Result<Vec<Step>> {
            Ok(Vec::new())
        }
        fn own_folders(&self, _id: &str) -> Vec<PathBuf> {
            self.folders.clone()
        }
    }

    fn store_with(sources: Vec<Box<dyn Source>>) -> Store {
        Store {
            system: crate::system::detect(),
            sources,
        }
    }

    fn arp_source() -> Box<dyn Source> {
        Box::new(Arp::with_cache(fixture(), std::env::temp_dir()))
    }

    #[test]
    fn a_winget_package_takes_its_joined_entry() {
        let mut package = Package::new(SourceKind::Winget, "Notepad++.Notepad++", "Notepad++");
        package.developer = Some("Notepad++ Team".to_string());
        package.facts.push((
            arp::REGISTRY_KEY_FACT.to_string(),
            r"HKLM32\Notepad-plus-plus".to_string(),
        ));
        let store = store_with(vec![
            Box::new(Manager {
                kind: SourceKind::Winget,
                package,
                folders: Vec::new(),
            }),
            arp_source(),
        ]);
        let reference = PackageRef {
            source: SourceKind::Winget,
            id: "Notepad++.Notepad++".to_string(),
        };
        let menu = vec![link(
            r"C:\Menu\Notepad++.lnk",
            r"C:\Program Files (x86)\Notepad++\notepad++.exe",
        )];
        let hints = for_package(&store, &reference, &menu, &no_files).expect("hints");
        assert_eq!(hints.package, reference);
        assert_eq!(
            hints.uninstall_key,
            Some((Hive::Hklm32, "Notepad-plus-plus".to_string()))
        );
        assert_eq!(
            hints.install_dir,
            Some(PathBuf::from(r"C:\Program Files (x86)\Notepad++"))
        );
        assert_eq!(
            hints.shortcuts,
            vec![PathBuf::from(r"C:\Menu\Notepad++.lnk")]
        );
    }

    #[test]
    fn a_scoop_package_adds_its_own_folder_and_needs_no_entry() {
        let package = Package::new(SourceKind::Scoop, "ripgrep", "ripgrep");
        let folder = PathBuf::from(r"C:\Users\test\scoop\apps\ripgrep");
        let store = store_with(vec![
            Box::new(Manager {
                kind: SourceKind::Scoop,
                package,
                folders: vec![folder.clone()],
            }),
            arp_source(),
        ]);
        let reference = PackageRef {
            source: SourceKind::Scoop,
            id: "ripgrep".to_string(),
        };
        let hints = for_package(&store, &reference, &[], &no_files).expect("hints");
        assert_eq!(hints.uninstall_key, None);
        assert_eq!(hints.extra_dirs, vec![folder]);
    }

    #[test]
    fn an_arp_package_that_is_not_listed_has_no_hints() {
        let store = store_with(vec![arp_source()]);
        let reference = PackageRef {
            source: SourceKind::Arp,
            id: r"HKLM\Nothing".to_string(),
        };
        assert_eq!(for_package(&store, &reference, &[], &no_files), None);
    }

    #[test]
    fn a_linux_source_has_no_hints_yet() {
        let store = store_with(vec![arp_source()]);
        let reference = PackageRef {
            source: SourceKind::Pacman,
            id: "steam".to_string(),
        };
        assert_eq!(for_package(&store, &reference, &[], &no_files), None);
    }
}
