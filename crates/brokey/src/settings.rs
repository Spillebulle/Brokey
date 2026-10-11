//! Preferences: a flat `key = value` file at `<config>/settings.conf`.
//!
//! Muster's `prefs.rs` shape, kept for the same reason: a settings file is a
//! compatibility promise, and `key = value` is the one that is easy to keep.
//! The rules that follow from that:
//!
//! - **An unknown key is kept, never dropped.** A file written by a newer
//!   Brokey survives a downgrade with everything the older build did not
//!   understand still in it. The lines ride along in [`Settings::unknown`] and
//!   are written back after the known ones.
//! - **A bad value falls back to its default.** One mistyped line in a
//!   hand-edited file must not cost every other setting in it. A figure out
//!   of range is clamped rather than refused.
//! - **The file is replaced, never rewritten in place.** The new text goes to
//!   a temporary file beside it and is renamed over the old one, so a crash
//!   mid-write leaves the previous settings rather than half a file.
//!
//! The struct is serde-derived with these exact field names because it
//! crosses to the page as JSON; the page's settings screen is the one place
//! that writes it, through `settings_set`.

use brokey_core::SourceKind;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The file's name inside the configuration directory.
pub const FILE_NAME: &str = "settings.conf";

/// The format's version. Written first and read but not branched on yet: it
/// is here so a later change has something to branch on.
const VERSION: u32 = 1;

/// How often the window may ask the sources for updates, in minutes. Below
/// five the pacman mirrors are being polled for nothing; above a week the
/// setting means "never", which is what `check_updates_on_start = false` is
/// for.
pub const UPDATE_CHECK_MIN: u32 = 5;
pub const UPDATE_CHECK_MAX: u32 = 7 * 24 * 60;

/// Which theme the interface wears. `System` follows the desktop and is the
/// default: an application that ignores the system preference is wrong half
/// of every day.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    Dark,
    Light,
    #[default]
    System,
}

impl Theme {
    pub const ALL: [Theme; 3] = [Theme::Dark, Theme::Light, Theme::System];

    pub const fn id(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
            Theme::System => "system",
        }
    }

    pub fn parse(id: &str) -> Option<Theme> {
        Theme::ALL.into_iter().find(|t| t.id() == id)
    }
}

/// Which program builds AUR packages. `Auto` takes paru, then yay, then the
/// built-in makepkg path, whichever is first present.
/// Whether Flatpak installs go to the system installation (through the
/// helper) or to the user's own (no password).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlatpakScope {
    #[default]
    System,
    User,
}

impl FlatpakScope {
    pub const ALL: [FlatpakScope; 2] = [FlatpakScope::System, FlatpakScope::User];

    pub const fn id(self) -> &'static str {
        match self {
            FlatpakScope::System => "system",
            FlatpakScope::User => "user",
        }
    }

    pub fn parse(id: &str) -> Option<FlatpakScope> {
        FlatpakScope::ALL.into_iter().find(|s| s.id() == id)
    }
}

impl Settings {
    /// The part of the settings the sources act on.
    pub fn preferences(&self) -> brokey_core::Preferences {
        brokey_core::Preferences {
            flatpak_user: self.flatpak_scope == FlatpakScope::User,
        }
    }
}

/// What is remembered between runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    /// Which sources a search asks when the page does not say. A source that
    /// is enabled but unavailable is still skipped by the store, with its
    /// reason; this list is the user's choice, not the machine's.
    pub enabled_sources: Vec<SourceKind>,
    /// Whether search shows plain packages beside applications. The page
    /// applies it; this only stores it.
    pub show_packages: bool,
    pub flatpak_scope: FlatpakScope,
    pub check_updates_on_start: bool,
    /// Whether Brokey may ask GitHub about its own new versions without
    /// being told to. Off means no request leaves the machine until the user
    /// presses the button.
    pub self_update_check: bool,
    pub update_check_minutes: u32,
    /// Groups the user has split, as `source:id` of the edition that was
    /// wrong. The grouper consults it so a bad name match stays undone.
    pub split: Vec<String>,
    /// Lines this build did not understand, kept so they are written back.
    /// Not sent to the page: it has no use for them and would send them back
    /// as nothing.
    #[serde(skip)]
    pub unknown: Vec<(String, String)>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            theme: Theme::System,
            enabled_sources: SourceKind::ALL.to_vec(),
            show_packages: false,
            flatpak_scope: FlatpakScope::System,
            check_updates_on_start: true,
            self_update_check: true,
            update_check_minutes: 60,
            split: Vec::new(),
            unknown: Vec::new(),
        }
    }
}

