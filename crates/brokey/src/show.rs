//! `brokey --show <source>:<id>`: another application hands the user to
//! Brokey for one installed application, with Remove ready.
//!
//! `docs/brokey-handoff.md` is the contract and Heft keeps a copy of it, so
//! what this module does is what that file says, and a change here that
//! changes the meaning of something there is a change to another
//! application's behaviour as well.
//!
//! Three halves, all here so the shared files carry one line each:
//!
//! - [`argument`] reads the command line. `main` asks it before Tauri starts,
//!   so a reference that is not understood exits 2 with a sentence and opens
//!   no window; the single-instance callback asks it of the arguments a
//!   second `brokey` forwarded.
//! - [`Pending`] holds the reference until the page takes it. The page asks
//!   with `show_take` when it loads and again whenever `brokey://show`
//!   arrives, so a reference is taken exactly once whichever comes first: an
//!   event sent before the page listened is not lost, because the reference
//!   is still waiting, and a page that asks twice gets nothing the second
//!   time.
//! - [`resolve`] finds the installed application holding the reference. It is
//!   a pure function of the installed list, and `show_resolve` hands it that
//!   list. Nothing here ever starts a plan: the page focuses Remove and the
//!   user presses it or does not.

use std::sync::Mutex;

use brokey_core::{App, PackageRef, SourceKind};
use serde::Serialize;

use crate::cli::parse_ref;
use crate::state::AppState;

/// The flag. One reference follows it and nothing else does.
pub const FLAG: &str = "--show";

/// What the running window is sent when a second `brokey --show` hands it a
/// reference. It carries nothing: the reference waits in [`Pending`] and the
/// page takes it, so the event is only a knock on the door.
pub const SHOW_EVENT: &str = "brokey://show";

/// What `--show` was given, read from the arguments after the program name.
///
/// `None` when the command line does not begin with `--show`, which is every
/// other launch. `Some(Err)` is a sentence for standard error, and the
/// process exits 2 with it: no colon, an unknown source word, an empty id,
/// no reference at all, or more than one argument after the flag.
pub fn argument(args: &[String]) -> Option<Result<PackageRef, String>> {
    let (first, rest) = args.split_first()?;
    if first != FLAG {
        return None;
    }
    Some(match rest {
        [] => Err(
            "--show needs a package reference, for example brokey --show winget:Valve.Steam."
                .to_string(),
        ),
        [reference] => parse_ref(reference),
        _ => Err(format!(
            "--show takes one package reference and was given {} arguments. Quote a reference that contains spaces.",
            rest.len()
        )),
    })
}

/// The reference waiting for the page, if any.
#[derive(Default)]
pub struct Pending(Mutex<Option<PackageRef>>);

impl Pending {
    pub fn new(reference: Option<PackageRef>) -> Pending {
        Pending(Mutex::new(reference))
    }

    /// Replace whatever was waiting: a second hand-off before the page took
    /// the first means the caller has moved on.
    pub fn put(&self, reference: PackageRef) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(reference);
    }

    /// Take it, leaving nothing behind.
    pub fn take(&self) -> Option<PackageRef> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// The page an installed reference lands on: the application, grouped as
/// Installed groups it, and which of its editions to select.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Shown {
    pub app: App,
    /// The edition's own reference, spelt as the installed list spells it,
    /// which can differ from what the caller sent in case or, for `msix`, in
    /// being the family name rather than the full name.
    pub edition: PackageRef,
}

/// A package full name reduced to its family name; anything else unchanged.
///
/// A full name is `Name_Version_Architecture_ResourceId_PublisherId`, the
/// folder name under `WindowsApps`, and the resource id is usually empty, so
/// `Microsoft.WindowsTerminal_1.21.2361.0_x64__8wekyb3d8bbwe` has five parts
/// with an empty fourth. The family name is the first and the last:
/// `Microsoft.WindowsTerminal_8wekyb3d8bbwe`. A package name cannot contain
/// an underscore, so splitting on it is exact.
pub fn family_name(id: &str) -> String {
    match id.split('_').collect::<Vec<_>>()[..] {
        [name, _version, _architecture, _resource, publisher]
            if !name.is_empty() && !publisher.is_empty() =>
        {
            format!("{name}_{publisher}")
        }
        _ => id.to_string(),
    }
}

