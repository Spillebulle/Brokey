//! Add/Remove Programs: the three uninstall keys, which between them are
//! the only record of the software a Windows machine has that no package
//! manager put there.
//!
//! This source answers `installed` and `plan(Remove)` and nothing else.
//! The registry knows what is on the machine and has no notion of a newer
//! version, so it never searches and never reports an update.
//!
//! Everything here except [`read`] is a pure function of [`RawEntry`], so
//! it is tested against a fixture rather than against whatever happens to
//! be installed on the machine running the tests.

use crate::model::{Command, Op, Package, PackageKind, Picture, SourceKind, Step};
use crate::{Error, Query, Result, Source, SourceStatus, Update};
use serde::Deserialize;
use std::path::PathBuf;

/// Which of the three uninstall keys an entry came from. It is part of the
/// package id, because the same key name can appear in more than one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Hive {
    /// `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall`
    Machine,
    /// The same under `WOW6432Node`: 32-bit software on a 64-bit machine.
    Machine32,
    /// `HKCU\...`: installed for this user, and removable without elevating.
    User,
}

impl Hive {
    pub fn id(self) -> &'static str {
        match self {
            Hive::Machine => "HKLM",
            Hive::Machine32 => "HKLM32",
            Hive::User => "HKCU",
        }
    }

    /// Removing something the whole machine has needs Administrator;
    /// removing something only this user has does not.
    pub fn needs_elevation(self) -> bool {
        !matches!(self, Hive::User)
    }
}

/// One uninstall key, as its values stand. Nothing is interpreted here.
#[derive(Clone, Debug, Deserialize)]
pub struct RawEntry {
    pub hive: Hive,
    /// The key's own name: an MSI ProductCode in braces, or a word the
    /// installer chose. Unique within its hive.
    pub key_name: String,
    pub display_name: Option<String>,
    pub display_version: Option<String>,
    pub publisher: Option<String>,
    /// Empty for 221 of the reference machine's 317 entries. Never trusted
    /// on its own; see `install_dir`.
    pub install_location: Option<String>,
    pub uninstall_string: Option<String>,
    pub quiet_uninstall_string: Option<String>,
    /// `1` when the Windows Installer owns this product. It is the flag
    /// Windows itself sets, and the only trustworthy way to tell a real MSI
    /// from an entry that merely happens to be keyed by a GUID, which the
    /// driver packages on the reference machine are.
    pub windows_installer: Option<u32>,
    /// A path to an `.exe` or `.ico`, optionally followed by `,` and a
    /// resource index.
    pub display_icon: Option<String>,
    pub system_component: Option<u32>,
    pub parent_key_name: Option<String>,
    /// "Security Update", "Update", "Hotfix" for the things Windows
    /// installed itself.
    pub release_type: Option<String>,
    /// Kilobytes, as the registry stores it.
    pub estimated_size: Option<u64>,
    pub url_info_about: Option<String>,
}

/// Whether this key is software a person would say they installed.
///
/// The reference machine has 343 keys and 317 with a name, of which 160 are
/// `SystemComponent`. What is left is roughly 157 applications. Each rule
/// here has a fixture entry where it fires and the fixture as a whole has a
/// test that the count does not drift.
pub fn is_application(e: &RawEntry) -> bool {
    if e.display_name.as_deref().unwrap_or("").trim().is_empty() {
        return false;
    }
    // Settings hides these and so does the store: runtimes, redistributables
    // and the plumbing of larger suites.
    if e.system_component.unwrap_or(0) != 0 {
        return false;
    }
    // A part of something else that has its own entry. Listing it would
    // show one application twice.
    if e.parent_key_name.is_some() {
        return false;
    }
    // Windows installed these itself and Brokey does not manage them. Only
    // the three the spec names: a fourth value needs a fixture entry where
    // it fires before it is added here.
    if matches!(
        e.release_type.as_deref(),
        Some("Security Update") | Some("Update") | Some("Hotfix")
    ) {
        return false;
    }
    true
}

/// Split a registry command line into a program and its arguments, the way
/// Windows does it: a leading quoted token may hold spaces, everything
/// after is split on whitespace. `None` when there is nothing to run.
///
/// This matters more than it looks. An uninstall string of
/// `"C:\Program Files\X\unins.exe" /S` split naively runs `C:\Program`.
pub fn split_command_line(s: &str) -> Option<(String, Vec<String>)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (program, rest) = if let Some(after) = s.strip_prefix('"') {
        let (program, rest) = after.split_once('"')?;
        (program.to_string(), rest)
    } else {
        match s.split_once(char::is_whitespace) {
            Some((program, rest)) => (program.to_string(), rest),
            None => (s.to_string(), ""),
        }
    };
    if program.is_empty() {
        return None;
    }
    let args = rest.split_whitespace().map(str::to_string).collect();
    Some((program, args))
}

