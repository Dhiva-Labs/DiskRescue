//! Ready-made end-to-end scenarios.
//!
//! Each function builds a disk (or a folder of loose files) below `dir` and
//! returns a [`Scenario`] whose [`TruthFile`] list says what is where and
//! what a recovery run is expected to achieve. The same manifest is written
//! to `<dir>/truth.json`.
//!
//! | Scenario | Layout | Tools needed |
//! |---|---|---|
//! | [`camera_sd_card`] | FAT32 superfloppy, `DCIM/100CANON`, JPEG+EXIF, CR2-like, MP4, one deleted photo and video | ffmpeg (video); mkfs.vfat optional |
//! | [`phone_exfat`] | GPT + exFAT, nested folders, HEIC, PNG, MP4 (HEVC when libx265 exists) | ffmpeg, mkfs.exfat |
//! | [`ntfs_disk`] | MBR + NTFS, nested folders, one deleted photo, a resident file, a named stream | ffmpeg, mkntfs, ntfs-3g + fusermount3 + /dev/fuse |
//! | [`ext4_disk`] | GPT + ext4, two deleted files | ffmpeg, mkfs.ext4, debugfs |
//! | [`damaged_media`] | loose broken JPEG/PNG/MP4/AVI files with donors and originals | ffmpeg (video), python3 + Pillow not needed |
//! | [`fragmented_fat32`] | FAT32 with a deleted JPEG split around a live file | none |
//!
//! A scenario fails with [`FixtureError::Missing`] when a required tool is
//! absent; callers (tests) turn that into a skip.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::damage;
use crate::disk::{self, DiskSpec, PartSpec, Scheme};
use crate::error::{FixtureError, Result};
use crate::fs::{self, FileSpec, FsSpec, FsType};
use crate::media::{
    self, AudioCodec, Container, ExifOpts, HeicOpts, JpegOpts, PngOpts, TiffOpts, VideoCodec,
    VideoOpts,
};
use crate::tools;

/// What a recovery run is expected to achieve for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recoverability {
    /// Present in the filesystem: extraction returns the exact bytes.
    Live,
    /// Deleted but its data was not touched: undelete and (when contiguous)
    /// carving return the exact bytes.
    DeletedIntact,
    /// Damaged but a repair can produce a decodable file (see `original` /
    /// `donor` for the reference).
    Repairable,
    /// Damaged; only a partial result is possible (for example the top part
    /// of a truncated picture).
    PartialOnly,
    /// A healthy file provided as the donor/reference for another file.
    Donor,
    /// A healthy copy of the file that was damaged (comparison target).
    Original,
}

/// One file of the ground truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruthFile {
    /// Path inside the volume (`/`-separated, no leading slash) or, for loose
    /// files, the path relative to the scenario root.
    pub path: String,
    /// Format tag: `jpeg`, `png`, `cr2`, `heic`, `mp4`, `avi`, `text`, ...
    pub kind: String,
    /// Size in bytes.
    pub size: u64,
    /// BLAKE3 of the content (of the undamaged file, for damaged ones: of
    /// the damaged bytes on disk; `original` names the healthy twin).
    pub blake3: String,
    /// Expected outcome.
    pub expect: Recoverability,
    /// Deleted from the filesystem after being written.
    pub deleted: bool,
    /// Absolute `(offset, length)` ranges of the data inside `disk_image`
    /// (empty for loose files and for files without clusters).
    pub extents: Vec<(u64, u64)>,
    /// True when the data is one contiguous run (carvable by signature).
    pub contiguous: bool,
    /// Modification time (Unix seconds), when written to the volume.
    pub mtime: Option<i64>,
    /// EXIF `DateTimeOriginal` for photos.
    pub exif_date: Option<String>,
    /// Loose files: the healthy twin of a damaged file.
    pub original: Option<String>,
    /// Loose files: a healthy file with the same stream parameters that a
    /// repair can borrow headers or `moov` data from.
    pub donor: Option<String>,
    /// Free-form remark.
    pub note: String,
}

/// A built scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scenario {
    /// Directory holding everything.
    pub root: PathBuf,
    /// The disk image, for volume scenarios.
    pub disk_image: Option<PathBuf>,
    /// `(start, size)` of the partitions inside the image (a superfloppy has
    /// one entry starting at 0).
    pub partitions: Vec<(u64, u64)>,
    /// Ground truth.
    pub files: Vec<TruthFile>,
    /// Remarks about how the scenario was built.
    pub notes: Vec<String>,
}

