//! Progress of the recovery pipeline (shared by recovery, scan and repair
//! jobs).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diskrescue_recover::report::{Outcome, Report, VolumeRecord};
use diskrescue_recover::{Event, Observer, Stage};

use super::{lock, RECENT};
use crate::text;

/// Counters of one pipeline stage.
#[derive(Debug, Clone, PartialEq)]
pub struct StageProgress {
    /// The stage.
    pub stage: Stage,
    /// Its label (`Searching the raw data for photos and videos`).
    pub label: String,
    /// Items (or bytes, for carving and the lost-volume scan) done.
    pub done: u64,
    /// Items in total (0 = unknown).
    pub total: u64,
    /// Bytes processed, when meaningful.
    pub bytes: u64,
    /// When the stage started.
    pub started: Instant,
    /// How long it took, once the next stage started.
    pub took: Option<Duration>,
}

impl StageProgress {
    /// The label while counters are shown (some stages do two jobs).
    pub fn counting_label(&self) -> String {
        text::stage_progress_label(&self.stage)
    }

    /// Fraction done (0..=1), when the total is known.
    pub fn fraction(&self) -> Option<f64> {
        (self.total > 0).then(|| text::fraction(self.done, self.total))
    }
}

/// Carver progress.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CarveProgress {
    /// Bytes searched.
    pub scanned: u64,
    /// Bytes to search.
    pub total: u64,
    /// Files found so far.
    pub files: u64,
}

/// How many files ended with each outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutcomeCounts {
    /// Recovered and validated.
    pub recovered: u64,
    /// Recovered, not deeply verifiable.
    pub recovered_unverified: u64,
    /// Damaged.
    pub damaged: u64,
    /// Repaired.
    pub repaired: u64,
    /// Partly repaired.
    pub partially_repaired: u64,
    /// Duplicates (not written again).
    pub duplicate: u64,
    /// Could not be recovered.
    pub failed: u64,
}

impl OutcomeCounts {
    /// The exact counts of a finished report.
    pub fn from_report(report: &Report) -> Self {
        let mut c = OutcomeCounts::default();
        for f in &report.files {
            c.add(f.outcome);
        }
        c
    }

    /// Count one more file with outcome `o`.
    pub fn add(&mut self, o: Outcome) {
        *self.slot(o) += 1;
    }

    /// Files with outcome `o`.
    pub fn get(&self, o: Outcome) -> u64 {
        let mut c = *self;
        *c.slot(o)
    }

    /// All files counted.
    pub fn total(&self) -> u64 {
        self.recovered
            + self.recovered_unverified
            + self.damaged
            + self.repaired
            + self.partially_repaired
            + self.duplicate
            + self.failed
    }

    fn slot(&mut self, o: Outcome) -> &mut u64 {
        match o {
            Outcome::Recovered => &mut self.recovered,
            Outcome::RecoveredUnverified => &mut self.recovered_unverified,
            Outcome::Damaged => &mut self.damaged,
            Outcome::Repaired => &mut self.repaired,
            Outcome::PartiallyRepaired => &mut self.partially_repaired,
            Outcome::Duplicate => &mut self.duplicate,
            Outcome::Failed => &mut self.failed,
        }
    }
}

/// A file the pipeline finished with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishedFile {
    /// Output path relative to the output folder (or the input path for
    /// repair).
    pub name: String,
    /// What happened to it.
    pub outcome: Outcome,
}

/// Everything the pipeline reported so far.
#[derive(Debug, Clone, Default)]
pub struct Pipeline {
    /// Current stage.
    pub stage: Option<Stage>,
    /// Its label.
    pub stage_label: String,
    /// Every stage so far, in order, with its counters.
    pub stages: Vec<StageProgress>,
    /// Carver progress, once carving started.
    pub carve: Option<CarveProgress>,
    /// Volumes examined.
    pub volumes: Arc<Vec<VolumeRecord>>,
    /// Warnings of the pipeline.
    pub warnings: Arc<Vec<String>>,
    /// Files per outcome. While the job runs, files are counted as the
    /// pipeline reports them (with the outcome known at that moment: a file
    /// is first counted when written, and again when a repair finishes);
    /// when the job ends the counts are replaced by the exact ones of the
    /// final report.
    pub counts: OutcomeCounts,
    /// The last [`RECENT`] files finished, oldest first.
    pub recent_files: Arc<VecDeque<FinishedFile>>,
}