/// The directory part of a Windows path, as text.
///
/// `std::path::Path` cannot do this. Off Windows a backslash is an ordinary
/// character, so `Path::new(r"C:\Program Files\X\unins.exe").parent()` answers
/// `Some("")` and every derivation below would be quietly wrong on the machine
/// most of this project's tests run on. These strings come out of a Windows
/// registry and describe a Windows machine whatever host is reading them, so
/// the splitting is spelt out and behaves the same everywhere.
///
/// Takes a path to a file. Given a path that already ends in a separator it
/// answers that same directory rather than its parent, which no caller here
/// wants and none asks for: the only input is an executable path out of
/// [`split_command_line`].
fn windows_parent(path: &str) -> Option<String> {
    let cut = path.rfind(['\\', '/'])?;
    Some(windows_dir(&path[..cut]))
}

/// A directory as text, with any trailing separator removed unless removing
/// it would change which directory is named.
///
/// `C:\` is a drive's root and `C:` is that drive's current directory, which
/// is somewhere else entirely and is never what a registry value meant. A
/// leftover scan reads [`install_dir`], so the difference between the two is
/// the difference between one folder and a whole drive.
fn windows_dir(text: &str) -> String {
    let trimmed = text.trim_end_matches(['\\', '/']);
    if trimmed.is_empty() || trimmed.ends_with(':') {
        format!("{trimmed}\\")
    } else {
        trimmed.to_string()
    }
}

/// The file name without its extension, as text, for the reason given on
/// [`windows_parent`].
fn windows_file_stem(path: &str) -> &str {
    let name = match path.rfind(['\\', '/']) {
        Some(cut) => &path[cut + 1..],
        None => path,
    };
    match name.rfind('.') {
        // A leading dot is the whole name, not an empty stem.
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

/// Whether a program is the Windows Installer rather than the
/// application's own uninstaller.
fn is_msiexec(program: &str) -> bool {
    windows_file_stem(program).eq_ignore_ascii_case("msiexec")
}

/// Where the application lives.
///
/// `InstallLocation` is empty for 221 of the reference machine's 317
/// entries, including every NSIS-built application on it, so the directory
/// of the uninstaller is the first answer and `InstallLocation` the
/// fallback. An `msiexec` uninstaller lives in the Windows directory and
/// says nothing about the application, so it never supplies one.
///
/// The `PathBuf` is a Windows path and is only a path on Windows.
/// Off it, take it apart with [`windows_parent`] rather than with `std::path`.
pub fn install_dir(e: &RawEntry) -> Option<PathBuf> {
    let from_uninstaller = e
        .uninstall_string
        .as_deref()
        .and_then(split_command_line)
        .filter(|(program, _)| !is_msiexec(program))
        .and_then(|(program, _)| windows_parent(&program))
        .map(PathBuf::from);
    from_uninstaller.or_else(|| {
        e.install_location
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(windows_dir(s)))
    })
}

/// The application's own icon. `DisplayIcon` is a path, optionally followed
/// by a comma and a resource index; the index is dropped because the page
/// asks the shell for the picture rather than for one numbered resource.
///
/// This value is never used as something to launch: it frequently points at
/// an uninstaller or at a file with no code in it at all.
pub fn icon(e: &RawEntry) -> Option<Picture> {
    let raw = e.display_icon.as_deref()?.trim();
    let raw = raw.strip_prefix('"').unwrap_or(raw);
    let path = match raw.rsplit_once(',') {
        // Only a trailing integer is an index. A bare comma in a path is not.
        Some((path, index)) if index.trim().parse::<i32>().is_ok() => path,
        _ => raw,
    };
    let path = path.trim().trim_end_matches('"');
    if path.is_empty() {
        return None;
    }
    Some(Picture::File(PathBuf::from(path)))
}

/// The id this source uses, naming the hive as well as the key, because the
/// same key name can appear in more than one of the three.
pub fn package_id(e: &RawEntry) -> String {
    format!("{}\\{}", e.hive.id(), e.key_name)
}

/// Whether the entry describes a driver rather than an application. Told by
/// the uninstaller being the Driver Install Frameworks tool, which is the
/// same on every machine, rather than by the display name, which is in the
/// language the machine was installed in.
fn is_driver(e: &RawEntry) -> bool {
    e.uninstall_string
        .as_deref()
        .and_then(split_command_line)
        .is_some_and(|(program, _)| {
            windows_file_stem(&program)
                .to_ascii_uppercase()
                .starts_with("DPINST")
        })
}

/// Everything the page draws for one entry. The registry records one version
/// and knows nothing about a newer one, so the installed version is also the
/// available version; saying anything else would draw an update that is not
/// there.
pub fn to_package(e: &RawEntry) -> Package {
    let name = e.display_name.clone().unwrap_or_default();
    let mut p = Package::new(SourceKind::Arp, package_id(e), name);
    p.kind = if is_driver(e) {
        PackageKind::Driver
    } else {
        PackageKind::App
    };
    p.installed = true;
    p.installed_version = e.display_version.clone();
    // The registry records one version and it is the installed one. Saying
    // it is also the available version keeps the Installed page from
    // drawing an update that does not exist.
    p.version = e.display_version.clone();
    p.developer = e.publisher.clone();
    p.homepage = e.url_info_about.clone();
    p.icon = icon(e);
    // EstimatedSize is kilobytes; Package counts bytes.
    p.installed_size = e.estimated_size.map(|kb| kb * 1024);
    if let Some(dir) = install_dir(e) {
        p.facts
            .push(("Installed to".to_string(), dir.display().to_string()));
    }
    p.facts.push(("Registry key".to_string(), package_id(e)));
    p
}

/// How an entry comes off the machine, best route first.
///
/// The two silent routes have to be counted together. On the reference
/// machine 64 of 317 entries carry a `QuietUninstallString` and 204 are
/// named by an MSI ProductCode, and 23 are both, so between them the two
/// cover 64 + 204 - 23 = 245 and leave 72 that need a window the user
/// clicks through. The 23 are the entries with a quiet string *and* a
/// ProductCode, not the 253 without a quiet string that have one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Removal {
    /// The publisher gave a silent switch. Nothing opens.
    Quiet(Command),
    /// The uninstall key carries `WindowsInstaller = 1` and is named by a
    /// well-formed ProductCode, so the Windows Installer removes it silently
    /// whatever the uninstall string says.
    Msi { product_code: String },
    /// The publisher's own uninstaller opens a window the user clicks
    /// through. The interface says so rather than drawing a progress rail.
    Interactive(Command),
}

