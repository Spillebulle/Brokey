//! winget: the primary Windows source.
//!
//! Search comes from the catalogue in [`index`], which Brokey fetches itself,
//! so this source is searchable on a machine that has never had winget. The
//! operations are `winget.exe` steps, which need the tool, so `available`
//! tracks the tool and `searchable` tracks the catalogue.

pub mod index;
pub mod query;
pub mod version;

use crate::model::{Command, SourceKind, SourceSetup, SourceStatus, Step};
use crate::{Result, Setup, Source};
use std::path::Path;
use std::sync::Arc;

/// Where the App Installer bundle and its hash come from. The release is
/// looked up at the moment it is needed rather than pinned, because pinning
/// a version means shipping a Brokey that installs an old winget forever.
pub const WINGET_CLI_LATEST: &str =
    "https://api.github.com/repos/microsoft/winget-cli/releases/latest";

pub const NOT_INSTALLED: &str = "winget is not installed, so nothing can be installed or removed through it. \
     Brokey still searches winget's catalogue, which it reads itself.";
/// Not wired up yet. `Source::setup` returns `Option<Setup>`, and `None`
/// carries no reason, so `transaction/plan.rs` cannot tell "already
/// installed" apart from "Microsoft could not be reached" and shows a
/// generic sentence for both. Reaching this constant needs the trait to
/// carry a reason through a failed setup, which is outside this task.
pub const NO_BOOTSTRAP: &str = "winget is not installed, and the App Installer release could not be reached, \
     so Brokey cannot set it up just now. Brokey still searches winget's catalogue.";
pub const SETUP_LABEL: &str = "Install winget";
pub const SETUP_SENTENCE: &str = "The App Installer package is downloaded from Microsoft, checked against the \
     hash Microsoft publishes with it, and installed for you alone. No \
     Administrator permission is needed.";

/// What a `winget-cli` release says about its bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bootstrap {
    pub url: String,
    /// The SHA-256 Microsoft publishes in a `.txt` beside the bundle.
    pub sha256: String,
    pub version: String,
}

pub struct Winget {
    client: Arc<crate::http::Client>,
}

impl Winget {
    pub fn new(client: Arc<crate::http::Client>) -> Winget {
        Winget { client }
    }
}

/// Download, check, install. Three steps because the check has to be able to
/// stop the plan: the Runner abandons a plan at the first failing step, so an
/// install can never run against a bundle whose hash did not match.
pub fn bootstrap_steps(b: &Bootstrap, into: &Path) -> Vec<Step> {
    let file = into.join("Microsoft.DesktopAppInstaller.msixbundle");
    let file = file.display().to_string();
    let step = |title: String, program: &str, args: Vec<String>, weight: u32| Step {
        source: SourceKind::Winget,
        title,
        command: Command {
            program: program.to_string(),
            args,
            env: Vec::new(),
            cwd: None,
        },
        // Nothing here elevates. App Installer registers for one user.
        needs_root: false,
        weight,
    };
    vec![
        step(
            format!("Downloading App Installer {}", b.version),
            "curl.exe",
            vec![
                "-L".to_string(),
                "--fail".to_string(),
                "--create-dirs".to_string(),
                "-o".to_string(),
                file.clone(),
                b.url.clone(),
            ],
            8,
        ),
        step(
            "Checking what was downloaded".to_string(),
            "powershell.exe",
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                format!(
                    "if ((Get-FileHash -Algorithm SHA256 -LiteralPath '{}').Hash -ne '{}') \
                     {{ Write-Error 'The App Installer package did not match the hash Microsoft \
                     publishes for it, so it was not installed.'; exit 1 }}",
                    ps_quote(&file),
                    ps_quote(&b.sha256)
                ),
            ],
            1,
        ),
        step(
            format!("Installing App Installer {}", b.version),
            "powershell.exe",
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                format!("Add-AppxPackage -LiteralPath '{}'", ps_quote(&file)),
            ],
            4,
        ),
    ]
}

/// A PowerShell single-quoted literal takes an apostrophe as two of them,
/// and treats everything else inside it literally. That is why the commands
/// below quote with apostrophes and escape nothing else.
fn ps_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// A SHA-256 as Microsoft publishes it: 64 hexadecimal characters and
/// nothing else. Worth checking because the text comes off the network, and
/// a value that is not a hash can never match, which would fail setup with
/// nothing useful to say about why.
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Where `winget.exe` is, if it is anywhere.
#[cfg(windows)]
pub fn winget_exe() -> Option<std::path::PathBuf> {
    crate::system::windows::which("winget")
}

