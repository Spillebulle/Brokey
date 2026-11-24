//! The holding area: where a cleared leftover goes instead of being deleted,
//! so that it can be put back.
//!
//! Each cleanup is one entry, a folder named by its id under one of two
//! roots: the user's (`%LOCALAPPDATA%\Brokey\Holding`) and the machine's
//! (`%ProgramData%\Brokey\Holding`, Administrators only). In the entry is a
//! `manifest.json` recording every item's original location and an `items`
//! folder with what was moved and what was exported: files and folders
//! themselves, registry keys and values, services' keys, task XML and
//! firewall rules as files that [`restore`] reads back.
//!
//! The manifest is written before each item moves and again after it, so a
//! failure half-way, a locked file or a crash, leaves every item that did
//! move recorded and restorable. Everything that changes the machine goes
//! through [`Mutate`], so all of this is tested against a temporary folder
//! and an in-memory registry; the live backend is [`windows::LiveMutate`].

use super::view::Hive;
use crate::model::{HoldingEntry, ItemKind};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(windows)]
pub mod bound;
#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
pub use windows::LiveMutate;

/// Entries older than this are emptied automatically.
pub const AUTO_EMPTY_DAYS: u32 = 30;

const MANIFEST: &str = "manifest.json";
const ITEMS: &str = "items";
const DAY: i64 = 86_400;

pub const REG_NONE: u32 = 0;
pub const REG_SZ: u32 = 1;
pub const REG_EXPAND_SZ: u32 = 2;
pub const REG_BINARY: u32 = 3;
pub const REG_DWORD: u32 = 4;
pub const REG_MULTI_SZ: u32 = 7;
pub const REG_QWORD: u32 = 11;

/// One thing to move into the holding area, in the location format the scan
/// and the guard use: a path; `HKLM\…`, `HKLM32\…` or `HKCU\…`;
/// `<key> | <value name>`; `service:<name>`; `task:<path>`;
/// `firewall:<rule name> | <rule id>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldItem {
    pub kind: ItemKind,
    pub location: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeldState {
    /// Recorded before it moves. Still this after a crash part-way.
    Moving,
    /// In the holding area.
    Held,
    /// Inside a folder or key held earlier in the same entry, and so moved
    /// with it and put back with it, never on its own.
    Covered,
    /// Moved in part: some of it is still where it was and the holding area
    /// has a whole copy. Restore puts back what is missing beside what is
    /// there, and an entry holding one is never emptied automatically.
    Partial,
}

/// An item in an entry: where it was, where it went, and how far it got.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    #[serde(flatten)]
    pub item: HoldItem,
    /// The moved file or folder, or the exported file, relative to the
    /// entry's folder. Empty for a covered item.
    pub stored: String,
    pub state: HeldState,
    /// The location of the held item this one is inside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covered_by: Option<String>,
    /// Bytes in the holding area.
    #[serde(default)]
    pub size: u64,
}

/// `manifest.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    /// The application's name, as the Settings page shows it.
    pub name: String,
    /// RFC 3339, UTC.
    pub created: String,
    pub items: Vec<Held>,
    /// The number the next stored item is given, so that a name is never
    /// reused within an entry.
    #[serde(default)]
    pub next: u32,
}

/// What a hold, a restore or an empty did: how many things it handled and,
/// for each one it could not, the location and a sentence saying why.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldReport {
    pub held: usize,
    pub failed: Vec<(String, String)>,
}

/// Why a [`Mutate`] operation did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The place it would go is taken.
    Occupied,
    /// It is not there.
    Missing,
    /// Anything else, as a sentence that names what went wrong and what to do.
    Failed(String),
    /// It was moved in part and could not be put back together where it
    /// was: some of it is still there and a whole copy is at the
    /// destination. The sentence says so.
    Partial(String),
}

pub type Done<T> = Result<T, Failure>;

/// One registry value, exactly: its type number and its bytes, whatever the
/// type, so that every kind round-trips unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegValue {
    pub ty: u32,
    #[serde(with = "hex_bytes")]
    pub data: Vec<u8>,
}

/// A registry key and everything under it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyNode {
    pub values: Vec<(String, RegValue)>,
    pub subkeys: Vec<(String, KeyNode)>,
}

/// Everything that changes the machine.
///
/// Each restore takes `merge`. Without it, it refuses with
/// [`Failure::Occupied`] when anything is already where it would go: the
/// item was wholly removed, so what is there now is new. With it, it puts
/// back only what is missing and never replaces what is there: for an item
/// that was removed in part, or that a crash interrupted.
pub trait Mutate {
    /// Make a holding root if it is not there. The machine's is made so that
    /// only Administrators and SYSTEM can change it.
    fn make_root(&mut self, root: &Path) -> Done<()>;
    /// Refuse an entry folder under `root` that something other than
    /// Brokey could have written: one in the machine's root not owned by
    /// Administrators or SYSTEM.
    fn trust_entry(&mut self, root: &Path, dir: &Path) -> Done<()>;
    /// Move a file or a folder to `to`, which must not exist; its parent is
    /// made. See [`move_file_or_folder`].
    fn move_path(&mut self, from: &Path, to: &Path) -> Done<()>;
    /// Put back from `from` what is missing at `to`, never replacing what is
    /// there. See [`merge_file_or_folder`].
    fn merge_path(&mut self, from: &Path, to: &Path) -> Done<()>;
    fn export_key(&mut self, hive: Hive, path: &str) -> Done<KeyNode>;
    fn delete_key(&mut self, hive: Hive, path: &str) -> Done<()>;
    fn restore_key(&mut self, hive: Hive, path: &str, key: &KeyNode, merge: bool) -> Done<()>;
    fn export_value(&mut self, hive: Hive, key: &str, name: &str) -> Done<RegValue>;
    fn delete_value(&mut self, hive: Hive, key: &str, name: &str) -> Done<()>;
    fn restore_value(
        &mut self,
        hive: Hive,
        key: &str,
        name: &str,
        value: &RegValue,
        merge: bool,
    ) -> Done<()>;
    /// The service's key under `HKLM\SYSTEM\CurrentControlSet\Services`.
    fn export_service(&mut self, name: &str) -> Done<KeyNode>;
    /// Stop it and delete it.
    fn delete_service(&mut self, name: &str) -> Done<()>;
    fn restore_service(&mut self, name: &str, key: &KeyNode, merge: bool) -> Done<()>;
    /// The task's XML definition.
    fn export_task(&mut self, path: &str) -> Done<String>;
    /// Stop it and delete it.
    fn delete_task(&mut self, path: &str) -> Done<()>;
    fn restore_task(&mut self, path: &str, xml: &str, merge: bool) -> Done<()>;
    /// The stored rule with this id, as its id and its rule text: one rule,
    /// kept as a list so that what is exported is what is put back.
    fn export_firewall_rule(&mut self, id: &str) -> Done<Vec<(String, String)>>;
    /// Delete exactly the rules `export_firewall_rule` returned.
    fn delete_firewall_rule(&mut self, id: &str, rules: &[(String, String)]) -> Done<()>;
    fn restore_firewall_rule(
        &mut self,
        id: &str,
        rules: &[(String, String)],
        merge: bool,
    ) -> Done<()>;
}

/// A holding id: what the helper accepts, and so all this makes or opens.
pub fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The user's holding root, `%LOCALAPPDATA%\Brokey\Holding`.
pub fn user_root() -> PathBuf {
    #[cfg(windows)]
    let base = windows::local_app_data();
    #[cfg(not(windows))]
    let base = directories::BaseDirs::new().map(|d| d.data_local_dir().to_path_buf());
    base.unwrap_or_else(std::env::temp_dir)
        .join("Brokey")
        .join("Holding")
}

/// The machine's holding root, `%ProgramData%\Brokey\Holding`. Taken from
/// the known folder, not the environment, which the user could point at a
/// folder of their own.
pub fn machine_root() -> PathBuf {
    #[cfg(windows)]
    let base = windows::program_data().unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    #[cfg(not(windows))]
    let base = PathBuf::from("/var/lib");
    base.join("Brokey").join("Holding")
}

// ---------------------------------------------------------------------------
// Locations

/// A location taken apart into what it names.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Place {
    Path(PathBuf),
    Key(Hive, String),
    Value(Hive, String, String),
    Service(String),
    Task(String),
    /// By its id, never by its name.
    Firewall(String),
}

pub(crate) fn parse_key(location: &str) -> Option<(Hive, String)> {
    let (hive, rest) = location.trim().split_once('\\')?;
    let hive = match hive.to_ascii_uppercase().as_str() {
        "HKLM" => Hive::Hklm,
        "HKLM32" => Hive::Hklm32,
        "HKCU" => Hive::Hkcu,
        _ => return None,
    };
    let rest = rest.trim_matches('\\');
    (!rest.is_empty()).then(|| (hive, rest.to_string()))
}

impl Place {
    fn parse(item: &HoldItem) -> Option<Place> {
        let loc = item.location.as_str();
        let named = |prefix: &str| {
            loc.strip_prefix(prefix)
                .filter(|n| !n.trim().is_empty())
                .map(str::to_string)
        };
        match item.kind {
            ItemKind::File | ItemKind::Folder | ItemKind::Shortcut => {
                let path = PathBuf::from(loc);
                path.is_absolute().then_some(Place::Path(path))
            }
            ItemKind::RegistryKey => parse_key(loc).map(|(h, p)| Place::Key(h, p)),
            ItemKind::RegistryValue => {
                let (key, name) = loc.split_once(" | ")?;
                parse_key(key).map(|(h, p)| Place::Value(h, p, name.to_string()))
            }
            ItemKind::Service => named("service:").map(Place::Service),
            ItemKind::Task => named("task:").map(Place::Task),
            ItemKind::FirewallRule => {
                firewall_parts(loc).map(|(_, id)| Place::Firewall(id.to_string()))
            }
        }
    }

    /// What the stored item is called inside `items`.
    fn stored_name(&self, n: u32) -> String {
        let ext = match self {
            Place::Path(_) => "",
            Place::Task(_) => ".xml",
            _ => ".json",
        };
        format!("{ITEMS}/{n}{ext}")
    }

    /// The registry key this names, as one comparable string, with the
    /// 32-bit view however it was spelt.
    fn key_text(hive: Hive, path: &str) -> String {
        let mut lower = path.to_lowercase().replace('/', "\\");
        let mut hive = hive;
        if hive == Hive::Hklm
            && let Some(rest) = lower.strip_prefix("software\\wow6432node")
            && (rest.is_empty() || rest.starts_with('\\'))
        {
            lower = format!("software{rest}");
            hive = Hive::Hklm32;
        }
        format!("{hive:?}\\{lower}")
    }

    /// `self` is held, and `other` lies strictly inside it.
    fn covers(&self, other: &Place) -> bool {
        match (self, other) {
            (Place::Path(dir), Place::Path(p)) => {
                inside(&path_text(p), &path_text(dir)) && path_text(p) != path_text(dir)
            }
            (Place::Key(h, k), Place::Key(h2, k2)) => {
                let (dir, key) = (Place::key_text(*h, k), Place::key_text(*h2, k2));
                inside(&key, &dir) && key != dir
            }
            (Place::Key(h, k), Place::Value(h2, k2, _)) => {
                inside(&Place::key_text(*h2, k2), &Place::key_text(*h, k))
            }
            _ => false,
        }
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// `path` is `dir` or lies inside it.
fn inside(path: &str, dir: &str) -> bool {
    path == dir
        || (path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'\\')
}

/// A firewall rule's location: `firewall:<name> | <id>`. The name is what
/// the page shows; the id, the value Windows stores the rule under, is what
/// is held, since another application's rule can have the same name.
pub fn firewall_location(name: &str, id: &str) -> String {
    format!("firewall:{name} | {id}")
}

/// The name and the id in a firewall rule's location. The id is what follows
/// the last ` | `, so a name may contain one and an id may not (the scan
/// never offers a rule whose id does).
pub fn firewall_parts(location: &str) -> Option<(&str, &str)> {
    let (name, id) = location.strip_prefix("firewall:")?.rsplit_once(" | ")?;
    (!name.trim().is_empty() && !id.trim().is_empty()).then_some((name, id))
}

// ---------------------------------------------------------------------------
// Sentences

/// The sentence for a file or folder that could not be moved.
pub fn io_sentence(location: &str, e: &io::Error) -> String {
    if let Some(HardLink(file)) = e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<HardLink>())
    {
        return format!(
            "{location} was left where it is, because {file} also exists under another name \
             elsewhere on the drive, and Brokey never moves a file like that. Delete it yourself \
             if you no longer want it."
        );
    }
    // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION.
    if matches!(e.raw_os_error(), Some(32 | 33)) && cfg!(windows) {
        return in_use(location);
    }
    if e.kind() == io::ErrorKind::PermissionDenied {
        return format!(
            "{location} could not be moved because it is in use or Brokey may not change it, \
             so it was left where it is. Close any program using it and try again."
        );
    }
    format!("{location} could not be moved ({e}), so it was left where it is. Try again.")
}

/// A file with a second name elsewhere (a hard link), which is never copied
/// as Administrator: the error a move carries for it, so that its sentence
/// is its own and does not ask the user to try again.
#[derive(Debug)]
pub struct HardLink(pub String);

impl std::fmt::Display for HardLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} also exists under another name elsewhere on the drive (a hard link)",
            self.0
        )
    }
}