/// Whether a key name is shaped like an MSI ProductCode: braces around a
/// GUID. The shape alone proves nothing, because plenty of things are keyed
/// by a GUID without being MSI products; `removal` asks for the
/// `WindowsInstaller` flag as well.
fn product_code(key_name: &str) -> Option<&str> {
    let inner = key_name.strip_prefix('{')?.strip_suffix('}')?;
    let groups: Vec<&str> = inner.split('-').collect();
    let expected = [8, 4, 4, 4, 12];
    if groups.len() != expected.len() {
        return None;
    }
    for (group, length) in groups.iter().zip(expected) {
        if group.len() != length || !group.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
    }
    Some(key_name)
}

fn command(line: &str) -> Option<Command> {
    let (program, args) = split_command_line(line)?;
    Some(Command {
        program,
        args,
        env: Vec::new(),
        cwd: None,
    })
}

pub fn removal(e: &RawEntry) -> Option<Removal> {
    if let Some(quiet) = e.quiet_uninstall_string.as_deref().and_then(command) {
        return Some(Removal::Quiet(quiet));
    }
    // The flag says the Windows Installer owns this product; the shape says
    // the key name is safe to hand it as an argument. A DIFX driver package
    // is keyed by a GUID and has the shape without the flag, and msiexec
    // would fail on it while Brokey reported a silent removal.
    if e.windows_installer == Some(1)
        && let Some(code) = product_code(&e.key_name)
    {
        return Some(Removal::Msi {
            product_code: code.to_string(),
        });
    }
    let interactive = e.uninstall_string.as_deref().and_then(command)?;
    Some(Removal::Interactive(interactive))
}

/// The step that removes the entry. `needs_root` means Administrator here:
/// software the whole machine has needs it, software only this user has
/// does not, which is what keeps the common case free of a prompt.
pub fn removal_step(e: &RawEntry) -> Option<Step> {
    let name = e.display_name.clone().unwrap_or_else(|| e.key_name.clone());
    let command = match removal(e)? {
        Removal::Quiet(c) | Removal::Interactive(c) => c,
        Removal::Msi { product_code } => Command {
            program: "msiexec.exe".to_string(),
            args: vec![
                "/x".to_string(),
                product_code,
                "/qn".to_string(),
                "/norestart".to_string(),
            ],
            env: Vec::new(),
            cwd: None,
        },
    };
    Some(Step {
        source: SourceKind::Arp,
        title: format!("Removing {name}"),
        command,
        needs_root: e.hive.needs_elevation(),
        weight: 10,
    })
}

/// "1 application" or "5 applications". The same shape as
/// `sources/linux/pacman.rs`'s `count`, kept local because that one is
/// private to its own module.
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// The Add/Remove Programs source. The entries are read once when the
/// source is built, as the pacman source reads its database once.
pub struct Arp {
    pub(crate) entries: Vec<RawEntry>,
}

