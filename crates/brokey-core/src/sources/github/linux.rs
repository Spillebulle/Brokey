//! GitHub releases on Linux: which asset a release offers this machine, and
//! the steps that install and remove it. A package for this distribution,
//! an AppImage, a Flatpak bundle, or a Linux archive as a last resort.
//!
//! Everything here is a pure function of its arguments, so it is compiled
//! and tested on Windows too. Only what reads pacman's own formats
//! (`alpmdb`) is gated, because that module lives under `sources::linux`.

use super::{Arch, Asset, AssetKind, Chosen, InstallRecord, err, step};
use crate::model::*;
use crate::{Result, SystemInfo};
#[cfg(unix)]
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// What a Linux machine can do something with, judged from the file name.
pub fn kind_of(name: &str) -> Option<AssetKind> {
    let lower = name.to_lowercase();
    let ends = |s: &str| lower.ends_with(s);
    if ends(".appimage") {
        Some(AssetKind::AppImage)
    } else if ends(".deb") {
        Some(AssetKind::Deb)
    } else if ends(".rpm") {
        Some(AssetKind::Rpm)
    } else if ends(".pkg.tar.zst") || ends(".pacman") {
        Some(AssetKind::Pacman)
    } else if ends(".flatpak") {
        Some(AssetKind::Flatpak)
    } else if (ends(".tar.gz") || ends(".tgz") || ends(".tar.xz") || ends(".zip"))
        && lower.contains("linux")
    {
        Some(AssetKind::Archive)
    } else {
        None
    }
}

/// The package kind this distribution installs natively, if any.
pub fn native(system: &SystemInfo) -> Option<AssetKind> {
    if system.is_arch_like() {
        Some(AssetKind::Pacman)
    } else if system.is_debian_like() {
        Some(AssetKind::Deb)
    } else if system.is_fedora_like() {
        Some(AssetKind::Rpm)
    } else {
        None
    }
}

/// The architecture an asset's name says it is for; `None` when the name
/// does not say.
pub fn arch_of(name: &str) -> Option<Arch> {
    let lower = name.to_lowercase();
    if ["aarch64", "arm64"].iter().any(|s| lower.contains(s)) {
        Some(Arch::Aarch64)
    } else if ["x86_64", "x86-64", "amd64", "x64"]
        .iter()
        .any(|s| lower.contains(s))
    {
        Some(Arch::X86_64)
    } else {
        None
    }
}

/// The best Linux asset for this machine: one for its architecture before
/// one that does not say, never one for the other; the distribution's own
/// package kind first, then an AppImage, then a Flatpak, then a package for
/// another distribution, then an archive.
pub fn pick_asset(
    assets: &[Asset],
    machine: Option<Arch>,
    native: Option<AssetKind>,
) -> Option<Chosen> {
    let mut candidates: Vec<(u8, u8, Chosen)> = assets
        .iter()
        .filter_map(|asset| {
            let kind = kind_of(&asset.name)?;
            let arch = arch_of(&asset.name);
            let arch_rank = match (arch, machine) {
                (Some(a), Some(m)) if a == m => 0,
                (None, _) | (Some(_), None) => 1,
                (Some(_), Some(_)) => return None,
            };
            let kind_rank = match kind {
                k if Some(k) == native => 0,
                AssetKind::AppImage => 1,
                AssetKind::Flatpak => 2,
                AssetKind::Archive => 4,
                _ => 3,
            };
            Some((
                arch_rank,
                kind_rank,
                Chosen {
                    asset: asset.clone(),
                    kind,
                    arch,
                },
            ))
        })
        .collect();
    candidates.sort_by(|a, b| (a.0, a.1, &a.2.asset.name).cmp(&(b.0, b.1, &b.2.asset.name)));
    candidates.into_iter().next().map(|c| c.2)
}