impl std::error::Error for HardLink {}

/// An account Windows can start a service as without a password.
pub fn needs_no_password(account: Option<&str>) -> bool {
    let Some(account) = account else {
        return true;
    };
    let lower = account.trim().to_lowercase();
    lower.is_empty()
        || lower == "localsystem"
        || lower.starts_with(r"nt authority\")
        || lower.starts_with(r"nt service\")
}

/// The account a service's key says it runs as (`ObjectName`).
fn service_account(key: &KeyNode) -> Option<String> {
    let value = key
        .values
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("ObjectName"))
        .map(|(_, v)| v)
        .filter(|v| matches!(v.ty, REG_SZ | REG_EXPAND_SZ))?;
    let units: Vec<u16> = value
        .data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .take_while(|&u| u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units)).filter(|a| !a.trim().is_empty())
}

/// Whether a task's definition says it runs whether the user is signed in
/// or not with a stored password, which registering it again would need.
pub fn task_needs_password(xml: &str) -> bool {
    let Some(start) = xml.find("<LogonType>") else {
        return false;
    };
    let rest = &xml[start + "<LogonType>".len()..];
    let logon = rest[..rest.find("</LogonType>").unwrap_or(rest.len())].trim();
    matches!(logon, "Password" | "InteractiveTokenOrPassword")
}

/// Why a service could not be put back: the end of a sentence.
pub fn password_reason_for_service(account: &str) -> String {
    format!("it runs as {account}, and Windows would need that account's password to put it back")
}

/// Why a scheduled task could not be put back: the end of a sentence.
pub fn password_reason_for_task() -> String {
    "it runs whether you are signed in or not, and Windows would need its account's password \
     to put it back"
        .to_string()
}

/// The sentence for an item left where it is because putting it back
/// would need a password.
pub fn left_for_password(location: &str, why: &str) -> String {
    format!("{location} was left where it is, because {why}.")
}

/// The sentence for a file another program has open.
pub fn in_use(location: &str) -> String {
    format!(
        "{location} is in use by another program, so it was left where it is. \
         Close the program and clear it again."
    )
}

fn sentence_for(location: &str, f: Failure) -> String {
    match f {
        Failure::Missing => {
            format!("{location} is no longer there, so there was nothing to move.")
        }
        Failure::Occupied => format!(
            "The holding area already has something in the place kept for {location}, so it \
             was left where it is. Empty the holding area and try again."
        ),
        Failure::Failed(s) | Failure::Partial(s) => s,
    }
}

// ---------------------------------------------------------------------------
// Files and folders

/// Move a file or folder to `to`, which must not exist; its parent is made.
///
/// On one volume this is one rename, which moves a junction or symbolic
/// link inside a folder as the link it is. Across volumes, or when
/// `force_copy` is set, it is a copy and then a removal, and the folder is
/// looked through first: a link or a cloud placeholder anywhere in it refuses
/// the whole item, since copying would follow the one and download the other.
/// A failed copy is removed again and a failed removal is undone from the
/// copy, so the item is either wholly moved or left where it was; when even
/// that undoing fails, the answer is [`Failure::Partial`] and the copy is
/// kept whole.
pub fn move_file_or_folder(from: &Path, to: &Path, force_copy: bool) -> Done<()> {
    move_with(from, to, force_copy, &mut Plain)
}

/// How a move or a merge reaches the disk. [`Plain`] is `std::fs`; the live
/// backend's [`bound::Bound`] binds every step to the place that was
/// checked, so that nothing swapped in after the check is gone through.
pub trait Disk {
    /// Make the folder something is about to go into, and those above it.
    fn make_parent(&mut self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)
    }
    /// Rename `from` to `to`, never replacing what is there.
    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        rename_no_replace(from, to)
    }
    /// A folder a copy is about to copy from: hold it as it is until
    /// [`Disk::release`].
    fn keep_source(&mut self, dir: &Path) -> io::Result<()> {
        let _ = dir;
        Ok(())
    }
    /// Make one folder that is not there yet.
    fn make_dir(&mut self, dir: &Path) -> io::Result<()> {
        fs::create_dir(dir)
    }
    /// Copy one file to a name that is not there yet.
    fn copy_file(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        fs::copy(from, to).map(|_| ())
    }
    /// A folder that is there already is about to be written into.
    fn enter(&mut self, dir: &Path) -> io::Result<()> {
        let _ = dir;
        Ok(())
    }
    /// Let go of what a copy holds, before anything is removed.
    fn release(&mut self) {}
    /// Remove one file, or one folder that must be empty, that a copy has
    /// copied. A folder with something in it fails as
    /// [`io::ErrorKind::DirectoryNotEmpty`].
    fn remove(&mut self, path: &Path, is_dir: bool) -> io::Result<()> {
        if is_dir {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        }
    }
}

/// `std::fs`, as it is.
pub struct Plain;

impl Disk for Plain {}

/// [`move_file_or_folder`] through `disk`.
pub fn move_with(from: &Path, to: &Path, force_copy: bool, disk: &mut dyn Disk) -> Done<()> {
    let shown = from.display().to_string();
    match fs::symlink_metadata(from) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Failure::Missing),
        Err(e) => return Err(Failure::Failed(io_sentence(&shown, &e))),
        Ok(_) => {}
    }
    if fs::symlink_metadata(to).is_ok() {
        return Err(Failure::Occupied);
    }
    if let Some(parent) = to.parent() {
        disk.make_parent(parent).map_err(|e| {
            Failure::Failed(format!(
                "Brokey could not make {} ({e}), so {shown} was left where it is. Try again.",
                parent.display()
            ))
        })?;
    }
    if !force_copy {
        match disk.rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(Failure::Occupied),
            Err(e) if crosses_volumes(&e) => {}
            Err(e) => return Err(Failure::Failed(io_sentence(&shown, &e))),
        }
    }
    copy_then_remove(from, to, disk)
}

fn crosses_volumes(e: &io::Error) -> bool {
    // ERROR_NOT_SAME_DEVICE on Windows, EXDEV elsewhere.
    if cfg!(windows) {
        e.raw_os_error() == Some(17)
    } else {
        e.raw_os_error() == Some(18)
    }
}

#[cfg(windows)]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    windows::rename_no_replace(from, to)
}

#[cfg(not(windows))]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    if fs::symlink_metadata(to).is_ok() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    fs::rename(from, to)
}

/// Files whose content lives online: reading them downloads it.
pub fn is_cloud_attributes(attributes: u32) -> bool {
    // FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.
    attributes & (0x0004_0000 | 0x0040_0000) != 0
}

#[cfg(windows)]
fn is_cloud(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    is_cloud_attributes(meta.file_attributes())
}

#[cfg(not(windows))]
fn is_cloud(_: &fs::Metadata) -> bool {
    false
}

/// Refuse what a copy must not go through.
fn check_copyable(item: &Path, path: &Path, meta: &fs::Metadata) -> Done<()> {
    let item = item.display();
    if meta.file_type().is_symlink() {
        return Err(Failure::Failed(format!(
            "{item} contains a link to another place ({}), and the holding area is on another \
             drive, so it was left where it is. Remove the link and clear it again.",
            path.display()
        )));
    }
    if is_cloud(meta) {
        return Err(Failure::Failed(format!(
            "{item} contains a file that is stored online ({}), and moving it would download \
             it, so it was left where it is.",
            path.display()
        )));
    }
    Ok(())
}

/// Everything in `root`, folders before what is in them, as paths relative
/// to it and whether each is a folder. Refuses links and cloud files.
fn survey(root: &Path) -> Done<Vec<(PathBuf, bool)>> {
    let shown = root.display().to_string();
    let read =
        |p: &Path| fs::symlink_metadata(p).map_err(|e| Failure::Failed(io_sentence(&shown, &e)));
    let meta = read(root)?;
    check_copyable(root, root, &meta)?;
    let mut out = vec![(PathBuf::new(), meta.is_dir())];
    let mut i = 0;
    while i < out.len() {
        let (rel, is_dir) = out[i].clone();
        i += 1;
        if !is_dir {
            continue;
        }
        let entries = fs::read_dir(under(root, &rel))
            .map_err(|e| Failure::Failed(io_sentence(&shown, &e)))?;
        for entry in entries {
            let entry = entry.map_err(|e| Failure::Failed(io_sentence(&shown, &e)))?;
            let path = entry.path();
            let meta = read(&path)?;
            check_copyable(root, &path, &meta)?;
            out.push((rel.join(entry.file_name()), meta.is_dir()));
        }
    }
    Ok(out)
}

/// `rel` under `base`; the empty path is `base` itself, with no separator
/// added, which would make a file read as a folder.
fn under(base: &Path, rel: &Path) -> PathBuf {
    if rel.as_os_str().is_empty() {
        base.to_path_buf()
    } else {
        base.join(rel)
    }
}

fn copy_then_remove(from: &Path, to: &Path, disk: &mut dyn Disk) -> Done<()> {
    let shown = from.display().to_string();
    let entries = survey(from)?;
    let copied = (|| -> Done<()> {
        for (rel, is_dir) in &entries {
            let (src, dst) = (under(from, rel), under(to, rel));
            // Looked at again just before: a folder swapped for a link since
            // the survey is refused, not followed.
            let meta =
                fs::symlink_metadata(&src).map_err(|e| Failure::Failed(io_sentence(&shown, &e)))?;
            check_copyable(from, &src, &meta)?;
            let done = if *is_dir {
                disk.keep_source(&src).and_then(|()| disk.make_dir(&dst))
            } else {
                disk.copy_file(&src, &dst)
            };
            done.map_err(|e| Failure::Failed(io_sentence(&shown, &e)))?;
        }
        Ok(())
    })();
    // What the copy held is let go of before anything is removed, here or
    // from where it came.
    disk.release();
    if let Err(f) = copied {
        // Nothing was removed from `from`; what was copied is not needed.
        // Should it not all go, it is still never taken for a whole copy:
        // the answer stays `Failed`, which keeps no record of it.
        discard(to);
        return Err(f);
    }
    // Only what was looked at and copied, deepest first: the survey lists
    // a folder before what is in it. A folder that still holds something
    // was given it since, and that stays where it is, with the folder.
    let mut kept_new = false;
    let removed = entries.iter().rev().try_for_each(|(rel, is_dir)| {
        match disk.remove(&under(from, rel), *is_dir) {
            Err(e) if *is_dir && e.kind() == io::ErrorKind::DirectoryNotEmpty => {
                kept_new = true;
                Ok(())
            }
            // Gone by itself since the copy: nothing to remove, and the
            // copy has it.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    });
    if removed.is_ok() && kept_new {
        return Err(Failure::Partial(format!(
            "{shown} was moved, apart from what was put in it after Brokey looked, which is \
             still where it was. Restore it from Settings to put the rest back beside it, or \
             delete it from the holding area."
        )));
    }
    if let Err(e) = removed {
        // Part of it may be gone: put back what is missing from the copy,
        // and only then let the copy go.
        let mut put_back = true;
        for (rel, is_dir) in &entries {
            let (src, dst) = (under(from, rel), under(to, rel));
            if fs::symlink_metadata(&src).is_ok() {
                continue;
            }
            let done = if *is_dir {
                disk.make_dir(&src)
            } else {
                disk.copy_file(&dst, &src)
            };
            put_back &= done.is_ok();
        }
        disk.release();
        if put_back {
            discard(to);
            return Err(Failure::Failed(io_sentence(&shown, &e)));
        }
        return Err(Failure::Partial(format!(
            "{shown} could only be moved in part ({e}): some of it is still where it was and a \
             whole copy is kept. Close any program using it, then restore it from Settings to \
             put it back together."
        )));
    }
    Ok(())
}

/// Put back from `from` whatever is missing at `to`, never replacing what is
/// there, and let go of each part of `from` once `to` has it. Used for an
/// item that was moved in part or that a crash interrupted, where some of
/// it is still in its place. A link at `to`, or inside it on the way, is
/// refused, never followed; so is a file where a folder was, or the other
/// way round. What could not be put back stays in `from`.
pub fn merge_file_or_folder(from: &Path, to: &Path, force_copy: bool) -> Done<()> {
    merge_with(from, to, force_copy, &mut Plain)
}

/// [`merge_file_or_folder`] through `disk`.
pub fn merge_with(from: &Path, to: &Path, force_copy: bool, disk: &mut dyn Disk) -> Done<()> {
    let shown = to.display().to_string();
    let from_meta = match fs::symlink_metadata(from) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Failure::Missing),
        Err(e) => return Err(Failure::Failed(io_sentence(&shown, &e))),
        Ok(m) => m,
    };
    let to_meta = match fs::symlink_metadata(to) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return move_with(from, to, force_copy, disk);
        }
        Err(e) => return Err(Failure::Failed(io_sentence(&shown, &e))),
        Ok(m) => m,
    };
    if to_meta.file_type().is_symlink() {
        return Err(Failure::Failed(format!(
            "{shown} is now a link to another place, so nothing was put back through it. \
             Remove the link and restore again."
        )));
    }
    match (from_meta.is_dir(), to_meta.is_dir()) {
        // The one in its place was never moved: it stays, and the copy goes.
        (false, false) => {
            discard(from);
            Ok(())
        }
        (true, true) => {
            // Held as it is while anything is put into it.
            disk.enter(to)
                .map_err(|e| Failure::Failed(io_sentence(&shown, &e)))?;
            let entries =
                fs::read_dir(from).map_err(|e| Failure::Failed(io_sentence(&shown, &e)))?;
            let mut first = None;
            for entry in entries {
                let entry = entry.map_err(|e| Failure::Failed(io_sentence(&shown, &e)))?;
                let inside = to.join(entry.file_name());
                if let Err(f) = merge_with(&entry.path(), &inside, force_copy, disk) {
                    first.get_or_insert(f);
                }
            }
            match first {
                None => {
                    let _ = fs::remove_dir(from);
                    Ok(())
                }
                Some(f) => Err(f),
            }
        }
        _ => Err(Failure::Failed(format!(
            "Something else is now at {shown}, so the copy in the holding area was kept. Move it \
             out of the way and restore again."
        ))),
    }
}

