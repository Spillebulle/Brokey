# Architecture

Dated 2026-09-10. What Brokey is, the shape it is built in, the rules that
shape settles, what was rejected and why, and what is still open. Superseded
sections are marked in place rather than rewritten.

## What it is for

One application that finds and installs software on any Linux machine from
every place that machine can get software: the distribution's own repositories
(pacman, apt, dnf), the AUR, Flathub and other Flatpak remotes, the Snap Store,
GitHub releases, and the machine's drivers and firmware. One search box, one
list of results with every source in it, one place to see and apply updates,
and the application keeps itself up to date through the same machinery.

The first machine is an Arch derivative (CachyOS), so pacman and the AUR are
built and tested first; Flatpak and Snap wherever they are installed; apt and
dnf are built to the same interface and are marked as untested until they have
run on a real Debian and a real Fedora.

**Superseded.** "Any Linux machine" above: `docs/superpowers/specs/2026-09-19-windows-support-design.md`
extends the same sentence to Windows, from winget, Add/Remove Programs,
Chocolatey, Scoop, the Microsoft Store and GitHub releases.

## Decisions

| | |
|---|---|
| Language | Rust for everything that touches the system; TypeScript for the interface |
| Interface | Tauri 2 window over a React + TypeScript page styled by `tokens.css` at the **desktop** scale. No `class="web"`, ever: this is a desktop application |
| Shape | One workspace: `brokey-core` (library, no window, every source and every rule, fully testable), `brokey-helper` (the one binary that runs as root), `brokey` (the Tauri application and a text mode from the same executable) |
| Privilege | The window never runs as root. The helper is started through `pkexec` with a polkit policy and reads a plan on stdin; it executes a closed list of operations and streams JSON events on stdout |
| Package databases | Read directly, in pure Rust, with no libalpm and no libapt linkage: `/var/lib/pacman/sync/*.db` and `/var/lib/pacman/local`, `/var/lib/apt/lists` and `/var/lib/dpkg/status`. One binary runs on every distribution |
| Metadata | AppStream is the spine: the distribution catalogue under `/usr/share/swcatalog` or `/var/lib/swcatalog`, Flatpak's per-remote `appstream.xml.gz`, and Flathub's web API. Icons and screenshots come from there; a package without a component is still listed, as a package rather than an application |
| Grouping | Results from different sources that are the same application are one row with several **editions**. The join key is the AppStream component id where both sides have one, then a normalised name match with a confidence score. Never a guess presented as certain |
| Accent | **#D42B48**, set as `--accent-fixed` with `--accent-h` at 18. Brokey takes the style guide's §2.3 exception for a brand colour matched exactly, so the accent is not on the derived ramp and `--accent-ink` is white. Umber is 60/68, Muster 200, HomeLab 160, Tally 255, all derived |
| Application id | `io.github.spillebulle.brokey`. Binary and package `brokey`, helper `brokey-helper` |
| Targets | Linux x86-64 and ARM64. Nothing else. **Superseded**: see `docs/superpowers/specs/2026-09-19-windows-support-design.md`, which adds Windows x86-64 and ARM64 |
| Licence | GPL-3.0-or-later |

## The shape

```
crates/
  brokey-core/        the library. sources/, appstream/, group/, transaction/, updates/, drivers/, selfupdate/, system/
  brokey-helper/      root helper: reads a Plan, runs it, streams Events. Nothing else
  brokey/       Tauri app: commands, events, settings, the `brokey <subcommand>` text mode
frontend/          React + TypeScript. tokens.css verbatim, app.css for components, pages/
packaging/         desktop entry, AppStream metainfo, polkit policy, linux/ (deb, rpm, PKGBUILD)
```

### brokey-core

**`Source`** is the one trait every provider implements:

