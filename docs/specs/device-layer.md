# Device layer: enumerating, classifying and safely opening block devices

Status: research spec, written 2026-10-12. Lines tagged `UNVERIFIED` were not
confirmed against a primary source and must be checked before the code that
depends on them is merged. Everything else was read from the cited source or
observed directly on the dev box (Linux 7.0.0-38-generic, util-linux 2.41.3,
systemd-udev on `/run/udev/data`).

Cross-references: `imaging-engine.md` (consumes `OpenedDevice` and
`DeviceProfile` from this layer), `partition-tables.md` (consumes the
`start`/`size` partition list as a hint only; it re-parses the MBR/GPT itself).

---

## 1. Overview and scope

The device layer is the only part of DiskRescue that touches a physical
source. Its contract:

1. **Enumerate** every candidate block device on the machine and attach a
   `DeviceInfo` to each: path, size, sector sizes, transport, identity strings
   (model/vendor/serial/wwid), removable/ro flags, partition list, mount state,
   and a `Class`.
2. **Classify** the device into one of
   `{Hdd, SsdSata, SsdNvme, UsbFlash, UsbHdd, UsbSsd, SdMmc, Optical, Virtual, Unknown}`
   with a confidence and the list of heuristics that fired, so the imaging
   engine can pick a strategy (seek-penalty aware vs flash-aware vs card) and
   the UI can show "why".
3. **Open read-only** with the strongest cache-bypassing mode the kernel
   allows, never requesting write access, never issuing any ioctl that changes
   kernel or device state, and verify alignment requirements for `pread`.
4. **Guard the output**: refuse (not warn) when the output directory or the
   mapfile would be written to a filesystem that lives on the source device
   (directly, through a partition, or through dm/md/LVM stacked on it), and
   warn loudly when any part of the source is mounted.
5. **Track liveness**: detect that the device vanished (USB reset, SATA link
   drop, NVMe controller reset) and re-find it by stable identity after it
   re-enumerates under a different name.
6. Optionally read **SMART** through `smartctl -j` if the binary exists.

Platforms: Linux is first class and is specified exhaustively. Windows is
best-effort (enumeration + property queries + unbuffered reads). macOS gets
one section of pointers only.

Out of scope here: the read loop itself, retries, mapfile (imaging spec),
SG_IO/ATA pass-through imaging modes, and SMART attribute interpretation beyond
the handful of fields listed in §2.11.

Hard rules (the whole tool depends on these):

- Open with `O_RDONLY`. Never `O_RDWR`, never `BLKROSET`, never `BLKRRPART`,
  never `BLKFLSBUF` on the source. Those ioctls need `CAP_SYS_ADMIN` anyway
  (block/ioctl.c) and `BLKFLSBUF` is pointless with `O_DIRECT`.
- Never `mount` the source. Never run `fsck`. Never write the mapfile or any
  temp file onto the source.
- Never `fork()` while an `O_DIRECT` read is in flight on a private mapping
  (open(2) NOTES); the tool spawns helpers (`smartctl`) only before opening
  the source or from a thread that has no outstanding direct reads.

---

## 2. Exact on-disk / wire details

### 2.1 Linux: `/sys/block/<dev>/` attributes

`/sys/block` lists whole disks only (partitions are subdirectories).
`/sys/class/block` lists disks and partitions flat. Every entry is a symlink
whose `realpath` is the device's position in the device tree
(`/sys/devices/...`); that realpath is what §3.2 pattern-matches.

All attribute files are ASCII text terminated by `\n`. Read them with a
bounded read (4096 bytes is enough), trim trailing whitespace, parse with
`str::parse::<u64>()`. Missing file is a normal condition (older kernels,
virtual devices); treat as `None`, never as an error.

| Path (relative to `/sys/block/<dev>/`) | Format | Meaning / source |
|---|---|---|
| `size` | decimal u64 | capacity in **512-byte units regardless of logical block size** (observed: `2000409264` for a 1 TB NVMe with 512 B LBAs; `blockdev --getsz` prints the same). Bytes = `size * 512`. |
| `removable` | `0`/`1` | `GENHD_FL_REMOVABLE` (genhd.c `disk_removable_show`). For SCSI/USB it is the INQUIRY RMB bit (sd.c). |
| `ro` | `0`/`1` | `get_disk_ro()`; for sd it is MODE SENSE device-specific byte bit 7 (WP). SD cards with lock switch → 1. |
| `hidden` | `0`/`1` | `GENHD_FL_HIDDEN`: "can't be opened from userspace". Skip when 1. |
| `dev` | `MAJ:MIN` decimal | the `dev_t` of the whole disk, e.g. `259:0`, `8:16`. |
| `diskseq` | decimal u64 | monotonically increasing per-attach sequence number (kernel ≥ 5.15). Changes after every re-plug: use it to detect "same name, different attach". |
| `partscan` | `0`/`1` | partition scanning enabled (≥ 6.10). |
| `capability` | hex | raw `GENHD_FL_*` bits; bit meanings changed across kernels, prefer the dedicated files. |
| `events` | words | `media_change`, `eject_request` supported events (sr, removable sd). |
| `events_poll_msecs` | decimal / `-1` | media polling interval; `-1` = default/disabled. |
| `stat` | 17 decimal fields | reads completed, merged, sectors, ms, writes…, in-flight, io ms, weighted ms, discards…, flush… |
| `inflight` | `R W` | two decimals. |
| `alignment_offset` | decimal bytes | how far LBA 0 is from the physical-sector grid (non-zero on 4Kn-emulating drives with misaligned MBR). |
| `discard_alignment` | decimal bytes | same for the discard unit. |
| `wwid` | string | NVMe namespaces only (`eui.…`, `nguid.…`, `uuid.…`). SCSI has it under `device/wwid`. |
| `nsid`, `eui`, `nguid`, `uuid`, `csi`, `nuse`, `metadata_bytes` | various | NVMe namespace attributes (nvme core sysfs). |
| `queue/rotational` | `0`/`1` | 1 = rotational. **Default is 1** when the driver cannot tell (sd.c returns early when VPD B1 absent). RW (admins can override). |
| `queue/logical_block_size` | bytes | smallest addressable unit; the unit `pread` offsets must be aligned to under `O_DIRECT`. |
| `queue/physical_block_size` | bytes | smallest atomic write unit (4096 on Advanced Format). |
| `queue/hw_sector_size` | bytes | legacy, equals logical_block_size. |
| `queue/minimum_io_size` | bytes | smallest request without penalty (== physical block size for disks, stripe for RAID). |
| `queue/optimal_io_size` | bytes | 0 when not reported (observed on NVMe). |
| `queue/max_sectors_kb` | KiB | max request the block layer will build for the filesystem; larger `pread`s are split by the kernel, no error. |
| `queue/max_hw_sectors_kb` | KiB | hardware max. |
| `queue/dma_alignment` | mask | user buffers for direct I/O must satisfy `addr & mask == 0` (observed `3` on NVMe, `511` typical for SCSI). Page-aligned buffers satisfy every known value. |
| `queue/virt_boundary_mask` | mask | segment boundary; irrelevant for page-aligned single buffers. |
| `queue/discard_granularity` | bytes | 0 when discard unsupported. |
| `queue/zoned` | `none`/`host-aware`/`host-managed` | SMR zone model. Reading is unaffected; record it. |
| `device` | symlink | the parent *physical* device (scsi_device, nvme controller, mmc card, virtio, loop has none). |
| `holders/` | dir of symlinks | devices stacked *on top of* this one (dm-0, md0). |
| `slaves/` | dir of symlinks | devices this one is built *from* (for dm/md). |
| `<dev>N` / `<dev>pN` | dirs | partitions (§2.2). |
| `loop/backing_file`, `loop/offset`, `loop/sizelimit` | | loop devices only. |
| `dm/name`, `dm/uuid` | | device-mapper only. |
| `md/level`, `md/array_state` | | md only. |

Kernel ABI reference for the above: `Documentation/ABI/stable/sysfs-block`.

### 2.2 Partitions

`/sys/block/<disk>/<part>/` exists for each partition the kernel scanned.
Naming: `sda1`, `nvme0n1p1`, `mmcblk0p1`, `loop0p1`, `vda1`.

| File | Meaning |
|---|---|
| `partition` | partition number (1-based) — **presence of this file is the authoritative "this is a partition" test** |
| `start` | first sector, **512-byte units** (observed `2048`, `534528`) |
| `size` | length in 512-byte units |
| `dev` | `MAJ:MIN` of the partition node |
| `alignment_offset`, `discard_alignment`, `stat`, `ro`, `uevent` | as for disks |

A partition's parent disk is the parent directory of
`realpath(/sys/class/block/<part>)`. `/sys/dev/block/<MAJ>:<MIN>` is a symlink
to the same realpath and is the way to go from a `st_dev`/`st_rdev` to sysfs.

Partition data from sysfs is a *hint*: on a failing drive the kernel may have
failed to read the table at all (no `<dev>N` dirs), or read a stale copy.
`partition-tables.md` re-parses from the image.

### 2.3 The `device/` chain per transport

`/sys/block/<dev>/device` points at the *first* physical parent. Walk upward
(`..`) along the realpath to reach bus-level devices. Each directory's
`subsystem` symlink names its bus (`scsi`, `usb`, `nvme`, `mmc`, `virtio`,
`pci`, `ata_port`...), and its `uevent` file has `DEVTYPE=` lines
(`usb_device`, `usb_interface`, `scsi_device`, `scsi_target`, `scsi_host`).

**SCSI device (sd*, sr*: SATA via libata, USB mass storage, SAS, iSCSI, virtio-scsi)**
`/sys/block/sdX/device` → `/sys/devices/.../host<H>/target<H>:<C>:<T>/<H>:<C>:<T>:<L>/`

| Attribute | Format | Notes |
|---|---|---|
| `vendor` | `%.8s` | 8-char INQUIRY vendor, space padded. libata reports `ATA     `. USB bridges often report the bridge maker (`JMicron `, `ASMT    `, `Generic `) or the drive maker. |
| `model` | `%.16s` | 16-char INQUIRY product. libata puts the first 16 chars of the ATA model string here. |
| `rev` | `%.4s` | firmware revision |
| `type` | `%d` | SCSI peripheral device type: `0` = direct-access (disk), `5` = CD/DVD, `7` = optical memory, `14` = simplified direct access (RBC), `1` = tape, `3` = processor |
| `scsi_level` | `%d` | ANSI level + 1 (e.g. `7` = SPC-4) |
| `state` | string | `running`, `offline`, `transport-offline`, `blocked`, `cancel`, `deleted`, `quiesce`, `created`, `created-blocked`. Anything but `running` → device is not usable right now. |
| `wwid` | string | from `scsi_vpd_lun_id()` (VPD 0x83): `naa.5000c500…`, `t10.ATA     MODEL   SERIAL`, `eui.…`. Only present when VPD 0x83 exists. |
| `vpd_pg80` | binary | raw Unit Serial Number VPD page (§2.7). libata synthesises it from IDENTIFY words 10–19, so it exists for SATA disks too. |
| `vpd_pg83` | binary | raw Device Identification VPD page |
| `vpd_pgb1` | binary | raw Block Device Characteristics page (§2.7) — present iff the device answered it; absent on most BOT bridges |
| `inquiry` | binary | raw standard INQUIRY response (byte 0 bits 4:0 = type, byte 1 bit 7 = RMB) |
| `queue_depth`, `timeout`, `eh_timeout` | decimal | `timeout` is the per-command timeout in seconds (default 30) — the imaging engine reads it to size its own timeouts |
| `ioerr_cnt`, `iodone_cnt`, `iorequest_cnt` | `0x%llx` | error counters; rising `ioerr_cnt` during imaging = media or link errors |
| `delete`, `rescan` | write-only | **never write** |

No serial attribute exists on scsi_device. Serial comes from (in order):
`vpd_pg80`, `wwid`, the USB parent's `serial`, or the udev DB `ID_SERIAL_SHORT`.

**NVMe namespace (nvme<C>n<N>)**
`/sys/block/nvme0n1/device` → the controller `/sys/devices/pci…/nvme/nvme0/`
(class `/sys/class/nvme/nvme0`). Observed attribute set: `address cntlid
cntrltype dctype dev firmware_rev kato model numa_node queue_count quirks
rescan_controller reset_controller serial sqsize state subsysnqn subsystem
transport uevent`.

| Attribute | Observed value | Notes |
|---|---|---|
| `model` | `KXG60ZNV1T02 KIOXIA                     ` | 40 chars, **space padded** — trim |
| `serial` | `307F71PHFGQL        ` | 20 chars, space padded — trim |
| `firmware_rev` | `AGHA4101` | 8 chars |
| `transport` | `pcie` | also `rdma`, `tcp`, `fc`, `loop`, `apple` |
| `state` | `live` | also `new`, `resetting`, `connecting`, `deleting`, `deleting (no IO)`, `dead` (sysfs.c state table) |
| `subsysnqn` | `nqn.2017-03.jp.co.toshiba:…` | |
| `cntrltype` | `io` | `io`/`discovery`/`admin` |
| `reset_controller`, `rescan_controller` | write-only | **never write** |

With `CONFIG_NVME_MULTIPATH` (default on distro kernels) the realpath of
`/sys/block/nvme0n1` can instead be
`/sys/devices/virtual/nvme-subsystem/nvme-subsys0/nvme0n1`, and `device` then
points at `nvme-subsys0` which carries `model`, `serial`, `subsysnqn` but **no
`transport`**; the controllers are `nvme-subsys0/nvme0`, `nvme1`… — read
`transport`/`state` from any child matching `nvme[0-9]*` (each is a symlink
to the controller: `nvme_init_subsystem()` does
`sysfs_create_link(&subsys->dev.kobj, &ctrl->device->kobj, dev_name(ctrl->device))`;
the subsystem device has no parent and class `nvme-subsystem`, hence the
`/sys/devices/virtual/nvme-subsystem/` realpath; `nvme_subsys_attrs` =
`model serial firmware_rev subsysnqn subsystype [iopolicy]`). The head is
created only when the subsystem's CMIC bit 1 (multi-controller) is set and
`nvme_core.multipath=Y` (or `multipath_always_on`, ≥ 6.16), so consumer
PCIe drives keep the plain form even on multipath kernels (observed on the
dev box: `nvme_core/parameters/multipath = Y`, yet
`realpath /sys/block/nvme0n1` = `/sys/devices/pci…/nvme/nvme0/nvme0n1`, while
`/sys/class/nvme-subsystem/nvme-subsys0/nvme0` → the controller exists).
Per-path disks `nvme<C>c<X>n<N>` under the controller carry
`GENHD_FL_HIDDEN` (`hidden=1`) and **do** appear in `/sys/block` — the
`hidden` skip in §3.1 drops them. Verified against
drivers/nvme/host/{core.c,multipath.c,sysfs.c}.

