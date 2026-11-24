//! winget: the primary Windows source.
//!
//! Search comes from the catalogue in [`index`], which Brokey fetches itself,
//! so this source is searchable on a machine that has never had winget. The
//! operations are `winget.exe` steps, which need the tool, so `available`
//! tracks the tool and `searchable` tracks the catalogue.

pub mod index;
pub mod manifest;
pub mod normalise;
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
/// What setting winget up answers when Microsoft's release could not be
/// asked. [`Source::try_setup`] carries it, followed by what the network
/// said, so the page tells "not just now" apart from "not here".
pub const NO_BOOTSTRAP: &str = "winget is not installed, and the App Installer release could not be reached, \
     so Brokey cannot set it up just now. Brokey still searches winget's catalogue.";
/// What setting winget up answers when there is nothing to set up.
pub const ALREADY_INSTALLED: &str = "winget is already installed, so there is nothing to set up.";
pub const SETUP_LABEL: &str = "Install winget";
pub const SETUP_SENTENCE: &str = "The App Installer package and the packages it depends on are downloaded \
     from Microsoft, each checked against the hash Microsoft publishes with it, and installed for you \
     alone. No Administrator permission is needed.";

/// The file the dependency archive is published as, beside the bundle.
pub const DEPENDENCIES_ZIP: &str = "DesktopAppInstaller_Dependencies.zip";

/// What a `winget-cli` release says about its bundle and its dependencies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bootstrap {
    pub url: String,
    /// The SHA-256 Microsoft publishes in a `.txt` beside the bundle.
    pub sha256: String,
    pub version: String,
    /// [`DEPENDENCIES_ZIP`]: the framework packages App Installer needs
    /// (VCLibs, the UWP desktop VCLibs and the Windows App Runtime at the
    /// time of writing), one folder per architecture. A machine that has
    /// never had winget is usually a machine missing these too: Server,
    /// Sandbox, a stripped image. Without them `Add-AppxPackage` fails with
    /// `0x80073CF3`.
    pub dependencies_url: String,
    /// The SHA-256 Microsoft publishes in a `.txt` beside the archive.
    pub dependencies_sha256: String,
    /// The folder inside the archive for this machine: `x64`, `arm64` or
    /// `x86`. See [`dependency_folder`].
    pub arch: String,
}

/// The two release assets setting winget up downloads and the two hash
/// files that vouch for them, chosen from a release's asset list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootstrapAssets {
    pub bundle_url: String,
    pub bundle_hash_url: String,
    pub dependencies_url: String,
    pub dependencies_hash_url: String,
}

/// Choose the bundle, the dependency archive and the hash file for each
/// from a release's `(name, download url)` pairs. Pure, so the choice is
/// tested against the real release's names on any machine.
///
/// Each hash file is found by the relationship Microsoft maintains, the
/// asset's own name with its extension replaced by `.txt`, and never by a
/// predicate on the name: the release carries two `.txt` files whose names
/// contain `DesktopAppInstaller`, and they hold different hashes.
pub fn bootstrap_assets(assets: &[(String, String)]) -> Result<BootstrapAssets> {
    let url_of = |name: &str| {
        assets
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, url)| url.clone())
    };
    let bundle = assets
        .iter()
        .find(|(n, _)| n.ends_with(".msixbundle"))
        .ok_or_else(|| {
            crate::Error::from_source(
                SourceKind::Winget,
                "The App Installer release has no package in it, so winget cannot be set up.",
            )
        })?;
    let bundle_hash_url = url_of(&hash_asset_name(&bundle.0)).ok_or_else(|| {
        crate::Error::from_source(
            SourceKind::Winget,
            "The App Installer release publishes no hash for its package, so Brokey \
             will not install it.",
        )
    })?;
    let dependencies_url = url_of(DEPENDENCIES_ZIP).ok_or_else(|| {
        crate::Error::from_source(
            SourceKind::Winget,
            "The App Installer release has no dependency archive in it, so winget cannot be \
             set up on a machine that is missing those packages.",
        )
    })?;
    let dependencies_hash_url = url_of(&hash_asset_name(DEPENDENCIES_ZIP)).ok_or_else(|| {
        crate::Error::from_source(
            SourceKind::Winget,
            "The App Installer release publishes no hash for its dependency archive, so Brokey \
             will not install it.",
        )
    })?;
    Ok(BootstrapAssets {
        bundle_url: bundle.1.clone(),
        bundle_hash_url,
        dependencies_url,
        dependencies_hash_url,
    })
}

/// The folder of the dependency archive that holds this architecture's
/// packages, from `std::env::consts::ARCH` or Windows' own spelling. The
/// archive has `x64`, `arm64` and `x86`; `None` for anything else.
pub fn dependency_folder(arch: &str) -> Option<&'static str> {
    match arch.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => Some("x64"),
        "aarch64" | "arm64" => Some("arm64"),
        "x86" | "i386" | "i686" => Some("x86"),
        _ => None,
    }
}

