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
        let mut m = holding::LiveMutate {
            machine_root: roots.machine.clone(),
            force_copy: false,
        };
        run_in(
            step,
            &mut ctx,
            roots,
            &super::super::view::LiveView::new(),
            &mut m,
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
    let err = check(
        &hold("abc", vec![item(ItemKind::RegistryKey, EXT)], true),
        &view,
        &roots,
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
        assert_eq!(check(&hold("abc", items, true), &view, &roots), Ok(()));
    }
    // Another extension in the same set is still asked about its own.
    let err = check(
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
        item(ItemKind::FirewallRule, "firewall:Acme"),
    ] {
        let location = refused.location.clone();
        let err = check(&hold("abc", vec![refused], false), &view, &roots)
            .expect_err("not a session item");
        assert!(err.contains(&location), "{err}");
    }
    assert_eq!(
        check(
            &hold(
                "abc",
                vec![item(ItemKind::RegistryKey, r"HKCU\Software\Acme")],
                false
            ),
            &view,
            &roots
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
            check(&hold("abc", vec![wrong.clone()], true), &view, &roots).is_err(),
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
                { "name": "Acme", "program": "C:\\Program Files\\Acme\\acme.exe" },
                { "name": "Twice", "program": "C:\\Program Files\\Acme\\acme.exe" },
                { "name": "Twice", "program": "%SystemRoot%\\System32\\svchost.exe" }
            ]
        }"#,
    );
    let roots = Roots::live();
    let ok = |location: &str, kind: ItemKind| {
        check(
            &hold("abc", vec![item(kind, location)], true),
            &view,
            &roots,
        )
    };
    assert_eq!(ok("service:AcmeUpdate", ItemKind::Service), Ok(()));
    assert_eq!(ok("service:Gone", ItemKind::Service), Ok(()));
    for name in ["Shared", "Driver", "Rooted", "Unset"] {
        let err = ok(&format!("service:{name}"), ItemKind::Service).expect_err(name);
        assert!(err.contains("Windows's own"), "{err}");
    }
    assert_eq!(ok("firewall:Acme", ItemKind::FirewallRule), Ok(()));
    assert!(ok("firewall:Twice", ItemKind::FirewallRule).is_err());

    let unlisted = FixtureView::from_json(r#"{ "unreadable_services": 1 }"#);
    let err = check(
        &hold("abc", vec![item(ItemKind::Service, "service:Gone")], true),
        &unlisted,
        &roots,
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
fn the_session_only_steps_are_not_taken_yet_and_start_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let roots = temp_roots(temp.path());
    for program in [AWAIT_GONE, SCAN, SHELL_OPEN] {
        let (result, events) = run_here(&step(program, vec!["{}".into()], false), &roots);
        let err = result.expect_err("Task 8 takes these");
        assert!(err.contains(program) && err.ends_with('.'), "{err}");
        assert!(events.is_empty());
    }
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
