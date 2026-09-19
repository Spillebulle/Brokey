# Windows support: design

Dated 2026-09-19. What Brokey on Windows is, which sources it reads, how it
elevates, what changes in `brokey-core`, and what was rejected.

This is the first of two specs. It covers the platform port: sources,
privilege, metadata, the page, packaging. The deep uninstall (leftover files,
registry residue, force delete, undo) is a second spec written after this one
lands, because it needs the Add/Remove Programs source and the elevated helper
that this spec puts in place. Where a decision here exists to make that second
spec possible, it says so.

`docs/architecture.md` is still the design for the whole application. This
spec extends it and supersedes its "Targets" row. Nothing in it re-litigates a
rule that holds on both platforms.

## What it is for

The same sentence as the Linux side, on a different machine: one search across
every place a Windows machine gets software, one install flow, one updates
page, and an application that keeps itself up to date through the same
machinery. A Windows machine gets software from winget, the Microsoft Store,
Chocolatey, Scoop, GitHub releases, and from installers downloaded and run by
hand, which is the largest category and the one no package manager records.

The reference machine is Windows 11 Pro 26H1, build 28120.2738, x86-64. Unlike the Linux
development machine it can open the window, because Tauri on Windows is
WebView2 and needs no `webkit2gtk-4.1`. The page can be developed against the
real backend for the first time rather than against `mock.ts`.

## Decisions

| | |
|---|---|
| Shape | One workspace, one library. `sources/windows/` beside `sources/linux/`, selected by `#[cfg]`. No second repository, no fork |
| Targets | Linux x86-64 and ARM64, **and Windows x86-64 and ARM64**. Supersedes the "Targets" row of `docs/architecture.md` |
| Sources | winget, Add/Remove Programs, Chocolatey, Scoop, Microsoft Store and MSIX, GitHub releases, Windows optional features |
| Privilege | The window never elevates. `brokey-helper.exe` is elevated per plan through `ShellExecuteEx` with `runas`, and the Plan and its Events travel over a named pipe because that call cannot redirect standard streams |
| Scope preference | Per-user wherever a source offers it, so the common install raises no prompt at all. Elevation only when the package forces it, and the confirm dialog says which package did |
| Package databases | Read directly, in pure Rust, as on Linux: winget's pre-indexed SQLite index, Chocolatey's `.nuspec` files, Scoop's bucket JSON, the uninstall registry. No shelling out to a tool to ask it what it knows |
| Metadata | No AppStream on Windows. A ladder ending in a Flathub AppStream lookup matched by name, carrying `confidence < 1` and labelled in the interface |
| Bundler | MSI through the Tauri bundler, then a winget manifest, a Scoop manifest and a Chocolatey package |
| Licence, hue, id | Unchanged: GPL-3.0-or-later, hue 300, `io.github.spillebulle.brokey` |

## The shape

### What splits and what does not

`brokey-core` stays one library. Three groups:

**Untouched, platform neutral.** `model.rs`, `group.rs`, `transaction/plan.rs`,
`updates.rs`, `http.rs`, `appstream/`. That these need no change is the
measure of whether the original boundaries were drawn correctly. If any of
them acquires a `#[cfg]`, the boundary was wrong and the fix belongs there
rather than in a conditional.

**Split by platform.** `system.rs`, `launch.rs`, `transaction/runner.rs`,
`transaction/allow.rs`, `selfupdate/install.rs`, and `vercmp.rs`, which becomes
`version/` with a comparator per family.

**New.** `sources/windows/`: `winget.rs`, `arp.rs`, `choco.rs`, `scoop.rs`,
`msix.rs`, `features.rs`. The nine existing sources move to `sources/linux/`,
which is a file move and a `mod` list, not a rewrite.

### SourceKind keeps every variant on both platforms

Only `sources::all()` is `#[cfg]`-selected. The enum itself carries Pacman,
Aur, Flatpak, Snap, Apt, Dnf, Github, Fwupd, Chwd, Winget, Arp, Choco, Scoop,
Msix and Features on every build.

