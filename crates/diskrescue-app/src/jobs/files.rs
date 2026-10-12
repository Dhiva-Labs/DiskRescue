//! Jobs on loose files: repair and verify.

use std::path::PathBuf;
use std::sync::Arc;

use diskrescue_recover::pipeline::VerifyResult;
use diskrescue_recover::report::{Outcome, Report};

use super::pipeline::{FinishedFile, OutcomeCounts, Pipeline, StateObserver};
use super::{spawn, JobHandle, JobOutcome, JobState};
use crate::plan::{RepairPlan, VerifyPlan};
use crate::preflight::{preflight_repair, preflight_verify};
use crate::recovery;
use crate::results::verify_verdict;

/// What a repair job shows.
#[derive(Debug, Clone, Default)]
pub struct RepairState {
    /// Number of input files (after wildcard expansion).
    pub inputs: usize,
    /// The folder for the repaired copies.
    pub output: PathBuf,
    /// Stage and outcome counters.
    pub pipeline: Pipeline,
    /// Every file finished so far (input path and outcome), in the order
    /// they completed.
    pub files: Arc<Vec<FinishedFile>>,
    /// The final report, with every detail (also written to
    /// `output/report.json`).
    pub report: Option<Arc<Report>>,
    /// The job has ended.
    pub done: bool,
}

impl JobState for RepairState {}

/// What a verify job shows.
#[derive(Debug, Clone, Default)]
pub struct VerifyState {
    /// Number of files to check (after wildcard expansion).
    pub total: usize,
    /// Results so far, in input order.
    pub results: Arc<Vec<VerifyResult>>,
    /// How many of them need attention (damaged, invalid, unrecognised).
    pub problems: usize,
    /// The job has ended.
    pub done: bool,
}

impl JobState for VerifyState {}

/// Repair damaged photos and videos in the background (`diskrescue
/// repair`). Inputs are never modified; repaired copies go to
/// `output/repaired/`. The repair runs as one unit: a stop request is
/// honoured before it starts but not in the middle (inputs are only read,
/// so letting it finish is safe).
pub fn start_repair(plan: RepairPlan) -> JobHandle<RepairState> {
    let output = plan.output.clone();
    spawn(
        "diskrescue-repair",
        false,
        RepairState {
            output: plan.output.clone(),
            ..RepairState::default()
        },
        move || vec![output],
        move |ctx| {
            let pf = preflight_repair(&plan);
            let outcome = (|| {
                if let Some(r) = pf.refusal() {
                    return JobOutcome::Refused(r);
                }
                let inputs = match recovery::check_repair(&plan.files, &plan.output, &plan.donors) {
                    Ok(i) => i,
                    Err(e) => return JobOutcome::from_error(e),
                };
                ctx.update(|s| s.inputs = inputs.len());
                if ctx.shared.is_cancelled() {
                    return JobOutcome::Cancelled;
                }
                let obs = StateObserver {
                    state: ctx.state.clone(),
                    project: |s: &mut RepairState| &mut s.pipeline,
                    on_file: Some(|s: &mut RepairState, name: &str, outcome: Outcome| {
                        Arc::make_mut(&mut s.files).push(FinishedFile {
                            name: name.to_string(),
                            outcome,
                        })
                    }),
                };
                match recovery::run_repair(
                    &inputs,
                    &plan.output,
                    &plan.donors,
                    plan.aggressive,
                    &obs,
                ) {
                    Ok(report) => {
                        ctx.update(|s| {
                            s.pipeline.counts = OutcomeCounts::from_report(&report);
                            s.report = Some(Arc::new(report));
                        });
                        JobOutcome::Finished
                    }
                    Err(e) => JobOutcome::from_error(e),
                }
            })();
            ctx.update(|s| {
                s.done = true;
                s.pipeline.finish();
            });
            outcome
        },
    )
}

/// Check whether files are intact, in the background (`diskrescue
/// verify`). Results appear as they complete (files are checked a few at a
/// time, in parallel); a stop request is honoured between batches.
pub fn start_verify(plan: VerifyPlan) -> JobHandle<VerifyState> {
    spawn(
        "diskrescue-verify",
        false,
        VerifyState::default(),
        Vec::new,
        move |ctx| {
            let pf = preflight_verify(&plan);
            let outcome = (|| {
                if let Some(r) = pf.refusal() {
                    return JobOutcome::Refused(r);
                }
                let files = pf.resolved.clone().unwrap_or_default();
                ctx.update(|s| s.total = files.len());
                let batch = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
                    .max(1);
                for chunk in files.chunks(batch) {
                    if ctx.shared.is_cancelled() {
                        return JobOutcome::Cancelled;
                    }
                    let results = diskrescue_recover::verify_files(chunk);
                    ctx.update(|s| {
                        s.problems += results.iter().filter(|r| verify_verdict(r).1).count();
                        Arc::make_mut(&mut s.results).extend(results);
                    });
                }
                JobOutcome::Finished
            })();
            ctx.update(|s| s.done = true);
            outcome
        },
    )
}
