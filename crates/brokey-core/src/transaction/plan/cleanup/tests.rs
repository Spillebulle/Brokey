use super::*;
use crate::cleanup::internal::{ScanRecord, is_internal};
use crate::cleanup::view::{FixtureView, Hive};
use crate::transaction::allow::CAN_BE_REPLACED;
use crate::transaction::plan::{build_in, clear_notices, holding_notices, self_elevating_notices};
use crate::{Query, Source, Store};
use std::path::PathBuf;

const UNINSTALLER: &str = r"C:\Program Files (x86)\Acme\uninstall.exe";
const STEAM: &str = crate::cleanup::internal::STEAM_UNINSTALL;

/// An Add/Remove source that answers `plan` with one removal step.
struct Arp {
    step: fn() -> Step,
}

impl Source for Arp {
    fn kind(&self) -> SourceKind {
        SourceKind::Arp
    }
    fn status(&self) -> SourceStatus {
        SourceStatus {
            kind: SourceKind::Arp,
            available: true,
            reason: None,
            detail: None,
            searchable: false,
            setup: None,
        }
    }
    fn search(&self, _: &Query) -> Result<Vec<Package>> {
        Ok(Vec::new())
    }
    fn installed(&self) -> Result<Vec<Package>> {
        Ok(Vec::new())
    }
    fn updates(&self) -> Result<Vec<Update>> {
        Ok(Vec::new())
    }
    fn details(&self, id: &str) -> Result<Package> {
        Err(Error::new(format!("{id} is not known.")))
    }
    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        Ok(match op {
            Op::Remove { .. } => vec![(self.step)()],
            _ => Vec::new(),
        })
    }
}

fn command(program: &str, args: &[&str]) -> Command {
    Command {
        program: program.to_string(),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: Vec::new(),
        cwd: None,
    }
}

fn uninstaller() -> Step {
    Step {
        source: SourceKind::Arp,
        title: "Removing Acme Studio".to_string(),
        command: command(UNINSTALLER, &[]),
        needs_root: true,
        weight: 10,
    }
}

fn steam_game() -> Step {
    Step {
        source: SourceKind::Arp,
        title: "Removing Cyberpunk 2077 through Steam".to_string(),
        command: command(STEAM, &["1091500"]),
        needs_root: false,
        weight: 10,
    }
}

fn store(step: fn() -> Step) -> Store {
    Store {
        system: crate::system::from_os_release("ID=arch\n"),
        sources: vec![Box::new(Arp { step })],
    }
}

fn acme() -> PackageRef {
    PackageRef {
        source: SourceKind::Arp,
        id: r"HKLM\Acme Studio_is1".to_string(),
    }
}

fn hints() -> Hints {
    Hints {
        package: acme(),
        name: "Acme Studio".to_string(),
        publisher: Some("Acme".to_string()),
        uninstall_key: Some((Hive::Hklm, "Acme Studio_is1".to_string())),
        product_code: None,
        install_dir: Some(PathBuf::from(r"C:\Program Files (x86)\Acme")),
        install_location: None,
        icon_dir: None,
        shortcuts: Vec::new(),
        extra_dirs: Vec::new(),
    }
}

fn remove() -> Op {
    Op::Remove { package: acme() }
}

/// Planning with nothing in it: the closed list admits everything, every
/// package has hints, the holding area is empty, and no view.
struct Machine {
    scans: Scans,
    refusal: Option<String>,
    hints: Option<Hints>,
    listing: Listing,
    view: Option<FixtureView>,
    installed: Vec<Package>,
}

impl Machine {
    fn new() -> Machine {
        Machine {
            scans: Scans::default(),
            refusal: None,
            hints: Some(hints()),
            listing: Listing::default(),
            view: None,
            installed: Vec::new(),
        }
    }

    fn plan(&self, store: &Store, ops: &[Op]) -> Result<Plan> {
        self.with(|planning| build_in(store, ops, planning))
    }

    fn with<R>(&self, f: impl FnOnce(&Planning) -> R) -> R {
        let check = |_: &Step| match &self.refusal {
            Some(why) => Err(why.clone()),
            None => Ok(()),
        };
        let hints = |_: &PackageRef| self.hints.clone();
        let listing = || self.listing.clone();
        let installed = || self.installed.clone();
        f(&Planning {
            scans: &self.scans,
            check: &check,
            hints: &hints,
            holding: &listing,
            view: self.view.as_ref().map(|v| v as &dyn SystemView),
            installed: &installed,
        })
    }
}

