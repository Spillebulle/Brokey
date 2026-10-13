#!/bin/sh
# Install Brokey's .deb beside other applications that enrol the machine in
# the archive, and check every rule of ../Packages/ENROLMENT.md for apt.
#
#   sudo sh packaging/linux/test-deb-keyring.sh dist/brokey_<version>_amd64.deb
#
# The defect the contract exists to prevent only shows when two applications
# are installed: dpkg refuses a package that ships a path another package
# already owns, so Brokey 0.1.2 would not install beside Umber. So this
# installs a second enrolling package (Brokey's own .deb, cut down and renamed)
# and a stand-in for a package from before the contract, which owned the
# keyring under /usr/share/keyrings/. Runs as root on a throwaway machine
# (CI): it installs and removes packages and deletes the archive's files.

set -eu

deb=$1
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
keyring=/etc/apt/keyrings/spillebulle-archive.gpg
old_keyring=/usr/share/keyrings/spillebulle-archive.gpg
shipped=/usr/share/brokey/spillebulle-archive.gpg
sources=/etc/apt/sources.list.d/spillebulle.sources
twin=spillebulle-twin
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fail() { printf 'deb enrolment: %s\n' "$1" >&2; exit 1; }
ok()   { printf '  ok  %s\n' "$1"; }

gpg --dearmor < "$root/packaging/linux/spillebulle-archive.asc" > "$work/expected.gpg"

# --- the second enrolling package --------------------------------------------
# Brokey's .deb with everything but its key and its scriptlets taken out, the
# key moved under the twin's own name, and the postinst pointed at it. It runs
# the very postinst under test, so it enrols exactly as another application
# following the contract would.
dpkg-deb -R "$deb" "$work/twin"
[ -f "$work/twin$shipped" ] || fail "the .deb does not ship the key at $shipped"
mkdir -p "$work/twin-key"
mv "$work/twin$shipped" "$work/twin-key/"
rm -rf "$work/twin/usr"
mkdir -p "$work/twin/usr/share/$twin"
mv "$work/twin-key/spillebulle-archive.gpg" "$work/twin/usr/share/$twin/"
rm -f "$work/twin/DEBIAN/md5sums" "$work/twin/DEBIAN/conffiles"
sed -i "s|/usr/share/brokey/|/usr/share/$twin/|g" "$work/twin/DEBIAN/postinst"
cat > "$work/twin/DEBIAN/control" <<EOF
Package: $twin
Version: 0.1.0
Architecture: all
Maintainer: Spillebulle <spillebulle@gmail.com>
Description: Stand-in for a second application that enrols the machine
EOF
dpkg-deb --build --root-owner-group "$work/twin" "$work/twin.deb" >/dev/null

# --- a package from before the contract -------------------------------------
# Umber and Muster 0.x shipped the keyring at /usr/share/keyrings/ as a file
# dpkg owns, and wrote a source naming it.
mkdir -p "$work/old/DEBIAN" "$work/old/usr/share/keyrings"
cp "$work/expected.gpg" "$work/old$old_keyring"
cat > "$work/old/DEBIAN/control" <<EOF
Package: spillebulle-old
Version: 0.1.0
Architecture: all
Maintainer: Spillebulle <spillebulle@gmail.com>
Description: Stand-in for an application that owned the archive keyring
EOF
cat > "$work/old/DEBIAN/postinst" <<EOF
#!/bin/sh
if [ ! -e $sources ]; then
    printf '%s\n' 'Types: deb' 'URIs: https://spillebulle.github.io/packages/deb/' \
        'Suites: ./' 'Signed-By: $old_keyring' > $sources
fi
EOF
chmod 755 "$work/old/DEBIAN/postinst"
dpkg-deb --build --root-owner-group "$work/old" "$work/old.deb" >/dev/null

# --- helpers ----------------------------------------------------------------
# With its real dependencies, through apt, as somebody installing it would.
install_brokey() {
    apt-get install -y -qq "$(realpath "$deb")" >/dev/null || fail "$1"
}
key_is_there() {
    cmp -s "$keyring" "$work/expected.gpg" && return
    ls -l /etc/apt/keyrings >&2 || true
    fail "$1"
}
source_names_the_shared_keyring() {
    [ -f "$sources" ] || fail "$1: no $sources"
    grep -qx "Signed-By: $keyring" "$sources" || { cat "$sources" >&2; fail "$1: $sources does not name $keyring"; }
    grep -qx 'URIs: https://spillebulle.github.io/packages/deb/' "$sources" || fail "$1: wrong URIs in $sources"
    grep -qx 'Suites: ./' "$sources" || fail "$1: wrong Suites in $sources"
}
owned_by_nobody() {
    for path in "$keyring" "$sources"; do
        if dpkg -S "$path" >/dev/null 2>&1; then
            fail "$path belongs to a package, so the next application cannot have it"
        fi
    done
}
clean() {
    dpkg --purge brokey "$twin" spillebulle-old >/dev/null 2>&1 || true
    rm -f "$keyring" "$old_keyring" "$sources" "$sources.disabled"
}

