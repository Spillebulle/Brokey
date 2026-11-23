//! How this copy was installed, whether a newer release exists, and the one
//! true thing to say about getting it. Muster's `update::install` shape,
//! transcribed: [`install::detect`] is a pure function of a [`Probe`],
//! [`release::latest`] asks GitHub through a six-hour cache (or past it, for
//! a check the user asked for), and [`remedy::remedy`] chooses from the two
//! what to say and what to run.
//!
//! [`check`] does the three in order and returns a [`SelfUpdate`] the page
//! draws as it is; [`plan`] turns an actionable [`Remedy`] into the steps
//! the transaction runner executes, so a self-update is logged, batched and
//! cancellable like every other install.
//!
//! # Release asset names
//!
//! The release workflow (`.github/workflows/release.yml`) must publish
//! these names, spelt exactly so, because this module names them to users
//! and `tests/selfupdate.rs` pins them:
//!
//! | Format | x86-64 | ARM64 |
//! |---|---|---|
//! | Debian package | `brokey_<v>_amd64.deb` | `brokey_<v>_arm64.deb` |
//! | rpm package | `brokey-<v>-1.x86_64.rpm` | `brokey-<v>-1.aarch64.rpm` |
//! | pacman package | `brokey-bin-<v>-1-x86_64.pkg.tar.zst` | not built |
//! | AppImage | `Brokey-<v>-x86_64.AppImage` | `Brokey-<v>-aarch64.AppImage` |
//! | Flatpak bundle | `brokey-<v>-x86_64.flatpak` (not built yet; the name is reserved so a bundle, when it ships, is found without a code change) | not built |
//! | Windows setup program | `brokey-setup-<v>-x64.exe` | `brokey-setup-<v>-arm64.exe` |
//!
//! A copy the Windows installer put in Program Files is told which setup
//! program to run rather than given a plan: Windows Installer owns its
//! files, and the helper's closed list has no entry that runs a downloaded
//! installer.
//!
//! `<v>` is the workspace version with no `v`; the tag is `v<v>`. No
//! tarball is published: a portable copy is told about the AppImage. A
//! format missing for an architecture yields a sentence with no file name.

pub mod install;
pub mod release;
pub mod remedy;
pub mod version;

pub use install::{Installation, Probe, detect};
pub use release::{Asset, RELEASES_PAGE, REPOSITORY, Release};
pub use remedy::{Arch, Format, Installer, Remedy, find, remedy};
pub use version::Version;

use crate::http::Client;
use crate::model::{Command, SourceKind, Step};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The newest release, as the page shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatestRelease {
    pub version: String,
    /// Markdown, from the release body.
    pub notes: String,
    /// Unix seconds.
    pub published: Option<i64>,
    /// The release's page on GitHub.
    pub url: String,
    /// Whether it is newer than this copy. `remedy` is `Some` exactly when
    /// this is true.
    pub newer: bool,
}

/// What the self-update check found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfUpdate {
    /// This copy's version.
    pub current: String,
    /// `None` when GitHub has no release yet, or could not be asked (then
    /// `error` says why).
    pub latest: Option<LatestRelease>,
    pub installation: Installation,
    /// "a Flatpak bundle", "the brokey package from the AUR", for the
    /// Settings page.
    pub installation_label: String,
    /// What to do about a newer release. `None` when there is none.
    pub remedy: Option<Remedy>,
    /// Why GitHub could not be asked, in a sentence, when it could not.
    pub error: Option<String>,
}

/// Detect the installation, ask GitHub, and choose the remedy. `fresh`
/// goes past the six-hour disk cache: a check the user asked for by name
/// must reach GitHub, or say why it could not.
pub fn check(client: &Client, probe: &Probe, fresh: bool) -> SelfUpdate {
    let installation = install::detect(probe);
    let current = Version::current();
    match release::latest(client, fresh) {
        Ok(latest) => assemble(
            &current,
            latest.as_ref(),
            installation,
            std::env::consts::ARCH,
            None,
        ),
        Err(e) => assemble(
            &current,
            None,
            installation,
            std::env::consts::ARCH,
            Some(e.message),
        ),
    }
}

/// The pure half of [`check`], so every combination can be tested without a
/// network.
pub fn assemble(
    current: &Version,
    latest: Option<&Release>,
    installation: Installation,
    arch: &str,
    error: Option<String>,
) -> SelfUpdate {
    let newer = latest.is_some_and(|r| r.version > *current);
    let remedy = latest
        .filter(|_| newer)
        .map(|r| remedy::remedy(&installation, &r.version, arch, &r.assets));
    SelfUpdate {
        current: current.to_string(),
        latest: latest.map(|r| LatestRelease {
            version: r.version.to_string(),
            notes: r.notes.clone(),
            published: r.published,
            url: r.url.clone(),
            newer,
        }),
        installation_label: installation.label(),
        installation,
        remedy,
        error,
    }
}

