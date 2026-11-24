# Leftover cleanup: design

Dated 2026-11-24. The second of the two Windows specs that
`2026-09-19-windows-support-design.md` promised: after Brokey removes an
application, find what its uninstaller left behind and offer to clear it.
This document is the design; the plan is written from it.

## What it is for

**After an uninstall there should be nothing left of the application.**
That is the whole point and the yardstick for every decision below. Windows
uninstallers routinely leave their install folder half-full, their settings
in AppData and ProgramData, their keys under `Software\<Vendor>`, a dead
Start menu folder, a scheduled updater task, a service. Brokey removes the
application through its own uninstaller as it does today, then looks for
what remains and shows it, and the user decides what goes.

The shape is Revo Uninstaller's: uninstall, scan, a ticked list, delete.
Brokey's difference is where it is careful: nothing another installed
application still uses is ever offered, and nothing is destroyed outright; it
goes to a holding area it can be restored from.

## Goals

- After a removal on Windows, a list of leftovers: files and folders,
  registry keys and values, shortcuts, services, scheduled tasks and startup
  entries, each with where it is, its size where that means anything, and how
  sure Brokey is that it belonged to the application.
- The user ticks what goes. Confident items come ticked; doubtful ones and
  the user's own data come unticked.
- Whatever is cleared goes to a **holding area**: files moved, registry keys,
  services and tasks exported, so any cleanup can be restored. The holding
  area is emptied by the user, or automatically after a period.
- **Forced removal** for an entry whose uninstaller is missing, broken, or
  cannot be run: skip the uninstaller, and clear the install folder, the
  uninstall key and the leftovers as above.
- **Steam games** are removed by asking Steam: their Add/Remove entry's
  uninstall command is `steam.exe steam://uninstall/<appid>`, run as the user.
- **An uninstaller in a folder the user can write to** (Steam itself, on the
  reference machine) is started as the user and asks for Administrator
  itself, the way Windows Settings starts it, instead of being refused.
- The same cleanup follows a removal through winget, Chocolatey or Scoop,
  using the Add/Remove entry those packages are joined to.

## Non-goals

