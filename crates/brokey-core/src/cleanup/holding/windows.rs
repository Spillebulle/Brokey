//! The live backend: what [`Mutate`] does on this machine.
//!
//! Files and folders move through [`move_file_or_folder`]; the registry
//! through `windows-registry`, every value copied as its type number and
//! its bytes; services through the Service Control Manager; scheduled tasks
//! through the Task Scheduler's COM interface, exported from their XML file
//! under `System32\Tasks`; firewall rules as the values Windows stores them
//! in under `FirewallRules`. Nothing here starts a program.

use super::{
    Done, Failure, KeyNode, Mutate, REG_DWORD, REG_EXPAND_SZ, REG_MULTI_SZ, REG_SZ, RegValue,
    firewall_rule_name, merge_with, move_with, path_text,
};
use crate::cleanup::view::Hive;
use crate::cleanup::view::windows::{long_path, wow6432};
use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use windows_sys::Wdk::System::Registry::NtDeleteKey;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, GetLastError, HANDLE,
    HLOCAL, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, AdjustTokenPrivileges, CreateWellKnownSid,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorDacl,
    GetSecurityDescriptorOwner, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SE_PRIVILEGE_ENABLED, SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES, WELL_KNOWN_SID_TYPE, WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{CreateDirectoryW, DELETE, MoveFileExW};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Registry::{
    HKEY, REG_CREATED_NEW_KEY, REG_LINK, REG_OPTION_NON_VOLATILE, REG_OPTION_OPEN_LINK,
    RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW,
};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
    OpenServiceW, QueryServiceStatus, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE,
    SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_STATUS, SERVICE_STOP, SERVICE_STOPPED,
};
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_ProgramData, FOLDERID_System, SHGetKnownFolderPath,
};

const SERVICES: &str = r"SYSTEM\CurrentControlSet\Services";
const FIREWALL_RULES: &str =
    r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules";
/// Owned by Administrators; full control for Administrators and SYSTEM,
/// read and list for signed-in users (so the window can show what is held),
/// and the parent's permissions are not inherited.
pub const ADMIN_ONLY_SDDL: &str = "O:BAD:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)(A;OICI;FR;;;AU)";
/// `ERROR_FILE_NOT_FOUND` and `ERROR_PATH_NOT_FOUND` as HRESULTs.
const NOT_FOUND: [i32; 2] = [0x8007_0002_u32 as i32, 0x8007_0003_u32 as i32];
const ERROR_SERVICE_DOES_NOT_EXIST: u32 = 1060;
const ERROR_SERVICE_MARKED_FOR_DELETE: u32 = 1072;
const ERROR_SERVICE_EXISTS: u32 = 1073;

fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain([0]).collect()
}

/// A rename that refuses to replace what is at `to` and never copies:
/// across volumes it fails with `ERROR_NOT_SAME_DEVICE`.
pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (wide(long_path(from)), wide(long_path(to)));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    let mut path: windows_sys::core::PWSTR = ptr::null_mut();
    // SAFETY: `id` is a valid GUID and `path` an out parameter; on success
    // it is a NUL-terminated string the shell allocated, read once and then
    // freed with `CoTaskMemFree`, which it must be even on failure.
    unsafe {
        let hr = SHGetKnownFolderPath(id, 0, ptr::null_mut(), &mut path);
        let out = (hr >= 0 && !path.is_null()).then(|| {
            let len = (0..).take_while(|&i| *path.add(i) != 0).count();
            PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(
                path, len,
            )))
        });
        CoTaskMemFree(path as *const _);
        out
    }
}

pub(crate) fn local_app_data() -> Option<PathBuf> {
    known_folder(&FOLDERID_LocalAppData)
}

pub(crate) fn program_data() -> Option<PathBuf> {
    known_folder(&FOLDERID_ProgramData)
}

/// Make the machine's holding root `path`, and the `ancestors` folders above
/// it that are Brokey's own (`%ProgramData%\Brokey`), folders that only
/// Administrators and SYSTEM can change: owned by Administrators, full
/// control for them and SYSTEM, read and list for signed-in users so that
/// the window, which is never elevated, can show what is held.
///
/// Each folder that is missing is made with those permissions. Each that is
/// there is checked: a link or a file is refused, and one that is owned by
/// someone else or that anyone else may write to (a standard user can make
/// `%ProgramData%\Brokey` before Brokey does) is taken back, ownership and
/// permissions, when this runs as Administrator, and refused otherwise.
/// It never leaves a folder weaker than that and never follows a link.
pub fn create_admin_only_dir(path: &Path, ancestors: usize) -> Done<()> {
    let mut chain: Vec<&Path> = path.ancestors().take(ancestors + 1).collect();
    chain.reverse();
    let descriptor = Descriptor::admin_only().map_err(|e| {
        Failure::Failed(format!(
            "Brokey could not prepare the holding area's permissions ({e}), so nothing was \
             moved. Try again."
        ))
    })?;
    for dir in chain {
        secure_dir(dir, &descriptor)?;
    }
    Ok(())
}

/// The admin-only descriptor, freed when it goes.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn admin_only() -> io::Result<Descriptor> {
        let sddl = wide(ADMIN_ONLY_SDDL);
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated and `descriptor` an out
        // parameter, freed with `LocalFree` in `drop`.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Descriptor(descriptor))
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by the conversion in `admin_only`, freed once.
        unsafe { LocalFree(self.0 as HLOCAL) };
    }
}

fn secure_dir(dir: &Path, descriptor: &Descriptor) -> Done<()> {
    let shown = dir.display();
    let failed = |e: io::Error| {
        Failure::Failed(format!(
            "Brokey could not make {shown} a folder only Administrators can change ({e}), so \
             nothing was moved. Try again."
        ))
    };
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            Err(Failure::Failed(format!(
                "{shown} is not a plain folder, so Brokey will not keep things there. Remove it \
                 and try again."
            )))
        }
        Ok(_) => {
            if is_admin_only(dir).map_err(failed)? {
                return Ok(());
            }
            enable_privilege("SeTakeOwnershipPrivilege");
            enable_privilege("SeRestorePrivilege");
            let taken = apply_descriptor(dir, descriptor.0).and_then(|()| is_admin_only(dir));
            match taken {
                Ok(true) => Ok(()),
                _ => Err(Failure::Failed(format!(
                    "{shown} could be changed by someone other than an Administrator, and Brokey \
                     could not take it back, so nothing was moved there. Remove the folder, or \
                     let Brokey ask for Administrator, and try again."
                ))),
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: 0,
            };
            let name = wide(long_path(dir));
            // SAFETY: `name` is NUL-terminated; `attributes` and the
            // descriptor it points to outlive the call.
            if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } == 0 {
                return Err(failed(io::Error::last_os_error()));
            }
            Ok(())
        }
        Err(e) => Err(failed(e)),
    }
}

/// Enable a privilege this process's token holds, if it holds it. A token
/// that does not (a process that is not elevated) is left as it is, and
/// what needed the privilege then fails on its own.
fn enable_privilege(name: &str) {
    let name = wide(name);
    // SAFETY: plain out parameters; the token handle is closed once.
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut token) == 0 {
            return;
        }
        let mut privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: std::mem::zeroed(),
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        if LookupPrivilegeValueW(
            ptr::null(),
            name.as_ptr(),
            &mut privileges.Privileges[0].Luid,
        ) != 0
        {
            AdjustTokenPrivileges(token, 0, &privileges, 0, ptr::null_mut(), ptr::null_mut());
        }
        CloseHandle(token);
    }
}

/// Rights that let a holder change a folder or what is in it.
const WRITE_RIGHTS: u32 = 0x0002 // FILE_WRITE_DATA / FILE_ADD_FILE
    | 0x0004 // FILE_APPEND_DATA / FILE_ADD_SUBDIRECTORY
    | 0x0010 // FILE_WRITE_EA
    | 0x0040 // FILE_DELETE_CHILD
    | 0x0100 // FILE_WRITE_ATTRIBUTES
    | 0x0001_0000 // DELETE
    | 0x0004_0000 // WRITE_DAC
    | 0x0008_0000 // WRITE_OWNER
    | 0x1000_0000 // GENERIC_ALL
    | 0x4000_0000; // GENERIC_WRITE

/// A well-known SID in a buffer of its own.
fn well_known(kind: WELL_KNOWN_SID_TYPE) -> [u8; SECURITY_MAX_SID_SIZE as usize] {
    let mut sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut size = SECURITY_MAX_SID_SIZE;
    // SAFETY: `sid` has room for any SID and `size` says so.
    unsafe { CreateWellKnownSid(kind, ptr::null_mut(), sid.as_mut_ptr() as PSID, &mut size) };
    sid
}

