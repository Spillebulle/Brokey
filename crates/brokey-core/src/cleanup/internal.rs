//! Brokey's own steps: the ones a plan carries out itself rather than by
//! starting a program. A step whose program starts with `brokey:` is one of
//! these, and neither the runner nor the helper ever spawns it.
//!
//! Four may run as Administrator, and they are the cleanup's entries on the
//! closed list: [`HOLD`] (move these items to the holding area), [`RESTORE`]
//! (put a holding entry back), [`EMPTY`] (delete one for good, or all) and
//! [`MSI_UNREGISTER`] (clear what the Windows Installer still records for a
//! product that was removed by force). The other four, [`AWAIT_GONE`],
//! [`SCAN`], [`SHELL_OPEN`] and [`STEAM_UNINSTALL`], only ever run as the user and are refused in
//! a step that asks for Administrator.
//!
//! [`check`] is the rule, and it is asked twice: by the closed list before
//! the first item of a plan moves, which refuses the whole plan, and again by
//! [`run`] immediately before the step acts, so that the time between the
//! check and the move is as short as it can be. What it asks of every path
//! is spec §8's: a full path on a fixed disk, no `.` or `..`, not a place
//! the guard protects, nor one of the places Windows keeps beside them that
//! the guard has no reason to name, and **no junction, symbolic link or
//! mount point at any part of it**, found by opening each part without
//! following it. A part that cannot be opened, or whose tag cannot be read,
//! is refused: not being able to look is never a reason to go ahead.
//!
//! The ownership guard is the window's job and is not repeated here; the
//! helper's job is that no path, however it got into a plan, reaches a
//! protected place.

use super::guard;
use super::hints::Hints;
use super::holding::{self, Failure, HeldState, HoldItem, HoldReport, Mutate};
use super::scan;
use super::view::{Hive, Known, SystemView, ViewError};
use crate::model::{Command, Event, Findings, ItemKind, Package, SourceKind, Step};
use crate::sources::windows::shortcut;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What every one of Brokey's own steps starts with.
pub const PREFIX: &str = "brokey:";
/// `args[0]` is a [`HoldPayload`] as JSON.
pub const HOLD: &str = "brokey:hold";
/// `args[0]` is the holding id.
pub const RESTORE: &str = "brokey:restore";
/// `args[0]` is the holding id, or `all`.
pub const EMPTY: &str = "brokey:empty";
/// `args[0]` is the ProductCode, braces and all; `args[1]` the holding id
/// its registration is exported to, so that it can be put back.
pub const MSI_UNREGISTER: &str = "brokey:msi-unregister";
/// `args[0]` is the [`Hints`] as JSON. As the user only.
pub const AWAIT_GONE: &str = "brokey:await-gone";
/// `args[0]` is a [`ScanPayload`] as JSON. As the user only.
pub const SCAN: &str = "brokey:scan";
/// `args` are the program and its arguments. As the user only.
pub const SHELL_OPEN: &str = "brokey:shell-open";
/// `args[0]` is a Steam app id, digits only. As the user only: the address
/// `steam://uninstall/<id>` is handed to Explorer, which hands it to Steam.
pub const STEAM_UNINSTALL: &str = "brokey:steam-uninstall";

/// The start of the uninstall key name Steam gives each game.
pub const STEAM_APP_KEY: &str = "Steam App ";

/// How long [`SHELL_OPEN`] waits on the uninstaller between two looks at
/// the cancel flag.
pub const SHELL_OPEN_SLICE: Duration = Duration::from_millis(250);

/// Why [`SHELL_OPEN`] stopped: the user stopped waiting. The uninstaller is
/// never killed; it is the user's window.
pub const STOPPED_WAITING: &str = "Brokey stopped waiting for the uninstaller. It is still open; \
                                   finish it or close it yourself.";

/// How long [`AWAIT_GONE`] leaves between two looks at the uninstall key.
pub const AWAIT_POLL: Duration = Duration::from_secs(2);

/// What the activity panel says while [`AWAIT_GONE`] waits, beside its two
/// buttons.
pub const WAITING_FOR_UNINSTALLER: &str =
    "Waiting for the uninstaller to finish. Close its window when it is done.";

/// Why [`AWAIT_GONE`] failed: the user said the uninstaller had finished and
/// the application is still listed. Nothing is scanned after it, because a
/// still-installed application has no leftovers, and the page offers forced
/// removal when a plan ends with this.
pub fn not_removed(name: &str) -> String {
    format!(
        "The uninstaller did not remove {name}. It may have been cancelled. Forced removal \
         clears it without the uninstaller."
    )
}

/// [`not_removed`] for a Steam game. Steam keeps its own record of the game,
/// so forced removal is never offered for one: the game is removed from
/// Steam's library instead.
pub fn steam_not_removed(name: &str) -> String {
    format!(
        "Steam did not remove {name}. It may have been cancelled. Remove the game from your \
         Steam library, then look in Installed."
    )
}

/// Whether an uninstall key is one Steam wrote for a game.
pub fn is_steam_app_key(key: &str) -> bool {
    key.strip_prefix(STEAM_APP_KEY)
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

/// A Steam app id as [`STEAM_UNINSTALL`] accepts it: one to ten digits.
pub fn is_steam_app_id(id: &str) -> bool {
    (1..=10).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

/// The steps that may run as Administrator.
const ROOT: [&str; 4] = [HOLD, RESTORE, EMPTY, MSI_UNREGISTER];
/// The steps that never may.
const SESSION_ONLY: [&str; 4] = [AWAIT_GONE, SCAN, SHELL_OPEN, STEAM_UNINSTALL];

/// The one thing that reaches `transaction::elevate::shell_open`: an
/// Add/Remove uninstaller the closed list has just refused because this
/// account could replace it, and for no other reason. Its fields are private
/// to this module, and the only constructor, [`Refused::by`], is called only
/// by [`SHELL_OPEN`]'s step with the closed list's answer in hand. So no
/// other code in the workspace can ask the shell to open anything.
pub(crate) struct Refused {
    program: PathBuf,
    args: Vec<String>,
}

impl Refused {
    /// The witness, when `answer` is the closed list refusing `program` as
    /// one this account can replace; `None` for any other answer.
    fn by(program: &str, args: &[String], answer: Result<(), String>) -> Option<Refused> {
        match answer {
            Err(why) if crate::transaction::allow::is_can_be_replaced(&why) => Some(Refused {
                program: PathBuf::from(program),
                args: args.to_vec(),
            }),
            _ => None,
        }
    }

    pub(crate) fn program(&self) -> &Path {
        &self.program
    }

    pub(crate) fn args(&self) -> &[String] {
        &self.args
    }
}

/// What [`HOLD`] carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldPayload {
    pub holding: String,
    /// The application's name, as the Settings page shows the entry.
    pub name: String,
    pub items: Vec<HoldItem>,
}

/// What [`SCAN`] carries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanPayload {
    pub hints: Hints,
    /// A forced removal: the application is still listed, its uninstaller
    /// was not run, and its own folder, key and Windows Installer
    /// registration are not claims that shield it.
    pub force: bool,
}

/// One scan, kept for the clear that follows it.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanRecord {
    /// What it searched by.
    pub hints: Hints,
    pub findings: Findings,
    /// Made for a forced removal, so clearing it also clears the product's
    /// Windows Installer registration when it has one.
    pub forced: bool,
}

