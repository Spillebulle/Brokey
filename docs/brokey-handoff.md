# Opening Brokey on one installed application

Dated 2026-11-23. The contract another application relies on to hand the user
to Brokey for one installed application, with Remove ready. Written for Heft,
which shows **Uninstall with Brokey** when a large folder belongs to an
installed application. Heft keeps a copy in its own `docs/`. A change here is
a change to that copy too, so it is made by adding to this file, never by
changing the meaning of something already in it.

## The call

```
brokey --show <source>:<id>
```

One argument after `--show`, a package reference written exactly as Brokey
writes one everywhere else (the settings, `brokey plan`, `brokey installed`):
the source word, a colon, and that source's own id. The source word is
lower case. Everything after the first colon is the id, so ids that contain
colons or backslashes need no escaping beyond what the caller's process API
needs for one argument.

| Source | Word | The id | Example |
|---|---|---|---|
| pacman | `pacman` | package name | `pacman:steam` |
| AUR | `aur` | package name | `aur:visual-studio-code-bin` |
| apt | `apt` | package name | `apt:gimp` |
| dnf | `dnf` | package name | `dnf:gimp` |
| Flatpak | `flatpak` | the installation's remote, then the full ref | `flatpak:flathub/app/org.gimp.GIMP/x86_64/stable` |
| Snap | `snap` | snap name | `snap:spotify` |
| GitHub releases | `github` | `owner/repo` | `github:Spillebulle/Brokey` |
| Add/Remove Programs | `arp` | hive, a backslash, the uninstall key's name | `arp:HKLM\{23170F69-40C1-2701-2201-000001000000}`, `arp:HKLM32\Steam`, `arp:HKCU\Discord` |
| winget | `winget` | package identifier | `winget:Valve.Steam` |
| Chocolatey | `choco` | package id | `choco:7zip` |
| Scoop | `scoop` | app name | `scoop:7zip` |
| Microsoft Store / MSIX | `msix` | package family name | `msix:Microsoft.WindowsTerminal_8wekyb3d8bbwe` |

The `arp` hives are `HKLM` (the 64-bit machine key), `HKLM32`
(`HKLM\SOFTWARE\WOW6432Node\...\Uninstall`) and `HKCU`. The key name is the
subkey of `...\CurrentVersion\Uninstall` as the registry spells it, which is
often a GUID and often not. `msix` also accepts a package full name
(`Microsoft.WindowsTerminal_1.21.2361.0_x64__8wekyb3d8bbwe`, the folder name
under `WindowsApps`) and reduces it to the family name.

When a folder belongs to software that several sources know about (a winget
package is also an Add/Remove Programs entry), the caller sends whichever it
found. Brokey groups editions itself, so either reference opens the same page.

## What Brokey does

1. **It opens that application's page**, the same page a click on it in
   Installed opens, with the edition the reference names selected and its
   **Remove** button focused. Nothing is removed until the user presses it and
   accepts the confirm step, exactly as for any other removal. Brokey never
   removes anything because it was started with `--show`.
2. **If Brokey is already open**, the running window comes to the front and
   goes to that page. The new process hands the reference over and exits.
3. **If nothing installed matches the reference**, Brokey opens Installed with
   a notice saying which reference it could not find and that the application
   may already have been removed.

## What the caller can rely on

**Exit codes of the process the caller started:**

| Code | Meaning |
|---|---|
| `0` | The reference was understood and handed to a window: a new one, which this process became, or the one already open, to which it was handed. |
| `2` | The reference was not understood: no colon, an unknown source word, or an empty id. No window opens. A sentence saying what was wrong goes to standard error. |

The exit of that process is **not** a signal that the user has finished. If a
window was already open the process exits at once; if not, it is the window,
and exits when the user closes Brokey, which may be long after the removal or
never. A caller that wants to know whether the folder is gone checks for it
itself: when the started process exits, and again whenever the caller's own
window regains focus. That is what Heft does, and it is correct in both cases.

## Finding Brokey and knowing it supports this

`--show` arrives in **Brokey 0.1.8**. A caller decides from the version, and
draws its control disabled, with a tooltip naming 0.1.8, when the installed
Brokey is older.

- **The version.** Read it from the package manager that installed Brokey
  (the pacman, dpkg or rpm database, or the `DisplayVersion` of Brokey's
  Add/Remove Programs entry on Windows), which a caller that already reads
  those databases has to hand. Or run `brokey --version`, which prints
  `brokey <version>` on one line and exits `0` in every Brokey ever released.
- **An older Brokey given `--show`** opens its window normally and ignores the
  argument. Nothing breaks, it just does not go to the page, which is why the
  version check comes first.
- **Where it is.** On Linux, `brokey` is on `PATH` (`/usr/bin/brokey`) for
  every package; for the AppImage there is no fixed path and the caller should
  treat Brokey as not installed. On Windows, from 0.1.8 the MSI registers
  `brokey.exe` under
  `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\brokey.exe`, whose
  default value is the full path, the same place `Win+R` looks. Before 0.1.8
  there is nothing to find there, which is consistent with the version rule.
