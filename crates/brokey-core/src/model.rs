//! The data every part of Brokey agrees on. Serialised as JSON to the page
//! (`frontend/src/types.ts` mirrors it field for field) and passed between the
//! sources, the grouper, the transaction runner and the helper.
//!
//! Everything here is plain data. Nothing in this module does I/O.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Where a package can come from. The order is the order sources are listed
/// in the interface and searched in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Pacman,
    Aur,
    Flatpak,
    Snap,
    Apt,
    Dnf,
    Github,
    Fwupd,
    Chwd,
}

impl SourceKind {
    pub const ALL: [SourceKind; 9] = [
        SourceKind::Pacman,
        SourceKind::Aur,
        SourceKind::Flatpak,
        SourceKind::Snap,
        SourceKind::Apt,
        SourceKind::Dnf,
        SourceKind::Github,
        SourceKind::Fwupd,
        SourceKind::Chwd,
    ];

    /// The stable, lower-case word used in settings, plans and the page.
    pub fn id(self) -> &'static str {
        match self {
            Self::Pacman => "pacman",
            Self::Aur => "aur",
            Self::Flatpak => "flatpak",
            Self::Snap => "snap",
            Self::Apt => "apt",
            Self::Dnf => "dnf",
            Self::Github => "github",
            Self::Fwupd => "fwupd",
            Self::Chwd => "chwd",
        }
    }

    /// What a badge says. Neutral words; colour is never per source.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pacman => "pacman",
            Self::Aur => "AUR",
            Self::Flatpak => "Flatpak",
            Self::Snap => "Snap",
            Self::Apt => "apt",
            Self::Dnf => "dnf",
            Self::Github => "GitHub",
            Self::Fwupd => "Firmware",
            Self::Chwd => "Drivers",
        }
    }

    pub fn parse(s: &str) -> Option<SourceKind> {
        Self::ALL.into_iter().find(|k| k.id() == s)
    }
}

/// What kind of thing a package is, which decides where it is listed and how
/// it is drawn. `App` has a desktop entry or an AppStream component of type
/// `desktop-application`; `Package` is everything else from a distribution
/// repository (libraries, tools, fonts are `Font`, runtimes and SDKs are
/// `Runtime`, Flatpak extensions and Snap plugs are `Addon`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageKind {
    App,
    Package,
    Runtime,
    Font,
    Addon,
    Driver,
    Firmware,
}

/// One thing in one source. `id` is source-native and opaque to everything
/// but that source: a pacman name, an AUR name, a Flatpak ref
/// (`flathub/app/org.gimp.GIMP/x86_64/stable`), a Snap name, a GitHub
/// `owner/repo`, an fwupd device id, a chwd profile name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PackageRef {
    pub source: SourceKind,
    pub id: String,
}

/// A picture the page can show: a URL it fetches itself, or a file on this
/// machine served through the asset protocol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum Picture {
    Url(String),
    File(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screenshot {
    pub image: Picture,
    /// A smaller rendition where the source offers one; the page uses it in
    /// rails and the full image in the viewer.
    pub thumbnail: Option<Picture>,
    pub caption: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// One installable thing from one source. Sources fill what they know and
/// leave the rest `None`; the grouper and the page never invent a value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Package {
    pub source: SourceKind,
    pub id: String,
    pub name: String,
    pub kind: PackageKind,
    pub summary: Option<String>,
    /// AppStream description markup (`<p>`, `<ul>`, `<ol>`, `<li>`, `<em>`,
    /// `<code>`) or plain text. The page sanitises it to exactly those tags.
    pub description: Option<String>,
    pub version: Option<String>,
    pub installed_version: Option<String>,
    pub installed: bool,
    /// The repository, remote or channel: "extra", "multilib", "aur",
    /// "flathub", "stable", "jammy-updates".
    pub repo: Option<String>,
    pub licence: Option<String>,
    pub homepage: Option<String>,
    pub developer: Option<String>,
    /// Unix seconds of the last update the source knows about.
    pub updated: Option<i64>,
    pub download_size: Option<u64>,
    pub installed_size: Option<u64>,
    /// 0 to 1 within the source, for sorting only.
    pub popularity: Option<f64>,
    /// What that figure is, for the page: "1257 votes", "100 548 installs last month".
    pub popularity_label: Option<String>,
    pub icon: Option<Picture>,
    pub screenshots: Vec<Screenshot>,
    pub categories: Vec<String>,
    /// The AppStream component id when the source knows it. The grouper's
    /// strongest join key.
    pub appstream_id: Option<String>,
    /// AUR's flag, or a package the distribution has marked as such.
    pub out_of_date: bool,
    /// Flatpak and Snap.
    pub sandboxed: bool,
    /// Source-specific facts for the detail page's key/value list, in the
    /// order they should be drawn: ("Maintainer", "…"), ("Votes", "…").
    pub facts: Vec<(String, String)>,
}

impl Package {
    /// A package with only what every source can fill in, for the sources
    /// to build on.
    pub fn new(source: SourceKind, id: impl Into<String>, name: impl Into<String>) -> Package {
        Package {
            source,
            id: id.into(),
            name: name.into(),
            kind: PackageKind::Package,
            summary: None,
            description: None,
            version: None,
            installed_version: None,
            installed: false,
            repo: None,
            licence: None,
            homepage: None,
            developer: None,
            updated: None,
            download_size: None,
            installed_size: None,
            popularity: None,
            popularity_label: None,
            icon: None,
            screenshots: Vec::new(),
            categories: Vec::new(),
            appstream_id: None,
            out_of_date: false,
            sandboxed: false,
            facts: Vec::new(),
        }
    }

    pub fn reference(&self) -> PackageRef {
        PackageRef {
            source: self.source,
            id: self.id.clone(),
        }
    }
}

/// How an edition was joined to its group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchedBy {
    /// The same AppStream component id on both sides.
    AppStream,
    /// The same normalised name (`steam`, `steam-bin`, `com.valvesoftware.Steam`).
    Name,
    /// The only member; nothing to match.
    Alone,
}

