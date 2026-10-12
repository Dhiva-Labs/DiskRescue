//! Imaging events as JSON lines: the format of `diskrescue image
//! --json-progress`, and its parser.
//!
//! The command line prints one object per line: a `start` object, then one
//! per engine event (`progress`, `phase`, `log`, `device_lost`,
//! `device_back`, `paused`, `saved`), and a final `finished` object with the
//! summary. Block-map snapshots are not part of the stream. [`parse_line`]
//! turns a line back into the engine's [`Event`] so that a front-end that
//! drives the command line (the privileged helper) shares its state code
//! with one that runs the engine itself.

use std::path::PathBuf;
use std::time::Duration;

use diskrescue_imaging::{Event, Phase, Progress, Stats, Summary};
use serde_json::{json, Value};

/// JSON for a phase.
pub fn phase_json(p: &Phase) -> Value {
    match p {
        Phase::Copying { pass, reverse } => {
            json!({"name": "copying", "pass": pass, "reverse": reverse, "text": p.to_string()})
        }
        Phase::Retrying { pass, reverse } => {
            json!({"name": "retrying", "pass": pass, "reverse": reverse, "text": p.to_string()})
        }
        Phase::Trimming => json!({"name": "trimming", "text": p.to_string()}),
        Phase::Dividing => json!({"name": "dividing", "text": p.to_string()}),
        Phase::Scraping => json!({"name": "scraping", "text": p.to_string()}),
        Phase::Finished => json!({"name": "finished", "text": p.to_string()}),
    }
}

/// JSON for rescue totals.
pub fn stats_json(s: &Stats) -> Value {
    json!({
        "rescued": s.rescued,
        "non_tried": s.non_tried,
        "non_trimmed": s.non_trimmed,
        "non_scraped": s.non_scraped,
        "bad": s.bad_sectors,
        "bad_areas": s.bad_areas,
        "total": s.total(),
        "percent_rescued": s.rescued_fraction() * 100.0,
    })
}

/// JSON for a summary (the `finished` event).
pub fn summary_json(s: &Summary) -> Value {
    json!({
        "event": "finished",
        "finished": s.finished,
        "cancelled": s.cancelled,
        "elapsed_secs": s.elapsed.as_secs_f64(),
        "errors": s.errors,
        "timeouts": s.timeouts,
        "slow_reads": s.slow_reads,
        "reattaches": s.reattaches,
        "image": s.output,
        "mapfile": s.mapfile,
        "stats": stats_json(&s.stats),
        "warnings": s.warnings,
    })
}

/// JSON for an engine event (map snapshots are not reported).
pub fn event_json(ev: &Event) -> Option<Value> {
    Some(match ev {
        Event::Progress(p) => json!({
            "event": "progress",
            "phase": phase_json(&p.phase),
            "position": p.current_pos,
            "size": p.size,
            "stats": stats_json(&p.stats),
            "rate": p.current_rate,
            "average_rate": p.average_rate,
            "elapsed_secs": p.elapsed.as_secs_f64(),
            "eta_secs": p.eta.map(|d| d.as_secs_f64()),
            "errors": p.errors,
            "timeouts": p.timeouts,
            "slow_reads": p.slow_reads,
            "reattaches": p.reattaches,
            "last_error": p.last_error,
        }),
        Event::Phase(p) => json!({"event": "phase", "phase": phase_json(p)}),
        Event::Log(level, msg) => json!({
            "event": "log",
            "level": level.to_string().to_lowercase(),
            "message": msg,
        }),
        Event::DeviceLost { at } => json!({"event": "device_lost", "position": at}),
        Event::DeviceBack { after } => {
            json!({"event": "device_back", "after_secs": after.as_secs_f64()})
        }
        Event::Paused(d) => json!({"event": "paused", "secs": d.as_secs_f64()}),
        Event::Saved => json!({"event": "saved"}),
        Event::Finished(s) => summary_json(s),
        Event::Map(_) => return None,
    })
}