impl Source for Winget {
    fn kind(&self) -> SourceKind {
        SourceKind::Winget
    }

    fn status(&self) -> SourceStatus {
        let kind = SourceKind::Winget;
        #[cfg(windows)]
        let tool = winget_exe();
        #[cfg(not(windows))]
        let tool: Option<std::path::PathBuf> = None;

        if tool.is_none() {
            return SourceStatus {
                kind,
                available: false,
                reason: Some(NOT_INSTALLED.to_string()),
                detail: None,
                // The catalogue is Brokey's own file, so search works either way.
                searchable: true,
                // What setting winget up would do. Whether Microsoft's release
                // is reachable is deliberately not asked here: `status()` runs
                // every time the page draws a source list, and two network
                // requests per draw to prove a remedy will work is a cost
                // nobody agreed to. `setup()` finds out, once, when the user
                // actually asks for it.
                setup: Some(SourceSetup {
                    label: SETUP_LABEL.to_string(),
                    sentence: SETUP_SENTENCE.to_string(),
                }),
            };
        }
        let detail = self
            .catalogue()
            .ok()
            .and_then(|db| query::count(&db).ok())
            .map(|n| format!("{n} packages"));
        SourceStatus {
            kind,
            available: true,
            reason: None,
            detail,
            searchable: true,
            setup: None,
        }
    }

    fn setup(&self) -> Option<Setup> {
        #[cfg(windows)]
        if winget_exe().is_some() {
            return None;
        }
        let b = self.bootstrap().ok()?;
        Some(Setup {
            ops: Vec::new(),
            steps: bootstrap_steps(&b, &self.client.download_dir()),
            notice: SETUP_SENTENCE.to_string(),
        })
    }

    fn search(&self, _query: &crate::Query) -> Result<Vec<crate::model::Package>> {
        todo!("Task 5")
    }

    fn installed(&self) -> Result<Vec<crate::model::Package>> {
        todo!("Task 6")
    }

    fn updates(&self) -> Result<Vec<crate::model::Update>> {
        todo!("Task 6")
    }

    fn details(&self, _id: &str) -> Result<crate::model::Package> {
        todo!("Task 5")
    }

    fn plan(&self, _op: &crate::model::Op) -> Result<Vec<Step>> {
        todo!("Task 7")
    }
}