This is deliberate and is the one place a conditional would have been cheaper.
If the enum shrank per platform, the serde words and `frontend/src/types.ts`
would differ per build, and `crates/brokey/tests/contract.rs` could only ever
check the half it was compiled for. That is the exact failure that shipped a
self-update button in 0.1.0 that never appeared: two sides spelling a tag
differently, each testing its own. `contract.rs` checks every variant on
either platform in one run.

### SystemInfo gains a platform field

`SystemInfo` keeps its shape so `types.ts` stays stable, and gains one field:

```rust
pub enum Platform { Linux, Windows }
```

On Windows it is filled from `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion`:
`distro_id` is `"windows"`, `distro_like` is empty, and `pretty_name` is the
edition, the display version and the build, as in
`Windows 11 Pro 26H1 (build 28120.2738)`. The page needs the field because the
nav swaps one item and the status bar names the machine differently; it must
not infer the platform from which sources happen to be present.

**`ProductName` in that key is not to be trusted and is never read.** On the
reference machine, which is Windows 11, it says `Windows 10 Pro`. Microsoft
never updated it, and a great deal of software reports the wrong operating
system because of it. The version is decided from `CurrentBuild`, where 22000
and above is Windows 11, and the name is composed from `EditionID`,
`DisplayVersion` and `CurrentBuild` with `UBR`. This has a fixture and a test,
because it is the kind of thing that looks like a one-line registry read and
is wrong on every machine.

### The sources

Each answers `status()` with a reason when it cannot work, as on Linux. None
of them runs anything: `plan()` returns steps and the Runner executes them.

**winget.** The primary source. Search reads the pre-indexed database that
Microsoft publishes at `https://cdn.winget.microsoft.com/cache`, which Brokey
fetches and caches itself rather than relying on winget having refreshed its
own copy. `source.msix` is 20.5 MB and `source2.msix` is 3.6 MB; both are
plain zip archives wrapping a SQLite `index.db`. The spec does not fix which
of the two to read; the implementation opens both once and takes the one whose
schema it can read, and records the answer here.

Reading the index rather than parsing `winget search` output is the same
decision as reading the pacman sync tarball rather than parsing `pacman -Ss`,
for the same reasons: it is faster, it is not a human-readable format that can
be reformatted under us, and it works when the tool is missing. That last
point matters: it makes winget `searchable` on a machine that does not have
it, which is the rule Flatpak and Snap already follow.

Install, remove and update are `winget.exe` steps, with `--scope user` where
the manifest permits it, and `--silent --accept-package-agreements
--accept-source-agreements` throughout.

**Add/Remove Programs.** The three uninstall keys, `HKLM`, `HKLM\WOW6432Node`
and `HKCU`. This source answers `installed()` and `plan(Remove)` only: the
registry records what is on the machine and has no notion of a newer version,
so it never searches and never reports an update. It exists because most
Windows software belongs to no package manager, and because the second spec
operates entirely on what it returns.

Its filter is load-bearing rather than cosmetic. On the reference machine, of
343 keys, 317 carry a `DisplayName` and 160 of those are `SystemComponent=1`:
runtimes and redistributables that Settings itself hides. The source drops
`SystemComponent`, entries with a `ParentKeyName`, and `ReleaseType` values of
`Security Update`, `Update` and `Hotfix`, leaving roughly 157 applications.
Like `group.rs`, this is a pure function of a set of registry values, so it is
tested against an exported hive checked into fixtures, and a heuristic added
to it means a fixture where it fires and one where it must not.

**Chocolatey.** Installed state from `C:\ProgramData\chocolatey\lib\<id>\<id>.nuspec`,
which is XML and parses with the `quick-xml` already in the workspace. Search
against the community OData feed. Install, remove and update are `choco` steps.