fn programs(plan: &Plan) -> Vec<&str> {
    plan.steps
        .iter()
        .map(|s| s.command.program.as_str())
        .collect()
}

#[test]
fn removing_an_arp_entry_waits_then_scans() {
    let plan = Machine::new()
        .plan(&store(uninstaller), &[remove()])
        .unwrap();
    assert_eq!(programs(&plan), vec![UNINSTALLER, AWAIT_GONE, SCAN]);
    let [_, wait, scan] = plan.steps.as_slice() else {
        unreachable!()
    };
    assert_eq!(wait.title, "Waiting for the uninstaller to finish");
    assert_eq!(scan.title, "Looking for what was left behind");
    assert!(!wait.needs_root && !scan.needs_root);
    let carried: Hints = serde_json::from_str(&wait.command.args[0]).unwrap();
    assert_eq!(
        carried,
        hints(),
        "the hints are read before the uninstaller"
    );
    let payload: ScanPayload = serde_json::from_str(&scan.command.args[0]).unwrap();
    assert!(!payload.force);
}

#[test]
fn a_removal_with_nothing_to_scan_by_neither_waits_nor_scans() {
    let mut machine = Machine::new();
    machine.hints = None;
    let plan = machine.plan(&store(uninstaller), &[remove()]).unwrap();
    assert_eq!(programs(&plan), vec![UNINSTALLER]);
}

#[test]
fn force_remove_runs_no_uninstaller() {
    let plan = Machine::new()
        .plan(&store(uninstaller), &[Op::ForceRemove { package: acme() }])
        .unwrap();
    assert_eq!(programs(&plan), vec![SCAN]);
    assert!(!plan.steps[0].needs_root);
    let payload: ScanPayload = serde_json::from_str(&plan.steps[0].command.args[0]).unwrap();
    assert!(payload.force);
    assert_eq!(payload.hints, hints());
}

#[test]
fn force_remove_of_something_no_longer_there_says_so() {
    let mut machine = Machine::new();
    machine.hints = None;
    let err = machine
        .plan(&store(uninstaller), &[Op::ForceRemove { package: acme() }])
        .unwrap_err();
    assert!(
        err.message.contains("nothing to remove by force"),
        "{}",
        err.message
    );
}

#[test]
fn a_steam_game_is_removed_through_steam_as_the_user() {
    let plan = Machine::new()
        .plan(&store(steam_game), &[remove()])
        .unwrap();
    assert_eq!(programs(&plan), vec![STEAM, AWAIT_GONE, SCAN]);
    assert!(
        plan.steps.iter().all(|s| !s.needs_root),
        "a Steam game never goes through the helper"
    );
}

fn replaceable() -> String {
    format!(
        "{CAN_BE_REPLACED}{UNINSTALLER}. Starting it as Administrator would hand Administrator \
         to whoever replaced it, so Brokey will not."
    )
}

#[test]
fn an_uninstaller_in_a_writable_folder_is_started_through_shell_open() {
    let mut machine = Machine::new();
    machine.refusal = Some(replaceable());
    let store = store(uninstaller);
    let plan = machine.plan(&store, &[remove()]).unwrap();
    assert_eq!(programs(&plan), vec![SHELL_OPEN, AWAIT_GONE, SCAN]);
    let open = &plan.steps[0];
    assert!(!open.needs_root, "Brokey asks for nothing");
    assert_eq!(open.command.args, vec![UNINSTALLER.to_string()]);
    assert_eq!(
        open.title,
        "Starting Acme Studio's uninstaller. Windows will ask you to allow it."
    );
    let notices = machine.with(|p| self_elevating_notices(&store, &[remove()], p));
    assert_eq!(
        notices,
        vec![
            "Windows will ask you to allow Acme Studio's uninstaller, because Brokey does not \
             start it as Administrator itself."
                .to_string()
        ]
    );
}

#[test]
fn any_other_refusal_is_left_for_the_closed_list_to_say() {
    let mut machine = Machine::new();
    machine.refusal = Some(format!(
        "The helper refused {UNINSTALLER}. It is not allowed."
    ));
    let store = store(uninstaller);
    let plan = machine.plan(&store, &[remove()]).unwrap();
    assert_eq!(programs(&plan), vec![UNINSTALLER, AWAIT_GONE, SCAN]);
    assert!(plan.steps[0].needs_root);
    assert!(
        machine
            .with(|p| self_elevating_notices(&store, &[remove()], p))
            .is_empty()
    );
}

