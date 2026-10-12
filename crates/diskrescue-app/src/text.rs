//! Human labels shared by every front-end: outcomes and their tone, stage
//! and phase names, profile descriptions, partition and file states, and
//! number formatting.
//!
//! Everything returns plain text without colour codes; a front-end maps
//! [`Tone`] to its own colours.

use std::time::Duration;

use diskrescue_core::device::Transport;
use diskrescue_core::format::{ExtentStatus, ValidationStatus};
use diskrescue_core::fs::EntryConfidence;
use diskrescue_core::partition::PartitionScheme;
use diskrescue_core::profile::ImagingProfile;
use diskrescue_core::time::Timestamp;
use diskrescue_imaging::Phase;
use diskrescue_recover::report::{Outcome, VolumeRecord};
use diskrescue_recover::Stage;

pub use diskrescue_core::util::human::{bytes, bytes_decimal, count, duration, rate};

/// How a result should look: front-ends pick their own colours for each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tone {
    /// Good news (recovered, repaired, intact).
    Good,
    /// Needs attention but is not lost (partly repaired, unverified).
    Warn,
    /// Damaged or failed.
    Bad,
    /// Neither (duplicates, information).
    Neutral,
}

// ---------------------------------------------------------------- numbers

/// Percentage with one decimal, never rounding up to 100.0 unless complete.
pub fn percent(part: u64, total: u64) -> String {
    if total == 0 {
        return "100%".into();
    }
    if part >= total {
        return "100%".into();
    }
    let p = part as f64 * 100.0 / total as f64;
    let p = (p * 10.0).floor() / 10.0;
    format!("{p:.1}%")
}

/// Fraction 0..=1 for a gauge.
pub fn fraction(part: u64, total: u64) -> f64 {
    if total == 0 {
        1.0
    } else {
        (part as f64 / total as f64).clamp(0.0, 1.0)
    }
}

/// Plural helper: `1 file`, `2 files` (with thousands separators).
pub fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", count(n), if n == 1 { one } else { many })
}

/// Quote one command-line argument for display in a copy-and-paste command.
pub fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,\\".contains(c));
    if safe {
        arg.to_string()
    } else if cfg!(windows) {
        // cmd.exe and PowerShell both understand double quotes.
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// Size as `29.8 GiB (32 GB)`: binary units plus the label size.
pub fn size_label(bytes_: u64) -> String {
    if bytes_ == 0 {
        return "0 B".to_string();
    }
    if bytes_ < 1000 {
        return bytes(bytes_);
    }
    format!(
        "{} ({})",
        bytes(bytes_),
        diskrescue_device::label_size(bytes_)
    )
}

/// `mm:ss` since the start of a job, for log lines.
pub fn log_stamp(at: Duration) -> String {
    let t = at.as_secs();
    format!("{:02}:{:02}", t / 60, t % 60)
}

/// A capture or modification date as `2024-07-03 10:21`, or `-`.
pub fn date_text(t: Option<Timestamp>) -> String {
    match t {
        Some(ts) => {
            let (y, mo, d, h, mi, _) = ts.civil();
            format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
        }
        None => "-".into(),
    }
}

// ---------------------------------------------------------------- outcomes

/// Label of a recovery outcome (`recovered`, `damaged`, ...).
pub fn outcome_label(o: Outcome) -> &'static str {
    o.label()
}

/// Tone of a recovery outcome.
pub fn outcome_tone(o: Outcome) -> Tone {
    match o {
        Outcome::Recovered | Outcome::Repaired => Tone::Good,
        Outcome::RecoveredUnverified | Outcome::PartiallyRepaired => Tone::Warn,
        Outcome::Damaged | Outcome::Failed => Tone::Bad,
        Outcome::Duplicate => Tone::Neutral,
    }
}

/// What a repair run did with a file, in one phrase.
pub fn repair_result_label(o: Outcome) -> &'static str {
    match o {
        Outcome::Recovered | Outcome::RecoveredUnverified => "no repair needed",
        Outcome::Repaired => "repaired",
        Outcome::PartiallyRepaired => "partly repaired",
        Outcome::Damaged => "could not be repaired",
        Outcome::Failed => "could not be checked",
        Outcome::Duplicate => "duplicate",
    }
}

/// State of a file before repair, from its validation.
pub fn validation_label(status: Option<ValidationStatus>) -> &'static str {
    match status {
        Some(ValidationStatus::Valid) => "intact",
        Some(ValidationStatus::Damaged) => "damaged",
        Some(ValidationStatus::Invalid) => "not valid",
        None => "unrecognised",
    }
}

