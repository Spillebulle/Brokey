# Configuration

Every setting Brokey keeps, where it lives, and when to change it.

Settings save as they are changed, to a flat `key = value` file at
`~/.config/brokey/settings.conf`. Unknown keys are kept; a value that does
not parse falls back to its default and nothing else is lost. The Settings
page edits every key below; the file is for a machine set up by hand.

| Key | Default | What it does |
|---|---|---|
| `theme` | `system` | `dark`, `light`, or `system` to follow the desktop. |
| `enabled_sources` | all | Comma list of sources searched by default: `pacman,aur,flatpak,snap,apt,dnf,github,fwupd,chwd`. A source not on this machine stays listed, disabled, with the reason; Flatpak and Snap are still searched through their public stores and can be set up from Settings or by installing one of their results. |
| `show_packages` | `false` | Search shows only applications unless this is on or the toolbar toggle is used. |
| `flatpak_scope` | `system` | Whether Flatpak installs go to the system installation (asks polkit) or to `~/.local/share/flatpak` (`user`, no password). |
| `check_updates_on_start` | `true` | Check every source for updates when the window opens. |
| `self_update_check` | `true` | Ask GitHub for a newer Brokey when the window opens, at most once every six hours. |
| `update_check_minutes` | `60` | How often the Updates page re-checks while the window is open. |
| `split` | empty | Editions the user has split out of a grouped row, as `source:id`, so the grouper keeps them apart. |

Environment variables, for development and for unusual machines:

| Variable | Effect |
|---|---|
| `BROKEY_HELPER` | Path to the privileged helper, instead of `/usr/lib/brokey/brokey-helper` or the one beside the executable. |
| `BROKEY_APPSTREAM_DIR` | Colon-separated extra AppStream roots (each with `xml/` and `icons/`), read after the system ones. |
| `WEBKIT_DISABLE_DMABUF_RENDERING` | Set to `1` by Brokey itself on machines with an NVIDIA device, where WebKitGTK otherwise renders a black window. Set it yourself to any value to stop that. |