/// One member of a group: the package and how sure the grouper is that it
/// belongs. `confidence` is 1.0 for an AppStream join and lower for a name
/// join; the page says "matched by name" beside anything under 1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edition {
    pub package: Package,
    pub matched_by: MatchedBy,
    pub confidence: f32,
}

/// The row the page shows: one application, its editions across sources.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct App {
    /// Stable within a search: the AppStream id, else `name:<normalised>`.
    pub key: String,
    pub name: String,
    pub kind: PackageKind,
    pub summary: Option<String>,
    pub icon: Option<Picture>,
    pub developer: Option<String>,
    pub categories: Vec<String>,
    /// True if any edition is installed.
    pub installed: bool,
    /// Newest `updated` across editions.
    pub updated: Option<i64>,
    /// Highest `popularity` across editions.
    pub popularity: Option<f64>,
    /// How well it matched the query, 0 to 1. The default sort.
    pub relevance: f32,
    pub editions: Vec<Edition>,
}

/// One thing that can be brought up to date.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Update {
    pub package: PackageRef,
    pub name: String,
    pub kind: PackageKind,
    pub summary: Option<String>,
    pub icon: Option<Picture>,
    pub from: Option<String>,
    pub to: String,
    pub download_size: Option<u64>,
    /// Unix seconds when the new version was published, where known.
    pub published: Option<i64>,
    /// This is Brokey itself.
    pub is_self: bool,
}

/// Whether a source can be used on this machine, and if not, why, in a
/// sentence the page shows in a tooltip.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceStatus {
    pub kind: SourceKind,
    pub available: bool,
    pub reason: Option<String>,
    /// A short line for the status bar: "paru 2.1.0", "flathub, fedora".
    pub detail: Option<String>,
    /// The source can answer a search through its public store even though
    /// `available` is false (Flathub's API without flatpak, the Snap Store
    /// without snapd). Installed and updates still need `available`.
    #[serde(default)]
    pub searchable: bool,
    /// How the store can set the source up from inside, when it can:
    /// `Op::Setup { source }` then plans it. `None` when the source is
    /// available, or when this system has no way to install the tool.
    #[serde(default)]
    pub setup: Option<SourceSetup>,
}

/// What the page says about setting a source up: the button and the
/// sentence under it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSetup {
    /// "Install Flatpak", "Install snapd", "Add Flathub".
    pub label: String,
    /// What setting it up does, one or two sentences.
    pub sentence: String,
}

/// What the user asked for. A plan is built from a list of these.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Op {
    Install {
        package: PackageRef,
    },
    Remove {
        package: PackageRef,
    },
    Update {
        package: PackageRef,
    },
    /// Everything this source can update, in one transaction.
    UpdateAll {
        source: SourceKind,
    },
    /// Refresh the source's index (pacman -Sy, apt update, flatpak appstream).
    Refresh {
        source: SourceKind,
    },
    /// Set the source up on this machine: install its tool through the
    /// distribution's source, then the source's own steps (adding Flathub,
    /// starting snapd). The planner expands it; see `Source::setup`.
    Setup {
        source: SourceKind,
    },
}

impl Op {
    /// The source that carries the operation out, or is set up by it.
    pub fn source(&self) -> SourceKind {
        match self {
            Op::Install { package } | Op::Remove { package } | Op::Update { package } => {
                package.source
            }
            Op::UpdateAll { source } | Op::Refresh { source } | Op::Setup { source } => *source,
        }
    }
}

/// A process to run. The helper validates `program` and `args` against its
/// closed list before running anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
}

/// One thing a plan does, in order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub source: SourceKind,
    /// What the activity panel says while it runs: "Installing steam".
    pub title: String,
    pub command: Command,
    pub needs_root: bool,
    /// Relative cost, for the progress fraction: a download-and-install of
    /// three packages weighs more than a database refresh.
    pub weight: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub ops: Vec<Op>,
    pub steps: Vec<Step>,
}

