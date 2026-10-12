//! A running job: starting it, polling its state about ten times a second,
//! and drawing the imaging dashboard or the recovery progress.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diskrescue_app::devices;
use diskrescue_app::helper;
use diskrescue_app::jobs::{
    self, ImageState, RecoverState, RecoverStatus, RepairState, ScanState, VerifyState,
};
use diskrescue_app::plan::{CarveMode, ImagePlan, RecoverPlan};
use diskrescue_app::results::verify_verdict;
use diskrescue_app::text::{
    self, bytes, duration, fraction, outcome_label, outcome_tone, percent, plural,
    stage_progress_label,
};
use diskrescue_app::{JobHandle, JobOutcome};
use diskrescue_recover::pipeline::Survey;
use diskrescue_recover::report::Report;
use diskrescue_recover::Stage;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use ratatui::Frame;

use crate::config::Elevation;
use crate::dashboard::{render_image_dashboard, DashboardView, Header};
use crate::model::{Overrides, Plan};
use crate::widgets::{ellipsize_start, fit, wrap_words, Theme};

/// The background job.
pub enum Job {
    /// Copying a device.
    Image(JobHandle<ImageState>),
    /// Recovering (maybe copying first).
    Recover(JobHandle<RecoverState>),
    /// Previewing.
    Scan(JobHandle<ScanState>),
    /// Repairing loose files.
    Repair(JobHandle<RepairState>),
    /// Checking loose files.
    Verify(JobHandle<VerifyState>),
}

/// The latest state of the job.
#[derive(Clone)]
pub enum Snap {
    /// Nothing polled yet.
    None,
    /// Copying.
    Image(Box<ImageState>),
    /// Recovering.
    Recover(Box<RecoverState>),
    /// Previewing.
    Scan(Box<ScanState>),
    /// Repairing.
    Repair(Box<RepairState>),
    /// Checking.
    Verify(Box<VerifyState>),
}

/// What the person sees when the job is over, and where Enter leads.
#[derive(Clone)]
pub enum Next {
    /// Nothing to browse.
    None,
    /// The recovered files.
    Results {
        /// The output folder.
        output: PathBuf,
        /// The report.
        report: Arc<Report>,
    },
    /// The preview.
    Scan {
        /// The preview.
        survey: Arc<Survey>,
        /// The source.
        source: PathBuf,
    },
    /// The verdicts.
    Verify,
    /// Recover from the finished image.
    Image(PathBuf),
}

/// Start the copy of a device, directly or through the helper.
fn start_copy(plan: ImagePlan, via: Option<&Elevation>) -> Result<JobHandle<ImageState>, String> {
    match via {
        None | Some(Elevation::Off) => Ok(jobs::start_image(plan)),
        Some(Elevation::Pkexec) => {
            helper::start_image_privileged(plan).map_err(|e| e.message().to_string())
        }
        Some(Elevation::Program(cli)) => Ok(helper::start_image_via_cli(
            plan,
            cli.clone(),
            helper::Elevation::None,
        )),
    }
}

/// A job and everything the screen shows about it.
pub struct Run {
    /// Heading.
    pub title: String,
    /// The operation, kept for the resume hint.
    pub plan: Plan,
    /// The job.
    pub job: Job,
    /// Its latest state.
    pub snap: Snap,
    /// How it ended.
    pub outcome: Option<JobOutcome>,
    /// What to do after a copy that was only the first step.
    pub followup: Option<RecoverPlan>,
    /// Step number and count.
    pub step: (usize, usize),
    /// The folder with the results.
    pub output: Option<PathBuf>,
    /// The copy is made through the privileged helper.
    pub privileged: bool,
    /// When it started.
    pub started: Instant,
    /// How long it took, once it has ended.
    pub ended: Option<Duration>,
    /// The person asked to stop.
    pub stop_requested: bool,
    /// Step 1 ended; the same job continues as step 2.
    pub notice: Option<String>,
    carve: bool,
    repair: bool,
}