impl Scenario {
    /// Look up a truth entry by path.
    pub fn file(&self, path: &str) -> Option<&TruthFile> {
        self.files.iter().find(|f| f.path == path)
    }

    /// The truth manifest as pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Load a scenario from `<dir>/truth.json`.
    pub fn load(dir: &Path) -> Result<Scenario> {
        let text = std::fs::read_to_string(dir.join("truth.json"))?;
        serde_json::from_str(&text).map_err(|e| FixtureError::invalid(format!("truth.json: {e}")))
    }

    fn write_truth(&self) -> Result<()> {
        std::fs::write(self.root.join("truth.json"), self.to_json())?;
        Ok(())
    }
}

// ---------------------------------------------------------------- helpers

/// A file to put on a volume plus its ground-truth attributes.
struct Item {
    path: String,
    data: Vec<u8>,
    kind: &'static str,
    mtime: Option<i64>,
    deleted: bool,
    exif_date: Option<String>,
    note: String,
    fragment: Option<(u32, u32)>,
}

impl Item {
    fn new(path: &str, kind: &'static str, data: Vec<u8>) -> Item {
        Item {
            path: path.to_string(),
            data,
            kind,
            mtime: None,
            deleted: false,
            exif_date: None,
            note: String::new(),
            fragment: None,
        }
    }
    fn at(mut self, mtime: i64) -> Item {
        self.mtime = Some(mtime);
        self
    }
    fn deleted(mut self) -> Item {
        self.deleted = true;
        self
    }
    fn exif(mut self, d: &str) -> Item {
        self.exif_date = Some(d.to_string());
        self
    }
    fn note(mut self, n: &str) -> Item {
        self.note = n.to_string();
        self
    }
}

/// `YYYY:MM:DD HH:MM:SS` to Unix seconds (UTC).
fn exif_to_unix(s: &str) -> i64 {
    diskrescue_core::time::Timestamp::from_exif_str(s)
        .map(|t| t.unix_secs)
        .unwrap_or(crate::fs::DEFAULT_MTIME)
}

fn photo(w: u32, h: u32, seed: u64, date: &str, model: &str) -> Result<Vec<u8>> {
    media::jpeg(&JpegOpts {
        width: w,
        height: h,
        quality: 82,
        seed,
        exif: Some(ExifOpts {
            date_time_original: date.to_string(),
            make: "Canon".into(),
            model: model.to_string(),
            orientation: 1,
            thumbnail: true,
        }),
        ..Default::default()
    })
}

fn clip(seed: u64, hevc: bool, faststart: bool) -> Result<Vec<u8>> {
    media::video(&VideoOpts {
        container: Container::Mp4,
        video: if hevc {
            VideoCodec::Hevc
        } else {
            VideoCodec::H264
        },
        audio: AudioCodec::Aac,
        seconds: 2.0,
        fps: 25,
        width: 320,
        height: 240,
        faststart,
        seed,
        ..Default::default()
    })
}

fn require_tools(names: &[&str]) -> Result<()> {
    for n in names {
        tools::require(n)?;
    }
    Ok(())
}

fn truth_from_items(items: &[Item], report: &fs::BuildReport, part_start: u64) -> Vec<TruthFile> {
    let mut out = Vec::new();
    for it in items {
        let norm = it.path.trim_matches('/').to_string();
        // Named streams are not placed files of their own.
        let placed = report.find(&norm);
        let (extents, contiguous) = match placed {
            Some(p) => {
                let abs: Vec<(u64, u64)> = p
                    .extents
                    .iter()
                    .map(|&(o, l)| (o + part_start, l))
                    .collect();
                let contig = abs.len() == 1;
                (abs, contig)
            }
            None => (Vec::new(), false),
        };
        out.push(TruthFile {
            path: norm,
            kind: it.kind.to_string(),
            size: it.data.len() as u64,
            blake3: diskrescue_core::util::hash::blake3_hex(&it.data),
            expect: if it.deleted {
                Recoverability::DeletedIntact
            } else {
                Recoverability::Live
            },
            deleted: it.deleted,
            extents,
            contiguous,
            mtime: Some(it.mtime.unwrap_or(crate::fs::DEFAULT_MTIME)),
            exif_date: it.exif_date.clone(),
            original: None,
            donor: None,
            note: it.note.clone(),
        });
    }
    out
}

