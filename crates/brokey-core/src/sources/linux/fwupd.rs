//! Firmware through fwupd. `fwupdmgr` is asked for JSON; nothing here talks
//! to the daemon over D-Bus. The bus would save a process spawn per question,
//! but it would add a bus library, and every distribution ships the command
//! beside the daemon anyway.
//!
//! Two shapes of question. As a [`Source`], every device fwupd knows with a
//! name and a version is an installed [`Package`] of kind
//! [`PackageKind::Firmware`], and every release it offers is an [`Update`].
//! For the Drivers page, [`devices`] gives every named device as a
//! [`FirmwareDevice`] with its pending update folded in.
//!
//! No step here asks for root. fwupd has its own polkit policy and prompts
//! for itself when a device needs it, so a `pkexec` in front of `fwupdmgr`
//! would be a second password for the same thing.

use crate::model::*;
use crate::system;
use crate::{Error, Op, Query, Result, Source};
use serde::Deserialize;
use std::sync::OnceLock;

/// One device as `fwupdmgr get-devices --json` and `get-updates --json`
/// print it. Only the fields the store draws; fwupd's JSON has many more.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Device {
    #[serde(rename = "DeviceId")]
    pub device_id: String,
    #[serde(rename = "Name")]
    pub name: Option<String>,
    #[serde(rename = "Vendor")]
    pub vendor: Option<String>,
    #[serde(rename = "Version")]
    pub version: Option<String>,
    #[serde(rename = "VersionFormat")]
    pub version_format: Option<String>,
    #[serde(rename = "Summary")]
    pub summary: Option<String>,
    #[serde(rename = "Plugin")]
    pub plugin: Option<String>,
    #[serde(rename = "Flags")]
    pub flags: Vec<String>,
    #[serde(rename = "Guid")]
    pub guids: Vec<String>,
    #[serde(rename = "UpdateError")]
    pub update_error: Option<String>,
    /// Newest first, as fwupd prints them.
    #[serde(rename = "Releases")]
    pub releases: Vec<Release>,
}

/// One release fwupd offers for a device.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Release {
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "Summary")]
    pub summary: Option<String>,
    /// AppStream description markup, passed to the page as it is.
    #[serde(rename = "Description")]
    pub description: Option<String>,
    #[serde(rename = "Size")]
    pub size: Option<u64>,
    /// Unix seconds.
    #[serde(rename = "Created")]
    pub created: Option<i64>,
    #[serde(rename = "Urgency")]
    pub urgency: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Listing {
    #[serde(rename = "Devices")]
    devices: Vec<Device>,
}

impl Device {
    pub fn needs_reboot(&self) -> bool {
        self.flags.iter().any(|f| f == "needs-reboot")
    }

    /// The release fwupd would install. fwupd lists releases newest first;
    /// comparing versions here was rejected because the order depends on
    /// the device's version format (hex, number, triplet, plain) and fwupd
    /// already applied it.
    pub fn newest_release(&self) -> Option<&Release> {
        self.releases.first()
    }
}

/// The devices in one `--json` listing.
pub fn parse_devices(json: &str) -> Result<Vec<Device>> {
    let listing: Listing = serde_json::from_str(json).map_err(|e| {
        err(format!(
            "fwupdmgr printed something that was not the expected JSON: {e}."
        ))
    })?;
    Ok(listing.devices)
}

/// The device as an installed package, or `None` when fwupd has no name or
/// no version for it: a device with neither is a bus entry, not firmware a
/// person can recognise in a list.
pub fn package(device: &Device) -> Option<Package> {
    let name = device.name.as_deref()?;
    let version = device.version.as_deref()?;
    let mut p = Package::new(SourceKind::Fwupd, &device.device_id, name);
    p.kind = PackageKind::Firmware;
    p.summary = Some(
        device
            .summary
            .clone()
            .unwrap_or_else(|| match &device.vendor {
                Some(vendor) => format!("{vendor} {name}"),
                None => name.to_string(),
            }),
    );
    p.developer = device.vendor.clone();
    p.installed = true;
    p.installed_version = Some(version.to_string());
    p.version = Some(
        device
            .newest_release()
            .map(|r| r.version.clone())
            .unwrap_or_else(|| version.to_string()),
    );
    p.updated = device.newest_release().and_then(|r| r.created);
    p.download_size = device.newest_release().and_then(|r| r.size);
    p.facts = facts(device);
    Some(p)
}