fn discard(path: &Path) {
    let _ = match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(_) => Ok(()),
    };
}

/// Bytes under a path, never following a link.
fn size_of(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path)
        .map(|entries| entries.flatten().map(|e| size_of(&e.path())).sum())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The manifest and exported files

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn save(dir: &Path, manifest: &Manifest) -> io::Result<()> {
    let text = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
    let temp = dir.join("manifest.json.new");
    write_synced(&temp, &text)?;
    fs::rename(&temp, dir.join(MANIFEST))
}

/// The entry's manifest; `Ok(None)` when there is none.
pub fn read_manifest(dir: &Path) -> io::Result<Option<Manifest>> {
    match fs::read(dir.join(MANIFEST)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Write an export to a file that must not be there yet: one that is, is
/// someone else's and [`Failure::Occupied`].
fn write_export_bytes(path: &Path, bytes: &[u8], location: &str) -> Done<u64> {
    use std::io::Write;
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        });
    if let Err(e) = &written
        && e.kind() == io::ErrorKind::AlreadyExists
    {
        return Err(Failure::Occupied);
    }
    written.map_err(|e| {
        Failure::Failed(format!(
            "Brokey could not write a copy of {location} into the holding area ({e}), so it was \
             left where it is. Check there is space on the drive and try again."
        ))
    })?;
    Ok(bytes.len() as u64)
}

fn read_export(path: &Path, location: &str) -> Done<Vec<u8>> {
    fs::read(path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            Failure::Missing
        } else {
            unreadable(location)
        }
    })
}

fn unreadable(location: &str) -> Failure {
    Failure::Failed(format!(
        "The copy of {location} in the holding area could not be read, so it was left there."
    ))
}

// ---------------------------------------------------------------------------
// Time

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (Howard Hinnant's civil-from-days).
pub fn rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(DAY);
    let rest = seconds.rem_euclid(DAY);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

fn created_seconds(created: &str) -> Option<i64> {
    crate::selfupdate::release::epoch_seconds(created)
}

// ---------------------------------------------------------------------------
// Hold

/// Move `items` into the holding entry `id` under `root`, recording each
/// before it moves. An item inside a folder or key held before it in the
/// same entry is counted as held and recorded as covered by it.
pub fn hold(
    root: &Path,
    id: &str,
    name: &str,
    items: &[HoldItem],
    m: &mut dyn Mutate,
) -> HoldReport {
    let mut report = HoldReport::default();
    let refuse_all = |report: &mut HoldReport, sentence: String| {
        for item in items {
            report
                .failed
                .push((item.location.clone(), sentence.clone()));
        }
    };
    if !valid_id(id) {
        refuse_all(&mut report, not_an_id(id, "moved"));
        return report;
    }
    if let Err(f) = m.make_root(root) {
        let sentence = match f {
            Failure::Failed(s) | Failure::Partial(s) => s,
            _ => format!(
                "Brokey could not make the holding area at {}, so nothing was moved. Try again.",
                root.display()
            ),
        };
        refuse_all(&mut report, sentence);
        return report;
    }
    let dir = root.join(id);
    if fs::symlink_metadata(&dir).is_ok()
        && let Err(f) = m.trust_entry(root, &dir)
    {
        refuse_all(&mut report, sentence_for(id, f));
        return report;
    }
    let opened = fs::create_dir_all(dir.join(ITEMS)).and_then(|()| read_manifest(&dir));
    let mut manifest = match opened {
        Ok(Some(manifest)) => manifest,
        Ok(None) => Manifest {
            id: id.to_string(),
            name: name.to_string(),
            created: rfc3339(now()),
            items: Vec::new(),
            next: 1,
        },
        Err(e) => {
            refuse_all(
                &mut report,
                format!(
                    "Brokey could not open the holding area entry {} ({e}), so nothing was \
                     moved. Try again.",
                    dir.display()
                ),
            );
            return report;
        }
    };
    for item in items {
        match hold_one(&dir, &mut manifest, item, m) {
            Ok(()) => report.held += 1,
            Err(sentence) => report.failed.push((item.location.clone(), sentence)),
        }
    }
    if manifest.items.is_empty() {
        let _ = remove_finished_entry(&dir);
    }
    report
}

/// Take away an entry whose record has nothing left in it: the record,
/// then the items folder and the entry's folder, each only if empty. What
/// is in either without a record (something that was there before the
/// entry used the place) is left, and the entry with it, to be deleted
/// only by a Delete now the user confirms.
fn remove_finished_entry(dir: &Path) -> io::Result<()> {
    for name in [MANIFEST, "manifest.json.new"] {
        match fs::remove_file(dir.join(name)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    let _ = fs::remove_dir(dir.join(ITEMS));
    if fs::read_dir(dir)?.next().is_some() {
        return Ok(());
    }
    fs::remove_dir(dir)
}

/// A stored item's path. `stored` is written with `/` and taken apart here,
/// since a verbatim `\\?\` path takes no other separator than `\`.
fn stored_path(dir: &Path, stored: &str) -> PathBuf {
    stored
        .split('/')
        .fold(dir.to_path_buf(), |p, part| p.join(part))
}

fn not_an_id(id: &str, what: &str) -> String {
    format!("{id} is not a name Brokey gives a holding area entry, so nothing was {what}.")
}

fn hold_one(
    dir: &Path,
    manifest: &mut Manifest,
    item: &HoldItem,
    m: &mut dyn Mutate,
) -> Result<(), String> {
    let location = item.location.as_str();
    let place = Place::parse(item).ok_or_else(|| {
        format!("{location} is not a place Brokey knows how to hold, so it was left where it is.")
    })?;
    let unsaved = |e: io::Error| {
        format!(
            "Brokey could not write the holding area's record ({e}), so {location} was left \
             where it is. Check there is space on the drive and try again."
        )
    };

    let parent = manifest
        .items
        .iter()
        .filter(|h| h.state == HeldState::Held)
        .find(|h| Place::parse(&h.item).is_some_and(|p| p.covers(&place)))
        .map(|h| h.item.location.clone());
    if let Some(parent) = parent {
        manifest.items.push(Held {
            item: item.clone(),
            stored: String::new(),
            state: HeldState::Covered,
            covered_by: Some(parent),
            size: 0,
        });
        return save(dir, manifest).map_err(|e| {
            manifest.items.pop();
            unsaved(e)
        });
    }

    // A place something already occupies (left by a crash before its
    // record was written, or put there since) is passed over, never
    // deleted or written over.
    let mut n = manifest.next.max(1);
    while fs::symlink_metadata(stored_path(dir, &place.stored_name(n))).is_ok() {
        n += 1;
    }
    manifest.next = n + 1;
    let stored = place.stored_name(n);
    manifest.items.push(Held {
        item: item.clone(),
        stored: stored.clone(),
        state: HeldState::Moving,
        covered_by: None,
        size: 0,
    });
    if let Err(e) = save(dir, manifest) {
        manifest.items.pop();
        return Err(unsaved(e));
    }

    let target = stored_path(dir, &stored);
    let last = manifest.items.len() - 1;
    let result = match take(&place, &target, location, m) {
        Ok(size) => {
            manifest.items[last].state = HeldState::Held;
            manifest.items[last].size = size;
            Ok(())
        }
        Err(Failure::Partial(sentence)) => {
            // Some of it is still in its place and the holding area has the
            // whole of it: the only whole copy, so it is kept and recorded.
            manifest.items[last].state = HeldState::Partial;
            manifest.items[last].size = size_of(&target);
            Err(sentence)
        }
        Err(Failure::Occupied) => {
            // Something took the place between the look and the move: it is
            // not this item's, so it is left there, and the item is still in
            // its own place.
            manifest.items.pop();
            Err(sentence_for(location, Failure::Occupied))
        }
        Err(f) => {
            // Everything is still in its place (`take` has made sure of it),
            // so whatever reached the holding area is not needed.
            manifest.items.pop();
            discard(&target);
            Err(sentence_for(location, f))
        }
    };
    // Should this write fail the record still says moving, which `restore`
    // puts back by merging whenever the stored copy is there.
    let _ = save(dir, manifest);
    result
}

/// What an item that is not a file or a folder is kept as.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Export {
    Key(KeyNode),
    Value(RegValue),
    Service(KeyNode),
    Task(String),
    Firewall(Vec<(String, String)>),
}

fn sorted_node(node: &KeyNode) -> KeyNode {
    let mut values = node.values.clone();
    values.sort_by_key(|(n, _)| n.to_lowercase());
    let mut subkeys: Vec<(String, KeyNode)> = node
        .subkeys
        .iter()
        .map(|(n, k)| (n.clone(), sorted_node(k)))
        .collect();
    subkeys.sort_by_key(|(n, _)| n.to_lowercase());
    KeyNode { values, subkeys }
}

impl Export {
    /// Why Windows would need a password to put this back, which the
    /// restore never has: the end of a sentence, or `None`.
    fn needs_a_password(&self) -> Option<String> {
        match self {
            Export::Service(key) => {
                let account = service_account(key);
                (!needs_no_password(account.as_deref()))
                    .then(|| password_reason_for_service(account.as_deref().unwrap_or_default()))
            }
            Export::Task(xml) => task_needs_password(xml).then(password_reason_for_task),
            _ => None,
        }
    }

    /// Read the item as it is now. A file or folder is moved rather than
    /// exported, so it has none.
    fn of(place: &Place, m: &mut dyn Mutate) -> Done<Export> {
        match place {
            Place::Path(_) => Err(Failure::Failed(
                "Brokey mixed up what it was holding, so nothing was changed.".into(),
            )),
            Place::Key(hive, path) => m.export_key(*hive, path).map(Export::Key),
            Place::Value(hive, key, name) => m.export_value(*hive, key, name).map(Export::Value),
            Place::Service(name) => m.export_service(name).map(Export::Service),
            Place::Task(path) => m.export_task(path).map(Export::Task),
            Place::Firewall(id) => m.export_firewall_rule(id).map(Export::Firewall),
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let json = match self {
            Export::Key(k) | Export::Service(k) => serde_json::to_vec_pretty(k),
            Export::Value(v) => serde_json::to_vec_pretty(v),
            Export::Firewall(r) => serde_json::to_vec_pretty(r),
            Export::Task(xml) => return xml.as_bytes().to_vec(),
        };
        // Plain data with string keys, which always serialises.
        json.unwrap_or_default()
    }

    fn read(place: &Place, stored: &Path, location: &str) -> Done<Export> {
        let bytes = read_export(stored, location)?;
        let bad = |_| unreadable(location);
        match place {
            Place::Path(_) => Err(unreadable(location)),
            Place::Key(..) => serde_json::from_slice(&bytes).map(Export::Key).map_err(bad),
            Place::Value(..) => serde_json::from_slice(&bytes)
                .map(Export::Value)
                .map_err(bad),
            Place::Service(_) => serde_json::from_slice(&bytes)
                .map(Export::Service)
                .map_err(bad),
            Place::Task(_) => String::from_utf8(bytes)
                .map(Export::Task)
                .map_err(|_| unreadable(location)),
            Place::Firewall(_) => serde_json::from_slice(&bytes)
                .map(Export::Firewall)
                .map_err(bad),
        }
    }

    fn delete(&self, place: &Place, m: &mut dyn Mutate) -> Done<()> {
        match (place, self) {
            (Place::Key(hive, path), _) => m.delete_key(*hive, path),
            (Place::Value(hive, key, name), _) => m.delete_value(*hive, key, name),
            (Place::Service(name), _) => m.delete_service(name),
            (Place::Task(path), _) => m.delete_task(path),
            (Place::Firewall(id), Export::Firewall(rules)) => m.delete_firewall_rule(id, rules),
            _ => Err(Failure::Failed(
                "Brokey mixed up what it was holding, so nothing was changed.".into(),
            )),
        }
    }

    fn restore(&self, place: &Place, merge: bool, m: &mut dyn Mutate) -> Done<()> {
        match (place, self) {
            (Place::Key(hive, path), Export::Key(k)) => m.restore_key(*hive, path, k, merge),
            (Place::Value(hive, key, name), Export::Value(v)) => {
                m.restore_value(*hive, key, name, v, merge)
            }
            (Place::Service(name), Export::Service(k)) => m.restore_service(name, k, merge),
            (Place::Task(path), Export::Task(xml)) => m.restore_task(path, xml, merge),
            (Place::Firewall(id), Export::Firewall(r)) => m.restore_firewall_rule(id, r, merge),
            _ => Err(Failure::Failed(
                "Brokey mixed up what it was holding, so nothing was put back.".into(),
            )),
        }
    }

    /// The same content in one order, for comparing two exports.
    fn normalised(&self) -> Export {
        match self {
            Export::Key(k) => Export::Key(sorted_node(k)),
            Export::Service(k) => Export::Service(sorted_node(k)),
            Export::Firewall(r) => {
                let mut r = r.clone();
                r.sort();
                Export::Firewall(r)
            }
            other => other.clone(),
        }
    }
}

/// Move or export-and-delete one item into `target`; its size there.
///
/// Every failure but [`Failure::Partial`] leaves the item wholly in its
/// place: a delete that fails part-way (a key with a subkey it may not
/// remove) is looked at
/// again, and what went is put back from the export, never replacing what
/// is there. Only when that cannot be done is the answer `Partial`, and the
/// export is then the one whole copy.
fn take(place: &Place, target: &Path, location: &str, m: &mut dyn Mutate) -> Done<u64> {
    if let Place::Path(path) = place {
        m.move_path(path, target)?;
        return Ok(size_of(target));
    }
    let export = Export::of(place, m)?;
    // What the restore could not recreate is never taken.
    if let Some(why) = export.needs_a_password() {
        return Err(Failure::Failed(left_for_password(location, &why)));
    }
    let size = write_export_bytes(target, &export.bytes(), location)?;
    let Err(failure) = export.delete(place, m) else {
        return Ok(size);
    };
    match Export::of(place, m) {
        // Nothing went.
        Ok(now) if now.normalised() == export.normalised() => Err(failure),
        // All of it went after all.
        Err(Failure::Missing) => Ok(size),
        // Some of it went: put that back, beside what is still there.
        Ok(_) => match export.restore(place, true, m) {
            Ok(()) => Err(failure),
            Err(_) => Err(Failure::Partial(format!(
                "{location} could only be removed in part: some of it is still there and a \
                 whole copy is kept in the holding area. Restore it from Settings to put it \
                 back together."
            ))),
        },
        // It cannot be told what went, so the export is kept.
        Err(_) => Err(Failure::Partial(format!(
            "{location} could not be checked after its removal failed, so the copy in the \
             holding area is kept. Restore it from Settings to put back anything that went."
        ))),
    }
}

/// Put one item back from `stored`. With `merge`, only what is missing is
/// put back and nothing there is replaced.
fn put_back(
    place: &Place,
    stored: &Path,
    location: &str,
    merge: bool,
    m: &mut dyn Mutate,
) -> Done<()> {
    match place {
        Place::Path(path) if merge => m.merge_path(stored, path),
        Place::Path(path) => m.move_path(stored, path),
        _ => Export::read(place, stored, location)?.restore(place, merge, m),
    }
}

fn restore_sentence(location: &str, f: Failure) -> String {
    match f {
        Failure::Occupied => format!(
            "{location} is there again, so the one in the holding area was kept. Move what is \
             there now out of the way and restore again."
        ),
        Failure::Missing => format!(
            "The copy of {location} is no longer in the holding area, so it could not be put back."
        ),
        Failure::Failed(s) | Failure::Partial(s) => s,
    }
}

/// Put back everything in entry `id`, newest first, and remove the entry
/// once all of it is back. Where something new now occupies a place, that
/// item is refused with a sentence and stays in the holding area. An item
/// moved in part, or one a crash left recorded as moving, is merged back:
/// what is missing is put beside what is still there.
pub fn restore(root: &Path, id: &str, m: &mut dyn Mutate) -> HoldReport {
    let mut report = HoldReport::default();
    if !valid_id(id) {
        report
            .failed
            .push((id.to_string(), not_an_id(id, "restored")));
        return report;
    }
    let dir = root.join(id);
    if fs::symlink_metadata(&dir).is_ok()
        && let Err(f) = m.trust_entry(root, &dir)
    {
        report.failed.push((id.to_string(), sentence_for(id, f)));
        return report;
    }
    let mut manifest = match read_manifest(&dir) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => {
            report.failed.push((
                id.to_string(),
                format!(
                    "There is nothing in the holding area called {id}, so nothing was restored."
                ),
            ));
            return report;
        }
        Err(e) => {
            report.failed.push((
                id.to_string(),
                format!(
                    "The holding area's record of {id} could not be read ({e}), so nothing was \
                     restored."
                ),
            ));
            return report;
        }
    };

    // Newest first, so that a folder held after something inside it is
    // back before that thing goes into it.
    for i in (0..manifest.items.len()).rev() {
        let held = manifest.items[i].clone();
        if held.state == HeldState::Covered {
            continue;
        }
        let location = held.item.location;
        let stored = stored_path(&dir, &held.stored);
        let Some(place) = Place::parse(&manifest.items[i].item) else {
            report.failed.push((
                location.clone(),
                format!(
                    "{location} is not a place Brokey knows how to put back, so it was kept in \
                     the holding area."
                ),
            ));
            continue;
        };
        if held.state == HeldState::Moving && fs::symlink_metadata(&stored).is_err() {
            // Recorded and then never moved: it is still where it was.
            manifest.items.remove(i);
        } else {
            let merge = held.state != HeldState::Held;
            match put_back(&place, &stored, &location, merge, m) {
                Ok(()) => {
                    if !matches!(place, Place::Path(_)) {
                        discard(&stored);
                    }
                    manifest.items.remove(i);
                    report.held += 1;
                }
                Err(f) => {
                    if matches!(f, Failure::Partial(_)) {
                        // Now the place has it whole and the holding area in
                        // part: the next restore merges what is left.
                        manifest.items[i].state = HeldState::Partial;
                        let _ = save(&dir, &manifest);
                    }
                    report
                        .failed
                        .push((location.clone(), restore_sentence(&location, f)));
                    continue;
                }
            }
        }
        // Kept current item by item, so that a crash part-way loses
        // nothing; the write after the loop is the one that is reported.
        let _ = save(&dir, &manifest);
    }

    // A covered item is back once what covers it is.
    let remaining: Vec<String> = manifest
        .items
        .iter()
        .filter(|h| h.state != HeldState::Covered)
        .map(|h| h.item.location.clone())
        .collect();
    let before = manifest.items.len();
    manifest.items.retain(|h| {
        h.state != HeldState::Covered
            || h.covered_by.as_ref().is_some_and(|p| remaining.contains(p))
    });
    report.held += before - manifest.items.len();

    let finished = if manifest.items.is_empty() {
        remove_finished_entry(&dir)
    } else {
        save(&dir, &manifest)
    };
    if let Err(e) = finished {
        report.failed.push((
            id.to_string(),
            format!(
                "The holding area's record of {id} could not be brought up to date ({e}). What \
                 was put back is back; restore again to finish."
            ),
        ));
    }
    report
}

/// Ids of the entries under a root, links left out. A root that is not
/// there has none; one that cannot be read is an error, never "none".
fn entry_ids(root: &Path) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|e| {
            fs::symlink_metadata(e.path()).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
        })
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|id| valid_id(id))
        .collect();
    ids.sort();
    Ok(ids)
}

