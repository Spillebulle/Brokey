use super::*;
#[cfg(windows)]
use crate::cleanup::holding::{Held, Manifest};
use crate::cleanup::view::FixtureView;
use crate::model::{Command, SourceKind};

fn step(program: &str, args: Vec<String>, root: bool) -> Step {
    Step {
        source: SourceKind::Arp,
        title: "Test".to_string(),
        command: Command {
            program: program.to_string(),
            args,
            env: Vec::new(),
            cwd: None,
        },
        needs_root: root,
        weight: 1,
    }
}

fn item(kind: ItemKind, location: &str) -> HoldItem {
    HoldItem {
        kind,
        location: location.to_string(),
    }
}

fn hold(id: &str, items: Vec<HoldItem>, root: bool) -> Step {
    let payload = HoldPayload {
        holding: id.to_string(),
        name: "Acme Studio".to_string(),
        items,
    };
    step(HOLD, vec![serde_json::to_string(&payload).unwrap()], root)
}

/// Answers every question about the disk and the registry one way.
struct Answer(Part);

impl Machine for Answer {
    fn part(&self, _: &str) -> Part {
        self.0
    }

    fn key(&self, _: Hive, _: &str) -> Part {
        self.0
    }
}

const PLAIN: Answer = Answer(Part::Plain);

fn temp_roots(temp: &Path) -> Roots {
    Roots {
        user: temp.join("user-holding"),
        machine: temp.join("machine-holding"),
    }
}

/// Run a step the way the runner does, against temporary roots, with every
/// event it sends kept.
fn run_here(step: &Step, roots: &Roots) -> (Result<Option<String>, String>, Vec<Event>) {
    let mut events = Vec::new();
    let mut sink = |e: Event| events.push(e);
    let never = || false;
    let mut ctx = InternalContext {
        plan: "p",
        step: 0,
        sink: &mut sink,
        cancelled: &never,
        finished: Arc::new(AtomicBool::new(false)),
        scans: Scans::default(),
    };
    #[cfg(windows)]
    let result = {
        let mut m = holding::LiveMutate::new(roots.machine.clone());
        run_in(
            step,
            &mut ctx,
            roots,
            &super::super::view::LiveView::new(),
            &mut m,
            &Live,
            &FakeSession::default(),
        )
    };
    #[cfg(not(windows))]
    let result = run(step, &mut ctx);
    let _ = roots;
    (result, events)
}

// ---------------------------------------------------------------------------
// A hold is checked as a set

/// A machine where `.acme` names the ProgID `Acme.Doc`, which is there.
fn classes() -> FixtureView {
    FixtureView::from_json(
        r#"{
            "registry": {
                "Hklm": {
                    "Software\\Classes\\.acme": { "values": [["", "Acme.Doc"]] },
                    "Software\\Classes\\Acme.Doc": { "subkeys": ["shell"] },
                    "Software\\Classes\\.other": { "values": [["", "Other.Doc"]] },
                    "Software\\Classes\\Other.Doc": { "subkeys": [] }
                }
            }
        }"#,
    )
}

const EXT: &str = r"HKLM\Software\Classes\.acme";
const PROGID: &str = r"HKLM\Software\Classes\Acme.Doc";

#[test]
fn an_extension_whose_progid_stays_is_refused() {
    let view = classes();
    let roots = Roots::live();
    let err = check_with(
        &hold("abc", vec![item(ItemKind::RegistryKey, EXT)], true),
        &view,
        &roots,
        &PLAIN,
    )
    .expect_err("Acme.Doc is still there and not going");
    assert!(err.contains("never clears"), "{err}");
}

#[test]
fn an_extension_whose_progid_goes_with_it_is_allowed() {
    let view = classes();
    let roots = Roots::live();
    for items in [
        vec![
            item(ItemKind::RegistryKey, EXT),
            item(ItemKind::RegistryKey, PROGID),
        ],
        vec![
            item(ItemKind::RegistryKey, PROGID),
            item(ItemKind::RegistryKey, EXT),
        ],
    ] {
        assert_eq!(
            check_with(&hold("abc", items, true), &view, &roots, &PLAIN),
            Ok(())
        );
    }
    // Another extension in the same set is still asked about its own.
    let err = check_with(
        &hold(
            "abc",
            vec![
                item(ItemKind::RegistryKey, EXT),
                item(ItemKind::RegistryKey, PROGID),
                item(ItemKind::RegistryKey, r"HKLM\Software\Classes\.other"),
            ],
            true,
        ),
        &view,
        &roots,
        &PLAIN,
    )
    .expect_err("Other.Doc stays");
    assert!(err.contains(".other"), "{err}");
}

#[test]
fn progid_keys_are_moved_before_the_extensions_that_name_them() {
    let items = vec![
        item(ItemKind::RegistryKey, EXT),
        item(
            ItemKind::RegistryValue,
            r"HKLM\Software\Classes\.acme\OpenWithProgids | Acme.Doc",
        ),
        item(ItemKind::RegistryKey, PROGID),
        item(ItemKind::Folder, r"C:\Program Files\Acme"),
        item(ItemKind::RegistryKey, r"HKLM32\Software\Classes\.acme"),
        item(
            ItemKind::RegistryKey,
            r"HKLM\Software\WOW6432Node\Classes\.acme",
        ),
    ];
    let order: Vec<String> = in_order(&items).into_iter().map(|i| i.location).collect();
    assert_eq!(
        order,
        vec![
            PROGID.to_string(),
            r"C:\Program Files\Acme".to_string(),
            EXT.to_string(),
            r"HKLM\Software\Classes\.acme\OpenWithProgids | Acme.Doc".to_string(),
            r"HKLM32\Software\Classes\.acme".to_string(),
            r"HKLM\Software\WOW6432Node\Classes\.acme".to_string(),
        ]
    );
}

#[test]
fn a_session_hold_takes_only_hkcu_keys_and_never_a_service() {
    let view = classes();
    let roots = Roots::live();
    for refused in [
        item(ItemKind::RegistryKey, r"HKLM\Software\Acme"),
        item(ItemKind::RegistryKey, r"HKLM32\Software\Acme"),
        item(ItemKind::Service, "service:AcmeUpdate"),
        item(ItemKind::Task, r"task:\Acme\Update"),
        item(ItemKind::FirewallRule, "firewall:Acme | {a}"),
    ] {
        let location = refused.location.clone();
        let err = check_with(&hold("abc", vec![refused], false), &view, &roots, &PLAIN)
            .expect_err("not a session item");
        assert!(err.contains(&location), "{err}");
    }
    assert_eq!(
        check_with(
            &hold(
                "abc",
                vec![item(ItemKind::RegistryKey, r"HKCU\Software\Acme")],
                false
            ),
            &view,
            &roots,
            &PLAIN
        ),
        Ok(())
    );
}

#[test]
fn a_key_and_a_value_must_say_which_they_are() {
    let view = classes();
    let roots = Roots::live();
    for wrong in [
        item(ItemKind::RegistryKey, r"HKLM\Software\Acme | Value"),
        item(ItemKind::RegistryValue, r"HKLM\Software\Acme"),
        item(ItemKind::RegistryKey, r"HKLM\Software\..\SYSTEM"),
        item(ItemKind::RegistryKey, r"HKLM\Software\\Acme"),
        item(ItemKind::RegistryKey, r"HKU\S-1-5-18\Software\Acme"),
        item(ItemKind::RegistryKey, "HKLM"),
    ] {
        assert!(
            check_with(
                &hold("abc", vec![wrong.clone()], true),
                &view,
                &roots,
                &PLAIN
            )
            .is_err(),
            "{wrong:?}"
        );
    }
}

