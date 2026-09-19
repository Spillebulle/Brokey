//! pacman's databases, read directly.
//!
//! The local database is a directory per installed package under
//! `/var/lib/pacman/local/<name>-<version>/` holding a `desc` file. A sync
//! database (`/var/lib/pacman/sync/<repo>.db`) is a tar archive, gzip or
//! zstd compressed, of the same `<name>-<version>/desc` layout. `desc` is a
//! sequence of `%KEY%` lines each followed by its values, blank-line
//! separated. That is the whole format, and reading it here rather than
//! through libalpm is what lets one binary run on every distribution.

use crate::{Error, Result};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// One `desc` file, keys as written (`NAME`, `VERSION`, `DESC`, …).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Desc {
    pub fields: HashMap<String, Vec<String>>,
}

impl Desc {
    pub fn parse(text: &str) -> Desc {
        let mut fields: HashMap<String, Vec<String>> = HashMap::new();
        let mut key: Option<String> = None;
        for line in text.lines() {
            if line.len() >= 2 && line.starts_with('%') && line.ends_with('%') {
                key = Some(line[1..line.len() - 1].to_string());
                fields.entry(key.clone().unwrap()).or_default();
            } else if line.is_empty() {
                continue;
            } else if let Some(k) = &key {
                fields.get_mut(k).unwrap().push(line.to_string());
            }
        }
        Desc { fields }
    }

    pub fn first(&self, key: &str) -> Option<&str> {
        self.fields
            .get(key)
            .and_then(|v| v.first())
            .map(String::as_str)
    }

    pub fn all(&self, key: &str) -> &[String] {
        self.fields.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn u64(&self, key: &str) -> Option<u64> {
        self.first(key).and_then(|s| s.parse().ok())
    }

    pub fn i64(&self, key: &str) -> Option<i64> {
        self.first(key).and_then(|s| s.parse().ok())
    }

    pub fn name(&self) -> &str {
        self.first("NAME").unwrap_or("")
    }

    pub fn version(&self) -> &str {
        self.first("VERSION").unwrap_or("")
    }
}

/// The installed packages, keyed by name.
#[derive(Clone, Debug, Default)]
pub struct LocalDb {
    pub packages: HashMap<String, Desc>,
}

impl LocalDb {
    pub const DEFAULT_PATH: &str = "/var/lib/pacman/local";

    pub fn load(dir: &Path) -> Result<LocalDb> {
        let mut packages = HashMap::new();
        let entries = std::fs::read_dir(dir)
            .map_err(|e| Error::new(format!("could not read {}: {e}", dir.display())))?;
        for entry in entries.flatten() {
            let path = entry.path().join("desc");
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let desc = Desc::parse(&text);
            if !desc.name().is_empty() {
                packages.insert(desc.name().to_string(), desc);
            }
        }
        Ok(LocalDb { packages })
    }

    pub fn get(&self, name: &str) -> Option<&Desc> {
        self.packages.get(name)
    }
}

/// The files pacman recorded for one installed package, as written in its
/// `files` entry (`usr/bin/steam`, no leading slash). The directory is found
/// by the name in `desc`, not by splitting the directory name, because a
/// package name may itself contain hyphens and digits (`lib32-gcc-libs`).
/// `None` when the package is not installed.
pub fn local_files(local_dir: &Path, name: &str) -> Option<Vec<String>> {
    let prefix = format!("{name}-");
    let entries = std::fs::read_dir(local_dir).ok()?;
    for entry in entries.flatten() {
        let dir_name = entry.file_name();
        if !dir_name.to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let dir = entry.path();
        let Ok(desc) = std::fs::read_to_string(dir.join("desc")) else {
            continue;
        };
        if Desc::parse(&desc).name() != name {
            continue;
        }
        let text = std::fs::read_to_string(dir.join("files")).unwrap_or_default();
        return Some(Desc::parse(&text).all("FILES").to_vec());
    }
    None
}

/// One repository's sync database.
#[derive(Clone, Debug, Default)]
pub struct SyncDb {
    pub repo: String,
    pub packages: HashMap<String, Desc>,
}

impl SyncDb {
    /// Read `<repo>.db`, whichever compression it uses.
    pub fn load(repo: &str, path: &Path) -> Result<SyncDb> {
        let bytes = std::fs::read(path)
            .map_err(|e| Error::new(format!("could not read {}: {e}", path.display())))?;
        Self::from_bytes(repo, &bytes)
    }