impl Arp {
    /// Gated because it reads the registry. The `Source` implementation
    /// below is not: it answers from `entries`, so the fixture tests
    /// compile and run on Linux as well.
    #[cfg(windows)]
    pub fn new() -> Arp {
        Arp { entries: read() }
    }

    fn applications(&self) -> impl Iterator<Item = &RawEntry> {
        self.entries.iter().filter(|e| is_application(e))
    }

    fn find(&self, id: &str) -> Option<&RawEntry> {
        self.applications().find(|e| package_id(e) == id)
    }
}

#[cfg(windows)]
impl Default for Arp {
    fn default() -> Arp {
        Arp::new()
    }
}

impl Source for Arp {
    fn kind(&self) -> SourceKind {
        SourceKind::Arp
    }

    fn status(&self) -> SourceStatus {
        let found = self.applications().count();
        SourceStatus {
            kind: SourceKind::Arp,
            available: true,
            reason: None,
            detail: Some(count(found, "application")),
            searchable: false,
            setup: None,
        }
    }

    /// The registry is a record of what is here, not a catalogue of what
    /// could be. Searching it would return only what is already installed,
    /// which the Installed page already shows.
    fn search(&self, _query: &Query) -> Result<Vec<crate::Package>> {
        Ok(Vec::new())
    }

    fn installed(&self) -> Result<Vec<crate::Package>> {
        Ok(self.applications().map(to_package).collect())
    }

    /// An uninstall key records one version, the installed one, and knows
    /// nothing about a newer one.
    fn updates(&self) -> Result<Vec<Update>> {
        Ok(Vec::new())
    }

    fn details(&self, id: &str) -> Result<crate::Package> {
        self.find(id).map(to_package).ok_or_else(|| {
            Error::from_source(
                SourceKind::Arp,
                format!("{id} is not in the uninstall registry. It may have been removed already."),
            )
        })
    }

    /// Removal is the only operation this source has. Any other kind is
    /// nothing for it to do, which is an empty list. An id it does not hold
    /// is a different thing and is an error: an empty list would be carried
    /// to a successful outcome, and the user would be told a removal worked
    /// when nothing had happened.
    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        let Op::Remove { package } = op else {
            return Ok(Vec::new());
        };
        let entry = self.find(&package.id).ok_or_else(|| {
            Error::from_source(
                SourceKind::Arp,
                format!(
                    "{} is not in the uninstall registry, so there is nothing to remove.",
                    package.id
                ),
            )
        })?;
        Ok(removal_step(entry).into_iter().collect())
    }
}

