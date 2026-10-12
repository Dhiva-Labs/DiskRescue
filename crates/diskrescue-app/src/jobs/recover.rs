//! The recovery job (`recover` and `carve`), optionally imaging first.

use std::path::PathBuf;
use std::sync::Arc;

use diskrescue_core::device::DeviceInfo;
use diskrescue_recover::report::Report;

use super::image::{imaging_outcome, run_image, ImageState};
use super::pipeline::{OutcomeCounts, Pipeline, StateObserver};
use super::{lock, spawn, Ctx, JobHandle, JobOutcome, JobState};
use crate::plan::{CarvePlan, RecoverPlan};
use crate::preflight::preflight_job;
use crate::recovery::{self, RecoveryJob};

/// Where a recovery job is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RecoverStatus {
    /// Running the safety checks.
    #[default]
    Checking,
    /// Copying the device into an image first (see [`RecoverState::image`]).
    Imaging,
    /// Opening the source.
    Opening,
    /// The pipeline runs (see [`RecoverState::pipeline`]).
    Recovering,
    /// The job has ended (see the outcome).
    Done,
}

/// What a recovery job shows.
#[derive(Debug, Clone, Default)]
pub struct RecoverState {
    /// Where the job is.
    pub status: RecoverStatus,
    /// The source as described (the device, also while imaging it).
    pub source: Option<DeviceInfo>,
    /// The output folder.
    pub output: PathBuf,
    /// Warnings from the checks and from opening the source.
    pub warnings: Vec<String>,
    /// The imaging step, when the plan images first.
    pub image: Option<ImageState>,
    /// Pipeline progress: stage, counters, carving, volumes, outcomes,
    /// recent files.
    pub pipeline: Pipeline,
    /// The final report (also written to `output/report.json`).
    pub report: Option<Arc<Report>>,
    /// The run was stopped early (the report covers what was found).
    pub interrupted: bool,
    /// A stop was requested.
    pub stopping: bool,
}

impl JobState for RecoverState {
    fn set_paused(&mut self, paused: bool) {
        // Only the imaging step pauses.
        if matches!(
            self.status,
            RecoverStatus::Checking | RecoverStatus::Imaging
        ) {
            if let Some(i) = self.image.as_mut() {
                i.paused = paused;
            }
        }
    }

    fn set_stopping(&mut self) {
        self.stopping = true;
        if let Some(i) = self.image.as_mut() {
            i.stopping = true;
        }
    }
}

impl RecoverState {
    fn warn(&mut self, w: String) {
        if !self.warnings.contains(&w) {
            self.warnings.push(w);
        }
    }
}

/// Recover photos and videos in the background (`diskrescue recover`).
/// With `image_to` the device is imaged first ([`RecoverState::image`]) and
/// recovery reads the image. The job runs [`crate::preflight::preflight_recover`]
/// first and is refused when anything blocks it.
pub fn start_recover(plan: RecoverPlan) -> JobHandle<RecoverState> {
    start_job(plan.job(), "diskrescue-recover")
}

/// Search the raw data only, in the background (`diskrescue carve`).
pub fn start_carve(plan: CarvePlan) -> JobHandle<RecoverState> {
    start_job(plan.job(), "diskrescue-carve")
}

fn start_job(job: RecoveryJob, name: &str) -> JobHandle<RecoverState> {
    let initial = RecoverState {
        output: job.output.clone(),
        image: job.image_plan().map(|p| ImageState::for_plan(&p)),
        ..RecoverState::default()
    };
    let written_job = job.clone();
    spawn(
        name,
        true,
        initial,
        move || written_job.written_paths(),
        move |ctx| {
            let outcome = run_job(&job, ctx);
            ctx.update(|s| {
                s.status = RecoverStatus::Done;
                s.pipeline.finish();
            });
            outcome
        },
    )
}

fn run_job(job: &RecoveryJob, ctx: &Ctx<RecoverState>) -> JobOutcome {
    let pf = preflight_job(job);
    ctx.update(|s| {
        if let Some(r) = &pf.resolved {
            s.source = Some(r.source.clone());
        }
        for w in &pf.warnings {
            s.warn(w.clone());
        }
    });
    if let Some(r) = pf.refusal() {
        return JobOutcome::Refused(r);
    }
    // The same checks, in the same order, as the command line.
    if let Err(e) = job.check() {
        return JobOutcome::from_error(e);
    }

    // Step 1 (optional): copy the device into an image.
    let mut source_path: PathBuf = job.source.clone();
    let mut map_path: Option<PathBuf> = job.map.clone();
    let mut origin = None;
    if let Some(plan) = job.image_plan() {
        if let Err(e) = job.check_output_against_device() {
            return JobOutcome::from_error(e);
        }
        ctx.update(|s| s.status = RecoverStatus::Imaging);
        let state = ctx.state.clone();
        let sink = move |f: &mut dyn FnMut(&mut ImageState)| {
            let mut s = lock(&state);
            f(s.image.get_or_insert_with(ImageState::default))
        };
        let done = match run_image(&plan, &ctx.shared, &sink) {
            Ok(d) => d,
            Err(e) => return JobOutcome::from_error(e),
        };
        if !done.finished() {
            return imaging_outcome(&done);
        }
        source_path = done.image.clone();
        map_path = Some(done.mapfile.clone());
        origin = Some(done.info.clone());
    }
    if ctx.shared.is_cancelled() {
        return JobOutcome::Cancelled;
    }

    // Step 2: open and recover.
    ctx.update(|s| s.status = RecoverStatus::Opening);
    let opened = match job.open(
        &source_path,
        map_path.as_deref(),
        Some(ctx.shared.cancel_flag()),
    ) {
        Ok(o) => o,
        Err(e) => return JobOutcome::from_error(e),
    };
    let warnings = job.warnings(&opened);
    let note = recovery::keep_previous_report(&job.output);
    ctx.update(|s| {
        if origin.is_none() {
            s.source = Some(opened.info.clone());
        }
        for w in warnings {
            s.warn(w);
        }
        if let Some(n) = note {
            s.warn(n);
        }
        s.status = RecoverStatus::Recovering;
    });
    let opts = job.options(ctx.shared.cancel_flag());
    let record = opened.record(origin.as_ref());
    let obs = StateObserver {
        state: ctx.state.clone(),
        project: |s: &mut RecoverState| &mut s.pipeline,
        on_file: None,
    };
    let report = match recovery::run_pipeline(opened.shared.clone(), record, &opts, &obs) {
        Ok(r) => r,
        Err(e) => return JobOutcome::from_error(e),
    };
    let interrupted = report.interrupted || ctx.shared.is_cancelled();
    ctx.update(|s| {
        s.interrupted = interrupted;
        s.pipeline.counts = OutcomeCounts::from_report(&report);
        s.report = Some(Arc::new(report));
    });
    if interrupted {
        JobOutcome::Cancelled
    } else {
        JobOutcome::Finished
    }
}