impl Run {
    /// Start `plan` on a background thread. With `via` the copy of a device
    /// is made by the helper program (which asks for administrator rights
    /// when `via` is [`Elevation::Pkexec`]).
    pub fn start(plan: Plan, ov: Overrides, via: Option<Elevation>) -> Result<Run, String> {
        let privileged = via.is_some();
        let title = plan.title();
        let mut followup = None;
        let mut steps = 1;
        let mut output = plan.output();
        let mut carve = false;
        let mut repair = false;
        let job = match plan.clone() {
            Plan::Image(mut p) => {
                p.allow_mounted |= ov.allow_mounted;
                p.force |= ov.force;
                p.no_space_check |= ov.skip_space;
                Job::Image(start_copy(p, via.as_ref())?)
            }
            Plan::Recover(mut p) => {
                p.allow_mounted |= ov.allow_mounted;
                carve = p.carve != CarveMode::Off;
                repair = !p.repair.no_repair;
                match p.image_to.clone() {
                    Some(image) if privileged || ov.force || ov.skip_space => {
                        // Two steps: copy (the plan's own override flags are not
                        // reachable through the combined job), then recover from
                        // the finished image.
                        let img = ImagePlan {
                            source: p.source.clone(),
                            image: image.clone(),
                            map: p.map.clone(),
                            tuning: p.tuning.clone(),
                            start: None,
                            size: None,
                            force: ov.force,
                            allow_mounted: p.allow_mounted,
                            no_space_check: ov.skip_space,
                            command_line: None,
                        };
                        let then = RecoverPlan {
                            source: image,
                            map: Some(img.mapfile()),
                            image_to: None,
                            allow_mounted: false,
                            tuning: Default::default(),
                            ..p.clone()
                        };
                        steps = 2;
                        followup = Some(then);
                        Job::Image(start_copy(img, via.as_ref())?)
                    }
                    _ => Job::Recover(jobs::start_recover(p)),
                }
            }
            Plan::Scan(p) => {
                carve = p.carve.is_some();
                Job::Scan(jobs::start_scan(p))
            }
            Plan::Repair(p) => {
                output = Some(p.output.clone());
                Job::Repair(jobs::start_repair(p))
            }
            Plan::Verify(p) => Job::Verify(jobs::start_verify(p)),
        };
        Ok(Run {
            title,
            plan,
            job,
            snap: Snap::None,
            outcome: None,
            followup,
            step: (1, steps),
            output,
            privileged,
            started: Instant::now(),
            ended: None,
            stop_requested: false,
            notice: None,
            carve,
            repair,
        })
    }

    /// True until the job (and its second step) has ended.
    pub fn is_running(&self) -> bool {
        self.outcome.is_none()
    }

    /// The job can be paused.
    pub fn can_pause(&self) -> bool {
        match &self.job {
            Job::Image(h) => h.can_pause(),
            Job::Recover(h) => h.can_pause() && self.in_imaging_step(),
            _ => false,
        }
    }

    fn in_imaging_step(&self) -> bool {
        matches!(&self.snap, Snap::Recover(s) if s.status == RecoverStatus::Imaging)
    }

    /// The job is paused.
    pub fn is_paused(&self) -> bool {
        match &self.job {
            Job::Image(h) => h.is_paused(),
            Job::Recover(h) => h.is_paused(),
            _ => false,
        }
    }

    /// Pause or resume.
    pub fn toggle_pause(&mut self) {
        if !self.can_pause() {
            return;
        }
        let now = !self.is_paused();
        match &self.job {
            Job::Image(h) => h.pause(now),
            Job::Recover(h) => h.pause(now),
            _ => {}
        }
    }

    /// Ask the job to stop and save what it has.
    pub fn stop(&mut self) {
        self.stop_requested = true;
        match &self.job {
            Job::Image(h) => h.cancel(),
            Job::Recover(h) => h.cancel(),
            Job::Scan(h) => h.cancel(),
            Job::Repair(h) => h.cancel(),
            Job::Verify(h) => h.cancel(),
        }
    }

    /// Take a new snapshot of the job; start the second step when the first
    /// one ended well.
    pub fn tick(&mut self) {
        let (finished, outcome) = match &self.job {
            Job::Image(h) => {
                self.snap = Snap::Image(Box::new(h.snapshot()));
                (h.is_finished(), h.outcome())
            }
            Job::Recover(h) => {
                self.snap = Snap::Recover(Box::new(h.snapshot()));
                (h.is_finished(), h.outcome())
            }
            Job::Scan(h) => {
                self.snap = Snap::Scan(Box::new(h.snapshot()));
                (h.is_finished(), h.outcome())
            }
            Job::Repair(h) => {
                self.snap = Snap::Repair(Box::new(h.snapshot()));
                (h.is_finished(), h.outcome())
            }
            Job::Verify(h) => {
                self.snap = Snap::Verify(Box::new(h.snapshot()));
                (h.is_finished(), h.outcome())
            }
        };
        if !finished || self.outcome.is_some() {
            return;
        }
        if let (Some(JobOutcome::Finished), Some(then), Job::Image(_)) =
            (&outcome, &self.followup, &self.job)
        {
            let then = then.clone();
            self.followup = None;
            self.step = (2, self.step.1);
            self.job = Job::Recover(jobs::start_recover(then));
            self.snap = Snap::None;
            return;
        }
        self.outcome = outcome;
        self.ended = Some(self.started.elapsed());
    }