/// Whether `wanted` names the installed id `have` for this source.
///
/// Windows ids are compared without regard to case, because the registry,
/// winget, Chocolatey and Scoop all treat them that way and a caller reading
/// one from a folder name may not have the installed spelling. `msix` also
/// takes a full name for its family name. Every other source compares
/// exactly, as its package manager does.
fn same_id(source: SourceKind, wanted: &str, have: &str) -> bool {
    match source {
        SourceKind::Msix => family_name(wanted).eq_ignore_ascii_case(&family_name(have)),
        SourceKind::Arp | SourceKind::Winget | SourceKind::Choco | SourceKind::Scoop => {
            wanted.eq_ignore_ascii_case(have)
        }
        _ => wanted == have,
    }
}

/// The installed application holding `reference`, and its edition.
///
/// Brokey groups editions, so a reference to any installed edition lands on
/// the application that holds it: an `arp` reference for something winget
/// also knows opens the same page as the `winget` one would. Only installed
/// editions count; a search result that happens to share a name is not
/// something to remove.
pub fn resolve(reference: &PackageRef, apps: &[App]) -> Option<Shown> {
    apps.iter().find_map(|app| {
        app.editions
            .iter()
            .map(|e| &e.package)
            .find(|p| {
                p.installed
                    && p.source == reference.source
                    && same_id(p.source, &reference.id, &p.id)
            })
            .map(|p| Shown {
                app: app.clone(),
                edition: p.reference(),
            })
    })
}

/// The reference waiting for the page, taken. `None` almost always: only a
/// launch with `--show`, or a hand-off from a second `brokey --show`, leaves
/// one.
#[tauri::command]
pub fn show_take(pending: tauri::State<'_, Pending>) -> Option<PackageRef> {
    pending.take()
}

/// The page for an installed reference, or `None` when nothing installed
/// matches it, in which case the page opens Installed with a notice.
#[tauri::command]
pub async fn show_resolve(
    state: tauri::State<'_, AppState>,
    reference: PackageRef,
) -> Result<Option<Shown>, String> {
    let state = state.inner().clone();
    match tauri::async_runtime::spawn_blocking(move || {
        resolve(&reference, &state.store().installed().apps)
    })
    .await
    {
        Ok(found) => Ok(found),
        Err(e) => Err(format!(
            "The store's worker stopped before it answered: {e}. Try again."
        )),
    }
}

