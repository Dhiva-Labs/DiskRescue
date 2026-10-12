//! Command-line definition (clap).

use std::path::PathBuf;
use std::time::Duration;

use clap::builder::PossibleValuesParser;
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use diskrescue_core::profile::ImagingProfile;

use crate::term::ColorChoice;
use crate::util::{parse_secs, parse_size};

const ABOUT: &str = "Recover photos and videos from failing disks, SD cards and USB drives";

const LONG_ABOUT: &str = "\
Recover photos and videos from failing or damaged hard disks, SSDs, USB pen drives and SD cards,
and repair damaged media files.

DiskRescue works in two steps that you can also run together:

  1. IMAGE the failing device into a file, reading the healthy areas first and coming back for the
     damaged ones. This is the only step that touches the device; it can be stopped and resumed.
  2. RECOVER photos and videos from the image: by name through the filesystem, by searching the
     raw data, then check and repair what came back.

The device is only ever read, never written to.";

const AFTER_HELP: &str = "\
Before you start:
  - Stop using the device. Every write can overwrite deleted photos.
  - Do not run chkdsk, fsck, \"Repair disk\" or \"Format\" on it.
  - Write the results to a different disk with enough free space.
  - Reading a raw device needs administrator rights (sudo) or membership of the 'disk' group.
    Image files need no special rights.

Examples:
  diskrescue list
  diskrescue image /dev/sdb card.img
  diskrescue recover card.img -o ~/Recovered
  diskrescue repair broken.mp4 -o fixed --donor good_clip.mp4

Exit codes: 0 finished, 1 error, 2 invalid command line, 3 refused for safety, 130 interrupted.";

/// DiskRescue command line.
#[derive(Debug, Parser)]
#[command(
    name = "diskrescue",
    version,
    about = ABOUT,
    long_about = LONG_ABOUT,
    after_help = AFTER_HELP,
    propagate_version = true,
    arg_required_else_help = true
)]
pub struct Cli {
    /// Show more detail (-v: what is happening, -vv: debugging output)
    #[arg(
        short = 'v',
        long,
        action = ArgAction::Count,
        global = true,
        help_heading = "Global options"
    )]
    pub verbose: u8,

    /// When to use colours: auto, always or never
    #[arg(
        long,
        value_enum,
        default_value_t = ColorChoice::Auto,
        global = true,
        value_name = "WHEN",
        hide_possible_values = true,
        help_heading = "Global options"
    )]
    pub color: ColorChoice,

    /// What to do
    #[command(subcommand)]
    pub command: Command,
}

/// The subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show disks, cards and USB drives
    List(ListArgs),
    /// Show device details, partitions and filesystems
    Info(InfoArgs),
    /// Make a careful, resumable copy of a failing device
    Image(ImageArgs),
    /// List what can be recovered, write nothing
    Scan(ScanArgs),
    /// Recover photos and videos
    Recover(RecoverArgs),
    /// Search the raw data only (ignore filesystems)
    Carve(CarveArgs),
    /// Repair damaged photos and videos
    Repair(RepairArgs),
    /// Check whether files are intact
    Verify(VerifyArgs),
    /// Show the progress map of an image
    Map(MapArgs),
}

/// `diskrescue list`
#[derive(Debug, Args)]
#[command(
    after_help = "Virtual devices (loop, device-mapper, RAM disks) are hidden unless --all is given."
)]
pub struct ListArgs {
    /// Include virtual devices
    #[arg(long)]
    pub all: bool,
    /// Print the devices as JSON
    #[arg(long)]
    pub json: bool,
}

/// `diskrescue info`
#[derive(Debug, Args)]
#[command(after_help = "Examples:\n  diskrescue info /dev/sdb\n  diskrescue info card.img --json")]
pub struct InfoArgs {
    /// Device (/dev/sdb, \\.\PhysicalDrive2), partition or image file
    pub source: PathBuf,
    /// Mapfile of the image (default: SOURCE.map when it exists)
    #[arg(long, value_name = "MAPFILE")]
    pub map: Option<PathBuf>,
    /// Print the details as JSON
    #[arg(long)]
    pub json: bool,
}

