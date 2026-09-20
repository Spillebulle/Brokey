#!/bin/sh
# Check that the packaging metadata agrees with itself.
#
#   sh packaging/check.sh
#
# The rule these all serve: the AppStream component, the desktop entry, the
# icons and the polkit policy must share one name, the application id, and
# the helper path the policy names must be the one the packages install and
# the one the runner looks for. None of this needs rpm, dpkg or a display, so
# it runs on every push.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

fail() { printf 'packaging: %s\n' "$1" >&2; exit 1; }
ok()   { printf '  ok  %s\n' "$1"; }

APP_ID=io.github.spillebulle.brokey
HELPER=/usr/lib/brokey/brokey-helper

metainfo="packaging/$APP_ID.metainfo.xml"
desktop="packaging/$APP_ID.desktop"
policy="packaging/$APP_ID.policy"

[ -f "$metainfo" ] || fail "no AppStream file at $metainfo"
[ -f "$desktop" ]  || fail "no desktop entry at $desktop"
ok "metainfo and desktop entry are named for $APP_ID"

id=$(sed -n 's/.*<id>\(.*\)<\/id>.*/\1/p' "$metainfo" | head -1)
[ "$id" = "$APP_ID" ] || fail "<id> is '$id', expected '$APP_ID'"
ok "<id> is $APP_ID"

launchable=$(sed -n 's/.*<launchable[^>]*>\(.*\)<\/launchable>.*/\1/p' "$metainfo" | head -1)
[ "$launchable" = "$APP_ID.desktop" ] || \
    fail "<launchable> is '$launchable' but the desktop entry installs as '$APP_ID.desktop'"
ok "<launchable> resolves to the installed desktop entry"

icon=$(sed -n 's/^Icon=\(.*\)$/\1/p' "$desktop" | head -1)
[ "$icon" = "$APP_ID" ] || fail "Icon is '$icon' but the icons install as '$APP_ID.png'"
ok "Icon= resolves to the installed icons"

for size in 16 32 48 64 128 256; do
    [ -f "assets/icons/brokey-$size.png" ] || \
        fail "assets/icons/brokey-$size.png is missing, and every package installs it"
done
ok "all six icon sizes are present"

# The polkit policy is written by the transaction module; until it exists the
# packages cannot grant the helper anything, and that is a failure worth
# naming rather than a package that installs and then cannot install anything.
[ -f "$policy" ] || fail "no polkit policy at $policy"
grep -q "org.freedesktop.policykit.exec.path\">$HELPER<" "$policy" || \
    fail "$policy does not name $HELPER as the helper path"
ok "the policy names $HELPER"

grep -q "$HELPER" packaging/linux/build-packages.sh || \
    fail "build-packages.sh does not install the helper to $HELPER"
grep -q "$HELPER" packaging/linux/PKGBUILD || \
    fail "PKGBUILD does not install the helper to $HELPER"
grep -rq "$HELPER" crates/brokey-core/src/transaction/ || \
    fail "the runner does not look for the helper at $HELPER"
ok "packages and the runner agree on the helper path"

# The version is stated in four places; the release test checks three of them
# against Cargo.toml and this checks the fourth, the AppStream release list.
version=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
grep -q "<release version=\"$version\"" "$metainfo" || \
    fail "$metainfo has no <release version=\"$version\">; add one for this version"
ok "metainfo lists release $version"

# Windows. Nothing here needs wix, a display or Pillow, so it runs on Linux
# with everything else, which is where this script mainly runs.
wxs=packaging/windows/brokey.wxs
[ -f "$wxs" ] || fail "no WiX source at $wxs"

# The taskbar matches a running window to an installed shortcut by the
# application id, so the id the shortcut carries and the id the application
# declares have to be one string. The .wxs says so beside the property and
# nothing checked it.
aumid=$(sed -n 's/.*System.AppUserModel.ID" Value="\([^"]*\)".*/\1/p' "$wxs" | head -1)
identifier=$(sed -n 's/^[[:space:]]*"identifier"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    crates/brokey/tauri.conf.json | head -1)
[ -n "$aumid" ] || \
    fail "$wxs sets no System.AppUserModel.ID on the Start menu shortcut; add one carrying the identifier from crates/brokey/tauri.conf.json"
[ "$aumid" = "$identifier" ] || \
    fail "the shortcut's System.AppUserModel.ID is '$aumid' and tauri.conf.json's identifier is '$identifier'; spell them the same, or the taskbar cannot match a running window to the installed shortcut"
ok "the shortcut's AppUserModel.ID is $identifier"

# Five GUIDs: the UpgradeCode and one per component. Two components sharing a
# GUID makes Windows Installer treat them as one component, so an upgrade or a
# removal reference-counts the wrong files.
guids=$(sed -n -e 's/.*UpgradeCode="\([^"]*\)".*/\1/p' -e 's/.*Guid="\([^"]*\)".*/\1/p' "$wxs")
written=$(printf '%s\n' "$guids" | wc -l | tr -d ' ')
[ "$written" -eq 5 ] || \
    fail "$wxs writes $written GUIDs and the package has five, the UpgradeCode and one for each of its four components; give the component that lost its Guid attribute a fresh one"
dupe=$(printf '%s\n' "$guids" | sort | uniq -d | head -1)
[ -z "$dupe" ] || \
    fail "$wxs uses the GUID $dupe more than once; every GUID in it is Brokey's alone, so generate a fresh one for the second"
ok "the five GUIDs are five and are all different"

# WiX's stock dialog set takes exactly two bitmap sizes and no others, and a
# wrong one is a stretched picture with no error anywhere. The sizes are read
# out of the BMP headers themselves, two little-endian 32-bit integers at
# bytes 18 and 22, which od does without any image library.
bmp_int() { od -An -tu4 -j "$2" -N 4 -v "$1" | tr -d ' \n'; }

check_bmp() {
    bmp="packaging/windows/$1.bmp"
    [ -f "$bmp" ] || fail "$bmp is missing and the MSI needs it; run python tools/make-art.py"
    declared=$(sed -n "s/^$2 = (\([0-9]*\), \([0-9]*\))\$/\1 \2/p" tools/make-art.py | head -1)
    [ -n "$declared" ] || \
        fail "tools/make-art.py declares no $2 size, so there is nothing to check $bmp against; restore the $2 constant"
    drawn="$(bmp_int "$bmp" 18) $(bmp_int "$bmp" 22)"
    [ "$drawn" = "$declared" ] || \
        fail "$bmp is ${drawn% *} by ${drawn#* } pixels and tools/make-art.py draws $2 at ${declared% *} by ${declared#* }; rerun python tools/make-art.py and commit the bitmap"
    ok "$bmp is ${declared% *} by ${declared#* }, the size make-art.py draws"
}

check_bmp banner BANNER
check_bmp dialog DIALOG