/// Read the three uninstall keys. The only impure function in the module,
/// and the only one gated: `windows-registry` is a Windows-only dependency,
/// so everything else here stays compiled and tested on both platforms.
#[cfg(windows)]
pub fn read() -> Vec<RawEntry> {
    let mut entries = Vec::new();
    let roots: [(Hive, &windows_registry::Key, &str); 3] = [
        (
            Hive::Machine,
            windows_registry::LOCAL_MACHINE,
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        ),
        (
            Hive::Machine32,
            windows_registry::LOCAL_MACHINE,
            r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
        ),
        (
            Hive::User,
            windows_registry::CURRENT_USER,
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        ),
    ];
    for (hive, root, path) in roots {
        let Ok(key) = root.open(path) else { continue };
        let Ok(names) = key.keys() else { continue };
        for name in names {
            let Ok(sub) = key.open(&name) else { continue };
            entries.push(RawEntry {
                hive,
                key_name: name,
                display_name: sub.get_string("DisplayName").ok(),
                display_version: sub.get_string("DisplayVersion").ok(),
                publisher: sub.get_string("Publisher").ok(),
                install_location: sub.get_string("InstallLocation").ok(),
                uninstall_string: sub.get_string("UninstallString").ok(),
                quiet_uninstall_string: sub.get_string("QuietUninstallString").ok(),
                windows_installer: sub.get_u32("WindowsInstaller").ok(),
                display_icon: sub.get_string("DisplayIcon").ok(),
                system_component: sub.get_u32("SystemComponent").ok(),
                parent_key_name: sub.get_string("ParentKeyName").ok(),
                release_type: sub.get_string("ReleaseType").ok(),
                estimated_size: sub.get_u32("EstimatedSize").ok().map(u64::from),
                url_info_about: sub.get_string("URLInfoAbout").ok(),
            });
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Op, Query, Source};

    fn fixture() -> Vec<RawEntry> {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/windows/arp/entries.json"
        ))
        .expect("the fixture is checked in beside the source");
        serde_json::from_str(&text).expect("the fixture parses")
    }

    fn named<'a>(entries: &'a [RawEntry], name: &str) -> &'a RawEntry {
        entries
            .iter()
            .find(|e| e.display_name.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("{name} is not in the fixture"))
    }

    /// An ordinary application with a name and an uninstaller is kept.
    #[test]
    fn a_plain_application_is_kept() {
        let entries = fixture();
        assert!(is_application(named(&entries, "7-Zip 26.00 (x64)")));
        assert!(is_application(named(&entries, "Obsidian")));
        assert!(is_application(named(&entries, "A per-user application")));
    }

    /// 160 of the reference machine's 317 named entries are
    /// SystemComponent=1: runtimes and redistributables that Settings
    /// itself hides. A store that showed them would be listing plumbing.
    #[test]
    fn a_system_component_is_dropped() {
        let entries = fixture();
        assert!(!is_application(named(
            &entries,
            "Office 16 Click-to-Run Licensing Component"
        )));
    }

    /// Updates and hotfixes are not software a person installed.
    #[test]
    fn an_update_is_dropped() {
        let entries = fixture();
        assert!(!is_application(named(&entries, "Update for Windows")));
    }

    /// A ParentKeyName means the entry is a part of something else that
    /// has its own entry, so listing it would show one application twice.
    #[test]
    fn a_child_of_another_entry_is_dropped() {
        let entries = fixture();
        assert!(!is_application(named(&entries, "A component of a suite")));
    }

    /// Nothing without a name can be drawn.
    #[test]
    fn an_entry_with_no_display_name_is_dropped() {
        let entries = fixture();
        let nameless = entries
            .iter()
            .find(|e| e.key_name == "NoName")
            .expect("the fixture has one");
        assert!(!is_application(nameless));
    }

    /// Driver packages are kept rather than dropped. They are real things
    /// on the machine and the second spec will want them listed. There is no
    /// driver rule in the filter and there must not be one: a package like
    /// this is kept because it has a name, no SystemComponent, no
    /// ParentKeyName and no ReleaseType, exactly like any other entry. What
    /// this test guards is that nobody later reaches for a name prefix such
    /// as "Windows Driver Package", which would drop every driver on a
    /// machine running in another language.
    #[test]
    fn a_driver_package_is_kept() {
        let entries = fixture();
        let driver = entries
            .iter()
            .find(|e| {
                e.display_name
                    .as_deref()
                    .is_some_and(|n| n.contains("Arduino"))
            })
            .expect("the fixture has one");
        assert!(is_application(driver));
    }

    /// The whole fixture at once, so a heuristic added later cannot quietly
    /// change the answer for everything.
    #[test]
    fn the_fixture_yields_exactly_the_five_applications() {
        let entries = fixture();
        let kept: Vec<&str> = entries
            .iter()
            .filter(|e| is_application(e))
            .filter_map(|e| e.display_name.as_deref())
            .collect();
        assert_eq!(kept.len(), 5, "{kept:?}");
    }

    /// Windows quoting: a quoted first token can hold spaces, and what
    /// follows is split on whitespace. Getting this wrong means running
    /// "C:\Program" with an argument of "Files\...".
    #[test]
    fn a_quoted_program_with_spaces_splits_correctly() {
        let (program, args) = split_command_line(
            "\"C:\\Program Files\\Obsidian\\Uninstall Obsidian.exe\" /allusers /S",
        )
        .expect("it splits");
        assert_eq!(
            program,
            "C:\\Program Files\\Obsidian\\Uninstall Obsidian.exe"
        );
        assert_eq!(args, ["/allusers", "/S"]);
    }

    /// The DIFX driver uninstallers are unquoted, in 8.3 short form, and
    /// take a path as an argument.
    #[test]
    fn an_unquoted_program_splits_on_whitespace() {
        let (program, args) = split_command_line(
            "C:\\PROGRA~1\\DIFX\\873032~1\\DPINST~1.EXE /u C:\\WINDOWS\\System32\\a.inf",
        )
        .expect("it splits");
        assert_eq!(program, "C:\\PROGRA~1\\DIFX\\873032~1\\DPINST~1.EXE");
        assert_eq!(args, ["/u", "C:\\WINDOWS\\System32\\a.inf"]);
    }

    #[test]
    fn an_empty_command_line_is_not_a_command() {
        assert_eq!(split_command_line("   "), None);
    }

    /// The registry's paths are Windows paths whatever host reads them, so the
    /// splitting is done on the text. `std::path::Path` would answer `Some("")`
    /// for the first of these off Windows, and the install directory would be
    /// silently wrong on the machine most of these tests run on.
    #[test]
    fn a_windows_parent_is_the_same_on_every_host() {
        assert_eq!(
            windows_parent("C:\\Program Files\\7-Zip\\Uninstall.exe"),
            Some("C:\\Program Files\\7-Zip".to_string())
        );
        // Installers write forward slashes too, and Windows accepts them.
        assert_eq!(
            windows_parent("C:/Program Files/7-Zip/Uninstall.exe"),
            Some("C:/Program Files/7-Zip".to_string())
        );
        // The drive's root. "C:" alone would name the drive's current
        // directory, which is somewhere else.
        assert_eq!(windows_parent("C:\\setup.exe"), Some("C:\\".to_string()));
        // The root of the current drive, which a registry value can name.
        assert_eq!(windows_parent("\\setup.exe"), Some("\\".to_string()));
        assert_eq!(windows_parent("setup.exe"), None);
    }

    /// `C:\` and `C:` name different places: the second is the drive's
    /// current directory. A leftover scan reads this value, so trimming the
    /// separator away would point it at a whole drive instead of a folder.
    #[test]
    fn a_drive_root_install_location_keeps_its_separator() {
        let entries = fixture();
        let odd = RawEntry {
            install_location: Some("C:\\".to_string()),
            ..named(&entries, "A per-user application").clone()
        };
        assert_eq!(install_dir(&odd), Some(std::path::PathBuf::from("C:\\")));
    }

    #[test]
    fn a_windows_file_stem_is_the_same_on_every_host() {
        assert_eq!(
            windows_file_stem("C:\\PROGRA~1\\DIFX\\873032~1\\DPINST~1.EXE"),
            "DPINST~1"
        );
        assert_eq!(windows_file_stem("MsiExec.exe"), "MsiExec");
        assert_eq!(windows_file_stem("C:\\bin\\thing"), "thing");
        // A leading dot is the whole name, not an empty stem.
        assert_eq!(windows_file_stem(".gitignore"), ".gitignore");
    }

    /// The directory comes from the uninstaller, because InstallLocation is
    /// empty for 221 of the reference machine's 317 entries.
    #[test]
    fn the_install_directory_comes_from_the_uninstaller() {
        let entries = fixture();
        let obsidian = named(&entries, "Obsidian");
        assert_eq!(obsidian.install_location, None);
        assert_eq!(
            install_dir(obsidian),
            Some(std::path::PathBuf::from("C:\\Program Files\\Obsidian"))
        );
    }

    /// Where both agree, the answer is the same, which is what makes the
    /// derivation safe to rely on.
    #[test]
    fn install_location_and_the_uninstaller_agree_where_both_are_present() {
        let entries = fixture();
        assert_eq!(
            install_dir(named(&entries, "7-Zip 26.00 (x64)")),
            Some(std::path::PathBuf::from("C:\\Program Files\\7-Zip"))
        );
    }

    /// An MSI uninstaller lives in the Windows directory and says nothing
    /// about where the application is, so it is never used as the source of
    /// a directory. InstallLocation is the only answer here.
    #[test]
    fn an_msiexec_uninstaller_never_supplies_a_directory() {
        let entries = fixture();
        let per_user = named(&entries, "A per-user application");
        assert_eq!(
            install_dir(per_user),
            Some(std::path::PathBuf::from(
                "C:\\Users\\test\\AppData\\Local\\Example"
            ))
        );
    }

    /// DisplayIcon carries a resource index after a comma. The file is what
    /// matters; the index is dropped.
    #[test]
    fn an_icon_index_is_stripped() {
        let entries = fixture();
        let icon = icon(named(&entries, "Obsidian")).expect("there is an icon");
        assert_eq!(
            icon,
            crate::model::Picture::File(std::path::PathBuf::from(
                "C:\\Program Files\\Obsidian\\Obsidian.exe"
            ))
        );
    }

    #[test]
    fn an_entry_with_no_icon_has_none() {
        let entries = fixture();
        assert_eq!(icon(named(&entries, "A per-user application")), None);
    }

    /// The id names the hive as well as the key, because the same key name
    /// can appear in more than one of the three. The second spec addresses
    /// entries by this id.
    #[test]
    fn the_id_names_the_hive_and_the_key() {
        let entries = fixture();
        assert_eq!(
            package_id(named(&entries, "7-Zip 26.00 (x64)")),
            "HKLM\\7-Zip"
        );
        assert_eq!(
            package_id(named(&entries, "A per-user application")),
            "HKCU\\{6f320b93-ee3c-4826-85e0-000000000002}"
        );
    }

    #[test]
    fn a_package_carries_what_the_page_draws() {
        let entries = fixture();
        let p = to_package(named(&entries, "7-Zip 26.00 (x64)"));
        assert_eq!(p.source, crate::model::SourceKind::Arp);
        assert_eq!(p.name, "7-Zip 26.00 (x64)");
        assert_eq!(p.installed_version.as_deref(), Some("26.00"));
        assert_eq!(p.version.as_deref(), Some("26.00"));
        assert!(p.installed);
        assert_eq!(p.developer.as_deref(), Some("Igor Pavlov"));
        assert_eq!(p.homepage.as_deref(), Some("https://www.7-zip.org/"));
        assert_eq!(p.kind, crate::model::PackageKind::App);
        // EstimatedSize is kilobytes in the registry and bytes in Package.
        assert_eq!(p.installed_size, Some(5133 * 1024));
    }

    /// A driver package is an application by the filter and a driver by
    /// kind, so the page can tell them apart without reading the name.
    #[test]
    fn a_driver_package_is_marked_as_a_driver() {
        let entries = fixture();
        let driver = entries
            .iter()
            .find(|e| {
                e.display_name
                    .as_deref()
                    .is_some_and(|n| n.contains("Arduino"))
            })
            .expect("the fixture has one");
        assert_eq!(to_package(driver).kind, crate::model::PackageKind::Driver);
    }

    /// The quiet string is preferred wherever there is one: nothing opens
    /// and the plan can report honestly that it finished.
    #[test]
    fn a_quiet_uninstall_string_is_preferred() {
        let entries = fixture();
        let Some(Removal::Quiet(command)) = removal(named(&entries, "Obsidian")) else {
            panic!("Obsidian has a quiet uninstall string");
        };
        assert_eq!(
            command.program,
            "C:\\Program Files\\Obsidian\\Uninstall Obsidian.exe"
        );
        assert_eq!(command.args, ["/allusers", "/S"]);
    }

    /// 253 of the reference machine's 317 entries have no quiet string, but
    /// 204 are MSI ProductCodes and msiexec removes those silently. Only 72
    /// are left that genuinely cannot be, which is a quarter of the machine
    /// rather than most of it.
    #[test]
    fn an_msi_without_a_quiet_string_is_still_silent() {
        let entries = fixture();
        let per_user = named(&entries, "A per-user application");
        assert_eq!(per_user.quiet_uninstall_string, None);
        let Some(Removal::Msi { product_code }) = removal(per_user) else {
            panic!("an MSI ProductCode key is removable by msiexec");
        };
        assert_eq!(product_code, "{6f320b93-ee3c-4826-85e0-000000000002}");
    }

    /// The flag alone is not enough either. An entry can say the Windows
    /// Installer owns it and still carry a key name msiexec cannot be given,
    /// and then the only honest route left is the publisher's uninstaller.
    #[test]
    fn the_windows_installer_flag_alone_is_not_an_msi() {
        let entries = fixture();
        let odd = RawEntry {
            quiet_uninstall_string: None,
            windows_installer: Some(1),
            ..named(&entries, "Obsidian").clone()
        };
        let Some(Removal::Interactive(command)) = removal(&odd) else {
            panic!("a key name that is not a ProductCode cannot go to msiexec");
        };
        assert_eq!(
            command.program,
            "C:\\Program Files\\Obsidian\\Uninstall Obsidian.exe"
        );
    }

    /// What is left opens the publisher's own uninstaller, and the
    /// interface says so rather than drawing a rail that cannot move.
    ///
    /// The driver package is the entry that makes this rule load-bearing. Its
    /// key name is a well-formed GUID, so a filter that went on shape alone
    /// would call it an MSI and answer `msiexec /x` on something the Windows
    /// Installer has never heard of. It has no `WindowsInstaller` flag, which
    /// is what settles it.
    #[test]
    fn everything_else_opens_the_publishers_uninstaller() {
        let entries = fixture();
        let driver = entries
            .iter()
            .find(|e| {
                e.display_name
                    .as_deref()
                    .is_some_and(|n| n.contains("Arduino"))
            })
            .expect("the fixture has one");
        let Some(Removal::Interactive(command)) = removal(driver) else {
            panic!("a DIFX driver has no quiet string and is not a Windows Installer product");
        };
        assert_eq!(
            command.program,
            "C:\\PROGRA~1\\DIFX\\873032~1\\DPINST~1.EXE"
        );
    }

    #[test]
    fn an_entry_with_no_uninstaller_at_all_cannot_be_removed() {
        let entries = fixture();
        assert!(removal(named(&entries, "Update for Windows")).is_none());
    }

    /// A key name that merely looks a bit like a GUID is not one. Only the
    /// exact ProductCode shape is treated as an MSI.
    #[test]
    fn a_key_name_that_is_not_a_product_code_is_not_an_msi() {
        let entries = fixture();
        assert!(matches!(
            removal(named(&entries, "7-Zip 26.00 (x64)")),
            Some(Removal::Quiet(_))
        ));
    }

    /// Removing what the whole machine has needs Administrator; removing
    /// what only this user has does not. This is what keeps the common
    /// case free of a prompt.
    #[test]
    fn only_a_machine_wide_entry_needs_elevation() {
        let entries = fixture();
        let machine = removal_step(named(&entries, "Obsidian")).expect("a step");
        let user = removal_step(named(&entries, "A per-user application")).expect("a step");
        assert!(machine.needs_root);
        assert!(!user.needs_root);
        assert_eq!(machine.source, crate::model::SourceKind::Arp);
        assert_eq!(machine.title, "Removing Obsidian");
    }

    #[test]
    fn an_msi_step_runs_msiexec_quietly() {
        let entries = fixture();
        let step = removal_step(named(&entries, "A per-user application")).expect("a step");
        assert_eq!(step.command.program, "msiexec.exe");
        assert_eq!(
            step.command.args,
            [
                "/x",
                "{6f320b93-ee3c-4826-85e0-000000000002}",
                "/qn",
                "/norestart"
            ]
        );
    }

    fn source_from_fixture() -> Arp {
        Arp { entries: fixture() }
    }

    /// The registry has no notion of a newer version, so this source never
    /// searches and never reports an update. A source that answered either
    /// would be inventing something.
    #[test]
    fn it_neither_searches_nor_updates() {
        let arp = source_from_fixture();
        assert!(arp.search(&Query::new("obsidian")).unwrap().is_empty());
        assert!(arp.updates().unwrap().is_empty());
    }

    #[test]
    fn installed_is_the_filtered_entries_as_packages() {
        let arp = source_from_fixture();
        let installed = arp.installed().unwrap();
        assert_eq!(installed.len(), 5);
        assert!(installed.iter().all(|p| p.installed));
        assert!(
            installed
                .iter()
                .any(|p| p.name == "Obsidian" && p.id == "HKLM\\Obsidian")
        );
    }

    #[test]
    fn details_answers_for_an_id_it_has_and_says_so_for_one_it_does_not() {
        let arp = source_from_fixture();
        assert_eq!(arp.details("HKLM\\Obsidian").unwrap().name, "Obsidian");
        let e = arp.details("HKLM\\Nothing").unwrap_err();
        assert!(e.message.contains("HKLM\\Nothing"), "{}", e.message);
    }

    /// The only operation this source plans. Anything else is a source that
    /// has nothing to do, which is an empty list rather than an error.
    #[test]
    fn it_plans_a_removal_and_nothing_else() {
        let arp = source_from_fixture();
        let reference = crate::model::PackageRef {
            source: SourceKind::Arp,
            id: "HKLM\\Obsidian".to_string(),
        };
        let steps = arp
            .plan(&Op::Remove {
                package: reference.clone(),
            })
            .unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].title, "Removing Obsidian");

        let install = arp.plan(&Op::Install { package: reference }).unwrap();
        assert!(install.is_empty());
    }

    /// An id this source does not hold is an error rather than an empty
    /// list. Empty means the source has nothing to do, which the Runner
    /// carries to a successful outcome, so a removal that found nothing to
    /// remove would report that it worked.
    #[test]
    fn planning_a_removal_for_an_id_it_does_not_have_says_so() {
        let arp = source_from_fixture();
        let e = arp
            .plan(&Op::Remove {
                package: crate::model::PackageRef {
                    source: SourceKind::Arp,
                    id: "HKLM\\Nothing".to_string(),
                },
            })
            .unwrap_err();
        assert!(e.message.contains("HKLM\\Nothing"), "{}", e.message);
        assert!(e.message.contains("nothing to remove"), "{}", e.message);
    }

    /// The source is always there: the registry is part of Windows. It says
    /// how many applications it found, for the status bar.
    #[test]
    fn it_is_always_available_and_says_how_much_it_found() {
        let arp = source_from_fixture();
        let status = arp.status();
        assert!(status.available);
        assert_eq!(status.reason, None);
        assert_eq!(status.detail.as_deref(), Some("5 applications"));
    }

    /// A machine with exactly one is not told it has "1 applications".
    #[test]
    fn one_application_is_singular() {
        assert_eq!(count(1, "application"), "1 application");
        assert_eq!(count(5, "application"), "5 applications");
    }

    /// The real registry on the machine running the tests. Ignored by
    /// default because its answer depends on what is installed, and run
    /// with `cargo test -- --ignored live_arp` when the parser changes.
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn live_arp_reads_this_machine() {
        let arp = Arp::new();
        let installed = arp.installed().unwrap();
        assert!(
            installed.len() > 10,
            "a real Windows machine has more than ten applications, found {}",
            installed.len()
        );
        assert!(installed.iter().all(|p| !p.name.is_empty()));
        // Every one of them must be removable somehow, or the ladder has a
        // gap that the fixture did not show.
        let stuck: Vec<&str> = arp
            .applications()
            .filter(|e| removal(e).is_none())
            .filter_map(|e| e.display_name.as_deref())
            .collect();
        assert!(stuck.is_empty(), "no route off the machine for {stuck:?}");
    }
}