#[test]
fn an_admitted_uninstaller_runs_through_the_helper_as_before() {
    let plan = Machine::new()
        .plan(&store(uninstaller), &[remove()])
        .unwrap();
    assert!(plan.steps[0].needs_root);
    assert_eq!(plan.steps[0].command.program, UNINSTALLER);
}

// ---------------------------------------------------------------------------
// Clearing

fn leftover(id: &str, kind: ItemKind, location: &str, scope: Scope) -> LeftoverItem {
    LeftoverItem {
        id: id.to_string(),
        kind,
        location: location.to_string(),
        size: None,
        confidence: Confidence::Certain,
        scope,
        ticked: true,
        reason: "It is in Acme Studio's folder.".to_string(),
    }
}

fn scanned(machine: &Machine, forced: bool, product_code: Option<&str>) {
    scanned_items(
        machine,
        forced,
        product_code,
        vec![
            leftover(
                "item-0",
                ItemKind::Folder,
                r"C:\Users\you\AppData\Roaming\Acme",
                Scope::User,
            ),
            leftover(
                "item-1",
                ItemKind::RegistryKey,
                r"HKCU\Software\Acme",
                Scope::User,
            ),
            leftover(
                "item-2",
                ItemKind::Folder,
                r"C:\Program Files (x86)\Acme",
                Scope::Machine,
            ),
            leftover(
                "item-3",
                ItemKind::RegistryKey,
                r"HKLM\Software\Acme",
                Scope::Machine,
            ),
            leftover(
                "item-4",
                ItemKind::Folder,
                r"C:\ProgramData\Acme",
                Scope::Machine,
            ),
        ],
    );
}

fn scanned_items(
    machine: &Machine,
    forced: bool,
    product_code: Option<&str>,
    items: Vec<LeftoverItem>,
) {
    let mut h = hints();
    h.product_code = product_code.map(str::to_string);
    let findings = Findings {
        scan: "scan-1".to_string(),
        package: acme(),
        name: "Acme Studio".to_string(),
        items,
        kept: Vec::new(),
        not_offered_cloud: 0,
        unreadable: 0,
    };
    machine.scans.lock().unwrap().insert(
        "scan-1".to_string(),
        ScanRecord {
            hints: h,
            findings,
            forced,
        },
    );
}

fn clear_op(items: &[&str]) -> Op {
    Op::ClearLeftovers {
        scan: "scan-1".to_string(),
        items: items.iter().map(|i| i.to_string()).collect(),
    }
}

fn payload(step: &Step) -> HoldPayload {
    serde_json::from_str(&step.command.args[0]).unwrap()
}

