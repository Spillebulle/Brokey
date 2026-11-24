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
    firewall_rule_name, move_file_or_folder, path_text,
};
use crate::cleanup::view::Hive;
use crate::cleanup::view::windows::{long_path, wow6432};
use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use windows_sys::Win32::Foundation::{GetLastError, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
    SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{CreateDirectoryW, DELETE, MoveFileExW};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, ControlService, CreateServiceW, DeleteService, OpenSCManagerW,
    OpenServiceW, QueryServiceStatus, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE,
    SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_STATUS, SERVICE_STOP, SERVICE_STOPPED,
};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_ProgramData, FOLDERID_System, SHGetKnownFolderPath,
};

const SERVICES: &str = r"SYSTEM\CurrentControlSet\Services";
const FIREWALL_RULES: &str =
    r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules";
/// Owned by Administrators; only Administrators and SYSTEM may open it, and
/// the parent's permissions are not inherited.
pub const ADMIN_ONLY_SDDL: &str = "O:BAD:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)";
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

/// Make `path` a folder that only Administrators and SYSTEM can open, owned
/// by Administrators. A folder already there has those permissions put on
/// it, and a link there is refused rather than followed. Not being an
/// Administrator makes this fail; it never makes a weaker folder.
pub fn create_admin_only_dir(path: &Path) -> Done<()> {
    let shown = path.display();
    let failed = |e: io::Error| {
        Failure::Failed(format!(
            "Brokey could not make the holding area {shown} for Administrators only ({e}), so \
             nothing was moved. Try again."
        ))
    };
    let sddl = wide(ADMIN_ONLY_SDDL);
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated and `descriptor` an out parameter,
    // freed with `LocalFree` below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(failed(io::Error::last_os_error()));
    }
    let result = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            Err(Failure::Failed(format!(
                "{shown} is not a plain folder, so Brokey will not keep things there. Remove it \
                 and try again."
            )))
        }
        Ok(_) => apply_descriptor(path, descriptor).map_err(failed),
        Err(_) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            let name = wide(long_path(path));
            // SAFETY: `name` is NUL-terminated; `attributes` and the
            // descriptor it points to outlive the call.
            if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } == 0 {
                Err(failed(io::Error::last_os_error()))
            } else {
                Ok(())
            }
        }
    };
    // SAFETY: allocated by the conversion above, freed once.
    unsafe { LocalFree(descriptor as HLOCAL) };
    result
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

fn open(hive: Hive, path: &str, write: bool) -> Done<windows_registry::Key> {
    let (root, sub) = root_of(hive, path);
    let mut options = root.options();
    options.read();
    if write {
        options.write();
    }
    options
        .open(&sub)
        .map_err(|e| registry_failure(&format!(r"{}\{path}", hive_word(hive)), e))
}

fn read_node(key: &windows_registry::Key) -> windows_registry::Result<KeyNode> {
    let mut node = KeyNode::default();
    for (name, value) in key.values()? {
        node.values.push((
            name,
            RegValue {
                ty: value.ty().into(),
                data: value.to_vec(),
            },
        ));
    }
    for name in key.keys()? {
        let sub = key.open(&name)?;
        node.subkeys.push((name, read_node(&sub)?));
    }
    Ok(node)
}

fn write_node(key: &windows_registry::Key, node: &KeyNode) -> windows_registry::Result<()> {
    for (name, value) in &node.values {
        key.set_bytes(name, value.ty.into(), &value.data)?;
    }
    for (name, sub) in &node.subkeys {
        write_node(&key.create(name)?, sub)?;
    }
    Ok(())
}

fn export_key(hive: Hive, path: &str) -> Done<KeyNode> {
    let location = format!(r"{}\{path}", hive_word(hive));
    let key = open(hive, path, false)?;
    read_node(&key).map_err(|e| registry_failure(&location, e))
}

fn delete_key(hive: Hive, path: &str) -> Done<()> {
    let (root, sub) = root_of(hive, path);
    root.remove_tree(&sub)
        .map_err(|e| registry_failure(&format!(r"{}\{path}", hive_word(hive)), e))
}

