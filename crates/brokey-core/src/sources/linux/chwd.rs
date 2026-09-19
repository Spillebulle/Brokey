//! Drivers through chwd, CachyOS's hardware detection tool. Only CachyOS
//! ships it, so on every other distribution this source says so and the
//! Drivers page shows the firmware panel alone.
//!
//! chwd has no machine-readable output, so its box-drawn tables are parsed
//! here. Three listings are read: `chwd --list` (each device with the
//! profiles that fit it and their priority), `chwd --list-installed`, and
//! `chwd --list -d` (the same with the device model and each profile's
//! description). The profile database under `/var/lib/chwd` is read too,
//! for the packages a profile would install; the listings do not print
//! that. Reading the database alone was rejected because only chwd knows
//! which profiles fit which device: that match runs its device id patterns
//! and conditional scripts.
//!
//! One profile can fit two devices (`fallback` fits every GPU). As packages
//! a profile is listed once, naming every device it fits; as devices each
//! one carries its own copy.

use crate::model::*;
use crate::system;
use crate::{Error, Op, Query, Result, Source};
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

/// Where chwd keeps its profile database; `local` overrides `db`.
const DB_DIRS: [&str; 2] = ["/var/lib/chwd/db", "/var/lib/chwd/local"];

/// One device as `chwd --list` prints it, with the profiles that fit it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListedDevice {
    /// The PCI address, "0000:01:00.0", or a USB bus address.
    pub id: String,
    /// "0300:10de:1f10": class, vendor, device.
    pub ids: String,
    /// What chwd prints after the ids: "VGA compatible controller NVIDIA Corporation".
    pub text: String,
    /// Profile name and priority, in chwd's order.
    pub profiles: Vec<(String, u32)>,
}

/// One device as `chwd --list -d` prints it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceDetail {
    pub ids: String,
    /// "VGA compatible controller NVIDIA Corporation TU106M [GeForce RTX 2070 Mobile]".
    pub text: String,
    /// Profile name and its description.
    pub descriptions: Vec<(String, String)>,
}

/// A profile as the database defines it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileDef {
    pub name: String,
    pub desc: Option<String>,
    pub priority: Option<u32>,
    pub packages: Vec<String>,
}

/// A data row of a box-drawn table: the cells between `│` (or `|`), split
/// on `┆` (or `|`). Border lines (`╭ ╰ ├ ╞`) and prose are `None`.
fn table_row(line: &str) -> Option<Vec<String>> {
    let t = line.trim();
    if !(t.starts_with('│') || t.starts_with('|')) {
        return None;
    }
    let inner = t.trim_matches(['│', '|']);
    let cells: Vec<String> = inner
        .split(['┆', '|'])
        .map(|c| c.trim().to_string())
        .collect();
    (cells.len() >= 2).then_some(cells)
}

/// The header line of a device: `> 0000:01:00.0 (0300:10de:1f10) VGA compatible controller NVIDIA Corporation:`.
fn device_header(line: &str) -> Option<(String, String, String)> {
    let rest = line.trim().strip_prefix("> ")?;
    let open = rest.find('(')?;
    let close = rest[open..].find(')')? + open;
    let id = rest[..open].trim();
    let ids = rest[open + 1..close].trim();
    // The `-d` listing's "> PCI Device:  (…)" header has the same shape but
    // no address in front; an address never contains a space.
    if id.is_empty() || id.contains(char::is_whitespace) || ids.split(':').count() != 3 {
        return None;
    }
    let text = rest[close + 1..].trim().trim_end_matches(':').trim();
    Some((id.to_string(), ids.to_string(), text.to_string()))
}

/// A `Name ┆ Priority` row turned into a profile entry; the header row and
/// anything without a number are skipped.
fn profile_row(cells: &[String]) -> Option<(String, u32)> {
    let priority = cells.get(1)?.parse().ok()?;
    let name = cells.first()?;
    (!name.is_empty()).then(|| (name.clone(), priority))
}

