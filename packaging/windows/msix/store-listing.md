# Microsoft Store listing (Partner Center)

The text entered in Partner Center for DiskRescue, kept here for future updates.
The package comes from the release workflow (artifact `microsoft-store`,
`DiskRescue-<version>-x64.msix`); `make-assets.py` renders the images.

## Pricing and availability

- Price: USD 9.99, one-time (the Store has no 10.00 tier). No free trial, no
  in-app purchases.
- Organizational licensing: Store-managed (online) allowed.
- Markets: all.
- Visibility: public.

## Properties

- Category: Utilities & tools (subcategory: Backup & manage files).
- Privacy policy: <https://github.com/Dhiva-Labs/DiskRescue/blob/main/PRIVACY.md>
- Website: <https://github.com/Dhiva-Labs/DiskRescue>
- Support contact: reachout@dhivalabs.com
- Product declarations: installs to alternate drives allowed.
- System requirements: Windows 10 version 1809 or later, 64-bit. Reading a whole
  drive needs an administrator account (or an administrator's password).

## Store listing (English, United States)

**Product name:** DiskRescue

**Short description:**

Get your photos and videos back from a failing, formatted or "needs formatting" SD card, USB drive or disk, and repair the files that came back broken. Read-only and careful: the drive you rescue is never written to.

**Description:**

DiskRescue recovers photos and videos from SD cards, USB pen drives, hard disks and SSDs that are failing, were reformatted, ask to be formatted, or had files deleted. Then it repairs the damaged ones.

Pick the drive, press Rescue, and browse your photos as they come back. Everything happens on your PC: DiskRescue never connects to the internet.

• Careful with failing drives. DiskRescue copies a dying drive the way a recovery lab does, with a read strategy for each kind of device: it skips over bad areas to save the healthy parts first, then goes back for the rest. A live map shows what has been read. Stop at any time and continue later.
• Recovers by name. Existing and deleted files with their folders and dates, from FAT, exFAT and NTFS (and ext4 from Linux). It still works when the card "needs formatting", the partition table is gone, or the boot sector is destroyed.
• Finds what has no name left. Carving scans free space for more than 30 photo, camera RAW, video and audio formats: JPEG, HEIC, PNG, CR2, CR3, NEF, ARW, DNG, MP4, MOV, AVI, MKV, MTS and more, each with its exact length.
• Repairs broken files. Rebuilds videos that say "moov atom not found" after a camera died while recording, using a healthy clip from the same camera. Fixes cut-off and grey-bottomed photos, destroyed JPEG headers, PNG checksum errors and AVI files without an index. Damaged originals are always kept next to the repaired copies.
• Safe by design. The drive you rescue is opened read-only and never written to, and DiskRescue refuses to save recovered files onto it.
• Honest results. A report lists every file, where it came from, whether it checks out, and which parts could not be read (filled with zeros, never invented).
• For experts too. The same engine as the `diskrescue` command line and a terminal app (`diskrescue-tui`), both included. Drive copies use the GNU ddrescue mapfile format.

DiskRescue is open source (MIT) at github.com/Dhiva-Labs/DiskRescue.

**What's new in this version:** First Windows release.

**Product features:**

1. Recover photos and videos from SD cards, USB drives, hard disks and SSDs
2. Careful, resumable copies of failing drives with a live block map
3. Deleted files with their names and folders (FAT, exFAT, NTFS, ext4)
4. Works when a card asks to be formatted or the partition table is gone
5. Carving for 30+ photo, camera RAW, video and audio formats
6. Repairs broken MP4/MOV videos, JPEG and PNG photos and AVI files
7. Never writes to the drive being rescued
8. Works offline: nothing leaves your PC
9. Command-line and terminal apps included

**Keywords:** photo recovery, data recovery, sd card recovery, recover deleted files, video repair, undelete, disk recovery

**Copyright:** © 2026 Dhiva Labs

**Developed by:** Dhiva Labs

**Screenshots:** `docs/store-screenshots/*.png` (1920×1080).

**Store logos:** `packaging/windows/msix/store-logos/` (9:16 poster art 1440×2160, 1:1 box art 1080×1080, 1:1 app tile icon 300×300).

## Age ratings (IARC)

All Other App Types; every question answered No (no ratings-relevant content, no user-to-user sharing, no featured online content, no age-restricted products, no location sharing, no digital purchases, no rewards, not a browser, not news/education).

## Certification notes

DiskRescue recovers photos and videos from drives and repairs damaged media files. It works offline and needs no account.

To test it:
1. Plug in a USB pen drive or SD card with a few photos on it, and delete some of them. (Without one, create a small virtual disk in Disk Management, format it FAT32 and copy photos onto it.)
2. Open DiskRescue. The drive is listed on "Rescue from a drive". Choose it, then Rescue.
3. Reading a whole drive needs administrator rights. DiskRescue then offers "Restart as administrator", which shows the standard UAC prompt. This is why the package declares allowElevation. It never elevates otherwise, and repairs and image files need no elevation.
4. Pick an output folder on another drive. DiskRescue refuses one on the drive being rescued. The existing and deleted photos are recovered and shown as thumbnails.
5. "Repair photos & videos" repairs a damaged JPEG, PNG, MP4/MOV or AVI file, and "Recover from an image file" works on a drive image without administrator rights.

The source drive is opened read-only and never written to. The package also installs the command-line program as the `diskrescue` and `diskrescue-tui` commands (app execution aliases).
