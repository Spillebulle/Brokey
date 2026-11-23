//! Windows optional features: Hyper-V, WSL, Windows Sandbox and the rest of
//! what is already on the machine and is switched on rather than downloaded.
//! The page that lists them takes the Drivers slot on Windows.
//!
//! **Reading.** The spec planned a `DISM /Online /Get-Features` query, the
//! shape `fwupd.rs` has for `fwupdmgr`. Measured on the reference machine,
//! that cannot work from the window: DISM refuses every `/Online` query
//! from a process that is not elevated, with error 740, and the window
//! never is. So the list is WMI's `Win32_OptionalFeature` class, asked
//! through COM, which is documented, answers a standard user, carries the
//! same names DISM uses and a caption a person can read, and involves no
//! tool's output at all. The registry under `Component Based Servicing\
//! Notifications\OptionalFeatures` was the other candidate: it is readable,
//! but it is undocumented, has no captions, and listed 114 features where
//! WMI listed 126.
//!
//! The OpenSSH client and server are not optional features but Windows
//! capabilities, which WMI does not list and DISM again only answers
//! elevated. Their state is read from the servicing stack's own package
//! records in the registry, `CurrentState` under `Component Based
//! Servicing\Packages`, which is what DISM itself consults. Only the two
//! capabilities in [`KNOWN`] are read or changed; the rest of the
//! capability list is out of reach of a standard user and is not guessed.
//!
//! **Changing.** Turning a feature on or off is a step through the helper,
//! `dism.exe` from the system directory with a fixed set of arguments,
//! which `transaction::allow` rebuilds with [`step`] and compares whole.
//! `/NoRestart` is always passed: the store never restarts a machine. DISM
//! then exits 3010 when the change needs a restart to finish, which is a
//! success, and [`restart_note`] is the one place that says so; the helper
//! and the runner carry the sentence to the end of the plan.
//!
//! Nothing here runs a program. The pure halves are tested on every
//! platform against a fixture captured from the reference machine; the
//! COM and registry reads are `#[cfg(windows)]`.

use crate::model::*;
use crate::{Error, Op, Query, Result, Source};
use serde::Deserialize;
use std::sync::Mutex;

/// The features a person is most likely to be looking for, in the order
/// the page lists them. `capability` marks the two that DISM treats as
/// capabilities rather than features: a different verb, and an id that is
/// only ever one of these.
pub struct Known {
    pub id: &'static str,
    pub name: &'static str,
    pub summary: &'static str,
    /// Other words search should find it by.
    pub keywords: &'static [&'static str],
    pub capability: bool,
    /// The servicing package name a capability's state is read from.
    pub package: &'static str,
}

pub const KNOWN: [Known; 8] = [
    Known {
        id: "Microsoft-Windows-Subsystem-Linux",
        name: "Windows Subsystem for Linux",
        summary: "Runs Linux distributions alongside Windows. WSL 2 also needs Virtual Machine Platform.",
        keywords: &["wsl", "linux", "ubuntu"],
        capability: false,
        package: "",
    },
    Known {
        id: "VirtualMachinePlatform",
        name: "Virtual Machine Platform",
        summary: "The virtualisation WSL 2 and other lightweight virtual machines run on.",
        keywords: &["wsl", "vmp", "virtualisation", "virtualization"],
        capability: false,
        package: "",
    },
    Known {
        id: "Microsoft-Hyper-V-All",
        name: "Hyper-V",
        summary: "Runs full virtual machines on the hypervisor built into Windows, with its manager and PowerShell module.",
        keywords: &[
            "hyperv",
            "virtual machine",
            "vm",
            "virtualisation",
            "virtualization",
        ],
        capability: false,
        package: "",
    },
    Known {
        id: "Containers-DisposableClientVM",
        name: "Windows Sandbox",
        summary: "A throwaway desktop for running something you do not trust. Everything in it is deleted when it closes.",
        keywords: &["sandbox"],
        capability: false,
        package: "",
    },
    Known {
        id: "HypervisorPlatform",
        name: "Windows Hypervisor Platform",
        summary: "Lets other virtualisation software, such as VirtualBox, VMware and QEMU, run on the Windows hypervisor.",
        keywords: &["whpx", "virtualbox", "vmware", "qemu"],
        capability: false,
        package: "",
    },
    Known {
        id: "NetFx3",
        name: ".NET Framework 3.5",
        summary: "Includes .NET 2.0 and 3.0. Older applications ask for it when they start.",
        keywords: &["dotnet", ".net", "netfx", "framework"],
        capability: false,
        package: "",
    },
    Known {
        id: "OpenSSH.Client~~~~0.0.1.0",
        name: "OpenSSH client",
        summary: "The ssh, scp and sftp commands.",
        keywords: &["ssh", "scp", "sftp", "openssh"],
        capability: true,
        package: "OpenSSH-Client-Package~",
    },
    Known {
        id: "OpenSSH.Server~~~~0.0.1.0",
        name: "OpenSSH server",
        summary: "Lets other machines connect to this one over SSH.",
        keywords: &["ssh", "sshd", "openssh"],
        capability: true,
        package: "OpenSSH-Server-Package~",
    },
];

