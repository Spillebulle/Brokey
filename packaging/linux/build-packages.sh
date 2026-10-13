#!/bin/bash
# Build the .deb and the .rpm from already-compiled binaries.
#
#   packaging/linux/build-packages.sh <version> <brokey> <brokey-helper> <arch> [outdir]
#
#   version     0.1.0
#   brokey   path to the compiled application (target/release/brokey)
#   brokey-helper  path to the compiled helper (target/release/brokey-helper)
#   arch        amd64 | arm64   (Debian spelling; the rpm one is derived)
#
# Written with `dpkg-deb` and `rpmbuild` directly rather than with Tauri's
# bundler, because the packages have to do two things the bundler cannot be
# told to do: install the helper and its polkit policy, and enrol the machine
# in the house archive (STYLE-GUIDE.md §18.2). The AppImage is the one format
# left to the bundler, since bundling WebKit's libraries into a portable file
# is exactly what it is good at.
#
# Runnable on any Debian-ish box with the tools installed, not only in CI.

set -euo pipefail

if [ $# -lt 4 ]; then
    sed -n '2,12p' "$0" >&2
    exit 2
fi

version=$1
binary=$2
helper=$3
arch=$4
outdir=${5:-dist}

root=$(cd -- "$(dirname -- "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

mkdir -p "$outdir"
outdir=$(cd "$outdir" && pwd)

case "$arch" in
    amd64) rpm_arch=x86_64  ;;
    arm64) rpm_arch=aarch64 ;;
    *) echo "unknown arch '$arch' (want amd64 or arm64)" >&2; exit 2 ;;
esac

for f in "$binary" "$helper"; do
    [ -x "$f" ] || { echo "no executable at '$f'" >&2; exit 1; }
done

# The window is WebKitGTK; the helper is started through pkexec, which is
# polkit's. Debian's package names for the same libraries differ between
# releases only in the appindicator line, hence the alternative.
DEB_DEPENDS="libc6, libgcc-s1, libwebkit2gtk-4.1-0, libgtk-3-0, libayatana-appindicator3-1 | libappindicator3-1, polkitd | policykit-1, pkexec | policykit-1, curl"
# appstream downloads the distribution's catalogue (names, icons, screenshots)
# with every apt update; recommended rather than required, as Debian does.
DEB_RECOMMENDS="appstream"

# RPM requirements are sonames so that Fedora, RHEL and openSUSE, which name
# the same packages differently, all resolve them.
RPM_SONAMES="libwebkit2gtk-4.1.so.0 libjavascriptcoregtk-4.1.so.0 libgtk-3.so.0 libgdk-3.so.0 libsoup-3.0.so.0"

APP_ID=io.github.spillebulle.brokey
# Spelt in full so packaging/check.sh can see that the packages, the polkit
# policy and the runner all name the same file.
HELPER=/usr/lib/brokey/brokey-helper

# --- the house archive -------------------------------------------------------
ARCHIVE_KEYRING=spillebulle-archive
ARCHIVE_BASE=https://spillebulle.github.io/packages
archive_key=$root/packaging/linux/$ARCHIVE_KEYRING.asc

if [ -f "$archive_key" ]; then
    command -v gpg >/dev/null 2>&1 || { echo "gpg is needed to dearmour $archive_key" >&2; exit 1; }
    gpg --dearmor < "$archive_key" > "$work/$ARCHIVE_KEYRING.gpg"
    echo "==> packages will enrol the machine in $ARCHIVE_BASE"
else
    archive_key=
    echo "!! $ARCHIVE_KEYRING.asc is not checked in, so these packages will not" >&2
    echo "!! enrol the machine in the archive and will never be upgraded by a" >&2
    echo "!! package manager." >&2
fi

