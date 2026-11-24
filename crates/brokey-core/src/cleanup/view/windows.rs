//! The live machine, read and never written. Files through `std::fs` and one
//! `FindFirstFileW` for a reparse tag, the registry through
//! `windows-registry`; nothing here starts a program.

use super::*;
use crate::sources::windows::shortcut;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    FILE_ATTRIBUTE_REPARSE_POINT, FindClose, FindFirstFileW, WIN32_FIND_DATAW,
};
use windows_sys::Win32::System::SystemServices::{
    IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};

const TASKS_DIR: &str = r"C:\Windows\System32\Tasks";
const SERVICES: &str = r"SYSTEM\CurrentControlSet\Services";
const FIREWALL_RULES: &str =
    r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules";
/// `ERROR_FILE_NOT_FOUND` and `ERROR_PATH_NOT_FOUND` as HRESULTs.
const NOT_FOUND: [i32; 2] = [0x8007_0002_u32 as i32, 0x8007_0003_u32 as i32];

/// Cloud files' tags are `0x9000_001A` with a state in bits 12 to 15.
fn is_cloud_tag(tag: u32) -> bool {
    tag & 0xFFFF_0FFF == 0x9000_001A
}

/// The machine this process is running on.
pub struct LiveView {
    profile: Option<PathBuf>,
}

impl Default for LiveView {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveView {
    pub fn new() -> LiveView {
        LiveView {
            profile: std::env::var_os("USERPROFILE").map(PathBuf::from),
        }
    }

    fn env(name: &str) -> Option<PathBuf> {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }
}

fn io_error(path: &Path, e: &std::io::Error) -> ViewError {
    let shown = path.display().to_string();
    if e.kind() == std::io::ErrorKind::NotFound {
        ViewError::Missing(shown)
    } else {
        ViewError::Unreadable(shown)
    }
}

/// The reparse tag of a path that is one, read from the directory entry
/// (`dwReserved0` holds the tag there) without opening what it leads to.
fn reparse_tag(path: &Path) -> Option<u32> {
    let wide: Vec<u16> = OsStr::new(path).encode_wide().chain([0]).collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call, `data` is a
    // plain-data out parameter, and the handle is closed on the one path
    // that opens it.
    unsafe {
        let mut data: WIN32_FIND_DATAW = std::mem::zeroed();
        let handle = FindFirstFileW(wide.as_ptr(), &mut data);
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        FindClose(handle);
        Some(data.dwReserved0)
    }
}

fn reparse_of_attributes(path: &Path, attributes: u32) -> Reparse {
    if attributes & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_RECALL_ON_OPEN) != 0 {
        return Reparse::Cloud;
    }
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return Reparse::None;
    }
    match reparse_tag(path) {
        Some(IO_REPARSE_TAG_MOUNT_POINT | IO_REPARSE_TAG_SYMLINK) => Reparse::NameSurrogate,
        Some(tag) if is_cloud_tag(tag) => Reparse::Cloud,
        _ => Reparse::None,
    }
}

/// The data of a registry value as text. Numbers are written in decimal and
/// a list of strings is joined with semicolons; binary data is empty, since
/// nothing here matches on it.
fn value_text(value: windows_registry::Value) -> String {
    use windows_registry::Type;
    match value.ty() {
        Type::U32 => u32::try_from(value)
            .map(|n| n.to_string())
            .unwrap_or_default(),
        Type::U64 => u64::try_from(value)
            .map(|n| n.to_string())
            .unwrap_or_default(),
        Type::MultiString => Vec::<String>::try_from(value)
            .map(|v| v.join(";"))
            .unwrap_or_default(),
        Type::String | Type::ExpandString => String::try_from(value).unwrap_or_default(),
        _ => String::new(),
    }
}

fn open(hive: Hive, path: &str) -> Result<windows_registry::Key, ViewError> {
    let (root, path) = match hive {
        Hive::Hklm => (windows_registry::LOCAL_MACHINE, path.to_string()),
        Hive::Hkcu => (windows_registry::CURRENT_USER, path.to_string()),
        Hive::Hklm32 => (windows_registry::LOCAL_MACHINE, wow6432(path)),
    };
    root.open(&path).map_err(|e| {
        if NOT_FOUND.contains(&e.code().0) {
            ViewError::Missing(path)
        } else {
            ViewError::Unreadable(path)
        }
    })
}

/// `Software\X` as 32-bit software sees it on a 64-bit machine.
fn wow6432(path: &str) -> String {
    match path.split_once('\\') {
        Some((first, rest)) if first.eq_ignore_ascii_case("software") => {
            format!(r"{first}\WOW6432Node\{rest}")
        }
        None if path.eq_ignore_ascii_case("software") => r"Software\WOW6432Node".to_string(),
        _ => path.to_string(),
    }
}