/// A service is cleared only when the program it runs is an
/// application's, and the list it is looked for in was read whole.
#[test]
fn a_service_that_runs_windows_or_cannot_be_placed_is_refused() {
    let view = FixtureView::from_json(
        r#"{
            "folders": {
                "Windows": "C:\\Windows",
                "ProgramFiles": "C:\\Program Files",
                "ProgramFilesX86": "C:\\Program Files (x86)",
                "ProgramData": "C:\\ProgramData",
                "AppData": "C:\\Users\\me\\AppData\\Roaming",
                "LocalAppData": "C:\\Users\\me\\AppData\\Local",
                "LocalLow": "C:\\Users\\me\\AppData\\LocalLow",
                "Profile": "C:\\Users\\me",
                "StartMenuMachine": "C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs",
                "StartMenuUser": "C:\\Users\\me\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs"
            },
            "services": [
                { "name": "AcmeUpdate", "image_path": "\"C:\\Program Files\\Acme\\update.exe\" /service" },
                { "name": "Shared", "image_path": "C:\\Windows\\System32\\svchost.exe -k netsvcs" },
                { "name": "Driver", "image_path": "System32\\drivers\\acme.sys" },
                { "name": "Rooted", "image_path": "\\SystemRoot\\System32\\drivers\\acme.sys" },
                { "name": "Unset", "image_path": "%UNKNOWN%\\acme.exe" }
            ],
            "firewall": [
                { "id": "{a}", "name": "Acme", "program": "C:\\Program Files\\Acme\\acme.exe" },
                { "id": "{t1}", "name": "Twice", "program": "C:\\Program Files\\Acme\\acme.exe" },
                { "id": "{t2}", "name": "Twice", "program": "%SystemRoot%\\System32\\svchost.exe" }
            ]
        }"#,
    );
    let roots = Roots::live();
    let ok = |location: &str, kind: ItemKind| {
        check_with(
            &hold("abc", vec![item(kind, location)], true),
            &view,
            &roots,
            &PLAIN,
        )
    };
    assert_eq!(ok("service:AcmeUpdate", ItemKind::Service), Ok(()));
    assert_eq!(ok("service:Gone", ItemKind::Service), Ok(()));
    for name in ["Shared", "Driver", "Rooted", "Unset"] {
        let err = ok(&format!("service:{name}"), ItemKind::Service).expect_err(name);
        assert!(err.contains("Windows's own"), "{err}");
    }
    assert_eq!(ok("firewall:Acme | {a}", ItemKind::FirewallRule), Ok(()));
    // Two rules share a name; only the one by this id goes, so only its
    // program is asked about.
    assert_eq!(ok("firewall:Twice | {t1}", ItemKind::FirewallRule), Ok(()));
    let err = ok("firewall:Twice | {t2}", ItemKind::FirewallRule).expect_err("svchost");
    assert!(err.contains("Windows's own"), "{err}");
    // A rule by its name alone, or by a name that is not the id's, is not
    // one Brokey offered.
    for location in ["firewall:Twice", "firewall:Acme | {t1}", "firewall:Acme | "] {
        let err = ok(location, ItemKind::FirewallRule).expect_err(location);
        assert!(err.contains("firewall rule"), "{err}");
    }

    let unlisted = FixtureView::from_json(r#"{ "unreadable_services": 1 }"#);
    let err = check_with(
        &hold("abc", vec![item(ItemKind::Service, "service:Gone")], true),
        &unlisted,
        &roots,
        &PLAIN,
    )
    .expect_err("it may be among the ones that could not be read");
    assert!(err.contains("could not be read"), "{err}");
}

// ---------------------------------------------------------------------------
// The Windows Installer

const CODE: &str = "{12345678-ABCD-EF01-2345-6789ABCDEF01}";
const PACKED: &str = "87654321DCBA10FE32547698BADCFE10";
const OTHER: &str = "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF";

fn installer(component_unreadable: bool) -> FixtureView {
    FixtureView::from_json(&format!(
        r#"{{
            "registry": {{
                "Hklm": {{
                    "Software\\Classes\\Installer\\Products\\{PACKED}": {{ "values": [["ProductName", "Acme"]] }},
                    "Software\\Classes\\Installer\\Products\\{OTHER}": {{ "values": [["ProductName", "Other"]] }},
                    "Software\\Microsoft\\Windows\\CurrentVersion\\Installer\\UserData": {{ "subkeys": ["S-1-5-18"] }},
                    "Software\\Microsoft\\Windows\\CurrentVersion\\Installer\\UserData\\S-1-5-18\\Products\\{PACKED}": {{ "subkeys": ["InstallProperties"] }},
                    "Software\\Microsoft\\Windows\\CurrentVersion\\Installer\\UserData\\S-1-5-18\\Products\\{OTHER}": {{ }},
                    "Software\\Microsoft\\Windows\\CurrentVersion\\Installer\\UserData\\S-1-5-18\\Components": {{ "subkeys": ["AAAA0000AAAA0000AAAA0000AAAA0000", "BBBB0000BBBB0000BBBB0000BBBB0000", "CCCC0000CCCC0000CCCC0000CCCC0000"] }},
                    "Software\\Microsoft\\Windows\\CurrentVersion\\Installer\\UserData\\S-1-5-18\\Components\\AAAA0000AAAA0000AAAA0000AAAA0000": {{ "values": [["{PACKED}", "C:\\Program Files\\Acme\\acme.exe"]] }},
                    "Software\\Microsoft\\Windows\\CurrentVersion\\Installer\\UserData\\S-1-5-18\\Components\\BBBB0000BBBB0000BBBB0000BBBB0000": {{ "values": [["{PACKED}", "C:\\Program Files\\Common Files\\acme.dll"], ["{OTHER}", "C:\\Program Files\\Common Files\\acme.dll"]] }},
                    "Software\\Microsoft\\Windows\\CurrentVersion\\Installer\\UserData\\S-1-5-18\\Components\\CCCC0000CCCC0000CCCC0000CCCC0000": {{ "values": [["{OTHER}", "C:\\Program Files\\Other\\other.exe"]], "unreadable": {component_unreadable} }}
                }}
            }}
        }}"#
    ))
}

#[test]
fn the_packed_code_is_what_the_installer_names_keys_by() {
    assert_eq!(guard::packed_product_code(CODE).as_deref(), Some(PACKED));
}

#[test]
fn unregistering_takes_only_what_names_the_product() {
    let items = msi_items(CODE, &installer(false)).unwrap();
    let base = r"HKLM\Software\Microsoft\Windows\CurrentVersion\Installer\UserData\S-1-5-18";
    assert_eq!(
        items,
        vec![
            item(
                ItemKind::RegistryKey,
                &format!(r"HKLM\Software\Classes\Installer\Products\{PACKED}")
            ),
            item(ItemKind::RegistryKey, &format!(r"{base}\Products\{PACKED}")),
            // Only this product's value was in it, so the key goes.
            item(
                ItemKind::RegistryKey,
                &format!(r"{base}\Components\AAAA0000AAAA0000AAAA0000AAAA0000")
            ),
            // Another product still uses it: only this product's value goes.
            item(
                ItemKind::RegistryValue,
                &format!(r"{base}\Components\BBBB0000BBBB0000BBBB0000BBBB0000 | {PACKED}")
            ),
        ]
    );
    assert!(
        items
            .iter()
            .all(|i| !i.location.contains(OTHER) || i.location.ends_with(PACKED)),
        "nothing of the other product's is taken"
    );
    // Every one is a shape a restore will put back.
    assert!(items.iter().all(msi_registration), "{items:?}");
}

#[test]
fn a_registration_that_cannot_be_read_whole_is_left_alone() {
    let err = msi_items(CODE, &installer(true)).expect_err("a component could not be read");
    assert!(err.contains("could not be read"), "{err}");
    assert!(err.ends_with('.'));
}

