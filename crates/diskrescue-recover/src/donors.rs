//! Choosing donor files for repairs.
//!
//! Several repairs need a healthy file from the same camera or phone: a JPEG
//! header transplant needs matching dimensions and sampling, a moov rebuild
//! needs a clip recorded with the same encoder settings. The pool collects
//! every recovered file that validated cleanly and ranks candidates for a
//! damaged file by how closely their metadata matches.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use diskrescue_core::media::MediaMeta;

/// A healthy file that may serve as a donor.
#[derive(Debug, Clone)]
pub struct DonorCandidate {
    /// Absolute path of the healthy file.
    pub path: PathBuf,
    /// Format id (`jpeg`, `mp4`).
    pub format: String,
    /// Its metadata.
    pub meta: MediaMeta,
    /// Size in bytes.
    pub size: u64,
    /// Supplied explicitly by the user (always ranked first).
    pub user_supplied: bool,
}

/// Thread-safe pool of donor candidates, grouped by format.
#[derive(Debug, Default)]
pub struct DonorPool {
    by_format: Mutex<HashMap<String, Vec<DonorCandidate>>>,
}

/// Cap per format so huge recoveries do not keep millions of candidates.
const MAX_PER_FORMAT: usize = 2000;

impl DonorPool {
    /// Empty pool.
    pub fn new() -> Self {
        DonorPool::default()
    }

    /// Add a candidate.
    pub fn add(&self, c: DonorCandidate) {
        let mut map = self.by_format.lock().unwrap_or_else(|p| p.into_inner());
        let list = map.entry(c.format.clone()).or_default();
        if c.user_supplied {
            list.insert(0, c);
        } else if list.len() < MAX_PER_FORMAT {
            list.push(c);
        }
    }

    /// Number of candidates for `format`.
    pub fn count(&self, format: &str) -> usize {
        self.by_format
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(format)
            .map_or(0, Vec::len)
    }

    /// Up to `n` best donors for a damaged file of `format` with metadata
    /// `damaged` (which may be mostly empty when the header is destroyed).
    pub fn pick(&self, format: &str, damaged: &MediaMeta, n: usize) -> Vec<PathBuf> {
        let map = self.by_format.lock().unwrap_or_else(|p| p.into_inner());
        let Some(list) = map.get(format) else {
            return Vec::new();
        };
        let mut scored: Vec<(i64, usize)> = list
            .iter()
            .enumerate()
            .map(|(i, c)| (score(c, damaged), i))
            .collect();
        // Highest score first; ties keep insertion order (stable sort).
        scored.sort_by_key(|&(score, _)| std::cmp::Reverse(score));
        scored
            .into_iter()
            .take(n)
            .map(|(_, i)| list[i].path.clone())
            .collect()
    }
}

fn eq_ci(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.trim().eq_ignore_ascii_case(y.trim()),
        _ => false,
    }
}

fn score(c: &DonorCandidate, d: &MediaMeta) -> i64 {
    let mut s = 0i64;
    if c.user_supplied {
        s += 1000;
    }
    if eq_ci(&c.meta.make, &d.make) {
        s += 20;
    }
    if eq_ci(&c.meta.model, &d.model) {
        s += 40;
    }
    if c.meta.width.is_some() && c.meta.width == d.width && c.meta.height == d.height {
        s += 50;
    }
    if eq_ci(&c.meta.video_codec, &d.video_codec) {
        s += 30;
    }
    if eq_ci(&c.meta.audio_codec, &d.audio_codec) {
        s += 10;
    }
    if eq_ci(&c.meta.software, &d.software) {
        s += 10;
    }
    // Prefer donors captured close in time to the damaged file: same session,
    // same settings. Within a day earns up to 10 points.
    if let (Some(a), Some(b)) = (c.meta.created, d.created) {
        let dt = (a.unix_secs - b.unix_secs).abs();
        if dt < 86_400 {
            s += 10 - (dt / 8_640).min(10);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(path: &str, model: &str, w: u32, user: bool) -> DonorCandidate {
        DonorCandidate {
            path: PathBuf::from(path),
            format: "jpeg".into(),
            meta: MediaMeta {
                model: Some(model.into()),
                width: Some(w),
                height: Some(w * 3 / 4),
                ..Default::default()
            },
            size: 1,
            user_supplied: user,
        }
    }

    #[test]
    fn ranks_by_similarity_and_user_choice() {
        let pool = DonorPool::new();
        pool.add(cand("a", "EOS 80D", 6000, false));
        pool.add(cand("b", "iPhone 12", 4032, false));
        pool.add(cand("c", "iPhone 12", 6000, false));
        let target = MediaMeta {
            model: Some("iphone 12".into()),
            width: Some(4032),
            height: Some(3024),
            ..Default::default()
        };
        assert_eq!(
            pool.pick("jpeg", &target, 2),
            vec![PathBuf::from("b"), PathBuf::from("c")]
        );
        pool.add(cand("u", "other", 1, true));
        assert_eq!(pool.pick("jpeg", &target, 1), vec![PathBuf::from("u")]);
        assert!(pool.pick("mp4", &target, 3).is_empty());
        assert_eq!(pool.count("jpeg"), 4);
    }
}