/// The package name in an Arch package's file name, which `makepkg` writes
/// as `<name>-<pkgver>-<pkgrel>-<arch>.pkg.tar<.ext>`. `None` for any other
/// shape, `.pacman` among them: electron-builder names that file after the
/// release, not after the package inside it.
#[cfg(unix)]
pub fn pkgname_from_file_name(file: &str) -> Option<String> {
    let stem = [".pkg.tar.zst", ".pkg.tar.xz", ".pkg.tar.gz", ".pkg.tar"]
        .iter()
        .find_map(|ext| file.strip_suffix(ext))?;
    let mut parts = stem.rsplitn(4, '-');
    let (_arch, _pkgrel, _pkgver, name) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    crate::sources::linux::alpmdb::is_package_name(name).then(|| name.to_string())
}

/// The pacman names of every Arch package these records say GitHub
/// installed: the `.PKGINFO` name where it was read, else the one in the
/// file name. pacman sees such a package as foreign, the same as one built
/// from the AUR, so the AUR source takes these out of its installed list
/// and never offers to rebuild them from a different upstream. A `.pacman`
/// file recorded before names were read has no name to give and is missing
/// here.
#[cfg(unix)]
pub fn pacman_names(records: &[InstallRecord]) -> HashSet<String> {
    records
        .iter()
        .filter(|r| r.kind == AssetKind::Pacman)
        .filter_map(|r| {
            r.pkgname
                .clone()
                .or_else(|| pkgname_from_file_name(&r.asset))
        })
        .collect()
}

/// [`pacman_names`] for the records under `data`, the store's data
/// directory (`Dirs::new().data` on a real machine).
#[cfg(unix)]
pub fn pacman_names_installed_by_github(data: &Path) -> HashSet<String> {
    pacman_names(&super::read_records(&super::records_path(data)))
}

/// What a plan needs to install one release, with no network in it.
pub struct InstallTarget<'a> {
    pub name: &'a str,
    pub chosen: &'a Chosen,
    pub downloads: &'a Path,
    pub home: &'a Path,
    pub native: Option<AssetKind>,
    pub has_curl: bool,
    /// Whether `libfuse.so.2` is on the machine ([`has_libfuse2`]), which a
    /// type 2 AppImage needs to mount itself and start.
    pub has_libfuse2: bool,
}

/// Where `libfuse.so.2` is on Arch (and Fedora's 64-bit lib directory, and
/// Debian's multiarch one). Arch's `fuse3` does not provide it; `fuse2`
/// does, and is not installed by default.
pub const LIBFUSE2: [&str; 3] = [
    "/usr/lib/libfuse.so.2",
    "/usr/lib64/libfuse.so.2",
    "/usr/lib/x86_64-linux-gnu/libfuse.so.2",
];

/// Whether any of [`LIBFUSE2`] is present.
pub fn has_libfuse2() -> bool {
    LIBFUSE2.iter().any(|p| Path::new(p).exists())
}

/// The check run on a download whose asset carries a digest, as `sh -c`
/// with the expected digits as `$1` and the file as `$2`. Through a shell
/// because the comparison needs a pipe or a check file and a step is one
/// argv with no stdin; the digits and the path are passed as arguments,
/// never written into the script, so neither is ever parsed as shell. The
/// file is read on stdin so its name needs no escaping for `sha256sum`'s
/// check-file format, and a file that does not match is deleted, so a
/// retry downloads it again rather than finding it there.
pub const CHECK_SCRIPT: &str = r#"sum=$(sha256sum < "$2") || exit 1; [ "${sum%% *}" = "$1" ] && exit 0; rm -f -- "$2"; echo "The download does not match the checksum GitHub published for it, so it was not installed and has been deleted. Try again; if it happens again, the file on GitHub is not the one its release describes." >&2; exit 1"#;

/// The file the asset is downloaded to. The name comes from GitHub, so
/// only its last path component is used.
pub fn download_path(downloads: &Path, asset: &str) -> PathBuf {
    downloads.join(super::file_name(asset))
}

