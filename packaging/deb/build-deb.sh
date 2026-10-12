#!/bin/sh
# Build diskrescue_<version>_amd64.deb from already-built binaries.
#
#   packaging/deb/build-deb.sh VERSION CLI TUI GUI OUTDIR
#
# CLI and TUI may be static (musl) builds; GUI links against the system's
# glibc and GTK, so build it on the oldest Ubuntu the package should support.
set -eu

VERSION=$1; CLI=$2; TUI=$3; GUI=$4; OUT=$5
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
PKG="$STAGE/diskrescue"

install -Dm755 "$CLI" "$PKG/usr/bin/diskrescue"
install -Dm755 "$TUI" "$PKG/usr/bin/diskrescue-tui"
install -Dm755 "$GUI" "$PKG/usr/bin/diskrescue-gui"
install -Dm644 "$ROOT/packaging/linux/diskrescue.desktop" "$PKG/usr/share/applications/diskrescue.desktop"
install -Dm644 "$ROOT/packaging/linux/diskrescue.svg" "$PKG/usr/share/icons/hicolor/scalable/apps/diskrescue.svg"
install -Dm644 "$ROOT/README.md" "$PKG/usr/share/doc/diskrescue/README.md"
install -Dm644 "$ROOT/docs/USAGE.md" "$PKG/usr/share/doc/diskrescue/USAGE.md"
gzip -9n -c "$ROOT/CHANGELOG.md" > "$STAGE/CHANGELOG.md.gz"
install -Dm644 "$STAGE/CHANGELOG.md.gz" "$PKG/usr/share/doc/diskrescue/CHANGELOG.md.gz"
{
  echo "diskrescue ($VERSION) unstable; urgency=medium"
  echo
  echo "  * Release $VERSION. See CHANGELOG.md.gz for the changes."
  echo
  echo " -- Dhiva Labs <dhivakar1010@gmail.com>  $(date -R -u)"
} | gzip -9n > "$STAGE/changelog.gz"
install -Dm644 "$STAGE/changelog.gz" "$PKG/usr/share/doc/diskrescue/changelog.gz"
{
  echo "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/"
  echo "Upstream-Name: DiskRescue"
  echo "Source: https://github.com/Dhiva-Labs/DiskRescue"
  echo
  echo "Files: *"
  echo "Copyright: 2026 Dhiva-Labs"
  echo "License: MIT"
  sed 's/^$/./; s/^/ /' "$ROOT/LICENSE"
} > "$STAGE/copyright"
install -Dm644 "$STAGE/copyright" "$PKG/usr/share/doc/diskrescue/copyright"

# Library dependencies of the dynamically linked GUI (the static CLI and
# TUI need none). dpkg-shlibdeps wants a debian/control to exist.
DEPS="libc6"
if command -v dpkg-shlibdeps >/dev/null 2>&1; then
  mkdir -p "$STAGE/debian"
  printf 'Source: diskrescue\n\nPackage: diskrescue\nArchitecture: amd64\n' > "$STAGE/debian/control"
  DEPS=$(cd "$STAGE" && dpkg-shlibdeps -O "$PKG/usr/bin/diskrescue-gui" 2>/dev/null | sed -n 's/^shlibs:Depends=//p')
fi
SIZE=$(du -sk "$PKG" | cut -f1)

mkdir -p "$PKG/DEBIAN"
cat > "$PKG/DEBIAN/control" <<CONTROL
Package: diskrescue
Version: $VERSION
Architecture: amd64
Maintainer: Dhiva Labs <dhivakar1010@gmail.com>
Installed-Size: $SIZE
Depends: $DEPS, libxkbcommon0, libgl1 | libegl1
Recommends: pkexec | policykit-1, xdg-desktop-portal
Suggests: smartmontools
Section: utils
Priority: optional
Homepage: https://github.com/Dhiva-Labs/DiskRescue
Description: Recover photos and videos from failing drives and repair them
 DiskRescue gets photos and videos back from hard disks, SSDs, USB drives
 and SD cards that are failing, have failed, were reformatted or had files
 deleted. It copies failing drives carefully (resumable, ddrescue
 compatible), recovers files by name from FAT, exFAT, NTFS and ext4,
 carves photos and videos from raw space, and repairs damaged JPEG, PNG,
 MP4/MOV and AVI files, including videos whose index was lost when a
 camera or phone died while recording.
 .
 Three programs share one engine: diskrescue-gui (desktop app),
 diskrescue-tui (terminal app) and diskrescue (command line).
CONTROL

mkdir -p "$OUT"
dpkg-deb --root-owner-group -Zxz --build "$PKG" "$OUT/diskrescue_${VERSION}_amd64.deb" >/dev/null
echo "$OUT/diskrescue_${VERSION}_amd64.deb"
