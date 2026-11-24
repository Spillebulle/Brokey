//! Brokey's own steps: the ones a plan carries out itself rather than by
//! starting a program. A step whose program starts with `brokey:` is one of
//! these, and neither the runner nor the helper ever spawns it.
//!
//! Four may run as Administrator, and they are the cleanup's entries on the
//! closed list: [`HOLD`] (move these items to the holding area), [`RESTORE`]
//! (put a holding entry back), [`EMPTY`] (delete one for good, or all) and
//! [`MSI_UNREGISTER`] (clear what the Windows Installer still records for a
//! product that was removed by force). The other three, [`AWAIT_GONE`],
//! [`SCAN`] and [`SHELL_OPEN`], only ever run as the user and are refused in
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
use crate::model::{Event, Findings, ItemKind, Step};
use crate::sources::windows::shortcut;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

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
/// `args[0]` is `{ hints, force }` as JSON. As the user only.
pub const SCAN: &str = "brokey:scan";
/// `args` are the program and its arguments. As the user only.
pub const SHELL_OPEN: &str = "brokey:shell-open";

/// The steps that may run as Administrator.
const ROOT: [&str; 4] = [HOLD, RESTORE, EMPTY, MSI_UNREGISTER];
/// The steps that never may.
const SESSION_ONLY: [&str; 3] = [AWAIT_GONE, SCAN, SHELL_OPEN];

/// What [`HOLD`] carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldPayload {
    pub holding: String,
    /// The application's name, as the Settings page shows the entry.
    pub name: String,
    pub items: Vec<HoldItem>,
}

/// Every scan this session has made, by scan id, with the hints it searched
/// by. [`SCAN`] fills it; clearing reads it, so the page never sends a path.
pub type Scans = Arc<Mutex<HashMap<String, (Hints, Findings)>>>;

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

/// Whether one of Brokey's own steps may run, and why not when it may not.
/// `view` is the machine as the guard reads it; `roots` is where a restore's
/// manifest is read from.
pub fn check(step: &Step, view: &dyn SystemView, roots: &Roots) -> Result<(), String> {
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
        return Ok(());
    }
    let args = &step.command.args;
    match program {
        HOLD => {
            let payload = hold_payload(args)?;
            check_hold(&payload, root, view)
        }
        RESTORE => {
            let id = one_argument(program, args)?;
            check_id(id)?;
            check_restore(roots.of(root), id, root, view)
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

fn check_hold(payload: &HoldPayload, root: bool, view: &dyn SystemView) -> Result<(), String> {
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
        check_item(item, root, Mode::Hold, view, &set)?;
    }
    Ok(())
}

/// The restore's own check: every destination in the manifest, by the same
/// rules as a clear. That the manifest lives in an Administrators-only
/// folder is a reason to trust it more, not a reason to stop checking.
fn check_restore(dir: &Path, id: &str, root: bool, view: &dyn SystemView) -> Result<(), String> {
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
        check_item(item, root, Mode::Restore, view, &set)?;
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
    view: &dyn SystemView,
    set: &[String],
) -> Result<(), String> {
    let location = item.location.as_str();
    match item.kind {
        ItemKind::File | ItemKind::Folder | ItemKind::Shortcut => {
            check_path(location, mode, view, set)
        }
        ItemKind::RegistryKey | ItemKind::RegistryValue => {
            check_key(item, root, mode)?;
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
    if under(Known::ProgramData, "microsoft")
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

fn check_path(
    location: &str,
    mode: Mode,
    view: &dyn SystemView,
    set: &[String],
) -> Result<(), String> {
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
    no_link_on_the_way(location, drive, &names, mode)?;
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

/// What opening one part of a path without following it found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
enum Part {
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
) -> Result<(), String> {
    let mut path = format!(r"\\?\{drive}:");
    for name in names {
        path.push('\\');
        path.push_str(name);
        let shown = path.trim_start_matches(r"\\?\");
        match open_part(&path) {
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

/// `IsReparseTagNameSurrogate`: the bit Windows sets on every tag whose
/// reparse point names another place (junctions, symbolic links, mount
/// points). Cloud files and App Execution Aliases do not set it.
#[cfg(windows)]
fn is_name_surrogate(tag: u32) -> bool {
    tag & 0x2000_0000 != 0
}

// ---------------------------------------------------------------------------
// The registry

/// Keys Windows keeps under `Software` that the guard has no reason to name,
/// since no scan offers them, as lower-case names under `Software`. A key is
/// refused when it is one of these or lies inside one; `Clients`,
/// `RegisteredApplications` and `ODBC` only down to the list itself, since
/// what an application put in them is that application's.
const CLASSES_WINDOWS_KEEPS: [&str; 8] = [
    "installer",
    "local settings",
    "protocols",
    "mime",
    "packagedcom",
    "extensions",
    "activatableclasses",
    "wow6432node",
];

fn key_windows_keeps(parts: &[String]) -> bool {
    let mut parts: Vec<&str> = parts.iter().map(String::as_str).collect();
    if parts.len() >= 2 && parts[1] == "wow6432node" {
        parts.remove(1);
    }
    match parts.as_slice() {
        ["software", "classes", first, ..] => CLASSES_WINDOWS_KEEPS.contains(first),
        ["software", "clients" | "odbc", rest @ ..] => rest.len() <= 1,
        ["software", "registeredapplications"] => true,
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

fn check_key(item: &HoldItem, root: bool, mode: Mode) -> Result<(), String> {
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
    if key_windows_keeps(&parts) {
        return Err(refuse(location, mode, "it is a place Brokey never clears"));
    }
    Ok(())
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
            let Some(name) = location
                .strip_prefix("firewall:")
                .filter(|n| sane_name(n, true) && !n.trim_start().starts_with('@'))
            else {
                return Err(refuse(
                    location,
                    mode,
                    "it is not the name of a firewall rule an application added",
                ));
            };
            let rules = view.firewall_rules();
            if rules.unreadable > 0 && mode == Mode::Hold {
                return Err(unlisted("firewall rules"));
            }
            // Every rule with the name goes, so every one is asked.
            if rules
                .items
                .iter()
                .filter(|r| r.name == name)
                .any(|r| runs_windows(&r.program, view))
            {
                return Err(windows("firewall rule"));
            }
            Ok(())
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
        let mut m = holding::LiveMutate {
            machine_root: roots.machine.clone(),
            force_copy: false,
        };
        run_in(step, ctx, &roots, &super::view::LiveView::new(), &mut m)
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
pub(crate) fn run_in(
    step: &Step,
    // What the session-only steps will reach for; the four that run here
    // need nothing from it.
    _ctx: &mut InternalContext,
    roots: &Roots,
    view: &dyn SystemView,
    m: &mut dyn Mutate,
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
    check(step, view, roots)?;
    let args = &step.command.args;
    match program {
        HOLD => {
            let payload = hold_payload(args)?;
            let items = in_order(&payload.items);
            let report = holding::hold(dir, &payload.holding, &payload.name, &items, m);
            outcome(report, items.len(), "moved to the holding area")
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
        _ => Err(not_yet(program)),
    }
}

/// Until the removal flow can wait and scan, these say so rather than
/// pretend.
fn not_yet(program: &str) -> String {
    format!(
        "This version of Brokey cannot take the step {program} yet. Update Brokey and try again."
    )
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