#[test]
fn clear_splits_user_and_machine_items_into_two_holds() {
    let machine = Machine::new();
    scanned(&machine, false, None);
    let plan = machine
        .plan(
            &store(uninstaller),
            &[clear_op(&["item-0", "item-1", "item-2", "item-3"])],
        )
        .unwrap();
    assert_eq!(programs(&plan), vec![HOLD, HOLD]);
    let (user, root) = (&plan.steps[0], &plan.steps[1]);
    assert!(!user.needs_root && root.needs_root);
    let (user, root) = (payload(user), payload(root));
    // One entry in each root, each with an id of its own, so that every
    // restore or delete names exactly one of them.
    assert_ne!(user.holding, root.holding);
    for id in [&user.holding, &root.holding] {
        assert!(crate::cleanup::holding::valid_id(id), "{id}");
    }
    assert_eq!(root.holding, format!("{}-m", user.holding));
    let entries = with_entries(vec![
        HoldingEntry {
            id: user.holding.clone(),
            ..entry("u", false, false)
        },
        HoldingEntry {
            id: root.holding.clone(),
            ..entry("m", true, false)
        },
    ]);
    let store = store(uninstaller);
    for (id, root) in [(&user.holding, false), (&root.holding, true)] {
        for op in [
            Op::EmptyHolding {
                holding: Some(id.clone()),
            },
            Op::Restore {
                holding: id.clone(),
            },
        ] {
            let plan = entries.plan(&store, std::slice::from_ref(&op)).unwrap();
            assert_eq!(
                plan.steps
                    .iter()
                    .map(|s| (s.command.args[0].as_str(), s.needs_root))
                    .collect::<Vec<_>>(),
                vec![(id.as_str(), root)],
                "{op:?}"
            );
        }
    }
    assert_eq!(user.name, "Acme Studio");
    let at = |p: &HoldPayload| {
        p.items
            .iter()
            .map(|i| i.location.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        at(&user),
        vec![r"C:\Users\you\AppData\Roaming\Acme", r"HKCU\Software\Acme"]
    );
    assert_eq!(
        at(&root),
        vec![r"C:\Program Files (x86)\Acme", r"HKLM\Software\Acme"]
    );
}

#[test]
fn only_what_was_ticked_is_cleared_and_a_side_with_nothing_has_no_step() {
    let machine = Machine::new();
    scanned(&machine, false, None);
    let plan = machine
        .plan(&store(uninstaller), &[clear_op(&["item-0"])])
        .unwrap();
    assert_eq!(programs(&plan), vec![HOLD]);
    assert!(!plan.steps[0].needs_root, "no prompt for user items alone");
    assert_eq!(payload(&plan.steps[0]).items.len(), 1);
}

#[test]
fn what_has_gone_since_the_scan_is_left_out_of_the_clear() {
    let mut machine = Machine::new();
    scanned(&machine, false, None);
    // Only the roaming folder and the HKLM key are still there.
    machine.view = Some(FixtureView::from_json(
        r#"{
            "dirs": { "C:\\Users\\you\\AppData\\Roaming\\Acme": [] },
            "registry": { "Hklm": { "Software\\Acme": {} } }
        }"#,
    ));
    let plan = machine
        .plan(
            &store(uninstaller),
            &[clear_op(&["item-0", "item-1", "item-2", "item-3"])],
        )
        .unwrap();
    assert_eq!(programs(&plan), vec![HOLD, HOLD]);
    assert_eq!(payload(&plan.steps[0]).items.len(), 1);
    assert_eq!(
        payload(&plan.steps[1]).items[0].location,
        r"HKLM\Software\Acme"
    );
}

/// The machine at clear time, with everything the scan found still there
/// and `extra` (more registry keys, as JSON members) beside it.
fn after_the_scan(extra: &str) -> FixtureView {
    let comma = if extra.trim().is_empty() { "" } else { "," };
    FixtureView::from_json(&format!(
        r#"{{
            "folders": {{
                "Windows": "C:\\Windows",
                "ProgramFiles": "C:\\Program Files",
                "ProgramFilesX86": "C:\\Program Files (x86)",
                "ProgramData": "C:\\ProgramData",
                "AppData": "C:\\Users\\you\\AppData\\Roaming",
                "LocalAppData": "C:\\Users\\you\\AppData\\Local",
                "LocalLow": "C:\\Users\\you\\AppData\\LocalLow",
                "Profile": "C:\\Users\\you",
                "StartMenuMachine": "C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs",
                "StartMenuUser": "C:\\Users\\you\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs"
            }},
            "dirs": {{
                "C:\\Users\\you\\AppData\\Roaming\\Acme": [],
                "C:\\Program Files (x86)\\Acme": [],
                "C:\\ProgramData\\Acme": []
            }},
            "registry": {{
                "Hkcu": {{ "Software\\Acme": {{}} }},
                "Hklm": {{
                    "Software\\Acme": {{}}{comma}
                    {extra}
                }}
            }}
        }}"#
    ))
}