/// Read-strategy tuning shared by `image` and `recover --image-to`.
#[derive(Debug, Args, Clone, Default)]
pub struct Tuning {
    /// Reading strategy, chosen from the device type by default
    #[arg(
        long,
        value_name = "NAME",
        value_parser = PossibleValuesParser::new(ImagingProfile::names().iter().copied())
    )]
    pub profile: Option<String>,
    /// Give up on a single read after this many seconds (all passes)
    #[arg(long, value_name = "SECS", value_parser = parse_secs)]
    pub timeout: Option<Duration>,
    /// Size of one read, for example 64k or 1M
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub block_size: Option<u64>,
    /// Number of times to retry unreadable sectors at the end
    #[arg(long, value_name = "N")]
    pub retries: Option<u32>,
    /// Skip the trimming pass (probing the edges of damaged areas)
    #[arg(long)]
    pub no_trim: bool,
    /// Skip the scraping pass (reading damaged areas sector by sector)
    #[arg(long)]
    pub no_scrape: bool,
    /// Copy from the end of the device towards the start
    #[arg(long)]
    pub reverse: bool,
}

/// `diskrescue image`
#[derive(Debug, Args)]
#[command(
    long_about = "Copy a device into an image file, reading the healthy areas first and coming back \
for the damaged ones. The progress is saved in a mapfile (GNU ddrescue format), so the copy can be \
stopped and resumed at any time: run the same command again.",
    after_help = "\
Profiles:
  hdd        internal hard disks: 64 KiB reads, exponential skipping over bad areas, second pass
             backwards, sector-level scraping last
  usb-hdd    hard disks in USB enclosures: as hdd, plus re-opening the device when the bridge stalls
  ssd, nvme  SATA and NVMe SSDs: large reads, short timeouts, no trimming, more retry passes
  usb-ssd    SSDs in USB enclosures: as ssd, with bridge handling
  usb-flash  pen drives: small reads, 10 s timeouts, waits for the drive to come back after it
             drops off the bus
  sd         SD and microSD cards: as usb-flash with 32 KiB reads
  generic    anything else: GNU ddrescue's defaults

In the full-screen view, q stops and saves (resume later) and p pauses.

Examples:
  diskrescue image /dev/sdb card.img
  diskrescue image /dev/sdb card.img --profile usb-flash --timeout 5
  diskrescue image /dev/sdb part.img --start 1M --size 4G"
)]
pub struct ImageArgs {
    /// Device (/dev/sdb, \\.\PhysicalDrive2), partition or image file to copy from
    pub source: PathBuf,
    /// Image file to create (or to resume)
    pub image: PathBuf,
    /// Where to keep the progress map (default: IMAGE.map)
    #[arg(long, value_name = "MAPFILE")]
    pub map: Option<PathBuf>,
    #[command(flatten)]
    pub tuning: Tuning,
    /// Copy only a part of the device: start offset, for example 1M
    #[arg(long, value_name = "OFFSET", value_parser = parse_size)]
    pub start: Option<u64>,
    /// Copy only a part of the device: length, for example 4G
    #[arg(long, value_name = "LEN", value_parser = parse_size)]
    pub size: Option<u64>,
    /// Plain progress lines instead of the full-screen view
    #[arg(long)]
    pub no_tui: bool,
    /// One JSON object per event on standard output
    #[arg(long)]
    pub json_progress: bool,
    /// Overwrite an existing image that has no mapfile, or continue onto a device that
    /// differs from the one copied before
    #[arg(long)]
    pub force: bool,
    /// Read a device even though it is mounted (not recommended)
    #[arg(long)]
    pub allow_mounted: bool,
    /// Do not check that the output disk has enough free space
    #[arg(long, hide = true)]
    pub no_space_check: bool,
}

