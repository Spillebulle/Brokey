# Changelog

Newest first. Each section is the release's notes, published verbatim.

## 0.1.5

- Windows joins the platforms Brokey installs, updates and removes software on, through winget and through Add or remove programs. The window never elevates: it builds the plan and a helper runs the privileged steps, costing one prompt per stretch that needs Administrator.
- There is a Windows installer for the first time: an MSI, and a setup executable that carries it and installs on a double-click. Brokey appears in Add or remove programs and can be removed from there. The `.msi` is published beside the setup executable for anyone deploying Brokey across a fleet.
- winget's catalogue is downloaded and read by Brokey itself, so search works on a machine that has never had winget installed.
- Still missing on Windows: applications have no icons or descriptions, an entry whose Add or remove programs name carries its version is listed more than once, Chocolatey, Scoop and the Microsoft Store are not there yet, and the installer neither installs nor checks for the Microsoft Edge WebView2 runtime that Brokey's window needs. Windows 11 has that runtime already; on a Windows 10 machine that does not, install the WebView2 Evergreen runtime from Microsoft before installing Brokey. Windows self-update is not in this release either: an installed copy is told to update the way it was installed, which for now means running the next setup executable by hand.

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
