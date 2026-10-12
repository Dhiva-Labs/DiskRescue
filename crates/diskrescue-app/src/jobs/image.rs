//! The imaging job.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diskrescue_core::device::DeviceInfo;
use diskrescue_core::profile::ImagingProfile;
use diskrescue_imaging::{Control, Event, Phase, Progress, Stats, StatusMap, Summary};

use super::{spawn, JobHandle, JobOutcome, JobState, Shared, RECENT};
use crate::error::Result;
use crate::imaging::{self, ImagingDone, Prepared, Setup};
use crate::plan::ImagePlan;
use crate::preflight::preflight_image;
use crate::text::{self, percent};

/// Where an imaging job is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ImageStatus {
    /// Running the safety checks.
    #[default]
    Checking,
    /// Opening the source and the image.
    Opening,
    /// Copying.
    Copying,
    /// The job has ended (see the outcome).
    Done,
}

/// One message of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// Time since the job started.
    pub at: Duration,
    /// Severity.
    pub level: log::Level,
    /// The message.
    pub text: String,
}

impl LogLine {
    /// `mm:ss  text`.
    pub fn display(&self) -> String {
        format!("{}  {}", text::log_stamp(self.at), self.text)
    }
}

/// What an imaging job shows.
#[derive(Debug, Clone, Default)]
pub struct ImageState {
    /// Where the job is.
    pub status: ImageStatus,
    /// The source, once described (as opened while copying).
    pub source: Option<DeviceInfo>,
    /// The image file.
    pub image: PathBuf,
    /// The progress map.
    pub mapfile: PathBuf,
    /// The strategy in use.
    pub profile: Option<ImagingProfile>,
    /// Bytes this copy covers.
    pub copy_size: u64,
    /// Totals of the earlier copy being continued.
    pub resumed_from: Option<Stats>,
    /// Warnings from opening the source.
    pub warnings: Vec<String>,
    /// Current phase.
    pub phase: Option<Phase>,
    /// Latest progress report of the engine.
    pub progress: Option<Progress>,
    /// Latest block-status map (about four snapshots a second while
    /// copying; read from the progress map every few seconds when the copy
    /// runs in the privileged helper).
    pub map: Option<Arc<StatusMap>>,
    /// The last [`RECENT`] messages.
    pub logs: Arc<VecDeque<LogLine>>,
    /// The device stopped responding and the engine waits for it.
    pub device_lost: bool,
    /// Where the device was lost.
    pub device_lost_at: Option<u64>,
    /// How long the device was gone the last time it came back.
    pub device_back_after: Option<Duration>,
    /// The engine's own pause to let the device recover (last one).
    pub engine_pause: Option<Duration>,
    /// Paused by the user.
    pub paused: bool,
    /// A stop was requested; the engine is saving and stopping.
    pub stopping: bool,
    /// When the progress map was last saved.
    pub last_saved: Option<Instant>,
    /// The engine's summary when the run ended.
    pub summary: Option<Summary>,
    /// The progress map says the image was complete already: nothing done.
    pub already_complete: bool,
    /// Totals at the end of an already complete image.
    pub final_stats: Option<Stats>,
    /// The command that resumes this copy from a terminal.
    pub resume_command: String,
    /// When the job started.
    pub started: Option<Instant>,
}

impl JobState for ImageState {
    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    fn set_stopping(&mut self) {
        self.stopping = true;
    }
}

impl ImageState {
    /// The state before the job starts.
    pub fn for_plan(plan: &ImagePlan) -> Self {
        ImageState {
            image: plan.image.clone(),
            mapfile: plan.mapfile(),
            resume_command: plan.command_text(),
            started: Some(Instant::now()),
            ..ImageState::default()
        }
    }

    /// The latest totals (from the last progress report, the summary, or an
    /// already complete map).
    pub fn stats(&self) -> Option<Stats> {
        self.summary
            .as_ref()
            .map(|s| s.stats)
            .or_else(|| self.progress.as_ref().map(|p| p.stats))
            .or(self.final_stats)
            .or(self.resumed_from)
    }

    /// Add a message to the log.
    pub fn log(&mut self, level: log::Level, text: impl Into<String>) {
        let at = self.started.map(|t| t.elapsed()).unwrap_or_default();
        let logs = Arc::make_mut(&mut self.logs);
        if logs.len() >= RECENT {
            logs.pop_front();
        }
        logs.push_back(LogLine {
            at,
            level,
            text: text.into(),
        });
    }

    /// Take an engine event into account.
    pub fn apply(&mut self, ev: &Event) {
        match ev {
            Event::Progress(p) => {
                self.phase = Some(p.phase);
                self.progress = Some(p.clone());
                self.engine_pause = None;
                if self.status != ImageStatus::Done {
                    self.status = ImageStatus::Copying;
                }
            }
            Event::Phase(p) => {
                self.phase = Some(*p);
                self.log(log::Level::Info, p.to_string());
            }
            Event::Log(_, msg) if text::is_engine_chatter(msg) => {}
            Event::Log(level, msg) => self.log(*level, msg.clone()),
            Event::DeviceLost { at } => {
                self.device_lost = true;
                self.device_lost_at = Some(*at);
                self.log(
                    log::Level::Warn,
                    format!(
                        "The device stopped responding at {}. Waiting for it to come back...",
                        text::bytes(*at)
                    ),
                );
            }
            Event::DeviceBack { after } => {
                self.device_lost = false;
                self.device_back_after = Some(*after);
                self.log(
                    log::Level::Info,
                    format!(
                        "The device is back after {}; continuing.",
                        text::duration(*after)
                    ),
                );
            }
            Event::Paused(d) => {
                self.engine_pause = Some(*d);
                self.log(
                    log::Level::Info,
                    format!(
                        "Pausing for {} to let the device recover.",
                        text::duration(*d)
                    ),
                );
            }
            Event::Map(m) => self.map = Some(m.clone()),
            Event::Saved => self.last_saved = Some(Instant::now()),
            Event::Finished(s) => {
                self.summary = Some(s.clone());
                self.device_lost = false;
            }
        }
    }
}