pub fn parse_list(text: &str) -> Vec<ListedDevice> {
    let mut devices: Vec<ListedDevice> = Vec::new();
    for line in text.lines() {
        if let Some((id, ids, text)) = device_header(line) {
            devices.push(ListedDevice {
                id,
                ids,
                text,
                profiles: Vec::new(),
            });
        } else if let Some(cells) = table_row(line)
            && let Some(entry) = profile_row(&cells)
            && let Some(device) = devices.last_mut()
        {
            device.profiles.push(entry);
        }
    }
    devices
}

pub fn parse_installed(text: &str) -> Vec<(String, u32)> {
    text.lines()
        .filter_map(table_row)
        .filter_map(|cells| profile_row(&cells))
        .collect()
}

pub fn parse_detail(text: &str) -> Vec<DeviceDetail> {
    let mut devices: Vec<DeviceDetail> = Vec::new();
    let mut current_profile: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("> ")
            && rest.contains("Device:")
            && let Some(open) = rest.find('(')
            && let Some(close) = rest[open..].find(')')
        {
            devices.push(DeviceDetail {
                ids: rest[open + 1..open + close].to_string(),
                ..DeviceDetail::default()
            });
            current_profile = None;
            continue;
        }
        let Some(device) = devices.last_mut() else {
            continue;
        };
        if let Some(cells) = table_row(line) {
            match cells[0].as_str() {
                "Name" => current_profile = Some(cells[1].clone()),
                "Desc" => {
                    if let Some(name) = &current_profile
                        && !device.descriptions.iter().any(|(n, _)| n == name)
                    {
                        device.descriptions.push((name.clone(), cells[1].clone()));
                    }
                }
                _ => {}
            }
        } else if device.text.is_empty() && !t.is_empty() && !t.starts_with('>') && !is_border(t) {
            device.text = t.to_string();
        }
    }
    devices
}

fn is_border(t: &str) -> bool {
    t.starts_with(['╭', '╰', '├', '╞', '+', '-'])
}

/// The profile database's `profiles.toml`: `[name]` sections with `desc`,
/// `priority` and `packages` lines. A real TOML parser is not in the
/// workspace and the format chwd writes is three keys and some scripts in
/// triple-quoted strings, which are skipped.
pub fn parse_profiles_toml(text: &str) -> Vec<ProfileDef> {
    let mut defs: Vec<ProfileDef> = Vec::new();
    let mut in_block: Option<&str> = None;
    for line in text.lines() {
        if let Some(delim) = in_block {
            if line.contains(delim) {
                in_block = None;
            }
            continue;
        }
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(name) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            defs.push(ProfileDef {
                name: name.trim().to_string(),
                ..ProfileDef::default()
            });
            continue;
        }
        let Some((key, value)) = t.split_once('=') else {
            continue;
        };
        let value = value.trim();
        for delim in ["\"\"\"", "'''"] {
            if let Some(rest) = value.strip_prefix(delim)
                && !rest.contains(delim)
            {
                in_block = Some(delim);
            }
        }
        if in_block.is_some() {
            continue;
        }
        let Some(def) = defs.last_mut() else { continue };
        let value = value.trim_matches(|c| c == '"' || c == '\'').trim();
        match key.trim() {
            "desc" => def.desc = Some(value.to_string()),
            "priority" => def.priority = value.parse().ok(),
            "packages" => def.packages = value.split_whitespace().map(str::to_string).collect(),
            _ => {}
        }
    }
    defs
}

/// The PCI class name for the class part of an id triple. The listing
/// prints the class name in front of the vendor with nothing between, and
/// this is how the two are told apart.
fn class_name(class_id: &str) -> Option<&'static str> {
    Some(match class_id {
        "0300" => "VGA compatible controller",
        "0302" => "3D controller",
        "0380" => "Display controller",
        "0200" => "Ethernet controller",
        "0280" => "Network controller",
        "0401" => "Multimedia audio controller",
        "0403" => "Audio device",
        "0c03" => "USB controller",
        "0d11" => "Bluetooth",
        _ => return None,
    })
}

