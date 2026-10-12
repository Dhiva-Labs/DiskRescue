//! Metadata extracted from media files (used for naming, sorting and reports).

use std::collections::BTreeMap;

use crate::time::Timestamp;

/// Broad category of a recovered file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaCategory {
    Photo,
    RawPhoto,
    Video,
    Audio,
    Document,
    Other,
}

impl MediaCategory {
    /// Directory name used when organising output.
    pub fn dir_name(&self) -> &'static str {
        match self {
            MediaCategory::Photo => "photos",
            MediaCategory::RawPhoto => "raw",
            MediaCategory::Video => "videos",
            MediaCategory::Audio => "audio",
            MediaCategory::Document => "documents",
            MediaCategory::Other => "other",
        }
    }
}

/// Metadata a format parser could extract without fully decoding the file.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MediaMeta {
    /// Pixel width.
    pub width: Option<u32>,
    /// Pixel height.
    pub height: Option<u32>,
    /// Duration in milliseconds (video/audio).
    pub duration_ms: Option<u64>,
    /// Video codec name (`h264`, `hevc`, `mjpeg`, ...).
    pub video_codec: Option<String>,
    /// Audio codec name (`aac`, `pcm`, ...).
    pub audio_codec: Option<String>,
    /// Camera make.
    pub make: Option<String>,
    /// Camera model.
    pub model: Option<String>,
    /// Software / encoder string.
    pub software: Option<String>,
    /// Best guess at when the media was captured.
    pub created: Option<Timestamp>,
    /// EXIF orientation (1..8).
    pub orientation: Option<u8>,
    /// Anything else worth reporting.
    pub extra: BTreeMap<String, String>,
}

impl MediaMeta {
    /// Fill `None` fields from `other` (keeps existing values).
    pub fn merge_missing(&mut self, other: &MediaMeta) {
        macro_rules! take {
            ($f:ident) => {
                if self.$f.is_none() {
                    self.$f = other.$f.clone();
                }
            };
        }
        take!(width);
        take!(height);
        take!(duration_ms);
        take!(video_codec);
        take!(audio_codec);
        take!(make);
        take!(model);
        take!(software);
        take!(created);
        take!(orientation);
        for (k, v) in &other.extra {
            self.extra.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }

    /// `WxH` string when both dimensions are known.
    pub fn dimensions(&self) -> Option<String> {
        Some(format!("{}x{}", self.width?, self.height?))
    }
}