    /// Where Enter leads once the job has ended.
    pub fn next(&self) -> Next {
        let Some(out) = &self.outcome else {
            return Next::None;
        };
        match &self.snap {
            Snap::Recover(s) => match (&s.report, out) {
                (Some(r), JobOutcome::Finished | JobOutcome::Cancelled) => Next::Results {
                    output: s.output.clone(),
                    report: r.clone(),
                },
                _ => Next::None,
            },
            Snap::Repair(s) => match (&s.report, out) {
                (Some(r), JobOutcome::Finished) => Next::Results {
                    output: s.output.clone(),
                    report: r.clone(),
                },
                _ => Next::None,
            },
            Snap::Scan(s) => match (&s.survey, out) {
                (Some(sv), JobOutcome::Finished | JobOutcome::Cancelled) => Next::Scan {
                    survey: sv.clone(),
                    source: self.plan.source().map(PathBuf::from).unwrap_or_default(),
                },
                _ => Next::None,
            },
            Snap::Verify(_) if matches!(out, JobOutcome::Finished | JobOutcome::Cancelled) => {
                Next::Verify
            }
            Snap::Image(s)
                if matches!(out, JobOutcome::Finished) && !s.image.as_os_str().is_empty() =>
            {
                Next::Image(s.image.clone())
            }
            _ => Next::None,
        }
    }

    /// The verification results, once the job is a verify job.
    pub fn verify_state(&self) -> Option<&VerifyState> {
        match &self.snap {
            Snap::Verify(s) => Some(s),
            _ => None,
        }
    }

    /// A short line about the progress, for the banner on the home screen.
    pub fn one_line(&self) -> String {
        let state = match &self.outcome {
            Some(JobOutcome::Finished) => "finished".to_string(),
            Some(JobOutcome::Cancelled) => "stopped".to_string(),
            Some(JobOutcome::Refused(_)) => "not started".to_string(),
            Some(JobOutcome::Failed(_)) => "failed".to_string(),
            None => match &self.snap {
                Snap::Image(s) => image_percent(s),
                Snap::Recover(s) => match (&s.image, s.status) {
                    (Some(i), RecoverStatus::Imaging) => format!("copying, {}", image_percent(i)),
                    _ => s.pipeline.stage_label.to_lowercase(),
                },
                Snap::Scan(s) => s.pipeline.stage_label.to_lowercase(),
                Snap::Repair(s) => s.pipeline.stage_label.to_lowercase(),
                Snap::Verify(s) => format!("{} of {}", s.results.len(), s.total),
                Snap::None => "starting".to_string(),
            },
        };
        format!("{}: {}", self.title, state)
    }

    /// Draw the job into `area` (the part of the screen between the header
    /// and the footer).
    pub fn draw(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let top_h = self.final_panel_height(area.width);
        let [top, body] =
            Layout::vertical([Constraint::Length(top_h), Constraint::Min(1)]).areas(area);
        if top_h > 0 {
            self.draw_final_panel(f, top, theme);
        }
        match &self.snap {
            Snap::None => {
                f.render_widget(
                    Paragraph::new(vec![
                        Line::from(Span::styled(self.title.clone(), theme.bold())),
                        Line::raw("Starting..."),
                    ]),
                    body,
                );
            }
            Snap::Image(s) => self.draw_dashboard(f, body, s, theme, self.step_label()),
            Snap::Recover(s) => {
                if s.status == RecoverStatus::Imaging
                    || (s.status == RecoverStatus::Checking && s.image.is_some())
                {
                    if let Some(img) = &s.image {
                        self.draw_dashboard(
                            f,
                            body,
                            img,
                            theme,
                            Some(
                                "Step 1 of 2: copying the device (recovery from the copy follows)"
                                    .to_string(),
                            ),
                        );
                        return;
                    }
                }
                self.draw_progress(f, body, theme, ProgressView::from_recover(s, self));
            }
            Snap::Scan(s) => self.draw_progress(f, body, theme, ProgressView::from_scan(s, self)),
            Snap::Repair(s) => {
                self.draw_progress(f, body, theme, ProgressView::from_repair(s, self))
            }
            Snap::Verify(s) => self.draw_verify(f, body, s, theme),
        }
    }

