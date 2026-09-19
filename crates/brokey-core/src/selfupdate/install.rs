//! How this copy of Brokey was installed, and therefore whether it may
//! replace itself.
//!
//! Brokey ships in six shapes, and they divide into two groups that are
//! treated completely differently:
//!
//! * **The ones Brokey owns.** An AppImage, which is a single file, and a
//!   portable copy somewhere under the user's own directories. Nothing else
//!   on the system has a record of these, so replacing the file *is* the
//!   update.
//! * **The ones a package manager owns.** The AUR package, the `-bin`
//!   package, the `.deb`, the `.rpm` and the Flatpak. Every one has a
//!   database entry listing the files it installed. Writing over those files
//!   behind the manager's back is wrong three times over: it is not
//!   permitted (they live under `/usr`, owned by root), it makes the record
//!   a lie, and the next system upgrade puts the old version back, silently,
//!   months later. For these the answer is either the Updates page or a
//!   downloaded package handed to the manager through the helper.
//!
//! Everything is decided by [`detect`], a pure function of a [`Probe`]: the
//! executable's path, the environment and a "does this path exist"
//! predicate. That is what lets the Debian and Fedora answers be tested on
//! an Arch machine, which is the only way they get tested at all.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// The package names this project publishes to pacman. `brokey` builds
/// from source in the AUR; `brokey-bin` is the release asset.
pub const PACMAN_SOURCE_PACKAGE: &str = "brokey";
pub const PACMAN_BIN_PACKAGE: &str = "brokey-bin";

/// The files the family's `.deb` and `.rpm` write on install (style guide
/// §18.2). Their presence is the difference between "your package manager
/// has the new version" and "your package manager has never heard of it".
/// `packaging/check.sh` is meant to fail if the scriptlets disagree with
/// these paths.
pub const APT_ARCHIVE_SOURCE: &str = "/etc/apt/sources.list.d/spillebulle.sources";
pub const RPM_ARCHIVE_SOURCE: &str = "/etc/yum.repos.d/spillebulle.repo";

/// How this copy got onto the machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Installation {
    /// Inside a Flatpak sandbox. The bundle is published as a file, not
    /// through a remote, so `flatpak update` finds nothing.
    Flatpak,
    /// One AppImage file, named by `$APPIMAGE`. Replacing that file is the
    /// whole update.
    AppImage { path: PathBuf },
    /// pacman's package: `brokey` from the AUR, which the AUR source
    /// reports as an ordinary update, or `brokey-bin`, which is installed
    /// from the release asset with `pacman -U`.
    Pacman { package: String },
    /// dpkg's package. `archive` says whether this machine has the
    /// Spillebulle archive, in which case apt has the new version.
    Dpkg { archive: bool },
    /// rpm's package, with the same archive flag.
    Rpm { archive: bool },
    /// A copy under the user's own directories, `/opt`, `/tmp` or
    /// `/usr/local`. Brokey may replace the binary.
    Portable,
    /// A system path with no manager behind it, or a path that could not be
    /// read. Not ours to touch.
    Unknown,
}

impl Installation {
    /// The stable lower-case word, the same one serde writes as `kind`.
    pub fn id(&self) -> &'static str {
        match self {
            Self::Flatpak => "flatpak",
            Self::AppImage { .. } => "appimage",
            Self::Pacman { .. } => "pacman",
            Self::Dpkg { .. } => "dpkg",
            Self::Rpm { .. } => "rpm",
            Self::Portable => "portable",
            Self::Unknown => "unknown",
        }
    }

    /// How the Settings page names it: "This copy is <label>."
    pub fn label(&self) -> String {
        match self {
            Self::Flatpak => "a Flatpak bundle".to_string(),
            Self::AppImage { path } => format!("an AppImage at {}", path.display()),
            Self::Pacman { package } if package == PACMAN_SOURCE_PACKAGE => {
                format!("the {package} package from the AUR")
            }
            Self::Pacman { package } => format!("the {package} package"),
            Self::Dpkg { archive: true } => {
                "a Debian package with the Spillebulle archive".to_string()
            }
            Self::Dpkg { archive: false } => {
                "a Debian package without the Spillebulle archive".to_string()
            }
            Self::Rpm { archive: true } => {
                "an rpm package with the Spillebulle archive".to_string()
            }
            Self::Rpm { archive: false } => {
                "an rpm package without the Spillebulle archive".to_string()
            }
            Self::Portable => "a portable copy".to_string(),
            Self::Unknown => "an unrecognised location".to_string(),
        }
    }

    /// Whether Brokey may write the new binary itself, without a package
    /// manager in between.
    pub fn is_self_updatable(&self) -> bool {
        matches!(self, Self::AppImage { .. } | Self::Portable)
    }
}

