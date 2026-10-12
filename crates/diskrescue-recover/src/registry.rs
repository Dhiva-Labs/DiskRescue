//! The set of file formats DiskRescue knows.

use std::sync::Arc;

use diskrescue_core::format::{FileFormat, FormatRegistry};
use diskrescue_core::media::MediaCategory;

use crate::filters::Selection;

/// Every format, for validation and repair.
pub fn all_formats() -> FormatRegistry {
    let mut reg = FormatRegistry::new();
    reg.register(Arc::new(diskrescue_jpeg::JpegFormat));
    reg.register(Arc::new(diskrescue_png::PngFormat));
    for f in diskrescue_stills::formats() {
        reg.register(f);
    }
    for f in diskrescue_isobmff::formats() {
        reg.register(f);
    }
    for f in diskrescue_video::formats() {
        reg.register(f);
    }
    reg
}

/// Formats the carver should look for, given what the user selected.
///
/// Audio formats are always registered with the carver even when audio was
/// not requested: an MP3 or FLAC stream that is not claimed by its own
/// format would otherwise be scanned for embedded cover-art JPEGs and
/// produce junk. Unwanted categories are dropped after carving instead.
pub fn carving_formats(sel: &Selection) -> FormatRegistry {
    let mut reg = FormatRegistry::new();
    for f in all_formats().all() {
        let cat = f.category();
        if sel.wants_category(cat) || cat == MediaCategory::Audio || is_container(f.as_ref()) {
            reg.register(f.clone());
        }
    }
    reg
}

/// Containers that commonly embed photos (so must be carved to keep their
/// embedded images from being emitted as separate files).
fn is_container(f: &dyn FileFormat) -> bool {
    matches!(f.category(), MediaCategory::Video | MediaCategory::RawPhoto)
}