pub fn known(id: &str) -> Option<&'static Known> {
    KNOWN.iter().find(|k| k.id == id)
}

/// One row of `Win32_OptionalFeature`, with WMI's own property names, which
/// is also the shape of the fixture captured from the reference machine.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RawFeature {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Caption")]
    pub caption: Option<String>,
    #[serde(rename = "InstallState")]
    pub install_state: u32,
}

/// `InstallState` as the class documents it: 1 enabled, 2 disabled,
/// 3 absent, 4 unknown. Anything else is unknown too.
pub fn state_of(install_state: u32) -> FeatureState {
    match install_state {
        1 => FeatureState::Enabled,
        2 => FeatureState::Disabled,
        3 => FeatureState::Absent,
        _ => FeatureState::Unknown,
    }
}

/// The servicing stack's `CurrentState` for an installed package. 0x70 is
/// installed and 0x80 permanent; every other value (staged, superseded,
/// pending) is not a capability somebody can use now.
const CBS_INSTALLED: [u32; 2] = [0x70, 0x80];

/// A capability's state from the servicing packages whose key names begin
/// with its prefix: enabled when one of them is installed, disabled when
/// some are recorded but none is, and absent when Windows records none,
/// which is what a capability that would be downloaded looks like.
/// `-Wrapper` packages are a different package of the same family and are
/// not counted.
pub fn capability_state(prefix: &str, packages: &[(String, u32)]) -> FeatureState {
    let mut seen = false;
    for (name, state) in packages {
        if !name.starts_with(prefix) {
            continue;
        }
        seen = true;
        if CBS_INSTALLED.contains(state) {
            return FeatureState::Enabled;
        }
    }
    if seen {
        FeatureState::Disabled
    } else {
        FeatureState::Absent
    }
}

/// The page's list: the well-known ones first in [`KNOWN`]'s order, then the
/// rest by name. A well-known feature WMI does not list is left out rather
/// than drawn: it is not on this edition or this build, and a row for it
/// would offer a switch that cannot work. The two capabilities are always
/// listed, from `packages`.
pub fn features(raw: &[RawFeature], packages: &[(String, u32)]) -> Vec<WindowsFeature> {
    let mut known_rows = Vec::new();
    for k in &KNOWN {
        let state = if k.capability {
            Some(capability_state(k.package, packages))
        } else {
            raw.iter()
                .find(|r| r.name == k.id)
                .map(|r| state_of(r.install_state))
        };
        if let Some(state) = state {
            known_rows.push(WindowsFeature {
                id: k.id.to_string(),
                name: k.name.to_string(),
                summary: Some(k.summary.to_string()),
                state,
                well_known: true,
            });
        }
    }
    let mut rest: Vec<WindowsFeature> = raw
        .iter()
        .filter(|r| !r.name.is_empty() && known(&r.name).is_none())
        .map(|r| WindowsFeature {
            id: r.name.clone(),
            name: r
                .caption
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .unwrap_or(&r.name)
                .to_string(),
            summary: None,
            state: state_of(r.install_state),
            well_known: false,
        })
        .collect();
    rest.sort_by_key(|f| f.name.to_lowercase());
    known_rows.extend(rest);
    known_rows
}

