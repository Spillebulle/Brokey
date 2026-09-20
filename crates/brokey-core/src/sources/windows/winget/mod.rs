//! winget: the primary Windows source.
//!
//! Search comes from the catalogue in [`index`], which Brokey fetches itself,
//! so this source is searchable on a machine that has never had winget. The
//! operations are `winget.exe` steps, which need the tool, so `available`
//! tracks the tool and `searchable` tracks the catalogue.

pub mod index;
pub mod manifest;
pub mod query;
pub mod version;

use crate::model::{Command, SourceKind, SourceSetup, SourceStatus, Step};
use crate::{Result, Setup, Source};
use std::path::Path;
use std::sync::Arc;

/// A catalogue row as the page's `Package`.
///
/// The index is a search index: it has an id, a name, a moniker, a version
/// and a publisher key, and nothing else. Every other field stays `None`
/// rather than being guessed at, which is what `Package` already expects of a
/// source that does not know them. This is what a search result is, and a
/// search never fetches anything per package. [`Source::details`] asks for
/// one package and can afford one request, so it fills the summary, the
/// description, the homepage, the licence, the publisher and the categories
/// from the locale manifest through [`manifest::describe`], which also
/// appends two rows to the facts column. Icons arrive with a later plan.
pub fn to_package(row: &query::Row) -> crate::model::Package {
    let mut facts = Vec::new();
    facts.push(("Package id".to_string(), row.id.clone()));
    if let Some(m) = &row.moniker
        && !m.trim().is_empty()
    {
        facts.push(("Moniker".to_string(), m.clone()));
    }
    crate::model::Package {
        source: SourceKind::Winget,
        id: row.id.clone(),
        name: row.name.clone(),
        kind: crate::model::PackageKind::App,
        summary: None,
        description: None,
        version: Some(row.latest_version.clone()),
        installed_version: None,
        installed: false,
        repo: Some("winget".to_string()),
        licence: None,
        homepage: None,
        // The index stores only `norm_publishers2`, which is a join key and not
        // a name: `igorpavlov`, `pythonsoftwarefoundation`. It has no column
        // holding the publisher as a person would recognise it, so a row from
        // the index leaves this empty rather than showing a fact nobody wrote.
        // `details` fills it from the locale manifest, which carries
        // `Publisher: The GIMP Team` in plain text. When Add/Remove Programs
        // knows the same application, its edition carries the real name too
        // and the grouped app shows that.
        developer: None,
        updated: None,
        download_size: None,
        installed_size: None,
        popularity: None,
        popularity_label: None,
        icon: None,
        screenshots: Vec::new(),
        categories: Vec::new(),
        appstream_id: None,
        out_of_date: false,
        sandboxed: false,
        facts,
    }
}

/// How long a locale manifest is trusted before it is fetched again. The
/// same length as [`index::MAX_AGE`], for the same reason: this is Brokey's
/// own snapshot of somebody else's catalogue, not a live query, and a
/// description does not change between two openings of a detail page.
pub const MANIFEST_MAX_AGE: std::time::Duration = index::MAX_AGE;

/// A catalogue row as the detail page's `Package`: [`to_package`], and then
/// whatever the locale manifest adds to it.
///
/// `fetch` is how the manifest is read. [`Source::details`] passes the cached
/// HTTP client; a test passes a closure, which is what lets the failing path
/// below be exercised without a network.
///
/// A fetch that fails, an id whose manifest has no derivable address, and a
/// manifest that says nothing all leave the package exactly as the index
/// described it. A missing description is not an error the page should show:
/// the package is still installable and everything the catalogue knows about
/// it is still there.
fn described_package(
    row: &query::Row,
    fetch: &dyn Fn(&str) -> Result<String>,
) -> crate::model::Package {
    let mut package = to_package(row);
    if let Some(url) = manifest::manifest_url(&row.id, &row.latest_version)
        && let Ok(text) = fetch(&url)
    {
        manifest::describe(&mut package, &manifest::parse(&text));
    }
    package
}

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
                // `Add-AppxPackage` has no `-LiteralPath` parameter, only
                // `-Path` (alias `PSPath`), unlike `Get-FileHash` above. Its
                // `-Path` is a plain string with no wildcard support, so
                // `ps_quote` is enough and no bracket escaping is needed.
                format!("Add-AppxPackage -Path '{}'", ps_quote(&file)),
            ],
            4,
        ),
    ]
}