/// The machine's own architecture rather than this process's: an x64
/// Brokey running emulated on an ARM64 machine gets the ARM64 App
/// Installer from the bundle, which needs the ARM64 frameworks.
#[cfg(windows)]
pub fn native_arch() -> &'static str {
    use windows_sys::Win32::System::SystemInformation::{
        IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64,
        IMAGE_FILE_MACHINE_I386,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};
    let mut process: IMAGE_FILE_MACHINE = 0;
    let mut native: IMAGE_FILE_MACHINE = 0;
    // SAFETY: the pseudo-handle from GetCurrentProcess needs no closing, and
    // both out-pointers are to locals that outlive the call.
    let ok = unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, &mut native) };
    if ok != 0 {
        match native {
            IMAGE_FILE_MACHINE_AMD64 => return "x86_64",
            IMAGE_FILE_MACHINE_ARM64 => return "aarch64",
            IMAGE_FILE_MACHINE_I386 => return "x86",
            _ => {}
        }
    }
    std::env::consts::ARCH
}

/// Where a failed request leaves setting winget up: [`NO_BOOTSTRAP`], then
/// what the network said, as a sentence.
pub fn unreachable(cause: &str) -> crate::Error {
    let cause = cause.trim();
    let message = if cause.is_empty() {
        NO_BOOTSTRAP.to_string()
    } else if cause.ends_with(['.', '!', '?']) {
        format!("{NO_BOOTSTRAP} {cause}")
    } else {
        format!("{NO_BOOTSTRAP} {cause}.")
    };
    crate::Error::from_source(SourceKind::Winget, message)
}

pub struct Winget {
    client: Arc<crate::http::Client>,
}

impl Winget {
    pub fn new(client: Arc<crate::http::Client>) -> Winget {
        Winget { client }
    }
}

/// Download both, check both, install. The check is a step of its own
/// because it has to be able to stop the plan: the Runner abandons a plan at
/// the first failing step, so nothing downloaded is unpacked or installed
/// unless both files matched the hashes Microsoft publishes for them.
///
/// The dependencies go in through `Add-AppxPackage -DependencyPath` rather
/// than an `Add-AppxPackage` each before the bundle. That is the form
/// Microsoft documents for App Installer, and it is the one that does not
/// fail on a machine that already has a newer copy of a framework: adding
/// an older framework on its own is refused with `0x80073D06`, while a
/// dependency that is already satisfied is simply not installed again.
pub fn bootstrap_steps(b: &Bootstrap, into: &Path) -> Vec<Step> {
    let file = into
        .join("Microsoft.DesktopAppInstaller.msixbundle")
        .display()
        .to_string();
    let zip = into.join(DEPENDENCIES_ZIP).display().to_string();
    let unpacked = into
        .join("DesktopAppInstaller_Dependencies")
        .display()
        .to_string();
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
    let curl = |to: &str, url: &str| {
        vec![
            "-L".to_string(),
            "--fail".to_string(),
            "--create-dirs".to_string(),
            "-o".to_string(),
            to.to_string(),
            url.to_string(),
        ]
    };
    let powershell = |script: String| {
        vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            script,
        ]
    };
    let check = |path: &str, hash: &str, what: &str| {
        format!(
            "if ((Get-FileHash -Algorithm SHA256 -LiteralPath '{}').Hash -ne '{}') \
             {{ Write-Error '{what} did not match the hash Microsoft publishes for it, \
             so nothing was installed.'; exit 1 }}",
            ps_quote(path),
            ps_quote(hash)
        )
    };
    vec![
        step(
            format!("Downloading App Installer {}", b.version),
            "curl.exe",
            curl(&file, &b.url),
            8,
        ),
        step(
            "Downloading the packages App Installer depends on".to_string(),
            "curl.exe",
            curl(&zip, &b.dependencies_url),
            6,
        ),
        step(
            "Checking what was downloaded".to_string(),
            "powershell.exe",
            powershell(format!(
                "{}; {}",
                check(&file, &b.sha256, "The App Installer package"),
                check(
                    &zip,
                    &b.dependencies_sha256,
                    "The archive of packages App Installer depends on"
                ),
            )),
            1,
        ),
        step(
            format!("Installing App Installer {}", b.version),
            "powershell.exe",
            // `Add-AppxPackage` has no `-LiteralPath` parameter, only
            // `-Path` (alias `PSPath`), unlike `Get-FileHash` above. Its
            // `-Path` is a plain string with no wildcard support, so
            // `ps_quote` is enough and no bracket escaping is needed. The
            // unpacked folder is emptied first so a package left by an older
            // release is never offered alongside the new one.
            powershell(format!(
                "$ErrorActionPreference = 'Stop'; \
                 $d = '{unpacked}'; \
                 if (Test-Path -LiteralPath $d) {{ Remove-Item -LiteralPath $d -Recurse -Force }}; \
                 Expand-Archive -LiteralPath '{zip}' -DestinationPath $d; \
                 $a = Join-Path $d '{arch}'; \
                 $deps = @(if (Test-Path -LiteralPath $a) {{ Get-ChildItem -LiteralPath $a -Filter *.appx | \
                 ForEach-Object {{ $_.FullName }} }}); \
                 if ($deps.Count -eq 0) {{ Write-Error 'The archive of packages App Installer \
                 depends on has none for {arch}, so nothing was installed.'; exit 1 }}; \
                 Add-AppxPackage -Path '{file}' -DependencyPath $deps",
                unpacked = ps_quote(&unpacked),
                zip = ps_quote(&zip),
                arch = ps_quote(&b.arch),
                file = ps_quote(&file),
            )),
            4,
        ),
    ]
}