/// Where [`plan`] puts a downloaded asset: the shared HTTP client's
/// `downloads` directory, the one `Client::download` writes to, and one of
/// the places the helper accepts a package file from.
pub fn download_dir() -> PathBuf {
    Client::shared().download_dir()
}

/// The steps that carry out an actionable remedy: a download into
/// `download_dir` (the HTTP client's `downloads` directory), then the
/// install. A remedy that is a row on the Updates page or a sentence has no
/// steps, and asking for them is an error carrying that sentence, so a
/// button wired to the wrong remedy says something true.
pub fn plan(remedy: &Remedy, download_dir: &Path) -> Result<Vec<Step>> {
    match remedy {
        Remedy::InstallAsset {
            asset,
            url,
            installer,
            ..
        } => {
            let file = download_dir.join(asset);
            Ok(vec![
                download(asset, url, &file),
                install(*installer, asset, &file),
            ])
        }
        Remedy::ReplaceFile {
            path, asset, url, ..
        } => {
            let file = download_dir.join(asset);
            Ok(vec![
                download(asset, url, &file),
                Step {
                    source: SourceKind::Github,
                    title: format!("Putting {asset} in place of {}", path.display()),
                    // `install` rather than `cp`: one call sets the mode and
                    // replaces the file atomically enough for a binary that
                    // is currently running from it. Never root: the helper's
                    // closed list has no entry that writes an arbitrary path
                    // as root, and adding one would be a second file writer
                    // with root for the sake of a copy in /opt.
                    command: Command {
                        program: "install".to_string(),
                        args: vec![
                            "-m755".to_string(),
                            file.display().to_string(),
                            path.display().to_string(),
                        ],
                        env: Vec::new(),
                        cwd: None,
                    },
                    needs_root: false,
                    weight: 1,
                },
            ])
        }
        Remedy::UpdatesPage { sentence, .. } | Remedy::Sentence { sentence } => {
            Err(Error::new(sentence.clone()))
        }
    }
}

/// The download step, the same shape the GitHub source uses. `--fail` so a
/// 404 page is never saved under a package's name and handed to pacman.
fn download(asset: &str, url: &str, file: &Path) -> Step {
    Step {
        source: SourceKind::Github,
        title: format!("Downloading {asset}"),
        command: Command {
            program: "curl".to_string(),
            args: vec![
                "-L".to_string(),
                "--fail".to_string(),
                "--create-dirs".to_string(),
                "-o".to_string(),
                file.display().to_string(),
                url.to_string(),
            ],
            env: Vec::new(),
            cwd: None,
        },
        needs_root: false,
        weight: 3,
    }
}

/// The install step. Non-interactive flags throughout, because the helper
/// runs with no terminal and a prompt would hang the plan.
fn install(installer: Installer, asset: &str, file: &Path) -> Step {
    let file = file.display().to_string();
    let (source, program, args, env, needs_root, tool) = match installer {
        Installer::PacmanU => (
            SourceKind::Pacman,
            "pacman",
            vec!["-U".to_string(), "--noconfirm".to_string(), file],
            Vec::new(),
            true,
            "pacman",
        ),
        Installer::DpkgI => (
            SourceKind::Apt,
            "apt-get",
            vec!["install".to_string(), "-y".to_string(), file],
            vec![("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())],
            true,
            "apt",
        ),
        Installer::RpmU => (
            SourceKind::Dnf,
            "dnf",
            vec!["install".to_string(), "-y".to_string(), file],
            Vec::new(),
            true,
            "dnf",
        ),
        Installer::FlatpakBundle => (
            SourceKind::Flatpak,
            "flatpak",
            vec![
                "install".to_string(),
                "--user".to_string(),
                "-y".to_string(),
                file,
            ],
            Vec::new(),
            false,
            "flatpak",
        ),
    };
    Step {
        source,
        title: format!("Installing {asset} with {tool}"),
        command: Command {
            program: program.to_string(),
            args,
            env,
            cwd: None,
        },
        needs_root,
        weight: 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_download_directory_is_the_clients() {
        // `Client::download` writes to `<cache>/downloads` under the
        // directory `Client::shared` is built with; this must say the same.
        let dir = download_dir();
        assert!(dir.ends_with("http/downloads"), "{}", dir.display());
        assert!(dir.is_absolute(), "{}", dir.display());
    }
}