fn modified(path: &Path) -> Option<i64> {
    let time = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let since = time.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(since.as_secs() as i64)
}

/// When an entry was made: its manifest's word, or failing that its folder's.
fn entry_created(dir: &Path, manifest: Option<&Manifest>) -> Option<i64> {
    manifest
        .and_then(|m| created_seconds(&m.created))
        .or_else(|| modified(dir))
}

/// An entry holding the only whole copy of something that is partly still
/// in its place: moved in part, or a move a crash interrupted. It is never
/// emptied automatically, since that would lose what it holds.
fn holds_only_copy(dir: &Path, manifest: &Manifest) -> bool {
    manifest.items.iter().any(|h| match h.state {
        HeldState::Partial => true,
        HeldState::Moving => fs::symlink_metadata(stored_path(dir, &h.stored)).is_ok(),
        HeldState::Held | HeldState::Covered => false,
    })
}

/// An entry that age must never empty: its record cannot be read (missing,
/// unreadable or not a manifest), so nobody can say what it holds, or it
/// holds the only whole copy of something moved in part.
fn incomplete(dir: &Path, manifest: &io::Result<Option<Manifest>>) -> bool {
    match manifest {
        Ok(Some(m)) => holds_only_copy(dir, m),
        _ => true,
    }
}

/// Delete entries for good: the one named, or every one; with
/// `older_than_days`, only those older than that, and never one that is
/// [`incomplete`]. Only a named or whole empty, which the user confirms,
/// deletes that.
pub fn empty(root: &Path, id: Option<&str>, older_than_days: Option<u32>) -> HoldReport {
    let mut report = HoldReport::default();
    let ids = match id {
        Some(id) if !valid_id(id) => {
            report
                .failed
                .push((id.to_string(), not_an_id(id, "emptied")));
            return report;
        }
        Some(id) => vec![id.to_string()],
        None => match entry_ids(root) {
            Ok(ids) => ids,
            Err(e) => {
                report.failed.push((
                    root.display().to_string(),
                    format!(
                        "Brokey could not look in the holding area {} ({e}), so nothing was \
                         emptied.",
                        root.display()
                    ),
                ));
                return report;
            }
        },
    };
    let now = now();
    for id in ids {
        let dir = root.join(&id);
        if let Some(days) = older_than_days {
            let read = read_manifest(&dir);
            if incomplete(&dir, &read) {
                continue;
            }
            let manifest = read.ok().flatten();
            let old = entry_created(&dir, manifest.as_ref())
                .is_some_and(|c| now - c > i64::from(days) * DAY);
            if !old {
                continue;
            }
        }
        match fs::remove_dir_all(&dir) {
            Ok(()) => report.held += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => report.failed.push((
                id.clone(),
                format!("There is nothing in the holding area called {id}."),
            )),
            Err(e) => report.failed.push((
                id,
                format!(
                    "{} could not be emptied ({e}). Close any program using it and try again.",
                    dir.display()
                ),
            )),
        }
    }
    report
}

/// What [`list`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    /// Newest first.
    pub entries: Vec<HoldingEntry>,
    /// Roots that are there and could not be read, so the page can say so
    /// rather than show an empty holding area.
    pub unreadable: Vec<String>,
}