pub fn parse_fixture(json: &str) -> Result<Vec<RawFeature>> {
    serde_json::from_str(json).map_err(|e| err(format!("The feature list was not readable: {e}.")))
}

// ------------------------------------------------------------- the steps

/// Whether a step turns something on or off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    On,
    Off,
}

/// Whether `id` may be put in a DISM argument: Windows's feature names are
/// letters, digits, dashes, dots and underscores, beginning with a letter
/// or a digit, so an option cannot be smuggled in as one in either of the
/// spellings DISM accepts. A capability's id is checked against [`KNOWN`]
/// instead, by [`step`].
pub fn valid_feature_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
}

/// The one command that changes a feature. A pure function of the program
/// path, the id and the direction, so `transaction::allow` can rebuild what
/// a plan should have carried and compare it whole. `None` when the id is
/// neither a valid feature name nor one of the known capabilities.
///
/// `/English` makes DISM's output something the log can be read by anyone
/// helping, whatever the display language. `/All` on enabling turns on the
/// parent features the one asked for depends on, which Hyper-V needs.
/// `/NoRestart` always: DISM never restarts the machine and never asks.
pub fn step(change: Change, id: &str, name: &str, dism: &str) -> Option<Step> {
    let capability = known(id).is_some_and(|k| k.capability);
    if !capability && !valid_feature_id(id) {
        return None;
    }
    let mut args = vec!["/English".to_string(), "/Online".to_string()];
    match (capability, change) {
        (false, Change::On) => {
            args.push("/Enable-Feature".to_string());
            args.push(format!("/FeatureName:{id}"));
            args.push("/All".to_string());
        }
        (false, Change::Off) => {
            args.push("/Disable-Feature".to_string());
            args.push(format!("/FeatureName:{id}"));
        }
        (true, Change::On) => {
            args.push("/Add-Capability".to_string());
            args.push(format!("/CapabilityName:{id}"));
        }
        (true, Change::Off) => {
            args.push("/Remove-Capability".to_string());
            args.push(format!("/CapabilityName:{id}"));
        }
    }
    args.push("/NoRestart".to_string());
    let title = match change {
        Change::On => format!("Turning on {name}"),
        Change::Off => format!("Turning off {name}"),
    };
    Some(Step {
        source: SourceKind::Features,
        title,
        command: Command {
            program: dism.to_string(),
            args,
            env: Vec::new(),
            cwd: None,
        },
        needs_root: true,
        weight: 6,
    })
}

/// The id and direction a DISM command carries, read back from its
/// arguments, for the closed list to rebuild from. `None` for anything
/// [`step`] never builds.
pub fn read_back(command: &Command) -> Option<(Change, String)> {
    let verb = command.args.get(2)?;
    let target = command.args.get(3)?;
    let (change, prefix) = match verb.as_str() {
        "/Enable-Feature" => (Change::On, "/FeatureName:"),
        "/Disable-Feature" => (Change::Off, "/FeatureName:"),
        "/Add-Capability" => (Change::On, "/CapabilityName:"),
        "/Remove-Capability" => (Change::Off, "/CapabilityName:"),
        _ => return None,
    };
    let id = target.strip_prefix(prefix)?;
    Some((change, id.to_string()))
}

/// What DISM's exit 3010, `ERROR_SUCCESS_REBOOT_REQUIRED`, means: the
/// change was made and finishes when Windows restarts.
pub const RESTART_REQUIRED: i32 = 3010;

/// The sentence the end of the plan carries when a change needs a restart.
pub const RESTART_NOTE: &str =
    "Windows needs a restart to finish this change. Restart the computer when it suits you.";