/// Every scan this session has made, by scan id. [`SCAN`] fills it;
/// clearing reads it, so the page never sends a path.
pub type Scans = Arc<Mutex<HashMap<String, ScanRecord>>>;

/// What one of Brokey's own steps may reach while it runs.
pub struct InternalContext<'a> {
    pub plan: &'a str,
    pub step: usize,
    /// For `Event::Waiting` and `Event::Leftovers`.
    pub sink: &'a mut dyn FnMut(Event),
    /// Whether the user has cancelled the plan.
    pub cancelled: &'a dyn Fn() -> bool,
    /// Set when the user says the uninstaller has finished.
    pub finished: Arc<AtomicBool>,
    pub scans: Scans,
}

/// Whether a step is one of Brokey's own, and so is never started as a
/// program. Any `brokey:` name is, known or not: an unknown one is refused
/// by [`check`] rather than handed to the system to look for.
pub fn is_internal(program: &str) -> bool {
    program.starts_with(PREFIX)
}

/// The two holding roots: the user's, for a step that runs as the user, and
/// the machine's, Administrators only, for one that runs as Administrator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Roots {
    pub user: PathBuf,
    pub machine: PathBuf,
}

impl Roots {
    pub fn live() -> Roots {
        Roots {
            user: holding::user_root(),
            machine: holding::machine_root(),
        }
    }

    fn of(&self, root: bool) -> &Path {
        if root { &self.machine } else { &self.user }
    }
}

// ---------------------------------------------------------------------------
// The rule

/// Whether an item is to be moved now or is being put back, which differs in
/// one place: what is to be moved must be there, all the way down, and what
/// is being put back is mostly not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Hold,
    Restore,
}

/// What a check asks of the machine directly rather than through the
/// guard's view: whether each part of a path is a link, and whether the way
/// to a registry key is. [`Live`] opens them; a test answers from a table.
pub(crate) trait Machine {
    /// One part of a path, in its `\\?\` spelling, opened as itself.
    fn part(&self, verbatim: &str) -> Part;
    /// The way to a key, each part opened as itself.
    fn key(&self, hive: Hive, path: &str) -> Part;
}

/// This machine.
pub(crate) struct Live;

impl Machine for Live {
    fn part(&self, verbatim: &str) -> Part {
        open_part(verbatim)
    }

    fn key(&self, hive: Hive, path: &str) -> Part {
        key_part(hive, path)
    }
}

/// The two things a check reads with.
struct Ask<'a> {
    view: &'a dyn SystemView,
    machine: &'a dyn Machine,
}

/// Whether one of Brokey's own steps may run, and why not when it may not.
/// `view` is the machine as the guard reads it; `roots` is where a restore's
/// manifest is read from.
pub fn check(step: &Step, view: &dyn SystemView, roots: &Roots) -> Result<(), String> {
    check_with(step, view, roots, &Live)
}

/// [`check`] with what is asked of the machine directly handed in.
pub(crate) fn check_with(
    step: &Step,
    view: &dyn SystemView,
    roots: &Roots,
    machine: &dyn Machine,
) -> Result<(), String> {
    let ask = Ask { view, machine };
    let program = step.command.program.as_str();
    let root = step.needs_root;
    if !is_internal(program) || !(ROOT.contains(&program) || SESSION_ONLY.contains(&program)) {
        return Err(format!(
            "The helper refused a step Brokey does not have: {program}. Update Brokey and try \
             again."
        ));
    }
    if !step.command.env.is_empty() || step.command.cwd.is_some() {
        return Err(format!(
            "The helper refused {program}, because it carries an environment or a working \
             folder and Brokey's own steps never do. Start the operation again from Brokey."
        ));
    }
    if SESSION_ONLY.contains(&program) {
        if root {
            return Err(format!(
                "The helper refused {program}, because it only ever runs as you and never as \
                 Administrator. Start the operation again from Brokey."
            ));
        }
        // What the shell is asked to open is an uninstaller the registry
        // names in full, never a word for it to look up, and a program
        // starting itself, never a document or a folder for some other
        // program to open.
        if program == SHELL_OPEN
            && step.command.args.first().is_none_or(|p| {
                path_parts(p).is_none() || !p.to_ascii_lowercase().ends_with(".exe")
            })
        {
            return Err(format!(
                "Brokey refused {program}, because it does not name a program by its full \
                 path. Remove the application again from Brokey."
            ));
        }
        if program == STEAM_UNINSTALL
            && !matches!(step.command.args.as_slice(), [id] if is_steam_app_id(id))
        {
            return Err(format!(
                "Brokey refused {program}, because it does not carry one Steam app id. Remove \
                 the game again from Brokey."
            ));
        }
        return Ok(());
    }
    let args = &step.command.args;
    match program {
        HOLD => {
            let payload = hold_payload(args)?;
            check_hold(&payload, root, &ask)
        }
        RESTORE => {
            let id = one_argument(program, args)?;
            check_id(id)?;
            check_restore(roots.of(root), id, root, &ask)
        }
        EMPTY => {
            let id = one_argument(program, args)?;
            if id != "all" {
                check_id(id)?;
            }
            Ok(())
        }
        _ => {
            if !root {
                return Err(format!(
                    "The helper refused {program}, because the Windows Installer's records \
                     can only be changed as Administrator. Start the operation again from \
                     Brokey."
                ));
            }
            let [code, id] = args.as_slice() else {
                return Err(format!(
                    "The helper refused {program}, because it needs a product code and a \
                     holding area entry and nothing else. Start the operation again from Brokey."
                ));
            };
            if crate::sources::windows::arp::product_code(code).is_none()
                || guard::packed_product_code(code).is_none()
            {
                return Err(format!(
                    "The helper refused to unregister \"{code}\", because it is not a Windows \
                     Installer product code. Start the operation again from Brokey."
                ));
            }
            check_id(id)
        }
    }
}

fn one_argument<'a>(program: &str, args: &'a [String]) -> Result<&'a str, String> {
    match args {
        [one] => Ok(one),
        _ => Err(format!(
            "The helper refused {program}, because it takes exactly one holding area entry. \
             Start the operation again from Brokey."
        )),
    }
}

fn check_id(id: &str) -> Result<(), String> {
    if holding::valid_id(id) {
        Ok(())
    } else {
        Err(format!(
            "The helper refused the holding area entry \"{id}\", because it is not a name \
             Brokey gives one. Start the operation again from Brokey."
        ))
    }
}

fn hold_payload(args: &[String]) -> Result<HoldPayload, String> {
    let bad = |why: &str| {
        format!(
            "The helper refused a step to move leftovers to the holding area, because {why}. \
             Start the operation again from Brokey."
        )
    };
    let [json] = args else {
        return Err(bad("it is not one list of items"));
    };
    serde_json::from_str(json).map_err(|_| bad("its list of items could not be read"))
}

