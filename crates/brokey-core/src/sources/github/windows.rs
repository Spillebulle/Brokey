//! GitHub releases on Windows: which asset a release offers this machine,
//! and the steps that install and remove it. A Windows Installer package
//! first, then a publisher's installer, then a bare program, then a zip
//! treated as a portable application.
//!
//! Everything here is a pure function of its arguments, so it is compiled
//! and tested on Linux too, the way `sources::windows::arp`'s filter is.
//!
//! **Nothing here needs the helper, and that is a decision.** Every step is
//! a session step. A per-machine MSI still asks for Administrator, but
//! Windows Installer asks for it itself, in a consent prompt that names the
//! package it is about to install; a publisher's installer that needs it
//! asks the same way, because its own manifest says so. The helper's closed
//! list refuses a program this process could put different bytes at
//! (`transaction::allow::check_step`), and a downloaded package in the
//! user's own cache is exactly such a file: an elevated `msiexec /i` on it
//! would install whatever an unprivileged process left there after the
//! checksum was taken. A prompt Windows raises about the file as it is
//! when the prompt appears grants nothing the user's own processes could
//! not already ask for.

use super::{Arch, Asset, AssetKind, Chosen, InstallRecord, err, step};
use crate::Result;
use crate::model::*;
use std::path::{Path, PathBuf};

/// A name that says the file is for another operating system.
const ELSEWHERE: [&str; 9] = [
    "linux", "macos", "mac", "osx", "darwin", "android", "ios", "freebsd", "appimage",
];
/// A name that says the file is for Windows. Only a zip has to say so; an
/// `.msi` or an `.exe` already has.
const WINDOWS: [&str; 4] = ["win", "windows", "win32", "win64"];
/// A zip that is not the application: debugging symbols and sources.
const NOT_THE_APPLICATION: [&str; 6] = ["symbols", "pdb", "debug", "dbg", "src", "source"];

/// The name cut into its words at anything that is not a letter or a
/// digit, lower-cased: `yt-dlp_win_x86.zip` is `yt dlp win x86 zip`.
fn words(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// What a Windows machine can do something with, judged from the file name.
pub fn kind_of(name: &str) -> Option<AssetKind> {
    let lower = name.to_lowercase();
    let words = words(name);
    let has = |list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    if has(&ELSEWHERE) {
        return None;
    }
    if let Some(stem) = lower.strip_suffix(".msi") {
        return (!stem.is_empty()).then_some(AssetKind::Msi);
    }
    if let Some(stem) = lower.strip_suffix(".exe") {
        if stem.is_empty() || stem.contains("uninstall") {
            return None;
        }
        return Some(if stem.contains("setup") || stem.contains("installer") {
            AssetKind::Installer
        } else {
            AssetKind::Exe
        });
    }
    if lower.ends_with(".zip") && has(&WINDOWS) && !has(&NOT_THE_APPLICATION) {
        return Some(AssetKind::Zip);
    }
    None
}

/// The architecture an asset's name says it is for; `None` when the name
/// does not say. Windows names use `x64`, `win64` and `win32` where Linux
/// names say `x86_64`, and a 32-bit build is worth knowing about here,
/// because Windows runs one on a 64-bit machine.
pub fn arch_of(name: &str) -> Option<Arch> {
    let lower = name.to_lowercase();
    let words = words(name);
    let word = |list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    if ["aarch64", "arm64"].iter().any(|s| lower.contains(s)) {
        Some(Arch::Aarch64)
    } else if ["x86_64", "x86-64", "amd64", "x64", "win64"]
        .iter()
        .any(|s| lower.contains(s))
    {
        Some(Arch::X86_64)
    } else if word(&["x86", "i386", "i686", "ia32", "win32", "32bit"]) {
        Some(Arch::X86)
    } else {
        None
    }
}

/// How well a build for `asset` runs on `machine`: 0 natively, 1 when the
/// name does not say, 2 or 3 through the emulation Windows ships (32-bit
/// x86 on x64; x64 and then x86 on ARM64), `None` not at all.
fn fit(asset: Option<Arch>, machine: Option<Arch>) -> Option<u8> {
    match (asset, machine) {
        (Some(a), Some(m)) if a == m => Some(0),
        (None, _) | (Some(_), None) => Some(1),
        (Some(Arch::X86), Some(Arch::X86_64)) => Some(2),
        (Some(Arch::X86_64), Some(Arch::Aarch64)) => Some(2),
        (Some(Arch::X86), Some(Arch::Aarch64)) => Some(3),
        _ => None,
    }
}

/// The best Windows asset for this machine. A build that runs natively, or
/// whose name does not say, before one that would run through emulation,
/// never one that cannot run; within that, an `.msi`, then an installer
/// (`-setup.exe`, `-installer.exe`), then a bare `.exe`, then a zip.
pub fn pick_asset(assets: &[Asset], machine: Option<Arch>) -> Option<Chosen> {
    let mut candidates: Vec<((bool, u8, u8), Chosen)> = assets
        .iter()
        .filter_map(|asset| {
            let kind = kind_of(&asset.name)?;
            let arch = arch_of(&asset.name);
            let fit = fit(arch, machine)?;
            let kind_rank = match kind {
                AssetKind::Msi => 0,
                AssetKind::Installer => 1,
                AssetKind::Exe => 2,
                _ => 3,
            };
            Some((
                (fit >= 2, kind_rank, fit),
                Chosen {
                    asset: asset.clone(),
                    kind,
                    arch,
                },
            ))
        })
        .collect();
    candidates.sort_by(|a, b| (a.0, &a.1.asset.name).cmp(&(b.0, &b.1.asset.name)));
    candidates.into_iter().next().map(|c| c.1)
}

/// What a plan needs to install one release on Windows, with no network
/// and no environment in it.
pub struct InstallTarget<'a> {
    /// The repository's name, which is what the steps and the files are
    /// called.
    pub name: &'a str,
    /// "owner/repo", already checked to be one.
    pub repo: &'a str,
    pub chosen: &'a Chosen,
    /// `<cache>\downloads`.
    pub downloads: &'a Path,
    /// Where portable applications go: [`programs_dir`] on a real machine.
    pub programs: &'a Path,
    /// The full path to `msiexec.exe`, from
    /// [`msiexec_program`](crate::sources::windows::arp::msiexec_program).
    pub msiexec: &'a str,
}

