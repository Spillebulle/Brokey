<p align="center">
  <picture>
    <source media="(prefers-color-scheme: light)" srcset="docs/images/banner-paper.png">
    <img src="docs/images/banner.png" alt="Brokey" width="560">
  </picture>
</p>

<p align="center">
  One store for every way a Linux machine gets software, built for one thing above all others: <b>one search, one install, every source</b>.
</p>

<p align="center">
  Searches pacman, the AUR, Flatpak, Snap, apt, dnf and GitHub releases · installs through one flow ·
  updates everything from one page · drivers and firmware · keeps itself current
</p>

![The Brokey window: a search for "steam" with the pacman, AUR and Flatpak editions grouped as one row, the sidebar and the status bar](docs/images/window.png)

> **Early days.** Searching, installing, removing and updating work on Arch and its derivatives, with Flatpak and Snap wherever they are installed. apt and dnf are written but have not yet run on a real Debian or Fedora. [What is not there yet](#what-is-not-there-yet) is honest about the rest.

## Install

**Brokey 0.1.4.** Take the file for your system, or browse the
[release itself](https://github.com/Spillebulle/Brokey/releases/latest) for the
notes and the checksums.

| Your system | x86-64 | ARM64 |
|---|---|---|
| Arch, CachyOS, EndeavourOS, Manjaro | [`.pkg.tar.zst`](https://github.com/Spillebulle/Brokey/releases/download/v0.1.4/brokey-bin-0.1.4-1-x86_64.pkg.tar.zst) | not built |
| Debian, Ubuntu, Mint, Pop!_OS | [`.deb`](https://github.com/Spillebulle/Brokey/releases/download/v0.1.4/brokey_0.1.4_amd64.deb) | [`.deb`](https://github.com/Spillebulle/Brokey/releases/download/v0.1.4/brokey_0.1.4_arm64.deb) |
| Fedora, RHEL, openSUSE | [`.rpm`](https://github.com/Spillebulle/Brokey/releases/download/v0.1.4/brokey-0.1.4-1.x86_64.rpm) | [`.rpm`](https://github.com/Spillebulle/Brokey/releases/download/v0.1.4/brokey-0.1.4-1.aarch64.rpm) |
| Any other Linux | [AppImage](https://github.com/Spillebulle/Brokey/releases/download/v0.1.4/Brokey-0.1.4-x86_64.AppImage), one file with nothing to install | [AppImage](https://github.com/Spillebulle/Brokey/releases/download/v0.1.4/Brokey-0.1.4-aarch64.AppImage) |

The `.deb` and `.rpm` add the [Spillebulle archive](https://spillebulle.github.io/packages/)
as they install, so `apt upgrade` or your usual system update carries Brokey
along with everything else. The Arch package is in no repository, so Brokey
updates it itself: it downloads the next release's package and installs it with
pacman when you say so. An AUR package is coming.

You need WebKitGTK 4.1 and polkit, which every desktop distribution ships and the
packages pull in. Installing asks for your password through the same prompt your
desktop uses for everything else, and the confirm step says beforehand how many
times it will ask.

Brokey checks for its own new versions when it starts and shows a notice with
the release notes. You can turn that off in **Settings**.

## Search

![Search results for gimp: one row for GIMP with its pacman, AUR and Flatpak editions, and the source, kind and sort controls in the toolbar](docs/images/search.png)

One box searches every source the machine has at once. The same application
from several sources is **one row with several editions**, joined by its
AppStream id where both sides have one and by name where they do not; a match
by name says so, and can be split if it is wrong.

Filter by source, by applications or every package, by installed; sort by
relevance, name, last updated, popularity or size. Flathub and the Snap Store
are searched even when Flatpak or snapd is not installed yet: installing from
them sets the tool up first in the same step, and Settings has a button to set
either up on its own.


## Details and install

![The Steam detail page: a screenshot as backdrop, the icon and name, the edition picker, the facts column and the description](docs/images/detail.png)

Icons, descriptions and screenshots come from AppStream and Flathub. The
facts column shows the version, size, licence and last update of the edition
in hand; the dropdown picks which edition to install.

Installing runs through one activity panel: what is happening, a progress
rail when the total is known and a sentence when it is not, and the full log
a click away. Everything that needs root goes through one small helper with a
closed list of commands, so the window itself never runs as root.


## Updates

![The Updates page: Brokey's own update first, then applications and packages with tick boxes, versions, sizes and the Update all button](docs/images/updates.png)

Every source is checked, without root, and the list can be updated whole or
by selection. On Arch a selection draws a notice: partial upgrades are not
supported there, and Update all is the safe choice.

Brokey's own update appears at the top and installs the way this copy was
installed.


## Drivers and firmware

On CachyOS the Drivers page lists each device with the profiles chwd offers
and which is installed. Firmware updates come from fwupd on every
distribution. Where no driver manager is known, the page says so rather than
guessing.

## What is not there yet

- apt and dnf are written to the same interface as pacman but have not run on a real Debian or Fedora. They are marked untested in the sources list until they have.
- Drivers are managed only through chwd (CachyOS). Elsewhere they install as ordinary packages.
- No reviews or ratings.
- An application installed by hand, such as a browser unpacked into your home folder, is not listed: no package manager has a record of it.
- AppImages from GitHub releases are placed in `~/.local/bin` without a menu entry yet.
- A Flatpak of Brokey itself is not published: a sandboxed store cannot reach the helper.
- Windows support is being built and is not in a release yet. On Windows, Brokey searches winget's catalogue, which it downloads and reads itself, so search works on a machine that has never had winget. It lists what Add/Remove Programs knows about, matches those entries to winget packages to find what has an update, and works out how each entry would be removed. Nothing runs yet: the helper is a stub on Windows, so a plan is built and nothing carries it out. Chocolatey, Scoop and the Microsoft Store come next.

## Controls

| Key | Does |
|---|---|
| `Ctrl` `F` or `/` | Focus the search box |
| `Escape` | Clear the search, close a menu or a dialog |
| `Enter` | Open the selected row |
| `Backspace` | Back to the results from a detail page |
| `Ctrl` `,` | Settings |

## Building from source

```sh
git clone https://github.com/Spillebulle/Brokey && cd Brokey
npm ci                        # the page
cargo build --release         # brokey, brokey-helper
cargo run -p brokey -- search steam     # the text mode, no window needed
npm run app:dev               # the window, needs webkit2gtk-4.1 and its headers
```

How the sources, the grouping and the helper fit together is in `docs/architecture.md`.

## Licence

GPL-3.0-or-later. Archivo is bundled under the SIL Open Font Licence
(`assets/fonts/OFL.txt`); icons are [Lucide](https://lucide.dev), ISC.
