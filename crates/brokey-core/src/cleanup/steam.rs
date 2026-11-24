//! Where a Steam game lives, read from Steam's own files.
//!
//! A game's Add/Remove Programs entry is `Steam App <id>`, and its uninstall
//! string runs `steam.exe`: the uninstaller's folder is all of Steam, and
//! every other game says the same. None of that is the game's home. The game
//! lives in `<library>\steamapps\common\<installdir>`, where `<installdir>`
//! is in `<library>\steamapps\appmanifest_<id>.acf` and the libraries are
//! listed in `<Steam>\steamapps\libraryfolders.vdf`. Steam deletes the
//! manifest when it uninstalls the game, so this is read with the hints,
//! before the uninstall, and by the guard for the games still installed.
//!
//! Beside the game folder Steam keeps two folders of the game's own, named
//! by its id in the same library: `steamapps\shadercache\<id>` and
//! `steamapps\workshop\content\<id>`. Both are there on Windows; Proton's
//! `compatdata` is not.
//!
//! Everything here is a pure function of the text it is handed.

use std::path::{Path, PathBuf};

/// Reads a small text file, or answers `None`.
pub type Read<'a> = &'a dyn Fn(&Path) -> Option<String>;

/// The largest file read as one of Steam's: these are a few kilobytes, and
/// anything far larger is not what it says it is.
const LARGEST: u64 = 4 * 1024 * 1024;

/// A small text file from the disk, for [`Read`]. What is not valid UTF-8
/// is read with replacement characters, which no path parsed here holds.
pub fn read_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > LARGEST {
        return None;
    }
    std::fs::read(path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// The game's id, for an uninstall key named `Steam App <id>`.
pub fn app_id(key_name: &str) -> Option<&str> {
    let id = key_name.trim().strip_prefix("Steam App ")?;
    (!id.is_empty() && id.len() <= 12 && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

/// A KeyValues document, as Steam writes `.vdf` and `.acf` files.
#[derive(Debug, PartialEq)]
enum Value {
    Text(String),
    Block(Vec<(String, Value)>),
}

/// The quoted strings and braces of a KeyValues document: `Some("{")` for
/// an opening brace, `None` for a closing one, and a string with a `"` in
/// front of it. Escapes are `\\` and `\"`; anything else after a backslash
/// is kept as written.
fn tokens(text: &str) -> Vec<Option<String>> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => out.push(Some("{".to_string())),
            '}' => out.push(None),
            '"' => {
                let mut word = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.peek() {
                            Some('\\') | Some('"') => word.push(chars.next().unwrap_or('\\')),
                            _ => word.push('\\'),
                        },
                        c => word.push(c),
                    }
                }
                out.push(Some(format!("\"{word}")));
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Parses a block's members from `at` until its closing brace.
fn block(tokens: &[Option<String>], at: &mut usize) -> Vec<(String, Value)> {
    let mut members = Vec::new();
    while *at < tokens.len() {
        let Some(key) = &tokens[*at] else {
            *at += 1;
            return members;
        };
        *at += 1;
        let Some(key) = key.strip_prefix('"') else {
            continue;
        };
        match tokens.get(*at) {
            Some(Some(next)) if next == "{" => {
                *at += 1;
                members.push((key.to_string(), Value::Block(block(tokens, at))));
            }
            Some(Some(next)) => {
                *at += 1;
                if let Some(text) = next.strip_prefix('"') {
                    members.push((key.to_string(), Value::Text(text.to_string())));
                }
            }
            _ => {}
        }
    }
    members
}

fn parse(text: &str) -> Vec<(String, Value)> {
    let tokens = tokens(text);
    block(&tokens, &mut 0)
}

fn member<'v>(members: &'v [(String, Value)], name: &str) -> Option<&'v Value> {
    members
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
}

/// The library folders `libraryfolders.vdf` lists, in its order.
pub fn libraries(vdf: &str) -> Vec<String> {
    let doc = parse(vdf);
    let Some(Value::Block(folders)) = member(&doc, "libraryfolders") else {
        return Vec::new();
    };
    folders
        .iter()
        .filter_map(|(_, folder)| match folder {
            // The current shape: a block with a `path`.
            Value::Block(fields) => match member(fields, "path") {
                Some(Value::Text(path)) => Some(path.clone()),
                _ => None,
            },
            // The shape before 2021: `"1" "D:\\SteamLibrary"`.
            Value::Text(path) if path.contains(['\\', '/', ':']) => Some(path.clone()),
            Value::Text(_) => None,
        })
        .filter(|p| !p.trim().is_empty())
        .collect()
}

/// The `installdir` an app manifest names, when it is one folder name.
pub fn install_dir(acf: &str) -> Option<String> {
    let doc = parse(acf);
    let Some(Value::Block(state)) = member(&doc, "AppState") else {
        return None;
    };
    let Some(Value::Text(dir)) = member(state, "installdir") else {
        return None;
    };
    let dir = dir.trim();
    // One name inside `common`, never a way out of it or the folder itself.
    let one_name = !dir.is_empty()
        && dir != "."
        && dir != ".."
        && !dir.contains(['\\', '/', ':'])
        && !dir.ends_with(['.', ' ']);
    one_name.then(|| dir.to_string())
}

/// Where one game lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Game {
    /// `<library>\steamapps\common\<installdir>`.
    pub folder: PathBuf,
    /// The folders beside it that Steam names by the game's id.
    pub by_id: Vec<PathBuf>,
}

fn under(dir: &str, tail: &str) -> String {
    format!(r"{}\{tail}", dir.trim_end_matches(['\\', '/']))
}

