//! diskrescue-fixtures: synthetic test data for DiskRescue.
//!
//! The crate builds everything the other crates and the end-to-end tests need
//! without touching a real device and without root:
//!
//! * [`media`]: JPEG (with hand-assembled EXIF and thumbnail), PNG (also
//!   Adam7), GIF, BMP, WebP, baseline TIFF and a CR2-like raw, HEIC and
//!   videos in nine containers. Content is a deterministic gradient + noise +
//!   text-like-block canvas, so truncation or corruption is visible.
//! * [`damage`]: truncate, zero, flip, cut, strip a top-level ISO BMFF box
//!   (e.g. `moov`), break a PNG chunk CRC, drop the AVI index.
//! * [`fs`]: populated FAT12/16/32, exFAT, NTFS and ext4 images, with
//!   deleted files, and a report of where every file's bytes ended up.
//! * [`disk`]: MBR / GPT / superfloppy disk images around those volumes.
//! * [`sets`]: ready-made scenarios with a ground-truth manifest
//!   (`truth.json`), also written to disk by the `gen-fixtures` binary:
//!   `gen-fixtures <out-dir> [--set NAME]`.
//! * [`raw`]: filesystem-less carving images (contiguous, unaligned, nested,
//!   fragmented, truncated, with holes and a ddrescue mapfile, ...) with truth.
//! * [`tools`]: tool discovery and execution helpers.
//!
//! # External tools and what happens without them
//!
//! | Function | Needs | If missing |
//! |---|---|---|
//! | `media::{png, gif, bmp, webp, tiff, cr2_like, baseline_jpeg}`, `media::jpeg` (baseline, EXIF) | nothing | n/a |
//! | `media::jpeg` with `progressive`, `restart_interval` or non-4:4:4 chroma | `python3` + Pillow | `Err(Missing)` |
//! | `media::video` | `ffmpeg` (+ libx264 / libx265 / libvpx / libmp3lame / libopus for those codecs) | `Err(Missing)` |
//! | `media::heic` | `ffmpeg` with libx265 | `Err(Missing)` |
//! | `fs::build` FAT | `mkfs.vfat` (optional; a built-in formatter takes over) | built-in formatter |
//! | `fs::build` exFAT | `mkfs.exfat` | `Err(Missing)` |
//! | `fs::build` NTFS | `mkntfs`; `ntfscp` for root-only specs; `ntfs-3g` + `fusermount3` + `/dev/fuse` for directories, deletions, timestamps and named streams; `ntfsinfo` for extents | no mount: flat `ntfscp` fallback, `deleted_supported = false`; no `mkntfs`: `Err(Missing)`; no `ntfsinfo`: empty extents |
//! | `fs::build` ext4 | `mkfs.ext4`; `debugfs` for deletions, extents and pinned timestamps | no mkfs: `Err(Missing)`; no debugfs: specs with deletions fail, extents empty |
//! | `disk::build`, `damage::*` | nothing | n/a |
//! | `raw::*` | `ffmpeg` (video samples); Pillow adds progressive / restart JPEG samples (skipped without it) | `Err(Missing)` / sample skipped and listed in `skipped` |
//! | `sets::*` | the union of what its volumes and media need | `Err(Missing)` (never a half-built scenario) |
//!
//! Tests that depend on a tool use [`tools::have`] and print
//! `skipped: <tool> missing` when it is absent.
//!
//! # Reproducibility
//!
//! Content is seeded (`xoshiro256**`), timestamps are explicit (default
//! 2024-05-06 07:08:09 UTC), volume IDs and UUIDs are pinned. FAT, exFAT and
//! ext4 images are byte-identical across builds; NTFS images differ in the
//! timestamps and the serial number that `mkntfs` and `ntfs-3g` stamp.
//!
//! # Safety
//!
//! Nothing opens a block device; mounts (NTFS only) happen under a fresh
//! temporary directory and are always undone by a drop guard.

#![warn(missing_docs)]

pub mod damage;
pub mod disk;
pub mod error;
pub mod fs;
pub mod media;
pub mod raw;
pub mod rng;
pub mod sets;
pub mod tools;

pub use error::{FixtureError, Result};
pub use tools::Missing;
