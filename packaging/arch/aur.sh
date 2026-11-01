#!/usr/bin/env bash
# Renders the AUR PKGBUILD for one release and proves it builds.
#
# usage: aur.sh <amd64.deb> <arm64.deb> <outdir>
#
# Fills PKGBUILD.in with the version, the release download URL and the
# sha256 of both .debs, then, in an archlinux:latest container, generates
# .SRCINFO, runs makepkg on the x86_64 source (the .deb is placed next to the
# PKGBUILD, so makepkg verifies the checksum without downloading), installs
# the result and runs it. Writes <name>-bin-<version>-aur.tar.gz holding
# PKGBUILD and .SRCINFO, ready to push to the AUR.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
[ $# -eq 3 ] || { echo "usage: $0 <amd64.deb> <arm64.deb> <outdir>" >&2; exit 2; }
deb_amd64="$(readlink -f "$1")"; deb_arm64="$(readlink -f "$2")"
mkdir -p "$3"; out="$(cd "$3" && pwd)"
# shellcheck source=/dev/null
. "$here/../linux-distros/distro.conf"
: "${VERSION:?set VERSION}"
: "${GITHUB_REPO:?distro.conf must set GITHUB_REPO}"
tag="${TAG_PREFIX-v}$VERSION"

work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
deps=""; for d in "${ARCH_DEPENDS[@]}"; do deps+="'$d' "; done
optdeps=""; for d in "${ARCH_OPTDEPENDS[@]}"; do optdeps+="'$d' "; done
sed -e "s|@NAME@|$NAME|g" -e "s|@VERSION@|$VERSION|g" \
    -e "s|@SUMMARY@|${SUMMARY//\'/}|g" -e "s|@HOMEPAGE@|$HOMEPAGE|g" \
    -e "s|@LICENSE@|$LICENSE|g" -e "s|@ARCH_DEPENDS@|${deps% }|g" \
    -e "s|@ARCH_OPTDEPENDS@|${optdeps% }|g" \
    -e "s|@RELEASE_URL@|https://github.com/$GITHUB_REPO/releases/download/$tag|g" \
    -e "s|@DEB_AMD64@|$(basename "$deb_amd64")|g" -e "s|@DEB_ARM64@|$(basename "$deb_arm64")|g" \
    -e "s|@SHA_AMD64@|$(sha256sum "$deb_amd64" | cut -d' ' -f1)|g" \
    -e "s|@SHA_ARM64@|$(sha256sum "$deb_arm64" | cut -d' ' -f1)|g" \
    "$here/PKGBUILD.in" > "$work/PKGBUILD"
cp "$deb_amd64" "$work/"
if grep -n '@[A-Z0-9_]*@' "$work/PKGBUILD"; then echo "unfilled placeholder" >&2; exit 1; fi

# makepkg refuses to run as root. The build user gets the runner's uid, so
# everything it writes into the bind mount stays removable by the trap.
docker run --rm -v "$work:/build" -e HOST_UID="$(id -u)" -e NAME="$NAME" \
  archlinux:latest bash -euo pipefail -c '
  pacman -Syu --noconfirm --needed -q base-devel namcap
  useradd -m -u "$HOST_UID" builder
  chown -R builder /build
  cd /build
  su builder -c "makepkg --printsrcinfo > .SRCINFO"
  su builder -c "makepkg --noconfirm --nodeps -f"
  namcap PKGBUILD
  pacman -U --noconfirm ./*-x86_64.pkg.tar.zst
  pacman -Qlq "$NAME-bin"
  "$NAME" --version
  cat .SRCINFO
'
tar -C "$work" -czf "$out/$NAME-bin-$VERSION-aur.tar.gz" PKGBUILD .SRCINFO
ls -l "$out/$NAME-bin-$VERSION-aur.tar.gz"
