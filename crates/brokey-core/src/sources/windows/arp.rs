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

use serde::Deserialize;

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
    // Windows installed these itself and Brokey does not manage them.
    if matches!(
        e.release_type.as_deref(),
        Some("Security Update") | Some("Update") | Some("Hotfix") | Some("ServicePack")
    ) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