**MMC/SD card (mmcblk<N>)**
`/sys/block/mmcblk0/device` → `/sys/devices/.../mmc_host/mmc0/mmc0:<rca>/`.
Attributes and printf formats (drivers/mmc/core/sd.c, mmc.c):

| Attribute | Format | Source |
|---|---|---|
| `type` | `MMC`, `SD`, `SDIO`, `SDcombo` | bus.c `type_show`. **SDHC/SDXC are not distinguished here** (only in the dmesg line). |
| `name` | `%s` | CID product name, 5 ASCII chars for SD (bits 103:64), 6 for MMC |
| `serial` | `0x%08x` | CID PSN bits 55:24 |
| `manfid` | `0x%06x` | CID MID bits 127:120 (e.g. `0x000003` SanDisk, `0x00001b` Samsung, `0x000027` Phison, `0x000074` Transcend; many clones use `0x000000`/`0x00009f`) |
| `oemid` | `0x%04x` | CID OID bits 119:104 |
| `hwrev`, `fwrev` | `0x%x` | CID bits 63:60 / 59:56 |
| `date` | `%02d/%04d` | month/year, year = CID bits 19:12 + 2000 |
| `cid` | `%08x%08x%08x%08x` | raw 128-bit CID |
| `csd` | `%08x%08x%08x%08x` | raw 128-bit CSD; **bits 127:126 (`CSD_STRUCTURE`)**: `0` = SDSC (≤ 2 GB), `1` = SDHC/SDXC (block addressed), `2` = SDUC. Within version 1, capacity > 32 GiB means SDXC by the SD spec's definition. |
| `scr` | `%08x%08x` | SD only |
| `ssr` | 16×`%08x` | SD status, SD only |
| `ocr` | `0x%08x` | |
| `rca` | `0x%04x` | |
| `erase_size`, `preferred_erase_size` | bytes | |
| `life_time`, `pre_eol_info` | `0x%02x 0x%02x` / `0x%02x` | eMMC 5.0+ only; `pre_eol_info` `0x02` = warning (80 % reserved consumed), `0x03` = urgent |

Sibling gendisks `mmcblk0boot0`, `mmcblk0boot1`, `mmcblk0rpmb` are eMMC
hardware partitions, not partitions of `mmcblk0`; enumerate them as separate
`Virtual`-like entries flagged `emmc_hw_partition`, never as candidates by
default.

**USB (walk up from the SCSI device)**
`/sys/devices/pci…/usb1/1-3/1-3:1.0/host6/target6:0:0/6:0:0:0/block/sdb`
(typical). Walk `..` from the scsi_device until a directory whose `uevent`
contains `DEVTYPE=usb_device` (equivalently: it has an `idVendor` file).
The directory *before* it (`1-3:1.0`) is the `usb_interface`.

usb_device attributes (drivers/usb/core/sysfs.c):

| Attribute | Format | Notes |
|---|---|---|
| `idVendor` | `%04x` | e.g. `0781` SanDisk, `0951` Kingston, `152d` JMicron, `174c` ASMedia, `0bc2` Seagate, `1058` WD, `04e8` Samsung, `05e3` Genesys (card readers, hubs), `058f` Alcor (card readers), `0bda` Realtek (card readers) |
| `idProduct` | `%04x` | |
| `bcdDevice` | `%04x` | |
| `serial` | string | **optional**; absent for many cheap sticks and most card readers; some vendors ship one constant for the whole product line (observed `000000000`, `SN0001` on this box) |
| `manufacturer`, `product` | string | optional |
| `removable` | `removable` / `fixed` / `unknown` | the *port's* connect-type from ACPI/hub descriptor, not the medium |
| `speed` | Mbit/s | `1.5`, `12`, `480`, `5000`, `10000`, `20000` |
| `busnum`, `devnum`, `devpath` | decimal / `1.3` | `devnum` changes on every re-enumeration; `devpath` (physical port path) does not |

usb_interface attributes: `bInterfaceClass` (`08` = mass storage),
`bInterfaceSubClass` (`06` = SCSI transparent, `02` ATAPI, `04` UFI floppy,
`05` SFF-8070i), `bInterfaceProtocol` (`50` = Bulk-Only Transport,
`62` = UAS), and the `driver` symlink whose basename is `usb-storage` or
`uas`.

**virtio (vd*)**: realpath contains `/virtio<N>/block/vd<X>`; `device/`
has no model; udev uses `ATTRS{serial}` when the hypervisor sets one.

**Virtual devices**: realpath starts with `/sys/devices/virtual/block/`
for `loop*`, `dm-*`, `md*`, `zram*`, `nbd*`, `ram*`. NVMe multipath heads
live under `/sys/devices/virtual/nvme-subsystem/` (not `.../virtual/block/`)
and are real hardware — see §2.4 ordering. `/sys/block/loop0/loop/backing_file`
is absent (dir missing) for unbound loops.

### 2.4 Transport detection from the realpath

`realpath("/sys/block/<dev>")` → match substrings in this order, first hit
wins:

| Substring | Transport | Note |
|---|---|---|
| `/nvme-subsystem/` | `Nvme` | **must precede the `/devices/virtual/` test**: multipath heads resolve to `/sys/devices/virtual/nvme-subsystem/nvme-subsysN/nvmeNnM` and would otherwise be misclassified as `Virtual` |
| `/devices/virtual/block/` | `Virtual` | then refine by name prefix (`loop`, `dm-`, `md`, `zram`, `nbd`, `ram`) |
| `/usb` followed by digit, i.e. regex `/usb[0-9]+/` | `Usb` | present for anything behind a USB host; check *before* `/ata` because a USB disk is still a SCSI device |
| `/ata[0-9]+/` | `Ata` (libata: SATA/PATA) | |
| `/nvme/` | `Nvme` | |
| `/mmc_host/` | `Mmc` | |
| `/virtio[0-9]+/` | `Virtio` | |
| `/ccw/` | `Ccw` (s390 DASD) | treat as `Unknown` |
| `/host[0-9]+/` only (none of the above) | `Scsi` | SAS, iSCSI, FC, RAID controllers, virtio-scsi, Hyper-V storvsc (`/vmbus/`) |
| anything else | `Unknown` | |

Then: name prefix `sr` **or** `device/type == 5` → `Optical` (regardless of
transport; a USB DVD drive is `Usb`+`Optical`). `lsblk` derives its `TRAN`
column the same way (`usb`, `sata`, `ata`, `nvme`, `mmc`, `virtio`, `sas`,
`iscsi`, `fc`, `spi`).

### 2.5 udev database `/run/udev/data/`

File name: `b<MAJ>:<MIN>` for block devices (`c…` char, `+<subsystem>:<sysname>`
others), e.g. `/run/udev/data/b259:0`. Text, one record per line:

| Prefix | Meaning | Example |
|---|---|---|
| `S:` | symlink path relative to `/dev/` | `S:disk/by-id/nvme-eui.0000…` |
| `L:` | devlink priority | `L:-100` |
| `E:` | property `KEY=VALUE` | `E:ID_SERIAL_SHORT=307F71PHFGQL` |
| `G:` | tag (all) | `G:systemd` |
| `Q:` | current tag | `Q:systemd` |
| `I:` | `USEC_INITIALIZED` (µs since boot) | `I:3000006` |
| `V:` | db format version | `V:1` |
| `W:` | deprecated (was the inotify watch handle); ignore (`sd-device.c` `handle_db_line`: `case 'W': /* Deprecated. */ return 0;`) | |

Source: systemd `src/libsystemd/sd-device/device-private.c`. The file only
exists after udev has processed the device; on a system without systemd-udev
(or inside a container) it is absent — fall back to sysfs. Reading it needs
no privileges (`/run/udev/data` is world-readable). Properties that matter:

| Key | Set by | Meaning |
|---|---|---|
| `ID_BUS` | `ata_id` → `ata`; `usb_id` → `usb` **only if `ID_BUS` is still unset**; `scsi_id` → `scsi`; `nvme` rules → (absent, use `ID_NSID`) | transport as udev saw it. On a non-removable USB disk whose bridge supports SAT, `ata_id` runs first (rule below) and wins: `ID_BUS=ata`, `ID_SERIAL=MODEL_SERIAL`, and the by-id link is `ata-…`; `usb_id` (always imported for USB, `IMPORT{builtin}="usb_id"`) then adds only the `ID_USB_*` keys (`udev-builtin-usb_id.c`: legacy `ID_BUS/ID_MODEL/ID_SERIAL/ID_VENDOR/ID_REVISION/ID_TYPE/ID_INSTANCE` are added only when `ID_BUS` is absent). So **do not** require `ID_BUS=usb` to recognise a USB disk; use sysfs transport. |
| `ID_SERIAL` | `ata_id`: `MODEL_SERIAL` (or just `MODEL` if no serial); `usb_id`: `VENDOR_MODEL_SERIAL-INSTANCE` where INSTANCE is `target:lun`, e.g. `SanDisk_Cruzer_4C530001…-0:0`; nvme rules: `MODEL_SERIAL` (+`_NSID` variant); mmc rules: `$attr{serial}` | the string used in `by-id` names |
| `ID_SERIAL_SHORT` | the bare serial | |
| `ID_MODEL`, `ID_VENDOR`, `ID_REVISION` | | |
| `ID_MODEL_ENC`, `ID_VENDOR_ENC` | `\x20`-escaped raw strings | preserve for display |
| `ID_USB_DRIVER` | `usb_id` from the interface's `driver` link | `usb-storage` or `uas` |
| `ID_USB_VENDOR_ID`, `ID_USB_MODEL_ID` | hex | from `idVendor`/`idProduct` |
| `ID_USB_SERIAL`, `ID_USB_SERIAL_SHORT` | | USB string descriptor serial; `usb_id` drops it if it contains bytes < 0x20, > 0x7f or `,` |
| `ID_USB_TYPE` / `ID_TYPE` | `disk`, `cd`, `floppy`, `generic` | from SCSI type |
| `ID_ATA_ROTATION_RATE_RPM` | `ata_id` from IDENTIFY word 217 | present only when the drive reports it: word `0x0001` → `=0` (non-rotating), `0x0401`–`0xFFFE` → `=<rpm>`, any other value → key omitted (ata_id.c: `if (word == 0x0001) printf("ID_ATA_ROTATION_RATE_RPM=0\n"); else if (word >= 0x0401 && word <= 0xfffe) printf("…=%d\n", word);`). ata_id obtains IDENTIFY via SG_IO ATA PASS-THROUGH (12-byte `0xa1` for ATA, 16-byte `0x85` for ATAPI) and falls back to `HDIO_GET_IDENTITY`. |
| `ID_ATA_SATA`, `ID_ATA_SATA_SIGNAL_RATE_GEN1/2` | `1` | |
| `ID_WWN`, `ID_WWN_WITH_EXTENSION` | `0x5000c500…` / `eui.…` for nvme | |
| `ID_PATH`, `ID_PATH_TAG` | `pci-0000:21:00.0-nvme-1`, `pci-0000:00:14.0-usb-0:3:1.0-scsi-0:0:0:0` | physical path; stable across re-plug *into the same port* |
| `ID_PART_TABLE_TYPE` | `gpt`/`dos` | from blkid (udev already read sector 0 and 1) |
| `ID_FS_TYPE`, `ID_FS_UUID`, `ID_FS_LABEL` | on partitions | |

Important for USB: rule
`KERNEL=="sd*[!0-9]|sr*", ENV{ID_SERIAL}!="?*", ATTR{removable}=="0", SUBSYSTEMS=="usb", IMPORT{program}="ata_id --export $devnode"`
(verified verbatim in the local systemd 259 `60-persistent-storage.rules`,
followed by the unconditional `KERNEL=="sd*[!0-9]|sr*", SUBSYSTEMS=="usb", IMPORT{builtin}="usb_id"`)
means udev already tried ATA IDENTIFY through the bridge (SAT) for
**non-removable** USB disks; if `ID_ATA_ROTATION_RATE_RPM` is in the db, the
bridge supports pass-through and the value is authoritative for HDD vs SSD.

`udevadm info -q property /dev/sdb` prints the same keys plus `DEVNAME`,
`DEVPATH`, `DEVTYPE=disk|partition`, `DISKSEQ`, `MAJOR`, `MINOR`, `DEVLINKS`
(space-separated), `TAGS`. The `udev` crate (libudev bindings) reads the same
db; the in-process parser above avoids the C dependency.

### 2.6 `/dev/disk/by-id/` naming (60-persistent-storage.rules)

| Pattern | Transport | Composition |
|---|---|---|
| `ata-<ID_SERIAL>` | SATA/PATA via libata | `MODEL_SERIAL` with spaces → `_` |
| `usb-<ID_USB_SERIAL>` (always, rule `ENV{ID_USB_SERIAL}=="?*"`) and `<ID_BUS>-<ID_SERIAL>` (rule `SYMLINK+="disk/by-id/$env{ID_BUS}-$env{ID_SERIAL}"`, i.e. `usb-…`, or `ata-MODEL_SERIAL` when `ata_id` succeeded through the bridge) | USB | `VENDOR_MODEL_SERIAL-0:0`; also `wwn-…` if `ID_WWN_WITH_EXTENSION` present |
| `nvme-<MODEL>_<SERIAL>`, `nvme-<MODEL>_<SERIAL>_<NSID>`, `nvme-eui.<hex>` / `nvme-nguid.<hex>` / `nvme-uuid.<hex>` | NVMe | observed: `nvme-KXG60ZNV1T02_KIOXIA_307F71PHFGQL`, `…_1`, `nvme-eui.00000000000000018ce38e05002143c9` |
| `mmc-<NAME>_<SERIAL>` | SD/MMC | e.g. `mmc-SL32G_0x1b2c3d4e` |
| `scsi-<scsi_id>` | SAS/FC/RAID | `scsi-3<naa>`, `scsi-S<vendor>_<model>_<serial>`, `scsi-1<t10>` |
| `virtio-<serial>` | virtio | |
| `wwn-0x<naa>` | any with VPD 0x83 NAA | |
| `-part<N>` suffix | | partition links |
| `/dev/disk/by-path/<ID_PATH>` | | physical slot; `pci-…-usb-0:3:1.0-scsi-0:0:0:0`, `pci-…-ata-1`, `pci-…-nvme-1` |
| `/dev/disk/by-diskseq/<N>` | | present on systemd ≥ 251 |