fn check_hold(payload: &HoldPayload, root: bool, ask: &Ask) -> Result<(), String> {
    check_id(&payload.holding)?;
    let name = payload.name.trim();
    if name.is_empty() || name.chars().count() > 260 || name.chars().any(char::is_control) {
        return Err(
            "The helper refused a step to move leftovers to the holding area, because the \
             application's name is not one Brokey would show. Start the operation again from \
             Brokey."
                .to_string(),
        );
    }
    if payload.items.is_empty() {
        return Err(
            "The helper refused a step to move leftovers to the holding area, because it has \
             nothing in it. Start the operation again from Brokey."
                .to_string(),
        );
    }
    let set: Vec<String> = payload.items.iter().map(|i| i.location.clone()).collect();
    for item in &payload.items {
        check_item(item, root, Mode::Hold, ask, &set)?;
    }
    Ok(())
}

/// The restore's own check: every destination in the manifest, by the same
/// rules as a clear. That the manifest lives in an Administrators-only
/// folder is a reason to trust it more, not a reason to stop checking.
fn check_restore(dir: &Path, id: &str, root: bool, ask: &Ask) -> Result<(), String> {
    let manifest = match holding::read_manifest(&dir.join(id)) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => {
            return Err(format!(
                "There is nothing in the holding area called {id}, so nothing was restored."
            ));
        }
        Err(e) => {
            return Err(format!(
                "The holding area's record of {id} could not be read ({e}), so nothing was \
                 restored."
            ));
        }
    };
    if manifest.id != id {
        return Err(format!(
            "The holding area's record of {id} names another entry, so nothing was restored."
        ));
    }
    // A covered item goes back with what covers it, never on its own.
    let items: Vec<&HoldItem> = manifest
        .items
        .iter()
        .filter(|h| h.state != HeldState::Covered)
        .map(|h| &h.item)
        .collect();
    let set: Vec<String> = items.iter().map(|i| i.location.clone()).collect();
    for item in items {
        if root && msi_registration(item) {
            continue;
        }
        check_item(item, root, Mode::Restore, ask, &set)?;
    }
    Ok(())
}

/// The sentence for an item the rule refuses: which item, why, and what to
/// do. `why` starts in lower case and has no full stop.
fn refuse(location: &str, mode: Mode, why: &str) -> String {
    match mode {
        Mode::Hold => format!(
            "The helper refused to move {location} to the holding area, because {why}. Nothing \
             was moved. Look for leftovers again and try again."
        ),
        Mode::Restore => format!(
            "The helper refused to put {location} back, because {why}. Nothing was restored."
        ),
    }
}

fn check_item(
    item: &HoldItem,
    root: bool,
    mode: Mode,
    ask: &Ask,
    set: &[String],
) -> Result<(), String> {
    let location = item.location.as_str();
    let view = ask.view;
    match item.kind {
        ItemKind::File | ItemKind::Folder | ItemKind::Shortcut => {
            check_path(location, mode, ask, set)
        }
        ItemKind::RegistryKey | ItemKind::RegistryValue => {
            check_key(item, root, mode, ask.machine)?;
            if guard::is_protected_among(location, view, set) {
                return Err(refuse(location, mode, "it is a place Brokey never clears"));
            }
            Ok(())
        }
        ItemKind::Service | ItemKind::Task | ItemKind::FirewallRule => {
            if !root {
                return Err(refuse(
                    location,
                    mode,
                    "services, scheduled tasks and firewall rules are only cleared as \
                     Administrator",
                ));
            }
            check_system(item, mode, view)
        }
    }
}

// ---------------------------------------------------------------------------
// Paths

/// Characters Windows does not allow in a name, beside control characters.
const NOT_IN_A_NAME: [char; 8] = ['<', '>', ':', '"', '|', '?', '*', '/'];

/// Names Windows reads as a device wherever they are, with any extension.
const DEVICES: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// What sits at the root of the system drive that Windows needs and that is
/// nobody's leftover. Names starting with `$` are Windows's too.
const DRIVE_ROOT: [&str; 11] = [
    "boot",
    "bootmgr",
    "bootnxt",
    "efi",
    "recovery",
    "system volume information",
    "pagefile.sys",
    "hiberfil.sys",
    "swapfile.sys",
    "config.msi",
    "documents and settings",
];

/// A path as a drive letter and its names, the `\\?\` prefix taken off. The
/// verbatim spelling is accepted because it is how a path longer than 260
/// characters is written; every other `\\` spelling (a share, a device, the
/// object namespace) is not a path on this computer's disks.
fn path_parts(location: &str) -> Option<(char, Vec<&str>)> {
    let rest = location.strip_prefix(r"\\?\").unwrap_or(location);
    let bytes = rest.as_bytes();
    if bytes.len() < 4 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' || bytes[2] != b'\\' {
        return None;
    }
    let names: Vec<&str> = rest[3..].trim_end_matches('\\').split('\\').collect();
    Some((char::from(bytes[0]), names))
}

/// Why a path's own spelling is refused, before the disk is asked anything.
fn spelling_problem(names: &[&str]) -> Option<&'static str> {
    if names
        .iter()
        .any(|n| n.is_empty() || *n == "." || *n == "..")
    {
        return Some("it has a part that is empty, \".\" or \"..\"");
    }
    for name in names {
        if name
            .chars()
            .any(|c| c.is_control() || NOT_IN_A_NAME.contains(&c))
        {
            return Some("it has a character Windows does not allow in a name");
        }
        if name.ends_with(['.', ' ']) {
            return Some(
                "it has a name ending in a dot or a space, which Windows reads as another name",
            );
        }
        let stem = name
            .split('.')
            .next()
            .unwrap_or(name)
            .trim_end()
            .to_lowercase();
        if DEVICES.contains(&stem.as_str()) {
            return Some("it has a name Windows reads as a device");
        }
    }
    None
}

/// The places Windows keeps that the guard has no reason to name, since no
/// scan would offer them: what is at the root of a drive for booting and
/// recovery, `ProgramData\Microsoft`, and the `Windows …` folders and
/// `Common Files\Microsoft Shared` under Program Files.
fn windows_keeps(drive: char, names: &[&str], view: &dyn SystemView) -> bool {
    let first = names[0].to_lowercase();
    if first.starts_with('$') || DRIVE_ROOT.contains(&first.as_str()) {
        return true;
    }
    let path = shortcut::normalise(&format!("{drive}:\\{}", names.join("\\")));
    let under = |which: Known, tail: &str| {
        view.known_folder(which).is_some_and(|f| {
            let base = shortcut::normalise(&f.to_string_lossy());
            let dir = format!("{base}\\{tail}");
            path == dir || path.starts_with(&format!("{dir}\\"))
        })
    };
    // The all-users Start menu is under `ProgramData\Microsoft`, but what is
    // inside its `Programs` folder is the applications'. The folder itself,
    // and the Start menu above it, stay Windows's.
    let in_machine_menu = view.known_folder(Known::StartMenuMachine).is_some_and(|f| {
        let menu = shortcut::normalise(&f.to_string_lossy());
        path.starts_with(&format!("{menu}\\"))
    });
    if (under(Known::ProgramData, "microsoft") && !in_machine_menu)
        || under(Known::ProgramFiles, "common files\\microsoft shared")
        || under(Known::ProgramFilesX86, "common files\\microsoft shared")
    {
        return true;
    }
    [Known::ProgramFiles, Known::ProgramFilesX86]
        .into_iter()
        .any(|which| {
            view.known_folder(which).is_some_and(|f| {
                let base = shortcut::normalise(&f.to_string_lossy());
                path.strip_prefix(&format!("{base}\\"))
                    .is_some_and(|rest| rest.starts_with("windows"))
            })
        })
}