/// Whether a step that exited with `code` succeeded and is waiting on a
/// restart. Only a Features step is read this way: a 3010 from anything
/// else is left to the ordinary rule, which treats every non-zero exit as
/// a failure.
pub fn restart_note(step: &Step, code: Option<i32>) -> Option<&'static str> {
    (step.source == SourceKind::Features && code == Some(RESTART_REQUIRED)).then_some(RESTART_NOTE)
}

/// `dism.exe` in the system directory, the only one this source names and
/// the only one the closed list admits.
#[cfg(windows)]
pub fn dism_program() -> String {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buffer = [0u16; 260];
    // SAFETY: `buffer` is a writable array of exactly the length passed
    // alongside it, which is what this call is documented to fill.
    let written = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    // As `arp::msiexec_program`: a failure or a truncation is not worth a
    // second attempt, and the fixed answer is the right one on every
    // machine where it would be reached.
    let directory = if written == 0 || written > buffer.len() {
        r"C:\Windows\System32".to_string()
    } else {
        String::from_utf16_lossy(&buffer[..written])
    };
    format!(r"{directory}\dism.exe")
}

/// Off Windows there is no DISM. The bare name keeps [`step`] testable.
#[cfg(not(windows))]
pub fn dism_program() -> String {
    "dism.exe".to_string()
}

fn err(message: impl Into<String>) -> Error {
    Error::from_source(SourceKind::Features, message)
}

// ------------------------------------------------------------- the reads

/// Every optional feature WMI lists, and the capabilities' packages.
#[cfg(windows)]
pub fn read() -> Result<Vec<WindowsFeature>> {
    let raw = wmi::optional_features().map_err(|e| {
        err(format!(
            "Windows did not answer the question of which features it has: {e}. Restart Brokey and try again; if it keeps happening, the Windows Management Instrumentation service is not running."
        ))
    })?;
    Ok(features(&raw, &capability_packages()))
}

#[cfg(not(windows))]
pub fn read() -> Result<Vec<WindowsFeature>> {
    Err(err("Windows features exist only on Windows."))
}

/// The page's report: the list, or the sentence for why there is none.
pub fn report() -> FeaturesReport {
    match read() {
        Ok(features) => FeaturesReport {
            available: true,
            note: None,
            features,
        },
        Err(e) => FeaturesReport {
            available: false,
            note: Some(e.message),
            features: Vec::new(),
        },
    }
}

/// The servicing packages of the two known capabilities, by key name, with
/// their `CurrentState`. A key that cannot be read is left out, which reads
/// as "not installed" rather than as a guess.
#[cfg(windows)]
fn capability_packages() -> Vec<(String, u32)> {
    const PACKAGES: &str =
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\Packages";
    let Ok(root) = windows_registry::LOCAL_MACHINE.open(PACKAGES) else {
        return Vec::new();
    };
    let Ok(names) = root.keys() else {
        return Vec::new();
    };
    names
        .filter(|name| {
            KNOWN
                .iter()
                .any(|k| k.capability && name.starts_with(k.package))
        })
        .filter_map(|name| {
            let state = root.open(&name).ok()?.get_u32("CurrentState").ok()?;
            Some((name, state))
        })
        .collect()
}

#[cfg(windows)]
mod wmi {
    //! `SELECT Name, Caption, InstallState FROM Win32_OptionalFeature`,
    //! through COM. Called on a worker thread: COM is initialised here for
    //! the thread in the multithreaded apartment, and uninitialised again
    //! only if this call is what initialised it.