/// Every entry in both roots. A machine entry past [`AUTO_EMPTY_DAYS`] is
/// `waiting`: it is emptied by the next plan that asks for Administrator
/// anyway; one that is [`incomplete`] never is.
pub fn list(user_root: &Path, machine_root: &Path) -> Listing {
    let now = now();
    let mut out = Listing::default();
    for (root, machine) in [(user_root, false), (machine_root, true)] {
        let ids = match entry_ids(root) {
            Ok(ids) => ids,
            Err(_) => {
                out.unreadable.push(root.display().to_string());
                continue;
            }
        };
        for id in ids {
            let dir = root.join(&id);
            let read = read_manifest(&dir);
            let keep = incomplete(&dir, &read);
            let manifest = read.ok().flatten();
            let seconds = entry_created(&dir, manifest.as_ref()).unwrap_or(now);
            let created = manifest
                .as_ref()
                .map(|m| m.created.clone())
                .filter(|c| created_seconds(c).is_some())
                .unwrap_or_else(|| rfc3339(seconds));
            let (name, size, items) = match manifest {
                Some(m) => (
                    m.name,
                    m.items.iter().map(|h| h.size).sum(),
                    m.items.len() as u32,
                ),
                // A record that cannot be read is still shown, so that it
                // can be emptied.
                None => (id.clone(), size_of(&dir), 0),
            };
            out.entries.push(HoldingEntry {
                id,
                name,
                created,
                size,
                items,
                scope_machine: machine,
                waiting: machine && !keep && now - seconds > i64::from(AUTO_EMPTY_DAYS) * DAY,
                incomplete: keep,
            });
        }
    }
    out.entries.sort_by(|a, b| b.created.cmp(&a.created));
    out
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let text: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        s.serialize_str(&text)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        if text.len() % 2 != 0 || !text.is_ascii() {
            return Err(D::Error::custom("not an even number of hex digits"));
        }
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(D::Error::custom))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// The machine as far as the holding area changes it: the registry,
    /// services, tasks and firewall rules in memory; files on disk under a
    /// temporary folder, moved by the same code the live backend uses.
    #[derive(Default)]
    struct Fake {
        /// Lower-case `Hive\path` to the key's own spelling and its values.
        keys: BTreeMap<String, (String, Vec<(String, RegValue)>)>,
        services: BTreeMap<String, KeyNode>,
        tasks: BTreeMap<String, String>,
        firewall: BTreeMap<String, String>,
        /// Paths whose move fails as a file another program has open does.
        locked: Vec<PathBuf>,
        /// Paths whose move leaves a half copy at the destination and fails,
        /// as a copy across drives whose clean-up also failed would.
        half_copied: Vec<PathBuf>,
        force_copy: bool,
        /// `delete_key` removes its deepest subkey and then fails, as
        /// `RegDeleteTreeW` does on a subkey it may not delete.
        delete_key_fails_part_way: bool,
        /// `delete_firewall_rule` removes the first rule and then fails.
        delete_firewall_fails: bool,
        /// Every merging restore fails.
        merge_fails: bool,
        /// Paths whose move finds its place in the holding area taken after
        /// it was looked at: something else is written there first, and the
        /// move answers `Occupied`, as a rename that will not replace does.
        taken_meanwhile: Vec<PathBuf>,
    }

    type Keys = BTreeMap<String, (String, Vec<(String, RegValue)>)>;
    type State = (
        Keys,
        BTreeMap<String, KeyNode>,
        BTreeMap<String, String>,
        BTreeMap<String, String>,
    );

    impl Fake {
        fn id(hive: Hive, path: &str) -> String {
            format!("{hive:?}\\{path}").to_lowercase()
        }

        fn put_key(&mut self, hive: Hive, path: &str, values: Vec<(&str, RegValue)>) {
            let values = values
                .into_iter()
                .map(|(n, v)| (n.to_string(), v))
                .collect();
            self.keys
                .insert(Fake::id(hive, path), (path.to_string(), values));
        }

        /// Everything but the files, with each key's values in name order,
        /// since the registry keeps no order of values.
        fn state(&self) -> State {
            let mut keys = self.keys.clone();
            for (_, values) in keys.values_mut() {
                values.sort_by(|a, b| a.0.cmp(&b.0));
            }
            (
                keys,
                self.services.clone(),
                self.tasks.clone(),
                self.firewall.clone(),
            )
        }

        fn node(&self, id: &str) -> KeyNode {
            let prefix = format!("{id}\\");
            let mut node = KeyNode {
                values: self.keys[id].1.clone(),
                subkeys: Vec::new(),
            };
            for (child, (spelling, _)) in &self.keys {
                if let Some(rest) = child.strip_prefix(&prefix)
                    && !rest.contains('\\')
                {
                    let leaf = spelling.rsplit('\\').next().unwrap().to_string();
                    node.subkeys.push((leaf, self.node(child)));
                }
            }
            node
        }

        /// Write a key tree; with `merge`, only what is missing.
        fn insert(&mut self, hive: Hive, path: &str, node: &KeyNode, merge: bool) {
            let entry = self
                .keys
                .entry(Fake::id(hive, path))
                .or_insert_with(|| (path.to_string(), Vec::new()));
            for (name, value) in &node.values {
                if !(merge && entry.1.iter().any(|(n, _)| n == name)) {
                    entry.1.push((name.clone(), value.clone()));
                }
            }
            for (leaf, sub) in &node.subkeys {
                self.insert(hive, &format!("{path}\\{leaf}"), sub, merge);
            }
        }

        fn merge_refused(&self, merge: bool) -> Done<()> {
            if merge && self.merge_fails {
                return Err(Failure::Failed("The merge failed.".into()));
            }
            Ok(())
        }
    }

    impl Mutate for Fake {
        fn make_root(&mut self, root: &Path) -> Done<()> {
            fs::create_dir_all(root).map_err(|e| Failure::Failed(e.to_string()))
        }
        fn trust_entry(&mut self, _: &Path, _: &Path) -> Done<()> {
            Ok(())
        }
        fn move_path(&mut self, from: &Path, to: &Path) -> Done<()> {
            if self.locked.iter().any(|l| l == from) {
                return Err(Failure::Failed(in_use(&from.display().to_string())));
            }
            if self.taken_meanwhile.iter().any(|l| l == from) {
                file(to, "theirs");
                return Err(Failure::Occupied);
            }
            if self.half_copied.iter().any(|l| l == from) {
                fs::create_dir_all(to).unwrap();
                fs::write(to.join("half"), "half").unwrap();
                return Err(Failure::Failed(in_use(&from.display().to_string())));
            }
            move_file_or_folder(from, to, self.force_copy)
        }
        fn merge_path(&mut self, from: &Path, to: &Path) -> Done<()> {
            self.merge_refused(true)?;
            merge_file_or_folder(from, to, self.force_copy)
        }
        fn export_key(&mut self, hive: Hive, path: &str) -> Done<KeyNode> {
            let id = Fake::id(hive, path);
            if !self.keys.contains_key(&id) {
                return Err(Failure::Missing);
            }
            Ok(self.node(&id))
        }
        fn delete_key(&mut self, hive: Hive, path: &str) -> Done<()> {
            let id = Fake::id(hive, path);
            let prefix = format!("{id}\\");
            if self.delete_key_fails_part_way {
                let deepest = self
                    .keys
                    .keys()
                    .filter(|k| k.starts_with(&prefix))
                    .max_by_key(|k| k.matches('\\').count())
                    .cloned();
                if let Some(deepest) = deepest {
                    self.keys.remove(&deepest);
                }
                return Err(Failure::Failed("Access is denied.".into()));
            }
            self.keys.retain(|k, _| *k != id && !k.starts_with(&prefix));
            Ok(())
        }
        fn restore_key(&mut self, hive: Hive, path: &str, key: &KeyNode, merge: bool) -> Done<()> {
            self.merge_refused(merge)?;
            if !merge && self.keys.contains_key(&Fake::id(hive, path)) {
                return Err(Failure::Occupied);
            }
            self.insert(hive, path, key, merge);
            Ok(())
        }
        fn export_value(&mut self, hive: Hive, key: &str, name: &str) -> Done<RegValue> {
            self.keys
                .get(&Fake::id(hive, key))
                .and_then(|(_, v)| v.iter().find(|(n, _)| n == name))
                .map(|(_, v)| v.clone())
                .ok_or(Failure::Missing)
        }
        fn delete_value(&mut self, hive: Hive, key: &str, name: &str) -> Done<()> {
            let values = &mut self.keys.get_mut(&Fake::id(hive, key)).unwrap().1;
            values.retain(|(n, _)| n != name);
            Ok(())
        }
        fn restore_value(
            &mut self,
            hive: Hive,
            key: &str,
            name: &str,
            value: &RegValue,
            merge: bool,
        ) -> Done<()> {
            self.merge_refused(merge)?;
            let entry = self
                .keys
                .entry(Fake::id(hive, key))
                .or_insert_with(|| (key.to_string(), Vec::new()));
            if entry.1.iter().any(|(n, _)| n == name) {
                return if merge {
                    Ok(())
                } else {
                    Err(Failure::Occupied)
                };
            }
            entry.1.push((name.to_string(), value.clone()));
            Ok(())
        }
        fn export_service(&mut self, name: &str) -> Done<KeyNode> {
            self.services.get(name).cloned().ok_or(Failure::Missing)
        }
        fn delete_service(&mut self, name: &str) -> Done<()> {
            self.services
                .remove(name)
                .map(|_| ())
                .ok_or(Failure::Missing)
        }
        fn restore_service(&mut self, name: &str, key: &KeyNode, merge: bool) -> Done<()> {
            self.merge_refused(merge)?;
            if self.services.contains_key(name) {
                return if merge {
                    Ok(())
                } else {
                    Err(Failure::Occupied)
                };
            }
            self.services.insert(name.to_string(), key.clone());
            Ok(())
        }
        fn export_task(&mut self, path: &str) -> Done<String> {
            self.tasks.get(path).cloned().ok_or(Failure::Missing)
        }
        fn delete_task(&mut self, path: &str) -> Done<()> {
            self.tasks.remove(path).map(|_| ()).ok_or(Failure::Missing)
        }
        fn restore_task(&mut self, path: &str, xml: &str, merge: bool) -> Done<()> {
            self.merge_refused(merge)?;
            if self.tasks.contains_key(path) {
                return if merge {
                    Ok(())
                } else {
                    Err(Failure::Occupied)
                };
            }
            self.tasks.insert(path.to_string(), xml.to_string());
            Ok(())
        }
        fn export_firewall_rule(&mut self, id: &str) -> Done<Vec<(String, String)>> {
            match self.firewall.get(id) {
                Some(rule) => Ok(vec![(id.to_string(), rule.clone())]),
                None => Err(Failure::Missing),
            }
        }
        fn delete_firewall_rule(&mut self, _: &str, rules: &[(String, String)]) -> Done<()> {
            if self.delete_firewall_fails {
                return Err(Failure::Failed("Access is denied.".into()));
            }
            for (id, _) in rules {
                self.firewall.remove(id);
            }
            Ok(())
        }
        fn restore_firewall_rule(
            &mut self,
            _: &str,
            rules: &[(String, String)],
            merge: bool,
        ) -> Done<()> {
            self.merge_refused(merge)?;
            if !merge && rules.iter().any(|(id, _)| self.firewall.contains_key(id)) {
                return Err(Failure::Occupied);
            }
            for (id, rule) in rules {
                self.firewall
                    .entry(id.clone())
                    .or_insert_with(|| rule.clone());
            }
            Ok(())
        }
    }

    fn value(ty: u32, data: &[u8]) -> RegValue {
        RegValue {
            ty,
            data: data.to_vec(),
        }
    }

    fn wide(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    /// One value of every type, the default value among them.
    fn every_type() -> Vec<(&'static str, RegValue)> {
        let mut multi = wide("one");
        multi.extend(wide("two"));
        multi.extend([0, 0]);
        vec![
            ("", value(REG_SZ, &wide("default"))),
            ("Path", value(REG_SZ, &wide(r"C:\Acme"))),
            (
                "Expand",
                value(REG_EXPAND_SZ, &wide(r"%ProgramFiles%\Acme")),
            ),
            ("Multi", value(REG_MULTI_SZ, &multi)),
            ("Count", value(REG_DWORD, &7u32.to_le_bytes())),
            ("Big", value(REG_QWORD, &u64::MAX.to_le_bytes())),
            ("Blob", value(REG_BINARY, &[0, 1, 0xfe, 0xff])),
            ("Nothing", value(REG_NONE, &[])),
            ("Odd", value(REG_NONE, &[9, 9, 9])),
        ]
    }

    fn item(kind: ItemKind, location: impl Into<String>) -> HoldItem {
        HoldItem {
            kind,
            location: location.into(),
        }
    }

    fn file(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn text(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    fn loc(path: &Path) -> String {
        path.display().to_string()
    }

    struct Machine {
        _temp: tempfile::TempDir,
        disk: PathBuf,
        root: PathBuf,
    }

    /// A temporary disk with a file and a folder tree on it, and a holding
    /// root beside it.
    fn machine() -> Machine {
        let temp = tempfile::tempdir().unwrap();
        let disk = temp.path().join("disk");
        file(&disk.join("notes.txt"), "hello");
        file(&disk.join("App").join("a.dat"), "a");
        file(&disk.join("App").join("sub").join("b.dat"), "bb");
        let root = temp.path().join("holding");
        Machine {
            disk,
            root,
            _temp: temp,
        }
    }

    fn set_created(root: &Path, id: &str, created: i64) {
        let dir = root.join(id);
        let mut manifest = read_manifest(&dir).unwrap().unwrap();
        manifest.created = rfc3339(created);
        save(&dir, &manifest).unwrap();
    }

    #[test]
    fn hold_then_restore_round_trips() {
        let mc = machine();
        let mut m = Fake::default();
        m.put_key(Hive::Hkcu, r"Software\Acme", every_type());
        m.put_key(
            Hive::Hkcu,
            r"Software\Acme\Sub",
            vec![("x", value(REG_SZ, &wide("y")))],
        );
        let run = r"Software\Microsoft\Windows\CurrentVersion\Run";
        m.put_key(
            Hive::Hkcu,
            run,
            vec![
                ("Acme", value(REG_SZ, &wide("acme.exe"))),
                ("Other", value(REG_SZ, &wide("other.exe"))),
            ],
        );
        m.services.insert(
            "AcmeSvc".into(),
            KeyNode {
                values: vec![("Start".into(), value(REG_DWORD, &2u32.to_le_bytes()))],
                subkeys: vec![("Parameters".into(), KeyNode::default())],
            },
        );
        m.tasks
            .insert(r"\Acme\Update".into(), "<Task>update</Task>".into());
        m.firewall.insert(
            "{1}".into(),
            r"v2.30|Action=Allow|App=C:\Acme\acme.exe|Name=Acme|".into(),
        );
        m.firewall
            .insert("{2}".into(), "v2.30|Action=Allow|Name=Other|".into());
        // Another application's rule by the same name stays.
        m.firewall.insert(
            "{3}".into(),
            r"v2.30|Action=Allow|App=C:\Other\acme.exe|Name=Acme|".into(),
        );
        let before = m.state();

        let items = vec![
            item(ItemKind::File, loc(&mc.disk.join("notes.txt"))),
            item(ItemKind::Folder, loc(&mc.disk.join("App"))),
            item(ItemKind::RegistryKey, r"HKCU\Software\Acme"),
            item(ItemKind::RegistryValue, format!(r"HKCU\{run} | Acme")),
            item(ItemKind::Service, "service:AcmeSvc"),
            item(ItemKind::Task, r"task:\Acme\Update"),
            item(ItemKind::FirewallRule, "firewall:Acme | {1}"),
        ];
        let report = hold(&mc.root, "acme-1", "Acme", &items, &mut m);
        assert_eq!(
            report,
            HoldReport {
                held: 7,
                failed: vec![]
            }
        );

        assert!(!mc.disk.join("notes.txt").exists());
        assert!(!mc.disk.join("App").exists());
        assert!(!m.keys.contains_key(&Fake::id(Hive::Hkcu, r"Software\Acme")));
        assert!(
            !m.keys
                .contains_key(&Fake::id(Hive::Hkcu, r"Software\Acme\Sub"))
        );
        let run_values = &m.keys[&Fake::id(Hive::Hkcu, run)].1;
        assert_eq!(run_values.len(), 1);
        assert_eq!(run_values[0].0, "Other");
        assert!(m.services.is_empty() && m.tasks.is_empty());
        assert_eq!(m.firewall.keys().collect::<Vec<_>>(), vec!["{2}", "{3}"]);

        let dir = mc.root.join("acme-1");
        let manifest = read_manifest(&dir).unwrap().unwrap();
        assert_eq!(manifest.id, "acme-1");
        assert_eq!(manifest.name, "Acme");
        assert!(
            created_seconds(&manifest.created).is_some(),
            "{}",
            manifest.created
        );
        assert_eq!(manifest.items.len(), 7);
        assert!(manifest.items.iter().all(|h| h.state == HeldState::Held));
        assert_eq!(manifest.items[1].size, 3);
        let stored = dir.join(&manifest.items[1].stored);
        assert_eq!(text(&stored.join("sub").join("b.dat")), "bb");

        let report = restore(&mc.root, "acme-1", &mut m);
        assert_eq!(
            report,
            HoldReport {
                held: 7,
                failed: vec![]
            }
        );
        assert_eq!(text(&mc.disk.join("notes.txt")), "hello");
        assert_eq!(text(&mc.disk.join("App").join("a.dat")), "a");
        assert_eq!(text(&mc.disk.join("App").join("sub").join("b.dat")), "bb");
        assert_eq!(m.state(), before);
        assert!(!dir.exists(), "a restored entry is removed");
    }

    #[test]
    fn every_value_type_survives_the_export_file() {
        let node = KeyNode {
            values: every_type()
                .into_iter()
                .map(|(n, v)| (n.to_string(), v))
                .collect(),
            subkeys: vec![("Sub".into(), KeyNode::default())],
        };
        let text = serde_json::to_string(&node).unwrap();
        assert_eq!(serde_json::from_str::<KeyNode>(&text).unwrap(), node);
        assert!(serde_json::from_str::<RegValue>(r#"{"ty":3,"data":"éé"}"#).is_err());
    }

    #[test]
    fn a_locked_file_is_named_and_everything_else_is_kept_restorable() {
        let mc = machine();
        file(&mc.disk.join("running.log"), "busy");
        let mut m = Fake {
            locked: vec![mc.disk.join("running.log")],
            ..Fake::default()
        };
        m.put_key(Hive::Hkcu, r"Software\Acme", every_type());
        let items = vec![
            item(ItemKind::File, loc(&mc.disk.join("notes.txt"))),
            item(ItemKind::File, loc(&mc.disk.join("running.log"))),
            item(ItemKind::Folder, loc(&mc.disk.join("App"))),
            item(ItemKind::RegistryKey, r"HKCU\Software\Acme"),
        ];
        let report = hold(&mc.root, "acme-1", "Acme", &items, &mut m);
        assert_eq!(report.held, 3);
        assert_eq!(report.failed.len(), 1);
        let (location, sentence) = &report.failed[0];
        assert_eq!(*location, loc(&mc.disk.join("running.log")));
        assert!(
            sentence.contains(location.as_str()) && sentence.contains("in use"),
            "{sentence}"
        );
        assert_eq!(
            text(&mc.disk.join("running.log")),
            "busy",
            "the locked file is untouched"
        );

        let manifest = read_manifest(&mc.root.join("acme-1")).unwrap().unwrap();
        let recorded: Vec<_> = manifest
            .items
            .iter()
            .map(|h| h.item.location.clone())
            .collect();
        assert_eq!(
            recorded,
            vec![
                loc(&mc.disk.join("notes.txt")),
                loc(&mc.disk.join("App")),
                r"HKCU\Software\Acme".to_string(),
            ]
        );

        let report = restore(&mc.root, "acme-1", &mut m);
        assert_eq!(
            report,
            HoldReport {
                held: 3,
                failed: vec![]
            }
        );
        assert_eq!(text(&mc.disk.join("notes.txt")), "hello");
        assert_eq!(text(&mc.disk.join("App").join("sub").join("b.dat")), "bb");
        assert!(m.keys.contains_key(&Fake::id(Hive::Hkcu, r"Software\Acme")));
    }

    #[test]
    fn restore_refuses_where_something_new_is_there() {
        let mc = machine();
        let mut m = Fake::default();
        m.put_key(Hive::Hkcu, r"Software\Acme", every_type());
        let items = vec![
            item(ItemKind::File, loc(&mc.disk.join("notes.txt"))),
            item(ItemKind::Folder, loc(&mc.disk.join("App"))),
            item(ItemKind::RegistryKey, r"HKCU\Software\Acme"),
        ];
        assert_eq!(hold(&mc.root, "acme-1", "Acme", &items, &mut m).held, 3);
        // The application was installed again in the meantime.
        file(&mc.disk.join("notes.txt"), "new");
        m.put_key(
            Hive::Hkcu,
            r"Software\Acme",
            vec![("New", value(REG_DWORD, &[1, 0, 0, 0]))],
        );

        let report = restore(&mc.root, "acme-1", &mut m);
        assert_eq!(report.held, 1);
        let refused: Vec<_> = report.failed.iter().map(|(l, _)| l.clone()).collect();
        assert_eq!(
            refused,
            vec![
                r"HKCU\Software\Acme".to_string(),
                loc(&mc.disk.join("notes.txt"))
            ]
        );
        for (location, sentence) in &report.failed {
            assert!(sentence.contains(location.as_str()), "{sentence}");
            assert!(sentence.ends_with('.'), "{sentence}");
        }
        assert_eq!(
            text(&mc.disk.join("notes.txt")),
            "new",
            "what is there now is left alone"
        );
        assert_eq!(text(&mc.disk.join("App").join("a.dat")), "a");
        assert_eq!(m.keys[&Fake::id(Hive::Hkcu, r"Software\Acme")].1.len(), 1);

        let manifest = read_manifest(&mc.root.join("acme-1")).unwrap().unwrap();
        assert_eq!(manifest.items.len(), 2, "the refused items stay held");
        let notes = &manifest.items[0];
        assert_eq!(text(&mc.root.join("acme-1").join(&notes.stored)), "hello");
    }

    #[test]
    fn empty_older_than_30_days_leaves_newer_entries() {
        let mc = machine();
        let mut m = Fake::default();
        let notes = vec![item(ItemKind::File, loc(&mc.disk.join("notes.txt")))];
        let app = vec![item(ItemKind::Folder, loc(&mc.disk.join("App")))];
        assert_eq!(hold(&mc.root, "old", "Old", &notes, &mut m).held, 1);
        assert_eq!(hold(&mc.root, "new", "New", &app, &mut m).held, 1);
        set_created(&mc.root, "old", now() - 31 * DAY);
        set_created(&mc.root, "new", now() - 29 * DAY);

        let report = empty(&mc.root, None, Some(30));
        assert_eq!(
            report,
            HoldReport {
                held: 1,
                failed: vec![]
            }
        );
        assert!(!mc.root.join("old").exists());
        assert!(mc.root.join("new").exists());

        let report = empty(&mc.root, Some("new"), None);
        assert_eq!(
            report,
            HoldReport {
                held: 1,
                failed: vec![]
            }
        );
        assert!(!mc.root.join("new").exists());
        assert!(
            !mc.disk.join("App").exists(),
            "emptying never puts anything back"
        );
    }

    #[test]
    fn list_marks_old_machine_entries_waiting() {
        let mc = machine();
        let machine_root = mc.disk.parent().unwrap().join("machine-holding");
        let mut m = Fake::default();
        file(&mc.disk.join("one"), "1");
        file(&mc.disk.join("two"), "22");
        file(&mc.disk.join("three"), "333");
        let one = |name: &str| vec![item(ItemKind::File, loc(&mc.disk.join(name)))];
        hold(&mc.root, "user-old", "User old", &one("notes.txt"), &mut m);
        hold(&mc.root, "user-new", "User new", &one("one"), &mut m);
        hold(
            &machine_root,
            "machine-old",
            "Machine old",
            &one("two"),
            &mut m,
        );
        hold(
            &machine_root,
            "machine-new",
            "Machine new",
            &one("three"),
            &mut m,
        );
        set_created(&mc.root, "user-old", now() - 40 * DAY);
        set_created(&mc.root, "user-new", now() - 3 * DAY);
        set_created(&machine_root, "machine-old", now() - 31 * DAY);
        set_created(&machine_root, "machine-new", now() - 2 * DAY);

        let entries = list(&mc.root, &machine_root).entries;
        let shown: Vec<_> = entries
            .iter()
            .map(|e| (e.id.as_str(), e.scope_machine, e.waiting, e.size, e.items))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("machine-new", true, false, 3, 1),
                ("user-new", false, false, 1, 1),
                ("machine-old", true, true, 2, 1),
                // A user entry is emptied at start-up, never waiting.
                ("user-old", false, false, 5, 1),
            ]
        );
        assert_eq!(entries[0].name, "Machine new");
        let newest = read_manifest(&machine_root.join("machine-new"))
            .unwrap()
            .unwrap();
        assert_eq!(entries[0].created, newest.created);
    }

    #[test]
    fn an_item_inside_a_folder_already_held_counts_as_held_and_goes_back_with_it() {
        let mc = machine();
        let mut m = Fake::default();
        m.put_key(Hive::Hkcu, r"Software\Acme", every_type());
        m.put_key(
            Hive::Hkcu,
            r"Software\Acme\Sub",
            vec![("x", value(REG_SZ, &wide("y")))],
        );
        let items = vec![
            item(ItemKind::Folder, loc(&mc.disk.join("App"))),
            item(
                ItemKind::File,
                loc(&mc.disk.join("App").join("sub").join("b.dat")),
            ),
            item(ItemKind::RegistryKey, r"HKCU\Software\Acme"),
            item(ItemKind::RegistryKey, r"HKCU\Software\Acme\Sub"),
            item(ItemKind::RegistryValue, r"HKCU\Software\Acme | Path"),
        ];
        let report = hold(&mc.root, "acme-1", "Acme", &items, &mut m);
        assert_eq!(
            report,
            HoldReport {
                held: 5,
                failed: vec![]
            }
        );
        let manifest = read_manifest(&mc.root.join("acme-1")).unwrap().unwrap();
        let states: Vec<_> = manifest
            .items
            .iter()
            .map(|h| (h.state, h.covered_by.clone()))
            .collect();
        assert_eq!(
            states,
            vec![
                (HeldState::Held, None),
                (HeldState::Covered, Some(loc(&mc.disk.join("App")))),
                (HeldState::Held, None),
                (HeldState::Covered, Some(r"HKCU\Software\Acme".into())),
                (HeldState::Covered, Some(r"HKCU\Software\Acme".into())),
            ]
        );

        let report = restore(&mc.root, "acme-1", &mut m);
        assert_eq!(
            report,
            HoldReport {
                held: 5,
                failed: vec![]
            }
        );
        assert_eq!(text(&mc.disk.join("App").join("sub").join("b.dat")), "bb");
        assert_eq!(m.keys.len(), 2);
        assert!(!mc.root.join("acme-1").exists());
    }

    #[test]
    fn a_covered_item_stays_when_its_folder_is_refused() {
        let mc = machine();
        let mut m = Fake::default();
        let items = vec![
            item(ItemKind::Folder, loc(&mc.disk.join("App"))),
            item(ItemKind::File, loc(&mc.disk.join("App").join("a.dat"))),
        ];
        hold(&mc.root, "acme-1", "Acme", &items, &mut m);
        file(&mc.disk.join("App").join("new.dat"), "new");
        let report = restore(&mc.root, "acme-1", &mut m);
        assert_eq!(report.held, 0);
        assert_eq!(report.failed.len(), 1);
        let manifest = read_manifest(&mc.root.join("acme-1")).unwrap().unwrap();
        assert_eq!(manifest.items.len(), 2);
    }

    #[test]
    fn a_copy_across_drives_round_trips() {
        let mc = machine();
        let mut m = Fake {
            force_copy: true,
            ..Fake::default()
        };
        let items = vec![
            item(ItemKind::Folder, loc(&mc.disk.join("App"))),
            item(ItemKind::File, loc(&mc.disk.join("notes.txt"))),
        ];
        assert_eq!(hold(&mc.root, "acme-1", "Acme", &items, &mut m).held, 2);
        assert!(!mc.disk.join("App").exists());
        assert!(!mc.disk.join("notes.txt").exists());
        assert_eq!(restore(&mc.root, "acme-1", &mut m).held, 2);
        assert_eq!(text(&mc.disk.join("App").join("sub").join("b.dat")), "bb");
        assert_eq!(text(&mc.disk.join("notes.txt")), "hello");
    }

    /// A junction, made the way a user without privileges can.
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
    fn a_copy_across_drives_refuses_a_folder_with_a_junction_inside() {
        let mc = machine();
        let outside = mc.disk.parent().unwrap().join("outside");
        file(&outside.join("secret.txt"), "keep");
        let link = mc.disk.join("App").join("sub").join("link");
        junction(&link, &outside);
        let mut m = Fake {
            force_copy: true,
            ..Fake::default()
        };
        let items = vec![item(ItemKind::Folder, loc(&mc.disk.join("App")))];
        let report = hold(&mc.root, "acme-1", "Acme", &items, &mut m);
        assert_eq!(report.held, 0);
        let (location, sentence) = &report.failed[0];
        assert_eq!(*location, loc(&mc.disk.join("App")));
        assert!(sentence.contains("link"), "{sentence}");
        assert!(sentence.contains(&loc(&link)), "{sentence}");
        assert_eq!(
            text(&mc.disk.join("App").join("sub").join("b.dat")),
            "bb",
            "nothing was moved"
        );
        assert_eq!(text(&link.join("secret.txt")), "keep");
        assert_eq!(text(&outside.join("secret.txt")), "keep");
        assert!(
            !mc.root.join("acme-1").exists(),
            "an entry with nothing in it is not kept"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_move_on_one_drive_takes_a_junction_as_a_link_and_leaves_its_target() {
        let mc = machine();
        let outside = mc.disk.parent().unwrap().join("outside");
        file(&outside.join("secret.txt"), "keep");
        junction(&mc.disk.join("App").join("link"), &outside);
        let mut m = Fake::default();
        let items = vec![item(ItemKind::Folder, loc(&mc.disk.join("App")))];
        assert_eq!(hold(&mc.root, "acme-1", "Acme", &items, &mut m).held, 1);
        assert_eq!(text(&outside.join("secret.txt")), "keep");
        assert_eq!(empty(&mc.root, Some("acme-1"), None).held, 1);
        assert_eq!(
            text(&outside.join("secret.txt")),
            "keep",
            "emptying removes the link, not what it leads to"
        );
    }

    #[test]
    fn cloud_placeholders_are_recognised_by_their_attributes() {
        assert!(is_cloud_attributes(0x0040_0000));
        assert!(is_cloud_attributes(0x0004_0000 | 0x20));
        assert!(!is_cloud_attributes(0x20 | 0x10 | 0x400));
    }

    #[test]
    fn an_item_recorded_but_never_moved_is_dropped_and_one_moved_is_put_back() {
        // What a crash between the two manifest writes leaves.
        let mc = machine();
        let dir = mc.root.join("acme-1");
        fs::create_dir_all(dir.join(ITEMS)).unwrap();
        fs::rename(mc.disk.join("App"), dir.join(ITEMS).join("1")).unwrap();
        let moving = |location: &Path, stored: &str| Held {
            item: item(ItemKind::Folder, loc(location)),
            stored: stored.into(),
            state: HeldState::Moving,
            covered_by: None,
            size: 0,
        };
        let manifest = Manifest {
            id: "acme-1".into(),
            name: "Acme".into(),
            created: rfc3339(now()),
            items: vec![
                moving(&mc.disk.join("App"), "items/1"),
                moving(&mc.disk.join("notes.txt"), "items/2"),
            ],
            next: 3,
        };
        save(&dir, &manifest).unwrap();
        let report = restore(&mc.root, "acme-1", &mut Fake::default());
        assert_eq!(
            report,
            HoldReport {
                held: 1,
                failed: vec![]
            }
        );
        assert_eq!(text(&mc.disk.join("App").join("a.dat")), "a");
        assert_eq!(text(&mc.disk.join("notes.txt")), "hello");
        assert!(!dir.exists());
    }

    #[test]
    fn only_ids_of_the_agreed_shape_are_used() {
        assert!(valid_id("discord-20261124-1"));
        assert!(valid_id(&"a".repeat(64)));
        for bad in ["", "Acme", "a/b", r"..\x", "a b", "a_b", &"a".repeat(65)] {
            assert!(!valid_id(bad), "{bad}");
        }
        let mc = machine();
        let items = vec![item(ItemKind::File, loc(&mc.disk.join("notes.txt")))];
        let report = hold(&mc.root, "..", "Acme", &items, &mut Fake::default());
        assert_eq!(report.held, 0);
        assert_eq!(report.failed.len(), 1);
        assert!(mc.disk.join("notes.txt").exists());
        assert_eq!(restore(&mc.root, "..", &mut Fake::default()).held, 0);
        assert_eq!(empty(&mc.root, Some(".."), None).held, 0);
    }

    #[test]
    fn created_is_rfc_3339_in_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_868_800), "2000-03-01T00:00:00Z");
        assert_eq!(rfc3339(1_795_523_445), "2026-11-24T12:30:45Z");
        assert_eq!(
            created_seconds(&rfc3339(1_795_523_445)),
            Some(1_795_523_445)
        );
    }

    fn acme_key(m: &mut Fake) {
        m.put_key(Hive::Hkcu, r"Software\Acme", every_type());
        m.put_key(
            Hive::Hkcu,
            r"Software\Acme\Sub",
            vec![("x", value(REG_SZ, &wide("y")))],
        );
        m.put_key(
            Hive::Hkcu,
            r"Software\Acme\Sub\Deeper",
            vec![("z", value(REG_DWORD, &[1, 0, 0, 0]))],
        );
    }

    #[test]
    fn a_key_deleted_in_part_is_put_back_whole_and_named() {
        let mc = machine();
        let mut m = Fake {
            delete_key_fails_part_way: true,
            ..Fake::default()
        };
        acme_key(&mut m);
        let before = m.state();
        let items = vec![item(ItemKind::RegistryKey, r"HKCU\Software\Acme")];
        let report = hold(&mc.root, "acme-1", "Acme", &items, &mut m);
        assert_eq!(report.held, 0);
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert_eq!(
            m.state(),
            before,
            "what went is back, and nothing else changed"
        );
        assert!(
            !mc.root.join("acme-1").exists(),
            "nothing is recorded as held"
        );
    }

    #[test]
    fn a_firewall_rule_that_will_not_go_is_left_with_its_namesake() {
        let mc = machine();
        let mut m = Fake {
            delete_firewall_fails: true,
            ..Fake::default()
        };
        m.firewall
            .insert("{1}".into(), "v2.30|Action=Allow|Name=Acme|".into());
        m.firewall
            .insert("{2}".into(), "v2.30|Action=Block|Name=Acme|".into());
        let before = m.state();
        let items = vec![item(ItemKind::FirewallRule, "firewall:Acme | {1}")];
        let report = hold(&mc.root, "acme-1", "Acme", &items, &mut m);
        assert_eq!(report.held, 0);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(m.state(), before);
        assert!(!mc.root.join("acme-1").exists());
    }

    #[test]
    fn a_delete_in_part_that_cannot_be_put_back_keeps_the_copy_as_moved_in_part() {
        let mc = machine();
        let mut m = Fake {
            delete_key_fails_part_way: true,
            merge_fails: true,
            ..Fake::default()
        };
        acme_key(&mut m);
        let before = m.state();
        let items = vec![item(ItemKind::RegistryKey, r"HKCU\Software\Acme")];
        let report = hold(&mc.root, "acme-1", "Acme", &items, &mut m);
        assert_eq!(report.held, 0);
        let (location, sentence) = &report.failed[0];
        assert_eq!(location, r"HKCU\Software\Acme");
        assert!(
            sentence.contains("in part") && sentence.contains("Restore"),
            "{sentence}"
        );
        assert_ne!(m.state(), before, "the deepest subkey is gone");

        let dir = mc.root.join("acme-1");
        let manifest = read_manifest(&dir).unwrap().unwrap();
        assert_eq!(manifest.items.len(), 1);
        assert_eq!(manifest.items[0].state, HeldState::Partial);
        assert!(stored_path(&dir, &manifest.items[0].stored).exists());

        // Never emptied automatically, however old: it is the only copy.
        set_created(&mc.root, "acme-1", now() - 90 * DAY);
        assert_eq!(empty(&mc.root, None, Some(30)).held, 0);
        assert!(dir.exists());
        let listed = list(&mc.disk, &mc.root);
        assert!(!listed.entries[0].waiting, "{listed:?}");

        // What the sentence says works: restore puts it back together.
        m.merge_fails = false;
        m.delete_key_fails_part_way = false;
        let report = restore(&mc.root, "acme-1", &mut m);
        assert_eq!(
            report,
            HoldReport {
                held: 1,
                failed: vec![]
            }
        );
        assert_eq!(m.state(), before);
        assert!(!dir.exists());
    }

    #[test]
    fn a_crash_between_export_and_delete_is_recovered_by_merging() {
        let mc = machine();
        let mut m = Fake::default();
        acme_key(&mut m);
        let before = m.state();
        let whole = m.export_key(Hive::Hkcu, r"Software\Acme").unwrap();
        // The export was written and the delete had removed one subkey
        // when the helper stopped.
        let dir = mc.root.join("acme-1");
        fs::create_dir_all(dir.join(ITEMS)).unwrap();
        fs::write(
            dir.join(ITEMS).join("1.json"),
            serde_json::to_vec(&whole).unwrap(),
        )
        .unwrap();
        m.keys
            .remove(&Fake::id(Hive::Hkcu, r"Software\Acme\Sub\Deeper"));
        let manifest = Manifest {
            id: "acme-1".into(),
            name: "Acme".into(),
            created: rfc3339(now()),
            items: vec![Held {
                item: item(ItemKind::RegistryKey, r"HKCU\Software\Acme"),
                stored: "items/1.json".into(),
                state: HeldState::Moving,
                covered_by: None,
                size: 0,
            }],
            next: 2,
        };
        save(&dir, &manifest).unwrap();
        let report = restore(&mc.root, "acme-1", &mut m);
        assert_eq!(
            report,
            HoldReport {
                held: 1,
                failed: vec![]
            }
        );
        assert_eq!(m.state(), before);
    }

    #[test]
    fn a_folder_moved_in_part_is_put_back_together_and_nothing_there_is_replaced() {
        let mc = machine();
        let app = mc.disk.join("App");
        let dir = mc.root.join("acme-1");
        // The holding area has the whole folder; in its place, a.dat is still
        // there (and changed since) and sub\b.dat is gone.
        fs::create_dir_all(dir.join(ITEMS)).unwrap();
        copy_tree(&app, &dir.join(ITEMS).join("1"));
        fs::write(app.join("a.dat"), "changed").unwrap();
        fs::remove_file(app.join("sub").join("b.dat")).unwrap();
        let manifest = Manifest {
            id: "acme-1".into(),
            name: "Acme".into(),
            created: rfc3339(now()),
            items: vec![Held {
                item: item(ItemKind::Folder, loc(&app)),
                stored: "items/1".into(),
                state: HeldState::Partial,
                covered_by: None,
                size: 3,
            }],
            next: 2,
        };
        save(&dir, &manifest).unwrap();
        let report = restore(&mc.root, "acme-1", &mut Fake::default());
        assert_eq!(
            report,
            HoldReport {
                held: 1,
                failed: vec![]
            }
        );
        assert_eq!(text(&app.join("a.dat")), "changed");
        assert_eq!(text(&app.join("sub").join("b.dat")), "bb");
        assert!(!dir.exists());
    }

    fn copy_tree(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap().flatten() {
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    #[test]
    fn a_half_copy_left_by_a_failed_move_is_never_recorded() {
        let mc = machine();
        let app = mc.disk.join("App");
        let mut m = Fake {
            half_copied: vec![app.clone()],
            ..Fake::default()
        };
        let items = vec![item(ItemKind::Folder, loc(&app))];
        let report = hold(&mc.root, "acme-1", "Acme", &items, &mut m);
        assert_eq!(report.held, 0);
        assert_eq!(report.failed.len(), 1);
        assert!(
            !mc.root.join("acme-1").exists(),
            "no record, and no half copy"
        );
        assert_eq!(text(&app.join("a.dat")), "a");
    }

    #[cfg(windows)]
    #[test]
    fn a_holding_root_that_cannot_be_read_is_reported_not_shown_empty() {
        let mc = machine();
        let notes = vec![item(ItemKind::File, loc(&mc.disk.join("notes.txt")))];
        hold(&mc.root, "acme-1", "Acme", &notes, &mut Fake::default());
        windows::tests::set_dacl(&mc.root, "D:P");
        let listed = list(&mc.disk.join("nothing"), &mc.root);
        let emptied = empty(&mc.root, None, Some(30));
        windows::tests::set_dacl(&mc.root, "D:P(A;OICI;FA;;;WD)");
        assert!(listed.entries.is_empty());
        assert_eq!(listed.unreadable, vec![loc(&mc.root)]);
        assert_eq!(emptied.held, 0);
        assert_eq!(emptied.failed.len(), 1, "{emptied:?}");
    }

    /// Plain `std::fs`, and a file put into the folder being moved once the
    /// copy is made and before anything is removed.
    struct Meanwhile(PathBuf);

    impl Disk for Meanwhile {
        fn release(&mut self) {
            file(&self.0, "new");
        }
    }

    /// A move across drives removes what it looked at and copied, deepest
    /// first, and nothing else: what was put in the folder since stays,
    /// with the folders it is in, and the item is held in part.
    #[test]
    fn a_copy_across_drives_leaves_what_appeared_after_it_looked() {
        let mc = machine();
        let app = mc.disk.join("App");
        let to = mc.root.join("moved");
        let new = app.join("sub").join("new.txt");
        let got = move_with(&app, &to, true, &mut Meanwhile(new.clone()));
        let Err(Failure::Partial(said)) = got else {
            panic!("held in part, not {got:?}");
        };
        assert!(said.contains("after Brokey looked"), "{said}");
        assert_eq!(text(&new), "new");
        assert!(!app.join("a.dat").exists() && !app.join("sub").join("b.dat").exists());
        assert_eq!(text(&to.join("a.dat")), "a");
        assert_eq!(text(&to.join("sub").join("b.dat")), "bb");
        assert!(!to.join("sub").join("new.txt").exists());
    }

    /// Plain `std::fs`, and a surveyed file deleted by someone else once the
    /// copy is made and before anything is removed.
    struct GoneMeanwhile(PathBuf);

    impl Disk for GoneMeanwhile {
        fn release(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    /// What has gone by itself after the copy needs no removing: the move
    /// still succeeds, and the copy has it.
    #[test]
    fn a_file_gone_after_the_copy_does_not_fail_the_move() {
        let mc = machine();
        let app = mc.disk.join("App");
        let to = mc.root.join("moved");
        let got = move_with(
            &app,
            &to,
            true,
            &mut GoneMeanwhile(app.join("sub").join("b.dat")),
        );
        assert!(got.is_ok(), "{got:?}");
        assert!(!app.exists());
        assert_eq!(text(&to.join("sub").join("b.dat")), "bb");
    }

    /// A service that runs as an account with a password, or a task saved
    /// to run whether the user is signed in or not, could not be put back
    /// without that password, so neither is taken; what can be put back is.
    #[test]
    fn what_could_not_be_put_back_without_a_password_is_never_taken() {
        let mc = machine();
        let mut m = Fake::default();
        let utf16 = |text: &str| -> Vec<u8> {
            text.encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect()
        };
        let service = |account: &str| KeyNode {
            values: vec![
                ("Start".into(), value(REG_DWORD, &2u32.to_le_bytes())),
                ("ObjectName".into(), value(REG_SZ, &utf16(account))),
            ],
            subkeys: Vec::new(),
        };
        m.services
            .insert("AcmeUser".into(), service(r".\acme-user"));
        m.services
            .insert("AcmeSystem".into(), service("LocalSystem"));
        let task = |logon: &str| {
            format!(
                "<Task><Principals><Principal><LogonType>{logon}</LogonType></Principal></Principals></Task>"
            )
        };
        m.tasks.insert(r"\Acme\Nightly".into(), task("Password"));
        m.tasks
            .insert(r"\Acme\Update".into(), task("InteractiveToken"));
        let items = vec![
            item(ItemKind::Service, "service:AcmeUser"),
            item(ItemKind::Service, "service:AcmeSystem"),
            item(ItemKind::Task, r"task:\Acme\Nightly"),
            item(ItemKind::Task, r"task:\Acme\Update"),
        ];
        let report = hold(&mc.root, "acme-1", "Acme", &items, &mut m);
        assert_eq!(report.held, 2, "{report:?}");
        let failed: Vec<&str> = report.failed.iter().map(|(_, why)| why.as_str()).collect();
        assert_eq!(
            failed,
            vec![
                r"service:AcmeUser was left where it is, because it runs as .\acme-user, and Windows would need that account's password to put it back.",
                r"task:\Acme\Nightly was left where it is, because it runs whether you are signed in or not, and Windows would need its account's password to put it back.",
            ]
        );
        assert_eq!(m.services.keys().collect::<Vec<_>>(), vec!["AcmeUser"]);
        assert_eq!(m.tasks.keys().collect::<Vec<_>>(), vec![r"\Acme\Nightly"]);
    }

    /// A hard link is refused for good, so its sentence never says to try
    /// again; any other failure still does.
    #[test]
    fn a_hard_link_has_a_sentence_of_its_own() {
        let link = io::Error::other(HardLink(r"C:\Acme\linked.dll".into()));
        let said = io_sentence(r"C:\Acme", &link);
        assert_eq!(
            said,
            r"C:\Acme was left where it is, because C:\Acme\linked.dll also exists under another name elsewhere on the drive, and Brokey never moves a file like that. Delete it yourself if you no longer want it."
        );
        let other = io::Error::other("the disk is full");
        assert!(io_sentence(r"C:\Acme", &other).ends_with("Try again."));
    }

    #[test]
    fn locations_are_read_in_the_scan_format() {
        let p = |kind, l: &str| Place::parse(&item(kind, l));
        assert_eq!(
            p(ItemKind::RegistryKey, r"HKLM32\Software\Acme"),
            Some(Place::Key(Hive::Hklm32, r"Software\Acme".into()))
        );
        assert_eq!(
            p(ItemKind::RegistryValue, r"HKCU\Software\X | Last used"),
            Some(Place::Value(
                Hive::Hkcu,
                r"Software\X".into(),
                "Last used".into()
            ))
        );
        assert_eq!(
            p(ItemKind::Service, "service:AcmeSvc"),
            Some(Place::Service("AcmeSvc".into()))
        );
        assert_eq!(
            p(ItemKind::Task, r"task:\Acme\U"),
            Some(Place::Task(r"\Acme\U".into()))
        );
        assert_eq!(
            p(ItemKind::FirewallRule, "firewall:Acme | {1}"),
            Some(Place::Firewall("{1}".into()))
        );
        // A name may hold the separator; the id is after the last one.
        assert_eq!(
            p(ItemKind::FirewallRule, "firewall:A | B | TCP Query User{2}"),
            Some(Place::Firewall("TCP Query User{2}".into()))
        );
        // By its name alone a rule is never held.
        assert_eq!(p(ItemKind::FirewallRule, "firewall:Acme"), None);
        assert_eq!(p(ItemKind::FirewallRule, "firewall:Acme | "), None);
        assert_eq!(p(ItemKind::RegistryKey, "HKCU"), None);
        assert_eq!(p(ItemKind::RegistryKey, r"HKCR\.txt"), None);
        assert_eq!(p(ItemKind::Folder, r"relative\path"), None);
        assert_eq!(p(ItemKind::Service, "service:"), None);
        // The 32-bit view however it is spelt.
        let held = Place::Key(Hive::Hklm, r"SOFTWARE\WOW6432Node\Acme".into());
        assert!(held.covers(&Place::Key(Hive::Hklm32, r"Software\Acme\Sub".into())));
        assert!(!held.covers(&Place::Key(Hive::Hklm32, r"Software\AcmeOther".into())));
    }
    /// Something already in the place an item would be stored (left by a
    /// crash before the record was written, or put there since) is never
    /// deleted or written over: the item goes to a fresh place.
    #[test]
    fn a_stored_place_already_taken_is_kept_and_the_item_goes_elsewhere() {
        let mc = machine();
        let mut m = Fake::default();
        m.put_key(Hive::Hkcu, r"Software\Acme", every_type());
        let dir = mc.root.join("acme-1");
        file(&dir.join(ITEMS).join("1"), "precious");
        file(&dir.join(ITEMS).join("2.json"), "also precious");
        let items = vec![
            item(ItemKind::File, loc(&mc.disk.join("notes.txt"))),
            item(ItemKind::RegistryKey, r"HKCU\Software\Acme"),
        ];
        let report = hold(&mc.root, "acme-1", "Acme", &items, &mut m);
        assert_eq!(report.held, 2, "{report:?}");
        assert_eq!(text(&dir.join(ITEMS).join("1")), "precious");
        assert_eq!(text(&dir.join(ITEMS).join("2.json")), "also precious");
        let manifest = read_manifest(&dir).unwrap().unwrap();
        let stored: Vec<&str> = manifest.items.iter().map(|h| h.stored.as_str()).collect();
        assert!(
            !stored.contains(&"items/1") && !stored.contains(&"items/2.json"),
            "{stored:?}"
        );
        let report = restore(&mc.root, "acme-1", &mut m);
        assert_eq!(report.held, 2, "{report:?}");
        assert_eq!(text(&mc.disk.join("notes.txt")), "hello");
        assert_eq!(text(&dir.join(ITEMS).join("1")), "precious");
    }

    /// A place taken between the look and the move: the item stays where it
    /// is, unrecorded, and what took the place is left there.
    #[test]
    fn a_stored_place_taken_during_the_move_is_never_deleted() {
        let mc = machine();
        let mut m = Fake {
            taken_meanwhile: vec![mc.disk.join("notes.txt")],
            ..Fake::default()
        };
        let items = vec![
            item(ItemKind::File, loc(&mc.disk.join("notes.txt"))),
            item(ItemKind::Folder, loc(&mc.disk.join("App"))),
        ];
        let report = hold(&mc.root, "acme-1", "Acme", &items, &mut m);
        assert_eq!(report.held, 1, "{report:?}");
        assert_eq!(report.failed[0].0, loc(&mc.disk.join("notes.txt")));
        assert!(
            report.failed[0].1.contains("already has something"),
            "{report:?}"
        );
        assert_eq!(text(&mc.disk.join("notes.txt")), "hello");
        let dir = mc.root.join("acme-1");
        assert_eq!(text(&dir.join(ITEMS).join("1")), "theirs");
        let manifest = read_manifest(&dir).unwrap().unwrap();
        assert_eq!(manifest.items.len(), 1);
        assert_eq!(manifest.items[0].item.location, loc(&mc.disk.join("App")));
    }

    /// The age-based empty never deletes an entry it cannot read, nor one
    /// holding something moved in part, however old; Delete now, which
    /// the user confirms, still does, and the listing says it is incomplete.
    #[test]
    fn the_auto_empty_skips_an_unreadable_or_partial_entry() {
        let mc = machine();
        let machine_root = mc.disk.parent().unwrap().join("machine-holding");
        let mut m = Fake::default();
        file(&mc.disk.join("one"), "1");
        file(&mc.disk.join("two"), "22");
        let one = |name: &str| vec![item(ItemKind::File, loc(&mc.disk.join(name)))];
        for root in [&mc.root, &machine_root] {
            hold(root, "corrupt", "Corrupt", &one("notes.txt"), &mut m);
            hold(root, "partial", "Partial", &one("one"), &mut m);
            hold(root, "whole", "Whole", &one("two"), &mut m);
            for id in ["corrupt", "partial", "whole"] {
                set_created(root, id, now() - 40 * DAY);
            }
            let partial = root.join("partial");
            let mut manifest = read_manifest(&partial).unwrap().unwrap();
            manifest.items[0].state = HeldState::Partial;
            save(&partial, &manifest).unwrap();
            fs::write(root.join("corrupt").join(MANIFEST), "{ not json").unwrap();
            // Restored for the next root.
            for (name, text) in [("notes.txt", "hello"), ("one", "1"), ("two", "22")] {
                file(&mc.disk.join(name), text);
            }
        }
        let listed = list(&mc.root, &machine_root).entries;
        let flags: BTreeMap<(bool, &str), (bool, bool)> = listed
            .iter()
            .map(|e| ((e.scope_machine, e.id.as_str()), (e.waiting, e.incomplete)))
            .collect();
        assert_eq!(flags[&(true, "corrupt")], (false, true));
        assert_eq!(flags[&(true, "partial")], (false, true));
        assert_eq!(flags[&(true, "whole")], (true, false));
        assert_eq!(flags[&(false, "corrupt")], (false, true));
        assert_eq!(flags[&(false, "whole")], (false, false));

        let report = empty(&mc.root, None, Some(30));
        assert!(report.failed.is_empty(), "{report:?}");
        assert!(mc.root.join("corrupt").exists());
        assert!(mc.root.join("partial").exists());
        assert!(!mc.root.join("whole").exists());

        // Delete now names the entry and deletes it.
        assert_eq!(empty(&mc.root, Some("corrupt"), None).held, 1);
        assert_eq!(empty(&mc.root, Some("partial"), None).held, 1);
        assert!(!mc.root.join("corrupt").exists());
    }
}