/// Split "VGA compatible controller NVIDIA Corporation" into the class and
/// the vendor, by the class table first and the word "controller" second.
fn split_class(ids: &str, text: &str) -> (Option<String>, Option<String>) {
    let class_id = ids.split(':').next().unwrap_or("");
    let class = class_name(class_id)
        .filter(|c| text.starts_with(c))
        .map(str::to_string)
        .or_else(|| {
            text.find(" controller")
                .map(|i| text[..i + " controller".len()].to_string())
        });
    match class {
        Some(class) => {
            let vendor = text[class.len()..].trim();
            (
                Some(class),
                (!vendor.is_empty()).then(|| vendor.to_string()),
            )
        }
        None => (None, None),
    }
}

/// The three listings and the database, joined into what the page draws.
pub fn assemble(
    list: &[ListedDevice],
    installed: &[(String, u32)],
    detail: &[DeviceDetail],
    defs: &[ProfileDef],
) -> Vec<DriverDevice> {
    let by_name: HashMap<&str, &ProfileDef> = defs.iter().map(|d| (d.name.as_str(), d)).collect();
    list.iter()
        .map(|device| {
            let (class, vendor) = split_class(&device.ids, &device.text);
            let detail = detail.iter().find(|d| d.ids == device.ids);
            let model = detail.map(|d| {
                let mut rest = d.text.as_str();
                for prefix in [class.as_deref(), vendor.as_deref()].into_iter().flatten() {
                    rest = rest.strip_prefix(prefix).unwrap_or(rest).trim();
                }
                rest.to_string()
            });
            let name = match model {
                Some(m) if !m.is_empty() => m,
                _ => device.text.clone(),
            };
            let top = device.profiles.iter().map(|(_, p)| *p).max();
            let profiles = device
                .profiles
                .iter()
                .map(|(profile, priority)| {
                    let def = by_name.get(profile.as_str());
                    let description = detail
                        .and_then(|d| d.descriptions.iter().find(|(n, _)| n == profile))
                        .map(|(_, desc)| desc.clone())
                        .or_else(|| def.and_then(|d| d.desc.clone()));
                    DriverProfile {
                        id: profile.clone(),
                        name: profile.clone(),
                        description,
                        installed: installed.iter().any(|(n, _)| n == profile),
                        recommended: Some(*priority) == top,
                        packages: def.map(|d| d.packages.clone()).unwrap_or_default(),
                    }
                })
                .collect();
            DriverDevice {
                id: device.id.clone(),
                name,
                vendor,
                class,
                profiles,
            }
        })
        .collect()
}

/// Profiles as packages: one per profile name, naming every device it fits.
pub fn packages(devices: &[DriverDevice], list: &[ListedDevice]) -> Vec<Package> {
    let mut out: Vec<Package> = Vec::new();
    for (device, listed) in devices.iter().zip(list) {
        for (profile, (_, priority)) in device.profiles.iter().zip(&listed.profiles) {
            if let Some(existing) = out.iter_mut().find(|p| p.id == profile.id) {
                if let Some((_, devices)) = existing.facts.iter_mut().find(|(k, _)| k == "Device") {
                    devices.push_str(", ");
                    devices.push_str(&device.name);
                }
                continue;
            }
            let mut p = Package::new(SourceKind::Chwd, &profile.id, &profile.name);
            p.kind = PackageKind::Driver;
            p.summary = Some(format!("{} driver profile", device.name));
            p.description = profile.description.clone();
            p.installed = profile.installed;
            p.developer = Some("CachyOS".to_string());
            p.facts = vec![
                ("Device".to_string(), device.name.clone()),
                ("Priority".to_string(), priority.to_string()),
                (
                    "Recommended".to_string(),
                    if profile.recommended { "Yes" } else { "No" }.to_string(),
                ),
            ];
            if !profile.packages.is_empty() {
                p.facts
                    .push(("Packages".to_string(), profile.packages.join(", ")));
            }
            out.push(p);
        }
    }
    out
}

/// Every `profiles.toml` under the database directories, later directories
/// overriding earlier ones so a local edit wins over the shipped file.
pub fn load_profile_defs() -> Vec<ProfileDef> {
    let mut defs: Vec<ProfileDef> = Vec::new();
    for dir in DB_DIRS {
        for def in profile_defs_under(Path::new(dir)) {
            defs.retain(|d| d.name != def.name);
            defs.push(def);
        }
    }
    defs
}