#[test]
fn only_the_installers_own_shapes_are_put_back_past_the_guard() {
    let base = r"HKLM\Software\Microsoft\Windows\CurrentVersion\Installer\UserData\S-1-5-18";
    for shape in [
        item(ItemKind::RegistryKey, &format!(r"{base}\Products")),
        item(
            ItemKind::RegistryKey,
            &format!(r"{base}\Products\{PACKED}\Sub"),
        ),
        item(ItemKind::RegistryKey, &format!(r"{base}\Components")),
        item(ItemKind::RegistryKey, r"HKLM\Software\Classes\Installer"),
        item(
            ItemKind::RegistryKey,
            r"HKLM\Software\Classes\Installer\Products\XYZ",
        ),
        item(
            ItemKind::RegistryValue,
            &format!(r"{base}\Components\AAAA0000AAAA0000AAAA0000AAAA0000 | Other"),
        ),
        item(
            ItemKind::RegistryKey,
            &format!(r"HKCU\Software\Classes\Installer\Products\{PACKED}"),
        ),
    ] {
        assert!(!msi_registration(&shape), "{shape:?}");
    }
}

// ---------------------------------------------------------------------------
// Restore re-checks every destination

#[cfg(windows)]
fn manifest_with(roots: &Roots, id: &str, items: Vec<HoldItem>) {
    let dir = roots.user.join(id);
    std::fs::create_dir_all(dir.join("items")).unwrap();
    let manifest = Manifest {
        id: id.to_string(),
        name: "Acme Studio".to_string(),
        created: "2026-11-24T00:00:00Z".to_string(),
        items: items
            .into_iter()
            .enumerate()
            .map(|(n, item)| Held {
                item,
                stored: format!("items/{}", n + 1),
                state: HeldState::Held,
                covered_by: None,
                size: 0,
            })
            .collect(),
        next: 1,
    };
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

#[cfg(windows)]
#[test]
fn a_restore_into_a_protected_place_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let roots = temp_roots(temp.path());
    let windows = std::env::var("SystemRoot").unwrap();
    let view = super::super::view::LiveView::new();
    let fine = temp.path().join("was-here").to_string_lossy().into_owned();
    for bad in [
        item(ItemKind::Folder, &format!(r"{windows}\System32\acme")),
        item(ItemKind::Folder, &format!(r"{windows}\System32")),
        item(ItemKind::Folder, r"relative\acme"),
        item(ItemKind::RegistryKey, r"HKLM\Software\Acme"),
        item(ItemKind::RegistryKey, r"HKCU\Software\Microsoft\Windows"),
    ] {
        manifest_with(
            &roots,
            "acme",
            vec![item(ItemKind::Folder, &fine), bad.clone()],
        );
        let err = check(&step(RESTORE, vec!["acme".into()], false), &view, &roots)
            .expect_err("a destination the rules refuse");
        assert!(err.contains("put") && err.contains(&bad.location), "{err}");
    }
    manifest_with(&roots, "acme", vec![item(ItemKind::Folder, &fine)]);
    assert_eq!(
        check(&step(RESTORE, vec!["acme".into()], false), &view, &roots),
        Ok(()),
        "a place that is simply not there any more is where a restore puts things"
    );
    // A record that names another entry is not this entry's record.
    let mut other = holding::read_manifest(&roots.user.join("acme"))
        .unwrap()
        .unwrap();
    other.id = "other".to_string();
    std::fs::write(
        roots.user.join("acme").join("manifest.json"),
        serde_json::to_vec(&other).unwrap(),
    )
    .unwrap();
    assert!(check(&step(RESTORE, vec!["acme".into()], false), &view, &roots).is_err());
    // Nor is there one for an entry that is not there.
    assert!(check(&step(RESTORE, vec!["nothing".into()], false), &view, &roots).is_err());
}

#[cfg(windows)]
#[test]
fn a_restore_through_a_junction_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let roots = temp_roots(temp.path());
    let elsewhere = temp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let link = temp.path().join("app");
    junction(&link, &elsewhere);
    let through = link.join("settings").to_string_lossy().into_owned();
    manifest_with(&roots, "acme", vec![item(ItemKind::Folder, &through)]);
    let err = check(
        &step(RESTORE, vec!["acme".into()], false),
        &super::super::view::LiveView::new(),
        &roots,
    )
    .expect_err("the way back leads somewhere else");
    assert!(err.contains("junction"), "{err}");
}

// ---------------------------------------------------------------------------
// Running

#[cfg(windows)]
fn junction(link: &Path, target: &Path) {
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(windows)]
#[test]
fn a_session_hold_moves_and_a_restore_puts_back() {
    let temp = tempfile::tempdir().unwrap();
    let roots = temp_roots(temp.path());
    let app = temp.path().join("Acme Studio");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("settings.ini"), "x").unwrap();
    let location = app.to_string_lossy().into_owned();

    let (result, _) = run_here(
        &hold("acme-1", vec![item(ItemKind::Folder, &location)], false),
        &roots,
    );
    assert_eq!(result, Ok(None));
    assert!(!app.exists(), "moved");
    let manifest = holding::read_manifest(&roots.user.join("acme-1"))
        .unwrap()
        .expect("recorded");
    assert_eq!(manifest.items[0].item.location, location);
    assert!(
        !roots.machine.exists(),
        "a session step never touches the machine's root"
    );

    let (result, _) = run_here(&step(RESTORE, vec!["acme-1".into()], false), &roots);
    assert_eq!(result, Ok(None));
    assert_eq!(
        std::fs::read_to_string(app.join("settings.ini")).unwrap(),
        "x"
    );

    let (result, _) = run_here(&step(EMPTY, vec!["acme-1".into()], false), &roots);
    assert!(
        result.is_err(),
        "restored entries are gone, so there is nothing to empty"
    );
}

/// The check is asked again right before the move, so a folder swapped for
/// a junction after the closed list passed the plan is still refused, and
/// nothing in the step moves.
#[cfg(windows)]
#[test]
fn a_hold_rechecks_right_before_it_moves() {
    let temp = tempfile::tempdir().unwrap();
    let roots = temp_roots(temp.path());
    let first = temp.path().join("first");
    std::fs::create_dir_all(&first).unwrap();
    let swapped = temp.path().join("swapped");
    let elsewhere = temp.path().join("elsewhere");
    std::fs::create_dir_all(elsewhere.join("data")).unwrap();
    junction(&swapped, &elsewhere);
    let (result, _) = run_here(
        &hold(
            "acme-1",
            vec![
                item(ItemKind::Folder, &first.to_string_lossy()),
                item(ItemKind::Folder, &swapped.join("data").to_string_lossy()),
            ],
            false,
        ),
        &roots,
    );
    let err = result.expect_err("refused");
    assert!(err.contains("junction"), "{err}");
    assert!(
        first.is_dir() && elsewhere.join("data").is_dir(),
        "nothing moved"
    );
    assert!(!roots.user.join("acme-1").exists());
}

/// The machine's root is made Administrators-only before anything is read
/// from it, and that starts with `%ProgramData%\Brokey` above it: a
/// junction there, which a standard user could have made before Brokey
/// did, refuses the step before anything happens.
#[cfg(windows)]
#[test]
fn a_root_step_refuses_a_machine_root_under_a_junction() {
    let temp = tempfile::tempdir().unwrap();
    let elsewhere = temp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let brokey = temp.path().join("Brokey");
    junction(&brokey, &elsewhere);
    let roots = Roots {
        user: temp.path().join("user-holding"),
        machine: brokey.join("Holding"),
    };
    for root_step in [
        step(RESTORE, vec!["acme".into()], true),
        step(EMPTY, vec!["all".into()], true),
        step(MSI_UNREGISTER, vec![CODE.into(), "acme".into()], true),
        hold(
            "acme",
            vec![item(
                ItemKind::Folder,
                &temp.path().join("x").to_string_lossy(),
            )],
            true,
        ),
    ] {
        let (result, _) = run_here(&root_step, &roots);
        let err = result.expect_err("the machine root is not Brokey's alone");
        assert!(err.contains("not a plain folder"), "{err}");
    }
    assert!(
        !elsewhere.join("Holding").exists(),
        "nothing was made through it"
    );
}

