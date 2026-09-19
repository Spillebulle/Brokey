//! AppStream catalogue: the parser, the index and icon resolution.
//!
//! A catalogue is a set of `<components>` XML files (gzip or plain) under a
//! directory such as `/usr/share/swcatalog/xml`, with cached icons beside it
//! under `icons/<origin>/<size>/`. The distribution's catalogue keys
//! components by `pkgname`; Flatpak's per-remote catalogue at
//! `/var/lib/flatpak/appstream/<remote>/<arch>/active/appstream.xml.gz`
//! keys them by `<bundle type="flatpak">app/<id>/<arch>/<branch></bundle>`.
//! One parser reads both.
//!
//! ## Where the files are read from
//!
//! [`Catalogue::load_system`] reads every `*.xml` and `*.xml.gz` under the
//! standard directories ([`Catalogue::system_xml_dirs`]) and resolves icons
//! against the `icons/` sibling of each. The environment variable
//! `BROKEY_APPSTREAM_DIR` adds extra roots: a colon-separated list of
//! directories that each contain `xml/` and `icons/`, read before the
//! standard ones. It exists so the store can be tested against an extracted
//! copy of a distribution's catalogue package on a machine that does not
//! have it installed:
//!
//! ```sh
//! BROKEY_APPSTREAM_DIR=/tmp/asd/usr/share/swcatalog brokey search steam
//! ```
//!
//! [`Catalogue::load_flatpak`] reads the per-remote catalogues of the system
//! installation and the user's, tagging every component with the remote's
//! name as its origin.
//!
//! ## What is kept
//!
//! Only untranslated elements count: anything carrying `xml:lang` is skipped
//! with its whole subtree, so a component whose translated `<description>`
//! comes before the English one still gets the English one. The description
//! keeps its markup (`p`, `ul`, `ol`, `li`, `em`, `code`), re-serialised
//! with whitespace runs collapsed, because the page renders exactly those
//! tags. Cached icons are resolved to the largest size that exists on disk;
//! a path that does not exist is never emitted, and a remote icon URL is the
//! fallback.
//!
//! ## Cost
//!
//! The Arch `extra` catalogue is 36 MB of XML with about 1500 components.
//! It is streamed through `quick-xml` (never a DOM) and parses in well under
//! a second. Because `Store::detect` repeats the load, each file's parse is
//! memoised in memory keyed on its path, size and modification time; nothing
//! is written to disk. Timing goes to `log::debug`.

mod icons;
mod index;
mod parser;

use crate::model::{Picture, Screenshot, SystemInfo};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Instant, SystemTime};

/// A component from a catalogue, reduced to what the store draws from it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Component {
    /// The component id, with a trailing `.desktop` stripped so the Arch
    /// catalogue's `com.valvesoftware.Steam.desktop` and Flathub's
    /// `com.valvesoftware.Steam` are the same key.
    pub id: String,
    /// Which catalogue it came from: "archlinux-arch-extra", "flathub",
    /// "debian-bookworm-main". The origin attribute of the file.
    pub origin: String,
    /// The distribution package that provides it, where the catalogue says.
    pub pkgname: Option<String>,
    /// The Flatpak ref, where the catalogue is a remote's:
    /// `app/org.gimp.GIMP/x86_64/stable`.
    pub bundle: Option<String>,
    pub name: String,
    pub summary: Option<String>,
    /// The `<description>` markup, inner XML kept as the page expects it.
    pub description: Option<String>,
    pub developer: Option<String>,
    pub licence: Option<String>,
    pub homepage: Option<String>,
    pub categories: Vec<String>,
    pub keywords: Vec<String>,
    /// The largest cached icon that exists on disk, else a remote icon URL.
    pub icon: Option<Picture>,
    pub screenshots: Vec<Screenshot>,
    /// `type="desktop-application"` (or console-application, web-application).
    pub is_app: bool,
    /// Newest `<release>` version and its timestamp, where present.
    pub latest_release: Option<(String, Option<i64>)>,
    /// The `type` attribute as written: "desktop-application", "addon",
    /// "font", "runtime", "codec". Sources map it to a `PackageKind`;
    /// `is_app` is the common case answered directly.
    pub component_type: String,
}

/// Every component this machine knows about, indexed three ways.
#[derive(Debug, Default)]
pub struct Catalogue {
    components: Vec<Component>,
    index: index::Index,
}