# --- the shared install tree -------------------------------------------------
stage_tree() {
    local prefix=$1
    install -Dm755 "$binary" "$prefix/bin/brokey"
    install -Dm755 "$helper" "$prefix${HELPER#/usr}"
    install -Dm644 "$root/packaging/$APP_ID.policy" \
        "$prefix/share/polkit-1/actions/$APP_ID.policy"
    install -Dm644 "$root/packaging/$APP_ID.desktop" \
        "$prefix/share/applications/$APP_ID.desktop"
    install -Dm644 "$root/packaging/$APP_ID.metainfo.xml" \
        "$prefix/share/metainfo/$APP_ID.metainfo.xml"
    for size in 16 32 48 64 128 256; do
        install -Dm644 "$root/assets/icons/brokey-$size.png" \
            "$prefix/share/icons/hicolor/${size}x${size}/apps/$APP_ID.png"
    done
    install -Dm644 "$root/LICENSE" "$prefix/share/doc/brokey/LICENSE"
    install -Dm644 "$root/README.md" "$prefix/share/doc/brokey/README.md"
    install -Dm644 "$root/CHANGELOG.md" "$prefix/share/doc/brokey/CHANGELOG.md"
}

# --- .deb --------------------------------------------------------------------
echo "==> building brokey_${version}_${arch}.deb"
deb="$work/deb"
stage_tree "$deb/usr"
mkdir -p "$deb/DEBIAN"
# The key ships under Brokey's own name and the postinst copies it to the
# shared keyring (../Packages/ENROLMENT.md). Every application from the
# archive needs the shared file, and dpkg refuses a second package that ships
# a path another already owns, so no package may own it.
if [ -n "$archive_key" ]; then
    install -Dm644 "$work/$ARCHIVE_KEYRING.gpg" "$deb/usr/share/brokey/spillebulle-archive.gpg"
fi
size=$(du -ks "$deb/usr" | cut -f1)
cat > "$deb/DEBIAN/control" <<EOF2
Package: brokey
Version: $version
Section: admin
Priority: optional
Architecture: $arch
Depends: $DEB_DEPENDS
Recommends: $DEB_RECOMMENDS
Installed-Size: $size
Maintainer: Spillebulle <spillebulle@gmail.com>
Homepage: https://github.com/Spillebulle/Brokey
Description: One store for every way a Linux machine gets software
 Brokey searches the distribution's repositories, the AUR, Flatpak, the
 Snap Store and GitHub releases from one box, shows the same application
 from several sources as one row, installs through one flow with one
 password prompt per batch, and keeps everything, itself included, up to
 date from one page.
EOF2
cat > "$deb/DEBIAN/postinst" <<'EOF2'
#!/bin/sh
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
fi
EOF2
# The archive, exactly as ../Packages/ENROLMENT.md writes it: the shared
# keyring and the source are written by the scriptlet, never shipped, because
# a conffile is removed on purge and both are shared with every application
# from the archive. Created, never overwritten, never removed; a `.disabled`
# marker is honoured; no step fails the install. A source from before the
# contract names /usr/share/keyrings/, which an upgrade of the package that
# owned it removes, so it is pointed at the shared copy.
if [ -n "$archive_key" ]; then
    cat >> "$deb/DEBIAN/postinst" <<'EOF2'

sources=/etc/apt/sources.list.d/spillebulle.sources
keyring=/etc/apt/keyrings/spillebulle-archive.gpg
if [ ! -e "$sources.disabled" ]; then
    mkdir -p /etc/apt/keyrings || true
    [ -e "$keyring" ] || install -m644 /usr/share/brokey/spillebulle-archive.gpg "$keyring" || true
    # Packages from before this contract shipped the keyring at
    # /usr/share/keyrings/, which an upgrade removes; point at the shared copy.
    if [ -e "$sources" ] && grep -q '^Signed-By: /usr/share/keyrings/spillebulle-archive.gpg$' "$sources"; then
        sed -i "s|^Signed-By: /usr/share/keyrings/spillebulle-archive.gpg\$|Signed-By: $keyring|" "$sources" || true
    fi
    if [ ! -e "$sources" ]; then
        printf '%s\n' \
            'Types: deb' \
            'URIs: https://spillebulle.github.io/packages/deb/' \
            'Suites: ./' \
            "Signed-By: $keyring" > "$sources" || true
    fi