    use super::RawFeature;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoSetProxyBlanket, CoUninitialize, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
        RPC_C_IMP_LEVEL_IMPERSONATE,
    };
    use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
    use windows::Win32::System::Variant::{VARIANT, VT_BSTR, VT_I4, VT_UI4, VariantClear};
    use windows::Win32::System::Wmi::{
        IEnumWbemClassObject, IWbemClassObject, IWbemLocator, WBEM_FLAG_FORWARD_ONLY,
        WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_INFINITE, WbemLocator,
    };
    use windows::core::{BSTR, PCWSTR, w};

    /// Balances a successful `CoInitializeEx`, including `S_FALSE`, which
    /// means the thread was already in this apartment and still counts.
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

    /// A `VARIANT` that is cleared when it goes, so a `BSTR` WMI allocated
    /// into it is freed.
    struct Value(VARIANT);

    impl Drop for Value {
        fn drop(&mut self) {
            // SAFETY: the variant is either zeroed or was filled by
            // `IWbemClassObject::Get`, and both are valid to clear.
            let _ = unsafe { VariantClear(&mut self.0) };
        }
    }

    pub fn optional_features() -> windows::core::Result<Vec<RawFeature>> {
        // SAFETY: every call below is a COM call made on this thread after
        // it has joined an apartment, with arguments that live for the
        // whole call; interfaces are released by their `Drop`, before the
        // `Apartment` guard, which was declared first and so drops last.
        unsafe {
            // A thread that is already in a single-threaded apartment
            // answers RPC_E_CHANGED_MODE, and COM is usable there as it is.
            let apartment = Apartment(CoInitializeEx(None, COINIT_MULTITHREADED).is_ok());
            let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
            let services = locator.ConnectServer(
                &BSTR::from(r"ROOT\CIMV2"),
                &BSTR::new(),
                &BSTR::new(),
                &BSTR::new(),
                0,
                &BSTR::new(),
                None,
            )?;
            // The process-wide default may be too low for WMI to
            // impersonate the caller, and `CoInitializeSecurity` belongs to
            // whoever started the process, which is WebView2's host in the
            // window. Setting the blanket on this one proxy needs neither.
            CoSetProxyBlanket(
                &services,
                RPC_C_AUTHN_WINNT,
                RPC_C_AUTHZ_NONE,
                PCWSTR::null(),
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            )?;
            let rows: IEnumWbemClassObject = services.ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from("SELECT Name, Caption, InstallState FROM Win32_OptionalFeature"),
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            )?;
            let mut out = Vec::new();
            loop {
                let mut row: [Option<IWbemClassObject>; 1] = [None];
                let mut returned = 0u32;
                rows.Next(WBEM_INFINITE, &mut row, &mut returned).ok()?;
                let Some(object) = row[0].take().filter(|_| returned == 1) else {
                    break;
                };
                out.push(RawFeature {
                    name: text(&object, w!("Name"))?.unwrap_or_default(),
                    caption: text(&object, w!("Caption"))?,
                    install_state: number(&object, w!("InstallState"))?.unwrap_or(4),
                });
            }
            drop(rows);
            drop(services);
            drop(locator);
            drop(apartment);
            Ok(out)
        }
    }

    /// A string property, `None` when WMI has it as null.
    unsafe fn text(
        object: &IWbemClassObject,
        name: PCWSTR,
    ) -> windows::core::Result<Option<String>> {
        let mut value = Value(VARIANT::default());
        // SAFETY: `value.0` is a zeroed VARIANT for WMI to fill, and the
        // union is read only through the field its `vt` names.
        unsafe {
            object.Get(name, 0, &mut value.0, None, None)?;
            let inner = &value.0.Anonymous.Anonymous;
            if inner.vt != VT_BSTR {
                return Ok(None);
            }
            Ok(Some(inner.Anonymous.bstrVal.to_string()))
        }
    }

    /// A 32-bit unsigned property, which WMI hands over as `VT_I4`.
    unsafe fn number(
        object: &IWbemClassObject,
        name: PCWSTR,
    ) -> windows::core::Result<Option<u32>> {
        let mut value = Value(VARIANT::default());
        // SAFETY: as in `text`.
        unsafe {
            object.Get(name, 0, &mut value.0, None, None)?;
            let inner = &value.0.Anonymous.Anonymous;
            Ok(match inner.vt {
                VT_I4 => Some(inner.Anonymous.lVal as u32),
                VT_UI4 => Some(inner.Anonymous.ulVal),
                _ => None,
            })
        }
    }
}

// ------------------------------------------------------------- the source

pub struct Features {
    /// Read once per change: `search` is called on every keystroke, and a
    /// WMI query is a cross-process call. Cleared by `finished`, so the
    /// list after a plan is the machine's and not the one before it.
    cache: Mutex<Option<Vec<WindowsFeature>>>,
}

