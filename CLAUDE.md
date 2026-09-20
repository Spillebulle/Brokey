# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Brokey is a desktop application store: one search across every place a machine
gets software, one install flow, one updates page, and an application that
updates itself through the same machinery. The name is from brokering: it
stands between the user and every package manager the machine has.
`docs/architecture.md` is the design document and is where the shape was
settled; read it before changing the shape. `brokey-core` is one library for
both platforms, split by `#[cfg]` where the machine actually differs
(`system/`, `sources/{linux,windows}/`, `launch.rs`, `transaction/runner.rs`,
`transaction/elevate/`);
see `docs/superpowers/specs/2026-09-19-windows-support-design.md` for the
Windows side of that split and what it supersedes in `architecture.md`.

**Early and building out.** The first machine is Arch (CachyOS): pacman and the
AUR are the reference sources. apt and dnf are written to the same interface and
are marked untested until they have run on a real Debian and Fedora. Windows
has four sources, in the order the page draws them: Add/Remove Programs
(`sources/windows/arp.rs`), winget (`sources/windows/winget/`), Chocolatey
(`sources/windows/choco.rs`) and Scoop (`sources/windows/scoop.rs`). None of
them shells out to read anything: winget's catalogue is downloaded and read by Brokey
itself, which is why search works without winget installed, and its installed
list is the registry joined to that catalogue; Chocolatey searches the
community feed over HTTP and reads its own `lib` directory; Scoop reads the
buckets on disk, or the main bucket over HTTP when there are none.
`brokey-helper` runs on Windows too, elevated through `ShellExecuteEx` and
answering on a pair of named pipes, so installing, updating and removing
work there. `README.md`'s "What is not there yet" is the
user-facing list and is kept honest.

The house reference for conventions is `../Muster` and `../Umber` (Rust
workspaces with the same release shape, the same updater rules, the same
packaging scriptlets). When a question here has an answer there, take it.

UI follows `../Design-Principles/STYLE-GUIDE.md` and uses `tokens.css`. The
accent is `#D42B48`, set as `--accent-fixed` with `--accent-h` at `18` and
`--accent-ink-fixed` at `#FFFFFF`: Brokey is the one app that takes the style
guide's §2.3 exception for a brand colour that must match exactly, so the
accent is not derived from the hue and `--accent-ink` resolves to white
rather than the house near-black. Those three values are the only thing
edited in this copy of `tokens.css`. Desktop application,
so **never** `class="web"` on the root.
Never a raw hex in a component.

## Decisions