/// The name of the `.txt` file Microsoft publishes beside a bundle, holding
/// its SHA-256. This is the relationship Microsoft actually maintains: the
/// hash file is the bundle's own name with `.msixbundle` replaced by `.txt`.
/// The release also carries a second `.txt` whose name contains
/// `DesktopAppInstaller`, the dependency archive's hash, which is why a
/// predicate on the name alone cannot tell the two apart. Pure, so it is
/// tested without a fixture and runs on Linux too.
pub fn hash_asset_name(bundle_name: &str) -> String {
    bundle_name.replace(".msixbundle", ".txt")
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    Install,
    Update,
    Remove,
}

/// One `winget.exe` call.
///
/// `--silent` and `--disable-interactivity` because the helper runs with no
/// terminal: a prompt would hang the plan rather than ask anybody anything.
/// `--exact` because the id is exact and a near match would install something
/// the user did not choose.
///
/// `program` is the full path to `winget.exe`, resolved by the caller. It is
/// a parameter rather than a lookup in here so that this stays a pure
/// function of its arguments, and so that resolution happens once, in the
/// one process whose environment is the right one to resolve it in. See
/// [`winget_program`].
pub fn operation_step(kind: OpKind, id: &str, program: &str) -> Step {
    let (verb, title) = match kind {
        OpKind::Install => ("install", format!("Installing {id}")),
        OpKind::Update => ("upgrade", format!("Updating {id}")),
        OpKind::Remove => ("uninstall", format!("Removing {id}")),
    };
    let mut args = vec![
        verb.to_string(),
        "--exact".to_string(),
        "--id".to_string(),
        id.to_string(),
        "--silent".to_string(),
        "--disable-interactivity".to_string(),
        "--accept-source-agreements".to_string(),
    ];
    if kind != OpKind::Remove {
        // Nothing is being agreed to when something is taken off.
        args.push("--accept-package-agreements".to_string());
    }
    Step {
        source: SourceKind::Winget,
        title,
        command: Command {
            program: program.to_string(),
            args,
            env: Vec::new(),
            cwd: None,
        },
        // `--disable-interactivity` tells winget not to prompt, so it cannot
        // ask for elevation on its own: a machine-scope package fails unless
        // the step is already elevated, and most popular winget packages are
        // machine scope. Choosing the scope per package needs the manifest,
        // which is the metadata ladder in a later plan.
        needs_root: true,
        weight: 10,
    }
}

/// The one step that updates everything winget can, which is a different
/// command from any single package's: `--all` in place of an id.
///
/// It lives here beside [`operation_step`] and takes its program the same
/// way, so the closed list can rebuild it and compare the whole command
/// rather than carrying a second copy of this argument list that could
/// drift away from this one.
pub fn update_all_step(program: &str) -> Step {
    Step {
        source: SourceKind::Winget,
        title: "Updating everything winget can".to_string(),
        command: Command {
            program: program.to_string(),
            args: vec![
                "upgrade".to_string(),
                "--all".to_string(),
                "--silent".to_string(),
                "--disable-interactivity".to_string(),
                "--accept-source-agreements".to_string(),
                "--accept-package-agreements".to_string(),
            ],
            env: Vec::new(),
            cwd: None,
        },
        needs_root: true,
        weight: 10,
    }
}

/// Where `winget.exe` is, if it is anywhere.
#[cfg(windows)]
pub fn winget_exe() -> Option<std::path::PathBuf> {
    crate::system::windows::which("winget")
}