**Scoop.** Buckets are JSON manifests on disk under `~/scoop/buckets`;
installed applications are directories under `~/scoop/apps` with a
`manifest.json` and a `current` junction. Never elevates, ever. When scoop is
absent, search fetches the bucket JSON from GitHub directly.

**Microsoft Store and MSIX.** Installed applications from the Appx state,
removal through the Appx API, and installation through winget's `msstore`
source, so the source depends on winget being present. The only Windows source
carrying real icons and screenshots, which the metadata ladder below leans on.

**GitHub releases.** The existing source. Its asset selection learns Windows:
prefer `.msi`, then `-setup.exe` or `-installer.exe`, then a bare `.exe`, then
a `.zip` treated as portable and unpacked into the user's local application
directory. Architecture matching gains `x64`, `amd64`, `arm64` and `win`.

**Windows optional features.** Hyper-V, WSL, the OpenSSH client, .NET
Framework 3.5, Windows Sandbox: software already on the machine that is
switched on rather than downloaded. Listing is a read-only
`DISM /Online /Get-Features` query, which is the shape `fwupd.rs` already has
for `fwupdmgr`. Enabling and disabling are steps through the helper. It takes
the nav slot the Drivers page occupies on Linux, because `chwd` and `fwupd`
have no meaning here and Windows Update manages drivers itself.

### Grouping has a stronger key on Windows than on Linux

`group.rs` does not change. What changes is the quality of what it is given.

On Linux the join key is the AppStream component id where both sides have one,
and a normalised name match with a confidence score where they do not. On
Windows there is a genuinely strong key: winget manifests carry
`AppsAndFeaturesEntries`, holding the MSI `ProductCode`, the `UpgradeCode` and
the ARP `DisplayName`. This is how `winget list` reports an available upgrade
for software installed outside winget, and it is exact rather than heuristic.
204 of the reference machine's 317 keys are ProductCode-shaped GUIDs.

So the Windows ladder is: ProductCode or UpgradeCode, then `DisplayName` plus
`Publisher` normalised, then name similarity. Only the last rung produces
`confidence < 1`, so Windows results are less speculatively grouped than Linux
ones, not more. The rule that grouping never lies is unchanged: an edition
joined by name still says "matched by name" in `text-dim`, and can still be
split from the detail page.

### Metadata and icons

Windows has no AppStream and nothing equivalent. winget manifests carry a
name, publisher, description, tags and a homepage, and an `Icons` field that
exists in the schema and is almost never filled in. Chocolatey's `.nuspec` has
an `iconUrl` that is often a dead link. Scoop manifests have a description and
nothing more. Only the Store catalogue carries real artwork.

Installed applications are the exception and need no help: the `DisplayIcon`
value in an uninstall key points at an `.exe` or `.ico`, optionally with a
resource index, and a genuine icon is extracted from it. The Installed page
therefore looks correct with no network at all.

Search results use a ladder, first match wins:

1. The Store catalogue, for anything it knows.
2. winget's `Icons`, then Chocolatey's `iconUrl`.
3. The extracted icon, when the application is already installed.
4. A Flathub AppStream lookup for the same application, matched by name.
5. The `control` block with the first letter, as specified in §7.

Rung 4 is the one that needs a rule. Firefox, GIMP, VLC and Blender are the
same software on both platforms, and Brokey already has the AppStream parser
and the Flathub client, so the artwork is free. It is also a guess. An icon
obtained that way carries the same `confidence < 1` that a name-matched
edition carries, and the detail page says "artwork matched by name" in
`text-dim` beside it. Never presented as certain.

**Screenshots are not taken from rung 4 in v1.** An icon matched to the wrong
application is a small wrongness; a screenshot gallery of the wrong
application is a large one. Screenshots come from the Store catalogue or from
nowhere until the match confidence has been measured against real data.

### Privilege

The window never elevates. That invariant is unchanged and is the reason the
rest of this section is as involved as it is.

