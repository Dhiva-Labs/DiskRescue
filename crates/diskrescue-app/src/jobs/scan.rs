//! The scan job: what could be recovered, writing nothing.

use std::sync::Arc;

use diskrescue_core::device::DeviceInfo;
use diskrescue_recover::pipeline::Survey;

use super::pipeline::{Pipeline, StateObserver};
use super::{spawn, JobHandle, JobOutcome, JobState};
use crate::plan::{selection, ScanPlan};
use crate::preflight::preflight_scan;
use crate::recovery;
use crate::source::{self, SourceRequest};

/// What a scan job shows.
#[derive(Debug, Clone, Default)]
pub struct ScanState {
    /// The source as opened.
    pub source: Option<DeviceInfo>,
    /// Warnings from opening the source.
    pub warnings: Vec<String>,
    /// Pipeline progress (stages, volumes, carving).
    pub pipeline: Pipeline,
    /// The result: volumes, files found through the filesystems and, when
    /// carving was asked for, files found in the raw data.
    pub survey: Option<Arc<Survey>>,
    /// The scan was stopped early (the survey is partial).
    pub interrupted: bool,
    /// The job has ended.
    pub done: bool,
}

impl JobState for ScanState {}

/// List what can be recovered from a source, in the background
/// (`diskrescue scan`). Nothing is written anywhere.
pub fn start_scan(plan: ScanPlan) -> JobHandle<ScanState> {
    spawn(
        "diskrescue-scan",
        false,
        ScanState::default(),
        Vec::new,
        move |ctx| {
            let pf = preflight_scan(&plan);
            let outcome = (|| {
                ctx.update(|s| {
                    s.source = pf.resolved.clone();
                    s.warnings.extend(pf.warnings.iter().cloned());
                });
                if let Some(r) = pf.refusal() {
                    return JobOutcome::Refused(r);
                }
                let mut req = SourceRequest::reading(&plan.source);
                req.map = plan.map.as_deref();
                req.cancel = Some(ctx.shared.cancel_flag());
                let opened = match source::open(&req) {
                    Ok(o) => o,
                    Err(e) => return JobOutcome::from_error(e),
                };
                ctx.update(|s| {
                    s.source = Some(opened.info.clone());
                    for w in &opened.warnings {
                        if !s.warnings.contains(w) {
                            s.warnings.push(w.clone());
                        }
                    }
                });
                let sel = selection(&plan.categories, &plan.state);
                let obs = StateObserver {
                    state: ctx.state.clone(),
                    project: |s: &mut ScanState| &mut s.pipeline,
                    on_file: None,
                };
                match recovery::run_survey(
                    opened.shared.clone(),
                    &sel,
                    plan.carve_policy(),
                    ctx.shared.cancel_flag(),
                    &obs,
                ) {
                    Ok(survey) => {
                        let interrupted = ctx.shared.is_cancelled();
                        ctx.update(|s| {
                            s.survey = Some(Arc::new(survey));
                            s.interrupted = interrupted;
                        });
                        if interrupted {
                            JobOutcome::Cancelled
                        } else {
                            JobOutcome::Finished
                        }
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