impl Default for Features {
    fn default() -> Self {
        Self::new()
    }
}

impl Features {
    pub fn new() -> Features {
        Features {
            cache: Mutex::new(None),
        }
    }

    fn list(&self) -> Result<Vec<WindowsFeature>> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(list) = cache.as_ref() {
            return Ok(list.clone());
        }
        let list = read()?;
        *cache = Some(list.clone());
        Ok(list)
    }

    fn find(&self, id: &str) -> Result<WindowsFeature> {
        self.list()?
            .into_iter()
            .find(|f| f.id == id)
            .ok_or_else(|| err(format!("Windows has no optional feature called {id}.")))
    }
}

/// The feature as a package, for search and the detail page.
pub fn package(f: &WindowsFeature) -> Package {
    let mut p = Package::new(SourceKind::Features, &f.id, &f.name);
    p.summary = Some(
        f.summary
            .clone()
            .unwrap_or_else(|| "A Windows optional feature.".to_string()),
    );
    p.developer = Some("Microsoft".to_string());
    p.installed = f.state == FeatureState::Enabled;
    p.facts = vec![
        ("Windows name".to_string(), f.id.clone()),
        (
            "State".to_string(),
            match f.state {
                FeatureState::Enabled => "On",
                FeatureState::Disabled => "Off",
                FeatureState::Absent => "Off, and turning it on downloads it from Windows Update",
                FeatureState::Unknown => "Unknown",
            }
            .to_string(),
        ),
    ];
    p
}

/// Whether a well-known feature answers the query, by its name, its
/// Windows name or one of its keywords. Only the well-known ones are
/// searched: the other hundred-odd are names like
/// `IIS-ASPNET45` that would crowd every search and are on the Features
/// page for whoever is looking for one.
pub fn matches(f: &WindowsFeature, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() || !f.well_known {
        return false;
    }
    let keywords = known(&f.id).map(|k| k.keywords).unwrap_or(&[]);
    f.name.to_lowercase().contains(&q)
        || f.id.to_lowercase().contains(&q)
        || keywords.iter().any(|k| k.contains(&q) || q.contains(k))
}

impl Source for Features {
    fn kind(&self) -> SourceKind {
        SourceKind::Features
    }

    /// Always there on Windows. A WMI failure is a failed read, said where
    /// it happens, not a missing source.
    fn status(&self) -> SourceStatus {
        SourceStatus {
            kind: SourceKind::Features,
            available: cfg!(windows),
            reason: (!cfg!(windows)).then(|| "Windows features exist only on Windows.".to_string()),
            detail: None,
            searchable: false,
            setup: None,
        }
    }

    fn search(&self, query: &Query) -> Result<Vec<Package>> {
        Ok(self
            .list()?
            .iter()
            .filter(|f| matches(f, &query.text))
            .take(query.limit.max(1))
            .map(package)
            .collect())
    }

    /// Nothing: the Installed page is applications, and an enabled feature
    /// is listed, with its switch, on the Features page.
    fn installed(&self) -> Result<Vec<Package>> {
        Ok(Vec::new())
    }

    /// Windows Update keeps features current; there is nothing to offer.
    fn updates(&self) -> Result<Vec<Update>> {
        Ok(Vec::new())
    }