    pub fn from_bytes(repo: &str, bytes: &[u8]) -> Result<SyncDb> {
        let plain = decompress(bytes)?;
        let mut archive = tar::Archive::new(plain.as_slice());
        let mut packages = HashMap::new();
        let entries = archive
            .entries()
            .map_err(|e| Error::new(format!("{repo}.db is not a tar archive: {e}")))?;
        for entry in entries {
            let mut entry = entry.map_err(|e| Error::new(format!("{repo}.db: {e}")))?;
            let path = entry.path().map(|p| p.to_path_buf()).unwrap_or_default();
            if path.file_name().and_then(|f| f.to_str()) != Some("desc") {
                continue;
            }
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|e| Error::new(format!("{repo}.db: {e}")))?;
            let desc = Desc::parse(&text);
            if !desc.name().is_empty() {
                packages.insert(desc.name().to_string(), desc);
            }
        }
        Ok(SyncDb {
            repo: repo.to_string(),
            packages,
        })
    }

    pub fn get(&self, name: &str) -> Option<&Desc> {
        self.packages.get(name)
    }
}

/// gzip, zstd or already plain, judged by the magic bytes.
fn decompress(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes)
            .read_to_end(&mut out)
            .map_err(|e| Error::new(format!("gzip: {e}")))?;
        Ok(out)
    } else if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        let mut out = Vec::new();
        ruzstd::decoding::StreamingDecoder::new(bytes)
            .map_err(|e| Error::new(format!("zstd: {e}")))?
            .read_to_end(&mut out)
            .map_err(|e| Error::new(format!("zstd: {e}")))?;
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}

/// The repositories `pacman.conf` names, in order, with their database
/// paths. A file pulled in with `Include =` may define repositories of its
/// own (pacman.conf(5) allows it, and a `[chaotic-aur]` kept in
/// `/etc/pacman.d/` is a common shape), so each included file is scanned
/// for `[name]` headers in place, one level deep: an `Include` inside an
/// included file is not followed, as pacman does not follow it either.
/// `include` answers an `Include =` path (which may be a glob) with the
/// text of every file it names; [`read_includes`] is the real reader and
/// the tests hand it strings.
pub fn repos(
    conf: &str,
    sync_dir: &Path,
    include: &dyn Fn(&str) -> Vec<String>,
) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut push = |name: &str| {
        let name = name.trim();
        if name != "options" {
            out.push((name.to_string(), sync_dir.join(format!("{name}.db"))));
        }
    };
    for line in conf.lines() {
        let line = line.trim();
        if let Some(name) = section_header(line) {
            push(name);
        } else if let Some(path) = include_path(line) {
            for text in include(path) {
                for inner in text.lines() {
                    if let Some(name) = section_header(inner.trim()) {
                        push(name);
                    }
                }
            }
        }
    }
    out
}

/// `[name]` on a line of its own, with the name.
fn section_header(line: &str) -> Option<&str> {
    line.strip_prefix('[').and_then(|l| l.strip_suffix(']'))
}

/// The path of an `Include = path` line.
fn include_path(line: &str) -> Option<&str> {
    let (key, value) = line.split_once('=')?;
    (key.trim() == "Include").then(|| value.trim())
}

/// Read the files an `Include =` names. A glob (`/etc/pacman.d/*.conf`) is
/// matched on the file name within its directory, in name order, as
/// pacman's `glob()` does.
pub fn read_includes(pattern: &str) -> Vec<String> {
    let path = Path::new(pattern);
    if !pattern.contains(['*', '?', '[']) {
        return std::fs::read_to_string(path).into_iter().collect();
    }
    let (Some(dir), Some(file)) = (path.parent(), path.file_name().and_then(|f| f.to_str())) else {
        return Vec::new();
    };
    let Some(re) = glob_regex(file) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| re.is_match(f))
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect()
}

