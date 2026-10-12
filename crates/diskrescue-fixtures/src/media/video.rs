//! Video container generator driven by `ffmpeg`.
//!
//! Inputs are synthetic (`testsrc2` + `sine` from lavfi), so no input files
//! are needed. Output is made bit-exact (`-fflags +bitexact`, fixed
//! `creation_time`) so equal options give equal bytes on the same ffmpeg
//! build. The seed shifts the picture hue and the tone frequency, so two
//! seeds produce files with identical stream parameters (SPS/PPS, track
//! layout) but different samples, which is what donor-based repair tests
//! need.
//!
//! Supported container/codec combinations are listed by [`Container::supports`]
//! and [`VideoOpts::check`]; WebM maps any audio request to Opus.
//!
//! Needs: `ffmpeg` (with libx264, libx265, libvpx, libmp3lame as asked for).

use crate::error::{FixtureError, Result};
use crate::tools;

/// Output container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Container {
    /// ISO BMFF `mp4`.
    Mp4,
    /// QuickTime `mov`.
    Mov,
    /// 3GPP `3gp`.
    ThreeGp,
    /// RIFF AVI.
    Avi,
    /// Matroska.
    Mkv,
    /// WebM.
    WebM,
    /// MPEG transport stream (188-byte packets).
    MpegTs,
    /// Blu-ray style transport stream (192-byte packets, 4-byte timecode).
    M2ts,
    /// MPEG program stream.
    MpegPs,
}

/// Video codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    /// H.264 (libx264).
    H264,
    /// H.265 (libx265).
    Hevc,
    /// MPEG-4 part 2.
    Mpeg4,
    /// Motion JPEG.
    Mjpeg,
    /// VP8 (libvpx).
    Vp8,
    /// VP9 (libvpx-vp9).
    Vp9,
    /// MPEG-2 video.
    Mpeg2,
}

/// Audio codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    /// AAC-LC.
    Aac,
    /// MP3 (libmp3lame).
    Mp3,
    /// 16-bit PCM.
    Pcm,
    /// No audio track.
    None,
}

impl Container {
    /// File extension conventionally used for this container.
    pub fn extension(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mov => "mov",
            Container::ThreeGp => "3gp",
            Container::Avi => "avi",
            Container::Mkv => "mkv",
            Container::WebM => "webm",
            Container::MpegTs => "ts",
            Container::M2ts => "m2ts",
            Container::MpegPs => "mpg",
        }
    }

    /// Whether this container can carry the given codec pair (WebM accepts
    /// any audio request and substitutes Opus).
    pub fn supports(self, v: VideoCodec, a: AudioCodec) -> bool {
        use AudioCodec as A;
        use Container as C;
        use VideoCodec as V;
        let video_ok = match self {
            C::Mp4 => matches!(v, V::H264 | V::Hevc | V::Mpeg4 | V::Vp9),
            C::Mov => matches!(v, V::H264 | V::Hevc | V::Mpeg4 | V::Mjpeg),
            C::ThreeGp => matches!(v, V::H264 | V::Mpeg4),
            C::Avi => matches!(v, V::H264 | V::Mpeg4 | V::Mjpeg),
            C::Mkv => true,
            C::WebM => matches!(v, V::Vp8 | V::Vp9),
            C::MpegTs | C::M2ts => matches!(v, V::H264 | V::Hevc | V::Mpeg2),
            C::MpegPs => matches!(v, V::Mpeg2),
        };
        let audio_ok = match self {
            C::Mp4 | C::ThreeGp | C::MpegTs | C::M2ts => matches!(a, A::Aac | A::Mp3 | A::None),
            C::Mov => true,
            C::Avi => true,
            C::Mkv => true,
            C::WebM => true,
            C::MpegPs => matches!(a, A::Mp3 | A::None),
        };
        video_ok && audio_ok
    }

    fn muxer_args(self) -> &'static [&'static str] {
        match self {
            Container::Mp4 => &["-f", "mp4"],
            Container::Mov => &["-f", "mov"],
            Container::ThreeGp => &["-f", "3gp"],
            Container::Avi => &["-f", "avi"],
            Container::Mkv => &["-f", "matroska"],
            Container::WebM => &["-f", "webm"],
            Container::MpegTs => &["-f", "mpegts"],
            Container::M2ts => &["-f", "mpegts", "-mpegts_m2ts_mode", "1"],
            Container::MpegPs => &["-f", "mpeg"],
        }
    }
}