| | |
|---|---|
| Language | Rust 2024 edition, stable toolchain, one workspace; TypeScript for the page |
| Interface | Tauri 2 + React 19 + Vite. Plain CSS: `frontend/src/tokens.css` (copied from Design-Principles, with the three accent values above set here under the style guide's 2.3 exception and nothing else edited) and `frontend/src/app.css` (components). No Tailwind, no CSS-in-JS |
| Icons | Lucide, through `lucide-react`. Nothing hand-drawn; nothing from a CDN |
| Font | Archivo, bundled from `assets/fonts/` |
| Databases | Read directly in pure Rust. No libalpm, no libapt, and on Windows no shelling out to winget/choco/etc to parse their output |
| Privilege | Linux: `brokey-helper` via `pkexec` with a closed list of operations. Windows: `ShellExecuteEx` with `runas`, the plan and the events carried on two named pipes whose DACL admits only this user and Administrators, and a closed list of its own. The window never runs as root or Administrator |
| Targets | Linux x86-64 and ARM64, and Windows x86-64 and ARM64. See `docs/superpowers/specs/2026-09-19-windows-support-design.md`, which supersedes the "Targets" row of `docs/architecture.md` |

## Commands

```sh
# Rust
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
cargo run -p brokey -- search steam        # text mode: exercises the sources without a window
cargo run -p brokey -- sources             # which sources this machine has and why not
cargo run -p brokey -- updates

# Frontend. package.json and node_modules are at the repository root, not in
# frontend/, so these run from the root (Node is in ~/.local/bin on the Linux
# development machine)
npm install && npm run build   # tsc + vite build into frontend/dist
npm run dev                    # vite dev server on :1420 with the mock backend

# The window. The scripts are app:dev and app:build; there is no `tauri dev`
# script, and `npm run tauri dev` would start the window with no page built
npm run app:dev                # Linux needs webkit2gtk-4.1 installed on the machine
npm run app:build

# Packaging sanity, no tools needed
sh packaging/check.sh
```

## Building without root

The development machine has no `webkit2gtk-4.1` installed and `sudo` needs a
password, so the package and its six missing dependencies were extracted from
the Arch repositories into `~/.local/opt/webkit` (no root needed) and Node into
`~/.local/opt/node`. `. tools/dev-env.sh` points pkg-config, the loader and
WebKit's process path at them; source it before `cargo check -p brokey` or
`cargo build`. It is enough to compile and to run every test, but not to open
the window: WebKitGTK spawns `/usr/lib/webkit2gtk-4.1/WebKitNetworkProcess`
from a path compiled into the library and ignores `WEBKIT_EXEC_PATH` in a
release build, so `npm run app:dev` dies with "Unable to spawn a new child
process". The window needs the real package: `sudo pacman -S --needed
webkit2gtk-4.1`. Until then the page is exercised in a browser against the
mock (`npm run dev`, then `http://localhost:1420/?view=search&q=steam&fast`)
and the core through the text mode.

## Layout

```
crates/brokey-core/src/
  lib.rs            re-exports; the Source trait; Package, App, Update, Plan, Step, Event
  model.rs          the data types, serde-derived, shared with the page as JSON
  sources/          one module per source, split by platform and selected by #[cfg]
    linux/          pacman.rs, aur.rs, flatpak.rs, snap.rs, apt.rs, dnf.rs, github.rs, fwupd.rs, chwd.rs
    windows/        arp.rs (Add/Remove Programs), winget/ (mod.rs, index.rs,
                    query.rs, version.rs), choco.rs, scoop.rs, pe.rs and
                    icon.rs (an installed application's own icon); the
                    Microsoft Store comes next, per the Windows spec
  appstream/        catalogue XML parser, icon resolution, index
  group.rs          packages -> apps. Pure. Fixture-tested
  transaction/      Plan building and the Runner (spawns the helper and user-session steps)
    elevate/        the one place that obtains privilege: unix.rs is pkexec with
                    piped stdio, windows.rs is ShellExecuteEx with runas and the
                    two named pipes the plan and the events travel on
  updates.rs        merged update list
  selfupdate/       install detection (Muster's install.rs shape), release check, remedy
  system/           mod.rs, linux.rs, windows.rs: which OS this is, which tools exist, paths
  vercmp.rs         pacman version comparison, tested against `vercmp`
  http.rs           one reqwest client with a user agent and a disk cache for icons
crates/brokey-helper/src/main.rs
crates/brokey/src/
  main.rs           text mode dispatch, then the window
  lib.rs            Tauri builder, plugins, state
  cli.rs            the text mode itself: argument parsing and its output
  commands.rs       every #[tauri::command], thin
  state.rs          the store and the settings the commands share
  settings.rs       key = value preferences
  setup/            the Windows setup executable, and the one sanctioned
                    exception to the first invariant: mod.rs (lifting the MSI
                    out, staging it, elevating msiexec), payload.rs (the
                    package carried on the end of the binary and its footer),
                    window.rs (the hand-written Win32 and GDI window it draws)
crates/brokey/icons/icon.ico
                    required by tauri-build for a Windows target, whatever
                    tauri.conf.json's bundle.icon lists
frontend/src/
  main.tsx, App.tsx shell, routing by view state
  api.ts            typed wrappers over invoke(); mock.ts stands in under `vite dev`
  tokens.css        copied from Design-Principles; the only edits here are
                    --accent-h, --accent-fixed and --accent-ink-fixed
  app.css           components, one block per §7 component, prefixed class names
  components/       Button, Dropdown, MultiSelect, Segmented, Toggle, Field, Badge, Row, Card, Panel, Dialog, Toast, Progress
  pages/            Search, AppDetail, Installed, Updates, Drivers, Settings
  activity/         the transaction panel and its event store
```

## Invariants

These were decided before the first line and are not re-litigated in a fix:

- **The window has no root.** A privileged operation is an entry in
  `brokey-helper`'s closed list, with a test that the helper refuses anything
  else. Never an elevation call site, `pkexec` or `ShellExecuteEx`, outside
  `transaction/elevate/`. **There is one exception and it is the setup
  executable**, `crates/brokey/src/setup/mod.rs`'s `run_installer`, which
  elevates `msiexec` on the MSI it has just lifted out of its own file. It
  cannot go through the helper: `brokey-helper.exe` is one of the files that
  MSI installs, so there is none on the machine yet. It is held narrow in code
  rather than in this sentence, in three places that each close what the last
  one leaves open: `install` reads `current_exe()` **before the window opens**,
  so the file cannot be swapped while somebody reads it; `run_installer` takes
  a `Staged`, whose fields only the `staging` module can fill and only out of
  bytes `payload::read` lifted from that image; and `still_the_package`
  compares the staged file byte for byte immediately before the prompt, so a
  file that changed after it was written is refused and nothing is elevated.
  The window it draws is never elevated itself, and neither is the Brokey the
  install leaves behind, with the one qualification
  `packaging/windows/brokey.wxs` records: an install started from a console
  that is already elevated leaves the msiexec client elevated too, so the
  MSI's Start Brokey tickbox starts Brokey elevated whatever
  `Impersonate="yes"` says, and nothing in a `.wxs` can prevent it. Anyone
  installing that way should untick the box, and anyone testing this
  invariant should know that an install from an administrator console proves
  nothing about it.
- **A source never runs anything.** `Source::plan` returns steps; the Runner
  runs them. This is what makes every source testable with fixtures.
- **Opening an installed application is the one process started outside the
  Runner, on Linux.** It changes nothing and never runs as root, so it is not
  a plan: `Source::launcher` finds the desktop entry in the package's own file
  list (or answers `flatpak run` / `snap run` / an AppImage), and
  `commands::logic::start` hands an entry to `gio launch`. Never parse `Exec`
  here. `crates/brokey-core/src/launch.rs` explains why, and why a session that
  started before Flatpak or snapd was installed cannot list their
  applications until the user logs in again. `launch.rs` and `Source::launcher`
  are `#[cfg(unix)]`; Windows has no launcher yet.
- **Grouping is a pure function** of `Vec<Package>` (`group.rs`). Adding a
  heuristic means adding a fixture where it fires and one where it must not.
- **No partial upgrade on Arch presented as safe.** See `docs/architecture.md`.
- **Progress is honest.** `Event::Progress` carries `Option<f32>`; `None` draws
  an empty rail and a sentence. Never fake a percentage.
- **Every source reports availability with a reason.** The page draws it
  disabled with that reason in the tooltip. A source is never silently skipped.
- **Copy rules apply to strings in Rust and TypeScript alike:** British
  spelling, sentence case, full stops in sentences, no em dashes, no emoji,
  says what happens. Errors name what went wrong and what to do.
- **One theme, three states** (dark / light / system) through `tokens.css`;
  components never read the theme class. A colour that is missing is a token
  added in **all three** blocks of `tokens.css` upstream, not a hex here.
- **Application icons have their own ladder** (`--app-icon-row` 32,
  `--app-icon-card` 48, `--app-icon-hero` 96), declared in `app.css` beside
  the tokens they extend and used by name. Interface icons stay 16/20/24.
- **Never an accent background** for a selected row, tab, nav item or card.
  Selection is `control` fill + strong text + a small accent mark.
- **The self-updater never prints a command that cannot work.** Every
  remedy is chosen by `selfupdate::install::detect`, a pure function of a
  probe, and `selfupdate/tests` assert the forbidden sentences never appear.
- **The page's types are the contract.** Every value the Rust side sends is
  checked against `frontend/src/types.ts` by `crates/brokey/tests/contract.rs`:
  keys, nested types, tags and enum words. A new field or command result gets
  a sample there. 0.1.0 shipped a self-update button that never appeared
  because the two sides spelt a tag differently and each tested its own.
- **CHANGELOG.md is the release notes.** `crates/brokey/tests/release.rs`
  fails if the section for the current version is missing or is not newest.
- **`frontend/src/tokens.css` is a copy, and the only thing edited in it
  here is the accent.** `--accent-h`, `--accent-fixed` and
  `--accent-ink-fixed` are set in this copy, under the style guide's 2.3
  exception for a brand colour that must match exactly. Every other change
  goes to Design-Principles first and is copied back.

## Things that look like shortcuts and are not

- Calling `pacman`, `flatpak` or `apt` from a Tauri command directly.
  Everything runs through a Plan so it is logged, batched, and cancellable.
- Parsing `pacman -Ss` output. The sync database is a tar of `desc` files;
  `sources/linux/pacman.rs` reads it and is ten times faster.
- Showing a spinner over an unknown. §7.18: an empty rail and a sentence.
- Using `<select>`, `<input type=checkbox>` or a stock `<button>` unstyled.
  Every control in §7 is painted in `app.css`; the sample markup in
  `Design-Principles/styleguide.src.html` is what to copy.
- A per-source colour. Colour is state; sources are neutral badges.
- Loading Lucide or Archivo from a URL. Both are bundled.
- Adding a fifth text rank or a second accent-tinted neutral.

## Testing

- `cargo test --workspace` runs everything that needs no network and no root:
  parsers against fixtures in `crates/brokey-core/tests/fixtures/`, grouping,
  vercmp against a table generated from `vercmp`, helper plan validation, the
  self-update remedies, the changelog guard.
- Tests that need the network are `#[ignore]` and named `live_*`; run them with
  `cargo test -- --ignored live_` on a machine that is online.
- The frontend has `npm run lint` (tsc) and `npm run check:design`, which
  greps `src/` for raw hexes, em dashes in strings, and `class="web"`.
- Screenshots for the README are taken by `tools/shots.mjs` against
  `vite dev` with the mock backend, dark theme, 1500 px wide, per §17.3.
