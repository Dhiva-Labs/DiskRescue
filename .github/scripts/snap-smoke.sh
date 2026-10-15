#!/usr/bin/env bash
# Installs a built DiskRescue snap (classic, unsigned) on this machine and
# starts its programs:
#
#   .github/scripts/snap-smoke.sh path/to/diskrescue_<version>_<arch>.snap
#
# The command-line and terminal programs run on core24's loader. The desktop
# app is started twice: as the launcher normally does (the host's loader), and
# with `getconf` shadowed so the launcher believes the host's glibc is too old
# and falls back to core24's loader (snap/local/diskrescue-gui-launch). The
# libraries that were really loaded are read from LD_DEBUG output.
set -euo pipefail

snap_file=$1
arch=$(dpkg --print-architecture)
case "$arch" in
    amd64) triplet=x86_64-linux-gnu ;;
    arm64) triplet=aarch64-linux-gnu ;;
    *) echo "unsupported architecture $arch"; exit 1 ;;
esac

sudo apt-get update
# The libraries every desktop has, which the desktop app loads at run time.
sudo apt-get install -y xvfb libxcursor1 libxi6 libxrandr2 libxkbcommon-x11-0 libx11-xcb1 \
    libgl1 libegl1 libgl1-mesa-dri libvulkan1 mesa-vulkan-drivers
sudo snap install --dangerous --classic "$snap_file"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
for app in diskrescue diskrescue.tui; do
    out=$("$app" --version)
    echo "$app: $out"
    grep -q "$version" <<<"$out"
done

# Runs the desktop app for ten seconds under a virtual display. 124 is
# timeout's status for a program that was still running, i.e. the window opened.
run_gui() {
    local log=$1 status=0
    shift
    "$@" xvfb-run -a timeout 10 diskrescue.gui 2>"$log" || status=$?
    tail -n 20 "$log"
    test "$status" -eq 124 || { echo "diskrescue.gui exited early with status $status"; exit 1; }
}

echo "== desktop app, host loader =="
log=$(mktemp)
run_gui "$log" env LD_DEBUG=libs
if grep -q "trying file=/snap/core24/current/lib/$triplet/libc.so.6" "$log"; then
    echo "the app was started on core24's libc although the host's glibc is new enough"; exit 1
fi

echo "== desktop app, core24 loader =="
fake=$(mktemp -d)
printf '#!/bin/sh\necho "GNU_LIBC_VERSION glibc 2.35"\n' > "$fake/getconf"
chmod +x "$fake/getconf"
log=$(mktemp)
run_gui "$log" env LD_DEBUG=libs "PATH=$fake:$PATH"
grep -q "trying file=/snap/core24/current/lib/$triplet/libc.so.6" "$log" \
    || { echo "the app did not load core24's libc through core24's loader"; exit 1; }
echo "both loaders start the desktop app"