/// Where the file lives on this machine.
pub fn path() -> PathBuf {
    brokey_core::system::Dirs::new().config.join(FILE_NAME)
}

impl Settings {
    /// Read the settings from the usual place. A missing or unreadable file
    /// is a first run, and the answer to both is the defaults.
    pub fn load() -> Settings {
        Settings::load_from(&path())
    }

    pub fn load_from(path: &Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(text) => parse(&text),
            Err(_) => Settings::default(),
        }
    }

    /// Write the settings to the usual place.
    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&path())
    }

    /// Write the settings to `path` by way of a temporary file beside it, so
    /// the old file is intact until the new one is complete.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("conf.tmp");
        std::fs::write(&tmp, render(self))?;
        std::fs::rename(&tmp, path)
    }

    /// Whether this source is one the user wants searched.
    pub fn is_enabled(&self, kind: SourceKind) -> bool {
        self.enabled_sources.contains(&kind)
    }

    /// Record that a group was split at this edition. Idempotent.
    pub fn add_split(&mut self, source: SourceKind, id: &str) {
        let key = format!("{}:{id}", source.id());
        if !self.split.contains(&key) {
            self.split.push(key);
        }
    }
}

/// The parser, over text rather than over a file, so the tests exercise the
/// half that can be wrong.
pub fn parse(text: &str) -> Settings {
    let mut settings = Settings::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "version" => {}
            "theme" => {
                if let Some(t) = Theme::parse(&value.to_ascii_lowercase()) {
                    settings.theme = t;
                }
            }
            "enabled_sources" => {
                if let Some(list) = parse_sources(value) {
                    settings.enabled_sources = list;
                }
            }
            "show_packages" => settings.show_packages = flag(value, settings.show_packages),
            // Retired in 0.1.6: Brokey builds AUR packages itself, so a
            // choice of paru or yay no longer means anything. Read and
            // dropped, rather than kept as an unknown line for ever.
            "aur_helper" => {}
            "flatpak_scope" => {
                if let Some(s) = FlatpakScope::parse(&value.to_ascii_lowercase()) {
                    settings.flatpak_scope = s;
                }
            }
            "check_updates_on_start" => {
                settings.check_updates_on_start = flag(value, settings.check_updates_on_start)
            }
            "self_update_check" => {
                settings.self_update_check = flag(value, settings.self_update_check)
            }
            "update_check_minutes" => {
                if let Ok(m) = value.parse::<u32>() {
                    settings.update_check_minutes = m.clamp(UPDATE_CHECK_MIN, UPDATE_CHECK_MAX);
                }
            }
            "split" => settings.split = parse_split(value),
            // A line from a newer build, or a typo. Kept rather than dropped:
            // the alternative is a downgrade wiping settings it did not
            // understand, and a typo the user can still see and fix.
            _ => settings.unknown.push((key.to_string(), value.to_string())),
        }
    }
    settings
}

/// `true` or `false`, anything else leaving the current value.
fn flag(value: &str, current: bool) -> bool {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => true,
        "false" | "no" | "off" | "0" => false,
        _ => current,
    }
}

/// A comma list of source ids. An empty list is honoured, because a user
/// who turned every source off meant it; a list where nothing parses is a
/// damaged line and yields `None` so the default stands.
fn parse_sources(value: &str) -> Option<Vec<SourceKind>> {
    let words: Vec<&str> = value
        .split(',')
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return Some(Vec::new());
    }
    let mut kinds: Vec<SourceKind> = words
        .iter()
        .filter_map(|w| SourceKind::parse(&w.to_ascii_lowercase()))
        .collect();
    if kinds.is_empty() {
        return None;
    }
    // Interface order, once each, so the file reads the same however the
    // page sent it.
    kinds.sort();
    kinds.dedup();
    Some(kinds)
}