/// The game `id`, found through the Steam installed at `steam`: the first
/// library whose `appmanifest_<id>.acf` names an install folder. `None`
/// when no library has the manifest.
pub fn game(steam: &str, id: &str, read: Read) -> Option<Game> {
    let steam = steam.trim();
    let mut libraries = vec![steam.to_string()];
    if let Some(vdf) = read(Path::new(&under(steam, r"steamapps\libraryfolders.vdf"))) {
        for library in self::libraries(&vdf) {
            let same = |l: &String| {
                l.trim_end_matches(['\\', '/'])
                    .eq_ignore_ascii_case(library.trim_end_matches(['\\', '/']))
            };
            if !libraries.iter().any(same) {
                libraries.push(library);
            }
        }
    }
    libraries.iter().find_map(|library| {
        let steamapps = under(library, "steamapps");
        let manifest = under(&steamapps, &format!("appmanifest_{id}.acf"));
        let dir = install_dir(&read(Path::new(&manifest))?)?;
        Some(Game {
            folder: PathBuf::from(under(&steamapps, &format!(r"common\{dir}"))),
            by_id: vec![
                PathBuf::from(under(&steamapps, &format!(r"shadercache\{id}"))),
                PathBuf::from(under(&steamapps, &format!(r"workshop\content\{id}"))),
            ],
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const LIBRARIES: &str = r#""libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
		"label"		""
		"apps"
		{
			"620"		"12000"
		}
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"
		"label"		"Games \"fast\" drive"
		"apps"
		{
			"1422450"		"39049299035"
		}
	}
}
"#;

    const DEADLOCK: &str = r#""AppState"
{
	"appid"		"1422450"
	"LauncherPath"		"C:\\Program Files (x86)\\Steam\\steam.exe"
	"name"		"Deadlock"
	"installdir"		"Deadlock"
	"InstalledDepots"
	{
		"1422456"
		{
			"manifest"		"5770000000000000000"
		}
	}
}
"#;

    fn files(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(p, t)| (p.to_lowercase(), t.to_string()))
            .collect()
    }

    #[test]
    fn the_id_is_read_only_from_a_steam_app_key() {
        assert_eq!(app_id("Steam App 1422450"), Some("1422450"));
        assert_eq!(app_id("Steam"), None);
        assert_eq!(app_id("Steam App "), None);
        assert_eq!(app_id(r"Steam App 1\..\2"), None);
        assert_eq!(app_id("Steam Application 10"), None);
    }

    #[test]
    fn libraries_are_read_in_both_shapes() {
        assert_eq!(
            libraries(LIBRARIES),
            vec![
                r"C:\Program Files (x86)\Steam".to_string(),
                r"D:\SteamLibrary".to_string()
            ]
        );
        let old = r#""LibraryFolders" { "TimeNextStatsReport" "1" "1" "E:\\Games\\Steam" }"#;
        assert_eq!(libraries(old), vec![r"E:\Games\Steam".to_string()]);
        assert!(libraries("not a document").is_empty());
    }

    #[test]
    fn the_install_dir_is_one_name_or_nothing() {
        assert_eq!(install_dir(DEADLOCK).as_deref(), Some("Deadlock"));
        for bad in ["", "..", ".", r"..\..\Steam", "C:", "a/b", "Deadlock."] {
            let acf = format!(r#""AppState" {{ "installdir" "{bad}" }}"#);
            assert_eq!(install_dir(&acf), None, "{bad:?}");
        }
    }

    #[test]
    fn a_game_is_found_in_the_library_that_has_its_manifest() {
        let disk = files(&[
            (
                r"C:\Program Files (x86)\Steam\steamapps\libraryfolders.vdf",
                LIBRARIES,
            ),
            (
                r"D:\SteamLibrary\steamapps\appmanifest_1422450.acf",
                DEADLOCK,
            ),
        ]);
        let read = |p: &Path| disk.get(&p.to_string_lossy().to_lowercase()).cloned();
        let game = game(r"C:\Program Files (x86)\Steam", "1422450", &read).expect("found");
        assert_eq!(
            game.folder,
            PathBuf::from(r"D:\SteamLibrary\steamapps\common\Deadlock")
        );
        assert_eq!(
            game.by_id,
            vec![
                PathBuf::from(r"D:\SteamLibrary\steamapps\shadercache\1422450"),
                PathBuf::from(r"D:\SteamLibrary\steamapps\workshop\content\1422450"),
            ]
        );
    }

    #[test]
    fn a_game_without_a_manifest_is_not_found_and_never_falls_back_to_steam() {
        let disk = files(&[(
            r"C:\Program Files (x86)\Steam\steamapps\libraryfolders.vdf",
            LIBRARIES,
        )]);
        let read = |p: &Path| disk.get(&p.to_string_lossy().to_lowercase()).cloned();
        assert_eq!(
            game(r"C:\Program Files (x86)\Steam", "1422450", &read),
            None
        );
        // Without the library list, Steam's own library is still looked in.
        let disk = files(&[(
            r"C:\Program Files (x86)\Steam\steamapps\appmanifest_1422450.acf",
            DEADLOCK,
        )]);
        let read = |p: &Path| disk.get(&p.to_string_lossy().to_lowercase()).cloned();
        assert_eq!(
            game(r"C:\Program Files (x86)\Steam", "1422450", &read).map(|g| g.folder),
            Some(PathBuf::from(
                r"C:\Program Files (x86)\Steam\steamapps\common\Deadlock"
            ))
        );
    }
}