/// The `start` line: what is being copied where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartInfo {
    /// The source.
    pub source: PathBuf,
    /// The image.
    pub image: PathBuf,
    /// The progress map.
    pub mapfile: PathBuf,
    /// Profile name.
    pub profile: String,
    /// Bytes the copy covers.
    pub size: u64,
    /// An earlier copy is being continued.
    pub resumed: bool,
    /// The command line recorded in the progress map.
    pub command_line: String,
}

/// JSON for the `start` line.
pub fn start_json(s: &StartInfo) -> Value {
    json!({
        "event": "start",
        "source": s.source,
        "image": s.image,
        "mapfile": s.mapfile,
        "profile": s.profile,
        "size": s.size,
        "resumed": s.resumed,
        "command_line": s.command_line,
    })
}

/// One parsed line.
#[derive(Debug, Clone)]
pub enum JsonLine {
    /// The `start` line.
    Start(StartInfo),
    /// An engine event.
    Event(Event),
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

fn f(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn secs(x: f64) -> Duration {
    if x.is_finite() && x > 0.0 {
        Duration::try_from_secs_f64(x).unwrap_or(Duration::MAX)
    } else {
        Duration::ZERO
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A phase from its JSON.
pub fn phase_from_json(v: &Value) -> Option<Phase> {
    let pass = v.get("pass").and_then(Value::as_u64).unwrap_or(1) as u32;
    let reverse = v.get("reverse").and_then(Value::as_bool).unwrap_or(false);
    Some(match v.get("name")?.as_str()? {
        "copying" => Phase::Copying { pass, reverse },
        "retrying" => Phase::Retrying { pass, reverse },
        "trimming" => Phase::Trimming,
        "dividing" => Phase::Dividing,
        "scraping" => Phase::Scraping,
        "finished" => Phase::Finished,
        _ => return None,
    })
}

/// Rescue totals from their JSON.
pub fn stats_from_json(v: &Value) -> Stats {
    let non_trimmed = u(v, "non_trimmed");
    let non_scraped = u(v, "non_scraped");
    let bad = u(v, "bad");
    Stats {
        rescued: u(v, "rescued"),
        non_tried: u(v, "non_tried"),
        non_trimmed,
        non_scraped,
        bad_sectors: bad,
        bad_areas: u(v, "bad_areas"),
        errsize: non_trimmed + non_scraped + bad,
    }
}

/// A summary from the `finished` line.
pub fn summary_from_json(v: &Value) -> Summary {
    Summary {
        stats: v.get("stats").map(stats_from_json).unwrap_or_default(),
        elapsed: secs(f(v, "elapsed_secs")),
        errors: u(v, "errors"),
        finished: v.get("finished").and_then(Value::as_bool).unwrap_or(false),
        cancelled: v.get("cancelled").and_then(Value::as_bool).unwrap_or(false),
        reattaches: u(v, "reattaches"),
        timeouts: u(v, "timeouts"),
        slow_reads: u(v, "slow_reads"),
        output: PathBuf::from(s(v, "image")),
        mapfile: PathBuf::from(s(v, "mapfile")),
        warnings: v
            .get("warnings")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|w| w.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn level_from(text: &str) -> log::Level {
    match text {
        "error" => log::Level::Error,
        "warn" | "warning" => log::Level::Warn,
        "debug" => log::Level::Debug,
        "trace" => log::Level::Trace,
        _ => log::Level::Info,
    }
}

/// Parse one line of `--json-progress` output. Lines that are not JSON
/// objects of a known kind give `None`.
pub fn parse_line(line: &str) -> Option<JsonLine> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let ev = match v.get("event")?.as_str()? {
        "start" => {
            return Some(JsonLine::Start(StartInfo {
                source: PathBuf::from(s(&v, "source")),
                image: PathBuf::from(s(&v, "image")),
                mapfile: PathBuf::from(s(&v, "mapfile")),
                profile: s(&v, "profile"),
                size: u(&v, "size"),
                resumed: v.get("resumed").and_then(Value::as_bool).unwrap_or(false),
                command_line: s(&v, "command_line"),
            }))
        }
        "progress" => Event::Progress(Progress {
            phase: v.get("phase").and_then(phase_from_json)?,
            stats: v.get("stats").map(stats_from_json).unwrap_or_default(),
            current_pos: u(&v, "position"),
            current_rate: f(&v, "rate"),
            average_rate: f(&v, "average_rate"),
            elapsed: secs(f(&v, "elapsed_secs")),
            eta: v.get("eta_secs").and_then(Value::as_f64).map(secs),
            errors: u(&v, "errors"),
            slow_reads: u(&v, "slow_reads"),
            timeouts: u(&v, "timeouts"),
            reattaches: u(&v, "reattaches"),
            last_error: v
                .get("last_error")
                .and_then(Value::as_str)
                .map(String::from),
            size: u(&v, "size"),
        }),
        "phase" => Event::Phase(v.get("phase").and_then(phase_from_json)?),
        "log" => Event::Log(level_from(&s(&v, "level")), s(&v, "message")),
        "device_lost" => Event::DeviceLost {
            at: u(&v, "position"),
        },
        "device_back" => Event::DeviceBack {
            after: secs(f(&v, "after_secs")),
        },
        "paused" => Event::Paused(secs(f(&v, "secs"))),
        "saved" => Event::Saved,
        "finished" => Event::Finished(summary_from_json(&v)),
        _ => return None,
    };
    Some(JsonLine::Event(ev))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress() -> Progress {
        Progress {
            phase: Phase::Retrying {
                pass: 2,
                reverse: true,
            },
            stats: Stats {
                rescued: 1000,
                non_tried: 10,
                non_trimmed: 20,
                non_scraped: 30,
                bad_sectors: 40,
                bad_areas: 2,
                errsize: 90,
            },
            current_pos: 512,
            current_rate: 1.5,
            average_rate: 2.5,
            elapsed: Duration::from_millis(1500),
            eta: Some(Duration::from_secs(3)),
            errors: 4,
            slow_reads: 5,
            timeouts: 6,
            reattaches: 7,
            last_error: Some("read error at 0x200".into()),
            size: 1100,
        }
    }

    #[test]
    fn events_survive_a_round_trip() {
        let summary = Summary {
            stats: progress().stats,
            elapsed: Duration::from_secs(9),
            errors: 1,
            finished: true,
            cancelled: false,
            reattaches: 0,
            timeouts: 2,
            slow_reads: 3,
            output: PathBuf::from("card.img"),
            mapfile: PathBuf::from("card.img.map"),
            warnings: vec!["note".into()],
        };
        for ev in [
            Event::Progress(progress()),
            Event::Phase(Phase::Copying {
                pass: 1,
                reverse: false,
            }),
            Event::Phase(Phase::Trimming),
            Event::Log(log::Level::Warn, "careful".into()),
            Event::DeviceLost { at: 4096 },
            Event::DeviceBack {
                after: Duration::from_secs(2),
            },
            Event::Paused(Duration::from_secs(5)),
            Event::Saved,
            Event::Finished(summary.clone()),
        ] {
            let text = event_json(&ev).unwrap().to_string();
            let Some(JsonLine::Event(back)) = parse_line(&text) else {
                panic!("not parsed: {text}");
            };
            assert_eq!(event_json(&back).unwrap().to_string(), text);
        }
        let Some(JsonLine::Event(Event::Progress(p))) = parse_line(
            &event_json(&Event::Progress(progress()))
                .unwrap()
                .to_string(),
        ) else {
            panic!()
        };
        assert_eq!(p, progress());
        let Some(JsonLine::Event(Event::Finished(s))) =
            parse_line(&summary_json(&summary).to_string())
        else {
            panic!()
        };
        assert_eq!(s, summary);
    }

    #[test]
    fn start_line_and_garbage() {
        let st = StartInfo {
            source: PathBuf::from("/dev/sdb"),
            image: PathBuf::from("a.img"),
            mapfile: PathBuf::from("a.img.map"),
            profile: "sd".into(),
            size: 42,
            resumed: true,
            command_line: "diskrescue image /dev/sdb a.img".into(),
        };
        let Some(JsonLine::Start(back)) = parse_line(&start_json(&st).to_string()) else {
            panic!()
        };
        assert_eq!(back, st);
        assert!(parse_line("warning: not json").is_none());
        assert!(parse_line("{\"event\":\"unknown\"}").is_none());
        assert!(parse_line("[1,2]").is_none());
    }
}