fn restore_key(hive: Hive, path: &str, node: &KeyNode) -> Done<()> {
    let location = format!(r"{}\{path}", hive_word(hive));
    match open(hive, path, false) {
        Ok(_) => return Err(Failure::Occupied),
        Err(Failure::Missing) => {}
        Err(f) => return Err(f),
    }
    let (root, sub) = root_of(hive, path);
    let written = root.create(&sub).and_then(|key| write_node(&key, node));
    written.map_err(|e| {
        // Half a key is worse than none: take back what was written.
        let _ = root.remove_tree(&sub);
        registry_failure(&location, e)
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

fn restore_service(name: &str, key: &KeyNode) -> Done<()> {
    let path = service_key(name)?;
    let config = ServiceConfig::from_key(name, key).map_err(Failure::Failed)?;
    let manager = scm(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE, name)?;
    if open_service(&manager, name, SERVICE_QUERY_STATUS).is_ok() {
        return Err(Failure::Occupied);
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
    let written = windows_registry::LOCAL_MACHINE
        .create(&path)
        .and_then(|k| write_node(&k, key));
    written.map_err(|e| {
        let e: io::Error = e.into();
        Failure::Failed(format!(
            "Service {name} was put back, but not all of its settings ({e}). Check it in Services."
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

fn restore_task(path: &str, xml: &str) -> Done<()> {
    task_parts(path).ok_or_else(|| bad_task(path))?;
    if com::exists(path).map_err(|e| task_failure(path, &e))? {
        return Err(Failure::Occupied);
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

fn restore_firewall_rule(name: &str, rules: &[(String, String)]) -> Done<()> {
    let location = format!("firewall:{name}");
    let key = open(Hive::Hklm, FIREWALL_RULES, true)?;
    if rules.iter().any(|(id, _)| key.get_type(id).is_ok()) {
        return Err(Failure::Occupied);
    }
    for (id, rule) in rules {
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
}

impl Default for LiveMutate {
    fn default() -> Self {
        LiveMutate {
            machine_root: super::machine_root(),
            force_copy: false,
        }
    }
}

impl Mutate for LiveMutate {
    fn make_root(&mut self, root: &Path) -> Done<()> {
        if path_text(root) == path_text(&self.machine_root) {
            return create_admin_only_dir(root);
        }
        std::fs::create_dir_all(root).map_err(|e| {
            Failure::Failed(format!(
                "Brokey could not make the holding area {} ({e}), so nothing was moved. Try again.",
                root.display()
            ))
        })
    }

    fn move_path(&mut self, from: &Path, to: &Path) -> Done<()> {
        move_file_or_folder(from, to, self.force_copy)
    }

    fn export_key(&mut self, hive: Hive, path: &str) -> Done<KeyNode> {
        export_key(hive, path)
    }

    fn delete_key(&mut self, hive: Hive, path: &str) -> Done<()> {
        delete_key(hive, path)
    }

    fn restore_key(&mut self, hive: Hive, path: &str, key: &KeyNode) -> Done<()> {
        restore_key(hive, path, key)
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

    fn restore_value(&mut self, hive: Hive, key: &str, name: &str, value: &RegValue) -> Done<()> {
        let location = format!(r"{}\{key} | {name}", hive_word(hive));
        let (root, sub) = root_of(hive, key);
        let opened = root
            .create(&sub)
            .map_err(|e| registry_failure(&location, e))?;
        if opened.get_type(name).is_ok() {
            return Err(Failure::Occupied);
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

    fn restore_service(&mut self, name: &str, key: &KeyNode) -> Done<()> {
        restore_service(name, key)
    }

    fn export_task(&mut self, path: &str) -> Done<String> {
        export_task(path)
    }

    fn delete_task(&mut self, path: &str) -> Done<()> {
        delete_task(path)
    }

    fn restore_task(&mut self, path: &str, xml: &str) -> Done<()> {
        restore_task(path, xml)
    }

    fn export_firewall_rule(&mut self, name: &str) -> Done<Vec<(String, String)>> {
        export_firewall_rule(name)
    }

    fn delete_firewall_rule(&mut self, name: &str, rules: &[(String, String)]) -> Done<()> {
        delete_firewall_rule(name, rules)
    }

    fn restore_firewall_rule(&mut self, name: &str, rules: &[(String, String)]) -> Done<()> {
        restore_firewall_rule(name, rules)
    }
}

#[cfg(test)]
mod tests {
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
    struct TestKey(String);

    impl TestKey {
        fn new(name: &str) -> TestKey {
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
        assert_eq!(m.restore_key(Hive::Hkcu, &key.0, &node), Ok(()));
        assert_eq!(
            m.restore_key(Hive::Hkcu, &key.0, &node),
            Err(Failure::Occupied)
        );

        let exported = m.export_key(Hive::Hkcu, &key.0).unwrap();
        assert_eq!(sorted(exported.clone()), sorted(node.clone()));
        assert_eq!(m.delete_key(Hive::Hkcu, &key.0), Ok(()));
        assert_eq!(m.export_key(Hive::Hkcu, &key.0), Err(Failure::Missing));
        assert_eq!(m.restore_key(Hive::Hkcu, &key.0, &exported), Ok(()));
        let again = m.export_key(Hive::Hkcu, &key.0).unwrap();
        assert_eq!(sorted(again), sorted(node));

        let held = m.export_value(Hive::Hkcu, &key.0, "Multi").unwrap();
        assert_eq!(held, value(REG_MULTI_SZ, &multi));
        assert_eq!(m.delete_value(Hive::Hkcu, &key.0, "Multi"), Ok(()));
        assert_eq!(
            m.export_value(Hive::Hkcu, &key.0, "Multi"),
            Err(Failure::Missing)
        );
        assert_eq!(m.restore_value(Hive::Hkcu, &key.0, "Multi", &held), Ok(()));
        assert_eq!(
            m.restore_value(Hive::Hkcu, &key.0, "Multi", &held),
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
        let mut m = LiveMutate {
            machine_root: temp.path().join("machine"),
            force_copy: false,
        };
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
            machine_root: temp.path().join("machine"),
            force_copy: true,
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
        match create_admin_only_dir(&root) {
            // Not elevated: Administrators cannot be made the owner, and
            // no folder is left behind with weaker permissions.
            Err(Failure::Failed(sentence)) => {
                assert!(!root.exists(), "{sentence}");
                assert!(sentence.contains("Administrators only"), "{sentence}");
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
                assert_eq!(sddl.matches("(A;").count(), 2, "{sddl}");
            }
        }
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
        let result = create_admin_only_dir(&link);
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
}