/// Options for [`video`].
#[derive(Debug, Clone, PartialEq)]
pub struct VideoOpts {
    /// Container format.
    pub container: Container,
    /// Video codec.
    pub video: VideoCodec,
    /// Audio codec.
    pub audio: AudioCodec,
    /// Duration in seconds.
    pub seconds: f32,
    /// Frames per second (clamped to 1..=240).
    pub fps: u32,
    /// Width (rounded down to an even number, clamped to 16..=4096).
    pub width: u32,
    /// Height (rounded down to an even number, clamped to 16..=4096).
    pub height: u32,
    /// Put `moov` before `mdat` (MP4/MOV/3GP only).
    pub faststart: bool,
    /// Write a fragmented MP4 (`frag_keyframe+empty_moov`).
    pub fragmented: bool,
    /// Content seed (hue and tone).
    pub seed: u64,
}

impl Default for VideoOpts {
    fn default() -> Self {
        VideoOpts {
            container: Container::Mp4,
            video: VideoCodec::H264,
            audio: AudioCodec::Aac,
            seconds: 2.0,
            fps: 25,
            width: 320,
            height: 240,
            faststart: false,
            fragmented: false,
            seed: 1,
        }
    }
}

impl VideoOpts {
    /// Reject combinations ffmpeg cannot mux.
    pub fn check(&self) -> Result<()> {
        if !self.container.supports(self.video, self.audio) {
            return Err(FixtureError::Unsupported(format!(
                "{:?} cannot carry {:?} + {:?}",
                self.container, self.video, self.audio
            )));
        }
        if self.fps == 0 || self.seconds <= 0.0 {
            return Err(FixtureError::invalid("fps and seconds must be positive"));
        }
        Ok(())
    }
}

fn is_iso(c: Container) -> bool {
    matches!(c, Container::Mp4 | Container::Mov | Container::ThreeGp)
}