`brokey-helper.exe` is a separate executable, as on Linux, reading one JSON
Plan, validating it against a closed list, running it, and streaming one JSON
Event per line. What changes is only how it is started and how it is spoken to.

Elevation on Windows is `ShellExecuteEx` with the `runas` verb. That call
**cannot redirect standard streams**, and `CreateProcess`, which can, cannot
elevate. So before elevating, the unelevated side creates a named pipe whose
DACL admits the current user and the Administrators group and nobody else,
passes the pipe's name as a command-line argument, and waits for the helper to
connect. The Plan goes down it and Events come back up it. From
`runner.rs`'s point of view the contract is unchanged: it holds a writer and a
line reader, and `CANCEL_LINE` still stops the helper between steps.

`allow.rs` gains a second closed list, cfg-selected, with the same test that
the helper refuses anything not on it.

There is no equivalent of polkit's `auth_admin_keep`, so an elevation cannot
be held across processes. The Linux rule, one helper run per maximal stretch
of consecutive root steps, is kept as written, and each stretch costs one UAC
prompt. Two consequences, both handled by machinery that already exists: the
confirm dialog says how many times permission will be asked when a plan has
more than one stretch, and `Event::AuthRequired` is emitted before each, so
the window can say what it is waiting for while the secure desktop is up
rather than appearing to have hung.

Per-user first is what keeps that count at zero in the common case. Scoop
never elevates, winget takes `--scope user` where the manifest allows it, and
Chocolatey can install to a user directory for some packages. A plan that must
elevate names the package that forced it.

### Setting a manager up from inside Brokey

Every Windows source that can be absent implements `Source::setup`, and the
planner expands `Op::Setup` through it with no new machinery, exactly as it
does for Flatpak and snapd today. An absent manager is `searchable`, so its
results sit beside everything else, and installing one of them sets the tool
up first in the same plan, with the confirm dialog saying so in one sentence.

| Source | Searchable without its tool | Setup | Elevates |
|---|---|---|---|
| winget | Yes, Brokey reads the CDN index itself | The App Installer `.msixbundle` and its dependency bundle, through `Add-AppxPackage` | No, per-user |
| Chocolatey | Yes, the community OData feed | `chocolatey.nupkg`, extracted to `C:\ProgramData\chocolatey`, then its bundled install script | Yes |
| Scoop | Yes, bucket JSON from GitHub | The pinned installer script, into `~/scoop` | No, per-user |
| Store and MSIX | Yes, the Store catalogue | Nothing of its own: `Op::Setup { winget }` first | No |
| Add/Remove Programs, Features | Always present | None | |

Two things follow. **Store's setup chains through winget's**: its `setup().ops`
carries `Op::Setup { Winget }` and its own steps come after, which is
structurally what Snap already does when it installs snapd through the
distribution's source before enabling its socket. The planner handles it
unchanged, because plan steps already keep the order of the operations that
produced them. And **only Chocolatey's bootstrap needs Administrator**, which
is what makes per-user-first worth having.

The part that is not a straight port is trust. On Linux, setting Flatpak up is
`pacman -S flatpak`, and the distribution's signature check is the
verification; Brokey gets it for free. The documented install for both
Chocolatey and Scoop is a script piped from a URL into a shell, elevated in
Chocolatey's case, which is the thing a closed list exists to prevent. So this
spec adds one invariant, stated in full below: nothing downloaded is run
before it is verified.

It is enforceable for all three. Microsoft publishes a 64-byte `.txt` beside
`Microsoft.DesktopAppInstaller_8wekyb3d8bbwe.msixbundle` in each `winget-cli`
release, and it is the bundle's SHA-256. Chocolatey's `choco.exe`, once the
`.nupkg` is extracted, is Authenticode-signed by Chocolatey Software, Inc.
Scoop's installer is pinned to a specific commit of `ScoopInstaller/Install`
rather than fetched through the redirecting `get.scoop.sh`. The helper's
closed list then allows a script at a path inside a directory Brokey controls
and only Administrators can write, which is a smaller grant than the four
command spellings the Linux list already carries.