#[test]
fn a_session_step_it_cannot_read_is_refused_and_starts_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let roots = temp_roots(temp.path());
    for program in [AWAIT_GONE, SCAN, SHELL_OPEN] {
        let (result, events) = run_here(&step(program, vec!["{}".into()], false), &roots);
        let err = result.expect_err("an empty payload is nothing Brokey wrote");
        assert!(err.contains(program) && err.ends_with('.'), "{err}");
        assert!(events.is_empty());
    }
}

// ---------------------------------------------------------------------------
// The steps that run as the user

/// The machine as the session steps see it, from a table. `shell_open`
/// records what it was asked and starts nothing.
#[derive(Default)]
struct FakeSession {
    /// Whether the uninstall key is still there, look after look; the last
    /// answer repeats.
    listed: Vec<bool>,
    looks: std::cell::Cell<usize>,
    installed: Vec<crate::model::Package>,
    failed_sources: u32,
    closed_list: Option<Result<(), String>>,
    opened: std::cell::RefCell<Vec<(PathBuf, Vec<String>)>>,
    exit_code: u32,
    /// Stands in for the uninstaller still running: `shell_open` waits on
    /// the cancel flag, as the live one does in its slices.
    still_running: bool,
    /// What `open_steam` was handed.
    steam: std::cell::RefCell<Vec<String>>,
}

impl Session for FakeSession {
    fn listed(&self, _: Hive, _: &str) -> bool {
        let n = self.looks.get();
        self.looks.set(n + 1);
        *self.listed.get(n).or(self.listed.last()).unwrap_or(&false)
    }
    fn poll(&self) -> Duration {
        Duration::from_millis(1)
    }
    fn installed(&self) -> (Vec<crate::model::Package>, u32) {
        (self.installed.clone(), self.failed_sources)
    }
    fn closed_list(&self, _: &Step) -> Result<(), String> {
        self.closed_list.clone().unwrap_or(Ok(()))
    }
    fn shell_open(&self, refused: &Refused, cancelled: &dyn Fn() -> bool) -> Result<u32, String> {
        self.opened
            .borrow_mut()
            .push((refused.program().to_path_buf(), refused.args().to_vec()));
        if self.still_running {
            while !cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            return Err(STOPPED_WAITING.to_string());
        }
        Ok(self.exit_code)
    }
    fn open_steam(&self, url: &str) -> Result<(), String> {
        self.steam.borrow_mut().push(url.to_string());
        Ok(())
    }
}

fn acme_hints() -> Hints {
    Hints {
        package: crate::model::PackageRef {
            source: SourceKind::Arp,
            id: r"HKLM\Acme Studio_is1".to_string(),
        },
        name: "Acme Studio".to_string(),
        publisher: Some("Acme".to_string()),
        uninstall_key: Some((Hive::Hklm, "Acme Studio_is1".to_string())),
        product_code: None,
        install_dir: Some(PathBuf::from(r"C:\Program Files\Acme Studio")),
        install_location: None,
        icon_dir: None,
        shortcuts: Vec::new(),
        extra_dirs: Vec::new(),
    }
}

/// Run one session step against `view` and `session`, with `finished` as
/// the page's "It has finished" and `cancel` as "Stop waiting".
fn run_session(
    step: &Step,
    view: &dyn SystemView,
    session: &FakeSession,
    finished: bool,
    cancel: bool,
) -> (Result<Option<String>, String>, Vec<Event>, Scans) {
    let mut events = Vec::new();
    let scans = Scans::default();
    let flag = Arc::new(AtomicBool::new(false));
    let result = {
        let mut sink = |e: Event| {
            // The page answers once it has been asked.
            if finished && matches!(e, Event::Waiting { .. }) {
                flag.store(true, Ordering::SeqCst);
            }
            events.push(e);
        };
        let cancelled = || cancel;
        let mut ctx = InternalContext {
            plan: "p",
            step: 1,
            sink: &mut sink,
            cancelled: &cancelled,
            finished: flag.clone(),
            scans: scans.clone(),
        };
        let temp = tempfile::tempdir().unwrap();
        let roots = temp_roots(temp.path());
        let mut m = OneKeyInUse {
            in_use: "",
            deleted: Vec::new(),
        };
        run_in(step, &mut ctx, &roots, view, &mut m, &PLAIN, session)
    };
    (result, events, scans)
}

fn await_step() -> Step {
    step(
        AWAIT_GONE,
        vec![serde_json::to_string(&acme_hints()).unwrap()],
        false,
    )
}

#[test]
fn await_gone_with_the_entry_already_gone_goes_straight_on() {
    let session = FakeSession {
        listed: vec![false],
        ..Default::default()
    };
    let (result, events, _) = run_session(
        &await_step(),
        &FixtureView::from_json("{}"),
        &session,
        false,
        false,
    );
    assert_eq!(result, Ok(None));
    assert!(events.is_empty(), "no waiting was needed: {events:?}");
}

#[test]
fn await_gone_waits_while_the_entry_is_there_and_goes_on_when_it_goes() {
    let session = FakeSession {
        listed: vec![true, true, true, false],
        ..Default::default()
    };
    let (result, events, _) = run_session(
        &await_step(),
        &FixtureView::from_json("{}"),
        &session,
        false,
        false,
    );
    assert_eq!(result, Ok(None));
    assert_eq!(
        events,
        vec![Event::Waiting {
            plan: "p".into(),
            step: 1,
            message: WAITING_FOR_UNINSTALLER.into(),
        }]
    );
}

/// Review Focus 3: the user cancels the publisher's uninstaller and says
/// it has finished. The entry is still there, so it was not removed: the
/// step fails with the sentence that offers forced removal, and no scan is
/// made, so nothing of a still-installed application is offered.
#[test]
fn await_gone_with_the_entry_still_there_offers_forced_removal() {
    let session = FakeSession {
        listed: vec![true],
        ..Default::default()
    };
    let (result, events, scans) = run_session(
        &await_step(),
        &FixtureView::from_json("{}"),
        &session,
        true,
        false,
    );
    assert_eq!(
        result,
        Err(
            "The uninstaller did not remove Acme Studio. It may have been cancelled. Forced \
             removal clears it without the uninstaller."
                .to_string()
        )
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Leftovers { .. })),
        "{events:?}"
    );
    assert!(scans.lock().unwrap().is_empty());
}

#[test]
fn it_has_finished_with_the_entry_gone_goes_on() {
    let session = FakeSession {
        listed: vec![true, false],
        ..Default::default()
    };
    // The first look finds it; the page then says finished; the last look
    // finds it gone.
    let (result, _, _) = run_session(
        &await_step(),
        &FixtureView::from_json("{}"),
        &session,
        true,
        false,
    );
    assert_eq!(result, Ok(None));
}

#[test]
fn stop_waiting_cancels_the_plan() {
    let session = FakeSession {
        listed: vec![true],
        ..Default::default()
    };
    let (result, _, _) = run_session(
        &await_step(),
        &FixtureView::from_json("{}"),
        &session,
        false,
        true,
    );
    assert_eq!(
        result,
        Err(crate::transaction::runner::CANCELLED.to_string())
    );
}

#[test]
fn a_package_with_no_uninstall_key_has_nothing_to_wait_for() {
    let mut hints = acme_hints();
    hints.uninstall_key = None;
    let session = FakeSession {
        listed: vec![true],
        ..Default::default()
    };
    let wait = step(
        AWAIT_GONE,
        vec![serde_json::to_string(&hints).unwrap()],
        false,
    );
    let (result, events, _) =
        run_session(&wait, &FixtureView::from_json("{}"), &session, false, false);
    assert_eq!(result, Ok(None));
    assert!(events.is_empty());
}

fn scan_of(hints: Hints, force: bool) -> Step {
    step(
        SCAN,
        vec![serde_json::to_string(&ScanPayload { hints, force }).unwrap()],
        false,
    )
}