/// Every `<Command>` inside an `<Exec>` of a task definition.
pub(crate) fn task_commands(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Exec>") {
        let after = &rest[start + "<Exec>".len()..];
        let end = after.find("</Exec>").unwrap_or(after.len());
        let exec = &after[..end];
        if let Some(c) = exec.find("<Command>") {
            let body = &exec[c + "<Command>".len()..];
            if let Some(close) = body.find("</Command>") {
                out.push(unescape(body[..close].trim()));
            }
        }
        rest = &after[end..];
    }
    out
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Task files are UTF-16 with a byte order mark as the scheduler writes them,
/// and UTF-8 as other tools do.
fn decode(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes))
            .into_owned()
    }
}

/// The `App=` and `Name=` of a firewall rule's `v2.31|Action=Allow|...|` text.
pub(crate) fn firewall_fields(rule: &str) -> (Option<&str>, Option<&str>) {
    let mut app = None;
    let mut name = None;
    for field in rule.split('|') {
        if let Some(v) = field.strip_prefix("App=") {
            app = Some(v);
        } else if let Some(v) = field.strip_prefix("Name=") {
            name = Some(v);
        }
    }
    (app, name)
}

impl SystemView for LiveView {
    fn known_folder(&self, which: Known) -> Option<PathBuf> {
        let profile = self.profile.clone();
        let in_profile = |tail: &str| profile.as_ref().map(|p| p.join(tail));
        match which {
            Known::ProgramFiles => Self::env("ProgramFiles"),
            Known::ProgramFilesX86 => Self::env("ProgramFiles(x86)"),
            Known::ProgramData => Self::env("ProgramData"),
            Known::AppData => Self::env("APPDATA"),
            Known::LocalAppData => Self::env("LOCALAPPDATA"),
            Known::LocalLow => in_profile(r"AppData\LocalLow"),
            Known::Documents => directories::UserDirs::new()
                .and_then(|d| d.document_dir().map(Path::to_path_buf))
                .or_else(|| in_profile("Documents")),
            Known::SavedGames => in_profile("Saved Games"),
            Known::StartMenuMachine => {
                Self::env("ProgramData").map(|p| p.join(r"Microsoft\Windows\Start Menu\Programs"))
            }
            Known::StartMenuUser => {
                Self::env("APPDATA").map(|p| p.join(r"Microsoft\Windows\Start Menu\Programs"))
            }
            Known::Desktop => directories::UserDirs::new()
                .and_then(|d| d.desktop_dir().map(Path::to_path_buf))
                .or_else(|| in_profile("Desktop")),
            Known::Windows => Self::env("SystemRoot"),
            Known::Profile => profile,
        }
    }

    fn list(&self, dir: &Path) -> Result<Vec<Entry>, ViewError> {
        let read = std::fs::read_dir(dir).map_err(|e| io_error(dir, &e))?;
        let mut out = Vec::new();
        for item in read {
            let Ok(item) = item else { continue };
            let path = item.path();
            // `symlink_metadata` describes the name and not what it leads to.
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let attributes = meta.file_attributes();
            out.push(Entry {
                name: item.file_name().to_string_lossy().into_owned(),
                is_dir: attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                size: meta.len(),
                reparse: reparse_of_attributes(&path, attributes),
            });
        }
        Ok(out)
    }

    fn exists(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok()
    }

    fn reparse(&self, path: &Path) -> Reparse {
        match std::fs::symlink_metadata(path) {
            Ok(meta) => reparse_of_attributes(path, meta.file_attributes()),
            Err(_) => Reparse::None,
        }
    }

