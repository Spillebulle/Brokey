//! Opening what was installed, and knowing when the desktop cannot see it yet.
//!
//! Opening an application is not a transaction: nothing on the machine
//! changes, nothing runs as root, and there is nothing to batch or cancel. So
//! it does not go through a Plan. A source answers [`crate::Source::launcher`]
//! with where the application's desktop entry is (found in the package's own
//! file list, never guessed from its name), or with a command when a format
//! has its own way to run what it installed (`flatpak run`, `snap run`, an
//! AppImage). The application hands a desktop entry to `gio launch`, which
//! reads `Exec`, `Terminal` and the field codes exactly as the desktop does;
//! parsing `Exec` here was rejected because every launcher that does it gets
//! quoting and `%U` subtly wrong.
//!
//! The second half is the reason this module exists. A desktop session reads
//! desktop entries from `$XDG_DATA_DIRS/applications`, and that variable is
//! fixed when the session starts. Flatpak and snapd add their export
//! directories to it through profile scripts, so a session that was running
//! when Flatpak was installed cannot see a single Flatpak application until
//! the user logs out and back in. The application inherits the session's
//! environment, so it can tell, and say so, instead of letting an install
//! look like it did nothing.
//!
//! **Windows.** The counterpart of the desktop entry is the shortcut the
//! installer made: a `.lnk` in the machine or user Start Menu whose target
//! lies inside the application's install directory
//! ([`crate::sources::windows::shortcut`] finds it); for an MSIX package,
//! `shell:AppsFolder\<AUMID>`; for Scoop, the shim in `~\scoop\shims`.
//! `DisplayIcon` is never a launch target. Every one of them is handed to
//! `explorer.exe`, which passes it to the running shell, so the application
//! starts at the user's own level even when Brokey itself was started
//! elevated (the MSI's Start Brokey tickbox from an administrator console
//! does that, as `packaging/windows/brokey.wxs` records). `ShellExecuteW`
//! from an elevated process would start it elevated too, which is why it is
//! not used here, and `ShellExecuteEx` with `runas` belongs to
//! `transaction/elevate/` alone. Windows has no counterpart of the session
//! notice: the Start Menu has no list fixed at login.

use crate::model::Command;
use std::path::{Path, PathBuf};

/// How to open one installed package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    /// A desktop entry, opened with `gio launch`.
    Entry(PathBuf),
    /// A command started as it is (`flatpak run org.x.Y`), for a package
    /// with no desktop entry the session could use.
    Command(Command),
    /// Windows: a Start Menu shortcut, opened as the shell opens it, so its
    /// arguments and working directory are the installer's.
    Shortcut(PathBuf),
    /// Windows: an MSIX application, by its Application User Model ID
    /// (`PackageFamilyName!AppId`), opened as `shell:AppsFolder\<AUMID>`.
    /// Build it with [`apps_folder`], which refuses anything not shaped
    /// like an AUMID.
    AppsFolder(String),
    /// Windows: a Scoop shim in `~\scoop\shims`, for an application Scoop
    /// made no shortcut for.
    Shim(PathBuf),
}

impl Launch {
    /// The process that opens it. On Windows this is `explorer.exe` with
    /// one argument, which [`windows_argument`] spells.
    pub fn command(&self) -> Command {
        match self {
            Launch::Entry(path) => Command {
                program: "gio".to_string(),
                args: vec!["launch".to_string(), path.display().to_string()],
                env: Vec::new(),
                cwd: None,
            },
            Launch::Command(command) => command.clone(),
            Launch::Shortcut(_) | Launch::AppsFolder(_) | Launch::Shim(_) => Command {
                program: explorer_program(),
                args: self.windows_argument().into_iter().collect(),
                env: Vec::new(),
                cwd: None,
            },
        }
    }