/// Between the scan and the clear another Acme application is installed
/// into Acme's folder. The clear rebuilds who uses what, leaves that folder
/// where it is and says why; everything else still moves.
#[test]
fn what_an_install_since_the_scan_uses_is_left_out_of_the_clear() {
    let mut machine = Machine::new();
    scanned(&machine, false, None);
    machine.view = Some(after_the_scan(
        r#""Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall": { "subkeys": ["Acme Draw"] },
        "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Acme Draw": {
            "values": [
                ["DisplayName", "Acme Draw"],
                ["Publisher", "Acme"],
                ["InstallLocation", "C:\\Program Files (x86)\\Acme\\Draw"],
                ["UninstallString", "\"C:\\Program Files (x86)\\Acme\\Draw\\uninstall.exe\""]
            ]
        }"#,
    ));
    let all = ["item-0", "item-1", "item-2", "item-3", "item-4"];
    let plan = machine
        .plan(&store(uninstaller), &[clear_op(&all)])
        .unwrap();
    assert_eq!(programs(&plan), vec![HOLD, HOLD]);
    let held: Vec<String> = plan
        .steps
        .iter()
        .flat_map(|s| payload(s).items)
        .map(|i| i.location)
        .collect();
    assert!(
        !held.contains(&r"C:\Program Files (x86)\Acme".to_string()),
        "{held:?}"
    );
    assert_eq!(held.len(), 4, "{held:?}");
    let said = machine.with(|p| clear_notices(&[clear_op(&all)], p));
    assert_eq!(
        said,
        vec![
            r"C:\Program Files (x86)\Acme was left where it is, because Acme Draw uses it now."
                .to_string()
        ]
    );

    // With nothing installed since, nothing is left out and nothing is said.
    machine.view = Some(after_the_scan(""));
    let plan = machine
        .plan(&store(uninstaller), &[clear_op(&all)])
        .unwrap();
    let held = plan
        .steps
        .iter()
        .map(|s| payload(s).items.len())
        .sum::<usize>();
    assert_eq!(held, 5);
    assert!(
        machine
            .with(|p| clear_notices(&[clear_op(&all)], p))
            .is_empty()
    );

    // When everything ticked is used now, the clear says so and moves nothing.
    machine.view = Some(after_the_scan(
        r#""Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall": { "subkeys": ["Acme Draw"] },
        "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Acme Draw": {
            "values": [
                ["DisplayName", "Acme Draw"],
                ["InstallLocation", "C:\\Program Files (x86)\\Acme\\Draw"]
            ]
        }"#,
    ));
    let err = machine
        .plan(&store(uninstaller), &[clear_op(&["item-2"])])
        .unwrap_err();
    assert!(
        err.message.contains("Acme Draw uses it now")
            && err.message.ends_with("Nothing was cleared."),
        "{}",
        err.message
    );
}

/// A firewall rule is held by its id, and only while the program it lets
/// through is still in the application's folder.
#[test]
fn a_firewall_rule_whose_program_is_now_elsewhere_is_left_out_of_the_clear() {
    let mut machine = Machine::new();
    let rule = "firewall:Acme | {1}";
    scanned_items(
        &machine,
        false,
        None,
        vec![leftover(
            "item-0",
            ItemKind::FirewallRule,
            rule,
            Scope::Machine,
        )],
    );
    let with_program = |program: &str| {
        FixtureView::from_json(&format!(
            r#"{{ "folders": {{
                "Windows": "C:\\Windows",
                "ProgramFiles": "C:\\Program Files",
                "ProgramFilesX86": "C:\\Program Files (x86)",
                "ProgramData": "C:\\ProgramData",
                "AppData": "C:\\Users\\you\\AppData\\Roaming",
                "LocalAppData": "C:\\Users\\you\\AppData\\Local",
                "LocalLow": "C:\\Users\\you\\AppData\\LocalLow",
                "Profile": "C:\\Users\\you",
                "StartMenuMachine": "C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs",
                "StartMenuUser": "C:\\Users\\you\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs"
            }},
            "dirs": {{ "C:\\Program Files (x86)\\Acme": [] }},
            "firewall": [{{ "id": "{{1}}", "name": "Acme", "program": "{program}" }}] }}"#,
            program = program.replace('\\', "\\\\")
        ))
    };
    machine.view = Some(with_program(r"C:\Program Files (x86)\Acme\acme.exe"));
    let plan = machine
        .plan(&store(uninstaller), &[clear_op(&["item-0"])])
        .unwrap();
    assert_eq!(payload(&plan.steps[0]).items[0].location, rule);

    machine.view = Some(with_program(r"C:\Program Files\Java\jre-17\bin\javaw.exe"));
    let err = machine
        .plan(&store(uninstaller), &[clear_op(&["item-0"])])
        .unwrap_err();
    assert!(
        err.message.contains(&format!(
            "{rule} was left where it is, because the program it lets through is no longer in \
             Acme Studio's folder."
        )),
        "{}",
        err.message
    );
}