- **Recording what an installer writes** (Revo's "traced" installs). It only
  helps applications installed through Brokey from now on and misses what an
  application writes after its first run, which is where most leftovers come
  from. It may come later as a refinement for Brokey's own installs; the
  scanner should not need it.
- **Scanning for applications nobody installed** (a folder in Downloads with
  an executable). Rejected in the Windows spec and still rejected.
- **A general registry cleaner.** Brokey cleans up after one application it
  just removed, never the machine at large.
- **Linux in the first phase.** See "Linux, second phase".

## Anti-goals

These would make it a failure even if it worked as specified:

- **Clearing anything another installed application uses.** One broken
  application costs more trust than a thousand leftover folders earn.
- **Clearing anything the user did not see in the list.**
- **Destroying anything on the first pass.** Everything is restorable until
  the holding area is emptied.
- **Being turned into a deletion tool.** A path that reaches the elevated
  helper because of a swapped folder or a junction must be refused there.

## Constraints

- The invariants in `CLAUDE.md` hold: a source never runs anything, the
  scanner returns findings and the clear-out is a plan the Runner runs; the
  window never elevates; a privileged operation is an entry on the helper's
  closed list with a test that it refuses anything else; progress is honest;
  copy rules; colour is state, so confidence is a word in a neutral badge.
- The page's types are the contract (`crates/brokey/tests/contract.rs`).
- Windows has 72 of 317 entries on the reference machine whose uninstaller
  needs clicking through. Those are both the commonest source of leftovers
  and the hardest to time, below.

## Approaches

Three were considered.

1. **Search after uninstall.** What Revo does by default: once the
   uninstaller is done, look in the usual places for things matching the
   application's name, publisher and install folder, and rank them. Works for
   everything already installed. It is guesswork, so false matches are the
   risk.
2. **Record at install time.** Snapshot before and after Brokey installs
   something and keep the difference. Precise, but only for applications
   Brokey installs from now on, slow to snapshot, and blind to what the
   application writes after its first run.
3. **Search after uninstall, guarded by ownership.** Approach 1, but nothing
   that another installed application still claims (an install folder, an MSI
   component, a Store package folder, a shortcut target) is ever offered, and
   everything cleared goes to a holding area that can be restored.

**Chosen: 3**, by the user, 2026-11-24. It is the only one that cleans up
applications already on the machine, which is the goal, and the guard plus
the holding area address the one real risk of a search.

## How it works

### 1. Hints are taken before the uninstaller runs

The Add/Remove entry disappears when the uninstall succeeds, so everything
the scan will search by is gathered when the removal is planned and carried
with the plan:

- display name, publisher, version
- the uninstall key path (hive and key name), and the MSI ProductCode where
  the key is one
- the derived install directory (`arp::install_dir`, already built, from the
  uninstaller's folder, confirmed by `InstallLocation`) and `InstallLocation`
  itself
- the directory of `DisplayIcon`, as a hint only
- the Start menu shortcuts that target the install directory (the launcher's
  resolution already finds these)
- the source and package id the user removed (winget, choco, scoop, arp),
  and for winget the package identifier and its normalised name

### 2. Knowing the uninstaller has really finished

The uninstaller process exiting is not the signal. NSIS uninstallers copy
themselves to Temp and exit at once; Steam returns before its own dialog has
been answered. **The signal is the Add/Remove entry being gone** (or, for a
package manager, the package leaving that source's installed list).

- When the step's process exits and the entry is gone: scan.
- When the process exits and the entry is still there: the activity panel
  says "Waiting for the uninstaller to finish" with an empty rail, and Brokey
  re-reads the entry every few seconds. Two buttons: **It has finished** and
  **Stop waiting**.
- If the user says it has finished and the entry is still there, the
  uninstall did not happen (cancelled, or failed): say so, and offer
  **Forced removal**, below. No scan of a still-installed application is
  offered as leftovers.

### 3. Where it looks (Windows)

Each location is searched for matches against the hints. A match by path is
the strongest kind: anything that *points into* the install directory belongs
to the application.

Files and folders:
- the install directory itself, and its parent if that is now empty and is
  named for the publisher (`Program Files\<Publisher>\` with nothing else in it)
- `%ProgramFiles%`, `%ProgramFiles(x86)%`: `<Publisher>\<Name>`, `<Name>`
- `%APPDATA%`, `%LOCALAPPDATA%`, `%USERPROFILE%\AppData\LocalLow`,
  `%ProgramData%`: `<Name>`, `<Publisher>\<Name>`, `<Publisher>` when nothing
  else of that publisher's remains installed
- Start menu folders (machine and user) and desktop shortcuts whose target
  lies in the install directory and no longer exists
- `Documents`, `Saved Games`, `Documents\My Games`: `<Name>` (user data, see
  confidence)

Registry (HKLM, HKLM WOW6432Node, HKCU):
- the uninstall key, if it is still there
- `Software\<Publisher>\<Name>`, `Software\<Name>`, and `Software\<Publisher>`
  when it has no other subkeys left
- `App Paths\<exe>` whose value points into the install directory
- `Run` and `RunOnce` values pointing into the install directory
- file association ProgIDs and `Applications\<exe>` whose open command points
  into the install directory, and the extension keys that reference only that
  ProgID

System:
- services whose `ImagePath` is in the install directory
- scheduled tasks whose action runs something in the install directory
- Windows Firewall rules whose program is in the install directory

Name matching uses the winget-style normaliser already ported
(`winget/normalise.rs`), so `Mozilla Firefox (x64 en-US)` and
`Mozilla Firefox` match, and never matches a name shorter than four
characters or a generic word (a short stop list: `common`, `shared`, `data`,
`update`, `temp`, the publisher-only words `microsoft`, `windows`, `intel`,
`nvidia`, `amd`, `google`).

**Cloud-synced folders are not offered.** Anything inside a folder OneDrive
(or another cloud provider) syncs, which shows as a cloud reparse tag or the
recall-on-open attributes, is left out of the list and counted in a line,
"N items in folders OneDrive syncs were not offered". Moving them out would
delete them from the user's other devices too, or download them first, and
neither is something the user saw. Documents taken over by OneDrive is the
common case, so most "Your data" items on such a machine are counted rather
than offered; that is the safe side of the trade.

### 4. Confidence

| Level | When | Ticked by default |
|---|---|---|
| **Certain** | It is the install directory, the uninstall key, or it points into the install directory | Yes |
| **Likely** | Its name is the application's normalised name, under its publisher or at a top level that is per-application by convention (AppData, ProgramData, `Software\`) | Yes |
| **Possible** | A weaker match: publisher only, a looser name | No |
| **Your data** | Under Documents, Saved Games or My Games, whatever the match | No |

The badge says the word. "Your data" is offered because cleanup means
nothing left, but unticked, because a save game cannot be reinstalled.

### 5. The ownership guard

Before anything is shown, every candidate is checked against what the
machine still claims. A candidate is dropped if it **is, contains, or lies
inside** something claimed by another installed application:

- every remaining Add/Remove entry's derived install directory and
  `InstallLocation`
- Windows Installer components: the paths and registry keys registered under
  `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Installer\UserData\*\Components`
  by any product still installed
- Store package folders and their data under `%LOCALAPPDATA%\Packages`
- Scoop's `apps\` and Chocolatey's `lib\` folders of other packages
- the targets of other applications' Start menu shortcuts
- services and scheduled tasks whose program is outside the install
  directory

The list ends with a line saying how many were kept because something else
uses them, with the names on hover, so the guard is visible rather than
silent.

**Protected locations are never offered at all**, whatever matches: the
Windows directory; the roots of Program Files, ProgramData, AppData, the
user profile and the Start menu; `Software\Microsoft`, `Software\Classes`
roots for extensions other applications reference, `Software\Policies`,
`Software\WOW6432Node` itself; anything reached through a junction or
symbolic link.

### 6. The list

Shown when the scan finishes, in the activity panel's place for the plan
that removed the application:

```
Leftovers from Discord                                   23 items, 412 MB

  Files and folders
  [x] C:\Users\you\AppData\Local\Discord              Certain    388 MB
  [x] C:\Users\you\AppData\Roaming\discord            Likely      24 MB
  [ ] C:\Users\you\Documents\Discord                  Your data   12 KB

  Registry
  [x] HKCU\Software\Discord                           Likely
  [x] HKCU\...\Run  Discord                            Certain

  Shortcuts
  [x] Start menu\Programs\Discord Inc                 Certain

  2 kept because other applications use them.

                       [ Keep everything ]  [ Move 21 to holding area ]
```

Built from the §7 components already in `app.css` (Row, Badge, the painted
checkbox, Button, Panel); no new component unless the style guide lacks one,
in which case it goes upstream first. Nothing in the list is clickable that
deletes immediately.

### 7. The holding area

- Files and folders are **moved**, not copied, into a holding area per
  cleanup: user items under `%LOCALAPPDATA%\Brokey\Holding\<id>\`, machine
  items under `%ProgramData%\Brokey\Holding\<id>\`, which only
  Administrators can write. A move across volumes is a copy and delete.
- Registry keys are exported to a file in the same folder and then deleted.
- Services and scheduled tasks are exported (service configuration; task
  XML), then stopped and deleted.
- A manifest in each folder records every item's original location so it can
  be put back.
- **Settings** gains a Holding area section: each cleanup with the
  application's name, date and size, **Restore** and **Delete now**, and
  **Empty**. Items older than 30 days are emptied automatically, which the
  section says. **Brokey never asks for Administrator at start-up for
  this**: user items past 30 days are emptied when Brokey starts; machine
  items past 30 days are emptied as an extra step of the next plan that
  elevates anyway, or when the user presses Empty, and the section shows
  them as waiting until then.
- Restore, Delete now and Empty for machine items are helper operations in
  the first plan, beside the clearing ones, so no kind is offered that
  cannot be put back. The helper re-checks every restore destination by the
  same rules as a clear (absolute, no `..`, not protected, no name-surrogate
  reparse point on the way) even though the manifest lives in an
  Administrators-only folder: that folder is a reason to trust the manifest
  more, not a reason to stop checking.
- Restoring puts files, keys, services and tasks back where they were and
  refuses, item by item with a sentence, where something new now occupies the
  place.

### 8. Privilege

- The scan reads only, as the user. Every location above is readable by a
  standard user on a default Windows install; what is not readable is listed
  as "Brokey could not look in N places" rather than skipped silently.
- Clearing user items (HKCU, the user's profile) is a plan step that does
  not need Administrator.
- Clearing machine items (HKLM, Program Files, ProgramData, services, tasks,
  firewall rules) is a plan step through the helper, as **a new structured
  operation on the closed list**, not a command line: "move these paths to
  holding area `<id>`", "export and delete these keys", "export and delete
  this service / task". The helper re-checks every item itself before acting:
  absolute, no `..`, not a protected location, **not a reparse point at any
  level** (opened without following links, so a folder swapped for a
  junction to `C:\Windows` between scan and clear is refused), and the
  holding destination is the Administrators-only folder. "Reparse point"
  here means a **name surrogate** (junction, symbolic link, mount point):
  those are what can redirect a path. Other reparse tags are not refused by
  the helper; the scan deals with cloud files, below. A refusal stops the
  whole plan before the first item, as the closed list does today. Tests
  that it refuses each of those.
- The ownership guard is the window's job (it needs every source's
  installed list); the helper's job is that no path, however it got there,
  reaches a protected place.

### 9. Forced removal

Offered on an entry's page (behind the usual confirm step) when its
uninstaller is missing, has failed, or the user reached it through "It has
finished" with the entry still present. It does not run the uninstaller. It
scans with the same hints, with the install directory and uninstall key as
Certain, and shows the same list; the confirm says plainly that the
publisher's uninstaller will not be run and why. It is also the remedy for an
entry whose uninstaller the helper refuses and that cannot self-elevate.

**MSI products.** An entry named by an MSI ProductCode is removed with
`msiexec /x` first, as today; forced removal is for when that fails. When it
is forced:
- the product being removed is **excluded from the ownership guard**, so its
  own components do not shield its own files; components it shares with
  another installed product still do;
- its Windows Installer registration is cleared too, because a product left
  half-registered blocks a later reinstall: the product's keys under
  `Installer\Products` and `Installer\UserData\<sid>\Products\<packed code>`,
  and its value under each `Components\<id>` key, deleting a component key
  only when no other product's value remains in it. This is a helper
  operation, exported to the holding area like any other key and restorable,
  and it only ever touches keys named by that one product's packed code.

### 10. Steam, and uninstallers in writable folders

- An entry whose uninstall command is `steam.exe steam://uninstall/<appid>`
  is removed by running that as the user, never through the helper. Steam
  shows its own confirmation; the "really finished" rule above covers the
  wait. The leftover scan then runs as for anything else (game folders under
  `steamapps\common` that Steam leaves, `Documents\My Games\<Name>` as Your
  data).
- An uninstaller the helper refuses because its folder is writable by the
  user (Steam itself) is started **as the user** and elevates itself through
  its own manifest, so Windows' prompt names the uninstaller, exactly as when
  started from Settings. Brokey does not elevate a file the user could have
  replaced; it lets Windows ask about that file. The confirm dialog says
  Windows will ask.
- **This is a second sanctioned exception** to "never an elevation call site
  outside `transaction/elevate/`", approved by the user on 2026-11-24, and it
  goes into `CLAUDE.md`'s first invariant beside the setup executable's, held
  narrow in code: it is reached only for a step the closed list refused with
  "a program this account can replace", for an uninstaller named by an
  Add/Remove entry; it is `ShellExecuteEx` with the `open` verb, never
  `runas`, so Brokey asks for nothing and Windows alone decides whether to
  prompt; and the confirm step has said so before it runs. It lives in
  `transaction/elevate/` all the same, so the one place to audit stays one
  place.

### 11. Other sources

- winget, Chocolatey and Scoop removals take their hints from the Add/Remove
  entry they are joined to (`Registry key` fact) plus the package's own
  folder, and scan the same way.
- Store applications: Windows deletes `%LOCALAPPDATA%\Packages\<family>`
  itself on removal, so the scan is limited to the general locations and
  usually finds nothing; that is fine and the list says "Nothing was left
  behind."
- GitHub portable applications: the folder Brokey unpacked into is the
  install directory.

### 12. Text mode

`brokey leftovers <source>:<id>` runs the scan against an application that
is **still installed** and prints what would be offered after its removal,
with confidence and the guard's verdicts, and changes nothing. It is how the
scanner is exercised on a real machine without uninstalling anything, and
the reference machine's output belongs in the plan's verification.

### 13. Heft

Unchanged contract (`docs/brokey-handoff.md`): Heft opens the page with
`--show`, the user removes and clears leftovers here, and Heft rescans when
Brokey exits or regains focus. The cleanup is what makes "Uninstall with
Brokey" free the whole folder rather than most of it.

## Testing

- The scanner is a pure function of the hints and a read-only view of the
  file system, registry, services and tasks, with that view behind a trait
  so it is fixture-tested on any OS, as `arp.rs` is against an exported hive.
- Every heuristic arrives with a fixture where it fires and one where it must
  not, as the grouping invariant requires: two applications from one
  publisher, a shared `Common Files` folder, an MSI component shared with
  another product, a junction inside an AppData folder, a name of three
  letters, a Steam game whose folder Steam left.
- The helper's new operations have refusal tests for every rule in
  "Privilege".
- The holding area round-trips: clear then restore leaves the fixture as it
  was, including a registry key with values of every type.
- Contract samples for every new type the page receives.
- A `live_` test runs the text-mode scan against an installed application on
  the machine and asserts only that it completes and offers nothing the
  guard should have dropped.

## Linux, second phase

The same flow, smaller, written as its own plan after Windows lands:
pacman's `.pacsave` files left beside configuration, `~/.config/<name>`,
`~/.local/share/<name>`, `~/.cache/<name>` matched against the package name
and its desktop entry's id, Flatpak's `~/.var/app/<id>` (or `flatpak
uninstall --delete-data`), Snap's `--purge`. The guard is the pacman, dpkg
or rpm file lists. The scanner trait above should not assume the registry.

## Left to the builder

- The holding area's on-disk format and the export format for registry keys,
  services and tasks, provided restore round-trips exactly.
- How the new helper operations are carried in a `Step` (today a `Step` is a
  `Command`), provided they are structured and on the closed list with
  refusal tests.
- The polling interval for "really finished", and how the stop list and the
  four-character rule are tuned against the reference machine.
- Whether services, tasks and firewall rules land in the first plan or a
  follow-up, provided the list never offers a kind it cannot hold and
  restore.

## Calls made in writing this, not asked

- Windows first, Linux as a second phase (the user did not say).
- "Your data" offered unticked rather than hidden.
- 30 days before the holding area empties itself.
- Possible matches offered unticked rather than hidden.
- No Administrator prompt at start-up: machine items wait for the next
  elevated plan or Empty.
- Restore, Delete now and Empty are helper operations in the first plan, and
  the helper re-checks restore destinations.
- Forced removal of an MSI product excludes that product from the guard and
  clears its Windows Installer registration, restorably.
- Only name-surrogate reparse points (junctions, links, mount points) are
  refused by the helper; cloud-synced items are left out of the list.

## As built

Where the build departed from or settled what is written above.

- **Planning (§2, §9, §10).** The cleanup's part of the planner is
  `transaction/plan/cleanup.rs`. Everything it asks of the machine (the
  closed list, the hints, the holding area, the live view) comes through a
  `Planning` value, so it is tested without the registry. The scans a clear
  reads are passed to `Store::plan_with` / `plan::build_with`;
  `Store::plan` passes none, so a clear planned through it says the list is
  no longer known.
- **A scan is kept with whether it was forced.** `internal::Scans` maps a
  scan id to a `ScanRecord { hints, findings, forced }` rather than to
  `(Hints, Findings)`, because §9's registration clear follows only a forced
  removal and nothing else in the record says so.
- **A clear is its own plan**, never added to a removal's: the runner checks
  every step of Brokey's own before the first runs, and a HOLD of something
  that is not there refuses the whole plan. Items that have gone since the
  scan (with a parent or with the uninstaller) are left out when the clear
  is planned; what cannot be read is kept in for the closed list to judge.
  User and machine items share one holding id, and after a forced removal
  of an MSI product `MSI_UNREGISTER` exports its registration into that same
  entry.
- **Waiting (§2).** `AWAIT_GONE` looks at the uninstall key every 2 seconds
  (`internal::AWAIT_POLL`). A package with no uninstall key (a package
  manager's own removal, an MSIX package) has nothing to wait for and goes
  straight on. "It has finished" with the key still there ends the plan
  with "The uninstaller did not remove <name>. It may have been cancelled.
  Forced removal clears it without the uninstaller.", and no scan is made.
- **The scan reads every source's installed list afresh**, not the window's
  store, which was read before the uninstaller ran and would still list the
  application, making its every file a claim. A source whose list cannot be
  read adds one to "could not look in N places".
- **The self-elevating uninstaller (§10)** is asked of the closed list twice:
  by the planner, which rewrites only an Add/Remove root step refused with
  the "program this account can replace" sentence, and again by `SHELL_OPEN`
  immediately before it starts, which starts nothing for any other answer.
  An uninstaller that ends with a non-zero code is a note, not a failure:
  the uninstall key, looked at by the next step, decides.
- **Restoring, emptying and the 30-day machine empty** are steps labelled
  with the Add/Remove source, which only chooses a badge. A plan that
  elevates for any reason gets a root `EMPTY` for each waiting machine
  entry it does not already restore or empty; a plan that does not elevate
  never gets one.
- **Steam.** A Steam game is not removed by running `steam.exe`: when Steam
  is not running that process is the Steam client, and the step would last
  until Steam closed and Cancel would kill Steam. Instead the step is
  Brokey's own `STEAM_UNINSTALL` (one app id, digits only), which hands
  `steam://uninstall/<id>` to Explorer as the user, the way opening an
  application does, and returns at once without reading Explorer's exit
  code, which means nothing. A plain Explorer step would not do, because
  the runner fails a step on a non-zero code. `AWAIT_GONE` does the
  waiting. A cancelled Steam uninstall ends with "Steam did not remove
  <name>. It may have been cancelled. Remove the game from your Steam
  library, then look in Installed." Forced removal is never offered for a
  `Steam App <id>` entry, and the planner refuses one, because Steam keeps
  its own record of the game.
- **A clear that would do nothing is an error**, not a plan that says
  "Nothing to do.": nothing ticked, or everything ticked already gone
  ("Everything ticked has already gone, so nothing was cleared."). A plan
  in which a clear is not the only operation is refused, so nothing before
  it can move what it holds and two clears of one scan never share a
  holding id.
- **The self-elevating uninstaller's call is sealed.**
  `transaction::elevate::shell_open` is `pub(crate)` and takes a
  `cleanup::internal::Refused`, made only by `Refused::by` from the closed
  list's replaceable refusal of that command. `SHELL_OPEN` names a full path
  to an `.exe` and nothing else. It waits on the uninstaller in 250 ms
  slices (`SHELL_OPEN_SLICE`); "Stop waiting" stops the wait and the plan
  and never kills the uninstaller, which is the user's window.