    /// What `explorer.exe` is handed for a Windows launch: the file, or
    /// the Apps folder path. `None` for the Linux kinds.
    pub fn windows_argument(&self) -> Option<String> {
        match self {
            Launch::Shortcut(path) | Launch::Shim(path) => Some(path.display().to_string()),
            Launch::AppsFolder(aumid) => Some(format!(r"shell:AppsFolder\{aumid}")),
            Launch::Entry(_) | Launch::Command(_) => None,
        }
    }

    /// What the page shows in a tooltip: the entry's or shortcut's file
    /// name, the command line, or the application's AUMID.
    pub fn describe(&self) -> String {
        match self {
            Launch::Entry(path) | Launch::Shortcut(path) | Launch::Shim(path) => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            Launch::Command(c) => std::iter::once(c.program.as_str())
                .chain(c.args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            Launch::AppsFolder(aumid) => aumid.clone(),
        }
    }

    /// Whether the process exits once the application is started (`gio
    /// launch`, `explorer.exe`) or stays for the application's lifetime
    /// (`flatpak run`).
    pub fn returns_at_once(&self) -> bool {
        !matches!(self, Launch::Command(_))
    }
}

/// An MSIX application to open by its AUMID, when `aumid` is shaped like
/// one: a package family name (`Name_PublisherId`) and an application id
/// joined by a single `!`, in the characters the packaging schema allows.
/// Anything else is refused rather than handed to `explorer.exe`, where a
/// comma or a quote would be read as a switch.
pub fn apps_folder(aumid: &str) -> Option<Launch> {
    let (family, app) = aumid.split_once('!')?;
    let allowed = |s: &str| {
        !s.is_empty()
            && s.len() <= 256
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    (allowed(family) && allowed(app) && family.contains('_'))
        .then(|| Launch::AppsFolder(aumid.to_string()))
}

/// `explorer.exe` in the Windows directory the kernel reports, not in
/// whichever directory `%SystemRoot%` or `PATH` names: an environment
/// variable is something any program that starts Brokey can choose.
#[cfg(windows)]
pub fn explorer_program() -> String {
    use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;

    let mut buffer = [0u16; 260];
    // SAFETY: `buffer` is a writable array of exactly the length passed
    // alongside it, which is what this call is documented to fill.
    let written =
        unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    let directory = if written == 0 || written > buffer.len() {
        r"C:\Windows".to_string()
    } else {
        String::from_utf16_lossy(&buffer[..written])
    };
    format!(r"{}\explorer.exe", directory.trim_end_matches('\\'))
}

/// Off Windows there is no Explorer; the bare name keeps
/// [`Launch::command`] a plain function for the tests that run on Linux.
#[cfg(not(windows))]
pub fn explorer_program() -> String {
    "explorer.exe".to_string()
}

/// A plain command, no environment, no working directory.
pub fn command(program: impl Into<String>, args: &[&str]) -> Command {
    Command {
        program: program.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: Vec::new(),
        cwd: None,
    }
}

/// The desktop entries among a package's files that exist under `root`.
/// `files` are the paths the package manager recorded, with or without a
/// leading slash (pacman writes `usr/share/...`, dpkg `/usr/share/...`).
pub fn entries_in<'a>(files: impl IntoIterator<Item = &'a str>, root: &Path) -> Vec<PathBuf> {
    files
        .into_iter()
        .map(|f| f.trim().trim_start_matches('/'))
        .filter(|f| f.starts_with("usr/share/applications/") && f.ends_with(".desktop"))
        .map(|f| root.join(f))
        .filter(|p| p.is_file())
        .collect()
}

/// The launcher for a package whose installed files are `files`: the best
/// listable desktop entry among them, preferring the given names.
pub fn from_files<'a>(
    files: impl IntoIterator<Item = &'a str>,
    root: &Path,
    preferred: &[&str],
) -> Option<Launch> {
    choose_entry(entries_in(files, root), preferred).map(Launch::Entry)
}