/// Which kinds of files to recover.
#[derive(Debug, Args, Clone, Default)]
pub struct CategoryArgs {
    /// Recover photos (JPEG, PNG, HEIC, ...)
    #[arg(long)]
    pub photos: bool,
    /// Recover videos (MP4, MOV, AVI, ...)
    #[arg(long)]
    pub videos: bool,
    /// Recover camera RAW photos (CR2, NEF, ARW, DNG, ...)
    #[arg(long)]
    pub raw: bool,
    /// Recover audio files
    #[arg(long)]
    pub audio: bool,
    /// Recover every file, not just media (default: photos, RAW and videos)
    #[arg(long, conflicts_with_all = ["photos", "videos", "raw", "audio"])]
    pub all_files: bool,
    /// Skip files smaller than this (thumbnails), for example 20k
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub min_size: Option<u64>,
}

/// Existing versus deleted files.
#[derive(Debug, Args, Clone, Default)]
pub struct StateArgs {
    /// Only files that still exist on the filesystem
    #[arg(long, conflicts_with = "deleted_only")]
    pub existing_only: bool,
    /// Only files that were deleted
    #[arg(long)]
    pub deleted_only: bool,
}

/// Carving policy as typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CarveArg {
    /// Free space plus anything the filesystem does not cover (everything when no
    /// filesystem is readable)
    Auto,
    /// Free space of readable filesystems only
    Free,
    /// The whole source, ignoring filesystems
    All,
    /// Do not search the raw data
    Off,
}

/// Repair behaviour.
#[derive(Debug, Args, Clone, Default)]
pub struct RepairOpts {
    /// Do not try to repair damaged files
    #[arg(long)]
    pub no_repair: bool,
    /// A healthy file from the same camera or phone, used as a reference (repeatable)
    #[arg(long, value_name = "FILE")]
    pub donor: Vec<PathBuf>,
    /// Allow repairs that cut damaged parts out
    #[arg(long)]
    pub aggressive: bool,
}

/// `diskrescue scan`
#[derive(Debug, Args)]
#[command(
    long_about = "List what could be recovered from a device or image without writing anything. \
Shows the volumes found, the files that can be recovered through the filesystem and, with --carve, \
the files found by searching the raw data.",
    after_help = "Examples:\n  diskrescue scan card.img\n  diskrescue scan /dev/sdb --deleted-only --limit 0\n  diskrescue scan card.img --carve --json"
)]
pub struct ScanArgs {
    /// Device, partition or image file
    pub source: PathBuf,
    #[command(flatten)]
    pub categories: CategoryArgs,
    #[command(flatten)]
    pub state: StateArgs,
    /// Also search the raw data (slow on large sources)
    #[arg(
        long,
        value_enum,
        value_name = "MODE",
        num_args = 0..=1,
        default_missing_value = "auto"
    )]
    pub carve: Option<CarveArg>,
    /// Show at most this many files in the table (0 = all)
    #[arg(long, default_value_t = 50, value_name = "N")]
    pub limit: usize,
    /// Mapfile of the image (default: SOURCE.map when it exists)
    #[arg(long, value_name = "MAPFILE")]
    pub map: Option<PathBuf>,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}

/// `diskrescue recover`
#[derive(Debug, Args)]
#[command(
    long_about = "Recover photos and videos from a device or an image: existing and deleted files \
by name through the filesystem, then files found by searching the raw data, then check and repair \
what came back.\n\nWith --image-to the device is copied to an image first (careful and resumable) \
and the recovery runs on the image. Without it the device is read directly, which is fine for a \
healthy device (for example after accidental deletion) but not for a failing one.",
    after_help = "\
Output:
  OUT/files/vol1_fat32_CANON/DCIM/100CANON/IMG_0001.JPG   existing files, original paths
  OUT/deleted/vol1_fat32_CANON/DCIM/...                   deleted files found through the filesystem
  OUT/carved/photos/2024/05/f00002c41000_20240501_123011.jpg
  OUT/repaired/...                                        repaired copies (damaged originals are kept)
  OUT/report.txt, OUT/report.json