fn fs_spec(kind: FsType, size: u64, cluster: Option<u32>, label: &str, items: &[Item]) -> FsSpec {
    let mut spec = FsSpec::new(kind, size);
    spec.cluster_size = cluster;
    spec.label = Some(label.to_string());
    for it in items {
        let mut f = FileSpec::new(it.path.clone(), it.data.clone());
        f.mtime = it.mtime;
        if let Some((a, b)) = it.fragment {
            f = f.fragmented(a, b);
        }
        spec.files.push(f);
        if it.deleted {
            spec.deleted.push(it.path.clone());
        }
    }
    spec
}

/// Build the volume, optionally wrap it in a partition table, write the
/// truth and return the scenario.
fn volume_scenario(
    dir: &Path,
    image_name: &str,
    spec: FsSpec,
    items: &[Item],
    scheme: Scheme,
    seed: u64,
    mut notes: Vec<String>,
) -> Result<Scenario> {
    std::fs::create_dir_all(dir)?;
    let disk_path = dir.join(image_name);
    let (report, partitions) = if scheme == Scheme::None {
        let rep = fs::build(&spec, &disk_path)?;
        let size = std::fs::metadata(&disk_path)?.len();
        (rep, vec![(0u64, size)])
    } else {
        let tmp = tempfile::tempdir()?;
        let vol = tmp.path().join("volume.img");
        let rep = fs::build(&spec, &vol)?;
        let mut ds = DiskSpec::new(scheme, vec![PartSpec::new(&vol)]);
        ds.seed = seed;
        let dr = disk::build(&ds, &disk_path)?;
        (rep, dr.partitions)
    };
    notes.push(format!("builder: {}", report.tool));
    notes.extend(report.notes.iter().cloned());
    let part_start = partitions.first().map(|p| p.0).unwrap_or(0);
    let files = truth_from_items(items, &report, part_start);
    let sc = Scenario {
        root: dir.to_path_buf(),
        disk_image: Some(disk_path),
        partitions,
        files,
        notes,
    };
    sc.write_truth()?;
    Ok(sc)
}

// ---------------------------------------------------------------- scenarios

/// A camera SD card: FAT32 without partition table, 4 KiB clusters,
/// `DCIM/100CANON` with eight EXIF photos (`IMG_0001..0008.JPG`, the fourth
/// deleted), a CR2-like raw, two MP4 clips (`MVI_0003.MP4` live,
/// `MVI_0009.MP4` deleted). Names are plain 8.3 uppercase, as cameras write.
pub fn camera_sd_card(dir: &Path) -> Result<Scenario> {
    require_tools(&["ffmpeg"])?;
    let mut items = Vec::new();
    for i in 1..=8u64 {
        let date = format!("2024:07:{:02} 10:{:02}:31", 10 + i, 20 + i);
        let mut it = Item::new(
            &format!("DCIM/100CANON/IMG_{i:04}.JPG"),
            "jpeg",
            photo(640, 480, i, &date, "EOS 80D")?,
        )
        .at(exif_to_unix(&date))
        .exif(&date);
        if i == 4 {
            it = it.deleted().note("deleted photo, data untouched");
        }
        items.push(it);
    }
    let cr2 = media::cr2_like(&TiffOpts {
        width: 256,
        height: 192,
        seed: 6,
        ..Default::default()
    });
    items.push(
        Item::new("DCIM/100CANON/CRW_0006.CR2", "cr2", cr2)
            .at(exif_to_unix("2024:07:16 10:26:31"))
            .exif("2024:05:06 07:08:09"),
    );
    items.push(
        Item::new("DCIM/100CANON/MVI_0003.MP4", "mp4", clip(3, false, false)?)
            .at(exif_to_unix("2024:07:13 11:00:00")),
    );
    items.push(
        Item::new("DCIM/100CANON/MVI_0009.MP4", "mp4", clip(9, false, false)?)
            .at(exif_to_unix("2024:07:19 18:30:00"))
            .deleted()
            .note("deleted video, moov at the end, data untouched"),
    );
    let spec = fs_spec(FsType::Fat32, 272 << 20, Some(4096), "EOS_DIGITAL", &items);
    volume_scenario(
        dir,
        "sd_card.img",
        spec,
        &items,
        Scheme::None,
        1,
        vec!["FAT32 superfloppy, 4 KiB clusters, no partition table".into()],
    )
}

