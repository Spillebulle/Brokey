//! Start Menu shortcuts, and which one opens an installed application.
//!
//! This is the Windows half of [`crate::launch`]. On Linux a package's own
//! file list names its desktop entry. Nothing on Windows records which
//! shortcut an installer made, so the rule is the spec's: the shortcut to
//! open is a `.lnk` under the machine or the user Start Menu whose target
//! lies inside the application's install directory, and that directory is
//! derived from the uninstaller, as [`arp::install_dir`] does it, with an
//! unquoted path read the way Windows reads it (see [`search_dir`]). The
//! shortcut is opened as it is, so its arguments and working directory are
//! the installer's; Brokey never guesses the binary and never launches
//! `DisplayIcon`, which frequently names an uninstaller or a file with no
//! code in it.
//!
//! A `.lnk` is read here rather than through `IShellLinkW`: the format is
//! documented ([MS-SHLLINK]) and short, reading it needs no COM apartment,
//! and a pure reader is tested against checked-in shortcuts on both
//! platforms, the way `pe.rs` reads icons. Only what resolution needs is
//! read: the target path, from the link information or the environment
//! variable block, and the arguments, which tell a Squirrel application's
//! `Update.exe --processStart` from its `Update.exe --uninstall`.
//!
//! Everything below [`parse`] is a pure function of shortcuts already read,
//! except [`start_menu`], which walks the two folders and is the only thing
//! here that touches the disk.
//!
//! [MS-SHLLINK]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-shllink/

use super::arp::{self, RawEntry};
use std::path::{Path, PathBuf};

/// What a `.lnk` file says, as far as opening an application needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Link {
    /// The file the shortcut starts, as an absolute Windows path. `None` for
    /// a shortcut to a shell folder, a URL, a network share, or an MSI
    /// advertised shortcut, none of which names a file inside an install
    /// directory.
    pub target: Option<String>,
    /// The command line arguments, empty when there are none.
    pub arguments: String,
}

const HEADER_SIZE: usize = 0x4C;
const LINK_CLSID: [u8; 16] = [
    0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

const HAS_LINK_TARGET_ID_LIST: u32 = 1 << 0;
const HAS_LINK_INFO: u32 = 1 << 1;
const HAS_NAME: u32 = 1 << 2;
const HAS_RELATIVE_PATH: u32 = 1 << 3;
const HAS_WORKING_DIR: u32 = 1 << 4;
const HAS_ARGUMENTS: u32 = 1 << 5;
const HAS_ICON_LOCATION: u32 = 1 << 6;
const IS_UNICODE: u32 = 1 << 7;
const FORCE_NO_LINK_INFO: u32 = 1 << 8;
const HAS_EXP_STRING: u32 = 1 << 9;

const VOLUME_ID_AND_LOCAL_BASE_PATH: u32 = 1 << 0;
const ENVIRONMENT_VARIABLE_BLOCK: u32 = 0xA000_0001;
/// TargetAnsi (260 bytes) then TargetUnicode (520), after size and signature.
const ENVIRONMENT_VARIABLE_BLOCK_SIZE: usize = 0x314;

/// A Start Menu shortcut is a few kilobytes. Anything far larger is not
/// one, and is not read into memory to find that out.
pub const MAX_LINK_BYTES: u64 = 1 << 20;

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let s = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let s = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// A NUL-terminated string in the system code page. Paths in a link's ANSI
/// fields are read as Latin-1, which is right for every ASCII path and for
/// Western code pages; the Unicode fields are preferred wherever a link has
/// them, which every link Windows has written since XP does.
fn ansi_z(bytes: &[u8], at: usize) -> Option<String> {
    let rest = bytes.get(at..)?;
    let end = rest.iter().position(|&b| b == 0)?;
    Some(rest[..end].iter().map(|&b| b as char).collect())
}

/// A NUL-terminated UTF-16 string, bounded by `limit` bytes when given.
fn unicode_z(bytes: &[u8], at: usize, limit: Option<usize>) -> Option<String> {
    let rest = bytes.get(at..)?;
    let rest = match limit {
        Some(n) => rest.get(..n)?,
        None => rest,
    };
    let units: Vec<u16> = utf16_units(rest).take_while(|&u| u != 0).collect();
    Some(String::from_utf16_lossy(&units))
}

/// Little-endian UTF-16 code units; an odd byte at the end is dropped.
fn utf16_units(bytes: &[u8]) -> impl Iterator<Item = u16> + '_ {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
}

/// The local path the link information carries, when it carries one.
fn link_info_path(bytes: &[u8], at: usize) -> Option<String> {
    let header_size = u32_at(bytes, at + 4)? as usize;
    let flags = u32_at(bytes, at + 8)?;
    if flags & VOLUME_ID_AND_LOCAL_BASE_PATH == 0 {
        return None;
    }
    let base_offset = u32_at(bytes, at + 16)? as usize;
    let suffix_offset = u32_at(bytes, at + 24)? as usize;
    // The Unicode offsets are present only when the header says it is long
    // enough to hold them.
    if header_size >= 0x24 {
        let base_u = u32_at(bytes, at + 28)? as usize;
        let suffix_u = u32_at(bytes, at + 32)? as usize;
        if base_u != 0 {
            let base = unicode_z(bytes, at.checked_add(base_u)?, None)?;
            let suffix = if suffix_u != 0 {
                unicode_z(bytes, at.checked_add(suffix_u)?, None).unwrap_or_default()
            } else {
                String::new()
            };
            return Some(join_suffix(base, &suffix));
        }
    }
    let base = ansi_z(bytes, at.checked_add(base_offset)?)?;
    let suffix = ansi_z(bytes, at.checked_add(suffix_offset)?).unwrap_or_default();
    Some(join_suffix(base, &suffix))
}

fn join_suffix(base: String, suffix: &str) -> String {
    if suffix.is_empty() {
        base
    } else if base.ends_with('\\') {
        format!("{base}{suffix}")
    } else {
        format!("{base}\\{suffix}")
    }
}