Examples:
  diskrescue recover card.img -o ~/Recovered
  diskrescue recover /dev/sdb --image-to card.img -o ~/Recovered
  diskrescue recover card.img -o out --videos --donor good_clip.mp4"
)]
pub struct RecoverArgs {
    /// Device, partition or image file
    pub source: PathBuf,
    /// Folder for the recovered files (must not be on the source device)
    #[arg(short, long, value_name = "DIR")]
    pub output: PathBuf,
    #[command(flatten)]
    pub categories: CategoryArgs,
    #[command(flatten)]
    pub state: StateArgs,
    /// How much of the raw data to search for photos and videos
    #[arg(long, value_enum, default_value_t = CarveArg::Auto, value_name = "MODE")]
    pub carve: CarveArg,
    #[command(flatten)]
    pub repair: RepairOpts,
    /// Number of worker threads (default: one per CPU)
    #[arg(short, long, value_name = "N")]
    pub jobs: Option<usize>,
    /// Copy the device to this image first, then recover from the image
    #[arg(long, value_name = "IMAGE")]
    pub image_to: Option<PathBuf>,
    /// Mapfile of the image (default: IMAGE.map, or SOURCE.map for an image source)
    #[arg(long, value_name = "MAPFILE")]
    pub map: Option<PathBuf>,
    /// Plain progress lines while imaging instead of the full-screen view
    #[arg(long)]
    pub no_tui: bool,
    /// Read a device even though it is mounted (not recommended)
    #[arg(long)]
    pub allow_mounted: bool,
    /// Progress and result as JSON lines on standard output
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub tuning: Tuning,
}

/// `diskrescue carve`
#[derive(Debug, Args)]
#[command(
    long_about = "Search the raw data of a device or image for photos and videos, ignoring \
filesystems. Use it when the filesystem is too damaged to read. Files are named by their position \
and, when known, their capture date.",
    after_help = "Examples:\n  diskrescue carve card.img -o out\n  diskrescue carve card.img -o out --free-only --photos"
)]
pub struct CarveArgs {
    /// Device, partition or image file
    pub source: PathBuf,
    /// Folder for the recovered files (must not be on the source device)
    #[arg(short, long, value_name = "DIR")]
    pub output: PathBuf,
    #[command(flatten)]
    pub categories: CategoryArgs,
    /// Search only the free space of readable filesystems (find deleted files)
    #[arg(long)]
    pub free_only: bool,
    #[command(flatten)]
    pub repair: RepairOpts,
    /// Number of worker threads (default: one per CPU)
    #[arg(short, long, value_name = "N")]
    pub jobs: Option<usize>,
    /// Mapfile of the image (default: SOURCE.map when it exists)
    #[arg(long, value_name = "MAPFILE")]
    pub map: Option<PathBuf>,
    /// Read a device even though it is mounted (not recommended)
    #[arg(long)]
    pub allow_mounted: bool,
    /// Progress and result as JSON lines on standard output
    #[arg(long)]
    pub json: bool,
}

/// `diskrescue repair`
#[derive(Debug, Args)]
#[command(
    long_about = "Repair damaged photos and videos. Your files are never modified; repaired copies \
are written to OUT/repaired/. A video that lost its index (\"moov atom not found\") needs a healthy \
clip from the same device and settings as a reference (--donor).",
    after_help = "Examples:\n  diskrescue repair broken.mp4 -o fixed --donor good_clip_same_phone.mp4\n  diskrescue repair IMG_0042.JPG IMG_0043.JPG -o fixed\n  diskrescue repair *.JPG -o fixed"
)]
pub struct RepairArgs {
    /// Files to repair (wildcards like *.JPG are expanded)
    #[arg(required = true, num_args = 1.., value_name = "FILE")]
    pub files: Vec<PathBuf>,
    /// Folder for the repaired copies
    #[arg(short, long, value_name = "DIR")]
    pub output: PathBuf,
    /// A healthy file from the same camera or phone, used as a reference (repeatable)
    #[arg(long, value_name = "FILE")]
    pub donor: Vec<PathBuf>,
    /// Allow repairs that cut damaged parts out
    #[arg(long)]
    pub aggressive: bool,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}