/// Discord after its uninstaller, as the scanner's own tests know it.
fn discord() -> (Hints, FixtureView) {
    let mut hints = acme_hints();
    hints.name = "Discord".to_string();
    hints.publisher = Some("Discord Inc.".to_string());
    hints.uninstall_key = None;
    hints.install_dir = Some(PathBuf::from(r"C:\Users\you\AppData\Local\Discord"));
    let view = FixtureView::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cleanup/discord.json"),
    );
    (hints, view)
}

#[test]
fn a_scan_keeps_its_findings_and_tells_the_page() {
    let session = FakeSession {
        failed_sources: 1,
        ..Default::default()
    };
    let (hints, view) = discord();
    let (result, events, scans) = run_session(
        &scan_of(hints.clone(), false),
        &view,
        &session,
        false,
        false,
    );
    assert_eq!(result, Ok(None));
    let scans = scans.lock().unwrap();
    let (id, record) = scans.iter().next().expect("one scan kept");
    assert!(!record.forced);
    assert_eq!(record.hints, hints);
    assert!(!record.findings.items.is_empty(), "{:?}", record.findings);
    assert_eq!(record.findings.unreadable, 1, "the source it could not ask");
    assert_eq!(
        events,
        vec![Event::Leftovers {
            plan: "p".into(),
            scan: id.clone(),
            count: record.findings.items.len(),
        }]
    );
}

#[test]
fn a_scan_that_finds_nothing_says_so() {
    let session = FakeSession::default();
    let (result, events, _) = run_session(
        &scan_of(acme_hints(), true),
        &FixtureView::from_json("{}"),
        &session,
        false,
        false,
    );
    assert_eq!(result, Ok(Some("Nothing was left behind.".to_string())));
    assert!(matches!(
        events.as_slice(),
        [Event::Leftovers { count: 0, .. }]
    ));
}

const REPLACEABLE: &str = r"C:\Program Files (x86)\Steam\uninstall.exe";

fn shell_open_of(program: &str) -> Step {
    step(SHELL_OPEN, vec![program.to_string()], false)
}

#[test]
fn shell_open_starts_only_what_the_closed_list_refused_as_replaceable() {
    let refused = FakeSession {
        closed_list: Some(Err(format!(
            "{}{REPLACEABLE}. Starting it as Administrator would hand Administrator to whoever \
             replaced it, so Brokey will not.",
            crate::transaction::allow::CAN_BE_REPLACED
        ))),
        ..Default::default()
    };
    let view = FixtureView::from_json("{}");
    let (result, _, _) = run_session(&shell_open_of(REPLACEABLE), &view, &refused, false, false);
    assert_eq!(result, Ok(None));
    assert_eq!(
        *refused.opened.borrow(),
        vec![(PathBuf::from(REPLACEABLE), Vec::<String>::new())]
    );

    for answer in [
        // The closed list admits it: it goes through the helper, not here.
        Ok(()),
        // Refused for any other reason: Brokey starts nothing.
        Err(format!(
            "The helper refused {REPLACEABLE}. It is not allowed."
        )),
    ] {
        let session = FakeSession {
            closed_list: Some(answer),
            ..Default::default()
        };
        let (result, _, _) =
            run_session(&shell_open_of(REPLACEABLE), &view, &session, false, false);
        assert!(result.is_err());
        assert!(session.opened.borrow().is_empty());
    }
}

#[test]
fn shell_open_of_a_bare_name_is_refused_before_anything_is_asked() {
    let session = FakeSession::default();
    let (result, _, _) = run_session(
        &shell_open_of("uninstall.exe"),
        &FixtureView::from_json("{}"),
        &session,
        false,
        false,
    );
    assert!(result.unwrap_err().contains("full path"));
    assert!(session.opened.borrow().is_empty());
}

#[test]
fn an_uninstaller_that_ends_with_a_code_leaves_the_verdict_to_the_key() {
    let session = FakeSession {
        closed_list: Some(Err(format!(
            "{}{REPLACEABLE}.",
            crate::transaction::allow::CAN_BE_REPLACED
        ))),
        exit_code: 2,
        ..Default::default()
    };
    let (result, _, _) = run_session(
        &shell_open_of(REPLACEABLE),
        &FixtureView::from_json("{}"),
        &session,
        false,
        false,
    );
    let note = result.unwrap().unwrap();
    assert!(note.contains("code 2"), "{note}");
}

fn replaceable_answer() -> Option<Result<(), String>> {
    Some(Err(format!(
        "{}{REPLACEABLE}.",
        crate::transaction::allow::CAN_BE_REPLACED
    )))
}

#[test]
fn shell_open_of_anything_but_a_program_is_refused() {
    let view = FixtureView::from_json("{}");
    for target in [
        r"C:\Program Files (x86)\Steam\readme.txt",
        r"C:\Program Files (x86)\Steam",
        r"C:\Program Files (x86)\Steam\uninstall.bat",
    ] {
        let session = FakeSession {
            closed_list: replaceable_answer(),
            ..Default::default()
        };
        let (result, _, _) = run_session(&shell_open_of(target), &view, &session, false, false);
        assert!(result.unwrap_err().contains("full path"), "{target}");
        assert!(session.opened.borrow().is_empty(), "{target}");
    }
}

/// "Stop waiting" while a self-elevating uninstaller is open stops the wait
/// and the plan, and kills nothing: the fake's uninstaller is still running
/// when the step returns.
#[test]
fn stop_waiting_on_a_self_elevating_uninstaller_leaves_it_running() {
    let session = FakeSession {
        closed_list: replaceable_answer(),
        still_running: true,
        ..Default::default()
    };
    let (result, _, _) = run_session(
        &shell_open_of(REPLACEABLE),
        &FixtureView::from_json("{}"),
        &session,
        false,
        true,
    );
    assert_eq!(result, Err(STOPPED_WAITING.to_string()));
    assert_eq!(session.opened.borrow().len(), 1);
}

#[test]
fn the_witness_is_made_only_from_a_replaceable_refusal() {
    let args = vec!["/S".to_string()];
    assert!(Refused::by(REPLACEABLE, &args, Ok(())).is_none());
    assert!(Refused::by(REPLACEABLE, &args, Err("The helper refused it.".into())).is_none());
    let witness = Refused::by(REPLACEABLE, &args, replaceable_answer().unwrap())
        .expect("the one refusal it is made from");
    assert_eq!(witness.program(), Path::new(REPLACEABLE));
    assert_eq!(witness.args(), args.as_slice());
}

#[test]
fn a_steam_game_is_handed_to_steam_through_its_address() {
    let session = FakeSession::default();
    let (result, events, _) = run_session(
        &step(STEAM_UNINSTALL, vec!["1091500".into()], false),
        &FixtureView::from_json("{}"),
        &session,
        false,
        false,
    );
    assert_eq!(result, Ok(None), "it returns at once; the next step waits");
    assert!(events.is_empty());
    assert_eq!(*session.steam.borrow(), vec!["steam://uninstall/1091500"]);
}

#[test]
fn a_steam_step_carries_one_app_id_of_digits_and_nothing_else() {
    for args in [
        vec![],
        vec!["10x".to_string()],
        vec!["10 -silent".to_string()],
        vec!["12345678901".to_string()],
        vec!["10".to_string(), "20".to_string()],
    ] {
        let session = FakeSession::default();
        let (result, _, _) = run_session(
            &step(STEAM_UNINSTALL, args.clone(), false),
            &FixtureView::from_json("{}"),
            &session,
            false,
            false,
        );
        assert!(result.is_err(), "{args:?}");
        assert!(session.steam.borrow().is_empty(), "{args:?}");
    }
    let root = step(STEAM_UNINSTALL, vec!["10".into()], true);
    assert!(
        check_with(
            &root,
            &FixtureView::from_json("{}"),
            &temp_roots(Path::new("x")),
            &PLAIN
        )
        .is_err()
    );
}