impl Winget {
    /// The catalogue, downloaded if the cached copy is missing or stale.
    fn catalogue(&self) -> Result<rusqlite::Connection> {
        let path = index::cached_path(&self.client.download_dir());
        let stale = match std::fs::metadata(&path) {
            Err(_) => true,
            Ok(m) => m
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_none_or(|age| age > index::MAX_AGE),
        };
        if stale {
            let bytes = self.client.get_bytes(index::CATALOGUE_URL)?;
            let db = index::database_from_msix(&bytes)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, db).map_err(|e| {
                crate::Error::new(format!("The winget catalogue could not be saved: {e}."))
            })?;
        }
        index::open(&path)
    }

    /// The newest App Installer release, and the hash published with it.
    fn bootstrap(&self) -> Result<Bootstrap> {
        #[derive(serde::Deserialize)]
        struct Release {
            tag_name: String,
            assets: Vec<Asset>,
        }
        #[derive(serde::Deserialize)]
        struct Asset {
            name: String,
            browser_download_url: String,
        }
        let release: Release = self.client.get_json(WINGET_CLI_LATEST, &[])?;
        let bundle = release
            .assets
            .iter()
            .find(|a| a.name.ends_with(".msixbundle"))
            .ok_or_else(|| {
                crate::Error::new(
                    "The App Installer release has no package in it, so winget cannot be set up."
                        .to_string(),
                )
            })?;
        let hash_asset = release
            .assets
            .iter()
            .find(|a| a.name.ends_with(".txt") && a.name.contains("DesktopAppInstaller"))
            .ok_or_else(|| {
                crate::Error::new(
                    "The App Installer release publishes no hash for its package, so Brokey \
                     will not install it."
                        .to_string(),
                )
            })?;
        let sha256 = self
            .client
            .get_text(&hash_asset.browser_download_url)?
            .trim()
            .to_string();
        if !is_sha256(&sha256) {
            return Err(crate::Error::new(
                "The hash published with the App Installer package is not a SHA-256, \
                 so Brokey will not install it."
                    .to_string(),
            ));
        }
        Ok(Bootstrap {
            url: bundle.browser_download_url.clone(),
            sha256,
            version: release.tag_name.trim_start_matches('v').to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bootstrap() -> Bootstrap {
        Bootstrap {
            url: "https://github.com/microsoft/winget-cli/releases/download/v1.29.290/\
                  Microsoft.DesktopAppInstaller_8wekyb3d8bbwe.msixbundle"
                .to_string(),
            sha256: "ab".repeat(32),
            version: "1.29.290".to_string(),
        }
    }

    /// Three steps, in the only order that is safe.
    #[test]
    fn setting_winget_up_downloads_then_verifies_then_installs() {
        let steps = bootstrap_steps(&bootstrap(), std::path::Path::new("C:/tmp"));
        assert_eq!(steps.len(), 3);
        assert!(
            steps[0].title.starts_with("Downloading"),
            "{}",
            steps[0].title
        );
        assert!(steps[1].title.starts_with("Checking"), "{}", steps[1].title);
        assert!(
            steps[2].title.starts_with("Installing"),
            "{}",
            steps[2].title
        );
    }

    /// The published hash reaches the command that checks it. Without this
    /// the verify step is decoration.
    #[test]
    fn the_verify_step_carries_the_published_hash() {
        let b = bootstrap();
        let steps = bootstrap_steps(&b, std::path::Path::new("C:/tmp"));
        let joined = steps[1].command.args.join(" ");
        assert!(joined.contains(&b.sha256), "the hash is in the command");
    }

    /// Nothing about setting winget up needs Administrator. This is what
    /// makes per-user-first worth having, and a regression here costs the
    /// user a UAC prompt they were promised they would not see.
    #[test]
    fn setting_winget_up_never_elevates() {
        for s in bootstrap_steps(&bootstrap(), std::path::Path::new("C:/tmp")) {
            assert!(!s.needs_root, "{} elevates", s.title);
        }
    }

    /// Every step says winget, so the activity panel attributes them.
    #[test]
    fn the_steps_belong_to_winget() {
        for s in bootstrap_steps(&bootstrap(), std::path::Path::new("C:/tmp")) {
            assert_eq!(s.source, SourceKind::Winget);
        }
    }

    /// An apostrophe in the hash must not be able to close the PowerShell
    /// literal. If it can, the rest of the value becomes PowerShell and the
    /// comparison stops being a comparison, so the step exits zero on a
    /// mismatch and the install runs against a bundle nobody checked.
    #[test]
    fn a_quote_in_the_hash_cannot_close_the_powershell_literal() {
        let mut b = bootstrap();
        b.sha256 = "aa' -and $false -and 'bb".to_string();
        let joined = bootstrap_steps(&b, std::path::Path::new("C:/tmp"))[1]
            .command
            .args
            .join(" ");
        assert!(joined.contains("aa'' -and $false -and ''bb"), "{joined}");
        assert!(!joined.contains("aa' -and"), "{joined}");
    }

    /// Windows account names contain apostrophes, so download paths do too.
    /// Both commands that name the file have to survive it.
    #[test]
    fn a_quote_in_the_path_cannot_close_the_powershell_literal() {
        let steps = bootstrap_steps(&bootstrap(), std::path::Path::new("C:/Users/O'Brien"));
        for i in [1, 2] {
            let joined = steps[i].command.args.join(" ");
            assert!(joined.contains("O''Brien"), "step {i}: {joined}");
            // Every apostrophe in the command is either one of the four that
            // open and close the two literals, or a doubled one from the data.
            assert_eq!(joined.matches('\'').count() % 2, 0, "step {i}: {joined}");
        }
    }

    /// What Microsoft publishes, and the shapes that mean something went wrong
    /// between their release page and here.
    #[test]
    fn only_a_real_sha256_is_accepted() {
        assert!(is_sha256(&"ab".repeat(32)));
        assert!(is_sha256("ABCDEF0123456789".repeat(4).as_str()));
        assert!(!is_sha256(&"ab".repeat(31)));
        assert!(!is_sha256(&format!("{}c", "ab".repeat(32))));
        assert!(!is_sha256(&format!("{}g", "ab".repeat(31) + "a")));
        assert!(!is_sha256(""));
        assert!(!is_sha256("<!DOCTYPE html><html>404</html>"));
        assert!(!is_sha256("aa' -and $false -and 'bb"));
    }
}