/// The program a winget step names: the full path to the `winget.exe` that
/// will really run.
///
/// Resolution happens here, in the unelevated process, and never in the
/// helper. `which` reads `PATH` from the calling process's own environment,
/// and winget is reached through a per-user app execution alias in
/// `%LOCALAPPDATA%\Microsoft\WindowsApps`. When elevation is answered with
/// an administrator's credentials the elevated helper has that
/// administrator's profile, so a bare name looked up there would search the
/// wrong profile and find nothing, or something else. The helper therefore
/// searches nothing at all and refuses a program that is not a full path;
/// this is the end that does the looking, because this is the end whose
/// environment is the user's.
///
/// What `which` finds is the alias and not the program. The alias is a
/// zero-length reparse point in a folder the invoking user has full control
/// of, and `CreateProcess` follows it to an executable under
/// `C:\Program Files\WindowsApps`, so the file a step should name is the
/// target and not the alias:
/// [`resolve_app_execution_alias`](crate::system::windows::resolve_app_execution_alias)
/// is what turns one into the other. Elevating the alias would elevate
/// whatever the user last put there, which is why the closed list refuses
/// it; elevating the target is the thing that was meant all along. When the
/// path is not an alias, or the alias cannot be read, what `which` found is
/// handed over unchanged and the closed list judges that instead.
///
/// The path is otherwise passed on as it stands and is not canonicalised.
///
/// When winget is not installed there is nothing to resolve and the bare
/// name stands in. `status()` already reports the source unavailable in
/// that case, so no plan should reach here; if one does, the helper refuses
/// it by the absolute-path rule rather than searching for it.
#[cfg(windows)]
pub fn winget_program() -> String {
    winget_exe()
        .map(|path| {
            crate::system::windows::resolve_app_execution_alias(&path)
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| "winget.exe".to_string())
}

/// There is no `winget.exe` to find off Windows, and `sources::all` never
/// selects this source there, so nothing resolves: the bare name is what a
/// step would carry, and the Linux closed list refuses it.
#[cfg(not(windows))]
pub fn winget_program() -> String {
    "winget.exe".to_string()
}

const BY_CODE_SQL: &str = "
SELECT p.id, p.name, p.moniker, p.latest_version, np.norm_publisher
FROM packages p
LEFT JOIN norm_publishers2 np ON np.package = p.rowid
WHERE p.rowid = (SELECT package FROM productcodes2 WHERE productcode = :code LIMIT 1)
   OR p.rowid = (SELECT package FROM upgradecodes2 WHERE upgradecode = :code LIMIT 1)
LIMIT 1
";

const BY_NAME_AND_PUBLISHER_SQL: &str = "
SELECT p.id, p.name, p.moniker, p.latest_version, np.norm_publisher
FROM packages p
JOIN norm_names2 nn ON nn.package = p.rowid AND nn.norm_name = :name
JOIN norm_publishers2 np ON np.package = p.rowid AND np.norm_publisher = :publisher
LIMIT 2
";

/// The catalogue package an uninstall entry is, if it is one.
///
/// Three rungs, exact first. The product and upgrade code rungs are exact.
/// The name rung requires the publisher to agree as well, and then requires
/// the answer to be unique: the index folds version families, so `python` and
/// `pythonsoftwarefoundation` name every Python at once. Nothing is returned
/// when more than one package fits, because a wrong join offers an update
/// that replaces one application with a different one.
pub fn match_entry(
    db: &rusqlite::Connection,
    e: &crate::sources::windows::arp::RawEntry,
) -> Result<Option<query::Row>> {
    // Codes are stored lower-case in the catalogue and spelt however the
    // installer felt in the registry. Fold both sides.
    let code = e.key_name.trim().to_lowercase();
    if let Some(row) = query::rows(db, BY_CODE_SQL, &[(":code", &code)])?
        .into_iter()
        .next()
    {
        return Ok(Some(row));
    }
    let (Some(name), Some(publisher)) = (&e.display_name, &e.publisher) else {
        return Ok(None);
    };
    let found = query::rows(
        db,
        BY_NAME_AND_PUBLISHER_SQL,
        &[
            (":name", &query::normalise(name)),
            (":publisher", &query::normalise(publisher)),
        ],
    )?;
    // Exactly one, or none. See the note above about version families.
    match found.len() {
        1 => Ok(found.into_iter().next()),
        _ => Ok(None),
    }
}

/// Everything the catalogue recognises on this machine.
pub fn installed_from(
    db: &rusqlite::Connection,
    entries: &[crate::sources::windows::arp::RawEntry],
) -> Result<Vec<crate::model::Package>> {
    let mut out = Vec::new();
    for e in entries
        .iter()
        .filter(|e| crate::sources::windows::arp::is_application(e))
    {
        if let Some(row) = match_entry(db, e)? {
            let mut p = to_package(&row);
            p.installed = true;
            p.installed_version = e.display_version.clone();
            out.push(p);
        }
    }
    Ok(out)
}

/// Those of them the catalogue has a newer version of.
pub fn updates_from(
    db: &rusqlite::Connection,
    entries: &[crate::sources::windows::arp::RawEntry],
) -> Result<Vec<crate::model::Update>> {
    let mut out = Vec::new();
    for e in entries
        .iter()
        .filter(|e| crate::sources::windows::arp::is_application(e))
    {
        let Some(row) = match_entry(db, e)? else {
            continue;
        };
        let have = e.display_version.clone().unwrap_or_default();
        if !version::newer(&have, &row.latest_version) {
            continue;
        }
        out.push(crate::model::Update {
            package: crate::model::PackageRef {
                source: SourceKind::Winget,
                id: row.id.clone(),
            },
            name: row.name.clone(),
            kind: crate::model::PackageKind::App,
            summary: None,
            icon: None,
            from: Some(have),
            to: row.latest_version.clone(),
            download_size: None,
            published: None,
            is_self: false,
        });
    }
    Ok(out)
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
        let detail = self.cached_detail();
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

    fn search(&self, query: &crate::Query) -> Result<Vec<crate::model::Package>> {
        let db = self.catalogue()?;
        let rows = query::search(&db, &query.text, query.limit)?;
        Ok(rows.iter().map(to_package).collect())
    }

    fn installed(&self) -> Result<Vec<crate::model::Package>> {
        #[cfg(windows)]
        {
            let db = self.catalogue()?;
            installed_from(&db, &crate::sources::windows::arp::read())
        }
        #[cfg(not(windows))]
        Ok(Vec::new())
    }

    fn updates(&self) -> Result<Vec<crate::model::Update>> {
        #[cfg(windows)]
        {
            let db = self.catalogue()?;
            updates_from(&db, &crate::sources::windows::arp::read())
        }
        #[cfg(not(windows))]
        Ok(Vec::new())
    }

    fn details(&self, id: &str) -> Result<crate::model::Package> {
        let db = self.catalogue()?;
        let row = query::by_id(&db, id)?.ok_or_else(|| {
            crate::Error::from_source(
                SourceKind::Winget,
                format!("{id} is not in the winget catalogue. The catalogue is a daily snapshot; a very new package may not be in it yet."),
            )
        })?;
        Ok(described_package(&row, &|url| {
            self.client.get_text_cached(url, MANIFEST_MAX_AGE)
        }))
    }

    fn plan(&self, op: &crate::model::Op) -> Result<Vec<Step>> {
        // Resolved once, here, where the environment is the user's own. The
        // elevated helper is handed the result and searches nothing.
        let program = winget_program();
        self.plan_with(op, &program)
    }
}

impl Winget {
    /// [`Source::plan`]'s body, with the resolved path to `winget.exe` taken
    /// as a parameter rather than looked up here. That is what lets a test
    /// drive every `Op` variant against a fixed path without winget being
    /// installed on the machine running the test; `plan` itself still
    /// resolves the program the normal way and is not otherwise changed.
    fn plan_with(&self, op: &crate::model::Op, program: &str) -> Result<Vec<Step>> {
        use crate::model::Op;
        let step = match op {
            Op::Install { package } if package.source == SourceKind::Winget => {
                operation_step(OpKind::Install, &package.id, program)
            }
            Op::Update { package } if package.source == SourceKind::Winget => {
                operation_step(OpKind::Update, &package.id, program)
            }
            Op::Remove { package } if package.source == SourceKind::Winget => {
                operation_step(OpKind::Remove, &package.id, program)
            }
            Op::UpdateAll { source } if *source == SourceKind::Winget => update_all_step(program),
            // Refresh is Brokey's own catalogue, not winget's, and `catalogue`
            // fetches it when it is stale. There is nothing to run.
            _ => return Ok(Vec::new()),
        };
        Ok(vec![step])
    }

    /// The catalogue's package count, read from whatever is already on disk.
    /// Never fetches: `status()` runs before every search, installed list,
    /// updates run and plan, and again whenever the page redraws its source
    /// list, so a stale or missing cache must not cost a multi-megabyte
    /// download, or, offline, the client's timeout. Answers `None` rather
    /// than downloading when there is no cached catalogue yet.
    fn cached_detail(&self) -> Option<String> {
        let path = index::cached_path(&self.client.download_dir());
        let db = index::open(&path).ok()?;
        let n = query::count(&db).ok()?;
        Some(format!("{n} packages"))
    }

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
        let hash_name = hash_asset_name(&bundle.name);
        let hash_asset = release
            .assets
            .iter()
            .find(|a| a.name == hash_name)
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

    /// A full path to winget, of the shape the caller hands over. It is not
    /// what `winget_program` answers on a real machine any more: that is
    /// the executable the App Execution Alias names, under
    /// `C:\Program Files\WindowsApps`. Nothing below reads the disk, so the
    /// shape is all these tests need.
    const WINGET: &str = r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\winget.EXE";

    /// Every operation is non-interactive, because the helper has no terminal
    /// and a prompt would hang the plan rather than ask anyone anything.
    #[test]
    fn every_operation_is_silent_and_pre_agreed() {
        for kind in [OpKind::Install, OpKind::Update, OpKind::Remove] {
            let s = operation_step(kind, "Valve.Steam", WINGET);
            let args = s.command.args.join(" ");
            assert!(args.contains("--silent"), "{args}");
            assert!(args.contains("--disable-interactivity"), "{args}");
            assert!(args.contains("--accept-source-agreements"), "{args}");
            assert_eq!(s.command.program, WINGET);
        }
    }

    /// A step carries the full path its caller resolved, not a bare name,
    /// and the closed list still recognises it. The helper searches for
    /// nothing, so a bare name would be refused there; this is the end that
    /// makes the path concrete.
    ///
    /// Windows only, because it asks `std::path` Windows questions. Off
    /// Windows a backslash is an ordinary character, `is_absolute` is false
    /// and `file_name` answers the whole string, so both assertions would
    /// fail on the machine most of this project's tests run on. The rest of
    /// this module stays ungated so the pure halves keep being tested there.
    #[cfg(windows)]
    #[test]
    fn a_step_carries_the_resolved_path() {
        let s = operation_step(OpKind::Install, "Valve.Steam", WINGET);
        assert!(
            std::path::Path::new(&s.command.program).is_absolute(),
            "{}",
            s.command.program
        );
        assert_eq!(
            std::path::Path::new(&s.command.program)
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase()),
            Some("winget.exe".to_string()),
            "the closed list compares the file name, so this must still be winget"
        );
    }

    /// An install names the package exactly. A near match would install
    /// something the user did not choose.
    #[test]
    fn an_install_is_exact_and_names_the_id() {
        let s = operation_step(OpKind::Install, "Valve.Steam", WINGET);
        assert_eq!(s.command.args[0], "install");
        assert!(s.command.args.contains(&"--exact".to_string()));
        assert!(s.command.args.contains(&"Valve.Steam".to_string()));
        assert_eq!(s.title, "Installing Valve.Steam");
    }

    /// The real release carries two files ending `.txt` whose names contain
    /// `DesktopAppInstaller`: the bundle's hash and the dependency archive's.
    /// They are different hashes, and an earlier version of this took whichever
    /// came first, which was the dependency one, so every setup failed
    /// verification against a bundle that was in fact correct.
    #[test]
    fn the_hash_that_is_chosen_belongs_to_the_bundle() {
        let names = [
            "DesktopAppInstallerPolicies.zip",
            "DesktopAppInstaller_Dependencies.json",
            "DesktopAppInstaller_Dependencies.txt",
            "DesktopAppInstaller_Dependencies.zip",
            "Microsoft.DesktopAppInstaller_8wekyb3d8bbwe.msixbundle",
            "Microsoft.DesktopAppInstaller_8wekyb3d8bbwe.txt",
        ];
        assert_eq!(
            hash_asset_name("Microsoft.DesktopAppInstaller_8wekyb3d8bbwe.msixbundle"),
            "Microsoft.DesktopAppInstaller_8wekyb3d8bbwe.txt"
        );
        assert!(names.contains(
            &hash_asset_name("Microsoft.DesktopAppInstaller_8wekyb3d8bbwe.msixbundle").as_str()
        ));
    }

    /// `Add-AppxPackage` has no `-LiteralPath` parameter, only `-Path`; asking
    /// for the wrong one fails at PowerShell's own parameter binding, before
    /// any error Brokey wrote. `Get-FileHash -LiteralPath` in the verify step
    /// is a different cmdlet, and does have that parameter, which is where
    /// the mistake came from.
    #[test]
    fn the_install_step_uses_the_parameter_add_appxpackage_actually_has() {
        let steps = bootstrap_steps(&bootstrap(), std::path::Path::new("C:/tmp"));
        let joined = steps[2].command.args.join(" ");
        assert!(joined.contains("-Path"), "{joined}");
        assert!(!joined.contains("-LiteralPath"), "{joined}");
    }

    /// Guards the fix for `status()` fetching the catalogue itself: it is
    /// called before every search, installed list, updates run and plan, and
    /// again whenever the page redraws its source list, so a stale or absent
    /// cache must never cost a multi-megabyte download (or, offline, the
    /// client's timeout). Against a client pointed at an empty cache
    /// directory, with no catalogue on disk, the detail must be `None` and
    /// the call must return at once rather than after a network round trip.
    #[test]
    fn status_never_fetches_the_catalogue() {
        let dir = tempfile::tempdir().unwrap();
        let w = Winget::new(Arc::new(crate::http::Client::new(dir.path().to_path_buf())));
        let start = std::time::Instant::now();
        assert_eq!(w.cached_detail(), None);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "took {:?}, which means it tried the network",
            start.elapsed()
        );
    }

    fn gimp_row() -> query::Row {
        query::Row {
            id: "GIMP.GIMP".to_string(),
            name: "GIMP".to_string(),
            moniker: Some("gimp".to_string()),
            latest_version: "3.2.4".to_string(),
            publisher: Some("thegimpteam".to_string()),
        }
    }

    fn gimp_manifest() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/winget/gimp.locale.yaml");
        std::fs::read_to_string(&path).expect("the fixture is checked in")
    }

    /// The detail page gets what the index never had. The address the fetch
    /// is asked for is checked too, because a package's manifest is the one
    /// thing here that could quietly become some other package's.
    #[test]
    fn a_detail_page_takes_what_the_manifest_says() {
        let asked = std::cell::RefCell::new(Vec::new());
        let p = described_package(&gimp_row(), &|url| {
            asked.borrow_mut().push(url.to_string());
            Ok(gimp_manifest())
        });
        assert_eq!(
            asked.into_inner(),
            [
                "https://raw.githubusercontent.com/microsoft/winget-pkgs/master/manifests/g/GIMP/GIMP/3.2.4/GIMP.GIMP.locale.en-US.yaml"
            ]
        );
        assert!(
            p.summary
                .is_some_and(|s| s.starts_with("GIMP is an acronym"))
        );
        assert_eq!(p.licence.as_deref(), Some("GPLv3"));
        assert_eq!(
            p.homepage.as_deref(),
            Some("https://www.gimp.org/downloads/")
        );
        assert_eq!(p.developer.as_deref(), Some("The GIMP Team"));
        assert!(p.categories.contains(&"image-editor".to_string()));
        // What the index already knew is still there.
        assert_eq!(p.version.as_deref(), Some("3.2.4"));
        assert!(
            p.facts
                .contains(&("Moniker".to_string(), "gimp".to_string()))
        );
    }

    /// A machine that is offline, a manifest that was never written, a
    /// GitHub that answers 404: the detail page still draws everything the
    /// catalogue knows. A missing description is not an error to show.
    ///
    /// The error below is shaped like a manifest on purpose. What a failed
    /// fetch carries is a sentence for the user, and reading it as the file
    /// that was not fetched would put that sentence on the detail page as
    /// though the publisher had written it.
    #[test]
    fn a_manifest_that_cannot_be_fetched_leaves_the_package_as_the_index_had_it() {
        let row = gimp_row();
        let p = described_package(&row, &|_| {
            Err(crate::Error::new(
                "License: could not reach raw.githubusercontent.com".to_string(),
            ))
        });
        assert_eq!(p, to_package(&row));
        assert_eq!(p.description, None);
        assert_eq!(p.homepage, None);
    }

    /// An id with no derivable address costs no request at all, rather than
    /// one that is certain to fail.
    #[test]
    fn an_id_with_no_derivable_address_is_never_fetched() {
        let mut row = gimp_row();
        row.id = "nodot".to_string();
        let asked = std::cell::Cell::new(0);
        let p = described_package(&row, &|_| {
            asked.set(asked.get() + 1);
            Ok(gimp_manifest())
        });
        assert_eq!(asked.get(), 0);
        assert_eq!(p, to_package(&row));
    }

    /// Search builds its rows with `to_package` and nothing else, so a page
    /// of twenty results costs no manifest fetches. This pins the half of
    /// that which is a fact about `to_package` rather than about the caller.
    #[test]
    fn a_search_row_carries_no_manifest_fields() {
        let p = to_package(&gimp_row());
        assert_eq!(p.summary, None);
        assert_eq!(p.description, None);
        assert_eq!(p.homepage, None);
        assert_eq!(p.licence, None);
        assert_eq!(p.developer, None);
        assert!(p.categories.is_empty());
    }

    /// Uninstall takes no package agreement. Nothing is being agreed to, and
    /// `winget uninstall` does not accept the flag at all, so passing it would
    /// fail the step rather than be ignored. Checked against winget 1.30.140.
    #[test]
    fn a_removal_does_not_accept_a_package_agreement() {
        let s = operation_step(OpKind::Remove, "Valve.Steam", WINGET);
        assert_eq!(s.command.args[0], "uninstall");
        assert!(
            !s.command
                .args
                .contains(&"--accept-package-agreements".to_string())
        );
    }

    /// Refreshing is Brokey's own catalogue, which `catalogue()` fetches when it
    /// is stale, and setting up is expanded by the planner from `setup()`.
    /// Neither is a `winget.exe` call, so neither plans one.
    #[test]
    fn refresh_and_setup_plan_nothing() {
        let w = Winget::new(crate::http::Client::shared());
        for op in [
            crate::model::Op::Refresh {
                source: SourceKind::Winget,
            },
            crate::model::Op::Setup {
                source: SourceKind::Winget,
            },
        ] {
            assert!(w.plan(&op).unwrap().is_empty(), "{op:?}");
        }
    }

    /// A plan for an operation this source has nothing to do with is empty, not
    /// an error. The store asks every source about every operation.
    #[test]
    fn an_operation_for_another_source_plans_nothing() {
        let w = Winget::new(crate::http::Client::shared());
        let op = crate::model::Op::Install {
            package: crate::model::PackageRef {
                source: SourceKind::Flatpak,
                id: "org.videolan.VLC".to_string(),
            },
        };
        assert!(w.plan(&op).unwrap().is_empty());
    }

    /// Updating everything winget can update is one step, not one per package.
    #[test]
    fn update_all_is_a_single_step() {
        let w = Winget::new(crate::http::Client::shared());
        let steps = w
            .plan(&crate::model::Op::UpdateAll {
                source: SourceKind::Winget,
            })
            .unwrap();
        assert_eq!(steps.len(), 1);
        assert!(steps[0].command.args.contains(&"--all".to_string()));
    }

    /// Ties `plan_with` to `allow`'s closed list, so a future producer added
    /// to its match is caught here rather than by someone remembering to
    /// grep for every place that builds a `winget.exe` command.
    ///
    /// Every `Op` variant is driven through, with the match below written
    /// with no wildcard arm: a variant added to `Op` later makes this fail
    /// to compile, which is what forces whoever adds it to also decide
    /// whether the step it produces needs a place in `ops` and, if the step
    /// needs root, that `check_step` actually admits it. Every step that
    /// comes back with `needs_root == true` is checked against an `Allowed`
    /// built the way the runner builds it, `with_registered_removals`,
    /// because that is what stands between the step and Administrator in
    /// the real path.
    ///
    /// `plan_with` takes the program as a parameter for exactly this: `plan`
    /// itself resolves `winget.exe` by searching the machine and then
    /// resolving the App Execution Alias it finds, which would make this
    /// test depend on winget being installed. The fixed path below is only
    /// the shape of a resolved program, and no such file needs to exist for
    /// this test: `plan_with` never touches the disk, and the gate admits a
    /// name under `C:\Users` that nothing unprivileged could create.
    #[cfg(windows)]
    #[test]
    fn every_step_plan_with_returns_passes_the_closed_list() {
        use crate::model::{Op, PackageRef};
        use crate::transaction::allow::{self, Allowed};

        const PROGRAM: &str = r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\winget.exe";
        let package = PackageRef {
            source: SourceKind::Winget,
            id: "Valve.Steam".to_string(),
        };
        let ops = [
            Op::Install {
                package: package.clone(),
            },
            Op::Remove {
                package: package.clone(),
            },
            Op::Update {
                package: package.clone(),
            },
            Op::UpdateAll {
                source: SourceKind::Winget,
            },
            Op::Refresh {
                source: SourceKind::Winget,
            },
            Op::Setup {
                source: SourceKind::Winget,
            },
        ];

        let w = Winget::new(crate::http::Client::shared());
        let allowed = Allowed::with_registered_removals();
        for op in &ops {
            // No wildcard: every current variant is named, so a variant
            // added to `Op` without a matching entry above fails to
            // compile here.
            match op {
                Op::Install { .. }
                | Op::Remove { .. }
                | Op::Update { .. }
                | Op::UpdateAll { .. }
                | Op::Refresh { .. }
                | Op::Setup { .. } => {}
            }
            for step in w.plan_with(op, PROGRAM).unwrap() {
                if step.needs_root {
                    assert_eq!(
                        allow::check_step(&step, &allowed),
                        Ok(()),
                        "plan_with({op:?}) produced a step the closed list refuses: {:?}",
                        step.command
                    );
                }
            }
        }
    }
}