    fn step_label(&self) -> Option<String> {
        (self.step.1 > 1).then(|| {
            format!(
                "Step {} of {}: copying the device (recovery follows)",
                self.step.0, self.step.1
            )
        })
    }

    fn draw_dashboard(
        &self,
        f: &mut Frame,
        area: Rect,
        s: &ImageState,
        theme: &Theme,
        step: Option<String>,
    ) {
        let mut notes: Vec<(String, ratatui::style::Style)> = Vec::new();
        if let Some(t) = step {
            notes.push((t, theme.accent()));
        }
        if s.stopping {
            notes.push((
                " Saving what has been done and stopping... ".to_string(),
                theme.warn(),
            ));
        } else if s.paused {
            notes.push((
                " PAUSED: nothing is being read. Press p to continue. ".to_string(),
                theme
                    .warn()
                    .add_modifier(ratatui::style::Modifier::REVERSED),
            ));
        }
        let rest = if !notes.is_empty() && area.height > 20 {
            let n = notes.len() as u16;
            let [a, b] = Layout::vertical([Constraint::Length(n), Constraint::Min(1)]).areas(area);
            let lines: Vec<Line<'static>> = notes
                .into_iter()
                .map(|(t, st)| Line::from(Span::styled(t, st)))
                .collect();
            f.render_widget(Paragraph::new(lines), a);
            b
        } else {
            area
        };
        let header = image_header(s, &self.plan);
        render_image_dashboard(
            f,
            rest,
            &DashboardView {
                header: &header,
                state: s,
                color: theme.color,
                stop_key: "s",
                can_pause: self.can_pause()
                    || matches!(self.job, Job::Image(_)) && !self.privileged,
                footer: false,
            },
        );
    }

    fn final_panel_height(&self, width: u16) -> u16 {
        match &self.outcome {
            None => 0,
            Some(o) => {
                let text = self.outcome_text(o);
                let w = width.saturating_sub(4) as usize;
                let n: usize = text.iter().map(|t| wrap_words(t, w).len()).sum();
                (n as u16 + 3).min(12)
            }
        }
    }

    fn outcome_text(&self, o: &JobOutcome) -> Vec<String> {
        let mut v = Vec::new();
        match o {
            JobOutcome::Finished => {
                v.push(self.finished_text());
                v.push(self.next_hint());
            }
            JobOutcome::Cancelled => {
                v.push("Stopped. Everything done so far is saved.".to_string());
                if let Some(c) = self.resume_command() {
                    v.push(format!(
                        "To continue later, run the same operation again, or from a terminal: {c}"
                    ));
                }
                v.push(self.next_hint());
            }
            JobOutcome::Refused(m) => {
                v.push("Nothing was started: it would not have been safe.".to_string());
                v.push(crate::model::plain_words(m));
                v.push("Press Esc to go back and change the settings.".to_string());
            }
            JobOutcome::Failed(m) => {
                v.push("It did not finish.".to_string());
                v.push(crate::model::plain_words(m));
                v.push("Press Esc to go back. Nothing was written to the device.".to_string());
            }
        }
        v
    }

    fn resume_command(&self) -> Option<String> {
        match &self.snap {
            Snap::Image(s) if !s.resume_command.is_empty() => Some(s.resume_command.clone()),
            Snap::Recover(s) => s
                .image
                .as_ref()
                .map(|i| i.resume_command.clone())
                .filter(|c| !c.is_empty()),
            _ => None,
        }
    }