/// A shell glob as pacman.conf uses it (`*` and `?`; anything else is
/// itself) as an anchored regular expression, so `nvidia*` in `IgnorePkg`
/// matches what pacman would skip. `None` only if the pattern cannot be
/// compiled, which no pattern made of escaped text and `.*` is.
pub fn glob_regex(pattern: &str) -> Option<regex::Regex> {
    let mut expression = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => expression.push_str(".*"),
            '?' => expression.push('.'),
            other => expression.push_str(&regex::escape(other.encode_utf8(&mut [0; 4]))),
        }
    }
    expression.push('$');
    regex::Regex::new(&expression).ok()
}

/// Whether the name matches the glob, exactly or by pattern.
pub fn glob_matches(pattern: &str, name: &str) -> bool {
    if !pattern.contains(['*', '?']) {
        return pattern == name;
    }
    glob_regex(pattern).is_some_and(|re| re.is_match(name))
}

/// pacman's rule for a package name: lower-case letters, digits and
/// `@ . _ + -`, not starting with a hyphen or a dot. Both the pacman and the
/// AUR source check a plan's name against it, so nothing shaped like an
/// option ever reaches an argv.
pub fn is_package_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '.'])
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "@._+-".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEAM: &str = "%FILENAME%\nsteam-1.0.0.87-3-x86_64.pkg.tar.zst\n\n%NAME%\nsteam\n\n%BASE%\nsteam\n\n%VERSION%\n1.0.0.87-3\n\n%DESC%\nValve's digital software delivery system\n\n%CSIZE%\n20371240\n\n%ISIZE%\n20475439\n\n%URL%\nhttps://steampowered.com/\n\n%LICENSE%\nLicenseRef-steam-subscriber-agreement\n\n%ARCH%\nx86_64\n\n%BUILDDATE%\n1785004799\n\n%DEPENDS%\nbash\ncoreutils\n";

    #[test]
    fn a_desc_file_reads_back() {
        let d = Desc::parse(STEAM);
        assert_eq!(d.name(), "steam");
        assert_eq!(d.version(), "1.0.0.87-3");
        assert_eq!(d.u64("CSIZE"), Some(20_371_240));
        assert_eq!(d.all("DEPENDS"), ["bash", "coreutils"]);
        assert_eq!(d.first("MISSING"), None);
        assert!(d.all("MISSING").is_empty());
    }

    #[test]
    fn repos_come_out_in_pacman_conf_order() {
        let conf = "[options]\nHoldPkg = pacman\n\n[cachyos-v3]\nInclude = /etc/pacman.d/x\n[core]\nInclude = /etc/pacman.d/mirrorlist\n#[testing]\n[extra]\nInclude = /etc/pacman.d/mirrorlist\n";
        let none = |_: &str| Vec::new();
        let r = repos(conf, Path::new("/var/lib/pacman/sync"), &none);
        let names: Vec<&str> = r.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["cachyos-v3", "core", "extra"]);
        assert_eq!(r[1].1, PathBuf::from("/var/lib/pacman/sync/core.db"));
    }

    #[test]
    fn an_included_file_may_define_repositories_in_place() {
        let conf = "[options]\nInclude = /etc/pacman.d/options.conf\n\n[core]\nInclude = /etc/pacman.d/mirrorlist\nInclude = /etc/pacman.d/*.conf\n[extra]\nInclude = /etc/pacman.d/mirrorlist\n";
        let include = |path: &str| {
            match path {
            "/etc/pacman.d/mirrorlist" => vec!["Server = https://m.example/$repo/os/$arch\n".to_string()],
            "/etc/pacman.d/*.conf" => vec![
                "# A local repository kept in its own file.\n[custom]\nSigLevel = Optional TrustAll\nServer = file:///home/custompkgs\n#[commented-out]\nInclude = /etc/pacman.d/nested.conf\n".to_string(),
            ],
            "/etc/pacman.d/options.conf" => vec!["Color\nParallelDownloads = 5\n".to_string()],
            _ => panic!("{path} is read but the include is one level deep"),
        }
        };
        let r = repos(conf, Path::new("/var/lib/pacman/sync"), &include);
        let names: Vec<&str> = r.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["core", "custom", "extra"]);
        assert_eq!(r[1].1, PathBuf::from("/var/lib/pacman/sync/custom.db"));
    }

    #[test]
    fn globs_match_the_way_pacman_conf_means_them() {
        assert!(glob_matches("nvidia*", "nvidia-utils"));
        assert!(glob_matches("nvidia*", "nvidia"));
        assert!(!glob_matches("nvidia*", "lib32-nvidia-utils"));
        assert!(glob_matches("*nvidia*", "lib32-nvidia-utils"));
        assert!(glob_matches("linux-cachyos", "linux-cachyos"));
        assert!(!glob_matches("linux-cachyos", "linux-cachyos-headers"));
        assert!(glob_matches("python-pyqt?", "python-pyqt6"));
        assert!(!glob_matches("python-pyqt?", "python-pyqt66"));
        assert!(
            glob_matches("a.b+c", "a.b+c"),
            "regex characters are literal"
        );
        assert!(!glob_matches("a.b", "axb"));
        assert_eq!(glob_regex("nvidia*").unwrap().as_str(), "^nvidia.*$");
    }

    #[test]
    fn package_names_that_look_like_options_are_refused() {
        assert!(is_package_name("steam"));
        assert!(is_package_name("lib32-gcc-libs"));
        assert!(is_package_name("python-pyqt6.sip"));
        assert!(is_package_name("nvidia-open-dkms+"));
        assert!(!is_package_name("-Rs"));
        assert!(!is_package_name("--sudo=/tmp/x"));
        assert!(!is_package_name("Steam"));
        assert!(!is_package_name("a b"));
        assert!(!is_package_name(""));
        assert!(!is_package_name("--noconfirm"));
    }

    #[test]
    fn a_plain_tar_is_a_database_too() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(STEAM.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "steam-1.0.0.87-3/desc", STEAM.as_bytes())
            .unwrap();
        let bytes = builder.into_inner().unwrap();
        let db = SyncDb::from_bytes("multilib", &bytes).unwrap();
        assert_eq!(db.get("steam").unwrap().version(), "1.0.0.87-3");
    }

    #[test]
    #[ignore = "reads this machine's pacman databases"]
    fn live_this_machine_s_databases_read() {
        let local = LocalDb::load(Path::new(LocalDb::DEFAULT_PATH)).unwrap();
        assert!(local.packages.len() > 10);
        let conf = std::fs::read_to_string("/etc/pacman.conf").unwrap();
        for (repo, path) in repos(&conf, Path::new("/var/lib/pacman/sync"), &read_includes) {
            if path.exists() {
                let db = SyncDb::load(&repo, &path).unwrap();
                assert!(!db.packages.is_empty(), "{repo} is empty");
            }
        }
    }
    #[test]
    fn a_package_s_files_are_found_by_its_recorded_name() {
        let dir = tempfile::tempdir().unwrap();
        let write = |sub: &str, file: &str, text: &str| {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
            std::fs::write(dir.path().join(sub).join(file), text).unwrap();
        };
        write(
            "notepadqq-2.0.0-1",
            "desc",
            "%NAME%\nnotepadqq\n\n%VERSION%\n2.0.0-1\n",
        );
        write(
            "notepadqq-2.0.0-1",
            "files",
            "%FILES%\nusr/\nusr/bin/notepadqq\nusr/share/applications/notepadqq.desktop\n\n%BACKUP%\n",
        );
        write(
            "notepadqq-plugins-1.0-1",
            "desc",
            "%NAME%\nnotepadqq-plugins\n",
        );
        write(
            "notepadqq-plugins-1.0-1",
            "files",
            "%FILES%\nusr/lib/x.so\n",
        );
        let files = local_files(dir.path(), "notepadqq").unwrap();
        assert_eq!(
            files,
            [
                "usr/",
                "usr/bin/notepadqq",
                "usr/share/applications/notepadqq.desktop"
            ]
        );
        assert_eq!(
            local_files(dir.path(), "notepadqq-plugins").unwrap(),
            ["usr/lib/x.so"]
        );
        assert_eq!(local_files(dir.path(), "steam"), None);
    }
}