/// `diskrescue verify`
#[derive(Debug, Args)]
#[command(
    long_about = "Check whether photos and videos are intact. Exits with status 1 when any file is \
damaged or not a valid media file, so scripts can use it.",
    after_help = "Examples:\n  diskrescue verify IMG_0042.JPG\n  diskrescue verify *.mp4 --json"
)]
pub struct VerifyArgs {
    /// Files to check (wildcards like *.JPG are expanded)
    #[arg(required = true, num_args = 1.., value_name = "FILE")]
    pub files: Vec<PathBuf>,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}

/// `diskrescue map`
#[derive(Debug, Args)]
#[command(
    long_about = "Show how much of a device has been copied: totals for every status and a picture of \
the whole device. Reads the ddrescue-format mapfile that `diskrescue image` keeps next to the image.",
    after_help = "Legend: + copied   ? not read yet   * failed, to examine   / failed, to retry   - unreadable\n\nExamples:\n  diskrescue map card.img.map\n  diskrescue map card.img --width 100"
)]
pub struct MapArgs {
    /// The mapfile (or the image whose IMAGE.map should be shown)
    pub mapfile: PathBuf,
    /// Width of the picture in characters (default: the terminal width)
    #[arg(long, value_name = "COLUMNS")]
    pub width: Option<usize>,
    /// Print the numbers as JSON
    #[arg(long)]
    pub json: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_recover_with_everything() {
        let cli = Cli::try_parse_from([
            "diskrescue",
            "-vv",
            "recover",
            "/dev/sdb",
            "-o",
            "out",
            "--image-to",
            "card.img",
            "--photos",
            "--videos",
            "--min-size",
            "20k",
            "--carve",
            "all",
            "--donor",
            "a.mp4",
            "--donor",
            "b.mp4",
            "--timeout",
            "5",
            "--block-size",
            "64k",
        ])
        .unwrap();
        assert_eq!(cli.verbose, 2);
        match cli.command {
            Command::Recover(r) => {
                assert_eq!(r.categories.min_size, Some(20 * 1024));
                assert_eq!(r.carve, CarveArg::All);
                assert_eq!(r.repair.donor.len(), 2);
                assert_eq!(r.tuning.block_size, Some(65_536));
                assert_eq!(r.tuning.timeout, Some(Duration::from_secs(5)));
            }
            other => panic!("wrong command {other:?}"),
        }
    }

    #[test]
    fn rejects_conflicting_and_unknown_options() {
        assert!(Cli::try_parse_from([
            "diskrescue",
            "recover",
            "x",
            "-o",
            "o",
            "--all-files",
            "--photos"
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "diskrescue",
            "recover",
            "x",
            "-o",
            "o",
            "--existing-only",
            "--deleted-only"
        ])
        .is_err());
        assert!(
            Cli::try_parse_from(["diskrescue", "image", "a", "b", "--profile", "turbo"]).is_err()
        );
        assert!(
            Cli::try_parse_from(["diskrescue", "image", "a", "b", "--block-size", "huge"]).is_err()
        );
        assert!(Cli::try_parse_from(["diskrescue", "verify"]).is_err());
        assert!(Cli::try_parse_from(["diskrescue", "recover", "x"]).is_err());
    }

    #[test]
    fn scan_carve_flag_may_omit_its_value() {
        let cli = Cli::try_parse_from(["diskrescue", "scan", "x", "--carve"]).unwrap();
        match cli.command {
            Command::Scan(s) => assert_eq!(s.carve, Some(CarveArg::Auto)),
            _ => panic!(),
        }
        let cli = Cli::try_parse_from(["diskrescue", "scan", "x"]).unwrap();
        match cli.command {
            Command::Scan(s) => assert_eq!(s.carve, None),
            _ => panic!(),
        }
    }
}