    fn finished_text(&self) -> String {
        match &self.snap {
            Snap::Image(s) => match s.stats() {
                Some(st) if s.already_complete => format!(
                    "The image was already complete ({} copied). Nothing had to be read again.",
                    bytes(st.total())
                ),
                Some(st) => format!(
                    "Copy complete: {} copied, {} could not be read.",
                    bytes(st.rescued),
                    bytes(st.bad_sectors)
                ),
                None => "Copy complete.".to_string(),
            },
            Snap::Recover(s) => {
                let c = &s.pipeline.counts;
                format!(
                    "Done: {} recovered, {} damaged, {} repaired, {} duplicates skipped.",
                    c.recovered + c.recovered_unverified,
                    c.damaged + c.failed,
                    c.repaired + c.partially_repaired,
                    c.duplicate
                )
            }
            Snap::Scan(s) => match &s.survey {
                Some(sv) => format!(
                    "Preview complete: {} found through the filesystem, {} found in the raw data. Nothing was written.",
                    plural(sv.files.len() as u64, "file", "files"),
                    sv.carved.len()
                ),
                None => "Preview complete.".to_string(),
            },
            Snap::Repair(s) => {
                let c = &s.pipeline.counts;
                format!(
                    "Done: {} repaired, {} partly repaired, {} could not be repaired, {} needed no repair.",
                    c.repaired,
                    c.partially_repaired,
                    c.damaged + c.failed,
                    c.recovered + c.recovered_unverified
                )
            }
            Snap::Verify(s) => format!(
                "Checked {}: {} need attention.",
                plural(s.total as u64, "file", "files"),
                s.problems
            ),
            Snap::None => "Done.".to_string(),
        }
    }

    fn next_hint(&self) -> String {
        match self.next() {
            Next::Results { .. } => {
                "Press Enter to browse the results, o to open the folder, h for the home screen."
                    .to_string()
            }
            Next::Scan { .. } => {
                "Press Enter to see what was found, h for the home screen.".to_string()
            }
            Next::Verify => "Press Enter to see the verdicts, h for the home screen.".to_string(),
            Next::Image(_) => {
                "Press Enter to recover files from the image, h for the home screen.".to_string()
            }
            Next::None => "Press h for the home screen.".to_string(),
        }
    }

