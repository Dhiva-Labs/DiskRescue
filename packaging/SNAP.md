# Snap Store

The name `diskrescue` is registered (2026-10-13). Nothing has been uploaded:
the snap needs classic confinement, and an upload with an unapproved
classic confinement blocks later releases until it is reviewed. Order:

1. Post the request below in the Snapcraft forum, category "store-requests",
   titled "Classic confinement request for diskrescue".
2. When it is approved, build from a clean export of the committed tree,
   never from the working tree: `snapcraft pack` copies the whole directory,
   `target/` included (tens of GB), into the build container. Keep the
   export under your home directory, where the LXD snap can see it.

   ```bash
   rm -rf ../diskrescue-snap && mkdir ../diskrescue-snap
   git archive HEAD | tar -x -C ../diskrescue-snap
   cd ../diskrescue-snap && snapcraft pack --use-lxd
   ```

   Test the snap in the `snaptest` LXD VM (`snap install --classic --dangerous`), running all three
   apps: `diskrescue list`, `diskrescue.tui` (terminal), `diskrescue.gui`
   (under xvfb-run, screenshot), and an image + recover of a file-backed
   source. Check that `diskrescue.gui` finds `$SNAP/bin/diskrescue` for the
   administrator copy. Test traps:
   - The VM is a server image: install the X and GL libraries every desktop
     has (`libxcursor1 libxi6 libxrandr2 libxkbcommon-x11-0 libx11-xcb1 libgl1
     libegl1 libgl1-mesa-dri libvulkan1 mesa-vulkan-drivers`), or the window
     fails with "libXcursor.so.1: cannot open shared object file".
   - Run the TUI as `script -qc "stty cols 120 rows 40; timeout --foreground 4
     diskrescue.tui" /dev/null`: without `stty` the pseudo-terminal is 0x0 and
     nothing is drawn; without `--foreground`, `timeout` puts it in a
     background process group and the first terminal mode change stops it
     (state `T`), so it never sees SIGTERM.
   - Start the desktop app both ways: `diskrescue.gui`, and the fallback
     `/snap/core24/current/lib64/ld-linux-x86-64.so.2 --library-path
     /snap/core24/current/lib/x86_64-linux-gnu $SNAP/bin/diskrescue-gui`.
3. `snapcraft upload --release=stable diskrescue_0.1.0_amd64.snap`.

Verified 2026-10-13 with that procedure (snaptest, Ubuntu 24.04): install,
list, image (byte-identical), recover of an existing and a deleted JPEG
(byte-identical), the output-on-source guard (exit 3), the TUI, and the
desktop app through both loaders.

## Architectures

`snap/snapcraft.yaml` lists `amd64` and `arm64` under `platforms`; each is
built on a machine of its own architecture. CI builds the arm64 snap on an
`ubuntu-24.04-arm` runner on every push, installs it there and starts all
three programs, the desktop app through both loaders (the check is
`.github/scripts/snap-smoke.sh`), and keeps the file as the
`diskrescue-snap-arm64` workflow artifact. It never uploads to the store.
On arm64 the fallback loader is
`/snap/core24/current/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1 --library-path
/snap/core24/current/lib/aarch64-linux-gnu`.

## How the snap is built

- `diskrescue` and `diskrescue-tui` are patched to core24's loader and
  libraries (`enable-patchelf`), so they behave the same on every host.
- `diskrescue-gui` stays on the host's loader. It loads the host's Vulkan and
  OpenGL drivers (and X11/Wayland libraries) at run time, and those are built
  against the host's glibc: Mesa on Ubuntu 26.04 needs glibc 2.43, newer than
  core24's 2.39, so under core24's loader the window could not open. Staging
  GL libraries does not help: `dlopen` from a patched binary never sees them
  (the linter reported all of them unused).
- `bin/diskrescue-gui-launch` (from `snap/local`) starts it on the host's
  loader when the host has glibc 2.39 or newer (what the binary needs), and on
  core24's loader with core24's libraries otherwise (Ubuntu 22.04: its drivers
  are older than core24's glibc, so they still load). `--library-path` is not
  inherited by the programs the app starts.
- Through core24's loader `current_exe()` names the loader, so the desktop app
  looks for its helper in `$SNAP/bin` first.

## Classic confinement request

> **Snap:** diskrescue (https://github.com/Dhiva-Labs/DiskRescue, MIT)
>
> **What it is:** a data recovery tool for photos and videos. It makes a
> careful, resumable copy of a failing hard disk, SSD, USB drive or SD card
> (like GNU ddrescue), recovers existing and deleted files from FAT, exFAT,
> NTFS and ext4, carves photos and videos from raw space, and repairs damaged
> JPEG, PNG, MP4/MOV and AVI files. It ships a command line, a terminal app
> and a desktop app.
>
> **Why classic:** the tool falls into the "tools that need to access
> arbitrary block devices and run privileged operations on them" category:
>
> - It must read any raw block device the user picks (`/dev/sd*`,
>   `/dev/mmcblk*`, `/dev/nvme*n*`, USB card readers, loop devices over disk
>   images), including devices that re-enumerate under a new name when a
>   failing USB drive drops off the bus; it finds them again by serial number
>   through sysfs and udev data.
> - The desktop app runs as the user and asks for administrator rights only
>   for the copy step, by running its own command-line program through
>   `pkexec`. A strictly confined snap cannot do this; with only the
>   `block-devices` interface users would have to run the whole desktop app as
>   root, which does not work on Wayland.
> - Recovered files and disk images (often hundreds of GB) must be written to
>   wherever the user has space: any mounted disk, `/mnt`, `/srv`, NAS mounts,
>   not only `$HOME` and `/media`. The tool refuses to write to the device
>   being recovered, which requires reading `/proc/self/mountinfo`, sysfs
>   holders/slaves and `/run/udev/data`.
> - It reads device health with `smartctl` when the host has it installed.
>
> It never writes to the source device: every device is opened read-only.
> Comparable tools (GNU ddrescue, TestDisk/PhotoRec, GParted) are distributed
> unconfined for the same reasons.