### Launching an installed application

The Linux invariant is that `Source::launcher` finds the desktop entry in the
package's own file list and never parses `Exec`. Its Windows counterpart is
that Brokey launches the shortcut the installer created and never guesses the
binary.

`launcher` resolves, in order: a `.lnk` under the machine or user Start Menu
whose target lies inside the application's install directory; for an MSIX
package, `shell:AppsFolder\<AUMID>`; for Scoop, the shim in `~/scoop/shims`.
`DisplayIcon` is used for the icon and never as the launch target, because it
frequently points at an uninstaller or a resource-only file. Nothing found
means no button, as on Linux.

The install directory is not read from `InstallLocation`. **221 of the
reference machine's 317 entries leave it empty**, including every NSIS-built
application on the box, so it is derived from the directory of the
uninstaller named in `UninstallString`, and `InstallLocation` is used only to
confirm it. The second spec depends on this being right, and this is where it
is established and tested.

### Removal, and the number that shapes it

`plan(Remove)` for an Add/Remove Programs entry takes the first of three
routes that applies: `QuietUninstallString` where there is one;
`msiexec /x {ProductCode} /qn` where the key is named by an MSI ProductCode;
and otherwise `UninstallString`, which opens the publisher's own uninstaller.

The two silent routes have to be counted together, and they overlap much less
than either suggests alone. 64 of the reference machine's 317 entries carry a
`QuietUninstallString` and 204 are named by an MSI ProductCode GUID; 23 are
both, so between them the two routes cover 64 + 204 - 23 = **245, leaving 72
that cannot be removed silently**. Roughly a quarter of the machine needs a
window the user clicks through, not most of it.

The arithmetic is spelt out because the `QuietUninstallString` count invites
the wrong subtraction. 253 entries lack a quiet string, and it is tempting to
read the 23 as the overlap with that figure rather than with its complement,
which would give 87 silent instead of 245. The 23 are the entries that have a
quiet string *and* a ProductCode.

That is better news than the `QuietUninstallString` count alone implies, and
it is why the route is a ladder rather than a single value. 72 entries is
still enough that the interface has to tell the truth about them: the confirm
dialog says the publisher's own uninstaller will open, and the activity panel
shows a step waiting on a window rather than a progress rail. This needs no
new event type, because `Event::Progress` with `fraction: None` and a sentence
is exactly the honest progress case §7.18 already specifies.

Those 72 are also the strongest argument for the second spec. An uninstaller
Brokey cannot drive is an uninstaller whose thoroughness Brokey cannot vouch
for, which is the gap a leftover scan exists to close.

### Self-update

`selfupdate::install::detect` stays a pure function of a probe and gains
Windows arms: installed by winget, by Scoop, by Chocolatey, as an MSI with a
ProductCode in the uninstall keys, or as a portable executable beside a
writable directory. Each maps to one remedy, and `selfupdate/tests` asserts
the impossible sentences never appear, as it does for the Linux arms.

### Packaging

MSI through the Tauri bundler, then a winget manifest submitted to
`microsoft/winget-pkgs`, a Scoop manifest, and a Chocolatey package. That is
the Windows mirror of what the sibling `Packages` repository does for apt and
rpm, and it means Brokey is distributed by the managers it brokers.

MSI rather than NSIS, for a reason this spec turned up: an MSI gets a
ProductCode in the uninstall keys, so Brokey lands among the 245 applications
on the reference machine that can be removed silently and cleanly rather than
among the 72 that cannot. Shipping a store that leaves behind the kind of mess
its second feature exists to clean up would be indefensible.

### The page

