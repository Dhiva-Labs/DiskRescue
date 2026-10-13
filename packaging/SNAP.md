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
   administrator copy.
3. `snapcraft upload --release=stable diskrescue_0.1.0_amd64.snap`.

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