# --- on its own ------------------------------------------------------------
clean
install_brokey "Brokey does not install on a machine without the archive"
dpkg -S "$shipped" 2>/dev/null | grep -q '^brokey:' || fail "$shipped is not Brokey's own file"
key_is_there "Brokey installed on its own writes no shared keyring"
source_names_the_shared_keyring "Brokey on its own"
owned_by_nobody
[ ! -e "$old_keyring" ] || fail "Brokey still writes $old_keyring"
ok "on its own it writes the shared keyring and the source, and owns neither"

# --- beside a second application, both orders -------------------------------
dpkg -i "$work/twin.deb" >/dev/null || fail "a second enrolling package does not install after Brokey"
key_is_there "the second package changed the keyring"
dpkg --purge "$twin" >/dev/null
key_is_there "removing the second package took the keyring away"
source_names_the_shared_keyring "after removing the second package"
ok "a second enrolling package installs after Brokey and leaves it enrolled when removed"

clean
dpkg -i "$work/twin.deb" >/dev/null || fail "the second package does not install on its own"
key_is_there "the second package on its own writes no keyring"
install_brokey "Brokey does not install beside a second enrolling package"
key_is_there "the keyring changed when Brokey was installed second"
dpkg --purge brokey >/dev/null
key_is_there "purging Brokey removed the keyring"
source_names_the_shared_keyring "after purging Brokey"
ok "Brokey installs after a second enrolling package, and purging it removes nothing shared"

# --- beside a package from before the contract -------------------------------
clean
dpkg -i "$work/old.deb" >/dev/null
grep -qx "Signed-By: $old_keyring" "$sources" || fail "the old stand-in wrote no old source"
install_brokey "Brokey does not install beside a package that owns the old keyring"
key_is_there "Brokey beside the old package wrote no shared keyring"
source_names_the_shared_keyring "migrating an old source"
dpkg --purge spillebulle-old >/dev/null
[ ! -e "$old_keyring" ] || fail "the stand-in did not take its keyring with it, so this proves nothing"
key_is_there "removing the old package took the shared keyring away"
ok "an old source is pointed at the shared keyring and survives the old package going"

# --- created, never overwritten ---------------------------------------------
clean
printf '%s\n' 'Types: deb' 'URIs: https://mirror.example/deb/' 'Suites: ./' \
    "Signed-By: $keyring" '# edited by hand' > "$sources"
printf 'not a key\n' > "$keyring"
install_brokey "Brokey does not install over an edited source"
grep -qx '# edited by hand' "$sources" || fail "Brokey overwrote an edited source"
grep -qx 'URIs: https://mirror.example/deb/' "$sources" || fail "Brokey overwrote an edited source"
[ "$(cat "$keyring")" = 'not a key' ] || fail "Brokey overwrote an existing keyring"
ok "an edited source and an existing keyring are left as they are"

# --- the opt-out ------------------------------------------------------------
clean
touch "$sources.disabled"
install_brokey "Brokey does not install on a machine that opted out"
[ ! -e "$sources" ] || fail "Brokey wrote $sources despite $sources.disabled"
[ ! -e "$keyring" ] || fail "Brokey wrote $keyring despite $sources.disabled"
dpkg -i "$work/twin.deb" >/dev/null
[ ! -e "$sources" ] || fail "an upgrade wrote $sources despite $sources.disabled"
ok "the .disabled marker is honoured"

# --- apt accepts what was written -------------------------------------------
clean
install_brokey "Brokey does not install"
apt-get update --error-on=any \
    -o Dir::Etc::sourcelist="$sources" -o Dir::Etc::sourceparts=- \
    -o APT::Get::List-Cleanup=0 >/dev/null || fail "apt does not accept the archive with this keyring"
ok "apt reads the archive with the keyring and source Brokey wrote"

# Left installed and enrolled, so the CI job can read the result.
