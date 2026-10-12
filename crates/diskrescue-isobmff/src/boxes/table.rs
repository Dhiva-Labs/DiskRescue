//! Sample N -> file offset arithmetic (`stsc` x `stco` x `stsz`), decode
//! and composition times (`stts`, `ctts`) and sync flags (`stss`).
//!
//! The arithmetic follows ISO/IEC 14496-12 §8.7: `stsc` runs are expanded
//! over the chunk list, each chunk's samples are laid out back to back from
//! its `stco` offset. Inconsistent tables (fewer chunk slots than samples,
//! more `stsc` samples than `stsz` entries) are clipped to the smaller
//! count and reported; structural errors (a `stsc` run starting at chunk 0
//! or not increasing) are errors.

use diskrescue_core::{Error, Result};

use super::Trak;

/// Upper bound on the samples a single table may describe.
pub const MAX_SAMPLES: u64 = 50_000_000;

/// Largest constant-size track (`stsz` with one size for all samples)
/// that [`sample_table`] lists sample by sample; longer ones (PCM audio)
/// go through [`chunk_runs`].
pub const MAX_CONSTANT_LISTED: u64 = 1 << 20;

/// One sample of a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Absolute file offset.
    pub offset: u64,
    /// Size in bytes.
    pub size: u32,
    /// Decode time in media ticks.
    pub dts: u64,
    /// Composition offset (`cts = dts + cts_offset`).
    pub cts_offset: i32,
    /// Duration in media ticks (from `stts`).
    pub duration: u32,
    /// Sync sample.
    pub keyframe: bool,
    /// 0-based chunk index.
    pub chunk: u32,
    /// 0-based sample index.
    pub index: u32,
    /// Sample description index (1-based) from `stsc`.
    pub sdi: u32,
}

impl Sample {
    /// One past the last byte.
    pub fn end(&self) -> u64 {
        self.offset.saturating_add(self.size as u64)
    }
}

/// Consistency findings of [`sample_table_checked`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableCheck {
    /// Samples declared by `stsz`.
    pub declared: u64,
    /// Samples the chunk map could place.
    pub placed: u64,
    /// Samples declared by `stts`.
    pub stts_samples: u64,
    /// Chunk offsets are non-decreasing.
    pub offsets_monotone: bool,
    /// Human-readable problems.
    pub problems: Vec<String>,
}

/// Build the sample table of a track. See [`sample_table_checked`] for the
/// variant that also reports inconsistencies.
pub fn sample_table(trak: &Trak) -> Result<Vec<Sample>> {
    sample_table_checked(trak).map(|(s, _)| s)
}