/// The entry to open among several: one a launcher would list (a
/// `Type=Application` entry that is neither `NoDisplay` nor `Hidden`), and
/// among those the one whose file name is closest to what the package is
/// called. `preferred` is tried in order against the file name without
/// `.desktop`, case-insensitively: an exact match first, then one that
/// contains it. Otherwise the first listable entry, in file-name order so the
/// answer does not depend on the order a database wrote.
pub fn choose_entry(mut entries: Vec<PathBuf>, preferred: &[&str]) -> Option<PathBuf> {
    entries.sort();
    entries.retain(|p| listable(p));
    let stem = |p: &PathBuf| {
        p.file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    for want in preferred
        .iter()
        .map(|w| w.to_lowercase())
        .filter(|w| !w.is_empty())
    {
        if let Some(p) = entries.iter().find(|p| stem(p) == want) {
            return Some(p.clone());
        }
    }
    for want in preferred
        .iter()
        .map(|w| w.to_lowercase())
        .filter(|w| !w.is_empty())
    {
        if let Some(p) = entries.iter().find(|p| stem(p).contains(&want)) {
            return Some(p.clone());
        }
    }
    entries.into_iter().next()
}

/// Whether a launcher would list this entry. Only the `[Desktop Entry]`
/// group counts: an action group's keys say nothing about the entry.
pub fn listable(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    listable_text(&text)
}

pub fn listable_text(text: &str) -> bool {
    let mut in_main = false;
    let mut kind = None;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match (key.trim(), value.trim()) {
            ("NoDisplay", "true") | ("Hidden", "true") => return false,
            ("Type", v) => kind = Some(v.to_string()),
            _ => {}
        }
    }
    kind.as_deref() == Some("Application")
}

/// What a desktop entry says about the application it starts: the parts a
/// store row needs when no AppStream catalogue describes the package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryInfo {
    pub path: PathBuf,
    pub name: Option<String>,
    pub comment: Option<String>,
    /// The icon file the entry's `Icon=` resolves to, when one is found.
    pub icon: Option<PathBuf>,
    pub categories: Vec<String>,
}

/// The `[Desktop Entry]` group's untranslated `Name`, `Comment`, `Icon` and
/// `Categories`. Localised keys (`Name[de]`) are skipped: the rest of the
/// store is in one language too.
pub fn read_entry(path: &Path, icon_roots: &[PathBuf]) -> Option<EntryInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut info = EntryInfo {
        path: path.to_path_buf(),
        name: None,
        comment: None,
        icon: None,
        categories: Vec::new(),
    };
    let mut in_main = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "Name" => info.name = Some(value.to_string()),
            "Comment" => info.comment = Some(value.to_string()),
            "Icon" => info.icon = resolve_icon(value, icon_roots),
            "Categories" => {
                info.categories = value
                    .split(';')
                    .filter(|c| !c.is_empty())
                    .map(str::to_string)
                    .collect()
            }
            _ => {}
        }
    }
    Some(info)
}

/// The best listable desktop entry among a package's files, read.
pub fn app_entry<'a>(
    files: impl IntoIterator<Item = &'a str>,
    root: &Path,
    preferred: &[&str],
    icon_roots: &[PathBuf],
) -> Option<EntryInfo> {
    choose_entry(entries_in(files, root), preferred).and_then(|p| read_entry(&p, icon_roots))
}

/// Where icon themes live on this machine, `hicolor` first because every
/// application installs its icon there, then the other themes in name order.
pub fn icon_roots() -> Vec<PathBuf> {
    icon_roots_under(Path::new("/usr/share/icons"))
}

pub fn icon_roots_under(icons: &Path) -> Vec<PathBuf> {
    let mut roots = vec![icons.join("hicolor")];
    let mut others: Vec<PathBuf> = std::fs::read_dir(icons)
        .map(|it| {
            it.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n != "hicolor"))
                .collect()
        })
        .unwrap_or_default();
    others.sort();
    roots.extend(others);
    roots
}

