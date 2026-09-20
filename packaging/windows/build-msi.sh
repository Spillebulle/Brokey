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
# Run from the repository root, since the paths below are relative to it.

set -eu

if [ $# -ne 3 ]; then
    echo "usage: $0 <version> <arch> <bindir>" >&2
    exit 2
fi

version="$1"
arch="$2"
bindir="$3"

for exe in brokey.exe brokey-helper.exe; do
    [ -f "$bindir/$exe" ] || {
        echo "no $exe in '$bindir'; build both first:" >&2
        echo "  cargo build --release -p brokey -p brokey-helper" >&2
        exit 1
    }
done

mkdir -p wixassets dist

cp assets/icons/brokey.ico wixassets/brokey.ico
# The installer's own artwork, drawn by `tools/make-art.py` and committed
# beside the .wxs rather than generated here.
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

echo "built dist/brokey-${version}-${arch}.msi"