    /// The size of what is in `dir`, without following any reparse point.
    /// What cannot be read counts as nothing.
    fn dir_size(&self, dir: &Path) -> u64 {
        let mut total = 0;
        let mut pending = vec![dir.to_path_buf()];
        while let Some(next) = pending.pop() {
            let Ok(read) = std::fs::read_dir(&next) else {
                continue;
            };
            for item in read.flatten() {
                let path = item.path();
                let Ok(meta) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                let attributes = meta.file_attributes();
                if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    continue;
                }
                if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    pending.push(path);
                } else {
                    total += meta.len();
                }
            }
        }
        total
    }

    fn subkeys(&self, hive: Hive, path: &str) -> Result<Vec<String>, ViewError> {
        let key = open(hive, path)?;
        let names = key
            .keys()
            .map_err(|_| ViewError::Unreadable(path.to_string()))?;
        Ok(names.collect())
    }

    fn values(&self, hive: Hive, path: &str) -> Result<Vec<(String, String)>, ViewError> {
        let key = open(hive, path)?;
        let values = key
            .values()
            .map_err(|_| ViewError::Unreadable(path.to_string()))?;
        Ok(values.map(|(name, v)| (name, value_text(v))).collect())
    }

    fn shortcut_target(&self, lnk: &Path) -> Option<PathBuf> {
        let bytes = std::fs::read(lnk).ok()?;
        let link = shortcut::parse(&bytes, &shortcut::expand_environment)?;
        link.target.map(PathBuf::from)
    }

    fn services(&self) -> Vec<Service> {
        let Ok(root) = windows_registry::LOCAL_MACHINE.open(SERVICES) else {
            return Vec::new();
        };
        let Ok(names) = root.keys() else {
            return Vec::new();
        };
        names
            .filter_map(|name| {
                let image_path = root.open(&name).ok()?.get_string("ImagePath").ok()?;
                Some(Service { name, image_path })
            })
            .collect()
    }

    /// Tasks whose definition cannot be read are left out: the scheduler
    /// keeps some of them readable by SYSTEM only.
    fn tasks(&self) -> Vec<ScheduledTask> {
        let root = Path::new(TASKS_DIR);
        let mut out = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            for item in read.flatten() {
                let path = item.path();
                let Ok(kind) = item.file_type() else { continue };
                if kind.is_dir() {
                    pending.push(path);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    let rel = path.strip_prefix(root).unwrap_or(&path);
                    out.push(ScheduledTask {
                        path: format!(r"\{}", rel.display()),
                        actions: task_commands(&decode(&bytes)),
                    });
                }
            }
        }
        out
    }

    fn firewall_rules(&self) -> Vec<FirewallRule> {
        let Ok(key) = windows_registry::LOCAL_MACHINE.open(FIREWALL_RULES) else {
            return Vec::new();
        };
        let Ok(values) = key.values() else {
            return Vec::new();
        };
        values
            .filter_map(|(id, value)| {
                let text = String::try_from(value).ok()?;
                let (app, name) = firewall_fields(&text);
                Some(FirewallRule {
                    name: name
                        .filter(|n| !n.starts_with('@'))
                        .unwrap_or(&id)
                        .to_string(),
                    program: app?.to_string(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_commands_reads_every_exec_and_only_those() {
        let xml = r#"<Task><Triggers><Command>not.exe</Command></Triggers><Actions>
            <Exec><Command>C:\A &amp; B\a.exe</Command><Arguments>-x</Arguments></Exec>
            <Exec><Command>"C:\b.exe"</Command></Exec>
            <ComHandler><ClassId>{x}</ClassId></ComHandler></Actions></Task>"#;
        assert_eq!(task_commands(xml), vec![r"C:\A & B\a.exe", r#""C:\b.exe""#]);
        assert!(task_commands("<Task></Task>").is_empty());
    }

    #[test]
    fn utf16_task_files_decode() {
        let mut bytes = vec![0xFF, 0xFE];
        for u in "<Exec><Command>x.exe</Command></Exec>".encode_utf16() {
            bytes.extend(u.to_le_bytes());
        }
        assert_eq!(task_commands(&decode(&bytes)), vec!["x.exe"]);
    }

    #[test]
    fn firewall_rule_text_gives_app_and_name() {
        let rule = r"v2.31|Action=Allow|Active=TRUE|Dir=In|App=C:\Acme\a.exe|Name=Acme|";
        assert_eq!(
            firewall_fields(rule),
            (Some(r"C:\Acme\a.exe"), Some("Acme"))
        );
        assert_eq!(firewall_fields("v2.31|Action=Block|"), (None, None));
    }

    #[test]
    fn the_32_bit_view_goes_under_wow6432node() {
        assert_eq!(wow6432(r"Software\Acme"), r"Software\WOW6432Node\Acme");
        assert_eq!(wow6432(r"SYSTEM\X"), r"SYSTEM\X");
    }

    #[test]
    fn cloud_tags_are_recognised_whatever_their_state_bits() {
        assert!(is_cloud_tag(0x9000_001A));
        assert!(is_cloud_tag(0x9000_901A));
        assert!(!is_cloud_tag(IO_REPARSE_TAG_SYMLINK));
    }

    #[test]
    #[ignore]
    fn live_view_reads_this_machine() {
        let v = LiveView::new();
        let pf = v.known_folder(Known::ProgramFiles).expect("Program Files");
        assert!(v.exists(&pf));
        assert!(!v.services().is_empty());
        if let Some(profile) = v.known_folder(Known::Profile) {
            let r = v.reparse(&profile.join("OneDrive"));
            assert!(matches!(r, Reparse::Cloud | Reparse::None), "{r:?}");
        }
        assert!(v.list(&pf).is_ok());
    }
}
