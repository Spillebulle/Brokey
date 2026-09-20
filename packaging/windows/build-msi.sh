#!/bin/sh
# Builds dist/brokey-<version>-<arch>.msi from an already-built binary pair.
#
#   sh packaging/windows/build-msi.sh <version> <arch> <bindir>
#   sh packaging/windows/build-msi.sh 0.1.4 x64 target/release
#
# One script so a person and CI run the same thing. The release workflow calls
# it rather than repeating the command line, which is how the two stopped
# agreeing in a sibling.
#
# Needs: wix 5, WixToolset.UI.wixext, WixToolset.Util.wixext.
#
#   dotnet tool install --global wix --version 5.0.2
#   wix extension add -g WixToolset.UI.wixext/5.0.2
#   wix extension add -g WixToolset.Util.wixext/5.0.2
#
# Util supplies WixShellExec, which is what the exit dialog's "Start Brokey"
# checkbox runs. It is pinned to the same version as the toolset: a custom
# action from a mismatched extension is a binary that may not be there under
# the name the .wxs asks for.
#
# Runs from anywhere: it moves to the repository root itself, the way
# `packaging/check.sh` does, so the paths below can all be repository paths.
# `<bindir>` is the one exception and stays the caller's, which is why it is
# resolved before that move.

set -eu

if [ $# -ne 3 ]; then
    echo "usage: $0 <version> <arch> <bindir>" >&2
    exit 2
fi

version="$1"
arch="$2"

# `<bindir>` is relative to wherever the caller is standing, not to the
# repository root, so it is made absolute here while that is still true. This
# also catches a directory that does not exist, before anything is copied.
bindir=$(CDPATH= cd -- "$3" 2>/dev/null && pwd) || {
    echo "no directory at '$3'." >&2
    exit 1
}

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$root"

for exe in brokey.exe brokey-helper.exe; do
    [ -f "$bindir/$exe" ] || {
        echo "no $exe in '$bindir'; build both first:" >&2
        echo "  cargo build --release -p brokey -p brokey-helper" >&2
        exit 1
    }
done

# The installer's own artwork and the application icon, drawn by
# `tools/make-art.py` and committed rather than generated here. Checked by name
# for the same reason the executables above are: a bare `cp` failure under
# `set -e` says what went wrong and not what to do.
for asset in packaging/windows/banner.bmp packaging/windows/dialog.bmp assets/icons/brokey.ico; do
    [ -f "$asset" ] || {
        echo "no $asset, which is committed and should be in the checkout." >&2
        echo "Draw the pictures and the icons again with:" >&2
        echo "  python tools/make-art.py" >&2
        exit 1
    }
done

mkdir -p wixassets dist

cp assets/icons/brokey.ico wixassets/brokey.ico
cp packaging/windows/banner.bmp packaging/windows/dialog.bmp wixassets/
sh packaging/windows/make-licence-rtf.sh LICENSE wixassets/licence.rtf

# The pdbtype option is set to none because WiX otherwise drops a .wixpdb
# beside the installer, which is not something to publish and not something to
# explain: the release upload takes everything in dist/.
wix build packaging/windows/brokey.wxs \
    -arch "$arch" \
    -ext WixToolset.UI.wixext \
    -ext WixToolset.Util.wixext \
    -pdbtype none \
    -d Version="$version" \
    -d BinDir="$bindir" \
    -d DocDir="." \
    -d AssetDir="wixassets" \
    -o "dist/brokey-${version}-${arch}.msi"

echo "built $root/dist/brokey-${version}-${arch}.msi"