/// The outcome of an imaging run: finished, stopped by the user, or
/// stopped before it was complete.
pub(crate) fn imaging_outcome(done: &ImagingDone) -> JobOutcome {
    if done.cancelled() {
        return JobOutcome::Cancelled;
    }
    if done.finished() {
        return JobOutcome::Finished;
    }
    let st = &done.stats;
    let mut msg = format!(
        "The copy stopped before it was complete ({} copied). Start it again to continue where it \
         left off.",
        percent(st.rescued, st.total())
    );
    if let Some(w) = done.summary.as_ref().and_then(|s| s.warnings.last()) {
        msg.push(' ');
        msg.push_str(w);
    }
    JobOutcome::Failed(msg)
}

/// Applies a change to the [`ImageState`] a run reports into.
pub(crate) type ImageSink = dyn Fn(&mut dyn FnMut(&mut ImageState));

/// Run the imaging step of `plan` on the current thread, reporting into an
/// [`ImageState`] through `sink`.
pub(crate) fn run_image(
    plan: &ImagePlan,
    shared: &Shared,
    sink: &ImageSink,
) -> Result<ImagingDone> {
    sink(&mut |s| s.status = ImageStatus::Opening);
    let setup = Setup {
        cancel: Some(shared.cancel_flag()),
        map_snapshot_interval: Some(Duration::from_millis(250)),
    };
    let prepared = imaging::prepare(plan, &setup, &mut |w| {
        sink(&mut |s| {
            for x in w {
                if !s.warnings.contains(x) {
                    s.warnings.push(x.clone());
                }
            }
        })
    })?;
    let ready = match prepared {
        Prepared::AlreadyComplete(done) => {
            sink(&mut |s| {
                s.source = Some(done.info.clone());
                s.already_complete = true;
                s.final_stats = Some(done.stats);
                s.mapfile = done.mapfile.clone();
                s.status = ImageStatus::Done;
            });
            return Ok(*done);
        }
        Prepared::Ready(r) => r,
    };
    let imaging::Ready {
        mut imager,
        info,
        profile,
        image,
        mapfile,
        resumed,
        before,
        copy_size,
        ..
    } = *ready;
    let first_map = Arc::new(imager.mapfile().map.clone());
    sink(&mut |s| {
        s.source = Some(info.clone());
        s.profile = Some(profile.clone());
        s.copy_size = copy_size;
        s.resumed_from = resumed.then_some(before);
        s.mapfile = mapfile.clone();
        s.map = Some(first_map.clone());
        s.status = ImageStatus::Copying;
        let line = format!(
            "Copying {} to {} with the {} profile: {}",
            info.path.display(),
            image.display(),
            profile.name,
            text::profile_blurb(&profile.name)
        );
        s.log(log::Level::Info, line);
    });
    let control = Control::new();
    shared.attach_control(control.clone());
    let result = imager.run(control, &mut |ev| sink(&mut |s| s.apply(&ev)));
    shared.detach_control();
    sink(&mut |s| s.status = ImageStatus::Done);
    match result {
        Ok(summary) => Ok(ImagingDone::from_summary(summary, info, image, mapfile)),
        Err(e) => Err(imaging::engine_error(&mapfile, e)),
    }
}

/// Image `plan.source` into `plan.image` in the background. The job runs
/// [`preflight_image`] first and is refused when anything blocks it.
/// [`JobHandle::pause`] pauses between reads; [`JobHandle::cancel`] saves
/// the progress map and stops (start the same plan again to resume).
pub fn start_image(plan: ImagePlan) -> JobHandle<ImageState> {
    let written_plan = plan.clone();
    spawn(
        "diskrescue-image",
        true,
        ImageState::for_plan(&plan),
        move || written_plan.written_paths(),
        move |ctx| {
            let pf = preflight_image(&plan);
            ctx.update(|s| {
                if let Some(r) = &pf.resolved {
                    s.source = Some(r.source.clone());
                    s.profile = r.profile.clone();
                    s.copy_size = r.copy_size;
                }
                for w in &pf.warnings {
                    if !s.warnings.contains(w) {
                        s.warnings.push(w.clone());
                    }
                }
            });
            if let Some(r) = pf.refusal() {
                ctx.update(|s| s.status = ImageStatus::Done);
                return JobOutcome::Refused(r);
            }
            let state = ctx.state.clone();
            let sink = move |f: &mut dyn FnMut(&mut ImageState)| f(&mut super::lock(&state));
            let res = run_image(&plan, &ctx.shared, &sink);
            ctx.update(|s| s.status = ImageStatus::Done);
            match res {
                Ok(done) => imaging_outcome(&done),
                Err(e) => JobOutcome::from_error(e),
            }
        },
    )
}