/// Whether an environment variable key matches the requested name.
///
/// On Windows, environment variable names are case-insensitive to the
/// operating system, but `std::env::vars()` returns them as spelt by the
/// parent process. A Git Bash shell passes `PATH`, whilst a GitHub Actions
/// runner may pass `Path`. Unix treats environment variable names as
/// case-sensitive and distinct, so an exact comparison must be used there.
#[cfg(windows)]
fn env_key_matches(actual: &str, requested: &str) -> bool {
    actual.eq_ignore_ascii_case(requested)
}

#[cfg(not(windows))]
fn env_key_matches(actual: &str, requested: &str) -> bool {
    actual == requested
}

/// Everything [`detect`] is allowed to look at.
///
/// A struct of injected readings rather than calls to `std::env` and
/// `Path::exists`, so the answer for every distribution can be tested on one
/// machine. The `exists` predicate accepts a trailing `*` in the last
/// component (`/var/lib/pacman/local/brokey-bin-*`) meaning "any entry
/// with that prefix", because pacman's local database names a directory by
/// package, version and release and the version is not known here. The
/// alternative, a directory listing in the probe, would widen the door every
/// test has to fake.
pub struct Probe {
    /// Where the running executable is. Empty when the platform would not
    /// say, which detects as [`Installation::Unknown`].
    pub exe: PathBuf,
    pub env: Vec<(String, String)>,
    pub exists: Box<dyn Fn(&Path) -> bool>,
}

impl Probe {
    /// The probe for the machine this is running on.
    pub fn current() -> Probe {
        Probe {
            exe: std::env::current_exe().unwrap_or_default(),
            env: std::env::vars().collect(),
            exists: Box::new(path_exists),
        }
    }

    /// A probe over fixed readings, for tests.
    pub fn fixed(exe: &str, env: &[(&str, &str)], present: &[&str]) -> Probe {
        let present: Vec<PathBuf> = present.iter().map(PathBuf::from).collect();
        Probe {
            exe: PathBuf::from(exe),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            exists: Box::new(move |path| present.iter().any(|p| p == path)),
        }
    }

    pub fn env(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(k, _)| env_key_matches(k, key))
            .map(|(_, v)| v.as_str())
    }

    fn exists(&self, path: &str) -> bool {
        (self.exists)(Path::new(path))
    }
}

impl fmt::Debug for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Probe")
            .field("exe", &self.exe)
            .field("env", &self.env)
            .finish_non_exhaustive()
    }
}

/// `Path::exists` with the one extension [`Probe`] documents: a last
/// component ending in `*` matches any entry in the parent directory that
/// starts with the part before it.
pub fn path_exists(path: &Path) -> bool {
    let Some(prefix) = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix('*'))
    else {
        return path.exists();
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    std::fs::read_dir(parent)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(prefix))
        })
        .unwrap_or(false)
}

/// Work out how this copy was installed.
pub fn detect(probe: &Probe) -> Installation {
    // Flatpak first, and before anything that looks at paths. Inside the
    // sandbox the executable is at /app/bin/brokey, a system path like
    // any other, and the host's package database can be visible through the
    // runtime. `/.flatpak-info` is the file the runtime itself puts there.
    if probe.env("FLATPAK_ID").is_some() || probe.exists("/.flatpak-info") {
        return Installation::Flatpak;
    }

    // An AppImage tells its payload where the image file is. The process
    // runs from a mount that disappears when it exits, so without the
    // variable there is nothing to replace; with the variable naming a file
    // that has gone (moved, deleted) there is nothing either, and writing a
    // new file at that path would leave a copy the user never asked for.
    if let Some(image) = probe.env("APPIMAGE") {
        let path = PathBuf::from(image);
        return if (probe.exists)(&path) {
            Installation::AppImage { path }
        } else {
            Installation::Unknown
        };
    }

    if probe.exe.as_os_str().is_empty() {
        return Installation::Unknown;
    }

    if is_portable_prefix(&probe.exe, probe.env("HOME")) {
        return Installation::Portable;
    }

    if !is_system_prefix(&probe.exe) {
        return Installation::Unknown;
    }

    // Which manager owns a system install, told apart by which package
    // database is present. pacman only exists on Arch and an rpm database
    // only on an rpm distribution, so the inference is sound in practice,
    // and the cost of being wrong is a sentence naming the wrong manager,
    // never a file being written. pacman before dpkg before rpm: a Debian
    // machine can carry an `rpm` tool and its empty database, an Arch
    // machine never carries dpkg's status file.
    if probe.exists("/var/lib/pacman") {
        // `brokey-*` also matches `brokey-bin-*`, so the bin package
        // is tested first.
        let bin = format!("/var/lib/pacman/local/{PACMAN_BIN_PACKAGE}-*");
        let source = format!("/var/lib/pacman/local/{PACMAN_SOURCE_PACKAGE}-*");
        return if probe.exists(&bin) {
            Installation::Pacman {
                package: PACMAN_BIN_PACKAGE.to_string(),
            }
        } else if probe.exists(&source) {
            Installation::Pacman {
                package: PACMAN_SOURCE_PACKAGE.to_string(),
            }
        } else {
            // pacman is here but has no record of us: a tarball unpacked
            // into /usr by hand. Not ours to overwrite, and not the AUR's.
            Installation::Unknown
        };
    }
    if probe.exists("/var/lib/dpkg/status") {
        return Installation::Dpkg {
            archive: probe.exists(APT_ARCHIVE_SOURCE),
        };
    }
    if probe.exists("/var/lib/rpm") || probe.exists("/usr/lib/sysimage/rpm") {
        return Installation::Rpm {
            archive: probe.exists(RPM_ARCHIVE_SOURCE),
        };
    }
    Installation::Unknown
}