/// A Steam game whose uninstall was cancelled in Steam: the sentence sends
/// the user to Steam's library and never offers forced removal.
#[test]
fn a_cancelled_steam_uninstall_points_at_the_steam_library() {
    let mut hints = acme_hints();
    hints.name = "Cyberpunk 2077".to_string();
    hints.uninstall_key = Some((Hive::Hklm32, "Steam App 1091500".to_string()));
    let session = FakeSession {
        listed: vec![true],
        ..Default::default()
    };
    let wait = step(
        AWAIT_GONE,
        vec![serde_json::to_string(&hints).unwrap()],
        false,
    );
    let (result, events, _) =
        run_session(&wait, &FixtureView::from_json("{}"), &session, true, false);
    let err = result.unwrap_err();
    assert_eq!(err, steam_not_removed("Cyberpunk 2077"));
    assert!(err.contains("Steam library"), "{err}");
    assert!(!err.contains("Forced removal"), "{err}");
    assert!(!events.iter().any(|e| matches!(e, Event::Leftovers { .. })));
}

#[test]
fn an_unknown_step_is_refused_when_run() {
    let temp = tempfile::tempdir().unwrap();
    let roots = temp_roots(temp.path());
    let (result, _) = run_here(&step("brokey:anything", vec![], false), &roots);
    assert!(result.is_err());
    assert!(is_internal("brokey:anything"));
    assert!(!is_internal(r"C:\Windows\System32\cmd.exe"));
}

#[test]
fn a_report_with_failures_names_them_and_what_was_kept() {
    let report = HoldReport {
        held: 2,
        failed: vec![(
            r"C:\Acme\log.txt".to_string(),
            r"C:\Acme\log.txt is in use. Close Acme and try again.".to_string(),
        )],
    };
    let err = outcome(report, 3, "moved to the holding area").unwrap_err();
    assert_eq!(
        err,
        r"2 of 3 items were moved to the holding area. What was moved can be restored from Settings. C:\Acme\log.txt is in use. Close Acme and try again."
    );
    assert_eq!(outcome(HoldReport::default(), 3, "moved"), Ok(None));
}

// ---------------------------------------------------------------------------
// Every refusal rule, one table each

/// A machine with its well-known folders where a stock Windows has them,
/// and `extra` (more JSON members) beside them.
fn machine(extra: &str) -> FixtureView {
    let comma = if extra.trim().is_empty() { "" } else { "," };
    FixtureView::from_json(&format!(
        r#"{{
            "folders": {{
                "Windows": "C:\\Windows",
                "ProgramFiles": "C:\\Program Files",
                "ProgramFilesX86": "C:\\Program Files (x86)",
                "ProgramData": "C:\\ProgramData",
                "AppData": "C:\\Users\\me\\AppData\\Roaming",
                "LocalAppData": "C:\\Users\\me\\AppData\\Local",
                "LocalLow": "C:\\Users\\me\\AppData\\LocalLow",
                "Profile": "C:\\Users\\me",
                "StartMenuMachine": "C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs",
                "StartMenuUser": "C:\\Users\\me\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs"
            }}{comma}
            {extra}
        }}"#
    ))
}

/// The refusal for `step` on `view` with the machine answering `answer`,
/// which must say `why` and keep the copy rules.
fn refused_for(step: &Step, view: &FixtureView, answer: Part, why: &str) {
    let err =
        check_with(step, view, &Roots::live(), &Answer(answer)).expect_err("the rule refuses this");
    assert!(err.contains(why), "expected {why:?} in: {err}");
    assert!(err.ends_with('.') && !err.contains('\u{2014}'), "{err}");
}

fn allowed_for(step: &Step, view: &FixtureView) {
    assert_eq!(check_with(step, view, &Roots::live(), &PLAIN), Ok(()));
}

fn hold_named(name: &str, items: Vec<HoldItem>) -> Step {
    let payload = HoldPayload {
        holding: "abc".to_string(),
        name: name.to_string(),
        items,
    };
    step(HOLD, vec![serde_json::to_string(&payload).unwrap()], true)
}

#[test]
fn a_hold_whose_name_is_not_one_brokey_would_show_is_refused() {
    let view = classes();
    let key = || vec![item(ItemKind::RegistryKey, r"HKLM\Software\Acme")];
    let long = "a".repeat(261);
    for name in ["", "   ", "Acme\u{7}Studio", "Acme\nStudio", long.as_str()] {
        refused_for(
            &hold_named(name, key()),
            &view,
            Part::Plain,
            "application's name",
        );
    }
    allowed_for(&hold_named(&"a".repeat(260), key()), &view);
}

#[test]
fn a_step_with_the_wrong_number_of_arguments_is_refused() {
    let view = classes();
    let code = "{12345678-1234-1234-1234-123456789012}".to_string();
    for program in [RESTORE, EMPTY] {
        for args in [vec![], vec!["abc".to_string(), "def".to_string()]] {
            refused_for(
                &step(program, args, true),
                &view,
                Part::Plain,
                "exactly one",
            );
        }
    }
    for args in [
        vec![code.clone()],
        vec![code.clone(), "abc".to_string(), "x".to_string()],
        vec![],
    ] {
        refused_for(
            &step(MSI_UNREGISTER, args, true),
            &view,
            Part::Plain,
            "a product code and a holding area entry",
        );
    }
    allowed_for(&step(EMPTY, vec!["all".to_string()], true), &view);
    allowed_for(
        &step(MSI_UNREGISTER, vec![code, "abc".to_string()], true),
        &view,
    );
}

#[cfg(windows)]
#[test]
fn a_name_windows_would_read_otherwise_is_refused() {
    let view = machine("");
    for c in [':', '*', '?', '|', '<', '>', '"', '\u{1}'] {
        let path = format!(r"C:\Acme\a{c}b");
        refused_for(
            &hold("abc", vec![item(ItemKind::Folder, &path)], true),
            &view,
            Part::Plain,
            "does not allow in a name",
        );
    }
    for path in [r"C:\Acme\name.", r"C:\Acme\name ", r"C:\Acme. \name"] {
        refused_for(
            &hold("abc", vec![item(ItemKind::Folder, path)], true),
            &view,
            Part::Plain,
            "ending in a dot or a space",
        );
    }
    allowed_for(
        &hold("abc", vec![item(ItemKind::Folder, r"C:\Acme\name.d")], true),
        &view,
    );
}

/// Each answer the walk can get, for a path to move and for one to put
/// back: a link refuses both, a part that cannot be asked refuses both, and
/// a missing part refuses only what is to be moved.
#[cfg(windows)]
#[test]
fn each_answer_the_walk_gets_is_heeded() {
    let view = machine("");
    let path = r"C:\Acme\App";
    let held = || hold("abc", vec![item(ItemKind::Folder, path)], true);
    refused_for(
        &held(),
        &view,
        Part::Link,
        "junction, a symbolic link or a mount point",
    );
    refused_for(
        &held(),
        &view,
        Part::Unknown,
        "could not be checked for a junction",
    );
    refused_for(&held(), &view, Part::Missing, "is not there");

    let temp = tempfile::tempdir().unwrap();
    let roots = temp_roots(temp.path());
    manifest_with(&roots, "acme", vec![item(ItemKind::Folder, path)]);
    let back = step(RESTORE, vec!["acme".into()], false);
    for (answer, verdict) in [
        (Part::Missing, Ok(())),
        (Part::Plain, Ok(())),
        (Part::Link, Err("junction")),
        (Part::Unknown, Err("could not be checked")),
    ] {
        let got = check_with(&back, &view, &roots, &Answer(answer));
        match verdict {
            Ok(()) => assert_eq!(got, Ok(()), "{answer:?}"),
            Err(why) => assert!(got.is_err_and(|e| e.contains(why)), "{answer:?}"),
        }
    }
}