/// A phone with exFAT storage under GPT: nested folders, JPEG photos, a
/// HEIC picture (a real HEIF container when ffmpeg has libx265, otherwise a
/// JPEG placeholder with the `.HEIC` name), PNG screenshots, an MP4 clip
/// (HEVC when available), a text file, one deleted photo and one deleted
/// screenshot.
pub fn phone_exfat(dir: &Path) -> Result<Scenario> {
    require_tools(&["ffmpeg", "mkfs.exfat"])?;
    let hevc = tools::ffmpeg_has_encoder("libx265");
    let mut notes = Vec::new();
    let mut items = Vec::new();
    for i in 1..=3u64 {
        let date = format!("2024:05:0{i} 10:15:00");
        let mut it = Item::new(
            &format!("DCIM/Camera/IMG_2024050{i}_101500.jpg"),
            "jpeg",
            photo(800, 600, 20 + i, &date, "Pixel Fixture")?,
        )
        .at(exif_to_unix(&date))
        .exif(&date);
        if i == 2 {
            it = it.deleted().note("deleted photo");
        }
        items.push(it);
    }
    match media::heic(&HeicOpts {
        seed: 5,
        ..Default::default()
    }) {
        Ok(h) => items.push(
            Item::new("DCIM/Camera/IMG_20240503_090000.HEIC", "heic", h)
                .at(exif_to_unix("2024:05:03 09:00:00")),
        ),
        Err(e) if e.is_missing() => {
            notes.push("HEIC is a JPEG placeholder: no libx265".into());
            items.push(
                Item::new(
                    "DCIM/Camera/IMG_20240503_090000.HEIC",
                    "jpeg",
                    photo(640, 480, 5, "2024:05:03 09:00:00", "Pixel Fixture")?,
                )
                .at(exif_to_unix("2024:05:03 09:00:00"))
                .note("JPEG data with a .HEIC name (placeholder)"),
            );
        }
        Err(e) => return Err(e),
    }
    for i in 1..=2u64 {
        let png = media::png(&PngOpts {
            width: 360,
            height: 640,
            seed: 40 + i,
            ..Default::default()
        });
        let mut it = Item::new(
            &format!("Pictures/Screenshots/Screenshot_2024050{i}-120000.png"),
            "png",
            png,
        )
        .at(exif_to_unix(&format!("2024:05:0{i} 12:00:00")));
        if i == 2 {
            it = it.deleted().note("deleted screenshot");
        }
        items.push(it);
    }
    items.push(
        Item::new(
            "Movies/VID_20240502_120000.mp4",
            "mp4",
            clip(7, hevc, true)?,
        )
        .at(exif_to_unix("2024:05:02 12:00:00"))
        .note(if hevc {
            "HEVC, faststart"
        } else {
            "H.264, faststart"
        }),
    );
    items.push(
        Item::new(
            "Documents/notes.txt",
            "text",
            b"phone fixture notes\n".repeat(30),
        )
        .at(exif_to_unix("2024:04:01 08:00:00")),
    );
    let spec = fs_spec(FsType::ExFat, 64 << 20, Some(4096), "PHONE", &items);
    volume_scenario(dir, "phone.img", spec, &items, Scheme::Gpt, 2, notes)
}