/// Read a `.lnk`. `None` when the bytes are not a shell link at all.
///
/// `expand` turns `%ProgramFiles%` and the like into the machine's own
/// directories, for a link whose target is held only in its environment
/// variable block. It is a parameter so that the reader stays a pure
/// function of its bytes; [`expand_environment`] is what the machine passes.
pub fn parse(bytes: &[u8], expand: &dyn Fn(&str) -> Option<String>) -> Option<Link> {
    if u32_at(bytes, 0)? as usize != HEADER_SIZE || bytes.get(4..20)? != LINK_CLSID {
        return None;
    }
    let flags = u32_at(bytes, 0x14)?;
    let mut at = HEADER_SIZE;
    let mut from_id_list = None;
    if flags & HAS_LINK_TARGET_ID_LIST != 0 {
        let size = u16_at(bytes, at)? as usize;
        from_id_list = bytes
            .get(at + 2..at.checked_add(2 + size)?)
            .and_then(id_list_path);
        at = at.checked_add(2 + size)?;
    }
    let mut target = None;
    if flags & HAS_LINK_INFO != 0 {
        let size = u32_at(bytes, at)? as usize;
        if flags & FORCE_NO_LINK_INFO == 0 {
            target = link_info_path(bytes, at);
        }
        at = at.checked_add(size)?;
    }
    let unicode = flags & IS_UNICODE != 0;
    let mut arguments = String::new();
    for (flag, keep) in [
        (HAS_NAME, false),
        (HAS_RELATIVE_PATH, false),
        (HAS_WORKING_DIR, false),
        (HAS_ARGUMENTS, true),
        (HAS_ICON_LOCATION, false),
    ] {
        if flags & flag == 0 {
            continue;
        }
        let count = u16_at(bytes, at)? as usize;
        let len = if unicode { count * 2 } else { count };
        let text = bytes.get(at + 2..at.checked_add(2 + len)?)?;
        if keep {
            arguments = if unicode {
                String::from_utf16_lossy(&utf16_units(text).collect::<Vec<_>>())
            } else {
                text.iter().map(|&b| b as char).collect()
            };
        }
        at += 2 + len;
    }
    if target.is_none() && flags & HAS_EXP_STRING != 0 {
        target = environment_target(bytes, at).and_then(|t| expand(&t));
    }
    // NSIS writes shortcuts with no link information at all, only the
    // item list (OrcaSlicer, Plex and Epic Games on the reference machine).
    let target = target.or(from_id_list);
    Some(Link {
        target: target.filter(|t| !t.is_empty()),
        arguments: arguments.trim().to_string(),
    })
}

/// `{20D04FE0-3AEA-1069-A2D8-08002B30309D}`, This PC, as it is stored.
const MY_COMPUTER: [u8; 16] = [
    0xE0, 0x4F, 0xD0, 0x20, 0xEA, 0x3A, 0x69, 0x10, 0xA2, 0xD8, 0x08, 0x00, 0x2B, 0x30, 0x30, 0x9D,
];
const FILE_ENTRY_EXTENSION: u32 = 0xBEEF_0004;

/// The file-system path an item list spells: This PC, then a drive
/// (`C:\`), then one file entry per folder down to the file. Anything else
/// (a known folder, a control panel item, a URL) is `None`: it is not a
/// file inside an install directory. A file entry's long name is read from
/// its `0xBEEF0004` extension block, whose layout moves with its version;
/// the 8.3 name in the entry itself is the fallback.
fn id_list_path(list: &[u8]) -> Option<String> {
    let mut items = Vec::new();
    let mut at = 0;
    loop {
        let size = u16_at(list, at)? as usize;
        if size == 0 {
            break;
        }
        items.push(list.get(at + 2..at.checked_add(size)?)?);
        at += size;
    }
    let (root, rest) = items.split_first()?;
    if root.first() != Some(&0x1F) || root.get(2..18)? != MY_COMPUTER {
        return None;
    }
    let (drive, entries) = rest.split_first()?;
    if drive.first()? & 0x70 != 0x20 {
        return None;
    }
    let mut path = ansi_z(drive, 1)?;
    if !(path.len() == 3 && path.ends_with(":\\")) {
        return None;
    }
    if entries.is_empty() {
        return None;
    }
    for (i, entry) in entries.iter().enumerate() {
        let name = file_entry_name(entry)?;
        if name.is_empty() || name.contains(['\\', '/']) {
            return None;
        }
        if i > 0 {
            path.push('\\');
        }
        path.push_str(&name);
    }
    Some(path)
}

/// A file entry item's long name. `entry` starts at the type byte.
fn file_entry_name(entry: &[u8]) -> Option<String> {
    let kind = *entry.first()?;
    if kind & 0x70 != 0x30 {
        return None;
    }
    // Type, unknown, size (4), modified (4), attributes (2), then the name.
    let short_at = 12;
    let unicode = kind & 0x04 != 0;
    let short = if unicode {
        unicode_z(entry, short_at, None)?
    } else {
        ansi_z(entry, short_at)?
    };
    let long = (short_at..entry.len().saturating_sub(4)).find_map(|i| {
        if u32_at(entry, i + 4)? != FILE_ENTRY_EXTENSION {
            return None;
        }
        let block_size = u16_at(entry, i)? as usize;
        let version = u16_at(entry, i + 2)?;
        let name_at = match version {
            0..=2 => return None,
            3..=6 => 20,
            7 => 38,
            8 => 42,
            _ => 46,
        };
        let block = entry.get(i..i.checked_add(block_size)?)?;
        unicode_z(block, name_at, Some(block_size.checked_sub(name_at)?))
    });
    Some(long.filter(|l| !l.is_empty()).unwrap_or(short))
}

/// The target held in the environment variable data block, unexpanded.
fn environment_target(bytes: &[u8], mut at: usize) -> Option<String> {
    loop {
        let size = u32_at(bytes, at)? as usize;
        if size < 8 {
            return None;
        }
        if u32_at(bytes, at + 4)? == ENVIRONMENT_VARIABLE_BLOCK
            && size >= ENVIRONMENT_VARIABLE_BLOCK_SIZE
        {
            let unicode = unicode_z(bytes, at + 8 + 260, Some(520)).filter(|s| !s.is_empty());
            return unicode.or_else(|| ansi_z(bytes, at + 8));
        }
        at = at.checked_add(size)?;
    }
}

