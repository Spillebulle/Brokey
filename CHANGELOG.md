# Changelog

Newest first. Each section is the release's notes, published verbatim.

## 0.1.6

- An install asks for your password once, and only when it needs to. Brokey keeps one helper for the whole change, so an AUR build that takes an hour between installing its dependencies and installing the result no longer asks a second or third time.
- AUR packages are built by Brokey itself rather than by paru or yay, which asked for the password on their own outside Brokey's list of what may run as root. Brokey finds dependencies that are themselves on the AUR and builds them first, imports the keys a package's sources are signed with, installs only the package you asked for from a split package, and installs base-devel and git the first time a build needs them. The AUR helper setting is gone.
- An AUR build brings the system up to date first, as a repository install always has, instead of installing dependencies from package lists that may be weeks old.
- A failed install says why: pacman's database is in use, a mirror could not be reached, two packages conflict and which one to remove, a file is already on disk, the keyring is out of date, a package has left the repositories, the disk is full, or a build failed. Before, nearly every failure read "failed with exit code 1".
- Changes run one at a time. A second install waits for the first, and an install waits while Pamac or a terminal is using pacman, instead of failing at once with "unable to lock database".
- Before an install from the Arch repositories or the AUR, the confirm step says how many pending system updates come with it, and that a restart follows when the kernel or a graphics driver is among them. After a change, Brokey says when a restart is needed and when an update saved a configuration file as .pacnew beside yours.
- The progress bar moves from the first package of a large pacman transaction. It used to stay empty until the tenth.
- A -git package's page says that Brokey cannot tell when it has new commits, and has a Rebuild button that builds the latest.
- When nothing on the desktop can ask for a password, the message says to install a polkit agent on a window manager such as Sway, i3 or Hyprland, where logging out changes nothing.
- One unreadable repository database no longer takes every pacman search and update down with it: that repository is left out, and the source says so. Databases compressed with xz are read.
- Snap on Arch enables snapd's AppArmor service when AppArmor is on, and a classic snap is no longer installed as a strict one when the Snap Store cannot be reached; Brokey says so instead.
- Installing from GitHub releases checks each download against the checksum GitHub publishes, installs fuse2 when an AppImage needs it, can update a Flatpak bundle in place, and a package installed this way is no longer also listed under the AUR.

## 0.1.5

- Windows joins the platforms Brokey installs, updates and removes software on, through winget, Add or remove programs, Chocolatey and Scoop. The window never elevates: it builds the plan and a helper runs the privileged steps, costing one prompt per stretch that needs Administrator.
- There is a Windows installer for the first time: an MSI, and a setup executable that carries it and installs on a double-click. Brokey appears in Add or remove programs and can be removed from there. The `.msi` is published beside the setup executable for anyone deploying Brokey across a fleet.
- winget's catalogue is downloaded and read by Brokey itself, so search works on a machine that has never had winget installed.
- An installed application on Windows shows its own icon, read out of the file its Add or remove programs entry points at and cached as a picture the page can draw, and a winget package's detail page shows the description from its manifest.
- Chocolatey and Scoop join winget and Add or remove programs as Windows sources, so one search covers all four. Both are searched whether or not the tool is on the machine, because Chocolatey's community feed and Scoop's main bucket answer over HTTP. Installing, updating and removing through either needs the tool itself, and the sources list says which one is missing and what that costs.
- Still missing on Windows: a search result has no icon unless the Chocolatey package behind it names one, an entry whose Add or remove programs name carries its version is listed more than once, the Microsoft Store is not there yet, and the installer neither installs nor checks for the Microsoft Edge WebView2 runtime that Brokey's window needs. Windows 11 has that runtime already; on a Windows 10 machine that does not, install the WebView2 Evergreen runtime from Microsoft before installing Brokey. Windows self-update is not in this release either: an installed copy is told to update the way it was installed, which for now means running the next setup executable by hand.

## 0.1.4

- On Debian, Ubuntu and Pop!_OS the Updates page lists what apt will actually install. Before, it listed every newer version in the package lists, including ones apt keeps back because of Pop!_OS's pinned repository, backports or Ubuntu's phased updates, and updating them did nothing.
- Update all on apt also installs the new packages an update needs, as `apt upgrade` does, instead of keeping those updates back.

## 0.1.3

- The .deb installs beside Umber, Muster and any other application from the Spillebulle archive. 0.1.2 and earlier refused, because each package claimed the archive's key as its own file.

## 0.1.2

- The Update Brokey button appears in the notice for a new version. In 0.1.0 and 0.1.1 the notice said what would happen but had nothing to click, so install 0.1.2 by hand once; updates work from the notice after that.
- Settings says how this copy was installed, and says so when GitHub could not be asked instead of claiming Brokey is up to date.
- After Brokey updates itself, a notice says to restart it.

## 0.1.1

- Installed applications are recognised as applications on every Arch machine, from the desktop entry their package installs, with the name, icon and categories their launcher shows. Before, only a machine with the AppStream catalogue installed saw them, and AUR applications never counted.
- The Arch packages now depend on archlinux-appstream-data, so names, icons, descriptions and screenshots appear for repository packages you have not installed yet. The .deb and .rpm recommend the same catalogue.

## 0.1.0

- Search across pacman, the AUR, Flatpak and Snap from one box, with the same application from several sources shown as one row.
- Install, remove and update through one flow, with one password prompt per batch. On Arch every install or update runs the full system upgrade with the package included, because Arch supports nothing less.
- An Updates page that checks every source, lets you update everything or a selection, and says when a selection would be a partial upgrade on Arch.
- Application icons, descriptions and screenshots from AppStream and Flathub.
- The AUR search passes over words the AUR refuses as too common and says so when every word is.
- Flathub and the Snap Store are searched before Flatpak or snapd is installed. Installing from them sets the tool up first, and Settings has a button to set either up on its own.
- The Flatpak installation and AUR helper settings now take effect.
- The confirm step says how many times you may be asked for your password, and marks the steps that ask.
- Open an installed application from Brokey: from the toast when an install finishes, on its page, and on the Installed list.
- After Flatpak or snapd is set up, Brokey says that the launcher lists their applications only after you log out and back in once.
- Drivers through chwd on CachyOS and firmware through fwupd.
- Brokey checks for its own new versions and installs them the way this copy was installed.