/// `%LOCALAPPDATA%\Programs\Brokey`: the folder Windows gives per-user
/// applications, and in it one Brokey owns, so that removing a portable
/// application can never take a folder some installer made. `None` when
/// Windows says this user has no profile.
pub fn programs_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.data_local_dir().join("Programs").join("Brokey"))
}

/// Where a release's file is downloaded: `<downloads>\<owner>\<repo>\<file>`.
/// One folder per repository, because an MSI is kept there to remove the
/// product with later, and two repositories both publishing `setup.msi`
/// must not share one. Only the last component of the asset's name is used.
pub fn download_path(downloads: &Path, repo: &str, asset: &str) -> PathBuf {
    let (owner, name) = repo.split_once('/').unwrap_or(("", repo));
    downloads
        .join(owner)
        .join(name)
        .join(super::file_name(asset))
}

/// Where a portable application is unpacked: `<programs>\<owner>\<repo>`.
/// This is the folder [`InstallRecord::path`] records.
pub fn portable_dir(programs: &Path, repo: &str) -> PathBuf {
    let (owner, name) = repo.split_once('/').unwrap_or(("", repo));
    programs.join(owner).join(name)
}

/// A PowerShell single-quoted literal takes an apostrophe as two of them
/// and treats everything else inside it literally, which is why every
/// script below quotes with apostrophes and escapes nothing else.
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// One `powershell.exe` step. `$ErrorActionPreference = 'Stop'` first, so
/// that a cmdlet that fails ends the script with a failure rather than
/// carrying on to the next line and reporting success.
fn powershell(title: String, script: String, weight: u32) -> Step {
    step(
        title,
        "powershell.exe",
        vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            format!("$ErrorActionPreference = 'Stop'; {script}"),
        ],
        false,
        weight,
    )
}

/// The sentence a check that fails writes, the same one Linux writes.
pub const MISMATCH: &str = "The download does not match the checksum GitHub published for it, so it was not installed and has been deleted. Try again; if it happens again, the file on GitHub is not the one its release describes.";

/// The check run on a download whose asset carries a digest. A file that
/// does not match is deleted, so a retry downloads it again rather than
/// finding it there. `Get-FileHash` answers in upper case and `-ne`
/// compares without case, so the digits need no folding.
pub fn check_script(path: &str, sha256: &str) -> String {
    format!(
        "$f = {}; if ((Get-FileHash -Algorithm SHA256 -LiteralPath $f).Hash -ne {}) \
         {{ Remove-Item -LiteralPath $f -Force; [Console]::Error.WriteLine({}); exit 1 }}",
        ps_quote(path),
        ps_quote(sha256),
        ps_quote(MISMATCH)
    )
}