fn check_path(location: &str, mode: Mode, ask: &Ask, set: &[String]) -> Result<(), String> {
    let view = ask.view;
    let Some((drive, names)) = path_parts(location) else {
        return Err(refuse(
            location,
            mode,
            "it is not a full path on a drive of this computer",
        ));
    };
    if let Some(why) = spelling_problem(&names) {
        return Err(refuse(location, mode, why));
    }
    if !fixed_volume(drive) {
        return Err(refuse(
            location,
            mode,
            "it is not on a disk that is part of this computer",
        ));
    }
    if windows_keeps(drive, &names, view) {
        return Err(refuse(location, mode, "it is a place Brokey never clears"));
    }
    // The links before the guard, so that a folder swapped for a junction is
    // refused as one, and not as whatever place the junction leads to.
    no_link_on_the_way(location, drive, &names, mode, ask.machine)?;
    let plain = format!("{drive}:\\{}", names.join("\\"));
    if guard::is_protected_among(&plain, view, set) {
        return Err(refuse(location, mode, "it is a place Brokey never clears"));
    }
    Ok(())
}

#[cfg(windows)]
fn fixed_volume(drive: char) -> bool {
    crate::transaction::allow::a_fixed_volume(drive as u8)
}

#[cfg(not(windows))]
fn fixed_volume(_: char) -> bool {
    false
}

/// What opening one part of a path, or of the way to a key, without
/// following it found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum Part {
    Plain,
    /// A junction, a symbolic link or a mount point.
    Link,
    Missing,
    /// It could not be opened, or its tag could not be read.
    Unknown,
}

/// Every part of the path, from the first name under the drive to the last,
/// opened as itself and never as what it leads to. A name surrogate at any
/// part refuses the item, and so does a part that cannot be asked. Being
/// missing refuses an item to be moved, which must be there; for one being
/// put back, the first missing part ends the walk, since the restore makes
/// what is missing as plain folders.
fn no_link_on_the_way(
    location: &str,
    drive: char,
    names: &[&str],
    mode: Mode,
    machine: &dyn Machine,
) -> Result<(), String> {
    let mut path = format!(r"\\?\{drive}:");
    for name in names {
        path.push('\\');
        path.push_str(name);
        let shown = path.trim_start_matches(r"\\?\");
        match machine.part(&path) {
            Part::Plain => {}
            Part::Link => {
                return Err(refuse(
                    location,
                    mode,
                    &format!(
                        "{shown} is a junction, a symbolic link or a mount point, which leads \
                         somewhere else"
                    ),
                ));
            }
            Part::Missing if mode == Mode::Restore => return Ok(()),
            Part::Missing => {
                return Err(refuse(location, mode, &format!("{shown} is not there")));
            }
            Part::Unknown => {
                return Err(refuse(
                    location,
                    mode,
                    &format!("{shown} could not be checked for a junction or a symbolic link"),
                ));
            }
        }
    }
    Ok(())
}

/// One part, opened with `FILE_FLAG_OPEN_REPARSE_POINT` so that a link is
/// opened as the link, and `FILE_FLAG_BACKUP_SEMANTICS` so that a folder
/// can be opened at all; then its attributes and reparse tag.
#[cfg(windows)]
fn open_part(verbatim: &str) -> Part {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GetLastError,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileAttributeTagInfo,
        GetFileInformationByHandleEx, OPEN_EXISTING,
    };

    let wide: Vec<u16> = std::ffi::OsStr::new(verbatim)
        .encode_wide()
        .chain([0])
        .collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call; no security
    // attributes and no template are passed.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    let Some(handle) = crate::system::windows::owned_handle(raw) else {
        // SAFETY: reads this thread's last error, set by the failed call.
        return match unsafe { GetLastError() } {
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Part::Missing,
            _ => Part::Unknown,
        };
    };
    let mut info = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    // SAFETY: the handle is open for the life of `handle`; `info` is a
    // plain-data out parameter of exactly the size passed.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle() as _,
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Part::Unknown;
    }
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && is_name_surrogate(info.ReparseTag)
    {
        Part::Link
    } else {
        Part::Plain
    }
}

#[cfg(not(windows))]
fn open_part(_: &str) -> Part {
    Part::Unknown
}

/// The way to a key, each part opened as itself (`REG_OPTION_OPEN_LINK`),
/// as the live backend walks it when it acts.
#[cfg(windows)]
fn key_part(hive: Hive, path: &str) -> Part {
    use super::holding::windows::{KeyWay, key_way};
    match key_way(hive, path) {
        KeyWay::Clear => Part::Plain,
        KeyWay::Link => Part::Link,
        KeyWay::Missing => Part::Missing,
        KeyWay::Unreadable => Part::Unknown,
    }
}

#[cfg(not(windows))]
fn key_part(_: Hive, _: &str) -> Part {
    Part::Unknown
}

/// `IsReparseTagNameSurrogate`: the bit Windows sets on every tag whose
/// reparse point names another place (junctions, symbolic links, mount
/// points). Cloud files and App Execution Aliases do not set it.
#[cfg(windows)]
fn is_name_surrogate(tag: u32) -> bool {
    tag & 0x2000_0000 != 0
}

// ---------------------------------------------------------------------------
// The registry

/// Keys under `Software\Classes` that Windows keeps and the guard has no
/// reason to name, since no scan offers them, in lower case: the class
/// store's own roots, and Windows's own ProgIDs, which an application that
/// made itself their handler leaves its command in. A key is refused when it
/// is one of these or lies inside one. `Clients`, `RegisteredApplications`
/// and `ODBC`, beside `Classes`, are refused only down to the list itself,
/// since what an application put in them is that application's.
const CLASSES_WINDOWS_KEEPS: &[&str] = &[
    "installer",
    "local settings",
    "protocols",
    "mime",
    "packagedcom",
    "extensions",
    "activatableclasses",
    "wow6432node",
    "batfile",
    "cmdfile",
    "comfile",
    "exefile",
    "regfile",
    "txtfile",
    "inifile",
    "inffile",
    "vbsfile",
    "jsfile",
    "wsffile",
    "msi.package",
    "msi.patch",
    "microsoft.powershellscript.1",
    "microsoft.powershelldata.1",
    "microsoft.powershellmodule.1",
    "lnkfile",
    "htmlfile",
    "folder",
    "directory",
    "drive",
    "allfilesystemobjects",
    "compressedfolder",
];

fn key_windows_keeps(parts: &[String], value: bool) -> bool {
    let mut parts: Vec<&str> = parts.iter().map(String::as_str).collect();
    if parts.len() >= 2 && parts[1] == "wow6432node" {
        parts.remove(1);
    }
    match parts.as_slice() {
        ["software", "classes", first, ..] => CLASSES_WINDOWS_KEEPS.contains(first),
        ["software", "clients" | "odbc", rest @ ..] => rest.len() <= 1,
        // Each value in it is one application's registration.
        ["software", "registeredapplications"] => !value,
        _ => false,
    }
}