impl Catalogue {
    /// The distribution's catalogues: every XML under the standard
    /// directories, with icons resolved against their `icons/` siblings.
    /// Missing directories are simply absent; a file that will not parse is
    /// logged and skipped, never fatal.
    ///
    /// The standard directories are fixed by the AppStream specification and
    /// are the same on every distribution, so nothing in `SystemInfo` changes
    /// where this looks; the parameter is kept so a distribution-specific
    /// location can be added without changing the callers.
    pub fn load_system(_system: &SystemInfo) -> Arc<Catalogue> {
        let mut dirs = extra_roots(std::env::var("BROKEY_APPSTREAM_DIR").ok().as_deref())
            .into_iter()
            .map(|root| root.join("xml"))
            .collect::<Vec<_>>();
        dirs.extend(Self::system_xml_dirs());
        Arc::new(Self::load_roots(&dirs))
    }

    /// Read a list of XML directories in order, each with its `icons/`
    /// sibling. `load_system` is this over the standard list; tests call it
    /// with a fixture directory.
    pub fn load_roots(xml_dirs: &[PathBuf]) -> Catalogue {
        let started = Instant::now();
        let mut components = Vec::new();
        for xml_dir in xml_dirs {
            if !xml_dir.is_dir() {
                continue;
            }
            let icons_dir = xml_dir.parent().map(|p| p.join("icons"));
            let found = Self::load_dir(xml_dir, icons_dir.as_deref(), None);
            components.extend(found.components);
        }
        let catalogue = Self::from_components(components);
        log::debug!(
            "appstream: {} components from {} directories in {:?}",
            catalogue.len(),
            xml_dirs.len(),
            started.elapsed()
        );
        catalogue
    }

    /// Flatpak's catalogues for every remote in both installations
    /// (system and `~/.local/share/flatpak`), each tagged with the remote
    /// name as its origin.
    pub fn load_flatpak() -> Catalogue {
        Self::load_flatpak_roots(&flatpak_roots())
    }

    /// [`load_flatpak`](Self::load_flatpak) over explicit installation roots
    /// (each containing `appstream/<remote>/<arch>/active/`), so the layout
    /// can be tested from a fixture.
    pub fn load_flatpak_roots(roots: &[PathBuf]) -> Catalogue {
        let started = Instant::now();
        let mut components = Vec::new();
        for root in roots {
            for (remote, active) in flatpak_active_dirs(&root.join("appstream")) {
                let found = Self::load_dir(&active, Some(&active.join("icons")), Some(&remote));
                components.extend(found.components);
            }
        }
        let catalogue = Self::from_components(components);
        log::debug!(
            "appstream: {} Flatpak components from {} installations in {:?}",
            catalogue.len(),
            roots.len(),
            started.elapsed()
        );
        catalogue
    }