/// `%Name%` replaced by the variable's value, each one, case-insensitively
/// as Windows does. `None` when one of them is not set, rather than a path
/// with a hole in it.
pub fn expand_with(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find('%')?;
        let name = &after[..end];
        if name.is_empty() {
            out.push('%');
        } else {
            out.push_str(&lookup(name)?);
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// [`expand_with`] against this process's environment.
pub fn expand_environment(text: &str) -> Option<String> {
    expand_with(text, &|name| {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    })
}

/// One shortcut found in the Start Menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shortcut {
    /// The `.lnk` file itself, which is what gets opened.
    pub path: PathBuf,
    pub target: String,
    pub arguments: String,
}

impl Shortcut {
    /// The shortcut's own name, which is what the Start Menu shows.
    fn name(&self) -> String {
        windows_file_stem(&self.path.to_string_lossy()).to_string()
    }

    /// How many folders deep in the Start Menu it sits, counting from
    /// `Programs`. Only compared between shortcuts, never shown.
    fn depth(&self) -> usize {
        self.path.components().count()
    }
}

/// A path as Windows compares it: one kind of separator, no trailing one,
/// and no case.
pub fn normalise(path: &str) -> String {
    path.trim()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// Whether `path` lies inside `dir`, comparing as Windows does.
pub fn inside(path: &str, dir: &str) -> bool {
    let path = normalise(path);
    let dir = normalise(dir);
    !dir.is_empty()
        && path.len() > dir.len()
        && path.starts_with(&dir)
        && path.as_bytes()[dir.len()] == b'\\'
}

fn windows_file_stem(path: &str) -> &str {
    let name = match path.rfind(['\\', '/']) {
        Some(cut) => &path[cut + 1..],
        None => path,
    };
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

fn windows_extension(path: &str) -> Option<String> {
    let name = &path[path.rfind(['\\', '/']).map_or(0, |c| c + 1)..];
    name.rfind('.')
        .filter(|&d| d > 0)
        .map(|d| name[d + 1..].to_lowercase())
}

/// Whether a directory is narrow enough to say which application a
/// shortcut belongs to. An uninstaller that lives in a shared directory
/// (`rundll32` in `System32`, a stub in `Program Files` itself) derives a
/// directory every second shortcut on the machine points into, and the
/// first of those is not this application. So a drive's root, anything in
/// the Windows directory, and the folders applications are installed
/// *under* rather than *to* are refused.
pub fn specific(dir: &str) -> bool {
    let dir = normalise(dir);
    let parts: Vec<&str> = dir.split('\\').filter(|p| !p.is_empty()).collect();
    let Some((drive, below)) = parts.split_first() else {
        return false;
    };
    if !(drive.len() == 2 && drive.ends_with(':')) {
        // A share or a relative path says nothing reliable about anything.
        return false;
    }
    !matches!(
        below,
        [] | ["windows", ..]
            | ["program files" | "program files (x86)" | "programdata" | "users"]
            | ["programdata", "microsoft", ..]
            | ["users", _]
            | ["users", _, "appdata"]
            | ["users", _, "appdata", "local" | "roaming" | "locallow"]
            | [
                "users",
                _,
                "appdata",
                "local",
                "programs" | "temp" | "microsoft",
            ]
            | ["users", _, "appdata", "roaming", "microsoft"]
    )
}

/// The uninstaller an entry names, split into its program and arguments.
///
/// [`arp::command_line`] reads it, the one reader of an uninstall string:
/// an unquoted path with spaces, such as Steam's `C:\Program Files (x86)\
/// Steam\uninstall.exe`, is read the way Windows reads it, so the directory
/// searched here is the one the Add/Remove Programs removal runs from.
pub fn uninstaller(e: &RawEntry) -> Option<(String, Vec<String>)> {
    arp::command_line(e.uninstall_string.as_deref()?)
}

fn is_msiexec(program: &str) -> bool {
    windows_file_stem(program).eq_ignore_ascii_case("msiexec")
}

fn parent(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches(['\\', '/']);
    let cut = trimmed.rfind(['\\', '/'])?;
    Some(&trimmed[..cut])
}

/// Where an application lives, for finding its shortcut.
///
/// The uninstaller's directory first, as the spec has it, with
/// [`uninstaller`]'s reading of an unquoted path. Two refinements, each
/// recognised by the uninstaller's own path and never by what a shortcut
/// points at, so neither can widen a directory to fit a shortcut:
///
/// - an uninstaller in a folder of its own named `uninstall` (Zoom's
///   `Zoom\uninstall\Installer.exe`, beside `Zoom\bin\Zoom.exe`) means the
///   folder above it;
/// - an `InstallLocation` that holds the uninstaller's directory confirms a
///   wider home, and is taken: Chrome, Edge and Vivaldi keep their
///   uninstaller in `Application\<version>\Installer` and the program in
///   `Application`, which is what their `InstallLocation` says.
///
/// An `msiexec` uninstaller lives in the system directory and says nothing,
/// so an MSI entry has only its `InstallLocation`.
pub fn search_dir(e: &RawEntry) -> Option<String> {
    let location = e
        .install_location
        .as_deref()
        .map(|l| l.trim().trim_end_matches(['\\', '/']).to_string())
        .filter(|l| !l.is_empty());
    let from_uninstaller = uninstaller(e)
        .filter(|(program, _)| !is_msiexec(program))
        .and_then(|(program, _)| parent(&program).map(str::to_string))
        .map(|dir| {
            let last = dir.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase();
            if matches!(last.as_str(), "uninstall" | "uninst" | "uninstaller") {
                parent(&dir).map(str::to_string).unwrap_or(dir)
            } else {
                dir
            }
        });
    match (from_uninstaller, location) {
        (Some(dir), Some(location)) if inside(&dir, &location) && specific(&location) => {
            Some(location)
        }
        (Some(dir), _) => Some(dir),
        (None, location) => location,
    }
}

/// One place an application is known to live, and what it is called there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Home {
    pub dir: String,
    /// The uninstaller, so a shortcut to it is never what Open opens.
    pub uninstaller: Option<(String, Vec<String>)>,
    /// The application's name, to choose between several shortcuts.
    pub name: Option<String>,
}

impl Home {
    /// Where an uninstall entry's application lives, when that can be
    /// told narrowly enough to be worth looking in.
    pub fn of(e: &RawEntry) -> Option<Home> {
        let dir = search_dir(e).filter(|d| specific(d))?;
        Some(Home {
            dir,
            uninstaller: uninstaller(e),
            name: e.display_name.clone(),
        })
    }
}

/// Whether a shortcut starts an uninstaller rather than the application.
///
/// The uninstaller lives in the install directory by construction, which
/// is how the directory was found, so its own shortcut is always a
/// candidate and has to be ruled out. It is the uninstaller when it starts
/// the program the uninstall entry names with the same arguments (Squirrel
/// applications start the application through the uninstaller's own
/// program with other arguments, so the program alone is not enough), or
/// when the program is named like one: Inno Setup's `unins000.exe`, NSIS's
/// `uninst.exe` and `Uninstall.exe`, `tesseract-uninstall.exe`. A program
/// whose name merely contains the word (`IObitUninstaller.exe`) is an
/// application about uninstalling and is kept.
fn is_uninstaller(s: &Shortcut, home: &Home) -> bool {
    let stem = windows_file_stem(&s.target).to_lowercase();
    if stem.starts_with("unins") || stem.ends_with("uninstall") {
        return true;
    }
    let Some((program, args)) = &home.uninstaller else {
        return false;
    };
    normalise(program) == normalise(&s.target)
        && args.join(" ").to_lowercase() == s.arguments.to_lowercase()
}

/// Whether a shortcut starts the uninstaller's own program, whatever its
/// arguments.
fn starts_the_uninstaller_s_program(s: &Shortcut, home: &Home) -> bool {
    home.uninstaller
        .as_ref()
        .is_some_and(|(program, _)| normalise(program) == normalise(&s.target))
}

/// The first word of a name, for [`fit`]: letters and digits only.
fn first_word(name: &str) -> String {
    name.split(|c: char| !c.is_alphanumeric())
        .find(|w| !w.is_empty())
        .unwrap_or("")
        .to_lowercase()
}

/// A name that does not fit at all; see [`fit`].
const NO_FIT: u8 = 3;

/// How well a shortcut's name fits the application's: 0 the same name,
/// 1 one contains the other (`Cheat Engine` in `Cheat Engine 7.6`), 2 both
/// begin with the same word of three letters or more (`Nmap 7.99` and
/// `Nmap - Zenmap GUI`), [`NO_FIT`] none of those.
fn fit(s: &Shortcut, name: Option<&str>) -> u8 {
    let Some(name) = name.map(str::to_lowercase).filter(|n| !n.is_empty()) else {
        return NO_FIT;
    };
    let own = s.name().to_lowercase();
    let word = first_word(&own);
    if own == name {
        0
    } else if !own.is_empty() && (name.contains(&own) || own.contains(&name)) {
        1
    } else if word.chars().count() >= 3 && word == first_word(&name) {
        2
    } else {
        NO_FIT
    }
}

/// The shortcut that opens an application living in `homes`, best first.
///
/// A candidate is a shortcut to a program (`.exe`) inside one of them that
/// is not an uninstaller. Two kinds are taken only when their name fits
/// the application's, because a name that does not fit means the shortcut
/// opens something else: one that passes arguments (`About Java` is
/// `javacpl.exe -tab about`), and one that starts the uninstaller's own
/// program (every Steam game's uninstaller is `steam.exe`, and Open on a
/// game must not open Steam).
///
/// Among candidates: the one named most like the application, then one
/// without arguments (the plain shortcut, before `reset preferences` or
/// `safe mode`), then the one nearest the top of the Start Menu, then by
/// path so that the answer does not depend on the order a folder was
/// listed in. `None` when there is none, which means no Open button.
pub fn choose<'a>(shortcuts: &'a [Shortcut], homes: &[Home]) -> Option<&'a Shortcut> {
    type Rank<'p> = (u8, bool, usize, &'p Path);
    let mut best: Option<(Rank, &Shortcut)> = None;
    for home in homes {
        for s in shortcuts {
            if !inside(&s.target, &home.dir)
                || windows_extension(&s.target).as_deref() != Some("exe")
                || is_uninstaller(s, home)
            {
                continue;
            }
            let fit = fit(s, home.name.as_deref());
            let with_arguments = !s.arguments.is_empty();
            if fit == NO_FIT && (with_arguments || starts_the_uninstaller_s_program(s, home)) {
                continue;
            }
            let rank = (fit, with_arguments, s.depth(), s.path.as_path());
            if best.as_ref().is_none_or(|(b, _)| rank < *b) {
                best = Some((rank, s));
            }
        }
    }
    best.map(|(_, s)| s)
}