/// The file an `Icon=` value names. An absolute path is taken as it is. A
/// name is looked up in each theme under the application sizes, largest
/// first (a store draws icons at 32 to 96 px), in both layouts themes use
/// (`48x48/apps` and `apps/48`), then in `/usr/share/pixmaps`. Only PNG and
/// SVG, which the page can draw.
pub fn resolve_icon(icon: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    let icon = icon.trim();
    if icon.starts_with('/') {
        let path = PathBuf::from(icon);
        return path.is_file().then_some(path);
    }
    if icon.is_empty() || icon.contains('/') {
        return None;
    }
    const SIZES: [&str; 9] = [
        "512", "256", "192", "128", "96", "64", "48", "scalable", "32",
    ];
    const EXTS: [&str; 2] = ["png", "svg"];
    for root in roots {
        for size in SIZES {
            let dir_a = if size == "scalable" {
                root.join("scalable/apps")
            } else {
                root.join(format!("{size}x{size}/apps"))
            };
            let dir_b = root.join("apps").join(size);
            for dir in [dir_a, dir_b] {
                for ext in EXTS {
                    let p = dir.join(format!("{icon}.{ext}"));
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
    }
    EXTS.iter()
        .map(|ext| PathBuf::from(format!("/usr/share/pixmaps/{icon}.{ext}")))
        .find(|p| p.is_file())
}

/// The directories the session reads desktop entries from: each entry of
/// `XDG_DATA_DIRS` with `applications` appended, and the user's own data
/// directory. An unset or empty variable means the specification's default,
/// `/usr/local/share:/usr/share`.
pub fn session_dirs(xdg_data_dirs: Option<&str>, xdg_data_home: Option<&Path>) -> Vec<PathBuf> {
    let dirs = match xdg_data_dirs {
        Some(v) if !v.trim().is_empty() => v.to_string(),
        _ => "/usr/local/share:/usr/share".to_string(),
    };
    let mut out: Vec<PathBuf> = dirs
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d.trim_end_matches('/')).join("applications"))
        .collect();
    if let Some(home) = xdg_data_home {
        out.push(home.join("applications"));
    }
    out
}

/// Whether a session with these directories lists entries from `dir`.
pub fn sees(session: &[PathBuf], dir: &Path) -> bool {
    let want = normalise(dir);
    session.iter().any(|d| normalise(d) == want)
}

fn normalise(p: &Path) -> String {
    p.display().to_string().trim_end_matches('/').to_string()
}

/// The running session's directories, from this process's environment,
/// which it inherited from the session.
pub fn this_session() -> Vec<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    session_dirs(
        std::env::var("XDG_DATA_DIRS").ok().as_deref(),
        data_home.as_deref(),
    )
}