/// The same for the way to a registry key.
#[test]
fn each_answer_the_registry_walk_gets_is_heeded() {
    let view = classes();
    let key = || {
        hold(
            "abc",
            vec![item(ItemKind::RegistryKey, r"HKLM\Software\Acme")],
            true,
        )
    };
    refused_for(&key(), &view, Part::Link, "registry link");
    refused_for(
        &key(),
        &view,
        Part::Unknown,
        "could not be checked for a registry link",
    );
    refused_for(&key(), &view, Part::Missing, "it is not there");
    allowed_for(&key(), &view);
}

#[cfg(windows)]
#[test]
fn a_registry_restore_heeds_the_walk_too() {
    let view = classes();
    let temp = tempfile::tempdir().unwrap();
    let roots = temp_roots(temp.path());
    manifest_with(
        &roots,
        "acme",
        vec![item(ItemKind::RegistryKey, r"HKCU\Software\Acme")],
    );
    let back = step(RESTORE, vec!["acme".into()], false);
    assert_eq!(
        check_with(&back, &view, &roots, &Answer(Part::Missing)),
        Ok(())
    );
    assert!(check_with(&back, &view, &roots, &Answer(Part::Link)).is_err());
}

#[test]
fn a_control_character_in_a_key_or_value_name_is_refused() {
    let view = classes();
    for (kind, location) in [
        (ItemKind::RegistryKey, "HKLM\\Software\\Ac\u{1}me"),
        (ItemKind::RegistryKey, "HKLM\\Software\\Acme\\\u{0}"),
        (ItemKind::RegistryValue, "HKLM\\Software\\Acme | Na\u{1}me"),
        (ItemKind::RegistryValue, "HKLM\\Software\\Acme | \n"),
    ] {
        let err = check_with(
            &hold("abc", vec![item(kind, location)], true),
            &view,
            &Roots::live(),
            &PLAIN,
        );
        assert!(err.is_err(), "{location:?}");
    }
}

#[test]
fn each_key_windows_keeps_beside_the_guards_is_refused() {
    let view = classes();
    for key in [
        r"HKLM\Software\Classes\Local Settings\Software",
        r"HKLM\Software\Classes\Protocols\Handler\acme",
        r"HKLM\Software\Classes\MIME\Database\Content Type\application/x-acme",
        r"HKLM\Software\Classes\PackagedCom\Package",
        r"HKLM\Software\Classes\Extensions\ContractId",
        r"HKLM\Software\Classes\ActivatableClasses\Package",
        r"HKLM\Software\Classes\exefile\shell\open",
        r"HKLM\Software\Classes\batfile",
        r"HKLM\Software\Classes\Msi.Package\shell",
        r"HKLM\Software\Classes\Microsoft.PowerShellScript.1\Shell",
        r"HKLM\Software\Classes\CompressedFolder\shell",
        r"HKLM\Software\Classes\Folder\shell\acme",
        r"HKLM\Software\Classes\htmlfile",
        r"HKLM\Software\Classes\txtfile\shell",
        r"HKLM32\Software\Classes\regfile",
        r"HKLM\Software\WOW6432Node\Classes\VBSFile",
        r"HKLM\Software\ODBC",
        r"HKLM\Software\ODBC\ODBC.INI",
        r"HKLM\Software\Clients\StartMenuInternet",
        r"HKLM\Software\RegisteredApplications",
    ] {
        refused_for(
            &hold("abc", vec![item(ItemKind::RegistryKey, key)], true),
            &view,
            Part::Plain,
            "never clears",
        );
    }
    // What an application put in them is its own.
    for (kind, key) in [
        (
            ItemKind::RegistryKey,
            r"HKLM\Software\ODBC\ODBC.INI\Acme DSN",
        ),
        (
            ItemKind::RegistryKey,
            r"HKLM\Software\Clients\StartMenuInternet\Acme",
        ),
        (
            ItemKind::RegistryValue,
            r"HKLM\Software\RegisteredApplications | Acme",
        ),
        (
            ItemKind::RegistryKey,
            r"HKLM\Software\Classes\Acme.Document",
        ),
    ] {
        allowed_for(&hold("abc", vec![item(kind, key)], true), &view);
    }
}

#[cfg(windows)]
#[test]
fn common_files_microsoft_shared_is_refused() {
    let view = machine("");
    for path in [
        r"C:\Program Files\Common Files\Microsoft Shared",
        r"C:\Program Files\Common Files\Microsoft Shared\Acme",
        r"C:\Program Files (x86)\Common Files\microsoft shared\Acme",
    ] {
        refused_for(
            &hold("abc", vec![item(ItemKind::Folder, path)], true),
            &view,
            Part::Plain,
            "never clears",
        );
    }
    allowed_for(
        &hold(
            "abc",
            vec![item(
                ItemKind::Folder,
                r"C:\Program Files\Common Files\Acme",
            )],
            true,
        ),
        &view,
    );
}

/// The all-users Start menu lies under `ProgramData\Microsoft`, which is
/// Windows's own, yet what is inside its `Programs` folder is the
/// applications', and the scan offers dead shortcuts there as Certain.
#[cfg(windows)]
#[test]
fn what_is_inside_the_machine_start_menu_may_be_cleared_and_the_menu_itself_not() {
    let view = machine("");
    let menu = r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs";
    for (kind, path) in [
        (ItemKind::Shortcut, format!(r"{menu}\Acme Studio.lnk")),
        (ItemKind::Folder, format!(r"{menu}\Acme")),
        (ItemKind::Shortcut, format!(r"{menu}\Acme\Acme Studio.lnk")),
    ] {
        allowed_for(&hold("abc", vec![item(kind, &path)], true), &view);
    }
    for path in [
        menu.to_string(),
        r"C:\ProgramData\Microsoft\Windows\Start Menu".to_string(),
        r"C:\ProgramData\Microsoft\Windows".to_string(),
        r"C:\ProgramData\Microsoft\Crypto".to_string(),
        r"C:\ProgramData\Microsoft\Crypto\RSA".to_string(),
        r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs.lnk".to_string(),
    ] {
        refused_for(
            &hold("abc", vec![item(ItemKind::Folder, &path)], true),
            &view,
            Part::Plain,
            "never clears",
        );
    }
}

#[test]
fn a_task_or_rule_brokey_cannot_place_is_refused() {
    let view = machine(
        r#""tasks": [
                { "path": "\\Acme\\Update", "actions": ["\"C:\\Program Files\\Acme\\update.exe\""] },
                { "path": "\\Acme\\Shell", "actions": ["C:\\Program Files\\Acme\\a.exe", "C:\\Windows\\System32\\cmd.exe /c x"] }
            ]"#,
    );
    let task = |path: &str| {
        hold(
            "abc",
            vec![item(ItemKind::Task, &format!("task:{path}"))],
            true,
        )
    };
    allowed_for(&task(r"\Acme\Update"), &view);
    refused_for(&task(r"\Acme\Shell"), &view, Part::Plain, "Windows's own");

    let unreadable_tasks = machine(r#""unreadable_tasks": 1"#);
    refused_for(
        &task(r"\Acme\Gone"),
        &unreadable_tasks,
        Part::Plain,
        "could not be read",
    );

    let rule = hold(
        "abc",
        vec![item(ItemKind::FirewallRule, "firewall:Acme | {a}")],
        true,
    );
    let unreadable_rules = machine(r#""unreadable_firewall": 2"#);
    refused_for(&rule, &unreadable_rules, Part::Plain, "could not be read");
    // An id not among the rules that let a program through (a port or a
    // service rule, Windows's own among them) is never held.
    refused_for(
        &rule,
        &machine(""),
        Part::Plain,
        "not a firewall rule that lets a program through",
    );
    allowed_for(
        &rule,
        &machine(
            r#""firewall": [{ "id": "{a}", "name": "Acme", "program": "C:\\Program Files\\Acme\\acme.exe" }]"#,
        ),
    );
}

/// A rule being put back is not there, which is no reason to refuse it.
#[cfg(windows)]
#[test]
fn a_firewall_rule_being_put_back_need_not_be_listed() {
    let temp = tempfile::tempdir().unwrap();
    let both = temp.path().join("holding");
    let roots = Roots {
        user: both.clone(),
        machine: both,
    };
    manifest_with(
        &roots,
        "acme",
        vec![item(ItemKind::FirewallRule, "firewall:Acme | {a}")],
    );
    let back = step(RESTORE, vec!["acme".into()], true);
    assert_eq!(check_with(&back, &machine(""), &roots, &PLAIN), Ok(()));
}

#[cfg(windows)]
#[test]
fn a_restore_whose_record_cannot_be_read_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let roots = temp_roots(temp.path());
    let entry = roots.user.join("acme");
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(entry.join("manifest.json"), "{ not a record").unwrap();
    let err = check_with(
        &step(RESTORE, vec!["acme".into()], false),
        &classes(),
        &roots,
        &PLAIN,
    )
    .expect_err("an unreadable record");
    assert!(err.contains("could not be read"), "{err}");
}