    fn draw_final_panel(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let Some(o) = &self.outcome else { return };
        let (style, mark, head) = match o {
            JobOutcome::Finished => (theme.good(), "+", "Finished"),
            JobOutcome::Cancelled => (theme.warn(), "!", "Stopped"),
            JobOutcome::Refused(_) => (theme.bad(), "x", "Not started"),
            JobOutcome::Failed(_) => (theme.bad(), "x", "Failed"),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(style)
            .title(Span::styled(format!(" {mark} {head} "), style));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let w = inner.width.saturating_sub(1) as usize;
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (i, t) in self.outcome_text(o).iter().enumerate() {
            for l in wrap_words(t, w) {
                lines.push(if i == 0 && matches!(o, JobOutcome::Finished) {
                    Line::from(Span::styled(l, theme.bold()))
                } else {
                    Line::raw(l)
                });
            }
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_verify(&self, f: &mut Frame, area: Rect, s: &VerifyState, theme: &Theme) {
        let [head, list] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(area);
        let ratio = if s.total == 0 {
            0.0
        } else {
            fraction(s.results.len() as u64, s.total as u64)
        };
        let lines = vec![
            Line::from(Span::styled(self.title.clone(), theme.bold())),
            Line::raw(format!(
                "{} of {} checked, {} need attention",
                s.results.len(),
                s.total,
                s.problems
            )),
        ];
        let [t, g] = Layout::vertical([Constraint::Length(2), Constraint::Length(1)]).areas(head);
        f.render_widget(Paragraph::new(lines), t);
        f.render_widget(
            Gauge::default()
                .ratio(ratio)
                .gauge_style(theme.good())
                .label(percent(s.results.len() as u64, s.total.max(1) as u64)),
            g,
        );
        let h = list.height as usize;
        let rows: Vec<Line<'static>> = s
            .results
            .iter()
            .rev()
            .take(h)
            .rev()
            .map(|r| {
                let (verdict, bad) = verify_verdict(r);
                let st = if bad { theme.bad() } else { theme.good() };
                Line::from(vec![
                    Span::styled(format!("{} ", fit(verdict, 12)), st),
                    Span::raw(ellipsize_start(
                        &r.path.display().to_string(),
                        list.width as usize - 13,
                    )),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(rows), list);
    }

    fn draw_progress(&self, f: &mut Frame, area: Rect, theme: &Theme, v: ProgressView<'_>) {
        let width = area.width as usize;
        let steps = v.steps(self.carve, self.repair);
        let volumes: Vec<String> = v
            .pipeline
            .volumes
            .iter()
            .map(|vol| {
                text::volume_line(vol)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        let vol_rows = volumes.len().min(2) as u16;
        let carve_row = u16::from(v.pipeline.carve.is_some() && !v.finished);
        let [head, steps_a, counts, vols, recent] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(steps.len() as u16),
            Constraint::Length(1 + carve_row),
            Constraint::Length(vol_rows),
            Constraint::Min(3),
        ])
        .areas(area);
        // Heading: what, and for how long.
        let elapsed = duration(self.ended.unwrap_or_else(|| self.started.elapsed()));
        f.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(self.title.clone(), theme.bold()),
                    Span::styled(format!("   {elapsed}"), theme.dim()),
                ]),
                Line::from(Span::styled(
                    format!(
                        "{}{}",
                        v.prefix,
                        ellipsize_start(
                            &v.subtitle,
                            width.saturating_sub(1 + v.prefix.chars().count())
                        )
                    ),
                    theme.dim(),
                )),
            ]),
            head,
        );
        // The steps as a checklist; the current one has a bar.
        for (i, st) in steps.iter().enumerate() {
            if i as u16 >= steps_a.height {
                break;
            }
            let row = Rect {
                y: steps_a.y + i as u16,
                height: 1,
                ..steps_a
            };
            draw_step(f, row, st, theme);
        }
        // Counters.
        let c = &v.pipeline.counts;
        let mut counters = vec![Line::from(vec![
            Span::styled(" found so far:  recovered ", theme.dim()),
            Span::styled(
                format!("{}", c.recovered + c.recovered_unverified),
                theme.good(),
            ),
            Span::styled("   damaged ", theme.dim()),
            Span::styled(format!("{}", c.damaged + c.failed), theme.bad()),
            Span::styled("   repaired ", theme.dim()),
            Span::styled(
                format!("{}", c.repaired + c.partially_repaired),
                theme.good(),
            ),
            Span::styled("   duplicates ", theme.dim()),
            Span::raw(format!("{}", c.duplicate)),
        ])];
        if carve_row > 0 {
            if let Some(cv) = &v.pipeline.carve {
                counters.push(Line::from(Span::styled(
                    format!(
                        " raw data search: {} of {} read, {} found",
                        bytes(cv.scanned),
                        bytes(cv.total),
                        plural(cv.files, "file", "files")
                    ),
                    theme.dim(),
                )));
            }
        }
        f.render_widget(Paragraph::new(counters), counts);
        // Volumes.
        if vol_rows > 0 {
            let mut lines: Vec<Line<'static>> = volumes
                .iter()
                .take(2)
                .map(|l| {
                    Line::from(Span::styled(
                        format!(" {}", ellipsize_start(l, width.saturating_sub(2))),
                        theme.dim(),
                    ))
                })
                .collect();
            if volumes.len() > 2 {
                lines[1] = Line::from(Span::styled(
                    format!(" ... and {} more volumes", volumes.len() - 1),
                    theme.dim(),
                ));
            }
            f.render_widget(Paragraph::new(lines), vols);
        }
        // Recent files, newest at the bottom.
        let rb = theme.block(" Latest files ");
        let ri = rb.inner(recent);
        f.render_widget(rb, recent);
        let h = ri.height as usize;
        let lines: Vec<Line<'static>> = v
            .pipeline
            .recent_files
            .iter()
            .rev()
            .take(h)
            .rev()
            .map(|file| {
                let tone = outcome_tone(file.outcome);
                Line::from(vec![
                    Span::styled(
                        format!(
                            "{} {} ",
                            theme.tone_mark(tone),
                            fit(outcome_label(file.outcome), 24)
                        ),
                        theme.tone(tone),
                    ),
                    Span::raw(ellipsize_start(&file.name, width.saturating_sub(32))),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(lines), ri);
    }
}

fn image_percent(s: &ImageState) -> String {
    match s.stats() {
        Some(st) => format!("{} copied", percent(st.rescued, st.total())),
        None => "starting".to_string(),
    }
}

fn image_header(s: &ImageState, plan: &Plan) -> Header {
    let (source, detail) = match &s.source {
        Some(info) => (
            format!("{} ({})", info.path.display(), devices::device_label(info)),
            format!("{} -> {}", info.kind.display_name(), s.image.display()),
        ),
        None => (
            plan.source()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            format!("-> {}", s.image.display()),
        ),
    };
    let profile = match &s.profile {
        Some(p) => format!("profile {}: {}", p.name, text::profile_blurb(&p.name)),
        None => String::new(),
    };
    Header {
        source,
        detail,
        profile,
    }
}

/// One line of the checklist.
#[derive(Debug, Clone)]
pub struct StepRow {
    /// What it does.
    pub label: String,
    /// Where it is.
    pub state: StepState,
}

/// Where a step is.
#[derive(Debug, Clone)]
pub enum StepState {
    /// Done, taking this long.
    Done(Option<Duration>),
    /// Running, with progress when known (0..=1) and a note.
    Active(Option<f64>, String),
    /// Not started.
    Pending,
    /// Not needed.
    Skipped,
}

fn draw_step(f: &mut Frame, row: Rect, st: &StepRow, theme: &Theme) {
    let (mark, style) = match &st.state {
        StepState::Done(_) => ("[x]", theme.good()),
        StepState::Active(..) => ("[>]", theme.bold()),
        StepState::Pending => ("[ ]", theme.dim()),
        StepState::Skipped => ("[-]", theme.dim()),
    };
    let w = row.width as usize;
    // Room for " [x] ", the label and up to 30 characters of progress.
    let label_w = w.saturating_sub(35).clamp(24, 46);
    let label = fit(&ellipsize_end_str(&st.label, label_w), label_w);
    let mut spans = vec![
        Span::styled(format!(" {mark} "), style),
        Span::styled(label, style),
    ];
    match &st.state {
        StepState::Done(t) => {
            spans.push(Span::styled(
                match t {
                    Some(d) if d.as_millis() > 0 => format!("done in {}", duration(*d)),
                    _ => "done".to_string(),
                },
                theme.dim(),
            ));
            f.render_widget(Paragraph::new(Line::from(spans)), row);
        }
        StepState::Active(ratio, note) => {
            let used = spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>();
            let rest = w.saturating_sub(used);
            let bar_w = rest.saturating_sub(note.chars().count() + 4).clamp(4, 24);
            let r = ratio.unwrap_or(0.0);
            let filled = ((bar_w as f64) * r).round() as usize;
            let bar = format!(
                "{}{}",
                "#".repeat(filled.min(bar_w)),
                "-".repeat(bar_w - filled.min(bar_w))
            );
            spans.push(Span::styled(format!("[{bar}] "), theme.good()));
            spans.push(Span::raw(note.clone()));
            f.render_widget(Paragraph::new(Line::from(spans)), row);
        }
        StepState::Pending => f.render_widget(Paragraph::new(Line::from(spans)), row),
        StepState::Skipped => {
            spans.push(Span::styled("not needed", theme.dim()));
            f.render_widget(Paragraph::new(Line::from(spans)), row);
        }
    }
}

fn ellipsize_end_str(s: &str, max: usize) -> String {
    crate::widgets::ellipsize_end(s, max)
}

/// What the recovery progress view needs from any of the jobs.
pub struct ProgressView<'a> {
    /// Stages so far.
    pub pipeline: &'a jobs::Pipeline,
    /// A second heading line, and words that come before it.
    pub subtitle: String,
    /// Shown before the subtitle; never shortened.
    pub prefix: &'static str,
    /// The job has ended.
    pub finished: bool,
    /// Imaging happened before (a rescue), and how far it is.
    pub imaging: Option<(bool, Option<f64>, String)>,
    /// Which slots exist for this kind of job.
    pub slots: &'static [Slot],
}

/// A place in the checklist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// Copying the device first.
    Imaging,
    /// Partition table.
    Partitions,
    /// File lists.
    Listing,
    /// Copying files found through the filesystem.
    Extracting,
    /// Raw data.
    Carving,
    /// Checking.
    Validating,
    /// Repairing.
    Repairing,
    /// Report.
    Report,
}

const RECOVER_SLOTS: &[Slot] = &[
    Slot::Imaging,
    Slot::Partitions,
    Slot::Listing,
    Slot::Extracting,
    Slot::Carving,
    Slot::Validating,
    Slot::Repairing,
    Slot::Report,
];
const SCAN_SLOTS: &[Slot] = &[Slot::Partitions, Slot::Listing, Slot::Carving];
const REPAIR_SLOTS: &[Slot] = &[Slot::Validating, Slot::Repairing];

fn slot_of(stage: &Stage) -> Slot {
    match stage {
        Stage::Partitions => Slot::Partitions,
        Stage::Listing { .. } => Slot::Listing,
        Stage::Extracting => Slot::Extracting,
        Stage::Carving => Slot::Carving,
        Stage::Validating => Slot::Validating,
        Stage::Repairing => Slot::Repairing,
        Stage::Report => Slot::Report,
    }
}

fn slot_label(slot: Slot) -> &'static str {
    match slot {
        Slot::Imaging => "Copy the device to an image",
        Slot::Partitions => "Read the partition table",
        Slot::Listing => "Read the file lists",
        Slot::Extracting => "Copy the files found by name",
        Slot::Carving => "Search the raw data for lost files",
        Slot::Validating => "Check the files",
        Slot::Repairing => "Repair damaged files",
        Slot::Report => "Write the report",
    }
}

impl<'a> ProgressView<'a> {
    fn from_recover(s: &'a RecoverState, run: &Run) -> Self {
        let imaging = s
            .image
            .as_ref()
            .map(|i| {
                (
                    s.status != RecoverStatus::Imaging && s.status != RecoverStatus::Checking,
                    i.stats().map(|st| fraction(st.rescued, st.total())),
                    image_percent(i),
                )
            })
            // The copy was made in a first step of its own.
            .or_else(|| (run.step.1 > 1).then(|| (true, None, String::new())));
        ProgressView {
            pipeline: &s.pipeline,
            prefix: if run.step.1 > 1 {
                "Step 2 of 2: recovering from the copy. "
            } else {
                ""
            },
            subtitle: s
                .source
                .as_ref()
                .map(|i| format!("from {} to {}", i.path.display(), s.output.display()))
                .unwrap_or_else(|| format!("to {}", s.output.display())),
            finished: run.outcome.is_some(),
            imaging,
            slots: RECOVER_SLOTS,
        }
    }