/// Download with curl, then install by kind. curl rather than the store's
/// own client because a download is a step the runner logs and can cancel,
/// and a plan must not fetch anything itself.
pub fn install_steps(target: &InstallTarget) -> Result<Vec<Step>> {
    let asset = &target.chosen.asset;
    let url = &asset.browser_download_url;
    let kind = target.chosen.kind;
    if kind == AssetKind::Archive {
        return Err(err(format!(
            "This release is an archive, not a package; download it from {url} and unpack it by hand."
        )));
    }
    if !matches!(
        kind,
        AssetKind::AppImage
            | AssetKind::Flatpak
            | AssetKind::Deb
            | AssetKind::Rpm
            | AssetKind::Pacman
    ) || (matches!(kind, AssetKind::Deb | AssetKind::Rpm | AssetKind::Pacman)
        && Some(kind) != target.native)
    {
        return Err(err(format!(
            "{} is {}, which this machine cannot install; download it by hand from {url} if you want it.",
            asset.name,
            kind.label()
        )));
    }
    if !target.has_curl {
        return Err(err(
            "curl is needed to download GitHub releases; install it and try again.",
        ));
    }
    let path = download_path(target.downloads, &asset.name);
    let path_str = path.to_string_lossy().into_owned();
    let mut steps = Vec::new();
    if kind == AssetKind::AppImage
        && !target.has_libfuse2
        && target.native == Some(AssetKind::Pacman)
    {
        // A type 2 AppImage mounts itself through libfuse 2 and without it
        // fails to start with a line about FUSE that names no package. The
        // install is the pacman source's own shape, a whole-system
        // upgrade with the name, because a bare -S is the partial upgrade
        // Arch does not support. Other distributions package it under
        // names that vary by release (libfuse2, libfuse2t64, fuse-libs),
        // so nothing is added there.
        let mut fuse = step(
            "Installing fuse2, which AppImages need to start, and updating the system".to_string(),
            "pacman",
            ["-Syu", "--noconfirm", "--needed", "fuse2"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            true,
            3,
        );
        fuse.command
            .env
            .push(("LC_ALL".to_string(), "C.UTF-8".to_string()));
        steps.push(fuse);
    }
    steps.push(step(
        format!("Downloading {}", asset.name),
        "curl",
        ["-L", "-sS", "--fail", "--create-dirs", "-o", &path_str, url]
            .into_iter()
            .map(str::to_string)
            .collect(),
        false,
        4,
    ));
    if let Some(sha256) = asset.sha256() {
        steps.push(step(
            format!("Checking {}", asset.name),
            "sh",
            vec![
                "-c".to_string(),
                CHECK_SCRIPT.to_string(),
                "sh".to_string(),
                sha256,
                path_str.clone(),
            ],
            false,
            1,
        ));
    }
    let title = format!("Installing {}", target.name);
    let s = |args: &[&str], needs_root: bool, weight: u32, program: &str| {
        step(
            title.clone(),
            program,
            args.iter().map(|a| a.to_string()).collect(),
            needs_root,
            weight,
        )
    };
    let install = match kind {
        AssetKind::Pacman => s(&["-U", "--noconfirm", &path_str], true, 4, "pacman"),
        AssetKind::Deb => {
            let mut st = s(&["install", "-y", &path_str], true, 4, "apt-get");
            st.command
                .env
                .push(("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string()));
            st
        }
        AssetKind::Rpm => s(&["install", "-y", &path_str], true, 4, "dnf"),
        // `--reinstall` because an update is the same command on a newer
        // bundle, and without it flatpak refuses an application that is
        // already installed. A session step, so not the helper's list.
        AssetKind::Flatpak => s(
            &[
                "install",
                "-y",
                "--noninteractive",
                "--user",
                "--reinstall",
                &path_str,
            ],
            false,
            4,
            "flatpak",
        ),
        AssetKind::AppImage => {
            let dest = appimage_path(target.home, target.name);
            s(
                &["-Dm755", &path_str, &dest.to_string_lossy()],
                false,
                1,
                "install",
            )
        }
        _ => unreachable!("every other kind was refused above"),
    };
    steps.push(install);
    Ok(steps)
}

pub fn appimage_path(home: &Path, name: &str) -> PathBuf {
    home.join(".local/bin").join(name)
}

/// Removing what a record says was installed. Only an AppImage is a file
/// this source owns; a package went into the package manager's database
/// and is removed there, and a Flatpak bundle's application id is not in
/// the record.
pub fn remove_steps(record: &InstallRecord, home: &Path) -> Result<Vec<Step>> {
    match record.kind {
        AssetKind::AppImage => {
            let path = appimage_path(home, &record.name);
            Ok(vec![step(
                format!("Removing {}", record.name),
                "rm",
                vec!["-f".to_string(), path.to_string_lossy().into_owned()],
                false,
                1,
            )])
        }
        kind => Err(err(format!(
            "{} was installed as {}; remove it with the tool that installed it.",
            record.name,
            kind.label()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_kinds_by_name() {
        assert_eq!(
            kind_of("Heroic-2.22.1-linux-x86_64.AppImage"),
            Some(AssetKind::AppImage)
        );
        assert_eq!(
            kind_of("nvim-linux-x86_64.appimage"),
            Some(AssetKind::AppImage)
        );
        assert_eq!(
            kind_of("Heroic-2.22.1-linux-amd64.deb"),
            Some(AssetKind::Deb)
        );
        assert_eq!(
            kind_of("Heroic-2.22.1-linux-x86_64.rpm"),
            Some(AssetKind::Rpm)
        );
        assert_eq!(
            kind_of("Heroic-2.22.1-linux-x64.pacman"),
            Some(AssetKind::Pacman)
        );
        assert_eq!(
            kind_of("brokey-0.1.0-1-x86_64.pkg.tar.zst"),
            Some(AssetKind::Pacman)
        );
        assert_eq!(kind_of("app.flatpak"), Some(AssetKind::Flatpak));
        assert_eq!(
            kind_of("Heroic-2.22.1-linux-x64.tar.xz"),
            Some(AssetKind::Archive)
        );
        assert_eq!(
            kind_of("nvim-linux-x86_64.tar.gz"),
            Some(AssetKind::Archive)
        );
        assert_eq!(kind_of("nvim-macos-x86_64.tar.gz"), None);
        assert_eq!(kind_of("Heroic-2.22.1-Setup-x64.exe"), None);
        assert_eq!(kind_of("Heroic-2.22.1-macOS-x64.zip"), None);
        assert_eq!(kind_of("nvim-win64.msi"), None);
        assert_eq!(kind_of("latest-linux.yml"), None);
    }

    #[test]
    fn architectures_by_name() {
        assert_eq!(arch_of("Heroic-linux-amd64.deb"), Some(Arch::X86_64));
        assert_eq!(arch_of("Heroic-linux-x64.pacman"), Some(Arch::X86_64));
        assert_eq!(arch_of("nvim-linux-x86_64.appimage"), Some(Arch::X86_64));
        assert_eq!(arch_of("nvim-linux-arm64.appimage"), Some(Arch::Aarch64));
        assert_eq!(arch_of("thing-aarch64.rpm"), Some(Arch::Aarch64));
        assert_eq!(arch_of("thing.AppImage"), None);
        // A 32-bit name says nothing a 64-bit Linux machine acts on, as
        // before Windows taught the shared enum about it.
        assert_eq!(arch_of("thing-i686.AppImage"), None);
    }

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.to_string(),
            size: 1,
            browser_download_url: format!("https://example.invalid/{name}"),
            digest: None,
        }
    }

    #[test]
    fn an_asset_for_the_other_architecture_is_never_picked() {
        let assets = [asset("tool-linux-arm64.AppImage")];
        assert!(pick_asset(&assets, Some(Arch::X86_64), None).is_none());
        assert!(pick_asset(&assets, Some(Arch::Aarch64), None).is_some());
        // A name that does not say is taken on any machine.
        let assets = [asset("tool.AppImage")];
        assert!(pick_asset(&assets, Some(Arch::Aarch64), None).is_some());
    }

    #[test]
    fn a_windows_kind_is_never_installed_here() {
        let chosen = Chosen {
            asset: asset("tool.msi"),
            kind: AssetKind::Msi,
            arch: None,
        };
        let e = install_steps(&InstallTarget {
            name: "tool",
            chosen: &chosen,
            downloads: Path::new("downloads"),
            home: Path::new("home"),
            native: Some(AssetKind::Pacman),
            has_curl: true,
            has_libfuse2: true,
        })
        .unwrap_err();
        assert!(
            e.message.contains("a Windows Installer package"),
            "{}",
            e.message
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_check_accepts_the_right_file_and_deletes_the_wrong_one() {
        if crate::system::which("sha256sum").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a file with spaces.AppImage");
        let check = |sum: &str| {
            std::process::Command::new("sh")
                .args(["-c", CHECK_SCRIPT, "sh", sum, file.to_str().unwrap()])
                .output()
                .unwrap()
        };
        std::fs::write(&file, b"abc").unwrap();
        let good = check("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(good.status.success());
        assert!(file.exists());
        let bad = check(&"0".repeat(64));
        assert!(!bad.status.success());
        assert!(!file.exists(), "a file that does not match is not kept");
        let said = String::from_utf8_lossy(&bad.stderr);
        assert!(
            said.starts_with("The download does not match the checksum GitHub published"),
            "{said}"
        );
        assert!(!said.contains('\u{2014}'));
        assert!(
            !check(&"0".repeat(64)).status.success(),
            "a missing file fails"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pacman_names_come_from_the_record_or_the_file_name() {
        assert_eq!(
            pkgname_from_file_name("brokey-0.1.5-1-x86_64.pkg.tar.zst").as_deref(),
            Some("brokey")
        );
        assert_eq!(
            pkgname_from_file_name("lib32-gcc-libs-14.2.1+r730-1-x86_64.pkg.tar.xz").as_deref(),
            Some("lib32-gcc-libs")
        );
        assert_eq!(
            pkgname_from_file_name("Heroic-2.22.1-linux-x64.pacman"),
            None
        );
        assert_eq!(pkgname_from_file_name("x86_64.pkg.tar.zst"), None);
        assert_eq!(pkgname_from_file_name("Tool-1-1-any.pkg.tar.zst"), None);

        let record = |asset: &str, kind: AssetKind, pkgname: Option<&str>| InstallRecord {
            repo: format!("someone/{asset}"),
            name: asset.to_string(),
            asset: asset.to_string(),
            version: "1".to_string(),
            kind,
            pkgname: pkgname.map(str::to_string),
            path: None,
        };
        let names = pacman_names(&[
            record(
                "Heroic-2.22.1-linux-x64.pacman",
                AssetKind::Pacman,
                Some("heroic"),
            ),
            record("brokey-0.1.5-1-x86_64.pkg.tar.zst", AssetKind::Pacman, None),
            // Recorded before names were read: nothing to go on.
            record("Old-1.0-linux-x64.pacman", AssetKind::Pacman, None),
            record("tool-1.0-1-x86_64.pkg.tar.zst", AssetKind::AppImage, None),
        ]);
        assert_eq!(
            names,
            HashSet::from(["heroic".to_string(), "brokey".to_string()])
        );
    }

    #[test]
    fn download_names_cannot_escape_the_directory() {
        let p = download_path(Path::new("/cache/downloads"), "../../evil.deb");
        assert_eq!(p, Path::new("/cache/downloads").join("evil.deb"));
    }
}