/// Where a copy is the user's own. `/usr/local` is here rather than in the
/// system set because it exists precisely so that locally installed software
/// has somewhere no package manager touches; `/opt` is where a tarball
/// unpacked by hand goes. When one of those is root's the replace step fails
/// with a permission error the activity panel shows, rather than being
/// refused up front, because a user who unpacked a copy there owns it more
/// often than not.
fn is_portable_prefix(exe: &Path, home: Option<&str>) -> bool {
    if let Some(home) = home.filter(|h| !h.is_empty())
        && exe.starts_with(home)
    {
        return true;
    }
    ["/opt", "/tmp", "/var/tmp", "/usr/local"]
        .into_iter()
        .any(|root| exe.starts_with(root))
}

/// Where a package manager would have put it.
fn is_system_prefix(exe: &Path) -> bool {
    ["/usr", "/bin", "/sbin", "/app", "/snap", "/nix/store"]
        .into_iter()
        .any(|root| exe.starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wildcard_matches_a_prefix_in_a_real_directory() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::create_dir(dir.path().join("brokey-bin-0.1.0-1")).expect("mkdir");
        std::fs::create_dir(dir.path().join("brokey-helper-0.1.0-1")).expect("mkdir");
        assert!(path_exists(&dir.path().join("brokey-bin-*")));
        assert!(path_exists(&dir.path().join("brokey-*")));
        assert!(!path_exists(&dir.path().join("brokey-git-*")));
        assert!(!path_exists(&dir.path().join("nothing-*")));
        // Without the star it is a plain existence test.
        assert!(path_exists(&dir.path().join("brokey-helper-0.1.0-1")));
        assert!(!path_exists(&dir.path().join("brokey-helper")));
        assert!(!path_exists(Path::new("/definitely/not/here-*")));
    }

    #[test]
    fn the_current_probe_reads_this_process() {
        let probe = Probe::current();
        assert!(!probe.exe.as_os_str().is_empty());
        assert!(probe.env("PATH").is_some());
        assert!((probe.exists)(Path::new("/")));
        assert!(format!("{probe:?}").starts_with("Probe"));
    }

    #[test]
    #[cfg(windows)]
    fn env_key_lookup_is_case_insensitive_on_windows() {
        // On Windows, the CI runner's shell passes `Path` whilst a Git Bash
        // shell passes `PATH`. Both names should find the same variable,
        // because Windows treats them as the same to the operating system.
        let probe = Probe::fixed(
            "C:\\Program Files\\brokey.exe",
            &[("Path", "C:\\Windows")],
            &[],
        );
        assert_eq!(probe.env("PATH"), Some("C:\\Windows"));
        assert_eq!(probe.env("Path"), Some("C:\\Windows"));
        assert_eq!(probe.env("path"), Some("C:\\Windows"));
    }

    #[test]
    #[cfg(not(windows))]
    fn env_key_lookup_is_case_sensitive_on_unix() {
        // On Unix, `PATH` and `Path` are distinct variables. A lookup for
        // one must not find the other.
        let probe = Probe::fixed(
            "/usr/bin/brokey",
            &[("PATH", "/usr/bin"), ("Path", "/home/user")],
            &[],
        );
        assert_eq!(probe.env("PATH"), Some("/usr/bin"));
        assert_eq!(probe.env("Path"), Some("/home/user"));
        assert_eq!(probe.env("path"), None);
    }

    #[test]
    fn the_id_is_what_serde_writes() {
        for installation in [
            Installation::Flatpak,
            Installation::AppImage {
                path: PathBuf::from("/home/a/Brokey.AppImage"),
            },
            Installation::Pacman {
                package: "brokey".into(),
            },
            Installation::Dpkg { archive: true },
            Installation::Rpm { archive: false },
            Installation::Portable,
            Installation::Unknown,
        ] {
            let json = serde_json::to_value(&installation).expect("serialises");
            assert_eq!(json["kind"], installation.id(), "{installation:?}");
            let back: Installation = serde_json::from_value(json).expect("deserialises");
            assert_eq!(back, installation);
            assert!(!installation.label().is_empty());
        }
    }
}