/// The detail page's key/value list. fwupd's plugin name is the one
/// internal that says something a user can place ("uefi_capsule" is the
/// firmware capsule path, "nvme" the drive), so it is kept under a plain
/// key; the flag list and the GUID count are jargon nothing on the page can
/// act on, and "Needs reboot" already says the one flag that matters.
fn facts(device: &Device) -> Vec<(String, String)> {
    let mut facts = Vec::new();
    if let Some(plugin) = &device.plugin {
        facts.push(("Updated through".to_string(), plugin.clone()));
    }
    if let Some(format) = &device.version_format {
        facts.push(("Version format".to_string(), format.clone()));
    }
    if let Some(error) = &device.update_error {
        facts.push(("Update error".to_string(), error.clone()));
    }
    facts.push((
        "Needs reboot".to_string(),
        if device.needs_reboot() { "Yes" } else { "No" }.to_string(),
    ));
    facts
}

/// The update fwupd offers for the device, if it lists a release.
pub fn update(device: &Device) -> Option<Update> {
    let release = device.newest_release()?;
    let name = device.name.clone()?;
    Some(Update {
        package: PackageRef {
            source: SourceKind::Fwupd,
            id: device.device_id.clone(),
        },
        name,
        kind: PackageKind::Firmware,
        summary: release.summary.clone(),
        icon: None,
        from: device.version.clone(),
        to: release.version.clone(),
        download_size: release.size,
        published: release.created,
        is_self: false,
    })
}

/// The Drivers page's view: every named device, with the update from the
/// `get-updates` listing folded in by device id.
pub fn fold(devices: &[Device], updates: &[Device]) -> Vec<FirmwareDevice> {
    devices
        .iter()
        .filter_map(|d| {
            let name = d.name.clone()?;
            let pending = updates
                .iter()
                .find(|u| u.device_id == d.device_id)
                .and_then(|u| u.newest_release());
            Some(FirmwareDevice {
                id: d.device_id.clone(),
                name,
                vendor: d.vendor.clone(),
                version: d.version.clone(),
                update_version: pending.map(|r| r.version.clone()),
                update_summary: pending.and_then(|r| r.summary.clone()),
                update_size: pending.and_then(|r| r.size),
                needs_reboot: d.needs_reboot(),
            })
        })
        .collect()
}

/// `fwupdmgr update -y --no-reboot-check <device>`. The `-y` answers
/// fwupd's own "proceed?" prompt; the reboot is left to the person, so the
/// store never restarts a machine.
pub fn update_step(device_id: &str, name: &str) -> Step {
    step(
        format!("Updating firmware for {name}"),
        vec!["update", "-y", "--no-reboot-check", device_id],
        6,
    )
}

pub fn update_all_step() -> Step {
    step(
        "Updating all firmware",
        vec!["update", "-y", "--no-reboot-check"],
        6,
    )
}

pub fn refresh_step() -> Step {
    step(
        "Refreshing firmware metadata",
        vec!["refresh", "--force"],
        2,
    )
}

fn step(title: impl Into<String>, args: Vec<&str>, weight: u32) -> Step {
    Step {
        source: SourceKind::Fwupd,
        title: title.into(),
        command: Command {
            program: "fwupdmgr".to_string(),
            args: args.into_iter().map(str::to_string).collect(),
            env: Vec::new(),
            cwd: None,
        },
        needs_root: false,
        weight,
    }
}

/// Every named device on this machine with its pending update, for the
/// Drivers page. Runs `fwupdmgr` twice.
pub fn devices() -> Result<Vec<FirmwareDevice>> {
    let devices = list_devices()?;
    let updates = list_updates(None)?;
    Ok(fold(&devices, &updates))
}

