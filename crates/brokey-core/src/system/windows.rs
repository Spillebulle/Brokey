//! What machine this is, on Windows.
//!
//! `ProductName` in `CurrentVersion` is not read. On the reference machine,
//! which is Windows 11, it says "Windows 10 Pro": Microsoft never updated
//! the value and a great deal of software reports the wrong operating
//! system because of it. `CurrentBuild` is the truth, and 22000 is where
//! Windows 11 begins.

use crate::model::{Platform, SystemInfo};
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
#[cfg(windows)]
use std::ptr;

#[cfg(windows)]
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    MAXIMUM_REPARSE_DATA_BUFFER_SIZE, OPEN_EXISTING,
};
#[cfg(windows)]
use windows_sys::Win32::System::IO::DeviceIoControl;
#[cfg(windows)]
use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
#[cfg(windows)]
use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_APPEXECLINK;

/// The values read from `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion`.
/// Kept apart from the reading so the naming is a pure function with tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistryVersion {
    /// Read but never consulted by [`from_registry_version`]; kept only so
    /// `the_version_comes_from_the_build_not_from_product_name` can set it
    /// to "Windows 10 Pro" against a Windows 11 build and prove the naming
    /// never reads it. See the module note.
    pub product_name: Option<String>,
    pub edition_id: Option<String>,
    pub display_version: Option<String>,
    pub current_build: Option<String>,
    pub ubr: Option<u32>,
}

/// The first build number of Windows 11.
const WINDOWS_11: u32 = 22000;

#[cfg(windows)]
pub fn detect() -> SystemInfo {
    from_registry_version(&read_registry_version())
}

/// The pure half of [`detect`], so it can be tested against a machine that
/// is not this one.
pub fn from_registry_version(v: &RegistryVersion) -> SystemInfo {
    let build: Option<u32> = v.current_build.as_deref().and_then(|b| b.parse().ok());
    let mut name = String::from("Windows");
    if let Some(build) = build {
        name.push(' ');
        name.push_str(if build >= WINDOWS_11 { "11" } else { "10" });
    }
    if let Some(edition) = v.edition_id.as_deref().map(edition_name) {
        name.push(' ');
        name.push_str(edition);
    }
    if let Some(display) = v.display_version.as_deref() {
        name.push(' ');
        name.push_str(display);
    }
    if let Some(build) = build {
        match v.ubr {
            Some(ubr) => name.push_str(&format!(" (build {build}.{ubr})")),
            None => name.push_str(&format!(" (build {build})")),
        }
    }
    SystemInfo {
        distro_id: "windows".to_string(),
        distro_like: Vec::new(),
        pretty_name: name,
        arch: std::env::consts::ARCH.to_string(),
        desktop: None,
        session: None,
        platform: Platform::Windows,
    }
}

/// `EditionID` is a bare word. These are the ones a desktop machine has;
/// anything else is shown as it is written, which is better than dropping it.
fn edition_name(edition_id: &str) -> &str {
    match edition_id {
        "Core" | "CoreN" | "CoreSingleLanguage" => "Home",
        "Professional" | "ProfessionalN" => "Pro",
        "ProfessionalWorkstation" => "Pro for Workstations",
        "Enterprise" | "EnterpriseN" => "Enterprise",
        "Education" | "EducationN" => "Education",
        other => other,
    }
}

#[cfg(windows)]
fn read_registry_version() -> RegistryVersion {
    let Ok(key) =
        windows_registry::LOCAL_MACHINE.open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
    else {
        return RegistryVersion::default();
    };
    RegistryVersion {
        product_name: key.get_string("ProductName").ok(),
        edition_id: key.get_string("EditionID").ok(),
        display_version: key.get_string("DisplayVersion").ok(),
        current_build: key.get_string("CurrentBuild").ok(),
        ubr: key.get_u32("UBR").ok(),
    }
}

/// The first directory on `PATH` holding an executable of that name.
/// Windows has no executable bit: a name without an extension is tried
/// against each extension in `PATHEXT`, in that order, the way the shell
/// does it.
#[cfg(windows)]
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    which_in(name, &path, &pathext)
}