/// The live walk of the registry, through the closed list's own door: a
/// key with a link on the way to it is refused before anything moves.
#[cfg(windows)]
#[test]
fn the_closed_list_refuses_a_key_with_a_registry_link_on_the_way() {
    use crate::cleanup::holding::windows::tests::{TestKey, make_link, remove_link};
    let key = TestKey::new("internal-link");
    let target = format!(r"{}\Target", key.0);
    windows_registry::CURRENT_USER
        .create(format!(r"{target}\Inner"))
        .unwrap();
    let way = format!(r"{}\Way", key.0);
    make_link(&way, &target);
    let item_at = |path: &str| item(ItemKind::RegistryKey, &format!(r"HKCU\{path}"));
    let err = check(
        &hold("abc", vec![item_at(&format!(r"{way}\Inner"))], false),
        &super::super::view::LiveView::new(),
        &Roots::live(),
    );
    remove_link(&way);
    let err = err.expect_err("a link on the way");
    assert!(err.contains("registry link"), "{err}");
    assert_eq!(
        check(
            &hold("abc", vec![item_at(&format!(r"{target}\Inner"))], false),
            &super::super::view::LiveView::new(),
            &Roots::live(),
        ),
        Ok(())
    );
}

// ---------------------------------------------------------------------------
// An extension stays when the ProgID it names could not go

/// The registry as the holding area changes it, in so far as these tests
/// need: every key exports empty and deletes, but for one that is in use.
struct OneKeyInUse {
    in_use: &'static str,
    deleted: Vec<String>,
}

impl Mutate for OneKeyInUse {
    fn make_root(&mut self, _: &Path) -> holding::Done<()> {
        Ok(())
    }
    fn trust_entry(&mut self, _: &Path, _: &Path) -> holding::Done<()> {
        Ok(())
    }
    fn move_path(&mut self, _: &Path, _: &Path) -> holding::Done<()> {
        Err(Failure::Failed("No files in this test.".into()))
    }
    fn merge_path(&mut self, _: &Path, _: &Path) -> holding::Done<()> {
        Err(Failure::Failed("No files in this test.".into()))
    }
    fn export_key(&mut self, _: Hive, path: &str) -> holding::Done<holding::KeyNode> {
        if path.eq_ignore_ascii_case(self.in_use) {
            Err(Failure::Failed(format!("{path} is in use.")))
        } else {
            Ok(holding::KeyNode::default())
        }
    }
    fn delete_key(&mut self, _: Hive, path: &str) -> holding::Done<()> {
        self.deleted.push(path.to_string());
        Ok(())
    }
    fn restore_key(
        &mut self,
        _: Hive,
        _: &str,
        _: &holding::KeyNode,
        _: bool,
    ) -> holding::Done<()> {
        Ok(())
    }
    fn export_value(&mut self, _: Hive, _: &str, _: &str) -> holding::Done<holding::RegValue> {
        Ok(holding::RegValue {
            ty: holding::REG_SZ,
            data: Vec::new(),
        })
    }
    fn delete_value(&mut self, _: Hive, key: &str, name: &str) -> holding::Done<()> {
        self.deleted.push(format!("{key} | {name}"));
        Ok(())
    }
    fn restore_value(
        &mut self,
        _: Hive,
        _: &str,
        _: &str,
        _: &holding::RegValue,
        _: bool,
    ) -> holding::Done<()> {
        Ok(())
    }
    fn export_service(&mut self, _: &str) -> holding::Done<holding::KeyNode> {
        Err(Failure::Missing)
    }
    fn delete_service(&mut self, _: &str) -> holding::Done<()> {
        Err(Failure::Missing)
    }
    fn restore_service(&mut self, _: &str, _: &holding::KeyNode, _: bool) -> holding::Done<()> {
        Err(Failure::Missing)
    }
    fn export_task(&mut self, _: &str) -> holding::Done<String> {
        Err(Failure::Missing)
    }
    fn delete_task(&mut self, _: &str) -> holding::Done<()> {
        Err(Failure::Missing)
    }
    fn restore_task(&mut self, _: &str, _: &str, _: bool) -> holding::Done<()> {
        Err(Failure::Missing)
    }
    fn export_firewall_rule(&mut self, _: &str) -> holding::Done<Vec<(String, String)>> {
        Err(Failure::Missing)
    }
    fn delete_firewall_rule(&mut self, _: &str, _: &[(String, String)]) -> holding::Done<()> {
        Err(Failure::Missing)
    }
    fn restore_firewall_rule(
        &mut self,
        _: &str,
        _: &[(String, String)],
        _: bool,
    ) -> holding::Done<()> {
        Err(Failure::Missing)
    }
}

#[test]
fn an_extension_stays_when_the_progid_it_names_could_not_go() {
    let view = FixtureView::from_json(
        r#"{
            "registry": {
                "Hkcu": {
                    "Software\\Classes\\.acme": { "values": [["", "Acme.Doc"]] },
                    "Software\\Classes\\Acme.Doc": { "subkeys": ["shell"] },
                    "Software\\Classes\\.other": { },
                    "Software\\Classes\\.zip\\OpenWithProgids": { "values": [["Acme.Doc", ""]] }
                }
            }
        }"#,
    );
    let temp = tempfile::tempdir().unwrap();
    let roots = temp_roots(temp.path());
    let mut m = OneKeyInUse {
        in_use: r"Software\Classes\Acme.Doc",
        deleted: Vec::new(),
    };
    let clear = hold(
        "acme-1",
        vec![
            item(ItemKind::RegistryKey, r"HKCU\Software\Classes\.acme"),
            item(
                ItemKind::RegistryValue,
                r"HKCU\Software\Classes\.zip\OpenWithProgids | Acme.Doc",
            ),
            item(ItemKind::RegistryKey, r"HKCU\Software\Classes\.other"),
            item(ItemKind::RegistryKey, r"HKCU\Software\Classes\Acme.Doc"),
        ],
        false,
    );
    let mut events = Vec::new();
    let mut sink = |e: Event| events.push(e);
    let never = || false;
    let mut ctx = InternalContext {
        plan: "p",
        step: 0,
        sink: &mut sink,
        cancelled: &never,
        finished: Arc::new(AtomicBool::new(false)),
        scans: Scans::default(),
    };
    let err = run_in(
        &clear,
        &mut ctx,
        &roots,
        &view,
        &mut m,
        &PLAIN,
        &FakeSession::default(),
    )
    .expect_err("the ProgID could not be moved");
    assert!(err.starts_with("1 of 4 items were moved"), "{err}");
    assert!(
        err.contains(r"Software\Classes\Acme.Doc is in use."),
        "{err}"
    );
    assert!(
        err.contains(r"HKCU\Software\Classes\.acme was left where it is, because HKCU\Software\Classes\Acme.Doc, which it names, could not be moved."),
        "{err}"
    );
    assert!(
        err.contains(r"OpenWithProgids | Acme.Doc was left where it is"),
        "{err}"
    );
    assert_eq!(m.deleted, vec![r"Software\Classes\.other".to_string()]);
}
