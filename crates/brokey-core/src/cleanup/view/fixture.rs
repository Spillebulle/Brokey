//! A machine described in JSON, for tests: the same shapes as the live view,
//! answered from a file.

use super::*;
use std::collections::{HashMap, HashSet};

#[derive(Deserialize, Default)]
#[serde(default)]
struct Key {
    subkeys: Vec<String>,
    values: Vec<(String, String)>,
    unreadable: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Snapshot {
    folders: HashMap<Known, String>,
    /// Folder path to what is in it.
    dirs: HashMap<String, Vec<Entry>>,
    /// Folders that exist and may not be listed.
    unreadable: Vec<String>,
    /// Paths whose own reparse kind is not `None`.
    reparse: HashMap<String, Reparse>,
    /// Folder path to how many of its entries cannot be read.
    unreadable_entries: HashMap<String, u32>,
    /// How many services, tasks and firewall rules cannot be read.
    unreadable_services: u32,
    unreadable_tasks: u32,
    unreadable_firewall: u32,
    /// Folder sizes that are not the sum of what they hold.
    sizes: HashMap<String, u64>,
    /// Hive to key path to the key.
    registry: HashMap<Hive, HashMap<String, Key>>,
    /// Shortcut path to target.
    shortcuts: HashMap<String, String>,
    services: Vec<Service>,
    tasks: Vec<ScheduledTask>,
    firewall: Vec<FirewallRule>,
}

pub struct FixtureView {
    folders: HashMap<Known, String>,
    dirs: HashMap<String, Vec<Entry>>,
    unreadable: HashSet<String>,
    unreadable_entries: HashMap<String, u32>,
    unreadable_counts: [u32; 3],
    reparse: HashMap<String, Reparse>,
    sizes: HashMap<String, u64>,
    registry: HashMap<(Hive, String), Key>,
    shortcuts: HashMap<String, String>,
    services: Vec<Service>,
    tasks: Vec<ScheduledTask>,
    firewall: Vec<FirewallRule>,
}

/// Windows paths compare without regard to case or to which slash.
fn norm(path: &str) -> String {
    path.replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

fn norm_path(path: &Path) -> String {
    norm(&path.to_string_lossy())
}

impl FixtureView {
    /// Panics on a file that is not a snapshot: it is a test's own input.
    pub fn load(path: &Path) -> FixtureView {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        Self::from_json(&text)
    }

    pub fn from_json(text: &str) -> FixtureView {
        let snap: Snapshot =
            serde_json::from_str(text).unwrap_or_else(|e| panic!("bad snapshot: {e}"));
        let mut registry = HashMap::new();
        for (hive, keys) in snap.registry {
            for (path, key) in keys {
                registry.insert((hive, norm(&path)), key);
            }
        }
        FixtureView {
            folders: snap.folders,
            dirs: snap.dirs.into_iter().map(|(p, e)| (norm(&p), e)).collect(),
            unreadable: snap.unreadable.iter().map(|p| norm(p)).collect(),
            unreadable_entries: snap
                .unreadable_entries
                .iter()
                .map(|(p, n)| (norm(p), *n))
                .collect(),
            unreadable_counts: [
                snap.unreadable_services,
                snap.unreadable_tasks,
                snap.unreadable_firewall,
            ],
            reparse: snap.reparse.iter().map(|(p, r)| (norm(p), *r)).collect(),
            sizes: snap.sizes.iter().map(|(p, s)| (norm(p), *s)).collect(),
            registry,
            shortcuts: snap
                .shortcuts
                .iter()
                .map(|(l, t)| (norm(l), t.clone()))
                .collect(),
            services: snap.services,
            tasks: snap.tasks,
            firewall: snap.firewall,
        }
    }

    fn entry_of(&self, key: &str) -> Option<&Entry> {
        let (parent, name) = key.rsplit_once('\\')?;
        self.dirs
            .get(parent)?
            .iter()
            .find(|e| e.name.to_lowercase() == name)
    }

    fn key(&self, hive: Hive, path: &str) -> Result<&Key, ViewError> {
        match self.registry.get(&(hive, norm(path))) {
            Some(k) if k.unreadable => Err(ViewError::Unreadable(path.to_string())),
            Some(k) => Ok(k),
            None => Err(ViewError::Missing(path.to_string())),
        }
    }

    fn size_of(&self, key: &str) -> u64 {
        if let Some(size) = self.sizes.get(key) {
            return *size;
        }
        let Some(entries) = self.dirs.get(key) else {
            return 0;
        };
        entries
            .iter()
            .filter(|e| e.reparse == Reparse::None)
            .map(|e| {
                if e.is_dir {
                    self.size_of(&format!("{key}\\{}", e.name.to_lowercase()))
                } else {
                    e.size
                }
            })
            .sum()
    }
}

impl SystemView for FixtureView {
    fn known_folder(&self, which: Known) -> Option<PathBuf> {
        self.folders.get(&which).map(PathBuf::from)
    }

    fn list(&self, dir: &Path) -> Result<Listed<Entry>, ViewError> {
        let key = norm_path(dir);
        if self.unreadable.contains(&key) {
            return Err(ViewError::Unreadable(dir.display().to_string()));
        }
        let items = self
            .dirs
            .get(&key)
            .cloned()
            .ok_or_else(|| ViewError::Missing(dir.display().to_string()))?;
        let unreadable = self.unreadable_entries.get(&key).copied().unwrap_or(0);
        Ok(Listed { items, unreadable })
    }

    fn exists(&self, path: &Path) -> bool {
        let key = norm_path(path);
        self.dirs.contains_key(&key)
            || self.unreadable.contains(&key)
            || self.entry_of(&key).is_some()
    }

    fn reparse(&self, path: &Path) -> Reparse {
        let key = norm_path(path);
        self.reparse
            .get(&key)
            .copied()
            .or_else(|| self.entry_of(&key).map(|e| e.reparse))
            .unwrap_or_default()
    }

    fn dir_size(&self, dir: &Path) -> u64 {
        self.size_of(&norm_path(dir))
    }

    fn subkeys(&self, hive: Hive, path: &str) -> Result<Vec<String>, ViewError> {
        self.key(hive, path).map(|k| k.subkeys.clone())
    }

    fn values(&self, hive: Hive, path: &str) -> Result<Vec<(String, String)>, ViewError> {
        self.key(hive, path).map(|k| k.values.clone())
    }

    fn shortcut_target(&self, lnk: &Path) -> Option<PathBuf> {
        self.shortcuts.get(&norm_path(lnk)).map(PathBuf::from)
    }

    fn services(&self) -> Listed<Service> {
        Listed {
            items: self.services.clone(),
            unreadable: self.unreadable_counts[0],
        }
    }

    fn tasks(&self) -> Listed<ScheduledTask> {
        Listed {
            items: self.tasks.clone(),
            unreadable: self.unreadable_counts[1],
        }
    }

    fn firewall_rules(&self) -> Listed<FirewallRule> {
        Listed {
            items: self.firewall.clone(),
            unreadable: self.unreadable_counts[2],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> FixtureView {
        FixtureView::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cleanup/minimal.json"),
        )
    }

    #[test]
    fn fixture_view_answers_what_the_json_says() {
        let v = minimal();
        assert_eq!(
            v.known_folder(Known::ProgramFiles),
            Some(PathBuf::from(r"C:\Program Files"))
        );
        let listed = v.list(Path::new(r"C:\Program Files\Acme")).unwrap();
        assert_eq!(listed.unreadable, 1);
        let listed = listed.items;
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "acme.exe");
        assert!(!listed[0].is_dir);
        assert_eq!(listed[0].size, 1000);
        assert!(listed[1].is_dir);
        // Case and slash direction do not matter on Windows.
        assert!(v.list(Path::new("c:/program files/ACME")).is_ok());

        assert_eq!(
            v.subkeys(Hive::Hkcu, r"Software\Acme").unwrap(),
            vec!["Settings".to_string()]
        );
        assert_eq!(
            v.values(Hive::Hkcu, r"Software\Acme\Settings").unwrap(),
            vec![("Theme".to_string(), "dark".to_string())]
        );
        assert_eq!(
            v.subkeys(Hive::Hklm, r"Software\Acme"),
            Err(ViewError::Missing(r"Software\Acme".into()))
        );

        assert_eq!(v.services().items.len(), 1);
        assert_eq!(v.services().items[0].name, "AcmeSvc");
        assert_eq!(
            v.tasks().items[0].actions,
            vec![r"C:\Program Files\Acme\acme.exe"]
        );
        assert_eq!(v.firewall_rules().items[0].name, "Acme");

        assert_eq!(
            v.reparse(Path::new(r"C:\Program Files\Acme\link")),
            Reparse::NameSurrogate
        );
        assert_eq!(
            v.reparse(Path::new(r"C:\Program Files\Acme\acme.exe")),
            Reparse::None
        );
        assert_eq!(
            v.reparse(Path::new(r"C:\Users\me\OneDrive")),
            Reparse::Cloud
        );

        assert!(v.exists(Path::new(r"C:\Program Files\Acme\acme.exe")));
        assert!(!v.exists(Path::new(r"C:\Program Files\Nothing")));
        assert_eq!(v.dir_size(Path::new(r"C:\Program Files\Acme")), 1000);
        assert_eq!(v.services().unreadable, 2);
        assert_eq!(v.tasks().unreadable, 3);
        assert_eq!(v.firewall_rules().unreadable, 0);
        assert_eq!(
            v.shortcut_target(Path::new(r"C:\Start\Acme.lnk")),
            Some(PathBuf::from(r"C:\Program Files\Acme\acme.exe"))
        );
    }

    #[test]
    fn a_path_marked_unreadable_is_an_error_and_not_a_missing_one() {
        let v = minimal();
        assert!(matches!(
            v.list(Path::new(r"C:\ProgramData\Locked")),
            Err(ViewError::Unreadable(_))
        ));
        assert!(v.exists(Path::new(r"C:\ProgramData\Locked")));
        assert!(matches!(
            v.values(Hive::Hklm, r"Software\Locked"),
            Err(ViewError::Unreadable(_))
        ));
    }
}