fn profile_defs_under(dir: &Path) -> Vec<ProfileDef> {
    let mut defs = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return defs;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            defs.extend(profile_defs_under(&path));
        } else if path.file_name().is_some_and(|n| n == "profiles.toml")
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            defs.extend(parse_profiles_toml(&text));
        }
    }
    defs
}

/// The three listings, parsed.
struct Listings {
    list: Vec<ListedDevice>,
    installed: Vec<(String, u32)>,
    detail: Vec<DeviceDetail>,
}

fn listings() -> Result<Listings> {
    let list = parse_list(&run(&["--list"])?);
    let installed = parse_installed(&run(&["--list-installed"])?);
    // Descriptions are a nicety: a chwd without `-d` still lists profiles.
    let detail = run(&["--list", "-d"])
        .map(|t| parse_detail(&t))
        .unwrap_or_default();
    Ok(Listings {
        list,
        installed,
        detail,
    })
}

/// Every device chwd knows with its profiles, for the Drivers page.
pub fn devices() -> Result<Vec<DriverDevice>> {
    let l = listings()?;
    Ok(assemble(
        &l.list,
        &l.installed,
        &l.detail,
        &load_profile_defs(),
    ))
}

fn run(args: &[&str]) -> Result<String> {
    system::run("chwd", args).map_err(|e| err(e.message))
}

fn err(message: impl Into<String>) -> Error {
    Error::from_source(SourceKind::Chwd, message)
}