/// What a running plan reports. `Progress.fraction` is `None` whenever the
/// total is not known; the page draws an empty rail and the message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    PlanStarted {
        plan: String,
        steps: usize,
    },
    AuthRequired {
        plan: String,
    },
    StepStarted {
        plan: String,
        step: usize,
        title: String,
    },
    Progress {
        plan: String,
        step: usize,
        fraction: Option<f32>,
        message: Option<String>,
    },
    Log {
        plan: String,
        step: usize,
        line: String,
        stderr: bool,
    },
    StepFinished {
        plan: String,
        step: usize,
        ok: bool,
        message: Option<String>,
    },
    PlanFinished {
        plan: String,
        ok: bool,
        message: String,
    },
}

/// Which operating system this is. The page needs it because the nav and
/// the status bar differ; it must never be inferred from which sources
/// happen to be present.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Linux,
    Windows,
}

/// The machine, as far as the sources need to know it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemInfo {
    /// `ID` from os-release: "cachyos", "arch", "debian", "ubuntu", "fedora".
    pub distro_id: String,
    /// `ID_LIKE`, split: ["arch"].
    pub distro_like: Vec<String>,
    pub pretty_name: String,
    pub arch: String,
    pub desktop: Option<String>,
    pub session: Option<String>,
    pub platform: Platform,
}

impl SystemInfo {
    pub fn is_arch_like(&self) -> bool {
        self.distro_id == "arch" || self.distro_like.iter().any(|d| d == "arch")
    }
    pub fn is_debian_like(&self) -> bool {
        self.distro_id == "debian"
            || self
                .distro_like
                .iter()
                .any(|d| d == "debian" || d == "ubuntu")
    }
    pub fn is_fedora_like(&self) -> bool {
        self.distro_id == "fedora"
            || self
                .distro_like
                .iter()
                .any(|d| d == "fedora" || d == "rhel")
    }
}

/// One driver profile a manager offers for a device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverProfile {
    /// The manager's own name for it: "nvidia-open-dkms.prime".
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub installed: bool,
    pub recommended: bool,
    /// What installing it would put on the machine, where known.
    pub packages: Vec<String>,
}

/// A device the driver manager knows about, with its profiles.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverDevice {
    /// PCI address or the manager's id: "0000:01:00.0".
    pub id: String,
    pub name: String,
    pub vendor: Option<String>,
    /// "VGA compatible controller", "Network controller".
    pub class: Option<String>,
    pub profiles: Vec<DriverProfile>,
}

/// One device fwupd reports, and the update it has for it if any.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareDevice {
    pub id: String,
    pub name: String,
    pub vendor: Option<String>,
    pub version: Option<String>,
    pub update_version: Option<String>,
    pub update_summary: Option<String>,
    pub update_size: Option<u64>,
    pub needs_reboot: bool,
}

/// What the Drivers page draws.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriversReport {
    /// "chwd", or `None` when no driver manager this store knows is present.
    pub manager: Option<String>,
    /// One sentence for the page when there is no manager or it failed.
    pub manager_note: Option<String>,
    pub devices: Vec<DriverDevice>,
    pub firmware_available: bool,
    pub firmware_note: Option<String>,
    pub firmware: Vec<FirmwareDevice>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shapes `frontend/src/types.ts` mirrors, pinned here so a change
    /// on either side shows up as a failing test rather than a blank page.
    #[test]
    fn a_source_status_and_a_setup_op_serialise_as_the_page_expects() {
        let status = SourceStatus {
            kind: SourceKind::Flatpak,
            available: false,
            reason: Some("Flatpak is not installed.".to_string()),
            detail: None,
            searchable: true,
            setup: Some(SourceSetup {
                label: "Install Flatpak".to_string(),
                sentence: "Installs Flatpak and adds Flathub.".to_string(),
            }),
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "kind": "flatpak",
                "available": false,
                "reason": "Flatpak is not installed.",
                "detail": null,
                "searchable": true,
                "setup": {"label": "Install Flatpak", "sentence": "Installs Flatpak and adds Flathub."}
            })
        );
        let back: SourceStatus = serde_json::from_value(json).unwrap();
        assert_eq!(back, status);
        // A status written before the two fields existed still reads.
        let old: SourceStatus = serde_json::from_str(
            r#"{"kind":"pacman","available":true,"reason":null,"detail":"core, extra"}"#,
        )
        .unwrap();
        assert!(!old.searchable);
        assert_eq!(old.setup, None);

        let op = Op::Setup {
            source: SourceKind::Snap,
        };
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json, serde_json::json!({"op": "setup", "source": "snap"}));
        assert_eq!(serde_json::from_value::<Op>(json).unwrap(), op);
        assert_eq!(op.source(), SourceKind::Snap);
        assert_eq!(
            Op::Install {
                package: PackageRef {
                    source: SourceKind::Aur,
                    id: "snapd".to_string()
                }
            }
            .source(),
            SourceKind::Aur
        );
    }
}