/// The hive and key names of a registry location, as `holding` reads it.
fn key_parts(location: &str) -> Option<(String, Vec<String>, Option<&str>)> {
    let (key, value) = match location.split_once(" | ") {
        Some((key, value)) => (key, Some(value)),
        None => (location, None),
    };
    let (hive, rest) = key.split_once('\\')?;
    let parts = rest
        .trim_end_matches('\\')
        .split('\\')
        .map(str::to_lowercase)
        .collect();
    Some((hive.to_ascii_uppercase(), parts, value))
}

fn check_key(item: &HoldItem, root: bool, mode: Mode, machine: &dyn Machine) -> Result<(), String> {
    let location = item.location.as_str();
    let Some((hive, parts, value)) = key_parts(location) else {
        return Err(refuse(
            location,
            mode,
            "it is not a registry key Brokey knows",
        ));
    };
    let allowed: &[&str] = if root { &["HKLM", "HKLM32"] } else { &["HKCU"] };
    if !allowed.contains(&hive.as_str()) {
        let why = if root {
            "as Administrator only keys in HKLM or its 32-bit view are cleared"
        } else {
            "as you only keys in HKCU are cleared"
        };
        return Err(refuse(location, mode, why));
    }
    if (item.kind == ItemKind::RegistryValue) != value.is_some()
        || value.is_some_and(|v| v.chars().any(char::is_control))
    {
        return Err(refuse(
            location,
            mode,
            "it is not a registry key Brokey knows",
        ));
    }
    if parts
        .iter()
        .any(|p| p.is_empty() || p == "." || p == ".." || p.chars().any(char::is_control))
    {
        return Err(refuse(
            location,
            mode,
            "it has a part that is empty, \".\" or \"..\"",
        ));
    }
    if key_windows_keeps(&parts, value.is_some()) {
        return Err(refuse(location, mode, "it is a place Brokey never clears"));
    }
    // The way to it, opened part by part as the live backend will open it:
    // a registry link anywhere on it would take the clear somewhere else.
    let hive = match hive.as_str() {
        "HKLM" => Hive::Hklm,
        "HKLM32" => Hive::Hklm32,
        _ => Hive::Hkcu,
    };
    match machine.key(hive, &parts.join("\\")) {
        Part::Plain => Ok(()),
        Part::Missing if mode == Mode::Restore => Ok(()),
        Part::Missing => Err(refuse(location, mode, "it is not there")),
        Part::Link => Err(refuse(
            location,
            mode,
            "it is a registry link to another key, or has one on the way to it",
        )),
        Part::Unknown => Err(refuse(
            location,
            mode,
            "the way to it could not be checked for a registry link",
        )),
    }
}

/// Whether a key or value lies under `Software\Classes\.<extension>`, which
/// is cleared after the ProgID keys it may name, so that an extension never
/// outlives, even for a moment, the guard's reason to let it go.
fn is_extension(location: &str) -> bool {
    key_parts(location).is_some_and(|(_, mut parts, _)| {
        if parts.len() >= 2 && parts[1] == "wow6432node" {
            parts.remove(1);
        }
        parts.len() >= 3
            && parts[0] == "software"
            && parts[1] == "classes"
            && parts[2].starts_with('.')
    })
}

/// The items in the order they are moved: everything else first, then the
/// extension keys, in the order they came.
fn in_order(items: &[HoldItem]) -> Vec<HoldItem> {
    let (extensions, rest): (Vec<&HoldItem>, Vec<&HoldItem>) =
        items.iter().partition(|i| is_extension(&i.location));
    rest.into_iter().chain(extensions).cloned().collect()
}

// ---------------------------------------------------------------------------
// Services, scheduled tasks and firewall rules

/// Whether a command runs a program of Windows's own, or one whose place
/// cannot be told: anything in the Windows folder or a protected place, a
/// path that is not a full one (a driver's `System32\drivers\…`), or one
/// with a variable left in it.
fn runs_windows(command: &str, view: &dyn SystemView) -> bool {
    let program = scan::program(command, &scan::environment(view));
    let program = program.trim().trim_matches('"');
    let Some((drive, names)) = path_parts(program) else {
        return true;
    };
    program.contains('%')
        || spelling_problem(&names).is_some()
        || windows_keeps(drive, &names, view)
        || guard::is_protected(program, view)
}

fn sane_name(name: &str, separators: bool) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name.chars().count() <= 256
        && !name.chars().any(char::is_control)
        && (separators || !name.contains(['\\', '/']))
}