/// A profile name as chwd spells them, and nothing that `chwd -i` could
/// read as a flag or a path.
fn valid_profile(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

fn step(title: String, flag: &str, profile: &str) -> Result<Step> {
    if !valid_profile(profile) {
        return Err(err(format!("{profile} is not a chwd profile name.")));
    }
    Ok(Step {
        source: SourceKind::Chwd,
        title,
        command: Command {
            program: "chwd".to_string(),
            args: vec![flag.to_string(), profile.to_string()],
            env: Vec::new(),
            cwd: None,
        },
        needs_root: true,
        weight: 8,
    })
}

pub fn install_step(profile: &str) -> Result<Step> {
    step(
        format!("Installing driver profile {profile}"),
        "-i",
        profile,
    )
}

pub fn remove_step(profile: &str) -> Result<Step> {
    step(format!("Removing driver profile {profile}"), "-r", profile)
}

pub struct Chwd {
    version: OnceLock<Option<String>>,
}

impl Chwd {
    pub fn new(_system: &SystemInfo) -> Chwd {
        Chwd {
            version: OnceLock::new(),
        }
    }

    fn all_packages(&self) -> Result<Vec<Package>> {
        let l = listings()?;
        let devices = assemble(&l.list, &l.installed, &l.detail, &load_profile_defs());
        Ok(packages(&devices, &l.list))
    }
}

impl Source for Chwd {
    fn kind(&self) -> SourceKind {
        SourceKind::Chwd
    }

    fn status(&self) -> SourceStatus {
        if system::which("chwd").is_none() {
            return SourceStatus {
                kind: SourceKind::Chwd,
                available: false,
                reason: Some(
                    "Drivers are managed by chwd on CachyOS; this system does not have it."
                        .to_string(),
                ),
                detail: None,
                searchable: false,
                setup: None,
            };
        }
        let detail = self
            .version
            .get_or_init(|| run(&["--version"]).ok().map(|v| v.trim().to_string()))
            .clone();
        SourceStatus {
            kind: SourceKind::Chwd,
            available: true,
            reason: None,
            detail,
            searchable: false,
            setup: None,
        }
    }

    fn search(&self, query: &Query) -> Result<Vec<Package>> {
        let q = query.text.trim().to_lowercase();
        Ok(self
            .all_packages()?
            .into_iter()
            .filter(|p| {
                let haystack = format!(
                    "{} {} {} driver",
                    p.name,
                    p.summary.as_deref().unwrap_or(""),
                    p.description.as_deref().unwrap_or("")
                )
                .to_lowercase();
                q.is_empty() || haystack.contains(&q)
            })
            .take(query.limit.max(1))
            .collect())
    }

    fn installed(&self) -> Result<Vec<Package>> {
        Ok(self
            .all_packages()?
            .into_iter()
            .filter(|p| p.installed)
            .collect())
    }

    /// chwd profiles are updated through the packages they install, which
    /// pacman lists; nothing to add here.
    fn updates(&self) -> Result<Vec<Update>> {
        Ok(Vec::new())
    }

    fn details(&self, id: &str) -> Result<Package> {
        self.all_packages()?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| {
                err(format!(
                    "{id} is not a driver profile chwd offers for this machine."
                ))
            })
    }

    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        match op {
            Op::Install { package } => Ok(vec![install_step(&package.id)?]),
            Op::Remove { package } => Ok(vec![remove_step(&package.id)?]),
            Op::Update { .. } | Op::UpdateAll { .. } | Op::Refresh { .. } | Op::Setup { .. } => {
                Ok(Vec::new())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_survive_ascii_borders_too() {
        assert_eq!(
            table_row("| nvidia-open-dkms | 10 |"),
            Some(vec!["nvidia-open-dkms".to_string(), "10".to_string()])
        );
        assert_eq!(
            table_row("│ intel    ┆ 4        │"),
            Some(vec!["intel".to_string(), "4".to_string()])
        );
        assert_eq!(table_row("├╌╌╌╌┼╌╌╌╌┤"), None);
        assert_eq!(table_row("> Installed profiles:"), None);
    }

    #[test]
    fn the_header_row_is_not_a_profile() {
        assert_eq!(
            profile_row(&["Name".to_string(), "Priority".to_string()]),
            None
        );
        assert_eq!(
            profile_row(&["fallback".to_string(), "3".to_string()]),
            Some(("fallback".to_string(), 3))
        );
    }

    #[test]
    fn a_device_header_is_split_into_its_parts() {
        let (id, ids, text) = device_header(
            "> 0000:01:00.0 (0300:10de:1f10) VGA compatible controller NVIDIA Corporation:",
        )
        .unwrap();
        assert_eq!(id, "0000:01:00.0");
        assert_eq!(ids, "0300:10de:1f10");
        assert_eq!(text, "VGA compatible controller NVIDIA Corporation");
        assert_eq!(device_header("> Installed profiles:"), None);
        assert_eq!(device_header("> PCI Device:  (0300:8086:3e9b)"), None);
    }

    #[test]
    fn class_and_vendor_come_apart() {
        assert_eq!(
            split_class(
                "0300:10de:1f10",
                "VGA compatible controller NVIDIA Corporation"
            ),
            (
                Some("VGA compatible controller".to_string()),
                Some("NVIDIA Corporation".to_string())
            )
        );
        assert_eq!(
            split_class("0282:14e4:4331", "Wireless controller Broadcom Inc."),
            (
                Some("Wireless controller".to_string()),
                Some("Broadcom Inc.".to_string())
            )
        );
        assert_eq!(split_class("ffff:0000:0000", "Something odd"), (None, None));
    }

    #[test]
    fn toml_scripts_are_skipped_and_keys_read() {
        let defs = parse_profiles_toml(
            "# comment\n[a.prime]\ndesc = 'A prime'\npriority = 11\npackages = 'x y'\npre_install = \"\"\"\n[not-a-section]\ndesc = 'wrong'\n\"\"\"\n[b]\ndesc = \"B\"\npriority = 3\n",
        );
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "a.prime");
        assert_eq!(defs[0].desc.as_deref(), Some("A prime"));
        assert_eq!(defs[0].priority, Some(11));
        assert_eq!(defs[0].packages, vec!["x", "y"]);
        assert_eq!(defs[1].name, "b");
        assert_eq!(defs[1].desc.as_deref(), Some("B"));
        assert!(defs[1].packages.is_empty());
    }

    #[test]
    fn profile_names_that_look_like_flags_are_refused() {
        assert!(install_step("nvidia-open-dkms.prime").is_ok());
        assert!(install_step("--force").is_err());
        assert!(install_step("a b").is_err());
        assert!(remove_step("").is_err());
        let s = remove_step("intel").unwrap();
        assert_eq!(s.command.args, vec!["-r", "intel"]);
        assert!(s.needs_root);
        assert_eq!(s.weight, 8);
    }
}
