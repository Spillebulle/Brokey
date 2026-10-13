#!/bin/sh
# Install the Arch package with its scriptlets and check it enrolled the
# machine in the archive exactly as ../Packages/ENROLMENT.md says.
#
#   sh packaging/linux/test-arch-enrolment.sh out/brokey-bin-<version>-1-x86_64.pkg.tar.zst
#
# Runs as root in a throwaway archlinux container (CI): it installs packages
# and edits /etc/pacman.conf.

set -eu

pkg=$1
fpr=7A9EA69E033F3C3EF4F53EB4A72BC0FD5E061ADA
conf=/etc/pacman.d/spillebulle

fail() { printf 'arch enrolment: %s\n' "$1" >&2; exit 1; }
ok()   { printf '  ok  %s\n' "$1"; }

# The archlinux image ships its keyring without the local master key, which
# every installed system has (`pacman-key --init` at install time) and which a
# local signature needs.
pacman-key --init >/dev/null 2>&1
pacman -Sy --noconfirm --needed archlinux-keyring >/dev/null

# The opt-out first: with the marker, nothing at all.
touch "$conf.disabled"
pacman -U --noconfirm "$pkg" >/dev/null || fail "the package does not install"
grep -q '^\[spillebulle\]' /etc/pacman.conf && fail "enrolled despite $conf.disabled"
[ ! -e "$conf" ] || fail "wrote $conf despite $conf.disabled"
ok "the .disabled marker is honoured"
pacman -R --noconfirm brokey-bin >/dev/null
rm -f "$conf.disabled"

# Installed with its scriptlets.
pacman -U --noconfirm "$pkg" >/dev/null || fail "the package does not install"
[ -f /usr/share/brokey/spillebulle-archive.asc ] || fail "the package does not ship its key"
[ "$(grep -c '^\[spillebulle\]' /etc/pacman.conf)" = 1 ] || fail "no [spillebulle] section in /etc/pacman.conf"
grep -qx "Include = $conf" /etc/pacman.conf || fail "the section does not include $conf"
[ -f "$conf" ] || fail "no $conf"
grep -qx 'SigLevel = Required DatabaseRequired' "$conf" || fail "$conf has the wrong SigLevel"
grep -qx 'Server = https://spillebulle.github.io/packages/arch/$arch' "$conf" || fail "$conf has the wrong Server"
pacman-key --list-keys "$fpr" >/dev/null 2>&1 || fail "the key is not in pacman's keyring"
pacman-key --list-sigs "$fpr" 2>/dev/null | grep -q '^sig' || fail "the key is not signed locally"
ok "installing enrols the machine: the section, $conf and the key, trusted"

# Created, never overwritten; an upgrade adds nothing twice.
printf '# edited by hand\n' >> "$conf"
pacman -U --noconfirm "$pkg" >/dev/null || fail "the package does not reinstall"
[ "$(grep -c '^\[spillebulle\]' /etc/pacman.conf)" = 1 ] || fail "the section was added twice"
grep -qx '# edited by hand' "$conf" || fail "an upgrade overwrote $conf"
ok "an upgrade adds nothing twice and keeps an edited $conf"

# Never removed.
pacman -R --noconfirm brokey-bin >/dev/null
grep -q '^\[spillebulle\]' /etc/pacman.conf || fail "removing Brokey removed the section"
[ -f "$conf" ] || fail "removing Brokey removed $conf"
ok "removing Brokey leaves the archive for the other applications"

# The proof that the key and the paths work: pacman reads the live archive's
# signed database with them.
pacman -Sy --noconfirm >/dev/null || fail "pacman does not accept the archive with the key and settings the package wrote"
ok "pacman verifies the archive's database"