/// Build the sample table and report table inconsistencies.
pub fn sample_table_checked(trak: &Trak) -> Result<(Vec<Sample>, TableCheck)> {
    let declared = trak.stsz.count() as u64;
    if declared > MAX_SAMPLES {
        return Err(Error::corrupt(
            "stsz box",
            format!("{declared} samples exceeds the {MAX_SAMPLES} limit"),
        ));
    }
    if matches!(trak.stsz, super::SampleSizes::Constant { .. }) && declared > MAX_CONSTANT_LISTED {
        // A few bytes of stsz can declare millions of equal-size samples
        // (uncompressed PCM has one per audio frame): use chunk_runs.
        return Err(Error::corrupt(
            "stsz box",
            format!("{declared} constant-size samples: too many to list one by one"),
        ));
    }
    let mut check = TableCheck {
        declared,
        offsets_monotone: true,
        ..Default::default()
    };
    // Validate stsc structure.
    let mut prev_first = 0u32;
    for (i, &(first, spc, _)) in trak.stsc.iter().enumerate() {
        if first == 0 || first <= prev_first {
            return Err(Error::corrupt(
                "stsc box",
                format!("entry {i} first_chunk {first} not increasing"),
            ));
        }
        if spc > 10_000_000 {
            return Err(Error::corrupt(
                "stsc box",
                format!("entry {i} samples_per_chunk {spc} implausible"),
            ));
        }
        prev_first = first;
    }
    if trak.stsc.first().is_some_and(|e| e.0 != 1) {
        check
            .problems
            .push("stsc does not start at chunk 1".to_string());
    }
    let chunk_count = trak.stco.len();
    let mut out: Vec<Sample> = Vec::with_capacity(declared.min(1 << 20) as usize);
    // stts / ctts / stss cursors.
    let mut stts_i = 0usize;
    let mut stts_left = trak.stts.first().map(|e| e.0).unwrap_or(0);
    let last_delta = trak.stts.last().map(|e| e.1).unwrap_or(0);
    let ctts = trak.ctts.as_deref().unwrap_or(&[]);
    let mut ctts_i = 0usize;
    let mut ctts_left = ctts.first().map(|e| e.0).unwrap_or(0);
    let stss = trak.stss.as_deref();
    let mut stss_i = 0usize;
    let mut dts = 0u64;
    let mut n: u32 = 0;
    let mut stsc_i = 0usize;
    let mut prev_off = 0u64;
    'chunks: for c in 0..chunk_count {
        let chunk_no = c as u32 + 1;
        while stsc_i + 1 < trak.stsc.len() && trak.stsc[stsc_i + 1].0 <= chunk_no {
            stsc_i += 1;
        }
        let Some(&(first, spc, sdi)) = trak.stsc.get(stsc_i) else {
            break;
        };
        if first > chunk_no {
            // Chunks before the first stsc run carry no samples.
            continue;
        }
        let mut off = trak.stco[c];
        if off < prev_off {
            check.offsets_monotone = false;
        }
        prev_off = off;
        for _ in 0..spc {
            if n as u64 >= declared {
                break 'chunks;
            }
            let size = trak.stsz.get(n).unwrap_or(0);
            // Durations.
            while stts_left == 0 && stts_i + 1 < trak.stts.len() {
                stts_i += 1;
                stts_left = trak.stts[stts_i].0;
            }
            let duration = if stts_left > 0 {
                stts_left -= 1;
                trak.stts[stts_i].1
            } else {
                last_delta
            };
            while ctts_left == 0 && ctts_i + 1 < ctts.len() {
                ctts_i += 1;
                ctts_left = ctts[ctts_i].0;
            }
            let cts_offset = if ctts_left > 0 {
                ctts_left -= 1;
                ctts[ctts_i].1
            } else {
                0
            };
            let keyframe = match stss {
                None => true,
                Some(list) => {
                    while stss_i < list.len() && (list[stss_i] as u64) < n as u64 + 1 {
                        stss_i += 1;
                    }
                    stss_i < list.len() && list[stss_i] == n + 1
                }
            };
            out.push(Sample {
                offset: off,
                size,
                dts,
                cts_offset,
                duration,
                keyframe,
                chunk: c as u32,
                index: n,
                sdi,
            });
            off = off.saturating_add(size as u64);
            dts = dts.saturating_add(duration as u64);
            n += 1;
        }
    }
    check.placed = out.len() as u64;
    check.stts_samples = trak.stts.iter().map(|e| e.0 as u64).sum();
    if check.placed < declared {
        check.problems.push(format!(
            "chunk map places {} of {} samples",
            check.placed, declared
        ));
    }
    if check.stts_samples != declared {
        check.problems.push(format!(
            "stts covers {} samples, stsz declares {}",
            check.stts_samples, declared
        ));
    }
    if !check.offsets_monotone {
        check
            .problems
            .push("chunk offsets are not monotone".to_string());
    }
    Ok((out, check))
}

/// One chunk of a track, without its samples expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRun {
    /// Absolute file offset of the chunk.
    pub offset: u64,
    /// Index (0-based) of its first sample.
    pub first_sample: u64,
    /// Samples in the chunk (clipped to the declared sample count).
    pub samples: u32,
}

