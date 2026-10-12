# Using DiskRescue

DiskRescue works in two steps that you can also run together:

1. **Image** the failing device into a file, reading the healthy areas first
   and coming back for the damaged ones. This is the only step that touches
   the device, and it can be stopped and resumed at any time.
2. **Recover** photos and videos from the image (or directly from a device
   that is healthy enough): by name through the filesystem, by carving the
   raw space, then validate and repair what came back.

Damaged files you already have (a video that will not play, a photo that
shows half grey) go straight to `diskrescue repair`.

## Before you start

- **Stop using the device.** Every write, including just plugging a card
  into a camera or phone, can overwrite deleted photos.
- **Do not run** `chkdsk`, `fsck`, "Repair disk" or "Format" on it, even if
  the system offers to. They rewrite the very structures recovery needs.
- **Use a different disk for the output**, with at least as much free space
  as the device (for an image) or as the files you expect back.
- **Run as root** (`sudo diskrescue ...`) or as a member of the `disk` group
  to read raw devices. Image files need no special rights.

## Three ways to use it

- **Desktop app** (`diskrescue-gui`): choose the drive, press **Rescue**,
  answer three questions (what to recover, where to save, review), and watch
  the copy and the recovery. When the drive needs administrator rights, choose
  **Copy with administrator rights** and enter your password. The Results page
  shows your photos and videos as thumbnails, with filters for damaged,
  repaired and deleted files and a **Repair with a reference file** button.
  **Repair photos & videos** fixes files you already have: drag them onto the
  window.
- **Terminal app** (`sudo diskrescue-tui`): the same flow with the keyboard.
  Arrow keys move, Enter chooses, `?` shows the keys for the current screen,
  `q` quits. While a drive is being copied, `p` pauses and `s` stops and saves
  (run the same rescue again to continue).
- **Command line** (`diskrescue`): described below.

## Commands

```
diskrescue list                         show disks, cards and USB drives
diskrescue info SOURCE                  device details, partitions, filesystems
diskrescue image SOURCE IMAGE           careful, resumable copy of a failing device
diskrescue scan SOURCE                  list what can be recovered, write nothing
diskrescue recover SOURCE -o DIR        recover photos and videos
diskrescue carve SOURCE -o DIR          carving only (ignore filesystems)
diskrescue repair FILE... -o DIR        repair damaged photos and videos
diskrescue verify FILE...               check whether files are intact (exit 1 if any is damaged)
diskrescue map MAPFILE                  show the progress map of an image
```

`SOURCE` is a whole device (`/dev/sdb`, `/dev/mmcblk0`, `/dev/nvme1n1`,
`\\.\PhysicalDrive2` on Windows), a partition (`/dev/sdb1`) or an image file.
When the image has a mapfile next to it (`card.img.map`), DiskRescue uses it
automatically to know which areas could not be read.

### `image`

```
diskrescue image /dev/sdb card.img
```

The mapfile defaults to `IMAGE.map` and uses GNU ddrescue's format. Run the
same command again to resume. The read strategy is chosen from the device
type; override it with `--profile`:

| Profile | Used for | What it does |
|---|---|---|
| `hdd` | internal hard disks | 64 KiB reads, exponential skipping over bad areas, second pass backwards, sector-level scraping last |
| `usb-hdd` | hard disks in USB enclosures | as `hdd`, plus re-opening the device when the USB bridge stalls |
| `ssd`, `nvme` | SATA and NVMe SSDs | large reads, short timeouts, no trimming (flash fails in whole pages), more retry passes |
| `usb-ssd` | SSDs in USB enclosures | as `ssd`, with bridge handling |
| `usb-flash` | pen drives | small reads, 10 s timeouts, re-opens and waits for the drive to come back after it drops off the bus, re-checks data after errors |
| `sd` | SD and microSD cards | as `usb-flash` with 32 KiB reads |
| `generic` | anything else | GNU ddrescue's defaults |

Useful options: `--timeout SECS`, `--block-size SIZE`, `--retries N`,
`--no-trim`, `--no-scrape`, `--reverse`, `--start OFFSET --size LEN` (image
part of a device), `--no-tui` (plain progress lines), `--json-progress`,
`--map MAPFILE` (keep the progress map elsewhere).

DiskRescue records which device it was copying (serial number, model, size,
first sector) next to the mapfile. Resuming onto a different device is
refused, so a second card plugged into the same reader can never be mixed
into the image. It also refuses to overwrite an existing image that has no
mapfile. `--force` overrides both, when you are sure.

In the full-screen view, `q` stops and saves (resume later), `p` pauses.

