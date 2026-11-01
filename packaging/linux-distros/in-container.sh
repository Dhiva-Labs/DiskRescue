#!/usr/bin/env bash
# Runs inside a distribution container started by verify.sh.
# MODE: rpm (Fedora) | rpm-suse (openSUSE) | pacman (Arch) | appimage (Fedora)
# FILE: the package to install, under /pkgs.
set -euo pipefail
# shellcheck source=/dev/null
. /linux-distros/distro.conf

# Tools for the checks, the display and GL stack when there is a GUI smoke
# test, and the smoke test's own needs (SMOKE_PKGS_* in distro.conf), per
# distribution.
gui=0
if [ -n "${GUI_CMD:-}" ]; then gui=1; fi
case "$MODE" in
  rpm|appimage)
    pkgs=(file findutils which procps-ng "${SMOKE_PKGS_DNF[@]}")
    if [ "$gui" = 1 ]; then
      pkgs+=(xorg-x11-server-Xvfb xorg-x11-xauth mesa-dri-drivers dbus-daemon)
    fi
    dnf -y -q install "${pkgs[@]}"
    ;;
  rpm-suse)
    zypper -n -q --gpg-auto-import-keys refresh
    pkgs=(file findutils which procps "${SMOKE_PKGS_SUSE[@]}")
    if [ "$gui" = 1 ]; then pkgs+=(xvfb-run Mesa-dri dbus-1); fi
    zypper -n -q install "${pkgs[@]}"
    ;;
  pacman)
    pkgs=(file findutils which procps-ng "${SMOKE_PKGS_ARCH[@]}")
    if [ "$gui" = 1 ]; then pkgs+=(xorg-server-xvfb xorg-xauth mesa dbus); fi
    pacman -Syu --noconfirm --needed -q "${pkgs[@]}"
    ;;
esac

case "$MODE" in
  rpm)      dnf -y install "$FILE"; files() { rpm -ql "$NAME"; } ;;
  rpm-suse) zypper -n install --allow-unsigned-rpm "$FILE"; files() { rpm -ql "$NAME"; } ;;
  pacman)   pacman -U --noconfirm "$FILE"; files() { pacman -Qlq "$NAME"; } ;;
  appimage)
    # The libraries an AppImage expects from the host (a desktop's display
    # and GL stack), from distro.conf.
    dnf -y -q install "${APPIMAGE_HOST_PKGS_DNF[@]}"
    cp "$FILE" /tmp/app.AppImage
    chmod +x /tmp/app.AppImage
    (cd /tmp && /tmp/app.AppImage --appimage-extract > /dev/null)
    files() { find /tmp/squashfs-root -type f; }
    ;;
  *) echo "unknown MODE $MODE" >&2; exit 2 ;;
esac

# Every ELF file the package installed must find all of its libraries. The
# package's own library directories go on LD_LIBRARY_PATH, as the app's
# RPATH/$ORIGIN would supply them at run time, so only libraries missing
# from the distribution are reported.
mapfile -t elfs < <(files | while IFS= read -r f; do
  [ -f "$f" ] && [ ! -L "$f" ] || continue
  [ "$(head -c 4 "$f" | od -An -tx1 | tr -d ' \n')" = 7f454c46 ] && echo "$f"
done)
libdirs="$(for f in "${elfs[@]}"; do case "$f" in *.so|*.so.*) dirname "$f" ;; esac; done | sort -u | paste -sd:)"
missing=0
for f in "${elfs[@]}"; do
  if LD_LIBRARY_PATH="$libdirs" ldd "$f" 2>&1 | grep 'not found' > /dev/null; then
    echo "missing libraries for $f:"
    LD_LIBRARY_PATH="$libdirs" ldd "$f" | grep 'not found'
    missing=1
  fi
done
echo "${#elfs[@]} ELF files checked with ldd"
[ "$missing" = 0 ] || exit 1

# Smoke test. SMOKE_CMD must exit 0; it runs with APP set to the program
# under test (the installed command, or the AppImage). GUI apps: GUI_CMD must
# also still be running after SMOKE_SECONDS under Xvfb, with no loader errors.
if [ "$MODE" = appimage ]; then
  export APPIMAGE_EXTRACT_AND_RUN=1
  cmd_prefix=/tmp/app.AppImage
  APP=/tmp/app.AppImage
else
  cmd_prefix=
  APP="$(command -v "$CMD")"
fi
export APP
if [ -n "${SMOKE_CMD:-}" ]; then
  bash -euo pipefail -c "$SMOKE_CMD"
fi
if [ -n "${GUI_CMD:-}" ]; then
  if [ -n "$cmd_prefix" ]; then gui=("$cmd_prefix"); else read -r -a gui <<< "$GUI_CMD"; fi
  cat > /tmp/smoke.sh <<'EOS'
log=/tmp/smoke.log
"$@" > "$log" 2>&1 &
pid=$!
secs="${SMOKE_SECONDS:-20}"
for _ in $(seq 1 "$secs"); do
  kill -0 "$pid" 2> /dev/null || break
  sleep 1
done
if kill -0 "$pid" 2> /dev/null; then
  echo "still running after ${secs}s"
  kill "$pid"
  ok=1
else
  wait "$pid" && rc=0 || rc=$?
  echo "exited early with status $rc"
  ok=0
fi
cat "$log"
if grep -E 'error while loading shared libraries|cannot open shared object|undefined symbol' "$log"; then ok=0; fi
[ "$ok" = 1 ]
EOS
  export HOME=/tmp/smoke-home SMOKE_SECONDS
  mkdir -p "$HOME"
  xvfb-run -a -s '-screen 0 1280x800x24' dbus-run-session -- bash /tmp/smoke.sh "${gui[@]}"
fi
echo "$MODE: OK"