/// The window leaves out, as the helper does, a service or task that could
/// not be put back without a password, and says why in the confirm.
#[test]
fn what_could_not_be_put_back_without_a_password_is_left_out_of_the_clear() {
    let mut machine = Machine::new();
    // Forced, so that the application's own services and tasks are not
    // claims, as in the scan.
    scanned_items(
        &machine,
        true,
        None,
        vec![
            leftover(
                "user",
                ItemKind::Service,
                "service:AcmeUser",
                Scope::Machine,
            ),
            leftover(
                "system",
                ItemKind::Service,
                "service:AcmeSystem",
                Scope::Machine,
            ),
            leftover(
                "nightly",
                ItemKind::Task,
                r"task:\Acme\Nightly",
                Scope::Machine,
            ),
            leftover(
                "update",
                ItemKind::Task,
                r"task:\Acme\Update",
                Scope::Machine,
            ),
        ],
    );
    machine.view = Some(FixtureView::from_json(
        r#"{
            "services": [
                { "name": "AcmeUser", "image_path": "C:\\Program Files (x86)\\Acme\\svc.exe", "account": ".\\acme-user" },
                { "name": "AcmeSystem", "image_path": "C:\\Program Files (x86)\\Acme\\svc.exe", "account": "LocalSystem" }
            ],
            "tasks": [
                { "path": "\\Acme\\Nightly", "actions": ["C:\\Program Files (x86)\\Acme\\a.exe"], "needs_password": true },
                { "path": "\\Acme\\Update", "actions": ["C:\\Program Files (x86)\\Acme\\a.exe"] }
            ]
        }"#,
    ));
    let all = ["user", "system", "nightly", "update"];
    let plan = machine
        .plan(&store(uninstaller), &[clear_op(&all)])
        .unwrap();
    let held: Vec<String> = payload(&plan.steps[0])
        .items
        .into_iter()
        .map(|i| i.location)
        .collect();
    assert_eq!(held, vec!["service:AcmeSystem", r"task:\Acme\Update"]);
    assert_eq!(
        machine.with(|p| clear_notices(&[clear_op(&all)], p)),
        vec![
            r"service:AcmeUser was left where it is, because it runs as .\acme-user, and Windows would need that account's password to put it back."
                .to_string(),
            r"task:\Acme\Nightly was left where it is, because it runs whether you are signed in or not, and Windows would need its account's password to put it back."
                .to_string(),
        ]
    );
}

#[test]
fn a_clear_of_an_unknown_scan_or_item_is_refused_with_a_sentence() {
    let machine = Machine::new();
    let err = machine
        .plan(&store(uninstaller), &[clear_op(&["item-0"])])
        .unwrap_err();
    assert!(
        err.message.contains("no longer has that list"),
        "{}",
        err.message
    );
    scanned(&machine, false, None);
    let err = machine
        .plan(&store(uninstaller), &[clear_op(&["item-99"])])
        .unwrap_err();
    assert!(
        err.message.contains("item-99 is not in the list"),
        "{}",
        err.message
    );
}

const CODE: &str = "{12345678-9ABC-DEF0-1234-56789ABCDEF0}";

#[test]
fn clearing_after_a_forced_msi_removal_unregisters_it_into_the_same_entry() {
    let machine = Machine::new();
    scanned(&machine, true, Some(CODE));
    let plan = machine
        .plan(&store(uninstaller), &[clear_op(&["item-0", "item-2"])])
        .unwrap();
    assert_eq!(programs(&plan), vec![HOLD, HOLD, MSI_UNREGISTER]);
    let msi = &plan.steps[2];
    assert!(msi.needs_root);
    assert_eq!(
        msi.command.args,
        vec![CODE.to_string(), payload(&plan.steps[1]).holding]
    );
}

/// The registration goes only with what says the application is there:
/// its uninstall key or its install folder, however that is spelt.
#[test]
fn a_forced_msi_is_unregistered_only_when_its_key_or_folder_is_ticked() {
    let machine = Machine::new();
    scanned(&machine, true, Some(CODE));
    let store = store(uninstaller);
    let plan = machine
        .plan(
            &store,
            &[clear_op(&["item-0", "item-1", "item-3", "item-4"])],
        )
        .unwrap();
    assert_eq!(programs(&plan), vec![HOLD, HOLD]);

    let uninstall = r"HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\Acme Studio_is1";
    scanned_items(
        &machine,
        true,
        Some(CODE),
        vec![
            leftover("key", ItemKind::RegistryKey, uninstall, Scope::Machine),
            leftover(
                "home",
                ItemKind::Folder,
                r"c:\program files (x86)\acme\",
                Scope::Machine,
            ),
            leftover(
                "data",
                ItemKind::Folder,
                r"C:\ProgramData\Acme",
                Scope::Machine,
            ),
        ],
    );
    for (ticked, unregistered) in [
        (&["key"][..], true),
        (&["home"][..], true),
        (&["data"][..], false),
    ] {
        let plan = machine.plan(&store, &[clear_op(ticked)]).unwrap();
        assert_eq!(
            programs(&plan).contains(&MSI_UNREGISTER),
            unregistered,
            "{ticked:?}"
        );
    }
}

