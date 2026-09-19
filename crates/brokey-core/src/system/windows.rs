//! What machine this is, on Windows.
//!
//! `ProductName` in `CurrentVersion` is not read. On the reference machine,
//! which is Windows 11, it says "Windows 10 Pro": Microsoft never updated
//! the value and a great deal of software reports the wrong operating
//! system because of it. `CurrentBuild` is the truth, and 22000 is where
//! Windows 11 begins.

use crate::model::{Platform, SystemInfo};
use std::path::{Path, PathBuf};

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
}