    /// Read one directory of `*.xml` / `*.xml.gz` files, resolving cached
    /// icons against `icons_dir`, tagging every component with `origin` when
    /// the file does not state one.
    pub fn load_dir(xml_dir: &Path, icons_dir: Option<&Path>, origin: Option<&str>) -> Catalogue {
        let started = Instant::now();
        let mut files: Vec<PathBuf> = match std::fs::read_dir(xml_dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| is_catalogue_file(p))
                .collect(),
            Err(e) => {
                log::debug!("appstream: not reading {}: {e}", xml_dir.display());
                return Catalogue::default();
            }
        };
        // Directory order is arbitrary; sorted, "core, extra, multilib" is
        // the same on every run, so which duplicate of an id wins is stable.
        files.sort();
        let mut components = Vec::new();
        for file in &files {
            match read_file_memoised(file, icons_dir, origin) {
                Ok(found) => components.extend(found.iter().cloned()),
                Err(e) => log::warn!("appstream: skipping {}: {e}", file.display()),
            }
        }
        let catalogue = Self::from_components(components);
        log::debug!(
            "appstream: {} components from {} files under {} in {:?}",
            catalogue.len(),
            files.len(),
            xml_dir.display(),
            started.elapsed()
        );
        catalogue
    }

    /// Parse one catalogue file's bytes (gzip or plain XML).
    pub fn parse(
        bytes: &[u8],
        icons_dir: Option<&Path>,
        origin: Option<&str>,
    ) -> crate::Result<Vec<Component>> {
        parser::parse(bytes, icons_dir, origin)
    }

    pub fn from_components(components: Vec<Component>) -> Catalogue {
        let index = index::Index::build(&components);
        Catalogue { components, index }
    }

    pub fn by_id(&self, id: &str) -> Option<&Component> {
        self.index.by_id(id).map(|i| &self.components[i])
    }

    /// The component a package provides. A package can provide several (a
    /// synthesiser with three desktop entries, Emacs and its client, every
    /// LibreOffice module); the desktop application named like the package
    /// wins, then the one whose id ends in the package name, then a
    /// reverse-DNS id over a legacy one, then the shortest name.
    /// [`by_pkgname_all`](Self::by_pkgname_all) has the rest.
    pub fn by_pkgname(&self, pkgname: &str) -> Option<&Component> {
        self.index.by_pkgname(pkgname).map(|i| &self.components[i])
    }

    /// Every component a package provides, in catalogue order.
    pub fn by_pkgname_all(&self, pkgname: &str) -> Vec<&Component> {
        self.index
            .by_pkgname_all(pkgname)
            .iter()
            .map(|&i| &self.components[i])
            .collect()
    }

    /// By Flatpak ref, `app/<id>/<arch>/<branch>`, or by just the id part.
    pub fn by_bundle(&self, bundle: &str) -> Option<&Component> {
        self.index.by_bundle(bundle).map(|i| &self.components[i])
    }

    /// Components matching the query in name, id, summary or keywords, best
    /// first. Used by the Flatpak source and as a fallback for any source
    /// whose own search is poor.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&Component> {
        self.index
            .search(&self.components, query, limit)
            .into_iter()
            .map(|i| &self.components[i])
            .collect()
    }

    pub fn components(&self) -> &[Component] {
        &self.components
    }

    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }

    pub fn len(&self) -> usize {
        self.components.len()
    }

    /// The standard XML directories, in the order they are read.
    pub fn system_xml_dirs() -> Vec<PathBuf> {
        [
            "/usr/share/swcatalog/xml",
            "/var/lib/swcatalog/xml",
            "/usr/share/app-info/xmls",
            "/var/lib/app-info/xmls",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    }
}

/// The extra catalogue roots named by `BROKEY_APPSTREAM_DIR`: separated the
/// way `PATH` is on this platform (`:` on Unix, `;` on Windows, both via
/// [`std::env::split_paths`], as `system::linux::which` already does),
/// empty entries ignored, relative entries ignored because the store's
/// working directory is not something a user can predict.
pub fn extra_roots(value: Option<&str>) -> Vec<PathBuf> {
    std::env::split_paths(value.unwrap_or_default())
        .filter(|p| p.is_absolute())
        .collect()
}

/// The Flatpak installations whose catalogues are read: the system one and
/// the user's (`$XDG_DATA_HOME/flatpak`, by default
/// `~/.local/share/flatpak`).
pub fn flatpak_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/var/lib/flatpak")];
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(data_home) = data_home {
        roots.push(data_home.join("flatpak"));
    }
    roots
}

/// Every `<remote>/<arch>/active` directory under a Flatpak installation's
/// `appstream/` that holds a catalogue, with the remote's name.
fn flatpak_active_dirs(appstream_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    let Ok(remotes) = std::fs::read_dir(appstream_dir) else {
        return found;
    };
    let mut remotes: Vec<PathBuf> = remotes.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    remotes.sort();
    for remote in remotes {
        let Some(name) = remote
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let Ok(arches) = std::fs::read_dir(&remote) else {
            continue;
        };
        let mut arches: Vec<PathBuf> = arches.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        arches.sort();
        for arch in arches {
            let active = arch.join("active");
            if active.join("appstream.xml.gz").is_file() || active.join("appstream.xml").is_file() {
                found.push((name.clone(), active));
            }
        }
    }
    found
}

fn is_catalogue_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    !name.starts_with('.')
        && (name.ends_with(".xml") || name.ends_with(".xml.gz"))
        && path.is_file()
}

/// What a memoised parse was made from. If any of it differs the file is
/// parsed again; the icons directory and origin are part of the key because
/// they change what the components say.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    icons_dir: Option<PathBuf>,
    origin: Option<String>,
}

struct Memo {
    stamp: Stamp,
    components: Arc<Vec<Component>>,
}