#[test]
fn an_msi_removed_by_its_own_uninstaller_is_not_unregistered_again() {
    let machine = Machine::new();
    scanned(&machine, false, Some(CODE));
    let plan = machine
        .plan(&store(uninstaller), &[clear_op(&["item-2"])])
        .unwrap();
    assert_eq!(programs(&plan), vec![HOLD]);
}

#[test]
fn a_clear_names_the_application_when_it_finishes() {
    let machine = Machine::new();
    scanned(&machine, false, None);
    assert_eq!(
        crate::transaction::runner::summary_with(&[clear_op(&["item-0"])], &machine.scans),
        "Cleared leftovers from Acme Studio."
    );
}

// ---------------------------------------------------------------------------
// The holding area

fn entry(id: &str, machine: bool, waiting: bool) -> HoldingEntry {
    HoldingEntry {
        id: id.to_string(),
        name: format!("{id} app"),
        created: "2026-10-01T00:00:00Z".to_string(),
        size: 1,
        items: 1,
        scope_machine: machine,
        waiting,
        incomplete: false,
    }
}

fn with_entries(entries: Vec<HoldingEntry>) -> Machine {
    let mut machine = Machine::new();
    machine.listing = Listing {
        entries,
        unreadable: Vec::new(),
    };
    machine
}

fn roots(plan: &Plan) -> Vec<(&str, &str, bool)> {
    plan.steps
        .iter()
        .map(|s| {
            (
                s.command.program.as_str(),
                s.command.args[0].as_str(),
                s.needs_root,
            )
        })
        .collect()
}

#[test]
fn a_restore_asks_for_administrator_only_when_the_entry_has_machine_items() {
    let machine = with_entries(vec![
        entry("a", false, false),
        entry("b", false, false),
        entry("b", true, false),
    ]);
    let store = store(uninstaller);
    let only_user = machine
        .plan(
            &store,
            &[Op::Restore {
                holding: "a".into(),
            }],
        )
        .unwrap();
    assert_eq!(roots(&only_user), vec![(RESTORE, "a", false)]);
    let both = machine
        .plan(
            &store,
            &[Op::Restore {
                holding: "b".into(),
            }],
        )
        .unwrap();
    assert_eq!(
        roots(&both),
        vec![(RESTORE, "b", false), (RESTORE, "b", true)]
    );
    assert!(
        machine
            .plan(
                &store,
                &[Op::Restore {
                    holding: "c".into()
                }]
            )
            .is_err()
    );
}

#[test]
fn emptying_asks_for_administrator_only_when_the_machine_has_entries() {
    let store = store(uninstaller);
    let user_only = with_entries(vec![entry("a", false, false)])
        .plan(&store, &[Op::EmptyHolding { holding: None }])
        .unwrap();
    assert_eq!(roots(&user_only), vec![(EMPTY, "all", false)]);
    let machine = with_entries(vec![entry("a", false, false), entry("m", true, false)]);
    let all = machine
        .plan(&store, &[Op::EmptyHolding { holding: None }])
        .unwrap();
    assert_eq!(
        roots(&all),
        vec![(EMPTY, "all", false), (EMPTY, "all", true)]
    );
    let one = machine
        .plan(
            &store,
            &[Op::EmptyHolding {
                holding: Some("m".into()),
            }],
        )
        .unwrap();
    assert_eq!(roots(&one), vec![(EMPTY, "m", true)]);
}

/// Deleting an incomplete entry is the user's call, and the confirm says
/// what it is before they make it: by Delete now and by Empty alike.
#[test]
fn the_confirm_says_an_entry_is_incomplete_before_it_is_deleted() {
    // Waiting as well, which `list` never says of an incomplete entry: the
    // planner does not take its word for it.
    let mut broken = entry("broken", true, true);
    broken.incomplete = true;
    let machine = with_entries(vec![broken, entry("whole", false, false)]);
    let one = |id: &str| {
        vec![Op::EmptyHolding {
            holding: Some(id.into()),
        }]
    };
    let said = machine.with(|p| holding_notices(&one("broken"), p));
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0].contains("broken app") && said[0].contains("incomplete"),
        "{said:?}"
    );
    assert!(
        machine
            .with(|p| holding_notices(&one("whole"), p))
            .is_empty()
    );
    let all = machine.with(|p| holding_notices(&[Op::EmptyHolding { holding: None }], p));
    assert_eq!(all, said);
    // A restore is not a delete, and the age-based empty never takes it.
    let restore = [Op::Restore {
        holding: "broken".into(),
    }];
    assert!(machine.with(|p| holding_notices(&restore, p)).is_empty());
    let plan = machine.plan(&store(uninstaller), &[remove()]).unwrap();
    assert!(!plan.steps.iter().any(|s| s.command.program == EMPTY));
}