/// The two Start Menu folders, the machine's first. From the environment,
/// which is only ever used to *find* shortcuts: what is opened is a file
/// found there, never a path a variable names.
pub fn start_menu_dirs() -> Vec<PathBuf> {
    let programs = |base: Option<std::ffi::OsString>| {
        base.filter(|b| !b.is_empty())
            .map(|b| PathBuf::from(b).join(r"Microsoft\Windows\Start Menu\Programs"))
    };
    [
        programs(std::env::var_os("ProgramData")),
        programs(std::env::var_os("APPDATA")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Every shortcut under `dirs` that names a file, read. Folders are walked
/// to any depth, but a junction or symbolic link is not followed, so a loop
/// on disk cannot hold the walk.
pub fn read_dirs(dirs: &[PathBuf], expand: &dyn Fn(&str) -> Option<String>) -> Vec<Shortcut> {
    let mut out = Vec::new();
    let mut pending: Vec<PathBuf> = dirs.to_vec();
    while let Some(dir) = pending.pop() {
        let Ok(it) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in it.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
                continue;
            }
            let is_lnk = path
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("lnk"));
            if !kind.is_file() || !is_lnk {
                continue;
            }
            if entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > MAX_LINK_BYTES {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if let Some(Link {
                target: Some(target),
                arguments,
            }) = parse(&bytes, expand)
            {
                out.push(Shortcut {
                    path,
                    target,
                    arguments,
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// The machine's Start Menu shortcuts, read at most once every few seconds.
///
/// The page asks what Open would open for every installed application in
/// one burst, and each answer is a search of the same few hundred
/// shortcuts. Reading them once per burst rather than once per application
/// is the difference between a list that draws its buttons at once and one
/// that reads the Start Menu a hundred and fifty times over. A few seconds
/// is short enough that a shortcut an install has just made is seen by the
/// time the page asks again after the plan.
pub fn start_menu() -> std::sync::Arc<Vec<Shortcut>> {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, Arc<Vec<Shortcut>>)>> = Mutex::new(None);
    const FRESH: Duration = Duration::from_secs(5);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((when, list)) = cache.as_ref()
        && when.elapsed() < FRESH
    {
        return list.clone();
    }
    let list = Arc::new(read_dirs(&start_menu_dirs(), &expand_environment));
    *cache = Some((Instant::now(), list.clone()));
    list
}

/// The shortcut that opens an uninstall entry's application, as a launch.
pub fn launch_for_entries(
    entries: &[&RawEntry],
    shortcuts: &[Shortcut],
) -> Option<crate::launch::Launch> {
    let homes: Vec<Home> = entries.iter().filter_map(|e| Home::of(e)).collect();
    choose(shortcuts, &homes).map(|s| crate::launch::Launch::Shortcut(s.path.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/windows/lnk/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// A shell link built field by field, for the shapes the checked-in
    /// shortcuts do not cover.
    struct Builder {
        flags: u32,
        id_list: Option<Vec<u8>>,
        link_info: Option<Vec<u8>>,
        strings: Vec<(u32, String)>,
        extra: Vec<u8>,
    }

    impl Builder {
        fn new() -> Builder {
            Builder {
                flags: IS_UNICODE,
                id_list: None,
                link_info: None,
                strings: Vec::new(),
                extra: Vec::new(),
            }
        }

        fn ansi(mut self) -> Builder {
            self.flags &= !IS_UNICODE;
            self
        }

        fn id_list(mut self, bytes: Vec<u8>) -> Builder {
            self.flags |= HAS_LINK_TARGET_ID_LIST;
            self.id_list = Some(bytes);
            self
        }

        /// Link information with a local base path, ANSI only or with the
        /// Unicode copy as well.
        fn local_path(mut self, ansi: &str, unicode: Option<&str>) -> Builder {
            self.flags |= HAS_LINK_INFO;
            let header: u32 = if unicode.is_some() { 0x24 } else { 0x1C };
            let mut body = Vec::new();
            // VolumeID: size, drive type, serial, label offset, empty label.
            let volume_at = header as usize;
            let volume = {
                let mut v = Vec::new();
                v.extend_from_slice(&17u32.to_le_bytes());
                v.extend_from_slice(&3u32.to_le_bytes());
                v.extend_from_slice(&0u32.to_le_bytes());
                v.extend_from_slice(&16u32.to_le_bytes());
                v.push(0);
                v
            };
            body.extend_from_slice(&volume);
            let base_at = volume_at + body.len();
            body.extend(ansi.bytes());
            body.push(0);
            let suffix_at = volume_at + body.len();
            body.push(0);
            let (base_u_at, suffix_u_at) = if let Some(u) = unicode {
                let a = volume_at + body.len();
                body.extend(u.encode_utf16().flat_map(|c| c.to_le_bytes()));
                body.extend([0, 0]);
                let b = volume_at + body.len();
                body.extend([0, 0]);
                (a as u32, b as u32)
            } else {
                (0, 0)
            };
            let size = header as usize + body.len();
            let mut info = Vec::new();
            info.extend_from_slice(&(size as u32).to_le_bytes());
            info.extend_from_slice(&header.to_le_bytes());
            info.extend_from_slice(&VOLUME_ID_AND_LOCAL_BASE_PATH.to_le_bytes());
            info.extend_from_slice(&(volume_at as u32).to_le_bytes());
            info.extend_from_slice(&(base_at as u32).to_le_bytes());
            info.extend_from_slice(&0u32.to_le_bytes());
            info.extend_from_slice(&(suffix_at as u32).to_le_bytes());
            if unicode.is_some() {
                info.extend_from_slice(&base_u_at.to_le_bytes());
                info.extend_from_slice(&suffix_u_at.to_le_bytes());
            }
            info.extend(body);
            self.link_info = Some(info);
            self
        }

        fn string(mut self, flag: u32, text: &str) -> Builder {
            self.flags |= flag;
            self.strings.push((flag, text.to_string()));
            self
        }

        fn environment(mut self, target: &str) -> Builder {
            self.flags |= HAS_EXP_STRING;
            let mut block = vec![0u8; ENVIRONMENT_VARIABLE_BLOCK_SIZE];
            block[..4].copy_from_slice(&(ENVIRONMENT_VARIABLE_BLOCK_SIZE as u32).to_le_bytes());
            block[4..8].copy_from_slice(&ENVIRONMENT_VARIABLE_BLOCK.to_le_bytes());
            for (i, b) in target.bytes().enumerate() {
                block[8 + i] = b;
            }
            for (i, u) in target.encode_utf16().enumerate() {
                block[268 + i * 2..270 + i * 2].copy_from_slice(&u.to_le_bytes());
            }
            self.extra.extend(block);
            self
        }

        fn build(self) -> Vec<u8> {
            let mut out = vec![0u8; HEADER_SIZE];
            out[..4].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
            out[4..20].copy_from_slice(&LINK_CLSID);
            out[0x14..0x18].copy_from_slice(&self.flags.to_le_bytes());
            if let Some(ids) = self.id_list {
                out.extend_from_slice(&(ids.len() as u16).to_le_bytes());
                out.extend(ids);
            }
            if let Some(info) = self.link_info {
                out.extend(info);
            }
            let mut strings = self.strings;
            strings.sort_by_key(|(f, _)| *f);
            for (_, text) in strings {
                if self.flags & IS_UNICODE != 0 {
                    let units: Vec<u16> = text.encode_utf16().collect();
                    out.extend_from_slice(&(units.len() as u16).to_le_bytes());
                    out.extend(units.iter().flat_map(|u| u.to_le_bytes()));
                } else {
                    out.extend_from_slice(&(text.len() as u16).to_le_bytes());
                    out.extend(text.bytes());
                }
            }
            out.extend(self.extra);
            out.extend_from_slice(&0u32.to_le_bytes());
            out
        }
    }

    #[test]
    fn a_real_shortcut_gives_its_target() {
        let link = parse(&fixture("Obsidian.lnk"), &no_env).expect("a shell link");
        assert_eq!(
            link.target.as_deref(),
            Some(r"C:\Program Files\Obsidian\Obsidian.exe")
        );
        assert_eq!(link.arguments, "");
    }

    #[test]
    fn a_real_uninstaller_shortcut_gives_its_target() {
        let link = parse(&fixture("Uninstall WizTree.lnk"), &no_env).expect("a shell link");
        assert_eq!(
            link.target.as_deref(),
            Some(r"C:\Program Files\WizTree\unins000.exe")
        );
    }

    #[test]
    fn a_real_shortcut_with_arguments_keeps_them() {
        let link = parse(&fixture("Google Chrome.lnk"), &no_env).expect("a shell link");
        assert_eq!(
            link.target.as_deref(),
            Some(r"C:\Program Files\Google\Chrome\Application\chrome.exe")
        );
        assert_eq!(link.arguments, "--source-shortcut-location=start-menu");
    }

    #[test]
    fn a_real_shortcut_with_only_an_item_list_is_read_from_it() {
        let link = parse(&fixture("OrcaSlicer.lnk"), &no_env).expect("a shell link");
        assert_eq!(
            link.target.as_deref(),
            Some(r"C:\Program Files\OrcaSlicer\orca-slicer.exe"),
            "NSIS writes no link information, only the item list"
        );
    }

    #[test]
    fn an_item_list_that_is_not_a_file_path_names_nothing() {
        // A root that is not This PC: a control panel item.
        let mut list = vec![20, 0, 0x1F, 0x50];
        list.extend([0xAA; 16]);
        list.extend([0, 0]);
        assert_eq!(id_list_path(&list), None);
        assert_eq!(id_list_path(&[]), None);
        assert_eq!(id_list_path(&[5, 0, 1]), None, "cut short");
    }

    #[test]
    fn the_unicode_path_is_preferred_to_the_ansi_one() {
        let bytes = Builder::new()
            .local_path(
                r"C:\Program Files\M?l\app.exe",
                Some(r"C:\Program Files\Maël\app.exe"),
            )
            .build();
        assert_eq!(
            parse(&bytes, &no_env).unwrap().target.as_deref(),
            Some(r"C:\Program Files\Maël\app.exe")
        );
    }

    #[test]
    fn an_ansi_link_is_read_too() {
        let bytes = Builder::new()
            .ansi()
            .id_list(vec![0, 0])
            .local_path(r"C:\Tools\x.exe", None)
            .string(HAS_NAME, "A comment")
            .string(HAS_ARGUMENTS, "--fast")
            .build();
        let link = parse(&bytes, &no_env).unwrap();
        assert_eq!(link.target.as_deref(), Some(r"C:\Tools\x.exe"));
        assert_eq!(link.arguments, "--fast");
    }

    #[test]
    fn a_link_with_only_an_environment_block_is_expanded() {
        let bytes = Builder::new()
            .id_list(vec![0, 0])
            .environment(r"%ProgramFiles%\App\app.exe")
            .build();
        let env = |text: &str| {
            expand_with(text, &|name: &str| {
                name.eq_ignore_ascii_case("programfiles")
                    .then(|| r"C:\Program Files".to_string())
            })
        };
        assert_eq!(
            parse(&bytes, &env).unwrap().target.as_deref(),
            Some(r"C:\Program Files\App\app.exe")
        );
        assert_eq!(
            parse(&bytes, &no_env).unwrap().target,
            None,
            "a variable that is not set is no target, not a broken one"
        );
    }

    #[test]
    fn a_link_with_no_file_target_has_none() {
        let bytes = Builder::new().id_list(vec![0, 0]).build();
        assert_eq!(parse(&bytes, &no_env).unwrap().target, None);
    }

    #[test]
    fn rubbish_is_refused_rather_than_panicked_over() {
        assert_eq!(parse(&[], &no_env), None);
        assert_eq!(parse(b"not a link at all", &no_env), None);
        let good = Builder::new()
            .local_path(r"C:\Tools\x.exe", Some(r"C:\Tools\x.exe"))
            .string(HAS_ARGUMENTS, "--fast")
            .build();
        for cut in 0..good.len() {
            let _ = parse(&good[..cut], &no_env);
        }
        let mut huge = good.clone();
        huge[HEADER_SIZE..HEADER_SIZE + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let _ = parse(&huge, &no_env);
    }

    #[test]
    fn expansion_replaces_every_variable_or_gives_up() {
        let env = |n: &str| (n == "A").then(|| "x".to_string());
        assert_eq!(expand_with("%A%\\%A%", &env).as_deref(), Some("x\\x"));
        assert_eq!(
            expand_with("no variables", &env).as_deref(),
            Some("no variables")
        );
        assert_eq!(expand_with("%B%\\y", &env), None);
        assert_eq!(
            expand_with("100%% sure", &env).as_deref(),
            Some("100% sure")
        );
        assert_eq!(expand_with("%unclosed", &env), None);
    }

    #[test]
    fn inside_compares_as_windows_does() {
        assert!(inside(
            r"C:\Program Files\Obsidian\Obsidian.exe",
            r"c:\program files\obsidian\"
        ));
        assert!(inside("C:/Apps/X/bin/x.exe", r"C:\Apps\X"));
        assert!(!inside(
            r"C:\Program Files\Obsidian2\o.exe",
            r"C:\Program Files\Obsidian"
        ));
        assert!(!inside(
            r"C:\Program Files\Obsidian",
            r"C:\Program Files\Obsidian"
        ));
        assert!(!inside(r"C:\x.exe", ""));
    }

    #[test]
    fn a_shared_directory_is_too_wide_to_name_an_application() {
        for wide in [
            r"C:\",
            r"C:\Windows\System32",
            r"C:\WINDOWS\SysWOW64",
            r"C:\Program Files",
            r"C:\Program Files (x86)\",
            r"C:\ProgramData",
            r"C:\ProgramData\Microsoft\Windows\Start Menu",
            r"C:\Users\me",
            r"C:\Users\me\AppData\Local",
            r"C:\Users\me\AppData\Local\Programs",
            r"C:\Users\me\AppData\Roaming",
            r"\\server\share\app",
            r"app",
        ] {
            assert!(!specific(wide), "{wide}");
        }
        for narrow in [
            r"C:\Program Files\Obsidian",
            r"C:\Program Files (x86)\Mozilla Firefox",
            r"C:\Users\me\AppData\Local\Discord",
            r"C:\Users\me\AppData\Local\Programs\Microsoft VS Code",
            r"C:\Python314",
            r"D:\Games\Thing",
        ] {
            assert!(specific(narrow), "{narrow}");
        }
    }

    fn entry(name: &str, uninstall: Option<&str>, location: Option<&str>) -> RawEntry {
        RawEntry {
            hive: arp::Hive::Machine,
            key_name: name.to_string(),
            display_name: Some(name.to_string()),
            display_version: None,
            publisher: None,
            install_location: location.map(str::to_string),
            uninstall_string: uninstall.map(str::to_string),
            quiet_uninstall_string: None,
            windows_installer: None,
            display_icon: None,
            system_component: None,
            parent_key_name: None,
            release_type: None,
            estimated_size: None,
            url_info_about: None,
        }
    }

    fn lnk(path: &str, target: &str, arguments: &str) -> Shortcut {
        Shortcut {
            path: PathBuf::from(path),
            target: target.to_string(),
            arguments: arguments.to_string(),
        }
    }

    /// A Start Menu shaped like the reference machine's, where each rule
    /// below has a shortcut it fires on.
    fn menu() -> Vec<Shortcut> {
        vec![
            lnk(
                r"M\Obsidian.lnk",
                r"C:\Program Files\Obsidian\Obsidian.exe",
                "",
            ),
            lnk(
                r"M\WizTree\WizTree.lnk",
                r"C:\Program Files\WizTree\WizTree64.exe",
                "",
            ),
            lnk(
                r"M\WizTree\Uninstall WizTree.lnk",
                r"C:\Program Files\WizTree\unins000.exe",
                "",
            ),
            lnk(
                r"M\Tesseract-OCR\Uninstall.lnk",
                r"C:\Program Files\Tesseract-OCR\tesseract-uninstall.exe",
                "",
            ),
            lnk(
                r"M\Tesseract-OCR\ReadMe.lnk",
                r"C:\Program Files\Tesseract-OCR\README.txt",
                "",
            ),
            lnk(
                r"U\Discord Inc\Discord.lnk",
                r"C:\Users\me\AppData\Local\Discord\Update.exe",
                "--processStart Discord.exe",
            ),
            lnk(
                r"U\Zoom\Zoom Workplace.lnk",
                r"C:\Users\me\AppData\Roaming\Zoom\bin\Zoom.exe",
                "",
            ),
            lnk(
                r"U\Zoom\Uninstall Zoom Workplace.lnk",
                r"C:\Users\me\AppData\Roaming\Zoom\uninstall\Installer.exe",
                "/uninstall",
            ),
            lnk(
                r"M\Revo Uninstaller\Revo Uninstaller.lnk",
                r"C:\Program Files\VS Revo Group\Revo Uninstaller\RevoUnin.exe",
                "",
            ),
            lnk(
                r"M\Revo Uninstaller\Uninstall Revo Uninstaller.lnk",
                r"C:\Program Files\VS Revo Group\Revo Uninstaller\unins000.exe",
                "",
            ),
            lnk(
                r"M\IObit Uninstaller.lnk",
                r"C:\Program Files (x86)\IObit\IObit Uninstaller\IObitUninstaller.exe",
                "",
            ),
            lnk(
                r"M\Control Panel.lnk",
                r"C:\Windows\System32\control.exe",
                "",
            ),
            lnk(
                r"M\Thing\Thing.lnk",
                r"C:\Program Files\Thing\thing.exe",
                "",
            ),
            lnk(
                r"M\Thing\Thing (safe mode).lnk",
                r"C:\Program Files\Thing\thing.exe",
                "--safe",
            ),
            lnk(r"M\Thing.lnk", r"C:\Program Files\Thing\thing.exe", ""),
        ]
    }

    fn opens(e: &RawEntry) -> Option<String> {
        let menu = menu();
        launch_for_entries(&[e], &menu).map(|l| match l {
            crate::launch::Launch::Shortcut(p) => p.to_string_lossy().into_owned(),
            other => panic!("not a shortcut: {other:?}"),
        })
    }

    #[test]
    fn the_shortcut_inside_the_uninstaller_s_directory_is_opened() {
        let e = entry(
            "Obsidian",
            Some(r#""C:\Program Files\Obsidian\Uninstall Obsidian.exe" /currentuser"#),
            None,
        );
        assert_eq!(opens(&e).as_deref(), Some(r"M\Obsidian.lnk"));
    }

    #[test]
    fn the_uninstaller_s_own_shortcut_is_never_opened() {
        let wiz = entry(
            "WizTree",
            Some(r#""C:\Program Files\WizTree\unins000.exe""#),
            None,
        );
        assert_eq!(opens(&wiz).as_deref(), Some(r"M\WizTree\WizTree.lnk"));
        let revo = entry(
            "Revo Uninstaller 2.5",
            Some(r#""C:\Program Files\VS Revo Group\Revo Uninstaller\unins000.exe""#),
            None,
        );
        assert_eq!(
            opens(&revo).as_deref(),
            Some(r"M\Revo Uninstaller\Revo Uninstaller.lnk"),
            "an application about uninstalling is still opened"
        );
        let iobit = entry(
            "IObit Uninstaller",
            Some(r#""C:\Program Files (x86)\IObit\IObit Uninstaller\unins000.exe""#),
            None,
        );
        assert_eq!(opens(&iobit).as_deref(), Some(r"M\IObit Uninstaller.lnk"));
    }

    #[test]
    fn an_application_whose_only_shortcuts_are_an_uninstaller_and_a_document_has_none() {
        let e = entry(
            "Tesseract-OCR",
            Some(r"C:\Program Files\Tesseract-OCR\tesseract-uninstall.exe"),
            None,
        );
        assert_eq!(opens(&e), None);
    }

    #[test]
    fn a_squirrel_application_opens_through_its_updater_but_not_its_uninstall() {
        let e = entry(
            "Discord",
            Some(r#""C:\Users\me\AppData\Local\Discord\Update.exe" --uninstall"#),
            None,
        );
        assert_eq!(opens(&e).as_deref(), Some(r"U\Discord Inc\Discord.lnk"));
        let only_uninstall = vec![lnk(
            r"U\Uninstall Discord.lnk",
            r"C:\Users\me\AppData\Local\Discord\Update.exe",
            "--uninstall",
        )];
        assert_eq!(launch_for_entries(&[&e], &only_uninstall), None);
    }

    #[test]
    fn an_uninstaller_in_a_folder_of_its_own_looks_one_level_up() {
        let e = entry(
            "Zoom Workplace",
            Some(r#""C:\Users\me\AppData\Roaming\Zoom\uninstall\Installer.exe" /uninstall"#),
            None,
        );
        assert_eq!(opens(&e).as_deref(), Some(r"U\Zoom\Zoom Workplace.lnk"));
        let elsewhere = entry(
            "Updater",
            Some(r#""C:\Program Files\Updater\Updater.exe" /remove"#),
            None,
        );
        assert_eq!(
            search_dir(&elsewhere).as_deref(),
            Some(r"C:\Program Files\Updater"),
            "a folder with any other name is the install directory itself"
        );
    }

    #[test]
    fn an_uninstaller_in_a_shared_directory_opens_nothing() {
        let e = entry(
            "Some driver",
            Some(r"C:\Windows\System32\rundll32.exe setupapi,InstallHinfSection"),
            None,
        );
        assert_eq!(opens(&e), None, "control.exe is in System32 too");
    }

    #[test]
    fn an_msi_entry_uses_its_install_location() {
        let e = entry(
            "Thing",
            Some("MsiExec.exe /X{11111111-2222-3333-4444-555555555555}"),
            Some(r"C:\Program Files\Thing\"),
        );
        assert_eq!(opens(&e).as_deref(), Some(r"M\Thing.lnk"));
        let without = entry(
            "Thing",
            Some("MsiExec.exe /X{11111111-2222-3333-4444-555555555555}"),
            None,
        );
        assert_eq!(
            opens(&without),
            None,
            "msiexec's own directory is never used"
        );
    }

    #[test]
    fn the_shortcut_named_like_the_application_and_nearest_the_top_wins() {
        let e = entry(
            "Thing",
            Some(r#""C:\Program Files\Thing\uninstall.exe""#),
            None,
        );
        assert_eq!(opens(&e).as_deref(), Some(r"M\Thing.lnk"));
        let mut menu = menu();
        menu.retain(|s| s.path != Path::new(r"M\Thing.lnk"));
        assert_eq!(
            launch_for_entries(&[&e], &menu),
            Some(crate::launch::Launch::Shortcut(PathBuf::from(
                r"M\Thing\Thing.lnk"
            ))),
            "the exact name before the safe mode one"
        );
    }

    #[test]
    fn an_unquoted_uninstaller_with_spaces_is_read_as_windows_reads_it() {
        let steam = entry(
            "Steam",
            Some(r"C:\Program Files (x86)\Steam\uninstall.exe"),
            None,
        );
        assert_eq!(
            uninstaller(&steam),
            Some((
                r"C:\Program Files (x86)\Steam\uninstall.exe".to_string(),
                vec![]
            ))
        );
        assert_eq!(
            search_dir(&steam).as_deref(),
            Some(r"C:\Program Files (x86)\Steam")
        );
        let menu = vec![lnk(
            r"M\Steam\Steam.lnk",
            r"C:\Program Files (x86)\Steam\steam.exe",
            "",
        )];
        assert!(launch_for_entries(&[&steam], &menu).is_some());
        let with_args = entry("X", Some(r"C:\Program Files\X\x.exe /uninstall /S"), None);
        assert_eq!(
            uninstaller(&with_args),
            Some((
                r"C:\Program Files\X\x.exe".to_string(),
                vec!["/uninstall".to_string(), "/S".to_string()]
            ))
        );
        let not_exe = entry("Y", Some(r"C:\Program Files\Y.exe.d\run.exe"), None);
        assert_eq!(
            uninstaller(&not_exe).map(|(p, _)| p).as_deref(),
            Some(r"C:\Program Files\Y.exe.d\run.exe"),
            "an .exe in the middle of a folder name is not the end of the program"
        );
    }

    #[test]
    fn a_game_whose_uninstaller_is_its_launcher_does_not_open_the_launcher() {
        let game = entry(
            "World of Tanks",
            Some(r#""C:\Program Files (x86)\Steam\steam.exe" steam://uninstall/1407200"#),
            None,
        );
        let menu = vec![lnk(
            r"M\Steam\Steam.lnk",
            r"C:\Program Files (x86)\Steam\steam.exe",
            "",
        )];
        assert_eq!(launch_for_entries(&[&game], &menu), None);
    }

    #[test]
    fn an_install_location_holding_the_uninstaller_is_the_wider_home() {
        let chrome = entry(
            "Google Chrome",
            Some(
                r#""C:\Program Files\Google\Chrome\Application\157.0.8103.79\Installer\setup.exe" --uninstall --system-level"#,
            ),
            Some(r"C:\Program Files\Google\Chrome\Application"),
        );
        assert_eq!(
            search_dir(&chrome).as_deref(),
            Some(r"C:\Program Files\Google\Chrome\Application")
        );
        let menu = vec![lnk(
            r"M\Google Chrome.lnk",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            "--source-shortcut-location=start-menu",
        )];
        assert!(launch_for_entries(&[&chrome], &menu).is_some());
        let unrelated = entry(
            "Thing",
            Some(r#""C:\Program Files\Thing\uninstall.exe""#),
            Some(r"C:\Program Files"),
        );
        assert_eq!(
            search_dir(&unrelated).as_deref(),
            Some(r"C:\Program Files\Thing"),
            "a location too wide to name an application confirms nothing"
        );
    }

    #[test]
    fn a_shortcut_with_arguments_opens_only_when_its_name_fits() {
        let java = entry(
            "Java 8 Update 202 (64-bit)",
            Some("MsiExec.exe /X{26A24AE4-039D-4CA4-87B4-2F64180202F0}"),
            Some(r"C:\Program Files\Java\jre1.8.0_202\"),
        );
        let nmap = entry(
            "Nmap 7.99",
            Some(r#""C:\Program Files (x86)\Nmap\uninstall.exe""#),
            None,
        );
        let menu = vec![
            lnk(
                r"M\Java\About Java.lnk",
                r"C:\Program Files\Java\jre1.8.0_202\bin\javacpl.exe",
                "-tab about",
            ),
            lnk(
                r"M\Nmap\Nmap - Zenmap GUI.lnk",
                r"C:\Program Files (x86)\Nmap\zenmap\bin\pythonw.exe",
                "-c run()",
            ),
        ];
        assert_eq!(launch_for_entries(&[&java], &menu), None);
        assert_eq!(
            launch_for_entries(&[&nmap], &menu),
            Some(crate::launch::Launch::Shortcut(PathBuf::from(
                r"M\Nmap\Nmap - Zenmap GUI.lnk"
            ))),
            "the same first word fits"
        );
    }

    #[test]
    fn the_plain_shortcut_comes_before_one_with_arguments() {
        let vlc = entry(
            "VLC Nightly",
            Some(r#""C:\Program Files\VideoLAN\VLC\uninstall.exe""#),
            None,
        );
        let menu = vec![
            lnk(
                r"M\VideoLAN\VLC media player - reset preferences and cache files.lnk",
                r"C:\Program Files\VideoLAN\VLC\vlc.exe",
                "--reset-config",
            ),
            lnk(
                r"M\VideoLAN\VLC media player.lnk",
                r"C:\Program Files\VideoLAN\VLC\vlc.exe",
                "",
            ),
        ];
        assert_eq!(
            launch_for_entries(&[&vlc], &menu),
            Some(crate::launch::Launch::Shortcut(PathBuf::from(
                r"M\VideoLAN\VLC media player.lnk"
            )))
        );
        let ce = entry(
            "Cheat Engine 7.6",
            Some(r#""C:\Program Files\Cheat Engine\unins000.exe""#),
            None,
        );
        let menu = vec![
            lnk(
                r"M\Cheat Engine\Cheat Engine (32-bit).lnk",
                r"C:\Program Files\Cheat Engine\cheatengine-i386.exe",
                "",
            ),
            lnk(
                r"M\Cheat Engine\Cheat Engine.lnk",
                r"C:\Program Files\Cheat Engine\Cheat Engine.exe",
                "",
            ),
        ];
        assert_eq!(
            launch_for_entries(&[&ce], &menu),
            Some(crate::launch::Launch::Shortcut(PathBuf::from(
                r"M\Cheat Engine\Cheat Engine.lnk"
            ))),
            "a name the application's contains fits better than one sharing its first word"
        );
    }

    #[test]
    fn an_entry_with_no_uninstaller_and_no_location_opens_nothing() {
        assert_eq!(opens(&entry("Ghost", None, None)), None);
    }

    /// Every shortcut in this machine's Start Menu is read, and the
    /// counts are printed for comparison with what the shell resolves.
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn live_start_menu_reads_this_machine() {
        let dirs = start_menu_dirs();
        assert!(!dirs.is_empty());
        let shortcuts = read_dirs(&dirs, &expand_environment);
        for s in &shortcuts {
            println!("{} -> {} {}", s.path.display(), s.target, s.arguments);
        }
        println!("{} shortcuts name a file", shortcuts.len());
        assert!(!shortcuts.is_empty());
    }

    /// What Open would open for everything installed on this machine,
    /// source by source. Resolves only: nothing is started.
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn live_installed_applications_resolve_on_this_machine() {
        let store = crate::Store::detect();
        for source in &store.sources {
            let Ok(installed) = source.installed() else {
                println!("{:?}: no installed list", source.kind());
                continue;
            };
            let mut opened = 0;
            for p in &installed {
                match source.launcher(&p.id) {
                    Some(l) => {
                        opened += 1;
                        println!("{:?}  {}  ->  {}", source.kind(), p.name, l.describe());
                    }
                    None => println!("{:?}  {}  ->  nothing", source.kind(), p.name),
                }
            }
            println!(
                "{:?}: {opened} of {} installed have something to open",
                source.kind(),
                installed.len()
            );
        }
    }
}