/// The pure half of [`which`], taking the two environment variables as
/// text. Compiled on both platforms so its tests run on both; only the
/// wrapper that reads the real environment is Windows-only.
pub fn which_in(name: &str, path: &str, pathext: &str) -> Option<PathBuf> {
    let has_extension = Path::new(name).extension().is_some();
    for dir in path.split(';').filter(|d| !d.is_empty()) {
        let base = Path::new(dir).join(name);
        if has_extension {
            if base.is_file() {
                return Some(base);
            }
            continue;
        }
        for extension in pathext.split(';').filter(|e| !e.is_empty()) {
            let candidate = Path::new(dir).join(format!("{name}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// `raw` as an [`OwnedHandle`], or `None` where Windows handed one of the
/// two values that are not a handle back.
///
/// `OwnedHandle` carries `rustc_layout_scalar_valid_range` attributes that
/// exclude both null and `INVALID_HANDLE_VALUE`, so wrapping either is
/// undefined behaviour rather than a value that would be caught later. None
/// of the calls in this crate returns null on success, so the null half is
/// unreachable today; it is checked because an invariant nothing states is
/// one the next call site will not know it has to keep.
#[cfg(windows)]
pub(crate) fn owned_handle(raw: HANDLE) -> Option<OwnedHandle> {
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: `raw` is a fresh handle from a call that reported success and
    // that nothing else has taken ownership of, and it is neither of the
    // two values `OwnedHandle` excludes, so this takes it and closes it
    // exactly once.
    Some(unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) })
}

/// The executable an App Execution Alias names, or `path` unchanged.
///
/// `winget.exe` in `%LOCALAPPDATA%\Microsoft\WindowsApps` is not a program.
/// It is a zero-length reparse point whose data names the real executable,
/// and `CreateProcess` follows it when the alias is started. The folder it
/// sits in grants the invoking user full control, measured on the
/// development machine by creating a file there and deleting it again from
/// an unelevated shell, so a step naming the alias names a file a standard
/// user can replace. This answers the file that will really run, which is
/// the one worth asking permission questions about.
///
/// Only version 3 of `IO_REPARSE_TAG_APPEXECLINK` is read. The version is
/// taken from the buffer rather than assumed, and any other value returns
/// `path` unchanged, because where the strings sit after it is not known to
/// be the same.
///
/// Everything else that can go wrong returns `path` unchanged too: a name
/// that is not on the disk, one that is not a reparse point, a reparse
/// point that cannot be opened or read, a tag that is not
/// `IO_REPARSE_TAG_APPEXECLINK`, a buffer that claims more than it carries
/// or holds fewer than three strings, a third string that is not absolute
/// under a drive letter, and a target that is not a file. No path is ever
/// returned that was not found on the disk.
///
/// Whether handing back an unresolved path is the safe direction is not a
/// property of this function, and this comment used to present it as one.
/// It is safe because of where the alias sits.
/// `%LOCALAPPDATA%\Microsoft\WindowsApps` grants the invoking user full
/// control, so the closed list refuses the alias as a program this account
/// can replace, and the failure ends there. Point this at an alias
/// somewhere an unprivileged process cannot write, a machine-wide one or a
/// future layout change, and the same failure hands on a path the closed
/// list will admit, after which `CreateProcess` follows reparse data
/// nothing examined. The day the alias moves, that stops being true, and
/// nothing in the code will notice.
///
/// What this does not do, and does not try to. It does not ask what is in
/// the target, who put it there or whether it is signed; that is the closed
/// list's question. It resolves nothing but an App Execution Alias, so a
/// symbolic link or a junction comes back as it stands. It follows no
/// chain: were the target an alias in turn, the first target is the answer.
/// And it says what was true of the disk at the moment it was asked, as
/// every filesystem answer does.
#[cfg(windows)]
pub fn resolve_app_execution_alias(path: &Path) -> PathBuf {
    alias_target(path).unwrap_or_else(|| path.to_path_buf())
}

/// The target of `path` when it is an App Execution Alias that leads
/// somewhere this machine really has, and `None` in every other case. See
/// [`resolve_app_execution_alias`], which is what turns the `None` back
/// into the original path.
#[cfg(windows)]
fn alias_target(path: &Path) -> Option<PathBuf> {
    use std::os::windows::fs::MetadataExt;

    // `symlink_metadata` asks about the name rather than about what it
    // leads to, which is the only way the attribute is visible at all. A
    // name that is not on the disk fails here.
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return None;
    }
    let target = PathBuf::from(appexeclink_target(&reparse_data(path)?)?);
    // Narrower than the question `transaction::allow` puts to the program
    // it is about to elevate, which also asks the volume what it is: this
    // only refuses a target that names no drive letter at all, a UNC path
    // among them. A target on a mapped network drive comes back from here
    // and is refused there. Then the target has to be a file that is really
    // there, because a path this never saw is a path this must not hand
    // on.
    if !under_a_drive_letter(&target) || !target.is_file() {
        return None;
    }
    Some(target)
}

/// Whether `path` is absolute under a drive letter, `X:\...` or the
/// `\\?\X:\...` form, rather than merely absolute. A UNC path is absolute
/// too, and a target of `\\somewhere\share\winget.exe` is one this would be
/// starting over the network.
#[cfg(windows)]
fn under_a_drive_letter(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    path.is_absolute()
        && matches!(
            path.components().next(),
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
        )
}

/// The raw reparse buffer of `path`, cut to the number of bytes Windows
/// says it wrote, or `None` when it cannot be read.
///
/// Two Win32 calls. `CreateFileW` opens the name itself rather than what it
/// leads to, and `DeviceIoControl` with `FSCTL_GET_REPARSE_POINT` copies the
/// reparse data out. Nothing outlives this function: the handle becomes an
/// `OwnedHandle` on the line after it is checked, so it is closed exactly
/// once on every path out of here, and the buffer is one `Vec<u8>` freed by
/// the same rule.
#[cfg(windows)]
fn reparse_data(path: &Path) -> Option<Vec<u8>> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // No access rights are asked for. A handle opened for nothing still
    // carries the metadata this needs, and asking for read access would
    // fail on a file whose contents are not this account's to read.
    // `FILE_FLAG_OPEN_REPARSE_POINT` is what stops Windows following the
    // link; `FILE_FLAG_BACKUP_SEMANTICS` is what lets a directory be opened
    // the same way, because nothing promises an alias is a file.
    // SAFETY: `wide` is a valid null-terminated wide string that outlives
    // the call; a null security-attributes pointer is the documented way to
    // ask for the default, and `OPEN_EXISTING` requires the template handle
    // to be null, which it is.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    // `owned_handle` rather than a bare check against
    // `INVALID_HANDLE_VALUE`, because null is the other value an
    // `OwnedHandle` may not hold. It closes the handle exactly once on
    // every path out of this function, including the failure below.
    let handle = owned_handle(handle)?;

    let mut buffer = vec![0u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize];
    let mut written: u32 = 0;
    // SAFETY: `handle` is open for the whole call; `FSCTL_GET_REPARSE_POINT`
    // takes no input, which is what the null pointer and the zero length
    // say; `buffer` is a writable allocation of exactly the length passed
    // and is borrowed for no longer than the call; `written` is a writable
    // out-parameter; and a null overlapped pointer asks for the blocking
    // form, which is the only form a handle opened without
    // `FILE_FLAG_OVERLAPPED` has.
    let read = unsafe {
        DeviceIoControl(
            handle.as_raw_handle() as HANDLE,
            FSCTL_GET_REPARSE_POINT,
            ptr::null(),
            0,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            &mut written,
            ptr::null_mut(),
        )
    };
    if read == 0 {
        return None;
    }
    buffer.truncate(written as usize);
    Some(buffer)
}

/// The third string of an `IO_REPARSE_TAG_APPEXECLINK` buffer, which is the
/// executable the alias names.
///
/// The layout is an eight-byte reparse header (`ULONG ReparseTag`,
/// `USHORT ReparseDataLength`, `USHORT Reserved`), then `ULONG Version`,
/// then consecutive null-terminated UTF-16 strings. Read back from
/// `%LOCALAPPDATA%\Microsoft\WindowsApps\winget.exe` on the development
/// machine: 406 bytes returned, tag `0x8000001B`, data length 398,
/// version 3, and four strings, being the package family name, the
/// application user model id, the executable under
/// `C:\Program Files\WindowsApps` and a flag of `0`.
///
/// Nothing here trusts the buffer. `data` is only as long as Windows said
/// it wrote, every read is taken from it with a checked range, and a
/// declared data length that runs past the end, an odd number of bytes of
/// strings, fewer than three strings, an empty third one and one that is
/// not UTF-16 all answer `None`.
#[cfg(windows)]
fn appexeclink_target(data: &[u8]) -> Option<String> {
    /// `ULONG ReparseTag`, `USHORT ReparseDataLength`, `USHORT Reserved`.
    /// `ReparseDataLength` counts the bytes that follow this header.
    const HEADER: usize = 8;
    /// The `ULONG Version` that comes first in the data.
    const VERSION_LENGTH: usize = 4;
    /// The one version this reads. See [`resolve_app_execution_alias`] for
    /// what happens to any other.
    const VERSION: u32 = 3;
    /// The package family name and the application user model id come
    /// first, so the executable is the third string.
    const TARGET: usize = 2;

    let tag = u32::from_le_bytes(data.get(..4)?.try_into().ok()?);
    if tag != IO_REPARSE_TAG_APPEXECLINK {
        return None;
    }
    let length = u16::from_le_bytes(data.get(4..6)?.try_into().ok()?) as usize;
    let version = u32::from_le_bytes(data.get(HEADER..HEADER + VERSION_LENGTH)?.try_into().ok()?);
    if version != VERSION {
        return None;
    }
    let strings = data.get(HEADER + VERSION_LENGTH..HEADER.checked_add(length)?)?;
    if !strings.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = strings
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    // Consecutive null-terminated strings, so splitting on the terminator
    // is the whole parse. A buffer holding fewer than three yields either
    // nothing or the empty tail after the last terminator, and both are
    // refused.
    let target = units.split(|unit| *unit == 0).nth(TARGET)?;
    if target.is_empty() {
        return None;
    }
    // Strict rather than lossy: a target that is not UTF-16 is not a path
    // to hand to anybody.
    String::from_utf16(target).ok()
}

/// No NVIDIA enquiry is made on Windows. `has_nvidia` exists only for the
/// WebKitGTK DMA-BUF workaround in `crates/brokey/src/lib.rs`, and WebKitGTK
/// never runs on Windows, so there is nothing here for it to detect.
pub fn has_nvidia() -> bool {
    false
}

/// Where Brokey keeps its own files, on Windows.
///
/// Unlike Linux there is nothing fixed in advance: Windows elevation (UAC)
/// runs as the same user with the same profile, so nothing scrubs the
/// environment the way `pkexec` does for the helper, and no closed allow
/// list needs a path it can name ahead of time. The `directories` crate's
/// ordinary answer is used as it comes: the cache under `%LOCALAPPDATA%`,
/// config and data under `%APPDATA%`, which is where the crate puts them.
pub struct Dirs {
    pub cache: PathBuf,
    pub config: PathBuf,
    pub data: PathBuf,
}

impl Dirs {
    pub fn new() -> Dirs {
        match directories::ProjectDirs::from("io.github", "spillebulle", "brokey") {
            Some(d) => Dirs {
                cache: d.cache_dir().to_path_buf(),
                config: d.config_dir().to_path_buf(),
                data: d.data_dir().to_path_buf(),
            },
            None => {
                // `directories` says this can happen when no profile can be
                // found at all. The temporary directory at least exists.
                let base = std::env::temp_dir().join("brokey");
                Dirs {
                    cache: base.join("cache"),
                    config: base.join("config"),
                    data: base.join("data"),
                }
            }
        }
    }
}

impl Default for Dirs {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(product: &str, build: &str) -> RegistryVersion {
        RegistryVersion {
            product_name: Some(product.to_string()),
            edition_id: Some("Professional".to_string()),
            display_version: Some("26H1".to_string()),
            current_build: Some(build.to_string()),
            ubr: Some(2738),
        }
    }

    /// The reference machine is Windows 11 and its ProductName says
    /// Windows 10 Pro. Microsoft never updated the value. Software that
    /// reads it reports the wrong operating system, so this one does not.
    #[test]
    fn the_version_comes_from_the_build_not_from_product_name() {
        let info = from_registry_version(&version("Windows 10 Pro", "28120"));
        assert_eq!(info.pretty_name, "Windows 11 Pro 26H1 (build 28120.2738)");
        assert_eq!(info.platform, crate::model::Platform::Windows);
        assert_eq!(info.distro_id, "windows");
        assert!(info.distro_like.is_empty());
    }

    #[test]
    fn a_build_below_22000_is_windows_10() {
        let info = from_registry_version(&version("Windows 10 Pro", "19045"));
        assert_eq!(info.pretty_name, "Windows 10 Pro 26H1 (build 19045.2738)");
    }

    /// Nothing in the key is guaranteed to be there. A machine that answers
    /// none of it still gets a name rather than an empty string.
    #[test]
    fn a_registry_that_says_nothing_still_names_the_machine() {
        let info = from_registry_version(&RegistryVersion {
            product_name: None,
            edition_id: None,
            display_version: None,
            current_build: None,
            ubr: None,
        });
        assert_eq!(info.pretty_name, "Windows");
    }

    /// UBR is the patch level and is missing on some installations.
    #[test]
    fn a_missing_ubr_leaves_the_build_bare() {
        let info = from_registry_version(&RegistryVersion {
            ubr: None,
            ..version("Windows 10 Pro", "28120")
        });
        assert_eq!(info.pretty_name, "Windows 11 Pro 26H1 (build 28120)");
    }

    /// EditionID is a bare word: Professional, Core, Enterprise. The name
    /// uses the word people know.
    #[test]
    fn edition_ids_become_the_words_people_use() {
        let core = RegistryVersion {
            edition_id: Some("Core".to_string()),
            ..version("Windows 10 Pro", "28120")
        };
        assert_eq!(
            from_registry_version(&core).pretty_name,
            "Windows 11 Home 26H1 (build 28120.2738)"
        );
    }

    /// The directories and the extensions come in as text so the search is
    /// a pure function. Nothing here touches the real PATH.
    #[test]
    fn a_bare_name_finds_the_executable_with_an_extension() {
        let dir = tempdir();
        std::fs::write(dir.join("choco.EXE"), b"").unwrap();
        let found = which_in("choco", dir.to_str().unwrap(), ".COM;.EXE;.BAT");
        assert!(same_file(
            &found.expect("it is found"),
            &dir.join("choco.EXE")
        ));
    }

    /// PATHEXT is tried in its own order, so a .com wins over a .exe when
    /// it comes first, which is what the shell does.
    #[test]
    fn pathext_is_tried_in_order() {
        let dir = tempdir();
        std::fs::write(dir.join("thing.EXE"), b"").unwrap();
        std::fs::write(dir.join("thing.COM"), b"").unwrap();
        let found = which_in("thing", dir.to_str().unwrap(), ".COM;.EXE");
        assert!(same_file(
            &found.expect("it is found"),
            &dir.join("thing.COM")
        ));
    }

    /// A name that already carries an extension is taken as it is.
    #[test]
    fn a_name_with_an_extension_is_not_extended_again() {
        let dir = tempdir();
        std::fs::write(dir.join("winget.exe"), b"").unwrap();
        let found = which_in("winget.exe", dir.to_str().unwrap(), ".EXE");
        assert_eq!(found, Some(dir.join("winget.exe")));
    }

    #[test]
    fn a_name_that_is_not_there_is_not_found() {
        let dir = tempdir();
        assert_eq!(which_in("absent", dir.to_str().unwrap(), ".EXE"), None);
    }

    /// Directories earlier in PATH win.
    #[test]
    fn the_first_directory_on_the_path_wins() {
        let first = tempdir();
        let second = tempdir();
        std::fs::write(first.join("dup.EXE"), b"").unwrap();
        std::fs::write(second.join("dup.EXE"), b"").unwrap();
        let path = format!("{};{}", first.display(), second.display());
        assert!(same_file(
            &which_in("dup", &path, ".EXE").expect("it is found"),
            &first.join("dup.EXE")
        ));
    }

    /// Says "the same file" rather than "the same string". The portable tests
    /// write their fixtures in the same case as the `PATHEXT` entry they pass,
    /// so for those two the spellings already agree. The `#[cfg(windows)]` test
    /// is the one that needs this: `PATHEXT` is conventionally upper case and
    /// the file on disk is usually lower case, and canonicalising is what lets
    /// the assertion see one file under two spellings. Returns `false` when
    /// either path cannot be canonicalised, for instance when it does not
    /// exist, which reads as a plain assertion failure and is acceptable in a
    /// test helper.
    fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    }

    /// A unique directory under the system temporary directory, removed by
    /// the operating system rather than by the test, so a failing test
    /// leaves its evidence behind.
    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "brokey-which-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Windows filesystems are case-insensitive. When `PATHEXT` is upper case
    /// (conventional) and the on-disk file is lower case (typical), `is_file()`
    /// still answers true and the function returns a match. This is a genuine
    /// property of the Windows implementation and the real-world scenario.
    #[test]
    #[cfg(windows)]
    fn windows_filesystem_is_case_insensitive_for_extension_matching() {
        let dir = tempdir();
        std::fs::write(dir.join("python.exe"), b"").unwrap();
        let found = which_in("python", dir.to_str().unwrap(), ".EXE");
        assert!(same_file(
            &found.expect("it is found despite case mismatch"),
            &dir.join("python.exe")
        ));
    }

    /// The regression this guards against: `Dirs` on Windows once answered
    /// `/tmp` turned into `C:\tmp` because it read the Unix-only `HOME`
    /// variable, which is unset here. The cache belongs under
    /// `%LOCALAPPDATA%`, never under the temporary directory, whenever that
    /// variable is set, which it always is in a normal session.
    #[test]
    #[cfg(windows)]
    fn the_cache_lands_under_local_app_data() {
        let dirs = Dirs::new();
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            assert!(
                dirs.cache.starts_with(&local_app_data),
                "expected {} to start under {local_app_data}",
                dirs.cache.display()
            );
        }
        assert_ne!(dirs.cache, dirs.config);
        assert_ne!(dirs.cache, dirs.data);
    }

    /// No sysfs, no PCI bus, nothing to read: Windows never has an opinion.
    #[test]
    fn there_is_no_nvidia_enquiry_on_windows() {
        assert!(!has_nvidia());
    }

    /// An ordinary file carries no reparse point, so there is nothing to
    /// resolve and the path comes back as it was given. This is the common
    /// case: `choco.exe` is a real file and every source but winget hands
    /// one of these in.
    #[cfg(windows)]
    #[test]
    fn a_file_that_is_not_a_reparse_point_comes_back_unchanged() {
        let dir = tempdir();
        let file = dir.join("plain.exe");
        std::fs::write(&file, b"not an alias").unwrap();
        assert_eq!(resolve_app_execution_alias(&file), file);
    }

    /// A name that is not on the disk comes back unchanged rather than
    /// panicking or inventing something. The caller hands it on to the
    /// closed list, which asks its own question about a path that is not
    /// there.
    #[cfg(windows)]
    #[test]
    fn a_path_that_is_not_there_comes_back_unchanged() {
        let missing = tempdir().join("nothing-is-here.exe");
        assert!(!missing.exists(), "the test invents a name nothing uses");
        assert_eq!(resolve_app_execution_alias(&missing), missing);
    }

    /// The real thing, on the machine running the test: the App Execution
    /// Alias winget is reached through. Skipped with a reason where it is
    /// absent rather than failed, so the suite stays green on a machine
    /// without winget and on Linux.
    ///
    /// What is asserted is what the resolution promises and no more: the
    /// answer is a different path from the alias, it is a file that is
    /// really there, and it is still a `winget.exe`. Nothing here asserts
    /// where it lives, because that is a package version in the name and it
    /// changes under every App Installer update.
    #[cfg(windows)]
    #[test]
    fn the_real_winget_alias_resolves_to_the_executable_that_runs() {
        let Ok(local) = std::env::var("LOCALAPPDATA") else {
            eprintln!("skipped: LOCALAPPDATA is not set, so the alias cannot be found");
            return;
        };
        let alias = Path::new(&local)
            .join("Microsoft")
            .join("WindowsApps")
            .join("winget.exe");
        if !alias.exists() {
            eprintln!("skipped: {} is not on this machine", alias.display());
            return;
        }

        let target = resolve_app_execution_alias(&alias);
        assert_ne!(target, alias, "the alias is not the program");
        assert!(
            target.is_file(),
            "{} was resolved, so it is on the disk",
            target.display()
        );
        assert_eq!(
            target
                .file_name()
                .map(|name| name.to_string_lossy().to_lowercase()),
            Some("winget.exe".to_string()),
            "{} is still winget",
            target.display()
        );
        eprintln!("{} resolved to {}", alias.display(), target.display());
    }
}