/// Owned by Administrators or SYSTEM, and nobody else may write to it or
/// delete from it: a folder only an Administrator could have filled.
pub fn is_admin_only(path: &Path) -> io::Result<bool> {
    let admins = well_known(WinBuiltinAdministratorsSid);
    let system = well_known(WinLocalSystemSid);
    let trusted = |sid: PSID| {
        // SAFETY: all three point to valid SIDs that outlive the call;
        // `EqualSid` only reads them.
        unsafe {
            EqualSid(sid, admins.as_ptr() as PSID) != 0
                || EqualSid(sid, system.as_ptr() as PSID) != 0
        }
    };

    let name = wide(path);
    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: out parameters; `owner` and `dacl` point into `descriptor`,
    // which is freed with `LocalFree` after the last use of either.
    unsafe {
        let code = GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        );
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        // A missing DACL lets everyone do everything.
        let mut ok = !owner.is_null() && trusted(owner) && !dacl.is_null();
        if ok {
            for i in 0..u32::from((*dacl).AceCount) {
                let mut ace: *mut core::ffi::c_void = ptr::null_mut();
                if GetAce(dacl, i, &mut ace) == 0 {
                    ok = false;
                    break;
                }
                let header = &*(ace as *const ACE_HEADER);
                match u32::from(header.AceType) {
                    ACCESS_DENIED_ACE_TYPE => {}
                    ACCESS_ALLOWED_ACE_TYPE => {
                        let allowed = &*(ace as *const ACCESS_ALLOWED_ACE);
                        let sid = &allowed.SidStart as *const u32 as PSID;
                        if allowed.Mask & WRITE_RIGHTS != 0 && !trusted(sid) {
                            ok = false;
                        }
                    }
                    // Anything else is not something Brokey writes.
                    _ => ok = false,
                }
            }
        }
        LocalFree(descriptor as HLOCAL);
        Ok(ok)
    }
}

fn apply_descriptor(path: &Path, descriptor: PSECURITY_DESCRIPTOR) -> io::Result<()> {
    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let (mut present, mut defaulted) = (0, 0);
    let name = wide(path);
    // SAFETY: `descriptor` is a valid self-relative descriptor; the owner and
    // DACL pointers point into it and are used while it lives.
    unsafe {
        if GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted) == 0
            || GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let code = SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            owner,
            ptr::null_mut(),
            dacl,
            ptr::null(),
        );
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code as i32));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The registry

fn hive_word(hive: Hive) -> &'static str {
    match hive {
        Hive::Hklm => "HKLM",
        Hive::Hklm32 => "HKLM32",
        Hive::Hkcu => "HKCU",
    }
}

fn root_of(hive: Hive, path: &str) -> (&'static windows_registry::Key, String) {
    match hive {
        Hive::Hklm => (windows_registry::LOCAL_MACHINE, path.to_string()),
        Hive::Hklm32 => (windows_registry::LOCAL_MACHINE, wow6432(path)),
        Hive::Hkcu => (windows_registry::CURRENT_USER, path.to_string()),
    }
}

/// A registry error, which converts to an `io::Error` carrying its HRESULT.
fn registry_failure(location: &str, e: impl Into<io::Error>) -> Failure {
    let e: io::Error = e.into();
    if e.raw_os_error()
        .is_some_and(|code| NOT_FOUND.contains(&code))
    {
        return Failure::Missing;
    }
    Failure::Failed(format!(
        "Brokey could not change {location} ({e}), so it was left where it is. Try again."
    ))
}

// Registry links. A key can be a symbolic link to another key anywhere in
// the registry (`REG_OPTION_CREATE_LINK` and a `SymbolicLinkValue`), and
// anyone who may create a subkey can make one. So nothing here opens a key
// by its whole path: each part is opened relative to the one before, as
// itself (`REG_OPTION_OPEN_LINK`), and a link refuses the key; a delete
// recurses by handle and deletes each key through its own handle
// (`NtDeleteKey`), never by name; a key is created one part at a time and
// only kept when it was made new or opened as itself and found not to be a
// link. The two links Windows itself keeps on the paths used here are
// followed: `HKLM\SYSTEM\CurrentControlSet`, which only Administrators can
// change, and `HKCU\Software\Classes`, the user's own.

const KEY_QUERY_VALUE: u32 = 0x0001;
const KEY_ENUMERATE_SUB_KEYS: u32 = 0x0008;
const KEY_WOW64_64KEY: u32 = 0x0100;
const KEY_READ: u32 = 0x2_0019;
const KEY_WRITE: u32 = 0x2_0006;
const KEY_DELETE: u32 = 0x1_0000;
const LINKS_WINDOWS_KEEPS: [(Hive, &str); 2] = [
    (Hive::Hklm, r"SYSTEM\CurrentControlSet"),
    (Hive::Hkcu, r"Software\Classes"),
];