/// Tone of a validation verdict.
pub fn validation_tone(status: Option<ValidationStatus>) -> Tone {
    match status {
        Some(ValidationStatus::Valid) => Tone::Good,
        Some(ValidationStatus::Damaged) => Tone::Warn,
        Some(ValidationStatus::Invalid) | None => Tone::Bad,
    }
}

// ---------------------------------------------------------------- recovery stages

/// Label shown for a pipeline stage.
pub fn stage_label(s: &Stage) -> String {
    match s {
        Stage::Partitions => "Reading the partition table".into(),
        Stage::Listing { volume, fs } => format!("Reading the file list of volume {volume} ({fs})"),
        Stage::Extracting => "Copying the files found through the filesystem".into(),
        Stage::Carving => "Searching the raw data for photos and videos".into(),
        Stage::Validating => "Checking the files".into(),
        Stage::Repairing => "Repairing damaged files".into(),
        Stage::Report => "Writing the report".into(),
    }
}

/// Label while a stage reports counters (some stages do two jobs: the
/// partition stage may scan the whole source for lost volumes, the listing
/// stage looks at nameless files).
pub fn stage_progress_label(s: &Stage) -> String {
    match s {
        Stage::Partitions => "Searching the whole source for lost volumes".into(),
        Stage::Listing { .. } => "Looking at files that have no recognisable name".into(),
        other => stage_label(other),
    }
}

/// Stable identifier of a stage (`carving`), for JSON and keys.
pub fn stage_id(s: &Stage) -> &'static str {
    match s {
        Stage::Partitions => "partitions",
        Stage::Listing { .. } => "listing",
        Stage::Extracting => "extracting",
        Stage::Carving => "carving",
        Stage::Validating => "validating",
        Stage::Repairing => "repairing",
        Stage::Report => "report",
    }
}

/// Line describing a volume found by the pipeline.
pub fn volume_line(v: &VolumeRecord) -> String {
    let fs =
        v.fs.map(|f| f.name().to_string())
            .unwrap_or_else(|| "no filesystem recognised".into());
    let label = v
        .label
        .as_deref()
        .map(|l| format!(" \"{l}\""))
        .unwrap_or_default();
    let mut s = format!(
        "Volume {}: {fs}{label}, {} at offset {} (found via {})",
        v.index,
        bytes(v.size),
        bytes(v.start),
        v.origin
    );
    if let Some(e) = &v.error {
        s.push_str(&format!("\n  note: {e}"));
    }
    s
}

/// Partitioning scheme of a scanned source.
pub fn scheme_label(s: PartitionScheme) -> &'static str {
    match s {
        PartitionScheme::Mbr => "MBR",
        PartitionScheme::Gpt => "GPT",
        PartitionScheme::None => {
            "none (the filesystem starts at the beginning, like a camera card)"
        }
        PartitionScheme::Unknown => "none found",
    }
}

/// Whether a file found through the filesystem exists or was deleted, and
/// how sure the parser is about its data.
pub fn entry_state_label(c: EntryConfidence, deleted: bool) -> &'static str {
    if !deleted {
        return "existing";
    }
    match c {
        EntryConfidence::Allocated => "existing",
        EntryConfidence::DeletedIntact => "deleted",
        EntryConfidence::DeletedGuessed => "deleted, guessed",
        EntryConfidence::DeletedNoData => "deleted, data gone",
    }
}

/// How the measurement of a carved file ended.
pub fn extent_label(s: ExtentStatus) -> &'static str {
    match s {
        ExtentStatus::Complete => "complete",
        ExtentStatus::Truncated => "cut short",
        ExtentStatus::Capped => "very large",
        ExtentStatus::UnknownLength => "length guessed",
    }
}

/// Name of a bus for device tables.
pub fn transport_label(t: Transport) -> &'static str {
    match t {
        Transport::Ata => "SATA/ATA",
        Transport::Sas => "SAS",
        Transport::Nvme => "NVMe",
        Transport::Usb => "USB",
        Transport::Mmc => "MMC/SD",
        Transport::Virtio => "virtio",
        Transport::Scsi => "SCSI",
        Transport::Firewire => "FireWire",
        Transport::Thunderbolt => "Thunderbolt",
        Transport::Virtual => "virtual",
        Transport::Unknown => "-",
    }
}

// ---------------------------------------------------------------- imaging

fn dir_text(reverse: bool) -> &'static str {
    if reverse {
        "backwards"
    } else {
        "forwards"
    }
}

