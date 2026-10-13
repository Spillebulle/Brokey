#!/bin/sh
# Build brokey-bin-<version>-1-x86_64.pkg.tar.zst from the release tarball.
#
#   sh packaging/linux/build-arch-package.sh <version> <tarball> [outdir]
#
#   version   0.1.0
#   tarball   brokey-<version>-x86_64-unknown-linux-gnu.tar.gz, as the
#             release workflow's Stage step writes it
#
# Runs as root in an Arch container (makepkg refuses root, so it builds as a
# user it creates). One script for the release and for CI, so the package CI
# installs is built the way the released one is: PKGBUILD-bin with its
# brokey.install beside it, which makepkg needs and does not fetch.

set -eu

if [ $# -lt 2 ]; then
    sed -n '2,8p' "$0" >&2
    exit 2
fi

version=$1
tarball=$2
outdir=${3:-out}

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
[ -f "$tarball" ] || { echo "no tarball at '$tarball'" >&2; exit 1; }
case "$(basename "$tarball")" in
    "brokey-${version}-x86_64-unknown-linux-gnu.tar.gz") ;;
    *) echo "'$tarball' is not named brokey-${version}-x86_64-unknown-linux-gnu.tar.gz, which PKGBUILD-bin asks for" >&2
       exit 1 ;;
esac

id builder >/dev/null 2>&1 || useradd -m builder
work=$(mktemp -d)
cp "$root/packaging/linux/PKGBUILD-bin" "$work/PKGBUILD"
cp "$root/packaging/linux/brokey.install" "$work/"
cp "$tarball" "$work/"
sed -i "s/^pkgver=.*/pkgver=${version}/" "$work/PKGBUILD"
chown -R builder "$work"
su builder -c "cd '$work' && makepkg --noconfirm --nodeps"

mkdir -p "$outdir"
cp "$work"/*.pkg.tar.zst "$outdir/"
rm -rf "$work"
ls -1 "$outdir"