/// A Windows disk: MBR + NTFS partition, `Pictures/<year>/<event>` folders,
/// a deleted photo, a small file that stays resident in the MFT record, a
/// named data stream (`:Zone.Identifier`) on one photo, and a video.
/// Requires a working `ntfs-3g` FUSE mount.
pub fn ntfs_disk(dir: &Path) -> Result<Scenario> {
    require_tools(&["ffmpeg", "mkntfs"])?;
    if !fs::ntfs_mount_available() {
        return Err(
            tools::Missing("ntfs-3g FUSE mount (ntfs-3g, fusermount3, /dev/fuse)".into()).into(),
        );
    }
    let mut items = Vec::new();
    let specs = [
        (
            "Pictures/2023/Summer/IMG_0100.jpg",
            1u64,
            "2023:07:01 14:00:00",
        ),
        (
            "Pictures/2023/Summer/IMG_0101.jpg",
            2,
            "2023:07:01 14:05:00",
        ),
        (
            "Pictures/2024/Spring/IMG_0200.jpg",
            3,
            "2024:04:10 09:30:00",
        ),
        (
            "Pictures/2024/Spring/IMG_0201.jpg",
            4,
            "2024:04:10 09:31:00",
        ),
    ];
    for (i, (p, seed, date)) in specs.iter().enumerate() {
        let mut it = Item::new(p, "jpeg", photo(640, 480, 60 + seed, date, "NTFS Cam")?)
            .at(exif_to_unix(date))
            .exif(date);
        if i == 3 {
            it = it.deleted().note("deleted photo");
        }
        items.push(it);
    }
    items.push(
        Item::new(
            "Pictures/2023/Summer/IMG_0100.jpg:Zone.Identifier",
            "stream",
            b"[ZoneTransfer]\r\nZoneId=3\r\n".to_vec(),
        )
        .note("alternate data stream of IMG_0100.jpg"),
    );
    items.push(
        Item::new(
            "Documents/readme.txt",
            "text",
            b"This small file fits inside its MFT record (resident data).\r\n".to_vec(),
        )
        .at(exif_to_unix("2024:01:02 03:04:05"))
        .note("resident: no clusters, invisible to carving"),
    );
    items.push(
        Item::new("Videos/holiday.mp4", "mp4", clip(11, false, false)?)
            .at(exif_to_unix("2023:07:02 18:00:00")),
    );
    let spec = fs_spec(FsType::Ntfs, 64 << 20, Some(4096), "WINDISK", &items);
    volume_scenario(dir, "windows.img", spec, &items, Scheme::Mbr, 3, Vec::new())
}

/// A Linux disk: GPT + ext4 (4 KiB blocks), photos and videos in nested
/// folders, one deleted photo and one deleted video (extents kept by
/// `debugfs rm`).
pub fn ext4_disk(dir: &Path) -> Result<Scenario> {
    require_tools(&["ffmpeg", "mkfs.ext4", "debugfs"])?;
    let mut items = Vec::new();
    for i in 1..=4u64 {
        let date = format!("2024:03:{:02} 16:{:02}:00", i, i * 5);
        let mut it = Item::new(
            &format!("Pictures/trip/IMG_{i:04}.jpg"),
            "jpeg",
            photo(640, 480, 80 + i, &date, "Linux Cam")?,
        )
        .at(exif_to_unix(&date))
        .exif(&date);
        if i == 2 {
            it = it
                .deleted()
                .note("deleted photo (debugfs rm keeps extents)");
        }
        items.push(it);
    }
    items.push(
        Item::new("Videos/clip1.mp4", "mp4", clip(13, false, false)?)
            .at(exif_to_unix("2024:03:05 17:00:00")),
    );
    items.push(
        Item::new("Videos/clip2.mp4", "mp4", clip(14, false, true)?)
            .at(exif_to_unix("2024:03:06 17:00:00"))
            .deleted()
            .note("deleted video"),
    );
    items.push(Item::new("notes.txt", "text", b"ext4 fixture\n".repeat(50)));
    let spec = fs_spec(FsType::Ext4, 64 << 20, Some(4096), "LINUXDISK", &items);
    volume_scenario(dir, "linux.img", spec, &items, Scheme::Gpt, 4, Vec::new())
}

/// FAT32 with a deleted JPEG stored in two fragments around a live file.
/// Carving by signature alone yields a truncated or corrupt image; fragment
/// recovery or a repair must stitch the halves.
pub fn fragmented_fat32(dir: &Path) -> Result<Scenario> {
    let a = media::jpeg(&JpegOpts {
        width: 640,
        height: 480,
        quality: 80,
        seed: 91,
        ..Default::default()
    })?;
    let b = media::baseline_jpeg(96, 64, 70, 92);
    let clusters = (a.len() as u64).div_ceil(4096) as u32;
    let first = (clusters / 2).max(1);
    let mut a_item = Item::new("DCIM/IMG_A.JPG", "jpeg", a)
        .deleted()
        .note("deleted, fragmented: second half starts after the live file B");
    a_item.fragment = Some((first, (b.len() as u32).div_ceil(4096)));
    let b_item = Item::new("DCIM/IMG_B.JPG", "jpeg", b).note("small live file inside A's gap");
    let items = vec![a_item, b_item];
    let spec = fs_spec(FsType::Fat32, 272 << 20, Some(4096), "FRAGTEST", &items);
    let mut sc = volume_scenario(
        dir,
        "fragmented.img",
        spec,
        &items,
        Scheme::None,
        5,
        vec!["A is written first with a gap, B fills the gap, then A is deleted".into()],
    )?;
    for f in &mut sc.files {
        if f.path == "DCIM/IMG_A.JPG" {
            f.expect = Recoverability::PartialOnly;
            f.contiguous = false;
        }
    }
    sc.write_truth()?;
    Ok(sc)
}