### `recover`

```
diskrescue recover card.img -o ~/Recovered
diskrescue recover /dev/sdb --image-to card.img -o ~/Recovered
```

With `--image-to`, the device is imaged first and recovery runs on the
image. Without it, DiskRescue reads the device directly, which is fine for a
healthy device (for example after accidental deletion) but not for a failing
one.

What to recover: `--photos`, `--videos`, `--raw`, `--audio`, `--all-files`
(default: photos, RAW and videos); `--existing-only` or `--deleted-only`;
`--min-size 20k` to skip thumbnails.

Carving: `--carve auto` (default: free space plus anything the filesystem
does not cover, or everything when no filesystem is readable), `free`,
`all`, `off`.

Other options: `--map MAPFILE` (the mapfile of an image made elsewhere, for
example by GNU ddrescue; `IMAGE.map` is found automatically), `-j N` (worker
threads), `--json` (one JSON event per line, for scripts and front-ends).

Repair: on by default. `--donor FILE` adds a healthy file from the same
camera or phone as a reference (several allowed); `--no-repair` turns it off;
`--aggressive` allows repairs that cut damaged parts out.

Output:

```
~/Recovered/
  files/vol1_fat32_CANON/DCIM/100CANON/IMG_0001.JPG   existing files, original paths
  deleted/vol1_fat32_CANON/DCIM/100CANON/IMG_0002.JPG deleted files found through the filesystem
  carved/photos/2024/05/f00002c41000_20240501_123011.jpg
  carved/videos/undated/f000031a0000.mp4
  repaired/...                                        repaired copies (damaged originals are kept)
  report.txt, report.json
```

`report.txt` lists every file that needs attention: damaged, repaired,
partially repaired, or containing unreadable sectors.

### `repair`

```
diskrescue repair broken.mp4 -o fixed --donor good_clip_same_phone.mp4
diskrescue repair IMG_0042.JPG IMG_0043.JPG -o fixed
```

| Problem | What DiskRescue does |
|---|---|
| Video says "moov atom not found" or will not open (camera or phone died while recording) | Rebuilds the index from a healthy reference clip recorded with the same device and settings (`--donor`) |
| Video stops early or skips | Clips the index to the data that is actually present |
| JPEG cut off, bottom grey | Keeps every decodable row and closes the file properly |
| JPEG will not open, header destroyed | Transplants the header from a healthy photo of the same camera and size (`--donor`) |
| JPEG without Huffman tables (from Motion-JPEG) | Inserts the standard tables |
| PNG "CRC error" or truncated | Fixes checksums, keeps every recoverable row |
| AVI that will not seek or play | Rebuilds the index |

Inputs are never modified; repaired copies go to `DIR/repaired/`.

## Situations

**SD card from a camera says "card error" or the computer asks to format
it.** The filesystem is damaged but the photos are usually intact. Do not
format. `diskrescue image /dev/mmcblk0 card.img` (or `/dev/sdX` in a USB
reader), then `diskrescue recover card.img -o out`. Carving finds the photos
even when the filesystem cannot be read.

**Pen drive disconnects, freezes or is very slow.** Use `image`; the
`usb-flash` profile re-opens the drive after it drops off the bus and waits
for it to reappear, recognising it by serial number. Try a different port or
cable if it keeps disconnecting. A drive that shows up with a capacity of 0
bytes or a few MB has a failed controller; software cannot fix that, but a
data recovery lab often can.

**Hard disk clicking, very slow, or with read errors.** Image it once,
carefully, and stop if the noise gets worse: each hour of running a
mechanically failing disk can cost data. The `hdd` profile reads everything
healthy first. Never open the disk. If it clicks continuously and reads
nothing, power it off and contact a lab.

**SSD.** Deleted files on an SSD are usually erased by TRIM within seconds
or minutes; recovering them rarely works. Files that still exist on a
failing SSD can be recovered: image it with the `ssd` or `nvme` profile and
do not power-cycle a failing SSD more than necessary.

**Photos deleted by accident.** Stop using the device, then `diskrescue
recover /dev/sdX -o out` (direct reading is fine on a healthy device). Look
in `out/deleted/` first, then `out/carved/`.

**Video from a phone or action camera will not play.** `diskrescue repair
broken.mp4 -o fixed --donor any_good_clip_from_same_device.mp4`. The donor
must come from the same device with the same resolution and frame rate.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | finished |
| 1 | error |
| 2 | invalid command line |
| 3 | refused for safety (output on the source, source mounted, ...) |
| 130 | interrupted with Ctrl-C (progress was saved) |