- `kind()`, `availability()`: whether this machine can use it and, if not,
  one sentence saying why ("Flatpak is not installed", "no AUR helper and no
  base-devel").
- `search(query) -> Vec<Package>`: what this source has matching the query.
- `installed() -> Vec<Package>`: what this source has put on the machine.
- `updates() -> Vec<Update>`: what it could bring up to date.
- `details(id) -> Package`: the full record for one thing, fetched lazily.
- `plan(op) -> Vec<Step>`: how an install, remove or update is carried out,
  as steps the helper or the user session will run. A source never runs
  anything itself.
- `setup() -> Option<Setup>`: when its tool is missing, how the store would
  set it up: operations for other sources first (installing `flatpak` through
  pacman, apt or dnf; `snapd` through the AUR, apt or dnf), then its own steps
  (adding Flathub; starting snapd), and the sentence the confirm dialog
  shows. The planner expands `Op::Setup { source }` through it.

Implemented: `pacman`, `aur`, `flatpak`, `snap`, `apt`, `dnf`, `github`,
`fwupd`, `chwd`. Each is a module; each declares whether its steps need root.

**`Package`** is one installable thing from one source. **`App`** is the
grouped row: a key, the best name, summary, icon, screenshots, categories and
a list of member packages (the editions). Grouping lives in `group/` and is a
pure function of a list of packages, so it is tested with fixtures and never
with a network.

**`appstream/`** parses catalogue XML (streaming, `quick-xml`) into
components, indexes them by id and by `pkgname`, resolves cached icons to
files on disk, and caches the parse keyed on the file's mtime. It is the same
parser for the Arch catalogue, the Debian catalogue and Flatpak's.

**`transaction/`** turns a list of operations into a `Plan`: steps grouped
by source and by whether they need root, so one pkexec prompt covers every
root step in the batch. The `Runner` executes a plan, spawns the helper for
the root group, runs user-session steps (Flatpak `--user`, makepkg) itself,
and emits `Event`s: step started, progress (a fraction only when the total is
known), a log line, step finished, plan finished.

**`updates/`** merges every source's updates, sorts, and records when it last
checked. **`selfupdate/`** is Muster's `update::install` transcribed: detect
how this copy was installed as a pure function of a probe, ask GitHub for the
newest release, and say the one true thing about how to get it (§18.3 of the
style guide).

### brokey-helper

A separate executable, deliberately tiny. Reads one JSON `Plan` on stdin,
validates it against a closed list (`pacman -S/-Syu/-Rs/-U`, `apt-get`, `dnf`,
`snap`, `flatpak --system`, `chwd`, `dpkg -i`, `rpm`, `pacman -U` of a file
the store downloaded, and the four exact commands setting a source up needs:
`flatpak remote-add --if-not-exists --system flathub` with Flathub's own
address, `systemctl enable --now snapd.socket`, `ln -sfn /var/lib/snapd/snap
/snap` and `snap wait system seed.loaded`), refuses anything else, runs the steps, streams one
JSON event per line on stdout. It has no network code, no search, no
metadata. `packaging/io.github.spillebulle.brokey.policy` grants it
`auth_admin_keep`, so a batch of installs is one password.

When the store is run from a development build the helper beside it is used
through plain `pkexec` and the generic dialog; nothing is installed to make
that work.

### brokey

Tauri commands are thin: each one calls into `brokey-core` and returns a
serialisable value. Events from a running transaction are forwarded to the
page as `transaction://event`. Settings are a flat `key = value` file in the
config directory (Muster's `prefs.rs` shape). The text mode
(`brokey search steam`, `brokey updates`, `brokey sources`) is the
same core with a table printer, and it is how the sources are exercised on a
machine with no display.

### The page

Desktop scale throughout. Shell per §6.1 of the style guide: menu bar 34 with
the accent mark and the name, a 240 px `dock` sidebar with nav rows (Search,
Installed, Updates, Drivers, Settings), a 26 px status bar naming the
distribution and which sources are live. Content over `window`.

Pages, and the modules each is made of (§10):

| Page | Modules |
|---|---|
| Search | Toolbar (search field, sources multi-select, kind segmented, sort dropdown, installed toggle), List of app rows, Empty state |
| App detail | Backdrop header (first screenshot, long ramp) + Detail hero (icon, name, developer, editions dropdown, one primary button), key/value facts, description, Media rail of screenshots, related packages |
| Installed | Toolbar, List |
| Updates | Toolbar, notice for a self-update, List with tick boxes, primary "Update all" |
| Drivers | Panel per device with its profiles; Panel for firmware (fwupd) |
| Settings | §9 shape: theme cards, sources, update checks, danger zone |
| Activity | A floating panel bottom-right while a transaction runs: step, progress rail, log toggle; Toast on completion |

**Application icons are a fourth kind of picture** and get their own ladder,
because they are square and neither interface icons (16/20/24) nor artwork
(the 16/9 and 2/3 ladders): `--app-icon-row` 32, `--app-icon-card` 48,
`--app-icon-hero` 96. A row carrying one is sized by it (32 + 12). A missing
icon is a `control` block at the same size with the first letter in
`text-dim`, never a gap.

Sources are told apart by a **badge** (§7.13) in neutral: "pacman", "AUR",
"Flatpak", "Snap", "apt", "dnf", "GitHub". Never a coloured pill per source:
colour in this family means state.

## Rules this settles

- **Never a partial upgrade on Arch.** Every pacman install or update runs as
  `pacman -Syu` with the names appended, which is the operation Arch supports;
  the Updates page lets a subset be ticked, and on pacman it draws a notice
  saying that updating any pacman package updates every pacman package. The
  notice is not a dialog and does not block.
- **The window has no root.** Every privileged step goes through the helper
  and its closed list. A new privileged operation is a new entry in that list
  with a test, not a new `pkexec` call site.
- **A source that cannot work says why and is not drawn as if it could.** The
  sources multi-select lists an unavailable source disabled with the reason
  in its tooltip; a search never silently omits it.
- **A source that is not installed can still be searched and set up.**
  Flatpak without flatpak and Snap without snapd are `searchable`: a search
  asks their public stores (Flathub's search API, api.snapcraft.io) so their
  editions sit beside the distribution's. Installing one of those results
  sets the tool up first in the same plan (`Op::Setup` before the install:
  the tool's package through the distribution's source, then adding Flathub
  or starting snapd), and the confirm dialog says so in one sentence. The
  helper's closed list allows exactly the post-install commands that needs
  and no other spelling of them. Plan steps keep the order of the operations
  that produced them, because a setup chain depends on it; only adjacent
  package-manager calls are joined. After the plan the store detects its
  sources again, so the source is available without a restart.
- **Progress is honest.** A fraction is shown only when the total is known
  (steps in a plan, bytes of a download with a length). pacman and flatpak
  output is streamed as a log; the rail stays empty with a sentence beside it
  until a step count is known.
- **AUR builds run as the user, never as root.** Dependencies from the
  repositories are installed through the helper first; `makepkg` then runs
  in the user session with `PACMAN_AUTH=(pkexec)` for the final `-U`. paru or
  yay are used when present (`--sudo pkexec`), and the built-in path when not.
- **Grouping never lies.** An edition joined by name match carries
  `confidence < 1` and the detail page says "matched by name" in `text-dim`
  beside it. A user can split a group from the detail page if the match is
  wrong; that choice is stored.
- **Self-update never prints a command that cannot work** (§18.3). The
  installation kind is detected, and the remedy is one of: an entry in the
  Updates page (AUR package or archive-enrolled deb/rpm), a download of the
  exact release asset installed through the helper (`pacman -U`, `dpkg -i`,
  `rpm -U`), an AppImage swap, or a sentence and no name.
- **No network at draw time.** Every remote fetch is behind a command; the
  page renders from what it has and shows `control` blocks where a picture is
  still coming.
- **Copy** follows §12: British spelling, sentences, no em dashes, says what
  happens ("Install", then "Installing 3 packages", then "Installed").

## Rejected

- **libalpm / libapt bindings.** They tie the binary to one distribution's
  libraries and version, which is the opposite of one binary for every
  machine. The database formats are simple text and are parsed directly.
- **PackageKit.** It exists on every distribution and would have handled
  polkit for free, but its alpm backend is unmaintained and its progress
  reporting is coarse; driving the native tools directly is what pamac,
  Discover on Arch and every AUR helper do.
- **egui.** The family's native stack, and the right one for Umber and
  Muster. A store is pictures, paragraphs and scrolling lists, and the design
  language is already a stylesheet; a webview renders the tokens as they are
  written. The cost is a dependency on webkit2gtk, which every desktop
  distribution ships.
- **A daemon.** A long-running root service would make batching and progress
  nicer, but it is a second thing to package, start and secure. The helper
  runs for the length of one plan and exits.
- **Tailwind.** `tokens.css` and a component sheet are the whole design; a
  utility framework would be a second vocabulary for the same words.
- **Coloured source pills.** Colour means state in this family (§2.5).

## Open

- Whether to read Flatpak's OSTree remote directly for update checks instead
  of `flatpak remote-ls --updates`. The CLI is used for now.
- A PackageKit-free driver path on plain Arch (not CachyOS): `chwd` is the
  only driver manager supported today; elsewhere the Drivers page shows the
  firmware panel and says drivers are not managed on this distribution.
- Flathub submission, once the Flatpak build is made to work with the
  helper (a sandboxed store cannot run pkexec; it would need the portal).
- Reviews and ratings: ODRS is public and used by GNOME Software. Not built.