/// Short phase name (`copying pass 1 (forwards)`).
pub fn phase_label(p: &Phase) -> String {
    match p {
        Phase::Copying { pass, reverse } => format!("copying pass {pass} ({})", dir_text(*reverse)),
        Phase::Trimming => "trimming failed areas".into(),
        Phase::Dividing => "dividing failed areas".into(),
        Phase::Scraping => "scraping failed areas".into(),
        Phase::Retrying { pass, reverse } => {
            format!("retrying pass {pass} ({})", dir_text(*reverse))
        }
        Phase::Finished => "finished".into(),
    }
}

/// Engine messages that only matter in a log file.
pub fn is_engine_chatter(msg: &str) -> bool {
    msg.starts_with("resuming: status")
}

/// "3 read errors, 1 timeout, 0 slow reads, 0 reconnects".
pub fn problems_text(errors: u64, timeouts: u64, slow: u64, reconnects: u64) -> String {
    format!(
        "{}, {}, {}, {}",
        plural(errors, "read error", "read errors"),
        plural(timeouts, "timeout", "timeouts"),
        plural(slow, "slow read", "slow reads"),
        plural(reconnects, "reconnect", "reconnects")
    )
}

/// What an imaging profile is for and does, in one sentence (matches
/// `docs/USAGE.md`).
pub fn profile_blurb(name: &str) -> &'static str {
    match name {
        "hdd" => "64 KiB reads, exponential skipping over bad areas, second pass backwards, sector-level scraping last",
        "usb-hdd" => "as hdd, plus re-opening the device when the USB bridge stalls",
        "ssd" | "nvme" => "large reads, short timeouts, no trimming (flash fails in whole pages), more retry passes",
        "usb-ssd" => "as ssd, with bridge handling",
        "usb-flash" => "small reads, 10 s timeouts, re-opens and waits for the drive to come back after it drops off the bus, re-checks data after errors",
        "sd" => "as usb-flash with 32 KiB reads",
        "optical" => "2 KiB sectors, long timeouts, for CD, DVD and Blu-ray drives",
        _ => "GNU ddrescue's defaults",
    }
}

fn secs(d: Duration) -> String {
    if d.subsec_millis() == 0 {
        format!("{} s", d.as_secs())
    } else {
        format!("{:.1} s", d.as_secs_f64())
    }
}

/// The numbers of a profile that matter, in one line.
pub fn profile_details(p: &ImagingProfile) -> String {
    let mut parts = vec![
        format!("reads of {}", bytes(p.block_size)),
        format!(
            "timeouts {} / {} / {} (copy / scrape / retry)",
            secs(p.timeout_copy),
            secs(p.timeout_scrape),
            secs(p.timeout_retry)
        ),
        format!("{} retry pass(es)", p.retry_passes),
    ];
    parts.push(if p.trim {
        "trimming on".into()
    } else {
        "trimming off".into()
    });
    parts.push(if p.scrape {
        "scraping on".into()
    } else {
        "scraping off".into()
    });
    if p.reopen_after_errors.is_some() {
        parts.push("re-opens the device after repeated errors".into());
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentages_never_round_up_to_done() {
        assert_eq!(percent(99_999, 100_000), "99.9%");
        assert_eq!(percent(1, 3), "33.3%");
        assert_eq!(percent(0, 0), "100%");
        assert_eq!(fraction(5, 10), 0.5);
        assert_eq!(plural(1, "file", "files"), "1 file");
        assert_eq!(plural(1200, "file", "files"), "1,200 files");
    }

    #[test]
    fn every_outcome_has_a_label_and_tone() {
        for o in [
            Outcome::Recovered,
            Outcome::RecoveredUnverified,
            Outcome::Damaged,
            Outcome::Repaired,
            Outcome::PartiallyRepaired,
            Outcome::Duplicate,
            Outcome::Failed,
        ] {
            assert!(!outcome_label(o).is_empty());
            assert!(!repair_result_label(o).is_empty());
            let _ = outcome_tone(o);
        }
        assert_eq!(outcome_tone(Outcome::Damaged), Tone::Bad);
        assert_eq!(outcome_tone(Outcome::Repaired), Tone::Good);
    }

    #[test]
    fn phases_and_profiles_in_plain_words() {
        assert_eq!(
            phase_label(&Phase::Copying {
                pass: 2,
                reverse: true
            }),
            "copying pass 2 (backwards)"
        );
        for n in ImagingProfile::names() {
            let p = ImagingProfile::by_name(n).unwrap();
            assert!(!profile_blurb(n).is_empty());
            assert!(profile_details(&p).contains("reads of"));
        }
        assert_eq!(log_stamp(Duration::from_secs(125)), "02:05");
        assert_eq!(size_label(32_018_267_136), "29.8 GiB (32 GB)");
    }
}