fi
EOF2
fi
cat > "$deb/DEBIAN/postrm" <<'EOF2'
#!/bin/sh
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
fi
EOF2
chmod 755 "$deb/DEBIAN/postinst" "$deb/DEBIAN/postrm"
dpkg-deb --build --root-owner-group "$deb" "$outdir/brokey_${version}_${arch}.deb" >/dev/null

# --- .rpm --------------------------------------------------------------------
echo "==> building brokey-${version}-1.${rpm_arch}.rpm"
rpmroot="$work/rpm"
mkdir -p "$rpmroot"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
buildroot="$work/rpmtree"
stage_tree "$buildroot/usr"
rpm_key=/etc/pki/rpm-gpg/RPM-GPG-KEY-spillebulle
if [ -n "$archive_key" ]; then
    install -Dm644 "$archive_key" "$buildroot$rpm_key"
fi
{
    echo "Name:           brokey"
    echo "Version:        $version"
    echo "Release:        1"
    echo "Summary:        One store for every way a Linux machine gets software"
    echo "License:        GPL-3.0-or-later"
    echo "URL:            https://github.com/Spillebulle/Brokey"
    echo "BuildArch:      $rpm_arch"
    for so in $RPM_SONAMES; do echo "Requires:       ${so}()(64bit)"; done
    echo "Requires:       polkit"
    # The catalogue of names, icons and screenshots; weak, as its package name
    # differs between Fedora (appstream-data) and openSUSE.
    echo "Recommends:     appstream-data"
    echo "Requires:       curl"
    echo "%global debug_package %{nil}"
    echo
    echo "%description"
    echo "Brokey searches the distribution's repositories, the AUR, Flatpak, the"
    echo "Snap Store and GitHub releases from one box and installs through one flow."
    echo
    echo "%install"
    echo "cp -a $buildroot/usr %{buildroot}/"
    [ -n "$archive_key" ] && echo "cp -a $buildroot/etc %{buildroot}/"
    echo
    echo "%post"
    echo "command -v update-desktop-database >/dev/null 2>&1 && \\"
    echo "    update-desktop-database -q /usr/share/applications || :"
    if [ -n "$archive_key" ]; then
        echo "repo=/etc/yum.repos.d/spillebulle.repo"
        echo "if [ ! -e \"\$repo\" ] && [ ! -e \"\$repo.disabled\" ]; then"
        # `|| :` because nothing here may fail the install (ENROLMENT.md).
        echo "    cat > \"\$repo\" <<'REPO' || :"
        echo "[spillebulle]"
        echo "name=Spillebulle"
        echo "baseurl=$ARCHIVE_BASE/rpm/"
        echo "enabled=1"
        echo "gpgcheck=1"
        echo "repo_gpgcheck=1"
        echo "gpgkey=file://$rpm_key"
        echo "REPO"
        echo "fi"
    fi
    echo
    echo "%postun"
    echo "command -v update-desktop-database >/dev/null 2>&1 && \\"
    echo "    update-desktop-database -q /usr/share/applications || :"
    echo
    echo "%files"
    [ -n "$archive_key" ] && echo "$rpm_key"
    echo "/usr/bin/brokey"
    echo "$HELPER"
    echo "/usr/share/polkit-1/actions/$APP_ID.policy"
    echo "/usr/share/applications/$APP_ID.desktop"
    echo "/usr/share/metainfo/$APP_ID.metainfo.xml"
    echo "/usr/share/icons/hicolor/*/apps/$APP_ID.png"
    echo "/usr/share/doc/brokey/"
} > "$rpmroot/SPECS/brokey.spec"

rpmbuild --define "_topdir $rpmroot" --define "_buildhost brokey-release" \
         -bb "$rpmroot/SPECS/brokey.spec" >/dev/null
find "$rpmroot/RPMS" -name '*.rpm' -exec cp {} "$outdir/" \;

echo
echo "built into $outdir:"
ls -1 "$outdir"