/// Generate a video file by running ffmpeg. Returns `Err(Missing)` when
/// ffmpeg (or the needed encoder) is not installed.
pub fn video(opts: &VideoOpts) -> Result<Vec<u8>> {
    opts.check()?;
    tools::require("ffmpeg")?;
    let need = match opts.video {
        VideoCodec::H264 => Some("libx264"),
        VideoCodec::Hevc => Some("libx265"),
        VideoCodec::Vp8 => Some("libvpx"),
        VideoCodec::Vp9 => Some("libvpx-vp9"),
        _ => None,
    };
    if let Some(enc) = need {
        if !tools::ffmpeg_has_encoder(enc) {
            return Err(tools::Missing(format!("ffmpeg encoder {enc}")).into());
        }
    }
    if opts.audio == AudioCodec::Mp3 && !tools::ffmpeg_has_encoder("libmp3lame") {
        return Err(tools::Missing("ffmpeg encoder libmp3lame".into()).into());
    }
    if opts.container == Container::WebM
        && opts.audio != AudioCodec::None
        && !tools::ffmpeg_has_encoder("libopus")
    {
        return Err(tools::Missing("ffmpeg encoder libopus".into()).into());
    }

    let w = (opts.width.clamp(16, 4096) / 2) * 2;
    let h = (opts.height.clamp(16, 4096) / 2) * 2;
    let fps = opts.fps.clamp(1, 240);
    let secs = format!("{:.3}", opts.seconds.clamp(0.04, 3600.0));
    let hue = (opts.seed.wrapping_mul(47) % 360).to_string();
    let freq = 220 + (opts.seed % 12) * 40;
    let vsrc = format!("testsrc2=size={w}x{h}:rate={fps}:duration={secs},hue=h={hue}");
    let asrc = format!(
        "sine=frequency={freq}:sample_rate=48000:duration={secs},aformat=channel_layouts=stereo"
    );
    let gop = fps.to_string();

    let mut args: Vec<String> = Vec::new();
    let mut push = |a: &[&str]| args.extend(a.iter().map(|s| s.to_string()));
    push(&["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
    push(&["-f", "lavfi", "-i", &vsrc]);
    if opts.audio != AudioCodec::None {
        push(&["-f", "lavfi", "-i", &asrc]);
    }
    push(&["-t", &secs]);
    push(&[
        "-fflags",
        "+bitexact",
        "-flags:v",
        "+bitexact",
        "-flags:a",
        "+bitexact",
    ]);
    push(&[
        "-map_metadata",
        "-1",
        "-metadata",
        "creation_time=2024-05-06T07:08:09Z",
    ]);
    match opts.video {
        VideoCodec::H264 => push(&[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "30",
            "-g",
            &gop,
            "-pix_fmt",
            "yuv420p",
        ]),
        VideoCodec::Hevc => {
            push(&[
                "-c:v",
                "libx265",
                "-preset",
                "ultrafast",
                "-crf",
                "32",
                "-x265-params",
                &format!("log-level=none:keyint={gop}:min-keyint={gop}"),
                "-pix_fmt",
                "yuv420p",
            ]);
            if is_iso(opts.container) {
                push(&["-tag:v", "hvc1"]);
            }
        }
        VideoCodec::Mpeg4 => push(&["-c:v", "mpeg4", "-q:v", "6", "-g", &gop]),
        VideoCodec::Mjpeg => push(&["-c:v", "mjpeg", "-q:v", "6", "-pix_fmt", "yuvj420p"]),
        VideoCodec::Vp8 => push(&[
            "-c:v",
            "libvpx",
            "-deadline",
            "good",
            "-cpu-used",
            "5",
            "-threads",
            "1",
            "-b:v",
            "300k",
            "-g",
            &gop,
        ]),
        VideoCodec::Vp9 => push(&[
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "good",
            "-cpu-used",
            "5",
            "-threads",
            "1",
            "-b:v",
            "300k",
            "-g",
            &gop,
        ]),
        VideoCodec::Mpeg2 => push(&[
            "-c:v",
            "mpeg2video",
            "-q:v",
            "6",
            "-g",
            &gop,
            "-pix_fmt",
            "yuv420p",
        ]),
    }
    match (opts.audio, opts.container) {
        (AudioCodec::None, _) => push(&["-an"]),
        (_, Container::WebM) => push(&["-c:a", "libopus", "-b:a", "64k"]),
        (AudioCodec::Aac, _) => push(&["-c:a", "aac", "-b:a", "96k"]),
        (AudioCodec::Mp3, _) => push(&["-c:a", "libmp3lame", "-b:a", "96k"]),
        (AudioCodec::Pcm, _) => push(&["-c:a", "pcm_s16le"]),
    }
    if is_iso(opts.container) {
        let mut flags = Vec::new();
        if opts.faststart {
            flags.push("+faststart");
        }
        if opts.fragmented {
            flags.push("+frag_keyframe+empty_moov");
        }
        if !flags.is_empty() {
            push(&["-movflags", &flags.join("")]);
        }
    }
    push(opts.container.muxer_args());

    let dir = tempfile::tempdir()?;
    let out = dir
        .path()
        .join(format!("out.{}", opts.container.extension()));
    args.push(out.to_string_lossy().into_owned());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    tools::run("ffmpeg", &refs)?;
    Ok(std::fs::read(&out)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_table_is_sane() {
        assert!(Container::Mp4.supports(VideoCodec::H264, AudioCodec::Aac));
        assert!(!Container::Mp4.supports(VideoCodec::Vp8, AudioCodec::Aac));
        assert!(!Container::WebM.supports(VideoCodec::H264, AudioCodec::None));
        let bad = VideoOpts {
            container: Container::WebM,
            video: VideoCodec::H264,
            ..Default::default()
        };
        assert!(matches!(bad.check(), Err(FixtureError::Unsupported(_))));
        let zero = VideoOpts {
            fps: 0,
            ..Default::default()
        };
        assert!(zero.check().is_err());
    }
}