/// The name of the `.txt` file Microsoft publishes beside an asset, holding
/// its SHA-256. This is the relationship Microsoft actually maintains: the
/// hash file is the asset's own name with its extension replaced by `.txt`,
/// for the bundle and for the dependency archive alike. The release carries
/// two `.txt` files whose names contain `DesktopAppInstaller`, which is why
/// a predicate on the name alone cannot tell the two apart. Pure, so it is
/// tested without a fixture and runs on Linux too.
pub fn hash_asset_name(asset_name: &str) -> String {
    match asset_name.rsplit_once('.') {
        Some((stem, _)) => format!("{stem}.txt"),
        None => format!("{asset_name}.txt"),
    }
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

/// The hash out of a published `.txt`, or the sentence that refuses it.
/// `what` names the file the hash is for, in the middle of a sentence.
pub fn published_hash(text: &str, what: &str) -> Result<String> {
    let hash = text.trim();
    if is_sha256(hash) {
        Ok(hash.to_string())
    } else {
        Err(crate::Error::from_source(
            SourceKind::Winget,
            format!(
                "The hash published with {what} is not a SHA-256, so Brokey will not install it."
            ),
        ))
    }
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
        // machine scope. An install whose manifest permits it is
        // `user_install_step` instead, and does not elevate.
        needs_root: true,
        weight: 10,
    }
}

/// An install for this user alone, where the package's installer manifest
/// permits it ([`manifest::permits_user_scope`]): [`operation_step`]'s
/// install with `--scope user`, and no Administrator. winget installs a
/// user-scope package without elevating, so the step runs in the user's own
/// session, which also puts it in the user's own profile rather than in
/// whichever administrator's profile answered the prompt.
///
/// It never needs root, so the helper's closed list never sees it; that
/// list still rebuilds only [`operation_step`] and [`update_all_step`].
pub fn user_install_step(id: &str, program: &str) -> Step {
    let mut step = operation_step(OpKind::Install, id, program);
    // After the id, where a reader of the command line would look for it.
    step.command
        .args
        .splice(4..4, ["--scope".to_string(), "user".to_string()]);
    step.needs_root = false;
    step
}

/// Whether `row`'s installer manifest permits a user-scope install. `fetch`
/// is how the manifest is read, as for [`described_package`]. Anything that
/// goes wrong (no derivable address, a failed fetch, a manifest that says
/// nothing) answers `false`, which keeps the machine install that always
/// worked: a missing manifest costs a prompt, never a failed install.
fn user_scope_permitted(row: &query::Row, fetch: &dyn Fn(&str) -> Result<String>) -> bool {
    manifest::installer_url(&row.id, &row.latest_version)
        .and_then(|url| fetch(&url).ok())
        .is_some_and(|text| manifest::permits_user_scope(&text))
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

/// `DISTINCT` because one package is stored under several forms of its name
/// (`python`, `python(X64)`, `python(X86)`), and two hits on one package are
/// still one package, not the ambiguity the caller refuses.
const BY_NAME_AND_PUBLISHER_SQL: &str = "
SELECT DISTINCT p.id, p.name, p.moniker, p.latest_version, np.norm_publisher
FROM packages p
JOIN norm_names2 nn ON nn.package = p.rowid AND nn.norm_name IN (:name, :name_x64, :name_x86)
JOIN norm_publishers2 np ON np.package = p.rowid AND np.norm_publisher = :publisher
LIMIT 2
";

/// The catalogue package an uninstall entry is, if it is one.
///
/// Three rungs, exact first. The product and upgrade code rungs are exact.
/// The name rung folds the display name and the publisher the way winget
/// folded the catalogue's ([`normalise`]), so `Advanced IP Scanner 2.5.1`
/// by `Famatech Corp.` meets `advancedipscanner` by `famatech`. It requires
/// the publisher to agree as well as the name, and then requires
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
    let [bare, x64, x86] = normalise::architecture_suffixes(&normalise::name(name));
    let publisher = normalise::publisher(publisher);
    if bare.is_empty() || publisher.is_empty() {
        return Ok(None);
    }
    let found = query::rows(
        db,
        BY_NAME_AND_PUBLISHER_SQL,
        &[
            (":name", &bare),
            (":name_x64", &x64),
            (":name_x86", &x86),
            (":publisher", &publisher),
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
            // Which Add/Remove Programs entry this is. `group.rs` joins the
            // two on it, because the names alone (`7-Zip` and
            // `7-Zip 26.03 (x64)`) share nothing it could join on.
            p.facts.push((
                crate::sources::windows::arp::REGISTRY_KEY_FACT.to_string(),
                crate::sources::windows::arp::package_id(e),
            ));
            out.push(p);
        }
    }
    Ok(out)
}