/// What the single-instance callback does with a second launch's arguments:
/// a `--show` reference waits for the page and the page is told. The first
/// argument is the second process's own path, which is skipped.
pub fn hand_over(app: &tauri::AppHandle, forwarded: &[String]) {
    use tauri::{Emitter, Manager};
    let Some(Ok(reference)) = argument(forwarded.get(1..).unwrap_or_default()) else {
        return;
    };
    app.state::<Pending>().put(reference);
    if let Err(e) = app.emit(SHOW_EVENT, ()) {
        log::warn!("could not send {SHOW_EVENT} to the page: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::{Pending, argument, family_name, resolve};
    use brokey_core::{App, Edition, MatchedBy, Package, PackageKind, PackageRef, SourceKind};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn reference(source: SourceKind, id: &str) -> PackageRef {
        PackageRef {
            source,
            id: id.to_string(),
        }
    }

    fn installed(source: SourceKind, id: &str, name: &str) -> Package {
        let mut p = Package::new(source, id, name);
        p.installed = true;
        p
    }

    fn app(key: &str, packages: Vec<Package>) -> App {
        App {
            key: key.to_string(),
            name: packages[0].name.clone(),
            kind: PackageKind::App,
            summary: None,
            icon: None,
            developer: None,
            categories: Vec::new(),
            installed: packages.iter().any(|p| p.installed),
            updated: None,
            popularity: None,
            relevance: 1.0,
            editions: packages
                .into_iter()
                .map(|package| Edition {
                    package,
                    matched_by: MatchedBy::Name,
                    confidence: 0.8,
                })
                .collect(),
        }
    }

    /// An Installed page in miniature: Steam known to both Add/Remove
    /// Programs and winget and grouped as one application, 7-Zip from
    /// Add/Remove Programs alone under a ProductCode, Terminal from the
    /// Store, and GIMP on Linux for the exact comparison.
    fn machine() -> Vec<App> {
        vec![
            app(
                "name:steam",
                vec![
                    installed(SourceKind::Winget, "Valve.Steam", "Steam"),
                    installed(SourceKind::Arp, "HKLM32\\Steam", "Steam"),
                ],
            ),
            app(
                "name:7zip",
                vec![installed(
                    SourceKind::Arp,
                    "HKLM\\{23170F69-40C1-2701-2201-000001000000}",
                    "7-Zip",
                )],
            ),
            app(
                "name:windowsterminal",
                vec![installed(
                    SourceKind::Msix,
                    "Microsoft.WindowsTerminal_8wekyb3d8bbwe",
                    "Windows Terminal",
                )],
            ),
            app(
                "org.gimp.GIMP",
                vec![
                    installed(SourceKind::Pacman, "gimp", "GIMP"),
                    Package::new(
                        SourceKind::Flatpak,
                        "flathub/app/org.gimp.GIMP/x86_64/stable",
                        "GIMP",
                    ),
                ],
            ),
        ]
    }

    // ------------------------------------------------------------ arguments

    #[test]
    fn a_launch_without_the_flag_is_not_a_hand_off() {
        assert_eq!(argument(&args(&[])), None);
        assert_eq!(argument(&args(&["search", "steam"])), None);
        // The flag counts only where the contract puts it, first.
        assert_eq!(argument(&args(&["search", "--show"])), None);
    }

    #[test]
    fn the_flag_and_a_reference_are_understood() {
        assert_eq!(
            argument(&args(&["--show", "winget:Valve.Steam"])),
            Some(Ok(reference(SourceKind::Winget, "Valve.Steam")))
        );
        // Everything after the first colon is the id, backslashes and all.
        assert_eq!(
            argument(&args(&[
                "--show",
                "arp:HKLM\\{23170F69-40C1-2701-2201-000001000000}"
            ])),
            Some(Ok(reference(
                SourceKind::Arp,
                "HKLM\\{23170F69-40C1-2701-2201-000001000000}"
            )))
        );
        assert_eq!(
            argument(&args(&[
                "--show",
                "flatpak:flathub/app/org.gimp.GIMP/x86_64/stable"
            ])),
            Some(Ok(reference(
                SourceKind::Flatpak,
                "flathub/app/org.gimp.GIMP/x86_64/stable"
            )))
        );
    }

    /// Each of these is exit 2 in `main`, with the sentence on standard
    /// error. The contract names the first three.
    #[test]
    fn a_reference_that_is_not_understood_is_a_sentence() {
        for bad in [
            vec!["--show", "steam"],
            vec!["--show", "nix:steam"],
            vec!["--show", "winget:"],
            vec!["--show"],
            vec!["--show", "winget:Valve.Steam", "extra"],
        ] {
            let sentence = argument(&args(&bad))
                .expect("the flag is there")
                .expect_err("and what follows it is wrong");
            assert!(sentence.ends_with('.'), "{bad:?}: {sentence}");
            assert!(!sentence.contains('\u{2014}'), "{bad:?}: {sentence}");
        }
    }

    // ------------------------------------------------------------ resolving

    #[test]
    fn an_exact_reference_lands_on_its_application() {
        let shown = resolve(&reference(SourceKind::Winget, "Valve.Steam"), &machine()).unwrap();
        assert_eq!(shown.app.key, "name:steam");
        assert_eq!(shown.edition, reference(SourceKind::Winget, "Valve.Steam"));

        let shown = resolve(
            &reference(
                SourceKind::Arp,
                "HKLM\\{23170F69-40C1-2701-2201-000001000000}",
            ),
            &machine(),
        )
        .unwrap();
        assert_eq!(shown.app.key, "name:7zip");
    }

    /// The contract's promise: the caller sends whichever reference it found,
    /// and either opens the same page, with that edition selected.
    #[test]
    fn a_grouped_edition_lands_on_the_same_page_as_its_sibling() {
        let by_arp = resolve(&reference(SourceKind::Arp, "HKLM32\\Steam"), &machine()).unwrap();
        let by_winget = resolve(&reference(SourceKind::Winget, "Valve.Steam"), &machine()).unwrap();
        assert_eq!(by_arp.app.key, by_winget.app.key);
        assert_eq!(by_arp.edition, reference(SourceKind::Arp, "HKLM32\\Steam"));
    }

    /// The registry and winget fold case, and a caller reading a key name
    /// from elsewhere may not have the installed spelling. The edition comes
    /// back as the installed list spells it, which is what the page selects
    /// by.
    #[test]
    fn windows_ids_match_without_regard_to_case() {
        let shown = resolve(&reference(SourceKind::Arp, "hklm32\\STEAM"), &machine()).unwrap();
        assert_eq!(shown.edition, reference(SourceKind::Arp, "HKLM32\\Steam"));
        assert!(resolve(&reference(SourceKind::Winget, "valve.steam"), &machine()).is_some());
    }

    #[test]
    fn linux_ids_match_exactly() {
        assert!(resolve(&reference(SourceKind::Pacman, "gimp"), &machine()).is_some());
        assert!(resolve(&reference(SourceKind::Pacman, "GIMP"), &machine()).is_none());
    }

    #[test]
    fn a_package_full_name_reduces_to_its_family_name() {
        assert_eq!(
            family_name("Microsoft.WindowsTerminal_1.21.2361.0_x64__8wekyb3d8bbwe"),
            "Microsoft.WindowsTerminal_8wekyb3d8bbwe"
        );
        // With a resource id, which some framework packages carry.
        assert_eq!(
            family_name("Microsoft.VCLibs.140.00_14.0.33519.0_x64_split.scale-100_8wekyb3d8bbwe"),
            "Microsoft.VCLibs.140.00_8wekyb3d8bbwe"
        );
        // A family name is left as it is, and so is anything else.
        for unchanged in [
            "Microsoft.WindowsTerminal_8wekyb3d8bbwe",
            "Spotify",
            "a_b_c",
        ] {
            assert_eq!(family_name(unchanged), unchanged);
        }

        let shown = resolve(
            &reference(
                SourceKind::Msix,
                "Microsoft.WindowsTerminal_1.21.2361.0_x64__8wekyb3d8bbwe",
            ),
            &machine(),
        )
        .unwrap();
        assert_eq!(
            shown.edition,
            reference(SourceKind::Msix, "Microsoft.WindowsTerminal_8wekyb3d8bbwe")
        );
    }

    #[test]
    fn nothing_installed_matching_is_none() {
        // Removed since the caller looked.
        assert!(
            resolve(
                &reference(SourceKind::Winget, "Mozilla.Firefox"),
                &machine()
            )
            .is_none()
        );
        // The same id under another source is a different package.
        assert!(resolve(&reference(SourceKind::Choco, "Valve.Steam"), &machine()).is_none());
        // An edition that is only available, not installed, is not something to remove.
        assert!(
            resolve(
                &reference(
                    SourceKind::Flatpak,
                    "flathub/app/org.gimp.GIMP/x86_64/stable"
                ),
                &machine()
            )
            .is_none()
        );
    }

    #[test]
    fn a_waiting_reference_is_taken_once() {
        let pending = Pending::new(Some(reference(SourceKind::Winget, "Valve.Steam")));
        assert_eq!(
            pending.take(),
            Some(reference(SourceKind::Winget, "Valve.Steam"))
        );
        assert_eq!(pending.take(), None);
        pending.put(reference(SourceKind::Arp, "HKCU\\Discord"));
        pending.put(reference(SourceKind::Arp, "HKLM32\\Steam"));
        assert_eq!(
            pending.take(),
            Some(reference(SourceKind::Arp, "HKLM32\\Steam"))
        );
    }
}