#[test]
fn a_plan_that_elevates_anyway_empties_the_waiting_machine_entries() {
    let machine = with_entries(vec![
        entry("old", true, true),
        entry("new", true, false),
        entry("mine", false, false),
    ]);
    let plan = machine.plan(&store(uninstaller), &[remove()]).unwrap();
    assert_eq!(programs(&plan), vec![UNINSTALLER, AWAIT_GONE, SCAN, EMPTY]);
    let last = plan.steps.last().unwrap();
    assert!(last.needs_root);
    assert_eq!(last.command.args, vec!["old".to_string()]);
}

#[test]
fn a_plan_that_does_not_elevate_never_adds_a_prompt_to_empty() {
    let machine = with_entries(vec![entry("old", true, true)]);
    let plan = machine.plan(&store(steam_game), &[remove()]).unwrap();
    assert_eq!(programs(&plan), vec![STEAM, AWAIT_GONE, SCAN]);
    // Nor for an entry the plan is putting back.
    let plan = machine
        .plan(
            &store(uninstaller),
            &[Op::Restore {
                holding: "old".into(),
            }],
        )
        .unwrap();
    assert_eq!(roots(&plan), vec![(RESTORE, "old", true)]);
}

#[test]
fn every_cleanup_step_is_one_of_brokeys_own() {
    let machine = with_entries(vec![entry("m", true, false)]);
    scanned(&machine, true, Some(CODE));
    let store = store(uninstaller);
    for op in [
        clear_op(&["item-0", "item-2"]),
        Op::Restore {
            holding: "m".into(),
        },
        Op::EmptyHolding { holding: None },
        Op::ForceRemove { package: acme() },
    ] {
        let plan = machine.plan(&store, std::slice::from_ref(&op)).unwrap();
        assert!(
            plan.steps.iter().all(|s| is_internal(&s.command.program)),
            "{op:?}: {:?}",
            programs(&plan)
        );
    }
}

#[test]
fn a_clear_with_everything_gone_says_so_rather_than_succeeding() {
    let mut machine = Machine::new();
    scanned(&machine, false, None);
    machine.view = Some(FixtureView::from_json("{}"));
    let err = machine
        .plan(&store(uninstaller), &[clear_op(&["item-0", "item-2"])])
        .unwrap_err();
    assert_eq!(err.message, ALL_GONE);
}

#[test]
fn a_clear_with_nothing_ticked_says_so() {
    let machine = Machine::new();
    scanned(&machine, false, None);
    let err = machine
        .plan(&store(uninstaller), &[clear_op(&[])])
        .unwrap_err();
    assert_eq!(err.message, NOTHING_TICKED);
}

#[test]
fn a_clear_is_never_planned_beside_anything_else() {
    let machine = Machine::new();
    scanned(&machine, false, None);
    let store = store(uninstaller);
    for ops in [
        vec![clear_op(&["item-0"]), clear_op(&["item-1"])],
        vec![remove(), clear_op(&["item-0"])],
        vec![clear_op(&["item-0"]), Op::EmptyHolding { holding: None }],
    ] {
        let err = machine.plan(&store, &ops).unwrap_err();
        assert_eq!(err.message, CLEAR_ALONE, "{ops:?}");
    }
}

#[test]
fn a_steam_game_is_never_removed_by_force() {
    let mut machine = Machine::new();
    let mut game = hints();
    game.name = "Cyberpunk 2077".to_string();
    game.uninstall_key = Some((Hive::Hklm32, "Steam App 1091500".to_string()));
    machine.hints = Some(game);
    let err = machine
        .plan(&store(steam_game), &[Op::ForceRemove { package: acme() }])
        .unwrap_err();
    assert!(err.message.contains("Steam library"), "{}", err.message);
}