/// Loose damaged files plus the healthy twins and donors needed to check
/// the repairs. Layout below `dir`:
///
/// ```text
/// originals/   healthy versions of every damaged file
/// donors/      healthy files with the same parameters (other content)
/// truncated.jpg  zeroed_header.jpg  bad_crc.png  truncated.png
/// no_moov.mp4  truncated.mp4  no_idx1.avi
/// ```
pub fn damaged_media(dir: &Path) -> Result<Scenario> {
    require_tools(&["ffmpeg"])?;
    std::fs::create_dir_all(dir.join("originals"))?;
    std::fs::create_dir_all(dir.join("donors"))?;
    let mut files: Vec<TruthFile> = Vec::new();

    let mut add = |rel: &str,
                   kind: &str,
                   data: &[u8],
                   expect: Recoverability,
                   original: Option<&str>,
                   donor: Option<&str>,
                   note: &str|
     -> Result<()> {
        let path = dir.join(rel);
        std::fs::write(&path, data)?;
        files.push(TruthFile {
            path: rel.to_string(),
            kind: kind.to_string(),
            size: data.len() as u64,
            blake3: diskrescue_core::util::hash::blake3_hex(data),
            expect,
            deleted: false,
            extents: Vec::new(),
            contiguous: false,
            mtime: None,
            exif_date: None,
            original: original.map(String::from),
            donor: donor.map(String::from),
            note: note.to_string(),
        });
        Ok(())
    };

    // JPEG: truncated.
    let jpg = photo(640, 480, 101, "2024:06:01 12:00:00", "Damage Cam")?;
    add(
        "originals/truncated.jpg",
        "jpeg",
        &jpg,
        Recoverability::Original,
        None,
        None,
        "",
    )?;
    add(
        "truncated.jpg",
        "jpeg",
        &damage::truncate(&jpg, jpg.len() * 55 / 100),
        Recoverability::Repairable,
        Some("originals/truncated.jpg"),
        None,
        "cut at 55%: appending EOI recovers the top part of the picture",
    )?;

    // JPEG: header zeroed, donor with identical tables.
    let plain = media::jpeg(&JpegOpts {
        width: 640,
        height: 480,
        quality: 85,
        seed: 102,
        ..Default::default()
    })?;
    let donor_jpg = media::jpeg(&JpegOpts {
        width: 640,
        height: 480,
        quality: 85,
        seed: 103,
        ..Default::default()
    })?;
    let sos = media::jpeg::segments(&plain)
        .iter()
        .find(|s| s.0 == 0xDA)
        .map(|s| s.1)
        .ok_or_else(|| FixtureError::corrupt("generated JPEG", "no SOS"))?;
    let mut broken = plain.clone();
    damage::zero_range(&mut broken, 0, sos);
    add(
        "originals/zeroed_header.jpg",
        "jpeg",
        &plain,
        Recoverability::Original,
        None,
        None,
        "",
    )?;
    add(
        "donors/donor.jpg",
        "jpeg",
        &donor_jpg,
        Recoverability::Donor,
        None,
        None,
        "same size and quality as zeroed_header.jpg, different pixels",
    )?;
    add(
        "zeroed_header.jpg",
        "jpeg",
        &broken,
        Recoverability::Repairable,
        Some("originals/zeroed_header.jpg"),
        Some("donors/donor.jpg"),
        "everything before SOS zeroed; scan data intact; tables come from the donor",
    )?;

    // PNG: bad CRC and truncated.
    let png = media::png(&PngOpts {
        width: 320,
        height: 240,
        seed: 104,
        idat_chunk: 4096,
        ..Default::default()
    });
    add(
        "originals/bad_crc.png",
        "png",
        &png,
        Recoverability::Original,
        None,
        None,
        "",
    )?;
    let mut bad = png.clone();
    damage::png_corrupt_crc(&mut bad, b"IDAT", 1);
    add(
        "bad_crc.png",
        "png",
        &bad,
        Recoverability::Repairable,
        Some("originals/bad_crc.png"),
        None,
        "CRC of the second IDAT chunk is wrong; the data is intact",
    )?;
    add(
        "originals/truncated.png",
        "png",
        &png,
        Recoverability::Original,
        None,
        None,
        "",
    )?;
    add(
        "truncated.png",
        "png",
        &damage::truncate(&png, png.len() / 2),
        Recoverability::PartialOnly,
        Some("originals/truncated.png"),
        None,
        "cut at 50%: no IEND, later IDAT chunks missing",
    )?;

    // MP4: moov stripped (+ donor), truncated.
    let mp4_opts = |seed: u64, faststart: bool| VideoOpts {
        container: Container::Mp4,
        video: VideoCodec::H264,
        audio: AudioCodec::Aac,
        seconds: 2.0,
        fps: 25,
        width: 320,
        height: 240,
        faststart,
        seed,
        ..Default::default()
    };
    let mp4 = media::video(&mp4_opts(111, false))?;
    let mp4_donor = media::video(&mp4_opts(112, false))?;
    add(
        "originals/no_moov.mp4",
        "mp4",
        &mp4,
        Recoverability::Original,
        None,
        None,
        "",
    )?;
    add(
        "donors/donor.mp4",
        "mp4",
        &mp4_donor,
        Recoverability::Donor,
        None,
        None,
        "same ffmpeg settings as no_moov.mp4, different content",
    )?;
    add(
        "no_moov.mp4",
        "mp4",
        &damage::strip_box(&mp4, b"moov"),
        Recoverability::Repairable,
        Some("originals/no_moov.mp4"),
        Some("donors/donor.mp4"),
        "moov removed (camera crashed before finalising); rebuild from the donor's moov",
    )?;
    let mp4_fast = media::video(&mp4_opts(113, true))?;
    add(
        "originals/truncated.mp4",
        "mp4",
        &mp4_fast,
        Recoverability::Original,
        None,
        None,
        "",
    )?;
    add(
        "truncated.mp4",
        "mp4",
        &damage::truncate(&mp4_fast, mp4_fast.len() * 60 / 100),
        Recoverability::PartialOnly,
        Some("originals/truncated.mp4"),
        None,
        "faststart file cut at 60%: moov present, mdat incomplete",
    )?;

    // AVI without idx1.
    let avi = media::video(&VideoOpts {
        container: Container::Avi,
        video: VideoCodec::Mpeg4,
        audio: AudioCodec::Pcm,
        seconds: 2.0,
        fps: 25,
        width: 320,
        height: 240,
        seed: 121,
        ..Default::default()
    })?;
    add(
        "originals/no_idx1.avi",
        "avi",
        &avi,
        Recoverability::Original,
        None,
        None,
        "",
    )?;
    add(
        "no_idx1.avi",
        "avi",
        &damage::avi_strip_idx1(&avi),
        Recoverability::Repairable,
        Some("originals/no_idx1.avi"),
        None,
        "idx1 chunk removed; the movi list is intact, the index can be rebuilt",
    )?;

    let sc = Scenario {
        root: dir.to_path_buf(),
        disk_image: None,
        partitions: Vec::new(),
        files,
        notes: vec!["loose files; see `original` and `donor` for reference data".into()],
    };
    sc.write_truth()?;
    Ok(sc)
}

/// Names accepted by [`build_named`], in the order `all` builds them.
pub const SET_NAMES: [&str; 6] = [
    "camera-sd-card",
    "phone-exfat",
    "ntfs-disk",
    "ext4-disk",
    "damaged-media",
    "fragmented-fat32",
];

/// Build one scenario by its command-line name into `dir`.
pub fn build_named(name: &str, dir: &Path) -> Result<Scenario> {
    match name {
        "camera-sd-card" => camera_sd_card(dir),
        "phone-exfat" => phone_exfat(dir),
        "ntfs-disk" => ntfs_disk(dir),
        "ext4-disk" => ext4_disk(dir),
        "damaged-media" => damaged_media(dir),
        "fragmented-fat32" => fragmented_fat32(dir),
        other => Err(FixtureError::invalid(format!(
            "unknown set {other:?}; known: {}",
            SET_NAMES.join(", ")
        ))),
    }
}