    fn from_scan(s: &'a ScanState, run: &Run) -> Self {
        ProgressView {
            pipeline: &s.pipeline,
            prefix: "",
            subtitle: s
                .source
                .as_ref()
                .map(|i| format!("looking at {}; nothing is written", i.path.display()))
                .unwrap_or_else(|| "nothing is written".to_string()),
            finished: run.outcome.is_some(),
            imaging: None,
            slots: SCAN_SLOTS,
        }
    }

    fn from_repair(s: &'a RepairState, run: &Run) -> Self {
        ProgressView {
            pipeline: &s.pipeline,
            prefix: "",
            subtitle: format!(
                "{} to {}",
                plural(s.inputs as u64, "file", "files"),
                s.output.join("repaired").display()
            ),
            finished: run.outcome.is_some(),
            imaging: None,
            slots: REPAIR_SLOTS,
        }
    }

    /// The checklist.
    pub fn steps(&self, carve: bool, repair: bool) -> Vec<StepRow> {
        let stages = &self.pipeline.stages;
        let current_slot = if self.finished {
            None
        } else {
            stages.last().map(|s| slot_of(&s.stage))
        };
        let order = |s: Slot| {
            self.slots
                .iter()
                .position(|x| *x == s)
                .unwrap_or(usize::MAX)
        };
        let mut rows = Vec::new();
        for &slot in self.slots {
            let latest = stages.iter().rev().find(|s| slot_of(&s.stage) == slot);
            let label = match (slot, latest) {
                (Slot::Listing, Some(s)) => match &s.stage {
                    Stage::Listing { volume, fs } => {
                        format!("Read the file list of volume {volume} ({})", fs.name())
                    }
                    _ => slot_label(slot).to_string(),
                },
                (Slot::Partitions, Some(s)) if s.total > 0 || s.done > 0 => {
                    stage_progress_label(&s.stage)
                }
                _ => slot_label(slot).to_string(),
            };
            let state = if slot == Slot::Imaging {
                match &self.imaging {
                    Some((true, _, _)) => StepState::Done(None),
                    Some((false, r, note)) => StepState::Active(*r, note.clone()),
                    None => continue,
                }
            } else if let Some(s) = latest {
                if Some(slot) == current_slot {
                    if slot == Slot::Carving {
                        let cv = self.pipeline.carve.unwrap_or_default();
                        StepState::Active(
                            (cv.total > 0).then(|| fraction(cv.scanned, cv.total)),
                            format!("{} found", plural(cv.files, "file", "files")),
                        )
                    } else {
                        StepState::Active(
                            s.fraction(),
                            if s.total > 0 {
                                format!("{} of {}", s.done, s.total)
                            } else {
                                "working...".to_string()
                            },
                        )
                    }
                } else {
                    StepState::Done(s.took)
                }
            } else if (slot == Slot::Carving && !carve)
                || (slot == Slot::Repairing && !repair)
                || self.finished
                || current_slot.is_some_and(|c| order(slot) < order(c))
            {
                StepState::Skipped
            } else {
                StepState::Pending
            };
            rows.push(StepRow { label, state });
        }
        rows
    }
}