Small changes, which is the point of having drawn the boundaries properly.
Nav swaps Drivers for Features. The status bar names the Windows edition and
build rather than the distribution. Source badges gain "winget", "Chocolatey",
"Scoop", "Store", "Features" and "Installed", still neutral, still never a
colour per source. The confirm dialog gains the two sentences described above,
about how many times permission is needed and about an uninstaller that opens
its own window. No new component, no new token, no new page.

## Rules this settles

- **The window never elevates**, on either platform. On Windows a privileged
  step is an entry in the helper's Windows closed list with a test, and
  `ShellExecuteEx` with `runas` is called in exactly one place, the same place
  `pkexec` is called on Linux.
- **Nothing Brokey downloaded is run before it is verified.** A bootstrap is a
  file Brokey fetched, whose SHA-256 or Authenticode signature it checked,
  executed from a directory only Administrators can write. Never a script
  piped from a URL into a shell. This is the Windows counterpart of the
  signature check a distribution's package manager performs for free.
- **Per-user before elevated.** Where a source can install for this user
  alone, it does, and a plan that must elevate names the package that forced
  it and says how many times permission will be asked.
- **A source still never runs anything.** `DISM /Online /Get-Features` and the
  OData and catalogue queries are read-only questions, the shape `fwupd.rs`
  already uses. Everything that changes the machine is a Step.
- **`SourceKind` is whole on both platforms**, so the page's types are checked
  against every variant in one run.
- **The install directory is derived from the uninstaller's path**, never
  taken on faith from `InstallLocation`.
- **Removal tells the truth about what it can and cannot do silently.**
- **Artwork matched across platforms is marked as matched**, with the same
  confidence rule and the same `text-dim` label that a name-matched edition
  carries. Screenshots are never matched that way in v1.
- Everything else in `CLAUDE.md` holds unchanged: honest progress, a source
  that says why it cannot work, grouping as a pure function, British spelling
  and sentence case, no accent background for selection, no raw hex.

## Testing

The fixture discipline survives intact and gets easier, because every Windows
source reads a file format rather than a tool's output. Into
`crates/brokey-core/tests/fixtures/windows/`: a small winget `index.db`, a
Chocolatey `lib` tree of `.nuspec` files, a Scoop bucket and `apps` directory,
an exported uninstall hive, and DISM feature output. Every parser stays a pure
function tested against them, and `cargo test --workspace` still needs no
network and no Administrator.

Three additions beyond ports of existing tests:

- The helper refuses a bootstrap script at a path outside its cache directory,
  and refuses a Plan whose steps are not on the Windows closed list.
- `contract.rs` checks both platforms' `SourceKind` words against `types.ts`.
- The ARP filter has a fixture where each heuristic fires and one where it
  must not, as `group.rs` requires of its own.

CI gains a `windows-latest` job running the same three commands. Live tests
stay `#[ignore]` and named `live_*`.

## Rejected

- **A Windows service.** One consent at setup and no UAC afterwards, which is
  what Steam and Chrome do. It is the daemon `docs/architecture.md` rejected
  on Linux, for reasons that apply more strongly here: a permanently running
  SYSTEM service whose second feature is deleting files and registry keys on
  request is a much larger thing to secure than a process that lives for one
  plan and exits.
- **Relaunching the window elevated.** Far less code, and common on Windows.
  It would put a webview with network access and a browser engine inside an
  Administrator token.
- **Shelling out to `winget search` and parsing the table.** The same mistake
  as parsing `pacman -Ss`, with the same answer: the index is a file, read it.
- **Windows Update, in any form,** including a read-only count on the Updates
  page. Windows updates itself, and a store that lists updates it cannot apply
  invites a button that cannot exist.
- **Driver management.** `chwd` and `fwupd` have no Windows counterpart worth
  building; Windows Update does this and does it adequately.
- **pip, npm, cargo, .NET tool and PowerShell Gallery**, which UniGetUI
  supports. Brokey is an application store and deliberately does not list pip
  or npm packages on Linux. Adding them on Windows alone would make the two
  platforms mean different things by "installed".