fn wide_str(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

fn link_failure(location: &str) -> Failure {
    Failure::Failed(format!(
        "{location} is a registry link to another key, or has one on the way to it or inside \
         it, so Brokey left it as it was. Remove the link and try again."
    ))
}

fn win32_failure(location: &str, code: u32) -> Failure {
    if code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND {
        return Failure::Missing;
    }
    registry_failure(location, io::Error::from_raw_os_error(code as i32))
}

/// One subkey of `parent`, opened as itself unless `follow`: a link is
/// opened as the link.
fn open_child(
    parent: &windows_registry::Key,
    name: &str,
    access: u32,
    follow: bool,
) -> Result<windows_registry::Key, u32> {
    let name = wide_str(name);
    let mut handle: HKEY = ptr::null_mut();
    let options = if follow { 0 } else { REG_OPTION_OPEN_LINK };
    // SAFETY: `parent` is an open key, `name` is NUL-terminated, and
    // `handle` is an out parameter taken into a `Key` on success.
    let code = unsafe {
        RegOpenKeyExW(
            parent.as_raw() as HKEY,
            name.as_ptr(),
            options,
            access | KEY_QUERY_VALUE | KEY_WOW64_64KEY,
            &mut handle,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(code);
    }
    // SAFETY: a key handle just opened, owned by nothing else.
    Ok(unsafe { windows_registry::Key::from_raw(handle as _) })
}

/// Whether a key opened as itself is a link: it has a `SymbolicLinkValue`
/// of type `REG_LINK`. A key that cannot be asked is answered as one.
fn is_link(key: &windows_registry::Key) -> bool {
    let name = wide_str("SymbolicLinkValue");
    let mut ty = 0u32;
    // SAFETY: an open key, a NUL-terminated name and one out parameter; no
    // data is read.
    let code = unsafe {
        RegQueryValueExW(
            key.as_raw() as HKEY,
            name.as_ptr(),
            ptr::null(),
            &mut ty,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    match code {
        ERROR_SUCCESS => ty == REG_LINK,
        ERROR_FILE_NOT_FOUND => false,
        _ => true,
    }
}

/// A subkey that must not be a link, opened or, with `create`, made.
/// Made means made new: one that turns out to be there already, perhaps put
/// there since it was looked for, is opened again as itself and asked.
fn child_unlinked(
    parent: &windows_registry::Key,
    name: &str,
    access: u32,
    create: bool,
    location: &str,
) -> Done<windows_registry::Key> {
    for _ in 0..3 {
        match open_child(parent, name, access, false) {
            Ok(key) if is_link(&key) => return Err(link_failure(location)),
            Ok(key) => return Ok(key),
            Err(code) if !create || code != ERROR_FILE_NOT_FOUND => {
                return Err(win32_failure(location, code));
            }
            Err(_) => {}
        }
        let wide = wide_str(name);
        let mut handle: HKEY = ptr::null_mut();
        let mut disposition = 0u32;
        // SAFETY: as `open_child`; `disposition` is an out parameter.
        let code = unsafe {
            RegCreateKeyExW(
                parent.as_raw() as HKEY,
                wide.as_ptr(),
                0,
                ptr::null(),
                REG_OPTION_NON_VOLATILE,
                access | KEY_QUERY_VALUE | KEY_WOW64_64KEY,
                ptr::null(),
                &mut handle,
                &mut disposition,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(win32_failure(location, code));
        }
        // SAFETY: a key handle just opened, owned by nothing else.
        let key = unsafe { windows_registry::Key::from_raw(handle as _) };
        if disposition == REG_CREATED_NEW_KEY {
            return Ok(key);
        }
        // It was there after all, and was opened through whatever it is:
        // let it go and open it as itself.
    }
    Err(link_failure(location))
}

/// `hive\path`, each part opened relative to the one before and as itself,
/// a link at any part refusing it; with `create`, the parts that are
/// missing are made one at a time.
fn walk(hive: Hive, path: &str, access: u32, create: bool) -> Done<windows_registry::Key> {
    let location = format!(r"{}\{path}", hive_word(hive));
    let (root, sub) = root_of(hive, path);
    let base = if hive == Hive::Hkcu {
        Hive::Hkcu
    } else {
        Hive::Hklm
    };
    let parts: Vec<&str> = sub.split('\\').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err(Failure::Failed(format!(
            "{location} is a whole hive, so Brokey left it as it was."
        )));
    }
    let mut current: Option<windows_registry::Key> = None;
    let mut so_far = String::new();
    for (i, part) in parts.iter().enumerate() {
        if !so_far.is_empty() {
            so_far.push('\\');
        }
        so_far.push_str(part);
        let want = if i + 1 == parts.len() {
            access
        } else if create {
            KEY_ENUMERATE_SUB_KEYS | KEY_QUERY_VALUE | KEY_WRITE
        } else {
            KEY_ENUMERATE_SUB_KEYS | KEY_QUERY_VALUE
        };
        let parent = current.as_ref().unwrap_or(root);
        let windows_link = LINKS_WINDOWS_KEEPS
            .iter()
            .any(|(h, p)| *h == base && p.eq_ignore_ascii_case(&so_far));
        let key = if windows_link {
            open_child(parent, part, want, true).map_err(|code| win32_failure(&location, code))?
        } else {
            child_unlinked(parent, part, want, create, &location)?
        };
        current = Some(key);
    }
    Ok(current.expect("a path with parts"))
}

fn open(hive: Hive, path: &str, write: bool) -> Done<windows_registry::Key> {
    let access = if write {
        KEY_READ | KEY_WRITE
    } else {
        KEY_READ
    };
    walk(hive, path, access, false)
}

/// What the way to a key is, as [`walk`] finds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyWay {
    /// There, and no link on the way to it or at it.
    Clear,
    /// A link at it or on the way.
    Link,
    Missing,
    /// It could not be asked.
    Unreadable,
}

/// The way to a key, for the closed list, which asks before anything
/// changes.
pub fn key_way(hive: Hive, path: &str) -> KeyWay {
    let location = format!(r"{}\{path}", hive_word(hive));
    match walk(hive, path, KEY_READ, false) {
        Ok(_) => KeyWay::Clear,
        Err(Failure::Missing) => KeyWay::Missing,
        Err(f) if f == link_failure(&location) => KeyWay::Link,
        Err(_) => KeyWay::Unreadable,
    }
}

fn read_node(key: &windows_registry::Key, location: &str) -> Done<KeyNode> {
    let mut node = KeyNode::default();
    for (name, value) in key.values().map_err(|e| registry_failure(location, e))? {
        node.values.push((
            name,
            RegValue {
                ty: value.ty().into(),
                data: value.to_vec(),
            },
        ));
    }
    let names: Vec<String> = key
        .keys()
        .map_err(|e| registry_failure(location, e))?
        .collect();
    for name in names {
        let sub = child_unlinked(key, &name, KEY_READ, false, location)?;
        node.subkeys.push((name, read_node(&sub, location)?));
    }
    Ok(node)
}

/// Write a key's values and subkeys. With `merge`, a value already there is
/// left as it is: only what is missing is written.
fn write_node(
    key: &windows_registry::Key,
    node: &KeyNode,
    merge: bool,
    location: &str,
) -> Done<()> {
    for (name, value) in &node.values {
        if merge && key.get_type(name).is_ok() {
            continue;
        }
        key.set_bytes(name, value.ty.into(), &value.data)
            .map_err(|e| registry_failure(location, e))?;
    }
    for (name, sub) in &node.subkeys {
        let child = child_unlinked(key, name, KEY_READ | KEY_WRITE, true, location)?;
        write_node(&child, sub, merge, location)?;
    }
    Ok(())
}

/// Delete a key and everything under it, each key through its own handle,
/// opened as itself: a link met on the way stops it there and is never
/// followed.
fn delete_tree(key: &windows_registry::Key, location: &str) -> Done<()> {
    let names: Vec<String> = key
        .keys()
        .map_err(|e| registry_failure(location, e))?
        .collect();
    for name in names {
        let child = child_unlinked(key, &name, KEY_READ | KEY_DELETE, false, location)?;
        delete_tree(&child, location)?;
    }
    // SAFETY: an open key with DELETE access; it is closed when `key` goes.
    let status = unsafe { NtDeleteKey(key.as_raw() as _) };
    if status != 0 {
        return Err(Failure::Failed(format!(
            "Brokey could not delete {location} (NTSTATUS {status:#x}), so some of it may be \
             left. Try again."
        )));
    }
    Ok(())
}

fn export_key(hive: Hive, path: &str) -> Done<KeyNode> {
    let location = format!(r"{}\{path}", hive_word(hive));
    let key = open(hive, path, false)?;
    read_node(&key, &location)
}

fn delete_key(hive: Hive, path: &str) -> Done<()> {
    let location = format!(r"{}\{path}", hive_word(hive));
    let key = walk(hive, path, KEY_READ | KEY_DELETE, false)?;
    // Looked through whole first, so that a link anywhere in it refuses the
    // key before anything is deleted.
    read_node(&key, &location)?;
    delete_tree(&key, &location)
}

fn restore_key(hive: Hive, path: &str, node: &KeyNode, merge: bool) -> Done<()> {
    let location = format!(r"{}\{path}", hive_word(hive));
    // Opened for writing only when merging, which is the one case that
    // writes into a key already there.
    match open(hive, path, merge) {
        Ok(_) if !merge => return Err(Failure::Occupied),

        Ok(key) => {
            // Some of it is still there: put back what is missing beside it,
            // and take nothing away whatever happens.
            return write_node(&key, node, true, &location);
        }
        Err(Failure::Missing) => {}
        Err(f) => return Err(f),
    }
    let key = walk(hive, path, KEY_READ | KEY_WRITE | KEY_DELETE, true)?;
    write_node(&key, node, false, &location).inspect_err(|_| {
        // Half a key that was not there before is worse than none: take
        // back what was written.
        let _ = delete_tree(&key, &location);
    })
}

// ---------------------------------------------------------------------------
// Services

/// What `CreateServiceW` needs, read out of a service's exported key.
#[derive(Debug, PartialEq, Eq)]
pub struct ServiceConfig {
    pub service_type: u32,
    pub start: u32,
    pub error_control: u32,
    pub image_path: String,
    pub display_name: Option<String>,
    pub group: Option<String>,
    /// Services, then groups with the `+` Windows marks them with.
    pub dependencies: Vec<String>,
    pub account: Option<String>,
}

/// Little-endian UTF-16 code units; an odd last byte is dropped.
fn utf16_units(bytes: &[u8]) -> Vec<u16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .collect()
}

/// The text of a `REG_SZ` or `REG_EXPAND_SZ`, up to its terminator.
fn reg_string(value: &RegValue) -> Option<String> {
    if !matches!(value.ty, REG_SZ | REG_EXPAND_SZ) {
        return None;
    }
    let units: Vec<u16> = utf16_units(&value.data)
        .into_iter()
        .take_while(|&u| u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

fn reg_multi(value: &RegValue) -> Vec<String> {
    if value.ty != REG_MULTI_SZ {
        return Vec::new();
    }
    utf16_units(&value.data)
        .split(|&u| u == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

fn reg_dword(value: &RegValue) -> Option<u32> {
    (value.ty == REG_DWORD && value.data.len() == 4)
        .then(|| u32::from_le_bytes(value.data[..4].try_into().unwrap()))
}

impl ServiceConfig {
    pub fn from_key(name: &str, key: &KeyNode) -> Result<ServiceConfig, String> {
        let get = |n: &str| {
            key.values
                .iter()
                .find(|(v, _)| v.eq_ignore_ascii_case(n))
                .map(|(_, v)| v)
        };
        let dword = |n: &str| get(n).and_then(reg_dword);
        let text = |n: &str| get(n).and_then(reg_string).filter(|s| !s.is_empty());
        let incomplete = || {
            format!(
                "The copy of service {name} in the holding area is missing its settings, so it \
                 could not be put back."
            )
        };
        let mut dependencies = get("DependOnService").map(reg_multi).unwrap_or_default();
        dependencies.extend(
            get("DependOnGroup")
                .map(reg_multi)
                .unwrap_or_default()
                .into_iter()
                .map(|g| format!("+{g}")),
        );
        Ok(ServiceConfig {
            service_type: dword("Type").ok_or_else(incomplete)?,
            start: dword("Start").ok_or_else(incomplete)?,
            error_control: dword("ErrorControl").unwrap_or(1),
            image_path: text("ImagePath").ok_or_else(incomplete)?,
            display_name: text("DisplayName"),
            group: text("Group"),
            dependencies,
            account: text("ObjectName"),
        })
    }
}

/// An account Windows can start a service as without a password.
pub fn needs_no_password(account: Option<&str>) -> bool {
    let Some(account) = account else {
        return true;
    };
    let lower = account.to_lowercase();
    lower == "localsystem"
        || lower.starts_with(r"nt authority\")
        || lower.starts_with(r"nt service\")
}

struct ServiceHandle(SC_HANDLE);

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        // SAFETY: a handle the SCM returned, closed once.
        unsafe { CloseServiceHandle(self.0) };
    }
}

fn scm(access: u32, name: &str) -> Done<ServiceHandle> {
    // SAFETY: null machine and database names mean this machine's active
    // database.
    let handle = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), access) };
    if handle.is_null() {
        return Err(Failure::Failed(format!(
            "Brokey could not reach the Service Control Manager ({}), so service {name} was \
             left as it is. Try again.",
            io::Error::last_os_error()
        )));
    }
    Ok(ServiceHandle(handle))
}

fn open_service(scm: &ServiceHandle, name: &str, access: u32) -> Result<ServiceHandle, u32> {
    let wide_name = wide(name);
    // SAFETY: `scm` is open and `wide_name` is NUL-terminated.
    let handle = unsafe { OpenServiceW(scm.0, wide_name.as_ptr(), access) };
    if handle.is_null() {
        // SAFETY: reads this thread's last error, set by the call above.
        return Err(unsafe { GetLastError() });
    }
    Ok(ServiceHandle(handle))
}

fn service_key(name: &str) -> Done<String> {
    if name.is_empty() || name.contains(['\\', '/']) {
        return Err(Failure::Failed(format!(
            "{name} is not a service name, so nothing was changed."
        )));
    }
    Ok(format!(r"{SERVICES}\{name}"))
}

fn delete_service(name: &str) -> Done<()> {
    let manager = scm(SC_MANAGER_CONNECT, name)?;
    let service = match open_service(&manager, name, SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE) {
        Ok(s) => s,
        Err(ERROR_SERVICE_DOES_NOT_EXIST) => return Err(Failure::Missing),
        Err(code) => {
            return Err(Failure::Failed(format!(
                "Brokey could not open service {name} ({}), so it was left as it is. Try again.",
                io::Error::from_raw_os_error(code as i32)
            )));
        }
    };
    // SAFETY: `service` is open with SERVICE_STOP and SERVICE_QUERY_STATUS,
    // and `status` is a plain out parameter.
    unsafe {
        let mut status: SERVICE_STATUS = std::mem::zeroed();
        // Fails harmlessly for a service that is not running.
        ControlService(service.0, SERVICE_CONTROL_STOP, &mut status);
        let mut stopped = false;
        for _ in 0..120 {
            if QueryServiceStatus(service.0, &mut status) != 0
                && status.dwCurrentState == SERVICE_STOPPED
            {
                stopped = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        if !stopped {
            return Err(Failure::Failed(format!(
                "Service {name} did not stop within 30 seconds, so it was left as it is. Stop it \
                 in Services and clear it again."
            )));
        }
        if DeleteService(service.0) == 0 && GetLastError() != ERROR_SERVICE_MARKED_FOR_DELETE {
            return Err(Failure::Failed(format!(
                "Brokey could not delete service {name} ({}). Try again.",
                io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}

fn restore_service(name: &str, key: &KeyNode, merge: bool) -> Done<()> {
    let path = service_key(name)?;
    let config = ServiceConfig::from_key(name, key).map_err(Failure::Failed)?;
    let manager = scm(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE, name)?;
    if open_service(&manager, name, SERVICE_QUERY_STATUS).is_ok() {
        // A service is deleted whole or not at all: when merging, one that
        // is there was never removed.
        return if merge {
            Ok(())
        } else {
            Err(Failure::Occupied)
        };
    }
    if !needs_no_password(config.account.as_deref()) {
        return Err(Failure::Failed(format!(
            "Service {name} runs as {}, and Windows needs that account's password to put it \
             back, so it was kept in the holding area.",
            config.account.as_deref().unwrap_or_default()
        )));
    }
    let name_w = wide(name);
    let display = config.display_name.as_deref().map(wide);
    let image = wide(&config.image_path);
    let group = config.group.as_deref().map(wide);
    let account = config.account.as_deref().map(wide);
    let dependencies: Option<Vec<u16>> = (!config.dependencies.is_empty()).then(|| {
        let mut list: Vec<u16> = config
            .dependencies
            .iter()
            .flat_map(|d| d.encode_utf16().chain([0]))
            .collect();
        list.push(0);
        list
    });
    let or_null = |v: &Option<Vec<u16>>| v.as_ref().map_or(ptr::null(), |w| w.as_ptr());
    // SAFETY: every string is NUL-terminated (the dependency list doubly)
    // and lives to the end of the call; null means "none" for each optional
    // one, and no password is passed for an account that needs none.
    let handle = unsafe {
        CreateServiceW(
            manager.0,
            name_w.as_ptr(),
            or_null(&display),
            SERVICE_QUERY_STATUS,
            config.service_type,
            config.start,
            config.error_control,
            image.as_ptr(),
            or_null(&group),
            ptr::null_mut(),
            or_null(&dependencies),
            or_null(&account),
            ptr::null(),
        )
    };
    if handle.is_null() {
        // SAFETY: reads this thread's last error, set by the call above.
        let code = unsafe { GetLastError() };
        if code == ERROR_SERVICE_EXISTS {
            return Err(Failure::Occupied);
        }
        return Err(Failure::Failed(format!(
            "Brokey could not put service {name} back ({}). Try again.",
            io::Error::from_raw_os_error(code as i32)
        )));
    }
    drop(ServiceHandle(handle));
    // The rest of its settings (its description, its parameters) as they were.
    let location = format!(r"HKLM\{path}");
    let written = walk(Hive::Hklm, &path, KEY_READ | KEY_WRITE, true)
        .and_then(|k| write_node(&k, key, false, &location));
    written.map_err(|f| {
        let why = match f {
            Failure::Failed(s) | Failure::Partial(s) => s,
            _ => format!("{location} could not be written."),
        };
        Failure::Failed(format!(
            "Service {name} was put back, but not all of its settings. {why} Check it in Services."
        ))
    })
}

// ---------------------------------------------------------------------------
// Scheduled tasks

/// `\Vendor\Update` as its folder and its name. `None` for anything that is
/// not a task path under the root.
pub fn task_parts(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix('\\')?;
    let parts: Vec<&str> = rest.split('\\').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains('/'))
    {
        return None;
    }
    let (leaf, folders) = parts.split_last()?;
    Some((format!(r"\{}", folders.join("\\")), leaf.to_string()))
}

/// A task file's text: UTF-16 with its byte order mark, as Windows writes
/// them, or UTF-8.
pub fn decode_task_xml(bytes: &[u8]) -> Option<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return String::from_utf16(&utf16_units(rest)).ok();
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8(bytes.to_vec()).ok()
}

fn bad_task(path: &str) -> Failure {
    Failure::Failed(format!(
        "{path} is not a scheduled task's path, so nothing was changed."
    ))
}

fn export_task(path: &str) -> Done<String> {
    task_parts(path).ok_or_else(|| bad_task(path))?;
    let system = known_folder(&FOLDERID_System).ok_or_else(|| {
        Failure::Failed(format!(
            "Brokey could not find the Windows folder, so task {path} was left as it is."
        ))
    })?;
    let file = path
        .trim_start_matches('\\')
        .split('\\')
        .fold(system.join("Tasks"), |p, part| p.join(part));
    let bytes = std::fs::read(&file).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            Failure::Missing
        } else {
            Failure::Failed(format!(
                "Brokey could not read task {path} ({e}), so it was left as it is. Try again."
            ))
        }
    })?;
    decode_task_xml(&bytes).ok_or_else(|| {
        Failure::Failed(format!(
            "Task {path} is not in a form Brokey can read back, so it was left as it is."
        ))
    })
}

mod com {
    //! The Task Scheduler, through COM. Called on the thread that asks:
    //! COM is joined here and left again only if this is what joined it.

    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows::Win32::System::TaskScheduler::{
        ITaskService, TASK_CREATE, TASK_LOGON_NONE, TaskScheduler,
    };
    use windows::Win32::System::Variant::VARIANT;
    use windows::core::BSTR;

    struct Apartment(bool);

    impl Drop for Apartment {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: paired with the successful `CoInitializeEx` on
                // this same thread that set the flag.
                unsafe { CoUninitialize() };
            }
        }
    }

    fn with_service<T>(
        f: impl FnOnce(&ITaskService) -> windows::core::Result<T>,
    ) -> windows::core::Result<T> {
        // SAFETY: COM calls on this thread after it has joined an apartment;
        // the service is released before the apartment guard, which was
        // made first and so drops last.
        unsafe {
            let _apartment = Apartment(CoInitializeEx(None, COINIT_MULTITHREADED).is_ok());
            let service: ITaskService =
                CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
            let none = VARIANT::default();
            service.Connect(&none, &none, &none, &none)?;
            f(&service)
        }
    }

    pub fn exists(path: &str) -> windows::core::Result<bool> {
        with_service(|s| unsafe {
            let root = s.GetFolder(&BSTR::from("\\"))?;
            Ok(root.GetTask(&BSTR::from(path)).is_ok())
        })
    }

    pub fn delete(folder: &str, name: &str) -> windows::core::Result<()> {
        // SAFETY: COM calls with arguments that live for each call.
        with_service(|s| unsafe {
            let folder = s.GetFolder(&BSTR::from(folder))?;
            let name = BSTR::from(name);
            let task = folder.GetTask(&name)?;
            // A task that is not running answers an error; either way it is
            // not running afterwards.
            let _ = task.Stop(0);
            folder.DeleteTask(&name, 0)
        })
    }

    pub fn register(path: &str, xml: &str) -> windows::core::Result<()> {
        // SAFETY: as above. No user and no password: the principal written
        // in the task's own XML is the one it runs as.
        with_service(|s| unsafe {
            let root = s.GetFolder(&BSTR::from("\\"))?;
            let none = VARIANT::default();
            root.RegisterTask(
                &BSTR::from(path),
                &BSTR::from(xml),
                TASK_CREATE.0,
                &none,
                &none,
                TASK_LOGON_NONE,
                &none,
            )?;
            Ok(())
        })
    }
}

fn task_failure(path: &str, e: &windows::core::Error) -> Failure {
    if NOT_FOUND.contains(&e.code().0) {
        return Failure::Missing;
    }
    Failure::Failed(format!(
        "Brokey could not change task {path} ({}), so it was left as it is. Try again.",
        e.message()
    ))
}

fn delete_task(path: &str) -> Done<()> {
    let (folder, name) = task_parts(path).ok_or_else(|| bad_task(path))?;
    com::delete(&folder, &name).map_err(|e| task_failure(path, &e))
}

fn restore_task(path: &str, xml: &str, merge: bool) -> Done<()> {
    task_parts(path).ok_or_else(|| bad_task(path))?;
    if com::exists(path).map_err(|e| task_failure(path, &e))? {
        // Deleted whole or not at all, as a service is.
        return if merge {
            Ok(())
        } else {
            Err(Failure::Occupied)
        };
    }
    com::register(path, xml).map_err(|e| task_failure(path, &e))
}

// ---------------------------------------------------------------------------
// Firewall rules

fn export_firewall_rule(name: &str) -> Done<Vec<(String, String)>> {
    let location = format!("firewall:{name}");
    let key = open(Hive::Hklm, FIREWALL_RULES, false)?;
    let values = key.values().map_err(|e| registry_failure(&location, e))?;
    let rules: Vec<(String, String)> = values
        .filter_map(|(id, value)| Some((id, String::try_from(value).ok()?)))
        .filter(|(id, rule)| firewall_rule_name(id, rule) == name)
        .collect();
    if rules.is_empty() {
        return Err(Failure::Missing);
    }
    Ok(rules)
}

fn delete_firewall_rule(name: &str, rules: &[(String, String)]) -> Done<()> {
    let location = format!("firewall:{name}");
    let key = open(Hive::Hklm, FIREWALL_RULES, true)?;
    for (id, _) in rules {
        key.remove_value(id)
            .map_err(|e| registry_failure(&location, e))?;
    }
    Ok(())
}

fn restore_firewall_rule(name: &str, rules: &[(String, String)], merge: bool) -> Done<()> {
    let location = format!("firewall:{name}");
    let key = open(Hive::Hklm, FIREWALL_RULES, true)?;
    let there = |id: &String| key.get_type(id).is_ok();
    if !merge && rules.iter().any(|(id, _)| there(id)) {
        return Err(Failure::Occupied);
    }
    for (id, rule) in rules {
        if merge && there(id) {
            continue;
        }

        key.set_string(id, rule)
            .map_err(|e| registry_failure(&location, e))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------

/// The machine this process runs on.
pub struct LiveMutate {
    /// The root that is made for Administrators only.
    pub machine_root: PathBuf,
    /// Copy and remove even on one volume, as across two. For tests.
    pub force_copy: bool,
    /// Called once an item is opened and checked and before it moves. For
    /// tests, which try to swap a folder there.
    pub between: Option<super::bound::Between>,
}

impl LiveMutate {
    /// The live backend for the given machine root.
    pub fn new(machine_root: PathBuf) -> LiveMutate {
        LiveMutate {
            machine_root,
            force_copy: false,
            between: None,
        }
    }

    fn bound(&self) -> super::bound::Bound {
        let mut bound = super::bound::Bound::default();
        bound.between = self.between;
        bound
    }

    /// A file or folder moved with every step bound to what was checked:
    /// the folders above `from` held still for the whole move, and see
    /// [`super::bound`] for the rest.
    fn bound_move(&self, from: &Path, to: &Path, merge: bool) -> Done<()> {
        let mut disk = self.bound();
        if let Some(parent) = from.parent() {
            match disk.pin_dir(parent, false) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Failure::Missing),
                Err(e) => {
                    return Err(Failure::Failed(super::io_sentence(
                        &from.display().to_string(),
                        &e,
                    )));
                }
            }
        }
        if merge {
            if let Some(parent) = to.parent() {
                disk.pin_dir(parent, true).map_err(|e| {
                    Failure::Failed(super::io_sentence(&to.display().to_string(), &e))
                })?;
            }
            merge_with(from, to, self.force_copy, &mut disk)
        } else {
            move_with(from, to, self.force_copy, &mut disk)
        }
    }
}

impl LiveMutate {
    fn is_machine_root(&self, root: &Path) -> bool {
        path_text(root) == path_text(&self.machine_root)
    }
}

impl Default for LiveMutate {
    fn default() -> Self {
        LiveMutate::new(super::machine_root())
    }
}

impl Mutate for LiveMutate {
    fn make_root(&mut self, root: &Path) -> Done<()> {
        if self.is_machine_root(root) {
            // `%ProgramData%\Brokey` is Brokey's too, and secured with it.
            return create_admin_only_dir(root, 1);
        }
        std::fs::create_dir_all(root).map_err(|e| {
            Failure::Failed(format!(
                "Brokey could not make the holding area {} ({e}), so nothing was moved. Try again.",
                root.display()
            ))
        })
    }

    fn trust_entry(&mut self, root: &Path, dir: &Path) -> Done<()> {
        if !self.is_machine_root(root) {
            return Ok(());
        }
        match is_admin_only(dir) {
            Ok(true) => Ok(()),
            _ => Err(Failure::Failed(format!(
                "{} was not made by Brokey as an Administrator, so nothing was moved into it or \
                 taken from it. Delete it and try again.",
                dir.display()
            ))),
        }
    }

    fn move_path(&mut self, from: &Path, to: &Path) -> Done<()> {
        self.bound_move(from, to, false)
    }

    fn merge_path(&mut self, from: &Path, to: &Path) -> Done<()> {
        self.bound_move(from, to, true)
    }

    fn export_key(&mut self, hive: Hive, path: &str) -> Done<KeyNode> {
        export_key(hive, path)
    }

    fn delete_key(&mut self, hive: Hive, path: &str) -> Done<()> {
        delete_key(hive, path)
    }

    fn restore_key(&mut self, hive: Hive, path: &str, key: &KeyNode, merge: bool) -> Done<()> {
        restore_key(hive, path, key, merge)
    }

    fn export_value(&mut self, hive: Hive, key: &str, name: &str) -> Done<RegValue> {
        let location = format!(r"{}\{key} | {name}", hive_word(hive));
        let value = open(hive, key, false)?
            .get_value(name)
            .map_err(|e| registry_failure(&location, e))?;
        Ok(RegValue {
            ty: value.ty().into(),
            data: value.to_vec(),
        })
    }

    fn delete_value(&mut self, hive: Hive, key: &str, name: &str) -> Done<()> {
        let location = format!(r"{}\{key} | {name}", hive_word(hive));
        open(hive, key, true)?
            .remove_value(name)
            .map_err(|e| registry_failure(&location, e))
    }

    fn restore_value(
        &mut self,
        hive: Hive,
        key: &str,
        name: &str,
        value: &RegValue,
        merge: bool,
    ) -> Done<()> {
        let location = format!(r"{}\{key} | {name}", hive_word(hive));
        let opened = walk(hive, key, KEY_READ | KEY_WRITE, true).map_err(|f| match f {
            Failure::Missing => registry_failure(&location, io::Error::from_raw_os_error(2)),
            f => f,
        })?;
        if opened.get_type(name).is_ok() {
            // When merging, one that is there was never removed.
            return if merge {
                Ok(())
            } else {
                Err(Failure::Occupied)
            };
        }
        opened
            .set_bytes(name, value.ty.into(), &value.data)
            .map_err(|e| registry_failure(&location, e))
    }

    fn export_service(&mut self, name: &str) -> Done<KeyNode> {
        export_key(Hive::Hklm, &service_key(name)?)
    }

    fn delete_service(&mut self, name: &str) -> Done<()> {
        service_key(name)?;
        delete_service(name)
    }

    fn restore_service(&mut self, name: &str, key: &KeyNode, merge: bool) -> Done<()> {
        restore_service(name, key, merge)
    }

    fn export_task(&mut self, path: &str) -> Done<String> {
        export_task(path)
    }

    fn delete_task(&mut self, path: &str) -> Done<()> {
        delete_task(path)
    }

    fn restore_task(&mut self, path: &str, xml: &str, merge: bool) -> Done<()> {
        restore_task(path, xml, merge)
    }

    fn export_firewall_rule(&mut self, name: &str) -> Done<Vec<(String, String)>> {
        export_firewall_rule(name)
    }

    fn delete_firewall_rule(&mut self, name: &str, rules: &[(String, String)]) -> Done<()> {
        delete_firewall_rule(name, rules)
    }

    fn restore_firewall_rule(
        &mut self,
        name: &str,
        rules: &[(String, String)],
        merge: bool,
    ) -> Done<()> {
        restore_firewall_rule(name, rules, merge)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::{HoldItem, HoldReport, REG_BINARY, REG_NONE, REG_QWORD, hold, restore};
    use super::*;
    use crate::model::ItemKind;

    fn value(ty: u32, data: &[u8]) -> RegValue {
        RegValue {
            ty,
            data: data.to_vec(),
        }
    }

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    fn sorted(mut node: KeyNode) -> KeyNode {
        node.values.sort_by(|a, b| a.0.cmp(&b.0));
        node.subkeys.sort_by(|a, b| a.0.cmp(&b.0));
        node.subkeys = node
            .subkeys
            .into_iter()
            .map(|(n, k)| (n, sorted(k)))
            .collect();
        node
    }

    /// A key of its own under `HKCU\Software\BrokeyTest`, removed again
    /// when the test ends, whatever happens.
    pub(crate) struct TestKey(pub(crate) String);

    impl TestKey {
        pub(crate) fn new(name: &str) -> TestKey {
            TestKey(format!(
                r"Software\BrokeyTest\{name}-{}",
                std::process::id()
            ))
        }
    }

    impl Drop for TestKey {
        fn drop(&mut self) {
            let _ = windows_registry::CURRENT_USER.remove_tree(&self.0);
            // The parent goes too once no other test is using it.
            if let Ok(parent) = windows_registry::CURRENT_USER.open(r"Software\BrokeyTest")
                && parent.keys().is_ok_and(|mut k| k.next().is_none())
            {
                let _ = windows_registry::CURRENT_USER.remove_tree(r"Software\BrokeyTest");
            }
        }
    }

    /// This user's SID, for a link's target, which is written in the
    /// kernel's own spelling of the registry.
    fn my_sid() -> String {
        let out = std::process::Command::new("whoami")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        text.trim()
            .rsplit(',')
            .next()
            .unwrap()
            .trim_matches('"')
            .to_string()
    }

    /// Make `HKCU\<at>` a registry link to `HKCU\<to>`, as anyone who may
    /// create a subkey can.
    pub(crate) fn make_link(at: &str, to: &str) {
        let (parent, name) = at.rsplit_once('\\').unwrap();
        let parent = windows_registry::CURRENT_USER.create(parent).unwrap();
        let name = wide_str(name);
        let mut handle: HKEY = ptr::null_mut();
        // SAFETY: test-only; an open key, NUL-terminated strings, out
        // parameters.
        unsafe {
            let code = RegCreateKeyExW(
                parent.as_raw() as HKEY,
                name.as_ptr(),
                0,
                ptr::null(),
                0x2, // REG_OPTION_CREATE_LINK
                0xF003F | 0x20,
                ptr::null(),
                &mut handle,
                ptr::null_mut(),
            );
            assert_eq!(code, ERROR_SUCCESS, "a link could be made in HKCU");
            let target: Vec<u16> = format!(r"\Registry\User\{}\{to}", my_sid())
                .encode_utf16()
                .collect();
            let value = wide_str("SymbolicLinkValue");
            let code = windows_sys::Win32::System::Registry::RegSetValueExW(
                handle,
                value.as_ptr(),
                0,
                REG_LINK,
                target.as_ptr().cast(),
                (target.len() * 2) as u32,
            );
            assert_eq!(code, ERROR_SUCCESS);
            windows_sys::Win32::System::Registry::RegCloseKey(handle);
        }
    }

    /// Delete the link itself, never what it leads to.
    pub(crate) fn remove_link(at: &str) {
        let (parent, name) = at.rsplit_once('\\').unwrap();
        let Ok(parent) = windows_registry::CURRENT_USER.open(parent) else {
            return;
        };
        if let Ok(link) = open_child(&parent, name, KEY_DELETE, false) {
            // SAFETY: an open key with DELETE access.
            unsafe { NtDeleteKey(link.as_raw() as _) };
        }
    }

    /// A link planted inside a key, or on the way to one, is never gone
    /// through: not to read it, not to delete it, not to write beside it.
    /// What it leads to is left exactly as it was, and so is the key.
    #[test]
    fn a_registry_link_is_never_followed() {
        let key = TestKey::new("links");
        let target = format!(r"{}\Target", key.0);
        let item = format!(r"{}\Item", key.0);
        let inside = format!(r"{item}\Link");
        let on_the_way = format!(r"{}\Way", key.0);
        windows_registry::CURRENT_USER
            .create(format!(r"{target}\Inner"))
            .unwrap()
            .set_u32("Keep", 1)
            .unwrap();
        windows_registry::CURRENT_USER
            .create(&item)
            .unwrap()
            .set_u32("Own", 2)
            .unwrap();
        make_link(&inside, &target);
        make_link(&on_the_way, &target);
        let mut m = LiveMutate::new(PathBuf::from(r"C:\nowhere"));

        let deleted = m.delete_key(Hive::Hkcu, &item);
        assert!(
            matches!(&deleted, Err(Failure::Failed(s)) if s.contains("registry link")),
            "{deleted:?}"
        );
        assert!(m.export_key(Hive::Hkcu, &item).is_err());
        let through = format!(r"{on_the_way}\Inner");
        assert!(matches!(
            m.export_key(Hive::Hkcu, &through),
            Err(Failure::Failed(s)) if s.contains("registry link")
        ));
        assert_eq!(key_way(Hive::Hkcu, &through), KeyWay::Link);
        assert_eq!(key_way(Hive::Hkcu, &item), KeyWay::Clear);
        assert_eq!(
            key_way(Hive::Hkcu, &format!(r"{item}\Absent")),
            KeyWay::Missing
        );
        assert!(m.delete_key(Hive::Hkcu, &through).is_err());
        assert!(m.delete_value(Hive::Hkcu, &through, "Keep").is_err());
        let restored = m.restore_key(
            Hive::Hkcu,
            &format!(r"{on_the_way}\New"),
            &KeyNode::default(),
            false,
        );
        assert!(restored.is_err(), "{restored:?}");

        // Nothing was deleted, followed or written.
        let inner = windows_registry::CURRENT_USER
            .open(format!(r"{target}\Inner"))
            .unwrap();
        assert_eq!(inner.get_u32("Keep").unwrap(), 1);
        assert!(
            windows_registry::CURRENT_USER
                .open(format!(r"{target}\New"))
                .is_err()
        );
        assert_eq!(
            windows_registry::CURRENT_USER
                .open(&item)
                .unwrap()
                .get_u32("Own")
                .unwrap(),
            2
        );

        remove_link(&inside);
        remove_link(&on_the_way);
        // With the link gone, the key goes as before.
        assert_eq!(m.delete_key(Hive::Hkcu, &item), Ok(()));
        assert!(windows_registry::CURRENT_USER.open(&target).is_ok());
    }

    /// The two links Windows keeps on these paths are followed.
    #[test]
    fn the_links_windows_keeps_are_followed() {
        assert_eq!(key_way(Hive::Hkcu, r"Software\Classes"), KeyWay::Clear);
        assert_eq!(
            key_way(Hive::Hklm, r"SYSTEM\CurrentControlSet\Services"),
            KeyWay::Clear
        );
    }

    #[test]
    fn registry_round_trips_every_value_type_in_a_test_key() {
        let key = TestKey::new("round-trip");
        let mut multi = utf16("one");
        multi.extend(utf16("two"));
        multi.extend([0, 0]);
        let node = KeyNode {
            values: vec![
                (String::new(), value(REG_SZ, &utf16("default"))),
                ("Sz".into(), value(REG_SZ, &utf16(r"C:\Acme"))),
                ("Expand".into(), value(REG_EXPAND_SZ, &utf16("%TEMP%"))),
                ("Multi".into(), value(REG_MULTI_SZ, &multi)),
                ("Dword".into(), value(REG_DWORD, &7u32.to_le_bytes())),
                ("Qword".into(), value(REG_QWORD, &u64::MAX.to_le_bytes())),
                ("Binary".into(), value(REG_BINARY, &[0, 1, 0xfe, 0xff])),
                ("None".into(), value(REG_NONE, &[])),
                ("NoneData".into(), value(REG_NONE, &[9, 9])),
            ],
            subkeys: vec![(
                "Sub".into(),
                KeyNode {
                    values: vec![("x".into(), value(REG_SZ, &utf16("y")))],
                    subkeys: vec![("Empty".into(), KeyNode::default())],
                },
            )],
        };
        let mut m = LiveMutate::default();
        assert_eq!(m.restore_key(Hive::Hkcu, &key.0, &node, false), Ok(()));
        assert_eq!(
            m.restore_key(Hive::Hkcu, &key.0, &node, false),
            Err(Failure::Occupied)
        );

        let exported = m.export_key(Hive::Hkcu, &key.0).unwrap();
        assert_eq!(sorted(exported.clone()), sorted(node.clone()));
        assert_eq!(m.delete_key(Hive::Hkcu, &key.0), Ok(()));
        assert_eq!(m.export_key(Hive::Hkcu, &key.0), Err(Failure::Missing));
        assert_eq!(m.restore_key(Hive::Hkcu, &key.0, &exported, false), Ok(()));
        let again = m.export_key(Hive::Hkcu, &key.0).unwrap();
        assert_eq!(sorted(again), sorted(node));

        let held = m.export_value(Hive::Hkcu, &key.0, "Multi").unwrap();
        assert_eq!(held, value(REG_MULTI_SZ, &multi));
        assert_eq!(m.delete_value(Hive::Hkcu, &key.0, "Multi"), Ok(()));
        assert_eq!(
            m.export_value(Hive::Hkcu, &key.0, "Multi"),
            Err(Failure::Missing)
        );
        assert_eq!(
            m.restore_value(Hive::Hkcu, &key.0, "Multi", &held, false),
            Ok(())
        );
        assert_eq!(
            m.restore_value(Hive::Hkcu, &key.0, "Multi", &held, false),
            Err(Failure::Occupied)
        );
    }

    #[test]
    fn a_file_another_program_has_open_is_named_and_the_rest_is_held() {
        use std::os::windows::fs::OpenOptionsExt;
        let temp = tempfile::tempdir().unwrap();
        let disk = temp.path().join("disk");
        std::fs::create_dir_all(disk.join("App")).unwrap();
        std::fs::write(disk.join("App").join("a.dat"), "a").unwrap();
        std::fs::write(disk.join("notes.txt"), "hello").unwrap();
        std::fs::write(disk.join("running.log"), "busy").unwrap();
        let key = TestKey::new("locked");
        windows_registry::CURRENT_USER
            .create(&key.0)
            .unwrap()
            .set_u32("Count", 3)
            .unwrap();
        // Open with no sharing at all, as a running program holds its log.
        let open = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(disk.join("running.log"))
            .unwrap();

        let root = temp.path().join("holding");
        let mut m = LiveMutate::new(temp.path().join("machine"));
        let item = |kind, location: String| HoldItem { kind, location };
        let shown = |p: PathBuf| p.display().to_string();
        let items = vec![
            item(ItemKind::File, shown(disk.join("notes.txt"))),
            item(ItemKind::File, shown(disk.join("running.log"))),
            item(ItemKind::Folder, shown(disk.join("App"))),
            item(ItemKind::RegistryKey, format!(r"HKCU\{}", key.0)),
        ];
        let report = hold(&root, "locked-1", "Acme", &items, &mut m);
        assert_eq!(report.held, 3, "{report:?}");
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, shown(disk.join("running.log")));
        assert!(report.failed[0].1.contains("in use"), "{:?}", report.failed);
        assert!(windows_registry::CURRENT_USER.open(&key.0).is_err());

        drop(open);
        let report = restore(&root, "locked-1", &mut m);
        assert_eq!(
            report,
            HoldReport {
                held: 3,
                failed: vec![]
            }
        );
        assert_eq!(
            std::fs::read_to_string(disk.join("App").join("a.dat")).unwrap(),
            "a"
        );
        assert_eq!(
            windows_registry::CURRENT_USER
                .open(&key.0)
                .unwrap()
                .get_u32("Count")
                .unwrap(),
            3
        );
    }

    #[test]
    fn a_folder_copied_across_drives_with_a_file_in_use_is_left_whole() {
        use std::os::windows::fs::OpenOptionsExt;
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("disk").join("App");
        std::fs::create_dir_all(app.join("sub")).unwrap();
        std::fs::write(app.join("a.dat"), "a").unwrap();
        std::fs::write(app.join("sub").join("b.dat"), "bb").unwrap();
        std::fs::write(app.join("sub").join("running.log"), "busy").unwrap();
        // Readable, so the copy succeeds; not deletable, so the removal
        // fails part-way, after other files may already be gone.
        let open = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(app.join("sub").join("running.log"))
            .unwrap();
        let root = temp.path().join("holding");
        let mut m = LiveMutate {
            force_copy: true,
            ..LiveMutate::new(temp.path().join("machine"))
        };
        let items = vec![HoldItem {
            kind: ItemKind::Folder,
            location: app.display().to_string(),
        }];
        let report = hold(&root, "copy-1", "Acme", &items, &mut m);
        drop(open);
        assert_eq!(report.held, 0);
        assert!(report.failed[0].1.contains("in use"), "{:?}", report.failed);
        for (file, text) in [
            ("a.dat", "a"),
            ("sub/b.dat", "bb"),
            ("sub/running.log", "busy"),
        ] {
            assert_eq!(
                std::fs::read_to_string(app.join(file)).unwrap(),
                text,
                "{file}"
            );
        }
        assert!(!root.join("copy-1").exists(), "no half copy is left behind");
    }

    #[test]
    fn the_machine_root_is_made_for_administrators_only_or_not_at_all() {
        use windows_sys::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Brokey").join("Holding");
        match create_admin_only_dir(&root, 1) {
            // Not elevated: Administrators cannot be made the owner, and
            // no folder is left behind with weaker permissions.
            Err(Failure::Failed(sentence)) => {
                assert!(!root.parent().unwrap().exists(), "{sentence}");
                assert!(
                    sentence.contains("only Administrators can change"),
                    "{sentence}"
                );
            }
            Err(other) => panic!("{other:?}"),
            Ok(()) => {
                let name = wide(&root);
                let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
                let mut text: windows_sys::core::PWSTR = ptr::null_mut();
                // SAFETY: out parameters, each freed with `LocalFree`.
                let sddl = unsafe {
                    let info = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
                    assert_eq!(
                        GetNamedSecurityInfoW(
                            name.as_ptr(),
                            SE_FILE_OBJECT,
                            info,
                            ptr::null_mut(),
                            ptr::null_mut(),
                            ptr::null_mut(),
                            ptr::null_mut(),
                            &mut descriptor,
                        ),
                        0
                    );
                    assert_ne!(
                        ConvertSecurityDescriptorToStringSecurityDescriptorW(
                            descriptor,
                            SDDL_REVISION_1,
                            info,
                            &mut text,
                            ptr::null_mut(),
                        ),
                        0
                    );
                    let len = (0..).take_while(|&i| *text.add(i) != 0).count();
                    let sddl = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
                    LocalFree(text as HLOCAL);
                    LocalFree(descriptor as HLOCAL);
                    sddl
                };
                assert!(sddl.starts_with("O:BA"), "{sddl}");
                assert!(sddl.contains("D:P"), "{sddl}");
                assert!(sddl.contains("(A;OICI;FA;;;BA)"), "{sddl}");
                assert!(sddl.contains("(A;OICI;FA;;;SY)"), "{sddl}");
                assert!(sddl.contains("(A;OICI;FR;;;AU)"), "{sddl}");
                assert_eq!(sddl.matches("(A;").count(), 3, "{sddl}");
            }
        }
    }

    /// Put a DACL from SDDL on a folder this user owns.
    pub(crate) fn set_dacl(path: &Path, sddl: &str) {
        let sddl = wide(sddl);
        let name = wide(path);
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let mut dacl: *mut ACL = ptr::null_mut();
        let (mut present, mut defaulted) = (0, 0);
        // SAFETY: out parameters; the descriptor is freed once at the end.
        unsafe {
            assert_ne!(
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                ),
                0
            );
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted);
            let code = SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl,
                ptr::null(),
            );
            LocalFree(descriptor as HLOCAL);
            assert_eq!(code, 0);
        }
    }

    #[test]
    fn a_brokey_folder_someone_else_made_is_taken_back_or_refused() {
        let temp = tempfile::tempdir().unwrap();
        let brokey = temp.path().join("Brokey");
        // Made first by this user, as a standard user can make
        // `%ProgramData%\Brokey` before Brokey ever runs.
        std::fs::create_dir_all(&brokey).unwrap();
        let root = brokey.join("Holding");
        match create_admin_only_dir(&root, 1) {
            Ok(()) => {
                // Elevated: both were taken back.
                assert!(is_admin_only(&brokey).unwrap());
                assert!(is_admin_only(&root).unwrap());
            }
            Err(Failure::Failed(sentence)) => {
                assert!(
                    sentence.contains(&brokey.display().to_string()),
                    "{sentence}"
                );
                assert!(!root.exists(), "nothing is made inside it");
            }
            Err(other) => panic!("{other:?}"),
        }
        assert!(
            !is_admin_only(temp.path()).unwrap_or(false),
            "a folder this user owns is not admin-only"
        );
    }

    #[test]
    fn an_entry_in_the_machine_root_that_someone_else_made_is_not_trusted() {
        let temp = tempfile::tempdir().unwrap();
        let machine = temp.path().join("machine");
        let entry = machine.join("acme-1");
        std::fs::create_dir_all(&entry).unwrap();
        let mut m = LiveMutate::new(machine.clone());
        let refused = m.trust_entry(&machine, &entry);
        assert!(
            matches!(&refused, Err(Failure::Failed(s)) if s.contains("not made by Brokey")),
            "{refused:?}"
        );
        // The user's own root has no such rule.
        assert_eq!(m.trust_entry(temp.path(), &entry), Ok(()));
        // And neither hold nor restore will use it.
        let report = super::super::restore(&machine, "acme-1", &mut m);
        assert_eq!(report.held, 0);
        assert!(
            report.failed[0].1.contains("not made by Brokey"),
            "{report:?}"
        );
    }

    #[test]
    fn a_key_left_in_part_is_merged_back_without_replacing_what_is_there() {
        let key = TestKey::new("merge");
        let node = KeyNode {
            values: vec![
                ("Kept".into(), value(REG_SZ, &utf16("old"))),
                ("Gone".into(), value(REG_DWORD, &1u32.to_le_bytes())),
            ],
            subkeys: vec![(
                "Sub".into(),
                KeyNode {
                    values: vec![("x".into(), value(REG_SZ, &utf16("y")))],
                    subkeys: vec![],
                },
            )],
        };
        // What is left after a delete that failed part-way, with one value
        // changed since by someone else.
        let left = windows_registry::CURRENT_USER.create(&key.0).unwrap();
        left.set_string("Kept", "new").unwrap();
        let mut m = LiveMutate::default();
        assert_eq!(
            m.restore_key(Hive::Hkcu, &key.0, &node, false),
            Err(Failure::Occupied)
        );
        assert_eq!(m.restore_key(Hive::Hkcu, &key.0, &node, true), Ok(()));
        assert_eq!(left.get_string("Kept").unwrap(), "new");
        assert_eq!(left.get_u32("Gone").unwrap(), 1);
        assert_eq!(left.open("Sub").unwrap().get_string("x").unwrap(), "y");
    }

    #[test]
    fn a_link_where_the_machine_root_goes_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        let link = temp.path().join("Holding");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let result = create_admin_only_dir(&link, 0);
        assert!(
            matches!(&result, Err(Failure::Failed(s)) if s.contains("not a plain folder")),
            "{result:?}"
        );
    }

    #[test]
    fn task_paths_are_split_and_odd_ones_refused() {
        assert_eq!(
            task_parts(r"\Acme\Update"),
            Some((r"\Acme".into(), "Update".into()))
        );
        assert_eq!(task_parts(r"\Update"), Some((r"\".into(), "Update".into())));
        for bad in [
            "Update",
            r"\",
            r"\Acme\",
            r"\Acme\..\Microsoft",
            r"\a/b",
            r"\\x",
        ] {
            assert_eq!(task_parts(bad), None, "{bad}");
        }
    }

    #[test]
    fn task_files_are_read_as_windows_writes_them_and_as_utf8() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><Task>\u{e9}</Task>";
        let mut utf16_file = vec![0xFF, 0xFE];
        utf16_file.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode_task_xml(&utf16_file).as_deref(), Some(xml));
        let mut utf8_file = vec![0xEF, 0xBB, 0xBF];
        utf8_file.extend(xml.as_bytes());
        assert_eq!(decode_task_xml(&utf8_file).as_deref(), Some(xml));
        assert_eq!(decode_task_xml(xml.as_bytes()).as_deref(), Some(xml));
        assert_eq!(decode_task_xml(&[0xFF, 0xFE, 0x00, 0xD8]), None);
    }

    #[test]
    fn a_service_key_gives_what_creating_it_again_needs() {
        let mut deps = utf16("RpcSs");
        deps.extend(utf16("Tcpip"));
        deps.extend([0, 0]);
        let mut groups = utf16("NetBIOSGroup");
        groups.extend([0, 0]);
        let image = "\"%ProgramFiles%\\Acme\\svc.exe\" -s";
        let key = KeyNode {
            values: vec![
                ("Type".into(), value(REG_DWORD, &16u32.to_le_bytes())),
                ("Start".into(), value(REG_DWORD, &2u32.to_le_bytes())),
                ("ErrorControl".into(), value(REG_DWORD, &1u32.to_le_bytes())),
                ("ImagePath".into(), value(REG_EXPAND_SZ, &utf16(image))),
                ("DisplayName".into(), value(REG_SZ, &utf16("Acme updater"))),
                ("ObjectName".into(), value(REG_SZ, &utf16("LocalSystem"))),
                ("DependOnService".into(), value(REG_MULTI_SZ, &deps)),
                ("DependOnGroup".into(), value(REG_MULTI_SZ, &groups)),
            ],
            subkeys: vec![("Parameters".into(), KeyNode::default())],
        };
        assert_eq!(
            ServiceConfig::from_key("AcmeSvc", &key),
            Ok(ServiceConfig {
                service_type: 16,
                start: 2,
                error_control: 1,
                image_path: image.into(),
                display_name: Some("Acme updater".into()),
                group: None,
                dependencies: vec!["RpcSs".into(), "Tcpip".into(), "+NetBIOSGroup".into()],
                account: Some("LocalSystem".into()),
            })
        );
        let missing = ServiceConfig::from_key("AcmeSvc", &KeyNode::default()).unwrap_err();
        assert!(missing.contains("AcmeSvc"), "{missing}");

        assert!(needs_no_password(None));
        assert!(needs_no_password(Some("LocalSystem")));
        assert!(needs_no_password(Some(r"NT AUTHORITY\LocalService")));
        assert!(needs_no_password(Some(r"NT SERVICE\AcmeSvc")));
        assert!(!needs_no_password(Some(r".\acme-user")));
    }

    #[test]
    fn service_names_never_reach_another_key() {
        let mut m = LiveMutate::default();
        for bad in ["", r"..\..\Software", "a/b"] {
            assert!(
                matches!(m.export_service(bad), Err(Failure::Failed(_))),
                "{bad}"
            );
        }
    }

    fn make_junction(link: &Path, target: &Path) {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "the junction was made");
    }

    /// Hold `disk\parent\app` (one file in it) into `holding`.
    fn held_app(temp: &Path, m: &mut LiveMutate) -> PathBuf {
        let app = temp.join("disk").join("parent").join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("a.dat"), "a").unwrap();
        let items = vec![HoldItem {
            kind: ItemKind::Folder,
            location: app.display().to_string(),
        }];
        let report = hold(&temp.join("holding"), "bound-1", "Acme", &items, m);
        assert_eq!(report.held, 1, "{report:?}");
        assert!(!app.exists());
        app
    }

    /// A folder on the way back that went missing and was made again as a
    /// junction: the restore is refused rather than writing through it, by
    /// a rename and by a copy across drives alike.
    #[test]
    fn a_restore_whose_missing_parent_is_now_a_junction_is_refused() {
        for force_copy in [false, true] {
            for swap in ["parent", "disk"] {
                let temp = tempfile::tempdir().unwrap();
                let mut m = LiveMutate {
                    force_copy,
                    ..LiveMutate::new(temp.path().join("machine"))
                };
                let app = held_app(temp.path(), &mut m);
                let elsewhere = temp.path().join("elsewhere");
                std::fs::create_dir_all(&elsewhere).unwrap();
                let swapped = if swap == "parent" {
                    app.parent().unwrap().to_path_buf()
                } else {
                    temp.path().join("disk")
                };
                std::fs::remove_dir_all(&swapped).unwrap();
                make_junction(&swapped, &elsewhere);

                let report = restore(&temp.path().join("holding"), "bound-1", &mut m);
                assert_eq!(report.held, 0, "{report:?}");
                assert!(
                    report.failed[0].1.contains("leads somewhere other"),
                    "{report:?}"
                );
                assert_eq!(
                    std::fs::read_dir(&elsewhere).unwrap().count(),
                    0,
                    "nothing was written through the junction ({force_copy}, {swap})"
                );
                // Still held, and restorable once the way back is real.
                std::fs::remove_dir(&swapped).unwrap();
                let report = restore(&temp.path().join("holding"), "bound-1", &mut m);
                assert_eq!(report.failed, vec![], "{force_copy}, {swap}");
                assert_eq!(std::fs::read_to_string(app.join("a.dat")).unwrap(), "a");
            }
        }
    }

    /// The folders above an item are held still from the check until it has
    /// moved: the hook stands where a swap would have to happen, and the swap
    /// fails there.
    #[test]
    fn a_folder_above_an_item_cannot_be_swapped_while_it_moves() {
        static TRIED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        fn try_to_swap(item: &Path) {
            TRIED.store(true, std::sync::atomic::Ordering::SeqCst);
            let parent = item.parent().unwrap();
            let away = parent.with_file_name("swapped-away");
            assert!(
                std::fs::rename(parent, &away).is_err(),
                "the folder above was held still"
            );
            assert!(
                std::fs::rename(
                    parent.parent().unwrap(),
                    parent.parent().unwrap().with_file_name("gone")
                )
                .is_err(),
                "and the one above that"
            );
            assert!(
                std::fs::rename(item, item.with_file_name("renamed")).is_err(),
                "and the item itself"
            );
        }
        let temp = tempfile::tempdir().unwrap();
        let mut m = LiveMutate {
            between: Some(try_to_swap),
            ..LiveMutate::new(temp.path().join("machine"))
        };
        let app = held_app(temp.path(), &mut m);
        assert!(
            TRIED.load(std::sync::atomic::Ordering::SeqCst),
            "the swap was tried"
        );
        assert!(app.parent().unwrap().is_dir());
        assert!(!temp.path().join("swapped-away").exists());
    }

    /// A folder swapped for a junction after the closed list checked it, and
    /// before the helper moved it: the move is refused and nothing at the
    /// junction's target moves.
    #[test]
    fn a_move_through_a_folder_swapped_after_the_check_is_refused() {
        for force_copy in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let real = temp.path().join("disk").join("vendor");
            std::fs::create_dir_all(real.join("app")).unwrap();
            let elsewhere = temp.path().join("elsewhere");
            std::fs::create_dir_all(elsewhere.join("app")).unwrap();
            std::fs::write(elsewhere.join("app").join("system.dat"), "x").unwrap();
            // The swap.
            std::fs::rename(&real, temp.path().join("disk").join("vendor-old")).unwrap();
            make_junction(&real, &elsewhere);

            let mut m = LiveMutate {
                force_copy,
                ..LiveMutate::new(temp.path().join("machine"))
            };
            let items = vec![HoldItem {
                kind: ItemKind::Folder,
                location: real.join("app").display().to_string(),
            }];
            let report = hold(
                &temp.path().join("holding"),
                "bound-1",
                "Acme",
                &items,
                &mut m,
            );
            assert_eq!(report.held, 0, "{report:?}");
            assert!(
                report.failed[0].1.contains("leads somewhere other"),
                "{report:?}"
            );
            assert_eq!(
                std::fs::read_to_string(elsewhere.join("app").join("system.dat")).unwrap(),
                "x"
            );
        }
    }
}