/// The sentence for a format whose applications the session cannot list
/// yet: `export_dirs` that exist and hold at least one entry, and that the
/// session does not read. `None` when every such directory is seen, or none
/// holds anything.
pub fn notice_for(label: &str, export_dirs: &[PathBuf], session: &[PathBuf]) -> Option<String> {
    let unseen = export_dirs.iter().any(|dir| {
        let holds_entries = std::fs::read_dir(dir)
            .map(|mut it| {
                it.any(|e| {
                    e.map(|e| e.path().extension().is_some_and(|x| x == "desktop"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        holds_entries && !sees(session, dir)
    });
    unseen.then(|| {
        format!(
            "{label} applications are installed but this desktop session started before {label} was set up, so your launcher does not list them yet. Log out and back in once to see them there; until then, open them from Brokey."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    const APP: &str = "[Desktop Entry]\nType=Application\nName=X\nExec=x %U\n";

    #[test]
    fn entries_are_found_in_a_file_list_whichever_way_it_writes_paths() {
        let root = tempfile::tempdir().unwrap();
        write(
            &root.path().join("usr/share/applications/notepadqq.desktop"),
            APP,
        );
        let files = [
            "usr/",
            "usr/bin/notepadqq",
            "usr/share/applications/notepadqq.desktop",
            "/usr/share/applications/missing.desktop",
            "usr/share/applications/",
            "usr/share/icons/hicolor/48x48/apps/notepadqq.png",
        ];
        let found = entries_in(files, root.path());
        assert_eq!(
            found,
            [root.path().join("usr/share/applications/notepadqq.desktop")]
        );
    }

    #[test]
    fn the_listable_entry_named_after_the_package_is_chosen() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write(&d.join("emacsclient.desktop"), APP);
        write(&d.join("emacs.desktop"), APP);
        write(
            &d.join("emacs-mail.desktop"),
            "[Desktop Entry]\nType=Application\nNoDisplay=true\n",
        );
        let all = vec![
            d.join("emacsclient.desktop"),
            d.join("emacs.desktop"),
            d.join("emacs-mail.desktop"),
        ];
        assert_eq!(
            choose_entry(all.clone(), &["emacs"]),
            Some(d.join("emacs.desktop"))
        );
        assert_eq!(
            choose_entry(all.clone(), &["client"]),
            Some(d.join("emacsclient.desktop")),
            "a contained name is the second choice"
        );
        assert_eq!(
            choose_entry(all, &["nothing"]),
            Some(d.join("emacs.desktop")),
            "else the first listable entry by name"
        );
        assert_eq!(
            choose_entry(vec![d.join("emacs-mail.desktop")], &["emacs"]),
            None
        );
    }

    #[test]
    fn only_the_main_group_decides_whether_an_entry_is_listed() {
        assert!(listable_text(APP));
        assert!(!listable_text("[Desktop Entry]\nType=Link\n"));
        assert!(!listable_text(
            "[Desktop Entry]\nType=Application\nHidden=true\n"
        ));
        assert!(listable_text(
            "[Desktop Entry]\nType=Application\n\n[Desktop Action new]\nNoDisplay=true\n"
        ));
    }

    #[test]
    // XDG directories are colon-separated Unix paths.
    #[cfg(unix)]
    fn the_session_s_directories_follow_the_specification_default() {
        let home = Path::new("/home/me/.local/share");
        let default = session_dirs(None, Some(home));
        assert_eq!(
            default,
            [
                PathBuf::from("/usr/local/share/applications"),
                PathBuf::from("/usr/share/applications"),
                PathBuf::from("/home/me/.local/share/applications"),
            ]
        );
        assert_eq!(session_dirs(Some(""), None).len(), 2);
        let with_flatpak = session_dirs(Some("/var/lib/flatpak/exports/share/:/usr/share"), None);
        assert!(sees(
            &with_flatpak,
            Path::new("/var/lib/flatpak/exports/share/applications")
        ));
        assert!(!sees(
            &default,
            Path::new("/var/lib/flatpak/exports/share/applications")
        ));
    }

    #[test]
    // XDG directories are colon-separated Unix paths.
    #[cfg(unix)]
    fn the_notice_speaks_only_of_entries_the_session_cannot_see() {
        let dir = tempfile::tempdir().unwrap();
        let exports = dir.path().join("exports/share/applications");
        let session_without = session_dirs(Some("/usr/share"), None);
        let session_with = session_dirs(
            Some(&format!(
                "{}:/usr/share",
                dir.path().join("exports/share").display()
            )),
            None,
        );

        assert_eq!(
            notice_for("Flatpak", std::slice::from_ref(&exports), &session_without),
            None,
            "no directory yet"
        );
        std::fs::create_dir_all(&exports).unwrap();
        assert_eq!(
            notice_for("Flatpak", std::slice::from_ref(&exports), &session_without),
            None,
            "nothing in it yet"
        );
        write(&exports.join("com.notepadqq.Notepadqq.desktop"), APP);
        let sentence = notice_for("Flatpak", std::slice::from_ref(&exports), &session_without)
            .expect("unseen entries");
        assert!(sentence.contains("Log out and back in once"), "{sentence}");
        assert!(!sentence.contains('\u{2014}'));
        assert_eq!(
            notice_for("Flatpak", &[exports], &session_with),
            None,
            "a session that sees them"
        );
    }

    #[test]
    fn an_entry_opens_through_gio_and_a_command_as_itself() {
        let entry = Launch::Entry(PathBuf::from("/usr/share/applications/notepadqq.desktop"));
        assert_eq!(entry.command().program, "gio");
        assert_eq!(
            entry.command().args,
            ["launch", "/usr/share/applications/notepadqq.desktop"]
        );
        assert_eq!(entry.describe(), "notepadqq.desktop");
        assert!(entry.returns_at_once());
        let run = Launch::Command(command("flatpak", &["run", "com.notepadqq.Notepadqq"]));
        assert_eq!(run.describe(), "flatpak run com.notepadqq.Notepadqq");
        assert!(!run.returns_at_once());
    }

    #[test]
    fn a_windows_launch_is_handed_to_explorer() {
        let lnk = Launch::Shortcut(PathBuf::from(
            r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Obsidian.lnk",
        ));
        let c = lnk.command();
        assert!(c.program.ends_with("explorer.exe"), "{}", c.program);
        assert_eq!(
            c.args,
            [r"C:\ProgramData\Microsoft\Windows\Start Menu\Programs\Obsidian.lnk"]
        );
        assert!(lnk.returns_at_once());
        let app = apps_folder("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App").unwrap();
        assert_eq!(
            app.windows_argument().as_deref(),
            Some(r"shell:AppsFolder\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App")
        );
        assert_eq!(
            app.describe(),
            "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"
        );
        assert_eq!(
            Launch::Entry(PathBuf::from("/x.desktop")).windows_argument(),
            None
        );
    }

    #[test]
    fn only_a_string_shaped_like_an_aumid_opens_the_apps_folder() {
        for bad in [
            "",
            "NoBang",
            "Family_x!",
            "!App",
            "NoUnderscore!App",
            "Family_x!App,/select",
            "Family_x!App\" /e",
            "Family_x!A!B",
            r"Family_x!..\..\x",
        ] {
            assert_eq!(apps_folder(bad), None, "{bad}");
        }
        assert!(apps_folder("Spotify.Spotify_zpdnekdrzrea0!Spotify").is_some());
    }
    #[test]
    fn an_entry_gives_its_name_comment_categories_and_icon() {
        let dir = tempfile::tempdir().unwrap();
        let icons = dir.path().join("icons");
        write(&icons.join("hicolor/256x256/apps/steam.png"), "png");
        write(&icons.join("hicolor/48x48/apps/steam.png"), "png");
        write(&icons.join("Adwaita/scalable/apps/btop.svg"), "svg");
        write(&icons.join("breeze/apps/48/kate.svg"), "svg");
        let roots = icon_roots_under(&icons);
        assert_eq!(roots[0], icons.join("hicolor"), "hicolor first");
        let entry = dir.path().join("steam.desktop");
        write(
            &entry,
            "[Desktop Entry]\nType=Application\nName=Steam\nName[de]=Dampf\nComment=Play games\nIcon=steam\nCategories=Network;Game;\n\n[Desktop Action x]\nName=Not this\n",
        );
        let info = read_entry(&entry, &roots).unwrap();
        assert_eq!(info.name.as_deref(), Some("Steam"));
        assert_eq!(info.comment.as_deref(), Some("Play games"));
        assert_eq!(info.categories, ["Network", "Game"]);
        assert_eq!(
            info.icon,
            Some(icons.join("hicolor/256x256/apps/steam.png")),
            "the largest size"
        );
        assert_eq!(
            resolve_icon("btop", &roots),
            Some(icons.join("Adwaita/scalable/apps/btop.svg"))
        );
        assert_eq!(
            resolve_icon("kate", &roots),
            Some(icons.join("breeze/apps/48/kate.svg")),
            "the other layout"
        );
        assert_eq!(resolve_icon("nothing-like-this", &roots), None);
        assert_eq!(resolve_icon("../etc/passwd", &roots), None);
    }
}