/// The uninstall entries the catalogue joins to the package `id`, through
/// the same [`match_entry`] that put the package on the Installed page, so
/// Open opens the application that row stands for and no other.
pub fn entries_for<'a>(
    db: &rusqlite::Connection,
    entries: &'a [crate::sources::windows::arp::RawEntry],
    id: &str,
) -> Result<Vec<&'a crate::sources::windows::arp::RawEntry>> {
    let mut out = Vec::new();
    for e in entries
        .iter()
        .filter(|e| crate::sources::windows::arp::is_application(e))
    {
        if match_entry(db, e)?.is_some_and(|row| row.id.eq_ignore_ascii_case(id)) {
            out.push(e);
        }
    }
    Ok(out)
}

/// Every installed package's id with the uninstall entry it was joined to,
/// worked out at most once every few seconds. The page asks what Open
/// would open for every installed package in one burst, and the join is a
/// catalogue query per uninstall entry, so doing it per package would be
/// thousands of queries where one pass is enough.
#[cfg(windows)]
fn joined(
    db: &rusqlite::Connection,
) -> Result<Arc<Vec<(String, crate::sources::windows::arp::RawEntry)>>> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    type Joined = Arc<Vec<(String, crate::sources::windows::arp::RawEntry)>>;
    static CACHE: Mutex<Option<(Instant, Joined)>> = Mutex::new(None);
    const FRESH: Duration = Duration::from_secs(5);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((when, list)) = cache.as_ref()
        && when.elapsed() < FRESH
    {
        return Ok(list.clone());
    }
    let mut list = Vec::new();
    for e in crate::sources::windows::arp::read()
        .into_iter()
        .filter(crate::sources::windows::arp::is_application)
    {
        if let Some(row) = match_entry(db, &e)? {
            list.push((row.id, e));
        }
    }
    let list = Arc::new(list);
    *cache = Some((Instant::now(), list.clone()));
    Ok(list)
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