/// `msiexec` started and waited for, with its code turned into a sentence.
/// `Start-Process` because `msiexec.exe` is a windowed program and the
/// call operator would not wait for it. The path goes to it wrapped in
/// double quotes made from `[char]34`, so that none are written into the
/// script and Windows' command-line quoting never sees one. 3010 is
/// success with a restart owed, and `ok` lists the other codes that mean
/// the work is done.
fn msiexec_script(
    msiexec: &str,
    verb: &str,
    path: &str,
    ok: &[u32],
    failed: &str,
    then: &str,
) -> String {
    let ok: Vec<String> = ok.iter().map(u32::to_string).collect();
    format!(
        "$q = [char]34; $p = Start-Process -FilePath {} -ArgumentList '{verb}', ($q + {} + $q), '/passive', '/norestart' -Wait -PassThru; \
         if ($p.ExitCode -eq 3010) {{ {then}'Done. Windows needs to restart to finish.'; exit 0 }}; \
         if ({}) {{ {then}exit 0 }}; \
         [Console]::Error.WriteLine({} + ' Windows Installer ended with code ' + $p.ExitCode + '; 1602 means it was cancelled and 1625 that permission was not given.'); exit 1",
        ps_quote(msiexec),
        ps_quote(path),
        ok.iter()
            .map(|c| format!("$p.ExitCode -eq {c}"))
            .collect::<Vec<_>>()
            .join(" -or "),
        ps_quote(failed),
    )
}

/// Download with curl, check it where GitHub published a checksum, then
/// install by kind. curl rather than the store's own client because a
/// download is a step the runner logs and can cancel, and a plan must not
/// fetch anything itself; `curl.exe` has shipped with Windows since 1803.
pub fn install_steps(target: &InstallTarget) -> Result<Vec<Step>> {
    let asset = &target.chosen.asset;
    let url = &asset.browser_download_url;
    let kind = target.chosen.kind;
    if !matches!(
        kind,
        AssetKind::Msi | AssetKind::Installer | AssetKind::Exe | AssetKind::Zip
    ) {
        return Err(err(format!(
            "{} is {}, which Windows cannot install; download it by hand from {url} if you want it.",
            asset.name,
            kind.label()
        )));
    }
    let path = download_path(target.downloads, target.repo, &asset.name);
    let path = path.to_string_lossy().into_owned();
    let mut steps = vec![step(
        format!("Downloading {}", asset.name),
        "curl.exe",
        ["-L", "-sS", "--fail", "--create-dirs", "-o", &path, url]
            .into_iter()
            .map(str::to_string)
            .collect(),
        false,
        4,
    )];
    if let Some(sha256) = asset.sha256() {
        steps.push(powershell(
            format!("Checking {}", asset.name),
            check_script(&path, &sha256),
            1,
        ));
    }
    let name = target.name;
    let dir = portable_dir(target.programs, target.repo);
    let dir = dir.to_string_lossy().into_owned();
    steps.push(match kind {
        AssetKind::Msi => powershell(
            format!("Installing {name}"),
            msiexec_script(
                target.msiexec,
                "/i",
                &path,
                &[0],
                &format!("{name} was not installed."),
                "",
            ),
            4,
        ),
        // Interactive, because the switch that would quiet it depends on
        // what built the installer (NSIS, Inno Setup, WiX, Squirrel and
        // others each spell it differently, and some have none), and the
        // name of the file does not say. A switch guessed wrong is ignored
        // at best and misread at worst. `Start-Process` rather than
        // starting it directly because it goes through the shell, which
        // honours an installer's manifest asking for Administrator; started
        // directly, such an installer fails with "the requested operation
        // requires elevation".
        AssetKind::Installer => powershell(
            format!("Running the installer for {name}; finish it in its own window"),
            format!(
                "$p = Start-Process -FilePath {} -Wait -PassThru; exit $p.ExitCode",
                ps_quote(&path)
            ),
            4,
        ),
        // A bare program is the application itself. It is copied under the
        // repository's name rather than the asset's, so that an update
        // replaces it instead of leaving the old version beside it.
        AssetKind::Exe => powershell(
            format!("Installing {name}"),
            format!(
                "New-Item -ItemType Directory -Force -Path {d} | Out-Null; \
                 Copy-Item -LiteralPath {} -Destination (Join-Path {d} {}) -Force",
                ps_quote(&path),
                ps_quote(&format!("{name}.exe")),
                d = ps_quote(&dir),
            ),
            1,
        ),
        // Unpacked over what is there, so an update keeps settings a
        // portable application writes beside itself. `tar.exe` from the
        // system directory, which Windows has shipped since 1803, because
        // it refuses a member whose path climbs out with `..` and strips a
        // drive letter, both measured on the reference machine.
        _ => powershell(
            format!("Unpacking {name}"),
            format!(
                "New-Item -ItemType Directory -Force -Path {d} | Out-Null; \
                 $ErrorActionPreference = 'Continue'; \
                 & (Join-Path $env:SystemRoot 'System32\\tar.exe') -x -f {} -C {d}; exit $LASTEXITCODE",
                ps_quote(&path),
                d = ps_quote(&dir),
            ),
            2,
        ),
    });
    Ok(steps)
}