    fn details(&self, id: &str) -> Result<Package> {
        Ok(package(&self.find(id)?))
    }

    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        let (change, package) = match op {
            Op::Install { package } => (Change::On, package),
            Op::Remove { package } => (Change::Off, package),
            Op::Update { .. } | Op::UpdateAll { .. } => {
                return Err(err(
                    "Windows features are turned on or off. Windows Update keeps them up to date.",
                ));
            }
            Op::Refresh { .. } | Op::Setup { .. } => return Ok(Vec::new()),
        };
        let feature = self.find(&package.id)?;
        let already = match change {
            Change::On => feature.state == FeatureState::Enabled,
            Change::Off => feature.state != FeatureState::Enabled,
        };
        if already {
            return Ok(Vec::new());
        }
        step(change, &feature.id, &feature.name, &dism_program())
            .map(|s| vec![s])
            .ok_or_else(|| {
                err(format!(
                    "{} is not a feature name Brokey can pass to DISM.",
                    feature.id
                ))
            })
    }

    fn finished(&self, _op: &Op, _ok: bool) {
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<RawFeature> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/windows/features/optional-features.json"
        );
        parse_fixture(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// The reference machine's OpenSSH client: one installed version, one
    /// superseded, one staged, and a wrapper package of the same family.
    fn ssh_packages() -> Vec<(String, u32)> {
        vec![
            (
                "OpenSSH-Client-Package-Wrapper~31bf3856ad364e35~amd64~~10.0.28000.3542".into(),
                0x70,
            ),
            (
                "OpenSSH-Client-Package~31bf3856ad364e35~amd64~~10.0.28000.3181".into(),
                0x50,
            ),
            (
                "OpenSSH-Client-Package~31bf3856ad364e35~amd64~~10.0.28000.3542".into(),
                0x70,
            ),
            (
                "OpenSSH-Client-Package~31bf3856ad364e35~amd64~~10.0.28000.7".into(),
                0x40,
            ),
        ]
    }

    #[test]
    fn the_reference_machine_lists_every_feature_wmi_gave() {
        let raw = fixture();
        assert_eq!(raw.len(), 126);
        let list = features(&raw, &ssh_packages());
        // 126 features, five of them well known and present (NetFx3 is not
        // on this build), plus the two capabilities.
        assert_eq!(list.len(), 128);
        let well: Vec<&str> = list
            .iter()
            .filter(|f| f.well_known)
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(
            well,
            [
                "Windows Subsystem for Linux",
                "Virtual Machine Platform",
                "Hyper-V",
                "Windows Sandbox",
                "Windows Hypervisor Platform",
                "OpenSSH client",
                "OpenSSH server",
            ]
        );
        assert!(
            list[..7].iter().all(|f| f.well_known),
            "the well-known ones come first"
        );
    }

    #[test]
    fn states_are_read_as_the_class_documents_them() {
        let list = features(&fixture(), &ssh_packages());
        let state = |id: &str| list.iter().find(|f| f.id == id).unwrap().state;
        assert_eq!(
            state("Microsoft-Windows-Subsystem-Linux"),
            FeatureState::Enabled
        );
        assert_eq!(state("Microsoft-Hyper-V-All"), FeatureState::Disabled);
        assert_eq!(
            state("Windows-Defender-Default-Definitions"),
            FeatureState::Absent
        );
        assert_eq!(state("OpenSSH.Client~~~~0.0.1.0"), FeatureState::Enabled);
        assert_eq!(state("OpenSSH.Server~~~~0.0.1.0"), FeatureState::Absent);
        assert_eq!(state_of(4), FeatureState::Unknown);
        assert_eq!(state_of(0), FeatureState::Unknown);
    }

    #[test]
    fn a_feature_without_a_caption_is_named_by_its_windows_name() {
        let list = features(&fixture(), &[]);
        let hns = list.iter().find(|f| f.id == "Containers-HNS").unwrap();
        assert_eq!(hns.name, "Containers-HNS");
        let telnet = list.iter().find(|f| f.id == "TelnetClient").unwrap();
        assert_eq!(telnet.name, "Telnet Client");
        assert_eq!(telnet.summary, None);
    }

    #[test]
    fn a_capability_with_no_installed_package_is_off() {
        let p = vec![("OpenSSH-Client-Package~x~amd64~~1".to_string(), 0x40)];
        assert_eq!(
            capability_state("OpenSSH-Client-Package~", &p),
            FeatureState::Disabled
        );
        assert_eq!(
            capability_state("OpenSSH-Client-Package~", &[]),
            FeatureState::Absent
        );
        let wrapper_only = vec![("OpenSSH-Client-Package-Wrapper~x".to_string(), 0x70)];
        assert_eq!(
            capability_state("OpenSSH-Client-Package~", &wrapper_only),
            FeatureState::Absent,
            "a wrapper is not the capability"
        );
    }

    #[test]
    fn steps_are_dism_with_no_restart_and_need_administrator() {
        let on = step(
            Change::On,
            "Microsoft-Windows-Subsystem-Linux",
            "WSL",
            "dism.exe",
        )
        .unwrap();
        assert_eq!(
            on.command.args,
            [
                "/English",
                "/Online",
                "/Enable-Feature",
                "/FeatureName:Microsoft-Windows-Subsystem-Linux",
                "/All",
                "/NoRestart"
            ]
        );
        assert_eq!(on.title, "Turning on WSL");
        assert!(on.needs_root);
        assert_eq!(on.source, SourceKind::Features);
        let off = step(
            Change::Off,
            "Containers-DisposableClientVM",
            "Windows Sandbox",
            "dism.exe",
        )
        .unwrap();
        assert_eq!(
            off.command.args,
            [
                "/English",
                "/Online",
                "/Disable-Feature",
                "/FeatureName:Containers-DisposableClientVM",
                "/NoRestart"
            ]
        );
        assert_eq!(off.title, "Turning off Windows Sandbox");
        let ssh = step(
            Change::On,
            "OpenSSH.Client~~~~0.0.1.0",
            "OpenSSH client",
            "dism.exe",
        )
        .unwrap();
        assert_eq!(
            ssh.command.args,
            [
                "/English",
                "/Online",
                "/Add-Capability",
                "/CapabilityName:OpenSSH.Client~~~~0.0.1.0",
                "/NoRestart"
            ]
        );
        for s in [&on, &off, &ssh] {
            assert_eq!(
                read_back(&s.command).map(|(_, id)| id),
                Some(s.command.args[3].split_once(':').unwrap().1.to_string())
            );
        }
    }

    #[test]
    fn an_id_that_could_be_an_option_builds_no_step() {
        for id in [
            "",
            "/Source:x",
            "-x",
            "a b",
            "a:b",
            "NetFx3 /LimitAccess",
            "x\"y",
            "Some.Capability~~~~1.0",
        ] {
            assert!(step(Change::On, id, "x", "dism.exe").is_none(), "{id:?}");
        }
        assert!(step(Change::On, &"a".repeat(129), "x", "dism.exe").is_none());
        assert!(step(Change::On, "NetFx3", ".NET", "dism.exe").is_some());
    }

    #[test]
    fn only_a_features_step_that_exits_3010_waits_on_a_restart() {
        let s = step(Change::On, "NetFx3", ".NET", "dism.exe").unwrap();
        assert_eq!(restart_note(&s, Some(3010)), Some(RESTART_NOTE));
        assert_eq!(restart_note(&s, Some(0)), None);
        assert_eq!(restart_note(&s, Some(1)), None);
        assert_eq!(restart_note(&s, None), None);
        let mut other = s.clone();
        other.source = SourceKind::Winget;
        assert_eq!(restart_note(&other, Some(3010)), None);
    }

    #[test]
    fn search_finds_well_known_features_by_their_everyday_words() {
        let list = features(&fixture(), &ssh_packages());
        let hits = |q: &str| -> Vec<String> {
            list.iter()
                .filter(|f| matches(f, q))
                .map(|f| f.name.clone())
                .collect()
        };
        assert!(hits("wsl").contains(&"Windows Subsystem for Linux".to_string()));
        assert!(hits("Sandbox") == ["Windows Sandbox"]);
        assert!(hits("ssh").contains(&"OpenSSH client".to_string()));
        assert!(
            hits("telnet").is_empty(),
            "only well-known features are searched"
        );
        assert!(hits("").is_empty());
    }

    #[test]
    fn sentences_follow_the_copy_rules() {
        let mut all: Vec<&str> = KNOWN.iter().flat_map(|k| [k.name, k.summary]).collect();
        all.push(RESTART_NOTE);
        for s in all {
            assert!(!s.contains('\u{2014}'), "no em dashes: {s}");
            assert!(!s.contains("ization"), "British spelling: {s}");
        }
        for k in &KNOWN {
            assert!(k.summary.ends_with('.'), "{}", k.summary);
        }
    }
}