/// [`Winget::fetch_catalogue`]'s body, with the download as a parameter so a
/// test can hand it the fixture or a failure. The age of what is on disk is
/// never asked here: deciding whether to fetch is the caller's business.
fn save_catalogue(path: &Path, download: &dyn Fn() -> Result<Vec<u8>>) -> Result<()> {
    let db = index::database_from_msix(&download()?)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, db)
        .map_err(|e| crate::Error::new(format!("The winget catalogue could not be saved: {e}.")))
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
        self.try_setup().ok()
    }

    /// Why there is no setup matters here, where it does not for most
    /// sources: "already installed" and "Microsoft could not be reached
    /// just now" are different sentences, and the second one is temporary.
    fn try_setup(&self) -> Result<Setup> {
        #[cfg(windows)]
        if winget_exe().is_some() {
            return Err(crate::Error::from_source(
                SourceKind::Winget,
                ALREADY_INSTALLED,
            ));
        }
        #[cfg(windows)]
        let arch = native_arch();
        #[cfg(not(windows))]
        let arch = std::env::consts::ARCH;
        let b = self.bootstrap(arch)?;
        Ok(Setup {
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

    /// Pressing refresh fetches the catalogue whatever its age. Otherwise
    /// [`Winget::catalogue`] only fetches it once a day, and a package
    /// published this morning would stay out of search and updates until
    /// tomorrow however often the user asked. A failure is the caller's line
    /// to report, and the copy already on disk goes on answering.
    fn refresh_index(&self) -> Result<()> {
        self.fetch_catalogue(&index::cached_path(&self.client.download_dir()))
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

    /// The Start Menu shortcut of the uninstall entry this package was
    /// joined to. Read from the catalogue already on disk: opening an
    /// application never downloads one.
    fn launcher(&self, id: &str) -> Option<crate::launch::Launch> {
        #[cfg(windows)]
        {
            use crate::sources::windows::shortcut;
            let path = index::cached_path(&self.client.download_dir());
            let db = index::open(&path).ok()?;
            let joined = joined(&db).ok()?;
            let entries: Vec<_> = joined
                .iter()
                .filter(|(row, _)| row.eq_ignore_ascii_case(id))
                .map(|(_, e)| e)
                .collect();
            shortcut::launch_for_entries(&entries, &shortcut::start_menu())
        }
        #[cfg(not(windows))]
        {
            let _ = id;
            None
        }
    }

    fn plan(&self, op: &crate::model::Op) -> Result<Vec<Step>> {
        // Resolved once, here, where the environment is the user's own. The
        // elevated helper is handed the result and searches nothing.
        let program = winget_program();
        // Only an install chooses its scope. An update or a removal acts on
        // what is already installed, and winget finds that itself.
        let for_user = match op {
            crate::model::Op::Install { package } if package.source == SourceKind::Winget => {
                self.permits_user_scope(&package.id)
            }
            _ => false,
        };
        self.plan_with(op, &program, for_user)
    }
}

impl Winget {
    /// [`Source::plan`]'s body, with the resolved path to `winget.exe` taken
    /// as a parameter rather than looked up here. That is what lets a test
    /// drive every `Op` variant against a fixed path without winget being
    /// installed on the machine running the test; `plan` itself still
    /// resolves the program the normal way and is not otherwise changed.
    ///
    /// `for_user` is whether the package's installer manifest permits a
    /// user-scope install, found by `plan` through the manifest and passed in
    /// for the same reason as the program.
    fn plan_with(&self, op: &crate::model::Op, program: &str, for_user: bool) -> Result<Vec<Step>> {
        use crate::model::Op;
        let step = match op {
            Op::Install { package } if package.source == SourceKind::Winget && for_user => {
                user_install_step(&package.id, program)
            }
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
            // Refresh is Brokey's own catalogue, not winget's: `catalogue`
            // fetches it when it is stale and `refresh_index` when asked,
            // neither of them a step. There is nothing to run.
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

    /// Whether the package's installer manifest permits a user-scope install.
    /// One fetch through the same cache the detail page's manifest uses, so
    /// a package whose page was just opened costs one more small file and a
    /// second install of it costs nothing.
    fn permits_user_scope(&self, id: &str) -> bool {
        let Ok(db) = self.catalogue() else {
            return false;
        };
        let Ok(Some(row)) = query::by_id(&db, id) else {
            return false;
        };
        user_scope_permitted(&row, &|url| {
            self.client.get_text_cached(url, MANIFEST_MAX_AGE)
        })
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
            self.fetch_catalogue(&path)?;
        }
        index::open(&path)
    }

    /// Download the catalogue and put it where [`Winget::catalogue`] reads
    /// it. Nothing on disk is touched until the download has arrived and
    /// unpacked, so a failure leaves the copy that was there answering.
    fn fetch_catalogue(&self, path: &Path) -> Result<()> {
        save_catalogue(path, &|| self.client.get_bytes(index::CATALOGUE_URL))
    }

    /// The newest App Installer release, the dependency archive beside it,
    /// and the hash published with each.
    fn bootstrap(&self, arch: &str) -> Result<Bootstrap> {
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
        let folder = dependency_folder(arch).ok_or_else(|| {
            crate::Error::from_source(
                SourceKind::Winget,
                format!(
                    "Microsoft publishes the packages App Installer depends on for x64, ARM64 \
                     and x86, and this machine is {arch}, so Brokey cannot set winget up here."
                ),
            )
        })?;
        let release: Release = self
            .client
            .get_json(WINGET_CLI_LATEST, &[])
            .map_err(|e| unreachable(&e.message))?;
        let pairs: Vec<(String, String)> = release
            .assets
            .into_iter()
            .map(|a| (a.name, a.browser_download_url))
            .collect();
        let chosen = bootstrap_assets(&pairs)?;
        let fetch = |url: &str| {
            self.client
                .get_text(url)
                .map_err(|e| unreachable(&e.message))
        };
        let sha256 = published_hash(
            &fetch(&chosen.bundle_hash_url)?,
            "the App Installer package",
        )?;
        let dependencies_sha256 = published_hash(
            &fetch(&chosen.dependencies_hash_url)?,
            "the archive of packages App Installer depends on",
        )?;
        Ok(Bootstrap {
            url: chosen.bundle_url,
            sha256,
            version: release.tag_name.trim_start_matches('v').to_string(),
            dependencies_url: chosen.dependencies_url,
            dependencies_sha256,
            arch: folder.to_string(),
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
            dependencies_url:
                "https://github.com/microsoft/winget-cli/releases/download/v1.29.290/\
                               DesktopAppInstaller_Dependencies.zip"
                    .to_string(),
            dependencies_sha256: "cd".repeat(32),
            arch: "x64".to_string(),
        }
    }

    /// Both downloads, then the check, then the install: the only order in
    /// which nothing downloaded is unpacked or installed before both hashes
    /// have matched.
    #[test]
    fn setting_winget_up_downloads_then_verifies_then_installs() {
        let steps = bootstrap_steps(&bootstrap(), std::path::Path::new("C:/tmp"));
        let titles: Vec<&str> = steps.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Downloading App Installer 1.29.290",
                "Downloading the packages App Installer depends on",
                "Checking what was downloaded",
                "Installing App Installer 1.29.290",
            ]
        );
        assert!(
            steps[1]
                .command
                .args
                .iter()
                .any(|a| a.ends_with(DEPENDENCIES_ZIP))
        );
        assert!(
            steps[1]
                .command
                .args
                .contains(&bootstrap().dependencies_url)
        );
        // Nothing before the check touches the archive except to write it.
        for s in &steps[..2] {
            assert_eq!(s.command.program, "curl.exe");
        }
    }

    /// Both published hashes reach the command that checks them, each beside
    /// its own file. Without this the verify step is decoration.
    #[test]
    fn the_verify_step_carries_both_published_hashes() {
        let b = bootstrap();
        let steps = bootstrap_steps(&b, std::path::Path::new("C:/tmp"));
        let joined = steps[2].command.args.join(" ");
        let bundle = joined
            .find(&b.sha256)
            .expect("the bundle's hash is in the command");
        let deps = joined
            .find(&b.dependencies_sha256)
            .expect("the archive's hash is in the command");
        let bundle_file = joined.find("msixbundle").unwrap();
        let deps_file = joined.find(DEPENDENCIES_ZIP).unwrap();
        assert!(
            bundle_file < bundle && bundle < deps_file && deps_file < deps,
            "{joined}"
        );
        assert_eq!(joined.matches("exit 1").count(), 2, "{joined}");
    }

    /// The install takes this machine's folder out of the archive and hands
    /// every package in it to `Add-AppxPackage` as a dependency of the
    /// bundle, and stops if the folder is empty rather than installing a
    /// bundle that is known to fail.
    #[test]
    fn the_install_step_adds_this_architectures_dependencies_with_the_bundle() {
        let mut b = bootstrap();
        b.arch = "arm64".to_string();
        let joined = bootstrap_steps(&b, std::path::Path::new("C:/tmp"))[3]
            .command
            .args
            .join(" ");
        assert!(
            joined.contains("$ErrorActionPreference = 'Stop'"),
            "{joined}"
        );
        assert!(joined.contains("Expand-Archive"), "{joined}");
        assert!(joined.contains("'arm64'"), "{joined}");
        assert!(!joined.contains("x64"), "{joined}");
        assert!(joined.contains("-DependencyPath $deps"), "{joined}");
        assert!(joined.contains("$deps.Count -eq 0"), "{joined}");
        let unpack = joined.find("Expand-Archive").unwrap();
        let add = joined.find("Add-AppxPackage").unwrap();
        assert!(unpack < add, "{joined}");
    }

    /// The archive has three folders. Every spelling of each that reaches
    /// this function picks one; anything else is refused rather than
    /// guessed, because a wrong guess installs frameworks that cannot load.
    #[test]
    fn the_dependency_folder_follows_the_machine() {
        for (arch, folder) in [
            ("x86_64", "x64"),
            ("AMD64", "x64"),
            ("aarch64", "arm64"),
            ("ARM64", "arm64"),
            ("x86", "x86"),
        ] {
            assert_eq!(dependency_folder(arch), Some(folder), "{arch}");
        }
        assert_eq!(dependency_folder("riscv64"), None);
        assert_eq!(dependency_folder(""), None);
    }

    /// The real release's asset list, as `api.github.com` gave it for
    /// v1.29.380. The four files setting up needs are chosen, and each hash
    /// is the one beside its own file.
    fn release_assets() -> Vec<(String, String)> {
        [
            "DesktopAppInstallerPolicies.zip",
            "DesktopAppInstaller_Dependencies.json",
            "DesktopAppInstaller_Dependencies.txt",
            "DesktopAppInstaller_Dependencies.zip",
            "e53e159d00e04f729cc2180cffd1c02e_License1.xml",
            "Microsoft.DesktopAppInstaller_8wekyb3d8bbwe.msixbundle",
            "Microsoft.DesktopAppInstaller_8wekyb3d8bbwe.txt",
        ]
        .into_iter()
        .map(|n| (n.to_string(), format!("https://example.invalid/{n}")))
        .collect()
    }

    #[test]
    fn the_release_gives_the_bundle_the_archive_and_a_hash_for_each() {
        let chosen = bootstrap_assets(&release_assets()).unwrap();
        assert_eq!(
            chosen,
            BootstrapAssets {
                bundle_url:
                    "https://example.invalid/Microsoft.DesktopAppInstaller_8wekyb3d8bbwe.msixbundle"
                        .to_string(),
                bundle_hash_url:
                    "https://example.invalid/Microsoft.DesktopAppInstaller_8wekyb3d8bbwe.txt"
                        .to_string(),
                dependencies_url: "https://example.invalid/DesktopAppInstaller_Dependencies.zip"
                    .to_string(),
                dependencies_hash_url:
                    "https://example.invalid/DesktopAppInstaller_Dependencies.txt".to_string(),
            }
        );
    }

    /// A release missing either file, or either hash, is refused with a
    /// sentence saying which. Installing the bundle without its archive is
    /// what this change exists to stop, so a missing archive is not skipped.
    #[test]
    fn a_release_missing_a_file_or_a_hash_says_which() {
        for (missing, says) in [
            (
                "Microsoft.DesktopAppInstaller_8wekyb3d8bbwe.msixbundle",
                "no package",
            ),
            (
                "Microsoft.DesktopAppInstaller_8wekyb3d8bbwe.txt",
                "no hash for its package",
            ),
            (
                "DesktopAppInstaller_Dependencies.zip",
                "no dependency archive",
            ),
            (
                "DesktopAppInstaller_Dependencies.txt",
                "no hash for its dependency archive",
            ),
        ] {
            let assets: Vec<_> = release_assets()
                .into_iter()
                .filter(|(n, _)| n != missing)
                .collect();
            let err = bootstrap_assets(&assets).unwrap_err();
            assert!(err.message.contains(says), "{missing}: {}", err.message);
            assert!(err.message.ends_with('.'), "{}", err.message);
            assert_eq!(err.source_kind, Some(SourceKind::Winget));
        }
    }

    /// What the page reads when Microsoft could not be asked: that it is
    /// temporary, then the network's own words, ending as a sentence.
    #[test]
    fn an_unreachable_release_says_not_just_now_and_why() {
        let err = unreachable("api.github.com did not answer in time");
        assert!(err.message.starts_with(NO_BOOTSTRAP), "{}", err.message);
        assert!(err.message.contains("just now"), "{}", err.message);
        assert!(
            err.message
                .ends_with("api.github.com did not answer in time."),
            "{}",
            err.message
        );
        assert_eq!(unreachable("  ").message, NO_BOOTSTRAP);
        assert_eq!(err.source_kind, Some(SourceKind::Winget));
    }

    #[test]
    fn a_published_hash_is_trimmed_and_anything_else_is_refused() {
        let hash = "BA875AFE9D190F61218985AC0292A99D1DB710BF93E13C68944CA9D89F0D82D1";
        assert_eq!(published_hash(&format!("{hash}\r\n"), "x").unwrap(), hash);
        let err = published_hash("<html>404</html>", "the archive").unwrap_err();
        assert_eq!(
            err.message,
            "The hash published with the archive is not a SHA-256, so Brokey will not install it."
        );
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
        let joined = bootstrap_steps(&b, std::path::Path::new("C:/tmp"))[2]
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
        for i in [2, 3] {
            let joined = steps[i].command.args.join(" ");
            assert!(joined.contains("O''Brien"), "step {i}: {joined}");
            // Every apostrophe in the command is either one of a pair that
            // opens and closes a literal, or a doubled one from the data.
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
        let joined = steps[3].command.args.join(" ");
        assert!(joined.contains("Add-AppxPackage -Path '"), "{joined}");
        assert!(!joined.contains("Add-AppxPackage -LiteralPath"), "{joined}");
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

    fn fixture_msix() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/windows/winget/source2.msix"
        ))
        .unwrap()
    }

    /// What pressing refresh does: the catalogue is replaced however recently
    /// it was written. The copy below was written a moment ago, which is as
    /// fresh as a file can be, and is replaced anyway.
    #[test]
    fn a_refresh_replaces_even_a_fresh_catalogue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("winget").join("index.db");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"yesterday").unwrap();
        save_catalogue(&path, &|| Ok(fixture_msix())).unwrap();
        let db = index::open(&path).unwrap();
        assert_eq!(query::count(&db).unwrap(), 9);
    }

    /// A refresh that cannot download leaves what was there, so search and
    /// updates go on answering from yesterday's catalogue, and says why.
    #[test]
    fn a_failed_refresh_leaves_the_catalogue_it_had() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        std::fs::write(&path, b"what was there").unwrap();
        let failed = save_catalogue(&path, &|| {
            Err(crate::Error::new(
                "Microsoft could not be reached.".to_string(),
            ))
        });
        assert!(failed.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"what was there");
        // Nor does a download that is not a catalogue overwrite it.
        assert!(save_catalogue(&path, &|| Ok(b"<html>".to_vec())).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"what was there");
    }

    /// The real catalogue, fetched by `refresh_index` into an empty cache.
    #[test]
    #[ignore]
    fn live_refresh_index_fetches_the_catalogue() {
        let dir = tempfile::tempdir().unwrap();
        let w = Winget::new(Arc::new(crate::http::Client::new(dir.path().to_path_buf())));
        w.refresh_index().unwrap();
        let db = index::open(&index::cached_path(&w.client.download_dir())).unwrap();
        assert!(query::count(&db).unwrap() > 10_000);
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

    fn obsidian_row() -> query::Row {
        query::Row {
            id: "Obsidian.Obsidian".to_string(),
            name: "Obsidian".to_string(),
            moniker: Some("obsidian".to_string()),
            latest_version: "1.15.1".to_string(),
            publisher: Some("obsidian".to_string()),
        }
    }

    fn installer_fixture(name: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/winget")
                .join(name),
        )
        .expect("the fixture is checked in")
    }

    /// A manifest with a user installer for every architecture gives an
    /// install for this user alone, which does not elevate. The address
    /// asked for is the installer manifest of the catalogue's version.
    #[test]
    fn a_package_whose_manifest_permits_it_installs_for_the_user() {
        let asked = std::cell::RefCell::new(Vec::new());
        let permitted = user_scope_permitted(&obsidian_row(), &|url| {
            asked.borrow_mut().push(url.to_string());
            Ok(installer_fixture("obsidian.installer.yaml"))
        });
        assert!(permitted);
        assert_eq!(
            asked.into_inner(),
            [
                "https://raw.githubusercontent.com/microsoft/winget-pkgs/master/manifests/o/Obsidian/Obsidian/1.15.1/Obsidian.Obsidian.installer.yaml"
            ]
        );
        let w = Winget::new(crate::http::Client::shared());
        let op = crate::model::Op::Install {
            package: crate::model::PackageRef {
                source: SourceKind::Winget,
                id: "Obsidian.Obsidian".to_string(),
            },
        };
        let steps = w.plan_with(&op, WINGET, permitted).unwrap();
        assert_eq!(steps.len(), 1);
        assert!(!steps[0].needs_root, "a user install never elevates");
        assert_eq!(
            steps[0].command.args[..6],
            [
                "install",
                "--exact",
                "--id",
                "Obsidian.Obsidian",
                "--scope",
                "user"
            ]
        );
        assert!(
            steps[0]
                .command
                .args
                .contains(&"--disable-interactivity".to_string())
        );
    }

    /// And a machine-scope manifest, a fetch that fails or a manifest that
    /// is not one keep the install that always worked: elevated, no scope.
    #[test]
    fn anything_short_of_permission_keeps_the_machine_install() {
        let row = obsidian_row();
        assert!(!user_scope_permitted(&row, &|_| Ok(installer_fixture(
            "7zip.installer.yaml"
        ))));
        assert!(!user_scope_permitted(&row, &|_| {
            Err(crate::Error::new(
                "GitHub could not be reached.".to_string(),
            ))
        }));
        assert!(!user_scope_permitted(&row, &|_| Ok(
            "<html>404</html>".to_string()
        )));
        let w = Winget::new(crate::http::Client::shared());
        let op = crate::model::Op::Install {
            package: crate::model::PackageRef {
                source: SourceKind::Winget,
                id: "7zip.7zip".to_string(),
            },
        };
        let steps = w.plan_with(&op, WINGET, false).unwrap();
        assert!(steps[0].needs_root);
        assert!(!steps[0].command.args.contains(&"--scope".to_string()));
    }

    /// Only an install chooses a scope; an update or a removal acts on what is
    /// installed, whatever `for_user` says.
    #[test]
    fn only_an_install_takes_the_user_scope() {
        let w = Winget::new(crate::http::Client::shared());
        let package = crate::model::PackageRef {
            source: SourceKind::Winget,
            id: "Obsidian.Obsidian".to_string(),
        };
        for op in [
            crate::model::Op::Update {
                package: package.clone(),
            },
            crate::model::Op::Remove { package },
        ] {
            let steps = w.plan_with(&op, WINGET, true).unwrap();
            assert!(steps[0].needs_root, "{op:?}");
            assert!(
                !steps[0].command.args.contains(&"--scope".to_string()),
                "{op:?}"
            );
        }
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
    /// an error. The store never asks for one: `transaction::plan`'s `Gatherer`
    /// looks the operation's own source up and asks that source alone. What
    /// this pins is the trait's contract, which a caller holding a `dyn Source`
    /// relies on.
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
                | Op::Setup { .. }
                | Op::ClearLeftovers { .. }
                | Op::Restore { .. }
                | Op::EmptyHolding { .. }
                | Op::ForceRemove { .. } => {}
            }
            for step in [false, true]
                .into_iter()
                .flat_map(|for_user| w.plan_with(op, PROGRAM, for_user).unwrap())
            {
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