/// One entry per file path, replaced when the file changes. Kept in memory
/// only: a disk cache would need a format, a version and an invalidation
/// story, and the parse it would save is a few hundred milliseconds.
static MEMO: LazyLock<Mutex<HashMap<PathBuf, Memo>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn read_file_memoised(
    path: &Path,
    icons_dir: Option<&Path>,
    origin: Option<&str>,
) -> crate::Result<Arc<Vec<Component>>> {
    let meta = std::fs::metadata(path)?;
    let stamp = Stamp {
        modified: meta.modified().ok(),
        len: meta.len(),
        icons_dir: icons_dir.map(Path::to_path_buf),
        origin: origin.map(str::to_string),
    };
    if let Some(memo) = MEMO.lock().unwrap_or_else(|e| e.into_inner()).get(path)
        && memo.stamp == stamp
    {
        log::debug!(
            "appstream: {} unchanged, {} components from memory",
            path.display(),
            memo.components.len()
        );
        return Ok(memo.components.clone());
    }
    let started = Instant::now();
    let bytes = std::fs::read(path)?;
    let components = Arc::new(parser::parse(&bytes, icons_dir, origin)?);
    log::debug!(
        "appstream: parsed {} components from {} ({} bytes) in {:?}",
        components.len(),
        path.display(),
        bytes.len(),
        started.elapsed()
    );
    MEMO.lock().unwrap_or_else(|e| e.into_inner()).insert(
        path.to_path_buf(),
        Memo {
            stamp,
            components: components.clone(),
        },
    );
    Ok(components)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_roots_reads_the_platform_path_list_and_drops_relative_entries() {
        assert!(extra_roots(None).is_empty());
        assert!(extra_roots(Some("")).is_empty());

        // Two directories under the current one, so they are absolute on
        // whatever platform runs this test, plus a relative entry that
        // must be dropped because the store's working directory is not
        // something a user can predict. Joined with `join_paths` so the
        // separator is whatever `extra_roots` itself reads with
        // `split_paths` (`:` on Unix, `;` on Windows).
        let here = std::env::current_dir().expect("a working directory");
        let a = here.join("swcatalog-a");
        let b = here.join("swcatalog-b");
        let value = std::env::join_paths([
            a.as_os_str(),
            std::ffi::OsStr::new("relative"),
            b.as_os_str(),
        ])
        .expect("none of these paths carry the path-list separator")
        .into_string()
        .expect("these paths are valid UTF-8");

        assert_eq!(extra_roots(Some(&value)), vec![a, b]);
    }

    #[cfg(unix)]
    #[test]
    fn flatpak_roots_start_with_the_system_installation() {
        let roots = flatpak_roots();
        assert_eq!(roots[0], PathBuf::from("/var/lib/flatpak"));
        assert!(roots.iter().all(|r| r.is_absolute()));
    }

    #[test]
    fn load_dir_on_a_missing_directory_is_empty() {
        let catalogue = Catalogue::load_dir(Path::new("/nonexistent/swcatalog/xml"), None, None);
        assert!(catalogue.is_empty());
        assert_eq!(catalogue.len(), 0);
    }

    #[test]
    fn a_changed_file_is_parsed_again_and_an_unchanged_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.xml");
        let xml = |id: &str| {
            format!(
                "<components origin=\"o\"><component type=\"desktop-application\"><id>{id}</id><name>N</name></component></components>"
            )
        };
        std::fs::write(&file, xml("first")).unwrap();
        let a = read_file_memoised(&file, None, None).unwrap();
        let b = read_file_memoised(&file, None, None).unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "an unchanged file comes back from memory"
        );
        assert_eq!(a[0].id, "first");
        // A different origin is a different result even for the same bytes.
        let c = read_file_memoised(&file, None, Some("other")).unwrap();
        assert_eq!(c[0].origin, "other");
        std::fs::write(&file, xml("second-longer")).unwrap();
        let d = read_file_memoised(&file, None, None).unwrap();
        assert_eq!(d[0].id, "second-longer");
    }

    #[test]
    fn catalogue_files_are_xml_or_gzipped_xml_and_not_hidden() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.xml", "b.xml.gz", ".hidden.xml", "c.txt", "d.gz"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let mut ok: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| is_catalogue_file(p))
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        ok.sort();
        assert_eq!(ok, vec!["a.xml", "b.xml.gz"]);
    }
}