/// The chunks of a track (`stsc` x `stco`) without expanding the samples,
/// for tracks whose samples are too many to list (uncompressed PCM, where
/// every audio frame is a sample: an hour at 48 kHz is 173 million).
/// Chunks after the declared sample count is reached are not listed.
pub fn chunk_runs(trak: &Trak) -> Result<Vec<ChunkRun>> {
    let declared = trak.stsz.count() as u64;
    let mut prev_first = 0u32;
    for (i, &(first, _, _)) in trak.stsc.iter().enumerate() {
        if first == 0 || first <= prev_first {
            return Err(Error::corrupt(
                "stsc box",
                format!("entry {i} first_chunk {first} not increasing"),
            ));
        }
        prev_first = first;
    }
    let mut out = Vec::with_capacity(trak.stco.len().min(1 << 20));
    let mut stsc_i = 0usize;
    let mut n = 0u64;
    for (c, &off) in trak.stco.iter().enumerate() {
        if n >= declared {
            break;
        }
        let chunk_no = c as u32 + 1;
        while stsc_i + 1 < trak.stsc.len() && trak.stsc[stsc_i + 1].0 <= chunk_no {
            stsc_i += 1;
        }
        let Some(&(first, spc, _)) = trak.stsc.get(stsc_i) else {
            break;
        };
        if first > chunk_no {
            continue;
        }
        let k = (spc as u64).min(declared - n) as u32;
        out.push(ChunkRun {
            offset: off,
            first_sample: n,
            samples: k,
        });
        n += k as u64;
    }
    Ok(out)
}

/// End (absolute) of the last byte any sample of `trak` occupies, without
/// listing more than [`MAX_SAMPLES`] samples: constant-size tracks are
/// measured chunk by chunk. `None` when the tables are unusable.
pub fn track_extent(trak: &Trak) -> Option<u64> {
    match trak.stsz {
        super::SampleSizes::Constant { size, .. } => chunk_runs(trak)
            .ok()?
            .iter()
            .map(|c| c.offset.saturating_add(c.samples as u64 * size as u64))
            .max(),
        _ => sample_table(trak).ok()?.iter().map(Sample::end).max(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::SampleSizes;

    #[test]
    fn arithmetic() {
        let t = Trak {
            stsc: vec![(1, 2, 1), (2, 1, 1)],
            stco: vec![100, 500, 900],
            stsz: SampleSizes::Table(vec![10, 20, 30, 40]),
            stts: vec![(4, 512)],
            ctts: Some(vec![(1, 1024), (3, 0)]),
            stss: Some(vec![1, 4]),
            ..Default::default()
        };
        let (s, check) = sample_table_checked(&t).unwrap();
        assert_eq!(s.len(), 4);
        assert_eq!(
            s.iter().map(|x| (x.offset, x.size)).collect::<Vec<_>>(),
            vec![(100, 10), (110, 20), (500, 30), (900, 40)]
        );
        assert_eq!(s[3].dts, 1536);
        assert_eq!(s[0].cts_offset, 1024);
        assert!(s[0].keyframe && !s[1].keyframe && s[3].keyframe);
        assert!(check.problems.is_empty());
    }

    #[test]
    fn inconsistent_tables_are_clipped_or_rejected() {
        let t = Trak {
            stsc: vec![(1, 2, 1)],
            stco: vec![0],
            stsz: SampleSizes::Constant { size: 4, count: 5 },
            stts: vec![(5, 1)],
            ..Default::default()
        };
        let (s, check) = sample_table_checked(&t).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(check.problems.len(), 1);
        let bad = Trak {
            stsc: vec![(2, 1, 1), (2, 1, 1)],
            ..t.clone()
        };
        assert!(sample_table(&bad).is_err());
        let huge = Trak {
            stsz: SampleSizes::Constant {
                size: 1,
                count: u32::MAX,
            },
            ..t
        };
        assert!(sample_table(&huge).is_err());
    }
}