fn list_devices() -> Result<Vec<Device>> {
    let json = system::run("fwupdmgr", &["get-devices", "--json"]).map_err(|e| err(e.message))?;
    parse_devices(&json)
}

/// The `get-updates` listing, for one device or for all. fwupd 2 prints an
/// empty list; older ones exit 2 and say "No updates available" or "No
/// updatable devices" instead. Neither is a failure.
fn list_updates(device_id: Option<&str>) -> Result<Vec<Device>> {
    let mut args = vec!["get-updates", "--json"];
    if let Some(id) = device_id {
        args.push(id);
    }
    match system::run("fwupdmgr", &args) {
        Ok(json) => parse_devices(&json),
        Err(e) if nothing_to_update(&e.message) => Ok(Vec::new()),
        Err(e) => Err(err(e.message)),
    }
}

fn nothing_to_update(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("no updates available") || m.contains("no updatable devices")
}

fn err(message: impl Into<String>) -> Error {
    Error::from_source(SourceKind::Fwupd, message)
}

pub struct Fwupd {
    /// Probed once: asking the daemon costs a process spawn, and `status`
    /// is called on every search across every source.
    status: OnceLock<SourceStatus>,
}

impl Fwupd {
    pub fn new(_system: &SystemInfo) -> Fwupd {
        Fwupd {
            status: OnceLock::new(),
        }
    }

    fn probe() -> SourceStatus {
        let kind = SourceKind::Fwupd;
        if system::which("fwupdmgr").is_none() {
            return SourceStatus {
                kind,
                available: false,
                reason: Some("fwupd is not installed.".to_string()),
                detail: None,
                searchable: false,
                setup: None,
            };
        }
        match system::run("fwupdmgr", &["get-devices", "--json"]) {
            Ok(_) => SourceStatus {
                kind,
                available: true,
                reason: None,
                detail: daemon_version(),
                searchable: false,
                setup: None,
            },
            Err(_) => SourceStatus {
                kind,
                available: false,
                reason: Some("The fwupd service is not running.".to_string()),
                detail: None,
                searchable: false,
                setup: None,
            },
        }
    }

    fn find(&self, id: &str) -> Result<Device> {
        list_devices()?
            .into_iter()
            .find(|d| d.device_id == id)
            .ok_or_else(|| err(format!("fwupd does not know a device with id {id}.")))
    }
}

/// "fwupd 2.1.7", from the `runtime org.freedesktop.fwupd` line of
/// `fwupdmgr --version`.
fn daemon_version() -> Option<String> {
    let text = system::run("fwupdmgr", &["--version"]).ok()?;
    text.lines()
        .filter(|l| l.contains("org.freedesktop.fwupd"))
        .filter_map(|l| l.split_whitespace().next_back())
        .next_back()
        .map(|v| format!("fwupd {v}"))
}

impl Source for Fwupd {
    fn kind(&self) -> SourceKind {
        SourceKind::Fwupd
    }

    fn status(&self) -> SourceStatus {
        self.status.get_or_init(Fwupd::probe).clone()
    }

    /// Matches the query against the device's name, vendor, summary and
    /// plugin, and against the word "firmware", so both "firmware" and a
    /// device name find it.
    fn search(&self, query: &Query) -> Result<Vec<Package>> {
        let q = query.text.trim().to_lowercase();
        let packages = list_devices()?
            .iter()
            .filter_map(package)
            .filter(|p| {
                let haystack = format!(
                    "{} {} {} firmware",
                    p.name,
                    p.developer.as_deref().unwrap_or(""),
                    p.summary.as_deref().unwrap_or("")
                )
                .to_lowercase();
                q.is_empty() || haystack.contains(&q)
            })
            .take(query.limit.max(1))
            .collect();
        Ok(packages)
    }

    fn installed(&self) -> Result<Vec<Package>> {
        Ok(list_devices()?.iter().filter_map(package).collect())
    }

    fn updates(&self) -> Result<Vec<Update>> {
        Ok(list_updates(None)?.iter().filter_map(update).collect())
    }