/// A comma list of `source:id`. Entries whose source is not one of ours are
/// dropped: they can never match a group and would only accumulate.
fn parse_split(value: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in value.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let Some((source, id)) = entry.split_once(':') else {
            continue;
        };
        if SourceKind::parse(source).is_none() || id.is_empty() {
            continue;
        }
        if !out.iter().any(|e| e == entry) {
            out.push(entry.to_string());
        }
    }
    out
}

/// The file's text, for writing or for showing.
pub fn render(settings: &Settings) -> String {
    let mut out = String::new();
    out.push_str(&format!("version = {VERSION}\n"));
    out.push_str(&format!("theme = {}\n", settings.theme.id()));
    let sources: Vec<&str> = settings.enabled_sources.iter().map(|k| k.id()).collect();
    out.push_str(&format!("enabled_sources = {}\n", sources.join(",")));
    out.push_str(&format!("show_packages = {}\n", settings.show_packages));
    out.push_str(&format!(
        "flatpak_scope = {}\n",
        settings.flatpak_scope.id()
    ));
    out.push_str(&format!(
        "check_updates_on_start = {}\n",
        settings.check_updates_on_start
    ));
    out.push_str(&format!(
        "self_update_check = {}\n",
        settings.self_update_check
    ));
    out.push_str(&format!(
        "update_check_minutes = {}\n",
        settings.update_check_minutes
    ));
    out.push_str(&format!("split = {}\n", settings.split.join(",")));
    for (key, value) in &settings.unknown {
        out.push_str(&format!("{key} = {value}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("brokey-test-{}-{nanos}-{name}", std::process::id()))
    }

    #[test]
    fn the_defaults_enable_every_source_and_follow_the_desktop() {
        let s = Settings::default();
        assert_eq!(s.theme, Theme::System);
        assert_eq!(s.enabled_sources, SourceKind::ALL.to_vec());
        assert!(!s.show_packages);
        assert_eq!(s.flatpak_scope, FlatpakScope::System);
        assert!(s.check_updates_on_start);
        assert!(s.self_update_check);
        assert_eq!(s.update_check_minutes, 60);
        assert!(s.split.is_empty());
    }

    #[test]
    fn everything_survives_a_round_trip() {
        let settings = Settings {
            theme: Theme::Light,
            enabled_sources: vec![SourceKind::Pacman, SourceKind::Aur, SourceKind::Github],
            show_packages: true,
            flatpak_scope: FlatpakScope::User,
            check_updates_on_start: false,
            self_update_check: false,
            update_check_minutes: 240,
            split: vec![
                "pacman:steam".into(),
                "flatpak:flathub/app/com.valvesoftware.Steam/x86_64/stable".into(),
            ],
            unknown: vec![("gizmo".into(), "12".into())],
        };
        assert_eq!(parse(&render(&settings)), settings);
    }

    #[test]
    fn an_unknown_key_is_kept_rather_than_refused() {
        // A file written by a later build. Losing the line on the next save
        // is the failure this guards.
        let s = parse("version = 9\ngizmo = 12\nshow_packages = true\n");
        assert!(s.show_packages);
        assert_eq!(s.unknown, vec![("gizmo".to_string(), "12".to_string())]);
        assert!(render(&s).contains("gizmo = 12\n"));
    }

    #[test]
    fn a_bad_value_leaves_the_default() {
        let s = parse("theme = purple\nflatpak_scope = everywhere\nshow_packages = maybe\n");
        assert_eq!(s.theme, Theme::System);
        assert_eq!(s.flatpak_scope, FlatpakScope::System);
        assert!(!s.show_packages);
        assert!(
            s.unknown.is_empty(),
            "a known key with a bad value is not an unknown key"
        );
    }

    #[test]
    fn a_figure_out_of_range_is_clamped_rather_than_dropped() {
        assert_eq!(
            parse("update_check_minutes = 1\n").update_check_minutes,
            UPDATE_CHECK_MIN
        );
        assert_eq!(
            parse("update_check_minutes = 999999\n").update_check_minutes,
            UPDATE_CHECK_MAX
        );
        assert_eq!(
            parse("update_check_minutes = soon\n").update_check_minutes,
            60
        );
    }

    #[test]
    fn the_source_list_is_tolerant() {
        assert_eq!(
            parse("enabled_sources = aur, pacman ,pacman,Flatpak\n").enabled_sources,
            vec![SourceKind::Pacman, SourceKind::Aur, SourceKind::Flatpak],
            "trimmed, deduplicated, interface order"
        );
        assert_eq!(
            parse("enabled_sources = \n").enabled_sources,
            Vec::<SourceKind>::new(),
            "empty is a choice"
        );
        assert_eq!(
            parse("enabled_sources = nix,portage\n").enabled_sources,
            SourceKind::ALL.to_vec(),
            "nothing recognisable is a damaged line"
        );
    }

    #[test]
    fn the_split_list_drops_what_cannot_match() {
        let s = parse("split = pacman:steam, nix:foo, aur:, pacman:steam, snap:spotify\n");
        assert_eq!(
            s.split,
            vec!["pacman:steam".to_string(), "snap:spotify".to_string()]
        );
    }

    #[test]
    fn adding_a_split_is_idempotent() {
        let mut s = Settings::default();
        s.add_split(SourceKind::Aur, "steam-git");
        s.add_split(SourceKind::Aur, "steam-git");
        assert_eq!(s.split, vec!["aur:steam-git".to_string()]);
    }

    #[test]
    fn a_damaged_file_is_the_defaults() {
        assert_eq!(
            parse("this is not a settings file\n\n#\n"),
            Settings::default()
        );
        assert_eq!(parse(""), Settings::default());
    }

    #[test]
    fn the_page_sees_the_field_names_as_written() {
        let json = serde_json::to_value(Settings::default()).unwrap();
        for key in [
            "theme",
            "enabled_sources",
            "show_packages",
            "flatpak_scope",
            "check_updates_on_start",
            "self_update_check",
            "update_check_minutes",
            "split",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
        assert_eq!(json["theme"], "system");
        assert_eq!(json["enabled_sources"][0], "pacman");
        assert!(
            json.get("unknown").is_none(),
            "the page has no use for unknown lines"
        );
        // A page sending only what it changed still parses.
        let partial: Settings = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(partial.theme, Theme::Dark);
        assert_eq!(partial.enabled_sources, SourceKind::ALL.to_vec());
    }

    #[test]
    fn saving_goes_through_a_temporary_file_and_reads_back() {
        let dir = scratch("save");
        let path = dir.join("nested").join(FILE_NAME);
        let mut s = Settings {
            theme: Theme::Dark,
            ..Settings::default()
        };
        s.add_split(SourceKind::Pacman, "steam");
        s.save_to(&path).expect("save");
        assert!(path.exists());
        assert!(
            !path.with_extension("conf.tmp").exists(),
            "the temporary file is renamed away"
        );
        assert_eq!(Settings::load_from(&path), s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_a_first_run() {
        assert_eq!(
            Settings::load_from(&scratch("missing")),
            Settings::default()
        );
    }

    #[test]
    fn preferences_carry_what_the_sources_act_on() {
        let mut s = Settings::default();
        assert_eq!(s.preferences(), brokey_core::Preferences::default());
        s.flatpak_scope = FlatpakScope::User;
        assert!(s.preferences().flatpak_user);
    }

    /// paru and yay are no longer used, so a file from 0.1.5 that chose one
    /// reads without complaint and the line is gone on the next save.
    #[test]
    fn the_retired_aur_helper_line_is_dropped() {
        let s = parse("aur_helper = paru\nshow_packages = true\n");
        assert!(s.show_packages);
        assert!(s.unknown.is_empty());
        assert!(!render(&s).contains("aur_helper"));
    }
}
