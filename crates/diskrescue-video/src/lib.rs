//! Video container and audio file formats for DiskRescue: AVI (with index
//! repair), Matroska/WebM, MPEG-TS/M2TS (AVCHD), MPEG program streams (MPG
//! and DVD VOB), and the audio formats a carver must not mistake for
//! something else: MP3, WAV (RIFF and RF64) and FLAC.
//!
//! Every format implements [`diskrescue_core::format::FileFormat`]:
//!
//! * `measure` finds the exact extent of a candidate at a signature hit,
//!   streaming through the source in bounded windows. Containers with a
//!   size field (AVI, Matroska with a known Segment size, WAV, FLAC with a
//!   known sample count, MPEG-PS with an end code) report `Complete`;
//!   formats without an end marker (TS, MP3 without an ID3v1 tag, live
//!   Matroska, MPEG-PS without an end code) report `UnknownLength` with the
//!   length of the run of valid structure; a cut or damaged file reports
//!   `Truncated` with `err_offset`; the caller's limit gives `Capped`.
//! * `validate` re-walks a whole file and reports what is wrong
//!   (`repairable_by` names the AVI strategies that apply).
//! * `repair` exists for AVI only ([`AviFormat::repair_with`] takes an
//!   [`AviStrategy`]).
//!
//! The per-file extension (`webm` or `mka` for Matroska, `ts` or `mts` for
//! transport streams, `vob` or `mpg` for program streams, `mp2` for layer II
//! audio) is reported in `MediaMeta::extra` under [`EXTENSION_KEY`]; every
//! format sets it, so the carver can always prefer it over the static
//! `FileFormat::extensions` list.
//!
//! Use [`formats`] to get all of them.

#![deny(missing_docs)]

use std::sync::Arc;

use diskrescue_core::format::FileFormat;

mod avi;
mod flac;
mod io;
mod meta;
mod mkv;
mod mp3;
mod mpa;
mod mpegps;
mod mpegts;
mod seg;
mod wav;

pub use avi::{AviFormat, AviStrategy};
pub use flac::FlacFormat;
pub use meta::EXTENSION_KEY;
pub use mkv::MkvFormat;
pub use mp3::Mp3Format;
pub use mpegps::MpegPsFormat;
pub use mpegts::MpegTsFormat;
pub use wav::WavFormat;

/// All formats of this crate: `avi`, `mkv`, `mts`, `mpg`, `mp3`, `wav`,
/// `flac`.
pub fn formats() -> Vec<Arc<dyn FileFormat>> {
    vec![
        Arc::new(AviFormat),
        Arc::new(MkvFormat),
        Arc::new(MpegTsFormat),
        Arc::new(MpegPsFormat),
        Arc::new(Mp3Format),
        Arc::new(WavFormat),
        Arc::new(FlacFormat),
    ]
}