/// Removing what a record says was installed.
///
/// An MSI is removed by handing Windows Installer the package it was
/// installed from, which is still in the download folder; a product that is
/// already gone (1605) counts as removed. A portable application is the
/// folder it was unpacked into, which Brokey owns. What a publisher's
/// installer put on the machine is not known here: its uninstaller is in
/// the uninstall keys, and the Add/Remove Programs source removes it.
///
/// `package_present` is whether the kept MSI is still on disk, asked by the
/// caller so this stays a function of its arguments.
pub fn remove_steps(
    record: &InstallRecord,
    downloads: &Path,
    programs: &Path,
    msiexec: &str,
    package_present: bool,
) -> Result<Vec<Step>> {
    let name = &record.name;
    match record.kind {
        AssetKind::Msi => {
            if !package_present {
                return Err(err(format!(
                    "The package {} that installed {name} is no longer in Brokey's cache, so Brokey cannot remove it. Remove it from its Add/Remove Programs entry on the Installed page.",
                    record.asset
                )));
            }
            let path = download_path(downloads, &record.repo, &record.asset);
            let path = path.to_string_lossy().into_owned();
            // Kept only for this; once it has worked the package goes too,
            // and a removal that failed keeps it for another try.
            let script = msiexec_script(
                msiexec,
                "/x",
                &path,
                &[0, 1605],
                &format!("{name} was not removed."),
                &format!(
                    "Remove-Item -LiteralPath {} -Force -ErrorAction SilentlyContinue; ",
                    ps_quote(&path)
                ),
            );
            Ok(vec![powershell(format!("Removing {name}"), script, 4)])
        }
        AssetKind::Exe | AssetKind::Zip => {
            let dir = portable_dir(programs, &record.repo);
            Ok(vec![powershell(
                format!("Removing {name}"),
                format!(
                    "$d = {}; if (Test-Path -LiteralPath $d) {{ Remove-Item -LiteralPath $d -Recurse -Force }}; \
                     $o = Split-Path -LiteralPath $d; \
                     if ((Test-Path -LiteralPath $o) -and -not (Get-ChildItem -LiteralPath $o -Force)) {{ Remove-Item -LiteralPath $o }}",
                    ps_quote(&dir.to_string_lossy())
                ),
                1,
            )])
        }
        AssetKind::Installer => Err(err(format!(
            "{name} was installed by its publisher's own installer, which Brokey cannot run backwards. Remove it from its Add/Remove Programs entry on the Installed page."
        ))),
        kind => Err(err(format!(
            "{name} was installed as {}; remove it with the tool that installed it.",
            kind.label()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.to_string(),
            size: 1,
            browser_download_url: format!("https://example.invalid/{name}"),
            digest: None,
        }
    }

    #[test]
    fn asset_kinds_by_name() {
        assert_eq!(kind_of("KeePassXC-2.8.0-x64.msi"), Some(AssetKind::Msi));
        assert_eq!(kind_of("nvim-win64.msi"), Some(AssetKind::Msi));
        assert_eq!(
            kind_of("Heroic-2.22.1-Setup-x64.exe"),
            Some(AssetKind::Installer)
        );
        assert_eq!(
            kind_of("PowerToysUserSetup-0.90.0-x64.exe"),
            Some(AssetKind::Installer)
        );
        assert_eq!(
            kind_of("OBS-Studio-31.0-Windows-Installer.exe"),
            Some(AssetKind::Installer)
        );
        assert_eq!(kind_of("yt-dlp.exe"), Some(AssetKind::Exe));
        assert_eq!(kind_of("yt-dlp_x86.exe"), Some(AssetKind::Exe));
        assert_eq!(kind_of("nvim-win64.zip"), Some(AssetKind::Zip));
        assert_eq!(kind_of("nvim-win-arm64.zip"), Some(AssetKind::Zip));
        assert_eq!(
            kind_of("ripgrep-14.1.1-x86_64-pc-windows-msvc.zip"),
            Some(AssetKind::Zip)
        );
        assert_eq!(kind_of("yt-dlp_win_x86.zip"), Some(AssetKind::Zip));

        // A zip has to say it is for Windows, and must not be another
        // system's: "darwin" contains "win" and is not a word of it.
        assert_eq!(kind_of("KeePassXC-2.8.0-x64.zip"), None);
        assert_eq!(kind_of("tool-darwin-arm64.zip"), None);
        assert_eq!(kind_of("Heroic-2.22.1-macOS-x64.zip"), None);
        assert_eq!(kind_of("yt-dlp_linux.zip"), None);
        assert_eq!(kind_of("tool-win64-symbols.zip"), None);
        assert_eq!(kind_of("tool-win64-pdb.zip"), None);
        // Signatures, digests and other systems' files.
        assert_eq!(kind_of("KeePassXC-2.8.0-x64.msi.DIGEST"), None);
        assert_eq!(kind_of("KeePassXC-2.8.0-x64.msi.sig"), None);
        assert_eq!(kind_of("Heroic-2.22.1-Setup-x64.exe.blockmap"), None);
        assert_eq!(kind_of("Heroic-2.22.1-linux-x86_64.AppImage"), None);
        assert_eq!(kind_of("nvim-linux-x86_64.tar.gz"), None);
        assert_eq!(kind_of("yt-dlp"), None);
        assert_eq!(kind_of("uninstall.exe"), None);
        assert_eq!(kind_of(".msi"), None);
    }

    #[test]
    fn architectures_by_name() {
        assert_eq!(arch_of("KeePassXC-2.8.0-x64.msi"), Some(Arch::X86_64));
        assert_eq!(arch_of("KeePassXC-2.8.0-ARM64.msi"), Some(Arch::Aarch64));
        assert_eq!(arch_of("nvim-win64.zip"), Some(Arch::X86_64));
        assert_eq!(arch_of("nvim-win-arm64.zip"), Some(Arch::Aarch64));
        assert_eq!(arch_of("tool-amd64.msi"), Some(Arch::X86_64));
        assert_eq!(
            arch_of("ripgrep-x86_64-pc-windows-msvc.zip"),
            Some(Arch::X86_64)
        );
        assert_eq!(arch_of("yt-dlp_x86.exe"), Some(Arch::X86));
        assert_eq!(arch_of("tool-win32.zip"), Some(Arch::X86));
        assert_eq!(arch_of("tool-i686.msi"), Some(Arch::X86));
        assert_eq!(arch_of("yt-dlp.exe"), None);
        assert_eq!(arch_of("Setup.exe"), None);
    }

    #[test]
    fn emulation_is_a_fallback_and_never_a_mismatch() {
        // x64 does not run on 32-bit, nor ARM64 on x64.
        assert!(pick_asset(&[asset("tool-arm64.msi")], Some(Arch::X86_64)).is_none());
        // 32-bit runs on x64, but loses to a native build of a lesser kind.
        let assets = [asset("tool-x86.msi"), asset("tool-win-x64.zip")];
        assert_eq!(
            pick_asset(&assets, Some(Arch::X86_64)).unwrap().asset.name,
            "tool-win-x64.zip"
        );
        assert_eq!(
            pick_asset(&assets[..1], Some(Arch::X86_64))
                .unwrap()
                .asset
                .name,
            "tool-x86.msi"
        );
        // On ARM64, x64 emulation before x86.
        let assets = [asset("tool-x86.msi"), asset("tool-x64.msi")];
        assert_eq!(
            pick_asset(&assets, Some(Arch::Aarch64)).unwrap().asset.name,
            "tool-x64.msi"
        );
    }

    #[test]
    fn the_folders_are_one_per_repository() {
        let downloads = Path::new("C:/cache/downloads");
        assert_eq!(
            download_path(downloads, "owner/repo", "../../setup.msi"),
            downloads.join("owner").join("repo").join("setup.msi")
        );
        assert_eq!(
            portable_dir(Path::new("P"), "owner/repo"),
            Path::new("P").join("owner").join("repo")
        );
    }

    #[test]
    fn quoting_doubles_an_apostrophe_and_nothing_else() {
        assert_eq!(
            ps_quote(r"C:\Users\O'Brien\a $b.msi"),
            r"'C:\Users\O''Brien\a $b.msi'"
        );
        let script = check_script(r"C:\x'y.msi", "ab");
        assert!(script.starts_with(r"$f = 'C:\x''y.msi';"), "{script}");
        assert!(
            !script.contains('"'),
            "no double quote reaches the command line"
        );
    }

    #[test]
    fn no_script_carries_a_double_quote() {
        let s = msiexec_script(
            r"C:\Windows\System32\msiexec.exe",
            "/i",
            r"C:\a b\c.msi",
            &[0],
            "x.",
            "",
        );
        assert!(!s.contains('"'), "{s}");
        assert!(s.contains("'/passive', '/norestart'"), "{s}");
    }

    /// The scripts themselves, run by the PowerShell that will run them:
    /// the check keeps a file that matches and deletes one that does not,
    /// with the sentence on stderr, and a bare program is copied under the
    /// repository's name.
    #[cfg(windows)]
    #[test]
    fn the_scripts_do_what_they_say_on_this_machine() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a file with O'Brien's spaces.msi");
        let run = |step: &Step| {
            std::process::Command::new(&step.command.program)
                .args(&step.command.args)
                .output()
                .unwrap()
        };
        let check = |sum: &str| {
            powershell(
                "Checking".to_string(),
                check_script(&file.to_string_lossy(), sum),
                1,
            )
        };
        std::fs::write(&file, b"abc").unwrap();
        let good = run(&check(
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ));
        assert!(good.status.success(), "{good:?}");
        assert!(file.exists());
        let bad = run(&check(&"0".repeat(64)));
        assert!(!bad.status.success());
        assert!(!file.exists(), "a file that does not match is not kept");
        let said = String::from_utf8_lossy(&bad.stderr);
        assert!(said.contains(MISMATCH), "{said}");

        let exe = dir
            .path()
            .join("downloads")
            .join("o")
            .join("r")
            .join("r-1.2.exe");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"MZ").unwrap();
        let mut chosen = Chosen {
            asset: asset("r-1.2.exe"),
            kind: AssetKind::Exe,
            arch: None,
        };
        let programs = dir.path().join("Programs");
        let downloads = dir.path().join("downloads");
        fn target<'a>(
            chosen: &'a Chosen,
            downloads: &'a Path,
            programs: &'a Path,
        ) -> InstallTarget<'a> {
            InstallTarget {
                name: "r",
                repo: "o/r",
                chosen,
                downloads,
                programs,
                msiexec: "msiexec.exe",
            }
        }
        // Only the install step: the download is curl's, and is not run here.
        let steps = install_steps(&target(&chosen, &downloads, &programs)).unwrap();
        let out = run(steps.last().unwrap());
        assert!(out.status.success(), "{out:?}");
        assert!(programs.join("o").join("r").join("r.exe").is_file());

        // A zip, unpacked by tar into the same folder.
        let zip_path = dir
            .path()
            .join("downloads")
            .join("o")
            .join("r")
            .join("r-win64.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            z.start_file("r/app.exe", options).unwrap();
            std::io::Write::write_all(&mut z, b"MZ").unwrap();
            z.finish().unwrap();
        }
        chosen.asset = asset("r-win64.zip");
        chosen.kind = AssetKind::Zip;
        let steps = install_steps(&target(&chosen, &downloads, &programs)).unwrap();
        let out = run(steps.last().unwrap());
        assert!(out.status.success(), "{out:?}");
        assert!(
            programs
                .join("o")
                .join("r")
                .join("r")
                .join("app.exe")
                .is_file()
        );

        // And removed, folder and the owner folder it leaves empty.
        let record = InstallRecord {
            repo: "o/r".to_string(),
            name: "r".to_string(),
            asset: "r-win64.zip".to_string(),
            version: "1".to_string(),
            kind: AssetKind::Zip,
            pkgname: None,
            path: None,
        };
        let steps = remove_steps(
            &record,
            Path::new("unused"),
            &programs,
            "msiexec.exe",
            false,
        )
        .unwrap();
        let out = run(&steps[0]);
        assert!(out.status.success(), "{out:?}");
        assert!(!programs.join("o").exists());
        assert!(programs.exists());
    }
}