A USB stick without a serial still gets a `usb-VENDOR_MODEL-0:0` link, so
two identical serial-less sticks collide; `by-path` disambiguates by port.

### 2.7 Binary pages worth decoding

**SCSI VPD page 0x80 (Unit Serial Number)** — `device/vpd_pg80`:

| Byte | Field |
|---|---|
| 0 | peripheral qualifier/type (bits 4:0 = type) |
| 1 | page code `0x80` |
| 2–3 | page length `n`, big-endian |
| 4 … 4+n−1 | product serial number, ASCII, space padded |

**SCSI VPD page 0xB1 (Block Device Characteristics)** — `device/vpd_pgb1`:

| Byte | Field |
|---|---|
| 0–1 | type / page code `0xB1` |
| 2–3 | page length (`0x003C`) |
| 4–5 | **medium rotation rate**, big-endian: `0x0000` not reported, `0x0001` non-rotating (SSD), `0x0401`–`0xFFFE` RPM |
| 6 | product type (0 not indicated, 1 CFast, 2 CompactFlash, 3 MemoryStick, 4 MultiMediaCard, 5 SD card, 6 XQD, 7 Universal Flash Storage card; ≥ 0xF0 vendor specific) — sg3_utils `product_type_arr[]` |
| 7 | bits 7:6 WABEREQ, 5:4 WACEREQ, 3:0 nominal form factor (`0` not reported, `1` 5.25", `2` 3.5", `3` 2.5", `4` 1.8", `5` < 1.8") — sg3_utils `decode_block_dev_ch_vpd()` |
| 8 | bit 6 MACT, bits 5:4 ZONED (sd.c: `(data[8] >> 4) & 3`; 1 = host-aware, 2 = host-managed via ZBC), bit 2 RBWZ, bit 1 FUAB, bit 0 VBULS; sg3_utils requires page length ≥ 64 bytes total to decode |

sd.c `sd_read_block_characteristics()`: `if (!vpd || vpd->len <= 8) return;` then
`rot = get_unaligned_be16(&vpd->data[4]); if (rot == 1) → lim->features &= ~(BLK_FEAT_ROTATIONAL | BLK_FEAT_ADD_RANDOM)`
and `sdkp->zoned = (vpd->data[8] >> 4) & 3`.
So a bridge that drops the page leaves `rotational=1`. Rotation-rate
value ranges per SBC-4 / sg3_utils `decode_block_dev_ch_vpd()`: `0x0000` not
reported, `0x0001` non-rotating, `0x0002`–`0x0400` and `0xFFFF` reserved,
`0x0401`–`0xFFFE` RPM. libata synthesises this page for SATA disks
(`ata_scsiop_inq_b1`: `rbuf[3] = 0x3c`, bytes 4–5 from word 217, byte 7 form
factor, byte 8 = zoned << 4).

**ATA IDENTIFY DEVICE (512 bytes, 256 little-endian 16-bit words; strings are
byte-swapped within each word)** — obtained via `HDIO_GET_IDENTITY`
(`0x030d`, arg = 512-byte buffer; libata serves it **without any capability
check**: `ata_sas_scsi_ioctl()` → `case HDIO_GET_IDENTITY: return ata_get_identity(ap, scsidev, arg);`
copies `dev->id` (256 words, host-endian) and returns `ENOMSG` when no ATA
device is attached; only `HDIO_DRIVE_CMD`/`HDIO_DRIVE_TASK` check
`capable(CAP_SYS_ADMIN) && capable(CAP_SYS_RAWIO)`. The call reaches libata
through `sd_ioctl` → `scsi_ioctl` → `hostt->ioctl`, so it is `-ENODEV` while
the device is in error recovery/offline, `-EINVAL` on hosts with no `ioctl`
op (usb-storage, uas) and `-ENOTTY` on NVMe) or SG_IO ATA PASS-THROUGH
(`ATA_12` `0xa1` / `ATA_16` `0x85` appear in **neither** list of
`scsi_cmd_allowed()` — not the read-safe list and not the `open_for_write`
list — so `sg_io()` returns `EPERM` without `CAP_SYS_RAWIO` on any open mode,
read-only or read-write; verified in drivers/scsi/scsi_ioctl.c):

| Words | Field |
|---|---|
| 10–19 | serial number (20 chars) |
| 23–26 | firmware revision (8 chars) |
| 27–46 | model number (40 chars) |
| 106 | bit 12 = logical sector > 256 words; bit 13 = multiple logical per physical; bits 3:0 = log2(physical/logical) |
| 117–118 | words per logical sector (if 106 bit 12) |
| 108–111 | WWN (NAA 5) |
| 217 | nominal media rotation rate: `0x0001` = non-rotating |
| 0 | bit 15 = 0 → ATA device, 1 → ATAPI; bit 7 removable |
| 217 absent/zero | not reported — assume rotational |

### 2.8 `/proc/self/mountinfo`

```
36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
(1)(2)(3)   (4)   (5)      (6)      (7)   (8) (9)   (10)         (11)
```

1 mount id, 2 parent id, **3 `major:minor` = `st_dev` of files on this mount**,
4 root within fs, 5 **mount point**, 6 per-mount options, 7 optional
`tag[:value]` fields (0 or more), 8 literal `-`, 9 fs type, **10 mount source**
(`/dev/nvme1n1p3`, `none`, `tmpfs`, UUID=…), 11 super options.
Fields 4, 5 and 10 are escaped: `\040` space, `\011` tab, `\012` newline,
`\134` backslash — decode before comparing paths. Parse by splitting on
spaces, locating the `-` separator from the left after field 6 (optional
fields never contain `-` alone).

Trap: field 3 is **not** the block device's `dev_t` for btrfs, overlayfs,
zfs, fuse (anonymous `0:NN` majors). Map a mount to a disk by *both*:
(a) field 3 == a partition's/disk's `dev` from sysfs, or (b) field 10 resolved
with `realpath` and `stat().st_rdev` to a block node whose `MAJ:MIN` is in the
source's device set (§3.4). Also scan `/proc/swaps` (column 1 = device path)
— a swap partition on the source is "mounted" for our purposes.

Observed on the dev box:
`33 2 259:8 / / rw,relatime shared:1 - ext4 /dev/nvme1n1p3 rw,stripe=32`
and `stat -c %d /` → `66312` = `(259 << 8) | 8` in the legacy encoding;
always decode `st_dev` with `libc::major()`/`minor()` (`gnu_dev_major`),
never by shifting.

### 2.9 Block ioctls (`<linux/fs.h>`, type `0x12`)

Numeric values: `_IO(0x12, n)` = `0x1200 | n`; `_IOR(0x12, n, size_t)` =
`0x80081200 | n` on 64-bit (`0x80041200 | n` on 32-bit).

| ioctl | Value (x86_64) | Arg | Returns | Capability |
|---|---|---|---|---|
| `BLKGETSIZE64` | `0x80081272` (`_IOR(0x12,114,size_t)`) | `*u64` | `bdev_nr_bytes()` — **bytes**; works on partitions too | none |
| `BLKSSZGET` | `0x1268` | `*int` | logical block size | none |
| `BLKPBSZGET` | `0x127B` | `*uint` | physical block size | none |
| `BLKIOMIN` | `0x1278` | `*uint` | minimum_io_size | none |
| `BLKIOOPT` | `0x1279` | `*uint` | optimal_io_size | none |
| `BLKALIGNOFF` | `0x127A` | `*int` | alignment_offset | none |
| `BLKROGET` | `0x125E` | `*int` | 1 if read-only | none |
| `BLKGETDISKSEQ` | `0x80081280` (`_IOR(0x12,128,__u64)`) | `*u64` | diskseq (≥ 5.15) | none |
| `BLKSECTGET` | `0x1267` | `*ushort` | max sectors per request | none |
| `BLKGETSIZE` | `0x1260` | `*long` | size/512 — **overflows > 2 TiB on 32-bit**; do not use | none |
| `BLKROSET` | `0x125D` | `*int` | sets kernel RO flag | `CAP_SYS_ADMIN` — **not used** |
| `BLKFLSBUF` | `0x1261` | — | drops page cache for the bdev | `CAP_SYS_ADMIN`, returns `EACCES` otherwise — **not used** |
| `BLKRRPART` | `0x125F` | — | re-read partition table | `CAP_SYS_ADMIN` — **not used** (fails with `EBUSY` when mounted anyway) |
| `HDIO_GETGEO` | `0x0301` | `struct hd_geometry` | fake CHS | not needed |
| `HDIO_GET_IDENTITY` | `0x030d` | 512 bytes | ATA IDENTIFY (libata, SATA only; `EINVAL` on usb-storage/uas, `ENOTTY` on NVMe, `ENOMSG` if libata has no device) | none (libata-scsi.c `ata_sas_scsi_ioctl`) |

All values above were checked against `/usr/include/linux/fs.h` and
`/usr/include/linux/hdreg.h` on the dev box (`BLKGETDISKSEQ _IOR(0x12,128,__u64)`,
`BLKGETSIZE64 _IOR(0x12,114,size_t)`). The `libc` crate 0.2.190 exports only
`BLKSSZGET`, `BLKPBSZGET`, `BLKIOMIN`, `BLKIOOPT` of these (see §6) — define
the rest locally.

On a **regular file** opened as a source (an existing image), every `BLK*`
ioctl fails with `ENOTTY`; use `fstat().st_size` and assume 512-byte logical
sectors unless the user overrides (imaging spec `--sector-size`).

### 2.10 Netlink uevents (`NETLINK_KOBJECT_UEVENT` = 15)

Socket: `socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC | SOCK_NONBLOCK, 15)`,
`bind` with `sockaddr_nl { nl_family: AF_NETLINK, nl_pid: 0, nl_groups: 1 }`.
Group **1** = raw kernel uevents (no privileges needed: `uevent_net_init`
registers the family with `.groups = 1, .flags = NL_CFG_F_NONROOT_RECV`, and
`kobject_uevent_env` broadcasts with `netlink_broadcast(uevent_sock, skb, 0, 1, …)`;
`EPERM` on bind in some containers/user namespaces → fall back to polling). Group
**2** = udev's post-processing broadcast (what `udevadm monitor -u` shows),
messages carry a 40-byte header: `char prefix[8] = "libudev\0"`,
`u32 magic = htonl(0xfeedcafe)`, `u32 header_size`, `u32 properties_off`,
`u32 properties_len`, `u32 filter_subsystem_hash`, `u32 filter_devtype_hash`,
`u32 filter_tag_bloom_hi`, `u32 filter_tag_bloom_lo`; properties follow at
`properties_off`.

Kernel (group 1) message = one datagram, **no `nlmsghdr`**, ≤ 2048 bytes
(`UEVENT_BUFFER_SIZE`, `include/linux/kobject.h`; at most `UEVENT_NUM_ENVP` =
64 variables; header built as `sprintf(scratch, "%s@%s", action_string, devpath)`,
`SEQNUM=%llu` is appended last, `SYNTH_UUID=` only for uevents triggered by a
write to a sysfs `uevent` file):

```
"<action>@<devpath>\0" "ACTION=<action>\0" "DEVPATH=<devpath>\0" "SUBSYSTEM=block\0"
"SYNTH_UUID=0\0"? "MAJOR=8\0" "MINOR=16\0" "DEVNAME=sdb\0" "DEVTYPE=disk\0"
"DISKSEQ=12\0" "SEQNUM=4711\0"
```

`action` ∈ `add remove change move online offline bind unbind`. Relevant
filter: `SUBSYSTEM=block` and `DEVTYPE=disk|partition`. `remove` of the disk
we are imaging → device gone. `change` on a disk with `DISK_MEDIA_CHANGE=1`
(sr/removable) → media swapped. `add` with the same `ID_SERIAL` (group 2) or
with the same resolved serial after sysfs re-read (group 1) → re-enumerated,
possibly under a new `DEVNAME`.

Polling fallback (no netlink): every 500 ms `stat("/sys/block/<dev>")`
→ `ENOENT` means removed; compare `diskseq` content on every reopen.
sysfs does **not** deliver inotify events for attribute changes; do not rely
on inotify.

### 2.11 SMART via `smartctl -j` (optional)

Run `smartctl -j -a <devnode>` (`--json=c` for compact) only if `smartctl`
is in `PATH`; it needs root (SG_IO/NVMe admin pass-through). Parse with
`serde_json::Value`; never fail the run because of it. Keys (smartmontools
≥ 7.0, `json_format_version: [1, 0]`):

| JSON path | Type | Use |
|---|---|---|
| `smartctl.exit_status` | int | bit 0 command line did not parse, bit 1 device open failed / no IDENTIFY DEVICE structure / device in low-power mode, bit 2 some SMART or other ATA command failed or SMART data checksum error, **bit 3 SMART status check returned "DISK FAILING"**, bit 4 prefail attributes ≤ threshold, bit 5 DISK OK but some usage/prefail attribute was ≤ threshold in the past, bit 6 device error log contains records of errors, bit 7 self-test log contains records of errors (smartctl(8) RETURN VALUES, smartmontools 7.4). |
| `smartctl.messages[].{string,severity}` | | show to user |
| `device.{name,info_name,type,protocol}` | | `type`: `sat`, `ata`, `nvme`, `scsi`, `usbjmicron`…; `protocol`: `ATA`/`NVMe`/`SCSI` |
| `model_name`, `serial_number`, `firmware_version` | string | cross-check identity |
| `user_capacity.{blocks,bytes}` | int | cross-check `BLKGETSIZE64` |
| `logical_block_size`, `physical_block_size` | int | |
| `rotation_rate` | int | 0 = SSD, else RPM — **the most reliable SSD/HDD signal for USB-SATA bridges** when `-d sat` works |
| `smart_status.passed` | bool | |
| `smart_status.nvme.value` | int | NVMe critical warning byte |
| `ata_smart_attributes.table[]` | array of `{id,name,value,worst,thresh,when_failed,flags{value,string,prefailure,updated_online,performance,error_rate,event_count,auto_keep},raw{value,string}}` | ids of interest: 5 Reallocated_Sector_Ct, 9 Power_On_Hours, 187 Reported_Uncorrect, 188 Command_Timeout, 194 Temperature, 196 Reallocated_Event_Count, 197 Current_Pending_Sector, 198 Offline_Uncorrectable, 199 UDMA_CRC_Error_Count |
| `nvme_smart_health_information_log.{critical_warning,temperature,available_spare,available_spare_threshold,percentage_used,data_units_read,data_units_written,host_reads,host_writes,controller_busy_time,power_cycles,power_on_hours,unsafe_shutdowns,media_errors,num_err_log_entries}` | ints | `critical_warning` bits: 0 spare low, 1 temperature, 2 reliability degraded, **3 read-only mode**, 4 volatile backup failed |
| `power_on_time.hours`, `power_cycle_count`, `temperature.current` | int | |
| `ata_smart_error_log.summary.count`, `ata_smart_self_test_log.standard.table[]` | | |

For USB bridges smartctl may need `-d sat`, `-d usbjmicron`, `-d usbsunplus`;
try `-d sat` once if the first call returns exit bit 1 or 2.

### 2.12 Windows (best effort)

**Enumeration.** Either probe `\\.\PhysicalDrive0` … `\\.\PhysicalDrive63`
with `CreateFileW(path, 0 /* no access */, FILE_SHARE_READ|FILE_SHARE_WRITE,
NULL, OPEN_EXISTING, 0, NULL)` — `dwDesiredAccess = 0` succeeds without
elevation and is enough for `FILE_ANY_ACCESS` IOCTLs; `GetLastError() ==
ERROR_FILE_NOT_FOUND (2)` means no such drive (gaps are possible, keep
probing to 63); or use `SetupDiGetClassDevsW(&GUID_DEVINTERFACE_DISK,
NULL, NULL, DIGCF_PRESENT|DIGCF_DEVICEINTERFACE)` +
`SetupDiEnumDeviceInterfaces` + `SetupDiGetDeviceInterfaceDetailW` to get
`\\?\scsi#disk&ven_…` paths, then `IOCTL_STORAGE_GET_DEVICE_NUMBER` to learn
`DeviceNumber` N. `GUID_DEVINTERFACE_DISK = {53F56307-B6BF-11D0-94F2-00A0C91EFB8B}`
(windows-sys 0.61.2 `Win32::System::Ioctl::GUID_DEVINTERFACE_DISK =
GUID::from_u128(0x53f56307_b6bf_11d0_94f2_00a0c91efb8b)` — note it lives in
the `Ioctl` module, not in `DeviceAndDriverInstallation`).

**IOCTL codes** (`CTL_CODE(DeviceType, Function, Method, Access)` =
`(DeviceType << 16) | (Access << 14) | (Function << 2) | Method`;
`windows-sys 0.61.2` values confirmed from docs.rs):

| IOCTL | Value | Access bits | In/out |
|---|---|---|---|
| `IOCTL_STORAGE_QUERY_PROPERTY` | `0x002D1400` (2954240) | `FILE_ANY_ACCESS` | in `STORAGE_PROPERTY_QUERY`, out depends on `PropertyId` |
| `IOCTL_STORAGE_GET_DEVICE_NUMBER` | `0x002D1080` (2953344) | `FILE_ANY_ACCESS` | out `STORAGE_DEVICE_NUMBER { DeviceType: u32, DeviceNumber: u32, PartitionNumber: u32 }` |
| `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` | `0x000700A0` (458912) | `FILE_ANY_ACCESS` | out `DISK_GEOMETRY_EX` (variable; pass ≥ 256 bytes) |
| `IOCTL_DISK_GET_LENGTH_INFO` | `0x0007405C` (475228) | **`FILE_READ_ACCESS`** → handle must have `GENERIC_READ` → elevation | out `GET_LENGTH_INFORMATION { Length: i64 }` |
| `IOCTL_DISK_GET_DRIVE_LAYOUT_EX` | `0x00070050` (458832) | `FILE_ANY_ACCESS` | partition table as Windows sees it (`CTL_CODE(IOCTL_DISK_BASE, 0x14, METHOD_BUFFERED, FILE_ANY_ACCESS)`; windows-sys 0.61.2 `= 458832`) |
| `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` | `0x00560000` (5636096) | `FILE_ANY_ACCESS` | on a volume handle (`\\.\C:`): `VOLUME_DISK_EXTENTS { NumberOfDiskExtents: u32, Extents: [DISK_EXTENT { DiskNumber: u32, StartingOffset: i64, ExtentLength: i64 }] }`; `ERROR_MORE_DATA (234)` → re-query with a bigger buffer (windows-sys 0.61.2 `= 5636096`) |

`STORAGE_PROPERTY_QUERY { PropertyId: u32, QueryType: u32 (PropertyStandardQuery = 0, PropertyExistsQuery = 1), AdditionalParameters: [u8; 1] }`.

`STORAGE_PROPERTY_ID` values: `StorageDeviceProperty = 0`,
`StorageAdapterProperty = 1`, `StorageDeviceIdProperty = 2`,
`StorageDeviceUniqueIdProperty = 3`, `StorageDeviceWriteCacheProperty = 4`,
`StorageMiniportProperty = 5`, **`StorageAccessAlignmentProperty = 6`**,
**`StorageDeviceSeekPenaltyProperty = 7`**, `StorageDeviceTrimProperty = 8`,
… `StorageDeviceIoCapabilityProperty = 48`, `StorageDeviceProtocolSpecificProperty = 50`,
`StorageDeviceTemperatureProperty = 52`.

`STORAGE_DEVICE_DESCRIPTOR` (response to `StorageDeviceProperty`; query
once with an **8-byte** `STORAGE_DESCRIPTOR_HEADER { Version: u32, Size: u32 }`
as the output buffer, read `Size`, then re-query with `Size` bytes — the
Microsoft remarks on `STORAGE_DEVICE_DESCRIPTOR` prescribe exactly this):

| Offset | Type | Field |
|---|---|---|
| 0 | u32 | `Version` |
| 4 | u32 | `Size` (total, incl. appended strings) |
| 8 | u8 | `DeviceType` (SCSI peripheral type; 0 disk, 5 CD-ROM) |
| 9 | u8 | `DeviceTypeModifier` |
| 10 | u8 (BOOLEAN) | `RemovableMedia` |
| 11 | u8 (BOOLEAN) | `CommandQueueing` |
| 12 | u32 | `VendorIdOffset` — byte offset **from the start of the structure** to NUL-terminated ASCII; 0 = none |
| 16 | u32 | `ProductIdOffset` |
| 20 | u32 | `ProductRevisionOffset` |
| 24 | u32 | `SerialNumberOffset` |
| 28 | u32 | `BusType` (enum below) |
| 32 | u32 | `RawPropertiesLength` |
| 36 | u8[] | `RawDeviceProperties` |

`STORAGE_BUS_TYPE`: `Unknown 0, Scsi 1, Atapi 2, Ata 3, 1394 4, Ssa 5,
Fibre 6, **Usb 7**, RAID 8, iScsi 9, Sas 10, **Sata 11**, **Sd 12**,
**Mmc 13**, Virtual 14, FileBackedVirtual 15, Spaces 16, **Nvme 17**, SCM 18,
Ufs 19, Nvmeof 20, Max 21, MaxReserved 0x7F` (current winioctl.h per Microsoft
Learn). **windows-sys 0.61.2 is generated from an older header**: it has no
`BusTypeNvmeof` and defines `BusTypeMax = 20`; the enum is in
`Win32::Storage::FileSystem::STORAGE_BUS_TYPE` (= `i32`), not in `Ioctl`.

Serial strings from this descriptor are frequently **byte-swapped pairs** for
ATA (`"3W2K1FDS"` for `W32KF1SD`) and may be hex-encoded for USB; normalise
by trimming and, for `BusTypeAta/Sata`, un-swapping when the string looks
like swapped ATA (UNVERIFIED heuristic — some drivers already fix it).

`DEVICE_SEEK_PENALTY_DESCRIPTOR { Version: u32, Size: u32, IncursSeekPenalty: BOOLEAN }`
(Windows 7+; windows-sys 0.61.2 types `BOOLEAN` fields — `IncursSeekPenalty`,
`RemovableMedia`, `CommandQueueing` — as Rust `bool`; when parsing raw byte
blobs read them as `u8`): `IncursSeekPenalty != 0` → rotational. Returns
`ERROR_INVALID_FUNCTION (1)` / `ERROR_NOT_SUPPORTED (50)` for devices that
don't answer (many USB bridges) → unknown, not HDD.

`STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR { Version, Size, BytesPerCacheLine,
BytesOffsetForCacheAlignment, BytesPerLogicalSector, BytesPerPhysicalSector,
BytesOffsetForSectorAlignment: u32 ×7 }` (Vista+).

`DISK_GEOMETRY_EX { Geometry: DISK_GEOMETRY { Cylinders: i64, MediaType: u32,
TracksPerCylinder: u32, SectorsPerTrack: u32, BytesPerSector: u32 },
DiskSize: i64, Data: [u8; 1] }` — `DiskSize` is the real byte length and
works with `dwDesiredAccess = 0` (unlike `GET_LENGTH_INFO`); `MediaType`
`RemovableMedia = 11`, `FixedMedia = 12`, `Unknown = 0`.

**Reading.** Reopen with `GENERIC_READ`, `FILE_SHARE_READ | FILE_SHARE_WRITE`,
`OPEN_EXISTING`, `FILE_FLAG_NO_BUFFERING (0x20000000) | FILE_FLAG_SEQUENTIAL_SCAN`.
Opening a physical drive with `GENERIC_READ` requires an elevated
(administrator) token since Vista; `ERROR_ACCESS_DENIED (5)` → tell the user
to re-run elevated. Source: CreateFileA, "Physical Disks and Volumes":
"Direct access to the disk or to a volume is restricted … The caller must
have administrative privileges … The dwCreationDisposition parameter must
have the OPEN_EXISTING flag … The dwDesiredAccess parameter can be zero,
allowing the application to query device attributes without accessing a
device"; Windows XP/Server 2003 are listed as not restricted.
Requirements with `FILE_FLAG_NO_BUFFERING` (and for device handles always):
offset and length multiples of the **logical** sector size; buffer address
aligned to the **physical** sector size (use `VirtualAlloc`, page aligned).
Position with `OVERLAPPED.Offset/OffsetHigh` per call (`ReadFile` on a
device handle, no `SetFilePointerEx` needed, thread-safe). A read that hits a
bad sector fails with `ERROR_CRC (23)`, `ERROR_SECTOR_NOT_FOUND (27)`,
`ERROR_IO_DEVICE (1117)` or `ERROR_DEVICE_NOT_CONNECTED (1167)`;
`ERROR_NOT_READY (21)` / `ERROR_NO_MEDIA_IN_DRIVE (1112)` for removed media.
`FSCTL_LOCK_VOLUME` / `FSCTL_DISMOUNT_VOLUME` are **not** needed for reads
and must not be issued (they are for writers).

**windows-sys 0.61.2 paths**: `Win32::Storage::FileSystem::{CreateFileW,
ReadFile, FILE_FLAG_NO_BUFFERING, FILE_FLAG_SEQUENTIAL_SCAN, FILE_SHARE_READ,
FILE_SHARE_WRITE, OPEN_EXISTING, FILE_GENERIC_READ}`;
`Win32::System::IO::DeviceIoControl`; `Win32::System::Ioctl::{IOCTL_*,
STORAGE_PROPERTY_QUERY, STORAGE_DEVICE_DESCRIPTOR, STORAGE_DESCRIPTOR_HEADER,
STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, DEVICE_SEEK_PENALTY_DESCRIPTOR,
GET_LENGTH_INFORMATION, DISK_GEOMETRY_EX, STORAGE_DEVICE_NUMBER,
StorageDeviceProperty, StorageDeviceSeekPenaltyProperty,
StorageAccessAlignmentProperty, PropertyStandardQuery, BusTypeUsb,
BusTypeNvme, BusTypeSata, BusTypeSd, BusTypeMmc}`;
`Win32::Foundation::{INVALID_HANDLE_VALUE, GetLastError, CloseHandle, GENERIC_READ}`;
`Win32::Devices::DeviceAndDriverInstallation::SetupDi*` (feature
`Win32_Devices_DeviceAndDriverInstallation`); `GUID_DEVINTERFACE_DISK` and
`VOLUME_DISK_EXTENTS`/`DISK_EXTENT` are in `Win32::System::Ioctl`;
`STORAGE_BUS_TYPE` and the `BusType*` constants are in
`Win32::Storage::FileSystem`. Numeric constants confirmed from the crate
source: `FILE_FLAG_NO_BUFFERING = 0x20000000`, `FILE_FLAG_SEQUENTIAL_SCAN =
0x08000000`, `GENERIC_READ = 0x80000000`, `FILE_GENERIC_READ = 0x120089`,
`ERROR_FILE_NOT_FOUND 2`, `ERROR_ACCESS_DENIED 5`, `ERROR_NOT_READY 21`,
`ERROR_CRC 23`, `ERROR_SECTOR_NOT_FOUND 27`, `ERROR_NOT_SUPPORTED 50`,
`ERROR_NO_MEDIA_IN_DRIVE 1112`, `ERROR_IO_DEVICE 1117`,
`ERROR_DEVICE_NOT_CONNECTED 1167`, `RemovableMedia 11`, `FixedMedia 12`. Cargo features:
`Win32_Foundation`, `Win32_Storage_FileSystem`, `Win32_System_IO`,
`Win32_System_Ioctl`, `Win32_Devices_DeviceAndDriverInstallation`,
`Win32_System_Memory` (VirtualAlloc).

### 2.13 macOS (pointers only)

- Whole disks are `/dev/diskN`, partitions `/dev/diskNsM`; always read the
  **raw** node `/dev/rdiskN` (character device, unbuffered, reads must be
  multiples of `DeviceBlockSize` and offsets block aligned; `/dev/diskN`
  goes through the buffer cache and will serve stale data on retries).
  Opening needs root (`sudo`) or the `disk` ownership that `diskutil`
  grants; `EBUSY` if any volume on it is mounted — run
  `diskutil unmountDisk /dev/diskN` first (user action, not automatic).
- `diskutil info -plist /dev/diskN` → XML plist (parse with `plist` crate).
  Keys: `DeviceIdentifier` (`disk2`), `DeviceNode`, `MediaName`,
  `DeviceBlockSize` (int), `TotalSize`/`Size` (int bytes), `SolidState`
  (bool), `Internal` (bool), `Removable`/`RemovableMedia` (bool), `Ejectable`,
  `BusProtocol` (`USB`, `SATA`, `PCI-Express`, `Secure Digital`, `Disk Image`,
  `Apple Fabric`), `VirtualOrPhysical` (`Physical`/`Virtual`), `WholeDisk`
  (bool), `IORegistryEntryName`, `Content` (`GUID_partition_scheme`,
  `FDisk_partition_scheme`), `SMARTStatus` (`Verified`/`Not Supported`),
  `MediaType` (`Generic`). Key spellings confirmed from two plist-consuming
  programs (munkireport `disk_report/scripts/disk_info`, Go `mountefi/disk`):
  `DeviceIdentifier`, `DeviceNode`, `WholeDisk`, `ParentWholeDisk`, `Content`,
  `MountPoint`, `FilesystemName`, `FilesystemType`, `VolumeName`, `VolumeUUID`,
  `DiskUUID`, `BusProtocol`, `SolidState`, `Internal`, `Ejectable`,
  `SMARTStatus`, `TotalSize`, `FreeSpace`; `diskutil list -plist` gives
  `AllDisksAndPartitions` (each with `Partitions`/`APFSVolumes`), `AllDisks`,
  `WholeDisks`, `VolumesFromDisks`. UNVERIFIED (no primary source found):
  `MediaName`, `DeviceBlockSize`, `Size`, `Removable`/`RemovableMedia`,
  `VirtualOrPhysical`, `IORegistryEntryName`, `MediaType` — treat as optional
  and fall back to `DKIOCGETBLOCKSIZE`/`DKIOCGETBLOCKCOUNT` for sector data.
- `ioctl(fd, DKIOCGETBLOCKSIZE, &u32)` and `DKIOCGETBLOCKCOUNT (&u64)` from
  `<sys/disk.h>` give the sector size and count without `diskutil`. xnu
  `bsd/sys/disk.h`: `DKIOCGETBLOCKSIZE = _IOR('d', 24, uint32_t)` =
  `0x40046418`, `DKIOCGETBLOCKCOUNT = _IOR('d', 25, uint64_t)` = `0x40086419`,
  `DKIOCGETPHYSICALBLOCKSIZE = _IOR('d', 77, uint32_t)` = `0x4004644D`
  (`_IOR` = `0x40000000 | (sizeof << 16) | ('d' << 8) | n`). The `libc` crate
  0.2.190 does **not** export these; define them locally.

---

## 3. Algorithms (pseudocode)

### 3.1 Enumerate (Linux)

```
fn enumerate() -> Vec<DeviceInfo>:
    for name in readdir("/sys/block") sorted:                # whole disks only
        base = "/sys/block/" + name
        if read_u8(base/"hidden") == Some(1): continue
        real = realpath(base)
        info = DeviceInfo { name, devnode: "/dev/" + name, sysfs: real, .. }
        info.devt        = parse_majmin(read(base/"dev"))
        info.size_bytes  = read_u64(base/"size")? * 512
        info.removable   = read_u8(base/"removable") == 1
        info.ro          = read_u8(base/"ro") == 1
        info.diskseq     = read_u64(base/"diskseq")
        info.lbs         = read_u32(base/"queue/logical_block_size").unwrap_or(512)
        info.pbs         = read_u32(base/"queue/physical_block_size").unwrap_or(lbs)
        info.rotational  = read_u8(base/"queue/rotational").map(|v| v == 1)
        info.max_sectors_kb = read_u32(base/"queue/max_sectors_kb")
        info.dma_align_mask = read_u32(base/"queue/dma_alignment").unwrap_or(511)
        info.zoned       = read(base/"queue/zoned")
        info.transport   = transport_from_realpath(real, name)        # §2.4
        info.identity    = identity(base, info.transport)             # §3.3
        info.partitions  = partitions(base, name)                      # §2.2
        info.udev        = parse_udev_db(info.devt)                    # §2.5, Option
        info.mounts      = mounts_for(info)                            # §3.4
        info.class       = classify(&info)                             # §3.2
        if info.size_bytes == 0 && info.removable: info.media_present = false
        push(info)
    # dm/md/loop are listed too (class Virtual) so the output guard can walk slaves/
```

`/dev/<name>` is the devtmpfs node; if the user passed a `by-id`/`by-path`
symlink, `realpath` it first and derive `name` from the basename; if they
passed a partition (`sdb1`), note `source_is_partition = true`, derive the
parent disk from `realpath("/sys/class/block/sdb1")/..`, and still run the
guard against the whole disk.

Size `0` with `removable = 1` is "no media" (card reader without a card):
list it, mark `media_present = false`, do not offer imaging.

### 3.2 Classification decision table

Inputs: `transport`, `rotational` (Option), `removable`, `size_bytes`,
`scsi_type`, `usb.{driver, speed, vendor_id, product, serial}`,
`udev.ID_ATA_ROTATION_RATE_RPM`, `vpd_b1.rotation_rate`, `vpd_b1.product_type`,
`mmc.type`, `smart.rotation_rate` (if available).

```
fn classify(d) -> (Class, Confidence, Vec<Reason>):
    if d.transport == Virtual:                                   -> Virtual, High
    if d.name starts_with "sr" || d.scsi_type == Some(5):        -> Optical, High
    if d.transport == Mmc:
        if d.mmc.type == "MMC": -> SdMmc{kind: Emmc}, High
        else:                   -> SdMmc{kind: Sd{csd_struct}}, High
    if d.transport == Nvme:                                      -> SsdNvme, High
    if d.transport == Ata:
        rot = first_some(d.vpd_b1.rotation_rate, d.udev.ata_rpm, d.rotational)
        if rot == NonRotating(1 / rpm 0 / rotational 0): -> SsdSata, High
        if rot == Rpm(_) or rotational == Some(1):
            # libata always has IDENTIFY; rotational=1 here means word 217 said so or was absent
            if d.udev.ata_rpm.is_some() || d.vpd_b1.present: -> Hdd, High
            else:                                            -> Hdd, Medium   # not reported
    if d.transport == Usb:
        # 1. product-type hints that the bridge exposes a card
        if d.vpd_b1.product_type in {4 MMC, 5 SD, 2 CF, 3 MS, 7 UFS}: -> SdMmc{via_usb_reader}, High
        if d.usb.vendor_id in CARD_READER_VIDS (05e3 Genesys, 058f Alcor, 0bda Realtek)
           && (d.removable || d.usb.product contains_ci "card"|"reader"|"SD"):
                                                                  -> SdMmc{via_usb_reader}, Medium
        if d.usb.product contains_ci "card reader" || "SD/MMC":   -> SdMmc{via_usb_reader}, Medium
        # 2. explicit rotation info passed through the bridge
        if d.vpd_b1.rotation_rate == Some(1) || d.udev.ata_rpm == Some(0) || d.smart.rotation_rate == Some(0):
            if d.removable: -> UsbFlash, Medium   # some sticks answer B1 = 1 too
            else:           -> UsbSsd, High
        if d.vpd_b1.rotation_rate == Some(rpm > 1) || d.udev.ata_rpm == Some(rpm > 0) || d.smart.rotation_rate > 0:
                                                                  -> UsbHdd, High
        # 3. no rotation info: rotational=1 is the kernel default, not evidence
        if d.removable && d.size_bytes <= 512 GiB:                -> UsbFlash, Medium
        if d.removable && d.size_bytes >  512 GiB:                -> UsbFlash, Low   # or external SSD lying
        if !d.removable && d.size_bytes >= 500 GB && d.usb.driver == "usb-storage": -> UsbHdd, Low
        if !d.removable && d.usb.driver == "uas":                  -> UsbHdd, Low  # UAS enclosures: HDD or SSD, unknown
        if !d.removable:                                           -> UsbHdd, Low
        -> UsbFlash, Low
    if d.transport == Scsi:
        match d.rotational { Some(false) -> SsdSata (really "SSD, SCSI"), Medium; Some(true) -> Hdd, Medium; None -> Unknown }
    if d.transport == Virtio or Ccw or Unknown: -> Unknown, Low
```

Every branch pushes a `Reason` (`"vpd_b1.rotation_rate=1"`,
`"udev ID_ATA_ROTATION_RATE_RPM=7200"`, `"removable=1 && size 61 GB"`, …).
The imaging engine maps classes to strategies; `Low`-confidence classes
make the engine run its first pass with the conservative (HDD) strategy and
re-evaluate after measuring seek latency (imaging spec).

Known misreports the table is designed around:

| Symptom | Cause | Mitigation above |
|---|---|---|
| USB-SATA bridge shows `rotational=1` for an SSD | BOT bridges don't forward VPD B1; sd.c default is rotational | treat `rotational=1` on USB as "unknown"; prefer `ID_ATA_ROTATION_RATE_RPM`, VPD B1, smartctl `-d sat` |
| USB stick shows `rotational=1` | same | `removable=1` + size ⇒ flash |
| USB SD reader shows as a USB disk with vendor `Generic`/`Mass Storage Device`, no serial, `removable=1` | reader is the SCSI device, card is the medium | VID list, product string, VPD B1 product type |
| USB HDD enclosure shows `removable=0` but `usb.removable=removable` | `usb_device/removable` is the port's connect type | never use `usb_device/removable` for the medium |
| Two identical sticks, same `ID_SERIAL` | vendor ships one serial | identity key includes `ID_PATH` + `diskseq` |
| SATA SSD behind a RAID HBA (`Scsi` transport) with `rotational=0` | HBA passes VPD B1 | fine; class SsdSata |
| NVMe behind a USB enclosure (`Usb` transport, UAS, `removable=0`, `rotational=0` sometimes) | bridge is SAT-less; may answer B1 | lands in UsbSsd via B1, else UsbHdd/Low — acceptable, engine re-evaluates |
| `removable=1` on internal SATA hot-swap bays | INQUIRY RMB from some firmware | transport `Ata` path ignores `removable` |
| eMMC on `Mmc` transport with `type=MMC` | | `SdMmc{Emmc}` — flash strategy, no card-reader timeouts |
| Loop device over the user's image file | | `Virtual`; allowed as source only with `--allow-virtual` |

### 3.3 Identity and re-finding after re-enumeration

```
fn identity(base, transport) -> Identity:
    dev = base/"device"
    match transport:
      Nvme: model=trim(read(dev/model)); serial=trim(read(dev/serial)); fw=read(dev/firmware_rev)
            wwid=read(base/"wwid")                       # eui./nguid./uuid.
      Ata | Scsi | Usb:
            vendor=trim(read(dev/vendor)); model=trim(read(dev/model)); fw=trim(read(dev/rev))
            scsi_type=read_u8(dev/type)
            serial = vpd80_serial(read_bin(dev/vpd_pg80))     # §2.7
            wwid   = read(dev/wwid)
            if transport == Usb:
                usbdev = walk_up_until(dev, |p| exists(p/"idVendor"))
                usb = { vid: read(usbdev/idVendor), pid: read(usbdev/idProduct),
                        serial: read(usbdev/serial), manufacturer, product,
                        speed, devpath: read(usbdev/devpath), busnum,
                        driver: basename(readlink(parent_of(usbdev child iface)/"driver")) }
      Mmc:  name=read(dev/name); serial=read(dev/serial); manfid, oemid, cid, csd, type, date
      Virtio: serial = read(dev/serial)
    udev = parse_udev_db(devt)         # may add ID_SERIAL, ID_ATA_ROTATION_RATE_RPM, ID_PATH
    identity.key = first_non_empty([wwid, "{model}_{serial}", udev.ID_SERIAL, "{usb.vid}:{usb.pid}:{usb.serial}", "mmc:{name}_{serial}"])
    identity.path_key = udev.ID_PATH or usb.devpath
```

Re-find (after `remove` uevent or `ENXIO`):

```
fn refind(old: &Identity, timeout) -> Option<DeviceInfo>:
    deadline = now + timeout                           # default 30 s, USB resets take 2–10 s
    loop until deadline:
        wait_uevent(action=add, subsystem=block, devtype=disk) or sleep 500ms
        for d in enumerate():
            if d.identity.key == old.key:                                return Some(d)   # name may differ (sdb -> sdc)
        for d in enumerate():                                            # serial-less sticks
            if old.key_is_weak && d.identity.path_key == old.path_key && d.size_bytes == old.size_bytes
               && d.identity.model == old.identity.model:                 return Some(d)
    None
```

Reject a candidate whose `size_bytes` differs from the original (a different
medium in the same reader) unless the user confirms. Record the new
`diskseq`. Also check `/dev/disk/by-id/<old link>` as a fast path: udev
re-creates it pointing at the new name.

### 3.4 Mount mapping and the output-directory guard

```
fn device_set(src: &DeviceInfo) -> HashSet<(u32,u32)>:
    s = { src.devt } ∪ { p.devt for p in src.partitions }
    # anything stacked on top (LVM/dm-crypt/md/bcache) is also "on the source":
    queue = [src.sysfs] ∪ [p.sysfs for p in src.partitions]
    while let Some(dir) = queue.pop():
        for h in readdir(dir/"holders"):             # e.g. dm-0
            hs = realpath(dir/"holders"/h); s.insert(parse_majmin(read(hs/"dev"))); queue.push(hs)
            for part in partitions_of(hs): s.insert(part.devt)
    return s

fn mounts_for(src) -> Vec<MountHit>:
    set = device_set(src)
    hits = []
    for line in /proc/self/mountinfo:
        (id, parent, majmin, root, mnt, opts, _, fstype, source, superopts) = parse(line)   # §2.8, unescape
        if majmin ∈ set: hits.push(mnt, fstype, opts, by="st_dev")
        else if source starts_with "/dev/":
            if let Ok(st) = stat(realpath(source)) and S_ISBLK(st) and (major(st.st_rdev), minor(st.st_rdev)) ∈ set:
                hits.push(mnt, fstype, opts, by="source")               # btrfs/overlay anonymous st_dev
    for line in /proc/swaps (skip header): if stat(col0).st_rdev ∈ set: hits.push(col0, "swap")
    return hits
```

Guard, run before creating the output directory *and* re-run before the
first write of the mapfile (udisks may automount in between):

```
fn guard_output(out_dir: &Path, src: &DeviceInfo) -> Result<(), Refusal>:
    create_dir_all(out_dir)?
    st = stat(out_dir)                                  # follows symlinks
    set = device_set(src)
    if (major(st.st_dev), minor(st.st_dev)) ∈ set: return Err(Refusal::OnSource{by:"st_dev"})
    # anonymous st_dev (btrfs/overlay/fuse): find the mount owning out_dir and check its source
    m = longest_prefix_mount(/proc/self/mountinfo, out_dir)     # unescape field 5; compare path components
    if m.source starts_with "/dev/" && blk_devt(realpath(m.source)) ∈ set: return Err(Refusal::OnSource{by:"mount source"})
    if m.fstype in {"btrfs"}: for dev in btrfs_devices(m)  (/sys/fs/btrfs/<UUID>/devices/*) if devt ∈ set: Err(..)
    if m.fstype in {"fuse", "fuseblk"} and the fuse source resolves into set: Err(..)   # ntfs-3g mounts of the source
    if statvfs(out_dir).f_bavail * f_frsize < src.size_bytes + mapfile_reserve: warn LowSpace   # not refusal; imaging spec can sparse/skip
    Ok(())
```

`Refusal` is fatal: print the mount point, the device set member that
matched, and the instruction "choose an output directory on a different
disk". If `mounts_for(src)` is non-empty but the output is elsewhere: print
each hit, say that a mounted source may give inconsistent reads and
that the mounting filesystem may *write* (journal replay, atime, fsck) and
recommend `umount` (never do it for the user; ddrescue manual, verbatim:
"Never try to rescue a r/w mounted partition. The resulting copy may be
useless." and "It is best that the device or partition to be rescued is not
mounted at all, not even read-only."). Proceed only with
`--i-know-its-mounted`.

Additional pre-flight: if `udisksd` is running (`/run/udisks2` exists or
`pgrep udisksd`), print one line telling the user that desktop automount can
re-mount the device after a USB reset, with the hint
`gsettings set org.gnome.desktop.media-handling automount false` (GNOME).

### 3.5 Opening

```
fn open_source(devnode) -> OpenedDevice:
    flags = O_RDONLY | O_CLOEXEC | O_NOCTTY | O_NONBLOCK?   # O_NONBLOCK is ignored for block devices; omit
    fd = open(devnode, O_RDONLY | O_CLOEXEC | O_DIRECT)
    mode = Direct
    if fd == Err(EINVAL):                 # O_DIRECT unsupported: regular files on tmpfs < 6.6, some FUSE, 9p, overlay (never for a block device)
        fd = open(devnode, O_RDONLY | O_CLOEXEC); mode = Buffered
        warn("direct I/O unavailable on {devnode}; the kernel page cache may mask errors — use posix_fadvise DONTNEED between passes")
    if fd == Err(EACCES|EPERM): return Err(NeedPrivilege(explain_permissions(devnode)))
    if fd == Err(ENXIO|ENODEV|ENOENT): return Err(Gone)
    if fd == Err(EBUSY): return Err(ExclusivelyHeld)    # defensive only: block/bdev.c never returns EBUSY for a non-exclusive O_RDONLY open
                                                         # (bd_prepare_to_claim runs only with BLK_OPEN_EXCL; bdev_may_open rejects only writers);
                                                         # a mounted or O_EXCL-held disk still opens read-only
    if fd == Err(ENOMEDIUM): return Err(NoMedia)        # sr / card reader without card
    st = fstat(fd)
    if S_ISBLK(st.st_mode):
        size = ioctl(BLKGETSIZE64)
        lbs  = ioctl(BLKSSZGET); pbs = ioctl(BLKPBSZGET); ro = ioctl(BLKROGET); seq = ioctl(BLKGETDISKSEQ).ok()
        if size == 0: return Err(NoMedia)
    elif S_ISREG(st.st_mode):
        size = st.st_size; lbs = 512 (user override); pbs = lbs
        if mode == Direct && kernel >= 6.1: (dio_mem_align, dio_offset_align) = statx(STATX_DIOALIGN)
    elif S_ISCHR(st.st_mode) && target_os = macos: (rdisk) lbs = DKIOCGETBLOCKSIZE; size = count*lbs
    else: return Err(NotABlockDevice)
    align = max(lbs, page_size)                         # buffer alignment; satisfies dma_alignment and 4Kn
    return OpenedDevice { fd, size, lbs, pbs, ro, diskseq: seq, mode, align, devnode, opened_at: now }
```

Rules for `pread` under `Direct`:

- `offset % lbs == 0`, `len % lbs == 0`, `buf as usize % align == 0`;
  otherwise `EINVAL` (block/fops.c `blkdev_dio_invalid`: returns
  `(iocb->ki_pos | iov_iter_count(iter)) & (bdev_logical_block_size(bdev) - 1)`;
  the user-buffer address is checked separately against
  `queue/dma_alignment` by `blkdev_iov_iter_get_pages`). The last partial sector of a device whose
  size is not a multiple of `lbs` cannot exist (sizes are sector multiples);
  for regular-file sources round the final read up to `lbs` and truncate the
  result to `size - offset`.
- Allocate buffers with `std::alloc::alloc(Layout::from_size_align(len, 4096))`
  or `posix_memalign`; reuse them; do not allocate per read.
- A `pread` may return **short** (`n < len`) without error on EOF only;
  with `O_DIRECT` on a block device an error in the middle of a request fails
  the whole request (`EIO`) — the imaging engine bisects (imaging spec).
- `EINTR`/`EAGAIN` → retry the same call.
- Under `Buffered` mode call `posix_fadvise(fd, off, len, POSIX_FADV_DONTNEED)`
  after every failed read and before every retry pass so that the retry
  reaches the medium; also `ioctl(BLKFLSBUF)` is **not** an option (needs
  `CAP_SYS_ADMIN`).
- Keep a single `pread` ≤ `max_sectors_kb` KiB (observed 128 KiB on NVMe,
  commonly 1280 KiB on SATA, 120 KiB on usb-storage) — larger is legal but
  the kernel splits it and one bad sector then fails a bigger range; the
  imaging spec picks 64 KiB/`cluster` anyway.
- Never `mmap` the device.

`explain_permissions(devnode)`:

```
st = stat(devnode)                         # typically brw-rw---- root:disk
me = (geteuid(), getgroups())
if euid == 0: "running as root but open failed: {errno} — SELinux/AppArmor or the device is gone"
elif st.gid is "disk" and me.groups contains st.gid: "you are in group disk; open failed with {errno}"
else: "reading {devnode} needs read permission on the node. Options: run with sudo; or add your user to group '{group}' (log out/in); or `sudo setfacl -m u:$USER:r {devnode}` for this session."
```

Plain `read()` on `/dev/sdX` needs only DAC read permission (group `disk`
on Debian/Ubuntu/Fedora, mode `0660`). `CAP_SYS_RAWIO` is **not** needed for
`read`/`pread`/`BLK*GET*` ioctls; it is needed for SG_IO commands outside
the read-safe list (ATA pass-through, vendor commands) and for
`/dev/sg*`. `CAP_SYS_ADMIN` is needed for `BLKFLSBUF`/`BLKROSET`/`BLKRRPART`
(unused) and for `smartctl`. Tell the user which of the two paths they are on
(`sudo` for everything, or group `disk` for imaging without SMART).

### 3.6 Liveness

```
fn watch(dev: &DeviceInfo) -> LivenessEvents:
    try netlink(group 1) filtered by DEVPATH == dev.sysfs_devpath (and prefix for partitions)
    else poll:
        every 500 ms: if !exists(dev.sysfs) -> Removed
                      if read(dev.sysfs/"device/state") not in {"running", "live", None} -> Offline(state)
                      if read_u64(dev.sysfs/"diskseq") != dev.diskseq -> Replaced
on read error:
    EIO:            media/link error, device still present -> imaging engine retry logic
    0 bytes (short) before size: capacity was set to 0 by del_gendisk -> check sysfs -> Removed
    ENXIO:          open() of a dead/absent disk (block/bdev.c: `if (!disk_live(disk)) ret = -ENXIO`)
    ENODEV:         ioctls routed through the SCSI midlayer (HDIO_*, SG_IO) while the sdev is in error recovery,
                    offline or deleted (scsi_ioctl.c: `if (!scsi_block_when_processing_errors(sdev)) return -ENODEV;`);
                    plain pread() on a dead disk yields 0 bytes (capacity 0) or EIO (bio_io_error), never ENODEV.
                    Treat ENXIO and ENODEV both as Removed; the per-driver survey beyond SCSI/bdev is UNVERIFIED
    ENOMEDIUM:      media ejected (sr, card reader)
    ETIMEDOUT:      not returned by the block layer; timeouts surface as EIO after `device/timeout` × retries
```

After `Removed` → `refind()` → reopen with `open_source()` (new fd; the old
fd is dead even if the same name comes back) → compare `size`, `lbs`,
identity key, then resume (imaging spec resumes from the mapfile).

### 3.7 Windows enumeration/open

```
for n in 0..64:
    path = format!(r"\\.\PhysicalDrive{n}")
    h = CreateFileW(path, 0, SHARE_READ|SHARE_WRITE, null, OPEN_EXISTING, 0, null)
    if h == INVALID: if GetLastError()==2 { continue } else { record error; continue }
    q = STORAGE_PROPERTY_QUERY { PropertyId: StorageDeviceProperty(0), QueryType: 0, .. }
    DeviceIoControl(h, IOCTL_STORAGE_QUERY_PROPERTY, &q, 12, &hdr, 8)  -> hdr.Size
    buf = vec![0u8; hdr.Size]; DeviceIoControl(.. &buf ..); desc = parse STORAGE_DEVICE_DESCRIPTOR
    strings at desc.*Offset (NUL-terminated ASCII, trim)
    seek: q.PropertyId = 7 -> DEVICE_SEEK_PENALTY_DESCRIPTOR (may fail -> None)
    align: q.PropertyId = 6 -> STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR (may fail -> lbs from geometry)
    geo: IOCTL_DISK_GET_DRIVE_GEOMETRY_EX -> DiskSize, BytesPerSector, MediaType
    class = match (desc.BusType, seek, desc.RemovableMedia):
        (17 Nvme, _, _) -> SsdNvme
        (11 Sata | 3 Ata, Some(false), _) -> SsdSata
        (11 Sata | 3 Ata, Some(true) | None, _) -> Hdd
        (12 Sd | 13 Mmc, _, _) -> SdMmc
        (7 Usb, Some(false), false) -> UsbSsd ; (7 Usb, Some(true), false) -> UsbHdd ; (7 Usb, _, true) -> UsbFlash ; (7 Usb, None, false) -> UsbHdd/Low
        (14 Virtual | 15 FileBackedVirtual | 16 Spaces, ..) -> Virtual
        (1 Scsi | 8 RAID | 10 Sas, Some(r), _) -> if r {Hdd} else {SsdSata}
        _ -> Unknown
    CloseHandle(h)
open for imaging: CreateFileW(path, GENERIC_READ, SHARE_READ|SHARE_WRITE, null, OPEN_EXISTING, FILE_FLAG_NO_BUFFERING|FILE_FLAG_SEQUENTIAL_SCAN, null)
    on ERROR_ACCESS_DENIED(5): NeedPrivilege("run as administrator")
output guard: GetVolumePathNameW(out_dir) -> volume root -> CreateFileW(r"\\.\C:", 0, ..) -> IOCTL_STORAGE_GET_DEVICE_NUMBER -> DeviceNumber == n ? Refuse
             (also IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS = 0x00560000 for spanned/striped volumes: any extent DiskNumber == n ? Refuse)
```

---

## 4. Edge cases and failure modes seen on real failing media

1. **Partition scan failed, disk still present.** Kernel logs
   `sdb: unable to read partition table`; `/sys/block/sdb/` has no `sdbN`
   dirs and `size` is correct. Do not infer "unpartitioned"; the partition
   spec reads the table from the image.
2. **Capacity reads as 0 or absurd (e.g. 2 TiB for a 4 GB stick, or the
   famous `size = 2^32` sectors).** Controller firmware in failure mode
   reports a bogus `READ CAPACITY`. Treat `size_bytes > 128 TiB` for USB flash
   or size that disagrees with `udev ID_PART_ENTRY_*` by >2× as suspect;
   let the user override the length (`--size`).
3. **Click-of-death HDD / spindle not spinning.** Device enumerates (INQUIRY
   answers from the PCB), every read times out after `device/timeout`
   (30 s) × 5 retries ≈ 2.5 min per request, then the SCSI EH resets the link
   (`dmesg: ata1: hard resetting link`), after which `device/state` may be
   `offline`. The device layer must expose `device/timeout` so the engine can
   use small requests and detect "all reads time out" quickly; the engine, not
   this layer, decides to stop.
4. **USB bridge resets under errors.** usb-storage/uas issue a port reset
   after command timeouts; the disk disappears (`remove` uevent) and comes
   back with a new name (`sdb` → `sdc`) and a new `diskseq`; `devnum`
   increments; `devpath` (port) unchanged. `refind()` by serial, else by
   `devpath` + size. Reopen; the old fd returns `0` (capacity set to 0 by
`__blk_mark_disk_dead`) or `EIO` forever.
5. **Stale page cache.** Without `O_DIRECT` a retry of a sector that errored
   may be served from the cache if an earlier read of a *larger* range
   succeeded partially; the ddrescue manual recommends `-d`. If `O_DIRECT`
   is unavailable (image file on tmpfs/FUSE), `posix_fadvise(DONTNEED)` and
   smaller reads mitigate.
6. **Advanced Format 4Kn drive (lbs = 4096)** — every offset/length must be a
   4096 multiple; MBR/GPT parsing in later layers must use `lbs`, not 512.
   `size` in sysfs is still in 512-byte units. USB bridges may present a 4Kn
   drive as 512e or 4Kn depending on firmware; the serial from the bridge
   may differ from the drive's own.
7. **SD cards that go read-only or "lock".** Controller enters a permanent RO
   state on wear-out: `ro=1` and `smart` unavailable; reads often still work
   at reduced speed. Some die mid-read: card reader keeps the device but
   every read returns `EIO` within ms; `refind()` is useless — advise the user
   to power-cycle the reader (physically) and resume.
8. **Counterfeit flash (capacity lies).** Reads beyond the real NAND return
   zeros or wrap to earlier sectors without error. Not detectable here;
   flag `UsbFlash` with `manfid 0x000000`/`oemid 0x0000` or vendor strings
   `General`/`USB Flash Disk`/`Mass Storage` as `suspect_counterfeit` for
   the validation layer.
9. **NVMe in read-only/degraded mode.** `critical_warning` bit 3 set,
   `state` may be `live`; reads work but can be slow with `media_errors`
   rising. Controller resets (`resetting` → `live`) keep the name; the fd
   survives (block layer requeues); `BLKGETDISKSEQ` unchanged.
10. **Device is a dm-crypt/LVM member.** The user wants `/dev/sdb` but
    `holders/` shows `dm-0` mounted at `/home`. `mounts_for()` reports it via
    the holders walk; the output guard refuses `/home/...`.
11. **Removed while enumerating.** Any sysfs read can fail with `ENOENT`
    mid-way; treat a missing `size` as "skip device", never panic.
12. **`O_DIRECT` + `fork()`** (open(2)): the SMART helper is spawned before
    opening or via `posix_spawn` from a thread with no outstanding direct
    reads; otherwise private-mapping corruption is possible.
13. **Optical media.** `sr0` has `lbs = 2048`, `size` is in 512-byte units
    (so `size % 4 == 0`), reads of the lead-out fail with `EIO` — normal.
    `BLKGETSIZE64` may report the last session only for multisession.
14. **Zoned/SMR drives** (`queue/zoned = host-managed`): sequential-only
    for writes, unaffected for reads; still report so the engine avoids
    assuming uniform latency.
15. **Hidden/partial devices**: `hidden=1` (multipath members), `md` members
    (`/sys/block/sdb/holders/md0`), `zram`, `nbd` — list for the guard,
    never offer as source unless `--allow-virtual`.
16. **Windows**: a disk with a RAW/damaged volume still has drive letters
    assigned; Windows may pop "You need to format the disk" — the tool must
    not answer it; reads through `\\.\PhysicalDriveN` are unaffected.
    `IncursSeekPenalty` is `TRUE` for most USB bridges regardless of medium.
    After a USB reset the `PhysicalDriveN` number is usually reused but not
    guaranteed; re-find by `SerialNumberOffset` string.

---

## 5. Test fixture strategy (no root, only the listed tools)

Everything in this layer that touches the kernel is behind two traits so the
logic is testable from a tempdir:

```rust
trait SysfsSource { fn read(&self, rel: &str) -> io::Result<Vec<u8>>; fn readdir(&self, rel: &str) -> io::Result<Vec<String>>; fn realpath(&self, rel: &str) -> io::Result<PathBuf>; fn readlink(..) }
trait ProcSource  { fn mountinfo(&self) -> io::Result<String>; fn swaps(&self) -> io::Result<String>; fn udev_db(&self, devt: (u32,u32)) -> io::Result<Option<String>>; fn stat_dev(&self, path: &Path) -> io::Result<(u32,u32)>; }
```

The real implementations wrap `/sys`, `/proc`, `/run/udev/data`; the test
implementation is a directory tree under `tests/fixtures/sysfs/<case>/` with
symlinks, created by a script. Fixtures are *recorded* from real machines
(a `disk_rescue dump-sysfs <dev>` subcommand writes exactly the files the
parser reads) and hand-written for pathological cases.

### 5.1 Fake sysfs trees (shell, no root)

`tests/fixtures/mk_sysfs_usb_stick.sh`:

```sh
#!/bin/sh
set -eu
R=${1:?root}
D=$R/sys/devices/pci0000:00/0000:00:14.0/usb1/1-3
I=$D/1-3:1.0
S=$I/host6/target6:0:0/6:0:0:0
B=$S/block/sdb
mkdir -p "$B/sdb1/queue" "$B/queue" "$B/holders" "$B/slaves" "$R/sys/block" "$R/sys/class/block" "$R/sys/dev/block" "$R/run/udev/data" "$R/sys/bus/usb/drivers/usb-storage"
# usb_device
printf '0781\n' > "$D/idVendor"; printf '5567\n' > "$D/idProduct"; printf '4C530001231012345678\n' > "$D/serial"
printf 'SanDisk\n' > "$D/manufacturer"; printf 'Cruzer Blade\n' > "$D/product"; printf '480\n' > "$D/speed"
printf '3\n' > "$D/devpath"; printf '1\n' > "$D/busnum"; printf '7\n' > "$D/devnum"; printf 'removable\n' > "$D/removable"
printf 'MAJOR=189\nMINOR=6\nDEVNAME=bus/usb/001/007\nDEVTYPE=usb_device\nDRIVER=usb\nPRODUCT=781/5567/126\nTYPE=0/0/0\nBUSNUM=001\nDEVNUM=007\n' > "$D/uevent"
# usb_interface
printf '08\n' > "$I/bInterfaceClass"; printf '06\n' > "$I/bInterfaceSubClass"; printf '50\n' > "$I/bInterfaceProtocol"
printf 'DEVTYPE=usb_interface\nDRIVER=usb-storage\nINTERFACE=8/6/80\n' > "$I/uevent"
ln -s ../../../../../../bus/usb/drivers/usb-storage "$I/driver"
# scsi_device
printf 'SanDisk \n' > "$S/vendor"; printf "Cruzer Blade    \n" > "$S/model"; printf '1.26\n' > "$S/rev"
printf '0\n' > "$S/type"; printf 'running\n' > "$S/state"; printf '30\n' > "$S/timeout"; printf '0x0\n' > "$S/ioerr_cnt"
printf 'DEVTYPE=scsi_device\nDRIVER=sd\n' > "$S/uevent"
printf '\000\200\000\0244C530001231012345678' > "$S/vpd_pg80"      # type 0, page 0x80, len 20, serial
# gendisk
printf '120176640\n' > "$B/size"; printf '1\n' > "$B/removable"; printf '0\n' > "$B/ro"; printf '0\n' > "$B/hidden"
printf '8:16\n' > "$B/dev"; printf '42\n' > "$B/diskseq"
printf '1\n' > "$B/queue/rotational"; printf '512\n' > "$B/queue/logical_block_size"; printf '512\n' > "$B/queue/physical_block_size"
printf '120\n' > "$B/queue/max_sectors_kb"; printf '511\n' > "$B/queue/dma_alignment"; printf 'none\n' > "$B/queue/zoned"
printf 'MAJOR=8\nMINOR=16\nDEVNAME=sdb\nDEVTYPE=disk\nDISKSEQ=42\n' > "$B/uevent"
ln -s ../../../6:0:0:0 "$B/device"
# partition
printf '1\n' > "$B/sdb1/partition"; printf '2048\n' > "$B/sdb1/start"; printf '120174592\n' > "$B/sdb1/size"; printf '8:17\n' > "$B/sdb1/dev"
# class/block, block, dev/block symlinks (relative, like the kernel)
ln -s "../devices/pci0000:00/0000:00:14.0/usb1/1-3/1-3:1.0/host6/target6:0:0/6:0:0:0/block/sdb" "$R/sys/block/sdb"
ln -s "../../devices/pci0000:00/0000:00:14.0/usb1/1-3/1-3:1.0/host6/target6:0:0/6:0:0:0/block/sdb" "$R/sys/class/block/sdb"
ln -s "../../devices/pci0000:00/0000:00:14.0/usb1/1-3/1-3:1.0/host6/target6:0:0/6:0:0:0/block/sdb/sdb1" "$R/sys/class/block/sdb1"
ln -s "../../devices/pci0000:00/0000:00:14.0/usb1/1-3/1-3:1.0/host6/target6:0:0/6:0:0:0/block/sdb" "$R/sys/dev/block/8:16"
ln -s "../../devices/pci0000:00/0000:00:14.0/usb1/1-3/1-3:1.0/host6/target6:0:0/6:0:0:0/block/sdb/sdb1" "$R/sys/dev/block/8:17"
# udev db
cat > "$R/run/udev/data/b8:16" <<'EOF'
S:disk/by-id/usb-SanDisk_Cruzer_Blade_4C530001231012345678-0:0
S:disk/by-path/pci-0000:00:14.0-usb-0:3:1.0-scsi-0:0:0:0
I:123456789
E:ID_VENDOR=SanDisk
E:ID_MODEL=Cruzer_Blade
E:ID_SERIAL=SanDisk_Cruzer_Blade_4C530001231012345678-0:0
E:ID_SERIAL_SHORT=4C530001231012345678
E:ID_BUS=usb
E:ID_USB_DRIVER=usb-storage
E:ID_USB_VENDOR_ID=0781
E:ID_USB_MODEL_ID=5567
E:ID_TYPE=disk
E:ID_PATH=pci-0000:00:14.0-usb-0:3:1.0-scsi-0:0:0:0
E:ID_PART_TABLE_TYPE=dos
G:systemd
Q:systemd
V:1
EOF
```

Variants (same script with parameters, one per test case):

| Case | Changes | Expected class / behaviour |
|---|---|---|
| `usb_stick` | above | `UsbFlash`, Medium; serial `4C53…`; by-id link resolved |
| `usb_sata_ssd_bot` | `removable=0`, size 1 TB, no `vpd_pgb1`, `rotational=1`, udev has `E:ID_ATA_ROTATION_RATE_RPM=0` | `UsbSsd`, High via udev |
| `usb_sata_ssd_nosat` | as above but no `ID_ATA_*` | `UsbHdd`, Low (documented misreport) |
| `usb_hdd_uas` | driver `uas`, `bInterfaceProtocol 62`, `vpd_pgb1` bytes `00 B1 00 3C 1C 20 …` (rate 7200) | `UsbHdd`, High |
| `usb_sd_reader_nocard` | VID `05e3`, product `USB Storage`, `size=0`, `removable=1` | `SdMmc{via_usb_reader}`, `media_present=false` |
| `usb_sd_reader_card` | same + `size=62333952`, vpd_pgb1 byte 6 = `05` | `SdMmc{via_usb_reader}`, High |
| `sata_hdd` | realpath `…/ata1/host0/target0:0:0/0:0:0:0/block/sda`, vendor `ATA     `, `vpd_pgb1` rate 5400, udev `ID_BUS=ata`, `ID_SERIAL=WDC_WD10EZEX_WD-WCC6Y…` | `Hdd`, High |
| `sata_ssd` | vpd_pgb1 rate `00 01`, `rotational=0` | `SsdSata`, High |
| `nvme` | recorded from the dev box (`/sys/block/nvme0n1`, padded model/serial) | `SsdNvme`; model trimmed; wwid `eui.…` |
| `nvme_multipath` | realpath under `/sys/devices/virtual/nvme-subsystem/nvme-subsys0/`, `device/` lacks `transport`, child symlink `nvme0 -> ../../../pci…/nvme/nvme0` with `transport=pcie`; sibling hidden `nvme0c0n1` (`hidden=1`) | `SsdNvme`, not `Virtual`; transport found via child; hidden sibling skipped |
| `mmc_sd` | `…/mmc_host/mmc0/mmc0:aaaa/block/mmcblk0`, `device/type=SD`, `csd` with bits 127:126 = 1 | `SdMmc{Sd, Sdhc_or_Sdxc}` |
| `mmc_emmc` | `type=MMC`, siblings `mmcblk0boot0`, `mmcblk0rpmb` | `SdMmc{Emmc}`; boot/rpmb not candidates |
| `optical` | `sr0`, `device/type=5`, `lbs=2048` | `Optical` |
| `loop`, `dm_on_usb` | `/sys/devices/virtual/block/loop0` (+`loop/backing_file`); `dm-0` with `slaves/sdb1`, `sdb1/holders/dm-0` | `Virtual`; `device_set(sdb)` includes `dm-0`'s devt |
| `vanish_midway` | `size` file deleted after enumeration starts (test deletes between two `read` calls through a mock) | device skipped, no panic |
| `no_udev` | `/run/udev/data` absent | identity falls back to sysfs |

Record the real NVMe fixture with
`find /sys/block/nvme0n1/ -maxdepth 2 -type f -readable -size -4k | xargs -I{} sh -c 'mkdir -p "$R$(dirname {})"; cat {} > "$R{}" 2>/dev/null'`
plus `cp -r --preserve=links` of the symlinks (`/sys/block/nvme0n1`,
`device`), done once by the `dump-sysfs` subcommand (it skips `trace/`,
`power/`, `mq/`, binary files > 4 KiB).

### 5.2 mountinfo / swaps / stat fixtures

Text files in `tests/fixtures/proc/`; the test `ProcSource` returns them and
maps `stat_dev(path)` from a hard-coded table
(`"/mnt/out" → (8,17)`, `"/home/user/out" → (0,45)` with mountinfo line
`… 0:45 / /home rw … - btrfs /dev/sdb1 …`, `"/tmp/out" → (0,28)` tmpfs).
Cases: output on the source partition (refuse by st_dev), output on btrfs on
the source (refuse by source), output on tmpfs (allow + low-space warning
when `statvfs` mock says 1 GiB), output on LVM over the source
(`holders` walk → refuse), source mounted elsewhere (warn), swap on source
(warn), escaped mount point `/mnt/my\040disk` (decoded and matched).

### 5.3 Opening and reading without root

- Build a 64 MiB image with `mkfs.vfat -F 32 -C img.fat 65536` (no root),
  open it through `open_source()`: `S_ISREG` branch, `BLK*` ioctls return
  `ENOTTY` (assert the fallback), `O_DIRECT` works on ext4/xfs (the project
  tmp dir), and on `tmpfs` either works (kernel ≥ 6.6) or fails with
  `EINVAL` → buffered fallback (test both by placing the image under
  `/dev/shm` when it exists; assert *a* valid mode is chosen, not which).
  tmpfs gained `O_DIRECT` in Linux 6.6 (commit e88e0d366f9c "tmpfs: trivial
  support for direct IO", Hugh Dickins, 2023-08-11); older kernels return
  `EINVAL` from `open(2)`.
- Alignment unit tests: `pread_aligned(off=511, len=512)` on the `Direct`
  mode must be rejected by our own precheck with `InvalidAlignment` before
  reaching the kernel; `len` not multiple of `lbs` likewise.
- Short-read-at-EOF test: image of 1 MiB + 300 bytes (`truncate -s
  1048876`), direct read of the last cluster returns 300 bytes after
  rounding — assert truncation logic.
- Error-injection: a `FakeFd` implementing the read trait that returns `EIO`
  for sectors in a set, `0` after a "removal" flag, `EINTR` once; verify the
  liveness state machine (§3.6) emits `Removed` on `0 bytes before size`.
- Netlink parser: feed recorded datagrams (bytes with embedded NULs, as
  Rust byte-string literals captured once with `udevadm monitor -k -p` and
  re-encoded) — `remove@/devices/…/block/sdb\0ACTION=remove\0…`; assert
  action/devpath/DEVNAME/DISKSEQ extraction and that a datagram with a
  `libudev` prefix (group 2) is parsed via its header offsets.
- udev db parser: the heredoc above plus a file with `L:`, `W:` and unknown
  prefixes (must be ignored), and a `E:` value containing `=`.
- VPD 0x80/0xB1 and CSD decoders: byte-array unit tests from the tables in
  §2.7 (rate 1, rate 7200, page too short → `None`, product type 5).
- Identity key stability: `refind()` test with two enumerations where `sdb`
  became `sdc` with the same `vpd_pg80` serial and a new `diskseq`; and the
  serial-less twin-stick case matching by `devpath` + size.
- Windows: pure-Rust parsers for `STORAGE_DEVICE_DESCRIPTOR` byte blobs
  (hand-built: offsets pointing inside the blob, offsets = 0, swapped ATA
  serial) run on Linux CI; the `DeviceIoControl` calls are behind a trait and
  only compiled/exercised on `cfg(windows)`.

No test needs `losetup`, `mount`, or `/dev/sdX`. An opt-in integration test
(`DISK_RESCUE_REAL_DEVICE=/dev/…`, skipped by default) runs
`enumerate()`+`open_source()`+one aligned read on a real node for manual QA.

---

## 6. Recommended Rust crates

Versions are `max_stable_version` from crates.io on 2026-10-12.

| Crate | Version | Use | Notes |
|---|---|---|---|
| `rustix` | 1.1.5 | `open` with `OFlags::DIRECT|CLOEXEC`, `pread`, `fstat`, `statx`, `ioctl` helpers (`rustix::ioctl::{Getter, NoArg}`), `posix_fadvise`, netlink socket (`rustix::net`) | pure Rust, no libc on Linux (`use-libc` optional). Preferred over `nix`. |
| `libc` | 0.2.190 | `NETLINK_KOBJECT_UEVENT` (= 15), `sockaddr_nl`, `ENOMEDIUM`, `POSIX_FADV_DONTNEED`, `BLKSSZGET`/`BLKPBSZGET`/`BLKIOMIN`/`BLKIOOPT` | Checked in the crate source (`src/unix/linux_like/linux/arch/generic/mod.rs`): on Linux it exports **only** `BLKSSZGET 0x1268`, `BLKPBSZGET 0x127B`, `BLKIOMIN 0x1278`, `BLKIOOPT 0x1279`. **Not exported**: `BLKGETSIZE64`, `BLKGETDISKSEQ`, `BLKROGET`, `BLKALIGNOFF`, `BLKSECTGET`, `HDIO_GET_IDENTITY`, `HDIO_GETGEO`, and the Apple `DKIOC*` — define them locally from §2.9/§2.13 (x86_64 values; MIPS/PowerPC/SPARC use the `0x2000….` encoding). `major()`/`minor()` helpers: use `rustix::fs::{major, minor}` or glibc's `gnu_dev_major` semantics. |
| `nix` | 0.31.3 | alternative to rustix (`nix::sys::stat`, `nix::fcntl::posix_fadvise`, `ioctl_read!` macros) | use only one of nix/rustix; rustix chosen |
| `udev` | 0.9.3 | libudev bindings (`Enumerator`, `MonitorBuilder` for group 2 events) | **optional feature** `udev-native`; default path parses `/run/udev/data` and raw netlink in-process to avoid the C dependency and to work on musl static builds |
| `serde` + `serde_json` | 1.0.x / 1.0.154 | `smartctl -j`, `lsblk -J` fallback, `DeviceInfo` serialisation for the report | |
| `memmap2` | 0.9.11 | **not** for devices; only for the mapfile/report | |
| `windows-sys` | 0.61.2 | all Win32 calls in §2.12 | features listed in §2.12 |
| `plist` | 1.10.1 | `diskutil info -plist` on macOS | |
| `io-uring` | 0.7.15 | optional future: batched `pread` with per-request timeouts (`IORING_OP_READ` + `IORING_OP_LINK_TIMEOUT`) | not needed for v1 |
| `thiserror` | 2.0.21 | error enums (`OpenError::NeedPrivilege`, `Refusal`) | |
| `tempfile` (dev) | 3.27.0 | fixture trees | |

Avoid `sysinfo`/`block-utils`: they hide the exact attribute reads this spec
depends on and do not expose VPD or udev data.

---

## 7. Sources

Primary (fetched or inspected 2026-10-12):

- Linux `Documentation/ABI/stable/sysfs-block` — https://www.kernel.org/doc/Documentation/ABI/stable/sysfs-block
- Linux `Documentation/ABI/testing/sysfs-bus-usb` — https://www.kernel.org/doc/Documentation/ABI/testing/sysfs-bus-usb
- Linux `Documentation/driver-api/mmc/mmc-dev-attrs` — https://www.kernel.org/doc/html/latest/driver-api/mmc/mmc-dev-attrs.html
- Linux `drivers/scsi/sd.c` (VPD B1 rotation, zoned, WP) — https://raw.githubusercontent.com/torvalds/linux/master/drivers/scsi/sd.c
- Linux `drivers/scsi/scsi_sysfs.c` (scsi_device attributes, formats, states) — https://raw.githubusercontent.com/torvalds/linux/master/drivers/scsi/scsi_sysfs.c
- Linux `drivers/scsi/scsi_ioctl.c` (`scsi_cmd_allowed`, read-safe command list) — https://raw.githubusercontent.com/torvalds/linux/master/drivers/scsi/scsi_ioctl.c
- Linux `drivers/usb/core/sysfs.c` (usb_device vs usb_interface attributes, `%04x`) — https://raw.githubusercontent.com/torvalds/linux/master/drivers/usb/core/sysfs.c
- Linux `drivers/mmc/core/bus.c` (`type` strings) and `drivers/mmc/core/sd.c` (attribute formats, CID/CSD bit fields) — https://raw.githubusercontent.com/torvalds/linux/master/drivers/mmc/core/bus.c , https://raw.githubusercontent.com/torvalds/linux/master/drivers/mmc/core/sd.c
- Linux `block/ioctl.c` (capability checks, `BLKGETSIZE64` = `bdev_nr_bytes`) — https://raw.githubusercontent.com/torvalds/linux/master/block/ioctl.c
- Linux `block/fops.c` (`blkdev_read_iter` EOF, `blkdev_dio_invalid` → `EINVAL`) — https://raw.githubusercontent.com/torvalds/linux/master/block/fops.c
- Linux `block/bdev.c` (`ENXIO` for dead/absent disks) — https://raw.githubusercontent.com/torvalds/linux/master/block/bdev.c
- Linux `block/genhd.c` (`del_gendisk`: `GD_DEAD`, `set_capacity(0)`, `removable`/`hidden`/`ro` show) — https://raw.githubusercontent.com/torvalds/linux/master/block/genhd.c
- Linux `lib/kobject_uevent.c` (uevent datagram format, group 1) — https://raw.githubusercontent.com/torvalds/linux/master/lib/kobject_uevent.c
- `/usr/include/linux/fs.h`, `/usr/include/linux/hdreg.h`, `/usr/include/linux/netlink.h`, `/usr/include/linux/capability.h`, `/usr/include/scsi/sg.h` on the dev box (ioctl numbers, `NETLINK_KOBJECT_UEVENT=15`, `CAP_SYS_RAWIO=17`, `CAP_SYS_ADMIN=21`, `SG_IO=0x2285`)
- open(2) man page, O_DIRECT section (alignment, `EINVAL`, fork warning); proc_pid_mountinfo(5); capabilities(7) — local man-pages
- systemd `rules.d/60-persistent-storage.rules` (local copy at `/usr/lib/udev/rules.d/`), `src/libsystemd/sd-device/device-private.c` (db format) — https://raw.githubusercontent.com/systemd/systemd/main/src/libsystemd/sd-device/device-private.c , `device-monitor-private.h` (libudev header) — https://raw.githubusercontent.com/systemd/systemd/main/src/libsystemd/sd-device/device-monitor-private.h , `src/udev/udev-builtin-usb_id.c` — https://raw.githubusercontent.com/systemd/systemd/main/src/udev/udev-builtin-usb_id.c , `src/udev/ata_id/ata_id.c` — https://raw.githubusercontent.com/systemd/systemd/main/src/udev/ata_id/ata_id.c
- Dev-box observations: `/sys/block/nvme0n1/*`, `/run/udev/data/b259:0`, `/run/udev/data/b259:8`, `/dev/disk/by-id/`, `/proc/self/mountinfo`, `udevadm info`, `lsblk -J`, `/sys/bus/usb/devices/*`
- GNU ddrescue manual (direct access, sector size, mounted-source warning) — https://www.gnu.org/software/ddrescue/manual/ddrescue_manual.html
- Microsoft: `STORAGE_BUS_TYPE` — https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ne-winioctl-storage_bus_type ; `STORAGE_DEVICE_DESCRIPTOR` — https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-storage_device_descriptor ; `STORAGE_PROPERTY_ID` — https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ne-winioctl-storage_property_id ; `STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR` — https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-storage_access_alignment_descriptor ; `DEVICE_SEEK_PENALTY_DESCRIPTOR` — https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-device_seek_penalty_descriptor ; `DISK_GEOMETRY_EX` — https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-disk_geometry_ex ; `IOCTL_DISK_GET_LENGTH_INFO` — https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-ioctl_disk_get_length_info ; File Buffering (NO_BUFFERING alignment) — https://learn.microsoft.com/en-us/windows/win32/fileio/file-buffering
- docs.rs `windows-sys` 0.61.2: `IOCTL_STORAGE_QUERY_PROPERTY` = 2954240, `IOCTL_DISK_GET_LENGTH_INFO` = 475228, `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` = 458912, `IOCTL_STORAGE_GET_DEVICE_NUMBER` = 2953344; `Win32::Storage::FileSystem` constants — https://docs.rs/windows-sys/latest/windows_sys/Win32/System/Ioctl/index.html , https://docs.rs/windows-sys/latest/windows_sys/Win32/Storage/FileSystem/index.html
- smartctl JSON key paths as consumed by Proxmox (`pve-storage` patch) — https://lists.proxmox.com/pipermail/pve-devel/2021-May/048155.html
- crates.io API for crate versions (nix, rustix, libc, udev, windows-sys, plist, serde_json, io-uring, memmap2)

Verified in the 2026-10-12 fact-check pass (sources used, all primary unless noted):

- Linux `drivers/ata/libata-scsi.c` (`ata_sas_scsi_ioctl` HDIO_GET_IDENTITY without capability check, `ata_scsiop_inq_80`/`_b1`) — https://raw.githubusercontent.com/torvalds/linux/master/drivers/ata/libata-scsi.c
- Linux `drivers/scsi/scsi_ioctl.c` re-read: `scsi_cmd_allowed()` has no ATA_12/ATA_16 in either list; `-ENODEV` from `scsi_block_when_processing_errors`; `-EINVAL` when `hostt->ioctl` is NULL; `sg_io` → `-EPERM`
- Linux `drivers/nvme/host/core.c`, `multipath.c`, `sysfs.c` (subsystem link `sysfs_create_link(&subsys->dev.kobj, …)`, head parent `subsys->dev`, CMIC gate, `multipath_always_on`, `nvme%dc%dn%d` hidden path disks, state/cntrltype/dctype strings, trimmed `%.*s` model/serial) — https://raw.githubusercontent.com/torvalds/linux/master/drivers/nvme/host/{core.c,multipath.c,sysfs.c}
- Linux `include/linux/kobject.h` (`UEVENT_BUFFER_SIZE 2048`, `UEVENT_NUM_ENVP 64`, `kobject_action`) and `lib/kobject_uevent.c` (`NL_CFG_F_NONROOT_RECV`, group 1)
- Linux `Documentation/ABI/testing/sysfs-devices-removable` (`removable`/`fixed`/`unknown`, driver-core attribute since 5.14) and `drivers/usb/core/hub.c` `set_usb_port_removable()`
- Linux `drivers/mmc/core/sd.c` (`mmc_decode_cid` bit offsets, `csd_struct = unstuff_bits(resp,126,2)`, `MMC_DEV_ATTR` formats) and `bus.c` `type_show`
- systemd `src/libsystemd/sd-device/sd-device.c` `handle_db_line` (`S L E G Q I V`; `W` deprecated) — https://raw.githubusercontent.com/systemd/systemd/main/src/libsystemd/sd-device/sd-device.c
- systemd `src/udev/udev-builtin-usb_id.c` (legacy `ID_BUS`/`ID_SERIAL` only when `ID_BUS` unset; serial sanitising `< 0x20`, `> 0x7f`, `,`) and `src/udev/ata_id/ata_id.c` (word 217 mapping, SG_IO `0xa1`/`0x85` then `HDIO_GET_IDENTITY`)
- Local `/lib/udev/rules.d/60-persistent-storage.rules` (systemd 259): ata_id-on-USB rule, unconditional `usb_id` import, `$env{ID_BUS}-$env{ID_SERIAL}` and `usb-$env{ID_USB_SERIAL}` links, nvme `ID_SERIAL` with `_NSID`, `by-diskseq`
- sg3_utils `src/sg_vpd_common.c` `decode_block_dev_ch_vpd()` (reference decoder for SBC-4 VPD B1: rotation-rate ranges, product type, form factor, byte 8 bits) — https://raw.githubusercontent.com/hreinecke/sg3_utils/master/src/sg_vpd_common.c
- smartctl(8) RETURN VALUES (smartmontools 7.4, Debian trixie man page) — https://manpages.debian.org/trixie/smartmontools/smartctl.8.en.html
- Linux 6.6 commit e88e0d366f9c "tmpfs: trivial support for direct IO"
- Microsoft Learn CreateFileA, "Physical Disks and Volumes" (administrative privileges, OPEN_EXISTING, `dwDesiredAccess = 0`) — https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilea ; STORAGE_BUS_TYPE / STORAGE_PROPERTY_ID / STORAGE_DEVICE_DESCRIPTOR pages re-read
- windows-sys 0.61.2 crate source (`~/.cargo/registry`): `IOCTL_DISK_GET_DRIVE_LAYOUT_EX = 458832`, `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS = 5636096`, `GUID_DEVINTERFACE_DISK`, `BusTypeMax = 20` (no `BusTypeNvmeof`), `BOOLEAN` → `bool`, module locations
- libc 0.2.190 crate source: Linux `BLK*` exports limited to `BLKSSZGET/BLKPBSZGET/BLKIOMIN/BLKIOOPT`; no `DKIOC*`
- Apple xnu `bsd/sys/disk.h` (`DKIOCGETBLOCKSIZE _IOR('d',24,uint32_t)`, `DKIOCGETBLOCKCOUNT _IOR('d',25,uint64_t)`) — https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/bsd/sys/disk.h
- crates.io API: `thiserror` 2.0.21, `tempfile` 3.27.0 (others re-confirmed)

Secondary / recalled (still tagged UNVERIFIED in text):

- macOS `diskutil info -plist` key spellings — https://dev.to/kishanbsh/getting-hard-disk-info-in-mac-using-terminal-2fn5 (human-readable output only); `DeviceIdentifier`, `DeviceNode`, `WholeDisk`, `ParentWholeDisk`, `Content`, `MountPoint`, `FilesystemName`, `FilesystemType`, `DiskUUID`, `VolumeName`, `VolumeUUID` and the `diskutil list -plist` keys `AllDisks`, `WholeDisks`, `VolumesFromDisks`, `AllDisksAndPartitions` confirmed from a plist-tagged Go consumer (https://pkg.go.dev/github.com/brokeyourbike/mountefi/disk); the remaining keys are unconfirmed
- Windows byte-swapped ATA serial un-swapping — a heuristic, not a documented behaviour
- Which non-SCSI drivers return `ENODEV` from read()/ioctl on a hot-removed device