- **Ninite, Npackd and PortableApps.com.** Ninite has no API, only a web form
  that builds a bundle. Npackd is effectively dormant. PortableApps.com has no
  clean public catalogue and its software leaves no installed record to find.
- **Scanning the disk for unmanaged applications.** A folder in Downloads with
  an executable and no uninstall entry is real, and closing that gap is
  tempting because the Linux README admits the same one. It is a heuristic
  with expensive false positives, and it belongs with the second spec if
  anywhere, where the machinery for judging what a directory belongs to
  already exists.

## Open

- Which of `source.msix` and `source2.msix` to read, and whether the smaller
  one is a complete index or a delta. Settled by opening both.
- `rusqlite` with `bundled` links a statically compiled SQLite. The objection
  recorded in `docs/architecture.md` was to libalpm and libapt, which tie one
  binary to one distribution's library version; a statically bundled SQLite is
  the opposite, adding no runtime dependency and being identical everywhere.
  This spec allows it, and notes that a pure-Rust reader of the read-only
  subset of the format would also do.
- Code signing. An unsigned MSI meets SmartScreen, and a certificate costs
  money each year. Until there is one, the README says plainly what the
  warning is and why it appears.
- Whether Chocolatey's per-user install path is worth supporting, or whether
  Chocolatey should simply be the source that always elevates.
- ARM64. The workspace should build for it and no machine exists to test it
  on, so it is marked untested in the sources list the way apt and dnf are.
- **The Windows edition ranking, and what counts as Brokey itself.** Adding
  the six Windows variants to `SourceKind` forced three exhaustive matches
  closed in `group.rs` and `updates.rs`. They were filled with placeholders,
  in enum order, and are inert while only one Windows source exists. Two of
  them are probably wrong and must be settled before a second Windows source
  lands: `Arp` currently ranks ahead of Chocolatey and Scoop, though an
  Add/Remove entry is provenance-unknown and should likely lose to a real
  package manager's edition; and `is_self_package` returns `false` for `Arp`,
  which breaks the moment Brokey ships as an MSI, because its own uninstall
  entry is exactly how it will appear. Settling either means a grouping
  fixture, per the invariant that a heuristic arrives with a fixture where it
  fires and one where it must not.
- How the second spec hooks in. The candidates are an install-time snapshot
  taken before a plan runs, a path and registry ownership index built from
  every source's installed list, and an undo journal written by the helper.
  This spec deliberately does not choose; it only guarantees that the
  Add/Remove Programs source exposes the derived install directory and the
  uninstall key path, which all three would need.

## Evidence

The measurements in this document were taken on the reference machine on
2026-09-19 and are what several decisions rest on, so they are recorded rather
than described.

| | |
|---|---|
| Uninstall keys across `HKLM`, `HKLM\WOW6432Node` and `HKCU` | 343 |
| Of those, carrying a `DisplayName` | 317 |
| `SystemComponent = 1`, hidden by Settings itself | 160 |
| With no `InstallLocation` | 221 |
| With no `QuietUninstallString` | 253 |
| With a `QuietUninstallString` | 64 |
| Keys named by an MSI ProductCode GUID | 204 |
| With both a quiet string and a ProductCode | 23 |
| Removable silently (a quiet string or an MSI ProductCode) | 245 |
| Removable only by a window the user clicks through | 72 |
| `ProductName` in `CurrentVersion`, on a Windows 11 machine | `Windows 10 Pro` |
| `https://cdn.winget.microsoft.com/cache/source.msix` | 20,544,365 bytes |
| `https://cdn.winget.microsoft.com/cache/source2.msix` | 3,615,306 bytes |
| `chocolatey.nupkg` from the community feed | 5,713,666 bytes |
| App Installer bundle, `winget-cli` v1.29.290 | 216,783,252 bytes, with a published SHA-256 |

Chocolatey was already installed on the machine; Scoop was not.