fn check_system(item: &HoldItem, mode: Mode, view: &dyn SystemView) -> Result<(), String> {
    let location = item.location.as_str();
    let windows = |what: &str| {
        refuse(
            location,
            mode,
            &format!("the {what} runs a program of Windows's own, or one Brokey cannot place"),
        )
    };
    let unlisted = |what: &str| {
        refuse(
            location,
            mode,
            &format!("Windows's list of {what} could not be read whole"),
        )
    };
    match item.kind {
        ItemKind::Service => {
            let Some(name) = location
                .strip_prefix("service:")
                .filter(|n| sane_name(n, false))
            else {
                return Err(refuse(location, mode, "it is not a service name"));
            };
            let services = view.services();
            match services
                .items
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(name))
            {
                Some(s) if runs_windows(&s.image_path, view) => Err(windows("service")),
                Some(_) => Ok(()),
                None if services.unreadable > 0 && mode == Mode::Hold => Err(unlisted("services")),
                // A service being put back is not there, so there is no
                // program of its to ask about here; what it will run is read
                // from its exported key in the holding area. That file is
                // safe to take at its word only because `trust_entry`
                // refuses an entry folder anyone but an Administrator could
                // have written, before the restore reads anything from it.
                None => Ok(()),
            }
        }
        ItemKind::Task => {
            let path = location.strip_prefix("task:").unwrap_or_default();
            let parts: Vec<&str> = path.strip_prefix('\\').unwrap_or("").split('\\').collect();
            if !sane_name(path, true)
                || parts
                    .iter()
                    .any(|p| p.trim().is_empty() || *p == "." || *p == "..")
            {
                return Err(refuse(location, mode, "it is not a scheduled task's path"));
            }
            if guard::is_protected(location, view) {
                return Err(refuse(location, mode, "it is a place Brokey never clears"));
            }
            let tasks = view.tasks();
            match tasks
                .items
                .iter()
                .find(|t| t.path.eq_ignore_ascii_case(path))
            {
                Some(t) if t.actions.iter().any(|a| runs_windows(a, view)) => {
                    Err(windows("scheduled task"))
                }
                Some(_) => Ok(()),
                None if tasks.unreadable > 0 && mode == Mode::Hold => {
                    Err(unlisted("scheduled tasks"))
                }
                None => Ok(()),
            }
        }
        _ => {
            let not_one = || {
                refuse(
                    location,
                    mode,
                    "it is not a firewall rule an application added, by its name and its id",
                )
            };
            let Some((name, id)) = holding::firewall_parts(location).filter(|(name, id)| {
                sane_name(name, true) && sane_name(id, true) && !name.trim_start().starts_with('@')
            }) else {
                return Err(not_one());
            };
            let rules = view.firewall_rules();
            if rules.unreadable > 0 && mode == Mode::Hold {
                return Err(unlisted("firewall rules"));
            }
            // Only the rule with this id goes; another application's rule
            // by the same name is not asked about and does not go.
            match rules.items.iter().find(|r| r.id.eq_ignore_ascii_case(id)) {
                Some(r) if r.shown_name() != name => Err(not_one()),
                Some(r) if runs_windows(&r.program, view) => Err(windows("firewall rule")),
                _ => Ok(()),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The Windows Installer

const INSTALLER_PRODUCTS: &str = r"Software\Classes\Installer\Products";
const USER_DATA: &str = r"Software\Microsoft\Windows\CurrentVersion\Installer\UserData";

fn packed_shape(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether an item in a manifest is one [`msi_items`] could have made: a
/// product's key by its packed code, or a component's key or its value by
/// a packed code. Those are put back without the guard's rules, which
/// protect everything under `Installer`, because they were taken by the
/// helper's own derivation and are exactly these shapes.
fn msi_registration(item: &HoldItem) -> bool {
    let Some((hive, parts, value)) = key_parts(&item.location) else {
        return false;
    };
    if hive != "HKLM" {
        return false;
    }
    let products: Vec<String> = INSTALLER_PRODUCTS
        .split('\\')
        .map(str::to_lowercase)
        .collect();
    let user_data: Vec<String> = USER_DATA.split('\\').map(str::to_lowercase).collect();
    let tail = |base: &[String]| parts.starts_with(base).then(|| &parts[base.len()..]);
    let key = item.kind == ItemKind::RegistryKey && value.is_none();
    if let Some([code]) = tail(&products) {
        return key && packed_shape(code);
    }
    match tail(&user_data) {
        Some([sid, which, code]) if !sid.is_empty() && which == "products" => {
            key && packed_shape(code)
        }
        Some([sid, which, component]) if !sid.is_empty() && which == "components" => {
            packed_shape(component)
                && match value {
                    None => key,
                    Some(v) => item.kind == ItemKind::RegistryValue && packed_shape(v),
                }
        }
        _ => false,
    }
}

/// What the Windows Installer still records for one product, as items for
/// the holding area: the product's key under `Installer\Products`, its key
/// under each `UserData\<sid>\Products`, and under each component key that
/// names it, the key when no other product's value remains in it and only
/// its value when one does. Nothing is chosen that is not named by this
/// product's packed code, or by a component key that holds its value.
///
/// A key that cannot be read refuses the whole thing: a registration half
/// cleared is what this exists to prevent.
pub fn msi_items(code: &str, view: &dyn SystemView) -> Result<Vec<HoldItem>, String> {
    let packed = guard::packed_product_code(code).ok_or_else(|| {
        format!(
            "{code} is not a Windows Installer product code, so its registration was left as it \
             was."
        )
    })?;
    let unreadable = |what: &str| {
        format!(
            "{what} could not be read, so the Windows Installer's record of {code} was left as \
             it was. Try again."
        )
    };
    let key = |path: String| HoldItem {
        kind: ItemKind::RegistryKey,
        location: format!(r"HKLM\{path}"),
    };
    let exists = |path: &str| match view.subkeys(Hive::Hklm, path) {
        Ok(_) => Ok(true),
        Err(ViewError::Missing(_)) => Ok(false),
        Err(ViewError::Unreadable(_)) => Err(unreadable(&format!(r"HKLM\{path}"))),
    };
    let mut items = Vec::new();
    let product = format!(r"{INSTALLER_PRODUCTS}\{packed}");
    if exists(&product)? {
        items.push(key(product));
    }
    let sids = match view.subkeys(Hive::Hklm, USER_DATA) {
        Ok(sids) => sids,
        Err(ViewError::Missing(_)) => Vec::new(),
        Err(ViewError::Unreadable(_)) => return Err(unreadable(&format!(r"HKLM\{USER_DATA}"))),
    };
    for sid in sids {
        let products = format!(r"{USER_DATA}\{sid}\Products\{packed}");
        if exists(&products)? {
            items.push(key(products));
        }
        let components = format!(r"{USER_DATA}\{sid}\Components");
        let ids = match view.subkeys(Hive::Hklm, &components) {
            Ok(ids) => ids,
            Err(ViewError::Missing(_)) => continue,
            Err(ViewError::Unreadable(_)) => {
                return Err(unreadable(&format!(r"HKLM\{components}")));
            }
        };
        for id in ids {
            let path = format!(r"{components}\{id}");
            let values = match view.values(Hive::Hklm, &path) {
                Ok(values) => values,
                Err(ViewError::Missing(_)) => continue,
                Err(ViewError::Unreadable(_)) => return Err(unreadable(&format!(r"HKLM\{path}"))),
            };
            let Some((ours, _)) = values.iter().find(|(n, _)| n.eq_ignore_ascii_case(&packed))
            else {
                continue;
            };
            let subkeys = match view.subkeys(Hive::Hklm, &path) {
                Ok(subkeys) => subkeys,
                Err(ViewError::Missing(_)) => continue,
                Err(ViewError::Unreadable(_)) => return Err(unreadable(&format!(r"HKLM\{path}"))),
            };
            let shared =
                !subkeys.is_empty() || values.iter().any(|(n, _)| !n.eq_ignore_ascii_case(&packed));
            if shared {
                items.push(HoldItem {
                    kind: ItemKind::RegistryValue,
                    location: format!(r"HKLM\{path} | {ours}"),
                });
            } else {
                items.push(key(path));
            }
        }
    }
    Ok(items)
}

// ---------------------------------------------------------------------------
// Running

/// Run one of Brokey's own steps on this machine. `Ok` carries a sentence
/// for a step that succeeded with something to say; `Err` the sentence for
/// one that failed or was refused.
pub fn run(step: &Step, ctx: &mut InternalContext) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        let roots = Roots::live();
        let mut m = holding::LiveMutate::new(roots.machine.clone());
        run_in(
            step,
            ctx,
            &roots,
            &super::view::LiveView::new(),
            &mut m,
            &Live,
            &LiveSession,
        )
    }
    #[cfg(not(windows))]
    {
        let _ = ctx;
        Err(format!(
            "{} is a step Brokey only takes on Windows so far.",
            step.command.program
        ))
    }
}

/// [`run`] with the roots, the view of the machine and what changes it
/// handed in, which is what lets it be tested in a temporary folder.
#[cfg_attr(not(windows), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_in(
    step: &Step,
    ctx: &mut InternalContext,
    roots: &Roots,
    view: &dyn SystemView,
    m: &mut dyn Mutate,
    machine: &dyn Machine,
    session: &dyn Session,
) -> Result<Option<String>, String> {
    let program = step.command.program.as_str();
    let root = step.needs_root;
    let dir = roots.of(root);
    // The machine's root, and `%ProgramData%\Brokey` above it, are made
    // Administrators-only, or refused when they cannot be, before anything
    // is read from them: a manifest read from a folder the user could
    // still write is a manifest the user wrote.
    if root && ROOT.contains(&program) {
        m.make_root(dir).map_err(|f| match f {
            Failure::Failed(s) | Failure::Partial(s) => s,
            _ => format!(
                "Brokey could not make the holding area at {}, so nothing was changed. Try \
                 again.",
                dir.display()
            ),
        })?;
    }
    check_with(step, view, roots, machine)?;
    let args = &step.command.args;
    match program {
        HOLD => {
            let payload = hold_payload(args)?;
            let report = hold_in_order(dir, &payload, view, m);
            outcome(report, payload.items.len(), "moved to the holding area")
        }
        RESTORE => {
            let report = holding::restore(dir, &args[0], m);
            outcome(report, 0, "put back")
        }
        EMPTY => {
            let id = (args[0] != "all").then_some(args[0].as_str());
            outcome(holding::empty(dir, id, None), 0, "emptied")
        }
        MSI_UNREGISTER => {
            let (code, id) = (&args[0], &args[1]);
            let items = msi_items(code, view)?;
            if items.is_empty() {
                return Ok(Some(format!(
                    "The Windows Installer had nothing left registered for {code}."
                )));
            }
            let name = format!("Windows Installer registration of {code}");
            let report = holding::hold(dir, id, &name, &items, m);
            outcome(report, items.len(), "moved to the holding area")
        }
        AWAIT_GONE => {
            let hints: Hints = serde_json::from_str(one_argument(program, args)?)
                .map_err(|_| unreadable_payload(program))?;
            await_gone(&hints, ctx, session)
        }
        SCAN => {
            let payload: ScanPayload = serde_json::from_str(one_argument(program, args)?)
                .map_err(|_| unreadable_payload(program))?;
            scan_step(payload, ctx, view, session)
        }
        SHELL_OPEN => shell_open_step(args, ctx, session),
        STEAM_UNINSTALL => {
            let id = one_argument(program, args)?;
            session.open_steam(&format!("steam://uninstall/{id}"))?;
            Ok(None)
        }
        // `check_with` refuses every other name before this.
        _ => Err(unreadable_payload(program)),
    }
}

// ---------------------------------------------------------------------------
// The steps that run as the user

/// What the session steps ask of the machine beyond the guard's view,
/// so that a test answers from a table and never waits two seconds, reads
/// the real registry or starts a program.
pub(crate) trait Session {
    /// Whether an uninstall key is still listed.
    fn listed(&self, hive: Hive, key: &str) -> bool;
    /// How long between two looks at it. [`AWAIT_POLL`] on a real machine.
    fn poll(&self) -> Duration;
    /// Every available source's installed list, and how many sources could
    /// not give theirs.
    fn installed(&self) -> (Vec<Package>, u32);
    /// What the closed list says of a step that would run as Administrator.
    fn closed_list(&self, step: &Step) -> Result<(), String>;
    /// Start the uninstaller the closed list refused as the user through
    /// the shell, and wait for it while `cancelled` says to; its exit code,
    /// or the sentence for why it did not start or why waiting stopped.
    fn shell_open(&self, refused: &Refused, cancelled: &dyn Fn() -> bool) -> Result<u32, String>;
    /// Hand a `steam://` address to Explorer, as the user, without waiting.
    fn open_steam(&self, url: &str) -> Result<(), String>;
}

/// This machine.
pub(crate) struct LiveSession;

impl Session for LiveSession {
    fn listed(&self, hive: Hive, key: &str) -> bool {
        #[cfg(windows)]
        {
            crate::sources::windows::arp::read()
                .iter()
                .any(|e| Hive::from(e.hive) == hive && e.key_name.eq_ignore_ascii_case(key))
        }
        #[cfg(not(windows))]
        {
            let _ = (hive, key);
            false
        }
    }

    fn poll(&self) -> Duration {
        AWAIT_POLL
    }

    fn installed(&self) -> (Vec<Package>, u32) {
        // Read now rather than taken from the window's store: the store was
        // read before the uninstaller ran and still lists the application,
        // whose every file would then be a claim that shields it.
        let store = crate::Store::detect();
        let mut packages = Vec::new();
        let mut failed = 0;
        for source in store.sources.iter().filter(|s| s.status().available) {
            match source.installed() {
                Ok(found) => packages.extend(found),
                Err(_) => failed += 1,
            }
        }
        (packages, failed)
    }

    fn closed_list(&self, step: &Step) -> Result<(), String> {
        #[cfg(windows)]
        {
            crate::transaction::allow::check_step(
                step,
                &crate::transaction::allow::Allowed::with_registered_removals(),
            )
        }
        #[cfg(not(windows))]
        {
            let _ = step;
            Err("The closed list is only asked on Windows.".to_string())
        }
    }

    fn shell_open(&self, refused: &Refused, cancelled: &dyn Fn() -> bool) -> Result<u32, String> {
        #[cfg(windows)]
        {
            crate::transaction::elevate::shell_open(refused, cancelled)
        }
        #[cfg(not(windows))]
        {
            let _ = cancelled;
            Err(format!(
                "{} can only be started this way on Windows.",
                refused.program().display()
            ))
        }
    }

    fn open_steam(&self, url: &str) -> Result<(), String> {
        // Explorer hands the address to Steam, as the user, and returns;
        // its exit code says nothing either way, so it is not read. A
        // thread reaps it so it leaves no zombie handle behind.
        let program = crate::launch::explorer_program();
        let mut child = std::process::Command::new(&program)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| {
                format!("Brokey could not hand the game to Steam ({e}). Remove it from Steam.")
            })?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

fn unreadable_payload(program: &str) -> String {
    format!(
        "Brokey could not read what the step {program} carries, so it was not taken. Start the \
         operation again from Brokey."
    )
}

/// Wait until the application's uninstall key is gone. The uninstaller's
/// process ending says nothing: an NSIS uninstaller copies itself to Temp
/// and exits at once, and Steam returns before its dialog is answered. So
/// the key is the signal. While it is there the page shows
/// [`WAITING_FOR_UNINSTALLER`] with **It has finished** (the `finished`
/// flag) and **Stop waiting** (cancel). "It has finished" with the key
/// still there is [`not_removed`], and the plan ends without a scan.
fn await_gone(
    hints: &Hints,
    ctx: &mut InternalContext,
    session: &dyn Session,
) -> Result<Option<String>, String> {
    // A package manager's own removal has no uninstall key to watch: its
    // command ends when the package is gone.
    let Some((hive, key)) = &hints.uninstall_key else {
        return Ok(None);
    };
    if !session.listed(*hive, key) {
        return Ok(None);
    }
    // Only an answer given while this step waits counts.
    ctx.finished.store(false, Ordering::SeqCst);
    (ctx.sink)(Event::Waiting {
        plan: ctx.plan.to_string(),
        step: ctx.step,
        message: WAITING_FOR_UNINSTALLER.to_string(),
    });
    let poll = session.poll();
    let slice = poll.min(Duration::from_millis(100));
    let mut looked = Instant::now();
    loop {
        std::thread::sleep(slice);
        if (ctx.cancelled)() {
            return Err(crate::transaction::runner::CANCELLED.to_string());
        }
        if ctx.finished.swap(false, Ordering::SeqCst) {
            return if session.listed(*hive, key) {
                Err(if is_steam_app_key(key) {
                    steam_not_removed(&hints.name)
                } else {
                    not_removed(&hints.name)
                })
            } else {
                Ok(None)
            };
        }
        if looked.elapsed() >= poll {
            looked = Instant::now();
            if !session.listed(*hive, key) {
                return Ok(None);
            }
        }
    }
}

/// A scan id no other scan this run has: the time and a counter.
fn new_scan_id() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("scan-{millis}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Look for what was left behind, guard out what other applications use,
/// keep the list for the clear that follows, and tell the page.
fn scan_step(
    payload: ScanPayload,
    ctx: &mut InternalContext,
    view: &dyn SystemView,
    session: &dyn Session,
) -> Result<Option<String>, String> {
    let ScanPayload { hints, force } = payload;
    let scanned = scan::scan_with(&hints, view, force);
    let (installed, failed) = session.installed();
    let claims = guard::claims(view, &installed, force.then_some(&hints));
    let id = new_scan_id();
    let mut findings = guard::findings(id.clone(), &hints, scanned, &claims, view);
    // A source whose installed list could not be read claims nothing, so
    // the list says it could not look rather than pretend it did.
    findings.unreadable += failed;
    let count = findings.items.len();
    ctx.scans.lock().unwrap_or_else(|p| p.into_inner()).insert(
        id.clone(),
        ScanRecord {
            hints,
            findings,
            forced: force,
        },
    );
    (ctx.sink)(Event::Leftovers {
        plan: ctx.plan.to_string(),
        scan: id,
        count,
    });
    Ok((count == 0).then(|| "Nothing was left behind.".to_string()))
}

/// Start an uninstaller that elevates itself, as the user, through the
/// shell's `open` verb, so that Windows' own prompt names it. Taken only
/// for what the closed list still refuses as a program this account can
/// replace and which the registry records as an Add/Remove removal; for
/// anything else Brokey starts nothing.
fn shell_open_step(
    args: &[String],
    ctx: &mut InternalContext,
    session: &dyn Session,
) -> Result<Option<String>, String> {
    let Some((program, rest)) = args.split_first() else {
        return Err(unreadable_payload(SHELL_OPEN));
    };
    let as_removal = Step {
        source: SourceKind::Arp,
        title: String::new(),
        command: Command {
            program: program.clone(),
            args: rest.to_vec(),
            env: Vec::new(),
            cwd: None,
        },
        needs_root: true,
        weight: 0,
    };
    let Some(refused) = Refused::by(program, rest, session.closed_list(&as_removal)) else {
        return Err(format!(
            "Brokey did not start {program}, because it is not an uninstaller Windows has to \
             ask about. Remove the application again from Brokey."
        ));
    };
    match session.shell_open(&refused, ctx.cancelled)? {
        0 => Ok(None),
        // The key, not the code, says whether it worked: the next step
        // looks at it.
        code => Ok(Some(format!(
            "The uninstaller ended with code {code}. Brokey checks whether it removed the \
             application."
        ))),
    }
}

/// A HOLD in two passes, as [`in_order`] orders it: everything but the
/// extension keys first, then each extension key, unless it names a ProgID
/// whose key was in the set and could not be moved. That one stays, said so,
/// since the guard let it go only because its ProgID was going too.
fn hold_in_order(
    dir: &Path,
    payload: &HoldPayload,
    view: &dyn SystemView,
    m: &mut dyn Mutate,
) -> HoldReport {
    let mut rest = in_order(&payload.items);
    let first_extension = rest
        .iter()
        .position(|i| is_extension(&i.location))
        .unwrap_or(rest.len());
    let extensions = rest.split_off(first_extension);
    let mut report = holding::hold(dir, &payload.holding, &payload.name, &rest, m);
    // The ProgID keys that stayed, by their name in lower case.
    let stuck: Vec<(String, String)> = report
        .failed
        .iter()
        .filter_map(|(location, _)| progid_of(location).map(|p| (p, location.clone())))
        .collect();
    let mut go = Vec::new();
    for item in extensions {
        let named = progids_named(&item.location, view);
        // A key that could not be read might name any of them.
        let blocked = named.iter().find_map(|n| match n {
            None => stuck.first(),
            Some(n) => stuck.iter().find(|(p, _)| p == n),
        });
        match blocked {
            Some((_, progid)) => report.failed.push((
                item.location.clone(),
                format!(
                    "{} was left where it is, because {progid}, which it names, could not be \
                     moved. Clear it again once {progid} has gone.",
                    item.location
                ),
            )),
            None => go.push(item),
        }
    }
    if !go.is_empty() {
        let more = holding::hold(dir, &payload.holding, &payload.name, &go, m);
        report.held += more.held;
        report.failed.extend(more.failed);
    }
    report
}

/// The ProgID a key under `Software\Classes` is, in lower case, when it is
/// one: a key directly under `Classes` that is not an extension.
fn progid_of(location: &str) -> Option<String> {
    let (_, mut parts, value) = key_parts(location)?;
    if parts.len() >= 2 && parts[1] == "wow6432node" {
        parts.remove(1);
    }
    match parts.as_slice() {
        [software, classes, progid]
            if software == "software"
                && classes == "classes"
                && value.is_none()
                && !progid.starts_with('.') =>
        {
            Some(progid.clone())
        }
        _ => None,
    }
}

/// The ProgIDs an extension item names, in lower case: one
/// `OpenWithProgids` value names its own; the extension's key names its
/// default and each of its `OpenWithProgids`. `None` in the list stands for
/// a key that could not be read, which might name anything.
fn progids_named(location: &str, view: &dyn SystemView) -> Vec<Option<String>> {
    let Some((hive, mut parts, value)) = key_parts(location) else {
        return vec![None];
    };
    if let Some(name) = value
        && parts.last().is_some_and(|p| p == "openwithprogids")
    {
        return vec![Some(name.trim().to_lowercase())];
    }
    let wow = parts.len() >= 2 && parts[1] == "wow6432node";
    if wow {
        parts.remove(1);
    }
    let hive = match hive.as_str() {
        "HKLM" if wow => Hive::Hklm32,
        "HKLM" => Hive::Hklm,
        "HKLM32" => Hive::Hklm32,
        _ => Hive::Hkcu,
    };
    let Some(extension) = parts.get(2) else {
        return vec![None];
    };
    let key = format!(r"Software\Classes\{extension}");
    let mut named = Vec::new();
    match view.values(hive, &key) {
        Ok(values) => named.extend(
            values
                .into_iter()
                .filter(|(n, _)| n.is_empty())
                .map(|(_, d)| Some(d.trim().to_lowercase())),
        ),
        Err(ViewError::Missing(_)) => {}
        Err(ViewError::Unreadable(_)) => named.push(None),
    }
    match view.values(hive, &format!(r"{key}\OpenWithProgids")) {
        Ok(values) => named.extend(
            values
                .into_iter()
                .map(|(n, _)| Some(n.trim().to_lowercase())),
        ),
        Err(ViewError::Missing(_)) => {}
        Err(ViewError::Unreadable(_)) => named.push(None),
    }
    named
}

/// A report as a step's result: nothing to say when everything was done;
/// otherwise how much was, and each thing that was not with its sentence.
fn outcome(report: HoldReport, total: usize, done: &str) -> Result<Option<String>, String> {
    if report.failed.is_empty() {
        return Ok(None);
    }
    let count = |n: usize| if n == 1 { "item was" } else { "items were" };
    let mut text = if total > 0 {
        format!("{} of {total} {} {done}.", report.held, count(total))
    } else {
        format!("{} {} {done}.", report.held, count(report.held))
    };
    if done == "moved to the holding area" && report.held > 0 {
        text.push_str(" What was moved can be restored from Settings.");
    }
    for (_, sentence) in &report.failed {
        text.push(' ');
        text.push_str(sentence);
    }
    Err(text)
}

#[cfg(test)]
mod tests;