impl Pipeline {
    /// The current stage's counters.
    pub fn current(&self) -> Option<&StageProgress> {
        self.stages.last()
    }

    fn counters(&mut self, done: u64, total: u64, bytes: u64) {
        let Some(cur) = self.stages.last_mut() else {
            return;
        };
        // Worker threads report in any order: never let the display go backwards.
        if total == cur.total {
            cur.done = cur.done.max(done);
            cur.bytes = cur.bytes.max(bytes);
        } else {
            cur.done = done;
            cur.total = total;
            cur.bytes = bytes;
        }
    }

    /// Take a pipeline event into account.
    pub fn apply(&mut self, e: Event) {
        match e {
            Event::Stage(s) => {
                if let Some(prev) = self.stages.last_mut() {
                    prev.took.get_or_insert(prev.started.elapsed());
                }
                let label = text::stage_label(&s);
                self.stages.push(StageProgress {
                    stage: s.clone(),
                    label: label.clone(),
                    done: 0,
                    total: 0,
                    bytes: 0,
                    started: Instant::now(),
                    took: None,
                });
                self.stage = Some(s);
                self.stage_label = label;
            }
            Event::Volume(v) => Arc::make_mut(&mut self.volumes).push(v),
            Event::Progress { done, total, bytes } => self.counters(done, total, bytes),
            Event::Carve {
                scanned,
                total,
                files,
            } => {
                let c = self.carve.get_or_insert_with(CarveProgress::default);
                if total == c.total {
                    c.scanned = c.scanned.max(scanned);
                    c.files = c.files.max(files);
                } else {
                    *c = CarveProgress {
                        scanned,
                        total,
                        files,
                    };
                }
                if matches!(self.stage, Some(Stage::Carving)) {
                    self.counters(scanned, total, scanned);
                }
            }
            Event::File { name, outcome } => {
                self.counts.add(outcome);
                let recent = Arc::make_mut(&mut self.recent_files);
                if recent.len() >= RECENT {
                    recent.pop_front();
                }
                recent.push_back(FinishedFile { name, outcome });
            }
            Event::Warning(w) => Arc::make_mut(&mut self.warnings).push(w),
        }
    }

    /// Mark the last stage as done.
    pub(crate) fn finish(&mut self) {
        if let Some(prev) = self.stages.last_mut() {
            prev.took.get_or_insert(prev.started.elapsed());
        }
    }
}

/// An [`Observer`] that feeds pipeline events into a part of a job state.
pub(crate) struct StateObserver<S> {
    pub(crate) state: Arc<Mutex<S>>,
    pub(crate) project: fn(&mut S) -> &mut Pipeline,
    pub(crate) on_file: Option<fn(&mut S, &str, Outcome)>,
}

impl<S: Send> Observer for StateObserver<S> {
    fn event(&self, e: Event) {
        let mut s = lock(&self.state);
        if let (Some(f), Event::File { name, outcome }) = (self.on_file, &e) {
            f(&mut s, name, *outcome);
        }
        (self.project)(&mut s).apply(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_never_go_backwards_and_files_are_capped() {
        let mut p = Pipeline::default();
        p.apply(Event::Progress {
            done: 1,
            total: 0,
            bytes: 0,
        });
        p.apply(Event::Stage(Stage::Extracting));
        p.apply(Event::Progress {
            done: 8,
            total: 11,
            bytes: 800,
        });
        p.apply(Event::Progress {
            done: 6,
            total: 11,
            bytes: 600,
        });
        let c = p.current().unwrap();
        assert_eq!((c.done, c.bytes), (8, 800));
        for i in 0..(RECENT + 50) {
            p.apply(Event::File {
                name: format!("f{i}"),
                outcome: if i % 2 == 0 {
                    Outcome::Recovered
                } else {
                    Outcome::Damaged
                },
            });
        }
        assert_eq!(p.recent_files.len(), RECENT);
        assert_eq!(p.counts.total(), (RECENT + 50) as u64);
        assert_eq!(p.counts.get(Outcome::Damaged), (RECENT as u64 + 50) / 2);
        p.apply(Event::Stage(Stage::Carving));
        p.apply(Event::Carve {
            scanned: 10,
            total: 100,
            files: 2,
        });
        assert_eq!(p.current().unwrap().done, 10);
        assert!(p.stages[0].took.is_some());
    }
}