    fn details(&self, id: &str) -> Result<Package> {
        let mut device = self.find(id)?;
        if let Some(with_updates) = list_updates(Some(id))?
            .into_iter()
            .find(|d| d.device_id == id)
        {
            device.releases = with_updates.releases;
        }
        let mut p = package(&device).ok_or_else(|| {
            err(format!(
                "{} has no version fwupd can show.",
                device.name.as_deref().unwrap_or(id)
            ))
        })?;
        p.description = device.newest_release().and_then(|r| r.description.clone());
        Ok(p)
    }

    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        match op {
            Op::Update { package } => {
                let device = self.find(&package.id)?;
                let name = device.name.as_deref().unwrap_or(&package.id);
                Ok(vec![update_step(&package.id, name)])
            }
            Op::UpdateAll { .. } => Ok(vec![update_all_step()]),
            Op::Refresh { .. } => Ok(vec![refresh_step()]),
            // The planner expands a setup through `Source::setup`.
            Op::Setup { .. } => Ok(Vec::new()),
            Op::Install { .. } | Op::Remove { .. } => Err(err(
                "Firmware is updated, not installed, and cannot be removed.",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_RELEASES: &str = r#"{"Devices":[{"Name":"UEFI dbx","DeviceId":"abc","Vendor":"Microsoft",
        "Version":"20230301","Flags":["updatable","needs-reboot"],
        "Releases":[{"Version":"20260402","Summary":"newest","Size":10,"Created":1756771200},
                    {"Version":"20250902","Summary":"older","Size":9,"Created":1756771200}]}]}"#;

    #[test]
    fn the_first_release_is_the_one_offered() {
        let devices = parse_devices(TWO_RELEASES).unwrap();
        let u = update(&devices[0]).unwrap();
        assert_eq!(u.to, "20260402");
        assert_eq!(u.from.as_deref(), Some("20230301"));
        assert_eq!(u.summary.as_deref(), Some("newest"));
        assert_eq!(u.download_size, Some(10));
        assert_eq!(u.published, Some(1756771200));
        assert_eq!(u.kind, PackageKind::Firmware);
        assert_eq!(u.package.id, "abc");
    }

    #[test]
    fn a_device_without_a_name_is_not_a_package() {
        let devices = parse_devices(r#"{"Devices":[{"DeviceId":"x","Version":"1"}]}"#).unwrap();
        assert!(package(&devices[0]).is_none());
        assert!(update(&devices[0]).is_none());
        assert!(fold(&devices, &[]).is_empty());
    }

    #[test]
    fn a_device_without_a_version_is_reported_but_not_listed() {
        let devices = parse_devices(r#"{"Devices":[{"DeviceId":"x","Name":"Panel"}]}"#).unwrap();
        assert!(package(&devices[0]).is_none());
        let folded = fold(&devices, &[]);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].name, "Panel");
        assert_eq!(folded[0].version, None);
    }

    #[test]
    fn bad_json_names_the_tool() {
        let e = parse_devices("not json").unwrap_err();
        assert!(e.message.starts_with("fwupdmgr printed"), "{}", e.message);
        assert_eq!(e.source_kind, Some(SourceKind::Fwupd));
    }

    #[test]
    fn an_empty_answer_is_not_a_failure() {
        assert!(nothing_to_update(
            "fwupdmgr get-updates failed: No updates available"
        ));
        assert!(nothing_to_update(
            "fwupdmgr get-updates failed: No updatable devices"
        ));
        assert!(!nothing_to_update(
            "fwupdmgr get-updates failed: failed to connect to daemon"
        ));
    }

    #[test]
    fn steps_never_need_root() {
        for s in [update_step("abc", "TPM"), update_all_step(), refresh_step()] {
            assert!(!s.needs_root);
            assert_eq!(s.command.program, "fwupdmgr");
            assert_eq!(s.source, SourceKind::Fwupd);
        }
        assert_eq!(update_step("abc", "TPM").title, "Updating firmware for TPM");
    }
}
